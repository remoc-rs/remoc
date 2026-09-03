//! Tracing settings of remote functions.

use ::tracing::{Span, level_filters::LevelFilter};
use std::{
    any::type_name,
    sync::{Arc, Mutex, Weak},
};

use crate::tracing::{self, SpanKind, Tracing, TracingContext};

/// Name of a remote function derived from its wrapper and argument types.
fn default_name<A>(wrapper: &str) -> String {
    format!("{wrapper}<{}>", type_name::<A>())
}

/// Creates the span of a call at the specified level.
///
/// The span is a child of the specified parent span or, if it is disabled,
/// of the current span.
fn call_span(
    level: LevelFilter, name: &str, kind: SpanKind, context: Option<&TracingContext>, parent: &Span,
) -> Span {
    if parent.is_none() {
        tracing::call_span!(target: "remoc::rfn::call", "call", level, name, kind, context)
    } else {
        tracing::call_span!(target: "remoc::rfn::call", parent: parent, "call", level, name, kind, context)
    }
}

/// Tracing settings of the provider of a remote function.
pub(super) struct ProviderSettings {
    /// Name of the function, if set.
    name: Option<String>,
    /// Level of the span processing a call.
    level: LevelFilter,
    /// Span within which calls are processed, disabled if not set.
    span: Span,
}

impl ProviderSettings {
    /// Creates the settings with tracing disabled.
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self { name: None, level: LevelFilter::OFF, span: Span::none() }))
    }

    /// Sets the span within which calls are processed.
    ///
    /// A disabled span removes the setting.
    pub fn set_span(&mut self, span: Span) {
        self.span = span;
    }

    /// Creates the span for processing a call of the remote function.
    ///
    /// If tracing is disabled, the configured span or, if none is
    /// configured, the current span is returned, so that the call is
    /// processed within it.
    pub fn server_span<A>(&self, wrapper: &str, context: Option<&TracingContext>) -> Span {
        if self.level == LevelFilter::OFF {
            return if self.span.is_none() { Span::current() } else { self.span.clone() };
        }

        let name = self.name.clone().unwrap_or_else(|| default_name::<A>(wrapper));
        tracing::server_span(context, |context| {
            call_span(self.level, &name, SpanKind::Server, context, &self.span)
        })
    }
}

/// Tracing settings of a remote function.
///
/// The settings travel with the remote function. While it is in the process it
/// was created in, the settings of its provider are linked to it and follow
/// changes of the name and the level.
#[derive(Clone)]
pub(super) struct Settings {
    /// Name of the function, if set.
    name: Option<String>,
    /// Tracing performed at the client.
    tracing: Tracing,
    /// Level of the span of a call at the client.
    level: LevelFilter,
    /// Settings of the provider, linked while in the process of creation.
    provider: Weak<Mutex<ProviderSettings>>,
}

crate::versioned::compact::impl_struct! {
    Settings,
    fields {
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::Option::is_none")]
        name: Option<String> => "_0",
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default_ref")]
        tracing: Tracing => "_1",
        #[serde(default = "crate::tracing::level_repr::off")]
        #[serde(with = "crate::tracing::level_repr")]
        #[serde(skip_serializing_if = "crate::tracing::level_repr::is_off")]
        level: LevelFilter => "_2",
    }
    default { provider }
}

impl Default for Settings {
    fn default() -> Self {
        Self { name: None, tracing: Tracing::default(), level: LevelFilter::OFF, provider: Weak::new() }
    }
}

impl Settings {
    /// Creates the settings linked to the specified provider settings.
    pub fn new(provider: &Arc<Mutex<ProviderSettings>>) -> Self {
        Self { provider: Arc::downgrade(provider), ..Default::default() }
    }

    /// Whether the settings may be left out because they hold their defaults.
    pub fn is_default_ref(this: &&Self) -> bool {
        crate::codec::skip::allow_skip()
            && this.name.is_none()
            && this.tracing == Tracing::default()
            && this.level == LevelFilter::OFF
    }

    /// Applies a change to the linked provider settings, if any.
    fn with_provider(&self, change: impl FnOnce(&mut ProviderSettings)) {
        if let Some(provider) = self.provider.upgrade() {
            change(&mut provider.lock().unwrap_or_else(|err| err.into_inner()));
        }
    }

    /// The name of the function.
    pub fn name<A>(&self, wrapper: &str) -> String {
        self.name.clone().unwrap_or_else(|| default_name::<A>(wrapper))
    }

    /// Sets the name of the function.
    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
        let name = self.name.clone();
        self.with_provider(|provider| provider.name = name);
    }

    /// The tracing performed at the client.
    pub fn tracing(&self) -> Tracing {
        self.tracing
    }

    /// Sets the tracing performed at the client.
    pub fn set_tracing(&mut self, tracing: Tracing) {
        self.tracing = tracing;
    }

    /// The level of the spans.
    pub fn level(&self) -> LevelFilter {
        self.level
    }

    /// Sets the level of the spans.
    pub fn set_level(&mut self, level: LevelFilter) {
        self.level = level;
        self.with_provider(|provider| provider.level = level);
    }

    /// Sets the span within which calls are processed by the provider.
    ///
    /// A disabled span removes the setting.
    pub fn set_span(&mut self, span: Span) {
        self.with_provider(|provider| provider.set_span(span));
    }

    /// Sets up the tracing of a call at the client.
    ///
    /// Returns the span of the call and the tracing context to send.
    pub fn client_call<A>(&self, wrapper: &str) -> (Span, Option<TracingContext>) {
        if self.level == LevelFilter::OFF {
            return (Span::none(), None);
        }

        let name = self.name::<A>(wrapper);
        tracing::client_call(self.tracing, |context| {
            call_span(self.level, &name, SpanKind::Client, context, &Span::none())
        })
    }
}
