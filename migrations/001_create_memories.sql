-- 001_create_memories.sql
-- Scope-addressed agent memory store.
-- Scope hierarchy: system -> tenant -> project -> agent.

CREATE TABLE IF NOT EXISTS memories (
    id            BIGSERIAL PRIMARY KEY,
    memory_id     TEXT        NOT NULL,
    scope         TEXT        NOT NULL CHECK (scope IN ('system', 'tenant', 'project', 'agent')),
    tenant_id     TEXT        NOT NULL DEFAULT '',
    project_id    TEXT        NOT NULL DEFAULT '',
    agent_id      TEXT        NOT NULL DEFAULT '',
    content       TEXT        NOT NULL CHECK (octet_length(content) <= 32768),
    tags          TEXT[]      NOT NULL DEFAULT '{}',
    owner_id      TEXT        NOT NULL DEFAULT '',
    ttl_seconds   BIGINT,
    expires_at    TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE memories IS 'Scope-scoped agent memories; read ACL is scope-inheritance based.';
