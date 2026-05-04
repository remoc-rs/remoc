//! A channel that exchanges binary data.
//!
//! Allows low-overhead exchange of binary data.
//!
//! Both endpoints can be local, remote, or forwarded over multiple remote hops.
//! When both ends remain local, a lightweight in-process [chmux](crate::chmux) loopback
//! connection is used automatically — no serialization overhead is incurred.
//! Forwarding, i.e. passing channel ends through intermediate remote endpoints, is supported.
//!
//! If the sole use is to transfer a large binary object in one direction and the receiver
//! may not always need the data, consider using a [lazy blob](crate::robj::lazy_blob) instead.
//!
//! This is a wrapper around a [chmux](crate::chmux) channel that allows a connection to be
//! established by sending the sender or receiver to a remote endpoint.

use std::sync::{Arc, Mutex};

mod receiver;
mod sender;

pub use receiver::Receiver;
pub use sender::Sender;

use super::interlock::{Interlock, Location};

/// Creates a new binary channel that is established by sending either the sender or receiver
/// over a remote channel.
pub fn channel() -> (Sender, Receiver) {
    let (sender_tx, sender_rx) = tokio::sync::mpsc::unbounded_channel();
    let (receiver_tx, receiver_rx) = tokio::sync::mpsc::unbounded_channel();
    let interlock = Arc::new(Mutex::new(Interlock::new()));
    let (local_tx, local_rx) = tokio::sync::oneshot::channel();

    let sender = Sender {
        sender: None,
        sender_rx,
        receiver_tx: Some(receiver_tx),
        interlock: interlock.clone(),
        successor_tx: std::sync::Mutex::new(None),
        local: sender::LocalConnect::Ready(local_tx),
    };
    let receiver = Receiver {
        receiver: None,
        sender_tx: Some(sender_tx),
        receiver_rx,
        interlock,
        successor_tx: std::sync::Mutex::new(None),
        local: receiver::LocalConnect::Ready(local_rx),
    };
    (sender, receiver)
}
