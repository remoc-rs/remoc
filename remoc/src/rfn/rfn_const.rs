use futures::{Future, future, pin_mut};
use std::{fmt, sync::Arc};
use tokio::sync::Semaphore;
use tracing::Instrument;

use super::{CallError, msg::RFnRequest};
use crate::{
    RemoteSend, codec,
    rch::{mpsc, oneshot},
};

/// Provides a remotely callable async [Fn] function.
///
/// Dropping the provider will stop making the function available for remote calls.
pub struct RFnProvider {
    keep_tx: Option<tokio::sync::oneshot::Sender<()>>,
    max_concurrency_tx: tokio::sync::watch::Sender<usize>,
}

impl fmt::Debug for RFnProvider {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RFnProvider").finish()
    }
}

impl RFnProvider {
    /// Default maximum number of concurrent [RFn] invocations.
    pub const DEFAULT_MAX_CONCURRENCY: usize = 32;

    /// Keeps the provider alive until it is not required anymore.
    ///
    /// This consumes the provider and keeps the function available until all
    /// corresponding [`RFn`] handles have been dropped.
    pub fn keep(mut self) {
        let _ = self.keep_tx.take().unwrap().send(());
    }

    /// Waits until the provider can be safely dropped.
    ///
    /// This is the case when the [RFn] is dropped.
    pub async fn done(&mut self) {
        self.keep_tx.as_mut().unwrap().closed().await
    }

    /// Returns the current maximum number of concurrent invocations.
    pub fn max_concurrency(&self) -> usize {
        *self.max_concurrency_tx.borrow()
    }

    /// Sets the maximum number of concurrent invocations of the remote function.
    ///
    /// The default is [RFnProvider::DEFAULT_MAX_CONCURRENCY].
    /// When the limit is reached, new invocations will wait until a running
    /// invocation completes.
    ///
    /// The limit can be changed at any time, including after the [RFn] has been
    /// sent to a remote endpoint.
    /// The new limit takes effect for subsequent invocations.
    /// Already running invocations are not affected.
    pub fn set_max_concurrency(&self, limit: usize) {
        self.max_concurrency_tx.send_if_modified(|current| {
            if *current != limit {
                *current = limit;
                true
            } else {
                false
            }
        });
    }
}

impl Drop for RFnProvider {
    fn drop(&mut self) {
        // empty
    }
}

/// Calls an async [Fn] function possibly located on a remote endpoint.
///
/// The remote function can be cloned and executed simultaneously from multiple callers.
/// For each invocation a new async task is spawned.
///
/// The function can take between zero and ten arguments.
///
/// # Example
///
/// In the following example the server sends a remote function that adds
/// two numbers to the client.
/// The client receives the remote function and calls it two times.
///
/// ```
/// use remoc::prelude::*;
///
/// type AddRFn = rfn::RFn<(u32, u32), Result<u32, rfn::CallError>>;
///
/// // This would be run on the client.
/// async fn client(mut rx: rch::base::Receiver<AddRFn>) {
///     let rfn = rx.recv().await.unwrap().unwrap();
///     assert_eq!(rfn.call(3, 5).await.unwrap(), 8);
///     assert_eq!(rfn.call(2, 10).await.unwrap(), 12);
/// }
///
/// // This would be run on the server.
/// async fn server(mut tx: rch::base::Sender<AddRFn>) {
///     let func = |x, y| async move { Ok(x + y) };
///     let rfn = rfn::RFn::new_2(func);
///     tx.send(rfn).await.unwrap();
/// }
/// # tokio_test::block_on(remoc::doctest::client_server(server, client));
/// ```
pub struct RFn<A, R, Codec = codec::Default> {
    request_tx: mpsc::Sender<RFnRequest<A, R, Codec>, Codec, 1>,
}

crate::versioned::compact::impl_struct! {
    RFn<A, R, Codec>,
    fields {
        request_tx: mpsc::Sender<RFnRequest<A, R, Codec>, Codec, 1> => "_0",
    }
    where A: RemoteSend, R: RemoteSend, Codec: codec::Codec
}

impl<A, R, Codec> Clone for RFn<A, R, Codec> {
    fn clone(&self) -> Self {
        Self { request_tx: self.request_tx.clone() }
    }
}

impl<A, R, Codec> fmt::Debug for RFn<A, R, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RFn").finish()
    }
}

impl<A, R, Codec> RFn<A, R, Codec>
where
    A: RemoteSend,
    R: RemoteSend,
    Codec: codec::Codec,
{
    /// Create a new remote function.
    fn new_int<F, Fut>(fun: F) -> Self
    where
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send,
    {
        let (rfn, provider) = Self::provided_int(fun);
        provider.keep();
        rfn
    }

    /// Create a new remote function and return it with its provider.
    ///
    /// See the [module-level documentation](super) for details.
    fn provided_int<F, Fut>(fun: F) -> (Self, RFnProvider)
    where
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send,
    {
        let (request_tx, request_rx) = mpsc::channel(1);
        let request_tx = request_tx.set_buffer();
        let mut request_rx = request_rx.set_buffer::<1>();
        let (keep_tx, keep_rx) = tokio::sync::oneshot::channel();
        let (max_concurrency_tx, mut max_concurrency_rx) =
            tokio::sync::watch::channel(RFnProvider::DEFAULT_MAX_CONCURRENCY);
        let fun = Arc::new(fun);

        wokio::spawn(
            async move {
                let mut semaphore = Arc::new(Semaphore::new(*max_concurrency_rx.borrow_and_update()));

                let term = async move {
                    if let Ok(()) = keep_rx.await {
                        future::pending().await
                    }
                };
                pin_mut!(term);

                loop {
                    tokio::select! {
                        biased;

                        () = &mut term => break,

                        Ok(()) = max_concurrency_rx.changed() => {
                            semaphore = Arc::new(Semaphore::new(*max_concurrency_rx.borrow_and_update()));
                        }

                        req_res = request_rx.recv() => {
                            match req_res {
                                Ok(Some(RFnRequest {argument, result_tx})) => {
                                    let fun_task = fun.clone();
                                    let semaphore = semaphore.clone();
                                    wokio::spawn(async move {
                                        let _permit = semaphore.acquire().await.ok();
                                        let result = fun_task(argument).await;
                                        let _ = result_tx.send(result);
                                    }.in_current_span());
                                }
                                Ok(None) => break,
                                Err(err) if err.is_disconnected() => break,
                                Err(_) => (),
                            }
                        }
                    }
                }
            }
            .in_current_span(),
        );

        (Self { request_tx }, RFnProvider { keep_tx: Some(keep_tx), max_concurrency_tx })
    }

    /// Try to call the remote function.
    async fn try_call_int(&self, argument: A) -> Result<R, CallError> {
        let (result_tx, result_rx) = oneshot::channel();
        let _ = self.request_tx.send(RFnRequest { argument, result_tx }).await;

        let result = result_rx.await?;
        Ok(result)
    }
}

impl<A, RT, RE, Codec> RFn<A, Result<RT, RE>, Codec>
where
    A: RemoteSend,
    RT: RemoteSend,
    RE: RemoteSend + From<CallError>,
    Codec: codec::Codec,
{
    /// Call the remote function.
    ///
    /// The [CallError] type must be convertible to the functions error type.
    async fn call_int(&self, argument: A) -> Result<RT, RE> {
        self.try_call_int(argument).await?
    }
}

// Calls for variable number of arguments.
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_0, provided_0, (&), );
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_1, provided_1, (&), arg1: A1);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_2, provided_2, (&), arg1: A1, arg2: A2);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_3, provided_3, (&), arg1: A1, arg2: A2, arg3: A3);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_4, provided_4, (&), arg1: A1, arg2: A2, arg3: A3, arg4: A4);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_5, provided_5, (&), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_6, provided_6, (&), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_7, provided_7, (&), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_8, provided_8, (&), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_9, provided_9, (&), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8, arg9: A9);
#[rustfmt::skip] arg_stub!(RFn, Fn, RFnProvider, new_10, provided_10, (&), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8, arg9: A9, arg10: A10);

impl<A, R, Codec> Drop for RFn<A, R, Codec> {
    fn drop(&mut self) {
        // empty
    }
}
