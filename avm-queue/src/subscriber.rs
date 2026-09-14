//! JetStream durable pull consumer: hands jobs to the executor.
//!
//! Consumption is instrumented symmetrically with the publisher: a
//! `queue.consume` span per batch, a `direction="consume"` counter, and an
//! `avm_queue_depth` gauge refreshed from the consumer's pending count.

use async_nats::jetstream::{
    self, consumer::pull::Config as PullConfig, stream::Config as StreamConfig, Message,
};
use avm_otel::metrics::{self, QUEUE_DEPTH, QUEUE_MESSAGES, QUEUE_MESSAGE_SIZE};
use avm_otel::TraceContext;
use avm_proto::{subjects, JobMessage};
use futures::StreamExt;

use crate::{js_err, QueueError, Result};

/// Low-cardinality label for the jobs stream (see `publisher.rs`).
const JOBS_ROOT: &str = "avm.jobs";

/// A claimed job plus its JetStream message, so the caller controls the ack.
pub struct JobHandle {
    pub job: JobMessage,
    message: Message,
}

impl JobHandle {
    /// Acknowledge successful processing (message is removed from the stream).
    pub async fn ack(&self) -> Result<()> {
        self.message.ack().await.map_err(js_err)
    }

    /// Negative-ack so JetStream redelivers after the configured backoff.
    pub async fn nak(&self) -> Result<()> {
        self.message
            .ack_with(jetstream::AckKind::Nak(None))
            .await
            .map_err(js_err)
    }

    /// Extend the ack deadline for a long-running job.
    pub async fn in_progress(&self) -> Result<()> {
        self.message
            .ack_with(jetstream::AckKind::Progress)
            .await
            .map_err(js_err)
    }

    /// The trace context the dispatcher attached, if the trace was sampled.
    ///
    /// Consumers use this as the parent of their own spans, which is what makes
    /// `gateway.dispatch_job → executor.run_container` a single trace.
    pub fn trace_context(&self) -> Option<TraceContext> {
        let mut ctx = TraceContext::parse_traceparent(&self.job.traceparent)?;
        if !self.job.tracestate.is_empty() {
            ctx.trace_state = self.job.tracestate.clone();
        }
        Some(ctx)
    }
}

/// Durable pull consumer over the `AVM_JOBS` stream.
pub struct Subscriber {
    consumer: jetstream::consumer::Consumer<PullConfig>,
}

impl Subscriber {
    /// Connect and bind a durable consumer named `durable`.
    ///
    /// `filter` narrows delivery, e.g. `avm.jobs.t_acme.>`; pass `None` for all.
    pub async fn connect(url: &str, durable: &str, filter: Option<&str>) -> Result<Self> {
        let client = async_nats::connect(url).await?;
        let js = jetstream::new(client);

        let stream = js
            .get_or_create_stream(StreamConfig {
                name: subjects::JOBS_STREAM.to_string(),
                subjects: vec![subjects::JOBS_WILDCARD.to_string()],
                max_messages: 1_000_000,
                ..Default::default()
            })
            .await
            .map_err(js_err)?;

        let consumer = stream
            .get_or_create_consumer(
                durable,
                PullConfig {
                    durable_name: Some(durable.to_string()),
                    filter_subject: filter.unwrap_or(subjects::JOBS_WILDCARD).to_string(),
                    max_deliver: 5,
                    ..Default::default()
                },
            )
            .await
            .map_err(js_err)?;

        tracing::info!(durable, "jetstream consumer bound");
        Ok(Self { consumer })
    }

    /// Pull up to `batch` jobs. Malformed payloads are term'd, not retried.
    pub async fn fetch(&self, batch: usize) -> Result<Vec<JobHandle>> {
        let span = tracing::debug_span!(
            "queue.consume",
            otel.kind = "consumer",
            subject = JOBS_ROOT,
            batch,
        );
        let _entered = span.enter();

        let mut messages = self
            .consumer
            .fetch()
            .max_messages(batch)
            .messages()
            .await
            .map_err(js_err)?;

        let reg = metrics::registry();
        let mut out = Vec::with_capacity(batch);
        while let Some(next) = messages.next().await {
            let message = next.map_err(js_err)?;
            reg.histogram_observe(
                QUEUE_MESSAGE_SIZE,
                &[("subject", JOBS_ROOT)],
                message.payload.len() as f64,
            );
            reg.counter_inc(QUEUE_MESSAGES, &[("subject", JOBS_ROOT), ("direction", "consume")]);

            match serde_json::from_slice::<JobMessage>(&message.payload) {
                Ok(job) => out.push(JobHandle { job, message }),
                Err(err) => {
                    reg.counter_inc(
                        QUEUE_MESSAGES,
                        &[("subject", JOBS_ROOT), ("direction", "poison")],
                    );
                    tracing::error!(%err, "poison job envelope; terminating message");
                    message
                        .ack_with(jetstream::AckKind::Term)
                        .await
                        .map_err(js_err)?;
                }
            }
        }
        Ok(out)
    }

    /// Refresh `avm_queue_depth` from the consumer's pending count.
    ///
    /// Best-effort: a failure to read consumer info is a telemetry problem, not
    /// a job-processing problem, so it is logged and swallowed.
    pub async fn record_depth(&self) {
        let mut consumer = self.consumer.clone();
        match consumer.info().await {
            Ok(info) => {
                metrics::registry().gauge_set(
                    QUEUE_DEPTH,
                    &[("subject", JOBS_ROOT)],
                    info.num_pending as f64,
                );
            }
            Err(err) => tracing::debug!(%err, "could not read consumer depth"),
        }
    }

    /// Long-lived loop: pull batches forever and invoke `handler` per job.
    pub async fn run<F, Fut>(&self, batch: usize, mut handler: F) -> Result<()>
    where
        F: FnMut(JobHandle) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<(), QueueError>>,
    {
        loop {
            self.record_depth().await;
            for handle in self.fetch(batch).await? {
                handler(handle).await?;
            }
        }
    }
}
