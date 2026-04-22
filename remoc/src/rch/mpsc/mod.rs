//! Multi producer single customer remote channel.
//!
//! The sender and receiver can both be sent to remote endpoints.
//! The channel also works if both halves are local.
//! Forwarding over multiple connections is supported.
//!
//! This has similar functionality as [tokio::sync::mpsc] with the additional
//! ability to work over remote connections.
//!
//! # Example
//!
//! In the following example the client sends a number and an MPSC channel sender to the server.
//! The server counts to the number and sends each value to the client over the MPSC channel.
//!
//! ```
//! use remoc::prelude::*;
//!
//! #[derive(Debug, serde::Serialize, serde::Deserialize)]
//! struct CountReq {
//!     up_to: u32,
//!     seq_tx: rch::mpsc::Sender<u32>,
//! }
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<CountReq>) {
//!     let (seq_tx, mut seq_rx) = rch::mpsc::channel(1);
//!     tx.send(CountReq { up_to: 4, seq_tx }).await.unwrap();
//!
//!     assert_eq!(seq_rx.recv().await.unwrap(), Some(0));
//!     assert_eq!(seq_rx.recv().await.unwrap(), Some(1));
//!     assert_eq!(seq_rx.recv().await.unwrap(), Some(2));
//!     assert_eq!(seq_rx.recv().await.unwrap(), Some(3));
//!     assert_eq!(seq_rx.recv().await.unwrap(), None);
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<CountReq>) {
//!     while let Some(CountReq { up_to, seq_tx }) = rx.recv().await.unwrap() {
//!         for i in 0..up_to {
//!             seq_tx.send(i).await.unwrap();
//!         }
//!     }
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```
//!

use bytes::Buf;
use futures::FutureExt;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll, ready},
};

use super::{ClosedReason, RemoteSendError, Sending, base};
use crate::{
    RemoteSend, chmux, codec, exec,
    rch::{BACKCHANNEL_MSG_CLOSE, BACKCHANNEL_MSG_ERROR},
};

mod distributor;
mod receiver;
mod sender;

pub use distributor::{DistributedReceiverHandle, Distributor};
pub use receiver::{Receiver, RecvError, TryRecvError};
pub use sender::{Permit, SendError, Sender, SenderSink, TrySendError};

/// Creates a bounded channel for communicating between asynchronous tasks with back pressure.
///
/// The sender and receiver may be sent to remote endpoints via channels.
pub fn channel<T, Codec>(local_buffer: usize) -> (Sender<T, Codec>, Receiver<T, Codec>)
where
    T: RemoteSend,
{
    assert!(local_buffer > 0, "local_buffer must not be zero");

    let (tx, rx) = tokio::sync::mpsc::channel(local_buffer);
    let (closed_tx, closed_rx) = tokio::sync::watch::channel(None);
    let (remote_send_err_tx, remote_send_err_rx) = tokio::sync::watch::channel(None);

    let sender = Sender::new(tx, closed_rx, remote_send_err_rx);
    let receiver = Receiver::new(rx, closed_tx, false, remote_send_err_tx, None);
    (sender, receiver)
}

/// Makes a local mpsc receiver forwardable to remote endpoints.
///
/// The returned [`Forwarding`] future resolves once forwarding has completed or an error occurs.
/// The returned receiver may be sent to remote endpoints via channels.
pub fn forward<T, Codec>(mut local_rx: tokio::sync::mpsc::Receiver<T>) -> (Forwarding, Receiver<T, Codec>)
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    let (tx, rx) = channel(1);

    let hnd = exec::spawn(async move {
        loop {
            let permit = match tx.reserve().await {
                Ok(permit) => permit,
                Err(err) if err.is_closed() => break,
                Err(err) => return Err(err),
            };
            match local_rx.recv().await {
                Some(v) => {
                    permit.send(v);
                }
                None => break,
            }
        }

        Ok(())
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
pub struct Forwarding(exec::task::JoinHandle<Result<(), SendError<()>>>);

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
    /// The remote sending half and local receiving half of the mpsc channels are dropped.
    pub fn stop(self) {
        self.0.abort();
    }
}

/// Extensions for MPSC channels.
pub trait MpscExt<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> {
    /// Sets the buffer size that will be used when sending the channel's sender and receiver
    /// to a remote endpoint.
    fn with_buffer<const NEW_BUFFER: usize>(
        self,
    ) -> (Sender<T, Codec, NEW_BUFFER>, Receiver<T, Codec, NEW_BUFFER, MAX_ITEM_SIZE>);

    /// Sets the maximum item size for the channel.
    fn with_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        self,
    ) -> (Sender<T, Codec, BUFFER>, Receiver<T, Codec, BUFFER, NEW_MAX_ITEM_SIZE>);
}

impl<T, Codec, const BUFFER: usize, const MAX_ITEM_SIZE: usize> MpscExt<T, Codec, BUFFER, MAX_ITEM_SIZE>
    for (Sender<T, Codec, BUFFER>, Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>)
where
    T: Send + 'static,
{
    fn with_buffer<const NEW_BUFFER: usize>(
        self,
    ) -> (Sender<T, Codec, NEW_BUFFER>, Receiver<T, Codec, NEW_BUFFER, MAX_ITEM_SIZE>) {
        let (tx, rx) = self;
        let tx = tx.set_buffer();
        let rx = rx.set_buffer();
        (tx, rx)
    }

    fn with_max_item_size<const NEW_MAX_ITEM_SIZE: usize>(
        self,
    ) -> (Sender<T, Codec, BUFFER>, Receiver<T, Codec, BUFFER, NEW_MAX_ITEM_SIZE>) {
        let (mut tx, rx) = self;
        tx.set_max_item_size(NEW_MAX_ITEM_SIZE);
        let rx = rx.set_max_item_size();
        (tx, rx)
    }
}

pub(crate) struct SendReq<T> {
    pub value: Result<T, RecvError>,
    pub result_tx: tokio::sync::oneshot::Sender<Result<(), base::SendError<T>>>,
}

impl<T> SendReq<T> {
    fn new(value: Result<T, RecvError>) -> Self {
        Self { value, result_tx: tokio::sync::oneshot::channel().0 }
    }

    fn ack(self) -> Result<T, RecvError> {
        let Self { value, result_tx } = self;
        let _ = result_tx.send(Ok(()));
        value
    }
}

pub(crate) fn send_req<T>(value: Result<T, RecvError>) -> (SendReq<T>, Sending<T>) {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let this = SendReq { value, result_tx };
    let sent = Sending(result_rx);
    (this, sent)
}

/// Send implementation for deserializer of Sender and serializer of Receiver.
async fn send_impl<T, Codec>(
    mut rx: tokio::sync::mpsc::Receiver<SendReq<T>>, raw_tx: chmux::Sender, mut raw_rx: chmux::Receiver,
    remote_send_err_tx: tokio::sync::watch::Sender<Option<RemoteSendError>>,
    closed_tx: tokio::sync::watch::Sender<Option<ClosedReason>>, max_item_size: usize,
) where
    T: Serialize + Send + 'static,
    Codec: codec::Codec,
{
    // Encode data using remote sender.
    let mut remote_tx = base::Sender::<Result<T, RecvError>, Codec>::new(raw_tx);
    remote_tx.set_max_item_size(max_item_size);

    // Process events.
    loop {
        tokio::select! {
            biased;

            // Back channel message from remote endpoint.
            backchannel_msg = raw_rx.recv() => {
                match backchannel_msg {
                    Ok(Some(mut msg)) if msg.remaining() >= 1 => {
                        match msg.get_u8() {
                            BACKCHANNEL_MSG_CLOSE => {
                                let _ = remote_send_err_tx.send(Some(RemoteSendError::Closed));
                                let _ = closed_tx.send(Some(ClosedReason::Closed));
                                break;
                            }
                            BACKCHANNEL_MSG_ERROR => {
                                let _ = remote_send_err_tx.send(Some(RemoteSendError::Forward));
                                let _ = closed_tx.send(Some(ClosedReason::Failed));
                                break;
                            }
                            _ => (),
                        }
                    },
                    Ok(Some(_)) => (),
                    Ok(None) => {
                        let _ = remote_send_err_tx.send(Some(RemoteSendError::Send(
                            base::SendErrorKind::Send(chmux::SendError::Closed { gracefully: false })
                        )));
                        let _ = closed_tx.send(Some(ClosedReason::Dropped));
                        break;
                    }
                    _ => {
                        let _ = remote_send_err_tx.send(Some(RemoteSendError::Send(
                            base::SendErrorKind::Send(chmux::SendError::ChMux)
                        )));
                        let _ = closed_tx.send(Some(ClosedReason::Failed));
                        break;
                    },
                }
            }

            // Data to send to remote endpoint.
            value_opt = rx.recv() => {
                match value_opt {
                    Some(value) => {
                        let SendReq { value, result_tx } = value;
                        match remote_tx.send(value).await {
                            Ok(()) => {
                                let _ = result_tx.send(Ok(()));
                            }
                            Err(err) => {
                                let _ = remote_send_err_tx.send(Some(RemoteSendError::Send(err.kind.clone())));
                                let _ = closed_tx.send(Some(ClosedReason::Failed));
                                if let Ok(item) = err.item
                                    && let Err(Err(err)) = result_tx.send(Err(base::SendError {
                                        kind: err.kind,
                                        item,
                                    }))
                                        && err.is_item_specific() {
                                            tracing::warn!(%err, "sending over remote channel failed");
                                        }
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

/// Receive implementation for serializer of Sender and deserializer of Receiver.
async fn recv_impl<T, Codec>(
    tx: &tokio::sync::mpsc::Sender<SendReq<T>>, mut raw_tx: chmux::Sender, raw_rx: chmux::Receiver,
    mut remote_send_err_rx: tokio::sync::watch::Receiver<Option<RemoteSendError>>,
    mut closed_rx: tokio::sync::watch::Receiver<Option<ClosedReason>>, max_item_size: usize,
) where
    T: DeserializeOwned + Send + 'static,
    Codec: codec::Codec,
{
    // Decode raw received data using remote receiver.
    let mut remote_rx = base::Receiver::<Result<T, RecvError>, Codec>::new(raw_rx);
    remote_rx.set_max_item_size(max_item_size);

    // Process events.
    loop {
        tokio::select! {
            biased;

            // Channel closure requested locally.
            res = closed_rx.changed() => {
                match res {
                    Ok(()) => {
                        let reason = closed_rx.borrow().clone();
                        match reason {
                            Some(ClosedReason::Closed) => {
                                let _ = raw_tx.send(vec![BACKCHANNEL_MSG_CLOSE].into()).await;
                            }
                            Some(ClosedReason::Dropped) => break,
                            Some(ClosedReason::Failed) => {
                                let _ = raw_tx.send(vec![BACKCHANNEL_MSG_ERROR].into()).await;
                            }
                            None => (),
                        }
                    },
                    Err(_) => break,
                }
            }

            // Notify remote endpoint of error.
            Ok(()) = remote_send_err_rx.changed() => {
                if remote_send_err_rx.borrow().as_ref().is_some() {
                    let _ = raw_tx.send(vec![BACKCHANNEL_MSG_ERROR].into()).await;
                }
            }

            // Data received from remote endpoint.
            res = remote_rx.recv() => {
                let mut is_final_err = false;
                let value = match res {
                    Ok(Some(value)) => value,
                    Ok(None) => break,
                    Err(err) => {
                        is_final_err = err.is_final();
                        Err(RecvError::RemoteReceive(err))
                    },
                };
                if tx.send(SendReq::new(value)).await.is_err() {
                    break;
                }
                if is_final_err {
                    break;
                }
            }
        }
    }
}
