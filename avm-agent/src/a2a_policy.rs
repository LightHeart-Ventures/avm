//! A2A trust policy — the declarative half of AVM's agent-to-agent security.
//!
//! Every agent publishes an [`AgentCard`]. The card carries an [`A2APolicy`]
//! that states *who is allowed to send this agent work*. The gateway enforces
//! it on every dispatch (see `avm_gateway::security::validate_a2a_dispatch`).
//!
//! Design rules (these are invariants, not defaults you can talk your way out
//! of):
//!
//! 1. **Tenant boundaries are hard.** No policy value can permit a
//!    cross-tenant dispatch. There is no `allow_cross_tenant` field, on
//!    purpose.
//! 2. **Intra-project A2A is deny-by-default.** Two agents sitting in the same
//!    project are *not* implicitly allowed to call each other; the callee must
//!    opt in with `allow_intra_project = true`.
//! 3. **Cross-project A2A is deny-by-default** and additionally requires the
//!    caller to be named in `trusted_peers` unless the callee's
//!    [`TrustDefault`] is `Allow`.
//! 4. **Self-dispatch is always allowed** (an agent may re-enter itself).

use serde::{Deserialize, Serialize};

/// What an agent does with a caller that is *not* named in `trusted_peers`.
///
/// Serialized as the lowercase strings `"deny"` / `"allow"` so an Agent Card
/// reads naturally in JSON/YAML.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustDefault {
    /// Reject unless the caller is explicitly listed in `trusted_peers`.
    ///
    /// This is the `Default` variant: an Agent Card that omits `a2a_policy`
    /// (or omits `default` within it) is closed, never open.
    #[default]
    Deny,
    /// Accept any caller that already cleared the tenant/project checks.
    Allow,
}

impl TrustDefault {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustDefault::Deny => "deny",
            TrustDefault::Allow => "allow",
        }
    }
}

/// The A2A trust policy an agent advertises on its Agent Card.
///
/// [`A2APolicy::default()`] is the *closed* policy: deny everything that is
/// not this agent calling itself. Anything more permissive is an explicit
/// operator decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct A2APolicy {
    /// Behaviour for callers not present in `trusted_peers`.
    pub default: TrustDefault,
    /// Agent IDs explicitly allowed to dispatch work to us.
    pub trusted_peers: Vec<String>,
    /// Allow peers in the *same* project to dispatch to us.
    pub allow_intra_project: bool,
    /// Allow peers in a *different* project of the *same tenant* to dispatch
    /// to us. Cross-tenant is never permitted regardless of this flag.
    pub allow_cross_project: bool,
}

impl Default for A2APolicy {
    fn default() -> Self {
        Self {
            default: TrustDefault::Deny,
            trusted_peers: Vec::new(),
            allow_intra_project: false,
            allow_cross_project: false,
        }
    }
}

impl A2APolicy {
    /// The closed policy — identical to [`Default`], spelled out for intent.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// True when `agent_id` is named in `trusted_peers`.
    pub fn is_trusted_peer(&self, agent_id: &str) -> bool {
        self.trusted_peers.iter().any(|p| p == agent_id)
    }

    /// Builder helper: add a trusted peer.
    pub fn with_trusted_peer(mut self, agent_id: impl Into<String>) -> Self {
        self.trusted_peers.push(agent_id.into());
        self
    }

    /// Builder helper: opt into intra-project A2A.
    pub fn allowing_intra_project(mut self) -> Self {
        self.allow_intra_project = true;
        self
    }

    /// Builder helper: opt into cross-project (same-tenant) A2A.
    pub fn allowing_cross_project(mut self) -> Self {
        self.allow_cross_project = true;
        self
    }
}

/// The scope triple an A2A dispatch is evaluated against.
///
/// Mirrors `avm.v1.Scope` from `proto/avm_service.proto` minus the `level`
/// discriminator, because an A2A endpoint is always at agent level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScope {
    pub tenant_id: String,
    pub project_id: String,
    pub agent_id: String,
}

impl AgentScope {
    pub fn new(
        tenant_id: impl Into<String>,
        project_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            project_id: project_id.into(),
            agent_id: agent_id.into(),
        }
    }

    pub fn same_tenant(&self, other: &AgentScope) -> bool {
        self.tenant_id == other.tenant_id
    }

    pub fn same_project(&self, other: &AgentScope) -> bool {
        self.same_tenant(other) && self.project_id == other.project_id
    }

    pub fn same_agent(&self, other: &AgentScope) -> bool {
        self.same_project(other) && self.agent_id == other.agent_id
    }
}

/// Minimal Agent Card — the security-relevant subset.
///
/// The A2A + Agent Card spike owns the full card (capabilities, `mcp_servers`,
/// `model_ref`, auth policy, …). This struct is `#[serde(default)]` on the
/// policy field and uses `#[serde(flatten)]`-friendly plain fields so the two
/// definitions merge without a schema break: the spike's card only needs to
/// gain `pub a2a_policy: A2APolicy` and reuse the types above.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    /// Where this agent lives — required for tenant/project boundary checks.
    pub scope: AgentScope,
    /// Who may dispatch A2A work to this agent. Defaults to deny-all.
    #[serde(default)]
    pub a2a_policy: A2APolicy,
}

impl AgentCard {
    /// A card with the closed default policy.
    pub fn new(name: impl Into<String>, scope: AgentScope) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            version: String::new(),
            scope,
            a2a_policy: A2APolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: A2APolicy) -> Self {
        self.a2a_policy = policy;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_closed() {
        let p = A2APolicy::default();
        assert_eq!(p.default, TrustDefault::Deny);
        assert!(p.trusted_peers.is_empty());
        assert!(!p.allow_intra_project);
        assert!(!p.allow_cross_project);
    }

    #[test]
    fn trust_default_serializes_lowercase() {
        let json = serde_json::to_string(&TrustDefault::Deny).unwrap();
        assert_eq!(json, "\"deny\"");
        let json = serde_json::to_string(&TrustDefault::Allow).unwrap();
        assert_eq!(json, "\"allow\"");
    }

    #[test]
    fn card_without_policy_field_deserializes_to_deny_all() {
        let raw = r#"{
            "name": "planner",
            "scope": { "tenant_id": "t1", "project_id": "p1", "agent_id": "a1" }
        }"#;
        let card: AgentCard = serde_json::from_str(raw).unwrap();
        assert_eq!(card.a2a_policy, A2APolicy::deny_all());
    }

    #[test]
    fn policy_roundtrips() {
        let p = A2APolicy::default()
            .with_trusted_peer("a-peer")
            .allowing_intra_project();
        let s = serde_json::to_string(&p).unwrap();
        let back: A2APolicy = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
        assert!(back.is_trusted_peer("a-peer"));
        assert!(!back.is_trusted_peer("someone-else"));
    }

    #[test]
    fn scope_comparisons() {
        let a = AgentScope::new("t1", "p1", "a1");
        let b = AgentScope::new("t1", "p1", "a2");
        let c = AgentScope::new("t1", "p2", "a3");
        let d = AgentScope::new("t2", "p1", "a1");

        assert!(a.same_project(&b));
        assert!(!a.same_agent(&b));
        assert!(a.same_tenant(&c));
        assert!(!a.same_project(&c));
        assert!(!a.same_tenant(&d));
        assert!(a.same_agent(&a));
    }
}
