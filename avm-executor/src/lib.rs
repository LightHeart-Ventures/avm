//! Executor: pulls jobs off JetStream and runs agent processes.
//!
//! Trace shape per job:
//!
//! ```text
//! executor.run_container        (child of the dispatching gateway span)
//!   ├─ container.pull_image
//!   ├─ container.run
//!   └─ container.cleanup
//! queue.publish_result
//! ```
//!
//! Platform metrics are unconditional; tenant-labelled series
//! (`avm_tenant_job_*`) are only recorded when the job envelope carries an
//! instrumentation level of `basic` or `detailed`.

pub mod agent_runner;
pub mod model_server;

use std::sync::Arc;
use std::time::Instant;

use avm_otel::metrics::{
    self, EXECUTOR_CONTAINER_DURATION, TENANT_JOB_COUNT, TENANT_JOB_DURATION,
};
use avm_otel::InstrumentationLevel;
use avm_proto::types::ResultMessage;
use avm_queue::{Publisher, Subscriber};
use avm_storage::{jobs, Db};
use tokio::sync::Semaphore;

pub use agent_runner::{AgentRunner, RunOutcome, RunSpec};
pub use model_server::{ExecutorKind, ModelServerSpec, Mount};

/// Executor backend label for `avm_executor_container_duration_seconds`.
///
/// One value today (fork/exec); OCI and Firecracker backends add their own.
const EXECUTOR_TYPE: &str = "process";

/// Executor tuning.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Stable id used for the durable consumer and `jobs.executor_id`.
    pub executor_id: String,
    /// Max concurrent agent processes.
    pub max_concurrency: usize,
    /// JetStream pull batch size.
    pub batch_size: usize,
    /// Hard wall-clock limit per job.
    pub wall_time_sec: u64,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            executor_id: format!("ex_{}", uuid::Uuid::new_v4().simple()),
            max_concurrency: 8,
            batch_size: 16,
            wall_time_sec: 900,
        }
    }
}

/// Job consumption loop with a bounded process pool.
pub struct Executor {
    db: Db,
    queue: Publisher,
    subscriber: Subscriber,
    runner: AgentRunner,
    permits: Arc<Semaphore>,
    cfg: ExecutorConfig,
}

impl Executor {
    pub fn new(db: Db, queue: Publisher, subscriber: Subscriber, cfg: ExecutorConfig) -> Self {
        Self {
            db,
            queue,
            subscriber,
            runner: AgentRunner::new(cfg.wall_time_sec),
            permits: Arc::new(Semaphore::new(cfg.max_concurrency)),
            cfg,
        }
    }

    /// Pull → execute → persist → publish result, forever.
    pub async fn run(&self) -> anyhow::Result<()> {
        tracing::info!(
            executor_id = %self.cfg.executor_id,
            concurrency = self.cfg.max_concurrency,
            "executor loop started"
        );

        loop {
            self.subscriber.record_depth().await;
            let handles = self.subscriber.fetch(self.cfg.batch_size).await?;
            if handles.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }

            for handle in handles {
                let _permit = self.permits.clone().acquire_owned().await?;
                let job = handle.job.clone();
                let level = InstrumentationLevel::parse(job.instrumentation());
                let trace = handle.trace_context();

                let span = tracing::info_span!(
                    "executor.run_container",
                    otel.kind = "consumer",
                    job_id = %job.job_id,
                    agent_id = %job.agent_id,
                    tenant_id = %job.scope.tenant_id,
                    project_id = %job.scope.project_id,
                    executor_id = %self.cfg.executor_id,
                    instrumentation_level = level.as_str(),
                    trace_id = trace.as_ref().map(|c| c.trace_id.as_str()).unwrap_or(""),
                    outcome = tracing::field::Empty,
                );
                let _entered = span.enter();

                let started = Instant::now();
                let outcome = self
                    .runner
                    .run(&RunSpec {
                        job_id: job.job_id.clone(),
                        agent_id: job.agent_id.clone(),
                        payload: job.payload.clone(),
                        scope: job.scope.clone(),
                        trace: trace.clone(),
                        instrumentation: level,
                    })
                    .await;

                let elapsed = started.elapsed().as_secs_f64();
                let traceparent = trace.as_ref().map(|c| c.to_traceparent()).unwrap_or_default();

                let (result, outcome_label) = match outcome {
                    Ok(RunOutcome { stdout, .. }) => {
                        jobs::complete(
                            &self.db,
                            &job.job_id,
                            serde_json::json!({ "stdout": stdout }),
                        )
                        .await?;
                        (
                            ResultMessage {
                                job_id: job.job_id.clone(),
                                status: jobs::status::SUCCEEDED.to_string(),
                                result: stdout,
                                error: String::new(),
                                traceparent: traceparent.clone(),
                                ..Default::default()
                            },
                            "succeeded",
                        )
                    }
                    Err(err) => {
                        let label = err.outcome();
                        let msg = err.to_string();
                        jobs::fail(&self.db, &job.job_id, &msg).await?;
                        (
                            ResultMessage {
                                job_id: job.job_id.clone(),
                                status: jobs::status::FAILED.to_string(),
                                result: String::new(),
                                error: msg,
                                traceparent: traceparent.clone(),
                                ..Default::default()
                            },
                            label,
                        )
                    }
                };

                span.record("outcome", outcome_label);
                self.record_job_metrics(&job.scope, level, outcome_label, elapsed);

                self.queue
                    .publish_result(&job.scope.tenant_id, &job.scope.project_id, &result)
                    .await?;
                handle.ack().await?;
            }
        }
    }

    /// Platform metrics always; tenant-labelled metrics only on opt-in.
    fn record_job_metrics(
        &self,
        scope: &avm_proto::types::Scope,
        level: InstrumentationLevel,
        outcome: &str,
        elapsed: f64,
    ) {
        let reg = metrics::registry();
        reg.histogram_observe(
            EXECUTOR_CONTAINER_DURATION,
            &[("executor_type", EXECUTOR_TYPE), ("outcome", outcome)],
            elapsed,
        );

        if !level.tenant_metrics() {
            return;
        }
        let labels = [
            ("tenant_id", scope.tenant_id.as_str()),
            ("project_id", scope.project_id.as_str()),
        ];
        reg.histogram_observe(TENANT_JOB_DURATION, &labels, elapsed);
        reg.counter_inc(
            TENANT_JOB_COUNT,
            &[
                ("tenant_id", scope.tenant_id.as_str()),
                ("project_id", scope.project_id.as_str()),
                ("outcome", outcome),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use avm_otel::{metrics, InstrumentationLevel};

    /// The opt-in contract, asserted at the level that decides it: `off` must
    /// never permit a tenant-labelled series.
    #[test]
    fn off_emits_no_tenant_series() {
        assert!(!InstrumentationLevel::Off.tenant_metrics());
        assert!(InstrumentationLevel::Basic.tenant_metrics());
        assert!(InstrumentationLevel::Detailed.tenant_metrics());

        let reg = metrics::Registry::new();
        reg.histogram_observe(
            metrics::EXECUTOR_CONTAINER_DURATION,
            &[("executor_type", "process"), ("outcome", "succeeded")],
            0.5,
        );
        // No tenant write happened, so the family has no series at all.
        assert_eq!(reg.series_count(metrics::TENANT_JOB_DURATION), 0);
        assert_eq!(reg.series_count(metrics::EXECUTOR_CONTAINER_DURATION), 1);
    }
}
