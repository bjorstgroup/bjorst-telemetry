# bjorst-telemetry

Tracing + OpenTelemetry bootstrap shared across Bjorst axum services.

A single `init` call installs a `tracing_subscriber` registry with an
`EnvFilter`, a stdout fmt layer, and — optionally — a batched OTLP span
exporter. The crate is backend-agnostic: Axiom is just an OTLP endpoint plus two
headers, so vendor specifics stay in the caller.

## Usage

```rust
use bjorst_telemetry::{init, LogFormat, OtlpConfig, TelemetryConfig};

#[tokio::main]
async fn main() {
    let _guard = init(TelemetryConfig {
        service_name: "my-api".into(),
        env_filter: "info,sqlx=warn,h2=warn".into(),
        log_format: LogFormat::Json,
        // None => stdout-only. Some(..) => also export spans over OTLP.
        otlp: Some(OtlpConfig {
            endpoint: "https://api.axiom.co/v1/traces".into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {token}")),
                ("X-Axiom-Dataset".into(), dataset),
            ],
        }),
    });

    // run the server; `_guard` flushes the final span batch on drop.
}
```

## Notes

- Call `init` once, early, inside a Tokio runtime (the OTLP batch exporter spawns
  on `runtime::Tokio`).
- If the OTLP exporter fails to build, the service degrades to stdout-only
  instead of crashing.
- Hold the returned `TelemetryGuard` for the lifetime of the process. Dropping it
  shuts telemetry down and flushes pending spans.

## License

MIT
