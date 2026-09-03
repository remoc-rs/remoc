//! Remote async functions and closures.
//!
//! This module contains wrappers around async functions and closures to make them
//! callable from a remote endpoint.
//! Since Rust differentiates between immutable, mutable and by-value functions,
//! remote wrappers for all three kinds of functions are provided here.
//!
//! All wrappers take between zero and ten arguments, but you can use a tuple as
//! argument if you need more than that.
//! The arguments and return type of the function must be [remote sendable](crate::RemoteSend).
//!
//! # Usage
//!
//! Create a wrapper locally and send it to a remote endpoint, for example over a
//! channel from the [rch](crate::rch) module.
//! You must use the `new_n` method where `n` is the number of arguments of the function.
//! You can also send the wrapper as part of a larger object, such as a struct, tuple
//! or enum.
//! Then use the `call` method on the remote endpoint to remotely invoke the local function.
//!
//! The wrapped function executes on the endpoint where the wrapper was created.
//! Arguments are transferred to that endpoint and the return value is transferred
//! back to the caller.
//!
//! # Return type
//!
//! Since a remote function call can fail due to connection problems, the return type
//! of the wrapped function must always be of the [Result] type.
//! Thus your function should return a [Result] type with an error type that can
//! convert from [CallError] and thus absorb the remote calling error.
//! If you return a different type the `call` method will not be available on the wrapper,
//! but you can still use the `try_call` method, which wraps the result into a [Result] type.
//!
//! # Cancellation
//!
//! If the caller drops the future while it is executing or the connection is interrupted
//! the remote function is automatically cancelled at the next `await` point.
//!
//! # Providers
//!
//! Optionally you can use the `provided` method of each wrapper to obtain a
//! provider for each remote function wrapper.
//! This allows you to drop the wrapped function without relying upon the
//! remote endpoint for that.
//! This is especially useful when you connect to untrusted remote endpoints
//! that could try to obtain and keep a large number of remote function wrappers to
//! perform a denial of service attack by exhausting your memory.
//!
//! # Concurrency limiting
//!
//! [RFn] spawns a new async task for each invocation, up to a configurable
//! maximum of [RFnProvider::DEFAULT_MAX_CONCURRENCY] concurrent invocations (default: 32).
//! When the limit is reached, new invocations wait until a running one completes.
//!
//! The limit can be adjusted dynamically via
//! [RFnProvider::set_max_concurrency](RFnProvider::set_max_concurrency)
//! and queried via [RFnProvider::max_concurrency](RFnProvider::max_concurrency).
//!
//! # Tracing
//!
//! Calls of a remote function can create [tracing](::tracing) spans,
//! one at the caller and one for processing the call where the function was created.
//! The caller sends the [context](crate::tracing::TracingContext) of its span along with
//! the call, so that the span processing it becomes a child of it.
//! If an OpenTelemetry layer is installed on the tracing subscriber, both spans thus appear
//! in one distributed trace; otherwise they share a random span id, which is recorded on
//! both of them, so that the logs of both endpoints can be matched.
//! See the [tracing](crate::tracing) module for details.
//!
//! By default no spans are created; use the `set_tracing_level` method of the wrapper
//! to enable them at the specified level.
//! The spans use the target `remoc::rfn::call` and are named after the function,
//! which by default is derived from the wrapper and argument types and can be
//! changed using the `set_name` method.
//!
//! Both settings are applied to the spans created at the caller and, as long as the
//! wrapper has not been sent to a remote endpoint, to the spans processing the calls.
//! A received wrapper can only change the spans of its own calls.
//! The settings take effect for subsequent calls.
//! The `set_tracing` method adjusts whether the caller creates a span and whether
//! it sends the context, see [`Tracing`](crate::tracing::Tracing).
//!
//! # Alternatives
//!
//! If you need to expose several functions remotely that operate on the same object
//! consider [remote trait calling](crate::rtc) instead.
//!

use std::{error::Error, fmt};

use crate::{
    chmux,
    rch::{base, oneshot},
};

/// An error occurred during calling a remote function.
#[derive(Clone, Debug)]
pub enum CallError {
    /// The provider was dropped before replying, or the remote function panicked.
    Dropped,
    /// Receiving or decoding the result failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel contained in the result failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel contained in the result failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// A failure was reported by an endpoint forwarding the call or result.
    ///
    /// The nested error is [`None`] when that endpoint reported a newer error
    /// variant that this version of Remoc does not recognize.
    Remote(Option<Box<CallError>>),
}

crate::versioned::compact::impl_enum! {
    CallError,
    recover = CallError::Remote(None),
    variants {
        Dropped => "_0",
        Receive(err: base::RecvError) => "_1",
        Connect(err: chmux::ConnectError) => "_2",
        Listen(err: chmux::ListenerError) => "_3",
        Remote(err: Option<Box<CallError>>) => "_50",
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dropped => write!(f, "provider dropped or function panicked"),
            Self::Receive(err) => write!(f, "receive error: {err}"),
            Self::Connect(err) => write!(f, "connect error: {err}"),
            Self::Listen(err) => write!(f, "listen error: {err}"),
            Self::Remote(Some(err)) => write!(f, "remote {err}"),
            Self::Remote(None) => write!(f, "unknown remote error"),
        }
    }
}

impl From<oneshot::RecvError> for CallError {
    fn from(err: oneshot::RecvError) -> Self {
        match err {
            oneshot::RecvError::Closed => Self::Dropped,
            oneshot::RecvError::Receive(err) => Self::Receive(err),
            oneshot::RecvError::Connect(err) => Self::Connect(err),
            oneshot::RecvError::Listen(err) => Self::Listen(err),
            oneshot::RecvError::Remote(err) => Self::Remote(err.map(|err| Box::new(Self::from(*err)))),
        }
    }
}

impl Error for CallError {}

/// Generate argument call stubs.
macro_rules! arg_stub {
    ($name:ident, $fn_type:ident, $provider_type:ident, $new:ident, $provided:ident, ( $( $self_prefix:tt )* ), $( $arg:ident : $arg_type:ident ),*) => {
        impl < $( $arg_type , )* R, Codec> $name < ($($arg_type ,)*), R, Codec>
        where
            $( $arg_type : RemoteSend ,)*
            R: RemoteSend,
            Codec: codec::Codec,
        {
            /// Creates a remote function backed by `fun`.
            ///
            /// The provider is retained automatically until the remote function
            /// is no longer usable. Use the corresponding `provided_*` constructor
            /// when its lifetime must be controlled explicitly.
            #[allow(unused_mut)]
            pub fn $new <F, Fut>(mut fun: F) -> Self
            where
                F: $fn_type ($($arg_type),*) -> Fut + Send + Sync + 'static,
                Fut: Future<Output = R> + Send,
            {
                Self::new_int(move |( $($arg ,)* )| fun($($arg),*))
            }

            /// Creates a remote function backed by `fun` and returns its provider.
            ///
            /// See the [module-level documentation](super) for details.
            #[allow(unused_mut)]
            pub fn $provided <F, Fut>(mut fun: F) -> (Self, $provider_type)
            where
                F: $fn_type ($($arg_type),*) -> Fut + Send + Sync + 'static,
                Fut: Future<Output = R> + Send,
            {
                Self::provided_int(move |( $($arg ,)* )| fun($($arg),*))
            }

            /// Calls the remote function and reports transport failures separately.
            ///
            /// The returned [`CallError`] represents failure to deliver the call or
            /// receive its result. Any application-level error contained in `R` is
            /// returned unchanged.
            #[allow(clippy::too_many_arguments)]
            pub async fn try_call( $( $self_prefix )* self, $( $arg : $arg_type ),* ) -> Result<R, CallError> {
                self.try_call_int(( $($arg ,)* )).await
            }
        }

        impl < $($arg_type ,)* RT, RE, Codec> $name < ($($arg_type ,)* ), Result<RT, RE>, Codec>
        where
            $( $arg_type : RemoteSend ,)*
            RT: RemoteSend,
            RE: RemoteSend + From<CallError>,
            Codec: codec::Codec,
        {
            /// Calls the remote function.
            ///
            /// Transport failures are converted into the function's error type using
            /// [`From<CallError>`](From). This lets callers handle application and
            /// communication errors through one result.
            #[allow(clippy::too_many_arguments)]
            pub async fn call($( $self_prefix )* self, $( $arg : $arg_type ),*) -> Result<RT, RE> {
                self.call_int(( $($arg ,)* )).await
            }
        }
    };
}

/// Generates the accessors for the tracing settings of a remote function.
macro_rules! trace_accessors {
    ($wrapper:literal) => {
        /// Name of the remote function.
        ///
        /// The name is used for the tracing spans of its calls.
        /// Unless set, it is derived from the wrapper and argument types.
        pub fn name(&self) -> String {
            self.trace.name::<A>($wrapper)
        }

        /// Sets the name of the remote function.
        ///
        /// See the [module-level documentation](super) for details on tracing.
        pub fn set_name(&mut self, name: impl Into<String>) {
            self.trace.set_name(Some(name.into()));
        }

        /// The tracing performed for calls of the remote function at the client.
        ///
        /// This is [`Tracing::Both`](crate::tracing::Tracing::Both) by default.
        pub fn tracing(&self) -> $crate::tracing::Tracing {
            self.trace.tracing()
        }

        /// Sets the tracing performed for calls of the remote function at the client.
        pub fn set_tracing(&mut self, tracing: $crate::tracing::Tracing) {
            self.trace.set_tracing(tracing);
        }

        /// The level of the tracing spans of calls.
        ///
        /// This is [`LevelFilter::OFF`](::tracing::level_filters::LevelFilter::OFF)
        /// by default, i.e. no spans are created.
        pub fn tracing_level(&self) -> ::tracing::level_filters::LevelFilter {
            self.trace.level()
        }

        /// Sets the level of the tracing spans of calls.
        ///
        /// See the [module-level documentation](super) for details on tracing.
        pub fn set_tracing_level(&mut self, level: ::tracing::level_filters::LevelFilter) {
            self.trace.set_level(level);
        }

        /// Sets the span within which calls of the remote function are processed.
        ///
        /// By default calls are processed within the span of the task serving the
        /// remote function, which is detached from the span the function was created in,
        /// so that the latter can close while the function lives on.
        /// Setting a span, for example [`Span::current()`](::tracing::Span::current)
        /// at creation, makes the processing of calls nest within it instead.
        /// The span is kept open until the provider of the remote function is dropped or,
        /// if it is kept, until the remote function is dropped.
        /// A disabled span restores the default.
        ///
        /// This only takes effect if the wrapper has not been sent to a remote endpoint yet.
        /// See the [module-level documentation](super) for details on tracing.
        pub fn set_span(&mut self, span: ::tracing::Span) {
            self.trace.set_span(span);
        }
    };
}

mod msg;
mod rfn_const;
mod rfn_mut;
mod rfn_once;
mod tracing;

pub use rfn_const::{RFn, RFnProvider};
pub use rfn_mut::{RFnMut, RFnMutProvider};
pub use rfn_once::{RFnOnce, RFnOnceProvider};
