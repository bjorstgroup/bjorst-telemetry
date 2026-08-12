//! Tracing + OpenTelemetry bootstrap shared across Bjorst axum services.
//!
//! A single [`init`] call wires a `tracing_subscriber` registry with:
//! - an `EnvFilter` (from a caller-supplied directive string),
//! - a stdout fmt layer ([`LogFormat`] — JSON, compact, or the default),
//! - and, when [`TelemetryConfig::otlp`] is set, a batched OTLP span exporter.
//!
//! [`init_with`] takes additional layers, each carrying its own filter. See its
//! documentation for why the caller's `env_filter` cannot be allowed to reach
//! them.
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
use tracing_subscriber::{
    fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer, Registry,
};

/// An extra layer for [`init_with`], erased so callers can pass a heterogeneous
/// list. Produce one with `my_layer.with_filter(…).boxed()`.
///
/// It is typed against [`Registry`] rather than the composed subscriber because
/// extra layers are installed directly onto the registry, beneath the fmt and
/// OTLP layers — see [`init_with`].
pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

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

/// The registry, the `extra` layers, and the filtered stdout layer — everything
/// [`init_with`] composes before the optional OTLP layer goes on top.
///
/// Separated out so it can be exercised under
/// [`tracing::subscriber::with_default`]. `init` installs a **global**
/// subscriber, which a process may do exactly once, so a test that called it
/// could never be joined by a second one; the interesting behaviour — which
/// layer sees which event — is all here.
///
/// The return type spells out `LookupSpan` because per-layer filtering needs
/// it: `Layer::with_filter` on the OTLP layer requires the subscriber beneath
/// to be one whose spans can be looked up, and an opaque `impl Subscriber`
/// loses that.
fn stdout_subscriber(
    cfg: &TelemetryConfig,
    extra: Vec<BoxedLayer>,
) -> impl tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a> + Send + Sync {
    let fmt_layer = match cfg.log_format {
        LogFormat::Json => fmt::layer().json().boxed(),
        LogFormat::Compact => fmt::layer().compact().with_target(false).boxed(),
        LogFormat::Default => fmt::layer().boxed(),
    }
    .with_filter(resolve_filter(&cfg.env_filter));

    // `extra` goes on first because a `BoxedLayer` is typed against `Registry`:
    // once fmt is layered on, the subscriber type is `Layered<_, Registry>` and
    // no `Layer<Registry>` fits it. Order carries no filtering meaning here —
    // every layer now decides for itself.
    tracing_subscriber::registry().with(extra).with(fmt_layer)
}

/// Install the global subscriber per `cfg` and return a flush guard.
///
/// Must be called once, early, inside a Tokio runtime (the OTLP batch exporter
/// spawns on `runtime::Tokio`). Panics if a global subscriber is already set —
/// matching fail-fast startup expectations of a binary's `main`.
pub fn init(cfg: TelemetryConfig) -> TelemetryGuard {
    init_with(cfg, Vec::new())
}

/// [`init`], plus `extra` layers that the caller's `env_filter` does not reach.
///
/// # Why the filter must not reach them
///
/// `env_filter` is what an operator sets `LOG_LEVEL` to, and it exists to
/// decide **what gets printed**. Installed the obvious way — as a layer on the
/// registry — it is not a printing decision at all: `Layered::enabled` is the
/// conjunction of every layer's, so a directive of `info` discards the event
/// for the whole subscriber and any layer added afterwards receives nothing.
///
/// That is fatal for a layer whose job is to *measure* rather than to print.
/// `sqlx` emits one event per statement carrying its own `elapsed_secs`, at
/// `DEBUG` — the level nobody runs a production API at — so a metrics layer
/// reading those events under a global `info` filter records zero for ever, and
/// looks exactly like a database that is never queried.
///
/// So the filter is applied **per layer**, to fmt and OTLP only, and `extra`
/// layers carry their own. Install them with their filter already attached:
///
/// ```no_run
/// # use tracing_subscriber::{filter::Targets, Layer};
/// # use tracing::level_filters::LevelFilter;
/// # fn make() -> bjorst_telemetry::BoxedLayer {
/// # let my_layer = tracing_subscriber::fmt::layer();
/// my_layer
///     .with_filter(Targets::new().with_target("sqlx::query", LevelFilter::DEBUG))
///     .boxed()
/// # }
/// ```
///
/// An unfiltered extra layer sees **every** event in the process, which is
/// rarely what is wanted and raises the global max-level hint to `TRACE`.
///
/// Existing `init` callers are unaffected: with no extra layers, a per-layer
/// filter on fmt (and on OTLP) admits and discards exactly what the global one
/// did.
pub fn init_with(cfg: TelemetryConfig, extra: Vec<BoxedLayer>) -> TelemetryGuard {
    let base = stdout_subscriber(&cfg, extra);

    let Some(otlp) = cfg.otlp else {
        base.init();
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
            base.init();
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
    let otel_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        // A second parse of the same directive: `EnvFilter` is not `Clone`, and
        // two layers cannot share one.
        .with_filter(resolve_filter(&cfg.env_filter));

    base.with(otel_layer).init();

    TelemetryGuard {
        provider: Some(provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::filter::Targets;

    /// Records the target of every event it is shown. Stands in for a metrics
    /// layer: the question is only ever whether the event arrives.
    struct Probe(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> Layer<S> for Probe {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .expect("probe mutex")
                .push(event.metadata().target().to_owned());
        }
    }

    fn cfg_at(level: &str) -> TelemetryConfig {
        TelemetryConfig {
            service_name: "svc".into(),
            env_filter: level.into(),
            log_format: LogFormat::Compact,
            otlp: None,
        }
    }

    /// The whole reason `init_with` exists. `sqlx` reports every statement's
    /// duration at DEBUG; an API runs at `info`. Under the old composition —
    /// `EnvFilter` as a layer on the registry — this event reached nothing, and
    /// a metrics layer subscribed to it recorded zero for ever while looking
    /// perfectly healthy.
    #[test]
    fn an_extra_layer_sees_debug_events_the_env_filter_excludes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = Probe(Arc::clone(&seen))
            .with_filter(Targets::new().with_target("sqlx::query", LevelFilter::DEBUG))
            .boxed();

        tracing::subscriber::with_default(
            stdout_subscriber(&cfg_at("info"), vec![probe]),
            || tracing::debug!(target: "sqlx::query", elapsed_secs = 0.5, "select 1"),
        );

        assert_eq!(
            seen.lock().expect("probe mutex").as_slice(),
            ["sqlx::query"]
        );
    }

    /// The corollary: an extra layer is not handed the whole process. Its own
    /// filter is the only thing narrowing it, so a filter that names one target
    /// must exclude the rest — otherwise every `debug!` in every dependency
    /// would be recorded as a query.
    #[test]
    fn an_extra_layers_own_filter_still_narrows_it() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = Probe(Arc::clone(&seen))
            .with_filter(Targets::new().with_target("sqlx::query", LevelFilter::DEBUG))
            .boxed();

        tracing::subscriber::with_default(stdout_subscriber(&cfg_at("info"), vec![probe]), || {
            tracing::debug!(target: "hyper::client", "connecting");
            tracing::error!(target: "some::other", "boom");
        });

        assert!(seen.lock().expect("probe mutex").is_empty());
    }

    /// The composition this change replaced, kept as a test so the reason is
    /// not just a comment: with `EnvFilter` as a layer on the registry, the
    /// identical probe under the identical directive receives nothing.
    ///
    /// Written the obvious way, `registry().with(filter).with(probe)` is what
    /// anyone would reach for — which is exactly why it is worth pinning.
    #[test]
    fn a_global_env_filter_layer_starves_the_extra_layer() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let probe = Probe(Arc::clone(&seen))
            .with_filter(Targets::new().with_target("sqlx::query", LevelFilter::DEBUG))
            .boxed();

        let old_composition = tracing_subscriber::registry()
            .with(resolve_filter("info"))
            .with(vec![probe]);

        tracing::subscriber::with_default(old_composition, || {
            tracing::debug!(target: "sqlx::query", elapsed_secs = 0.5, "select 1");
        });

        assert!(
            seen.lock().expect("probe mutex").is_empty(),
            "a global EnvFilter layer used to admit this event; the whole change is unnecessary"
        );
    }

    /// `init` is `init_with` with nothing extra, and must compose to the same
    /// thing it always did: no extra layer, so nothing outside the filter is
    /// observed at all.
    #[test]
    fn no_extra_layers_composes_as_before() {
        tracing::subscriber::with_default(stdout_subscriber(&cfg_at("info"), Vec::new()), || {
            tracing::debug!(target: "sqlx::query", "select 1");
        });
    }

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
