//! A channel that exchanges values of arbitrary type with a remote endpoint
//! and is primarily used as the initial channel after [establishing a connection](crate::Connect)
//! with a remote endpoint.
//!
//! Each value is serialized into binary format before sending and deserialized
//! after it has been received.
//!
//! The sender and receiver of this channel cannot be sent to a remote endpoint.
//! However, you can send (objects containing) senders and receivers of other
//! channel types (for example [mpsc](super::mpsc), [oneshot](super::oneshot) or
//! [watch](super::watch)) via this channel to a remote endpoint.
//!
//! # Example
//!
//! In the following example the client sends a number over a base channel to the server.
//! The server converts it into a string and sends it back over another base channel.
//! The base channels have been obtained using a [connect function](crate::Connect).
//!
//! ```
//! use remoc::prelude::*;
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<u16>, mut rx: rch::base::Receiver<String>) {
//!     tx.send(1).await.unwrap();
//!     assert_eq!(rx.recv().await.unwrap(), Some("1".to_string()));
//!
//!     tx.send(2).await.unwrap();
//!     assert_eq!(rx.recv().await.unwrap(), Some("2".to_string()));
//!
//!     tx.send(3).await.unwrap();
//!     assert_eq!(rx.recv().await.unwrap(), Some("3".to_string()));
//! }
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<String>, mut rx: rch::base::Receiver<u16>) {
//!     while let Some(number) = rx.recv().await.unwrap() {
//!         tx.send(number.to_string()).await.unwrap();
//!     }
//! }
//! # tokio_test::block_on(remoc::doctest::client_server_bidir(client, server));
//! ```
//!

use serde::{Serialize, de::DeserializeOwned};
use std::{
    cell::RefCell,
    error::Error,
    fmt,
    rc::{Rc, Weak},
};

mod io;
mod receiver;
mod sender;
mod storage_ref;

pub use receiver::{ErasedReceiver, PortDeserializer, Receiver, RecvError};
pub use sender::{Closed, ErasedSender, PortSerializer, SendError, SendErrorKind, Sender};
pub use storage_ref::{StorageRef, StorageRefHandle, storage_ref};

use crate::{
    RemoteSend,
    chmux::{self, AnyStorage},
    codec,
};

/// Chunk queue length for big data (de-)serialization.
const BIG_DATA_CHUNK_QUEUE: usize = 32;

/// Limit for counting big data instances.
const BIG_DATA_LIMIT: i8 = 16;

/// Creating the remote channel failed.
#[derive(Debug, Clone)]
pub enum ConnectError {
    /// The connect request failed.
    Connect(chmux::ConnectError),
    /// Listening for the remote connect request failed.
    Listen(chmux::ListenerError),
    /// The remote endpoint did not send a connect request.
    NoConnectRequest,
}

crate::versioned::impl_enum! {
    ConnectError,
    versioner = crate::versioned::RemocCompact,
    variants {
        Connect(err: chmux::ConnectError) => "_0",
        Listen(err: chmux::ListenerError) => "_1",
        NoConnectRequest => "_2",
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConnectError::Connect(err) => write!(f, "connect error: {err}"),
            ConnectError::Listen(err) => write!(f, "listen error: {err}"),
            ConnectError::NoConnectRequest => write!(f, "no connect request received"),
        }
    }
}

impl Error for ConnectError {}

impl From<chmux::ConnectError> for ConnectError {
    fn from(err: chmux::ConnectError) -> Self {
        Self::Connect(err)
    }
}

impl From<chmux::ListenerError> for ConnectError {
    fn from(err: chmux::ListenerError) -> Self {
        Self::Listen(err)
    }
}

/// Create a remote channel over an existing [chmux] connection.
///
/// This will send a connect request over the client and accept
/// one connection request from the listener.
///
/// Other connections may coexist on the chmux connection.
pub async fn connect<Tx, Rx, Codec>(
    client: &chmux::Client, listener: &mut chmux::Listener,
) -> Result<(Sender<Tx, Codec>, Receiver<Rx, Codec>), ConnectError>
where
    Tx: RemoteSend,
    Rx: RemoteSend,
    Codec: codec::Codec,
{
    async fn connect_raw(
        client: &chmux::Client, listener: &mut chmux::Listener,
    ) -> Result<(chmux::Sender, chmux::Receiver), ConnectError> {
        let (client_sr, listener_sr) = tokio::join!(client.connect_port(), listener.accept());
        let (raw_sender, _) = client_sr?;
        let (_, raw_receiver) = listener_sr?.ok_or(ConnectError::NoConnectRequest)?;
        Ok((raw_sender, raw_receiver))
    }

    let (raw_sender, raw_receiver) = connect_raw(client, listener).await?;
    Ok((Sender::new(raw_sender), Receiver::new(raw_receiver)))
}

/// Extensions for base channels.
pub trait BaseExt<T, Codec> {
    /// Sets the maximum item size for the channel.
    fn with_max_item_size(self, max_item_size: usize) -> (Sender<T, Codec>, Receiver<T, Codec>);
}

impl<T, Codec> BaseExt<T, Codec> for (Sender<T, Codec>, Receiver<T, Codec>)
where
    T: Serialize + DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    fn with_max_item_size(self, max_item_size: usize) -> (Sender<T, Codec>, Receiver<T, Codec>) {
        let (mut tx, mut rx) = self;
        tx.set_max_item_size(max_item_size);
        rx.set_max_item_size(max_item_size);
        (tx, rx)
    }
}

thread_local! {
    /// Storage of channel multiplexer for currently active serialization or deserialization.
    static ANY_STORAGE: RefCell<Weak<chmux::AnyStorage>> = const { RefCell::new(Weak::new()) };
}

/// Register storage as active.
///
/// Returned reference must be held as long as storage should stay registered.
#[must_use]
fn register_storage(storage: AnyStorage) -> Rc<AnyStorage> {
    let storage = Rc::new(storage);
    let weak = Rc::downgrade(&storage);
    ANY_STORAGE.with(move |i| i.replace(weak));
    storage
}

/// Returns the data storage of the channel multiplexer that performs the
/// current serialization or deserialization.
///
/// `None` is returned if currently no serialization or deserialization takes place.
pub fn storage() -> Option<chmux::AnyStorage> {
    ANY_STORAGE.with(|i| i.borrow().upgrade().map(|s| (*s).clone()))
}

/// Calls the provided function with storage of the channel multiplexer that performs the
/// current serialization or deserialization and returns the result.
///
/// `None` is returned if currently no serialization or deserialization takes place.
pub fn with_storage<T>(f: impl FnOnce(&chmux::AnyStorage) -> T) -> Option<T> {
    ANY_STORAGE.with(|i| i.borrow().upgrade().map(|s| f(&s)))
}
