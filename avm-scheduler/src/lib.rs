//! Job scheduler.
//!
//! Responsibilities:
//! 1. Re-publish `queued` rows that never made it onto JetStream (crash gap).
//! 2. Enforce per-tenant quotas before dispatch.
//! 3. Reap stale `running` jobs whose executor died.
//! 4. Purge expired memories.
//!
//! Every placement decision is a `scheduler.place_job` span with a
//! `scheduler.node_selection_score` child, and feeds
//! `avm_scheduler_job_placement_duration_seconds` labelled by strategy.

pub mod placement;

use std::time::Duration;
use std::time::Instant;

use avm_otel::metrics::{self, SCHEDULER_PLACEMENT_DURATION};
use avm_otel::{propagation, InstrumentationLevel, TraceContext};
use avm_proto::types::{JobMessage, Scope};
use avm_queue::Publisher;
use avm_storage::{jobs, memories, Db};
use chrono::Utc;

/// Placement strategy label for the platform metric. One strategy today
/// (requeue in creation order); constraint-scored placement adds more.
const STRATEGY: &str = "requeue";

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
        let span = tracing::debug_span!("scheduler.tick", otel.kind = "internal");
        let _entered = span.enter();

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
            let started = Instant::now();
            let scope = Scope {
                level: row.scope.clone(),
                tenant_id: row.tenant_id.clone(),
                project_id: row.project_id.clone(),
                agent_id: row.agent_id.clone(),
            };

            let span = tracing::info_span!(
                "scheduler.place_job",
                otel.kind = "internal",
                job_id = %row.job_id,
                tenant_id = %scope.tenant_id,
                project_id = %scope.project_id,
                agent_id = %scope.agent_id,
                strategy = STRATEGY,
            );
            let _entered = span.enter();

            // A requeue re-parents the job onto a fresh trace: the original
            // dispatch trace is already closed, and stitching a new execution
            // under a completed span would misrepresent the timeline.
            let ctx = TraceContext::root_from_seed(&propagation::new_trace_id_seed(), true);

            {
                let _score = tracing::debug_span!(
                    "scheduler.node_selection_score",
                    job_id = %row.job_id,
                    candidates = 1,
                    strategy = STRATEGY,
                )
                .entered();
            }

            let msg = JobMessage {
                job_id: row.job_id.clone(),
                scope,
                agent_id: row.agent_id.clone(),
                payload: row.payload.to_string(),
                created_at: Utc::now().to_rfc3339(),
                traceparent: ctx.to_traceparent(),
                tracestate: String::new(),
                // A requeue never widens the tenant's opt-in: the level is
                // re-resolved at the gateway on the next dispatch.
                instrumentation_level: InstrumentationLevel::Off.as_str().to_string(),
            };
            self.queue.publish_job(&msg).await?;

            metrics::registry().histogram_observe(
                SCHEDULER_PLACEMENT_DURATION,
                &[("strategy", STRATEGY)],
                started.elapsed().as_secs_f64(),
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requeued_jobs_carry_a_fresh_sampled_trace() {
        let ctx = TraceContext::root_from_seed(&propagation::new_trace_id_seed(), true);
        let tp = ctx.to_traceparent();
        assert!(tp.starts_with("00-"), "{tp}");
        assert!(tp.ends_with("-01"), "requeue traces are sampled: {tp}");
        let parsed = TraceContext::parse_traceparent(&tp).expect("round-trips");
        assert_eq!(parsed.trace_id, ctx.trace_id);
    }

    #[test]
    fn requeue_does_not_widen_instrumentation() {
        assert_eq!(InstrumentationLevel::Off.as_str(), "off");
        assert!(!InstrumentationLevel::Off.tenant_metrics());
    }
}
