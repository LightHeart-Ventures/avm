//! Legacy tracing bootstrap, delegating to [`avm_otel`].
//!
//! Kept so existing call sites (`avm_observability::init("avm-gateway")`) keep
//! compiling while services migrate to `avm_otel::init_otel`.

use avm_otel::OtelConfig;

/// Telemetry knobs, normally derived from the environment.
///
/// Retained for backward compatibility; internally this is converted into an
/// [`avm_otel::OtelConfig`].
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// `service.name` resource attribute.
    pub service_name: String,
    /// Default filter directives when `RUST_LOG` is unset.
    pub default_filter: String,
    /// Emit newline-delimited JSON instead of pretty text.
    pub json: bool,
    /// OTLP collector endpoint (used when the `otlp` feature is enabled).
    pub otlp_endpoint: Option<String>,
}

impl TelemetryConfig {
    pub fn new(service_name: impl Into<String>) -> Self {
        let service_name = service_name.into();
        let cfg = OtelConfig::from_env(&service_name, env!("CARGO_PKG_VERSION"));
        Self {
            service_name,
            default_filter: cfg.default_filter,
            json: cfg.json_logs,
            otlp_endpoint: cfg.endpoint,
        }
    }
}

/// Install the global subscriber for `service_name`. Idempotent-ish: a second
/// call is a no-op because the global default is already set.
pub fn init(service_name: &str) {
    init_with(TelemetryConfig::new(service_name));
}

/// Install the global subscriber from an explicit config.
pub fn init_with(cfg: TelemetryConfig) {
    let mut otel = OtelConfig::from_env(&cfg.service_name, env!("CARGO_PKG_VERSION"));
    otel.json_logs = cfg.json;
    otel.default_filter = cfg.default_filter.clone();
    otel.endpoint = cfg.otlp_endpoint.clone();
    let _ = avm_otel::init_otel_with(otel);
}
