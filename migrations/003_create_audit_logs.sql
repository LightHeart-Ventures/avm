-- 003_create_audit_logs.sql
-- Append-only audit trail for every control-plane mutation and agent action.

CREATE TABLE IF NOT EXISTS audit_logs (
    id           BIGSERIAL PRIMARY KEY,
    event_id     TEXT        NOT NULL,
    actor_id     TEXT        NOT NULL DEFAULT '',   -- user, agent or "system"
    actor_type   TEXT        NOT NULL DEFAULT 'system'
                             CHECK (actor_type IN ('user','agent','system','executor')),
    action       TEXT        NOT NULL,              -- job.submit, memory.write, tenant.update ...
    scope        TEXT        NOT NULL DEFAULT 'system',
    tenant_id    TEXT        NOT NULL DEFAULT '',
    project_id   TEXT        NOT NULL DEFAULT '',
    agent_id     TEXT        NOT NULL DEFAULT '',
    entity_type  TEXT        NOT NULL DEFAULT '',
    entity_id    TEXT        NOT NULL DEFAULT '',
    detail       JSONB       NOT NULL DEFAULT '{}'::jsonb,
    trace_id     TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE audit_logs IS 'Append-only. Never UPDATE or DELETE rows here.';
