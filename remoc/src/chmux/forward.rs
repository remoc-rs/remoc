//! Channel data forwarding.

use bytes::Buf;
use std::{fmt, num::Wrapping};
use tracing::Instrument;

use super::{ConnectError, PortReq, Received, RecvChunkError, RecvError, SendError};
use crate::exec;

/// An error occurred during forwarding of a message.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ForwardError {
    /// Sending failed.
    Send(SendError),
    /// Receiving failed.
    Recv(RecvError),
}

impl From<SendError> for ForwardError {
    fn from(err: SendError) -> Self {
        Self::Send(err)
    }
}

impl From<RecvError> for ForwardError {
    fn from(err: RecvError) -> Self {
        Self::Recv(err)
    }
}

impl fmt::Display for ForwardError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ForwardError::Send(err) => write!(f, "forward send failed: {err}"),
            ForwardError::Recv(err) => write!(f, "forward receive failed: {err}"),
        }
    }
}

impl From<ForwardError> for std::io::Error {
    fn from(err: ForwardError) -> Self {
        match err {
            ForwardError::Send(err) => err.into(),
            ForwardError::Recv(err) => err.into(),
        }
    }
}

impl std::error::Error for ForwardError {}

/// Forwards all data received from a receiver to a sender.
pub(crate) async fn forward(rx: &mut super::Receiver, tx: &mut super::Sender) -> Result<usize, ForwardError> {
    // Required to avoid borrow checking loop limitation.
    fn spawn_forward(id: u32, mut rx: super::Receiver, mut tx: super::Sender) {
        exec::spawn(
            async move {
                if let Err(err) = forward(&mut rx, &mut tx).await {
                    tracing::debug!("port forwarding for id {id} failed: {err}");
                }
            }
            .in_current_span(),
        );
    }

    let override_graceful_close = tx.is_graceful_close_overridden();
    tx.set_override_graceful_close(true);

    let mut total = Wrapping(0);
    let mut closed = false;

    enum Event {
        Received(Option<Received>),
        Closed,
    }

    loop {
        let event = tokio::select! {
            res = rx.recv_any() => Event::Received(res?),
            () = tx.closed(), if !closed => Event::Closed,
        };

        match event {
            // Data received.
            Event::Received(Some(Received::Data(data))) => {
                total += data.remaining();
                tx.send(data.into()).await?;
            }

            // Data chunks received.
            Event::Received(Some(Received::Chunks)) => {
                let mut chunk_tx = tx.send_chunks();
                loop {
                    match rx.recv_chunk().await {
                        Ok(Some(chunk)) => {
                            total += chunk.remaining();
                            chunk_tx = chunk_tx.send(chunk).await?;
                        }
                        Ok(None) => {
                            chunk_tx.finish().await?;
                            break;
                        }
                        Err(RecvChunkError::Cancelled) => break,
                        Err(RecvChunkError::ChMux) => return Err(ForwardError::Recv(RecvError::ChMux)),
                    }
                }
            }

            // Ports received.
            Event::Received(Some(Received::Requests(reqs))) => {
                let allocator = tx.port_allocator();

                // Allocate local outgoing ports for forwarding.
                let mut ports = Vec::new();
                let mut wait = false;
                for req in &reqs {
                    let port = allocator.allocate_local().await;
                    ports.push(PortReq::new(port).with_id(req.id()));
                    wait |= req.is_wait();
                }

                // Connect them.
                let connects = tx.connect(ports, wait).await?;
                for (req, connect) in reqs.into_iter().zip(connects) {
                    exec::spawn(
                        async move {
                            let id = req.id();
                            match connect.await {
                                Ok((out_tx, out_rx)) => {
                                    let in_port = out_tx.port_allocator().allocate_local().await;
                                    match req.accept_from(in_port).await {
                                        Ok((in_tx, in_rx)) => {
                                            spawn_forward(id, out_rx, in_tx);
                                            spawn_forward(id, in_rx, out_tx);
                                        }
                                        Err(err) => {
                                            tracing::debug!(
                                                "port forwarding for id {id} failed to accept: {err}"
                                            );
                                        }
                                    }
                                }
                                Err(err) => {
                                    tracing::debug!("port forwarding for id {id} failed to connect: {err}");
                                    req.reject(matches!(
                                        err,
                                        ConnectError::LocalPortsExhausted | ConnectError::RemotePortsExhausted
                                    ))
                                    .await;
                                }
                            }
                        }
                        .in_current_span(),
                    );
                }
            }

            // End received.
            Event::Received(None) => break,

            // Forwarding sender closed.
            Event::Closed => {
                rx.close().await;
                closed = true;
            }
        }
    }

    tx.set_override_graceful_close(override_graceful_close);

    Ok(total.0)
}
