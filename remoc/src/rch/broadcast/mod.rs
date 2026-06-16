//! A multi-producer, multi-consumer broadcast queue with receivers that may be located on remote endpoints.
//!
//! Each sent value is seen by all consumers.
//! The senders must be local, while the receivers can be sent to
//! remote endpoints.
//! Forwarding is supported.
//!
//! This has similar functionality as [tokio::sync::broadcast] with the additional
//! ability to work over remote connections.

use serde::{Deserialize, Serialize};

use crate::{RemoteSend, codec};

mod receiver;
mod sender;

pub use receiver::{Receiver, ReceiverStream, RecvError, StreamError, TryRecvError};
pub use sender::{Broadcasting, SendError, Sender, Sending, WeakSender};

/// Broadcast transport message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum BroadcastMsg<T> {
    /// Value.
    Value(T),
    /// Lagged notification.
    Lagged,
}

/// Create a bounded, multi-producer, multi-consumer channel where each sent value is broadcasted to all active receivers.
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
