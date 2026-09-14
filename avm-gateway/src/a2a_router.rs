//! A2A discovery surface: the Agent Card this gateway advertises.
//!
//! `GET /.well-known/agent-card.json` serves [`AgentCard::example`], per the
//! spec in `docs/A2A_AGENT_CARD.md`.
//!
//! Inbound dispatch (`POST /a2a/task`) lives in [`crate::a2a`], which
//! authorizes every task against the caller's scope before routing it.
//! [`handle_task`] here is the envelope-level validation the card spec
//! describes — target, capability and allow-list checks against a concrete
//! [`AgentCard`] — kept for callers that speak the richer [`A2ATask`]
//! envelope. It does not dispatch real work yet.

use std::sync::Arc;

use avm_agent::{
    A2AError, A2AResponse, A2ATask, AgentCard, ErrorCode, TaskResult, Usage, AGENT_CARD_PATH,
};
use axum::{extract::State, routing::get, Json, Router};

/// Card this gateway advertises at [`AGENT_CARD_PATH`].
#[derive(Debug, Clone)]
pub struct CardState {
    /// The Agent Card served at [`AGENT_CARD_PATH`].
    pub card: Arc<AgentCard>,
}

impl Default for CardState {
    fn default() -> Self {
        Self {
            card: Arc::new(AgentCard::example()),
        }
    }
}

/// Build the Agent Card router. Merge it into the gateway's main router.
pub fn router(state: CardState) -> Router {
    Router::new()
        .route(AGENT_CARD_PATH, get(agent_card))
        .with_state(state)
}

/// `GET /.well-known/agent-card.json`
async fn agent_card(State(state): State<CardState>) -> Json<AgentCard> {
    Json((*state.card).clone())
}

/// Envelope-level validation of an [`A2ATask`] against a concrete card.
///
/// Pure, so it is testable without a live server. Scope authorization for the
/// wire path is done by [`crate::security::validate_a2a_dispatch`].
pub fn handle_task(card: &AgentCard, task: A2ATask) -> A2AResponse {
    if let Err(err) = task.validate() {
        return A2AResponse::failed(&task.task_id, err);
    }

    if let Some(target) = task.target_agent.as_deref() {
        if target != card.agent_id {
            return A2AResponse::failed(
                &task.task_id,
                A2AError::new(
                    ErrorCode::WrongTarget,
                    format!("this agent is {}, not {target}", card.agent_id),
                ),
            );
        }
    }

    if let Some(cap) = task.context.capability.as_deref() {
        if !card.has_capability(cap) {
            return A2AResponse::failed(
                &task.task_id,
                A2AError::new(
                    ErrorCode::UnknownCapability,
                    format!("no such capability {cap}"),
                ),
            );
        }
    }

    if !card.auth_policy.permits(&task.source_agent.agent_id) {
        return A2AResponse::failed(
            &task.task_id,
            A2AError::new(
                ErrorCode::Forbidden,
                format!("{} is not on the allow-list", task.source_agent.agent_id),
            ),
        );
    }

    // Test-only: echo instead of executing.
    A2AResponse::succeeded(
        &task.task_id,
        TaskResult {
            content: format!("echo: {}", task.instructions),
            output: Some(serde_json::json!({
                "echo": true,
                "source_agent": task.source_agent.agent_id,
                "capability": task.context.capability,
            })),
            artifacts: Vec::new(),
        },
        Usage::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use avm_agent::{TaskStatus, A2A_TASK_PATH};

    fn card() -> AgentCard {
        AgentCard::example()
    }

    fn task() -> A2ATask {
        let mut t = A2ATask::new("ag_planner", "Review PR #42.");
        t.target_agent = Some("ag_pr_reviewer".into());
        t.context.capability = Some("review_pull_request".into());
        t
    }

    #[test]
    fn happy_path_echoes_the_instructions() {
        let r = handle_task(&card(), task());
        assert_eq!(r.status, TaskStatus::Succeeded);
        assert_eq!(r.http_status(), 200);
        assert_eq!(r.result.unwrap().content, "echo: Review PR #42.");
    }

    #[test]
    fn wrong_target_is_rejected_404() {
        let mut t = task();
        t.target_agent = Some("ag_someone_else".into());
        let r = handle_task(&card(), t);
        assert_eq!(r.status, TaskStatus::Rejected);
        assert_eq!(r.error.unwrap().code, ErrorCode::WrongTarget);
    }

    #[test]
    fn unknown_capability_is_rejected() {
        let mut t = task();
        t.context.capability = Some("deploy_to_prod".into());
        let r = handle_task(&card(), t);
        assert_eq!(r.error.unwrap().code, ErrorCode::UnknownCapability);
    }

    #[test]
    fn caller_outside_the_allow_list_is_forbidden() {
        let mut t = task();
        t.source_agent = avm_agent::AgentRef::new("ag_stranger");
        let r = handle_task(&card(), t);
        let err = r.error.unwrap();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(!err.retryable);
    }

    #[test]
    fn malformed_task_is_rejected_before_auth() {
        let mut t = task();
        t.instructions = String::new();
        let r = handle_task(&card(), t);
        assert_eq!(r.error.unwrap().code, ErrorCode::InvalidTask);
    }

    #[test]
    fn response_task_id_always_echoes_the_request() {
        let t = task();
        let id = t.task_id.clone();
        assert_eq!(handle_task(&card(), t).task_id, id);
    }

    #[tokio::test]
    async fn card_endpoint_serves_the_example_card() {
        let Json(served) = agent_card(State(CardState::default())).await;
        served.validate().unwrap();
        assert_eq!(served.agent_id, "ag_pr_reviewer");
        assert!(served.has_capability("review_pull_request"));
    }

    #[test]
    fn routes_are_registered_at_the_well_known_paths() {
        // Compile-time proof the router builds with the spec'd constants.
        let _ = router(CardState::default());
        assert_eq!(AGENT_CARD_PATH, "/.well-known/agent-card.json");
        assert_eq!(A2A_TASK_PATH, "/a2a/task");
    }
}
