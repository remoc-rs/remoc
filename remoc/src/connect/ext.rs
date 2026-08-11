//! Connection extensions.

use std::{error::Error, fmt, future::Future};

use crate::{
    chmux::ChMuxError,
    connect::ConnectError,
    exec::{self, MaybeSend},
    rch::base::{RecvError, SendError},
};

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
/// This trait is implemented for the return value of the [Connect::io] and [Connect::framed]
/// methods, when the default codec is used and the transport has `'static` lifetime.
///
/// [provide](Self::provide) and [consume](Self::consume) are a matching pair:
/// one endpoint provides a single value and the other consumes it.
/// This is especially convenient together with [remote trait calling](crate::rtc),
/// where the server provides the client of a remote object and the client consumes it.
/// Both methods spawn the connection dispatcher, so there is no connection future
/// left to take care of.
///
/// # Example
///
/// In the following example the trait `Counter` is made remotely callable.
/// The server listens on TCP port 9878 and the client connects to it.
///
/// The server creates a `CounterObj`, obtains a `CounterServerSharedMut` and a
/// `CounterClient` for it and *provides* the client to the remote endpoint.
/// The client *consumes* that client and calls trait methods on it.
///
/// A fully worked version of this example, split into separate client and server crates,
/// is available in the
/// [examples directory](https://github.com/remoc-rs/remoc/tree/master/examples/rtc).
#[cfg_attr(
    feature = "rtc",
    doc = r##"
```
use std::{net::Ipv4Addr, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use remoc::prelude::*;
use remoc::rtc::CallError;

// Trait defining the remote service.
#[rtc::remote]
pub trait Counter {
    async fn value(&self) -> Result<u32, CallError>;
    async fn increase(&mut self, by: u32) -> Result<(), CallError>;
}

// Server implementation object.
pub struct CounterObj {
    value: u32,
}

impl Counter for CounterObj {
    async fn value(&self) -> Result<u32, CallError> {
        Ok(self.value)
    }

    async fn increase(&mut self, by: u32) -> Result<(), CallError> {
        self.value += by;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    // For demonstration we run both client and server in
    // the same process. In real life connect_client() and
    // connect_server() would run on different machines.
    tokio::join!(connect_client(), connect_server());
}

// This would be run on the client.
async fn connect_client() {
    // Wait for server to be ready.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Establish TCP connection.
    let socket = TcpStream::connect((Ipv4Addr::LOCALHOST, 9878)).await.unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Establish Remoc connection over TCP and consume (i.e. receive)
    // the counter client from the server.
    let mut counter: CounterClient =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
            .consume()
            .await
            .unwrap();

    // Call methods on the remote object.
    assert_eq!(counter.value().await.unwrap(), 0);
    counter.increase(5).await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 5);
}

// This would be run on the server.
async fn connect_server() {
    // Listen for incoming TCP connection.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 9878)).await.unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Create the server and client for the counter object.
    //
    // Current limitations of the Rust compiler require that we explicitly
    // specify the codec.
    let counter_obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
    let (server, client) =
        CounterServerSharedMut::<_, remoc::codec::Default>::new(counter_obj, 1);

    // Establish Remoc connection over TCP and provide (i.e. send)
    // the counter client to the client.
    remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
        .provide(client)
        .await
        .unwrap();

    // Serve incoming method calls until the client disconnects.
    server.serve(true).await.unwrap();
}
```
"##
)]
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
    ) -> impl Future<Output = Result<(), ProvideError<TransportSinkError, TransportStreamError>>> + MaybeSend;

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
    ) -> impl Future<Output = Result<T, ConsumeError<TransportSinkError, TransportStreamError>>> + MaybeSend;
}

impl<TransportSinkError, TransportStreamError, T, ConnectFuture, ConnectionFuture>
    ConnectExt<T, TransportSinkError, TransportStreamError> for ConnectFuture
where
    T: RemoteSend,
    TransportSinkError: Send + Error + 'static,
    TransportStreamError: Send + Error + 'static,
    ConnectionFuture: Future<Output = Result<(), ChMuxError<TransportSinkError, TransportStreamError>>>
        + Unpin
        + MaybeSend
        + 'static,
    ConnectFuture: Future<
            Output = Result<
                (
                    Connect<ConnectionFuture>,
                    base::Sender<T, crate::codec::Default>,
                    base::Receiver<T, crate::codec::Default>,
                ),
                ConnectError<TransportSinkError, TransportStreamError>,
            >,
        > + MaybeSend,
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
