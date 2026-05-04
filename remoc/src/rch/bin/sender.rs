use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::{
    fmt, mem,
    sync::{Arc, Mutex},
};

use super::{
    super::{
        ConnectError,
        base::{PortDeserializer, PortSerializer},
    },
    Interlock, Location,
};
use crate::chmux;

/// A binary channel sender.
pub struct Sender {
    pub(super) sender: Option<Result<chmux::Sender, ConnectError>>,
    pub(super) sender_rx: tokio::sync::mpsc::UnboundedReceiver<Result<chmux::Sender, ConnectError>>,
    pub(super) receiver_tx: Option<tokio::sync::mpsc::UnboundedSender<Result<chmux::Receiver, ConnectError>>>,
    pub(super) interlock: Arc<Mutex<Interlock>>,
    pub(super) successor_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Self>>>,
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

/// A binary channel sender in transport.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TransportedSender {
    /// chmux port number.
    pub port: u32,
}

impl Sender {
    async fn connect(&mut self) {
        if self.sender.is_none() {
            self.sender = Some(self.sender_rx.recv().await.unwrap_or(Err(ConnectError::Dropped)));
        }
    }

    /// Establishes the connection and returns a reference to the chmux sender channel
    /// to the remote endpoint.
    pub async fn get(&mut self) -> Result<&mut chmux::Sender, ConnectError> {
        self.connect().await;
        self.sender.as_mut().unwrap().as_mut().map_err(|err| err.clone())
    }

    /// Establishes the connection and returns the chmux sender channel
    /// to the remote endpoint.
    pub async fn into_inner(mut self) -> Result<chmux::Sender, ConnectError> {
        self.connect().await;
        self.sender.take().unwrap()
    }

    /// Forward data.
    async fn forward(successor_rx: tokio::sync::oneshot::Receiver<Self>, rx: super::Receiver) {
        let Ok(tx) = successor_rx.await else { return };
        let Ok(mut tx) = tx.into_inner().await else { return };
        let Ok(mut rx) = rx.into_inner().await else { return };
        if let Err(err) = rx.forward(&mut tx).await {
            tracing::debug!("forwarding binary channel failed: {err}");
        }
    }
}

impl Serialize for Sender {
    /// Serializes this sender for sending over a chmux channel.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let receiver_tx = self.receiver_tx.clone();
        let interlock_confirm = {
            let mut interlock = self.interlock.lock().unwrap();
            if interlock.receiver.check_local() { Some(interlock.sender.start_send()) } else { None }
        };

        match (receiver_tx, interlock_confirm) {
            // Local-remote connection.
            (Some(receiver_tx), Some(interlock_confirm)) => {
                let port = PortSerializer::connect(|connect| {
                    async move {
                        let _ = interlock_confirm.send(());

                        match connect.await {
                            Ok((_, raw_rx)) => {
                                let _ = receiver_tx.send(Ok(raw_rx));
                            }
                            Err(err) => {
                                let _ = receiver_tx.send(Err(ConnectError::Connect(err)));
                            }
                        }
                    }
                    .boxed()
                })?;

                TransportedSender { port }.serialize(serializer)
            }

            // Forwarding.
            _ => {
                let (successor_tx, successor_rx) = tokio::sync::oneshot::channel();
                *self.successor_tx.lock().unwrap() = Some(successor_tx);
                let (tx, rx) = super::channel();
                PortSerializer::spawn(Self::forward(successor_rx, rx))?;

                tx.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for Sender {
    /// Deserializes this sender after it has been received over a chmux channel.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let TransportedSender { port } = TransportedSender::deserialize(deserializer)?;

        let (sender_tx, sender_rx) = tokio::sync::mpsc::unbounded_channel();
        PortDeserializer::accept(port, |local_port, request| {
            async move {
                match request.accept_from(local_port).await {
                    Ok((raw_tx, _)) => {
                        let _ = sender_tx.send(Ok(raw_tx));
                    }
                    Err(err) => {
                        let _ = sender_tx.send(Err(ConnectError::Listen(err)));
                    }
                }
            }
            .boxed()
        })?;

        Ok(Self {
            sender: None,
            sender_rx,
            receiver_tx: None,
            interlock: Arc::new(Mutex::new(Interlock { sender: Location::Local, receiver: Location::Remote })),
            successor_tx: std::sync::Mutex::new(None),
        })
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let successor_tx = self.successor_tx.lock().unwrap().take();
        if let Some(successor_tx) = successor_tx {
            let dummy = Self {
                sender: None,
                sender_rx: tokio::sync::mpsc::unbounded_channel().1,
                receiver_tx: None,
                interlock: Arc::new(Mutex::new(Interlock::new())),
                successor_tx: std::sync::Mutex::new(None),
            };
            let _ = successor_tx.send(mem::replace(self, dummy));
        }
    }
}
