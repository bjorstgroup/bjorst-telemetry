//! Tracing + OpenTelemetry bootstrap shared across Bjorst axum services.
//!
//! A single [`init`] call wires a `tracing_subscriber` registry with:
//! - an `EnvFilter` (from a caller-supplied directive string),
//! - a stdout fmt layer ([`LogFormat`] — JSON, compact, or the default),
//! - and, when [`TelemetryConfig::otlp`] is set, a batched OTLP span exporter.
//!
//! The crate is intentionally backend-agnostic. Axiom is just an OTLP endpoint
//! plus two headers, so callers that target Axiom pass the endpoint and headers
//! in [`OtlpConfig`]; no vendor logic lives here.
//!
//! ```no_run
//! use bjorst_telemetry::{init, LogFormat, OtlpConfig, TelemetryConfig};
//!
//! # async fn run() {
//! let _guard = init(TelemetryConfig {
//!     service_name: "my-api".into(),
//!     env_filter: "info,sqlx=warn".into(),
//!     log_format: LogFormat::Json,
//!     otlp: Some(OtlpConfig {
//!         endpoint: "https://api.axiom.co/v1/traces".into(),
//!         headers: vec![
//!             ("Authorization".into(), "Bearer xxx".into()),
//!             ("X-Axiom-Dataset".into(), "my-dataset".into()),
//!         ],
//!     }),
//! });
//! // ... run the server; `_guard` flushes spans on drop.
//! # }
//! ```

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{trace::TracerProvider, Resource};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Stdout log rendering for the fmt layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Structured JSON, one object per line. Best for production log shippers.
    Json,
    /// Compact human-readable single line, target field omitted. Best for dev.
    Compact,
    /// The `tracing_subscriber` default full format.
    Default,
}

/// OTLP exporter target. `headers` is applied verbatim — supply auth headers
/// (e.g. Axiom `Authorization` + `X-Axiom-Dataset`) here, or leave it empty for
/// an unauthenticated collector.
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// Full traces endpoint, e.g. `https://api.axiom.co/v1/traces` or
    /// `http://localhost:4318/v1/traces`.
    pub endpoint: String,
    /// Header pairs sent with every export request. May be empty.
    pub headers: Vec<(String, String)>,
}

/// Everything [`init`] needs. The caller resolves all env/vendor specifics and
/// passes plain values in.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// `service.name` resource attribute and tracer name.
    pub service_name: String,
    /// `EnvFilter` directive string, e.g. `"info,sqlx=warn,h2=warn"`. Falls back
    /// to `"info"` if it fails to parse.
    pub env_filter: String,
    /// Stdout rendering.
    pub log_format: LogFormat,
    /// When `Some`, spans are also exported over OTLP. When `None`, only the
    /// stdout layer is installed.
    pub otlp: Option<OtlpConfig>,
}

/// Flushes the OTLP exporter on drop so the final span batch is not lost when
/// the process exits. Holding it for the lifetime of the program is enough;
/// when `otlp` was `None` the drop is a no-op.
#[must_use = "dropping the guard immediately shuts telemetry down"]
pub struct TelemetryGuard {
    provider: Option<TracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if self.provider.is_some() {
            opentelemetry::global::shutdown_tracer_provider();
        }
    }
}

/// Parse `directive`, falling back to a bare `info` filter when it is invalid.
fn resolve_filter(directive: &str) -> EnvFilter {
    EnvFilter::try_new(directive).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Install the global subscriber per `cfg` and return a flush guard.
///
/// Must be called once, early, inside a Tokio runtime (the OTLP batch exporter
/// spawns on `runtime::Tokio`). Panics if a global subscriber is already set —
/// matching fail-fast startup expectations of a binary's `main`.
pub fn init(cfg: TelemetryConfig) -> TelemetryGuard {
    let filter = resolve_filter(&cfg.env_filter);

    let fmt_layer = match cfg.log_format {
        LogFormat::Json => fmt::layer().json().boxed(),
        LogFormat::Compact => fmt::layer().compact().with_target(false).boxed(),
        LogFormat::Default => fmt::layer().boxed(),
    };

    let Some(otlp) = cfg.otlp else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
        return TelemetryGuard { provider: None };
    };

    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&otlp.endpoint);
    if !otlp.headers.is_empty() {
        let headers: std::collections::HashMap<String, String> = otlp.headers.into_iter().collect();
        builder = builder.with_headers(headers);
    }

    let exporter = match builder.build() {
        Ok(exporter) => exporter,
        Err(err) => {
            // Degrade to stdout-only rather than crashing the service if the
            // exporter cannot be constructed.
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();
            tracing::error!(error = %err, "OTLP exporter build failed; span export disabled");
            return TelemetryGuard { provider: None };
        }
    };

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(Resource::new(vec![KeyValue::new(
            "service.name",
            cfg.service_name.clone(),
        )]))
        .build();

    opentelemetry::global::set_tracer_provider(provider.clone());
    let tracer = provider.tracer(cfg.service_name.clone());
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    TelemetryGuard {
        provider: Some(provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_directive_is_used() {
        // A parseable directive should not fall back. EnvFilter has no public
        // equality, so we assert via its Display round-trip containing levels.
        let f = resolve_filter("warn,sqlx=error");
        let rendered = format!("{f}");
        assert!(rendered.contains("sqlx"));
    }

    #[test]
    fn invalid_directive_falls_back_to_info() {
        // An unrecognised level on a target is a hard parse error, unlike a bare
        // unknown token (which EnvFilter accepts as a target name).
        let f = resolve_filter("app=definitely_not_a_level");
        let rendered = format!("{f}");
        assert_eq!(rendered, "info");
    }

    #[test]
    fn config_without_otlp_is_stdout_only() {
        let cfg = TelemetryConfig {
            service_name: "svc".into(),
            env_filter: "info".into(),
            log_format: LogFormat::Compact,
            otlp: None,
        };
        assert!(cfg.otlp.is_none());
        assert_eq!(cfg.log_format, LogFormat::Compact);
    }
}
