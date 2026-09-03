//! This library crate defines the remote pizzeria service and provides the
//! tracing setup shared by client and server.
//!
//! The client and server depend on it.
#![warn(missing_docs)]

use remoc::prelude::*;

/// TCP port the server is listening on.
pub const TCP_PORT: u16 = 9873;

/// A pizza on the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pizza {
    /// Tomatoes, mozzarella and basil.
    Margherita,
    /// Tomatoes, mozzarella and salami.
    Salami,
    /// Tomatoes, mozzarella, ham and pineapple.
    Hawaii,
}

/// Callback reporting the progress of an order.
///
/// The remote function is created by the client and called by the server
/// for each completed step of preparing the pizza. Its calls are traced as
/// well, when the client enables it on the function.
pub type Progress = rfn::RFn<(String,), ()>;

/// Remote pizzeria service.
///
/// The `tracing` argument makes the client create a span at info level for
/// each call and the server one for processing it, which is linked into the
/// trace of the client.
#[rtc::remote(server(Shared), tracing)]
pub trait Pizzeria {
    /// The pizzas on offer.
    ///
    /// Querying the menu is cheap and frequent, so no span is created for it.
    #[tracing(level = "off")]
    async fn menu(&self) -> Result<Vec<Pizza>, rtc::CallError>;

    /// Prepares and bakes the specified pizza, reporting each completed step
    /// through the progress callback.
    async fn order(&self, pizza: Pizza, progress: Progress) -> Result<String, rtc::CallError>;
}

/// Initializes logging to the terminal and, if the environment variable
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set, span export to the OpenTelemetry
/// collector at that endpoint.
///
/// The spans are exported under the specified service name.
pub fn init_tracing(service_name: &'static str) -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::trace::TracerProvider;
    use tracing_subscriber::{
        Layer, filter, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt,
    };

    let provider = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").map(|_| {
        let exporter = opentelemetry_otlp::SpanExporter::builder().with_tonic().build().unwrap();
        opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(opentelemetry_sdk::Resource::builder().with_service_name(service_name).build())
            .build()
    });

    // The OpenTelemetry layer assigns globally meaningful identifiers to the
    // spans of the tracing crate and exports them.
    let otel_layer = provider.as_ref().map(|p| {
        tracing_opentelemetry::layer().with_tracer(p.tracer(service_name)).with_filter(filter::LevelFilter::INFO)
    });

    // Log to the terminal at info level, overridable via `RUST_LOG`.
    // Opening and closing spans are logged as well, so that the spans of the
    // calls and their identifiers are visible without a collector.
    let fmt_layer = tracing_subscriber::fmt::layer().with_span_events(FmtSpan::NEW | FmtSpan::CLOSE).with_filter(
        filter::EnvFilter::builder().with_default_directive(filter::LevelFilter::INFO.into()).from_env_lossy(),
    );

    tracing_subscriber::registry().with(fmt_layer).with(otel_layer).init();

    provider
}
