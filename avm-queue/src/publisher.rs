//! JetStream publisher: puts jobs and results on the bus.
//!
//! Every publish is a span (`queue.publish_job` / `queue.publish_result`) and
//! feeds two platform metric families: message size and message count. Series
//! are labelled by the **stream root** (`avm.jobs`, `avm.results`) rather than
//! the fully-qualified subject, because the latter embeds tenant and project
//! ids and would make metric cardinality grow with the customer base.

use async_nats::jetstream::{self, stream::Config as StreamConfig};
use avm_otel::metrics::{self, QUEUE_MESSAGES, QUEUE_MESSAGE_SIZE};
use avm_proto::{subjects, JobMessage, ResultMessage};

use crate::{js_err, Result};

/// Low-cardinality label for the jobs stream.
const JOBS_ROOT: &str = "avm.jobs";
/// Low-cardinality label for the results stream.
const RESULTS_ROOT: &str = "avm.results";

/// Publishes AVM envelopes to JetStream, creating the streams on first use.
#[derive(Clone)]
pub struct Publisher {
    js: jetstream::Context,
}

impl Publisher {
    /// Connect to NATS and ensure the AVM streams exist.
    pub async fn connect(url: &str) -> Result<Self> {
        let client = async_nats::connect(url).await?;
        let js = jetstream::new(client);
        let publisher = Self { js };
        publisher.ensure_streams().await?;
        Ok(publisher)
    }

    /// Idempotently create `AVM_JOBS` and `AVM_RESULTS`.
    pub async fn ensure_streams(&self) -> Result<()> {
        for (name, subject) in [
            (subjects::JOBS_STREAM, subjects::JOBS_WILDCARD),
            (subjects::RESULTS_STREAM, subjects::RESULTS_WILDCARD),
        ] {
            self.js
                .get_or_create_stream(StreamConfig {
                    name: name.to_string(),
                    subjects: vec![subject.to_string()],
                    max_messages: 1_000_000,
                    ..Default::default()
                })
                .await
                .map_err(js_err)?;
            tracing::debug!(stream = name, subject, "jetstream stream ready");
        }
        Ok(())
    }

    /// Publish a job to `avm.jobs.<tenant>.<project>` and await the ack.
    ///
    /// The job's `traceparent` travels inside the envelope, so the trace
    /// survives JetStream replay and is visible in `nats stream view`.
    pub async fn publish_job(&self, job: &JobMessage) -> Result<()> {
        let span = tracing::info_span!(
            "queue.publish_job",
            otel.kind = "producer",
            job_id = %job.job_id,
            tenant_id = %job.scope.tenant_id,
            project_id = %job.scope.project_id,
            instrumentation_level = job.instrumentation(),
            subject = JOBS_ROOT,
        );
        let _entered = span.enter();

        let subject = subjects::job_subject(&job.scope.tenant_id, &job.scope.project_id);
        let payload = serde_json::to_vec(job)?;
        record_publish(JOBS_ROOT, payload.len());

        self.js
            .publish(subject.clone(), payload.into())
            .await
            .map_err(js_err)?
            .await
            .map_err(js_err)?;
        tracing::info!(job_id = %job.job_id, %subject, "job published");
        Ok(())
    }

    /// Publish a result to `avm.results.<tenant>.<project>` and await the ack.
    pub async fn publish_result(
        &self,
        tenant_id: &str,
        project_id: &str,
        result: &ResultMessage,
    ) -> Result<()> {
        let span = tracing::info_span!(
            "queue.publish_result",
            otel.kind = "producer",
            job_id = %result.job_id,
            tenant_id = %tenant_id,
            project_id = %project_id,
            outcome = %result.status,
            subject = RESULTS_ROOT,
        );
        let _entered = span.enter();

        let subject = subjects::result_subject(tenant_id, project_id);
        let payload = serde_json::to_vec(result)?;
        record_publish(RESULTS_ROOT, payload.len());

        self.js
            .publish(subject.clone(), payload.into())
            .await
            .map_err(js_err)?
            .await
            .map_err(js_err)?;
        tracing::info!(job_id = %result.job_id, %subject, status = %result.status, "result published");
        Ok(())
    }

    /// Escape hatch for callers needing the raw JetStream context.
    pub fn context(&self) -> &jetstream::Context {
        &self.js
    }
}

/// Observe one outbound message against the platform families.
fn record_publish(root: &str, bytes: usize) {
    let reg = metrics::registry();
    reg.histogram_observe(QUEUE_MESSAGE_SIZE, &[("subject", root)], bytes as f64);
    reg.counter_inc(QUEUE_MESSAGES, &[("subject", root), ("direction", "publish")]);
}
