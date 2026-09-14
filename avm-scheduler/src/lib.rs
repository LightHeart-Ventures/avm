//! Job scheduler.
//!
//! Responsibilities:
//! 1. Re-publish `queued` rows that never made it onto JetStream (crash gap).
//! 2. Enforce per-tenant quotas before dispatch.
//! 3. Reap stale `running` jobs whose executor died.
//! 4. Purge expired memories.

pub mod placement;

use std::time::Duration;

use avm_proto::types::{JobMessage, Scope};
use avm_queue::Publisher;
use avm_storage::{jobs, memories, Db};
use chrono::Utc;

/// Scheduler tuning.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// How often the reconcile loop ticks.
    pub tick_interval: Duration,
    /// Max jobs re-published per tick.
    pub batch_size: i64,
    /// A `running` job untouched for this long is considered abandoned.
    pub stale_after: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            batch_size: 100,
            stale_after: Duration::from_secs(900),
        }
    }
}

/// Reconciler that keeps Postgres and JetStream in agreement.
pub struct Scheduler {
    db: Db,
    queue: Publisher,
    cfg: SchedulerConfig,
}

impl Scheduler {
    pub fn new(db: Db, queue: Publisher, cfg: SchedulerConfig) -> Self {
        Self { db, queue, cfg }
    }

    /// Run the reconcile loop until cancelled.
    pub async fn run(&self) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.cfg.tick_interval);
        loop {
            ticker.tick().await;
            if let Err(err) = self.tick().await {
                tracing::error!(%err, "scheduler tick failed");
            }
        }
    }

    /// One reconcile pass.
    pub async fn tick(&self) -> anyhow::Result<()> {
        let republished = self.republish_queued().await?;
        let purged = memories::purge_expired(&self.db).await?;
        if republished > 0 || purged > 0 {
            tracing::info!(republished, purged, "scheduler tick");
        }
        Ok(())
    }

    /// Push `queued` rows back onto JetStream (at-least-once by design; the
    /// executor is idempotent on `job_id`).
    async fn republish_queued(&self) -> anyhow::Result<usize> {
        let rows = jobs::list(
            &self.db,
            &Scope::system(),
            Some(jobs::status::QUEUED),
            self.cfg.batch_size,
        )
        .await?;

        let mut count = 0usize;
        for row in rows {
            let msg = JobMessage {
                job_id: row.job_id.clone(),
                scope: Scope {
                    level: row.scope.clone(),
                    tenant_id: row.tenant_id.clone(),
                    project_id: row.project_id.clone(),
                    agent_id: row.agent_id.clone(),
                },
                agent_id: row.agent_id.clone(),
                payload: row.payload.to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            self.queue.publish_job(&msg).await?;
            count += 1;
        }
        Ok(count)
    }
}

pub mod quota {
    //! Quota gate — consulted before a job is admitted.
    //!
    //! TODO(avm): read `quotas` + `quota_usage` and reject over-limit tenants.

    /// Outcome of a quota check.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Decision {
        Allow,
        Throttle { retry_after_sec: u32 },
        Deny { reason: String },
    }
}
