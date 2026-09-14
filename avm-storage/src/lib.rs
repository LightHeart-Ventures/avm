//! PostgreSQL persistence for AVM.
//!
//! * [`db`] — pool construction and migration runner
//! * [`memories`] — scope-addressed memory read/write
//! * [`jobs`] — durable job CRUD and state transitions

pub mod db;
pub mod jobs;
pub mod memories;

pub use db::{connect, migrate, Db, DbConfig};

/// Errors surfaced by the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("scope {reader} may not access scope {target}")]
    ScopeDenied { reader: String, target: String },
    #[error("invalid input: {0}")]
    Invalid(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, StorageError>;
