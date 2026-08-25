#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/remoc-rs/remoc/master/.misc/Remoc.png",
    html_favicon_url = "https://raw.githubusercontent.com/remoc-rs/remoc/master/.misc/Remoc.png"
)]

//! Remoc 🦑 — remote multiplexed objects and channels
//!
//! Remoc makes remote interaction between Rust programs seamless and smooth.
//! Over a [single underlying transport], such as TCP or TLS, it provides:
//!
//!   * [multiple channels] of different types like [MPSC], [oneshot], [watch], etc.,
//!   * [remote synchronization] primitives,
//!   * calling of [remote functions] and [trait methods] on a remote object (RPC),
//!   * [remotely observable collections].
//!
//! Remoc is written in 100% safe Rust, builds upon [Tokio], and uses [Serde]
//! with the compact, forward- and backward-compatible [Postbag] binary codec.
//! Remoc does not depend on any particular [transport type].
//!
//! [single underlying transport]: Connect
//! [multiple channels]: rch
//! [MPSC]: rch::mpsc
//! [oneshot]: rch::oneshot
//! [watch]: rch::watch
//! [remote synchronization]: robj
//! [remote functions]: rfn
//! [trait methods]: rtc
//! [Tokio]: tokio
//! [Serde]: serde
//! [Postbag]: codec::Postbag
//! [transport type]: Connect
//! [remotely observable collections]: robs
//!
//! # Introduction
//!
//! A common pattern in Rust programs is to use channels to communicate between
//! threads and async tasks.
//! Setting up a channel is done in a single line and it largely avoids the need
//! for shared state and the associated complexity.
//! Remoc extends this programming model to distributed systems by providing
//! channels that work seamlessly over remote connections.
//!
//! For that it uses Serde and the [Postbag] binary codec to
//! serialize and deserialize data as it is transmitted over an underlying
//! transport, which might be a [TCP network connection], a [WebSocket],
//! [UNIX pipe], or even a [serial link].
//! Postbag is designed for protocol evolution, allowing many changes to message
//! types without requiring both endpoints to be upgraded at once.
//!
//! Opening a new [channel] is straightforward, just send the sender or receiver half
//! of the new channel over an existing channel, like you would do between local
//! threads and tasks.
//! All channels are multiplexed over the same remote connection, with data being
//! transmitted in chunks to avoid one channel blocking another if a large message
//! is transmitted.
//!
//! Building upon its remote channels, Remoc allows calling of [remote functions] and
//! closures.
//! Furthermore, a trait can be made [remotely callable] with automatically generated
//! client and server implementations, resembling a classical remote procedure
//! calling (RPC) model.
//!
//! [TCP network connection]: transports::tcp
//! [WebSocket]: transports::websocket
//! [UNIX pipe]: transports::process
//! [serial link]: https://docs.rs/tokio-serial
//! [channel]: rch
//! [remotely callable]: rtc
//!
//! # When to use Remoc
//!
//! Remoc is a good fit once two or more Rust programs need to interact and
//! you would rather express that interaction as channels, function calls and
//! trait objects than design and maintain a custom wire protocol.
//! Typical uses include splitting an application into cooperating processes,
//! talking to a sandboxed child process, connecting a UI to a backend it does
//! not share memory with, or driving a service from Rust code compiled to
//! WebAssembly. The processes can either run on the same machine or talk
//! to each other via the network.
//!
//! Remoc is *not*:
//!
//!   * a service mesh -- it connects exactly two endpoints over one transport
//!     connection that you provide; discovery, load balancing and routing
//!     between more than two endpoints are outside its scope;
//!   * a message broker -- channels and remote objects live only as long as
//!     the connection and the process holding them; nothing is persisted or
//!     replayed after a restart;
//!   * a network security layer -- Remoc neither encrypts nor authenticates
//!     the connection itself, see the security section below;
//!   * a cross-language protocol -- both endpoints of a connection run Rust
//!     code using Remoc.
//!
//! Remoc pays off once you want function calling, bidirectional streams,
//! callbacks, or to pass live channels and objects between endpoints as
//! naturally as you would locally.
//!
//! # Getting started
//!
//! A Remoc application normally follows these steps:
//!
//! 1. Establish an ordered, reliable transport, such as a TCP or TLS stream.
//! 2. Call [Connect::io] for a byte stream or [Connect::framed] for a stream of
//!    binary messages.
//! 3. Spawn the returned connection future so it can drive all communication.
//! 4. Use the returned [base channel](rch::base), or exchange one initial value
//!    with [ConnectExt::provide] and [ConnectExt::consume].
//! 5. Send additional channel halves, RTC clients, or remote objects wherever
//!    they are needed.
//!
//! The [channel example](#channels) below demonstrates the base-channel approach
//! in one process and the [RPC example](#remote-procedure-calls) the remote trait
//! calling approach. For separate client and server crates, see the
//! [RTC example](https://github.com/remoc-rs/remoc/tree/master/examples/rtc).
//!
//! ## Connecting
//!
//! Remoc implements no transport itself and thus depends on no networking crate;
//! it runs over any byte stream you already have.
//! Pass an [AsyncRead] and [AsyncWrite] pair, such as a TCP or TLS connection, to
//! [Connect::io], or a [Sink] and [Stream] of binary packets, such as a WebSocket,
//! to [Connect::framed].
//! Both hand you a [base channel](rch::base), over which all further channels and
//! remote objects are exchanged.
//!
//! If a single value is all you need to exchange, for example an [RTC](rtc) client,
//! [ConnectExt::provide] and [ConnectExt::consume] establish the connection, transfer that
//! value and spawn the connection dispatcher for you.
//!
//! The [transports] module contains worked examples for TCP, TLS, WebSocket,
//! pipes to a child process and aggregated, failure-resilient links.
//!
//! [AsyncRead]: tokio::io::AsyncRead
//! [AsyncWrite]: tokio::io::AsyncWrite
//! [Sink]: futures::Sink
//! [Stream]: futures::Stream
//!
//! ## Connection lifecycle
//!
//! [Connect::io] and [Connect::framed] return a dispatcher future alongside
//! the base channel; nothing is sent or received until it is polled, usually
//! by spawning it as shown in the example below.
//! The dispatcher terminates, resolving that future, once the transport is
//! closed or once every channel, sender and receiver derived from the
//! connection on both endpoints has been dropped.
//! Transport and protocol errors, including a connection timeout, terminate
//! the connection early and cause pending channel operations on the affected
//! endpoint to fail. Serialization and item-size errors are reported by the
//! channel operation that encountered them.
//!
//! # Choosing channels, functions, objects or traits
//!
//! Everything in this crate builds on [remote channels](rch); which
//! higher-level building block to reach for depends on how your interaction
//! is shaped:
//!
//!   * a [remote channel](rch) directly, to stream a sequence of values in
//!     one or both directions, for example events, log lines or a stream of
//!     computed values;
//!   * a [remote function](rfn), to expose a single async function or
//!     closure without declaring a trait;
//!   * [remote trait calling](rtc), when a remote endpoint should expose
//!     several related methods, optionally backed by shared mutable state;
//!     it generates a client and server for you from an ordinary trait;
//!   * a [remote object](robj), when it is a value's identity, or its
//!     lazily-fetched contents, that must cross the connection rather than a
//!     stream of updates -- see its
//!     [type-choosing table](crate::robj#choosing-an-object-type);
//!   * an [observable collection](robs), when a remote endpoint needs a
//!     live, read-only mirror of a map, set, list or vector that changes
//!     over time.
//!
//! These combine freely: an RTC method can take or return a channel, and a
//! channel's item can contain a remote object.
//!
//! ## Local use
//!
//! The channels in [rch], [RTC](rtc) clients and [remote objects](robj) also work when
//! both halves stay within one process, without establishing a connection at all.
//! Values are then passed directly and are serialized only once a half has actually
//! been sent to a remote endpoint, making local use nearly as cheap as the
//! corresponding [tokio::sync] channel.
//!
//! Thus the same code can run in one process or split over a connection, making it a
//! deployment decision whether a component is local or remote.
//! It also allows a client and server design to be tested without any transport.
//! The involved types must implement [RemoteSend] either way.
//!
//! # Forward and backward compatibility
//!
//! Distributed systems often require that endpoints running different software
//! versions interact.
//! Remoc therefore uses the full [Postbag codec](codec::Postbag) by default.
//! It includes field and variant identifiers and the encoded length of each
//! value, allowing a receiver to skip data it does not know.
//!
//! With suitable Serde attributes, Postbag supports common schema changes:
//!
//!   * fields can be added, removed, or reordered; whenever a receiver expects
//!     a field that the sender omits, that field needs `#[serde(default)]`;
//!   * enum variants can be added, removed, or reordered; an older receiver
//!     needs a `#[serde(other)]` variant to accept an unknown one;
//!   * fields and variants can be renamed without breaking compatibility when
//!     they use stable numbered identifiers such as `#[serde(rename = "_0")]`.
//!
//! An identifier is part of the protocol: changing it is breaking, and an
//! identifier retired from one field or variant must not later be reused for
//! another. Changes to a field's type and other structural transformations are
//! not automatically compatible.
//!
//! Use [`codec::recoverable`] to confine an
//! incompatible field to its default value, or the [versioned] module when the
//! old representation must be transformed explicitly.
//!
//! See [`codec::Postbag`] and the [Postbag documentation](postbag) for the
//! complete compatibility table and format limitations.
//!
//! # Security
//!
//! Remoc neither encrypts nor authenticates the connection; it is designed
//! to run on top of a transport that already provides the properties you
//! need. If a connection crosses a trust boundary, wrap the transport in TLS
//! (see [transports::tls]) or another secure channel before passing it to
//! [Connect::io] or [Connect::framed].
//!
//! When exchanging data with an untrusted or unauthenticated endpoint, also
//! review the [size considerations](crate::rch#size-considerations) in the
//! remote channel module and the
//! [`Cfg::max_ports`](crate::Cfg::max_ports) and
//! [`Cfg::connect_queue`](crate::Cfg::connect_queue) settings, which
//! bound how many channels a peer can make you open.
//!
//! # Tracing
//!
//! Remoc uses the [Tracing crate](tracing) for logging of events.
//! Setting the log level to `TRACE` logs multiplexer lifetime events and messages as they are being processed.
//!
//! # Example
//!
//! The following examples show the two most common styles: exchanging values over
//! channels, and calling methods on a remote object.
//!
//! ## Channels
//!
//! In the following example the server listens on TCP port 9870 and the client connects to it.
//! Then both ends establish a Remoc connection using [Connect::io] over the TCP connection.
//! The connection dispatchers are spawned onto new tasks and the `client()` and `server()` functions
//! are called with the established [base channel](crate::rch::base).
//!
//! Then, the client creates a new [remote MPSC channel](crate::rch::mpsc) and sends it inside
//! a count request to the server.
//! The server receives the count request and counts on the provided channel.
//! The client receives each counted number over the new channel.
//!
#![cfg_attr(
    feature = "rch",
    doc = r##"
```
use std::net::Ipv4Addr;
use tokio::net::{TcpStream, TcpListener};
use remoc::prelude::*;

#[tokio::main]
async fn main() {
    // For demonstration we run both client and server in
    // the same process. In real life connect_client() and
    // connect_server() would run on different machines.
    tokio::join!(connect_client(), connect_server());
}

// This would be run on the client.
// It establishes a Remoc connection over TCP to the server.
async fn connect_client() {
    // Wait for server to be ready.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Establish TCP connection.
    let socket =
        TcpStream::connect((Ipv4Addr::LOCALHOST, 9870)).await.unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Establish Remoc connection over TCP.
    // The connection is always bidirectional, but we can just drop
    // the unneeded receiver.
    let (conn, tx, _rx): (_, _, rch::base::Receiver<()>) =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
        .await.unwrap();
    tokio::spawn(conn);

    // Run client.
    client(tx).await;
}

// This would be run on the server.
// It accepts a Remoc connection over TCP from the client.
async fn connect_server() {
    // Listen for incoming TCP connection.
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 9870)).await.unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Establish Remoc connection over TCP.
    // The connection is always bidirectional, but we can just drop
    // the unneeded sender.
    let (conn, _tx, rx): (_, rch::base::Sender<()>, _) =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
        .await.unwrap();
    tokio::spawn(conn);

    // Run server.
    server(rx).await;
}

// User-defined data structures needs to implement Serialize
// and Deserialize.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CountReq {
    up_to: u32,
    // Most Remoc types like channels can be included in serializable
    // data structures for transmission to remote endpoints.
    seq_tx: rch::mpsc::Sender<u32>,
}

// This would be run on the client.
// It sends a count request to the server and receives each number
// as it is counted over a newly established MPSC channel.
async fn client(mut tx: rch::base::Sender<CountReq>) {
    // By sending seq_tx over an existing remote channel, a new remote
    // channel is automatically created and connected to the server.
    // This all happens inside the existing TCP connection.
    let (seq_tx, mut seq_rx) = rch::mpsc::channel();
    tx.send(CountReq { up_to: 4, seq_tx }).await.unwrap();

    // Receive counted numbers over new channel.
    assert_eq!(seq_rx.recv().await.unwrap(), Some(0));
    assert_eq!(seq_rx.recv().await.unwrap(), Some(1));
    assert_eq!(seq_rx.recv().await.unwrap(), Some(2));
    assert_eq!(seq_rx.recv().await.unwrap(), Some(3));
    assert_eq!(seq_rx.recv().await.unwrap(), None);
}

// This would be run on the server.
// It receives a count request from the client and sends each number
// as it is counted over the MPSC channel sender provided by the client.
async fn server(mut rx: rch::base::Receiver<CountReq>) {
    // Receive count request and channel sender to use for counting.
    while let Some(CountReq { up_to, seq_tx }) = rx.recv().await.unwrap() {
        for i in 0..up_to {
            // Send each counted number over provided channel.
            seq_tx.send(i).await.unwrap();
        }
    }
}
```
"##
)]
//!
//! ## Remote procedure calls
//!
//! Channels are the foundation, but a remote endpoint that should expose several
//! related methods is usually better served by [remote trait calling](rtc).
//! Tagging a trait generates a client that implements it and servers that execute
//! the calls on your object.
//!
//! In the following example the server listens on TCP port 9871 and sends the counter
//! client to the client, which then calls the trait methods on it.
//! Each call is transferred over the connection and executed on the counter object
//! held by the server.
//!
#![cfg_attr(
    feature = "rtc",
    doc = r##"
```
use std::{net::Ipv4Addr, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use remoc::prelude::*;
use remoc::rtc::CallError;

// Tagging the trait generates CounterClient and the CounterServer* types.
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
    // the same process.
    tokio::join!(connect_client(), connect_server());
}

// This would be run on the server.
async fn connect_server() {
    // Accept TCP connection.
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, 9871)).await.unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Create the server and its client for the counter object.
    let counter_obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
    let (server, client) =
        CounterServerSharedMut::<_, remoc::codec::Default>::new(counter_obj, 1);

    // Establish the Remoc connection and send the client to the remote endpoint.
    remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
        .provide(client).await.unwrap();

    // Execute the calls made by the remote endpoint on the counter object.
    server.serve(true).await.unwrap();
}

// This would be run on the client.
async fn connect_client() {
    // Wait for server to be ready.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Establish TCP connection.
    let socket =
        TcpStream::connect((Ipv4Addr::LOCALHOST, 9871)).await.unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Establish the Remoc connection and receive the counter client.
    let mut counter: CounterClient =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
            .consume().await.unwrap();

    // CounterClient implements Counter, so calling it looks like a local call,
    // but is executed on the counter object located on the server.
    counter.increase(5).await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 5);
}
```
"##
)]
//!
//! [ConnectExt::provide] and [ConnectExt::consume] establish the connection and
//! transfer the client in one step.
//! When a connection already exists, the client can be sent over any
//! [channel](rch) instead, just like the channel halves above.
//!
//! # Next steps
//!
//! * Browse the [transports] module for adaptable TCP, TLS, WebSocket and
//!   child-process examples, including a resilient, reconnecting transport
//!   for unreliable networks.
//! * Read the [rtc] module documentation, and the fully worked
//!   [RTC example](https://github.com/remoc-rs/remoc/tree/master/examples/rtc)
//!   with client and server split into separate crates, if a remote endpoint
//!   should expose more than a handful of functions.
//!

pub mod prelude;

pub mod chmux;
pub use chmux::Cfg;

#[cfg(feature = "serde")]
pub mod codec;

#[cfg(feature = "serde")]
pub mod versioned;

#[cfg(feature = "rch")]
pub mod rch;

#[cfg(feature = "rch")]
mod remote_send;
#[cfg(feature = "rch")]
pub use remote_send::RemoteSend;

#[cfg(feature = "rch")]
mod connect;
#[cfg(all(doc, feature = "rch"))]
pub use connect::transports;
#[cfg(feature = "rch")]
pub use connect::{
    BoxConnect, Connect, ConnectError, LocalBoxConnect, LoopbackConnect,
    ext::{ConnectExt, ConsumeError, ProvideError},
};

#[cfg(feature = "rfn")]
pub mod rfn;

#[cfg(feature = "robj")]
pub mod robj;

#[cfg(feature = "robs")]
pub mod robs;

#[cfg(feature = "rtc")]
pub mod rtc;

// Re-export serde for remoc_macro used by rtc.
#[doc(hidden)]
#[cfg(feature = "rtc")]
pub use serde as _serde;

#[cfg(any(feature = "rfn", feature = "robj"))]
mod provider;
#[cfg(any(feature = "rfn", feature = "robj"))]
pub use provider::Provider;

#[doc(hidden)]
#[cfg(feature = "rch")]
pub mod doctest;

mod util;
mod varint;
