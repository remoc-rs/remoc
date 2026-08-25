use futures::FutureExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned, ser};
use std::{
    error::Error,
    fmt,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use super::{
    super::{
        ConnectError, SendErrorExt,
        base::{self, PortDeserializer, PortSerializer},
    },
    Interlock, Location,
};
use crate::{
    chmux,
    codec::{self, SerializationError},
};

pub use super::super::base::Closed;

/// An error returned when a value cannot be sent.
///
/// The original value is retained in [`item`](Self::item), allowing the caller
/// to retry it elsewhere or recover ownership.
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

/// The reason a local/remote channel send failed.
#[derive(Debug, Clone)]
pub enum SendErrorKind {
    /// The codec could not encode the value.
    Serialize(SerializationError),
    /// Transferring the encoded value failed; see [`chmux::SendError`].
    Send(chmux::SendError),
    /// Establishing the remote half of this channel failed; see [`ConnectError`].
    Connect(ConnectError),
    /// The encoded value exceeds the channel's configured item-size limit.
    MaxItemSizeExceeded,
}

crate::versioned::compact::impl_enum! {
    SendErrorKind,
    variants {
        Serialize(err: SerializationError) => "_0",
        Send(err: chmux::SendError) => "_1",
        Connect(err: ConnectError) => "_2",
        MaxItemSizeExceeded => "_3",
    }
}

impl<T> SendError<T> {
    pub(crate) fn new(kind: SendErrorKind, item: T) -> Self {
        Self { kind, item }
    }

    /// Returns `true` if the remote receiver explicitly closed the channel.
    pub fn is_closed(&self) -> bool {
        matches!(&self.kind, SendErrorKind::Send(err) if err.is_closed())
    }

    /// Returns `true` if the channel can no longer transfer values.
    pub fn is_disconnected(&self) -> bool {
        matches!(&self.kind, SendErrorKind::Send(_) | SendErrorKind::Connect(_))
    }

    /// Returns whether the failure was caused by the value being sent.
    pub fn is_item_specific(&self) -> bool {
        matches!(&self.kind, SendErrorKind::Serialize(_) | SendErrorKind::MaxItemSizeExceeded)
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
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::MaxItemSizeExceeded => write!(f, "maximum item size exceeded"),
        }
    }
}

impl From<base::SendErrorKind> for SendErrorKind {
    fn from(err: base::SendErrorKind) -> Self {
        match err {
            base::SendErrorKind::Serialize(err) => Self::Serialize(err),
            base::SendErrorKind::Send(err) => Self::Send(err),
            base::SendErrorKind::MaxItemSizeExceeded => Self::MaxItemSizeExceeded,
        }
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}

impl<T> From<base::SendError<T>> for SendError<T> {
    fn from(err: base::SendError<T>) -> Self {
        Self { kind: err.kind.into(), item: err.item }
    }
}

impl<T> Error for SendError<T> where T: fmt::Debug {}

/// The sending half of a local/remote channel.
///
/// Exactly one half of the channel must be transferred to a remote endpoint
/// before this sender can be used. Unlike an [`mpsc`](crate::rch::mpsc) sender,
/// this type is not cloneable or forwardable.
pub struct Sender<T, Codec = codec::Default> {
    pub(super) sender: Option<Result<base::Sender<T, Codec>, ConnectError>>,
    pub(super) sender_rx: tokio::sync::mpsc::UnboundedReceiver<Result<base::Sender<T, Codec>, ConnectError>>,
    pub(super) receiver_tx:
        Option<tokio::sync::mpsc::UnboundedSender<Result<base::Receiver<T, Codec>, ConnectError>>>,
    pub(super) interlock: Arc<Mutex<Interlock>>,
    pub(super) max_item_size: usize,
}

impl<T, Codec> fmt::Debug for Sender<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

/// A local/remote channel sender in transport.
struct TransportedSender {
    /// chmux port number.
    port: u32,
    /// Maximum item size in bytes.
    max_item_size: u64,
}

crate::versioned::compact::impl_struct! {
    TransportedSender,
    fields {
        port: u32 => "_0",
        data: PhantomData<()> = PhantomData,
        codec: PhantomData<()> = PhantomData,
        #[serde(default = "crate::rch::default_max_item_size")]
        #[serde(skip_serializing_if = "crate::rch::is_default_max_item_size")]
        max_item_size: u64 => "_1",
    }
}

impl<T, Codec> Sender<T, Codec>
where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    /// Establishes the connection and returns a reference to the remote sender.
    async fn get(&mut self) -> Result<&mut base::Sender<T, Codec>, ConnectError> {
        if self.sender.is_none() {
            self.sender = Some(self.sender_rx.recv().await.unwrap_or(Err(ConnectError::Dropped)));
            if let Some(Ok(sender)) = &mut self.sender {
                sender.set_max_item_size(self.max_item_size);
            }
        }

        self.sender.as_mut().unwrap().as_mut().map_err(|err| err.clone())
    }

    /// Sends one value, waiting until it has been handed to the connection.
    ///
    /// A successful return does not mean that the remote application has processed
    /// the value. On failure, the returned [`SendError`] contains the original
    /// value.
    pub async fn send(&mut self, item: T) -> Result<(), SendError<T>> {
        match self.get().await {
            Ok(sender) => Ok(sender.send(item).await?),
            Err(err) => Err(SendError::new(SendErrorKind::Connect(err), item)),
        }
    }

    /// Returns whether the remote receiver has closed the channel.
    ///
    /// Establishes the channel connection on first use.
    pub async fn is_closed(&mut self) -> Result<bool, ConnectError> {
        Ok(self.get().await?.is_closed())
    }

    /// Returns a future that completes when the remote receiver closes.
    ///
    /// Establishes the channel connection on first use. Await the returned
    /// [`Closed`] future to wait for closure.
    pub async fn closed(&mut self) -> Result<Closed, ConnectError> {
        Ok(self.get().await?.closed())
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
        if let Some(Ok(sender)) = &mut self.sender {
            sender.set_max_item_size(self.max_item_size);
        }
    }
}

impl<T, Codec> Serialize for Sender<T, Codec>
where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    /// Serializes this sender for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let max_item_size = self.max_item_size;
        let receiver_tx =
            self.receiver_tx.clone().ok_or_else(|| ser::Error::custom("cannot forward received sender"))?;

        let interlock_confirm = {
            let mut interlock = self.interlock.lock().unwrap();
            if !interlock.receiver.check_local() {
                return Err(ser::Error::custom("cannot send sender because receiver has been sent"));
            }
            interlock.receiver.start_send()
        };

        let port = PortSerializer::connect_port(async move |connect| {
            let _ = interlock_confirm.send(());

            match connect.await {
                Ok((_, raw_rx)) => {
                    let mut rx = base::Receiver::new(raw_rx);
                    rx.set_max_item_size(max_item_size);
                    let _ = receiver_tx.send(Ok(rx));
                }
                Err(err) => {
                    let _ = receiver_tx.send(Err(ConnectError::Connect(err)));
                }
            }
        })?;

        TransportedSender { port, max_item_size: max_item_size.try_into().unwrap_or(u64::MAX) }
            .serialize(serializer)
    }
}

impl<'de, T, Codec> Deserialize<'de> for Sender<T, Codec>
where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    /// Deserializes this sender after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let TransportedSender { port, max_item_size } = TransportedSender::deserialize(deserializer)?;
        let max_item_size = usize::try_from(max_item_size).unwrap_or(usize::MAX);

        let (sender_tx, sender_rx) = tokio::sync::mpsc::unbounded_channel();
        PortDeserializer::accept(port, move |request| {
            async move {
                match request.accept().await {
                    Ok((raw_tx, _)) => {
                        let mut tx = base::Sender::new(raw_tx);
                        tx.set_max_item_size(max_item_size);
                        let _ = sender_tx.send(Ok(tx));
                    }
                    Err(err) => {
                        let _ = sender_tx.send(Err(ConnectError::Listen(err)));
                    }
                }
            }
            .boxed()
        })?;

        Ok(Self {
            sender: None,
            sender_rx,
            receiver_tx: None,
            interlock: Arc::new(Mutex::new(Interlock { sender: Location::Local, receiver: Location::Remote })),
            max_item_size,
        })
    }
}
