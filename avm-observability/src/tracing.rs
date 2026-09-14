//! Tracing + (optional) OpenTelemetry setup.
//!
//! Default build installs a structured `tracing-subscriber` stack driven by
//! `RUST_LOG`. Building with `--features otlp` is the hook where the OTLP
//! pipeline gets wired to the collector at `OTEL_EXPORTER_OTLP_ENDPOINT`.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Telemetry knobs, normally derived from the environment.
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
        Self {
            service_name: service_name.into(),
            default_filter: "info,avm=debug".to_string(),
            json: std::env::var("AVM_LOG_FORMAT").as_deref() == Ok("json"),
            otlp_endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
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
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.default_filter.clone()));

    let registry = tracing_subscriber::registry().with(filter);

    let installed = if cfg.json {
        registry
            .with(fmt::layer().json().with_current_span(true))
            .try_init()
            .is_ok()
    } else {
        registry
            .with(fmt::layer().with_target(true))
            .try_init()
            .is_ok()
    };

    if installed {
        tracing::info!(
            service.name = %cfg.service_name,
            otlp_endpoint = ?cfg.otlp_endpoint,
            otlp_enabled = cfg!(feature = "otlp"),
            "telemetry initialised"
        );
    }

    #[cfg(feature = "otlp")]
    {
        // TODO(avm): wire opentelemetry_sdk TracerProvider + opentelemetry-otlp
        // exporter here and layer `tracing_opentelemetry::layer()` onto the
        // registry above. Pinned behind a feature so the default build does not
        // depend on a fast-moving OTel API surface.
        tracing::warn!("otlp feature enabled but exporter wiring is not implemented yet");
    }
}

/// Standard span field names, so every crate tags traces the same way.
pub mod fields {
    pub const JOB_ID: &str = "avm.job_id";
    pub const AGENT_ID: &str = "avm.agent_id";
    pub const TENANT_ID: &str = "avm.tenant_id";
    pub const PROJECT_ID: &str = "avm.project_id";
    pub const SCOPE: &str = "avm.scope";
}
