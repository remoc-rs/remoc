//! Utility functions.

use bytes::{Buf, Bytes};
use futures::{FutureExt, future::BoxFuture};
use std::{fmt, future::Future, sync::Arc};

use wokio::runtime;

/// Debug formatter for [Bytes].
pub fn dbg_bytes(bytes: &Bytes, f: &mut fmt::Formatter) -> fmt::Result {
    const LIMIT: usize = 16;

    if bytes.len() > LIMIT {
        let tmp = bytes.clone().copy_to_bytes(LIMIT);
        write!(f, "{tmp:?}...[{} bytes]", bytes.len())
    } else {
        write!(f, "{bytes:?}")
    }
}

/// Debug formatter for `Option<Bytes>`.
pub fn dbg_option_bytes(bytes: &Option<Bytes>, f: &mut fmt::Formatter) -> fmt::Result {
    match bytes {
        Some(bytes) => {
            write!(f, "Some(")?;
            dbg_bytes(bytes, f)?;
            write!(f, ")")?;
        }
        None => write!(f, "None")?,
    }
    Ok(())
}

/// Creates the span of a task that outlives the operation spawning it.
///
/// Level, name and fields are specified as for [`span!`](tracing::span).
macro_rules! task_span {
    ($level:expr, $name:literal $(, $($fields:tt)*)?) => {{
        let span = ::tracing::span!($level, $name $(, $($fields)*)?);
        span.follows_from(::tracing::Span::current());
        span
    }};
}
pub(crate) use task_span;

/// Spawns tasks onto the runtime that was current when the spawner was created.
#[derive(Clone)]
pub(crate) struct Spawner(Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>);

impl fmt::Debug for Spawner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Spawner").finish()
    }
}

impl Spawner {
    /// Creates a spawner for the current runtime.
    ///
    /// # Panics
    /// Panics if called outside of a runtime context.
    #[track_caller]
    pub fn current() -> Self {
        let handle = runtime::Handle::current();
        Self(Arc::new(move |task| {
            handle.spawn(task);
        }))
    }

    /// Spawns a task.
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        (self.0)(task.boxed())
    }
}

/// Implements `Send` and `Sync` explicitly for a type whose auto trait
/// implementations would otherwise be derived from its fields.
///
/// Only active with the `explicit-auto-traits` crate feature.
macro_rules! explicit_auto_traits {
    (
        $ty:ident $( [ $($ty_args:tt)* ] )? ;
        Send: [ $($send_generics:tt)* ] $( where [ $($send_where:tt)* ] )? ;
        $( Sync: [ $($sync_generics:tt)* ] $( where [ $($sync_where:tt)* ] )? ; )?
        fields: { $($field:ident),* $(,)? }
    ) => {
        $crate::util::explicit_auto_traits! {
            @send $ty $( [ $($ty_args)* ] )? ;
            Send: [ $($send_generics)* ] $( where [ $($send_where)* ] )? ;
            pattern: [ $ty { $($field),* } ] ;
            fields: $($field),*
        }
        $crate::util::explicit_auto_traits! {
            @sync $ty $( [ $($ty_args)* ] )? ;
            $( Sync: [ $($sync_generics)* ] $( where [ $($sync_where)* ] )? ; )?
            pattern: [ $ty { $($field),* } ] ;
            fields: $($field),*
        }
    };
    (
        $ty:ident $( [ $($ty_args:tt)* ] )? ;
        Send: [ $($send_generics:tt)* ] $( where [ $($send_where:tt)* ] )? ;
        $( Sync: [ $($sync_generics:tt)* ] $( where [ $($sync_where:tt)* ] )? ; )?
        fields: ( $($field:ident),* $(,)? )
    ) => {
        $crate::util::explicit_auto_traits! {
            @send $ty $( [ $($ty_args)* ] )? ;
            Send: [ $($send_generics)* ] $( where [ $($send_where)* ] )? ;
            pattern: [ $ty ( $($field),* ) ] ;
            fields: $($field),*
        }
        $crate::util::explicit_auto_traits! {
            @sync $ty $( [ $($ty_args)* ] )? ;
            $( Sync: [ $($sync_generics)* ] $( where [ $($sync_where)* ] )? ; )?
            pattern: [ $ty ( $($field),* ) ] ;
            fields: $($field),*
        }
    };
    (
        @send $ty:ident $( [ $($ty_args:tt)* ] )? ;
        Send: [ $($generics:tt)* ] $( where [ $($where:tt)* ] )? ;
        pattern: [ $pattern:pat ] ;
        fields: $($field:ident),*
    ) => {
        #[cfg(feature = "explicit-auto-traits")]
        #[allow(unsafe_code)]
        const _: () = {
            // SAFETY: every field is `Send` under these bounds, as checked by `fields_are_send`.
            unsafe impl<$($generics)*> Send for $ty $( < $($ty_args)* > )? $( where $($where)* )? {}

            #[allow(dead_code)]
            fn fields_are_send<$($generics)*>(value: &$ty $( < $($ty_args)* > )?) $( where $($where)* )? {
                fn check<F: Send>(_: &F) {}
                let $pattern = value;
                $( check($field); )*
            }
        };
    };
    (
        @sync $ty:ident $( [ $($ty_args:tt)* ] )? ;
        Sync: [ $($generics:tt)* ] $( where [ $($where:tt)* ] )? ;
        pattern: [ $pattern:pat ] ;
        fields: $($field:ident),*
    ) => {
        #[cfg(feature = "explicit-auto-traits")]
        #[allow(unsafe_code)]
        const _: () = {
            // SAFETY: every field is `Sync` under these bounds, as checked by `fields_are_sync`.
            unsafe impl<$($generics)*> Sync for $ty $( < $($ty_args)* > )? $( where $($where)* )? {}

            #[allow(dead_code)]
            fn fields_are_sync<$($generics)*>(value: &$ty $( < $($ty_args)* > )?) $( where $($where)* )? {
                fn check<F: Sync>(_: &F) {}
                let $pattern = value;
                $( check($field); )*
            }
        };
    };
    (
        @sync $ty:ident $( [ $($ty_args:tt)* ] )? ;
        pattern: [ $pattern:pat ] ;
        fields: $($field:ident),*
    ) => {};
}
pub(crate) use explicit_auto_traits;
