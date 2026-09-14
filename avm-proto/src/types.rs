//! Serde mirrors of the queue envelopes declared in `proto/avm_service.proto`.
//!
//! These are the structs actually placed on NATS JetStream. Keeping them
//! hand-written (a) removes the `protoc` build dependency from the data path
//! and (b) makes the payloads human-readable in `nats stream view`.

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMessage {
    pub job_id: String,
    pub scope: Scope,
    pub agent_id: String,
    /// Opaque JSON task payload handed to the agent process.
    pub payload: String,
    /// RFC-3339 UTC timestamp.
    pub created_at: String,
}

/// Completion envelope published to `avm.results.<tenant>.<project>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMessage {
    pub job_id: String,
    /// "succeeded" | "failed" | "cancelled"
    pub status: String,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub error: String,
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
        };
        let encoded = serde_json::to_vec(&msg).unwrap();
        let decoded: JobMessage = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.job_id, "job_1");
        assert_eq!(decoded.scope.level(), Some(ScopeLevel::Project));
    }
}
