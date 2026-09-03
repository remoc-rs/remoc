//! Tracing of remote calls.
//!
//! Remote calls can create [tracing] spans: one at the client for the
//! duration of the call and one at the server for processing it.
//! They record the full name of the called method or function as `otel.name`
//! and their side as `otel.kind`, which is `client` or `server`.
//!
//! Together with the call, the client sends a [`TracingContext`] identifying
//! its span. The server records it in the fields `trace_id` and `span_id` of
//! its span, so that the logs of both sides can be correlated, and links its
//! span into the distributed trace of the caller, if there is one.
//!
//! How the identifiers are obtained and how spans are linked is the job of
//! the registered [tracing providers](TracingProvider). If an OpenTelemetry layer
//! is installed on the tracing subscriber, the identifiers of OpenTelemetry
//! are used.
//! Otherwise each call is identified by a [random span id](RandomSpanId),
//! which requires no support from the tracing subscriber.

use std::{
    fmt,
    sync::{Arc, LazyLock, RwLock},
};
use tracing::Span;
use uuid::Uuid;

/// Tracing context of a remote call.
///
/// This identifies the span of the caller using the
/// [W3C trace context](https://www.w3.org/TR/trace-context/) data model.
/// It is transferred to the remote endpoint together with the call, so that the span
/// processing the call can be linked into the distributed trace of the caller.
///
/// The identifiers are assigned by the registered [tracing providers](TracingProvider).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TracingContext {
    /// Identifier of the distributed trace, shared by all spans belonging to it.
    ///
    /// This is zero if the context does not belong to a distributed trace and
    /// only identifies the span of the caller.
    pub trace_id: u128,
    /// Identifier of the span of the caller.
    pub span_id: u64,
    /// Trace flags.
    pub flags: u8,
}

crate::versioned::compact::impl_struct! {
    TracingContext,
    fields {
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default_ref")]
        trace_id: u128 => "_0",
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default_ref")]
        span_id: u64 => "_1",
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default_ref")]
        flags: u8 => "_2",
    }
}

impl TracingContext {
    /// Flag indicating that the trace is sampled.
    pub const SAMPLED: u8 = 0x01;

    /// Whether the trace is sampled.
    pub fn is_sampled(&self) -> bool {
        self.flags & Self::SAMPLED != 0
    }

    /// Whether the context belongs to a distributed trace.
    ///
    /// A context without a trace id only identifies the span of the caller.
    pub fn has_trace(&self) -> bool {
        self.trace_id != 0
    }

    /// Tracing context of the current span.
    ///
    /// This is [`None`] if no registered [tracing provider](TracingProvider)
    /// can determine the context of the span.
    pub fn current() -> Option<Self> {
        Self::from_span(&Span::current())
    }

    /// Tracing context of the specified span.
    ///
    /// This is obtained from the active [tracing provider](TracingProvider)
    /// and [`None`] if it cannot determine the context of the span.
    pub fn from_span(span: &Span) -> Option<Self> {
        active_provider()?.context_of(span)
    }

    /// Makes the remote span identified by this tracing context the parent
    /// of the specified span.
    ///
    /// This is performed by the active [tracing provider](TracingProvider).
    /// Returns whether it has done so.
    pub fn set_parent_of(&self, span: &Span) -> bool {
        active_provider().is_some_and(|provider| provider.attach_parent(self, span))
    }

    /// Records the identifiers as fields of the span.
    ///
    /// The trace id is recorded as `trace_id` if present and the span id as
    /// `span_id`, both in hexadecimal notation.
    /// Fields the span does not declare are silently skipped.
    fn record(&self, span: &Span) {
        if let Some(trace_id) = self.trace_id_field() {
            span.record("trace_id", trace_id);
        }
        span.record("span_id", self.span_id_field());
    }

    /// The trace id as span field value, if present.
    pub(crate) fn trace_id_field(&self) -> Option<tracing::field::DisplayValue<HexId<u128>>> {
        self.has_trace().then(|| tracing::field::display(HexId(self.trace_id)))
    }

    /// The span id as span field value.
    pub(crate) fn span_id_field(&self) -> tracing::field::DisplayValue<HexId<u64>> {
        tracing::field::display(HexId(self.span_id))
    }
}

/// Identifier formatted as zero-padded hexadecimal number.
pub(crate) struct HexId<T>(T);

impl fmt::Display for HexId<u128> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl fmt::Display for HexId<u64> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl fmt::Debug for TracingContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TracingContext")
            .field("trace_id", &format_args!("{:032x}", self.trace_id))
            .field("span_id", &format_args!("{:016x}", self.span_id))
            .field("flags", &format_args!("{:02x}", self.flags))
            .finish()
    }
}

/// Provides tracing contexts of spans and links spans into distributed traces.
///
/// A tracing provider connects the tracing subscriber of the application
/// with the tracing contexts transported by remote calls.
/// It is queried by the client to obtain the [tracing context](TracingContext)
/// sent with a call and by the server to link the span processing the call
/// to the span of the caller.
///
/// The first provider that is [active](Self::is_active), in the order
/// returned by [`tracing_providers`], is used.
/// Use [`register_tracing_provider`] to add a provider that takes precedence
/// over the registered ones, for example one that obtains the identifiers from
/// a custom layer of the tracing subscriber, or [`set_tracing_providers`] to
/// replace them entirely.
///
/// You must implement either [`new_context`](Self::new_context) or
/// [`context_of`](Self::context_of).
pub trait TracingProvider: Send + Sync + 'static {
    /// Whether the provider is currently able to provide tracing contexts.
    ///
    /// The first active provider is used.
    fn is_active(&self) -> bool {
        true
    }

    /// Tracing context for a span that is about to be created.
    ///
    /// A provider that assigns identifiers on its own, without support from
    /// the tracing subscriber, returns them here, so that they can be
    /// recorded when the span is created.
    /// Returns [`None`] if the identifiers are only known once the span
    /// exists, in which case the span is created and
    /// [`context_of`](Self::context_of) is queried for it.
    fn new_context(&self) -> Option<TracingContext> {
        None
    }

    /// Tracing context of the specified span.
    ///
    /// Returns [`None`] if the provider cannot determine the context of the span.
    fn context_of(&self, span: &Span) -> Option<TracingContext> {
        let _ = span;
        None
    }

    /// Makes the remote span identified by the tracing context the parent of
    /// the specified span.
    ///
    /// Returns whether the provider has done so.
    fn attach_parent(&self, context: &TracingContext, span: &Span) -> bool;
}

/// Registered tracing providers, in the order they are tried.
type Providers = Arc<[Arc<dyn TracingProvider>]>;

/// Registered tracing providers, in the order they are tried.
#[allow(clippy::vec_init_then_push)]
static PROVIDERS: LazyLock<RwLock<Providers>> = LazyLock::new(|| {
    let mut providers: Vec<Arc<dyn TracingProvider>> = Vec::new();
    #[cfg(feature = "otel")]
    providers.push(Arc::new(OpenTelemetry));
    providers.push(Arc::new(RandomSpanId));
    RwLock::new(providers.into())
});

/// Snapshot of the registered tracing providers.
fn providers() -> Providers {
    PROVIDERS.read().unwrap_or_else(|err| err.into_inner()).clone()
}

/// The first active tracing provider.
fn active_provider() -> Option<Arc<dyn TracingProvider>> {
    providers().iter().find(|provider| provider.is_active()).cloned()
}

/// The registered [tracing providers](TracingProvider), in the order they are tried.
pub fn tracing_providers() -> Vec<Arc<dyn TracingProvider>> {
    providers().to_vec()
}

/// Replaces the registered [tracing providers](TracingProvider).
///
/// The providers are tried in the order specified.
pub fn set_tracing_providers(providers: Vec<Arc<dyn TracingProvider>>) {
    *PROVIDERS.write().unwrap_or_else(|err| err.into_inner()) = providers.into();
}

/// Registers a [tracing provider](TracingProvider).
///
/// The provider is tried before the already registered providers.
pub fn register_tracing_provider(provider: impl TracingProvider) {
    let mut guard = PROVIDERS.write().unwrap_or_else(|err| err.into_inner());
    let mut providers: Vec<Arc<dyn TracingProvider>> = vec![Arc::new(provider)];
    providers.extend(guard.iter().cloned());
    *guard = providers.into();
}

/// Tracing provider that identifies each call by a random span id.
///
/// This allows the spans of the client and the server to be correlated using
/// the `span_id` field, which is recorded on the span of the call at
/// the client and on the span processing it at the server, without any
/// support from the tracing subscriber.
/// No trace id is assigned, since spans of the application cannot be
/// identified without such support.
///
/// The identifier is assigned when the span of a call is created at the
/// client. No context is provided for existing spans, thus nothing is sent
/// when a client does not create a span for a call.
pub struct RandomSpanId;

impl TracingProvider for RandomSpanId {
    fn new_context(&self) -> Option<TracingContext> {
        let (span_id, _) = Uuid::new_v4().as_u64_pair();
        Some(TracingContext { trace_id: 0, span_id: span_id.max(1), flags: 0 })
    }

    fn attach_parent(&self, _context: &TracingContext, _span: &Span) -> bool {
        true
    }
}

/// The tracing a client performs for its calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Tracing {
    /// No span is created at the client and no tracing context is sent to
    /// the server.
    Off,
    /// A span is created at the client for each call, but no tracing context
    /// is sent to the server.
    ///
    /// This measures the calls locally without linking the spans of the server
    /// to them.
    Client,
    /// The [tracing context](TracingContext) of the current span is sent to
    /// the server, so that its spans join the trace of the caller, but no span
    /// is created at the client.
    Server,
    /// A span is created at the client for each call and its tracing context
    /// is sent to the server, so that the spans of the server become children
    /// of the span of the call.
    ///
    /// This is the default.
    #[default]
    Both,
}

crate::versioned::compact::impl_enum! {
    Tracing,
    variants {
        Off => "_0",
        Client => "_1",
        Server => "_2",
        #[serde(other)]
        Both => "_3",
    }
}

/// Sets up the tracing of a call at the client.
///
/// The function `span` creates the span of the call, recording the tracing
/// context passed to it, if any. It is not called if the client does not
/// create a span.
///
/// Returns the span of the call, which is disabled if the client does not
/// create one, and the tracing context to send to the server, if any.
///
/// The context is obtained from the active [tracing provider](TracingProvider).
/// If it assigns the context on its own, it is recorded when the span is
/// created; otherwise the provider is queried for the context of the created
/// span, which is then recorded afterwards.
pub(crate) fn client_call(
    tracing: Tracing, span: impl FnOnce(Option<&TracingContext>) -> Span,
) -> (Span, Option<TracingContext>) {
    match tracing {
        Tracing::Off => (Span::none(), None),
        Tracing::Server => (Span::none(), TracingContext::from_span(&Span::current())),
        Tracing::Client | Tracing::Both => {
            let (span, context) = match active_provider() {
                Some(provider) => match provider.new_context() {
                    Some(context) => (span(Some(&context)), Some(context)),
                    None => {
                        let span = span(None);
                        let context = provider.context_of(&span);
                        if let Some(context) = &context {
                            context.record(&span);
                        }
                        (span, context)
                    }
                },
                None => (span(None), None),
            };
            (span, context.filter(|_| tracing == Tracing::Both))
        }
    }
}

/// Creates the span for processing a call at the server.
///
/// The function `span` creates the span, recording the tracing context
/// passed to it, if any. When the caller provided a tracing context, the
/// span of the caller becomes the parent of the created span, linking it
/// into the distributed trace of the caller.
pub(crate) fn server_span(
    context: Option<&TracingContext>, span: impl FnOnce(Option<&TracingContext>) -> Span,
) -> Span {
    let span = span(context);
    if let Some(context) = context {
        context.set_parent_of(&span);
    }
    span
}

/// Side of a call that a span represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanKind {
    /// The span covers the call at the calling side.
    Client,
    /// The span covers the processing of the call at the serving side.
    Server,
}

impl SpanKind {
    /// The value of the `otel.kind` field.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

/// Creates a span for a call at the specified level.
///
/// The target and name of the span must be literals. If a parent is specified,
/// the span is created as its child instead of the child of the current span.
/// The span records the full method name as `otel.name` and the [span kind](SpanKind) as `otel.kind`.
/// The fields `trace_id` and `span_id` are set from the tracing context, if provided.
/// [`LevelFilter::OFF`](tracing::level_filters::LevelFilter::OFF) yields a disabled span.
macro_rules! call_span {
    (target: $target:literal, $(parent: $parent:expr,)? $name:literal, $level:expr, $method:expr, $kind:expr, $context:expr) => {{
        let level: ::tracing::level_filters::LevelFilter = $level;
        let method: &str = $method;
        let kind: $crate::tracing::SpanKind = $kind;
        let context: ::std::option::Option<&$crate::tracing::TracingContext> = $context;
        let trace_id = context.and_then(|context| context.trace_id_field());
        let span_id = context.map(|context| context.span_id_field());

        match level.into_level() {
            Some(::tracing::Level::ERROR) => ::tracing::error_span!(target: $target, $(parent: $parent,)? $name,
                otel.name = method, otel.kind = kind.as_str(), trace_id = trace_id, span_id = span_id),
            Some(::tracing::Level::WARN) => ::tracing::warn_span!(target: $target, $(parent: $parent,)? $name,
                otel.name = method, otel.kind = kind.as_str(), trace_id = trace_id, span_id = span_id),
            Some(::tracing::Level::INFO) => ::tracing::info_span!(target: $target, $(parent: $parent,)? $name,
                otel.name = method, otel.kind = kind.as_str(), trace_id = trace_id, span_id = span_id),
            Some(::tracing::Level::DEBUG) => ::tracing::debug_span!(target: $target, $(parent: $parent,)? $name,
                otel.name = method, otel.kind = kind.as_str(), trace_id = trace_id, span_id = span_id),
            Some(::tracing::Level::TRACE) => ::tracing::trace_span!(target: $target, $(parent: $parent,)? $name,
                otel.name = method, otel.kind = kind.as_str(), trace_id = trace_id, span_id = span_id),
            None => ::tracing::Span::none(),
        }
    }};
}
pub(crate) use call_span;

/// Runs the future within the span.
///
/// The output type is explicit, so that the generated code can name it.
#[doc(hidden)]
pub fn instrumented<T>(
    fut: impl std::future::Future<Output = T>, span: Span,
) -> impl std::future::Future<Output = T> {
    tracing::Instrument::instrument(fut, span)
}

/// Transported representation of a level filter.
///
/// The level is encoded as a number, with unknown values read as
/// [`LevelFilter::OFF`](tracing::level_filters::LevelFilter::OFF).
#[doc(hidden)]
pub mod level_repr {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::borrow::Borrow;
    use tracing::{Level, level_filters::LevelFilter};

    /// The default level.
    pub fn off() -> LevelFilter {
        LevelFilter::OFF
    }

    /// Whether the level may be left out because it is off.
    pub fn is_off<L: Borrow<LevelFilter>>(level: &L) -> bool {
        crate::codec::skip::allow_skip() && *level.borrow() == LevelFilter::OFF
    }

    /// Serializes the level.
    pub fn serialize<S: Serializer, L: Borrow<LevelFilter>>(level: &L, serializer: S) -> Result<S::Ok, S::Error> {
        let repr: u8 = match level.borrow().into_level() {
            None => 0,
            Some(Level::ERROR) => 1,
            Some(Level::WARN) => 2,
            Some(Level::INFO) => 3,
            Some(Level::DEBUG) => 4,
            Some(Level::TRACE) => 5,
        };
        repr.serialize(serializer)
    }

    /// Deserializes the level.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<LevelFilter, D::Error> {
        Ok(match u8::deserialize(deserializer)? {
            1 => LevelFilter::ERROR,
            2 => LevelFilter::WARN,
            3 => LevelFilter::INFO,
            4 => LevelFilter::DEBUG,
            5 => LevelFilter::TRACE,
            _ => LevelFilter::OFF,
        })
    }
}

#[cfg(feature = "otel")]
mod otel {
    use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
    use std::fmt;
    use tracing::Span;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    use super::{TracingContext, TracingProvider};

    /// The OpenTelemetry span context is invalid and thus has no tracing context.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct InvalidSpanContext;

    impl fmt::Display for InvalidSpanContext {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "invalid OpenTelemetry span context")
        }
    }

    impl std::error::Error for InvalidSpanContext {}

    impl TryFrom<&SpanContext> for TracingContext {
        type Error = InvalidSpanContext;

        /// Converts an OpenTelemetry span context into a tracing context.
        fn try_from(span_context: &SpanContext) -> Result<Self, Self::Error> {
            if !span_context.is_valid() {
                return Err(InvalidSpanContext);
            }

            Ok(Self {
                trace_id: u128::from_be_bytes(span_context.trace_id().to_bytes()),
                span_id: u64::from_be_bytes(span_context.span_id().to_bytes()),
                flags: span_context.trace_flags().to_u8(),
            })
        }
    }

    impl From<TracingContext> for SpanContext {
        /// Converts the tracing context into the OpenTelemetry span context
        /// of a remote span.
        fn from(context: TracingContext) -> Self {
            SpanContext::new(
                TraceId::from_bytes(context.trace_id.to_be_bytes()),
                SpanId::from_bytes(context.span_id.to_be_bytes()),
                TraceFlags::new(context.flags),
                true,
                TraceState::default(),
            )
        }
    }

    /// Tracing provider that uses the OpenTelemetry context of spans.
    ///
    /// This requires an OpenTelemetry layer to be installed on the tracing
    /// subscriber. It yields no context for spans that have no valid
    /// OpenTelemetry context.
    ///
    /// The provider is inactive while the current span has no valid
    /// OpenTelemetry context, since then either no OpenTelemetry layer is
    /// installed or the span is not traced, and thus the spans of calls
    /// made within it cannot be linked into a distributed trace.
    /// It is active when there is no current span, since a call made
    /// outside of any span starts a new trace.
    pub struct OpenTelemetry;

    impl TracingProvider for OpenTelemetry {
        fn is_active(&self) -> bool {
            let current = Span::current();
            current.is_none() || self.context_of(&current).is_some()
        }

        fn context_of(&self, span: &Span) -> Option<TracingContext> {
            let cx = span.context();
            let span_ref = cx.span();
            TracingContext::try_from(span_ref.span_context()).ok()
        }

        fn attach_parent(&self, context: &TracingContext, span: &Span) -> bool {
            if !context.has_trace() {
                return false;
            }

            let cx = opentelemetry::Context::new().with_remote_span_context(SpanContext::from(*context));
            span.set_parent(cx).is_ok()
        }
    }
}

#[cfg(feature = "otel")]
pub use otel::{InvalidSpanContext, OpenTelemetry};
