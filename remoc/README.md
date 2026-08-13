# Remoc 🦑 — remote multiplexed objects and channels

Remoc makes remote interaction between Rust programs seamless and smooth.
Over a [single underlying transport], such as TCP or TLS, it provides:

  * [multiple channels] of different types like [MPSC], [oneshot], [watch], etc.,
  * [remote synchronization] primitives,
  * calling of [remote functions] and [trait methods on a remote object (RPC)],
  * [remotely observable collections].

Remoc is written in [100% safe Rust], builds upon [Tokio], works with any type
and data format supported by [Serde] and does not depend on any particular
transport type.

[single underlying transport]: https://docs.rs/remoc/latest/remoc/struct.Connect.html#physical-transport
[multiple channels]: https://docs.rs/remoc/latest/remoc/rch/index.html
[MPSC]: https://docs.rs/remoc/latest/remoc/rch/mpsc/index.html
[oneshot]: https://docs.rs/remoc/latest/remoc/rch/oneshot/index.html
[watch]: https://docs.rs/remoc/latest/remoc/rch/watch/index.html
[remote synchronization]: https://docs.rs/remoc/latest/remoc/robj/index.html
[remote functions]: https://docs.rs/remoc/latest/remoc/rfn/index.html
[remotely observable collections]: https://docs.rs/remoc/latest/remoc/robs/index.html
[trait methods on a remote object (RPC)]: https://docs.rs/remoc/latest/remoc/rtc/index.html
[100% safe Rust]: https://www.rust-lang.org/
[Tokio]: https://tokio.rs
[Serde]: https://serde.rs

[![crates.io page](https://img.shields.io/crates/v/remoc)](https://crates.io/crates/remoc)
[![docs.rs page](https://docs.rs/remoc/badge.svg)](https://docs.rs/remoc)
[![Apache 2 license](https://img.shields.io/crates/l/remoc)](https://raw.githubusercontent.com/remoc-rs/remoc/master/LICENSE)
[![codecov](https://codecov.io/gh/remoc-rs/remoc/branch/master/graph/badge.svg?token=UDMOOK0QT8)](https://codecov.io/gh/remoc-rs/remoc)

## Introduction

A common pattern in Rust programs is to use channels to communicate between
threads and async tasks.
Setting up a channel is done in a single line and it largely avoids the need
for shared state and the associated complexity.
Remoc extends this programming model to distributed systems by providing
channels that work seamlessly over remote connections.

For that it uses Serde to serialize and deserialize data as it gets transmitted
over an underlying transport,
which might be a [TCP network connection], a [WebSocket], [UNIX pipe], or even a
[serial link].

Opening a new channel is straightforward, just send the sender or receiver half
of the new channel over an existing channel, like you would do between local
threads and tasks.
All channels are multiplexed over the same remote connection, with data being
transmitted in chunks to avoid one channel blocking another if a large message
is transmitted.

Building upon its remote channels, Remoc allows calling of remote functions and
closures.
Furthermore, a trait can be made remotely callable with automatically generated
client and server implementations, resembling a classical remote procedure
calling (RPC) model.

[TCP network connection]: https://docs.rs/remoc/latest/remoc/transports/tcp/index.html
[WebSocket]: https://docs.rs/remoc/latest/remoc/transports/websocket/index.html
[UNIX pipe]: https://docs.rs/remoc/latest/remoc/transports/process/index.html
[serial link]: https://docs.rs/tokio-serial


## Connecting

Remoc implements no transport itself and thus depends on no networking crate;
it runs over any byte stream you already have.
Pass an `AsyncRead` and `AsyncWrite` pair, such as a TCP or TLS connection, to
[`Connect::io`], or a `Sink` and `Stream` of binary packets, such as a WebSocket,
to [`Connect::framed`].
Both hand you a base channel, over which all further channels and remote objects
are exchanged.

The [transports] module contains worked examples for TCP, TLS, WebSocket, pipes
to a child process and aggregated, failure-resilient links.

[`Connect::io`]: https://docs.rs/remoc/latest/remoc/struct.Connect.html#method.io
[`Connect::framed`]: https://docs.rs/remoc/latest/remoc/struct.Connect.html#method.framed
[transports]: https://docs.rs/remoc/latest/remoc/transports/index.html


## Forward and backward compatibility

Distributed systems often require that endpoints running different software
versions interact.
By utilizing a self-describing data format like Postbag for encoding of your data
for transport, you can ensure a high level of backward and forward compatibility.

It is always possible to add new fields to enums and struct and utilize the
`#[serde(default)]` attribute to provide default values for that field if it
was sent by an older client that has no knowledge of it.
Likewise additional, unknown fields are silently ignored when received,
allowing you to extend the data format without disturbing legacy endpoints.

Check the documentation of the employed data format for details.


## Crate features

Most functionality of Remoc is gated by crate features.
The following features are available:

  * `serde` enables the `codec` module and implements serialize and
    deserialize for all configuration and error types.
  * `rch` enables remote channels provided by the `rch` module.
  * `rfn` enables remote function calling provided by the `rfn` module.
  * `robj` enables remote object utilities provided by the `robj` module.
  * `robs` enables remotely observable collections provided by the `robs` module.
  * `rtc` enables remote trait calling provided by the `rtc` module.

The meta-feature `full` enables all features from above but no codecs.
By default the `full` feature is enabled.

### Data formats for transmission (codecs)

The following features enable additional data formats (codecs) for transmission:

  * `codec-bincode` provides the Bincode 1 and 2 formats 
  * `codec-ciborium` provides the CBOR format
  * `codec-json` provides the JSON format
  * `codec-message-pack` provides the MessagePack format
  * `codec-postcard` provides the Postcard format

The feature `full-codecs` enables all data formats.

The [Postbag codec](https://crates.io/crates/postbag) is always available and used by default.

### JavaScript and web support

Remoc supports compiling to the WebAssembly targets `wasm32-unknown-unknown`,
`wasm32-wasip1` and `wasm32-wasip1-threads`. If you are targeting a JavaScript
runtime environment (like a web browser) you must enable the `js` crate feature.
This will enable JavaScript promises support and spawn tasks onto the browser's
native event queue.


## Supported Rust versions

Remoc is built against the latest stable release.
The minimum supported Rust version (MSRV) is 1.95.

## Example

This is a short example; for a fully worked remote trait calling (RTC) example
see the [examples directory](https://github.com/remoc-rs/remoc/tree/master/examples).

In the following example the server listens on TCP port 9870 and the client connects to it.
Then both ends establish a Remoc connection using `Connect::io()` over the TCP connection.
The connection dispatchers are spawned onto new tasks and the `client()` and `server()` functions
are called with the established base channel.
Then, the client creates a new remote MPSC channel and sends it inside a count request to the 
server.
The server receives the count request and counts on the provided channel.
The client receives each counted number over the new channel.

```rust
use std::net::Ipv4Addr;
use tokio::net::{TcpStream, TcpListener};
use remoc::prelude::*;

#[tokio::main]
async fn main() {
    // For demonstration we run both client and server in
    // the same process. In real life connect_client() and
    // connect_server() would run on different machines.
    futures::join!(connect_client(), connect_server());
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
    while let Some(CountReq {up_to, seq_tx}) = rx.recv().await.unwrap()
    {
        for i in 0..up_to {
            // Send each counted number over provided channel.
            seq_tx.send(i).await.unwrap();
        }
    }
}
```

## Development

Development on native platforms is straightforward. Use `cargo test` to run tests as usual.

To run tests in a JavaScript runtime environment (for example `wasm32-unknown-unknown` with `js` feature) 
install [`wasm-bindgen-test-runner`](https://github.com/wasm-bindgen/wasm-bindgen) and 
[Google ChromeDriver](https://developer.chrome.com/docs/chromedriver/downloads).
Then use the following command to execute the test suite:

```
WASM_BINDGEN_USE_BROWSER=1 WASM_BINDGEN_TEST_TIMEOUT=90 cargo +nightly test --target wasm32-unknown-unknown --all-features --release --tests
```

A proper web-compatible runtime environment is required. Thus Node.js will not work. Deno should
work, but it currently has some issues with the interaction between WebAssembly and async execution.


## Sponsors

Development of Remoc is partially sponsored by
[ENQT GmbH](https://enqt.de/)
and
[mlilabs GmbH](https://www.mlilabs.de/).

## License

Remoc is licensed under the [Apache 2.0 license].

[Apache 2.0 license]: https://github.com/remoc-rs/remoc/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Remoc by you, shall be licensed as Apache 2.0, without any
additional terms or conditions.
