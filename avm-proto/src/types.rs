//! Serde mirrors of the queue envelopes declared in `proto/avm_service.proto`.
//!
//! These are the structs actually placed on NATS JetStream. Keeping them
//! hand-written (a) removes the `protoc` build dependency from the data path
//! and (b) makes the payloads human-readable in `nats stream view`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Scope level in the AVM hierarchy: system → tenant → project → agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeLevel {
    System,
    Tenant,
    Project,
    Agent,
}

impl ScopeLevel {
    /// Scopes a reader at this level is allowed to resolve (self + ancestors).
    pub fn visible(&self) -> &'static [ScopeLevel] {
        use ScopeLevel::*;
        match self {
            System => &[System],
            Tenant => &[System, Tenant],
            Project => &[System, Tenant, Project],
            Agent => &[System, Tenant, Project, Agent],
        }
    }

    /// True when a reader at `self` may read a memory written at `target`.
    pub fn can_read(&self, target: ScopeLevel) -> bool {
        self.visible().contains(&target)
    }

    /// Lowercase wire representation (matches the SQL CHECK constraint).
    pub fn as_str(&self) -> &'static str {
        match self {
            ScopeLevel::System => "system",
            ScopeLevel::Tenant => "tenant",
            ScopeLevel::Project => "project",
            ScopeLevel::Agent => "agent",
        }
    }
}

/// Fully-qualified scope address.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub level: String,
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub agent_id: String,
}

impl Scope {
    pub fn system() -> Self {
        Self {
            level: "system".into(),
            ..Default::default()
        }
    }

    pub fn tenant(tenant_id: impl Into<String>) -> Self {
        Self {
            level: "tenant".into(),
            tenant_id: tenant_id.into(),
            ..Default::default()
        }
    }

    pub fn project(tenant_id: impl Into<String>, project_id: impl Into<String>) -> Self {
        Self {
            level: "project".into(),
            tenant_id: tenant_id.into(),
            project_id: project_id.into(),
            ..Default::default()
        }
    }

    pub fn agent(
        tenant_id: impl Into<String>,
        project_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            level: "agent".into(),
            tenant_id: tenant_id.into(),
            project_id: project_id.into(),
            agent_id: agent_id.into(),
        }
    }

    pub fn level(&self) -> Option<ScopeLevel> {
        match self.level.as_str() {
            "system" => Some(ScopeLevel::System),
            "tenant" => Some(ScopeLevel::Tenant),
            "project" => Some(ScopeLevel::Project),
            "agent" => Some(ScopeLevel::Agent),
            _ => None,
        }
    }
}

/// Work envelope published to `avm.jobs.<tenant>.<project>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobMessage {
    pub job_id: String,
    pub scope: Scope,
    pub agent_id: String,
    /// Opaque JSON task payload handed to the agent process.
    pub payload: String,
    /// RFC-3339 UTC timestamp.
    pub created_at: String,

    // --- observability (see `avm-otel`) -----------------------------------
    /// W3C `traceparent` of the span that dispatched this job.
    ///
    /// Carrying it on the envelope (rather than as a NATS header) keeps the
    /// context intact across JetStream replay and `nats stream view` dumps.
    /// Empty when the dispatching trace was not sampled.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub traceparent: String,

    /// W3C `tracestate`, propagated verbatim when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tracestate: String,

    /// Instrumentation depth requested for this job: `off` | `basic` | `detailed`.
    ///
    /// Resolved at the gateway from the tenant/project instrumentation config.
    /// Empty (or absent, for envelopes written before this field existed) is
    /// read as `off` — platform telemetry only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instrumentation_level: String,
}

impl JobMessage {
    /// Instrumentation depth, defaulting to `off` for legacy envelopes.
    pub fn instrumentation(&self) -> &str {
        if self.instrumentation_level.is_empty() {
            "off"
        } else {
            self.instrumentation_level.as_str()
        }
    }

    /// True when the dispatcher attached a sampled trace context.
    pub fn has_trace(&self) -> bool {
        !self.traceparent.is_empty()
    }
}

/// Completion envelope published to `avm.results.<tenant>.<project>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResultMessage {
    pub job_id: String,
    /// "succeeded" | "failed" | "cancelled"
    pub status: String,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub error: String,

    // --- observability (see `avm-otel`) -----------------------------------
    /// Custom metrics published by the agent, forwarded only when the job's
    /// `instrumentation_level` is `basic` or `detailed`.
    ///
    /// Keys are metric names (sanitised into Prometheus form by the collector
    /// side); values are plain numbers. Deliberately *not* free-form JSON so a
    /// misbehaving agent cannot smuggle payload data out through telemetry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metrics: BTreeMap<String, f64>,

    /// W3C `traceparent` of the span that produced this result, echoed so the
    /// consumer can stitch the completion onto the originating trace.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub traceparent: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_acl_is_inheritance_based() {
        assert!(ScopeLevel::Project.can_read(ScopeLevel::Tenant));
        assert!(ScopeLevel::Project.can_read(ScopeLevel::System));
        assert!(!ScopeLevel::Tenant.can_read(ScopeLevel::Project));
        assert!(!ScopeLevel::System.can_read(ScopeLevel::Agent));
    }

    #[test]
    fn job_message_roundtrips() {
        let msg = JobMessage {
            job_id: "job_1".into(),
            scope: Scope::project("t_acme", "b_payments"),
            agent_id: "ag_pr_reviewer".into(),
            payload: "{\"task\":\"review\"}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        };
        let encoded = serde_json::to_vec(&msg).unwrap();
        let decoded: JobMessage = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.job_id, "job_1");
        assert_eq!(decoded.scope.level(), Some(ScopeLevel::Project));
    }

    #[test]
    fn observability_fields_are_optional_on_the_wire() {
        // An envelope written before the observability fields existed must
        // still decode — and must read as "no tracing, no opt-in".
        let legacy = br#"{
            "job_id": "job_legacy",
            "scope": { "level": "project", "tenant_id": "t", "project_id": "b" },
            "agent_id": "ag",
            "payload": "{}",
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let decoded: JobMessage = serde_json::from_slice(legacy).unwrap();
        assert_eq!(decoded.instrumentation(), "off");
        assert!(!decoded.has_trace());

        // And a default-valued envelope must not emit the empty fields.
        let encoded = serde_json::to_string(&JobMessage {
            job_id: "j".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(!encoded.contains("traceparent"), "{encoded}");
        assert!(!encoded.contains("instrumentation_level"), "{encoded}");
    }

    #[test]
    fn result_metrics_are_omitted_when_empty() {
        let r = ResultMessage {
            job_id: "j".into(),
            status: "succeeded".into(),
            ..Default::default()
        };
        let encoded = serde_json::to_string(&r).unwrap();
        assert!(!encoded.contains("metrics"), "{encoded}");

        let mut with = r.clone();
        with.metrics.insert("tokens_in".into(), 1234.0);
        let encoded = serde_json::to_string(&with).unwrap();
        assert!(encoded.contains("\"tokens_in\":1234.0"), "{encoded}");
    }
}
