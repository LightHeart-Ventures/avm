//! Model catalogue, pull history and per-node placement.
//!
//! Bytes live in the node-local content-addressed store (`avm-models`);
//! these tables are the cluster-wide view: what exists, who pulled it,
//! whether the checksum verified, and where it is cached right now.

use chrono::{DateTime, Utc};

use crate::{Db, Result};

/// A row of the `models` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ModelRow {
    pub digest: String,
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub size_bytes: i64,
    pub backend: String,
    pub artifact_type: String,
    pub labels: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A row of the `model_pulls` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ModelPullRow {
    pub pull_id: String,
    pub digest: String,
    pub node_id: String,
    pub status: String,
    pub bytes_pulled: i64,
    pub duration_ms: Option<i64>,
    pub checksum_ok: Option<bool>,
    pub source: String,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// A row of the `model_placements` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ModelPlacementRow {
    pub digest: String,
    pub node_id: String,
    pub status: String,
    pub serving: bool,
    pub endpoint: Option<String>,
    pub last_access: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input for [`upsert`].
#[derive(Debug, Clone)]
pub struct NewModel {
    pub digest: String,
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub size_bytes: i64,
    pub backend: String,
}

/// Pull lifecycle states (mirrors the SQL CHECK constraint).
pub mod pull_status {
    pub const PULLING: &str = "pulling";
    pub const VERIFIED: &str = "verified";
    pub const FAILED: &str = "failed";
    pub const EVICTED: &str = "evicted";
}

/// Residency states (mirrors the SQL CHECK constraint).
pub mod placement_status {
    pub const RESIDENT: &str = "resident";
    pub const CACHED: &str = "cached";
    pub const ABSENT: &str = "absent";
    pub const PULLING: &str = "pulling";
    pub const FAILED: &str = "failed";
}

/// Register (or refresh) a model in the catalogue. Idempotent on `digest`.
pub async fn upsert(db: &Db, model: &NewModel) -> Result<ModelRow> {
    let row = sqlx::query_as::<_, ModelRow>(
        r#"
        INSERT INTO models (digest, registry, repository, tag, size_bytes, backend)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (digest) DO UPDATE
           SET registry   = EXCLUDED.registry,
               repository = EXCLUDED.repository,
               tag        = COALESCE(EXCLUDED.tag, models.tag),
               size_bytes = GREATEST(EXCLUDED.size_bytes, models.size_bytes),
               backend    = COALESCE(NULLIF(EXCLUDED.backend, ''), models.backend),
               updated_at = now()
        RETURNING digest, registry, repository, tag, size_bytes, backend,
                  artifact_type, labels, created_at, updated_at
        "#,
    )
    .bind(&model.digest)
    .bind(&model.registry)
    .bind(&model.repository)
    .bind(&model.tag)
    .bind(model.size_bytes)
    .bind(&model.backend)
    .fetch_one(db)
    .await?;
    Ok(row)
}

/// Look up one model by digest.
pub async fn get(db: &Db, digest: &str) -> Result<Option<ModelRow>> {
    let row = sqlx::query_as::<_, ModelRow>(
        r#"
        SELECT digest, registry, repository, tag, size_bytes, backend,
               artifact_type, labels, created_at, updated_at
          FROM models
         WHERE digest = $1
        "#,
    )
    .bind(digest)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Open a pull record; returns the `pull_id`.
pub async fn begin_pull(
    db: &Db,
    pull_id: &str,
    digest: &str,
    node_id: &str,
    source: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO model_pulls (pull_id, digest, node_id, status, source)
        VALUES ($1, $2, $3, 'pulling', $4)
        ON CONFLICT (pull_id) DO NOTHING
        "#,
    )
    .bind(pull_id)
    .bind(digest)
    .bind(node_id)
    .bind(source)
    .execute(db)
    .await?;
    Ok(())
}

/// Close a pull record with the checksum verdict.
pub async fn finish_pull(
    db: &Db,
    pull_id: &str,
    checksum_ok: bool,
    bytes_pulled: i64,
    duration_ms: i64,
    error: Option<&str>,
) -> Result<()> {
    let status = if checksum_ok {
        pull_status::VERIFIED
    } else {
        pull_status::FAILED
    };
    sqlx::query(
        r#"
        UPDATE model_pulls
           SET status = $2, checksum_ok = $3, bytes_pulled = $4,
               duration_ms = $5, error = $6, finished_at = now()
         WHERE pull_id = $1
        "#,
    )
    .bind(pull_id)
    .bind(status)
    .bind(checksum_ok)
    .bind(bytes_pulled)
    .bind(duration_ms)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// Pull history for a model, newest first.
pub async fn pull_history(db: &Db, digest: &str, limit: i64) -> Result<Vec<ModelPullRow>> {
    let rows = sqlx::query_as::<_, ModelPullRow>(
        r#"
        SELECT pull_id, digest, node_id, status, bytes_pulled, duration_ms,
               checksum_ok, source, error, started_at, finished_at
          FROM model_pulls
         WHERE digest = $1
         ORDER BY started_at DESC
         LIMIT $2
        "#,
    )
    .bind(digest)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Publish residency for `(digest, node_id)` — called by the executor after
/// a pull completes and after a model server reports healthy.
pub async fn set_placement(
    db: &Db,
    digest: &str,
    node_id: &str,
    status: &str,
    serving: bool,
    endpoint: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO model_placements (digest, node_id, status, serving, endpoint, last_access, updated_at)
        VALUES ($1, $2, $3, $4, $5, now(), now())
        ON CONFLICT (digest, node_id) DO UPDATE
           SET status = EXCLUDED.status,
               serving = EXCLUDED.serving,
               endpoint = EXCLUDED.endpoint,
               updated_at = now()
        "#,
    )
    .bind(digest)
    .bind(node_id)
    .bind(status)
    .bind(serving)
    .bind(endpoint)
    .execute(db)
    .await?;
    Ok(())
}

/// All placements for a model — the scheduler's residency input.
pub async fn placements(db: &Db, digest: &str) -> Result<Vec<ModelPlacementRow>> {
    let rows = sqlx::query_as::<_, ModelPlacementRow>(
        r#"
        SELECT digest, node_id, status, serving, endpoint, last_access, updated_at
          FROM model_placements
         WHERE digest = $1
         ORDER BY serving DESC, status, node_id
        "#,
    )
    .bind(digest)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Everything a node currently holds — used to rebuild node labels on restart.
pub async fn node_placements(db: &Db, node_id: &str) -> Result<Vec<ModelPlacementRow>> {
    let rows = sqlx::query_as::<_, ModelPlacementRow>(
        r#"
        SELECT digest, node_id, status, serving, endpoint, last_access, updated_at
          FROM model_placements
         WHERE node_id = $1 AND status <> 'absent'
         ORDER BY last_access DESC
        "#,
    )
    .bind(node_id)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Drop a placement after GC evicted the blob.
pub async fn evict_placement(db: &Db, digest: &str, node_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM model_placements WHERE digest = $1 AND node_id = $2")
        .bind(digest)
        .bind(node_id)
        .execute(db)
        .await?;
    Ok(())
}
