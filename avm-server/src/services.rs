//! gRPC service implementations.
//!
//! Each submodule will implement the generated `*Server` trait from
//! [`avm_proto::v1`] once `protoc` codegen is enabled in CI. Today they expose
//! transport-agnostic handlers so the business logic can be tested without a
//! gRPC harness.

use avm_proto::types::{JobMessage, Scope};
use avm_storage::jobs::{self, NewJob};
use chrono::Utc;
use uuid::Uuid;

use crate::AppState;

/// Accept a job: persist it, then publish it to JetStream.
///
/// Postgres is written first so a queue failure leaves a recoverable `queued`
/// row the scheduler can re-publish.
pub async fn submit_job(
    state: &AppState,
    scope: Scope,
    agent_id: &str,
    payload: serde_json::Value,
    priority: i32,
    max_retries: i32,
    idempotency_key: Option<String>,
) -> anyhow::Result<String> {
    let job_id = format!("job_{}", Uuid::new_v4().simple());

    let row = jobs::create(
        &state.db,
        &NewJob {
            job_id: job_id.clone(),
            scope: scope.clone(),
            agent_id: agent_id.to_string(),
            payload: payload.clone(),
            priority,
            max_retries,
            idempotency_key,
        },
    )
    .await?;

    let msg = JobMessage {
        job_id: row.job_id.clone(),
        scope,
        agent_id: agent_id.to_string(),
        payload: payload.to_string(),
        created_at: Utc::now().to_rfc3339(),
        ..Default::default()
    };
    state.queue.publish_job(&msg).await?;

    tracing::info!(job_id = %row.job_id, agent_id, "job accepted");
    Ok(row.job_id)
}

/// Fetch a job by id.
pub async fn get_job(state: &AppState, job_id: &str) -> anyhow::Result<jobs::JobRow> {
    Ok(jobs::get(&state.db, job_id).await?)
}

/// Cancel a job that has not finished.
pub async fn cancel_job(state: &AppState, job_id: &str, reason: &str) -> anyhow::Result<()> {
    jobs::cancel(&state.db, job_id, reason).await?;
    tracing::info!(job_id, reason, "job cancelled");
    Ok(())
}

pub mod tenant {
    //! TODO(avm): TenantService — CRUD + quota management against `quotas`.
}

pub mod agent {
    //! TODO(avm): AgentService — registration, discovery, heartbeat.
}

pub mod memory {
    //! MemoryService handlers delegate straight to `avm_storage::memories`,
    //! which enforces the scope ACL.
    pub use avm_storage::memories::{delete, list_visible, read, write, NewMemory};
}
