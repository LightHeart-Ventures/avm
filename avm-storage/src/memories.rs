//! Scope-addressed memory read/write.
//!
//! ACL rule: a reader at scope level *L* may resolve memories written at *L*
//! and at every ancestor level ( system ⊃ tenant ⊃ project ⊃ agent ).

use avm_proto::types::{Scope, ScopeLevel};
use chrono::{DateTime, Utc};

use crate::{Db, Result, StorageError};

/// A row of the `memories` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemoryRow {
    pub id: i64,
    pub memory_id: String,
    pub scope: String,
    pub tenant_id: String,
    pub project_id: String,
    pub agent_id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub owner_id: String,
    pub ttl_seconds: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input for [`write`].
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub memory_id: String,
    pub scope: Scope,
    pub content: String,
    pub tags: Vec<String>,
    pub owner_id: String,
    pub ttl_seconds: Option<i64>,
}

const MAX_CONTENT_BYTES: usize = 32 * 1024;

/// Upsert a memory on its `(scope tuple, memory_id)` unique key.
pub async fn write(db: &Db, mem: &NewMemory) -> Result<MemoryRow> {
    if mem.content.len() > MAX_CONTENT_BYTES {
        return Err(StorageError::Invalid(format!(
            "content exceeds {MAX_CONTENT_BYTES} bytes"
        )));
    }

    let row = sqlx::query_as::<_, MemoryRow>(
        r#"
        INSERT INTO memories
            (memory_id, scope, tenant_id, project_id, agent_id,
             content, tags, owner_id, ttl_seconds, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
                CASE WHEN $9::BIGINT IS NULL THEN NULL
                     ELSE now() + ($9::BIGINT * INTERVAL '1 second') END)
        ON CONFLICT (scope, tenant_id, project_id, agent_id, memory_id)
        DO UPDATE SET content    = EXCLUDED.content,
                      tags       = EXCLUDED.tags,
                      owner_id   = EXCLUDED.owner_id,
                      ttl_seconds= EXCLUDED.ttl_seconds,
                      expires_at = EXCLUDED.expires_at,
                      updated_at = now()
        RETURNING id, memory_id, scope, tenant_id, project_id, agent_id,
                  content, tags, owner_id, ttl_seconds, created_at, updated_at
        "#,
    )
    .bind(&mem.memory_id)
    .bind(&mem.scope.level)
    .bind(&mem.scope.tenant_id)
    .bind(&mem.scope.project_id)
    .bind(&mem.scope.agent_id)
    .bind(&mem.content)
    .bind(&mem.tags)
    .bind(&mem.owner_id)
    .bind(mem.ttl_seconds)
    .fetch_one(db)
    .await?;

    Ok(row)
}

/// Read one memory, enforcing the scope ACL before touching the database.
pub async fn read(
    db: &Db,
    reader: &Scope,
    target: &Scope,
    memory_id: &str,
) -> Result<MemoryRow> {
    authorize(reader, target)?;

    let row = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT id, memory_id, scope, tenant_id, project_id, agent_id,
               content, tags, owner_id, ttl_seconds, created_at, updated_at
        FROM memories
        WHERE scope = $1 AND tenant_id = $2 AND project_id = $3
          AND agent_id = $4 AND memory_id = $5
          AND (expires_at IS NULL OR expires_at > now())
        "#,
    )
    .bind(&target.level)
    .bind(&target.tenant_id)
    .bind(&target.project_id)
    .bind(&target.agent_id)
    .bind(memory_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("memory {memory_id}")))?;

    Ok(row)
}

/// List every memory visible to `reader` (own scope + inherited ancestors).
pub async fn list_visible(
    db: &Db,
    reader: &Scope,
    tags: &[String],
    limit: i64,
) -> Result<Vec<MemoryRow>> {
    let level = reader
        .level()
        .ok_or_else(|| StorageError::Invalid(format!("unknown scope level {}", reader.level)))?;

    let visible: Vec<String> = level
        .visible()
        .iter()
        .map(|l| l.as_str().to_string())
        .collect();

    let rows = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT id, memory_id, scope, tenant_id, project_id, agent_id,
               content, tags, owner_id, ttl_seconds, created_at, updated_at
        FROM memories
        WHERE scope = ANY($1)
          AND (tenant_id  = '' OR tenant_id  = $2)
          AND (project_id = '' OR project_id = $3)
          AND (agent_id   = '' OR agent_id   = $4)
          AND ($5::TEXT[] = '{}' OR tags && $5::TEXT[])
          AND (expires_at IS NULL OR expires_at > now())
        ORDER BY updated_at DESC
        LIMIT $6
        "#,
    )
    .bind(&visible)
    .bind(&reader.tenant_id)
    .bind(&reader.project_id)
    .bind(&reader.agent_id)
    .bind(tags)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows)
}

/// Delete a memory the reader owns/can reach. Returns `true` if a row went away.
pub async fn delete(db: &Db, reader: &Scope, memory_id: &str) -> Result<bool> {
    let affected = sqlx::query(
        r#"
        DELETE FROM memories
        WHERE memory_id = $1 AND scope = $2 AND tenant_id = $3
          AND project_id = $4 AND agent_id = $5
        "#,
    )
    .bind(memory_id)
    .bind(&reader.level)
    .bind(&reader.tenant_id)
    .bind(&reader.project_id)
    .bind(&reader.agent_id)
    .execute(db)
    .await?
    .rows_affected();

    Ok(affected > 0)
}

/// Purge expired rows; called periodically by the scheduler.
pub async fn purge_expired(db: &Db) -> Result<u64> {
    let affected = sqlx::query("DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at <= now()")
        .execute(db)
        .await?
        .rows_affected();
    Ok(affected)
}

fn authorize(reader: &Scope, target: &Scope) -> Result<()> {
    let r = reader
        .level()
        .ok_or_else(|| StorageError::Invalid(format!("unknown scope level {}", reader.level)))?;
    let t = target
        .level()
        .ok_or_else(|| StorageError::Invalid(format!("unknown scope level {}", target.level)))?;

    let level_ok = r.can_read(t);
    let tenant_ok = target.tenant_id.is_empty() || target.tenant_id == reader.tenant_id;
    let project_ok = target.project_id.is_empty() || target.project_id == reader.project_id;
    let agent_ok = t != ScopeLevel::Agent || target.agent_id == reader.agent_id;

    if level_ok && tenant_ok && project_ok && agent_ok {
        Ok(())
    } else {
        Err(StorageError::ScopeDenied {
            reader: reader.level.clone(),
            target: target.level.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_reader_may_read_tenant_memory() {
        let reader = Scope::project("t_acme", "b_payments");
        let target = Scope::tenant("t_acme");
        assert!(authorize(&reader, &target).is_ok());
    }

    #[test]
    fn cross_tenant_reads_are_denied() {
        let reader = Scope::project("t_acme", "b_payments");
        let target = Scope::tenant("t_competitor");
        assert!(authorize(&reader, &target).is_err());
    }

    #[test]
    fn tenant_reader_may_not_read_project_memory() {
        let reader = Scope::tenant("t_acme");
        let target = Scope::project("t_acme", "b_payments");
        assert!(authorize(&reader, &target).is_err());
    }
}
