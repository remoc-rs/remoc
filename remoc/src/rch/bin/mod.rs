//! A channel that exchanges binary data.
//!
//! This channel transfers byte-oriented messages without applying a Serde codec.
//!
//! Both endpoints can be local, remote, or forwarded over multiple remote hops.
//! When both ends remain local, a lightweight in-process [chmux](crate::chmux) loopback
//! connection is used automatically — no serialization overhead is incurred.
//! Forwarding, i.e. passing channel ends through intermediate remote endpoints, is supported.
//!
//! If the sole use is to transfer a large binary object in one direction and the receiver
//! may not always need the data, consider using a [lazy blob](crate::robj::lazy_blob) instead.
//!
//! Use it when the payload is already encoded as bytes. For structured values, prefer
//! one of the typed channels in the parent module.
//!
//! # Usage
//!
//! Both halves hand out the underlying [chmux](crate::chmux) channel via `get`, which
//! establishes the connection on first use. Messages are then exchanged using
//! [`chmux::Sender::send`](crate::chmux::Sender::send) and
//! [`chmux::Receiver::recv`](crate::chmux::Receiver::recv).
//!
//! # Example
//!
//! In the following example the client sends binary messages to the server.
//!
//! ```
//! use bytes::{Buf, Bytes};
//! use remoc::prelude::*;
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<rch::bin::Receiver>) {
//!     let (mut bin_tx, bin_rx) = rch::bin::channel();
//!     tx.send(bin_rx).await.unwrap();
//!
//!     let bin_tx = bin_tx.get().await.unwrap();
//!     bin_tx.send(Bytes::from_static(b"hello")).await.unwrap();
//!     bin_tx.send(Bytes::from_static(b"world")).await.unwrap();
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<rch::bin::Receiver>) {
//!     let mut bin_rx = rx.recv().await.unwrap().unwrap();
//!     let bin_rx = bin_rx.get().await.unwrap();
//!
//!     let mut msgs = Vec::new();
//!     while let Some(data) = bin_rx.recv().await.unwrap() {
//!         msgs.push(data.chunk().to_vec());
//!     }
//!
//!     assert_eq!(msgs, vec![b"hello".to_vec(), b"world".to_vec()]);
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```

use std::sync::{Arc, Mutex};

mod receiver;
mod sender;

pub use receiver::Receiver;
pub use sender::Sender;

use super::interlock::{Interlock, Location};

/// Creates a new binary channel that is established by sending either the sender or receiver
/// over a remote channel.
///
/// The returned halves may remain local or be transferred independently. Sending and
/// receiving methods establish the underlying channel connection on first use.
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
