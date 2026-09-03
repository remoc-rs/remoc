#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/remoc-rs/remoc/master/.misc/Remoc.png",
    html_favicon_url = "https://raw.githubusercontent.com/remoc-rs/remoc/master/.misc/Remoc.png"
)]

//! Remoc 🦑 — RPC, remote multiplexed objects and channels for Rust
//!
//! Remoc makes remote interaction between Rust programs seamless and smooth.
//! It is an RPC library in which a call can also pass channels and remote objects
//! that stay usable after the call has returned.
//!
//! Over a [single underlying transport], such as TCP or TLS, it provides:
//!
//!   * calling of [trait methods] on a remote object (RPC) and of [remote functions],
//!   * [multiple channels] of different types like [MPSC], [oneshot], [watch], etc.,
//!   * [remote synchronization] primitives,
//!   * [remotely observable collections].
//!
//! Remoc is written in 100% safe Rust, builds upon [Tokio], and uses [Serde]
//! with the compact, forward- and backward-compatible [Postbag] binary codec.
//! Remoc does not depend on any particular [transport type].
//!
//! An illustrated overview and benchmarks are available at [remoc.rs](https://remoc.rs).
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
#![cfg_attr(
    feature = "rch",
    doc = r##"
```
# use remoc::prelude::*;
// Most Remoc types, like channel halves, can be part of
// serializable data structures.
#[derive(serde::Serialize, serde::Deserialize)]
struct CountReq {
    up_to: u32,
    seq_tx: rch::mpsc::Sender<u32>,
}

# async fn client(mut tx: rch::base::Sender<CountReq>) {
// Sending the sender half opens a new channel to the remote endpoint,
// inside the connection that is already established.
let (seq_tx, mut seq_rx) = rch::mpsc::channel();
tx.send(CountReq { up_to: 4, seq_tx }).await.unwrap();

// The remote endpoint counts up to 4 over the channel we provided.
while let Some(i) = seq_rx.recv().await.unwrap() {
    println!("{i}");
}
# }
```
"##
)]
//!
//! See the [channel example](#channels) below for the complete, runnable version,
//! including establishing the connection and the code of the remote endpoint.
//!
//! Building upon its remote channels, Remoc allows calling of [remote functions] and
//! closures.
//! Furthermore, a trait can be made [remotely callable] with automatically generated
//! client and server implementations, resembling a classical remote procedure
//! calling (RPC) model; see the [RPC example](#remote-procedure-calls) below.
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
//! The processes can either run on the same machine or talk to each other
//! via the network.
//!
//! Use Remoc to:
//!
//!   * build distributed applications that exchange [live channels](rch)
//!     and [objects](robj), not just messages;
//!   * expose a set of related operations as an ordinary Rust trait and
//!     [call it](rtc) from the other endpoint;
//!   * talk to a sandboxed or otherwise isolated [child process](transports::process);
//!   * connect a UI to a backend it does not share memory with, including
//!     from Rust code compiled to WebAssembly;
//!   * give a remote endpoint a live, read-only
//!     [mirror of a collection](robs) that keeps changing.
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
//! in one process; the [rtc module](rtc#example) does the same for the remote
//! trait calling approach. For separate client and server crates, see the
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
//! | You want to | Use |
//! |---|---|
//! | stream a sequence of values in one or both directions, for example events, log lines or computed values | a [remote channel](rch) |
//! | expose a single async function or closure, without declaring a trait | a [remote function](rfn) |
//! | expose several related methods, optionally backed by shared mutable state | [remote trait calling](rtc) |
//! | move a value's identity, or its lazily-fetched contents, across the connection rather than a stream of updates | a [remote object](robj) |
//! | give a remote endpoint a live, read-only mirror of a map, set, list or vector that changes over time | an [observable collection](robs) |
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
//! # Logging and tracing
//!
//! Remoc uses the [Tracing crate](::tracing) for logging of events.
//! Setting the log level to `TRACE` logs multiplexer lifetime events and messages as they are being processed.
//!
//! Remote trait and function calls can create spans at both endpoints, which are
//! linked into one distributed trace when OpenTelemetry is used; see the
//! [tracing module](crate::tracing).
//!
//! # Example
//!
//! The following example shows a complete Remoc application: two endpoints
//! connected over TCP that exchange values over channels.
//! It is followed by a look at [remote procedure calls](#remote-procedure-calls),
//! the other common style of using Remoc.
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
//! the calls on your object, resembling a classical RPC model while remaining
//! free to pass channels and remote objects through the calls:
//!
#![cfg_attr(
    feature = "rtc",
    doc = r##"
```
use remoc::prelude::*;
use remoc::rtc::CallError;

// Tagging the trait generates CounterClient and the CounterServer* types.
#[rtc::remote]
pub trait Counter {
    async fn value(&self) -> Result<u32, CallError>;

    async fn increase(&mut self, by: u32) -> Result<(), CallError>;

    // Methods can take and return channels and other remote objects.
    async fn watch(&mut self) -> Result<rch::watch::Receiver<u32>, CallError>;
}

// CounterClient implements Counter, so calling it looks like a local call,
// but is executed on the counter object located on the server.
async fn use_counter(mut counter: CounterClient) -> Result<(), CallError> {
    counter.increase(5).await?;
    assert_eq!(counter.value().await?, 5);

    // The watch receiver returned by the call stays connected to the
    // counter object and reports every change made to it.
    let watch_rx = counter.watch().await?;
    assert_eq!(*watch_rx.borrow().unwrap(), 5);

    Ok(())
}
```
"##
)]
//!
//! The client is [remote sendable](RemoteSend), so it can be sent over any
//! [channel](rch), just like the channel halves above, or transferred while
//! establishing the connection using [ConnectExt::provide] and [ConnectExt::consume].
//!
//! See the [rtc module](rtc) for the server side, connecting and a
//! [complete example](rtc#example).
//!
//! ## Transports
//!
//! Browse the [transports module](transports) for adaptable TCP, TLS, WebSocket and
//! child-process examples, including a resilient, reconnecting transport
//! for unreliable networks.
//!

pub mod prelude;

pub mod chmux;
pub use chmux::Cfg;

#[cfg(feature = "serde")]
pub mod codec;

#[cfg(any(feature = "rtc", feature = "rfn"))]
pub mod tracing;
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
#[cfg(feature = "rch")]
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
