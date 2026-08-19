//! Initial connection functions.

use bytes::Bytes;
use futures::{
    Future, FutureExt, Sink, Stream, StreamExt, TryStreamExt,
    future::{BoxFuture, LocalBoxFuture},
};
use std::{
    convert::Infallible,
    error::Error,
    fmt, io,
    pin::Pin,
    sync::{Arc, atomic::AtomicBool},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::{
    RemoteSend,
    chmux::{ChMux, ChMuxError},
    codec,
    rch::base,
};

pub mod ext;
mod io_transport;
#[cfg(doc)]
pub mod transports;

/// Error occurred during establishing a connection over a physical transport.
#[derive(Debug, Clone)]
pub enum ConnectError<TransportSinkError, TransportStreamError> {
    /// Establishing [chmux](crate::chmux) connection failed.
    ChMux(ChMuxError<TransportSinkError, TransportStreamError>),
    /// Opening initial [remote](crate::rch::base) channel failed.
    Connect(base::ConnectError),
}

impl<TransportSinkError, TransportStreamError> fmt::Display
    for ConnectError<TransportSinkError, TransportStreamError>
where
    TransportSinkError: fmt::Display,
    TransportStreamError: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ChMux(err) => write!(f, "chmux error: {err}"),
            Self::Connect(err) => write!(f, "channel connect failed: {err}"),
        }
    }
}

impl<TransportSinkError, TransportStreamError> Error for ConnectError<TransportSinkError, TransportStreamError>
where
    TransportSinkError: Error,
    TransportStreamError: Error,
{
}

impl<TransportSinkError, TransportStreamError> From<ChMuxError<TransportSinkError, TransportStreamError>>
    for ConnectError<TransportSinkError, TransportStreamError>
{
    fn from(err: ChMuxError<TransportSinkError, TransportStreamError>) -> Self {
        Self::ChMux(err)
    }
}

impl<TransportSinkError, TransportStreamError> From<base::ConnectError>
    for ConnectError<TransportSinkError, TransportStreamError>
{
    fn from(err: base::ConnectError) -> Self {
        Self::Connect(err)
    }
}

/// Methods for establishing a connection over a physical transport.
///
/// You must poll the returned [Connect] future or spawn it onto a task for the connection to work.
/// If you only want to exchange a single value, such as an [RTC](crate::rtc) client, the
/// [provide](crate::ConnectExt::provide) and [consume](crate::ConnectExt::consume) methods
/// from the [ConnectExt](crate::ConnectExt) trait do this for you.
///
/// # Physical transport
///
/// All functionality in Remoc requires that a connection over a physical
/// transport is established.
/// The underlying transport can either be of packet type (implementing [Sink] and [Stream])
/// or a socket-like object (implementing [AsyncRead] and [AsyncWrite]).
/// In both cases it must be ordered and reliable.
/// That means that all packets must arrive in the order they have been sent
/// and no packets must be lost.
/// The maximum packet size can be limited, see [the configuration](crate::Cfg) for that.
///
/// [TCP] is an example of an underlying transport that is suitable.
/// But there are many more candidates, for example, [UNIX domain sockets],
/// [pipes between processes], [serial links], [Bluetooth L2CAP streams], etc.
///
/// The [connect functions](Connect) are used to establish a
/// [base channel connection](crate::rch::base) over a physical transport.
/// Then, additional channels can be opened by sending either the sender or receiver
/// half of them over the established base channel or another connected channel.
/// See the examples in the [remote channel module](crate::rch) for details.
///
/// [Sink]: futures::Sink
/// [Stream]: futures::Stream
/// [AsyncRead]: tokio::io::AsyncRead
/// [AsyncWrite]: tokio::io::AsyncWrite
/// [TCP]: https://docs.rs/tokio/1.12.0/tokio/net/struct.TcpStream.html
/// [UNIX domain sockets]: https://docs.rs/tokio/1.12.0/tokio/net/struct.UnixStream.html
/// [pipes between processes]: https://docs.rs/tokio/1.12.0/tokio/process/struct.Child.html
/// [serial links]: https://docs.rs/tokio-serial/5.4.1/tokio_serial/
/// [Bluetooth L2CAP streams]: https://docs.rs/bluer/0.10.4/bluer/l2cap/struct.Stream.html
///
/// # Convenience functions
///
/// Methods from the [ConnectExt](crate::ConnectExt) trait can be used on the return values
/// of all connect methods.
/// They streamline connection handling when a single value, such as a [RTC](crate::rtc) client,
/// should be exchanged over the connection and the flexibility of a base channel is not necessary.
///
/// [ConnectExt::provide](crate::ConnectExt::provide) sends a single value to the remote endpoint
/// and [ConnectExt::consume](crate::ConnectExt::consume) receives it there.
/// Both establish the connection and spawn its dispatcher, so no [Connect] future is left to
/// take care of.
/// See the [ConnectExt](crate::ConnectExt) documentation for a worked example using
/// [remote trait calling](crate::rtc).
///
/// # Example
///
/// In the following example the server listens on TCP port 9875 and the client connects to it.
/// Then both ends establish a Remoc connection using [Connect::io] over the TCP connection.
/// The connection dispatchers are spawned onto new tasks and the `client` and `server` functions
/// are called with the established [base channel](crate::rch::base).
///
/// See also the [ConnectExt example](crate::ConnectExt#example), especially if you want to
/// exchange an [remotely callable trait](crate::rtc) client.
///
/// ```
/// use std::net::Ipv4Addr;
/// use tokio::net::{TcpStream, TcpListener};
/// use remoc::prelude::*;
///
/// #[tokio::main]
/// async fn main() {
///     // For demonstration we run both client and server in
///     // the same process. In real life connect_client() and
///     // connect_server() would run on different machines.
///     tokio::join!(connect_client(), connect_server());
/// }
///
/// // This would be run on the client.
/// async fn connect_client() {
///     // Wait for server to be ready.
///     tokio::time::sleep(std::time::Duration::from_secs(1)).await;
///
///     // Establish TCP connection.
///     let socket = TcpStream::connect((Ipv4Addr::LOCALHOST, 9875)).await.unwrap();
///     let (socket_rx, socket_tx) = socket.into_split();
///
///     // Establish Remoc connection over TCP.
///     let (conn, tx, rx) =
///         remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await.unwrap();
///     tokio::spawn(conn);
///
///     // Run client.
///     client(tx, rx).await;
/// }
///
/// // This would be run on the server.
/// async fn connect_server() {
///     // Listen for incoming TCP connection.
///     let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 9875)).await.unwrap();
///     let (socket, _) = listener.accept().await.unwrap();
///     let (socket_rx, socket_tx) = socket.into_split();
///
///     // Establish Remoc connection over TCP.
///     let (conn, tx, rx) =
///         remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await.unwrap();
///     tokio::spawn(conn);
///
///     // Run server.
///     server(tx, rx).await;
/// }
///
/// // This would be run on the client.
/// async fn client(mut tx: rch::base::Sender<u16>, mut rx: rch::base::Receiver<String>) {
///     tx.send(1).await.unwrap();
///     assert_eq!(rx.recv().await.unwrap(), Some("1".to_string()));
/// }
///
/// // This would be run on the server.
/// async fn server(mut tx: rch::base::Sender<String>, mut rx: rch::base::Receiver<u16>) {
///     while let Some(number) = rx.recv().await.unwrap() {
///         tx.send(number.to_string()).await.unwrap();
///     }
/// }
/// ```
#[must_use = "You must poll or spawn the Connect future for the connection to work."]
pub struct Connect<F>(F);

/// A [connection](Connect) with a boxed connection future.
///
/// Obtain it using [Connect::boxed].
pub type BoxConnect<'transport, TransportSinkError, TransportStreamError> =
    Connect<BoxFuture<'transport, Result<(), ChMuxError<TransportSinkError, TransportStreamError>>>>;

/// A [connection](Connect) with a boxed connection future that is not [Send].
///
/// Obtain it using [Connect::boxed_local].
pub type LocalBoxConnect<'transport, TransportSinkError, TransportStreamError> =
    Connect<LocalBoxFuture<'transport, Result<(), ChMuxError<TransportSinkError, TransportStreamError>>>>;

impl Connect<()> {
    /// Establishes a connection over a framed transport (a [sink](Sink) and a [stream](Stream) of binary data) and
    /// returns a remote [sender](base::Sender) and [receiver](base::Receiver).
    ///
    /// This establishes a [chmux](crate::chmux) connection over the transport and opens a remote channel.
    ///
    /// You must poll the returned [Connect] future or spawn it for the connection to work.
    /// Alternatively, use [provide](crate::ConnectExt::provide) or
    /// [consume](crate::ConnectExt::consume) to exchange a single value over the connection,
    /// which takes care of this for you.
    ///
    /// # Panics
    /// Panics if the chmux configuration is invalid.
    pub async fn framed<TransportSink, TransportSinkError, TransportStream, TransportStreamError, Tx, Rx, Codec>(
        cfg: crate::Cfg, transport_sink: TransportSink, transport_stream: TransportStream,
    ) -> Result<
        (
            Connect<
                impl Future<Output = Result<(), ChMuxError<TransportSinkError, TransportStreamError>>> + Unpin,
            >,
            base::Sender<Tx, Codec>,
            base::Receiver<Rx, Codec>,
        ),
        ConnectError<TransportSinkError, TransportStreamError>,
    >
    where
        TransportSink: Sink<Bytes, Error = TransportSinkError> + Unpin,
        TransportSinkError: Error + Send + Sync + 'static,
        TransportStream: Stream<Item = Result<Bytes, TransportStreamError>> + Unpin,
        TransportStreamError: Error + Send + Sync + 'static,
        Tx: RemoteSend,
        Rx: RemoteSend,
        Codec: codec::Codec,
    {
        let (mux, client, mut listener) = ChMux::new(cfg, transport_sink, transport_stream).await?;
        let mut connection = Connect(Box::pin(mux.run()));

        tokio::select! {
            biased;
            Err(err) = &mut connection => Err(err.into()),
            result = base::connect(&client, &mut listener) => {
                match result {
                    Ok((tx, rx)) => Ok((connection, tx, rx)),
                    Err(err) => Err(err.into()),
                }
            }
        }
    }

    /// Establishes a buffered connection over an IO transport (an [AsyncRead] and [AsyncWrite]) and
    /// returns a remote [sender](base::Sender) and [receiver](base::Receiver).
    ///
    /// A [chmux](crate::chmux) connection is established over the transport and a remote channel is opened.
    /// This prepends a length header to each chmux packet for transportation over the unframed connection.
    ///
    /// This method performs internal buffering of reads and writes with the buffer size specified
    /// by [`Cfg::io_buffer_size`](crate::Cfg::io_buffer_size).
    ///
    /// You must poll the returned [Connect] future or spawn it for the connection to work.
    /// Alternatively, use [provide](crate::ConnectExt::provide) or
    /// [consume](crate::ConnectExt::consume) to exchange a single value over the connection,
    /// which takes care of this for you.
    ///
    /// # Panics
    /// Panics if the chmux configuration is invalid.
    pub async fn io<Read, Write, Tx, Rx, Codec>(
        mut cfg: crate::Cfg, input: Read, output: Write,
    ) -> Result<
        (
            Connect<impl Future<Output = Result<(), ChMuxError<io::Error, io::Error>>> + Unpin>,
            base::Sender<Tx, Codec>,
            base::Receiver<Rx, Codec>,
        ),
        ConnectError<io::Error, io::Error>,
    >
    where
        Read: AsyncRead + Unpin,
        Write: AsyncWrite + Unpin,
        Tx: RemoteSend,
        Rx: RemoteSend,
        Codec: codec::Codec,
    {
        let varint = Arc::new(AtomicBool::new(false));

        let encoder = io_transport::LengthCodec::new(u32::MAX, varint.clone());
        let transport_sink = io_transport::FilterFlushOuter(FramedWrite::with_capacity(
            io_transport::FilterFlushInner::new(output),
            encoder,
            cfg.io_buffer_size,
        ));

        let decoder = io_transport::LengthCodec::new(cfg.max_frame_length(), varint.clone());
        let transport_stream =
            FramedRead::with_capacity(input, decoder, cfg.io_buffer_size).map_ok(|item| item.freeze());

        cfg.io_frame_len_varint = Some(varint);

        Connect::framed(cfg, transport_sink, transport_stream).await
    }
}

impl<F, TransportSinkError, TransportStreamError> Connect<F>
where
    F: Future<Output = Result<(), ChMuxError<TransportSinkError, TransportStreamError>>> + Unpin,
{
    /// Boxes the connection future.
    pub fn boxed<'transport>(self) -> BoxConnect<'transport, TransportSinkError, TransportStreamError>
    where
        F: Send + 'transport,
    {
        Connect(self.0.boxed())
    }

    /// Boxes the connection future, without requiring it to be [Send].
    pub fn boxed_local<'transport>(self) -> LocalBoxConnect<'transport, TransportSinkError, TransportStreamError>
    where
        F: 'transport,
    {
        Connect(self.0.boxed_local())
    }
}

impl<F: Future + Unpin> Future for Connect<F> {
    /// Result of connection after it has been terminated.
    type Output = F::Output;

    /// This future runs the dispatcher for this connection.
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Pin::into_inner(self).0.poll_unpin(cx)
    }
}

type LoopbackSendError = futures::channel::mpsc::SendError;
type LoopbackRecvError = Infallible;

/// A loopback connection.
pub type LoopbackConnect = BoxConnect<'static, LoopbackSendError, LoopbackRecvError>;

impl LoopbackConnect {
    /// Establishes a connection over a local loopback transport and
    /// returns a [sender](base::Sender) and [receiver](base::Receiver).
    ///
    /// This establishes a [chmux](crate::chmux) connection over the loopback transport and opens a channel.
    ///
    /// You must poll the returned [Connect] future or spawn it for the connection to work.
    ///
    /// # Panics
    /// Panics if the chmux configuration is invalid.
    pub async fn loopback<Tx, Rx, Codec>(
        cfg: crate::Cfg,
    ) -> (LoopbackConnect, base::Sender<Tx, Codec>, base::Receiver<Rx, Codec>)
    where
        Tx: RemoteSend,
        Rx: RemoteSend,
        Codec: codec::Codec,
    {
        let (a_transport_tx, a_transport_rx) = futures::channel::mpsc::channel(cfg.transport_send_queue);
        let (b_transport_tx, b_transport_rx) = futures::channel::mpsc::channel(cfg.transport_send_queue);

        let a_transport_rx = a_transport_rx.map(Ok);
        let b_transport_rx = b_transport_rx.map(Ok);

        let ((a_connect, a_base_tx, _a_base_rx), (b_connect, _b_base_tx, b_base_rx)) = tokio::try_join!(
            Connect::framed::<_, _, _, _, _, (), _>(cfg.clone(), a_transport_tx, b_transport_rx),
            Connect::framed::<_, _, _, _, (), _, _>(cfg.clone(), b_transport_tx, a_transport_rx),
        )
        .unwrap();

        let connection = Connect(
            async move {
                tokio::try_join!(a_connect, b_connect)?;
                Ok(())
            }
            .boxed(),
        );

        (connection, a_base_tx, b_base_rx)
    }
}
