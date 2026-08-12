use bytes::{Buf, Bytes};
use futures::{
    Future,
    future::{BoxFuture, FutureExt},
};
use serde::de::DeserializeOwned;
use std::{
    any::Any,
    cell::RefCell,
    collections::HashMap,
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
    panic,
    rc::{Rc, Weak},
};
use tracing::Instrument;

use super::{super::DEFAULT_MAX_ITEM_SIZE, BIG_DATA_CHUNK_QUEUE, io::ChannelBytesReader, register_storage};
use crate::{
    chmux::{self, Received, RecvChunkError},
    codec::{self, AnySend, DeserializationError, ErasedDeserializer, StreamingUnavailable},
    exec::{
        self,
        task::{self, JoinHandle},
    },
};

/// An error that occurred during receiving from a remote endpoint.
#[derive(Clone, Debug)]
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

crate::versioned::compact::impl_enum! {
    RecvError,
    variants {
        Receive(err: chmux::RecvError) => "_0",
        Deserialize(err: DeserializationError) => "_1",
        MissingPorts(ports: Vec<u32>) => "_2",
        MaxItemSizeExceeded => "_3",
    }
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
    /// Callbacks by remote port.
    #[allow(clippy::type_complexity)]
    expected: HashMap<u32, Box<dyn FnOnce(chmux::Request) -> BoxFuture<'static, ()> + Send + 'static>>,
    tasks: Vec<BoxFuture<'static, ()>>,
}

impl PortDeserializer {
    thread_local! {
        pub(super) static INSTANCE: RefCell<Weak<RefCell<PortDeserializer>>> = const { RefCell::new(Weak::new()) };
    }

    /// Create a new port deserializer and register it as active.
    fn start() -> Rc<RefCell<PortDeserializer>> {
        let this = Rc::new(RefCell::new(Self { expected: HashMap::new(), tasks: Vec::new() }));
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
    /// Calls the specified function with the received connect request.
    pub fn accept<E, C, F>(remote_port: u32, callback: C) -> Result<(), E>
    where
        E: serde::de::Error,
        C: FnOnce(chmux::Request) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let this = Self::instance()?;
        let mut this = this.borrow_mut();
        this.expected.insert(remote_port, Box::new(|request| callback(request).boxed()));

        Ok(())
    }

    /// Spawn a task.
    pub fn spawn<E>(task: impl Future<Output = ()> + Send + 'static) -> Result<(), E>
    where
        E: serde::de::Error,
    {
        let this = Self::instance()?;
        let mut this = this.borrow_mut();

        this.tasks.push(task.boxed());
        Ok(())
    }

    /// Returns the data storage of the channel multiplexer that performs the current deserialization.    
    pub fn storage<E>() -> Result<chmux::AnyStorage, E>
    where
        E: serde::de::Error,
    {
        super::storage()
            .ok_or_else(|| serde::de::Error::custom("storage is only available during deserialization"))
    }

    /// Calls the provided function with storage of the channel multiplexer that performs the
    /// current deserialization and returns the result.
    pub fn with_storage<T, E>(f: impl FnOnce(&chmux::AnyStorage) -> T) -> Result<T, E>
    where
        E: serde::de::Error,
    {
        super::with_storage(f)
            .ok_or_else(|| serde::de::Error::custom("storage is only available during deserialization"))
    }
}

/// Receives arbitrary values from a remote endpoint.
///
/// Values may be or contain any channel from this crate.
pub struct Receiver<T, Codec = codec::Default> {
    erased: ErasedReceiver,
    _phantom: fn(T, Codec),
}

impl<T, Codec> fmt::Debug for Receiver<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Receiver").field(&self.erased).finish()
    }
}

impl<T, Codec> Deref for Receiver<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    type Target = ErasedReceiver;

    fn deref(&self) -> &Self::Target {
        &self.erased
    }
}

impl<T, Codec> DerefMut for Receiver<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.erased
    }
}

impl<T, Codec> Receiver<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    /// Creates a base remote receiver from a [chmux] receiver.
    pub fn new(receiver: chmux::Receiver) -> Self {
        Self { erased: ErasedReceiver::typed::<T, Codec>(receiver), _phantom: |_, _| () }
    }

    /// Consumes this base remote receiver and returns the underlying [chmux] receiver.
    pub fn into_inner(self) -> chmux::Receiver {
        self.erased.into_inner()
    }

    /// Returns the arbitrary data storage of the channel multiplexer.
    pub fn storage(&self) -> chmux::AnyStorage {
        self.erased.storage()
    }

    fn from_any(any_item: AnySend) -> T {
        let Ok(item) = any_item.downcast::<T>() else { panic!("mismatched type for Receiver") };
        *item
    }

    /// Receive an item from the remote endpoint.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        self.erased.recv_erased().await.map(|opt| opt.map(Self::from_any))
    }
}

/// Type-erased version of [`Receiver`].
///
/// Values may be or contain any channel from this crate.
pub struct ErasedReceiver {
    deserializer: ErasedDeserializer,
    receiver: chmux::Receiver,
    recved: Option<Option<Received>>,
    data: DataSource,
    item: Option<Box<dyn Any + Send>>,
    port_deser: Option<PortDeserializer>,
    default_max_ports: Option<usize>,
    max_item_size: usize,
}

impl fmt::Debug for ErasedReceiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ErasedReceiver")
            .field("deserializer", &self.deserializer)
            .field("receiver", &self.receiver)
            .finish()
    }
}

enum DataSource {
    None,
    Buffered(Option<chmux::DataBuf>),
    Streamed {
        tx: Option<tokio::sync::mpsc::Sender<Result<Bytes, ()>>>,
        #[allow(clippy::type_complexity)]
        task: JoinHandle<Result<(Box<dyn Any + Send>, PortDeserializer), DeserializationError>>,
        total: usize,
    },
}

impl ErasedReceiver {
    /// Creates a base remote receiver from an erased deserializer and [chmux] receiver.
    pub fn new(deserializer: ErasedDeserializer, receiver: chmux::Receiver) -> Self {
        Self {
            deserializer,
            receiver,
            recved: None,
            data: DataSource::None,
            item: None,
            port_deser: None,
            default_max_ports: None,
            max_item_size: DEFAULT_MAX_ITEM_SIZE,
        }
    }

    /// Creates a type-erased base remote receiver for the specfied type `T` and codec from a [chmux] receiver.
    pub fn typed<T, Codec>(receiver: chmux::Receiver) -> Self
    where
        T: DeserializeOwned + Send + 'static,
        Codec: codec::Codec,
    {
        Self::new(ErasedDeserializer::new::<T, Codec>(), receiver)
    }

    /// Consumes this base remote receiver and returns the underlying [chmux] receiver.
    pub fn into_inner(self) -> chmux::Receiver {
        self.receiver
    }

    /// Receive an item of type `T` used for calling [`ErasedReceiver::typed`] from the remote endpoint.
    ///
    /// The received item is returned type erased.
    pub async fn recv_erased(&mut self) -> Result<Option<AnySend>, RecvError> {
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
                            let deserializer = self.deserializer.clone();
                            let storage = self.receiver.storage();
                            let (tx, rx) = tokio::sync::mpsc::channel(BIG_DATA_CHUNK_QUEUE);

                            let task = task::spawn_blocking(move || {
                                let mut cbr = ChannelBytesReader::new(rx);

                                let _storage_ref = register_storage(storage);
                                let pds_ref = PortDeserializer::start();
                                let item = deserializer.deserialize(&mut cbr)?;
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

                        let _storage_ref = register_storage(self.receiver.storage());
                        let pdf_ref = PortDeserializer::start();
                        let item_res = self.deserializer.deserialize(&mut data.reader());
                        self.data = DataSource::None;
                        self.item = Some(item_res?);
                        self.port_deser = Some(PortDeserializer::finish(pdf_ref));
                    }

                    // Observe deserialization of streamed data.
                    DataSource::Streamed { tx, task, total } => {
                        enum FeedError {
                            RecvChunkError(RecvChunkError),
                            MaxItemSizeExceeded,
                        }

                        // Feed received data chunks to deserialization thread.
                        if let Some(tx) = &tx {
                            let res = loop {
                                let tx_permit = match tx.reserve().await {
                                    Ok(tx_permit) => tx_permit,
                                    _ => {
                                        break Ok(());
                                    }
                                };

                                match self.receiver.recv_chunk().await {
                                    Ok(Some(chunk)) => {
                                        *total += chunk.remaining();
                                        if *total > self.max_item_size {
                                            break Err(FeedError::MaxItemSizeExceeded);
                                        }

                                        tx_permit.send(Ok(chunk));
                                    }
                                    Ok(None) => break Ok(()),
                                    Err(err) => break Err(FeedError::RecvChunkError(err)),
                                }
                            };

                            match res {
                                Ok(()) => (),
                                Err(FeedError::RecvChunkError(RecvChunkError::Cancelled)) => {
                                    self.data = DataSource::None;
                                    continue 'restart;
                                }
                                Err(FeedError::RecvChunkError(RecvChunkError::ChMux)) => {
                                    self.data = DataSource::None;
                                    return Err(RecvError::Receive(chmux::RecvError::ChMux));
                                }
                                Err(FeedError::MaxItemSizeExceeded) => {
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

            // Connect received ports.
            let pds = self.port_deser.as_mut().unwrap();
            if !pds.expected.is_empty() {
                // Set port limit.
                //
                // Allow the reception of additional ports for forward compatibility,
                // i.e. our deserializer may use an older version of the struct
                // which is missing some ports that the remote endpoint sent.
                self.receiver.set_max_ports(pds.expected.len() + self.default_max_ports.unwrap());

                // Receive port requests from chmux.
                let requests = match self.receiver.recv_any().await? {
                    Some(chmux::Received::Requests(requests)) => requests,
                    other => {
                        // Current send operation has been aborted and this is data from
                        // next send operation, so we restart.
                        self.recved = Some(other);
                        self.data = DataSource::None;
                        self.item = None;
                        self.port_deser = None;
                        continue 'restart;
                    }
                };

                // Call port callbacks from received objects, ignoring superfluous requests for
                // forward compatibility.
                for request in requests {
                    if let Some(callback) = pds.expected.remove(&request.id()) {
                        exec::spawn(callback(request).in_current_span());
                    }
                }

                // But error on ports that we expect but that are missing.
                if !pds.expected.is_empty() {
                    return Err(RecvError::MissingPorts(pds.expected.keys().copied().collect()));
                }
            }

            // Spawn registered tasks.
            for task in pds.tasks.drain(..) {
                exec::spawn(task.in_current_span());
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

    /// Returns the arbitrary data storage of the channel multiplexer.
    pub fn storage(&self) -> chmux::AnyStorage {
        self.receiver.storage()
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
