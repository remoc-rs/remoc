# Remoc 🦑 — remote multiplexed objects and channels

Remoc makes remote interaction between Rust programs seamless and smooth.
Over a [single underlying transport], such as TCP or TLS, it provides:

  * [multiple channels] of different types like [MPSC], [oneshot], [watch], etc.,
  * [remote synchronization] primitives,
  * calling of [remote functions] and [trait methods on a remote object (RPC)],
  * [remotely observable collections].

Remoc is written in [100% safe Rust], builds upon [Tokio], and uses [Serde]
with the compact, forward- and backward-compatible [Postbag] binary codec.
Remoc does not depend on any particular transport type.

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
[Postbag]: https://crates.io/crates/postbag

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

For that it uses Serde and the [Postbag] binary codec to serialize
and deserialize data as it is transmitted over an underlying transport, which
might be a [TCP network connection], a [WebSocket], [UNIX pipe], or even a
[serial link]. Postbag is designed for protocol evolution, allowing many changes
to message types without requiring both endpoints to be upgraded at once.

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


## When to use Remoc

Remoc is a good fit once two or more Rust programs need to interact and you
would rather express that interaction as channels, function calls and trait
objects than design and maintain a custom wire protocol.
Typical uses include splitting an application into cooperating processes,
talking to a sandboxed child process, connecting a UI to a backend it does
not share memory with, or driving a service from Rust code compiled to
WebAssembly. The processes can either run on the same machine or talk
to each other via the network.

Remoc is *not*:

  * a service mesh — it connects exactly two endpoints over one transport
    connection that you provide; discovery, load balancing and routing
    between more than two endpoints are outside its scope,
  * a message broker — channels and remote objects live only as long as the
    connection and the process holding them; nothing is persisted or
    replayed after a restart,
  * a network security layer — Remoc neither encrypts nor authenticates the
    connection itself, see [Security](#security) below,
  * a cross-language protocol — both endpoints of a connection run Rust code
    using Remoc.

Remoc pays off once you want function calling, bidirectional streams, 
callbacks, or to pass live channels and objects between endpoints as 
naturally as you would locally.


## Getting started

Add Remoc, Tokio, and Serde to an application:

```console
cargo add remoc
cargo add serde --features derive
cargo add tokio --features macros,rt-multi-thread,net
```

The default Remoc features include channels, remote functions and objects,
observable collections, and remote trait calling. Applications that only use
part of the API can disable default features; see [Crate features](#crate-features).

A Remoc application normally follows these steps:

1. Establish an ordered, reliable transport, such as a TCP or TLS stream.
2. Call `Connect::io` for a byte stream or `Connect::framed` for a stream of
   binary messages.
3. Spawn the returned connection future so it can drive all communication.
4. Use the returned base channel directly, or exchange one initial value with
   `ConnectExt::provide` and `ConnectExt::consume`.
5. Send additional channel halves, RTC clients, or remote objects wherever
   they are needed.

The complete [example](#example) in this README demonstrates the base-channel
approach in one process. For separate client and server crates using remote
trait calling, see the [RTC example].

[RTC example]: https://github.com/remoc-rs/remoc/tree/master/examples/rtc


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


## Choosing channels, functions, objects or traits

Everything in Remoc builds on [remote channels]; which higher-level building
block to reach for depends on how your interaction is shaped:

  * a [remote channel] directly, to stream a sequence of values in one or
    both directions, for example events, log lines or a stream of computed
    values,
  * a [remote function], to expose a single async function or closure
    without declaring a trait,
  * [remote trait calling (RTC)], when a remote endpoint should expose
    several related methods, optionally backed by shared mutable state; it
    generates a client and server for you from an ordinary trait,
  * a [remote object], when it is a value's identity, or its lazily-fetched
    contents, that must cross the connection rather than a stream of
    updates,
  * an [observable collection], when a remote endpoint needs a live,
    read-only mirror of a map, set, list or vector that changes over time.

These combine freely: an RTC method can take or return a channel, and a
channel's item can contain a remote object.

[remote channel]: https://docs.rs/remoc/latest/remoc/rch/index.html
[remote function]: https://docs.rs/remoc/latest/remoc/rfn/index.html
[remote trait calling (RTC)]: https://docs.rs/remoc/latest/remoc/rtc/index.html
[remote object]: https://docs.rs/remoc/latest/remoc/robj/index.html
[observable collection]: https://docs.rs/remoc/latest/remoc/robs/index.html


## Forward and backward compatibility

Distributed systems often require that endpoints running different software
versions interact.
Remoc therefore uses the [Postbag Full codec] by default. It includes field and
variant identifiers and the encoded length of each value, allowing a receiver
to skip data it does not know.

With suitable Serde attributes, Postbag supports common schema changes:

  * fields can be added, removed, or reordered; whenever a receiver expects a
    field that the sender omits, that field needs `#[serde(default)]`,
  * enum variants can be added, removed, or reordered; an older receiver needs
    a `#[serde(other)]` variant to accept an unknown one,
  * fields and variants can be renamed without breaking compatibility when
    they use stable numbered identifiers such as `#[serde(rename = "_0")]`.

An identifier is part of the protocol: changing it is breaking, and an
identifier retired from one field or variant must not later be reused for
another. Changes to a field's type and other structural transformations are not
automatically compatible. 

Use [`codec::recoverable`][recoverable] to confine an incompatible field to its
default value, or the [versioned] module when the old representation must be
transformed explicitly.

See [`codec::Postbag`][Postbag codec] and the [Postbag documentation] for the
complete compatibility table and format limitations.

[Postbag Full codec]: https://docs.rs/remoc/latest/remoc/codec/type.Postbag.html
[Postbag codec]: https://docs.rs/remoc/latest/remoc/codec/type.Postbag.html
[Postbag documentation]: https://docs.rs/postbag
[recoverable]: https://docs.rs/remoc/latest/remoc/codec/recoverable/index.html
[versioned]: https://docs.rs/remoc/latest/remoc/versioned/index.html


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

The meta-feature `full` enables all features from above but no additional codecs.
By default the `full` feature is enabled.

### Data formats for transmission (codecs)

The following features enable additional data formats (codecs) for transmission:

  * `codec-bincode` provides the Bincode 1 and 2 formats 
  * `codec-ciborium` provides the CBOR format
  * `codec-json` provides the JSON format
  * `codec-message-pack` provides the MessagePack format
  * `codec-postcard` provides the Postcard format

The feature `full-codecs` enables all additional data formats.

Remoc uses the full [Postbag codec] by default, and this is the recommended
choice for most applications because it combines compact binary encoding with
schema evolution support.
Alternative codecs are available for specialized requirements, such as
interoperating with an existing format or making a different size,
human-readability, or performance trade-off. Choosing one also changes the
protocol's compatibility properties, so review that codec's documentation
before doing so.

### JavaScript and web support

Remoc supports compiling to the WebAssembly targets `wasm32-unknown-unknown`,
`wasm32-wasip1` and `wasm32-wasip1-threads`. If you are targeting a JavaScript
runtime environment (like a web browser) you must enable the `js` crate feature.
This will enable JavaScript promises support and spawn tasks onto the browser's
native event queue.


## Security

Remoc neither encrypts nor authenticates the connection; it is designed to
run on top of a transport that already provides the properties you need.
If a connection crosses a trust boundary, wrap the transport in TLS or
another secure channel — see the [TLS transport example] — before passing
it to `Connect::io` or `Connect::framed`.

When exchanging data with an untrusted or unauthenticated endpoint, also
review the [size considerations] in the remote channel module and the
`max_ports` and `connect_queue` settings of [`Cfg`], which bound how many
channels a peer can make you open.

[TLS transport example]: https://docs.rs/remoc/latest/remoc/transports/tls/index.html
[size considerations]: https://docs.rs/remoc/latest/remoc/rch/index.html#size-considerations
[`Cfg`]: https://docs.rs/remoc/latest/remoc/chmux/struct.Cfg.html


## Logging

Remoc uses the [tracing] crate for logging.
Setting the log level to `TRACE` logs multiplexer lifetime events and
messages as they are processed.

[tracing]: https://docs.rs/tracing


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
