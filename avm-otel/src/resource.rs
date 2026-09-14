//! OpenTelemetry resource: the immutable identity of *this* process.
//!
//! Attributes follow the OTel semantic conventions so any backend groups AVM
//! services correctly without per-backend mapping rules.

use crate::config::OtelConfig;

/// Resolved resource attribute set for one AVM process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resource {
    attrs: Vec<(String, String)>,
}

impl Resource {
    /// Detect the resource for `cfg`: service identity, host, process, plus
    /// anything supplied via `OTEL_RESOURCE_ATTRIBUTES` (which wins on conflict,
    /// matching the SDK spec's "explicit config overrides detection" rule).
    pub fn detect(cfg: &OtelConfig) -> Self {
        let mut r = Self::default();

        r.set("service.name", &cfg.service_name);
        r.set("service.version", &cfg.service_version);
        r.set("service.namespace", "avm");
        r.set("telemetry.sdk.name", "avm-otel");
        r.set("telemetry.sdk.language", "rust");
        r.set("telemetry.sdk.version", env!("CARGO_PKG_VERSION"));

        if let Some(host) = hostname() {
            r.set("host.name", &host);
        }
        r.set("process.pid", &std::process::id().to_string());
        if let Some(env_name) = std::env::var("AVM_ENV").ok().filter(|s| !s.is_empty()) {
            r.set("deployment.environment", &env_name);
        }

        for (k, v) in &cfg.resource_attributes {
            r.set(k, v);
        }
        r
    }

    /// Insert or replace one attribute.
    pub fn set(&mut self, key: &str, value: &str) {
        match self.attrs.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value.to_string(),
            None => self.attrs.push((key.to_string(), value.to_string())),
        }
    }

    /// Look up one attribute.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// All attributes, sorted by key for stable rendering.
    pub fn attributes(&self) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> =
            self.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        out.sort_unstable_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// Render as the `k=v,k=v` form understood by `OTEL_RESOURCE_ATTRIBUTES`.
    pub fn to_otel_string(&self) -> String {
        self.attributes()
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Render as Prometheus label pairs (`k="v"`), with dots normalised to
    /// underscores so the names are valid Prometheus label names.
    pub fn to_prom_labels(&self) -> Vec<(String, String)> {
        self.attributes()
            .into_iter()
            .map(|(k, v)| (crate::metrics::sanitize_name(k), v.to_string()))
            .collect()
    }
}

/// Best-effort hostname without pulling in a C-binding crate: prefer the
/// container/k8s-friendly `HOSTNAME` env var, fall back to `/etc/hostname`.
fn hostname() -> Option<String> {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.trim().is_empty() {
            return Some(h.trim().to_string());
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OtelConfig {
        let mut c = OtelConfig::from_env("avm-test", "9.9.9");
        c.service_name = "avm-test".into();
        c.service_version = "9.9.9".into();
        c.resource_attributes = vec![("deployment.environment".into(), "ci".into())];
        c
    }

    #[test]
    fn detects_service_identity() {
        let r = Resource::detect(&cfg());
        assert_eq!(r.get("service.name"), Some("avm-test"));
        assert_eq!(r.get("service.version"), Some("9.9.9"));
        assert_eq!(r.get("service.namespace"), Some("avm"));
        assert_eq!(r.get("telemetry.sdk.language"), Some("rust"));
        assert!(r.get("process.pid").is_some());
    }

    #[test]
    fn explicit_attributes_override_detection() {
        let r = Resource::detect(&cfg());
        assert_eq!(r.get("deployment.environment"), Some("ci"));
    }

    #[test]
    fn renders_stable_sorted_otel_string() {
        let mut r = Resource::default();
        r.set("z.key", "1");
        r.set("a.key", "2");
        assert_eq!(r.to_otel_string(), "a.key=2,z.key=1");
    }

    #[test]
    fn prom_labels_sanitize_dots() {
        let mut r = Resource::default();
        r.set("service.name", "avm-gateway");
        assert_eq!(
            r.to_prom_labels(),
            vec![("service_name".to_string(), "avm-gateway".to_string())]
        );
    }
}
