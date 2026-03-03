use bytes::{Buf, Bytes};
use futures::{
    Future,
    future::{BoxFuture, FutureExt},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    any::Any,
    cell::RefCell,
    collections::HashMap,
    error::Error,
    fmt,
    io::Read,
    marker::PhantomData,
    panic,
    rc::{Rc, Weak},
};
use tracing::Instrument;

use super::{super::DEFAULT_MAX_ITEM_SIZE, BIG_DATA_CHUNK_QUEUE, io::ChannelBytesReader};
use crate::{
    chmux::{self, AnyStorage, DataBuf, Received, RecvChunkError},
    codec::{self, DeserializationError, StreamingUnavailable},
    exec::{
        self,
        task::{self, JoinHandle},
    },
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
    inner: RecvInner,
    _t: PhantomData<T>,
    _codec: PhantomData<Codec>,
}

impl<T, Codec> fmt::Debug for Receiver<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

/// Function pointer type for type-erased deserialization.
///
/// Only this small function gets monomorphized per (T, Codec) pair.
type DeserializeFn = fn(&mut dyn Read) -> Result<Box<dyn Any + Send>, DeserializationError>;

/// Deserialization for a specific (T, Codec) pair.
fn deserialize_erased<T, Codec>(reader: &mut dyn Read) -> Result<Box<dyn Any + Send>, DeserializationError>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    let item: T = <Codec as codec::Codec>::deserialize(reader)?;
    Ok(Box::new(item))
}

/// Non-generic data source for deserialization.
enum DataSource {
    None,
    Buffered(Option<DataBuf>),
    Streamed {
        tx: Option<tokio::sync::mpsc::Sender<Result<Bytes, ()>>>,
        task: JoinHandle<Result<(Box<dyn Any + Send>, PortDeserializer), DeserializationError>>,
        total: usize,
    },
}

/// Non-generic inner state of a [`Receiver`].
struct RecvInner {
    receiver: chmux::Receiver,
    recved: Option<Option<Received>>,
    data: DataSource,
    item: Option<Box<dyn Any + Send>>,
    port_deser: Option<PortDeserializer>,
    default_max_ports: Option<usize>,
    max_item_size: usize,
    deserialize: DeserializeFn,
}

/// Non-generic: perform the entire receive operation using type-erased deserialization.
///
/// This function is shared across all (T, Codec) pairs, avoiding monomorphization
/// of the large async state machine.
#[inline(never)]
async fn recv_erased(inner: &mut RecvInner) -> Result<Option<Box<dyn Any + Send>>, RecvError> {
    if inner.default_max_ports.is_none() {
        inner.default_max_ports = Some(inner.receiver.max_ports());
    }

    'restart: loop {
        if inner.item.is_none() {
            // Receive data or start streaming it.
            if let DataSource::None = &inner.data {
                if inner.recved.is_none() {
                    inner.recved = Some(inner.receiver.recv_any().await?);
                }

                if let Some(Some(Received::Chunks)) = &inner.recved
                    && !exec::are_threads_available().await
                {
                    return Err(RecvError::Deserialize(DeserializationError::new(StreamingUnavailable)));
                }

                inner.data = match inner.recved.take().unwrap() {
                    Some(Received::Data(data)) => DataSource::Buffered(Some(data)),
                    Some(Received::Chunks) => {
                        // Start deserialization thread.
                        let allocator = inner.receiver.port_allocator();
                        let handle_storage = inner.receiver.storage();
                        let (tx, rx) = tokio::sync::mpsc::channel(BIG_DATA_CHUNK_QUEUE);
                        let deser_fn = inner.deserialize;
                        let task = task::spawn_blocking(move || {
                            let mut cbr = ChannelBytesReader::new(rx);
                            let pds_ref = PortDeserializer::start(allocator, handle_storage);
                            let item = deser_fn(&mut cbr)?;
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
            match &mut inner.data {
                DataSource::None => unreachable!(),

                DataSource::Buffered(None) => {
                    inner.data = DataSource::None;
                    continue 'restart;
                }

                // Deserialize data from buffer.
                DataSource::Buffered(Some(data)) => {
                    if data.remaining() > inner.max_item_size {
                        inner.data = DataSource::None;
                        return Err(RecvError::MaxItemSizeExceeded);
                    }

                    let pds_ref = PortDeserializer::start(
                        inner.receiver.port_allocator(),
                        inner.receiver.storage(),
                    );
                    let item = (inner.deserialize)(&mut data.reader())
                        .map_err(RecvError::Deserialize)?;
                    let pds = PortDeserializer::finish(pds_ref);
                    inner.data = DataSource::None;
                    inner.item = Some(item);
                    inner.port_deser = Some(pds);
                }

                // Observe deserialization of streamed data.
                DataSource::Streamed { tx, task, total } => {
                    // Feed received data chunks to deserialization thread.
                    let tx_taken = tx.take();
                    let total_ref = total;
                    if let Some(tx_ref) = &tx_taken {
                        loop {
                            let tx_permit = match tx_ref.reserve().await {
                                Ok(tx_permit) => tx_permit,
                                _ => break,
                            };

                            match inner.receiver.recv_chunk().await {
                                Ok(Some(chunk)) => {
                                    *total_ref += chunk.remaining();
                                    if *total_ref > inner.max_item_size {
                                        inner.data = DataSource::None;
                                        return Err(RecvError::MaxItemSizeExceeded);
                                    }
                                    tx_permit.send(Ok(chunk));
                                }
                                Ok(None) => break,
                                Err(RecvChunkError::Cancelled) => {
                                    inner.data = DataSource::None;
                                    continue 'restart;
                                }
                                Err(RecvChunkError::ChMux) => {
                                    inner.data = DataSource::None;
                                    return Err(RecvError::Receive(chmux::RecvError::ChMux));
                                }
                            }
                        }
                    }
                    drop(tx_taken);

                    // Get deserialized item.
                    match task.await {
                        Ok(Ok((item, pds))) => {
                            inner.item = Some(item);
                            inner.port_deser = Some(pds);
                            inner.data = DataSource::None;
                        }
                        Ok(Err(err)) => {
                            inner.data = DataSource::None;
                            return Err(RecvError::Deserialize(err));
                        }
                        Err(err) => {
                            inner.data = DataSource::None;
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
        let pds = inner.port_deser.as_mut().unwrap();
        if !pds.expected.is_empty() {
            inner.receiver.set_max_ports(pds.expected.len() + inner.default_max_ports.unwrap());

            let requests = match inner.receiver.recv_any().await? {
                Some(chmux::Received::Requests(requests)) => requests,
                other => {
                    // Current send operation has been aborted and this is data from
                    // next send operation, so we restart.
                    inner.recved = Some(other);
                    inner.data = DataSource::None;
                    inner.item = None;
                    inner.port_deser = None;
                    continue 'restart;
                }
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

        return Ok(Some(inner.item.take().unwrap()));
    }
}

impl<T, Codec> Receiver<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    /// Creates a base remote receiver from a [chmux] receiver.
    pub fn new(receiver: chmux::Receiver) -> Self {
        Self {
            inner: RecvInner {
                receiver,
                recved: None,
                data: DataSource::None,
                item: None,
                port_deser: None,
                default_max_ports: None,
                max_item_size: DEFAULT_MAX_ITEM_SIZE,
                deserialize: deserialize_erased::<T, Codec>,
            },
            _t: PhantomData,
            _codec: PhantomData,
        }
    }

    /// Receive an item from the remote endpoint.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        match recv_erased(&mut self.inner).await {
            Ok(Some(any)) => Ok(Some(*any.downcast::<T>().expect("item type mismatch in recv"))),
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Close the channel.
    ///
    /// This stops the remote endpoint from sending more data, but allows already sent data
    /// to be received.
    pub async fn close(&mut self) {
        self.inner.receiver.close().await
    }

    /// The maximum allowed size in bytes of an item to be received.
    ///
    /// The default value is [DEFAULT_MAX_ITEM_SIZE].
    pub fn max_item_size(&self) -> usize {
        self.inner.max_item_size
    }

    /// Sets the maximum allowed size in bytes of an item to be received.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.inner.max_item_size = max_item_size;
    }
}
