//! A2A task invocation: the envelopes exchanged on `POST /a2a/task`.

use serde::{Deserialize, Serialize};

use crate::card::AgentRef;
use crate::discovery::SCHEMA_VERSION;

/// Default wall-clock budget for a task, in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

/// Wall-clock budget for a task.
///
/// `seconds` is the budget the *caller* is willing to wait. `deadline`, when
/// present, is an absolute RFC-3339 instant and always wins over `seconds`:
/// it survives queue hops without drifting, which a relative budget does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTimeout {
    /// Relative budget in seconds.
    pub seconds: u64,
    /// Absolute RFC-3339 UTC deadline. Authoritative when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
}

impl Default for TaskTimeout {
    fn default() -> Self {
        Self {
            seconds: DEFAULT_TIMEOUT_SECONDS,
            deadline: None,
        }
    }
}

impl TaskTimeout {
    /// A relative-only budget.
    pub fn seconds(seconds: u64) -> Self {
        Self {
            seconds,
            deadline: None,
        }
    }
}

/// A blob carried alongside a task or a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// Caller-chosen name, unique within the task.
    pub name: String,
    /// IANA media type, e.g. `text/markdown`, `application/json`.
    #[serde(default)]
    pub mime_type: String,
    /// Inline content. Mutually exclusive with `uri`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Out-of-band location for content too large to inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// Everything the callee needs besides the free-text instructions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskContext {
    /// Scope the work executes under. The callee MUST NOT widen it.
    #[serde(default)]
    pub scope: avm_proto::types::Scope,
    /// Capability being invoked, when the caller read it off the Agent Card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// Task that spawned this one, for fan-out trees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// Trace correlation id, propagated into OTel spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Structured input, validated against the capability's `input_schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    /// Attachments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

/// Work submitted from one agent to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2ATask {
    /// Schema identifier; see [`crate::discovery::SCHEMA_VERSION`].
    pub schema_version: String,
    /// Caller-generated idempotency key. Re-submitting the same `task_id`
    /// MUST return the original response, not start a second run.
    pub task_id: String,
    /// Who is asking.
    pub source_agent: AgentRef,
    /// Which agent the caller believes it is addressing. Advisory: the
    /// receiver rejects a mismatch rather than silently accepting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent: Option<String>,
    /// Natural-language statement of the work.
    pub instructions: String,
    /// Structured context.
    #[serde(default)]
    pub context: TaskContext,
    /// Wall-clock budget.
    #[serde(default)]
    pub timeout: TaskTimeout,
    /// RFC-3339 UTC submission timestamp.
    pub created_at: String,
}

impl A2ATask {
    /// A task from `source_agent` with a fresh UUID `task_id` and `created_at`.
    pub fn new(source_agent: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            task_id: format!("task_{}", uuid::Uuid::new_v4().simple()),
            source_agent: AgentRef::new(source_agent),
            target_agent: None,
            instructions: instructions.into(),
            context: TaskContext::default(),
            timeout: TaskTimeout::default(),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Reject a task this runtime cannot safely execute.
    pub fn validate(&self) -> Result<(), A2AError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(A2AError::new(
                ErrorCode::UnsupportedSchema,
                format!("unsupported task schema {}", self.schema_version),
            ));
        }
        if self.task_id.is_empty() {
            return Err(A2AError::new(
                ErrorCode::InvalidTask,
                "task_id must not be empty",
            ));
        }
        if self.instructions.trim().is_empty() {
            return Err(A2AError::new(
                ErrorCode::InvalidTask,
                "instructions must not be empty",
            ));
        }
        if self.timeout.seconds == 0 {
            return Err(A2AError::new(
                ErrorCode::InvalidTask,
                "timeout.seconds must be > 0",
            ));
        }
        Ok(())
    }
}

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Queued; poll or await the callback.
    Accepted,
    /// Actively executing.
    Running,
    /// Finished; `result` is populated.
    Succeeded,
    /// Finished unsuccessfully; `error` is populated.
    Failed,
    /// Refused before execution (auth, schema, capability).
    Rejected,
    /// Deadline elapsed.
    TimedOut,
    /// Cancelled by the caller or the control plane.
    Cancelled,
}

impl TaskStatus {
    /// True once no further transition is possible.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, TaskStatus::Accepted | TaskStatus::Running)
    }
}

/// Machine-readable failure taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Credential missing or invalid.
    Unauthenticated,
    /// Credential valid but caller not permitted.
    Forbidden,
    /// Malformed envelope.
    InvalidTask,
    /// `schema_version` not implemented.
    UnsupportedSchema,
    /// Requested capability is not on the Agent Card.
    UnknownCapability,
    /// `target_agent` does not match this agent.
    WrongTarget,
    /// Deadline elapsed before completion.
    Timeout,
    /// Callee is over its quota or concurrency cap.
    Overloaded,
    /// Agent ran but failed.
    ExecutionFailed,
    /// A dependency (MCP server, model, database) failed.
    UpstreamFailure,
    /// Anything unclassified.
    Internal,
}

impl ErrorCode {
    /// Whether a caller should retry with backoff.
    ///
    /// Retrying a `Forbidden` or `InvalidTask` will never succeed; retrying an
    /// `Overloaded` or `UpstreamFailure` usually will.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ErrorCode::Timeout
                | ErrorCode::Overloaded
                | ErrorCode::UpstreamFailure
                | ErrorCode::Internal
        )
    }

    /// HTTP status the gateway maps this code to.
    pub fn http_status(&self) -> u16 {
        match self {
            ErrorCode::Unauthenticated => 401,
            ErrorCode::Forbidden => 403,
            ErrorCode::InvalidTask | ErrorCode::UnsupportedSchema => 400,
            ErrorCode::UnknownCapability | ErrorCode::WrongTarget => 404,
            ErrorCode::Timeout => 504,
            ErrorCode::Overloaded => 429,
            ErrorCode::ExecutionFailed => 422,
            ErrorCode::UpstreamFailure => 502,
            ErrorCode::Internal => 500,
        }
    }
}

/// Structured failure attached to an [`A2AResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct A2AError {
    /// Failure class.
    pub code: ErrorCode,
    /// Human-readable detail. MUST NOT contain credentials.
    pub message: String,
    /// Whether the caller should retry; mirrors [`ErrorCode::retryable`].
    pub retryable: bool,
}

impl A2AError {
    /// Build an error, deriving `retryable` from `code`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.retryable(),
        }
    }
}

/// Token / cost accounting for a completed task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens consumed.
    #[serde(default)]
    pub input_tokens: u64,
    /// Completion tokens produced.
    #[serde(default)]
    pub output_tokens: u64,
    /// Number of MCP tool invocations.
    #[serde(default)]
    pub tool_calls: u32,
    /// Wall-clock execution time.
    #[serde(default)]
    pub duration_ms: u64,
    /// Cost in micro-USD (integer, to keep the wire format exact).
    #[serde(default)]
    pub cost_micro_usd: u64,
}

impl Usage {
    /// `input_tokens + output_tokens`.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Successful output of a task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// Prose answer.
    #[serde(default)]
    pub content: String,
    /// Structured output, validated against the capability's `output_schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// Produced attachments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

/// Answer to an [`A2ATask`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2AResponse {
    /// Schema identifier; see [`crate::discovery::SCHEMA_VERSION`].
    pub schema_version: String,
    /// Echoes the submitted `task_id`.
    pub task_id: String,
    /// Lifecycle state.
    pub status: TaskStatus,
    /// Present iff `status == Succeeded`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    /// Present iff `status` is a failure state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<A2AError>,
    /// Accounting. Zeroed while the task is non-terminal.
    #[serde(default)]
    pub usage: Usage,
    /// RFC-3339 UTC timestamp of this state transition.
    pub updated_at: String,
}

impl A2AResponse {
    fn base(task_id: &str, status: TaskStatus) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            task_id: task_id.to_string(),
            status,
            result: None,
            error: None,
            usage: Usage::default(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Task queued.
    pub fn accepted(task_id: &str) -> Self {
        Self::base(task_id, TaskStatus::Accepted)
    }

    /// Task finished successfully.
    pub fn succeeded(task_id: &str, result: TaskResult, usage: Usage) -> Self {
        let mut r = Self::base(task_id, TaskStatus::Succeeded);
        r.result = Some(result);
        r.usage = usage;
        r
    }

    /// Task failed. `status` is derived from the error code so that a timeout
    /// reports `TimedOut` and an auth failure reports `Rejected`.
    pub fn failed(task_id: &str, error: A2AError) -> Self {
        let status = match error.code {
            ErrorCode::Timeout => TaskStatus::TimedOut,
            ErrorCode::Unauthenticated
            | ErrorCode::Forbidden
            | ErrorCode::InvalidTask
            | ErrorCode::UnsupportedSchema
            | ErrorCode::UnknownCapability
            | ErrorCode::WrongTarget => TaskStatus::Rejected,
            _ => TaskStatus::Failed,
        };
        let mut r = Self::base(task_id, status);
        r.error = Some(error);
        r
    }

    /// HTTP status the gateway should return alongside this body.
    pub fn http_status(&self) -> u16 {
        match (&self.error, self.status) {
            (Some(e), _) => e.code.http_status(),
            (None, TaskStatus::Accepted) => 202,
            _ => 200,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> A2ATask {
        let mut t = A2ATask::new("ag_planner", "Review PR #42 in LightHeart-Ventures/avm.");
        t.target_agent = Some("ag_pr_reviewer".into());
        t.context.capability = Some("review_pull_request".into());
        t.context.scope = avm_proto::types::Scope::project("t_lightheart", "b_avm");
        t
    }

    #[test]
    fn task_roundtrips_through_json() {
        let t = task();
        let back: A2ATask = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn new_task_is_valid_and_uniquely_identified() {
        let a = A2ATask::new("ag_a", "do a thing");
        let b = A2ATask::new("ag_a", "do a thing");
        a.validate().unwrap();
        assert_ne!(a.task_id, b.task_id);
        assert!(a.task_id.starts_with("task_"));
    }

    #[test]
    fn empty_instructions_are_rejected() {
        let mut t = task();
        t.instructions = "   ".into();
        let err = t.validate().unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidTask);
    }

    #[test]
    fn unknown_schema_is_rejected() {
        let mut t = task();
        t.schema_version = "avm.a2a/v0".into();
        assert_eq!(t.validate().unwrap_err().code, ErrorCode::UnsupportedSchema);
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let mut t = task();
        t.timeout = TaskTimeout::seconds(0);
        assert!(t.validate().is_err());
    }

    #[test]
    fn default_timeout_is_five_minutes() {
        assert_eq!(TaskTimeout::default().seconds, 300);
        assert!(TaskTimeout::default().deadline.is_none());
    }

    #[test]
    fn response_roundtrips_and_omits_empty_fields() {
        let r = A2AResponse::succeeded(
            "task_1",
            TaskResult {
                content: "looks good".into(),
                ..Default::default()
            },
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"error\""), "error must be omitted: {json}");
        let back: A2AResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert_eq!(back.usage.total_tokens(), 15);
    }

    #[test]
    fn failure_status_is_derived_from_error_code() {
        let timeout = A2AResponse::failed("t", A2AError::new(ErrorCode::Timeout, "deadline"));
        assert_eq!(timeout.status, TaskStatus::TimedOut);
        assert_eq!(timeout.http_status(), 504);

        let denied = A2AResponse::failed("t", A2AError::new(ErrorCode::Forbidden, "nope"));
        assert_eq!(denied.status, TaskStatus::Rejected);
        assert_eq!(denied.http_status(), 403);

        let boom = A2AResponse::failed("t", A2AError::new(ErrorCode::ExecutionFailed, "panicked"));
        assert_eq!(boom.status, TaskStatus::Failed);
        assert_eq!(boom.http_status(), 422);
    }

    #[test]
    fn accepted_is_202_and_non_terminal() {
        let r = A2AResponse::accepted("task_1");
        assert_eq!(r.http_status(), 202);
        assert!(!r.status.is_terminal());
        assert!(TaskStatus::Succeeded.is_terminal());
        assert!(TaskStatus::TimedOut.is_terminal());
    }

    #[test]
    fn retryability_matches_the_taxonomy() {
        assert!(ErrorCode::Overloaded.retryable());
        assert!(ErrorCode::UpstreamFailure.retryable());
        assert!(!ErrorCode::Forbidden.retryable());
        assert!(!ErrorCode::InvalidTask.retryable());
        assert!(A2AError::new(ErrorCode::Timeout, "x").retryable);
    }

    #[test]
    fn status_is_snake_case_on_the_wire() {
        let json = serde_json::to_string(&TaskStatus::TimedOut).unwrap();
        assert_eq!(json, "\"timed_out\"");
    }
}
