-- 006_add_agent_isolation_rls.sql
--
-- Phase 4 of the network-isolation rollout (see IMPLEMENTATION_PLAN.md
-- § "Network Isolation & A2A Security"): Postgres Row-Level Security.
--
-- THREAT THIS LAYER ADDRESSES
--   An application bug, a missing WHERE clause, or a compromised service
--   credential that reaches the database directly. The gateway (Phase 1),
--   NATS ACLs (Phase 2) and NetworkPolicies (Phase 3) all sit *above* the
--   database; RLS is the last boundary. It converts "we always remember to
--   filter by tenant_id" from a code-review convention into a database
--   invariant.
--
-- HOW IT WORKS
--   Every connection that serves tenant traffic must set two GUCs before it
--   issues a query:
--
--       SET LOCAL app.tenant_id  = 't_abc123';
--       SET LOCAL app.project_id = 'p_xyz789';   -- optional, '' widens to tenant
--
--   Use SET LOCAL (transaction-scoped) so a pooled connection cannot leak a
--   previous tenant's identity to the next checkout. `avm-storage` is
--   responsible for issuing these at the start of every transaction.
--
--   Policies are FORCEd, so even the table owner is subject to them. Migration
--   and maintenance jobs must connect as a role holding BYPASSRLS.
--
-- NOTE ON TABLE NAMES
--   The design doc names the isolated tables `agent_state`, `agent_memory` and
--   `execution_logs`. In the current schema those roles are filled by
--   `memories` (001), `jobs` (002) and `audit_logs` (003). This migration
--   applies the policies to whichever of the six names actually exist, so it
--   is correct both today and after the A2A spike renames land.

-- ---------------------------------------------------------------------------
-- Helper: current tenant / project from the connection GUCs.
-- `true` as the second arg => return NULL instead of erroring when unset.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION avm_current_tenant() RETURNS TEXT
    LANGUAGE sql STABLE
    AS $$ SELECT COALESCE(NULLIF(current_setting('app.tenant_id', true), ''), NULL) $$;

CREATE OR REPLACE FUNCTION avm_current_project() RETURNS TEXT
    LANGUAGE sql STABLE
    AS $$ SELECT COALESCE(NULLIF(current_setting('app.project_id', true), ''), NULL) $$;

COMMENT ON FUNCTION avm_current_tenant() IS
    'Tenant bound to the current transaction via SET LOCAL app.tenant_id. NULL when unset.';
COMMENT ON FUNCTION avm_current_project() IS
    'Project bound to the current transaction via SET LOCAL app.project_id. NULL widens to whole tenant.';

-- ---------------------------------------------------------------------------
-- Apply the isolation policy to every isolated table that exists.
--
-- Policy semantics:
--   * app.tenant_id unset  -> zero rows visible (fail closed, not fail open).
--   * app.tenant_id set    -> only rows with a matching tenant_id.
--   * app.project_id set   -> additionally narrowed to that project; rows with
--                             an empty project_id (tenant-scoped rows) stay
--                             visible because they are ancestors of the
--                             project scope.
--   * WITH CHECK mirrors USING, so a write cannot plant a row into another
--     tenant either.
-- ---------------------------------------------------------------------------

DO $$
DECLARE
    t              TEXT;
    has_project    BOOLEAN;
    using_expr     TEXT;
    isolated_table TEXT[] := ARRAY[
        'agent_state', 'agent_memory', 'execution_logs',  -- names from the design doc
        'memories',    'jobs',         'audit_logs'       -- names in the current schema
    ];
BEGIN
    FOREACH t IN ARRAY isolated_table LOOP
        CONTINUE WHEN to_regclass('public.' || quote_ident(t)) IS NULL;

        -- Every isolated table must carry tenant_id; skip loudly if it does not.
        IF NOT EXISTS (
            SELECT 1 FROM information_schema.columns
            WHERE table_schema = 'public' AND table_name = t AND column_name = 'tenant_id'
        ) THEN
            RAISE WARNING 'avm rls: table %.% has no tenant_id column, skipping', 'public', t;
            CONTINUE;
        END IF;

        SELECT EXISTS (
            SELECT 1 FROM information_schema.columns
            WHERE table_schema = 'public' AND table_name = t AND column_name = 'project_id'
        ) INTO has_project;

        using_expr := 'avm_current_tenant() IS NOT NULL AND tenant_id = avm_current_tenant()';
        IF has_project THEN
            using_expr := using_expr
                || ' AND (avm_current_project() IS NULL'
                || ' OR project_id = avm_current_project()'
                || ' OR project_id = '''')';
        END IF;

        EXECUTE format('ALTER TABLE public.%I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE public.%I FORCE ROW LEVEL SECURITY', t);

        EXECUTE format('DROP POLICY IF EXISTS avm_tenant_isolation ON public.%I', t);
        EXECUTE format(
            'CREATE POLICY avm_tenant_isolation ON public.%I
                 AS PERMISSIVE FOR ALL TO PUBLIC
                 USING (%s) WITH CHECK (%s)',
            t, using_expr, using_expr
        );

        RAISE NOTICE 'avm rls: enabled tenant isolation on public.% (project scoped: %)', t, has_project;
    END LOOP;
END
$$;

-- ---------------------------------------------------------------------------
-- Roles.
--
--   avm_app      — the runtime role. Subject to RLS. Grant it to the
--                  avm-server / avm-executor / avm-gateway credentials.
--   avm_migrator — schema + maintenance. BYPASSRLS so migrations and the
--                  scheduler's purge job can see every row.
--
-- Created only if absent; grants are left to the deployment (they depend on
-- which tables exist at rollout time).
-- ---------------------------------------------------------------------------

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'avm_app') THEN
        CREATE ROLE avm_app NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'avm_migrator') THEN
        CREATE ROLE avm_migrator NOLOGIN BYPASSRLS;
    ELSE
        ALTER ROLE avm_migrator BYPASSRLS;
    END IF;
EXCEPTION WHEN insufficient_privilege THEN
    RAISE WARNING 'avm rls: not enough privilege to create avm_app / avm_migrator; create them manually';
END
$$;
