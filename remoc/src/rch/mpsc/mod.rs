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
use futures::{FutureExt, future::BoxFuture};
use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll, ready},
};

use super::{ClosedReason, RemoteSendError, Sending, base};
use crate::{
    RemoteSend, chmux,
    codec::{self, AnySend, ErasedDeserializer, ErasedSerializer},
    exec,
    rch::{
        BACKCHANNEL_MSG_CLOSE, BACKCHANNEL_MSG_ERROR,
        base::{ErasedReceiver, ErasedSender},
    },
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

    let sender = Sender::new(tx, closed_rx, remote_send_err_rx, None);
    let receiver = Receiver::new(rx, closed_tx, false, remote_send_err_tx, None, None);
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

/// Extensions for mpsc channels.
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

    /// Sets the number of additional parallel transfer channels.
    ///
    /// A value of 1 is not recommended; see [`Cfg::mpsc_parallel`](crate::Cfg::mpsc_parallel)
    /// for what to choose.
    fn with_parallel(
        self, parallel: usize,
    ) -> (Sender<T, Codec, BUFFER>, Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>);
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

    fn with_parallel(
        self, parallel: usize,
    ) -> (Sender<T, Codec, BUFFER>, Receiver<T, Codec, BUFFER, MAX_ITEM_SIZE>) {
        let (mut tx, mut rx) = self;
        tx.set_parallel(Some(parallel));
        rx.set_parallel(Some(parallel));
        (tx, rx)
    }
}

/// Request to send data.
pub(crate) struct SendReq<T> {
    /// Value to send.
    pub value: Result<T, RecvError>,
    /// Channel for reporting result of sending.
    ///
    /// Present only if the sender awaits the send result via [`Sending`].
    pub result_tx: Option<tokio::sync::oneshot::Sender<Result<(), base::SendError<T>>>>,
}

impl<T> SendReq<T> {
    /// Creates a send request without result reporting.
    fn new(value: Result<T, RecvError>) -> Self {
        Self { value, result_tx: None }
    }

    /// Acknowledge reception and return contained value.
    fn ack(self) -> Result<T, RecvError> {
        let Self { value, result_tx } = self;
        if let Some(result_tx) = result_tx {
            let _ = result_tx.send(Ok(()));
        }
        value
    }
}

/// Type-erased access to [SendReq].
pub(crate) trait ErasedSendReq {
    /// Take the value out, replacing it with a dummy value.
    fn take_value(&mut self) -> AnySend;
    /// Report successful sending.
    fn result_ok(&mut self);
    /// Report a send error, returning it back if nobody listens on the result channel.
    fn result_err(&mut self, err: base::SendError<AnySend>) -> Result<(), base::SendError<AnySend>>;
}

impl<T> ErasedSendReq for SendReq<T>
where
    T: Send + 'static,
{
    fn take_value(&mut self) -> AnySend {
        let value = mem::replace(&mut self.value, Err(RecvError::RemoteConnect(chmux::ConnectError::Rejected)));
        Box::new(value)
    }

    fn result_ok(&mut self) {
        if let Some(result_tx) = self.result_tx.take() {
            let _ = result_tx.send(Ok(()));
        }
    }

    fn result_err(&mut self, err: base::SendError<AnySend>) -> Result<(), base::SendError<AnySend>> {
        let item: Result<T, RecvError> = *err.item.downcast().expect("type mismatch in SendReq");
        let Ok(item) = item else { return Ok(()) };
        let err = base::SendError { kind: err.kind, item };

        // Report the error to the caller if nobody is awaiting the send result.
        let err = match self.result_tx.take() {
            Some(result_tx) => match result_tx.send(Err(err)) {
                Ok(()) => return Ok(()),
                Err(res) => res.expect_err("sent item was error"),
            },
            None => err,
        };
        Err(base::SendError { kind: err.kind, item: Box::new(err.item) as AnySend })
    }
}

/// Create a send request and corresponding [Sending] instance for receiving result of send operation.
pub(crate) fn send_req<T>(value: Result<T, RecvError>) -> (SendReq<T>, Sending<T>) {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let this = SendReq { value, result_tx: Some(result_tx) };
    let sent = Sending(result_rx);
    (this, sent)
}

trait ErasedMpscRx {
    fn recv_erased(&'_ mut self) -> BoxFuture<'_, Option<Box<dyn ErasedSendReq + Send>>>;
}

impl<T> ErasedMpscRx for tokio::sync::mpsc::Receiver<SendReq<T>>
where
    T: Send + 'static,
{
    fn recv_erased(&'_ mut self) -> BoxFuture<'_, Option<Box<dyn ErasedSendReq + Send>>> {
        async { self.recv().await.map(|send_req| Box::new(send_req) as Box<dyn ErasedSendReq + Send>) }.boxed()
    }
}

/// Send implementation for deserializer of Sender and serializer of Receiver.
async fn send_impl(
    erased_serializer: ErasedSerializer, mut rx: Box<dyn ErasedMpscRx + Send>, raw_txs: Vec<chmux::Sender>,
    mut raw_rx: chmux::Receiver, remote_send_err_tx: tokio::sync::watch::Sender<Option<RemoteSendError>>,
    closed_tx: tokio::sync::watch::Sender<Option<ClosedReason>>, max_item_size: usize,
) {
    // Send request handler.
    let handle_send_req = {
        let remote_send_err_tx = remote_send_err_tx.clone();
        let closed_tx = closed_tx.clone();

        async move |remote_tx: &mut ErasedSender, mut send_req: Box<dyn ErasedSendReq + Send>| match remote_tx
            .send_erased(send_req.take_value())
            .await
        {
            Ok(()) => send_req.result_ok(),
            Err(err) => {
                let _ = remote_send_err_tx.send(Some(RemoteSendError::Send(err.kind.clone())));
                let _ = closed_tx.send(Some(ClosedReason::Failed));
                if let Err(err) = send_req.result_err(err)
                    && err.is_item_specific()
                {
                    tracing::warn!(%err, "sending over remote channel failed");
                }
            }
        }
    };

    // Create erased base channel senders.
    let mut remote_txs = raw_txs.into_iter().map(|raw_tx| {
        let mut remote_tx = base::ErasedSender::new(erased_serializer.clone(), raw_tx);
        remote_tx.set_max_item_size(max_item_size);
        remote_tx
    });

    // Setup single or multi base channel operation.
    enum BaseTxs {
        Single(Box<ErasedSender>),
        Multi(VecDeque<tokio::sync::mpsc::Sender<Box<dyn ErasedSendReq + Send>>>),
    }
    let mut base_txs = match remote_txs.len() {
        0 => panic!("need at least one raw channel"),
        1 => BaseTxs::Single(Box::new(remote_txs.next().unwrap())),
        _ => BaseTxs::Multi(
            remote_txs
                .map(|mut remote_tx| {
                    let handle_send_req = handle_send_req.clone();
                    let (send_req_tx, mut send_req_rx) =
                        tokio::sync::mpsc::channel::<Box<dyn ErasedSendReq + Send>>(1);
                    exec::spawn(async move {
                        while let Some(send_req) = send_req_rx.recv().await {
                            handle_send_req(&mut remote_tx, send_req).await;
                        }
                    });
                    send_req_tx
                })
                .collect(),
        ),
    };

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
            send_req_opt = rx.recv_erased() => {
                let Some(send_req) = send_req_opt else { break };
                match &mut base_txs {
                    BaseTxs::Single(remote_tx) => handle_send_req(remote_tx, send_req).await,
                    BaseTxs::Multi (txs) => {
                        if txs.front().unwrap().send(send_req).await.is_err() {
                            break
                        }
                        txs.rotate_left(1);
                    }
                }
            }
        }
    }
}

trait ErasedMpscTx {
    fn send(&'_ self, value: AnySend) -> BoxFuture<'_, Result<(), ()>>;
    fn send_err(&'_ self, err: RecvError) -> BoxFuture<'_, Result<(), ()>>;
}

impl<T> ErasedMpscTx for tokio::sync::mpsc::Sender<SendReq<T>>
where
    T: Send + 'static,
{
    fn send(&'_ self, value: AnySend) -> BoxFuture<'_, Result<(), ()>> {
        let value: Result<T, RecvError> = *value.downcast().expect("type mismatch in mpsc receiver");
        async { self.send(SendReq::new(value)).await.map_err(|_| ()) }.boxed()
    }

    fn send_err(&'_ self, err: RecvError) -> BoxFuture<'_, Result<(), ()>> {
        async { self.send(SendReq::new(Err(err))).await.map_err(|_| ()) }.boxed()
    }
}

/// Receive implementation for serializer of Sender and deserializer of Receiver.
async fn recv_impl(
    erased_deserializer: ErasedDeserializer, tx: &(dyn ErasedMpscTx + Send + Sync), mut raw_tx: chmux::Sender,
    raw_rxs: Vec<chmux::Receiver>, mut remote_send_err_rx: tokio::sync::watch::Receiver<Option<RemoteSendError>>,
    mut closed_rx: tokio::sync::watch::Receiver<Option<ClosedReason>>, max_item_size: usize,
) {
    // Create erased base channel receivers.
    let mut remote_rxs = raw_rxs.into_iter().map(|raw_rx| {
        let mut remote_rx = base::ErasedReceiver::new(erased_deserializer.clone(), raw_rx);
        remote_rx.set_max_item_size(max_item_size);
        remote_rx
    });

    // Setup single or multi base channel operation.
    enum BaseRxs {
        Single(Box<ErasedReceiver>),
        Multi(VecDeque<tokio::sync::mpsc::Receiver<Result<Option<AnySend>, base::RecvError>>>),
    }
    let mut base_rxs = match remote_rxs.len() {
        0 => panic!("need at least one raw channel"),
        1 => BaseRxs::Single(Box::new(remote_rxs.next().unwrap())),
        _ => BaseRxs::Multi(
            remote_rxs
                .map(|mut remote_rx| {
                    let (recved_tx, recved_rx) = tokio::sync::mpsc::channel(1);
                    exec::spawn(async move {
                        loop {
                            tokio::select! {
                                biased;
                                () = recved_tx.closed() => break,
                                res = remote_rx.recv_erased() => {
                                    let _ = recved_tx.send(res).await;
                                }
                            }
                        }
                    });
                    recved_rx
                })
                .collect(),
        ),
    };

    // Process events.
    loop {
        let recv_task = async {
            match &mut base_rxs {
                BaseRxs::Single(remote_rx) => remote_rx.recv_erased().await,
                BaseRxs::Multi(rxs) => {
                    let res = rxs
                        .front_mut()
                        .unwrap()
                        .recv()
                        .await
                        .unwrap_or(Err(base::RecvError::Receive(chmux::RecvError::ChMux)));
                    rxs.rotate_left(1);
                    res
                }
            }
        };

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
            res = recv_task => {
                match res {
                    Ok(Some(value)) => {
                        if tx.send(value).await.is_err() {
                            break
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        let is_final_err = err.is_final();
                        if tx.send_err(RecvError::RemoteReceive(err)).await.is_err() || is_final_err {
                            break
                        }
                    }
                }
            }
        }
    }
}
