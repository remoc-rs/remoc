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
//! Each wrapper spawns an async tasks that processes function execution requests
//! from the remote endpoint.
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
//! Note that the function is executed locally.
//! Only the arguments and return value are transmitted from and to the remote endpoint.
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
    /// Provider was dropped or function panicked.
    Dropped,
    /// Receiving from a remote endpoint failed.
    RemoteReceive(base::RecvError),
    /// Connecting a sent channel failed.
    RemoteConnect(chmux::ConnectError),
    /// Listening for a connection from a received channel failed.
    RemoteListen(chmux::ListenerError),
}

crate::versioned::impl_enum! {
    CallError,
    versioner = crate::versioned::RemocCompact,
    variants {
        Dropped => "_0",
        RemoteReceive(err: base::RecvError) => "_1",
        RemoteConnect(err: chmux::ConnectError) => "_2",
        RemoteListen(err: chmux::ListenerError) => "_3",
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Dropped => write!(f, "provider dropped or function panicked"),
            Self::RemoteReceive(err) => write!(f, "receive error: {err}"),
            Self::RemoteConnect(err) => write!(f, "connect error: {err}"),
            Self::RemoteListen(err) => write!(f, "listen error: {err}"),
        }
    }
}

impl From<oneshot::RecvError> for CallError {
    fn from(err: oneshot::RecvError) -> Self {
        match err {
            oneshot::RecvError::Closed => Self::Dropped,
            oneshot::RecvError::RemoteReceive(err) => Self::RemoteReceive(err),
            oneshot::RecvError::RemoteConnect(err) => Self::RemoteConnect(err),
            oneshot::RecvError::RemoteListen(err) => Self::RemoteListen(err),
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
            /// Create a new remote function.
            #[allow(unused_mut)]
            pub fn $new <F, Fut>(mut fun: F) -> Self
            where
                F: $fn_type ($($arg_type),*) -> Fut + Send + Sync + 'static,
                Fut: Future<Output = R> + Send,
            {
                Self::new_int(move |( $($arg ,)* )| fun($($arg),*))
            }

            /// Create a new remote function and return it with its provider.
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

            /// Try to call the remote function.
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
            /// Call the remote function.
            ///
            /// The [CallError] type must be convertible to the functions error type.
            #[allow(clippy::too_many_arguments)]
            pub async fn call($( $self_prefix )* self, $( $arg : $arg_type ),*) -> Result<RT, RE> {
                self.call_int(( $($arg ,)* )).await
            }
        }
    };
}

mod msg;
mod rfn_const;
mod rfn_mut;
mod rfn_once;

pub use rfn_const::{RFn, RFnProvider};
pub use rfn_mut::{RFnMut, RFnMutProvider};
pub use rfn_once::{RFnOnce, RFnOnceProvider};
