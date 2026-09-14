//! Durable job records: create, claim, complete, retry.

use avm_proto::types::Scope;
use chrono::{DateTime, Utc};

use crate::{Db, Result, StorageError};

/// A row of the `jobs` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub job_id: String,
    pub scope: String,
    pub tenant_id: String,
    pub project_id: String,
    pub agent_id: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub priority: i32,
    pub retry_count: i32,
    pub max_retries: i32,
    pub last_error: Option<String>,
    pub result: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Input for [`create`].
#[derive(Debug, Clone)]
pub struct NewJob {
    pub job_id: String,
    pub scope: Scope,
    pub agent_id: String,
    pub payload: serde_json::Value,
    pub priority: i32,
    pub max_retries: i32,
    pub idempotency_key: Option<String>,
}

/// Terminal + transitional job states.
pub mod status {
    pub const QUEUED: &str = "queued";
    pub const RUNNING: &str = "running";
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
}

/// Insert a queued job. Idempotent on `(tenant_id, idempotency_key)`.
pub async fn create(db: &Db, job: &NewJob) -> Result<JobRow> {
    let row = sqlx::query_as::<_, JobRow>(
        r#"
        INSERT INTO jobs
            (job_id, scope, tenant_id, project_id, agent_id,
             payload, status, priority, max_retries, idempotency_key)
        VALUES ($1, $2, $3, $4, $5, $6, 'queued', $7, $8, $9)
        ON CONFLICT (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL
        DO UPDATE SET payload = jobs.payload
        RETURNING job_id, scope, tenant_id, project_id, agent_id, payload, status,
                  priority, retry_count, max_retries, last_error, result,
                  created_at, started_at, finished_at
        "#,
    )
    .bind(&job.job_id)
    .bind(&job.scope.level)
    .bind(&job.scope.tenant_id)
    .bind(&job.scope.project_id)
    .bind(&job.agent_id)
    .bind(&job.payload)
    .bind(job.priority)
    .bind(job.max_retries)
    .bind(&job.idempotency_key)
    .fetch_one(db)
    .await?;

    Ok(row)
}

/// Fetch a job by id.
pub async fn get(db: &Db, job_id: &str) -> Result<JobRow> {
    sqlx::query_as::<_, JobRow>(
        r#"
        SELECT job_id, scope, tenant_id, project_id, agent_id, payload, status,
               priority, retry_count, max_retries, last_error, result,
               created_at, started_at, finished_at
        FROM jobs WHERE job_id = $1
        "#,
    )
    .bind(job_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("job {job_id}")))
}

/// Claim the next `limit` queued jobs for an executor.
///
/// Uses `FOR UPDATE SKIP LOCKED` so multiple schedulers can poll concurrently.
pub async fn claim_queued(db: &Db, executor_id: &str, limit: i64) -> Result<Vec<JobRow>> {
    let rows = sqlx::query_as::<_, JobRow>(
        r#"
        WITH picked AS (
            SELECT job_id FROM jobs
            WHERE status = 'queued'
            ORDER BY priority ASC, created_at ASC
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        UPDATE jobs
           SET status = 'running', started_at = now(), executor_id = $1
         WHERE job_id IN (SELECT job_id FROM picked)
        RETURNING job_id, scope, tenant_id, project_id, agent_id, payload, status,
                  priority, retry_count, max_retries, last_error, result,
                  created_at, started_at, finished_at
        "#,
    )
    .bind(executor_id)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows)
}

/// Mark a job succeeded with its result payload.
pub async fn complete(db: &Db, job_id: &str, result: serde_json::Value) -> Result<JobRow> {
    finish(db, job_id, status::SUCCEEDED, Some(result), None).await
}

/// Mark a job failed. Re-queues it when retries remain.
pub async fn fail(db: &Db, job_id: &str, error: &str) -> Result<JobRow> {
    let row = sqlx::query_as::<_, JobRow>(
        r#"
        UPDATE jobs
           SET retry_count = retry_count + 1,
               last_error  = $2,
               status      = CASE WHEN retry_count + 1 <= max_retries
                                  THEN 'queued' ELSE 'failed' END,
               finished_at = CASE WHEN retry_count + 1 <= max_retries
                                  THEN NULL ELSE now() END
         WHERE job_id = $1
        RETURNING job_id, scope, tenant_id, project_id, agent_id, payload, status,
                  priority, retry_count, max_retries, last_error, result,
                  created_at, started_at, finished_at
        "#,
    )
    .bind(job_id)
    .bind(error)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("job {job_id}")))?;

    Ok(row)
}

/// Cancel a job that has not reached a terminal state.
pub async fn cancel(db: &Db, job_id: &str, reason: &str) -> Result<JobRow> {
    finish(db, job_id, status::CANCELLED, None, Some(reason)).await
}

/// List jobs for a scope, newest first.
pub async fn list(
    db: &Db,
    scope: &Scope,
    status_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<JobRow>> {
    let rows = sqlx::query_as::<_, JobRow>(
        r#"
        SELECT job_id, scope, tenant_id, project_id, agent_id, payload, status,
               priority, retry_count, max_retries, last_error, result,
               created_at, started_at, finished_at
        FROM jobs
        WHERE ($1 = '' OR tenant_id = $1)
          AND ($2 = '' OR project_id = $2)
          AND ($3::TEXT IS NULL OR status = $3)
        ORDER BY created_at DESC
        LIMIT $4
        "#,
    )
    .bind(&scope.tenant_id)
    .bind(&scope.project_id)
    .bind(status_filter)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows)
}

async fn finish(
    db: &Db,
    job_id: &str,
    new_status: &str,
    result: Option<serde_json::Value>,
    error: Option<&str>,
) -> Result<JobRow> {
    sqlx::query_as::<_, JobRow>(
        r#"
        UPDATE jobs
           SET status = $2, result = COALESCE($3, result),
               last_error = COALESCE($4, last_error), finished_at = now()
         WHERE job_id = $1
        RETURNING job_id, scope, tenant_id, project_id, agent_id, payload, status,
                  priority, retry_count, max_retries, last_error, result,
                  created_at, started_at, finished_at
        "#,
    )
    .bind(job_id)
    .bind(new_status)
    .bind(result)
    .bind(error)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("job {job_id}")))
}
