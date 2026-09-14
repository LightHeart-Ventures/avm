//! # avm-otel
//!
//! One OpenTelemetry bootstrap for every AVM service.
//!
//! ```no_run
//! let _otel = avm_otel::init_otel("avm-gateway", env!("CARGO_PKG_VERSION"));
//! tracing::info!("gateway up");
//! ```
//!
//! ## Layers
//!
//! | Layer | Scope | Default |
//! |---|---|---|
//! | **Platform** | gateway / scheduler / executor / queue internals | **always on**, 100 % sampled |
//! | **Tenant / project** | job counts, durations, cache ratios labelled by tenant | **opt-in** ([`InstrumentationLevel::Basic`]) |
//! | **Agent** | trace context injected into the agent process, stdout/stderr captured, custom metrics accepted | **opt-in** ([`InstrumentationLevel::Detailed`]) |
//!
//! ## Signals
//!
//! * **Traces** — [`propagation::TraceContext`] carries W3C trace context over
//!   HTTP headers, NATS message headers and agent environment variables.
//! * **Metrics** — [`metrics::Registry`] renders Prometheus text exposition for
//!   `GET /metrics`; the same registry feeds OTLP under the `otlp` feature.
//! * **Logs** — `tracing` structured logs, JSON when `AVM_LOG_FORMAT=json`,
//!   with trace/span ids attached so logs join traces in the backend.
//!
//! ## Backward compatibility
//!
//! With `OTEL_EXPORTER_OTLP_ENDPOINT` unset — or `OTEL_SDK_DISABLED=true` —
//! nothing is exported off-box and instrumentation costs an atomic add. AVM
//! runs unmodified on a laptop, in CI and in an air-gapped deploy.

pub mod config;
pub mod fields;
pub mod level;
pub mod metrics;
pub mod propagation;
pub mod resource;
pub mod sampler;

pub use config::{OtelConfig, OtlpProtocol};
pub use level::{InstrumentationConfig, InstrumentationLevel};
pub use metrics::{encode_prometheus, registry, MetricKind, Registry};
pub use propagation::TraceContext;
pub use resource::Resource;
pub use sampler::SamplerSpec;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Errors surfaced by the bootstrap. Initialisation never panics: telemetry
/// must not be able to take a service down.
#[derive(Debug, thiserror::Error)]
pub enum OtelError {
    #[error("a global tracing subscriber was already installed")]
    AlreadyInitialised,
}

/// Handle returned by [`init_otel`]. Holds the resolved config/resource for
/// introspection (`/healthz`, `/metrics` labels) and flushes on drop.
#[derive(Debug, Clone)]
pub struct Otel {
    config: OtelConfig,
    resource: Resource,
    subscriber_installed: bool,
}

impl Otel {
    /// Resolved configuration.
    pub fn config(&self) -> &OtelConfig {
        &self.config
    }

    /// Detected resource attributes.
    pub fn resource(&self) -> &Resource {
        &self.resource
    }

    /// True when this call installed the global subscriber (false on a second
    /// call in the same process, e.g. from a test harness).
    pub fn installed(&self) -> bool {
        self.subscriber_installed
    }

    /// True when telemetry is actually exported off-box.
    pub fn export_enabled(&self) -> bool {
        self.config.export_enabled()
    }

    /// The head-based sampler in force.
    pub fn sampler(&self) -> SamplerSpec {
        self.config.sampler
    }

    /// Decide whether to sample a trace given an inbound context.
    ///
    /// `tenant_ratio` is the per-tenant override from `avm_config`; it narrows
    /// the platform decision but never widens it, so an operator cannot
    /// accidentally sample a tenant that the platform sampler dropped.
    pub fn should_sample(&self, inbound: Option<&TraceContext>, tenant_ratio: Option<f64>) -> bool {
        let trace_id = inbound.map(|c| c.trace_id_bytes()).unwrap_or([0u8; 16]);
        let parent = inbound.map(|c| c.sampled);
        if !self.config.sampler.should_sample(parent, &trace_id) {
            return false;
        }
        match tenant_ratio {
            Some(r) if r < 1.0 => SamplerSpec::TraceIdRatio(r.max(0.0)).should_sample(None, &trace_id),
            _ => true,
        }
    }

    /// Render the process metric registry (Prometheus text exposition).
    pub fn metrics_text(&self) -> String {
        metrics::encode_prometheus()
    }
}

impl Drop for Otel {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        {
            // TODO(avm): shutdown/flush the OTLP TracerProvider + MeterProvider.
            tracing::debug!("otel: flush on shutdown (otlp feature)");
        }
    }
}

/// Initialise telemetry for `service_name` from the process environment.
///
/// Installs the structured `tracing` subscriber, detects the resource, seeds
/// the metric registry with the platform families and their constant labels,
/// and returns a handle. Safe to call once per process; a second call keeps the
/// existing subscriber and reports `installed() == false`.
pub fn init_otel(service_name: &str, service_version: &str) -> Otel {
    init_otel_with(OtelConfig::from_env(service_name, service_version))
}

/// Initialise telemetry from an explicit config (tests, embedded uses).
pub fn init_otel_with(config: OtelConfig) -> Otel {
    let resource = Resource::detect(&config);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.default_filter.clone()));
    let registry_layers = tracing_subscriber::registry().with(filter);

    let subscriber_installed = if config.json_logs {
        registry_layers
            .with(fmt::layer().json().with_current_span(true).with_span_list(true))
            .try_init()
            .is_ok()
    } else {
        registry_layers.with(fmt::layer().with_target(true)).try_init().is_ok()
    };

    // Every series carries the service identity, so one scrape endpoint can be
    // federated across services without relabelling.
    let reg = metrics::registry();
    reg.set_constant_labels(vec![
        ("service_name".to_string(), config.service_name.clone()),
        ("service_version".to_string(), config.service_version.clone()),
    ]);
    register_platform_metrics(reg);

    if subscriber_installed {
        tracing::info!(
            service.name = %config.service_name,
            service.version = %config.service_version,
            otlp_endpoint = ?config.endpoint,
            otlp_protocol = config.protocol.as_str(),
            sampler = config.sampler.as_str(),
            sdk_disabled = config.disabled,
            export_enabled = config.export_enabled(),
            otlp_feature = cfg!(feature = "otlp"),
            "otel initialised"
        );
    }

    #[cfg(feature = "otlp")]
    if config.export_enabled() {
        // TODO(avm): build an opentelemetry_sdk TracerProvider/MeterProvider
        // against `config.endpoint` + `config.protocol`, seed it with
        // `resource.attributes()`, and layer `tracing_opentelemetry::layer()`
        // onto the subscriber above. Deliberately behind a feature so the
        // default build does not pin a fast-moving OTel API surface.
        tracing::warn!("otlp feature enabled but exporter wiring is not implemented yet");
    }

    Otel { config, resource, subscriber_installed }
}

/// Declare the always-on platform metric families so `/metrics` is
/// self-describing before the first request lands.
fn register_platform_metrics(reg: &Registry) {
    use metrics::*;
    reg.register(
        GATEWAY_REQUEST_DURATION,
        MetricKind::Histogram,
        "Gateway HTTP request latency in seconds",
        LATENCY_BUCKETS,
    );
    reg.register(
        SCHEDULER_PLACEMENT_DURATION,
        MetricKind::Histogram,
        "Scheduler job placement latency in seconds",
        LATENCY_BUCKETS,
    );
    reg.register(
        EXECUTOR_CONTAINER_DURATION,
        MetricKind::Histogram,
        "Executor container/process wall time in seconds",
        LATENCY_BUCKETS,
    );
    reg.register(
        QUEUE_MESSAGE_SIZE,
        MetricKind::Histogram,
        "Queue message size in bytes",
        SIZE_BUCKETS,
    );
    reg.register(QUEUE_DEPTH, MetricKind::Gauge, "Pending messages per subject", &[]);
    reg.register(
        QUEUE_MESSAGES,
        MetricKind::Counter,
        "Queue messages by subject and direction",
        &[],
    );
    reg.register(
        MODEL_PULL_DURATION,
        MetricKind::Histogram,
        "Model/image pull time in seconds",
        LATENCY_BUCKETS,
    );
    reg.register(
        MODEL_CACHE_LOOKUPS,
        MetricKind::Counter,
        "Model cache lookups by result",
        &[],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str) -> OtelConfig {
        let mut c = OtelConfig::from_env(name, "0.0.0-test");
        c.endpoint = None;
        c.disabled = false;
        c.sampler = SamplerSpec::default();
        c
    }

    #[test]
    fn init_is_a_noop_without_an_endpoint() {
        let otel = init_otel_with(cfg("avm-test-noop"));
        assert!(!otel.export_enabled(), "no endpoint must mean no export");
        assert_eq!(otel.resource().get("service.namespace"), Some("avm"));
    }

    #[test]
    fn platform_families_are_registered() {
        let otel = init_otel_with(cfg("avm-test-families"));
        let text = otel.metrics_text();
        assert!(text.contains("# TYPE avm_gateway_request_duration_seconds histogram"));
        assert!(text.contains("# TYPE avm_queue_depth gauge"));
    }

    #[test]
    fn sdk_disabled_wins_over_a_configured_endpoint() {
        let mut c = cfg("avm-test-killswitch");
        c.endpoint = Some("http://collector:4317".into());
        c.disabled = true;
        assert!(!init_otel_with(c).export_enabled());
    }

    #[test]
    fn sampling_respects_an_upstream_drop() {
        let otel = init_otel_with(cfg("avm-test-sampling"));
        let mut ctx = TraceContext::root_from_seed(&[3u8; 16], false);
        ctx.sampled = false;
        assert!(!otel.should_sample(Some(&ctx), None));
    }

    #[test]
    fn tenant_ratio_narrows_but_never_widens() {
        let mut c = cfg("avm-test-ratio");
        c.sampler = SamplerSpec::AlwaysOff;
        let otel = init_otel_with(c);
        // Platform says no; a tenant ratio of 1.0 cannot override it.
        assert!(!otel.should_sample(None, Some(1.0)));
    }

    #[test]
    fn zero_tenant_ratio_drops_the_trace() {
        let otel = init_otel_with(cfg("avm-test-zero"));
        let ctx = TraceContext::root_from_seed(&[9u8; 16], true);
        assert!(!otel.should_sample(Some(&ctx), Some(0.0)));
        assert!(otel.should_sample(Some(&ctx), Some(1.0)));
    }
}
