//! Performing a series of calls without awaiting each result.

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
/// [`Call`](super::Call).
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
