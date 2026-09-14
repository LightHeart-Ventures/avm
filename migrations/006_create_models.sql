-- 006_create_models.sql
-- Model distribution metadata. The bytes live in the node-local
-- content-addressed store (/var/lib/avm/models/blobs); Postgres is the
-- source of truth for provenance, pull history and where each model is cached.

CREATE TABLE IF NOT EXISTS models (
    digest          TEXT        PRIMARY KEY,            -- sha256:<64 hex>
    registry        TEXT        NOT NULL DEFAULT '',    -- ghcr.io
    repository      TEXT        NOT NULL DEFAULT '',    -- lightheart/qwen3-8b
    tag             TEXT,                               -- advisory only
    size_bytes      BIGINT      NOT NULL DEFAULT 0,
    backend         TEXT        NOT NULL DEFAULT '',    -- llama.cpp | vllm | tgi
    artifact_type   TEXT        NOT NULL DEFAULT 'application/vnd.avm.model.v1+json',
    labels          JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT ck_models_digest_algo CHECK (digest LIKE 'sha256:%')
);

COMMENT ON TABLE models IS 'Catalogue of known model artifacts, keyed by content digest.';

-- Append-only pull + verification audit trail.
CREATE TABLE IF NOT EXISTS model_pulls (
    pull_id         TEXT        PRIMARY KEY,
    digest          TEXT        NOT NULL REFERENCES models (digest) ON DELETE CASCADE,
    node_id         TEXT        NOT NULL,
    status          TEXT        NOT NULL DEFAULT 'pulling'
                                CHECK (status IN ('pulling','verified','failed','evicted')),
    bytes_pulled    BIGINT      NOT NULL DEFAULT 0,
    duration_ms     BIGINT,
    checksum_ok     BOOLEAN,
    source          TEXT        NOT NULL DEFAULT '',    -- registry host or mirror
    error           TEXT,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at     TIMESTAMPTZ
);

COMMENT ON TABLE model_pulls IS 'Per-node pull history and checksum verification results.';

-- Current residency per (model, node). Written by the executor after startup
-- and read by the scheduler to build node labels.
CREATE TABLE IF NOT EXISTS model_placements (
    digest          TEXT        NOT NULL REFERENCES models (digest) ON DELETE CASCADE,
    node_id         TEXT        NOT NULL,
    status          TEXT        NOT NULL DEFAULT 'absent'
                                CHECK (status IN ('resident','cached','absent','pulling','failed')),
    serving         BOOLEAN     NOT NULL DEFAULT FALSE, -- a model server is live on this node
    endpoint        TEXT,                               -- http://node:8081 when serving
    last_access     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (digest, node_id)
);

COMMENT ON TABLE model_placements IS 'Where each model is cached/served; drives placement scoring.';

CREATE INDEX IF NOT EXISTS ix_model_pulls_digest_started
    ON model_pulls (digest, started_at DESC);

CREATE INDEX IF NOT EXISTS ix_model_pulls_node
    ON model_pulls (node_id, started_at DESC);

CREATE INDEX IF NOT EXISTS ix_model_placements_node
    ON model_placements (node_id, status);

CREATE INDEX IF NOT EXISTS ix_model_placements_serving
    ON model_placements (digest, serving) WHERE serving;
