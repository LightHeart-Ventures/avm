//! A2A surface: Agent Card discovery + inbound task submission.
//!
//! Both endpoints are **test-only scaffolding** for the spec in
//! `docs/A2A_AGENT_CARD.md`:
//!
//! * `GET /.well-known/agent-card.json` serves [`AgentCard::example`].
//! * `POST /a2a/task` validates the envelope and echoes the instructions back.
//!
//! Neither one dispatches real work yet — see the TODOs below.

use std::sync::Arc;

use avm_agent::{
    A2AError, A2AResponse, A2ATask, AgentCard, ErrorCode, TaskResult, Usage, A2A_TASK_PATH,
    AGENT_CARD_PATH,
};
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};

/// Card this gateway advertises, plus whatever the A2A handlers need.
#[derive(Debug, Clone)]
pub struct A2AState {
    /// The Agent Card served at [`AGENT_CARD_PATH`].
    pub card: Arc<AgentCard>,
}

impl Default for A2AState {
    fn default() -> Self {
        Self {
            card: Arc::new(AgentCard::example()),
        }
    }
}

/// Build the A2A router. Merge it into the gateway's main router.
pub fn router(state: A2AState) -> Router {
    Router::new()
        .route(AGENT_CARD_PATH, get(agent_card))
        .route(A2A_TASK_PATH, post(submit_task))
        .with_state(state)
}

/// `GET /.well-known/agent-card.json`
async fn agent_card(State(state): State<A2AState>) -> Json<AgentCard> {
    Json((*state.card).clone())
}

/// `POST /a2a/task`
///
/// TODO(avm): authenticate against `card.auth_policy`, enforce
/// `context.scope`, then publish a `JobMessage` onto
/// `avm.jobs.<tenant>.<project>` and return `202 Accepted` with a poll URL.
async fn submit_task(
    State(state): State<A2AState>,
    Json(task): Json<A2ATask>,
) -> (StatusCode, Json<A2AResponse>) {
    let response = handle_task(&state.card, task);
    let status =
        StatusCode::from_u16(response.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(response))
}

/// Pure core of [`submit_task`], so it is testable without a live server.
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
    use avm_agent::TaskStatus;

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
        let Json(served) = agent_card(State(A2AState::default())).await;
        served.validate().unwrap();
        assert_eq!(served.agent_id, "ag_pr_reviewer");
        assert!(served.has_capability("review_pull_request"));
    }

    #[tokio::test]
    async fn task_endpoint_maps_status_codes() {
        let (code, Json(body)) = submit_task(State(A2AState::default()), Json(task())).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.status, TaskStatus::Succeeded);

        let mut bad = task();
        bad.target_agent = Some("ag_nope".into());
        let (code, _) = submit_task(State(A2AState::default()), Json(bad)).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    #[test]
    fn routes_are_registered_at_the_well_known_paths() {
        // Compile-time proof the router builds with the spec'd constants.
        let _ = router(A2AState::default());
        assert_eq!(AGENT_CARD_PATH, "/.well-known/agent-card.json");
        assert_eq!(A2A_TASK_PATH, "/a2a/task");
    }
}
