use serde::{Deserialize, Serialize, de::DeserializeOwned, ser};
use std::{
    error::Error,
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use super::{
    super::{
        ConnectError,
        base::{self, PortDeserializer, PortSerializer},
    },
    Interlock, Location,
};
use crate::{
    chmux,
    codec::{self, DeserializationError},
    versioned::RemocCompact,
};

/// An error that occurred during receiving from a remote endpoint.
#[derive(Clone, Debug)]
pub enum RecvError {
    /// Receiving data over the chmux channel failed.
    Receive(chmux::RecvError),
    /// Deserialization of received data failed.
    Deserialize(DeserializationError),
    /// chmux ports required for deserialization of received channels were not received.
    MissingPorts(Vec<u32>),
    /// Connecting to remote channel failed.
    Connect(ConnectError),
    /// Maximum item size was exceeded.
    MaxItemSizeExceeded,
}

crate::versioned::impl_enum! {
    RecvError,
    versioner = crate::versioned::RemocCompact,
    variants {
        Receive(err: chmux::RecvError) => "_0",
        Deserialize(err: DeserializationError) => "_1",
        MissingPorts(ports: Vec<u32>) => "_2",
        Connect(err: ConnectError) => "_3",
        MaxItemSizeExceeded => "_4",
    }
}

impl From<base::RecvError> for RecvError {
    fn from(err: base::RecvError) -> Self {
        match err {
            base::RecvError::Receive(err) => Self::Receive(err),
            base::RecvError::Deserialize(err) => Self::Deserialize(err),
            base::RecvError::MissingPorts(ports) => Self::MissingPorts(ports),
            base::RecvError::MaxItemSizeExceeded => Self::MaxItemSizeExceeded,
        }
    }
}

impl From<ConnectError> for RecvError {
    fn from(err: ConnectError) -> Self {
        Self::Connect(err)
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
            Self::Connect(err) => write!(f, "connect error: {err}"),
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
            Self::Connect(_) => true,
            Self::Deserialize(_) | Self::MissingPorts(_) | Self::MaxItemSizeExceeded => false,
        }
    }
}

/// The receiver part of a local/remote channel.
pub struct Receiver<T, Codec = codec::Default> {
    pub(super) receiver: Option<Result<base::Receiver<T, Codec>, ConnectError>>,
    pub(super) sender_tx:
        Option<tokio::sync::mpsc::UnboundedSender<Result<base::Sender<T, Codec>, ConnectError>>>,
    pub(super) receiver_rx: tokio::sync::mpsc::UnboundedReceiver<Result<base::Receiver<T, Codec>, ConnectError>>,
    pub(super) interlock: Arc<Mutex<Interlock>>,
    pub(super) max_item_size: usize,
}

impl<T, Codec> fmt::Debug for Receiver<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

/// A raw chmux channel receiver in transport.
struct TransportedReceiver {
    /// chmux port number.
    port: u32,
    /// Maximum item size in bytes.
    max_item_size: u64,
}

const fn default_max_item_size() -> u64 {
    u64::MAX
}

crate::versioned::impl_struct! {
    TransportedReceiver,
    versioner = RemocCompact,
    fields {
        port: u32 => "_0",
        data: PhantomData<()> = PhantomData,
        codec: PhantomData<()> = PhantomData,
        #[serde(default = "default_max_item_size")]
        max_item_size: u64 => "_1",
    }
}

impl<T, Codec> Receiver<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    async fn connect(&mut self) {
        if self.receiver.is_none() {
            self.receiver = Some(self.receiver_rx.recv().await.unwrap_or(Err(ConnectError::Dropped)));
            if let Some(Ok(receiver)) = &mut self.receiver {
                receiver.set_max_item_size(self.max_item_size);
            }
        }
    }

    /// Establishes the connection and returns a reference to the remote receiver.
    async fn get(&mut self) -> Result<&mut base::Receiver<T, Codec>, ConnectError> {
        self.connect().await;
        self.receiver.as_mut().unwrap().as_mut().map_err(|err| err.clone())
    }

    /// Receive an item from the remote endpoint.
    pub async fn recv(&mut self) -> Result<Option<T>, RecvError> {
        let receiver = self.get().await?;
        let item = receiver.recv().await?;
        Ok(item)
    }

    /// Close the channel.
    ///
    /// This stops the remote endpoint from sending more items, but allows already sent items
    /// to be received.
    pub async fn close(&mut self) {
        if let Ok(receiver) = self.get().await {
            receiver.close().await;
        }
    }

    /// Maximum allowed item size in bytes.
    pub fn max_item_size(&self) -> usize {
        self.max_item_size
    }

    /// Sets the maximum allowed item size in bytes.
    ///
    /// This does not change the maximum allowed item size on the remote endpoint
    /// if the sender or receiver has already been sent to or received from the remote endpoint.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.max_item_size = max_item_size;
        if let Some(Ok(receiver)) = &mut self.receiver {
            receiver.set_max_item_size(self.max_item_size);
        }
    }
}

impl<T, Codec> Serialize for Receiver<T, Codec>
where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    /// Serializes this receiver for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let max_item_size = self.max_item_size;
        let sender_tx =
            self.sender_tx.clone().ok_or_else(|| ser::Error::custom("cannot forward received receiver"))?;

        let interlock_confirm = {
            let mut interlock = self.interlock.lock().unwrap();
            if !interlock.sender.check_local() {
                return Err(ser::Error::custom("cannot send receiver because sender has been sent"));
            }
            interlock.sender.start_send()
        };

        let port = PortSerializer::connect_port(async move |connect| {
            let _ = interlock_confirm.send(());

            match connect.await {
                Ok((raw_tx, _)) => {
                    let mut tx = base::Sender::new(raw_tx);
                    tx.set_max_item_size(max_item_size);
                    let _ = sender_tx.send(Ok(tx));
                }
                Err(err) => {
                    let _ = sender_tx.send(Err(ConnectError::Connect(err)));
                }
            }
        })?;

        TransportedReceiver { port, max_item_size: max_item_size.try_into().unwrap_or(u64::MAX) }
            .serialize(serializer)
    }
}

impl<'de, T, Codec> Deserialize<'de> for Receiver<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    /// Deserializes this receiver after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let TransportedReceiver { port, max_item_size } = TransportedReceiver::deserialize(deserializer)?;
        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);

        let (receiver_tx, receiver_rx) = tokio::sync::mpsc::unbounded_channel();
        PortDeserializer::accept(port, async move |request| match request.accept().await {
            Ok((_, raw_rx)) => {
                let mut rx = base::Receiver::new(raw_rx);
                rx.set_max_item_size(max_item_size);
                let _ = receiver_tx.send(Ok(rx));
            }
            Err(err) => {
                let _ = receiver_tx.send(Err(ConnectError::Listen(err)));
            }
        })?;

        Ok(Self {
            receiver: None,
            sender_tx: None,
            receiver_rx,
            interlock: Arc::new(Mutex::new(Interlock { sender: Location::Remote, receiver: Location::Local })),
            max_item_size,
        })
    }
}
