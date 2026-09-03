//! Tracing spans of remote trait calls.

use ::tracing::{Span, level_filters::LevelFilter};

use crate::tracing::{self, SpanKind, Tracing, TracingContext};

/// Creates the span of a call at the specified level.
fn call_span(level: LevelFilter, method: &str, kind: SpanKind, context: Option<&TracingContext>) -> Span {
    tracing::call_span!(target: "remoc::rtc::call", "call", level, method, kind, context)
}

/// Sets up the tracing of a call at the client.
///
/// Returns the span of the call, which is disabled if the client does not
/// create one, and the tracing context to send to the server, if any.
/// Pass the full method name, including the trait, as `method`.
#[doc(hidden)]
pub fn client_call(level: LevelFilter, method: &str, tracing: Tracing) -> (Span, Option<TracingContext>) {
    tracing::client_call(tracing, |context| call_span(level, method, SpanKind::Client, context))
}

/// Creates the span for processing a call at the server.
///
/// The span is linked into the distributed trace of the caller, if it
/// provided a tracing context.
pub(super) fn server_span(level: LevelFilter, method: &str, context: Option<&TracingContext>) -> Span {
    tracing::server_span(context, |context| call_span(level, method, SpanKind::Server, context))
}

/// Creates the span of a task forwarding the requests of a pipelined client.
#[doc(hidden)]
pub fn pipeline_forward_span() -> Span {
    crate::util::task_span!(::tracing::Level::TRACE, "rtc_pipeline_forward")
}
