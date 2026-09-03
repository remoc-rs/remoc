//! The `tracing(level = "...")` argument and attribute of the `remote` attribute.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::sync::{Arc, Mutex};

use remoc::{prelude::*, rtc::CallError, tracing::Tracing};
use tracing::Level;
use tracing_subscriber::{Layer, layer::SubscriberExt};

use crate::loop_channel;

#[rtc::remote(tracing(level = "debug"))]
pub trait Leveled {
    async fn standard(&self) -> Result<u32, CallError>;

    #[tracing(level = "warn")]
    async fn important(&self) -> Result<u32, CallError>;

    #[tracing]
    async fn plain(&self) -> Result<u32, CallError>;

    #[tracing(level = "off")]
    async fn silent(&self) -> Result<u32, CallError>;
}

pub struct LeveledObj;

impl Leveled for LeveledObj {
    async fn standard(&self) -> Result<u32, CallError> {
        Ok(1)
    }

    async fn important(&self) -> Result<u32, CallError> {
        Ok(2)
    }

    async fn plain(&self) -> Result<u32, CallError> {
        Ok(3)
    }

    async fn silent(&self) -> Result<u32, CallError> {
        Ok(4)
    }
}

/// Extracts the `otel.name` and `otel.kind` fields of a call span.
#[derive(Default)]
struct CallVisitor {
    method: Option<String>,
    kind: Option<String>,
}

impl tracing::field::Visit for CallVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "otel.name" => self.method = Some(value.to_string()),
            "otel.kind" => self.kind = Some(value.to_string()),
            _ => (),
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

/// A recorded call span.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallSpan {
    level: Level,
    method: String,
    kind: String,
}

fn call_span(level: Level, method: &str, kind: &str) -> CallSpan {
    CallSpan { level, method: method.to_string(), kind: kind.to_string() }
}

/// Records the level, method name and kind of every call span.
#[derive(Clone, Default)]
struct SpanRecorder(Arc<Mutex<Vec<CallSpan>>>);

impl SpanRecorder {
    /// Removes and returns the recorded spans.
    fn take(&self) -> Vec<CallSpan> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

impl<S> Layer<S> for SpanRecorder
where
    S: tracing::Subscriber,
{
    fn on_new_span(
        &self, attrs: &tracing::span::Attributes, _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<S>,
    ) {
        if attrs.metadata().target() != "remoc::rtc::call" {
            return;
        }

        let mut visitor = CallVisitor::default();
        attrs.record(&mut visitor);
        self.0.lock().unwrap().push(CallSpan {
            level: *attrs.metadata().level(),
            method: visitor.method.unwrap_or_default(),
            kind: visitor.kind.unwrap_or_default(),
        });
    }
}

/// The client and the server create a span per call at the specified level.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn tracing_level_is_applied() {
    crate::init();

    let recorder = SpanRecorder::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<LeveledClient>().await;

    let (server, client) = LeveledServerShared::new(Arc::new(LeveledObj));
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();

        assert_eq!(client.standard().await.unwrap(), 1);
        assert_eq!(client.important().await.unwrap(), 2);
        assert_eq!(client.plain().await.unwrap(), 3);
        assert_eq!(client.silent().await.unwrap(), 4);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();

    assert_eq!(
        recorder.take(),
        vec![
            call_span(Level::DEBUG, "Leveled::standard", "client"),
            call_span(Level::DEBUG, "Leveled::standard", "server"),
            call_span(Level::WARN, "Leveled::important", "client"),
            call_span(Level::WARN, "Leveled::important", "server"),
            call_span(Level::INFO, "Leveled::plain", "client"),
            call_span(Level::INFO, "Leveled::plain", "server"),
        ],
        "unexpected call spans"
    );
}

/// The tracing setting of a client selects the spans created at the client and
/// travels with the client.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn tracing_setting_of_client() {
    crate::init();

    let recorder = SpanRecorder::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<LeveledClient>().await;

    let (server, mut client) = LeveledServerShared::new(Arc::new(LeveledObj));
    assert_eq!(client.tracing(), Tracing::Both, "default tracing setting");
    client.set_tracing(Tracing::Off);
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.tracing(), Tracing::Off, "tracing setting was not transferred");

        // The server creates its span regardless of the setting of the client.
        let expected = [
            (Tracing::Off, vec!["server"]),
            (Tracing::Client, vec!["client", "server"]),
            (Tracing::Server, vec!["server"]),
            (Tracing::Both, vec!["client", "server"]),
        ];
        for (tracing, kinds) in expected {
            let mut client = client.clone();
            client.set_tracing(tracing);
            assert_eq!(client.tracing(), tracing);

            recorder.take();
            assert_eq!(client.plain().await.unwrap(), 3);
            let spans: Vec<_> = recorder.take().into_iter().map(|span| span.kind).collect();
            assert_eq!(spans, kinds, "unexpected call spans for {tracing:?}");
        }
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}

#[rtc::remote]
pub trait Untraced {
    async fn quiet(&self) -> Result<u32, CallError>;
}

pub struct UntracedObj;

impl Untraced for UntracedObj {
    async fn quiet(&self) -> Result<u32, CallError> {
        Ok(4)
    }
}

/// Without the tracing attribute no span is created.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn no_span_by_default() {
    crate::init();

    let recorder = SpanRecorder::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<UntracedClient>().await;

    let (server, client) = UntracedServerShared::new(Arc::new(UntracedObj));
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.quiet().await.unwrap(), 4);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();

    assert_eq!(recorder.take(), vec![], "no call span expected");
}

/// Extracts the identifier fields of a call span.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CallIds {
    kind: String,
    trace_id: Option<String>,
    span_id: Option<String>,
    /// Whether the span id was already set when the span was created.
    span_id_at_creation: bool,
}

impl tracing::field::Visit for CallIds {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "otel.kind" {
            self.kind = value.to_string();
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let value = Some(format!("{value:?}"));
        match field.name() {
            "trace_id" => self.trace_id = value,
            "span_id" => self.span_id = value,
            _ => (),
        }
    }
}

/// Records the identifier fields of every call span, including those recorded
/// after the span was created.
#[derive(Clone, Default)]
struct IdRecorder(Arc<Mutex<Vec<(tracing::span::Id, CallIds)>>>);

impl IdRecorder {
    /// Removes and returns the recorded spans.
    fn take(&self) -> Vec<CallIds> {
        std::mem::take(&mut *self.0.lock().unwrap()).into_iter().map(|(_, ids)| ids).collect()
    }
}

impl<S> Layer<S> for IdRecorder
where
    S: tracing::Subscriber,
{
    fn on_new_span(
        &self, attrs: &tracing::span::Attributes, id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<S>,
    ) {
        if attrs.metadata().target() != "remoc::rtc::call" {
            return;
        }

        let mut ids = CallIds::default();
        attrs.record(&mut ids);
        ids.span_id_at_creation = ids.span_id.is_some();
        self.0.lock().unwrap().push((id.clone(), ids));
    }

    fn on_record(
        &self, id: &tracing::span::Id, values: &tracing::span::Record,
        _ctx: tracing_subscriber::layer::Context<S>,
    ) {
        let mut spans = self.0.lock().unwrap();
        if let Some((_, ids)) = spans.iter_mut().find(|(span_id, _)| span_id == id) {
            values.record(ids);
        }
    }
}

/// Without OpenTelemetry, each call is identified by a random span id that is
/// recorded as `span_id` at the client and the server.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn random_span_id_correlates_spans() {
    crate::init();

    let recorder = IdRecorder::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<LeveledClient>().await;

    let (server, client) = LeveledServerShared::new(Arc::new(LeveledObj));
    a_tx.send(client).await.unwrap();

    // The calls are made within a span, as they would be in an application.
    // Without an OpenTelemetry layer, this makes the OpenTelemetry provider
    // step aside, if it is compiled in, so that the random span id is
    // available when the spans are created.
    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();

        let mut seen = Vec::new();
        for _ in 0..2 {
            recorder.take();
            assert_eq!(client.plain().await.unwrap(), 3);
            let spans = recorder.take();
            assert_eq!(spans.len(), 2, "unexpected call spans: {spans:?}");
            let (client_span, server_span) = (&spans[0], &spans[1]);

            assert_eq!(client_span.kind, "client");
            assert_eq!(server_span.kind, "server");
            let span_id = client_span.span_id.clone().expect("span id not recorded at client");
            assert_eq!(span_id.len(), 16, "span id is not 64 bit hexadecimal: {span_id}");
            assert_eq!(server_span.span_id.as_ref(), Some(&span_id), "span id not recorded at server");
            assert_eq!(client_span.trace_id, None, "unexpected trace id at client");
            assert_eq!(server_span.trace_id, None, "unexpected trace id at server");
            assert!(client_span.span_id_at_creation, "span id not set when creating client span");
            assert!(server_span.span_id_at_creation, "span id not set when creating server span");

            assert!(!seen.contains(&span_id), "span id repeated: {span_id}");
            seen.push(span_id);
        }

        // Without a span at the client, there is nothing to correlate and no
        // identifiers are sent.
        client.set_tracing(Tracing::Server);
        recorder.take();
        assert_eq!(client.plain().await.unwrap(), 3);
        let spans = recorder.take();
        assert_eq!(spans.len(), 1, "unexpected call spans: {spans:?}");
        assert_eq!(spans[0].kind, "server");
        assert_eq!(spans[0].span_id, None, "unexpected span id without client span");
    };
    let client_task = tracing::Instrument::instrument(client_task, tracing::info_span!("test"));

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
