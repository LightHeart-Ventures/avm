//! NATS JetStream transport for AVM.
//!
//! * [`publisher`] — enqueue [`avm_proto::JobMessage`] / [`avm_proto::ResultMessage`]
//! * [`subscriber`] — durable pull consumers for executors and result collectors

pub mod publisher;
pub mod subscriber;

pub use publisher::Publisher;
pub use subscriber::{JobHandle, Subscriber};

/// Errors surfaced by the queue layer.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("nats connect error: {0}")]
    Connect(#[from] async_nats::ConnectError),
    #[error("jetstream error: {0}")]
    JetStream(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, QueueError>;

/// Default NATS endpoint (matches `docker-compose.yml`).
pub const DEFAULT_NATS_URL: &str = "nats://localhost:4222";

/// Read `NATS_URL` from the environment, falling back to the compose default.
pub fn nats_url_from_env() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| DEFAULT_NATS_URL.to_string())
}

pub(crate) fn js_err<E: std::fmt::Display>(e: E) -> QueueError {
    QueueError::JetStream(e.to_string())
}
