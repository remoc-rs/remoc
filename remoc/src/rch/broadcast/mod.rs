//! A multi-producer, multi-consumer broadcast queue with receivers that may be located on remote endpoints.
//!
//! Each sent value is seen by all consumers.
//! The senders must be local, while the receivers can be sent to
//! remote endpoints.
//! The channel also works if every receiver stays local.
//! Forwarding is supported.
//!
//! Its API follows [tokio::sync::broadcast], with receivers that can also be
//! transferred over Remoc connections. Unlike the MPSC channel, broadcast does
//! not apply back pressure: a receiver that cannot keep up reports that it lagged
//! and resumes with a newer value.
//!
//! # Example
//!
//! The sender stays on the endpoint that creates the channel. Receivers can be
//! subscribed locally and then sent wherever the values are needed.
//!
//! In the following example the server keeps one receiver for itself and sends
//! another to the client. Both observe every value.
//!
//! ```
//! use remoc::prelude::*;
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<rch::broadcast::Receiver<u32>>) {
//!     let (bc_tx, mut local_rx) = rch::broadcast::channel::<u32, _, 16>(16);
//!
//!     // Every receiver subscribed before a value is sent will observe it.
//!     tx.send(bc_tx.subscribe(16)).await.unwrap();
//!
//!     bc_tx.send(1).unwrap();
//!     bc_tx.send(2).unwrap();
//!
//!     assert_eq!(local_rx.recv().await.unwrap(), 1);
//!     assert_eq!(local_rx.recv().await.unwrap(), 2);
//! }
//!
//! // This would be run on the client.
//! async fn client(mut rx: rch::base::Receiver<rch::broadcast::Receiver<u32>>) {
//!     let mut bc_rx = rx.recv().await.unwrap().unwrap();
//!
//!     assert_eq!(bc_rx.recv().await.unwrap(), 1);
//!     assert_eq!(bc_rx.recv().await.unwrap(), 2);
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(server, client));
//! ```

use crate::{RemoteSend, codec};

mod receiver;
mod sender;

pub use receiver::{Receiver, ReceiverStream, RecvError, StreamError, TryRecvError};
pub use sender::{Broadcasting, SendError, Sender, Sending, WeakSender};

/// Broadcast transport message.
#[derive(Clone, Debug)]
pub(crate) enum BroadcastMsg<T> {
    /// Value.
    Value(T),
    /// Lagged notification.
    Lagged,
}

crate::versioned::compact::impl_enum! {
    BroadcastMsg<T>,
    variants {
        Value(value: T) => "_0",
        Lagged => "_1",
    }
    where T: RemoteSend
}

/// Creates a bounded channel that broadcasts each value to every active receiver.
///
/// `send_buffer` is the per-receiver backlog retained before that receiver starts
/// lagging and older values are dropped for it. Additional receivers can be
/// created with [`Sender::subscribe`], each with its own independent backlog.
pub fn channel<T, Codec, const RECEIVE_BUFFER: usize>(
    send_buffer: usize,
) -> (Sender<T, Codec>, Receiver<T, Codec, RECEIVE_BUFFER>)
where
    T: RemoteSend + Clone,
    Codec: codec::Codec,
{
    let sender = Sender::new();
    let receiver = sender.subscribe(send_buffer);
    (sender, receiver)
}
