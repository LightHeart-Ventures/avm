//! Environment-driven OpenTelemetry configuration.
//!
//! Every knob is a standard OTel environment variable so AVM stays backend
//! agnostic (Grafana Cloud, Honeycomb, Datadog, a local collector, ...):
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | collector/backend base URL | unset → export is a no-op |
//! | `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` \| `http/protobuf` | `grpc` |
//! | `OTEL_SDK_DISABLED` | kill switch | `false` |
//! | `OTEL_TRACES_SAMPLER` | head-based sampler name | `parentbased_always_on` |
//! | `OTEL_TRACES_SAMPLER_ARG` | sampler argument (ratio) | `1.0` |
//! | `OTEL_SERVICE_NAME` | overrides the compiled-in service name | crate name |
//! | `OTEL_RESOURCE_ATTRIBUTES` | `k=v,k=v` resource labels | empty |
//! | `AVM_LOG_FORMAT` | `json` for NDJSON logs | pretty text |
//! | `AVM_METRICS_ADDR` | `/metrics` listen addr for scrapers | unset |

use crate::sampler::SamplerSpec;

/// Wire protocol used to reach the OTLP endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    Grpc,
    HttpProtobuf,
}

impl OtlpProtocol {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "http/protobuf" | "http" | "httpprotobuf" => Self::HttpProtobuf,
            _ => Self::Grpc,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Grpc => "grpc",
            Self::HttpProtobuf => "http/protobuf",
        }
    }
}

/// Fully-resolved telemetry configuration for one process.
#[derive(Debug, Clone)]
pub struct OtelConfig {
    /// `service.name` resource attribute.
    pub service_name: String,
    /// `service.version` resource attribute (normally the crate version).
    pub service_version: String,
    /// OTLP endpoint; `None` means "export is a no-op".
    pub endpoint: Option<String>,
    /// OTLP wire protocol.
    pub protocol: OtlpProtocol,
    /// Hard kill switch (`OTEL_SDK_DISABLED=true`).
    pub disabled: bool,
    /// Head-based sampler applied at ingress.
    pub sampler: SamplerSpec,
    /// Default `RUST_LOG` directives when the env var is unset.
    pub default_filter: String,
    /// Emit newline-delimited JSON logs instead of pretty text.
    pub json_logs: bool,
    /// Extra `OTEL_RESOURCE_ATTRIBUTES` key/value pairs.
    pub resource_attributes: Vec<(String, String)>,
    /// Optional listen address for the Prometheus `/metrics` endpoint.
    pub metrics_addr: Option<String>,
}

impl OtelConfig {
    /// Build a config for `service_name`, reading every knob from the process
    /// environment. `service_version` should normally be `env!("CARGO_PKG_VERSION")`.
    pub fn from_env(service_name: impl Into<String>, service_version: impl Into<String>) -> Self {
        let service_name = env("OTEL_SERVICE_NAME").unwrap_or_else(|| service_name.into());

        Self {
            service_name,
            service_version: service_version.into(),
            endpoint: env("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|s| !s.trim().is_empty()),
            protocol: env("OTEL_EXPORTER_OTLP_PROTOCOL")
                .map(|s| OtlpProtocol::parse(&s))
                .unwrap_or(OtlpProtocol::Grpc),
            disabled: env("OTEL_SDK_DISABLED")
                .map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(false),
            sampler: SamplerSpec::from_env(),
            default_filter: env("AVM_LOG_DEFAULT_FILTER")
                .unwrap_or_else(|| "info,avm=debug".to_string()),
            json_logs: env("AVM_LOG_FORMAT").as_deref() == Some("json"),
            resource_attributes: parse_resource_attributes(
                env("OTEL_RESOURCE_ATTRIBUTES").unwrap_or_default().as_str(),
            ),
            metrics_addr: env("AVM_METRICS_ADDR").filter(|s| !s.trim().is_empty()),
        }
    }

    /// True when telemetry should actually be exported off-box.
    ///
    /// A missing endpoint is *not* an error: the SDK degrades to a no-op so a
    /// developer laptop and an air-gapped deploy both run unmodified.
    pub fn export_enabled(&self) -> bool {
        !self.disabled && self.endpoint.is_some()
    }
}

/// Parse the W3C-Baggage-style `k1=v1,k2=v2` form used by
/// `OTEL_RESOURCE_ATTRIBUTES`. Malformed pairs are skipped, never fatal.
pub fn parse_resource_attributes(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() || v.is_empty() {
                return None;
            }
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_attributes_parse_and_skip_garbage() {
        let attrs = parse_resource_attributes("env=prod, region=us-east-2 ,broken,=x,y=");
        assert_eq!(
            attrs,
            vec![
                ("env".to_string(), "prod".to_string()),
                ("region".to_string(), "us-east-2".to_string()),
            ]
        );
    }

    #[test]
    fn empty_resource_attributes_yield_nothing() {
        assert!(parse_resource_attributes("").is_empty());
    }

    #[test]
    fn protocol_defaults_to_grpc() {
        assert_eq!(OtlpProtocol::parse("nonsense"), OtlpProtocol::Grpc);
        assert_eq!(OtlpProtocol::parse("http/protobuf"), OtlpProtocol::HttpProtobuf);
    }
}
