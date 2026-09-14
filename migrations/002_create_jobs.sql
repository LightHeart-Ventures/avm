-- 002_create_jobs.sql
-- Durable job records. NATS JetStream carries the delivery; Postgres is
-- the source of truth for status, retries and results.

CREATE TABLE IF NOT EXISTS jobs (
    job_id          TEXT        PRIMARY KEY,
    scope           TEXT        NOT NULL CHECK (scope IN ('system', 'tenant', 'project', 'agent')),
    tenant_id       TEXT        NOT NULL DEFAULT '',
    project_id      TEXT        NOT NULL DEFAULT '',
    agent_id        TEXT        NOT NULL DEFAULT '',
    payload         JSONB       NOT NULL DEFAULT '{}'::jsonb,
    status          TEXT        NOT NULL DEFAULT 'queued'
                                CHECK (status IN ('queued','running','succeeded','failed','cancelled')),
    priority        INT         NOT NULL DEFAULT 100,
    retry_count     INT         NOT NULL DEFAULT 0,
    max_retries     INT         NOT NULL DEFAULT 3,
    last_error      TEXT,
    result          JSONB,
    idempotency_key TEXT,
    executor_id     TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ
);

COMMENT ON TABLE jobs IS 'Durable job state; JetStream handles at-least-once delivery.';
