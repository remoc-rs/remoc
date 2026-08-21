# Changelog
All notable changes to Remoc will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.20.0 - 2026-08-13
This release makes reduces the size of transmitted data, increases throughput and 
introduces a system for evolving the serialized representation of your own types 
while staying compatible with older versions.

Remoc 0.20 remains wire-compatible with Remoc 0.19 and 0.18.

The default codec is now always Postbag and can no longer be changed using crate
features. If you were selecting a codec via a `default-codec-*` feature, see the
notes under *Deprecated* and *Removed* below.

### Added
- performance: data of a newly opened channel is now sent immediately, without
  waiting for the remote endpoint to confirm the port; this removes one round-trip
  of latency when a channel is used for the first time
- performance: messages of the channel multiplexer use variable integer encoding,
  which reduces the per-message overhead; furthermore Remoc's own types, such as 
  the headers exchanged by the channels, use a compact serialized representation
- new [transports module](https://docs.rs/remoc/0.20/remoc/transports/index.html)
  containing worked, copy-and-paste ready examples for connecting over different 
  transports
- new [versioned module](https://docs.rs/remoc/0.20/remoc/versioned/index.html) for
  evolving the serialized representation of a type
- performance: endpoints agree on the newest Postbag data format both can read,
  so a connection to Remoc 0.19 or 0.18 keeps using the format those understand
  while two current endpoints use the newer, smaller one
- rch::mpsc: optional additional parallel transfer channels, which serialize and
  deserialize items concurrently and thus increase throughput over high-bandwidth
  links
- rch::base: `storage` and `with_storage` to access the data storage of the
  connection that performs the current serialization or deserialization, plus
  `StorageRef` and `storage_ref` to obtain the storage of the connection over which
  a value is transferred
- chmux: `AnyStorage` can now store values indexed by their type using `insert`,
  `get`, `with` and `remove`, and exposes the configuration of the connection
  via `AnyStorage::cfg`
- chmux, rch::base: `Receiver::set_global_credits_allowed` lets the receiving side
  keep a channel out of the receive buffer shared by all channels, for when it
  expects to consume the received data slowly; global credits are used only when
  both endpoints allow it

### Changed
- connect: the connection future is no longer required to be `Send`, which allows
  using transports based on JavaScript objects in a web browser
- **BREAKING**: rch, chmux: `is_final` has been removed from the error types of send
  and receive operations; use `is_disconnected` instead
- **BREAKING**: chmux: `Cfg::ports_exhausted` has been remodelled and is now 
  acutally being honored
- **BREAKING**: chmux: `AnyStorage::insert`, `get` and `remove` have been renamed to
  `insert_entry`, `get_entry` and `remove_entry`
- **BREAKING**: chmux: the port allocation and connect API used by custom channel
  implementations has changed; `PortNumber` and `PortReq` have been replaced by
  `AllocatedLocalPort`, `SidePort` and `ConnectReq`, `Client::connect` is now
  `Client::connect_port` and `Listener::accept_from` has been removed
- **BREAKING**: chmux, rch::base: `Sender::are_global_credits_used` and
  `set_global_credits_use` have been renamed to `are_global_credits_allowed` and
  `set_global_credits_allowed`, matching the new methods of the same name on the
  receiving side
- **BREAKING**: connect: `Connect` is now generic over the type of its connection
  future; use `BoxConnect` (obtained via `Connect::boxed`) where the previous type
  parameters are required

### Deprecated
- codec: selecting the default codec via a `default-codec-*` crate feature is
  deprecated, because crate features are global to an application and thus a library
  using Remoc would be forced to use the default codec selected by the application.
  If required, specify the codec per channel using its generic type parameter instead.
- rch::lr: the local/remote channel is deprecated in favor of an `rch::mpsc` channel.

### Removed
- **BREAKING**: codec: the crate features `codec-postbag` and `default-codec-postbag`
  have been removed, since the Postbag codec is always available and the default;
  remove them from your `Cargo.toml`

### Fixed
- chmux: `Cfg::ports_exhausted` was ignored and connect requests always waited for
  a port to become available
- chmux: `SendError::is_closed` reported false when the remote endpoint closed the
  channel without doing so gracefully
- rch, chmux: errors caused by exhausted ports or a rejected channel are now
  reported as a disconnection


## 0.19.1 - 2026-08-05
This release considerably reduces the compile time and code size of crates using
Remoc and makes the type-erased base channel available for use by custom channel
implementations.

Remoc 0.19.1 remains wire-compatible with Remoc 0.19 and 0.18.

### Added
- rch: type-erased base channel via `base::ErasedSender` and `base::ErasedReceiver`,
  which send and receive `Box<dyn Any + Send>` values using an item type and codec
  that are fixed at construction time
- codec: `ErasedSerializer` and `ErasedDeserializer` for type-erased serialization
  and deserialization, together with the `AnySend` type alias

### Changed
- rch: the `Debug` implementations of the send error types no longer require the item
  type to implement `Debug` and no longer include the item in their output
- considerably reduced monomorphization; thus reducing compile time and binary size


## 0.19.0 - 2026-08-03
This is a large release that significantly improves throughput and latency of the
underlying channel multiplexer, adds monitoring and rate limiting facilities and
makes many channels usable locally.

Remoc 0.19 remains wire-compatible with Remoc 0.18, i.e. endpoints running either
version can still talk to each other.

### Added
- rch: new [I/O channel](https://docs.rs/remoc/0.19/remoc/rch/io/index.html) that
  implements `AsyncRead` and `AsyncWrite` for streaming binary data of known or
  unknown size, with integrity verification once the transfer completes
- rch: rate limiting for watch channels; both the sender (`watch::Sender::set_rate_limit`)
  and the receiver (`watch::Receiver::set_rate_limit`) can request a minimum delay
  between value updates; intermediate values are coalesced and the latest value is
  always eventually delivered
- rch: `watch::TransferStrategy` and `WatchExt::with_transfer_strategy` to trade off
  throughput, latency and buffer usage of a watch channel
- rch: `watch::Receiver::wait_for`, `has_changed`, `mark_changed` and `mark_unchanged`
- rch: `watch::Sender::send_if_modified` and `send_if_different`
- rch: `mpsc::Sender::try_reserve`
- rch: `mpsc::forward` and `oneshot::forward` to forward a local Tokio channel to a
  remote endpoint, plus `Receiver::forwarded` constructors for mpsc, oneshot and watch
- rch: `broadcast::WeakSender` and `broadcast::Sender::downgrade`, `strong_count`
  and `weak_count`
- rch: `Sending::dropped` creates a handle for a value that was never queued for
  sending, because the receiving half was already gone
- rch: `SendingError::map_item` and `base::SendError::map_item` replace the unsent
  value of a send error
- rch: `bin` channels can now be used fully locally; when both ends stay in the same
  process a lightweight loopback is used and no serialization takes place
- rch: `base::Sender::into_inner` and `base::Receiver::into_inner` to obtain the
  underlying chmux channel
- rtc: the request receiver (`TraitReqReceiver`) can now be sent to a remote endpoint,
  which then handles the requests of the client; a set request receiver monitor is not
  transferred
- rtc: every server variant can now be created from a request receiver, either using
  `TraitReqReceiver::into_server`, `into_server_ref`, `into_server_ref_mut`,
  `into_server_shared` and `into_server_shared_mut`, or using the `from_req_receiver`
  function of the corresponding server trait; together with the above this allows a
  remote endpoint to attach a target object to a client that is already connected
- rtc: `ReqReceiver::forward` hands the requests of a request receiver over to another
  client, which lets whatever that client is connected to execute them; replies are
  delivered from there directly to the original caller
- rtc: `TraitClient::new(request_buffer)` creates a client together with the request
  receiver connected to it, mirroring `ReqReceiver::new`; calls made on the client
  before the request receiver is attached to a target object are queued
- rtc: pipelining via the `#[pipelinable]` attribute on a trait method returning the
  client of another remotable trait. It generates a `<name>_pipelined` twin method taking
  the request receiver of that client, so that the caller can use the client while the
  call that provides the object is still in flight. The twin is a provided trait
  method, whose default implementation forwards the requests to the returned client
  and which can be overridden to serve the request receiver directly. Use
  `#[pipelinable(name)]` to name the twin method differently.
- rtc: `Client` has a new associated type `ReqReceiver`, naming the request receiver
  whose requests can be forwarded to the client
- rtc: `ReplyTo::complete` and `PipelinableReplyTo::complete` reply to a request,
  the latter handling both an ordinary call and a handed over request receiver, so
  that a request can be replied to the same way whether or not its method is
  pipelinable. The returned handle reports whether the reply was transmitted and, if
  not, the reply that could not be transmitted.
- rtc: `CallError::Consumed` and `ConsumedExt::unconsumed`, for the case that
  an object was consumed by a method taking `self` by value while its requests were
  being served
- rtc: [monitors](https://docs.rs/remoc/0.19/remoc/rtc/monitor/index.html) that
  observe and control every request of a client (`MonitorableClient::set_monitor`),
  server (`MonitorableServer::set_monitor`) and request receiver
  (`MonitorableReqReceiver::set_monitor`). A monitor can pass, delay, guard, drop or
  reject each request. The following ready-to-use monitors are provided:
    - `RateLimitMonitor` — limits the request rate using a sliding window,
    - `ConcurrentLimitMonitor` — limits the number of concurrently processed requests,
    - `IncompatibleClientMonitor` — logs and limits requests from clients that are
      partly incompatible with the server,
    - `IncompatibleServerMonitor` — logs and throttles calls to methods that are not
      supported by the server.
  
  Monitors can be combined using `ChainedMonitor`.
- rtc: remote traits may now declare associated types
- rtc: request enums expose `trait_name()` and `method_name()` for logging and
  monitoring purposes
- rfn: the number of concurrent invocations of a remote function is now limited;
  configurable via `RFnProvider::set_max_concurrency` (default: 32)
- robj: lazy blobs can now be used fully locally, without any copying of the data
- connect: `Connect::loopback` for establishing a connection to yourself, which is
  useful for testing and for uniformly handling local and remote objects
- chmux: `Sender::flush` for explicitly flushing the transport, together with the
  `Cfg::flush_interval` option
- chmux: `Sender::all_received` to await that the remote endpoint has received all
  data sent so far

### Changed
- performance: the channel multiplexer now uses a dynamically sized, globally shared
  receive buffer. This substantially improves throughput, especially when the underlying 
  connection has high latency.
- performance: connections established with `Connect::io` are now buffered internally
  by default; the buffer size is configured via `Cfg::io_buffer_size`
- **BREAKING**: chmux: the configuration (`Cfg`) has changed:
    - `receive_buffer` has been replaced by `port_receive_buffer`,
      `port_receive_throttle` and `shared_receive_buffer`,
    - `flush_delay` has been replaced by the optional `flush_interval`,
    - `io_buffer_size` has been added,
    - the presets `Cfg::balanced()`, `Cfg::compact()` and `Cfg::throughput()` have
      been removed
    - `Cfg` no longer implements `Serialize`, `Deserialize`, `PartialEq`, `Eq`,
      `PartialOrd`, `Ord` and `Hash`,
- **BREAKING**: rtc: the request receiver server variant now receives requests of
  type `rtc::Req<Value, Ref, RefMut>`, which groups the methods of a trait by how
  they take `self`. Correspondingly the macro now generates `...ReqValue`,
  `...ReqRef` and `...ReqRefMut` enums instead of a single `...Req` enum.
- **BREAKING**: rtc: `ReqReceiver` no longer implements `Stream` directly; call
  `ReqReceiver::into_stream()` to obtain a `ReqReceiverStream`
- **BREAKING**: rtc: `ServerBase` has a new associated type `ReqReceiver`, naming the
  request receiver every server variant can be created from
- **BREAKING**: rtc: `ServeError` has a new `Forward` variant, reported when forwarding
  a request to another client fails
- **BREAKING**: rtc: `ReqReceiver::forward` returns the client it forwarded to, or
  `None` when the object was consumed by a method taking `self` by value
- **BREAKING**: rtc: the reply channel of a request is now transferred as `ReplyTo`
  instead of the bare `ReplySender`, so that a method can later gain the `#[pipelinable]`
  attribute without breaking its clients. `ReplyTo` offers the same functions as
  `ReplySender`, thus code handling requests of a request receiver is unaffected.
  Endpoints of Remoc 0.19 and earlier are unaffected as well, since they continue to
  receive the bare reply sender.
- **BREAKING**: rtc: `ReqReceiver` is no longer accepted as a server variant in the
  `server(...)` argument of the `remote` attribute; the request receiver is now always
  generated
- **BREAKING**: rtc: `OnReqReceiveError` and `ServerBase::set_on_req_receive_error`
  have been removed; use a [server monitor](https://docs.rs/remoc/0.19/remoc/rtc/trait.ServerMonitor.html)
  to react to failing requests
- **BREAKING**: rch: `watch::ChangedError` has a new `Recv` variant, so that a
  receive error is no longer misreported as a closed channel
- update MSRV to 1.95
- `Connect::io_buffered` is removed; just use `Connect::io` together with
  `Cfg::io_buffer_size`

### Fixed
- robs: subscriptions to mirrors of an observable collection now end when the
  original subscription ends
- rfn: fixed a panic when an `RFnOnce` was dropped without being called
- rch: `watch::Receiver::changed` on a receiver that was sent to a
  remote endpoint no longer returns immediately for the initial value; it now waits
  for an actual change of the value


## 0.18.3 - 2025-09-19
### Added
- robs: added remotely observable VecDeque

## 0.18.2 - 2025-09-08
### Changed
- codec: update Postbag to 0.4.0 (fully compatible)

## 0.18.1 - 2025-09-08
### Changed
- rtc: optimize generated code size of dispatch functions
- codec: update Postbag to 0.3.0 (fully compatible)

## 0.18.0 - 2025-09-07
### Changed
- **BREAKING**: Default codec changed from JSON to Postbag for improved efficiency and 
  full Rust type system fidelity while maintaining forward and backward compatibility
  between data structures.
  Users needing to interact with previous remoc versions should specify the 
  `default-codec-json` feature in their `Cargo.toml` dependency:
  ```toml
  [dependencies]
  remoc = { version = "0.18", no-default-features = true, features = ["full", "default-codec-json"] }
  ```
  This change does not affect users already using specific `default-codec-...` features.

## 0.17.3 - 2025-09-04
### Added
- codec: added experimental Postbag codec

## 0.17.2 - 2025-08-29
### Added
- rtc: broadcast::Sender::new method

## 0.17.1 - 2025-08-29
### Changed
- rtc: relax Send bounds on RTC traits and methods where possible

## 0.17.0 - 2025-08-29
### Added
- rtc: make by-value server always return target object when serving is done, 
  even in error case

## 0.16.1 - 2025-08-28
### Added
- rtc: implement Stream for ReqReceiver

## 0.16.0 - 2025-08-26
### Added
- rtc: allow limiting the generated server variants by using #[rtc::remote(server(...))]
### Changed
- rtc: use Rust built-in support for async trait methods by default.
  If dyn-capable traits are required, specify #[rtc::remote(async_trait)] to
  use the async-trait attribute macro as before.
- update MSRV to 1.89

## 0.15.10 - 2025-08-19
### Added
- better integration with tracing crate
- rtc: use current tracing span for spawned request handlers

## 0.15.9 - 2025-07-31
### Fixed
- WebAssembly support

## 0.15.8 - 2025-07-31
### Added
- rch: mpsc::SenderSink that implements Sink for a Sender
- rch: watch::forward for forwarding a local Tokio watch Receiver to a
  remote endpoint
### Fixed
- rch: watch::SendError::is_closed query function

## 0.15.7 - 2025-07-24
### Added
- rch: mpsc::Receiver::recv_many method
- rch: mpsc::Receiver::len and is_empty methods

## 0.15.6 - 2025-07-14
### Fixed
- remote trait calling (RTC): do not require generic parameters of a remote trait  
  to implement Clone

## 0.15.5 - 2025-07-10
### Fixed
- missing docs warnings in remote traits by Fabian

## 0.15.4 - 2025-06-27
### Fixed
- remotely observable collections: workaround for possibly incorrect sync
  due to serde issue serde-rs/serde#2224 when using non-self-describing
  codecs like postcard

## 0.15.3 - 2025-03-21
### Fixed
- base receiver: cancel safety

## 0.15.2 - 2025-03-21
### Fixed
- threads: make availability test async

## 0.15.1 - 2025-03-07
### Added
- codec: Bincode 2 support
### Changed
- flush send buffer when flow credits are returned to solve a potential
  bandwidth bottleneck
- update rand dependency to 0.9
- update getrandom dependency to 0.3

## 0.15.0 - 2025-01-16
### Added
- support for WebAssembly targets wasm32-unknown-unknown, wasm32-wasip1 and
  wasm32-wasip1-threads
- JavaScript runtime environment support for all WebAssembly targets enabled
  by the `js` feature
- rch: status querying of value enqueued for sending using the `Sending` handle
- rtc: server fails when sending a reply to a request fails
- rtc: allow configuration of behavior when receiving a request fails using the
  `ServerBase::set_on_req_receive_error` method

## 0.14.0 - 2024-08-02
### Added
- remote trait calling (RTC): generate ReqReceiver type for handling requests from
  clients as messages

## 0.13.1 - 2024-07-14
### Added
- codec: added Postcard codec by Firaenix

## 0.13.0 - 2024-04-03
### Added
- chmux: forward channel closing
### Changed
- make forward a function of Receiver

## 0.12.0 - 2024-04-03
### Added
- chmux: allow specification of custom id (u32) when connecting a new channel
- chmux: recursive port data forwarding
- rch::bin: allow forwarding of binary channel
### Changed
- chmux: protocol version is now 3; fully backward compatible, but custom id and
  forwarding requires endpoint of same or higher version

## 0.11.7 - 2024-03-22
### Added
- remote trait calling (RTC): default method in remote traits
- remote trait calling (RTC): allow specification of supertraits in remote traits

## 0.11.6 - 2024-03-13
### Added
- broadcast channel: method on sender to subscribe with specific maximum item size
- remote channels: convenience methods for setting maximum item size on
  (sender, receiver)-tuple

## 0.11.5 - 2024-03-13
### Added
- watch channel: check() method on sender to check that no item-specific send errors
  have occurred
- allow querying whether error is due to item being sent on all send error types

## 0.11.4 - 2024-03-13
### Changed
- watch channel: fail sender if send error is caused by item being sent; this results
  in subsequent send calls to fail, making the error visible to the caller
- docs: clarify that mpsc and watch channel error reporting may be delayed

## 0.11.3 - 2023-11-08
### Fixed
- build with no enabled features

## 0.11.2 - 2023-11-08
### Fixed
- re-export serde for remoc_macro

## 0.11.1 - 2023-11-04
### Fixed
- unrecoverable error condition in remote channel receiver when deserialization fails

## 0.11.0 - 2023-09-13
### Added
- methods to adjust the maximum item size in serialized form of a remote channel
- methods to adjust the maximum request and reply size for remote trait calling (RTC)
### Changed
- update minimum supported Rust version to 1.72
- limit serialized item size of remote channels to 16 MB by default to prevent
  denial-of-service attacks by a malicious remote endpoint that sends arbitrarily
  large items to cause an out-of-memory condition on the receiving endpoint
- make RTC value trait server require that target is Send + Sync
### Fixed
- RTC method not accepting any arguments if self was taken by value
### Removed
- serde_cbor codec

## 0.10.3 - 2023-03-22
### Fixed
- do not panic when observable list task terminates

## 0.10.2 - 2023-03-22
### Added
- Add argument `clone` to `rtc::remote` attribute. When present, this
  forces the generated client to be clonable, even if the trait contains
  methods taking the receiver by mutable reference.
### Changed
- clarify Send+Sync requirements in RTC docs

## 0.10.1 - 2023-02-01
### Added
- configuration option `flush_delay` to configure flush delay when no data
  to send is available

## 0.10.0 - 2022-05-25
### Added
- move remotely observable collections from remoc-obs crate into `robs` module
- `rch::watch::Receiver::send_modify` method
- `chmux` errors can now be converted into `std::io::Error`
### Changed
- minimum supported Rust version (MSRV) is 1.59
- remove `rch::buffer` types and use const generics directly to specify
  buffer sizes of received channel halves
- update `uuid` to 1.0
### Fixed
- fix infinite recursion in `std::fmt::Debug` implementation on some types

## 0.9.16 - 2022-02-24
### Added
- reference to remoc-obs crate for remotely observable collections

## 0.9.15 - 2022-02-08
### Changed
- optimize default configuration for higher throughput
### Added
- configuration defaults optimized for low memory usage or high throughput
- enhanced configuration documentation

## 0.9.14 - 2022-02-02
### Fixed
- fix build when no default codec was selected

## 0.9.13 - 2022-01-26
### Added
- ConnectExt trait that allows for replacement of the base channel by
  another object, such as an RTC client or remote broadcast channel
- RTC example in examples/rtc
### Changed
- optimized CI by baptiste0928
- updated rmp-serde to 1.0

## 0.9.12 - 2022-01-24
### Fixed
- export rch::watch::ChangedError

## 0.9.11 - 2022-01-17
### Added
- conversions between remote channel receive errors
- error message when trying to use lifetimes or function generics in a remote trait

## 0.9.10 - 2022-01-03
### Added
- Cbor codec using ciborium, contributed by baptiste0928
### Deprecated
- legacy Cbor codec using serde_cbor

## 0.9.9 - 2021-12-10
### Added
- rch::mpsc::Receiver implements the Stream trait
- ReceiverStream for rch::broadcast::Receiver and rch::watch::Receiver
- rch::watch::Sender::send_replace

## 0.9.8 - 2021-12-07
### Added
- rch::SendErrorExt and rch::SendResultExt for quick querying if a send error
  was due to disconnection

## 0.9.7 - 2021-11-26
### Added
- rch::mpsc::Receiver::try_recv, error and take_error
- rch::mpsc::Sender::closed_reason
- `full-codecs` crate feature to activate all codecs
### Changed
- An mpsc channel receiver will hold back a receive error due to connection failure
  if other senders are still active. The error will be returned after all other
  senders have been disconnected.
- Fixes premature drop of RwLock owners.

## 0.9.6 - 2021-11-18
### Added
- add rtc::Client to prelude

## 0.9.5 - 2021-11-17
### Added
- rtc::Client trait implemented by all generated clients. This allows to
  receive notifications when the server has been dropped or disconnected.
- configuration options for transport queue lengths
### Changed
- fix mpsc channel close notifications not being delivered sometimes

## 0.9.4 - 2021-11-17
### Changed
- fix build when no default codec is set

## 0.9.3 - 2021-11-11
### Changed
- fix premature chmux termination with outstanding remote port requests
- fix build with Rust 1.51

## 0.9.2 - 2021-11-11
### Changed
- fix send error being missed during threaded serialization

## 0.9.1 - 2021-11-02
### Changed
- fix panic during threaded deserialization
- propagate panics from serializers and deserializers spawned into threads

## 0.9.0 - 2021-11-01
### Added
- `is_final()` on channel error types
### Changed
- terminate providers and RTC servers when a final receive error occurs
### Removed
- `chmux::Cfg::trace_id` because using tracing spans makes it redundant

## 0.8.2 - 2021-10-29
### Added
- blocking send and receive functions for rch::mpsc
### Changed
- switch to `tracing` crate for logging

## 0.8.1 - 2021-10-26
### Changed
- remove `default-codec-json` from `full` feature

## 0.8.0 - 2021-10-21
- initial release
