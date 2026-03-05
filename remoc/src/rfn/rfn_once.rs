use futures::Future;
use serde::{Deserialize, Serialize};
use std::fmt;
use tracing::Instrument;

use super::{CallError, msg::RFnRequest};
use crate::{RemoteSend, codec, exec, rch::oneshot};

/// Provides a remotely callable async [FnOnce] function.
///
/// Dropping the provider will stop making the function available for remote calls.
pub struct RFnOnceProvider {
    keep_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl fmt::Debug for RFnOnceProvider {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RFnOnceProvider").finish()
    }
}

impl RFnOnceProvider {
    /// Keeps the provider alive until it is not required anymore.
    pub fn keep(mut self) {
        let _ = self.keep_tx.take().unwrap().send(());
    }

    /// Waits until the provider can be safely dropped.
    ///
    /// This is the case when the [RFnOnce] is evaluated or dropped.
    pub async fn done(&mut self) {
        self.keep_tx.as_mut().unwrap().closed().await
    }
}

impl Drop for RFnOnceProvider {
    fn drop(&mut self) {
        // empty
    }
}

/// Calls an async [FnOnce] function possibly located on a remote endpoint.
///
/// The function can take between zero and ten arguments.
///
/// # Example
///
/// In the following example the server sends a remote function that returns
/// a string that is moved into the closure.
/// The client receives the remote function and calls it.
///
/// ```
/// use remoc::prelude::*;
///
/// type GetRFnOnce = rfn::RFnOnce<(), Result<String, rfn::CallError>>;
///
/// // This would be run on the client.
/// async fn client(mut rx: rch::base::Receiver<GetRFnOnce>) {
///     let mut rfn_once = rx.recv().await.unwrap().unwrap();
///     assert_eq!(rfn_once.call().await.unwrap(), "Hallo".to_string());
/// }
///
/// // This would be run on the server.
/// async fn server(mut tx: rch::base::Sender<GetRFnOnce>) {
///     let msg = "Hallo".to_string();
///     let func = || async move { Ok(msg) };
///     let rfn_once = rfn::RFnOnce::new_0(func);
///     tx.send(rfn_once).await.unwrap();
/// }
/// # tokio_test::block_on(remoc::doctest::client_server(server, client));
/// ```
#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "A: RemoteSend, R: RemoteSend, Codec: codec::Codec"))]
#[serde(bound(deserialize = "A: RemoteSend, R: RemoteSend, Codec: codec::Codec"))]
pub struct RFnOnce<A, R, Codec = codec::Default> {
    request_tx: oneshot::Sender<RFnRequest<A, R, Codec>, Codec>,
}

impl<A, R, Codec> fmt::Debug for RFnOnce<A, R, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RFnOnce").finish()
    }
}

impl<A, R, Codec> RFnOnce<A, R, Codec>
where
    A: RemoteSend,
    R: RemoteSend,
    Codec: codec::Codec,
{
    /// Create a new remote function.
    fn new_int<F, Fut>(fun: F) -> Self
    where
        F: FnOnce(A) -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send,
    {
        let (rfn, provider) = Self::provided_int(fun);
        provider.keep();
        rfn
    }

    /// Create a new remote function and return it with its provider.
    ///
    /// See the [module-level documentation](super) for details.
    fn provided_int<F, Fut>(fun: F) -> (Self, RFnOnceProvider)
    where
        F: FnOnce(A) -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send,
    {
        let (request_tx, request_rx) = oneshot::channel();
        let (keep_tx, keep_rx) = tokio::sync::oneshot::channel();

        exec::spawn(
            async move {
                tokio::select! {
                    biased;

                    Err(_) = keep_rx => (),

                    req_res = request_rx => {
                        if let Ok(RFnRequest {argument, result_tx}) = req_res {
                            let result = fun(argument).await;
                            let _ = result_tx.send(result);
                        }
                    }
                }
            }
            .in_current_span(),
        );

        (Self { request_tx }, RFnOnceProvider { keep_tx: Some(keep_tx) })
    }

    /// Try to call the remote function.
    async fn try_call_int(self, argument: A) -> Result<R, CallError> {
        let (result_tx, result_rx) = oneshot::channel();
        let _ = self.request_tx.send(RFnRequest { argument, result_tx });

        let result = result_rx.await?;
        Ok(result)
    }
}

impl<A, RT, RE, Codec> RFnOnce<A, Result<RT, RE>, Codec>
where
    A: RemoteSend,
    RT: RemoteSend,
    RE: RemoteSend + From<CallError>,
    Codec: codec::Codec,
{
    /// Call the remote function.
    ///
    /// The [CallError] type must be convertible to the functions error type.
    async fn call_int(self, argument: A) -> Result<RT, RE> {
        self.try_call_int(argument).await?
    }
}

// Calls for variable number of arguments.
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_0, provided_0, (),);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_1, provided_1, (), arg1: A1);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_2, provided_2, (), arg1: A1, arg2: A2);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_3, provided_3, (), arg1: A1, arg2: A2, arg3: A3);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_4, provided_4, (), arg1: A1, arg2: A2, arg3: A3, arg4: A4);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_5, provided_5, (), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_6, provided_6, (), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_7, provided_7, (), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_8, provided_8, (), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_9, provided_9, (), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8, arg9: A9);
#[rustfmt::skip] arg_stub!(RFnOnce, FnOnce, RFnOnceProvider, new_10, provided_10, (), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8, arg9: A9, arg10: A10);
