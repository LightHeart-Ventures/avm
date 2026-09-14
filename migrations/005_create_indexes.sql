-- 005_create_indexes.sql
-- Uniqueness constraints and read-path indexes.

-- memories: one row per (scope tuple, memory_id); scope+memory_id is the hot lookup.
CREATE UNIQUE INDEX IF NOT EXISTS ux_memories_scope_memory_id
    ON memories (scope, tenant_id, project_id, agent_id, memory_id);

CREATE INDEX IF NOT EXISTS ix_memories_scope_memory_id
    ON memories (scope, memory_id);

CREATE INDEX IF NOT EXISTS ix_memories_tenant_project
    ON memories (tenant_id, project_id);

CREATE INDEX IF NOT EXISTS ix_memories_tags
    ON memories USING GIN (tags);

CREATE INDEX IF NOT EXISTS ix_memories_expires_at
    ON memories (expires_at) WHERE expires_at IS NOT NULL;

-- jobs: scheduler polls queued work by (status, priority, created_at).
CREATE INDEX IF NOT EXISTS ix_jobs_status_priority
    ON jobs (status, priority, created_at);

CREATE INDEX IF NOT EXISTS ix_jobs_scope
    ON jobs (tenant_id, project_id, status);

CREATE INDEX IF NOT EXISTS ix_jobs_agent
    ON jobs (agent_id, status);

CREATE UNIQUE INDEX IF NOT EXISTS ux_jobs_idempotency_key
    ON jobs (tenant_id, idempotency_key) WHERE idempotency_key IS NOT NULL;

-- audit_logs: time-ordered scans per scope.
CREATE UNIQUE INDEX IF NOT EXISTS ux_audit_logs_event_id
    ON audit_logs (event_id);

CREATE INDEX IF NOT EXISTS ix_audit_logs_scope_created
    ON audit_logs (tenant_id, project_id, created_at DESC);

CREATE INDEX IF NOT EXISTS ix_audit_logs_action
    ON audit_logs (action, created_at DESC);

-- quotas: one config row per (tenant, project).
CREATE UNIQUE INDEX IF NOT EXISTS ux_quotas_tenant_project
    ON quotas (tenant_id, project_id);

CREATE UNIQUE INDEX IF NOT EXISTS ux_quota_usage_window
    ON quota_usage (tenant_id, project_id, window_start);
