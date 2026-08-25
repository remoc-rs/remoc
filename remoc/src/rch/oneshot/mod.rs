//! A one-shot channel is used for sending a single message between asynchronous, remote tasks.
//!
//! The sender and receiver can both be sent to remote endpoints.
//! The channel also works if both halves are local.
//! Forwarding over multiple connections is supported.
//!
//! Its API follows [tokio::sync::oneshot], with channel halves that can also be
//! transferred over Remoc connections. The [`Sender`] is consumed when sending,
//! while the [`Receiver`] is a future that resolves to the transmitted value.
//!
//! # Example
//!
//! In the following example the client sends a number and a oneshot channel sender to the server.
//! The server squares the received number and sends the result back over the oneshot channel.
//!
//! ```
//! use remoc::prelude::*;
//!
//! #[derive(Debug, serde::Serialize, serde::Deserialize)]
//! struct SquareReq {
//!     number: u32,
//!     result_tx: rch::oneshot::Sender<u32>,
//! }
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<SquareReq>) {
//!     let (result_tx, result_rx) = rch::oneshot::channel();
//!     tx.send(SquareReq { number: 4, result_tx }).await.unwrap();
//!     let result = result_rx.await.unwrap();
//!     assert_eq!(result, 16);
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<SquareReq>) {
//!     while let Some(req) = rx.recv().await.unwrap() {
//!         req.result_tx.send(req.number * req.number).unwrap();
//!     }
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```
//!

use futures::FutureExt;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
};

use super::mpsc;
use crate::{RemoteSend, codec};

mod receiver;
mod sender;

pub use receiver::{Receiver, RecvError, TryRecvError};
pub use sender::{SendError, Sender};

/// Creates a one-shot channel for sending one value.
///
/// The sender and receiver may be sent to remote endpoints via channels.
pub fn channel<T, Codec>() -> (Sender<T, Codec>, Receiver<T, Codec>)
where
    T: Serialize + DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    let (tx, rx) = mpsc::with_local_buffer(1);
    let tx = tx.set_buffer();
    let rx = rx.set_buffer();
    (Sender(tx), Receiver(rx))
}

/// Makes a local oneshot receiver forwardable to remote endpoints.
///
/// The returned [`Forwarding`] future resolves once forwarding has completed or an error occurs.
/// The returned receiver may be sent to remote endpoints via channels.
///
/// Dropping the [`Forwarding`] handle does not stop forwarding. Call
/// [`Forwarding::stop`] to stop it explicitly.
pub fn forward<T, Codec>(local_rx: tokio::sync::oneshot::Receiver<T>) -> (Forwarding, Receiver<T, Codec>)
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    let (tx, rx) = channel();

    let hnd = wokio::spawn(async move {
        tokio::select! {
            biased;
            () = tx.closed() => Ok(()),
            res = local_rx => {
                match res {
                    Ok(v) => match tx.send(v) {
                        Ok(_) => Ok(()),
                        Err(err) if err.is_closed() => Ok(()),
                        Err(err) => Err(err.without_item()),
                    },
                    Err(_) => Ok(()),
                }
            }
        }
    });

    (Forwarding(hnd), rx)
}

/// Handle to obtain the result of forwarding a local receiver remotely by [`forward`].
///
/// Await this to obtain the result of the forwarding operation.
/// The operation is assumed to have finished successfully if either the local or remote
/// channel is closed or dropped.
///
/// Dropping this *does not* stop forwarding.
pub struct Forwarding(wokio::task::JoinHandle<Result<(), SendError<()>>>);

impl fmt::Debug for Forwarding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Forwarding").finish()
    }
}

impl Future for Forwarding {
    type Output = Result<(), SendError<()>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match ready!(self.0.poll_unpin(cx)) {
            Ok(res) => Poll::Ready(res),
            Err(_) => Poll::Ready(Err(SendError::Closed(()))),
        }
    }
}

impl Forwarding {
    /// Stops forwarding.
    ///
    /// The remote sending half and local receiving half of the oneshot channels are dropped.
    pub fn stop(self) {
        self.0.abort();
    }
}

/// Extensions for oneshot channels.
pub trait OneshotExt<T, Codec, const MAX_ITEM_SIZE: usize> {
    /// Sets the maximum item size for the channel.
    fn with_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        self,
    ) -> (Sender<T, Codec>, Receiver<T, Codec, NEW_MAX_ITEM_SIZE>);
}

impl<T, Codec, const MAX_ITEM_SIZE: usize> OneshotExt<T, Codec, MAX_ITEM_SIZE>
    for (Sender<T, Codec>, Receiver<T, Codec, MAX_ITEM_SIZE>)
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    fn with_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        self,
    ) -> (Sender<T, Codec>, Receiver<T, Codec, NEW_MAX_ITEM_SIZE>) {
        let (mut tx, rx) = self;
        tx.set_max_item_size(NEW_MAX_ITEM_SIZE);
        let rx = rx.set_max_item_size();
        (tx, rx)
    }
}
