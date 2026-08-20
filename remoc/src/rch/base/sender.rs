use bytes::BytesMut;
use futures::{
    Future,
    future::{BoxFuture, FutureExt},
};
use serde::{Serialize, ser};
use std::{
    any::Any,
    cell::RefCell,
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
    panic,
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
};
use tracing::Instrument;
use wokio::{self, task};

use super::{
    super::{DEFAULT_MAX_ITEM_SIZE, SendErrorExt},
    BIG_DATA_CHUNK_QUEUE, BIG_DATA_LIMIT,
    io::{ChannelBytesWriter, LimitedBytesWriter},
    register_storage,
};
use crate::{
    chmux::{self, AllReceived, AnyStorage, ConnectReq},
    codec::{self, AnySend, ErasedSerializer, SerializationError, StreamingUnavailable},
};

pub use crate::chmux::Closed;

/// An error returned when a base-channel value cannot be sent.
///
/// The original value is retained in [`item`](Self::item), allowing the caller
/// to recover ownership after a failed send.
#[derive(Clone, custom_debug::Debug)]
pub struct SendError<T> {
    /// The reason the value could not be sent.
    pub kind: SendErrorKind,
    /// The value that could not be sent.
    #[debug(skip)]
    pub item: T,
}

crate::versioned::compact::impl_struct! {
    SendError<T>,
    fields {
        kind: SendErrorKind => "_0",
        item: T => "_1",
    }
    where T: crate::RemoteSend
}

impl<T: 'static> SendError<T> {
    pub(crate) fn from_any(err: SendError<Box<dyn Any + Send>>) -> Self {
        let SendError { kind, item } = err;
        let Ok(item) = item.downcast::<T>() else { panic!("mismatched type for SendError") };
        Self { kind, item: *item }
    }
}

/// The reason a base-channel send failed.
#[derive(Debug, Clone)]
pub enum SendErrorKind {
    /// The codec could not encode the value.
    Serialize(SerializationError),
    /// Transferring the encoded value failed; see [`chmux::SendError`].
    Send(chmux::SendError),
    /// The encoded value exceeds the channel's configured item-size limit.
    MaxItemSizeExceeded,
}

crate::versioned::compact::impl_enum! {
    SendErrorKind,
    variants {
        Serialize(err: SerializationError) => "_0",
        Send(err: chmux::SendError) => "_1",
        MaxItemSizeExceeded => "_2",
    }
}

impl SendErrorKind {
    /// Returns `true` if the remote receiver explicitly closed the channel.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Send(err) if err.is_closed())
    }

    /// Returns `true` if the channel can no longer transfer values.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::Send(err) if err.is_disconnected())
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

    /// Returns `true` if the remote receiver explicitly closed the channel.
    pub fn is_closed(&self) -> bool {
        self.kind.is_closed()
    }

    /// Returns `true` if the channel can no longer transfer values.
    pub fn is_disconnected(&self) -> bool {
        self.kind.is_disconnected()
    }

    /// Returns whether the error was caused by the value being sent.
    pub fn is_item_specific(&self) -> bool {
        self.kind.is_item_specific()
    }

    /// Discards the unsent value and returns the same error with `()` in its place.
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
        write!(f, "{}", self.kind)
    }
}

impl<T> Error for SendError<T> where T: fmt::Debug {}

/// Gathers ports to send to the remote endpoint during object serialization.
pub struct PortSerializer {
    allocator: chmux::PortAllocator,
    #[allow(clippy::type_complexity)]
    requests:
        Vec<(chmux::ConnectReq, Box<dyn FnOnce(chmux::Connect) -> BoxFuture<'static, ()> + Send + 'static>)>,
    tasks: Vec<BoxFuture<'static, ()>>,
}

impl PortSerializer {
    thread_local! {
        static INSTANCE: RefCell<Weak<RefCell<PortSerializer>>> = const { RefCell::new(Weak::new()) };
    }

    /// Create a new port serializer and register it as active.
    fn start(allocator: chmux::PortAllocator) -> Rc<RefCell<Self>> {
        let this = Rc::new(RefCell::new(Self { allocator, requests: Vec::new(), tasks: Vec::new() }));
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

    /// Open a chmux port to the remote endpoint using the specified connection request.
    ///
    /// Calls the specified function with the connect object.
    pub fn connect<E, C, F>(connect_req: ConnectReq, callback: C) -> Result<(), E>
    where
        E: serde::ser::Error,
        C: FnOnce(chmux::Connect) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let this = Self::instance()?;
        let mut this = this.borrow_mut();

        this.requests.push((connect_req, Box::new(|connect| callback(connect).boxed())));
        Ok(())
    }

    /// Allocates a connection request.
    pub fn connect_req<E>() -> Result<ConnectReq, E>
    where
        E: serde::ser::Error,
    {
        let this = Self::instance()?;
        let this = this.borrow_mut();

        this.allocator.connect_req().map_err(ser::Error::custom)
    }

    /// Open a chmux port to the remote endpoint using the specified connection request.
    ///
    /// This function waits until a local and remote port become available. When
    /// possible, it provisionally opens the port so data can be sent while the
    /// remote listener's acceptance is still pending; see
    /// [`ConnectReq::try_pre_connect`].
    ///
    /// Returns the local port number and calls the specified function with the connect object.    
    pub fn connect_port<E, C, F>(callback: C) -> Result<u32, E>
    where
        E: serde::ser::Error,
        C: FnOnce(chmux::Connect) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let req = Self::connect_req()?.try_pre_connect();
        let port = req.port();

        Self::connect(req, callback)?;

        Ok(port)
    }

    /// Spawn a task.
    pub fn spawn<E>(task: impl Future<Output = ()> + Send + 'static) -> Result<(), E>
    where
        E: serde::ser::Error,
    {
        let this = Self::instance()?;
        let mut this = this.borrow_mut();

        this.tasks.push(task.boxed());
        Ok(())
    }

    /// Returns the data storage of the channel multiplexer that performs the current serialization.    
    pub fn storage<E>() -> Result<chmux::AnyStorage, E>
    where
        E: serde::ser::Error,
    {
        super::storage().ok_or_else(|| ser::Error::custom("storage is only available during serialization"))
    }

    /// Calls the provided function with storage of the channel multiplexer that performs the
    /// current serialization and returns the result.
    pub fn with_storage<T, E>(f: impl FnOnce(&chmux::AnyStorage) -> T) -> Result<T, E>
    where
        E: serde::ser::Error,
    {
        super::with_storage(f).ok_or_else(|| ser::Error::custom("storage is only available during serialization"))
    }
}

/// The sending half of a connection's base channel.
///
/// A base channel is created together with a Remoc [`Connect`](crate::Connect)
/// future and provides the first typed channel between two endpoints. Values may
/// contain other Remoc channel halves or remote objects, which establishes those
/// objects on the receiving endpoint.
///
/// The base sender is not cloneable. To add independently owned producers, send
/// an [`mpsc::Sender`](crate::rch::mpsc::Sender) over it.
pub struct Sender<T, Codec = codec::Default> {
    erased: ErasedSender,
    _phantom: fn(T, Codec),
}

impl<T, Codec> fmt::Debug for Sender<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Sender").field(&self.erased).finish()
    }
}

impl<T, Codec> Deref for Sender<T, Codec>
where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    type Target = ErasedSender;

    fn deref(&self) -> &Self::Target {
        &self.erased
    }
}

impl<T, Codec> DerefMut for Sender<T, Codec>
where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.erased
    }
}

impl<T, Codec> Sender<T, Codec>
where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    /// Creates a base remote sender from a [chmux] sender.
    pub fn new(sender: chmux::Sender) -> Self {
        Self { erased: ErasedSender::typed::<T, Codec>(sender), _phantom: |_, _| () }
    }

    /// Consumes this base remote sender and returns the underlying [chmux] sender.
    pub fn into_inner(self) -> chmux::Sender {
        self.erased.into_inner()
    }

    /// Returns the arbitrary data storage of the channel multiplexer.
    pub fn storage(&self) -> chmux::AnyStorage {
        self.erased.storage()
    }

    /// Sends one value to the remote endpoint.
    ///
    /// Any Remoc channels or remote objects contained in the value are connected
    /// as part of the send. A successful return means the value was handed to the
    /// connection; it does not mean the remote application has processed it.
    ///
    /// If sending fails, the returned [`SendError`] contains the original value.
    pub async fn send(&mut self, item: T) -> Result<(), SendError<T>> {
        self.erased.send_erased(Box::new(item)).await.map_err(SendError::<T>::from_any)
    }
}

/// Typed-erased version of [`Sender`].
///
/// Values may be or contain any channel from this crate.
pub struct ErasedSender {
    serializer: ErasedSerializer,
    sender: chmux::Sender,
    big_data: i8,
    /// Encoded size of the last buffered item, used to size the next buffer.
    last_size: usize,
    max_item_size: usize,
}

impl fmt::Debug for ErasedSender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ErasedSender")
            .field("serializer", &self.serializer)
            .field("sender", &self.sender)
            .finish()
    }
}

impl ErasedSender {
    /// Creates a base remote sender from an erased serializer a [chmux] sender.
    pub fn new(serializer: ErasedSerializer, sender: chmux::Sender) -> Self {
        Self { serializer, sender, big_data: 0, last_size: 0, max_item_size: DEFAULT_MAX_ITEM_SIZE }
    }

    /// Creates a type-erased base remote sender for the specified type `T` and codec from a [chmux] sender.
    pub fn typed<T, Codec>(sender: chmux::Sender) -> Self
    where
        T: Serialize + Send + 'static,
        Codec: codec::Codec,
    {
        Self::new(ErasedSerializer::new::<T, Codec>(), sender)
    }

    /// Consumes this base remote sender and returns the underlying [chmux] sender.
    pub fn into_inner(self) -> chmux::Sender {
        self.sender
    }

    fn serialize_buffered(
        serializer: &ErasedSerializer, allocator: chmux::PortAllocator, storage: AnyStorage, item: &dyn Any,
        limit: usize, capacity: usize,
    ) -> Result<Option<(BytesMut, PortSerializer)>, SerializationError> {
        let mut lw = LimitedBytesWriter::new(limit, capacity);
        let _storage_ref = register_storage(storage);
        let ps_ref = PortSerializer::start(allocator);

        match serializer.serialize(&mut lw, item, capacity.clamp(512, 8_192)) {
            _ if lw.overflow() => return Ok(None),
            Ok(()) => (),
            Err(err) => return Err(err),
        };

        let ps = PortSerializer::finish(ps_ref);
        Ok(Some((lw.into_inner().unwrap(), ps)))
    }

    async fn serialize_streaming(
        serializer: ErasedSerializer, allocator: chmux::PortAllocator, storage: AnyStorage, item: AnySend,
        tx: tokio::sync::mpsc::Sender<BytesMut>, chunk_size: usize,
    ) -> Result<(AnySend, PortSerializer, usize), (SerializationError, AnySend)> {
        if !wokio::task::has_threads().await {
            return Err((SerializationError::new(StreamingUnavailable), item));
        }

        let mut cbw = ChannelBytesWriter::new(tx);

        let item_arc = Arc::new(Mutex::new(item));
        let item_arc_task = item_arc.clone();

        let result = task::spawn_blocking(move || {
            let _storage_ref = register_storage(storage);
            let ps_ref = PortSerializer::start(allocator);

            let item = item_arc_task.lock().unwrap();
            serializer.serialize(&mut cbw, &**item, chunk_size)?;

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

    /// Sends a type-erased item over the channel.
    ///
    /// The item may contain ports that will be serialized and connected as well.
    ///
    /// The underlying type of `item` must be `T` as used for calling `AnySender::new::<T, Codec>`.
    ///
    /// # Panics
    /// Panics if the underlying type of `item` does not match `T`.
    pub async fn send_erased(&mut self, item: AnySend) -> Result<(), SendError<AnySend>> {
        self.serializer.check_type(&*item);

        // Determine if it is worthy to try buffered serialization.
        let data_ps = if self.big_data <= 0 {
            // Try buffered serialization.
            match Self::serialize_buffered(
                &self.serializer,
                self.sender.port_allocator(),
                self.sender.storage(),
                &*item,
                self.sender.max_data_size(),
                self.last_size * 110 / 100,
            ) {
                Ok(Some(v)) => {
                    self.big_data = (self.big_data - 1).max(-BIG_DATA_LIMIT);
                    self.last_size = v.0.len();
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
                let (tx, mut rx) = tokio::sync::mpsc::channel(BIG_DATA_CHUNK_QUEUE);
                let ser_task = Self::serialize_streaming(
                    self.serializer.clone(),
                    self.sender.port_allocator(),
                    self.sender.storage(),
                    item,
                    tx,
                    self.sender.chunk_size(),
                );

                enum SendTaskError {
                    SendError(chmux::SendError),
                    MaxItemSizeExceeded,
                }

                let mut sc = self.sender.send_chunks();
                let max_item_size = self.max_item_size;
                let send_task = async move {
                    let mut total = 0;
                    while let Some(chunk) = rx.recv().await {
                        total += chunk.len();
                        if total > max_item_size {
                            return Err(SendTaskError::MaxItemSizeExceeded);
                        }

                        sc = sc.send(chunk.freeze()).await.map_err(SendTaskError::SendError)?;
                    }
                    Ok(sc)
                };

                match tokio::join!(ser_task, send_task) {
                    (Ok((item, ps, size)), Ok(sc)) => {
                        if let Err(err) = sc.finish().await {
                            return Err(SendError::new(SendErrorKind::Send(err), item));
                        }

                        if size <= self.sender.max_data_size() {
                            self.big_data = (self.big_data - 1).max(-BIG_DATA_LIMIT);
                        }

                        (item, ps)
                    }
                    (Ok((item, _, _)), Err(err)) | (Err((_, item)), Err(err)) => {
                        // When sending fails, the serialization task will either finish
                        // or fail due to rx being dropped.
                        let kind = match err {
                            SendTaskError::SendError(err) => SendErrorKind::Send(err),
                            SendTaskError::MaxItemSizeExceeded => SendErrorKind::MaxItemSizeExceeded,
                        };
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

        let PortSerializer { requests, tasks, .. } = ps;

        // Extract ports and connect callbacks.
        let (ports, callbacks): (Vec<_>, Vec<_>) = requests.into_iter().unzip();

        // Request connecting chmux ports.
        let connects = if ports.is_empty() {
            Vec::new()
        } else {
            match self.sender.connect(ports).await {
                Ok(connects) => connects,
                Err(err) => return Err(SendError::new(SendErrorKind::Send(err), item)),
            }
        };

        // Ensure that item is dropped before calling connection callbacks.
        drop(item);

        // Call callbacks of BaseSenders and BaseReceivers with obtained
        // chmux connect requests.
        //
        // We have to spawn a task for this to ensure cancellation safety.
        for (callback, connect) in callbacks.into_iter().zip(connects) {
            wokio::spawn(callback(connect).in_current_span());
        }

        // Spawn registered tasks.
        for task in tasks {
            wokio::spawn(task.in_current_span());
        }

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

    /// Returns the arbitrary data storage of the channel multiplexer.
    pub fn storage(&self) -> chmux::AnyStorage {
        self.sender.storage()
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

    /// Returns whether the remote endpoint supports calling [all_received](Self::all_received).
    ///
    /// See [chmux::Sender::is_all_received_supported](crate::chmux::Sender::is_all_received_supported) for details.
    pub fn is_all_received_supported(&self) -> bool {
        self.sender.is_all_received_supported()
    }

    /// Returns a future that resolves once the remote channel layer has received
    /// all encoded items sent on this channel up to this call.
    ///
    /// This does not mean the remote application has decoded or processed those
    /// items yet. See
    /// [chmux::Sender::all_received](crate::chmux::Sender::all_received) for details.
    pub fn all_received(&self) -> AllReceived {
        self.sender.all_received()
    }

    /// Returns whether this channel may use global credits for sending items.
    ///
    /// See [chmux::Sender::are_global_credits_allowed](crate::chmux::Sender::are_global_credits_allowed) for details.
    pub fn are_global_credits_allowed(&self) -> bool {
        self.sender.are_global_credits_allowed()
    }

    /// Sets whether this channel may use global credits for sending items.
    ///
    /// See [chmux::Sender::set_global_credits_allowed](crate::chmux::Sender::set_global_credits_allowed) for details.
    pub fn set_global_credits_allowed(&mut self, allowed: bool) {
        self.sender.set_global_credits_allowed(allowed);
    }
}
