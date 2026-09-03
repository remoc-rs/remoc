use ::tracing::Instrument;
use futures::{Future, future, pin_mut};
use std::{
    fmt,
    sync::{Arc, Mutex},
};

use super::{CallError, msg::RFnRequest, tracing};
use crate::{
    RemoteSend, codec,
    rch::{mpsc, oneshot},
};

/// Provides a remotely callable async [FnMut] function.
///
/// Dropping the provider will stop making the function available for remote calls.
pub struct RFnMutProvider {
    keep_tx: Option<tokio::sync::oneshot::Sender<()>>,
    settings: Arc<Mutex<tracing::ProviderSettings>>,
}

impl fmt::Debug for RFnMutProvider {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RFnMutProvider").finish()
    }
}

impl RFnMutProvider {
    /// Keeps the provider alive until it is not required anymore.
    ///
    /// This consumes the provider and keeps the function available until all
    /// corresponding [`RFnMut`] handles have been dropped.
    pub fn keep(mut self) {
        let _ = self.keep_tx.take().unwrap().send(());
    }

    /// Waits until the provider can be safely dropped.
    ///
    /// This is the case when the [RFnMut] is dropped.
    pub async fn done(&mut self) {
        self.keep_tx.as_mut().unwrap().closed().await
    }
}

impl Drop for RFnMutProvider {
    fn drop(&mut self) {
        // Release the span calls are processed within, unless the provider
        // is kept, so that the span can close before the serving task ends.
        if self.keep_tx.is_some() {
            self.settings.lock().unwrap_or_else(|err| err.into_inner()).set_span(::tracing::Span::none());
        }
    }
}

/// Calls an async [FnMut] function possibly located on a remote endpoint.
///
/// The function can take between zero and ten arguments.
///
/// # Example
///
/// In the following example the server sends a remote function that sums
/// numbers using an internal accumulator.
/// The client receives the remote function and calls it three times.
///
/// ```
/// use remoc::prelude::*;
///
/// type SumRFnMut = rfn::RFnMut<(u32,), Result<u32, rfn::CallError>>;
///
/// // This would be run on the client.
/// async fn client(mut rx: rch::base::Receiver<SumRFnMut>) {
///     let mut rfn_mut = rx.recv().await.unwrap().unwrap();
///     assert_eq!(rfn_mut.call(3).await.unwrap(), 3);
///     assert_eq!(rfn_mut.call(2).await.unwrap(), 5);
///     assert_eq!(rfn_mut.call(11).await.unwrap(), 16);
/// }
///
/// // This would be run on the server.
/// async fn server(mut tx: rch::base::Sender<SumRFnMut>) {
///     let mut sum = 0;
///     let func = move |x| {
///         sum += x;
///         async move { Ok(sum) }
///     };
///     let rfn_mut = rfn::RFnMut::new_1(func);
///     tx.send(rfn_mut).await.unwrap();
/// }
/// # tokio_test::block_on(remoc::doctest::client_server(server, client));
/// ```
pub struct RFnMut<A, R, Codec = codec::Default> {
    request_tx: mpsc::Sender<RFnRequest<A, R, Codec>, Codec, 1>,
    trace: tracing::Settings,
}

crate::versioned::compact::impl_struct! {
    RFnMut<A, R, Codec>,
    fields {
        request_tx: mpsc::Sender<RFnRequest<A, R, Codec>, Codec, 1> => "_0",
        #[serde(default)]
        #[serde(skip_serializing_if = "tracing::Settings::is_default_ref")]
        trace: tracing::Settings => "_1",
    }
    where A: RemoteSend, R: RemoteSend, Codec: codec::Codec
}

impl<A, R, Codec> fmt::Debug for RFnMut<A, R, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RFnMut").field("name", &self.trace.name::<A>("RFnMut")).finish()
    }
}

impl<A, R, Codec> RFnMut<A, R, Codec>
where
    A: RemoteSend,
    R: RemoteSend,
    Codec: codec::Codec,
{
    /// Create a new remote function.
    fn new_int<F, Fut>(fun: F) -> Self
    where
        F: FnMut(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send,
    {
        let (rfn, provider) = Self::provided_int(fun);
        provider.keep();
        rfn
    }

    /// Create a new remote function and return it with its provider.
    ///
    /// See the [module-level documentation](super) for details.
    fn provided_int<F, Fut>(mut fun: F) -> (Self, RFnMutProvider)
    where
        F: FnMut(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send,
    {
        let (request_tx, request_rx) = mpsc::with_local_buffer(1);
        let request_tx = request_tx.set_buffer();
        let mut request_rx = request_rx.set_buffer::<1>();
        let (keep_tx, keep_rx) = tokio::sync::oneshot::channel();
        let provider_settings = tracing::ProviderSettings::new();
        let task_settings = provider_settings.clone();

        wokio::spawn(
            async move {
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

                        req_res = request_rx.recv() => {
                            match req_res {
                                Ok(Some(RFnRequest {argument, result_tx, tracing})) => {
                                    let span = task_settings.lock().unwrap().server_span::<A>("RFnMut", tracing.as_ref());
                                    let result = fun(argument).instrument(span).await;
                                    let _ = result_tx.send(result);
                                }
                                Ok(None) => break,
                                Err(err) if err.is_disconnected() => break,
                                Err(_) => (),
                            }
                        }
                    }
                }
            }
            .instrument(crate::util::task_span!(::tracing::Level::TRACE, "rfn_provider")),
        );

        let rfn = Self { request_tx, trace: tracing::Settings::new(&provider_settings) };
        (rfn, RFnMutProvider { keep_tx: Some(keep_tx), settings: provider_settings })
    }

    /// Try to call the remote function.
    async fn try_call_int(&mut self, argument: A) -> Result<R, CallError> {
        let (span, tracing) = self.trace.client_call::<A>("RFnMut");

        async move {
            let (result_tx, result_rx) = oneshot::channel();
            let _ = self.request_tx.send(RFnRequest { argument, result_tx, tracing }).await;

            let result = result_rx.await?;
            Ok(result)
        }
        .instrument(span)
        .await
    }

    trace_accessors!("RFnMut");
}

impl<A, RT, RE, Codec> RFnMut<A, Result<RT, RE>, Codec>
where
    A: RemoteSend,
    RT: RemoteSend,
    RE: RemoteSend + From<CallError>,
    Codec: codec::Codec,
{
    /// Call the remote function.
    ///
    /// The [CallError] type must be convertible to the functions error type.
    async fn call_int(&mut self, argument: A) -> Result<RT, RE> {
        self.try_call_int(argument).await?
    }
}

// Calls for variable number of arguments.
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_0, provided_0, (&mut), );
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_1, provided_1, (&mut), arg1: A1);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_2, provided_2, (&mut), arg1: A1, arg2: A2);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_3, provided_3, (&mut), arg1: A1, arg2: A2, arg3: A3);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_4, provided_4, (&mut), arg1: A1, arg2: A2, arg3: A3, arg4: A4);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_5, provided_5, (&mut), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_6, provided_6, (&mut), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_7, provided_7, (&mut), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_8, provided_8, (&mut), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_9, provided_9, (&mut), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8, arg9: A9);
#[rustfmt::skip] arg_stub!(RFnMut, FnMut, RFnMutProvider, new_10, provided_10, (&mut), arg1: A1, arg2: A2, arg3: A3, arg4: A4, arg5: A5, arg6: A6, arg7: A7, arg8: A8, arg9: A9, arg10: A10);

impl<A, R, Codec> Drop for RFnMut<A, R, Codec> {
    fn drop(&mut self) {
        // empty
    }
}
