//! AVM control plane.
//!
//! Owns the gRPC surface (`TenantService`, `AgentService`, `JobService`,
//! `MemoryService`), writes durable state through [`avm_storage`] and hands
//! accepted work to [`avm_queue`].

pub mod services;

use avm_queue::Publisher;
use avm_storage::Db;

/// Process-wide dependencies handed to each gRPC service implementation.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub queue: Publisher,
}

impl AppState {
    pub fn new(db: Db, queue: Publisher) -> Self {
        Self { db, queue }
    }
}

/// Server runtime configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub database_url: String,
    pub nats_url: String,
    pub run_migrations: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:50051".to_string(),
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgres://avm:avm@localhost:5432/avm".to_string()),
            nats_url: avm_queue::nats_url_from_env(),
            run_migrations: true,
        }
    }
}
