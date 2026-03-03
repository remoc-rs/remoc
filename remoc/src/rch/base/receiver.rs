use bytes::{Buf, Bytes};
use futures::{
    Future,
    future::{BoxFuture, FutureExt},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    cell::RefCell,
    collections::HashMap,
    error::Error,
    fmt,
    marker::PhantomData,
    panic,
    rc::{Rc, Weak},
};
use tracing::Instrument;

use super::{super::DEFAULT_MAX_ITEM_SIZE, BIG_DATA_CHUNK_QUEUE, io::ChannelBytesReader};
use crate::{
    chmux::{self, AnyStorage, Received, RecvChunkError},
    codec::{self, DeserializationError, StreamingUnavailable},
    exec::{
        self,
        task::{self, JoinHandle},
    },
    rch::base::io::IoReader,
};

/// An error that occurred during receiving from a remote endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RecvError {
    /// Receiving data over the chmux channel failed.
    Receive(chmux::RecvError),
    /// Deserialization of received data failed.
    Deserialize(DeserializationError),
    /// Chmux ports required for deserialization of received channels were not received.
    MissingPorts(Vec<u32>),
    /// Maximum item size was exceeded.
    MaxItemSizeExceeded,
}

impl From<chmux::RecvError> for RecvError {
    fn from(err: chmux::RecvError) -> Self {
        Self::Receive(err)
    }
}

impl From<DeserializationError> for RecvError {
    fn from(err: DeserializationError) -> Self {
        Self::Deserialize(err)
    }
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Deserialize(err) => write!(f, "deserialization error: {err}"),
            Self::MissingPorts(ports) => write!(
                f,
                "missing chmux ports: {}",
                ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
            ),
            Self::MaxItemSizeExceeded => write!(f, "maximum item size exceeded"),
        }
    }
}

impl Error for RecvError {}

impl RecvError {
    /// Returns whether the error is final, i.e. no further receive operation can succeed.
    pub fn is_final(&self) -> bool {
        match self {
            Self::Receive(err) => err.is_final(),
            Self::Deserialize(_) | Self::MissingPorts(_) | Self::MaxItemSizeExceeded => false,
        }
    }
}

/// Gathers ports sent from the remote endpoint during deserialization.
pub struct PortDeserializer {
    allocator: chmux::PortAllocator,
    /// Callbacks by remote port.
    #[allow(clippy::type_complexity)]
    expected: HashMap<
        u32,
        (
            chmux::PortNumber,
            Box<dyn FnOnce(chmux::PortNumber, chmux::Request) -> BoxFuture<'static, ()> + Send + 'static>,
        ),
    >,
    storage: AnyStorage,
    tasks: Vec<BoxFuture<'static, ()>>,
}

impl PortDeserializer {
    thread_local! {
        static INSTANCE: RefCell<Weak<RefCell<PortDeserializer>>> = const { RefCell::new(Weak::new()) };
    }

    /// Create a new port deserializer and register it as active.
    fn start(allocator: chmux::PortAllocator, storage: AnyStorage) -> Rc<RefCell<PortDeserializer>> {
        let this =
            Rc::new(RefCell::new(Self { allocator, expected: HashMap::new(), storage, tasks: Vec::new() }));
        let weak = Rc::downgrade(&this);
        Self::INSTANCE.with(move |i| i.replace(weak));
        this
    }

    /// Gets the active port deserializer for this thread.
    fn instance<E>() -> Result<Rc<RefCell<Self>>, E>
    where
        E: serde::de::Error,
    {
        match Self::INSTANCE.with(|i| i.borrow().upgrade()) {
            Some(this) => Ok(this),
            None => Err(serde::de::Error::custom("this remoc object can only be deserialized during receiving")),
        }
    }

    /// Deregister the active port deserializer and return it.
    fn finish(this: Rc<RefCell<PortDeserializer>>) -> Self {
        match Rc::try_unwrap(this) {
            Ok(i) => i.into_inner(),
            Err(_) => panic!("PortDeserializer is referenced after deserialization finished"),
        }
    }

    /// Accept the chmux port with the specified remote port number sent from the remote endpoint.
    ///
    /// Returns the local port number and calls the specified function with the received connect request.
    pub fn accept<E>(
        remote_port: u32,
        callback: impl FnOnce(chmux::PortNumber, chmux::Request) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> Result<u32, E>
    where
        E: serde::de::Error,
    {
        let this = Self::instance()?;
        let mut this =
            this.try_borrow_mut().expect("PortDeserializer is referenced multiple times during deserialization");
        let local_port =
            this.allocator.try_allocate().ok_or_else(|| serde::de::Error::custom("ports exhausted"))?;
        let local_port_num = *local_port;
        this.expected.insert(remote_port, (local_port, Box::new(callback)));

        Ok(local_port_num)
    }

    /// Returns the data storage of the channel multiplexer.
    pub fn storage<E>() -> Result<AnyStorage, E>
    where
        E: serde::de::Error,
    {
        let this = Self::instance()?;
        let this =
            this.try_borrow().expect("PortDeserializer is referenced multiple times during deserialization");

        Ok(this.storage.clone())
    }

    /// Spawn a task.
    pub fn spawn<E>(task: impl Future<Output = ()> + Send + 'static) -> Result<(), E>
    where
        E: serde::de::Error,
    {
        let this = Self::instance()?;
        let mut this =
            this.try_borrow_mut().expect("PortDeserializer is referenced multiple times during deserialization");

        this.tasks.push(task.boxed());
        Ok(())
    }
}

/// Receives arbitrary values from a remote endpoint.
///
/// Values may be or contain any channel from this crate.
pub struct Receiver<T, Codec = codec::Default> {
    receiver: chmux::Receiver,
    recved: Option<Option<Received>>,
    data: DataSource<T>,
    item: Option<T>,
    port_deser: Option<PortDeserializer>,
    default_max_ports: Option<usize>,
    max_item_size: usize,
    _codec: PhantomData<Codec>,
}

impl<T, Codec> fmt::Debug for Receiver<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

/// Result of feeding received chunks to the deserialization thread.
enum FeedChunksResult {
    /// All chunks have been fed successfully.
    Done,
    /// The current send operation was cancelled; restart.
    Cancelled,
    /// The chmux connection failed.
    ChMux,
    /// Maximum item size was exceeded.
    MaxItemSizeExceeded,
}

/// Non-generic: feed received data chunks from chmux to the deserialization thread.
///
/// Separated from the generic `Receiver::recv` to avoid monomorphization.
#[inline(never)]
async fn feed_recv_chunks(
    receiver: &mut chmux::Receiver,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, ()>>,
    total: &mut usize,
    max_item_size: usize,
) -> FeedChunksResult {
    loop {
        let tx_permit = match tx.reserve().await {
            Ok(tx_permit) => tx_permit,
            _ => return FeedChunksResult::Done,
        };

        match receiver.recv_chunk().await {
            Ok(Some(chunk)) => {
                *total += chunk.remaining();
                if *total > max_item_size {
                    return FeedChunksResult::MaxItemSizeExceeded;
                }
                tx_permit.send(Ok(chunk));
            }
            Ok(None) => return FeedChunksResult::Done,
            Err(RecvChunkError::Cancelled) => return FeedChunksResult::Cancelled,
            Err(RecvChunkError::ChMux) => return FeedChunksResult::ChMux,
        }
    }
}

/// Non-generic: receive port requests and spawn callbacks from deserialized objects.
///
/// Separated from the generic `Receiver::recv` to avoid monomorphization.
///
/// Returns `Ok(None)` on success, `Ok(Some(received))` if the received message
/// was not port requests (requires restart), or `Err` on failure.
#[inline(never)]
async fn receive_and_connect_ports(
    receiver: &mut chmux::Receiver,
    pds: &mut PortDeserializer,
    default_max_ports: usize,
) -> Result<Option<Option<Received>>, RecvError> {
    if !pds.expected.is_empty() {
        receiver.set_max_ports(pds.expected.len() + default_max_ports);

        let requests = match receiver.recv_any().await? {
            Some(chmux::Received::Requests(requests)) => requests,
            other => return Ok(Some(other)),
        };

        for request in requests {
            if let Some((local_port, callback)) = pds.expected.remove(&request.id()) {
                exec::spawn(callback(local_port, request).in_current_span());
            }
        }

        if !pds.expected.is_empty() {
            return Err(RecvError::MissingPorts(pds.expected.keys().copied().collect()));
        }
    }

    for task in pds.tasks.drain(..) {
        exec::spawn(task.in_current_span());
    }

    Ok(None)
}

enum DataSource<T> {
    None,
    Buffered(Option<chmux::DataBuf>),
    Streamed {
        tx: Option<tokio::sync::mpsc::Sender<Result<Bytes, ()>>>,
        task: JoinHandle<Result<(T, PortDeserializer), DeserializationError>>,
        total: usize,
    },
}

impl<T, Codec> Receiver<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    /// Creates a base remote receiver from a [chmux] receiver.
    pub fn new(receiver: chmux::Receiver) -> Self {
        Self {
            receiver,
            recved: None,
            data: DataSource::None,
            item: None,
            port_deser: None,
            default_max_ports: None,
            max_item_size: DEFAULT_MAX_ITEM_SIZE,
            _codec: PhantomData,
        }
    }

    /// Receive an item from the remote endpoint.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        if self.default_max_ports.is_none() {
            self.default_max_ports = Some(self.receiver.max_ports());
        }

        'restart: loop {
            if self.item.is_none() {
                // Receive data or start streaming it.
                if let DataSource::None = &self.data {
                    if self.recved.is_none() {
                        self.recved = Some(self.receiver.recv_any().await?);
                    }

                    if let Some(Some(Received::Chunks)) = &self.recved
                        && !exec::are_threads_available().await
                    {
                        return Err(RecvError::Deserialize(DeserializationError::new(StreamingUnavailable)));
                    }

                    self.data = match self.recved.take().unwrap() {
                        Some(Received::Data(data)) => DataSource::Buffered(Some(data)),
                        Some(Received::Chunks) => {
                            // Start deserialization thread.
                            let allocator = self.receiver.port_allocator();
                            let handle_storage = self.receiver.storage();
                            let (tx, rx) = tokio::sync::mpsc::channel(BIG_DATA_CHUNK_QUEUE);
                            let task = task::spawn_blocking(move || {
                                let mut cbr = ChannelBytesReader::new(rx);

                                let pds_ref = PortDeserializer::start(allocator, handle_storage);
                                let item = <Codec as codec::Codec>::deserialize(IoReader::Channel(&mut cbr))?;
                                let pds = PortDeserializer::finish(pds_ref);

                                Ok((item, pds))
                            });
                            DataSource::Streamed { tx: Some(tx), task, total: 0 }
                        }
                        Some(Received::Requests(_)) => continue 'restart,
                        None => return Ok(None),
                    };
                }

                // Deserialize data.
                match &mut self.data {
                    DataSource::None => unreachable!(),

                    DataSource::Buffered(None) => {
                        self.data = DataSource::None;
                        continue 'restart;
                    }

                    // Deserialize data from buffer.
                    DataSource::Buffered(Some(data)) => {
                        if data.remaining() > self.max_item_size {
                            self.data = DataSource::None;
                            return Err(RecvError::MaxItemSizeExceeded);
                        }

                        let pdf_ref =
                            PortDeserializer::start(self.receiver.port_allocator(), self.receiver.storage());
                        let item_res =
                            <Codec as codec::Codec>::deserialize(IoReader::DataBuf(&mut data.reader()));
                        self.data = DataSource::None;
                        self.item = Some(item_res?);
                        self.port_deser = Some(PortDeserializer::finish(pdf_ref));
                    }

                    // Observe deserialization of streamed data.
                    DataSource::Streamed { tx, task, total } => {
                        // Feed received data chunks to deserialization thread (non-generic).
                        if let Some(tx_ref) = &tx {
                            match feed_recv_chunks(&mut self.receiver, tx_ref, total, self.max_item_size).await
                            {
                                FeedChunksResult::Done => (),
                                FeedChunksResult::Cancelled => {
                                    self.data = DataSource::None;
                                    continue 'restart;
                                }
                                FeedChunksResult::ChMux => {
                                    self.data = DataSource::None;
                                    return Err(RecvError::Receive(chmux::RecvError::ChMux));
                                }
                                FeedChunksResult::MaxItemSizeExceeded => {
                                    self.data = DataSource::None;
                                    return Err(RecvError::MaxItemSizeExceeded);
                                }
                            }
                        }
                        *tx = None;

                        // Get deserialized item.
                        match task.await {
                            Ok(Ok((item, pds))) => {
                                self.item = Some(item);
                                self.port_deser = Some(pds);
                                self.data = DataSource::None;
                            }
                            Ok(Err(err)) => {
                                self.data = DataSource::None;
                                return Err(RecvError::Deserialize(err));
                            }
                            Err(err) => {
                                self.data = DataSource::None;
                                match err.try_into_panic() {
                                    Ok(payload) => panic::resume_unwind(payload),
                                    Err(err) => {
                                        return Err(RecvError::Deserialize(DeserializationError::new(err)));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Connect received ports (non-generic).
            let pds = self.port_deser.as_mut().unwrap();
            match receive_and_connect_ports(
                &mut self.receiver,
                pds,
                self.default_max_ports.unwrap(),
            )
            .await
            {
                Ok(None) => (),
                Ok(Some(other)) => {
                    // Current send operation has been aborted and this is data from
                    // next send operation, so we restart.
                    self.recved = Some(other);
                    self.data = DataSource::None;
                    self.item = None;
                    self.port_deser = None;
                    continue 'restart;
                }
                Err(err) => return Err(err),
            }

            return Ok(Some(self.item.take().unwrap()));
        }
    }

    /// Close the channel.
    ///
    /// This stops the remote endpoint from sending more data, but allows already sent data
    /// to be received.
    pub async fn close(&mut self) {
        self.receiver.close().await
    }

    /// The maximum allowed size in bytes of an item to be received.
    ///
    /// The default value is [DEFAULT_MAX_ITEM_SIZE].
    pub fn max_item_size(&self) -> usize {
        self.max_item_size
    }

    /// Sets the maximum allowed size in bytes of an item to be received.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.max_item_size = max_item_size;
    }
}
