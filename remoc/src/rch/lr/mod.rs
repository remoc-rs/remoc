//! A channel that exchanges values of arbitrary type with a remote endpoint and
//! is established by sending exactly one half of it over an existing channel.
//!
//! This is a lighter and more restricted version of an [MPSC channel](super::mpsc).
//! Forwarding is not supported and exactly one half of the channel must be sent to
//! a remote endpoint. Prefer [MPSC](super::mpsc) unless the lower resource usage is
//! important and these placement restrictions fit the application.
//!
//! Attempting to use the channel while both halves remain local, or to transfer both
//! halves, fails when the connection is established.
//!
//! # Example
//!
//! In the following example the client creates a channel and sends its sender to the
//! server, which uses it to send values back.
//!
//! ```
//! use remoc::prelude::*;
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<rch::lr::Sender<u32>>) {
//!     let (lr_tx, mut lr_rx) = rch::lr::channel();
//!
//!     // Exactly one half must go to the remote endpoint; the receiver stays here.
//!     tx.send(lr_tx).await.unwrap();
//!
//!     assert_eq!(lr_rx.recv().await.unwrap(), Some(1));
//!     assert_eq!(lr_rx.recv().await.unwrap(), Some(2));
//!     assert_eq!(lr_rx.recv().await.unwrap(), None);
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<rch::lr::Sender<u32>>) {
//!     let mut lr_tx = rx.recv().await.unwrap().unwrap();
//!     lr_tx.send(1).await.unwrap();
//!     lr_tx.send(2).await.unwrap();
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```

use std::sync::{Arc, Mutex};

mod receiver;
mod sender;

use super::{
    DEFAULT_MAX_ITEM_SIZE,
    interlock::{Interlock, Location},
};
pub use receiver::{Receiver, RecvError};
pub use sender::{SendError, SendErrorKind, Sender};

/// Creates a new local/remote channel that is established by sending either the sender or receiver
/// over a remote channel.
///
/// Exactly one returned half must be transferred to a remote endpoint before values
/// can be exchanged.
pub fn channel<T, Codec>() -> (Sender<T, Codec>, Receiver<T, Codec>) {
    let (sender_tx, sender_rx) = tokio::sync::mpsc::unbounded_channel();
    let (receiver_tx, receiver_rx) = tokio::sync::mpsc::unbounded_channel();
    let interlock = Arc::new(Mutex::new(Interlock { sender: Location::Local, receiver: Location::Local }));

    let sender = Sender {
        sender: None,
        sender_rx,
        receiver_tx: Some(receiver_tx),
        interlock: interlock.clone(),
        max_item_size: DEFAULT_MAX_ITEM_SIZE,
    };
    let receiver = Receiver {
        receiver: None,
        sender_tx: Some(sender_tx),
        receiver_rx,
        interlock,
        max_item_size: DEFAULT_MAX_ITEM_SIZE,
    };
    (sender, receiver)
}
