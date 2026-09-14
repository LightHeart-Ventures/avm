//! Built-in gateway tools, with schemas generated from the argument structs
//! the handlers actually deserialize.
//!
//! Generating rather than hand-writing these means a signature can never drift
//! from the implementation: change the struct, the published JSON-Schema
//! changes with it (and its fingerprint, which is how agents notice).

use schemars::JsonSchema;
use serde::Deserialize;

use crate::schema::{to_json_schema, ToolSchema, ToolSource};

/// Arguments for `avm_memory_read`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MemoryReadArgs {
    /// Memory key to resolve.
    pub memory_id: String,
    /// Also search ancestor scopes (system → tenant → project → agent).
    #[serde(default)]
    pub include_inherited: bool,
}

/// Arguments for `avm_memory_write`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MemoryWriteArgs {
    /// Memory key. Empty creates a new record.
    #[serde(default)]
    pub memory_id: String,
    /// Body, 32 KB max.
    pub content: String,
    /// Free-form labels for retrieval.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional expiry, in seconds.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// Arguments for `avm_job_submit`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct JobSubmitArgs {
    /// Agent to run the job.
    pub agent_id: String,
    /// Opaque JSON payload handed to the agent process.
    pub payload: String,
    /// Optional idempotency key.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Arguments for `avm_job_status`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct JobStatusArgs {
    /// Job to inspect.
    pub job_id: String,
}

/// Every tool the gateway serves itself.
///
/// Names match `avm_gateway::McpRouter`'s local routes.
pub fn builtin_tools() -> Vec<ToolSchema> {
    vec![
        ToolSchema::new(
            "avm_memory_read",
            Some("Read a memory visible to the caller's scope.".into()),
            to_json_schema::<MemoryReadArgs>(),
            None,
            ToolSource::builtin(),
        ),
        ToolSchema::new(
            "avm_memory_write",
            Some("Write a memory into the caller's scope.".into()),
            to_json_schema::<MemoryWriteArgs>(),
            None,
            ToolSource::builtin(),
        ),
        ToolSchema::new(
            "avm_job_submit",
            Some("Submit a job to an agent in the caller's project.".into()),
            to_json_schema::<JobSubmitArgs>(),
            None,
            ToolSource::builtin(),
        ),
        ToolSchema::new(
            "avm_job_status",
            Some("Fetch the current status of a submitted job.".into()),
            to_json_schema::<JobStatusArgs>(),
            None,
            ToolSource::builtin(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::validate_call;
    use serde_json::json;

    #[test]
    fn every_builtin_has_an_object_schema() {
        let tools = builtin_tools();
        assert_eq!(tools.len(), 4);
        for t in &tools {
            assert_eq!(
                t.input_schema["type"], "object",
                "{} is not an object",
                t.name
            );
            assert!(t.description.is_some());
            assert!(t.fingerprint.starts_with("fnv1a64:"));
        }
    }

    #[test]
    fn generated_schema_matches_the_struct_contract() {
        let tools = builtin_tools();
        let read = tools.iter().find(|t| t.name == "avm_memory_read").unwrap();
        assert_eq!(read.required(), vec!["memory_id"]);

        assert!(
            validate_call(read, &json!({ "memory_id": "m1" }))
                .unwrap()
                .valid
        );
        assert!(!validate_call(read, &json!({})).unwrap().valid);
        assert!(
            !validate_call(read, &json!({ "memory_id": 1 }))
                .unwrap()
                .valid
        );
    }

    #[test]
    fn job_submit_requires_agent_and_payload() {
        let tools = builtin_tools();
        let submit = tools.iter().find(|t| t.name == "avm_job_submit").unwrap();
        let mut required = submit.required();
        required.sort_unstable();
        assert_eq!(required, vec!["agent_id", "payload"]);
        assert!(
            validate_call(submit, &json!({ "agent_id": "ag_1", "payload": "{}" }))
                .unwrap()
                .valid
        );
    }
}
