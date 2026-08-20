//! A channel that exchanges values of arbitrary type with a remote endpoint and
//! is established by sending exactly one half of it over an existing channel.
//!
//! **Deprecated.** Use an [MPSC channel](super::mpsc) instead.
//!
//! This channel is a more restricted version of an [MPSC channel](super::mpsc) that
//! uses fewer resources: it does not support cloning or forwarding and exactly one
//! half of it must be sent to a remote endpoint. Attempting to use the channel while
//! both halves remain local, or to transfer both halves, fails when the connection is
//! established rather than at the call site.
//!
//! # Migrating to an MPSC channel
//!
//! Replace [`lr::channel()`](channel) with [`mpsc::channel(buffer)`](super::mpsc::channel)
//! and remove the placement restrictions; an [MPSC](super::mpsc) sender is additionally
//! [clonable](Clone) and its halves may be forwarded over further connections.
//! [`lr::Sender::send`](Sender::send) takes `&mut self` and awaits until the value has
//! been handed to the connection, while [`mpsc::Sender::send`](super::mpsc::Sender::send)
//! takes `&self` and returns a [`Sending`](super::Sending) handle that reports the
//! result of the transfer.
//!

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
