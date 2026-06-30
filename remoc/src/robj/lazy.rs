//! Lazy transmission of values.
//!
//! This allows a remote endpoint to optionally request the transmission of a value.
//! For example, a client may only be interested sometimes in the value of a field
//! of a larger struct.
//! By wrapping the value of this field in a [Lazy], the value of the field is
//! not initially transmitted with the struct, but the client can request it by
//! calling [Lazy::get] if interested.
//! While this can save transmission bandwidth the drawback is an additional
//! delay of the connection round-trip time when the lazy value is requested.
//!
//! This can be forwarded over multiple remote endpoints.
//!
//! # Example
//!
//! In the following example the client sends a message to the server.
//! The value of the field `big_data` is not initially transmitted, but
//! retrieved when the server calls `get()` on it.
//!
//! ```
//! use remoc::prelude::*;
//! use remoc::robj::lazy::Lazy;
//!
//! #[derive(Debug, serde::Serialize, serde::Deserialize)]
//! struct Msg {
//!     data: u32,
//!     big_data: Lazy<String>,
//! }
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<Msg>) {
//!     let msg = Msg { data: 123, big_data: Lazy::new("long data".to_string()) };
//!     tx.send(msg).await.unwrap();
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<Msg>) {
//!     let msg = rx.recv().await.unwrap().unwrap();
//!     assert_eq!(msg.data, 123);
//!     assert_eq!(*msg.big_data.get().await.unwrap(), "long data");
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```
//!

use futures::{
    Future, FutureExt,
    future::{self, BoxFuture, MaybeDone},
};
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt, marker::PhantomData, ops::Deref, pin::Pin, sync::Arc};
use tokio::sync::Mutex;

use crate::{
    RemoteSend, chmux, codec, exec,
    rch::{base, mpsc, oneshot},
};

/// An error occurred during fetching a lazily transmitted value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FetchError {
    /// Provider dropped before getting the value.
    Dropped,
    /// Receiving from a remote endpoint failed.
    RemoteReceive(base::RecvError),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    RemoteListen(chmux::ListenerError),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dropped => write!(f, "lazy provider dropped"),
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
        }
    }
}

impl From<oneshot::RecvError> for FetchError {
    fn from(err: oneshot::RecvError) -> Self {
        match err {
            oneshot::RecvError::Closed => Self::Dropped,
            oneshot::RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            oneshot::RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            oneshot::RecvError::RemoteListen(err) => Self::RemoteListen(err),
        }
    }
}

impl Error for FetchError {}

/// Lazy provider.
///
/// Stores a value and sends it to the [lazy consumer](Lazy)
/// when it requests the value.
///
/// If the lazy provider is dropped, the stored value is dropped and
/// the lazy consumer cannot request it anymore.
pub struct Provider {
    keep_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Provider").finish()
    }
}

impl Provider {
    /// Keeps the provider alive until the value is requested
    /// or not required anymore.
    pub fn keep(mut self) {
        let _ = self.keep_tx.take().unwrap().send(());
    }

    /// Waits until the lazy provider can be safely dropped.
    ///
    /// This is the case when the value is requested by the [lazy consumer](Lazy)
    /// or the consumer is dropped.
    pub async fn done(&mut self) {
        self.keep_tx.as_mut().unwrap().closed().await
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        // empty
    }
}

/// Lazy value.
///
/// Allow the reception of a value when requested.
///
/// See [module-level documentation](self) for details.
#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: codec::Codec"))]
pub struct Lazy<T, Codec = codec::Default> {
    request_tx: mpsc::Sender<oneshot::Sender<T, Codec>, Codec, 1>,
    #[serde(skip)]
    #[serde(default)]
    #[allow(clippy::type_complexity)]
    fetch_task: Arc<Mutex<Option<Pin<Box<MaybeDone<BoxFuture<'static, Result<Arc<T>, FetchError>>>>>>>>,
}

impl<T, Codec> fmt::Debug for Lazy<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Lazy").finish()
    }
}

impl<T, Codec> Lazy<T, Codec>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Creates a new lazy value that will receive the specified value.
    ///
    /// The value is stored locally until [get](Self::get) is called or this
    /// object is dropped.
    pub fn new(value: T) -> Self {
        Self::new_future(async move { value })
    }

    /// Creates a new lazy value that will receive the value returned by the
    /// specified future.
    ///
    /// The future is stored unevaluated until [get](Self::get) is called or this
    /// object is dropped.
    pub fn new_future<F>(value_fut: F) -> Self
    where
        F: Future<Output = T> + Send + 'static,
    {
        let (lazy, provider) = Self::provided_future(value_fut);
        provider.keep();
        lazy
    }

    /// Creates a new pair of lazy value and provider with the specified value.
    pub fn provided(value: T) -> (Self, Provider) {
        Self::provided_future(async move { value })
    }

    /// Creates a new pair of lazy value and provider with the value returned by
    /// the specified future.
    pub fn provided_future<F>(value_fut: F) -> (Self, Provider)
    where
        F: Future<Output = T> + Send + 'static,
    {
        let (request_tx, request_rx) = mpsc::channel::<oneshot::Sender<T, Codec>, _>(1);
        let request_tx = request_tx.set_buffer::<1>();
        let mut request_rx = request_rx.set_buffer::<1>();
        let (keep_tx, keep_rx) = tokio::sync::oneshot::channel();

        exec::spawn(async move {
            tokio::select! {
                res = request_rx.recv() => {
                    if let Ok(Some(value_tx)) = res {
                        let value = value_fut.await;
                        let _ = value_tx.send(value);
                    }
                },
                Err(_) = keep_rx => (),
            }
        });

        let provider = Provider { keep_tx: Some(keep_tx) };
        let lazy = Lazy { request_tx, fetch_task: Default::default() };

        (lazy, provider)
    }

    /// Fetches and caches the value from the provider.
    async fn fetch(&self) {
        let mut fetch_task = self.fetch_task.lock().await;

        if fetch_task.is_none() {
            let req_tx = self.request_tx.clone();
            *fetch_task = Some(Box::pin(future::maybe_done(
                async move {
                    let (value_tx, value_rx) = oneshot::channel();
                    let _ = req_tx.send(value_tx).await;
                    let value = value_rx.await?;
                    Ok(Arc::new(value))
                }
                .boxed(),
            )));
        }

        fetch_task.as_mut().unwrap().await;
    }

    /// Requests the value and returns a reference to it.
    ///
    /// The value is stored locally once received and subsequent
    /// invocations of this function will return a reference to
    /// the local copy.
    pub async fn get(&self) -> Result<Ref<'_, T>, FetchError> {
        self.fetch().await;

        let mut res_task = self.fetch_task.lock().await;
        match res_task.as_mut().unwrap().as_mut().output_mut().unwrap() {
            Ok(value) => Ok(Ref { value: value.clone(), _lifetime: PhantomData }),
            Err(err) => Err(err.clone()),
        }
    }

    /// Consumes this object and returns the value.
    pub async fn into_inner(self) -> Result<T, FetchError> {
        self.fetch().await;

        let mut res_task = self.fetch_task.lock().await;
        res_task.as_mut().unwrap().as_mut().take_output().unwrap().map(|arc| match Arc::try_unwrap(arc) {
            Ok(value) => value,
            Err(_) => unreachable!("no other reference can exist"),
        })
    }
}

/// A reference to a lazily received value.
pub struct Ref<'a, T> {
    value: Arc<T>,
    _lifetime: PhantomData<&'a ()>,
}

impl<T> fmt::Debug for Ref<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", **self)
    }
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> Drop for Ref<'_, T> {
    fn drop(&mut self) {
        // empty
    }
}
