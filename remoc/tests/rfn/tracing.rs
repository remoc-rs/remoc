//! Tracing spans of remote function calls.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::{
    any::type_name,
    sync::{Arc, Mutex},
};

use remoc::{
    rfn::{CallError, RFn, RFnMut, RFnOnce},
    tracing::Tracing,
};
use tracing::{Instrument, Level, info_span, level_filters::LevelFilter};
use tracing_subscriber::{Layer, layer::SubscriberExt};

use crate::loop_channel;

/// A recorded call span.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CallSpan {
    level: Option<Level>,
    name: String,
    kind: String,
    span_id: Option<String>,
    /// Whether the span id was already set when the span was created.
    span_id_at_creation: bool,
}

impl tracing::field::Visit for CallSpan {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "otel.name" => self.name = value.to_string(),
            "otel.kind" => self.kind = value.to_string(),
            _ => (),
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "span_id" {
            self.span_id = Some(format!("{value:?}"));
        }
    }
}

/// Records every call span, including fields recorded after its creation.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<(tracing::span::Id, CallSpan)>>>);

impl Recorder {
    /// Removes and returns the recorded spans.
    fn take(&self) -> Vec<CallSpan> {
        std::mem::take(&mut *self.0.lock().unwrap()).into_iter().map(|(_, span)| span).collect()
    }
}

impl<S> Layer<S> for Recorder
where
    S: tracing::Subscriber,
{
    fn on_new_span(
        &self, attrs: &tracing::span::Attributes, id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<S>,
    ) {
        if attrs.metadata().target() != "remoc::rfn::call" {
            return;
        }

        let mut span = CallSpan { level: Some(*attrs.metadata().level()), ..Default::default() };
        attrs.record(&mut span);
        span.span_id_at_creation = span.span_id.is_some();
        self.0.lock().unwrap().push((id.clone(), span));
    }

    fn on_record(
        &self, id: &tracing::span::Id, values: &tracing::span::Record,
        _ctx: tracing_subscriber::layer::Context<S>,
    ) {
        let mut spans = self.0.lock().unwrap();
        if let Some((_, span)) = spans.iter_mut().find(|(span_id, _)| span_id == id) {
            values.record(span);
        }
    }
}

/// Performs a call and returns the recorded client and server spans.
async fn traced_call(recorder: &Recorder, call: impl Future<Output = i16>) -> (CallSpan, CallSpan) {
    recorder.take();
    assert_eq!(call.await, -17);
    let spans = recorder.take();
    assert_eq!(spans.len(), 2, "unexpected call spans: {spans:?}");
    assert_eq!(spans[0].kind, "client");
    assert_eq!(spans[1].kind, "server");
    (spans[0].clone(), spans[1].clone())
}

/// By default no spans are created.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn no_spans_by_default() {
    crate::init();

    let recorder = Recorder::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<RFn<_, _>>().await;

    let rfn = RFn::new_1(|arg: i16| async move { Ok::<_, CallError>(-arg) });
    assert_eq!(rfn.tracing_level(), LevelFilter::OFF);
    assert_eq!(rfn.tracing(), Tracing::Both);
    a_tx.send(rfn).await.unwrap();
    let rfn = b_rx.recv().await.unwrap().unwrap();

    assert_eq!(rfn.call(17).await.unwrap(), -17);
    assert_eq!(recorder.take(), vec![], "no call span expected");
}

/// The spans of the caller and the provider are named after the function and
/// correlated by the span id. The provider follows the settings of the remote
/// function only in the process it was created in.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn spans_follow_settings() {
    crate::init();

    let recorder = Recorder::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<RFn<_, _>>().await;

    let mut rfn = RFn::new_1(|arg: i16| async move { Ok::<_, CallError>(-arg) });
    rfn.set_tracing_level(LevelFilter::INFO);
    let default_name = format!("RFn<{}>", type_name::<(i16,)>());
    assert_eq!(rfn.name(), default_name);

    // The clone stays linked to the provider.
    let mut local = rfn.clone();
    a_tx.send(rfn).await.unwrap();
    let mut rfn = b_rx.recv().await.unwrap().unwrap();
    assert_eq!(rfn.tracing_level(), LevelFilter::INFO, "tracing level was not transferred");
    assert_eq!(rfn.name(), default_name);

    async {
        // Both spans are named after the function and share the span id.
        let (client, server) = traced_call(&recorder, async { rfn.call(17).await.unwrap() }).await;
        assert_eq!(client.name, default_name);
        assert_eq!(server.name, default_name);
        assert_eq!(client.level, Some(Level::INFO));
        assert_eq!(server.level, Some(Level::INFO));
        let span_id = client.span_id.clone().expect("span id not recorded at client");
        assert_eq!(span_id.len(), 16, "span id is not 64 bit hexadecimal: {span_id}");
        assert_eq!(server.span_id.as_ref(), Some(&span_id), "span id not recorded at server");
        assert!(client.span_id_at_creation, "span id not set when creating client span");
        assert!(server.span_id_at_creation, "span id not set when creating server span");

        // Changes in the process of creation reach the provider, but not the
        // remote function that was sent away.
        local.set_name("negate");
        local.set_tracing_level(LevelFilter::DEBUG);
        let (client, server) = traced_call(&recorder, async { rfn.call(17).await.unwrap() }).await;
        assert_eq!(client.name, default_name);
        assert_eq!(client.level, Some(Level::INFO));
        assert_eq!(server.name, "negate");
        assert_eq!(server.level, Some(Level::DEBUG));

        // Changes to the received remote function do not reach the provider.
        rfn.set_name("evil");
        let (client, server) = traced_call(&recorder, async { rfn.call(17).await.unwrap() }).await;
        assert_eq!(client.name, "evil");
        assert_eq!(server.name, "negate");

        // Without propagation the server span is not correlated.
        rfn.set_tracing(Tracing::Client);
        let (client, server) = traced_call(&recorder, async { rfn.call(17).await.unwrap() }).await;
        assert!(client.span_id.is_some());
        assert_eq!(server.span_id, None, "unexpected span id without propagation");

        // Disabling tracing at the caller leaves the span of the provider.
        rfn.set_tracing(Tracing::Both);
        rfn.set_tracing_level(LevelFilter::OFF);
        recorder.take();
        assert_eq!(rfn.call(17).await.unwrap(), -17);
        let spans = recorder.take();
        assert_eq!(spans.len(), 1, "unexpected call spans: {spans:?}");
        assert_eq!(spans[0].kind, "server");
        assert_eq!(spans[0].span_id, None, "unexpected span id without client span");
    }
    .instrument(info_span!("test"))
    .await;
}

/// Mutable and by-value remote functions create spans as well.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn rfn_mut_and_once_spans() {
    crate::init();

    let recorder = Recorder::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<(RFnMut<_, _>, RFnOnce<_, _>)>().await;

    let mut rfn_mut = RFnMut::new_1(|arg: i16| async move { Ok::<_, CallError>(-arg) });
    rfn_mut.set_tracing_level(LevelFilter::WARN);
    let mut rfn_once = RFnOnce::new_1(|arg: i16| async move { Ok::<_, CallError>(-arg) });
    rfn_once.set_tracing_level(LevelFilter::INFO);
    rfn_once.set_name("once");

    a_tx.send((rfn_mut, rfn_once)).await.unwrap();
    let (mut rfn_mut, rfn_once) = b_rx.recv().await.unwrap().unwrap();

    async {
        let (client, server) = traced_call(&recorder, async { rfn_mut.call(17).await.unwrap() }).await;
        let name = format!("RFnMut<{}>", type_name::<(i16,)>());
        assert_eq!(client.name, name);
        assert_eq!(server.name, name);
        assert_eq!(client.level, Some(Level::WARN));
        assert_eq!(server.level, Some(Level::WARN));
        assert!(client.span_id.is_some());
        assert_eq!(server.span_id, client.span_id);

        let (client, server) = traced_call(&recorder, async { rfn_once.call(17).await.unwrap() }).await;
        assert_eq!(client.name, "once");
        assert_eq!(server.name, "once");
        assert_eq!(client.level, Some(Level::INFO));
        assert!(client.span_id.is_some());
        assert_eq!(server.span_id, client.span_id);
    }
    .instrument(info_span!("test"))
    .await;
}
