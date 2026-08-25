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
//! | `Arc<Target>` | `TraitServerShared` ([ServerShared]) | in parallel, with `serve(true)` |
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
//! via a oneshot reply channel.
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
//! `TraitReqReceiver::new(request_buffer)` returns the request receiver first and
//! `TraitClient::new(request_buffer)` the client first. Calls made on the client before
//! the request receiver is attached to a target object are queued.
//!
//! See [ReqReceiver] for details.
//!
//! # Usage
//!
//! Tag your trait with the [remote attribute](remote).
//! Call `new()` on a server type to create a server and corresponding client instance for a
//! target object, which must implement the trait.
//! Send the client to a remote endpoint and then call `serve()` on the server instance to
//! start processing requests by the client.
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
//! # Error handling
//!
//! Since a remote trait call can fail due to connection problems, the return type
//! of all trait functions must always be of the [Result] type.
//! The error type must be able to convert from [CallError] and thus absorb the remote calling error.
//!
//! There is no timeout imposed on a remote call, but the underlying [chmux] connection
//! [pings the remote endpoint](chmux::Cfg::connection_timeout) by default.
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
//! The name `_59` is reserved for the reply channel of a request and cannot be used.
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
//! The server creates a `CounterObj` and obtains a `CounterServerSharedMut` and `CounterClient` for it.
//! The `CounterClient` is then sent to the client, which receives it and calls
//! trait methods on it.
//!
//! ```
//! use std::sync::Arc;
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
//! // This would be run on the client.
//! async fn client(mut rx: rch::base::Receiver<CounterClient>) {
//!     let mut remote_counter = rx.recv().await.unwrap().unwrap();
//!     let mut watch_rx = remote_counter.watch().await.unwrap();
//!
//!     assert_eq!(remote_counter.value().await.unwrap(), 0);
//!
//!     remote_counter.increase(20).await.unwrap();
//!     assert_eq!(remote_counter.value().await.unwrap(), 20);
//!
//!     remote_counter.increase(45).await.unwrap();
//!     assert_eq!(remote_counter.value().await.unwrap(), 65);
//!
//!     assert_eq!(*watch_rx.borrow().unwrap(), 65);
//! }
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<CounterClient>) {
//!     let mut counter_obj = Arc::new(RwLock::new(CounterObj::new()));
//!
//!     let (server, client) = CounterServerSharedMut::new(counter_obj, 1);
//!     tx.send(client).await.unwrap();
//!     server.serve(true).await.unwrap();
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(server, client));
//! ```
//!

pub mod monitor;

mod calls;
pub use calls::calls;

mod reply;
pub use reply::ReplySender;
#[doc(hidden)]
pub use reply::{Completing, IsPipelinableReply, IsReply, PipelinableReplyTo, Reply, ReplyTo, reply_channel};

use futures::future::BoxFuture;
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
    RemoteSend, chmux, codec,
    rch::{SendingError, SendingErrorKind, base, mpsc, oneshot},
};

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
/// The `server(...)` argument allows to limit the generated server variants.
/// Supported variants are: `Value`, `Ref`, `RefMut`, `Shared`, `SharedMut`.
/// Multiple variants can be specified as a comma-separated list.
/// For example, when `#[remoc::rtc::remote(server(SharedMut))]` is applied to `trait Trait` only the
/// `TraitServerSharedMut` server will be generated.
/// If unspecified, all server variants are generated.
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
/// `OtherTraitClient::new(request_buffer)`, hand the request receiver over and call
/// methods on the client without waiting for the call to complete; these calls are
/// queued until the object is available.
///
/// The twin returns a [`Call`], so that the request receiver is handed over immediately
/// and calls on the client can be started without waiting for the session call to
/// complete. Serving of the request receiver continues in the background, so the caller
/// keeps the client it created for as long as it likes. The default implementation calls
/// the original method and [forwards](ReqReceiver::forward) the requests to the client
/// it returns.
///
/// Adding the attribute to an existing method keeps its requests wire compatible.
/// However, calling the twin method on an endpoint that does not know it fails with
/// a receive error there.
///
/// All [serde field attributes](https://serde.rs/field-attrs.html) `#[serde(...)]`
/// are allowed on the arguments of the functions.
/// They will be transferred to the respective field of the request struct that will
/// be sent to the server when the method is called by the client.
/// This can be used to customize serialization and provide defaults for forward and backward
/// compatibility.
///
pub use remoc_macro::remote;

/// Call a method on a remotable trait failed.
#[derive(Debug, Clone)]
pub enum CallError {
    /// The object is not being served.
    ///
    /// The request was never accepted, because serving of the object had already
    /// finished: [`serve`](ServerShared::serve) returned, or the server was dropped
    /// without ever being served.
    NotServed,
    /// Processing the request failed.
    ///
    /// The request was accepted but no reply arrived. The server may have panicked
    /// while handling it, sending the reply may have failed on the server side, or
    /// the request may have been dropped by a client or server monitor.
    Dropped,
    /// Encoding or transferring the request failed; see [`base::SendErrorKind`].
    Send(base::SendErrorKind),
    /// Receiving or decoding the reply failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel carried by the request or reply failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel carried by the request or reply failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// An endpoint forwarding the call or reply could not complete the transfer.
    Forward,
    /// A failure was reported by an endpoint forwarding the call or reply.
    ///
    /// The nested error is [`None`] when that endpoint reported a newer error
    /// variant that this version of Remoc does not recognize.
    Remote(Option<Box<CallError>>),
}

crate::versioned::compact::impl_enum! {
    CallError,
    recover = CallError::Remote(None),
    variants {
        NotServed => "_0",
        Dropped => "_1",
        Send(err: base::SendErrorKind) => "_2",
        Receive(err: base::RecvError) => "_3",
        Connect(err: chmux::ConnectError) => "_4",
        Listen(err: chmux::ListenerError) => "_5",
        Forward => "_6",
        Remote(err: Option<Box<CallError>>) => "_50",
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::NotServed => write!(f, "the remote object is no longer served"),
            Self::Dropped => write!(f, "processing request failed"),
            Self::Send(err) => write!(f, "send error: {err}"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Forward => write!(f, "forwarding error"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl Error for CallError {}

impl<T> From<mpsc::SendError<T>> for CallError {
    fn from(err: mpsc::SendError<T>) -> Self {
        match err {
            mpsc::SendError::Closed(_) => Self::NotServed,
            mpsc::SendError::Send(err) if err.is_closed() => Self::NotServed,
            mpsc::SendError::Send(err) => Self::Send(err),
            mpsc::SendError::Connect(err) => Self::Connect(err),
            mpsc::SendError::Listen(err) => Self::Listen(err),
            mpsc::SendError::Forward => Self::Forward,
        }
    }
}

impl From<oneshot::RecvError> for CallError {
    fn from(err: oneshot::RecvError) -> Self {
        match err {
            oneshot::RecvError::Closed => Self::Dropped,
            oneshot::RecvError::Receive(base::RecvError::Receive(chmux::RecvError::Rejected {
                no_ports: false,
            })) => Self::NotServed,
            oneshot::RecvError::Connect(chmux::ConnectError::Rejected) => Self::NotServed,
            oneshot::RecvError::Receive(err) => Self::Receive(err),
            oneshot::RecvError::Connect(err) => Self::Connect(err),
            oneshot::RecvError::Listen(err) => Self::Listen(err),
            oneshot::RecvError::Remote(err) => Self::Remote(err.map(|err| Box::new(Self::from(*err)))),
        }
    }
}

/// The request enum of a remotely callable trait.
#[doc(hidden)]
pub trait ReqEnum {
    /// The name of the remotely callable trait this request enum belongs to.
    fn trait_name() -> &'static str;

    /// Trait method name this request enum variant belongs to.
    ///
    /// # Panics
    /// Panics when called on the `__Phantom` variant.
    fn method_name(&self) -> &'static str;

    /// Whether the caller permits the server to dispatch this request on its own task.
    ///
    /// # Panics
    /// Panics when called on the `__Phantom` variant.
    fn allow_spawn(&self) -> bool;
}

/// A request from client to server.
///
/// This groups the methods of a remotable trait by how they take `self`.
/// Each variant holds a per-kind request enum that in turn has one variant per
/// method of that kind.
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

    /// Creates a client and the [request receiver](Self::ReqReceiver) connected to it.
    ///
    /// `request_buffer` bounds the number of calls that can be queued for sending.
    ///
    /// Calls made on the client are queued until the request receiver is
    /// [attached to a target object](ReqReceiver) or its requests are
    /// [forwarded](ReqReceiver::forward) to another client.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn new(request_buffer: usize) -> (Self, Self::ReqReceiver)
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

    /// The maximum allowed size of a reply in bytes.
    fn max_reply_size(&self) -> usize;

    /// Sets the maximum allowed size of a reply in bytes.
    fn set_max_reply_size(&mut self, max_reply_size: usize);

    /// Whether the server may dispatch calls made by this client on their own tasks.
    ///
    /// This is `true` by default. When set to `false` the server serves the calls of
    /// this client sequentially, in the order they were made, even when it was started
    /// with `spawn` enabled. Calls of other clients are unaffected.
    ///
    /// Only [`ServerShared`] and [`ServerSharedMut`] dispatch in parallel at all, and
    /// only calls to methods taking `&self`. A call to a method taking `&mut self`
    /// requires exclusive access to the target and is always served sequentially,
    /// thus this has no effect on it.
    fn allow_spawn(&self) -> bool;

    /// Sets whether the server may dispatch calls made by this client on their own tasks.
    fn set_allow_spawn(&mut self, allow_spawn: bool);

    /// Whether the server shall stop serving when a call made by this client fails.
    ///
    /// This is `false` by default. When set to `true` a call that returns an error
    /// makes the server stop with [`ServeError::CallFailed`] after the error has been
    /// sent to this client.
    fn stop_on_error(&self) -> bool;

    /// Sets whether the server shall stop serving when a call made by this client fails.
    fn set_stop_on_error(&mut self, stop_on_error: bool);
}

/// A remote method call that has been started.
///
/// This is returned by the `<name>_call` twin of every method of a remotable trait,
/// which starts the call without waiting for its result. Await this to obtain the
/// result.
///
/// Starting several calls before awaiting their results avoids one round trip per
/// call, since the requests are transferred to the server without waiting for the
/// reply of the preceding one. The requests are transferred in the order the calls
/// were started.
///
/// Dropping this without awaiting it cancels the call, unless the method is marked
/// with `#[no_cancel]`. Note that the object may already have been called when the
/// cancellation reaches the server.
#[must_use = "the RTC call is cancelled when Call is dropped"]
pub struct Call<R> {
    method: &'static str,
    inner: CallInner<R>,
}

enum CallInner<R> {
    /// The result is already available, because the object was called locally.
    Ready(Option<R>),
    /// The result is awaited from the server.
    Pending(BoxFuture<'static, R>),
}

impl<R> fmt::Debug for Call<R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Call").field("method", &self.method).finish()
    }
}

impl<R> Call<R> {
    /// A call that has already been performed, yielding `result`.
    #[doc(hidden)]
    pub fn ready(method: &'static str, result: R) -> Self {
        Self { method, inner: CallInner::Ready(Some(result)) }
    }

    /// A call whose result is obtained by awaiting `result`.
    #[doc(hidden)]
    pub fn pending(method: &'static str, result: impl Future<Output = R> + Send + 'static) -> Self {
        Self { method, inner: CallInner::Pending(result.boxed()) }
    }
}

impl<T, E> Call<Result<T, E>> {
    /// Applies `op` to the error of the call, leaving its value untouched.
    ///
    /// Use this to bring calls with differing error types to a common one, for
    /// example to await them together using [`try_join`](tokio::try_join), which
    /// requires all of them to have the same error type.
    pub fn map_err<F>(self, op: impl FnOnce(E) -> F + Send + 'static) -> Call<Result<T, F>>
    where
        T: Send + 'static,
        E: 'static,
        F: Send + 'static,
    {
        let Self { method, inner } = self;

        let inner = match inner {
            CallInner::Ready(result) => CallInner::Ready(result.map(|result| result.map_err(op))),
            CallInner::Pending(result) => CallInner::Pending(async move { result.await.map_err(op) }.boxed()),
        };

        Call { method, inner }
    }
}

impl<T, E> Call<Result<T, E>>
where
    T: Send + 'static,
    E: fmt::Display + Send + 'static,
{
    /// Lets the RTC call continue in the background.
    ///
    /// Errors are logged with [warning log level](tracing::Level::WARN).
    pub fn spawn(self) {
        wokio::spawn(
            async move {
                let method = self.method;
                if let Err(err) = self.await {
                    tracing::warn!(%err, method, "calling a remote method failed");
                }
            }
            .in_current_span(),
        );
    }
}

/// Maps the error of a started call before it is awaited.
///
/// This is implemented for the future returned by every `<name>_call` method of a
/// remotable trait, so that calls with differing error types can be brought to a
/// common one before they are awaited together.
pub trait CallFutureExt<T, E>: Future<Output = Call<Result<T, E>>> + Sized {
    /// Applies `op` to the error of the call, leaving its value untouched.
    ///
    /// This is [`Call::map_err`] applied to the call once it has been started.
    #[allow(clippy::async_yields_async)]
    fn map_err<F>(self, op: impl FnOnce(E) -> F + Send + 'static) -> impl Future<Output = Call<Result<T, F>>>
    where
        T: Send + 'static,
        E: 'static,
        F: Send + 'static,
    {
        async move { self.await.map_err(op) }
    }
}

impl<Fut, T, E> CallFutureExt<T, E> for Fut where Fut: Future<Output = Call<Result<T, E>>> {}

impl<R> Unpin for Call<R> {}

impl<R> Future for Call<R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match &mut self.get_mut().inner {
            CallInner::Ready(result) => Poll::Ready(result.take().expect("Call polled after completion")),
            CallInner::Pending(result) => result.poll_unpin(cx),
        }
    }
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

/// Allows setting the [client monitor](ClientMonitor) on a [client](Client).
pub trait MonitorableClient {
    /// Type of request by value (`self`).
    type Value: ReqEnum;
    /// Type of request by reference (`&self`).
    type Ref: ReqEnum;
    /// Type of request by mutable reference (`&mut self`).
    type RefMut: ReqEnum;

    /// Sets the [client monitor](ClientMonitor).
    fn set_monitor(&mut self, monitor: impl ClientMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);
}

/// Allows monitoring each request a client makes.
pub trait ClientMonitor<Value, Ref, RefMut>: Send + Sync
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    /// Called for each request before sending it to server.
    ///
    /// The function can inspect the request and decide whether it should be
    /// sent to the server for processing or dropped.
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision>;
}

/// Decision on how a request should be processed made by the [client monitor](ClientMonitor).
pub enum CallDecision {
    /// Process the request normally.
    ///
    /// The request is sent to the server for processing.
    Pass,
    /// Guard the request and process it normally.
    ///
    /// The request is processed as if [`Pass`](Self::Pass) is specified.
    /// However, the supplied [`CallGuard`] is held during processing and dropped
    /// once the request is finished.
    Guard(Box<dyn CallGuard>),
    /// Drop the request.
    ///
    /// The called client method fails with [`CallError::Dropped`].
    Drop,
}

impl fmt::Debug for CallDecision {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "Pass"),
            Self::Guard(_) => write!(f, "Guard"),
            Self::Drop => write!(f, "Drop"),
        }
    }
}

/// Request call guard.
///
/// It is held until the guarded request is processed and then dropped.
pub trait CallGuard: Send {
    /// Notifies the request call guard that the called method returned
    /// an error.
    fn failed(&mut self) {}

    /// Notifies the request call guard that receiving the reply from the
    /// server failed.
    fn reply_failed(&mut self, err: &oneshot::RecvError) {
        let _ = err;
    }
}

/// Combines two [client](ClientMonitor) or [server](ServerMonitor) monitors into one.
///
/// Construct it directly from the two monitors to combine, for example
/// `ChainedMonitor(first, second)`, and install the result on a client or server.
/// To combine more than two monitors, nest the construction, e.g.
/// `ChainedMonitor(a, ChainedMonitor(b, c))`.
///
/// For each request the two monitors are evaluated in order: first `self.0`, then
/// `self.1`. The combined decision is formed as follows:
///
///  * If a monitor drops the request ([`CallDecision::Drop`] / [`DispatchDecision::Drop`]),
///    the request is dropped and the remaining monitor is not evaluated.
///  * For a server monitor, if a monitor returns [`DispatchDecision::Error`], serving
///    stops with that error and the remaining monitor is not evaluated.
///  * Otherwise the request passes. Any guard produced by either monitor is held for
///    the duration of the request and released once it finishes. Guards are released
///    in reverse order, i.e. `self.1`'s guard is dropped before `self.0`'s, and guard
///    notifications ([`failed`](CallGuard::failed), [`reply_failed`](CallGuard::reply_failed)
///    and [`failed`](DispatchGuard::failed)) are forwarded to both.
///
/// Because evaluation is sequential and short-circuits, the order matters for monitors
/// that account for a request only while their returned future is awaited (such as the
/// [rate](monitor::RateLimitMonitor) and [concurrent](monitor::ConcurrentLimitMonitor)
/// limiters): a request dropped or rejected by `self.0` is never seen by `self.1`.
pub struct ChainedMonitor<A, B>(pub A, pub B);

impl<A, B, Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for ChainedMonitor<A, B>
where
    A: ClientMonitor<Value, Ref, RefMut>,
    B: ClientMonitor<Value, Ref, RefMut>,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        let pre_call_0 = self.0.pre_call(req);
        let pre_call_1 = self.1.pre_call(req);

        async move {
            let guard_0 = match pre_call_0.await {
                CallDecision::Pass => None,
                CallDecision::Guard(guard) => Some(guard),
                CallDecision::Drop => return CallDecision::Drop,
            };

            let guard_1 = match pre_call_1.await {
                CallDecision::Pass => None,
                CallDecision::Guard(guard) => Some(guard),
                CallDecision::Drop => return CallDecision::Drop,
            };

            match (guard_0, guard_1) {
                (None, None) => CallDecision::Pass,
                (Some(guard0), None) => CallDecision::Guard(guard0),
                (None, Some(guard1)) => CallDecision::Guard(guard1),
                (Some(guard0), Some(guard1)) => CallDecision::Guard(Box::new(ChainedCallGuard(guard1, guard0))),
            }
        }
        .boxed()
    }
}

struct ChainedCallGuard(Box<dyn CallGuard>, Box<dyn CallGuard>);
impl CallGuard for ChainedCallGuard {
    fn failed(&mut self) {
        self.0.failed();
        self.1.failed();
    }

    fn reply_failed(&mut self, err: &oneshot::RecvError) {
        self.0.reply_failed(err);
        self.1.reply_failed(err);
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
    /// Creates a server and the client connected to it.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn new(target: Target, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](ServerMonitor) and kept.
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
    /// Creates a server and the client connected to it.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn new(target: &'target Target, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](ServerMonitor) and kept.
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
    /// Creates a server and the client connected to it.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn new(target: &'target mut Target, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](ServerMonitor) and kept.
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
    /// Creates a server and the client connected to it.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn new(target: Arc<Target>, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](ServerMonitor) and kept.
    fn from_req_receiver(target: Arc<Target>, req_rx: Self::ReqReceiver) -> Self;

    /// Serves the target object.
    ///
    /// If `spawn` is true, remote calls are executed in parallel by spawning a task per call.
    ///
    /// Serving ends when the client is dropped.
    fn serve(self, spawn: bool) -> impl Future<Output = Result<(), ServeError>>;
}

/// A server that shares ownership of a mutable target.
///
/// The target is held in a Tokio [`RwLock`](tokio::sync::RwLock). Immutable calls
/// may run concurrently, while mutable calls acquire the write lock.
pub trait ServerSharedMut<Target, Codec>: ServerBase
where
    Self: Sized,
{
    /// Creates a server and the client connected to it.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be processed.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn new(target: Arc<tokio::sync::RwLock<Target>>, request_buffer: usize) -> (Self, Self::Client);

    /// Creates a server that processes the requests of an existing request receiver.
    ///
    /// This allows to serve a client that is already connected to the request receiver,
    /// for example a client on a remote endpoint that sent the request receiver here.
    ///
    /// Requests that are already queued in the request receiver are processed by the
    /// server. A [request receiver monitor](ReqReceiverMonitor) set on the request
    /// receiver is converted into a [server monitor](ServerMonitor) and kept.
    fn from_req_receiver(target: Arc<tokio::sync::RwLock<Target>>, req_rx: Self::ReqReceiver) -> Self;

    /// Serves the target object.
    ///
    /// If `spawn` is true, remote calls taking a `&self` reference are executed
    /// in parallel by spawning a task per call.
    /// Remote calls taking a `&mut self` reference are serialized by obtaining a write lock.
    ///
    /// Serving ends when the client is dropped.
    fn serve(self, spawn: bool) -> impl Future<Output = Result<(), ServeError>>;
}

/// Receives remote method calls as values instead of dispatching them to a target.
///
/// This is useful when requests need custom scheduling, routing, persistence, or
/// authorization. Every request contains a one-shot reply sender that must be used
/// to complete the corresponding client call.
///
/// Unlike the other server types, a request receiver can be sent to a remote endpoint,
/// which then handles the requests of the client. Any [request receiver
/// monitor](ReqReceiverMonitor) that was set is not transferred and must be set again
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
///     let (mut req_rx, client) = CounterReqReceiver::new(1);
///     tx.send(client).await.unwrap();
///
///     let mut value = 0;
///     while let Some(req) = req_rx.recv().await.unwrap() {
///         match req {
///             Req::Ref(CounterReqRef::Value { __reply_tx }) => {
///                 let _ = __reply_tx.send(Ok(value));
///             }
///             Req::RefMut(CounterReqRefMut::Increase { __reply_tx, by }) => {
///                 value += by;
///                 let _ = __reply_tx.send(Ok(()));
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

    /// Creates a request receiver and the client connected to it.
    ///
    /// `request_buffer` bounds the number of calls that can wait to be received.
    ///
    /// # Panics
    ///
    /// Panics if `request_buffer` is zero.
    fn new(request_buffer: usize) -> (Self, Self::Client);

    /// Receives the next request, i.e. method call, from the client.
    ///
    /// Handle the request by first matching on the [`Req::Value`], [`Req::Ref`]
    /// and [`Req::RefMut`] variants, which group the methods by how they take
    /// `self`, and then on the variants of the contained per-kind request enum,
    /// one per method. Reply with the result on the oneshot sender provided in
    /// the `__reply_tx` field of each method variant.
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
    /// Each request passes through the [request receiver monitor](ReqReceiverMonitor)
    /// of this request receiver, if one is set, and then through the
    /// [client monitor](ClientMonitor) of `client`, if one is set.
    ///
    /// A [call guard](CallGuard) returned by its client monitor is released
    /// after forwarding instead of being held until the request has been processed.
    ///
    /// The maximum reply size of `client` does not apply.
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
/// [request receiver monitor](ReqReceiverMonitor), if one is set.
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

/// Allows setting the [server monitor](ServerMonitor) on a [server](ServerBase).
pub trait MonitorableServer {
    /// Type of request by value (`self`).
    type Value: ReqEnum;
    /// Type of request by reference (`&self`).
    type Ref: ReqEnum;
    /// Type of request by mutable reference (`&mut self`).
    type RefMut: ReqEnum;

    /// Sets the [server monitor](ServerMonitor).
    fn set_monitor(&mut self, monitor: impl ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);
}

/// Allows monitoring each request a server handles.
pub trait ServerMonitor<Value, Ref, RefMut>: Send
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    /// Called for each request before dispatch to its handling method.
    ///
    /// The function can inspect the request and decide whether it should be
    /// handled, dropped or the server should fail with a custom error.
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision>;
}

/// Allows setting the [request receiver monitor](ReqReceiverMonitor) on a
/// [request receiver](ReqReceiver).
pub trait MonitorableReqReceiver {
    /// Type of request by value (`self`).
    type Value: ReqEnum;
    /// Type of request by reference (`&self`).
    type Ref: ReqEnum;
    /// Type of request by mutable reference (`&mut self`).
    type RefMut: ReqEnum;

    /// Sets the [request receiver monitor](ReqReceiverMonitor).
    fn set_monitor(&mut self, monitor: impl ReqReceiverMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static);
}

/// Allows monitoring each request a [request receiver](ReqReceiver) receives.
///
/// Unlike a [server monitor](ServerMonitor), it cannot guard a request or stop
/// the receiver with a custom error; it can only let a request [pass](RecvDecision::Pass)
/// or [drop](RecvDecision::Drop) it.
pub trait ReqReceiverMonitor<Value, Ref, RefMut>: Send
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    /// Called for each received request before it is returned from
    /// [`ReqReceiver::recv`].
    ///
    /// The function can inspect the request and decide whether it should be
    /// returned to the caller or dropped.
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision>;
}

impl<A, B, Value, Ref, RefMut> ReqReceiverMonitor<Value, Ref, RefMut> for ChainedMonitor<A, B>
where
    A: ReqReceiverMonitor<Value, Ref, RefMut>,
    B: ReqReceiverMonitor<Value, Ref, RefMut>,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_recv<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        let pre_recv_0 = self.0.pre_recv(req);
        let pre_recv_1 = self.1.pre_recv(req);

        async move {
            match pre_recv_0.await {
                RecvDecision::Pass => (),
                RecvDecision::Drop => return RecvDecision::Drop,
            }

            pre_recv_1.await
        }
        .boxed()
    }
}

/// Decision on how a received request should be processed made by the
/// [request receiver monitor](ReqReceiverMonitor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvDecision {
    /// Return the request to the caller of [`ReqReceiver::recv`].
    Pass,
    /// Drop the request and receive the next one.
    ///
    /// The client-side method fails with [`CallError::Dropped`].
    Drop,
}

impl<A, B, Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for ChainedMonitor<A, B>
where
    A: ServerMonitor<Value, Ref, RefMut>,
    B: ServerMonitor<Value, Ref, RefMut>,
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let pre_dispatch_0 = self.0.pre_dispatch(req);
        let pre_dispatch_1 = self.1.pre_dispatch(req);

        async move {
            let guard_0 = match pre_dispatch_0.await {
                DispatchDecision::Pass => None,
                DispatchDecision::Guard(guard) => Some(guard),
                DispatchDecision::Drop => return DispatchDecision::Drop,
                DispatchDecision::Error(err) => return DispatchDecision::Error(err),
            };

            let guard_1 = match pre_dispatch_1.await {
                DispatchDecision::Pass => None,
                DispatchDecision::Guard(guard) => Some(guard),
                DispatchDecision::Drop => return DispatchDecision::Drop,
                DispatchDecision::Error(err) => return DispatchDecision::Error(err),
            };

            match (guard_0, guard_1) {
                (None, None) => DispatchDecision::Pass,
                (Some(guard0), None) => DispatchDecision::Guard(guard0),
                (None, Some(guard1)) => DispatchDecision::Guard(guard1),
                (Some(guard0), Some(guard1)) => {
                    DispatchDecision::Guard(Box::new(ChainedDispatchGuard(guard1, guard0)))
                }
            }
        }
        .boxed()
    }
}

struct ChainedDispatchGuard(Box<dyn DispatchGuard>, Box<dyn DispatchGuard>);
impl DispatchGuard for ChainedDispatchGuard {
    fn failed(&mut self) {
        self.0.failed();
        self.1.failed();
    }
}

/// Request dispatch guard.
///
/// It is held until the guarded request is processed and then dropped.
pub trait DispatchGuard: Send {
    /// Notifies the request dispatch guard that the called method returned
    /// an error.
    fn failed(&mut self) {}
}

/// Decision on how a request should be processed made by the [server monitor](ServerMonitor).
pub enum DispatchDecision {
    /// Process the request normally.
    ///
    /// In case of the server monitor, the request is dispatched to the corresponding
    /// function of the remotable trait implementation.
    Pass,
    /// Guard the request and process it normally.
    ///
    /// The request is processed as if [`Pass`](Self::Pass) is specified.
    /// However, the supplied [`DispatchGuard`] is held during processing and dropped
    /// once the request is finished.
    Guard(Box<dyn DispatchGuard>),
    /// Drop the request.
    ///
    /// The client-side method fails with [`CallError::Dropped`].
    Drop,
    /// Stop serving and fail returning [`ServeError::Monitor`].
    Error(Box<dyn Error + Send>),
}

impl fmt::Debug for DispatchDecision {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "Pass"),
            Self::Guard(_) => write!(f, "Guard"),
            Self::Drop => write!(f, "Drop"),
            Self::Error(err) => f.debug_tuple("Error").field(err).finish(),
        }
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! server_monitor_pre_dispatch {
    ($monitor:expr, $req:expr) => {
        match $monitor.pre_dispatch(&$req).await {
            ::remoc::rtc::DispatchDecision::Pass => ::std::boxed::Box::new(::remoc::rtc::DefaultGuard),
            ::remoc::rtc::DispatchDecision::Guard(guard) => guard,
            ::remoc::rtc::DispatchDecision::Drop => {
                match &$req {
                    Ok(None) => (),
                    Err(err) if err.is_disconnected() => (),
                    _ => continue,
                }
                ::std::boxed::Box::new(::remoc::rtc::DefaultGuard)
            }
            ::remoc::rtc::DispatchDecision::Error(err) => return Err(::remoc::rtc::ServeError::Monitor(err)),
        }
    };
    ($monitor:expr, $req:expr, $target:expr) => {
        match $monitor.pre_dispatch(&$req).await {
            ::remoc::rtc::DispatchDecision::Pass => ::std::boxed::Box::new(::remoc::rtc::DefaultGuard),
            ::remoc::rtc::DispatchDecision::Guard(guard) => guard,
            ::remoc::rtc::DispatchDecision::Drop => {
                match &$req {
                    Ok(None) => (),
                    Err(err) if err.is_disconnected() => (),
                    _ => continue,
                }
                ::std::boxed::Box::new(::remoc::rtc::DefaultGuard)
            }
            ::remoc::rtc::DispatchDecision::Error(err) => {
                return (Some($target), Err(::remoc::rtc::ServeError::Monitor(err)))
            }
        }
    };
}
#[doc(hidden)]
pub use crate::server_monitor_pre_dispatch;

#[macro_export]
#[doc(hidden)]
macro_rules! req_receiver_monitor_pre_recv {
    ($monitor:expr, $req:expr) => {
        match $monitor.pre_recv(&$req).await {
            ::remoc::rtc::RecvDecision::Pass => (),
            ::remoc::rtc::RecvDecision::Drop => match &$req {
                Ok(None) => (),
                Err(err) if err.is_disconnected() => (),
                _ => continue,
            },
        }
    };
}
#[doc(hidden)]
pub use crate::req_receiver_monitor_pre_recv;

/// The default [client](ClientMonitor) and [server](ServerMonitor).
///
/// It passes all requests.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct DefaultMonitor;

impl<Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for DefaultMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_call<'a>(&self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        let _ = req;
        std::future::ready(CallDecision::Pass).boxed()
    }
}

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut> for DefaultMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let _ = req;
        std::future::ready(DispatchDecision::Pass).boxed()
    }
}

impl<Value, Ref, RefMut> ReqReceiverMonitor<Value, Ref, RefMut> for DefaultMonitor
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_recv<'a>(
        &mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, RecvDecision> {
        let _ = req;
        std::future::ready(RecvDecision::Pass).boxed()
    }
}

#[doc(hidden)]
pub fn default_client_monitor<Value, Ref, RefMut>() -> Arc<dyn ClientMonitor<Value, Ref, RefMut>>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    Arc::new(DefaultMonitor)
}

#[doc(hidden)]
pub fn default_req_receiver_monitor<Value, Ref, RefMut>() -> Box<dyn ReqReceiverMonitor<Value, Ref, RefMut>>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    Box::new(DefaultMonitor)
}

/// Adapts a [request receiver monitor](ReqReceiverMonitor) into a
/// [server monitor](ServerMonitor).
///
/// Both are invoked at the same point of the request pipeline and
/// [`RecvDecision`] is a subset of [`DispatchDecision`], thus every decision
/// can be translated.
struct ReqReceiverMonitorAsServerMonitor<Value, Ref, RefMut>(Box<dyn ReqReceiverMonitor<Value, Ref, RefMut>>);

impl<Value, Ref, RefMut> ServerMonitor<Value, Ref, RefMut>
    for ReqReceiverMonitorAsServerMonitor<Value, Ref, RefMut>
where
    Value: ReqEnum,
    Ref: ReqEnum,
    RefMut: ReqEnum,
{
    fn pre_dispatch<'a>(
        &'a mut self, req: &'a Result<Option<Req<Value, Ref, RefMut>>, mpsc::RecvError>,
    ) -> BoxFuture<'a, DispatchDecision> {
        let pre_recv = self.0.pre_recv(req);

        async move {
            match pre_recv.await {
                RecvDecision::Pass => DispatchDecision::Pass,
                RecvDecision::Drop => DispatchDecision::Drop,
            }
        }
        .boxed()
    }
}

/// Converts the monitor of a [request receiver](ReqReceiver) into the monitor of a
/// server, when the request receiver is converted into a server.
#[doc(hidden)]
pub fn req_receiver_monitor_as_server_monitor<Value, Ref, RefMut>(
    monitor: Box<dyn ReqReceiverMonitor<Value, Ref, RefMut>>,
) -> Box<dyn ServerMonitor<Value, Ref, RefMut>>
where
    Value: ReqEnum + 'static,
    Ref: ReqEnum + 'static,
    RefMut: ReqEnum + 'static,
{
    Box::new(ReqReceiverMonitorAsServerMonitor(monitor))
}

/// The default [call](CallGuard) and [dispatch](DispatchGuard).
///
/// It does nothing.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct DefaultGuard;

impl CallGuard for DefaultGuard {}
impl DispatchGuard for DefaultGuard {}

/// An error that terminates an RTC server.
///
/// Individual application errors returned by trait methods are sent to the
/// caller and do not terminate serving.
#[derive(Debug)]
pub enum ServeError {
    /// Receiving a request from the client failed.
    ReqReceive(mpsc::RecvError),
    /// Sending a reply to the client failed,
    ReplySend(SendingErrorKind),
    /// Forwarding a request to another client failed.
    ///
    /// This can only occur while [forwarding](ReqReceiver::forward) requests.
    Forward(mpsc::SendError<()>),
    /// Server failed because [server monitor](ServerMonitor) returned [`DispatchDecision::Error`].
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
        Self::ReplySend(err.kind())
    }
}

impl From<SendingErrorKind> for ServeError {
    fn from(err: SendingErrorKind) -> Self {
        Self::ReplySend(err)
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
            Self::ReplySend(err) => write!(f, "failed to send reply to RTC request: {err}"),
            Self::Forward(err) => write!(f, "failed to forward RTC request: {err}"),
            Self::Monitor(err) => write!(f, "failed by server monitor: {err}"),
            Self::CallFailed { method } => write!(f, "RTC call to {method} failed"),
        }
    }
}

impl Error for ServeError {}

impl From<ServeError> for CallError {
    fn from(err: ServeError) -> Self {
        match err {
            ServeError::ReqReceive(err) => match err {
                mpsc::RecvError::Receive(err) => Self::Receive(err),
                mpsc::RecvError::Connect(err) => Self::Connect(err),
                mpsc::RecvError::Listen(err) => Self::Listen(err),
                mpsc::RecvError::Remote(_) => Self::Forward,
            },
            ServeError::ReplySend(SendingErrorKind::Send(err)) => Self::Send(err),
            ServeError::ReplySend(SendingErrorKind::Dropped) => Self::Dropped,
            ServeError::Forward(err) => Self::from(err),
            ServeError::Monitor(_) => Self::Forward,
            ServeError::CallFailed { .. } => Self::Forward,
        }
    }
}

// Re-exports for proc macro usage.
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
pub use wokio::task::spawn;
#[doc(hidden)]
pub type ReplyErrorSender = tokio::sync::mpsc::Sender<ServeError>;
#[doc(hidden)]
pub use futures::future::FutureExt;
#[doc(hidden)]
pub use futures::stream::Stream;
#[doc(hidden)]
pub use futures::stream::StreamExt;
#[doc(hidden)]
pub use tracing::Instrument;

/// Create channel for queueing reply sending errors.
#[doc(hidden)]
pub fn reply_error_channel() -> (ReplyErrorSender, tokio::sync::mpsc::Receiver<ServeError>) {
    tokio::sync::mpsc::channel(16)
}

/// Broadcast sender with no subscribers.
#[doc(hidden)]
pub fn empty_client_drop_tx() -> local_broadcast::Sender<()> {
    local_broadcast::channel(1).0
}

/// Missing maximum reply size value for backwards compatibility.
#[doc(hidden)]
pub const fn missing_max_reply_size() -> usize {
    usize::MAX
}

/// Completes a call by replying to the request.
#[doc(hidden)]
pub async fn complete_call<R, Codec>(
    reply_to: ReplyTo<R, Codec>, method: &'static str, err_tx: &ReplyErrorSender,
    mut dispatch_guard: Box<dyn DispatchGuard>, result: R,
) where
    R: IsReply,
    Reply<R>: RemoteSend,
    Codec: codec::Codec,
{
    if result.is_error() {
        dispatch_guard.failed();
        if reply_to.stop_on_error() {
            let _ = err_tx.send(ServeError::CallFailed { method }).await;
        }
    }

    let Ok(sending) = reply_to.send(result) else { return };

    let err_tx = err_tx.clone();
    wokio::spawn(
        async move {
            if let Err(err) = sending.await {
                let kind = err.kind();
                match &kind {
                    SendingErrorKind::Send(base::SendErrorKind::Send(_)) => return,
                    SendingErrorKind::Dropped => return,
                    _ => (),
                }
                let _ = err_tx.send(kind.into()).await;
            }

            drop(dispatch_guard);
        }
        .in_current_span(),
    );
}

/// Serialization for `max_reply_size` field.
#[doc(hidden)]
pub mod serde_max_reply_size {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::borrow::Borrow;

    /// Serialization function.
    ///
    /// This is generic over `T` so that it can also be used for a field holding
    /// a reference, as used by the generated serialization types.
    pub fn serialize<T, S>(max_reply_size: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Borrow<usize>,
        S: Serializer,
    {
        let max_reply_size = u64::try_from(*max_reply_size.borrow()).unwrap_or(u64::MAX);
        max_reply_size.serialize(serializer)
    }

    /// Deserialization function.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<usize, D::Error>
    where
        D: Deserializer<'de>,
    {
        let max_reply_size = u64::deserialize(deserializer)?;
        Ok(usize::try_from(max_reply_size).unwrap_or(usize::MAX))
    }
}
