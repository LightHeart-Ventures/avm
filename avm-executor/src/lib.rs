//! Executor: pulls jobs off JetStream and runs agent processes.

pub mod agent_runner;

use std::sync::Arc;

use avm_proto::types::ResultMessage;
use avm_queue::{Publisher, Subscriber};
use avm_storage::{jobs, Db};
use tokio::sync::Semaphore;

pub use agent_runner::{AgentRunner, RunOutcome, RunSpec};

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
            let handles = self.subscriber.fetch(self.cfg.batch_size).await?;
            if handles.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }

            for handle in handles {
                let _permit = self.permits.clone().acquire_owned().await?;
                let job = handle.job.clone();

                let outcome = self
                    .runner
                    .run(&RunSpec {
                        job_id: job.job_id.clone(),
                        agent_id: job.agent_id.clone(),
                        payload: job.payload.clone(),
                        scope: job.scope.clone(),
                    })
                    .await;

                let result = match outcome {
                    Ok(RunOutcome { stdout, .. }) => {
                        jobs::complete(
                            &self.db,
                            &job.job_id,
                            serde_json::json!({ "stdout": stdout }),
                        )
                        .await?;
                        ResultMessage {
                            job_id: job.job_id.clone(),
                            status: jobs::status::SUCCEEDED.to_string(),
                            result: stdout,
                            error: String::new(),
                        }
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        jobs::fail(&self.db, &job.job_id, &msg).await?;
                        ResultMessage {
                            job_id: job.job_id.clone(),
                            status: jobs::status::FAILED.to_string(),
                            result: String::new(),
                            error: msg,
                        }
                    }
                };

                self.queue
                    .publish_result(&job.scope.tenant_id, &job.scope.project_id, &result)
                    .await?;
                handle.ack().await?;
            }
        }
    }
}
