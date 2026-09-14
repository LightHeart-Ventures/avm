//! Hierarchical instrumentation scoping.
//!
//! AVM separates **platform** telemetry (always on, operator-owned) from
//! **tenant / project / agent** telemetry (opt-in, customer-scoped):
//!
//! * `Off` — platform spans and platform metrics only. No tenant-labelled
//!   series are emitted at all, so a quiet tenant costs nothing and leaks
//!   nothing.
//! * `Basic` — tenant-labelled counters and durations; job-level spans carry
//!   `tenant_id` / `project_id`.
//! * `Detailed` — everything in `Basic`, plus trace-context injection into the
//!   agent process, agent stdout/stderr captured as correlated log events, and
//!   agent-published custom metrics accepted from the result envelope.
//!
//! The level travels on the job envelope so every hop (gateway → scheduler →
//! executor → agent) reaches the same verdict without another config lookup.

use serde::{Deserialize, Serialize};

/// Per-tenant / per-project instrumentation level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentationLevel {
    /// Platform-only telemetry. The default for every tenant.
    #[default]
    Off,
    /// Tenant-scoped metrics and job spans.
    Basic,
    /// Basic plus agent-level trace propagation, log capture, custom metrics.
    Detailed,
}

impl InstrumentationLevel {
    /// Parse the wire form; unknown values degrade to [`Self::Off`] so a typo
    /// in config can never silently *enable* customer-scoped telemetry.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "basic" => Self::Basic,
            "detailed" => Self::Detailed,
            _ => Self::Off,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Detailed => "detailed",
        }
    }

    /// True when tenant/project-labelled metric series may be recorded.
    pub fn tenant_metrics(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// True when trace context should be injected into the agent process and
    /// agent output captured as correlated logs.
    pub fn agent_tracing(&self) -> bool {
        matches!(self, Self::Detailed)
    }

    /// Kubernetes / pod label applied by the scheduler when detailed tracing is
    /// on, so collectors can select instrumented workloads.
    pub fn pod_label(&self) -> Option<(&'static str, &'static str)> {
        self.agent_tracing().then_some(("otel.io/instrumentation", "detailed"))
    }
}

impl std::fmt::Display for InstrumentationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-tenant (optionally per-project) instrumentation settings, as stored in
/// the `avm_config` table and served by `GET/POST /config/instrumentation`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentationConfig {
    pub tenant_id: String,
    /// Empty means "applies to every project in the tenant".
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub level: InstrumentationLevel,
    /// Head-based sampling ratio applied to this tenant's traces, `0.0..=1.0`.
    #[serde(default = "default_ratio")]
    pub sample_ratio: f64,
}

fn default_ratio() -> f64 {
    1.0
}

impl InstrumentationConfig {
    /// The safe default for an unconfigured tenant: platform telemetry only.
    pub fn off(tenant_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            project_id: String::new(),
            level: InstrumentationLevel::Off,
            sample_ratio: 1.0,
        }
    }

    /// True when this row governs `(tenant, project)`.
    pub fn matches(&self, tenant_id: &str, project_id: &str) -> bool {
        self.tenant_id == tenant_id
            && (self.project_id.is_empty() || self.project_id == project_id)
    }

    /// Clamp user-supplied input into a legal range.
    pub fn normalized(mut self) -> Self {
        if !self.sample_ratio.is_finite() {
            self.sample_ratio = 1.0;
        }
        self.sample_ratio = self.sample_ratio.clamp(0.0, 1.0);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_and_unknown_input_fails_closed() {
        assert_eq!(InstrumentationLevel::default(), InstrumentationLevel::Off);
        assert_eq!(InstrumentationLevel::parse("DETAILED"), InstrumentationLevel::Detailed);
        assert_eq!(InstrumentationLevel::parse("verbose"), InstrumentationLevel::Off);
    }

    #[test]
    fn off_emits_no_tenant_series() {
        assert!(!InstrumentationLevel::Off.tenant_metrics());
        assert!(InstrumentationLevel::Basic.tenant_metrics());
        assert!(InstrumentationLevel::Detailed.tenant_metrics());
    }

    #[test]
    fn only_detailed_propagates_into_the_agent() {
        assert!(!InstrumentationLevel::Basic.agent_tracing());
        assert!(InstrumentationLevel::Detailed.agent_tracing());
        assert_eq!(
            InstrumentationLevel::Detailed.pod_label(),
            Some(("otel.io/instrumentation", "detailed"))
        );
        assert_eq!(InstrumentationLevel::Basic.pod_label(), None);
    }

    #[test]
    fn tenant_wide_row_matches_every_project() {
        let cfg = InstrumentationConfig::off("t_acme");
        assert!(cfg.matches("t_acme", "b_anything"));
        assert!(!cfg.matches("t_other", "b_anything"));
    }

    #[test]
    fn project_row_is_narrower() {
        let mut cfg = InstrumentationConfig::off("t_acme");
        cfg.project_id = "b_payments".into();
        assert!(cfg.matches("t_acme", "b_payments"));
        assert!(!cfg.matches("t_acme", "b_billing"));
    }

    #[test]
    fn ratio_is_clamped() {
        let mut cfg = InstrumentationConfig::off("t_acme");
        cfg.sample_ratio = 7.5;
        assert_eq!(cfg.normalized().sample_ratio, 1.0);
    }

    #[test]
    fn level_roundtrips_through_json() {
        let json = serde_json::to_string(&InstrumentationLevel::Detailed).unwrap();
        assert_eq!(json, "\"detailed\"");
        let back: InstrumentationLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, InstrumentationLevel::Detailed);
    }
}
