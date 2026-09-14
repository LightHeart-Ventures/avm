//! Protobuf/gRPC definitions for the AVM control plane.
//!
//! Generated code lives in [`v1`] (produced by `build.rs` from
//! `proto/avm_service.proto`). Hand-written, serde-friendly mirrors of the
//! queue envelopes live in [`types`] so that `avm-queue` can ship JSON over
//! NATS without a hard dependency on `protoc` being installed.

pub mod v1 {
    #![allow(clippy::all, missing_docs, unused_imports)]
    include!(concat!(env!("OUT_DIR"), "/avm.v1.rs"));
}

pub mod tools;
pub mod types;

pub use tools::{ToolCall, ToolCallValidation, ToolDefinition};
pub use types::{JobMessage, ResultMessage, Scope, ScopeLevel};

/// NATS subject helpers.
pub mod subjects {
    /// JetStream stream holding queued work.
    pub const JOBS_STREAM: &str = "AVM_JOBS";
    /// JetStream stream holding job results.
    pub const RESULTS_STREAM: &str = "AVM_RESULTS";
    /// Wildcard binding for the jobs stream.
    pub const JOBS_WILDCARD: &str = "avm.jobs.>";
    /// Wildcard binding for the results stream.
    pub const RESULTS_WILDCARD: &str = "avm.results.>";

    /// `avm.jobs.<tenant>.<project>`
    pub fn job_subject(tenant_id: &str, project_id: &str) -> String {
        format!(
            "avm.jobs.{}.{}",
            norm(tenant_id),
            norm(project_id)
        )
    }

    /// `avm.results.<tenant>.<project>`
    pub fn result_subject(tenant_id: &str, project_id: &str) -> String {
        format!(
            "avm.results.{}.{}",
            norm(tenant_id),
            norm(project_id)
        )
    }

    fn norm(s: &str) -> &str {
        if s.is_empty() {
            "_"
        } else {
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::subjects;

    #[test]
    fn subjects_are_hierarchical() {
        assert_eq!(
            subjects::job_subject("t_acme", "b_payments"),
            "avm.jobs.t_acme.b_payments"
        );
        assert_eq!(subjects::result_subject("t_acme", ""), "avm.results.t_acme._");
    }
}
