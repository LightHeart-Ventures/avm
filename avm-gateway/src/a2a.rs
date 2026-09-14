//! `POST /a2a/task` — the authorized entrypoint for agent-to-agent dispatch.
//!
//! This is the **wiring** half of Phase 1 in `IMPLEMENTATION_PLAN.md`
//! § *Network Isolation & A2A Security*. [`crate::security`] holds the decision
//! logic; this module is the HTTP boundary that guarantees the decision is
//! actually taken. The invariant is narrow and load-bearing:
//!
//! > No A2A task is enqueued before [`validate_a2a_dispatch`] returns `Ok`.
//!
//! A denial never reaches the dispatch path, always returns `403`, and always
//! leaves an audit record behind.
//!
//! # Threat note on `source`
//!
//! The source scope must be the **authenticated** caller identity. Accepting it
//! from the request body would make every check self-asserted and therefore
//! worthless — a caller could simply claim the target's own scope and take the
//! self-dispatch short-circuit. See [`A2ATaskRequest::source`] for the
//! follow-on work that binds it to the agent token.

use std::collections::HashMap;
use std::sync::Arc;

use avm_agent::{AgentCard, AgentScope};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::security::{
    validate_a2a_dispatch, A2ADispatch, SecurityAudit, SecurityError, SecurityEvent, TracingAudit,
};

/// Resolves the Agent Card of a dispatch target.
///
/// The card carries the `a2a_policy` the gateway enforces, so this lookup is on
/// the critical path of every A2A call. An unresolvable target is a **denial**,
/// never an implicit allow — see [`A2AState::authorize`].
pub trait CardResolver: Send + Sync {
    fn resolve(&self, scope: &AgentScope) -> Option<AgentCard>;
}

/// In-memory card registry, keyed by the full scope triple.
///
/// Sufficient for the spike and for tests. Production resolves against the
/// `agent_cards` table in `avm-storage`, which is itself RLS-protected in
/// Phase 4 so a compromised gateway cannot read another tenant's cards.
#[derive(Debug, Default, Clone)]
pub struct StaticCardRegistry {
    cards: HashMap<String, AgentCard>,
}

impl StaticCardRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a card under its own scope.
    pub fn insert(&mut self, card: AgentCard) {
        self.cards.insert(Self::key(&card.scope), card);
    }

    /// Builder form of [`Self::insert`].
    pub fn with_card(mut self, card: AgentCard) -> Self {
        self.insert(card);
        self
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// Scope triple → registry key. Tenant first so the key space cannot
    /// collide across tenants that reuse a project or agent name.
    fn key(scope: &AgentScope) -> String {
        format!(
            "{}/{}/{}",
            scope.tenant_id, scope.project_id, scope.agent_id
        )
    }
}

impl CardResolver for StaticCardRegistry {
    fn resolve(&self, scope: &AgentScope) -> Option<AgentCard> {
        self.cards.get(&Self::key(scope)).cloned()
    }
}

/// Inbound A2A dispatch request.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct A2ATaskRequest {
    /// Correlation id, echoed on the response and into the audit record.
    pub task_id: String,
    /// Scope of the calling agent.
    ///
    /// TODO(avm, Phase 1 follow-up): derive this from the authenticated agent
    /// token / mTLS peer identity instead of the body. Until then this endpoint
    /// is only safe behind the executor, which injects the scope it spawned the
    /// agent with (the same trust model as [`crate::ToolCall::scope`]).
    pub source: AgentScope,
    /// Scope of the agent being called.
    pub target: AgentScope,
    /// Opaque task payload, handed to the target agent unmodified.
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Response for an authorized dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2ATaskAccepted {
    pub task_id: String,
    /// Always `"accepted"`.
    pub status: String,
    /// Which rule permitted the call (`trusted_peer`, `self_dispatch`, ...).
    /// Surfaced so a caller can tell an explicit grant from an open policy.
    pub authorized_by: String,
}

/// Response body for a denied dispatch. Mirrors `avm.v1.A2AError`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2AErrorBody {
    /// The tagged `ScopeError` / `AuthError` union.
    pub error: SecurityError,
    /// Stable machine-readable code for clients and metrics.
    pub reason_code: String,
    /// Human-readable detail.
    pub message: String,
}

/// A denial, rendered as `403` with an [`A2AErrorBody`].
#[derive(Debug)]
pub struct A2ARejection(pub SecurityError);

impl From<SecurityError> for A2ARejection {
    fn from(err: SecurityError) -> Self {
        Self(err)
    }
}

impl IntoResponse for A2ARejection {
    fn into_response(self) -> Response {
        let body = A2AErrorBody {
            reason_code: self.0.reason_code().to_string(),
            message: self.0.to_string(),
            error: self.0,
        };
        // Always 403, never 404: the response must not disclose whether the
        // target agent exists in another tenant.
        (StatusCode::FORBIDDEN, Json(body)).into_response()
    }
}

/// Handler state: how to find a target's card, and where decisions are audited.
#[derive(Clone)]
pub struct A2AState {
    cards: Arc<dyn CardResolver>,
    audit: Arc<dyn SecurityAudit>,
}

impl A2AState {
    /// State with the default [`TracingAudit`] sink.
    pub fn new(cards: Arc<dyn CardResolver>) -> Self {
        Self {
            cards,
            audit: Arc::new(TracingAudit),
        }
    }

    /// Override the audit sink (tests, or a DB-backed `audit_logs` writer).
    pub fn with_audit(mut self, audit: Arc<dyn SecurityAudit>) -> Self {
        self.audit = audit;
        self
    }

    /// Authorize one dispatch. `Ok` means, and only means, that every rule in
    /// [`crate::security`] passed.
    ///
    /// An unresolvable target card is denied rather than skipped: a missing
    /// card means an unknown policy, and an unknown policy is not an allow.
    pub fn authorize(&self, req: &A2ATaskRequest) -> Result<SecurityEvent, A2ARejection> {
        let dispatch = A2ADispatch::new(&req.task_id, req.source.clone(), req.target.clone());

        let Some(card) = self.cards.resolve(&req.target) else {
            // Audited by hand: there is no card to run the rule set against,
            // and an unaudited rejection would be a hole in the trail.
            let event = SecurityEvent {
                task_id: dispatch.task_id.clone(),
                source_tenant: dispatch.source.tenant_id.clone(),
                source_project: dispatch.source.project_id.clone(),
                source_agent: dispatch.source.agent_id.clone(),
                target_tenant: dispatch.target.tenant_id.clone(),
                target_project: dispatch.target.project_id.clone(),
                target_agent: dispatch.target.agent_id.clone(),
                allowed: false,
                reason: "target_card_unresolved".to_string(),
                detail: "no agent card registered for dispatch target".to_string(),
            };
            self.audit.record(&event);
            return Err(A2ARejection(SecurityError::Auth(unresolved_target(
                &dispatch,
            ))));
        };

        validate_a2a_dispatch(&dispatch, &card, self.audit.as_ref()).map_err(A2ARejection)
    }
}

/// An unresolved target is reported as a card/target mismatch: from the
/// caller's side the two are indistinguishable, which is deliberate — neither
/// response reveals whether the agent exists.
fn unresolved_target(dispatch: &A2ADispatch) -> crate::security::AuthError {
    crate::security::AuthError::CardTargetMismatch {
        card_agent: String::new(),
        target_agent: dispatch.target.agent_id.clone(),
    }
}

/// `POST /a2a/task`.
///
/// Authorize first, dispatch second. The ordering is the whole point of this
/// handler; the `?` below is the gate.
pub async fn post_a2a_task(
    State(state): State<A2AState>,
    Json(req): Json<A2ATaskRequest>,
) -> Result<(StatusCode, Json<A2ATaskAccepted>), A2ARejection> {
    let event = state.authorize(&req)?;

    // TODO(avm): hand off to the executor (publish on the target's
    // `avm.<tenant>.<project>.tasks` NATS subject). The A2A + Agent Card spike
    // owns the transport; Phase 2 adds the per-tenant NATS credentials that
    // make this subject unreachable from another tenant even if this check were
    // bypassed. Until then an authorized call is accepted and parked.
    tracing::debug!(
        task_id = %req.task_id,
        target_agent = %req.target.agent_id,
        "a2a dispatch authorized; transport handoff pending"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(A2ATaskAccepted {
            task_id: req.task_id,
            status: "accepted".to_string(),
            authorized_by: event.reason,
        }),
    ))
}

/// Router exposing `POST /a2a/task`, ready to `merge()` into the gateway.
pub fn router(state: A2AState) -> Router {
    Router::new()
        .route("/a2a/task", post(post_a2a_task))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::CollectingAudit;
    use avm_agent::A2APolicy;

    fn scope(t: &str, p: &str, a: &str) -> AgentScope {
        AgentScope::new(t, p, a)
    }

    fn state_with(card: AgentCard, audit: Arc<CollectingAudit>) -> A2AState {
        let registry = StaticCardRegistry::new().with_card(card);
        A2AState::new(Arc::new(registry)).with_audit(audit)
    }

    fn req(source: AgentScope, target: AgentScope) -> A2ATaskRequest {
        A2ATaskRequest {
            task_id: "task-1".into(),
            source,
            target,
            payload: serde_json::json!({ "goal": "summarize" }),
        }
    }

    #[tokio::test]
    async fn trusted_peer_is_accepted_with_202() {
        let target = scope("t1", "p1", "callee");
        let card = AgentCard::new("callee", target.clone()).with_policy(
            A2APolicy::default()
                .with_trusted_peer("caller")
                .allowing_intra_project(),
        );
        let audit = Arc::new(CollectingAudit::new());
        let state = state_with(card, audit.clone());

        let (status, body) =
            post_a2a_task(State(state), Json(req(scope("t1", "p1", "caller"), target)))
                .await
                .expect("dispatch should be authorized");

        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body.status, "accepted");
        assert_eq!(body.authorized_by, "trusted_peer");
        assert_eq!(audit.len(), 1, "every decision is audited");
        assert!(audit.events()[0].allowed);
    }

    #[tokio::test]
    async fn intra_project_is_denied_by_default() {
        let target = scope("t1", "p1", "callee");
        let card = AgentCard::new("callee", target.clone());
        let audit = Arc::new(CollectingAudit::new());
        let state = state_with(card, audit.clone());

        let rejection = post_a2a_task(State(state), Json(req(scope("t1", "p1", "caller"), target)))
            .await
            .expect_err("intra-project dispatch is deny-by-default");

        assert_eq!(rejection.0.reason_code(), "intra_project_denied");
        assert_eq!(
            rejection.into_response().status(),
            StatusCode::FORBIDDEN,
            "denials are 403"
        );
        assert_eq!(audit.len(), 1);
        assert!(!audit.events()[0].allowed);
    }

    #[tokio::test]
    async fn cross_tenant_is_denied_even_when_the_policy_is_open() {
        let target = scope("t2", "p1", "callee");
        let card = AgentCard::new("callee", target.clone()).with_policy(A2APolicy {
            default: avm_agent::TrustDefault::Allow,
            trusted_peers: vec!["caller".into()],
            allow_intra_project: true,
            allow_cross_project: true,
        });
        let audit = Arc::new(CollectingAudit::new());
        let state = state_with(card, audit.clone());

        let rejection = post_a2a_task(State(state), Json(req(scope("t1", "p1", "caller"), target)))
            .await
            .expect_err("the tenant boundary is not policy-controllable");

        assert_eq!(rejection.0.reason_code(), "cross_tenant_denied");
        assert_eq!(audit.len(), 1);
    }

    #[tokio::test]
    async fn unknown_target_is_denied_and_audited() {
        let known = scope("t1", "p1", "someone-else");
        let audit = Arc::new(CollectingAudit::new());
        let state = state_with(AgentCard::new("other", known), audit.clone());

        let rejection = post_a2a_task(
            State(state),
            Json(req(scope("t1", "p1", "caller"), scope("t1", "p1", "ghost"))),
        )
        .await
        .expect_err("an unresolvable card must not be an implicit allow");

        assert_eq!(rejection.into_response().status(), StatusCode::FORBIDDEN);
        assert_eq!(audit.len(), 1, "rejection is still audited");
        let event = &audit.events()[0];
        assert!(!event.allowed);
        assert_eq!(event.reason, "target_card_unresolved");
    }

    #[test]
    fn registry_keys_do_not_collide_across_tenants() {
        let a = AgentCard::new("a", scope("t1", "p1", "same-name"));
        let b = AgentCard::new("b", scope("t2", "p1", "same-name"));
        let reg = StaticCardRegistry::new().with_card(a).with_card(b);

        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.resolve(&scope("t1", "p1", "same-name")).unwrap().name,
            "a"
        );
        assert_eq!(
            reg.resolve(&scope("t2", "p1", "same-name")).unwrap().name,
            "b"
        );
        assert!(reg.resolve(&scope("t3", "p1", "same-name")).is_none());
    }
}
