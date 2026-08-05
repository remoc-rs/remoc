//! Transport examples.
//!
//! Remoc implements no transport itself; it runs over any byte stream, i.e. any
//! [AsyncRead] and [AsyncWrite] pair passed to [Connect::io], or any [Sink] and
//! [Stream] of [Bytes](bytes::Bytes) passed to [Connect::framed].
//! Consequently Remoc has no dependency on any particular networking crate and
//! never constrains which version of one you use.
//!
//! This module collects worked examples for common transports.
//!
//! A [UNIX domain socket](https://docs.rs/tokio/1/tokio/net/struct.UnixStream.html)
//! is used exactly like [TCP](tcp), and any other [AsyncRead] and [AsyncWrite] pair,
//! such as a [serial link](https://docs.rs/tokio-serial) or a
//! [Bluetooth L2CAP stream](https://docs.rs/bluer), follows the same pattern.
//!
//! If the underlying network is unreliable, see [aggligator], which keeps a Remoc
//! connection alive across link failures.
//!
//! [AsyncRead]: tokio::io::AsyncRead
//! [AsyncWrite]: tokio::io::AsyncWrite
//! [Sink]: futures::Sink
//! [Stream]: futures::Stream
//! [Connect::io]: super::Connect::io
//! [Connect::framed]: super::Connect::framed

// The snippet files in this directory are not modules of Remoc.
// They are embedded into the documentation below and compiled by examples/transports.

/// Aggregated and resilient links, using [Aggligator](https://docs.rs/aggligator).
///
/// Aggligator combines multiple links, for example over several network interfaces,
/// into a single connection with their combined bandwidth.
/// Individual links may fail and be reconnected while the connection itself stays
/// alive, so a Remoc connection running over it survives network outages without
/// losing any of its channels or remote objects.
///
/// The aggregated connection is an [AsyncRead](tokio::io::AsyncRead) and
/// [AsyncWrite](tokio::io::AsyncWrite) pair, so it is passed to
/// [Connect::io](super::Connect::io) like a plain socket.
/// Besides TCP, [transports](https://crates.io/keywords/aggligator-transport) for
/// TLS, WebSocket, SOCKS5, USB and Bluetooth are available.
///
/// Take care that [Cfg::connection_timeout](crate::Cfg::connection_timeout), 150 seconds
/// by default, exceeds the time Aggligator keeps a connection without a working link,
/// 120 seconds by default, otherwise Remoc closes the connection during an outage that
/// Aggligator would have recovered from.
#[doc = "```ignore"]
#[doc = include_str!("aggligator.rs")]
#[doc = "```"]
pub mod aggligator {}

/// TCP.
///
/// A [TcpStream](https://docs.rs/tokio/1/tokio/net/struct.TcpStream.html) is split into
/// its reading and writing half and handed to [Connect::io](super::Connect::io).
#[doc = "```ignore"]
#[doc = include_str!("tcp.rs")]
#[doc = "```"]
pub mod tcp {}

/// TLS, using [tokio-rustls](https://docs.rs/tokio-rustls).
///
/// A TLS stream is an [AsyncRead](tokio::io::AsyncRead) and
/// [AsyncWrite](tokio::io::AsyncWrite), so it is split and handed to
/// [Connect::io](super::Connect::io) unchanged.
///
/// # Client
#[doc = "```ignore"]
#[doc = include_str!("tls_client.rs")]
#[doc = "```"]
///
/// # Server
#[doc = "```ignore"]
#[doc = include_str!("tls_server.rs")]
#[doc = "```"]
pub mod tls {}

/// WebSocket, using [tokio-tungstenite](https://docs.rs/tokio-tungstenite) or
/// [axum](https://docs.rs/axum).
///
/// A WebSocket is message-oriented rather than byte-oriented, so it is used with
/// [Connect::framed](super::Connect::framed) and each chmux packet is carried as one
/// binary message.
/// The adaptation must use [futures::future::ready] rather than an `async` block,
/// because [Connect::framed](super::Connect::framed) requires an [Unpin] transport.
///
/// # Client
#[doc = "```ignore"]
#[doc = include_str!("websocket_client.rs")]
#[doc = "```"]
///
/// # Server
#[doc = "```ignore"]
#[doc = include_str!("websocket_axum.rs")]
#[doc = "```"]
pub mod websocket {}

/// Pipes to a child process.
///
/// This runs Remoc between a parent and a child process over the child's standard
/// input and output, without involving the network stack at all.
#[doc = "```ignore"]
#[doc = include_str!("process.rs")]
#[doc = "```"]
pub mod process {}
