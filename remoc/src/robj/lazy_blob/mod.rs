//! Lazy transmission of binary data.
//!
//! This allows an endpoint to optionally request the transmission of binary data.
//! For example, a client may only be interested sometimes in some data or it
//! wants to know the size of the data before receiving it.
//! By wrapping the binary data in a [LazyBlob], the client can query its size by
//! [LazyBlob::len] and request transfer by calling [LazyBlob::get].
//!
//! When sent over a remote channel, transmission is performed over a [chmux] binary
//! channel without the overhead of a [codec], and takes place in chunks so that other
//! channels are not blocked when transferring a large amount of binary data.
//! When used locally (without sending over a remote channel), no copying occurs and
//! the data is transferred directly.
//!
//! A [LazyBlob] can be forwarded over multiple remote endpoints.
//! The size of the binary data is limited by [usize::MAX].
//!
//! # Security
//!
//! When dealing with untrusted clients, always check the size of the binary data using
//! [LazyBlob::len] before requesting it with [LazyBlob::get] to avoid denial of service
//! attacks by memory exhaustion.
//! The declared length originates from the remote peer and should be validated against
//! an application-specific maximum before fetching the data.
//!
//! # Example
//!
//! In the following example the client sends a message to the server.
//! The value of the field `binary` is not initially transmitted, only its length.
//! The server checks the length and then retrieves the binary data.
//!
//! ```
//! use remoc::prelude::*;
//! use remoc::robj::lazy_blob::LazyBlob;
//!
//! #[derive(Debug, serde::Serialize, serde::Deserialize)]
//! struct Msg {
//!     data: u32,
//!     binary: LazyBlob,
//! }
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<Msg>) {
//!     let binary = vec![64; 1_000_000];
//!     let msg = Msg { data: 123, binary: LazyBlob::new(binary.into()) };
//!     tx.send(msg).await.unwrap();
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<Msg>) {
//!     let msg = rx.recv().await.unwrap().unwrap();
//!     assert_eq!(msg.data, 123);
//!     assert_eq!(msg.binary.len().unwrap(), 1_000_000);
//!
//!     let binary = msg.binary.get().await.unwrap();
//!     assert_eq!(Vec::from(binary), vec![64; 1_000_000]);
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```
//!

use bytes::Bytes;
use futures::{
    FutureExt, future,
    future::{BoxFuture, MaybeDone},
};
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, fmt, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tracing::Instrument;

use crate::{
    chmux,
    chmux::DataBuf,
    codec, exec,
    rch::{ConnectError, mpsc},
};

mod fw_bin;

/// The size of the binary data exceeds [usize::MAX] on this platform.
#[derive(Debug, Clone)]
pub struct UsizeExceeded(pub u64);

impl fmt::Display for UsizeExceeded {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "binary data ({} bytes) exceeds maximum array size", self.0)
    }
}

impl std::error::Error for UsizeExceeded {}

/// An error occurred fetching the binary data from the remote endpoint.
#[derive(Debug, Clone)]
pub enum FetchError {
    /// The provider has been dropped.
    Dropped,
    /// The size of the binary data exceeds [usize::MAX] on this platform.
    Size(UsizeExceeded),
    /// Receiving the binary data from the remote endpoint failed.
    RemoteReceive(chmux::RecvError),
    /// Connecting a sent channel failed.
    RemoteConnect(ConnectError),
}

impl From<UsizeExceeded> for FetchError {
    fn from(err: UsizeExceeded) -> Self {
        Self::Size(err)
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dropped => write!(f, "provider was dropped"),
            Self::Size(err) => write!(f, "{err}"),
            Self::RemoteReceive(err) => write!(f, "receive error: {}", &err),
            Self::RemoteConnect(err) => write!(f, "connect error: {}", &err),
        }
    }
}

impl std::error::Error for FetchError {}

/// Holds the data for a [LazyBlob].
///
/// Dropping the provider will stop making the data available for remote fetching.
pub struct Provider {
    keep_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Provider").finish()
    }
}

impl Provider {
    /// Keeps the provider alive until it is not required anymore.
    pub fn keep(mut self) {
        let _ = self.keep_tx.take().unwrap().send(());
    }

    /// Waits until the provider can be safely dropped.
    ///
    /// This is the case when all associated [LazyBlob]s requested
    /// and received the data or have been dropped.
    pub async fn done(&mut self) {
        self.keep_tx.as_mut().unwrap().closed().await
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        // empty
    }
}

/// Lazily transferred binary data. 🐡💤
///
/// Allows efficient transmission of large binary data on-demand.
///
/// See [module-level documentation](self) for details.
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "Codec: codec::Codec"))]
#[serde(bound(deserialize = "Codec: codec::Codec"))]
pub struct LazyBlob<Codec = codec::Default> {
    req_tx: mpsc::Sender<fw_bin::Sender, Codec, 1>,
    len: u64,
    #[serde(skip)]
    #[serde(default)]
    #[allow(clippy::type_complexity)]
    fetch_task: Arc<Mutex<Option<Pin<Box<MaybeDone<BoxFuture<'static, Result<DataBuf, FetchError>>>>>>>>,
}

impl<Codec> fmt::Debug for LazyBlob<Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LazyBlob").field("len", &self.len).finish()
    }
}

impl<Codec> LazyBlob<Codec>
where
    Codec: codec::Codec,
{
    /// Create a new LazyBlob with the specified data.
    ///
    /// The length of the data must not exceed [usize::MAX] on both the sender
    /// and receiver side.
    pub fn new(data: Bytes) -> Self {
        let (lazy_blob, provider) = Self::provided(data);
        provider.keep();
        lazy_blob
    }

    /// Create a new LazyBlob with the specified data and return it together with
    /// its provider.
    pub fn provided(data: Bytes) -> (Self, Provider) {
        let (keep_tx, keep_rx) = tokio::sync::oneshot::channel();
        let (req_tx, req_rx) = mpsc::channel(1);
        let req_tx = req_tx.set_buffer();
        let mut req_rx = req_rx.set_buffer::<1>();
        let len = data.len() as _;

        exec::spawn(
            async move {
                let do_send = async move {
                    loop {
                        let fw_tx: fw_bin::Sender = match req_rx.recv().await {
                            Ok(Some(fw_tx)) => fw_tx,
                            Ok(None) => break,
                            Err(err) if err.is_final() => break,
                            Err(_) => continue,
                        };

                        let data = data.clone();
                        exec::spawn(
                            async move {
                                match fw_tx.into_inner() {
                                    Some(fw_bin::InnerSender::Local(tx)) => {
                                        let _ = tx.send(data);
                                    }
                                    Some(fw_bin::InnerSender::Remote(bin_tx)) => {
                                        let Ok(mut tx) = bin_tx.into_inner().await else { return };
                                        let _ = tx.send(data).await;
                                    }
                                    None => return,
                                }
                            }
                            .in_current_span(),
                        );
                    }
                };

                tokio::select! {
                    () = do_send => (),
                    Err(_) = keep_rx => (),
                }
            }
            .in_current_span(),
        );

        let lazy_blob = LazyBlob { req_tx, len, fetch_task: Default::default() };
        let provider = Provider { keep_tx: Some(keep_tx) };
        (lazy_blob, provider)
    }

    /// Returns true if the binary data has zero length.
    pub fn is_empty(&self) -> bool {
        matches!(self.len(), Ok(0))
    }

    /// Returns the length of the binary data.
    ///
    /// This will not fetch the data.
    pub fn len(&self) -> Result<usize, UsizeExceeded> {
        usize::try_from(self.len).map_err(|_| UsizeExceeded(self.len))
    }

    /// Fetches and caches the binary data from the provider.
    async fn fetch(&self) -> Result<(), FetchError> {
        let mut fetch_task = self.fetch_task.lock().await;

        if fetch_task.is_none() {
            let req_tx = self.req_tx.clone();
            let len = self.len()?;
            *fetch_task = Some(Box::pin(future::maybe_done(
                async move {
                    let (fw_tx, fw_rx) = fw_bin::channel();
                    let _ = req_tx.send(fw_tx).await;
                    match fw_rx.into_inner().await {
                        Some(fw_bin::InnerReceiver::Local(data)) => Ok(data.into()),
                        Some(fw_bin::InnerReceiver::Remote(bin_rx)) => {
                            let mut rx = bin_rx.into_inner().await.map_err(FetchError::RemoteConnect)?;
                            rx.set_max_data_size(len);
                            rx.recv().await.map_err(FetchError::RemoteReceive)?.ok_or(FetchError::Dropped)
                        }
                        None => Err(FetchError::Dropped),
                    }
                }
                .boxed(),
            )));
        }

        fetch_task.as_mut().unwrap().await;

        Ok(())
    }

    /// Returns a shared reference to the binary data.
    ///
    /// The binary data is fetched when this function is first called and
    /// then cached locally.
    pub async fn get(&self) -> Result<DataBuf, FetchError> {
        self.fetch().await?;

        let mut res = self.fetch_task.lock().await;
        res.as_mut().unwrap().as_mut().output_mut().unwrap().clone()
    }

    /// Returns the binary data.
    ///
    /// The binary data is fetched when not already cached by a previous
    /// call to [get](Self::get).
    pub async fn into_inner(mut self) -> Result<DataBuf, FetchError> {
        self.fetch().await?;

        match Arc::try_unwrap(self.fetch_task) {
            Ok(fetch_task) => {
                let mut res = fetch_task.lock().await;
                res.as_mut().unwrap().as_mut().take_output().unwrap()
            }
            Err(shared_fetch_task) => {
                self.fetch_task = shared_fetch_task;
                self.get().await
            }
        }
    }
}
