-- 004_create_quotas.sql
-- Tenant / project quota configuration plus rolling usage counters.

CREATE TABLE IF NOT EXISTS quotas (
    id                    BIGSERIAL PRIMARY KEY,
    tenant_id             TEXT        NOT NULL,
    project_id            TEXT        NOT NULL DEFAULT '',
    max_concurrent_agents INT         NOT NULL DEFAULT 10,
    max_daily_tokens      BIGINT      NOT NULL DEFAULT 1000000,
    max_monthly_spend_usd NUMERIC(12,4) NOT NULL DEFAULT 100.0,
    max_jobs_per_minute   INT         NOT NULL DEFAULT 60,
    allowed_models        TEXT[]      NOT NULL DEFAULT '{}',
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS quota_usage (
    id                BIGSERIAL PRIMARY KEY,
    tenant_id         TEXT        NOT NULL,
    project_id        TEXT        NOT NULL DEFAULT '',
    window_start      TIMESTAMPTZ NOT NULL,
    window_end        TIMESTAMPTZ NOT NULL,
    concurrent_agents INT         NOT NULL DEFAULT 0,
    tokens_used       BIGINT      NOT NULL DEFAULT 0,
    spend_usd         NUMERIC(12,4) NOT NULL DEFAULT 0.0,
    jobs_submitted    INT         NOT NULL DEFAULT 0,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE quotas IS 'Declared limits. project_id = '''' means tenant-wide default.';
COMMENT ON TABLE quota_usage IS 'Rolling usage counters per billing/rate window.';
