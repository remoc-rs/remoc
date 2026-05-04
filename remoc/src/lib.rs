#![cfg_attr(not(feature = "js"), forbid(unsafe_code))]
#![cfg_attr(feature = "js", deny(unsafe_code))]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/ENQT-GmbH/remoc/master/.misc/Remoc.png",
    html_favicon_url = "https://raw.githubusercontent.com/ENQT-GmbH/remoc/master/.misc/Remoc.png"
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
//! Remoc is written in 100% safe Rust, builds upon [Tokio], works with any type
//! and [data format] supported by [Serde] and does not depend on any particular
//! [transport type].
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
//! [data format]: codec
//! [Serde]: serde
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
//! For that it uses Serde to serialize and deserialize data as it gets transmitted
//! over an underlying transport,
//! which might be a [TCP network connection], a WebSocket, [UNIX pipe],
//! or even a [serial link].
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
//! [TCP network connection]: https://docs.rs/tokio/1.12.0/tokio/net/struct.TcpStream.html
//! [UNIX domain sockets]: https://docs.rs/tokio/1.12.0/tokio/net/struct.UnixStream.html
//! [UNIX pipe]: https://docs.rs/tokio/1.12.0/tokio/process/struct.Child.html
//! [serial link]: https://docs.rs/tokio-serial/5.4.1/tokio_serial/
//! [channel]: rch
//! [remotely callable]: rtc
//!
//! # Forward and backward compatibility
//!
//! Distributed systems often require that endpoints running different software
//! versions interact.
//! By utilizing a data format like [Postbag](postbag) for encoding of your data
//! for transport, you can ensure a high level of backward and forward compatibility.
//!
//! It is always possible to add new fields to enums and struct and utilize the
//! `#[serde(default)]` attribute to provide default values for that field if it
//! was sent by an older client that has no knowledge of it.
//! Likewise additional, unknown fields are silently ignored when received,
//! allowing you to extend the data format without disturbing legacy endpoints.
//!
//! Check the documentation of the employed data format for details.
//!
//! # Crate features
//!
//! The modules of Remoc are gated by crate features, as shown in the reference below.
//! For ease of use all features are enabled by default.
//! See the [codec module](codec) documentation on how to select a default codec.
//!
//! ### JavaScript and web support
//!
//! Remoc supports compiling to the WebAssembly targets `wasm32-unknown-unknown`,
//! `wasm32-wasip1` and `wasm32-wasip1-threads`. If you are targeting a JavaScript
//! execution environment (like a web browser) you must enable the `js` crate feature.
//! This will enable JavaScript promises support and spawn tasks onto the browser's
//! native event queue.
//!
//! # Tracing
//!
//! Remoc uses the [Tracing crate](tracing) for logging of events.
//! Setting the log level to `TRACE` logs multiplexer lifetime events and messages as they are being processed.
//!
//! # Example
//!
//! This is a short example; for a fully worked remote trait calling (RTC) example
//! see the [examples directory](https://github.com/ENQT-GmbH/remoc/tree/master/examples).
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
    let (seq_tx, mut seq_rx) = rch::mpsc::channel(1);
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

pub mod prelude;

#[doc(hidden)]
pub mod exec;

pub mod chmux;
pub use chmux::Cfg;

#[cfg(feature = "serde")]
#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]
pub mod codec;

#[cfg(feature = "rch")]
#[cfg_attr(docsrs, doc(cfg(feature = "rch")))]
pub mod rch;

#[cfg(feature = "rch")]
mod remote_send;
#[cfg(feature = "rch")]
#[cfg_attr(docsrs, doc(cfg(feature = "rch")))]
pub use remote_send::RemoteSend;

#[cfg(feature = "rch")]
mod connect;
#[cfg(feature = "rch")]
#[cfg_attr(docsrs, doc(cfg(feature = "rch")))]
pub use connect::{Connect, ConnectError, LoopbackConnect};

#[cfg(feature = "rch")]
mod connect_ext;
#[cfg(feature = "rch")]
#[cfg_attr(docsrs, doc(cfg(feature = "rch")))]
pub use connect_ext::{ConnectExt, ConsumeError, ProvideError};

#[cfg(feature = "rfn")]
#[cfg_attr(docsrs, doc(cfg(feature = "rfn")))]
pub mod rfn;

#[cfg(feature = "robj")]
#[cfg_attr(docsrs, doc(cfg(feature = "robj")))]
pub mod robj;

#[cfg(feature = "robs")]
#[cfg_attr(docsrs, doc(cfg(feature = "robs")))]
pub mod robs;

#[cfg(feature = "rtc")]
#[cfg_attr(docsrs, doc(cfg(feature = "rtc")))]
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
#[cfg(all(feature = "rch", feature = "default-codec-set"))]
pub mod doctest;
