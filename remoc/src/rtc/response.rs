//! Sending the response to a remote method call.

use futures::{FutureExt, ready};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::Instrument;

use crate::{
    codec,
    rch::{Sending, SendingError, oneshot},
};

#[doc(hidden)]
pub type ResponseErrorSender = tokio::sync::mpsc::Sender<super::ServeError>;

/// Create channel for queueing response sending errors.
#[doc(hidden)]
pub fn response_error_channel() -> (ResponseErrorSender, tokio::sync::mpsc::Receiver<super::ServeError>) {
    tokio::sync::mpsc::channel(16)
}

/// The type a method's return value is transferred as.
#[doc(hidden)]
pub type TransportedResponse<R> = <R as Response>::Compact;

/// A method return type, which is always a `Result<T, E>`.
#[doc(hidden)]
pub trait Response: Sized {
    /// The type the value is transferred as.
    type Compact: From<Self> + Into<Self>;

    /// The return type without the value, i.e. `Result<(), E>`.
    type WithoutValue: Response;

    /// Whether the method returned an error.
    fn is_error(&self) -> bool;

    /// Builds the return value from a failure of the call itself.
    fn from_call_error(err: super::CallError) -> Self;

    /// The successful return value without a value.
    fn without_value() -> Self::WithoutValue;
}

impl<T, E> Response for Result<T, E>
where
    E: From<super::CallError>,
{
    type Compact = crate::versioned::result::Result<T, E>;
    type WithoutValue = Result<(), E>;

    fn is_error(&self) -> bool {
        self.is_err()
    }

    fn from_call_error(err: super::CallError) -> Self {
        Err(err.into())
    }

    fn without_value() -> Result<(), E> {
        Ok(())
    }
}

/// The return type of a method that can hand over a [request receiver](super::ReqReceiver).
///
/// This is implemented for every `Result<T, E>` where `T` is the [client](super::Client)
/// of a remotable trait.
#[doc(hidden)]
pub trait PipelinableResponse: Response {
    /// The client returned by the method.
    type Client;

    /// The request receiver of the returned client.
    type ReqReceiver;

    /// Splits into the returned client, or the error converted into the return type
    /// of the method taking the request receiver.
    fn split(self) -> Result<Self::Client, Self::WithoutValue>;

    /// Converts into the return type of the method taking the request receiver,
    /// discarding the returned client.
    fn into_without_value(self) -> Self::WithoutValue {
        match self.split() {
            Ok(_client) => Self::without_value(),
            Err(without_value) => without_value,
        }
    }
}

impl<T, E> PipelinableResponse for Result<T, E>
where
    T: super::Client,
    E: From<super::CallError>,
{
    type Client = T;
    type ReqReceiver = T::ReqReceiver;

    fn split(self) -> Result<T, Result<(), E>> {
        match self {
            Ok(client) => Ok(client),
            Err(err) => Err(Err(err)),
        }
    }
}

/// Sends the response to a remote method call.
///
/// Send the return value of the method to response to the call.
/// Dropping this without sending makes the call fail at the caller.
#[derive(Serialize, Deserialize)]
#[serde(bound = "R: Response, TransportedResponse<R>: crate::RemoteSend, Codec: codec::Codec")]
pub struct ResponseSender<R, Codec = codec::Default>(oneshot::Sender<TransportedResponse<R>, Codec>)
where
    R: Response;

/// Creates a channel for responding to a remote method call.
#[doc(hidden)]
pub fn response_channel<R, Codec>(
    max_item_size: usize,
) -> (ResponseSender<R, Codec>, oneshot::Receiver<TransportedResponse<R>, Codec>)
where
    R: Response,
    TransportedResponse<R>: crate::RemoteSend,
    Codec: codec::Codec,
{
    let (mut tx, rx) = oneshot::channel();
    tx.set_max_item_size(max_item_size);
    (ResponseSender(tx), rx)
}

impl<R, Codec> std::fmt::Debug for ResponseSender<R, Codec>
where
    R: Response,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("ResponseSender").finish()
    }
}

impl<R, Codec> ResponseSender<R, Codec>
where
    R: Response,
    TransportedResponse<R>: crate::RemoteSend,
    Codec: codec::Codec,
{
    /// Sends the response over this channel.
    ///
    /// `Ok` means that the response was queued for sending; the returned handle
    /// reports whether it was sent.
    pub fn send(self, response: R) -> Result<Sending<TransportedResponse<R>>, oneshot::SendError<R>> {
        self.0.send(response.into()).map_err(|err| match err {
            oneshot::SendError::Closed(response) => oneshot::SendError::Closed(response.into()),
            oneshot::SendError::Dropped => oneshot::SendError::Dropped,
            oneshot::SendError::Failed => oneshot::SendError::Failed,
        })
    }

    /// Completes when the caller is no longer interested in the response, because
    /// the call was cancelled, the client was dropped or the connection failed.
    pub async fn closed(&self) {
        self.0.closed().await
    }

    /// Returns whether the caller is no longer interested in the response.
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// The maximum size of the response in bytes.
    ///
    /// A response exceeding this is not sent.
    pub fn max_item_size(&self) -> usize {
        self.0.max_item_size()
    }
}

/// Responder to a remote call.
///
/// This is the `__responder` field of a request generated by the
/// [`remote`](super::remote) macro.
///
/// Use [complete](Self::complete) to send the result.
///
/// Dropping this without sending makes the call fail at the caller.
pub struct Responder<R, Codec = codec::Default>
where
    R: Response,
{
    /// Channel the response is sent over.
    tx: ResponseSender<R, Codec>,
    /// Whether the server shall dispatch the call inline, i.e. process it before
    /// receiving further requests.
    sequential: bool,
    /// Whether the server shall stop serving when the call fails.
    stop_on_error: bool,
}

impl<R, Codec> std::fmt::Debug for Responder<R, Codec>
where
    R: Response,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Responder")
            .field("sequential", &self.sequential)
            .field("stop_on_error", &self.stop_on_error)
            .finish()
    }
}

impl<R, Codec> From<ResponseSender<R, Codec>> for Responder<R, Codec>
where
    R: Response,
{
    fn from(responder: ResponseSender<R, Codec>) -> Self {
        Self { tx: responder, sequential: false, stop_on_error: false }
    }
}

impl<R, Codec> From<Responder<R, Codec>> for ResponseSender<R, Codec>
where
    R: Response,
{
    fn from(responder: Responder<R, Codec>) -> Self {
        responder.tx
    }
}

impl<R, Codec> Responder<R, Codec>
where
    R: Response,
{
    /// Creates a new response target with the specified call options.
    #[doc(hidden)]
    pub fn new(tx: ResponseSender<R, Codec>, sequential: bool, stop_on_error: bool) -> Self {
        Self { tx, sequential, stop_on_error }
    }

    /// Whether the server shall dispatch the call inline, i.e. process it before
    /// receiving further requests.
    pub fn sequential(&self) -> bool {
        self.sequential
    }

    /// Whether the server shall stop serving when the call fails.
    pub fn stop_on_error(&self) -> bool {
        self.stop_on_error
    }
}

impl<R, Codec> Responder<R, Codec>
where
    R: Response,
    TransportedResponse<R>: crate::RemoteSend,
    Codec: codec::Codec,
{
    /// Completes the call with the result of the method.
    ///
    /// The response is queued for sending; the returned handle reports whether it was
    /// transmitted and may be discarded.
    pub async fn complete(self, result: R) -> Sending<TransportedResponse<R>> {
        self.send(result).unwrap_or_else(|_| Sending::dropped())
    }

    /// Sends the response over this channel.
    ///
    /// `Ok` means that the response was queued for sending; the returned handle
    /// reports whether it was sent.
    pub fn send(self, response: R) -> Result<Sending<TransportedResponse<R>>, oneshot::SendError<R>> {
        self.tx.send(response)
    }

    /// Completes when the caller is no longer interested in the response, because
    /// the call was cancelled, the client was dropped or the connection failed.
    pub async fn closed(&self) {
        self.tx.closed().await
    }

    /// Whether the caller is no longer interested in the response.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// The maximum allowed size of the response in bytes.
    pub fn max_item_size(&self) -> usize {
        self.tx.max_item_size()
    }

    /// Returns the underlying response sender.
    pub fn into_sender(self) -> ResponseSender<R, Codec> {
        self.tx
    }
}

/// Completes a call by responding to the request.
#[doc(hidden)]
pub async fn complete_call<R, Codec>(
    responder: Responder<R, Codec>, method: &'static str, err_tx: &ResponseErrorSender,
    mut dispatch_guard: Box<dyn super::monitor::DispatchGuard>, result: R,
) where
    R: Response,
    TransportedResponse<R>: crate::RemoteSend,
    Codec: codec::Codec,
{
    if result.is_error() {
        dispatch_guard.failed();
        if responder.stop_on_error() {
            let _ = err_tx.send(super::ServeError::CallFailed { method }).await;
        }
    }

    let Ok(sending) = responder.send(result) else { return };

    let err_tx = err_tx.clone();
    wokio::spawn(
        async move {
            if let Err(err) = sending.await {
                let kind = err.kind();
                match &kind {
                    crate::rch::SendingErrorKind::Send(crate::rch::base::SendErrorKind::Send(_)) => return,
                    crate::rch::SendingErrorKind::Dropped => return,
                    _ => (),
                }
                let _ = err_tx.send(kind.into()).await;
            }

            drop(dispatch_guard);
        }
        .in_current_span(),
    );
}

/// Responder to a pipelineable remote call.
///
/// This is the `__responder` field of a request generated by the
/// [`remote`](super::remote) macro for a trait method that
/// carries the `#[pipelinable]` attribute.
///
/// Use [complete](Self::complete) to send the result.
///
/// Dropping this without sending makes the call fail at the caller.
pub enum PipelinableResponder<R, Codec = codec::Default>
where
    R: PipelinableResponse,
{
    /// Send the result of the method back to the caller.
    Normal(Responder<R, Codec>),
    /// Let the client returned by the method execute the requests of `req_rx`,
    /// then send the client back to the caller.
    Pipeline {
        /// Requests that the client returned by the method should execute.
        req_rx: R::ReqReceiver,
        /// Channel for sending the result once all requests have been executed.
        ///
        /// The client is [`None`] when the object was consumed by a method taking
        /// `self` by value.
        responder: Responder<<R as Response>::WithoutValue, Codec>,
    },
}

impl<R, Codec> std::fmt::Debug for PipelinableResponder<R, Codec>
where
    R: PipelinableResponse,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Normal(_) => f.debug_tuple("Normal").finish(),
            Self::Pipeline { .. } => f.debug_struct("Pipeline").finish(),
        }
    }
}

impl<R, Codec> PipelinableResponder<R, Codec>
where
    R: PipelinableResponse,
{
    /// Whether the server shall dispatch the call inline, i.e. process it before
    /// receiving further requests.
    pub fn sequential(&self) -> bool {
        match self {
            Self::Normal(responder) => responder.sequential(),
            Self::Pipeline { responder, .. } => responder.sequential(),
        }
    }

    /// Whether the server shall stop serving when the call fails.
    pub fn stop_on_error(&self) -> bool {
        match self {
            Self::Normal(responder) => responder.stop_on_error(),
            Self::Pipeline { responder, .. } => responder.stop_on_error(),
        }
    }
}

impl<R, Codec> PipelinableResponder<R, Codec>
where
    R: PipelinableResponse,
    TransportedResponse<R>: crate::RemoteSend,
    TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
    Codec: codec::Codec,
{
    /// Completes the call with the result of the method.
    ///
    /// For [`Normal`](Self::Normal) the result is sent to the caller.
    ///
    /// For [`Pipeline`](Self::Pipeline) the returned client executes the requests of
    /// the handed over request receiver in the background; an error result is passed
    /// through unchanged. This returns as soon as serving has been started.
    pub async fn complete(self, result: R) -> Completing<R>
    where
        <R as PipelinableResponse>::ReqReceiver: super::ReqReceiver<Codec>
            + super::ServerBase<Client = <R as PipelinableResponse>::Client>
            + Send
            + 'static,
        <R as PipelinableResponse>::Client: Send + 'static,
    {
        match self {
            Self::Normal(responder) => {
                Completing::Normal(responder.send(result).unwrap_or_else(|_| Sending::dropped()))
            }
            Self::Pipeline { req_rx, responder } => {
                let pipelined = match result.split() {
                    Ok(client) => {
                        crate::rtc::spawn(tracing::Instrument::in_current_span(async move {
                            let mut req_rx = req_rx;
                            let _ = super::ReqReceiver::forward(&mut req_rx, client).await;
                        }));
                        R::without_value()
                    }
                    Err(pipelined) => pipelined,
                };

                Completing::Pipeline(responder.send(pipelined).unwrap_or_else(|_| Sending::dropped()))
            }
        }
    }

    /// Completes when the caller is no longer interested in the response, because
    /// the call was cancelled, the client was dropped or the connection failed.
    pub async fn closed(&self) {
        match self {
            Self::Normal(responder) => responder.closed().await,
            Self::Pipeline { responder, .. } => responder.closed().await,
        }
    }

    /// Whether the caller is no longer interested in the response.
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Normal(responder) => responder.is_closed(),
            Self::Pipeline { responder, .. } => responder.is_closed(),
        }
    }

    /// Whether a request receiver was handed over, i.e. this is
    /// [`Pipeline`](Self::Pipeline).
    pub fn is_pipelined(&self) -> bool {
        matches!(self, Self::Pipeline { .. })
    }
}

/// Handle to obtain the result of [completing](PipelinableResponder::complete) a call.
///
/// Await this handle to obtain the result of transmitting the response. This is optional
/// and only necessary if you want to explicitly handle errors that can occur while
/// sending.
///
/// You *should not* delay the receipt of further requests by awaiting this handle.
/// Dropping it *does not* abort transmitting the response.
pub enum Completing<R>
where
    R: PipelinableResponse,
{
    /// The response of an ordinary call is being transmitted.
    Normal(Sending<TransportedResponse<R>>),
    /// The response of a pipelined call is being transmitted.
    Pipeline(Sending<TransportedResponse<<R as Response>::WithoutValue>>),
}

impl<R> std::fmt::Debug for Completing<R>
where
    R: PipelinableResponse,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Normal(_) => f.debug_tuple("Normal").finish(),
            Self::Pipeline(_) => f.debug_tuple("Pipeline").finish(),
        }
    }
}

impl<R> Future for Completing<R>
where
    R: PipelinableResponse,
{
    type Output = Result<(), SendingError<<R as Response>::WithoutValue>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let res = match self.get_mut() {
            Self::Normal(sending) => ready!(sending.poll_unpin(cx))
                .map_err(|err| err.map_item(|response| Into::<R>::into(response).into_without_value())),
            Self::Pipeline(sending) => ready!(sending.poll_unpin(cx))
                .map_err(|err| err.map_item(Into::<<R as Response>::WithoutValue>::into)),
        };
        Poll::Ready(res)
    }
}

// ============================================================================
// Transported representations
// ============================================================================

// Serialization of [`Responder`].
impl<R, Codec> crate::versioned::Versioned for Responder<R, Codec>
where
    R: Response,
    TransportedResponse<R>: crate::RemoteSend,
    TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
    Codec: codec::Codec,
{
    type Versioner = crate::versioned::compact::CompactVersioner;

    type CurrentRef<'transport>
        = transported::ResponderRef<'transport, R, (), Codec>
    where
        Self: 'transport;

    fn as_current<'transport>(&'transport self) -> Result<Self::CurrentRef<'transport>, crate::versioned::Error> {
        Ok(transported::ResponderRef {
            tx: transported::SenderRef::Full(&self.tx),
            req_rx: None,
            sequential: self.sequential,
            stop_on_error: self.stop_on_error,
        })
    }

    type Current = transported::TransportedResponder<R, (), Codec>;

    fn from_current(current: Self::Current) -> Result<Self, crate::versioned::Error> {
        let transported::TransportedResponder { tx, req_rx, sequential, stop_on_error } = current;
        match (tx, req_rx) {
            (transported::Sender::Full(tx), None) => Ok(Self { tx, sequential, stop_on_error }),
            _ => Err(transported::unsupported_combination()),
        }
    }

    type OldRef<'transport>
        = &'transport ResponseSender<R, Codec>
    where
        Self: 'transport;

    fn as_old<'transport>(&'transport self) -> Result<Self::OldRef<'transport>, crate::versioned::Error> {
        if self.sequential || self.stop_on_error {
            return Err(transported::unsupported());
        }

        Ok(&self.tx)
    }

    type Old = ResponseSender<R, Codec>;

    fn from_old(old: Self::Old) -> Result<Self, crate::versioned::Error> {
        Ok(Self::from(old))
    }
}

crate::impl_serde! {
    Responder<R, Codec>
    where
        R: Response + 'static,
        TransportedResponse<R>: crate::RemoteSend,
        TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
        Codec: codec::Codec
}

// Serialization of [`PipelinableResponder`], using the same representation as [`Responder`].
impl<R, Codec> crate::versioned::Versioned for PipelinableResponder<R, Codec>
where
    R: PipelinableResponse,
    <R as PipelinableResponse>::ReqReceiver: crate::RemoteSend,
    TransportedResponse<R>: crate::RemoteSend,
    TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
    Codec: codec::Codec,
{
    type Versioner = crate::versioned::compact::CompactVersioner;

    type CurrentRef<'transport>
        = transported::ResponderRef<'transport, R, <R as PipelinableResponse>::ReqReceiver, Codec>
    where
        Self: 'transport;

    fn as_current<'transport>(&'transport self) -> Result<Self::CurrentRef<'transport>, crate::versioned::Error> {
        Ok(match self {
            Self::Normal(responder) => transported::ResponderRef {
                tx: transported::SenderRef::Full(&responder.tx),
                req_rx: None,
                sequential: responder.sequential,
                stop_on_error: responder.stop_on_error,
            },
            Self::Pipeline { req_rx, responder } => transported::ResponderRef {
                tx: transported::SenderRef::WithoutValue(&responder.tx),
                req_rx: Some(req_rx),
                sequential: responder.sequential,
                stop_on_error: responder.stop_on_error,
            },
        })
    }

    type Current = transported::TransportedResponder<R, <R as PipelinableResponse>::ReqReceiver, Codec>;

    fn from_current(current: Self::Current) -> Result<Self, crate::versioned::Error> {
        let transported::TransportedResponder { tx, req_rx, sequential, stop_on_error } = current;
        match (tx, req_rx) {
            (transported::Sender::Full(tx), None) => {
                Ok(Self::Normal(Responder { tx, sequential, stop_on_error }))
            }
            (transported::Sender::WithoutValue(tx), Some(req_rx)) => {
                Ok(Self::Pipeline { req_rx, responder: Responder { tx, sequential, stop_on_error } })
            }
            _ => Err(transported::unsupported_combination()),
        }
    }

    type OldRef<'transport>
        = &'transport ResponseSender<R, Codec>
    where
        Self: 'transport;

    fn as_old<'transport>(&'transport self) -> Result<Self::OldRef<'transport>, crate::versioned::Error> {
        match self {
            Self::Normal(responder) => crate::versioned::Versioned::as_old(responder),
            Self::Pipeline { .. } => Err(transported::unsupported()),
        }
    }

    type Old = ResponseSender<R, Codec>;

    fn from_old(old: Self::Old) -> Result<Self, crate::versioned::Error> {
        Ok(Self::Normal(Responder::from(old)))
    }
}

crate::impl_serde! {
    PipelinableResponder<R, Codec>
    where
        R: PipelinableResponse + 'static,
        <R as PipelinableResponse>::ReqReceiver: crate::RemoteSend,
        TransportedResponse<R>: crate::RemoteSend,
        TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
        Codec: codec::Codec
}

// The common transported representation of [`Responder`] and [`PipelinableResponder`].
mod transported {
    use super::*;

    /// Response channel of a transported response, for serialization.
    #[derive(Serialize)]
    #[serde(bound = "")]
    pub enum SenderRef<'transport, R, Codec>
    where
        R: Response,
        TransportedResponse<R>: crate::RemoteSend,
        TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
        Codec: codec::Codec,
    {
        #[serde(rename = "_0")]
        Full(&'transport ResponseSender<R, Codec>),
        #[serde(rename = "_1")]
        WithoutValue(&'transport ResponseSender<<R as Response>::WithoutValue, Codec>),
    }

    /// Response channel of a transported response, for deserialization.
    #[derive(Deserialize)]
    #[serde(bound = "")]
    pub enum Sender<R, Codec>
    where
        R: Response,
        TransportedResponse<R>: crate::RemoteSend,
        TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
        Codec: codec::Codec,
    {
        #[serde(rename = "_0")]
        Full(ResponseSender<R, Codec>),
        #[serde(rename = "_1")]
        WithoutValue(ResponseSender<<R as Response>::WithoutValue, Codec>),
    }

    /// Transported response, for serialization.
    #[derive(Serialize)]
    #[serde(bound = "")]
    pub struct ResponderRef<'transport, R, Rx, Codec>
    where
        R: Response,
        Rx: crate::RemoteSend,
        TransportedResponse<R>: crate::RemoteSend,
        TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
        Codec: codec::Codec,
    {
        #[serde(rename = "_0")]
        pub tx: SenderRef<'transport, R, Codec>,
        #[serde(rename = "_1")]
        #[serde(skip_serializing_if = "crate::codec::skip::Option::is_none")]
        pub req_rx: Option<&'transport Rx>,
        #[serde(rename = "_2")]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default")]
        pub sequential: bool,
        #[serde(rename = "_3")]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default")]
        pub stop_on_error: bool,
    }

    /// Transported response, for deserialization.
    #[derive(Deserialize)]
    #[serde(bound = "")]
    pub struct TransportedResponder<R, Rx, Codec>
    where
        R: Response,
        Rx: crate::RemoteSend,
        TransportedResponse<R>: crate::RemoteSend,
        TransportedResponse<<R as Response>::WithoutValue>: crate::RemoteSend,
        Codec: codec::Codec,
    {
        #[serde(rename = "_0")]
        pub tx: Sender<R, Codec>,
        #[serde(rename = "_1")]
        #[serde(default = "none")]
        pub req_rx: Option<Rx>,
        #[serde(rename = "_2")]
        #[serde(default)]
        pub sequential: bool,
        #[serde(rename = "_3")]
        #[serde(default)]
        pub stop_on_error: bool,
    }

    pub fn none<T>() -> Option<T> {
        None
    }

    pub fn unsupported() -> crate::versioned::Error {
        "the remote endpoint does not support the options of this method call".into()
    }

    pub fn unsupported_combination() -> crate::versioned::Error {
        "unsupported combination of response channel and request receiver".into()
    }
}
