//! Connection extensions.

use std::{error::Error, fmt, future::Future};

use crate::{
    chmux::ChMuxError,
    connect::ConnectError,
    exec,
    rch::base::{RecvError, SendError},
};

#[cfg(feature = "default-codec-set")]
use crate::{RemoteSend, connect::Connect, rch::base};

/// Error occurred during establishing a providing connection.
#[cfg_attr(docsrs, doc(cfg(feature = "rch")))]
#[derive(Debug, Clone)]
pub enum ProvideError<TransportSinkError, TransportStreamError> {
    /// Channel multiplexer error.
    ChMux(ChMuxError<TransportSinkError, TransportStreamError>),
    /// Connection error.
    Connect(ConnectError<TransportSinkError, TransportStreamError>),
    /// Sending provided value failed.
    Send(SendError<()>),
}

impl<TransportSinkError, TransportStreamError> From<ChMuxError<TransportSinkError, TransportStreamError>>
    for ProvideError<TransportSinkError, TransportStreamError>
{
    fn from(err: ChMuxError<TransportSinkError, TransportStreamError>) -> Self {
        Self::ChMux(err)
    }
}

impl<TransportSinkError, TransportStreamError> From<ConnectError<TransportSinkError, TransportStreamError>>
    for ProvideError<TransportSinkError, TransportStreamError>
{
    fn from(err: ConnectError<TransportSinkError, TransportStreamError>) -> Self {
        Self::Connect(err)
    }
}

impl<T, TransportSinkError, TransportStreamError> From<SendError<T>>
    for ProvideError<TransportSinkError, TransportStreamError>
{
    fn from(err: SendError<T>) -> Self {
        Self::Send(err.without_item())
    }
}

impl<TransportSinkError, TransportStreamError> fmt::Display
    for ProvideError<TransportSinkError, TransportStreamError>
where
    TransportSinkError: fmt::Display,
    TransportStreamError: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ChMux(err) => write!(f, "chmux error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Send(err) => write!(f, "send error: {err}"),
        }
    }
}

impl<TransportSinkError, TransportStreamError> Error for ProvideError<TransportSinkError, TransportStreamError>
where
    TransportSinkError: fmt::Debug + fmt::Display,
    TransportStreamError: fmt::Debug + fmt::Display,
{
}

/// Error occurred during establishing a consuming connection.
#[cfg_attr(docsrs, doc(cfg(feature = "rch")))]
#[derive(Debug, Clone)]
pub enum ConsumeError<TransportSinkError, TransportStreamError> {
    /// Channel multiplexer error.
    ChMux(ChMuxError<TransportSinkError, TransportStreamError>),
    /// Connection error.
    Connect(ConnectError<TransportSinkError, TransportStreamError>),
    /// Receiving the value to consume failed.
    Recv(RecvError),
    /// No value to consume was received.
    NoValueReceived,
}

impl<TransportSinkError, TransportStreamError> From<ChMuxError<TransportSinkError, TransportStreamError>>
    for ConsumeError<TransportSinkError, TransportStreamError>
{
    fn from(err: ChMuxError<TransportSinkError, TransportStreamError>) -> Self {
        Self::ChMux(err)
    }
}

impl<TransportSinkError, TransportStreamError> From<ConnectError<TransportSinkError, TransportStreamError>>
    for ConsumeError<TransportSinkError, TransportStreamError>
{
    fn from(err: ConnectError<TransportSinkError, TransportStreamError>) -> Self {
        Self::Connect(err)
    }
}

impl<TransportSinkError, TransportStreamError> From<RecvError>
    for ConsumeError<TransportSinkError, TransportStreamError>
{
    fn from(err: RecvError) -> Self {
        Self::Recv(err)
    }
}

impl<TransportSinkError, TransportStreamError> fmt::Display
    for ConsumeError<TransportSinkError, TransportStreamError>
where
    TransportSinkError: fmt::Display,
    TransportStreamError: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ChMux(err) => write!(f, "chmux error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Recv(err) => write!(f, "receive error: {err}"),
            Self::NoValueReceived => write!(f, "no value was received for consumption"),
        }
    }
}

impl<TransportSinkError, TransportStreamError> Error for ConsumeError<TransportSinkError, TransportStreamError>
where
    TransportSinkError: fmt::Debug + fmt::Display,
    TransportStreamError: fmt::Debug + fmt::Display,
{
}

/// Convenience methods for connection handling.
///
/// This trait is implemented for the return value of any [Connect] method
/// using the default codec and a transport with `'static` lifetime.
#[cfg_attr(docsrs, doc(cfg(feature = "rch")))]
pub trait ConnectExt<T, TransportSinkError, TransportStreamError> {
    /// Establishes the connection and provides a single value to the remote endpoint.
    ///
    /// The value is sent over the base channel and then the base channel is closed.
    /// The connection dispatcher is spawned onto a new task and a warning message is logged
    /// if the connection fails.
    ///
    /// This is intended to be used with the [consume](Self::consume) method on
    /// the remote endpoint.
    fn provide(
        self, value: T,
    ) -> impl Future<Output = Result<(), ProvideError<TransportSinkError, TransportStreamError>>> + Send;

    /// Establishes the connection and consumes a single value from the remote endpoint.
    ///
    /// The value is received over the base channel and then the base channel is closed.
    /// The connection dispatcher is spawned onto a new task and a warning message is logged
    /// if the connection fails.
    ///
    /// This is intended to be used with the [provide](Self::provide) method on
    /// the remote endpoint.
    fn consume(
        self,
    ) -> impl Future<Output = Result<T, ConsumeError<TransportSinkError, TransportStreamError>>> + Send;
}

#[cfg(feature = "default-codec-set")]
impl<TransportSinkError, TransportStreamError, T, ConnectFuture>
    ConnectExt<T, TransportSinkError, TransportStreamError> for ConnectFuture
where
    T: RemoteSend,
    TransportSinkError: Send + Error + 'static,
    TransportStreamError: Send + Error + 'static,
    ConnectFuture: Future<
            Output = Result<
                (
                    Connect<'static, TransportSinkError, TransportStreamError>,
                    base::Sender<T, crate::codec::Default>,
                    base::Receiver<T, crate::codec::Default>,
                ),
                ConnectError<TransportSinkError, TransportStreamError>,
            >,
        > + Send,
{
    async fn provide(self, value: T) -> Result<(), ProvideError<TransportSinkError, TransportStreamError>> {
        use tracing::Instrument;

        let (mut conn, mut tx, _) = self.await?;

        tokio::select! {
            biased;
            res = &mut conn => res?,
            res = tx.send(value) => res?,
        }

        exec::spawn(conn.in_current_span());

        Ok(())
    }

    async fn consume(self) -> Result<T, ConsumeError<TransportSinkError, TransportStreamError>> {
        use tracing::Instrument;

        let (mut conn, _, mut rx) = self.await?;

        let value = tokio::select! {
            biased;
            res = &mut conn => {
                res?;
                return Err(ConsumeError::NoValueReceived);
            },
            res = rx.recv() => {
                match res? {
                    Some(value) => value,
                    None => return Err(ConsumeError::NoValueReceived),
                }
            }
        };

        exec::spawn(conn.in_current_span());

        Ok(value)
    }
}
