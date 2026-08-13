//! Channel data forwarding.

use bytes::Buf;
use std::{fmt, num::Wrapping};
use tracing::Instrument;

use super::{ConnectError, Received, RecvChunkError, RecvError, SendError, port_allocator::PortsExhausted};

/// An error occurred during forwarding of a message.
#[derive(Debug, Clone)]
pub enum ForwardError {
    /// Sending failed.
    Send(SendError),
    /// Receiving failed.
    Recv(RecvError),
    /// All local ports are in use.
    LocalPortsExhausted,
}

#[cfg(feature = "serde")]
crate::versioned::compact::impl_enum! {
    ForwardError,
    variants {
        Send(err: SendError) => "_0",
        Recv(err: RecvError) => "_1",
        LocalPortsExhausted => "_2",
    }
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

impl From<PortsExhausted> for ForwardError {
    fn from(_: PortsExhausted) -> Self {
        Self::LocalPortsExhausted
    }
}

impl fmt::Display for ForwardError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Send(err) => write!(f, "forward send failed: {err}"),
            Self::Recv(err) => write!(f, "forward receive failed: {err}"),
            Self::LocalPortsExhausted => write!(f, "all local ports are in use"),
        }
    }
}

impl From<ForwardError> for std::io::Error {
    fn from(err: ForwardError) -> Self {
        use std::io::ErrorKind;
        match err {
            ForwardError::Send(err) => err.into(),
            ForwardError::Recv(err) => err.into(),
            ForwardError::LocalPortsExhausted => Self::new(ErrorKind::AddrInUse, err),
        }
    }
}

impl std::error::Error for ForwardError {}

/// Forwards all data received from a receiver to a sender.
pub(crate) async fn forward(rx: &mut super::Receiver, tx: &mut super::Sender) -> Result<usize, ForwardError> {
    // Required to avoid borrow checking loop limitation.
    fn spawn_forward(id: u32, mut rx: super::Receiver, mut tx: super::Sender) {
        wokio::spawn(
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
            biased;
            () = tx.closed(), if !closed => Event::Closed,
            res = rx.recv_any() => Event::Received(res?),
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
                let mut connect_reqs = Vec::new();
                for req in &reqs {
                    let mut connect_req = allocator.connect_req()?;
                    connect_req = connect_req.with_id(req.id());
                    if !req.is_wait() {
                        connect_req = connect_req.no_wait();
                    }
                    if req.is_pre_connected() {
                        connect_req = connect_req.try_pre_connect();
                    }
                    connect_reqs.push(connect_req);
                }

                // Connect them.
                let connects = tx.connect(connect_reqs).await?;
                for (req, connect) in reqs.into_iter().zip(connects) {
                    wokio::spawn(
                        async move {
                            let id = req.id();
                            match connect.await {
                                Ok((out_tx, out_rx)) => match req.accept().await {
                                    Ok((in_tx, in_rx)) => {
                                        spawn_forward(id, out_rx, in_tx);
                                        spawn_forward(id, in_rx, out_tx);
                                    }
                                    Err(err) => {
                                        tracing::debug!("port forwarding for id {id} failed to accept: {err}");
                                    }
                                },
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
