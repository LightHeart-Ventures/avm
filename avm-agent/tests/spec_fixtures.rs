//! Contract tests: the JSON printed in `docs/A2A_AGENT_CARD.md` must stay
//! byte-compatible with the Rust types.
//!
//! The fixtures under `tests/fixtures/` are the documents embedded verbatim in
//! the spec doc. If a struct changes shape without the doc following, these
//! fail — which is the point.

use avm_agent::{A2ATask, AgentCard, ErrorCode, TaskStatus};

const CARD_FIXTURE: &str = include_str!("fixtures/agent-card.example.json");
const TASK_FIXTURE: &str = include_str!("fixtures/a2a-task.example.json");

#[test]
fn documented_card_matches_the_example_constructor() {
    let from_doc: AgentCard = serde_json::from_str(CARD_FIXTURE).expect("fixture parses");
    assert_eq!(from_doc, AgentCard::example());
}

#[test]
fn documented_card_has_no_undocumented_fields() {
    // Round-tripping through serde_json::Value catches both directions:
    // a field the doc omits, and a field the doc invents.
    let from_doc: serde_json::Value = serde_json::from_str(CARD_FIXTURE).unwrap();
    let from_code = serde_json::to_value(AgentCard::example()).unwrap();
    assert_eq!(from_doc, from_code);
}

#[test]
fn documented_task_parses_and_validates() {
    let task: A2ATask = serde_json::from_str(TASK_FIXTURE).expect("fixture parses");
    task.validate().expect("documented task is valid");
    assert_eq!(task.target_agent.as_deref(), Some("ag_pr_reviewer"));
    assert_eq!(task.timeout.seconds, 300);
    assert_eq!(
        task.context.capability.as_deref(),
        Some("review_pull_request")
    );
}

#[test]
fn a_card_round_trips_a_task_it_advertises() {
    let card = AgentCard::example();
    let task: A2ATask = serde_json::from_str(TASK_FIXTURE).unwrap();
    let capability = task.context.capability.clone().unwrap();
    assert!(card.has_capability(&capability));
    assert!(card.auth_policy.permits(&task.source_agent.agent_id));
    assert_eq!(task.target_agent.unwrap(), card.agent_id);
}

#[test]
fn every_error_code_has_a_distinct_http_mapping_class() {
    // 4xx = caller's fault and not retryable; 5xx/429 = ours and retryable.
    for code in [
        ErrorCode::Unauthenticated,
        ErrorCode::Forbidden,
        ErrorCode::InvalidTask,
        ErrorCode::UnsupportedSchema,
        ErrorCode::UnknownCapability,
        ErrorCode::WrongTarget,
        ErrorCode::ExecutionFailed,
    ] {
        assert!((400..500).contains(&code.http_status()), "{code:?}");
        assert!(!code.retryable(), "{code:?} must not be retryable");
    }
    for code in [
        ErrorCode::Timeout,
        ErrorCode::Overloaded,
        ErrorCode::UpstreamFailure,
        ErrorCode::Internal,
    ] {
        assert!(code.retryable(), "{code:?} must be retryable");
    }
}

#[test]
fn terminal_states_are_exactly_the_finished_ones() {
    for s in [
        TaskStatus::Succeeded,
        TaskStatus::Failed,
        TaskStatus::Rejected,
        TaskStatus::TimedOut,
        TaskStatus::Cancelled,
    ] {
        assert!(s.is_terminal(), "{s:?}");
    }
    for s in [TaskStatus::Accepted, TaskStatus::Running] {
        assert!(!s.is_terminal(), "{s:?}");
    }
}
