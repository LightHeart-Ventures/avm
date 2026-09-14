//! Connection pool management and migrations.

use sqlx::postgres::{PgPoolOptions, Postgres};
use sqlx::Pool;
use std::time::Duration;

use crate::Result;

/// Shared handle to the Postgres pool.
pub type Db = Pool<Postgres>;

/// Pool tuning knobs (usually loaded from env).
#[derive(Debug, Clone)]
pub struct DbConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            url: "postgres://avm:avm@localhost:5432/avm".to_string(),
            max_connections: 16,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(10),
        }
    }
}

impl DbConfig {
    /// Read `DATABASE_URL` / `AVM_DB_MAX_CONNECTIONS` from the environment.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(url) = std::env::var("DATABASE_URL") {
            cfg.url = url;
        }
        if let Some(n) = std::env::var("AVM_DB_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        {
            cfg.max_connections = n;
        }
        cfg
    }
}

/// Build a pool from `cfg`.
pub async fn connect(cfg: &DbConfig) -> Result<Db> {
    tracing::info!(max_connections = cfg.max_connections, "connecting to postgres");
    let pool = PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .min_connections(cfg.min_connections)
        .acquire_timeout(cfg.acquire_timeout)
        .connect(&cfg.url)
        .await?;
    Ok(pool)
}

/// Apply every migration in `migrations/` (embedded at compile time).
pub async fn migrate(db: &Db) -> Result<()> {
    tracing::info!("running migrations");
    sqlx::migrate!("../migrations").run(db).await?;
    Ok(())
}

/// Cheap liveness probe used by `/healthz`.
pub async fn ping(db: &Db) -> Result<()> {
    sqlx::query("SELECT 1").execute(db).await?;
    Ok(())
}
