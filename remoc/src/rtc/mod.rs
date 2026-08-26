//! Remote trait calling.
//!
//! This module allows calling of methods on an object located on a remote endpoint via a trait.
//!
//! By tagging a trait with the [remote attribute](remote), server, client and request receiver
//! types are generated for that trait.
//! The client type contains an automatically generated implementation of the trait.
//! Each call is encoded into a request and sent to the server.
//! The server accepts requests from the client and calls the requested trait method on an object
//! implementing that trait located on the server.
//! It then transmits the result back to the client.
//!
//! # Client type
//!
//! Assuming that the trait is called `Trait`, the client will be called `TraitClient`.
//!
//! The client type implements the trait and is [remote sendable](crate::RemoteSend) over
//! a [remote channel](crate::rch) or any other means to a remote endpoint.
//! All methods called on the client will be forwarded to the server and executed there.
//!
//! The client type also implements the [Client] trait which provides a notification
//! when the connection to the server has been lost.
//!
//! If the trait takes the receiver only by reference (`&self`) the client is [clonable](Clone).
//! To force the client to be clonable, even if it takes the receiver by mutable reference (`&mut self`),
//! specify the `clone` argument to the [remote attribute](remote).
//!
//! # Server types
//!
//! Assuming the trait is called `Trait`, the server names will all start with `TraitServer`.
//!
//! There is one server per way of holding the target object, and that is their only
//! difference: each takes requests from the client and invokes the trait methods on the
//! object. So start from how your code holds it — the left column is what you hand to
//! `new()`, with `Target` standing for the type of your object:
//!
//! | You hold | Server type | Calls are executed |
//! |---|---|---|
//! | `Target` | `TraitServer` ([Server]) | one at a time; serving ends when a method taking `self` is called |
//! | `Arc<Target>` | `TraitServerShared` ([ServerShared]) | in parallel, with `serve()` |
//! | `Arc<`[`RwLock`](tokio::sync::RwLock)`<Target>>` | `TraitServerSharedMut` ([ServerSharedMut]) | `&self` in parallel, `&mut self` serialized by the write lock |
//! | `&Target` | `TraitServerRef` ([ServerRef]) | one at a time |
//! | `&mut Target` | `TraitServerRefMut` ([ServerRefMut]) | one at a time |
//! | nothing, you want the requests themselves | `TraitReqReceiver` ([ReqReceiver]) | by you, as messages |
//!
//! If unsure, take `TraitServerSharedMut`, even when a single client is the only user.
//!
//! A server is only generated when the trait permits it, so the table above is also the
//! list of what the trait's receivers allow:
//!
//!   * `TraitServer` is always generated,
//!   * `TraitServerRefMut` and `TraitServerSharedMut` are generated when the receiver is
//!     *never* taken by value,
//!   * `TraitServerRef` and `TraitServerShared` are generated when the receiver is
//!     *never* taken by value and mutable reference.
//!
//! The first three require the target object to be [`Send`] + [`Sync`] and are the
//! recommended ones; in exchange the futures returned by [`ServerShared::serve`] and
//! [`ServerSharedMut::serve`] implement [`Send`] themselves.
//! `TraitServerRef` and `TraitServerRefMut` place no such requirement on the object,
//! which is what makes them useful for targets that are neither.
//!
//! # Request receiver type
//!
//! Assuming the trait is called `Trait`, the request receiver will be called `TraitReqReceiver`.
//!
//! The request receiver is also a server. However, instead of invoking the trait methods
//! on a target object, it allows you to process each request as a message and send the result
//! via a oneshot response channel.
//!
//! Unlike the other server types, the request receiver can itself be sent to a remote
//! endpoint, which then handles the requests of the client.
//!
//! Every other server variant can also be created from a request receiver, either using
//! `TraitReqReceiver::into_server`, `into_server_ref`, `into_server_ref_mut`,
//! `into_server_shared` and `into_server_shared_mut` or using the `from_req_receiver`
//! function of the server, for example [`ServerSharedMut::from_req_receiver`].
//! Thus a request receiver can be sent to a remote endpoint, which then attaches a
//! target object to it and serves the client.
//!
//! Alternatively the requests can be handed over to another client using
//! [`ReqReceiver::forward`], which lets whatever that client is connected to execute
//! them.
//!
//! A client and the request receiver connected to it can be created from either side:
//! `TraitReqReceiver::new()` returns the request receiver first and
//! `TraitClient::new()` the client first. Calls made on the client before
//! the request receiver is attached to a target object are queued.
//!
//! See [ReqReceiver] for details.
//!
//! # Usage
//!
//! Tag your trait with the [remote attribute](remote).
//! Call `new()` on a server type to create a server and corresponding client instance for a
//! target object, which must implement the trait.
//! Send the client to a remote endpoint, see the [example](#example), and then call
//! `serve()` on the server instance to start processing requests by the client.
//!
//! The client can also be used on the endpoint that created it, without a connection
//! being involved at all; see [local use](crate#local-use).
//!
//! # Several calls at once
//!
//! A call is an ordinary future, so calls that do not depend on each other need not be
//! made one after another:
//!
//! ```ignore
//! let (a, b, c) = tokio::try_join!(client.a(), client.b(), client.c())?;
//! ```
//!
//! All three requests are sent together and the results arrive after one round trip
//! instead of three, which matters as soon as the endpoints are not next to each other.
//! Note that the processing order of the calls on the server side is undefined.
//!
//! When the calls do depend on each other, the `<name>_call` twin of every method starts
//! a call without awaiting its result and returns a [`Call`]. The [`calls!`] macro writes
//! a series of such calls, awaiting the results only at the end:
//!
//! ```ignore
//! let value = calls!(client.increase_call(20); client.multiply_call(2); client.value_call());
//! ```
//!
//! Use [`Client::set_sequential`] to have the server process the calls of a client one
//! after another, so that they take effect in the order they were made.
//!
//! # Pipelining
//!
//! A method that returns a [client](Client) of another remotable trait normally costs a
//! round trip before that object can be used: the caller must wait for the client to
//! arrive. Mark the method `#[pipelinable]` and the caller can skip that wait by
//! creating the client itself and handing its [request receiver](ReqReceiver) into the
//! call:
//!
//! ```ignore
//! let (mut counter, counter_rx) = CounterClient::new();
//!
//! // Opening the counter and using it takes a single round trip.
//! let value = calls!(
//!     dir.open_counter_pipelined("mine".to_string(), counter_rx);
//!     counter.increase_call(20);
//!     counter.value_call()
//! );
//! ```
//!
//! See the [pipelining] module for how it works and a worked example.
//!
//! # Monitoring
//!
//! Every request a client makes, a server handles or a request receiver receives can be
//! observed and controlled by a [monitor], which can delay, drop or reject it.
//! Rate limiting, limiting the number of concurrent calls and rejecting incompatible
//! endpoints are [provided](monitor#structs).
//!
//! # Error handling
//!
//! Since a remote trait call can fail due to connection problems, the return type
//! of all trait functions must always be of the [Result] type.
//! The error type must be able to convert from [CallError] and thus absorb the remote calling error.
//!
//! There is no timeout imposed on a remote call, but the underlying [chmux](crate::chmux) connection
//! [pings the remote endpoint](crate::chmux::Cfg::connection_timeout) by default.
//! If the underlying connection fails, all remote calls will automatically fail.
//! You can wrap remote calls using [tokio::time::timeout] if you need to use
//! per-call timeouts.
//!
//! # Cancellation
//!
//! If the client drops the future of a call while it is executing or the connection is interrupted
//! the trait function on the server is automatically cancelled at the next `await` point.
//! You can apply the `#[no_cancel]` attribute to a method to always run it to completion.
//!
//! # Associated types
//!
//! A remote trait may declare associated types (`type Item: RemoteSend;`).
//! Each associated type is lifted to an additional generic parameter on the generated
//! client, request enums and request receiver.
//! The lifted parameter is always prefixed with `__` (e.g. `type Item` becomes `__Item`)
//! to avoid collisions with the trait's own generic parameters and to signal that it
//! originates from a lifted associated type.
//! When sending the client, the concrete type for each associated type must be supplied
//! as a type argument (e.g. `StorageClient<__Item = String>`).
//!
//! Generic associated types (GATs) and associated type defaults are not supported.
//!
//! # Forward and backward compatibility
//!
//! All request arguments are packed into an enum case named after the function.
//! Each argument corresponds to a field with the same name.
//! Thus it is always safe to add new arguments at the end and apply the `#[serde(default)]`
//! attribute to them.
//! Arguments that are passed by the client but are unknown to the server will be silently discarded.
//!
//! Also, new functions can be added to the trait without breaking backward compatibility.
//! Calling a non-existent function (for example when the client is newer than the server) will
//! result in an error, but the server will continue serving.
//! It is thus safe to just attempt to call a server function to see if it is available.
//!
//! # Compact representation
//!
//! Serde attributes applied to a trait method are applied to its request enum case
//! and serde attributes applied to a method argument are applied to the corresponding
//! field of that case.
//!
//! Since the [Postbag codec](crate::codec::Postbag) encodes a case or field named
//! `_0` through `_58` using a single byte, the size of a request can be reduced
//! considerably by renaming them:
//!
//! ```
//! # use remoc::prelude::*;
//! # use remoc::rtc::CallError;
//! #[rtc::remote]
//! pub trait Counter {
//!     #[serde(rename = "_0")]
//!     async fn value(&self) -> Result<u32, CallError>;
//!
//!     #[serde(rename = "_1")]
//!     async fn increase(&mut self, #[serde(rename = "_0")] by: u32) -> Result<(), CallError>;
//! }
//! ```
//!
//! This changes the serialized representation of the affected methods, thus both
//! endpoints must agree on it.
//! Since renaming is applied per method, methods can be migrated individually and
//! new methods using the compact representation can be added to an existing trait
//! without affecting its other methods.
//!
//! The name `_59` is reserved for the response channel of a request and cannot be used.
//! It is only used when the corresponding method is renamed.
//!
//! # Alternatives
//!
//! If you just need to expose a function remotely using [remote functions](crate::rfn) is simpler.
//!
//! # Example
//!
//! This is a short example only; a fully worked example with client and server split into
//! their own crates is available in the
//! [examples directory](https://github.com/remoc-rs/remoc/tree/master/examples/rtc).
//! This can also be used as a template to get started quickly.
//!
//! In the following example a trait `Counter` is defined and marked as remotely callable.
//! It is implemented on the `CounterObj` struct.
//! The server listens on TCP port 9871, creates a `CounterObj` and obtains a
//! `CounterServerSharedMut` and `CounterClient` for it.
//! The `CounterClient` is transferred to the client while the Remoc connection is
//! established, using [`ConnectExt::provide`](crate::ConnectExt::provide) on the server
//! and [`ConnectExt::consume`](crate::ConnectExt::consume) on the client.
//! The client then calls trait methods on the received client, which are executed on the
//! counter object held by the server.
//!
//! ```
//! use std::{net::Ipv4Addr, sync::Arc};
//! use tokio::net::{TcpListener, TcpStream};
//! use tokio::sync::RwLock;
//! use remoc::prelude::*;
//! use remoc::rtc::CallError;
//!
//! // Custom error type that can convert from CallError.
//! #[derive(Debug, serde::Serialize, serde::Deserialize)]
//! pub enum IncreaseError {
//!     Overflow,
//!     Call(CallError),
//! }
//!
//! impl From<CallError> for IncreaseError {
//!     fn from(err: CallError) -> Self {
//!         Self::Call(err)
//!     }
//! }
//!
//! // Trait defining remote service.
//! #[rtc::remote]
//! pub trait Counter {
//!     async fn value(&self) -> Result<u32, CallError>;
//!
//!     async fn watch(&mut self) -> Result<rch::watch::Receiver<u32>, CallError>;
//!
//!     #[no_cancel]
//!     async fn increase(&mut self, #[serde(default)] by: u32)
//!         -> Result<(), IncreaseError>;
//! }
//!
//! // Server implementation object.
//! pub struct CounterObj {
//!     value: u32,
//!     watchers: Vec<rch::watch::Sender<u32>>,
//! }
//!
//! impl CounterObj {
//!     pub fn new() -> Self {
//!         Self { value: 0, watchers: Vec::new() }
//!     }
//! }
//!
//! // Server implementation of trait methods.
//! impl Counter for CounterObj {
//!     async fn value(&self) -> Result<u32, CallError> {
//!         Ok(self.value)
//!     }
//!
//!     async fn watch(&mut self) -> Result<rch::watch::Receiver<u32>, CallError> {
//!         let (tx, rx) = rch::watch::channel(self.value);
//!         self.watchers.push(tx);
//!         Ok(rx)
//!     }
//!
//!     async fn increase(&mut self, by: u32) -> Result<(), IncreaseError> {
//!         match self.value.checked_add(by) {
//!             Some(new_value) => self.value = new_value,
//!             None => return Err(IncreaseError::Overflow),
//!         }
//!
//!         for watch in &self.watchers {
//!             let _ = watch.send(self.value);
//!         }
//!
//!         Ok(())
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     // For demonstration we run both client and server in
//!     // the same process. In real life connect_client() and
//!     // connect_server() would run on different machines.
//!     tokio::join!(connect_client(), connect_server());
//! }
//!
//! // This would be run on the server.
//! async fn connect_server() {
//!     // Accept TCP connection.
//!     let listener =
//!         TcpListener::bind((Ipv4Addr::LOCALHOST, 9871)).await.unwrap();
//!     let (socket, _) = listener.accept().await.unwrap();
//!     let (socket_rx, socket_tx) = socket.into_split();
//!
//!     // Create the server and its client for the counter object.
//!     let counter_obj = Arc::new(RwLock::new(CounterObj::new()));
//!     let (server, client) =
//!         CounterServerSharedMut::<_, remoc::codec::Default>::new(counter_obj);
//!
//!     // Establish the Remoc connection over TCP and send the client
//!     // to the remote endpoint.
//!     remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
//!         .provide(client).await.unwrap();
//!
//!     // Execute the calls made by the remote endpoint on the counter object.
//!     server.serve().await.unwrap();
//! }
//!
//! // This would be run on the client.
//! async fn connect_client() {
//!     // Wait for server to be ready.
//!     tokio::time::sleep(std::time::Duration::from_secs(1)).await;
//!
//!     // Establish TCP connection.
//!     let socket =
//!         TcpStream::connect((Ipv4Addr::LOCALHOST, 9871)).await.unwrap();
//!     let (socket_rx, socket_tx) = socket.into_split();
//!
//!     // Establish the Remoc connection over TCP and receive the counter client.
//!     let mut counter: CounterClient =
//!         remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
//!             .consume().await.unwrap();
//!
//!     // CounterClient implements Counter, so calling it looks like a local call,
//!     // but is executed on the counter object located on the server.
//!     let mut watch_rx = counter.watch().await.unwrap();
//!     assert_eq!(counter.value().await.unwrap(), 0);
//!
//!     counter.increase(20).await.unwrap();
//!     assert_eq!(counter.value().await.unwrap(), 20);
//!
//!     counter.increase(45).await.unwrap();
//!     assert_eq!(counter.value().await.unwrap(), 65);
//!
//!     // The watch receiver returned by the call stays connected to the counter
//!     // object and reports every change made to it.
//!     while *watch_rx.borrow_and_update().unwrap() != 65 {
//!         watch_rx.changed().await.unwrap();
//!     }
//! }
//! ```
//!
//! [`ConnectExt::provide`](crate::ConnectExt::provide) and
//! [`ConnectExt::consume`](crate::ConnectExt::consume) are a shorthand for the common
//! case of transferring exactly one value while connecting.
//! When a connection already exists, send the client over any [channel](crate::rch)
//! instead, exactly like a channel half:
//!
//! ```ignore
//! tx.send(client).await?;
//! ```
//!
//! This is also the way to hand out more than one client over the same connection.
//!

pub mod monitor;
pub mod pipelining;

mod call;
pub use call::{Call, CallError, CallFutureExt, calls};

mod response;
pub use response::{
    Completing, PipelinableResponder, PipelinableResponse, Responder, Response, ResponseSender,
    TransportedResponse,
};
#[doc(hidden)]
pub use response::{ResponseErrorSender, complete_call, response_channel, response_error_channel};

use std::{
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};
use tokio_util::sync::ReusableBoxFuture;

use crate::{
    RemoteSend,
    rch::{DEFAULT_BUFFER, SendingError, SendingErrorKind, mpsc},
};

/// Default maximum number of calls a server processes concurrently.
///
/// The current default parallelism is 32.
pub const DEFAULT_PARALLELISM: usize = 32;

/// Denotes a trait as remotely callable and generate a client and servers for it.
///
/// See [module-level documentation](self) for details and examples.
///
/// This generates the client, server and request receiver structs for the trait.
/// If the trait is called `Trait` the client will be called `TraitClient` and
/// the name of the servers will start with `TraitServer`. The request receiver
/// will be called `TraitReqReceiver`.
///
/// # Requirements
///
/// Each trait method must be either be
///
///   * an `async fn` and have return type `Result<T, E>`,
///   * a `fn` and have return type `impl Future<Output = Result<T, E>> + Send`,
///
/// where `T` and `E` are [remote sendable](crate::RemoteSend) and `E` must
/// implemented [`From`]`<`[`CallError`]`>`.
/// All arguments must also be [remote sendable](crate::RemoteSend).
/// Of course, you can use all remote types from Remoc in your arguments and return type,
/// for example [remote channels](crate::rch) and [remote objects](crate::rch).
///
/// Since the generated code relies on [Tokio](tokio) macros, you must add a dependency
/// to Tokio in your `Cargo.toml`.
///
/// # Generics, associated types and lifetimes
///
/// The trait may be generic with constraints on the generic arguments.
/// You will probably need to constrain them on [RemoteSend].
/// Method definitions within the remote trait may use generic arguments from the trait
/// definition, but must not introduce generic arguments in the method definition.
///
/// Associated types (`type Foo: Bound;`) are supported.
/// Each associated type is lifted to an additional generic parameter on the generated
/// `TraitClient`, request enums and request receiver.
/// To avoid collisions with the trait's own generic parameters and to make the origin
/// of the parameter visible, the lifted parameter is always prefixed with `__`
/// (e.g. `type Item` becomes `__Item`).
/// Method signatures may refer to associated types using `Self::Foo` or the qualified
/// form `<Self as Trait>::Foo`; both are rewritten to the lifted parameter in the
/// generated code.
/// Generic associated types (GATs) and associated type defaults are not supported.
///
/// Lifetimes are not allowed on remote traits and their methods.
///
/// # Default implementations of methods
///
/// Default implementations of methods may be provided.
/// However, this requires specifying [`Send`] and [`Sync`] as supertraits of the remote trait.
///
/// # Attributes
///
/// If the `clone` argument is specified (by invoking the attribute macro as `#[remoc::rtc::remote(clone)]`),
/// the generated `TraitClient` will even be [clonable](std::clone::Clone) when the trait contains
/// methods taking the receiver by mutable reference (`&mut self`).
/// In this case the client can invoke more than one mutable method simultaneously; however,
/// the execution on the server will be serialized through locking.
///
/// If the `async_trait` argument is specified (by invoking the attribute macro as `#[remoc::rtc::remote(async_trait)]`),
/// the remote trait will be processed through the [`#[async_trait] macro`](https://docs.rs/async-trait), enabling
/// `dyn` dispatch. You must then include `async-trait` as a dependency in your `Cargo.toml` and apply the
/// `#[async_trait::async_trait]` attribute on all implementations of the trait.
///
/// If the `debug` argument is specified (by invoking the attribute macro as `#[remoc::rtc::remote(debug)]`),
/// the generated request enums implement [`Debug`](std::fmt::Debug), showing the called method and its
/// arguments. This requires every method argument and every associated type to implement
/// [`Debug`](std::fmt::Debug) as well.
///
/// The `server(...)` argument allows to limit the generated server variants.
/// Supported variants are: `Value`, `Ref`, `RefMut`, `Shared`, `SharedMut`.
/// Multiple variants can be specified as a comma-separated list.
/// For example, when `#[remoc::rtc::remote(server(SharedMut))]` is applied to `trait Trait` only the
/// `TraitServerSharedMut` server will be generated.
/// If unspecified, all server variants are generated.
/// Specify an empty list, i.e. `#[remoc::rtc::remote(server())]`, to generate no server at all;
/// this is useful when the requests are only handled through the request receiver.
/// The request receiver `TraitReqReceiver` is always generated.
///
/// If the `#[no_cancel]` attribute is applied on a trait method, it will run to completion,
/// even if the client cancels the request by dropping the future.
///
/// If the `#[pipelinable]` attribute is applied on a trait method returning the
/// [client](Client) of another remotable trait, a twin method called `<name>_pipelined`
/// is generated, which additionally takes the [request receiver](ReqReceiver) of that
/// client. Specify `#[pipelinable(name)]` to name that method differently. The caller can
/// thus create the client and request receiver pair itself using
/// `OtherTraitClient::new()`, hand the request receiver over and call
/// methods on the client without waiting for the call to complete; these calls are
/// queued until the object is available.
///
/// The twin returns a [`Call`], so that the request receiver is handed over immediately
/// and calls on the client can be started without waiting for the session call to
/// complete. Serving of the request receiver continues in the background, so the caller
/// keeps the client it created for as long as it likes. The default implementation calls
/// the original method and [forwards](ReqReceiver::forward) the requests to the client
/// it returns. It should not be overridden.
///
/// Adding the attribute to an existing method keeps its requests wire compatible.
/// However, calling the twin method on an endpoint that does not know it fails with
/// a receive error there.
///
/// See the [pipelining] module for how to combine the resulting calls and in which order
/// the server executes them.
///
/// All [serde field attributes](https://serde.rs/field-attrs.html) `#[serde(...)]`
/// are allowed on the arguments of the functions.
/// They will be transferred to the respective field of the request struct that will
/// be sent to the server when the method is called by the client.
/// This can be used to customize serialization and provide defaults for forward and backward
/// compatibility.
///
pub use remoc_macro::remote;

/// The request enum of a remotely callable trait.
///
/// One is generated per remotable trait and kind of `self` reference, holding one
/// variant per method. [Monitors](monitor) are generic over them.
pub trait ReqEnum {
    /// The name of the remotely callable trait this request enum belongs to.
    fn trait_name() -> &'static str;

    /// Trait method name this request enum variant belongs to.
    ///
    /// # Panics
    /// Panics when called on the `__Phantom` variant.
    fn method_name(&self) -> &'static str;

    /// Whether the caller requests the server to dispatch this request inline.
    ///
    /// # Panics
    /// Panics when called on the `__Phantom` variant.
    fn sequential(&self) -> bool;
}

/// A request from client to server.
///
/// This groups the methods of a remotable trait by how they take `self`.
/// Each variant holds a per-kind request enum that in turn has one variant per
/// method of that kind.
#[derive(Debug)]
pub enum Req<Value, Ref, RefMut> {
    /// Request for a method taking self by value (`self`).
    Value(Value),
    /// Request for a method taking self by reference (`&self`).
    Ref(Ref),
    /// Request for a method taking self by mutable reference (`&mut self`).
    RefMut(RefMut),
}

crate::versioned::compact::impl_enum! {
    Req<Value, Ref, RefMut>,
    variants {
        Value(req: Value) => "_0",
        Ref(req: Ref) => "_1",
        RefMut(req: RefMut) => "_2",
    }
    where Value: RemoteSend, Ref: RemoteSend, RefMut: RemoteSend
}

impl<Value, Ref, RefMut> Req<Value, Ref, RefMut>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    /// The name of the remotely callable trait this request enum belongs to.
    pub fn trait_name() -> &'static str {
        let trait_name = Value::trait_name();
        assert_eq!(trait_name, Ref::trait_name());
        assert_eq!(trait_name, RefMut::trait_name());
        trait_name
    }

    /// Trait method name this request enum variant belongs to.
    ///
    /// # Panics
    /// Panics when called on the `__Phantom` variant.
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::Value(req) => req.method_name(),
            Self::Ref(req) => req.method_name(),
            Self::RefMut(req) => req.method_name(),
        }
    }
}

/// Client of a remotable trait.
pub trait Client {
    /// The [request receiver](ReqReceiver) of the same remotable trait.
    ///
    /// Requests received by it can be [forwarded](ReqReceiver::forward) to this client.
    type ReqReceiver;

    /// Creates a client and the [request receiver](Self::ReqReceiver) connected to it with the default request buffer size.
    ///
    /// The request buffer is [`DEFAULT_BUFFER`] calls. Use
    /// [`with_request_buffer`](Self::with_request_buffer) to choose a different size.
    fn new() -> (Self, Self::ReqReceiver)
    where
        Self: Sized,
    {
        Self::with_request_buffer(DEFAULT_BUFFER)
    }

    /// Creates a client and the [request receiver](Self::ReqReceiver) connected to it with the specified request buffer size.
    ///
    /// `request_buffer` bounds the number of calls that can be queued for sending locally while they
    /// are waiting to be serialized, transferred or deserialized.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn with_request_buffer(request_buffer: usize) -> (Self, Self::ReqReceiver)
    where
        Self: Sized;

    /// Returns the current capacity of the channel for sending requests to
    /// the server.
    ///
    /// Zero is returned when the server has been dropped or the connection
    /// has been lost.
    fn capacity(&self) -> usize;

    /// Returns a future that completes when the server or client has been
    /// dropped or the connection between them has been lost.
    ///
    /// In this case no more requests from this client will succeed.
    fn closed(&self) -> Closed;

    /// Returns whether the server has been dropped or the connection to it
    /// has been lost.
    fn is_closed(&self) -> bool;

    /// The maximum allowed size of a request in bytes.
    fn max_request_size(&self) -> usize;

    /// Sets the maximum allowed size of a request in bytes.
    ///
    /// This does not change the maximum request size the server will accept
    /// if this client has been received from a remote endpoint.
    fn set_max_request_size(&mut self, max_request_size: usize);

    /// The maximum allowed size of a response in bytes.
    fn max_response_size(&self) -> usize;

    /// Sets the maximum allowed size of a response in bytes.
    fn set_max_response_size(&mut self, max_response_size: usize);

    /// Whether the server processes the calls of this client one after another.
    ///
    /// This is `false` by default. When set to `true` each call is dispatched inline,
    /// i.e. the server runs it to completion before receiving the next request.
    ///
    /// Only [`ServerShared`] and [`ServerSharedMut`] dispatch concurrently, and only
    /// calls to methods taking `&self`, thus this has no effect otherwise.
    ///
    /// Since the call is dispatched inline, the server receives no requests at all while
    /// it runs, including those of other clients it may serve.
    fn sequential(&self) -> bool;

    /// Sets whether the server processes the calls of this client one after another.
    fn set_sequential(&mut self, sequential: bool);

    /// Whether the server shall stop serving when a call made by this client fails.
    ///
    /// This is `false` by default. When set to `true` a call that returns an error
    /// makes the server stop with [`ServeError::CallFailed`] after the error has been
    /// sent to this client.
    ///
    /// Serving stops entirely, not just for this client. That is the intended effect
    /// when the server object belongs to this client, as is usual, but a server shared
    /// between unrelated clients stops serving all of them.
    fn stop_on_error(&self) -> bool;

    /// Sets whether the server shall stop serving when a call made by this client fails.
    fn set_stop_on_error(&mut self, stop_on_error: bool);
}

/// A future that completes when the server or client has been dropped
/// or the connection between them has been lost.
///
/// This can be obtained via [Client::closed].
pub struct Closed(ReusableBoxFuture<'static, ()>);

impl fmt::Debug for Closed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("Closed").finish()
    }
}

impl Closed {
    #[doc(hidden)]
    pub fn new(fut: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(ReusableBoxFuture::new(fut))
    }
}

impl Future for Closed {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.as_mut().0.poll_unpin(cx)
    }
}

/// Base trait shared between all server variants of a remotable trait.
pub trait ServerBase {
    /// The client type, which can be sent to a remote endpoint.
    type Client: Client;

    /// The [request receiver](ReqReceiver) type of the same remotable trait.
    ///
    /// Every server variant can be created from a request receiver, which allows
    /// serving a client that is already connected, possibly over a connection to
    /// a remote endpoint.
    type ReqReceiver: ServerBase<Client = Self::Client>;
}

/// A server that owns the target of a remotely callable trait.
///
/// This variant processes calls one at a time and supports methods that consume
/// `self`. The future returned by [`serve`](Self::serve) must be polled for calls
/// to be processed.
pub trait Server<Target, Codec>: ServerBase
where
    Self: Sized,
{
    /// Creates a server and the client connected to it with the default request buffer size.
    ///
    /// The request buffer is [`DEFAULT_BUFFER`] calls. Use
    /// [`with_request_buffer`](Self::with_request_buffer) to choose a different size.
    fn new(target: Target) -> (Self, Self::Client) {
        Self::with_request_buffer(target, DEFAULT_BUFFER)
    }

    /// Creates a server and the client connected to it with the specified request buffer size.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed locally while they
    /// are waiting to be serialized, transferred or deserialized.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn with_request_buffer(target: Target, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](monitor::ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](monitor::ServerMonitor) and kept.
    fn from_req_receiver(target: Target, req_rx: Self::ReqReceiver) -> Self;

    /// Serves the target object.
    ///
    /// Serving ends when the client is dropped or a method taking self by value
    /// is called. In the first case, the target object is returned and, in the
    /// second case, None is returned.
    fn serve(self) -> impl Future<Output = (Option<Target>, Result<(), ServeError>)>;
}

/// A server that borrows the target of a remotely callable trait.
///
/// Calls are processed one at a time. The server cannot outlive the borrowed
/// target, and its [`serve`](Self::serve) future must be polled.
pub trait ServerRef<'target, Target, Codec>: ServerBase
where
    Self: Sized,
{
    /// Creates a server and the client connected to it with the default request buffer size.
    ///
    /// The request buffer is [`DEFAULT_BUFFER`] calls. Use
    /// [`with_request_buffer`](Self::with_request_buffer) to choose a different size.
    fn new(target: &'target Target) -> (Self, Self::Client) {
        Self::with_request_buffer(target, DEFAULT_BUFFER)
    }

    /// Creates a server and the client connected to it with the specified request buffer size.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed locally while they
    /// are waiting to be serialized, transferred or deserialized.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn with_request_buffer(target: &'target Target, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](monitor::ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](monitor::ServerMonitor) and kept.
    fn from_req_receiver(target: &'target Target, req_rx: Self::ReqReceiver) -> Self;

    /// Serves the target object.
    ///
    /// Serving ends when the client is dropped.
    fn serve(self) -> impl Future<Output = Result<(), ServeError>>;
}

/// A server that mutably borrows the target of a remotely callable trait.
///
/// Calls are processed one at a time. The server cannot outlive the borrowed
/// target, and its [`serve`](Self::serve) future must be polled.
pub trait ServerRefMut<'target, Target, Codec>: ServerBase
where
    Self: Sized,
{
    /// Creates a server and the client connected to it with the default request buffer size.
    ///
    /// The request buffer is [`DEFAULT_BUFFER`] calls. Use
    /// [`with_request_buffer`](Self::with_request_buffer) to choose a different size.
    fn new(target: &'target mut Target) -> (Self, Self::Client) {
        Self::with_request_buffer(target, DEFAULT_BUFFER)
    }

    /// Creates a server and the client connected to it with the specified request buffer size.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed locally while they
    /// are waiting to be serialized, transferred or deserialized.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn with_request_buffer(target: &'target mut Target, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](monitor::ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](monitor::ServerMonitor) and kept.
    fn from_req_receiver(target: &'target mut Target, req_rx: Self::ReqReceiver) -> Self;

    /// Serves the target object.
    ///
    /// Serving ends when the client is dropped.
    fn serve(self) -> impl Future<Output = Result<(), ServeError>>;
}

/// A server that shares ownership of an immutable target.
///
/// The target is held in an [`Arc`]. Calls can be processed concurrently when
/// [`serve`](Self::serve) is invoked with `spawn` set to `true`.
pub trait ServerShared<Target, Codec>: ServerBase
where
    Self: Sized,
    Self::Client: Clone,
{
    /// Creates a server and the client connected to it with the default request buffer size.
    ///
    /// The request buffer is [`DEFAULT_BUFFER`] calls. Use
    /// [`with_request_buffer`](Self::with_request_buffer) to choose a different size.
    fn new(target: Arc<Target>) -> (Self, Self::Client) {
        Self::with_request_buffer(target, DEFAULT_BUFFER)
    }

    /// Creates a server and the client connected to it with the specified request buffer size.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed locally while they
    /// are waiting to be serialized, transferred or deserialized.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn with_request_buffer(target: Arc<Target>, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](monitor::ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](monitor::ServerMonitor) and kept.
    fn from_req_receiver(target: Arc<Target>, req_rx: Self::ReqReceiver) -> Self;

    /// The maximum number of calls that are dispatched concurrently.
    ///
    /// This is [`DEFAULT_PARALLELISM`] by default. Each call that is dispatched
    /// concurrently runs on its own task.
    ///
    /// Zero means that calls are dispatched inline, i.e. one call at a time.
    fn parallelism(&self) -> usize;

    /// Sets the maximum number of calls that are dispatched concurrently.
    ///
    /// This must be set before [serving](Self::serve).
    fn set_parallelism(&mut self, parallelism: usize);

    /// Serves the target object.
    ///
    /// Up to [`parallelism`](Self::parallelism) calls are processed concurrently.
    ///
    /// Serving ends when the client is dropped.
    fn serve(self) -> impl Future<Output = Result<(), ServeError>>;
}

/// A server that shares ownership of a mutable target.
///
/// The target is held in a Tokio [`RwLock`](tokio::sync::RwLock). Immutable calls
/// may run concurrently, while mutable calls acquire the write lock.
pub trait ServerSharedMut<Target, Codec>: ServerBase
where
    Self: Sized,
{
    /// Creates a server and the client connected to it with the default request buffer size.
    ///
    /// The request buffer is [`DEFAULT_BUFFER`] calls. Use
    /// [`with_request_buffer`](Self::with_request_buffer) to choose a different size.
    fn new(target: Arc<tokio::sync::RwLock<Target>>) -> (Self, Self::Client) {
        Self::with_request_buffer(target, DEFAULT_BUFFER)
    }

    /// Creates a server and the client connected to it with the specified request buffer size.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed locally while they
    /// are waiting to be serialized, transferred or deserialized.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn with_request_buffer(
        target: Arc<tokio::sync::RwLock<Target>>, request_buffer: usize,
    ) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](monitor::ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](monitor::ServerMonitor) and kept.
    fn from_req_receiver(target: Arc<tokio::sync::RwLock<Target>>, req_rx: Self::ReqReceiver) -> Self;

    /// The maximum number of calls that are dispatched concurrently.
    ///
    /// This is [`DEFAULT_PARALLELISM`] by default. Each call that is dispatched
    /// concurrently runs on its own task.
    ///
    /// Zero means that calls are dispatched inline, i.e. one call at a time.
    ///
    /// Only calls to methods taking `&self` are dispatched concurrently.
    fn parallelism(&self) -> usize;

    /// Sets the maximum number of calls that are dispatched concurrently.
    ///
    /// This must be set before [serving](Self::serve).
    fn set_parallelism(&mut self, parallelism: usize);

    /// Serves the target object.
    ///
    /// Up to [`parallelism`](Self::parallelism) calls taking a `&self` reference are
    /// processed concurrently. Calls taking a `&mut self` reference are serialized by
    /// obtaining a write lock.
    ///
    /// Serving ends when the client is dropped.
    fn serve(self) -> impl Future<Output = Result<(), ServeError>>;
}

/// Receives remote method calls as values instead of dispatching them to a target.
///
/// This is useful when requests need custom scheduling, routing, persistence, or
/// authorization. Every request contains a one-shot response sender that must be used
/// to complete the corresponding client call.
///
/// Unlike the other server types, a request receiver can be sent to a remote endpoint,
/// which then handles the requests of the client. Any [request receiver
/// monitor](monitor::ReqReceiverMonitor) that was set is not transferred and must be set again
/// on the receiving endpoint.
///
/// A request receiver can also be turned into any other server variant, which then
/// dispatches the requests to a target object. Use `TraitReqReceiver::into_server`,
/// `into_server_ref`, `into_server_ref_mut`, `into_server_shared` and
/// `into_server_shared_mut` for that, or the `from_req_receiver` function of the
/// server, for example [`ServerSharedMut::from_req_receiver`].
///
/// The requests can also be handed over to another client using [`forward`](Self::forward),
/// which lets whatever that client is connected to execute them.
///
/// # Example
///
/// In the following example the server handles the calls of the client as messages
/// instead of implementing the `Counter` trait.
///
/// ```
/// use remoc::prelude::*;
/// use remoc::rtc::{CallError, Req};
///
/// #[rtc::remote]
/// pub trait Counter {
///     async fn value(&self) -> Result<u32, CallError>;
///     async fn increase(&mut self, by: u32) -> Result<(), CallError>;
/// }
///
/// // This would be run on the server.
/// async fn server(mut tx: rch::base::Sender<CounterClient>) {
///     let (mut req_rx, client) = CounterReqReceiver::new();
///     tx.send(client).await.unwrap();
///
///     let mut value = 0;
///     while let Some(req) = req_rx.recv().await.unwrap() {
///         match req {
///             Req::Ref(CounterReqRef::Value { __responder }) => {
///                 let _ = __responder.send(Ok(value));
///             }
///             Req::RefMut(CounterReqRefMut::Increase { __responder, by }) => {
///                 value += by;
///                 let _ = __responder.send(Ok(()));
///             }
///             _ => (),
///         }
///     }
/// }
///
/// // This would be run on the client.
/// async fn client(mut rx: rch::base::Receiver<CounterClient>) {
///     let mut counter = rx.recv().await.unwrap().unwrap();
///     counter.increase(10).await.unwrap();
///     assert_eq!(counter.value().await.unwrap(), 10);
/// }
/// # tokio_test::block_on(remoc::doctest::client_server(server, client));
/// ```
pub trait ReqReceiver<Codec>: ServerBase
where
    Self: Sized,
{
    /// Type of request by value (`self`).
    type Value: ReqEnum;
    /// Type of request by reference (`&self`).
    type Ref: ReqEnum;
    /// Type of request by mutable reference (`&mut self`).
    type RefMut: ReqEnum;

    /// Creates a request receiver and the client connected to it with the default request buffer size.
    ///
    /// The request buffer is [`DEFAULT_BUFFER`] calls. Use
    /// [`with_request_buffer`](Self::with_request_buffer) to choose a different size.
    fn new() -> (Self, Self::Client) {
        Self::with_request_buffer(DEFAULT_BUFFER)
    }

    /// Creates a request receiver and the client connected to it with the specified request buffer size.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be received locally while they
    /// are waiting to be serialized, transferred or deserialized.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn with_request_buffer(request_buffer: usize) -> (Self, Self::Client);

    /// Receives the next request, i.e. method call, from the client.
    ///
    /// Handle the request by first matching on the [`Req::Value`], [`Req::Ref`]
    /// and [`Req::RefMut`] variants, which group the methods by how they take
    /// `self`, and then on the variants of the contained per-kind request enum,
    /// one per method. Response with the result on the [responder](Responder) provided
    /// in the `__responder` field of each method variant.
    ///
    /// Returns `Ok(None)` after all clients have been dropped and all queued
    /// requests have been received.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe. If canceled before returning, no request is
    /// consumed.
    #[allow(clippy::type_complexity)]
    fn recv(
        &mut self,
    ) -> impl Future<Output = Result<Option<Req<Self::Value, Self::Ref, Self::RefMut>>, mpsc::RecvError>> + Send;

    /// Closes the receiver half of the request channel without dropping it.
    ///
    /// This allows to process outstanding requests while stopping the client
    /// from sending new requests.
    fn close(&mut self);

    /// Forwards all requests to the specified client.
    ///
    /// The client executes the requests, i.e. they are handled by whatever the
    /// client is connected to, which may be a server on a remote endpoint.
    ///
    /// Forwarding ends when all clients of this request receiver have been dropped
    /// and all queued requests have been forwarded, or when `client` is disconnected.
    /// `client` is then returned, unless a request for a method taking `self` by value
    /// was forwarded, in which case [`None`] is returned because the object is no
    /// longer served.
    ///
    /// The returned future must be polled for requests to be forwarded; spawn it
    /// as a task if forwarding should proceed in the background.
    ///
    /// # Monitors
    ///
    /// Each request passes through the [request receiver monitor](monitor::ReqReceiverMonitor)
    /// of this request receiver, if one is set, and then through the
    /// [client monitor](monitor::ClientMonitor) of `client`, if one is set.
    ///
    /// A [call guard](monitor::CallGuard) returned by its client monitor is released
    /// after forwarding instead of being held until the request has been processed.
    ///
    /// The maximum response size of `client` does not apply.
    fn forward(
        self, client: Self::Client,
    ) -> impl Future<Output = Result<Option<Self::Client>, ServeError>> + Send;

    /// Converts the request receiver into a [stream](Stream) of requests.
    fn into_stream(self) -> ReqReceiverStream<Self, Codec>
    where
        Self: Send + 'static,
        Codec: 'static,
    {
        ReqReceiverStream::new(self)
    }
}

/// A [stream](Stream) of requests received from the client of a remotable trait.
///
/// This is created by [`ReqReceiver::into_stream`] and yields the requests
/// returned by [`ReqReceiver::recv`]. Each request passes through the
/// [request receiver monitor](monitor::ReqReceiverMonitor), if one is set.
pub struct ReqReceiverStream<R, Codec>
where
    R: ReqReceiver<Codec> + Send + 'static,
    Codec: 'static,
{
    #[allow(clippy::type_complexity)]
    inner: ReusableBoxFuture<'static, (Result<Option<Req<R::Value, R::Ref, R::RefMut>>, mpsc::RecvError>, R)>,
    close: bool,
}

impl<R, Codec> fmt::Debug for ReqReceiverStream<R, Codec>
where
    R: ReqReceiver<Codec> + Send + 'static,
    Codec: 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ReqReceiverStream").finish()
    }
}

impl<R, Codec> ReqReceiverStream<R, Codec>
where
    R: ReqReceiver<Codec> + Send + 'static,
    Codec: 'static,
{
    /// Creates a new request receiver stream wrapping the given request receiver.
    pub fn new(req_rx: R) -> Self {
        Self { inner: ReusableBoxFuture::new(Self::make_future(req_rx, false)), close: false }
    }

    /// Closes the receiver half of the request channel after the next request
    /// is received, preventing the client from sending new requests.
    ///
    /// Already sent requests will still be received.
    pub fn close(&mut self) {
        self.close = true;
    }

    #[allow(clippy::type_complexity)]
    async fn make_future(
        mut req_rx: R, close: bool,
    ) -> (Result<Option<Req<R::Value, R::Ref, R::RefMut>>, mpsc::RecvError>, R) {
        if close {
            req_rx.close();
        }

        let result = req_rx.recv().await;
        (result, req_rx)
    }
}

impl<R, Codec> Stream for ReqReceiverStream<R, Codec>
where
    R: ReqReceiver<Codec> + Send + 'static,
    Codec: 'static,
{
    type Item = Result<Req<R::Value, R::Ref, R::RefMut>, mpsc::RecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let (result, req_rx) = ready!(self.inner.poll(cx));

        let close = self.close;
        self.inner.set(Self::make_future(req_rx, close));

        Poll::Ready(result.transpose())
    }
}

impl<R, Codec> Unpin for ReqReceiverStream<R, Codec> where R: ReqReceiver<Codec> + Send + 'static {}

/// An error that terminates an RTC server.
///
/// Individual application errors returned by trait methods are sent to the
/// caller and do not terminate serving.
#[derive(Debug)]
pub enum ServeError {
    /// Receiving a request from the client failed.
    ReqReceive(mpsc::RecvError),
    /// Sending a response to the client failed,
    ResponseSend(SendingErrorKind),
    /// Forwarding a request to another client failed.
    ///
    /// This can only occur while [forwarding](ReqReceiver::forward) requests.
    Forward(mpsc::SendError<()>),
    /// Server failed because [server monitor](monitor::ServerMonitor) returned [`DispatchDecision::Error`](monitor::DispatchDecision::Error).
    Monitor(Box<dyn Error + Send>),
    /// A call failed and the caller requested the server to stop serving in that case.
    CallFailed {
        /// Name of the method that failed.
        method: &'static str,
    },
}

impl From<mpsc::RecvError> for ServeError {
    fn from(err: mpsc::RecvError) -> Self {
        Self::ReqReceive(err)
    }
}

impl<T> From<SendingError<T>> for ServeError {
    fn from(err: SendingError<T>) -> Self {
        Self::ResponseSend(err.kind())
    }
}

impl From<SendingErrorKind> for ServeError {
    fn from(err: SendingErrorKind) -> Self {
        Self::ResponseSend(err)
    }
}

impl From<mpsc::SendError<()>> for ServeError {
    fn from(err: mpsc::SendError<()>) -> Self {
        Self::Forward(err)
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ReqReceive(err) => write!(f, "failed to receive RTC request: {err}"),
            Self::ResponseSend(err) => write!(f, "failed to send response to RTC request: {err}"),
            Self::Forward(err) => write!(f, "failed to forward RTC request: {err}"),
            Self::Monitor(err) => write!(f, "failed by server monitor: {err}"),
            Self::CallFailed { method } => write!(f, "RTC call to {method} failed"),
        }
    }
}

impl Error for ServeError {}

// Re-exports for proc macro usage.
#[doc(hidden)]
pub use futures::future::FutureExt;
#[doc(hidden)]
pub use futures::stream::Stream;
#[doc(hidden)]
pub use futures::stream::StreamExt;
#[doc(hidden)]
pub use serde::{Deserialize, Serialize};
#[doc(hidden)]
pub use tokio::select;
#[doc(hidden)]
pub use tokio::sync::RwLock as LocalRwLock;
#[doc(hidden)]
pub use tokio::sync::broadcast as local_broadcast;
#[doc(hidden)]
pub use tokio::sync::mpsc as local_mpsc;
#[doc(hidden)]
pub use tracing::Instrument;
#[doc(hidden)]
pub use wokio::task::spawn;

/// Semaphore limiting the number of concurrently dispatched calls.
#[doc(hidden)]
pub fn dispatch_semaphore(parallelism: usize) -> std::sync::Arc<tokio::sync::Semaphore> {
    std::sync::Arc::new(tokio::sync::Semaphore::new(parallelism))
}

/// Acquires a permit to dispatch a call on its own task.
#[doc(hidden)]
pub async fn acquire_dispatch_permit(
    semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
) -> tokio::sync::OwnedSemaphorePermit {
    std::sync::Arc::clone(semaphore).acquire_owned().await.expect("dispatch semaphore is never closed")
}

/// Broadcast sender with no subscribers.
#[doc(hidden)]
pub fn empty_client_drop_tx() -> local_broadcast::Sender<()> {
    local_broadcast::channel(1).0
}
