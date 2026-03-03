use bytes::BytesMut;
use futures::{
    Future,
    future::{BoxFuture, FutureExt},
};
use serde::{Deserialize, Serialize, ser};
use std::{
    cell::RefCell,
    error::Error,
    fmt,
    io::BufWriter,
    marker::PhantomData,
    panic,
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
};
use tracing::Instrument;

use super::{
    super::{DEFAULT_MAX_ITEM_SIZE, SendErrorExt},
    BIG_DATA_CHUNK_QUEUE, BIG_DATA_LIMIT,
    io::{ChannelBytesWriter, LimitedBytesWriter},
};
use crate::{
    chmux::{self, AnyStorage, PortReq},
    codec::{self, SerializationError, StreamingUnavailable},
    exec::{self, task},
    rch::base::io::IoWriter,
};

pub use crate::chmux::Closed;

/// An error that occurred during remote sending.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SendError<T> {
    /// Error kind.
    pub kind: SendErrorKind,
    /// Item that could not be sent.
    pub item: T,
}

/// Error kind that occurred during remote sending.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SendErrorKind {
    /// Serialization of the item failed.
    Serialize(SerializationError),
    /// Sending of the serialized item over the chmux channel failed.
    Send(chmux::SendError),
    /// Maximum item size was exceeded.
    MaxItemSizeExceeded,
}

impl SendErrorKind {
    /// True, if the remote endpoint closed the channel.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Send(err) if err.is_closed())
    }

    /// True, if the remote endpoint closed the channel, was dropped or the connection failed.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::Send(_))
    }

    /// Returns whether the error is final, i.e. no further send operation can succeed.
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Send(_))
    }

    /// Returns whether the error is caused by the item to be sent.
    pub fn is_item_specific(&self) -> bool {
        matches!(self, Self::Serialize(_) | Self::MaxItemSizeExceeded)
    }
}

impl<T> SendError<T> {
    pub(crate) fn new(kind: SendErrorKind, item: T) -> Self {
        Self { kind, item }
    }

    /// Returns true, if error it due to channel being closed.
    pub fn is_closed(&self) -> bool {
        self.kind.is_closed()
    }

    /// True, if the remote endpoint closed the channel, was dropped or the connection failed.
    pub fn is_disconnected(&self) -> bool {
        self.kind.is_disconnected()
    }

    /// Returns whether the error is final, i.e. no further send operation can succeed.
    pub fn is_final(&self) -> bool {
        self.kind.is_final()
    }

    /// Returns whether the error is caused by the item to be sent.
    pub fn is_item_specific(&self) -> bool {
        self.kind.is_item_specific()
    }

    /// Returns the error without the contained item.
    pub fn without_item(self) -> SendError<()> {
        SendError { kind: self.kind, item: () }
    }
}

impl<T> SendErrorExt for SendError<T> {
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    fn is_disconnected(&self) -> bool {
        self.is_disconnected()
    }

    fn is_final(&self) -> bool {
        self.is_final()
    }

    fn is_item_specific(&self) -> bool {
        self.is_item_specific()
    }
}

impl fmt::Display for SendErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Serialize(err) => write!(f, "serialization error: {err}"),
            Self::Send(err) => write!(f, "send error: {err}"),
            Self::MaxItemSizeExceeded => write!(f, "maximum item size exceeded"),
        }
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", &self.kind)
    }
}

impl<T> Error for SendError<T> where T: fmt::Debug {}

/// Gathers ports to send to the remote endpoint during object serialization.
pub struct PortSerializer {
    allocator: chmux::PortAllocator,
    #[allow(clippy::type_complexity)]
    requests:
        Vec<(chmux::PortNumber, Box<dyn FnOnce(chmux::Connect) -> BoxFuture<'static, ()> + Send + 'static>)>,
    storage: AnyStorage,
    tasks: Vec<BoxFuture<'static, ()>>,
}

impl PortSerializer {
    thread_local! {
        static INSTANCE: RefCell<Weak<RefCell<PortSerializer>>> = const { RefCell::new(Weak::new()) };
    }

    /// Create a new port serializer and register it as active.
    fn start(allocator: chmux::PortAllocator, storage: AnyStorage) -> Rc<RefCell<Self>> {
        let this = Rc::new(RefCell::new(Self { allocator, requests: Vec::new(), storage, tasks: Vec::new() }));
        let weak = Rc::downgrade(&this);
        Self::INSTANCE.with(move |i| i.replace(weak));
        this
    }

    /// Gets the active port serializer for this thread.
    fn instance<E>() -> Result<Rc<RefCell<Self>>, E>
    where
        E: serde::ser::Error,
    {
        match Self::INSTANCE.with(|i| i.borrow().upgrade()) {
            Some(this) => Ok(this),
            None => Err(ser::Error::custom("this remoc object can only be serialized for sending")),
        }
    }

    /// Deregister the active port serializer and return it.
    fn finish(this: Rc<RefCell<Self>>) -> Self {
        match Rc::try_unwrap(this) {
            Ok(i) => i.into_inner(),
            Err(_) => panic!("PortSerializer is referenced after serialization finished"),
        }
    }

    /// Open a chmux port to the remote endpoint.
    ///
    /// Returns the local port number and calls the specified function with the connect object.
    pub fn connect<E>(
        callback: impl FnOnce(chmux::Connect) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> Result<u32, E>
    where
        E: serde::ser::Error,
    {
        let this = Self::instance()?;
        let mut this =
            this.try_borrow_mut().expect("PortSerializer is referenced multiple times during serialization");

        let local_port = this.allocator.try_allocate().ok_or_else(|| ser::Error::custom("ports exhausted"))?;
        let local_port_num = *local_port;
        this.requests.push((local_port, Box::new(callback)));

        Ok(local_port_num)
    }

    /// Returns the data storage of the channel multiplexer.
    pub fn storage<E>() -> Result<AnyStorage, E>
    where
        E: serde::ser::Error,
    {
        let this = Self::instance()?;
        let this = this.try_borrow().expect("PortSerializer is referenced multiple times during serialization");

        Ok(this.storage.clone())
    }

    /// Spawn a task.
    pub fn spawn<E>(task: impl Future<Output = ()> + Send + 'static) -> Result<(), E>
    where
        E: serde::ser::Error,
    {
        let this = Self::instance()?;
        let mut this =
            this.try_borrow_mut().expect("PortSerializer is referenced multiple times during serialization");

        this.tasks.push(task.boxed());
        Ok(())
    }
}

/// Non-generic: stream serialized chunks over chmux.
///
/// Separated from the generic `Sender::send` to avoid monomorphization.
#[inline(never)]
async fn send_chunks_from_stream(
    sender: &mut chmux::Sender,
    mut rx: tokio::sync::mpsc::Receiver<BytesMut>,
    max_item_size: usize,
) -> Result<(), SendErrorKind> {
    let mut sc = sender.send_chunks();
    let mut total = 0;
    while let Some(chunk) = rx.recv().await {
        total += chunk.len();
        if total > max_item_size {
            return Err(SendErrorKind::MaxItemSizeExceeded);
        }
        sc = sc.send(chunk.freeze()).await.map_err(SendErrorKind::Send)?;
    }
    sc.finish().await.map_err(SendErrorKind::Send)
}

/// Non-generic: connect ports obtained during serialization and spawn callbacks.
///
/// Separated from the generic `Sender::send` to avoid monomorphization.
#[inline(never)]
async fn connect_ports_and_spawn(
    sender: &mut chmux::Sender,
    ps: PortSerializer,
) -> Result<(), chmux::SendError> {
    let PortSerializer { requests, tasks, .. } = ps;

    let mut ports = Vec::new();
    let mut callbacks = Vec::new();
    for (port, callback) in requests {
        ports.push(PortReq::new(port));
        callbacks.push(callback);
    }

    let connects = if ports.is_empty() {
        Vec::new()
    } else {
        sender.connect(ports, true).await?
    };

    for (callback, connect) in callbacks.into_iter().zip(connects.into_iter()) {
        exec::spawn(callback(connect).in_current_span());
    }

    for task in tasks {
        exec::spawn(task.in_current_span());
    }

    Ok(())
}

/// Sends arbitrary values to a remote endpoint.
///
/// Values may be or contain any channel from this crate.
pub struct Sender<T, Codec = codec::Default> {
    sender: chmux::Sender,
    big_data: i8,
    max_item_size: usize,
    _data: PhantomData<T>,
    _codec: PhantomData<Codec>,
}

impl<T, Codec> fmt::Debug for Sender<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T, Codec> Sender<T, Codec>
where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    /// Creates a base remote sender from a [chmux] sender.
    pub fn new(sender: chmux::Sender) -> Self {
        Self {
            sender,
            big_data: 0,
            max_item_size: DEFAULT_MAX_ITEM_SIZE,
            _data: PhantomData,
            _codec: PhantomData,
        }
    }

    fn serialize_buffered(
        allocator: chmux::PortAllocator, storage: AnyStorage, item: &T, limit: usize,
    ) -> Result<Option<(BytesMut, PortSerializer)>, SerializationError> {
        let mut lw = LimitedBytesWriter::new(limit);
        let ps_ref = PortSerializer::start(allocator, storage);

        match <Codec as codec::Codec>::serialize(IoWriter::Limited(&mut lw), &item) {
            _ if lw.overflow() => return Ok(None),
            Ok(()) => (),
            Err(err) => return Err(err),
        };

        let ps = PortSerializer::finish(ps_ref);
        Ok(Some((lw.into_inner().unwrap(), ps)))
    }

    async fn serialize_streaming(
        allocator: chmux::PortAllocator, storage: AnyStorage, item: T, tx: tokio::sync::mpsc::Sender<BytesMut>,
        chunk_size: usize,
    ) -> Result<(T, PortSerializer, usize), (SerializationError, T)> {
        if !exec::are_threads_available().await {
            return Err((SerializationError::new(StreamingUnavailable), item));
        }

        let cbw = ChannelBytesWriter::new(tx);
        let mut cbw = BufWriter::with_capacity(chunk_size, cbw);

        let item_arc = Arc::new(Mutex::new(item));
        let item_arc_task = item_arc.clone();

        let result = task::spawn_blocking(move || {
            let ps_ref = PortSerializer::start(allocator, storage);

            let item = item_arc_task.lock().unwrap();
            <Codec as codec::Codec>::serialize(IoWriter::Channel(&mut cbw), &*item)?;

            let cbw = cbw.into_inner().map_err(|_| {
                SerializationError::new(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "flush failed"))
            })?;

            let ps = PortSerializer::finish(ps_ref);
            Ok((ps, cbw.written()))
        })
        .await;

        let item = match Arc::try_unwrap(item_arc) {
            Ok(item_mutex) => match item_mutex.into_inner() {
                Ok(item) => item,
                Err(err) => err.into_inner(),
            },
            Err(_) => unreachable!("serialization task has terminated"),
        };

        match result {
            Ok(Ok((ps, written))) => Ok((item, ps, written)),
            Ok(Err(err)) => Err((err, item)),
            Err(err) => match err.try_into_panic() {
                Ok(payload) => panic::resume_unwind(payload),
                Err(err) => Err((SerializationError::new(err), item)),
            },
        }
    }

    /// Sends an item over the channel.
    ///
    /// The item may contain ports that will be serialized and connected as well.
    pub async fn send(&mut self, item: T) -> Result<(), SendError<T>> {
        // Determine if it is worthy to try buffered serialization.
        let data_ps = if self.big_data <= 0 {
            // Try buffered serialization.
            match Self::serialize_buffered(
                self.sender.port_allocator(),
                self.sender.storage(),
                &item,
                self.sender.max_data_size(),
            ) {
                Ok(Some(v)) => {
                    self.big_data = (self.big_data - 1).max(-BIG_DATA_LIMIT);
                    Some(v)
                }
                Ok(None) => {
                    self.big_data = (self.big_data + 1).min(BIG_DATA_LIMIT);
                    None
                }
                Err(err) => return Err(SendError::new(SendErrorKind::Serialize(err), item)),
            }
        } else {
            // Buffered serialization unlikely to succeed.
            None
        };

        let (item, ps) = match data_ps {
            Some((data, ps)) => {
                if data.len() > self.max_item_size {
                    return Err(SendError::new(SendErrorKind::MaxItemSizeExceeded, item));
                }

                // Send buffered data.
                if let Err(err) = self.sender.send(data.freeze()).await {
                    return Err(SendError::new(SendErrorKind::Send(err), item));
                }
                (item, ps)
            }

            None => {
                // Stream data while serializing.
                let (tx, rx) = tokio::sync::mpsc::channel(BIG_DATA_CHUNK_QUEUE);
                let allocator = self.sender.port_allocator();
                let storage = self.sender.storage();
                let chunk_size = self.sender.chunk_size();
                let ser_task = Self::serialize_streaming(allocator, storage, item, tx, chunk_size);
                let send_task = send_chunks_from_stream(&mut self.sender, rx, self.max_item_size);

                match tokio::join!(ser_task, send_task) {
                    (Ok((item, ps, size)), Ok(())) => {
                        if size <= self.sender.max_data_size() {
                            self.big_data = (self.big_data - 1).max(-BIG_DATA_LIMIT);
                        }

                        (item, ps)
                    }
                    (Ok((item, _, _)), Err(kind)) | (Err((_, item)), Err(kind)) => {
                        // When sending fails, the serialization task will either finish
                        // or fail due to rx being dropped.
                        return Err(SendError::new(kind, item));
                    }
                    (Err((err, item)), _) => {
                        // When serialization fails, the send task will finish successfully
                        // since the rx stream will end normally.
                        return Err(SendError::new(SendErrorKind::Serialize(err), item));
                    }
                }
            }
        };

        // Connect ports obtained during serialization (non-generic).
        if let Err(err) = connect_ports_and_spawn(&mut self.sender, ps).await {
            return Err(SendError::new(SendErrorKind::Send(err), item));
        }

        // Ensure that item is dropped before connection callbacks run.
        drop(item);

        Ok(())
    }

    /// True, once the remote endpoint has closed its receiver.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// Returns a future that will resolve when the remote endpoint closes its receiver.
    pub fn closed(&self) -> Closed {
        self.sender.closed()
    }

    /// The maximum allowed size in bytes of an item to be sent.
    ///
    /// The default value is [DEFAULT_MAX_ITEM_SIZE].
    pub fn max_item_size(&self) -> usize {
        self.max_item_size
    }

    /// Sets the maximum allowed size in bytes of an item to be sent.
    ///
    /// This does not change the maximum allowed size on the receive end.
    /// Thus if the maximum allowed size is larger on the sender than on the
    /// [receiver](super::Receiver), sending of oversized items will succeed but the receiver
    /// will fail with a [MaxItemSizeExceeded error](super::RecvError::MaxItemSizeExceeded) when
    /// trying to receive the item.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.max_item_size = max_item_size;
    }
}
