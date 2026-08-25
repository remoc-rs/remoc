//! Calling a method on a remotable trait.

use futures::{FutureExt, future::BoxFuture};
use std::{
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::Instrument;

use super::ServeError;
use crate::{
    chmux,
    rch::{SendingErrorKind, base, mpsc, oneshot},
};

#[doc(inline)]
pub use crate::calls;

/// Performs a series of remote calls without awaiting the result of each one.
///
/// Every call is started before the result of any of them is awaited, so that the
/// requests are transferred without a round trip per call. The requests are
/// transferred in the order the calls appear.
///
/// This is a convenience for a straight line of calls; performing the calls by hand
/// works just as well and is required for anything more involved, see below.
///
/// The calls are separated by semicolons and are written using the
/// `<name>_call` variant of the trait methods, which starts a call and returns a
/// [`Call`].
///
/// ```ignore
/// let value = calls!(
///     counter.increase_call(2);
///     counter.multiply_call(10);
///     counter.value_call()
/// );
/// ```
///
/// As in a block, the value of the last call is returned, unless it is followed by
/// a semicolon, in which case `()` is returned. The values of all other calls are
/// discarded.
///
/// A call that fails returns from the enclosing function, like the
/// [`?` operator](https://doc.rust-lang.org/std/ops/trait.Try.html) does. Thus this
/// must be used within a function returning [`Result`], and the error of each call
/// is converted into its error type using [`From`].
///
/// # Pipelined session
///
/// A call that hands over a [request receiver](super::ReqReceiver), i.e. the
/// `<name>_pipelined` variant of a trait method, can be specified like any other
/// call:
///
/// ```ignore
/// let value = calls!(
///     dir.open_counter_pipelined("counter".to_string(), counter_rx);
///     counter.increase_call(2);
///     counter.value_call()
/// );
/// ```
///
/// Specify it before the calls on the client it establishes, both so that the
/// requests are performed in the right order and so that a failure to establish the
/// session is reported instead of those calls failing with a
/// [`CallError`](super::CallError) caused by it.
///
/// Any number of them can be specified, for example to hand a request receiver over
/// through a client that has itself only just been established this way.
///
/// # Errors
///
/// The calls may have differing error types, as long as all of them convert into the
/// error type of the enclosing function. Use
/// [`CallFutureExt::map_err`](super::CallFutureExt::map_err) when a call has an error
/// type that does not.
///
/// # Outside a function returning [`Result`]
///
/// Obtain the result as a value by wrapping this in an async block that states the
/// error type:
///
/// ```ignore
/// let value = async { Ok::<_, MyError>(calls!(counter.value_call())) }.await;
/// ```
///
/// Note that `async { calls!(…) }` does not work, since the block would have to
/// return a [`Result`] for the calls to be propagated.
///
/// # Complex cases
///
/// This handles a straight line of calls. Perform the phases by hand when you need
/// to interleave other work, use the value of an intermediate call, or handle the
/// errors of individual calls:
///
/// ```ignore
/// let session = dir.open_counter_pipelined(name, counter_rx).await;
/// let a = counter.increase_call(2).await;
/// let b = counter.value_call().await;
///
/// session.await?;
/// a.await?;
/// let value = b.await?;
/// ```
///
/// Use [`Call::map_err`](super::Call::map_err) to bring calls with differing error
/// types to a common one there, for example to await them together using
/// [`try_join`](tokio::try_join).
#[doc(hidden)]
#[macro_export]
macro_rules! calls {
    // Starts each call, in order, collecting the started calls into a nested tuple.
    (@start) => { () };
    (@start $head:expr $(; $tail:expr)*) => {{
        let __head = $head.await;
        ($crate::calls!(@start $($tail);*), __head)
    }};

    // Awaits the results in the same order, returning the value of the last call.
    (@value $started:expr ; $last:expr) => {{
        let ((), __head) = $started;
        __head.await?
    }};
    (@value $started:expr ; $head:expr $(; $tail:expr)+) => {{
        let (__rest, __head) = $started;
        __head.await?;
        $crate::calls!(@value __rest ; $($tail);+)
    }};

    // Awaits the results in the same order, discarding the value of the last call.
    (@unit $started:expr ; $last:expr) => {{
        let ((), __head) = $started;
        __head.await?;
    }};
    (@unit $started:expr ; $head:expr $(; $tail:expr)+) => {{
        let (__rest, __head) = $started;
        __head.await?;
        $crate::calls!(@unit __rest ; $($tail);+)
    }};

    // Discarding the value of the last call.
    ($($call:expr);+ ;) => {{
        let __started = $crate::calls!(@start $($call);+);
        $crate::calls!(@unit __started ; $($call);+)
    }};

    // Returning the value of the last call.
    ($($call:expr);+) => {{
        let __started = $crate::calls!(@start $($call);+);
        $crate::calls!(@value __started ; $($call);+)
    }};
}

/// Call a method on a remotable trait failed.
#[derive(Debug, Clone)]
pub enum CallError {
    /// The object is not being served.
    ///
    /// The request was never accepted, because serving of the object had already
    /// finished: [`serve`](super::ServerShared::serve) returned, or the server was dropped
    /// without ever being served.
    NotServed,
    /// Processing the request failed.
    ///
    /// The request was accepted but no response arrived. The server may have panicked
    /// while handling it, sending the response may have failed on the server side, or
    /// the request may have been dropped by a client or server monitor.
    Dropped,
    /// Encoding or transferring the request failed; see [`base::SendErrorKind`].
    Send(base::SendErrorKind),
    /// Receiving or decoding the response failed; see [`base::RecvError`].
    Receive(base::RecvError),
    /// Opening a channel carried by the request or response failed; see [`chmux::ConnectError`].
    Connect(chmux::ConnectError),
    /// Preparing a channel carried by the request or response failed; see [`chmux::ListenerError`].
    Listen(chmux::ListenerError),
    /// An endpoint forwarding the call or response could not complete the transfer.
    Forward,
    /// A failure was reported by an endpoint forwarding the call or response.
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

/// A remote method call that has been started.
///
/// This is returned by the `<name>_call` twin of every method of a remotable trait,
/// which starts the call without waiting for its result. Await this to obtain the
/// result.
///
/// Starting several calls before awaiting their results avoids one round trip per
/// call, since the requests are transferred to the server without waiting for the
/// response of the preceding one. The requests are transferred in the order the calls
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

impl From<ServeError> for CallError {
    fn from(err: ServeError) -> Self {
        match err {
            ServeError::ReqReceive(err) => match err {
                mpsc::RecvError::Receive(err) => Self::Receive(err),
                mpsc::RecvError::Connect(err) => Self::Connect(err),
                mpsc::RecvError::Listen(err) => Self::Listen(err),
                mpsc::RecvError::Remote(_) => Self::Forward,
            },
            ServeError::ResponseSend(SendingErrorKind::Send(err)) => Self::Send(err),
            ServeError::ResponseSend(SendingErrorKind::Dropped) => Self::Dropped,
            ServeError::Forward(err) => Self::from(err),
            ServeError::Monitor(_) => Self::Forward,
            ServeError::CallFailed { .. } => Self::Forward,
        }
    }
}
