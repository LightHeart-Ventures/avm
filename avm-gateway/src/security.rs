//! A2A dispatch authorization — the gateway layer of AVM's defence-in-depth.
//!
//! This is **Phase 1** of the network-isolation rollout described in
//! `IMPLEMENTATION_PLAN.md` § *Network Isolation & A2A Security*. It blocks a
//! forbidden agent-to-agent dispatch at the API boundary, before any job is
//! published to NATS. The other three layers (NATS subject ACLs, Kubernetes
//! NetworkPolicies, Postgres RLS) are independent and land in Phases 2–4;
//! none of them is a substitute for this one.
//!
//! # Decision order
//!
//! [`validate_a2a_dispatch`] evaluates in this fixed order and returns on the
//! first failure. Order matters: the hardest boundary is checked first so a
//! cross-tenant attempt can never be masked by a permissive peer list.
//!
//! | # | Check                | Failure                                  |
//! |---|----------------------|------------------------------------------|
//! | 0 | self-dispatch        | *always allowed, short-circuits*         |
//! | 1 | tenant match         | [`ScopeError::CrossTenantDenied`]        |
//! | 2 | cross-project opt-in | [`ScopeError::CrossProjectDenied`]       |
//! | 3 | intra-project opt-in | [`ScopeError::IntraProjectDenied`]       |
//! | 4 | peer trust           | [`AuthError::PeerNotTrusted`]            |
//!
//! Every call — pass **or** fail — emits an audit record via
//! [`SecurityAudit`]. There is no silent path.

use avm_agent::{A2APolicy, AgentCard, AgentScope, TrustDefault};
use serde::{Deserialize, Serialize};

/// A request to dispatch an A2A task from `source` to `target`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A2ADispatch {
    /// Correlation id for the task being dispatched.
    pub task_id: String,
    /// Scope of the agent making the call (authenticated, not self-asserted).
    pub source: AgentScope,
    /// Scope of the agent being called.
    pub target: AgentScope,
}

impl A2ADispatch {
    pub fn new(task_id: impl Into<String>, source: AgentScope, target: AgentScope) -> Self {
        Self {
            task_id: task_id.into(),
            source,
            target,
        }
    }
}

/// A violation of the tenant/project scope boundary.
///
/// Maps to `avm.v1.ScopeError` in `proto/avm_service.proto` and to HTTP 403.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "code")]
pub enum ScopeError {
    /// Cross-tenant dispatch. Unconditional — no policy can permit this.
    #[error("cross-tenant A2A dispatch denied: {source_tenant} -> {target_tenant}")]
    CrossTenantDenied {
        source_tenant: String,
        target_tenant: String,
    },

    /// Same tenant, different project, and the callee has not set
    /// `allow_cross_project`.
    #[error("cross-project A2A dispatch denied: {source_project} -> {target_project} (target has allow_cross_project=false)")]
    CrossProjectDenied {
        source_project: String,
        target_project: String,
    },

    /// Same project, and the callee has not set `allow_intra_project`.
    /// This is the deny-by-default case the design calls for.
    #[error("intra-project A2A dispatch denied: {project} (target has allow_intra_project=false; intra-project A2A is deny-by-default)")]
    IntraProjectDenied { project: String },
}

/// A violation of the callee's peer-trust policy.
///
/// Maps to `avm.v1.AuthError` in `proto/avm_service.proto` and to HTTP 403.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "code")]
pub enum AuthError {
    /// Scope checks passed but the caller is not in `trusted_peers` and the
    /// callee's default is `deny`.
    #[error("agent {source_agent} is not a trusted peer of {target_agent}")]
    PeerNotTrusted {
        source_agent: String,
        target_agent: String,
    },

    /// The dispatch names a target the card does not describe. Guards against
    /// a card/route mismatch being treated as an implicit allow.
    #[error("agent card {card_agent} does not match dispatch target {target_agent}")]
    CardTargetMismatch {
        card_agent: String,
        target_agent: String,
    },
}

/// Either kind of denial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "kind", content = "error")]
pub enum SecurityError {
    #[error(transparent)]
    Scope(#[from] ScopeError),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl SecurityError {
    /// Short stable reason code for audit logs and metrics labels.
    pub fn reason_code(&self) -> &'static str {
        match self {
            SecurityError::Scope(ScopeError::CrossTenantDenied { .. }) => "cross_tenant_denied",
            SecurityError::Scope(ScopeError::CrossProjectDenied { .. }) => "cross_project_denied",
            SecurityError::Scope(ScopeError::IntraProjectDenied { .. }) => "intra_project_denied",
            SecurityError::Auth(AuthError::PeerNotTrusted { .. }) => "peer_not_trusted",
            SecurityError::Auth(AuthError::CardTargetMismatch { .. }) => "card_target_mismatch",
        }
    }

    /// HTTP status this denial maps to. Always 403 — we do not leak whether
    /// the target agent exists.
    pub fn http_status(&self) -> u16 {
        403
    }
}

/// Why a dispatch was allowed. Recorded so an audit reader can tell an
/// explicit grant from a wide-open policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowReason {
    /// Agent dispatching to itself.
    SelfDispatch,
    /// Caller named in the target's `trusted_peers`.
    TrustedPeer,
    /// Target's `default` is `allow` and the scope gates passed.
    TrustDefaultAllow,
}

/// The outcome of one authorization check. Always produced, always audited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityEvent {
    pub task_id: String,
    pub source_tenant: String,
    pub source_project: String,
    pub source_agent: String,
    pub target_tenant: String,
    pub target_project: String,
    pub target_agent: String,
    /// `true` when the dispatch was permitted.
    pub allowed: bool,
    /// Stable code: an [`AllowReason`] or a [`SecurityError::reason_code`].
    pub reason: String,
    /// Human-readable detail (the `Display` of the error, empty on allow).
    #[serde(default)]
    pub detail: String,
}

impl SecurityEvent {
    fn base(dispatch: &A2ADispatch) -> Self {
        Self {
            task_id: dispatch.task_id.clone(),
            source_tenant: dispatch.source.tenant_id.clone(),
            source_project: dispatch.source.project_id.clone(),
            source_agent: dispatch.source.agent_id.clone(),
            target_tenant: dispatch.target.tenant_id.clone(),
            target_project: dispatch.target.project_id.clone(),
            target_agent: dispatch.target.agent_id.clone(),
            allowed: false,
            reason: String::new(),
            detail: String::new(),
        }
    }

    fn allow(dispatch: &A2ADispatch, reason: AllowReason) -> Self {
        let reason = match reason {
            AllowReason::SelfDispatch => "self_dispatch",
            AllowReason::TrustedPeer => "trusted_peer",
            AllowReason::TrustDefaultAllow => "trust_default_allow",
        };
        Self {
            allowed: true,
            reason: reason.to_string(),
            ..Self::base(dispatch)
        }
    }

    fn deny(dispatch: &A2ADispatch, err: &SecurityError) -> Self {
        Self {
            allowed: false,
            reason: err.reason_code().to_string(),
            detail: err.to_string(),
            ..Self::base(dispatch)
        }
    }
}

/// Sink for [`SecurityEvent`]s.
///
/// The default [`TracingAudit`] writes a structured `tracing` event, which the
/// OTel pipeline in `avm-observability` exports. A production deployment
/// should also persist to the `audit_logs` table (migration `003`).
pub trait SecurityAudit: Send + Sync {
    fn record(&self, event: &SecurityEvent);
}

/// Audit sink that emits a structured `tracing` event per decision.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingAudit;

impl SecurityAudit for TracingAudit {
    fn record(&self, event: &SecurityEvent) {
        if event.allowed {
            tracing::info!(
                target: "avm.security.a2a",
                task_id = %event.task_id,
                source_tenant = %event.source_tenant,
                source_project = %event.source_project,
                source_agent = %event.source_agent,
                target_tenant = %event.target_tenant,
                target_project = %event.target_project,
                target_agent = %event.target_agent,
                allowed = true,
                reason = %event.reason,
                "a2a dispatch allowed"
            );
        } else {
            tracing::warn!(
                target: "avm.security.a2a",
                task_id = %event.task_id,
                source_tenant = %event.source_tenant,
                source_project = %event.source_project,
                source_agent = %event.source_agent,
                target_tenant = %event.target_tenant,
                target_project = %event.target_project,
                target_agent = %event.target_agent,
                allowed = false,
                reason = %event.reason,
                detail = %event.detail,
                "a2a dispatch DENIED"
            );
        }
    }
}

/// Audit sink that collects events in memory. Test/diagnostic use.
#[derive(Debug, Default)]
pub struct CollectingAudit {
    events: std::sync::Mutex<Vec<SecurityEvent>>,
}

impl CollectingAudit {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn events(&self) -> Vec<SecurityEvent> {
        self.events.lock().expect("audit mutex poisoned").clone()
    }
    pub fn len(&self) -> usize {
        self.events.lock().expect("audit mutex poisoned").len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecurityAudit for CollectingAudit {
    fn record(&self, event: &SecurityEvent) {
        self.events
            .lock()
            .expect("audit mutex poisoned")
            .push(event.clone());
    }
}

/// Authorize an A2A dispatch against the target agent's card.
///
/// Returns the [`SecurityEvent`] that was audited on success, or the
/// [`SecurityError`] on denial. The event is recorded through `audit` in
/// **both** cases before this function returns.
///
/// See the module docs for the decision order.
pub fn validate_a2a_dispatch(
    dispatch: &A2ADispatch,
    target_card: &AgentCard,
    audit: &dyn SecurityAudit,
) -> Result<SecurityEvent, SecurityError> {
    match evaluate(dispatch, target_card) {
        Ok(reason) => {
            let event = SecurityEvent::allow(dispatch, reason);
            audit.record(&event);
            Ok(event)
        }
        Err(err) => {
            let event = SecurityEvent::deny(dispatch, &err);
            audit.record(&event);
            Err(err)
        }
    }
}

/// Pure decision function — no side effects, no audit. Exposed for callers
/// that need to pre-flight a policy (e.g. an admission dry-run).
pub fn evaluate(
    dispatch: &A2ADispatch,
    target_card: &AgentCard,
) -> Result<AllowReason, SecurityError> {
    let src = &dispatch.source;
    let tgt = &dispatch.target;
    let policy: &A2APolicy = &target_card.a2a_policy;

    // (0) The card must actually describe the target we are routing to.
    //     Checked before self-dispatch so a mismatched card cannot be used to
    //     manufacture a self-dispatch short-circuit.
    if target_card.scope != *tgt {
        return Err(AuthError::CardTargetMismatch {
            card_agent: target_card.scope.agent_id.clone(),
            target_agent: tgt.agent_id.clone(),
        }
        .into());
    }

    // (0b) An agent may always re-enter itself.
    if src.same_agent(tgt) {
        return Ok(AllowReason::SelfDispatch);
    }

    // (1) Tenant boundary — hard, unconditional, not policy-controllable.
    if !src.same_tenant(tgt) {
        return Err(ScopeError::CrossTenantDenied {
            source_tenant: src.tenant_id.clone(),
            target_tenant: tgt.tenant_id.clone(),
        }
        .into());
    }

    // (2)/(3) Project boundary — deny-by-default in both directions.
    if src.project_id != tgt.project_id {
        if !policy.allow_cross_project {
            return Err(ScopeError::CrossProjectDenied {
                source_project: src.project_id.clone(),
                target_project: tgt.project_id.clone(),
            }
            .into());
        }
    } else if !policy.allow_intra_project {
        return Err(ScopeError::IntraProjectDenied {
            project: tgt.project_id.clone(),
        }
        .into());
    }

    // (4) Peer trust.
    if policy.is_trusted_peer(&src.agent_id) {
        return Ok(AllowReason::TrustedPeer);
    }
    match policy.default {
        TrustDefault::Allow => Ok(AllowReason::TrustDefaultAllow),
        TrustDefault::Deny => Err(AuthError::PeerNotTrusted {
            source_agent: src.agent_id.clone(),
            target_agent: tgt.agent_id.clone(),
        }
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(scope: AgentScope, policy: A2APolicy) -> AgentCard {
        AgentCard::new("target", scope).with_policy(policy)
    }

    fn dispatch(src: AgentScope, tgt: AgentScope) -> A2ADispatch {
        A2ADispatch::new("task-1", src, tgt)
    }

    // ---- rule 1: tenant boundary is hard -----------------------------------

    #[test]
    fn cross_tenant_is_denied_even_with_a_fully_open_policy() {
        let tgt = AgentScope::new("t2", "p1", "callee");
        let open = A2APolicy {
            default: TrustDefault::Allow,
            trusted_peers: vec!["caller".into()],
            allow_intra_project: true,
            allow_cross_project: true,
        };
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let err = validate_a2a_dispatch(&d, &card(tgt, open), &audit).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::Scope(ScopeError::CrossTenantDenied { .. })
        ));
        assert_eq!(err.reason_code(), "cross_tenant_denied");
        assert_eq!(audit.len(), 1);
        assert!(!audit.events()[0].allowed);
    }

    // ---- rule 2: intra-project is deny-by-default --------------------------

    #[test]
    fn intra_project_denied_by_default() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let err = validate_a2a_dispatch(&d, &card(tgt, A2APolicy::default()), &audit).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::Scope(ScopeError::IntraProjectDenied { .. })
        ));
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn intra_project_allowed_when_opted_in_and_peer_trusted() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let policy = A2APolicy::default()
            .allowing_intra_project()
            .with_trusted_peer("caller");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let ev = validate_a2a_dispatch(&d, &card(tgt, policy), &audit).unwrap();
        assert!(ev.allowed);
        assert_eq!(ev.reason, "trusted_peer");
        assert_eq!(audit.len(), 1);
    }

    #[test]
    fn intra_project_opt_in_alone_is_not_enough_when_default_is_deny() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let policy = A2APolicy::default().allowing_intra_project();
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let err = validate_a2a_dispatch(&d, &card(tgt, policy), &audit).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::Auth(AuthError::PeerNotTrusted { .. })
        ));
    }

    #[test]
    fn intra_project_allowed_when_default_is_allow() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let policy = A2APolicy {
            default: TrustDefault::Allow,
            allow_intra_project: true,
            ..A2APolicy::default()
        };
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let ev = validate_a2a_dispatch(&d, &card(tgt, policy), &audit).unwrap();
        assert_eq!(ev.reason, "trust_default_allow");
    }

    // ---- rule 3: cross-project ---------------------------------------------

    #[test]
    fn cross_project_denied_by_default() {
        let tgt = AgentScope::new("t1", "p2", "callee");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let err = validate_a2a_dispatch(&d, &card(tgt, A2APolicy::default()), &audit).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::Scope(ScopeError::CrossProjectDenied { .. })
        ));
    }

    #[test]
    fn cross_project_allowed_when_opted_in_and_peer_trusted() {
        let tgt = AgentScope::new("t1", "p2", "callee");
        let policy = A2APolicy::default()
            .allowing_cross_project()
            .with_trusted_peer("caller");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let ev = validate_a2a_dispatch(&d, &card(tgt, policy), &audit).unwrap();
        assert!(ev.allowed);
    }

    #[test]
    fn intra_project_opt_in_does_not_leak_into_cross_project() {
        let tgt = AgentScope::new("t1", "p2", "callee");
        let policy = A2APolicy::default()
            .allowing_intra_project()
            .with_trusted_peer("caller");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let err = validate_a2a_dispatch(&d, &card(tgt, policy), &audit).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::Scope(ScopeError::CrossProjectDenied { .. })
        ));
    }

    // ---- rule 4: peer trust -------------------------------------------------

    #[test]
    fn untrusted_peer_rejected_with_auth_error() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let policy = A2APolicy::default()
            .allowing_intra_project()
            .with_trusted_peer("someone");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();

        let err = validate_a2a_dispatch(&d, &card(tgt, policy), &audit).unwrap_err();
        assert_eq!(err.reason_code(), "peer_not_trusted");
        assert_eq!(err.http_status(), 403);
    }

    // ---- rule 0: self-dispatch / card integrity ------------------------------

    #[test]
    fn self_dispatch_always_allowed() {
        let s = AgentScope::new("t1", "p1", "a1");
        let d = dispatch(s.clone(), s.clone());
        let audit = CollectingAudit::new();

        let ev = validate_a2a_dispatch(&d, &card(s, A2APolicy::deny_all()), &audit).unwrap();
        assert_eq!(ev.reason, "self_dispatch");
    }

    #[test]
    fn card_target_mismatch_is_rejected() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let wrong = AgentScope::new("t1", "p1", "somebody-else");
        let policy = A2APolicy {
            default: TrustDefault::Allow,
            allow_intra_project: true,
            ..A2APolicy::default()
        };
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt);
        let audit = CollectingAudit::new();

        let err = validate_a2a_dispatch(&d, &card(wrong, policy), &audit).unwrap_err();
        assert!(matches!(
            err,
            SecurityError::Auth(AuthError::CardTargetMismatch { .. })
        ));
    }

    // ---- auditing ------------------------------------------------------------

    #[test]
    fn every_check_is_audited_exactly_once() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let audit = CollectingAudit::new();

        // allow
        let policy = A2APolicy::default()
            .allowing_intra_project()
            .with_trusted_peer("caller");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        validate_a2a_dispatch(&d, &card(tgt.clone(), policy), &audit).unwrap();
        // deny
        let d2 = dispatch(AgentScope::new("t9", "p1", "caller"), tgt.clone());
        validate_a2a_dispatch(&d2, &card(tgt, A2APolicy::default()), &audit).unwrap_err();

        let events = audit.events();
        assert_eq!(events.len(), 2);
        assert!(events[0].allowed);
        assert!(!events[1].allowed);
        assert_eq!(events[1].reason, "cross_tenant_denied");
        assert!(!events[1].detail.is_empty());
    }

    #[test]
    fn security_event_serializes() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();
        let _ = validate_a2a_dispatch(&d, &card(tgt, A2APolicy::default()), &audit);
        let json = serde_json::to_string(&audit.events()[0]).unwrap();
        assert!(json.contains("\"allowed\":false"));
        assert!(json.contains("intra_project_denied"));
    }

    #[test]
    fn evaluate_is_side_effect_free() {
        let tgt = AgentScope::new("t1", "p1", "callee");
        let d = dispatch(AgentScope::new("t1", "p1", "caller"), tgt.clone());
        let audit = CollectingAudit::new();
        let _ = evaluate(&d, &card(tgt, A2APolicy::default()));
        assert!(audit.is_empty());
    }
}
