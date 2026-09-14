//! Canonical span / log field names.
//!
//! Centralised so every AVM service labels the same concept the same way — a
//! dashboard query written against the gateway keeps working against the
//! executor.
//!
//! **Privacy rule:** these are the *only* identifiers AVM attaches to
//! telemetry. Job payloads, tool arguments, auth tokens, model weights and
//! agent output bodies are never span attributes. See
//! `docs/observability/otel-architecture.md#threat-model`.

/// `tenant_id` — owning tenant.
pub const TENANT_ID: &str = "tenant_id";
/// `project_id` — owning project/board.
pub const PROJECT_ID: &str = "project_id";
/// `agent_id` — agent identity, when the span is agent-scoped.
pub const AGENT_ID: &str = "agent_id";
/// `job_id` — job envelope identity.
pub const JOB_ID: &str = "job_id";
/// `scope_level` — system | tenant | project | agent.
pub const SCOPE_LEVEL: &str = "scope_level";
/// `instrumentation_level` — off | basic | detailed.
pub const INSTRUMENTATION_LEVEL: &str = "instrumentation_level";
/// `outcome` — succeeded | failed | timeout | cancelled.
pub const OUTCOME: &str = "outcome";
/// `subject` — NATS subject.
pub const SUBJECT: &str = "subject";
/// `executor_id` — which executor replica ran the job.
pub const EXECUTOR_ID: &str = "executor_id";

/// Canonical span names, so the trace tree is greppable across services.
pub mod span {
    /// Root span for an inbound A2A task at the gateway.
    pub const GATEWAY_DISPATCH_A2A_TASK: &str = "gateway.dispatch_a2a_task";
    /// Root span for an inbound job submission at the gateway.
    pub const GATEWAY_DISPATCH_JOB: &str = "gateway.dispatch_job";
    /// Gateway auth/scope check.
    pub const GATEWAY_AUTH_CHECK: &str = "gateway.auth_check";
    /// Gateway MCP tool dispatch.
    pub const GATEWAY_TOOL_CALL: &str = "gateway.tool_call";
    /// Scheduler placement decision.
    pub const SCHEDULER_PLACE_JOB: &str = "scheduler.place_job";
    /// Scheduler node/constraint scoring.
    pub const SCHEDULER_NODE_SELECTION: &str = "scheduler.node_selection_score";
    /// Scheduler reconcile pass.
    pub const SCHEDULER_TICK: &str = "scheduler.tick";
    /// Executor container/process lifecycle.
    pub const EXECUTOR_RUN_CONTAINER: &str = "executor.run_container";
    /// Image / model pull.
    pub const CONTAINER_PULL_IMAGE: &str = "container.pull_image";
    /// Process execution proper.
    pub const CONTAINER_RUN: &str = "container.run";
    /// Cleanup after the process exits.
    pub const CONTAINER_CLEANUP: &str = "container.cleanup";
    /// Queue publish of a job envelope.
    pub const QUEUE_PUBLISH_JOB: &str = "queue.publish_job";
    /// Queue publish of a result envelope.
    pub const QUEUE_PUBLISH_RESULT: &str = "queue.publish_result";
    /// Queue consume / fetch batch.
    pub const QUEUE_CONSUME: &str = "queue.consume";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_names_are_dotted_and_lowercase() {
        for name in [
            span::GATEWAY_DISPATCH_JOB,
            span::SCHEDULER_PLACE_JOB,
            span::EXECUTOR_RUN_CONTAINER,
            span::QUEUE_PUBLISH_RESULT,
        ] {
            assert!(name.contains('.'), "{name} is not namespaced");
            assert_eq!(name, name.to_ascii_lowercase());
        }
    }

    #[test]
    fn identity_fields_are_snake_case() {
        for f in [TENANT_ID, PROJECT_ID, AGENT_ID, JOB_ID, SCOPE_LEVEL] {
            assert!(!f.contains('.') && !f.contains('-'), "{f} is not snake_case");
        }
    }
}
