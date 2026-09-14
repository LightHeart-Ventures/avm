//! JetStream durable pull consumer: hands jobs to the executor.

use async_nats::jetstream::{
    self,
    consumer::pull::Config as PullConfig,
    stream::Config as StreamConfig,
    Message,
};
use avm_proto::{subjects, JobMessage};
use futures::StreamExt;

use crate::{js_err, QueueError, Result};

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
        let mut messages = self
            .consumer
            .fetch()
            .max_messages(batch)
            .messages()
            .await
            .map_err(js_err)?;

        let mut out = Vec::with_capacity(batch);
        while let Some(next) = messages.next().await {
            let message = next.map_err(js_err)?;
            match serde_json::from_slice::<JobMessage>(&message.payload) {
                Ok(job) => out.push(JobHandle { job, message }),
                Err(err) => {
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

    /// Long-lived loop: pull batches forever and invoke `handler` per job.
    pub async fn run<F, Fut>(&self, batch: usize, mut handler: F) -> Result<()>
    where
        F: FnMut(JobHandle) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<(), QueueError>>,
    {
        loop {
            for handle in self.fetch(batch).await? {
                handler(handle).await?;
            }
        }
    }
}
