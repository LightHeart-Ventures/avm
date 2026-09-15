# Memory Scope Isolation — Full Security Design

**Status:** Accepted design  
**Issue addressed:** #2 — Memory scope isolation is described but not enforced  
**Threat level:** Critical (tenant isolation violation risk)

---

## Problem Statement

ARCHITECTURE.md specifies a four-level scope hierarchy (`system → tenant → project → agent`) and
describes which scopes are readable by which agents. However, the current design has **no
enforcement layer**: nothing prevents an agent from passing an arbitrary `scope_id` to
`avm_get_memory` and reading memories that belong to a sibling agent, a sibling project, or even a
different tenant.

### Attack Vectors

#### AV-1: Agent-scope squatting

```
ag_spybot (t_acme, b_payments) calls:
  avm_get_memory(scope="agent", scope_id="ag_secrets_store")
```

The MCP Gateway currently trusts the `scope_id` parameter verbatim. `ag_secrets_store` is a
sibling agent in the same project. Result: cross-agent memory read.

#### AV-2: Cross-project horizontal escalation

```
ag_spybot (t_acme, b_payments) calls:
  avm_get_memory(scope="project", scope_id="b_infra")
```

`b_infra` is a sibling project in the same tenant. The hierarchy says project agents cannot read
sibling projects, but without enforcement this read succeeds.

#### AV-3: Cross-tenant escalation

```
ag_spybot (t_acme, b_payments) calls:
  avm_get_memory(scope="tenant", scope_id="t_competitor")
```

Reads another tenant's memories entirely.

#### AV-4: Workflow step context leakage

When a workflow runs agents A → B → C, step results are passed between agents. There is no spec
for whether step results carry the source agent's full memory snapshot. A malicious agent could
surface sensitive data from a prior step's context by inspecting its stdin payload.

---

## Fix Design

The fix has three layers: **JobContext** (immutable identity bound at launch), **MemoryService
enforcement** (server-side ACL), and **MCP Gateway validation** (first line of defense). All three
must be present; any single layer alone is not sufficient.

### Layer 1: Immutable JobContext

At job launch, the **Scheduler** constructs a `JobContext` and injects it as a signed,
non-forgeable token. The agent process **cannot modify it**.

```rust
// crates/avm-executor/src/job_context.rs

/// Immutable identity for a running job. Created by the Scheduler, injected
/// via an env var (AVM_JOB_CONTEXT) as a base64-encoded, HMAC-signed JSON blob.
/// The MCP Gateway and MemoryService verify the HMAC before trusting any field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobContext {
    pub job_id: String,       // "job_abc123"
    pub tenant_id: String,    // "t_acme"
    pub project_id: String,   // "b_payments"
    pub agent_id: String,     // "ag_pr_reviewer"
    pub scope: MemoryScope,   // the agent's *own* scope level
    pub scope_id: String,     // the agent's *own* scope_id
    pub issued_at: i64,       // Unix timestamp; reject if >1h old
    pub nonce: String,        // random 32-byte hex, prevents replay
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MemoryScope {
    System,
    Tenant,
    Project,
    Agent,
}

impl JobContext {
    /// Verify HMAC-SHA256 using the shared `AVM_CONTEXT_SIGNING_KEY`.
    /// Returns Err if signature is bad or token is expired.
    pub fn verify_and_decode(token: &str, signing_key: &[u8]) -> Result<Self, ContextError> {
        // 1. base64-decode → (payload_json, hmac_bytes)
        // 2. verify HMAC-SHA256(payload_json, signing_key) == hmac_bytes
        // 3. check issued_at + 3600 > now()
        // 4. deserialize JobContext from payload_json
        todo!("implement in avm-executor/src/job_context.rs")
    }

    /// Sign and encode for injection at job launch.
    pub fn sign_and_encode(&self, signing_key: &[u8]) -> String {
        todo!("implement in avm-executor/src/job_context.rs")
    }
}
```

**Injection:**
```bash
# Scheduler injects into every container/process
export AVM_JOB_CONTEXT="<base64-hmac-signed-token>"
# Agent binary, MCP Gateway, and MemoryService all read this
```

---

### Layer 2: MemoryService ACL Enforcement

The `MemoryService` **ignores** the `scope` and `scope_id` fields the agent passes in its request.
Instead, it derives the permitted access set exclusively from the `JobContext`.

```rust
// crates/avm-memory/src/scope_acl.rs

use crate::job_context::{JobContext, MemoryScope};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MemoryAccessError {
    #[error("scope {requested_scope}/{requested_id} is not readable by job {job_id} (agent scope: {agent_scope}/{agent_id})")]
    ScopeViolation {
        requested_scope: String,
        requested_id: String,
        job_id: String,
        agent_scope: String,
        agent_id: String,
    },
    #[error("agent-scoped memories are private to {owner_agent_id}; this job is {job_id}")]
    AgentScopePrivate {
        owner_agent_id: String,
        job_id: String,
    },
    #[error("cross-tenant access denied: job tenant={job_tenant}, requested tenant={requested_tenant}")]
    CrossTenantDenied {
        job_tenant: String,
        requested_tenant: String,
    },
}

/// Returns Ok(()) if the job described by `ctx` is permitted to read
/// a memory at (target_scope, target_scope_id). Returns Err otherwise.
pub fn check_read_access(
    ctx: &JobContext,
    target_scope: &MemoryScope,
    target_scope_id: &str,
) -> Result<(), MemoryAccessError> {
    match target_scope {
        MemoryScope::System => {
            // System memories: readable by everyone.
            Ok(())
        }

        MemoryScope::Tenant => {
            // Must belong to the same tenant.
            if target_scope_id != ctx.tenant_id {
                return Err(MemoryAccessError::CrossTenantDenied {
                    job_tenant: ctx.tenant_id.clone(),
                    requested_tenant: target_scope_id.to_string(),
                });
            }
            Ok(())
        }

        MemoryScope::Project => {
            // Must be the agent's own project (or a parent project if we add hierarchy later).
            // Sibling projects are DENIED.
            if target_scope_id != ctx.project_id {
                return Err(MemoryAccessError::ScopeViolation {
                    requested_scope: "project".into(),
                    requested_id: target_scope_id.to_string(),
                    job_id: ctx.job_id.clone(),
                    agent_scope: format!("{:?}", ctx.scope),
                    agent_id: ctx.agent_id.clone(),
                });
            }
            Ok(())
        }

        MemoryScope::Agent => {
            // Agent-scoped memories are PRIVATE to the owning agent.
            // No parent can read them. No sibling can read them.
            if target_scope_id != ctx.agent_id {
                return Err(MemoryAccessError::AgentScopePrivate {
                    owner_agent_id: target_scope_id.to_string(),
                    job_id: ctx.job_id.clone(),
                });
            }
            Ok(())
        }
    }
}

/// Write access: agent can write to its own scope and below.
/// A project agent can write project or agent memories but NOT tenant/system.
pub fn check_write_access(
    ctx: &JobContext,
    target_scope: &MemoryScope,
    target_scope_id: &str,
) -> Result<(), MemoryAccessError> {
    match (&ctx.scope, target_scope) {
        // System agents can write anywhere (within same tenant — system is cross-tenant infra).
        (MemoryScope::System, _) => Ok(()),

        // Tenant agents can write tenant + project + agent memories in their tenant.
        (MemoryScope::Tenant, MemoryScope::System) => {
            Err(MemoryAccessError::ScopeViolation {
                requested_scope: "system".into(),
                requested_id: target_scope_id.to_string(),
                job_id: ctx.job_id.clone(),
                agent_scope: "tenant".into(),
                agent_id: ctx.agent_id.clone(),
            })
        }

        // Project agents can write project or agent memories — NOT tenant or system.
        (MemoryScope::Project, MemoryScope::Tenant) |
        (MemoryScope::Project, MemoryScope::System) => {
            Err(MemoryAccessError::ScopeViolation {
                requested_scope: format!("{:?}", target_scope).to_lowercase(),
                requested_id: target_scope_id.to_string(),
                job_id: ctx.job_id.clone(),
                agent_scope: "project".into(),
                agent_id: ctx.agent_id.clone(),
            })
        }

        // Agent-scope write: only the owning agent.
        (_, MemoryScope::Agent) if target_scope_id != ctx.agent_id => {
            Err(MemoryAccessError::AgentScopePrivate {
                owner_agent_id: target_scope_id.to_string(),
                job_id: ctx.job_id.clone(),
            })
        }

        // All other combinations: re-validate tenant ownership + project ownership.
        _ => check_read_access(ctx, target_scope, target_scope_id),
    }
}
```

---

### Layer 3: MCP Gateway First-Line Validation

The MCP Gateway is the **first** place tool calls land; it should reject obviously-bad scope claims
before they even reach the MemoryService. This provides defense-in-depth and faster error feedback.

```rust
// crates/avm-gateway/src/mcp_router.rs  (additions)

use crate::job_context::JobContext;
use avm_memory::scope_acl::{check_read_access, check_write_access, MemoryScope};

impl McpRouter {
    /// Called for every `avm_get_memory` tool invocation.
    pub fn handle_get_memory(
        &self,
        ctx: &JobContext,
        params: GetMemoryParams,
    ) -> Result<MemoryResponse, McpError> {
        // Parse the requested scope/scope_id from params.
        let target_scope = MemoryScope::from_str(&params.scope)
            .map_err(|_| McpError::invalid_param("scope", &params.scope))?;

        // ENFORCE: reject before touching the store.
        check_read_access(ctx, &target_scope, &params.scope_id)
            .map_err(|e| McpError::permission_denied(e.to_string()))?;

        // Audit log the access attempt (success or failure was already handled above).
        self.audit.log(AuditEvent::memory_read(ctx, &params));

        // Forward to MemoryService (which will also enforce — defense-in-depth).
        self.memory_service.read(ctx, params)
    }

    /// Called for every `avm_upsert_memory` tool invocation.
    pub fn handle_upsert_memory(
        &self,
        ctx: &JobContext,
        params: UpsertMemoryParams,
    ) -> Result<MemoryResponse, McpError> {
        let target_scope = MemoryScope::from_str(&params.scope)
            .map_err(|_| McpError::invalid_param("scope", &params.scope))?;

        check_write_access(ctx, &target_scope, &params.scope_id)
            .map_err(|e| McpError::permission_denied(e.to_string()))?;

        self.audit.log(AuditEvent::memory_write(ctx, &params));
        self.memory_service.upsert(ctx, params)
    }
}
```

---

### Layer 4: Audit Logging

Every memory access (success and failure) is logged as a structured event. This enables:
- Security forensics: "did ag_spybot ever try to read ag_secrets_store?"
- Anomaly detection: burst of 403 MemoryAccessDenied from one agent
- Compliance: tenant-level memory access reports

```rust
// crates/avm-observability/src/audit.rs

#[derive(Debug, Serialize)]
pub struct AuditEvent {
    pub timestamp: DateTime<Utc>,
    pub event_type: AuditEventType,
    pub job_id: String,
    pub agent_id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub target_scope: String,
    pub target_scope_id: String,
    pub target_memory_id: Option<String>,
    pub outcome: AuditOutcome,  // "allowed" | "denied"
    pub denial_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub enum AuditEventType {
    MemoryRead,
    MemoryWrite,
    MemoryDelete,
    AgentInvocation,
}

#[derive(Debug, Serialize)]
pub enum AuditOutcome {
    Allowed,
    Denied,
}
```

**Prometheus counters to add:**
```
avm_memory_access_total{tenant_id, scope, outcome}  // outcome=allowed|denied
avm_memory_access_denied_total{tenant_id, reason}   // reason=cross_tenant|sibling_project|agent_private
```

---

## Test Cases

```rust
// tests/integration/memory_scope_isolation.rs

#[test]
fn project_agent_cannot_read_sibling_project_memory() {
    let ctx = make_ctx("t_acme", "b_payments", "ag_pr_reviewer", MemoryScope::Project);
    let result = check_read_access(&ctx, &MemoryScope::Project, "b_infra");
    assert!(matches!(result, Err(MemoryAccessError::ScopeViolation { .. })));
}

#[test]
fn project_agent_cannot_read_sibling_agent_memory() {
    let ctx = make_ctx("t_acme", "b_payments", "ag_pr_reviewer", MemoryScope::Project);
    let result = check_read_access(&ctx, &MemoryScope::Agent, "ag_task_executor");
    assert!(matches!(result, Err(MemoryAccessError::AgentScopePrivate { .. })));
}

#[test]
fn project_agent_cannot_read_other_tenant_memory() {
    let ctx = make_ctx("t_acme", "b_payments", "ag_pr_reviewer", MemoryScope::Project);
    let result = check_read_access(&ctx, &MemoryScope::Tenant, "t_competitor");
    assert!(matches!(result, Err(MemoryAccessError::CrossTenantDenied { .. })));
}

#[test]
fn agent_can_read_own_agent_memory() {
    let ctx = make_ctx("t_acme", "b_payments", "ag_pr_reviewer", MemoryScope::Agent);
    let result = check_read_access(&ctx, &MemoryScope::Agent, "ag_pr_reviewer");
    assert!(result.is_ok());
}

#[test]
fn agent_can_read_parent_project_memory() {
    let ctx = make_ctx("t_acme", "b_payments", "ag_pr_reviewer", MemoryScope::Agent);
    let result = check_read_access(&ctx, &MemoryScope::Project, "b_payments");
    assert!(result.is_ok());
}

#[test]
fn agent_can_read_parent_tenant_memory() {
    let ctx = make_ctx("t_acme", "b_payments", "ag_pr_reviewer", MemoryScope::Agent);
    let result = check_read_access(&ctx, &MemoryScope::Tenant, "t_acme");
    assert!(result.is_ok());
}

#[test]
fn forged_job_context_is_rejected() {
    // Attacker crafts a JobContext with tenant_id="t_competitor"
    let fake_ctx_json = r#"{"job_id":"j1","tenant_id":"t_competitor","project_id":"b_payments",...}"#;
    // Attempt to verify with the real signing key
    let result = JobContext::verify_and_decode(fake_ctx_json, REAL_SIGNING_KEY);
    assert!(result.is_err()); // HMAC mismatch
}

#[test]
fn expired_job_context_is_rejected() {
    let old_ctx = make_ctx_at_time("t_acme", "b_payments", "ag_pr_reviewer", now() - 7200);
    let result = JobContext::verify_and_decode(&old_ctx.sign(KEY), KEY);
    assert!(result.is_err()); // expired
}
```

---

## Workflow Step Isolation (AV-4)

Workflow step results are passed between agents as opaque `JobResult` structs (synthesis text +
tool call records). The design rule is:

- **Step results contain only the synthesis text and tool call logs** — not raw memory snapshots.
- The workflow engine does NOT inject the prior step's full `JobContext` into the next step.
- If step A needs to pass data to step B, it must do so explicitly via a shared project-scoped memory write: `avm_upsert_memory(scope="project", ...)`.
- The workflow engine enforces this at the `WorkflowStep` boundary: `step_results` in `WorkflowRun` is a `Map<step_id, JobResult>`, not `Map<step_id, MemorySnapshot>`.

This prevents AV-4 by design: no step can accidentally surface another agent's private memories
because the struct boundary strips them.

---

## Architecture Update Summary

| Component | Change Required | Priority |
|-----------|-----------------|----------|
| `avm-executor/src/job_context.rs` | New: `JobContext` struct + HMAC sign/verify | **P0** |
| `avm-memory/src/scope_acl.rs` | New: `check_read_access` + `check_write_access` | **P0** |
| `avm-gateway/src/mcp_router.rs` | Add: scope validation before forwarding to MemoryService | **P0** |
| `avm-server/src/memory_service.rs` | Add: re-validate JobContext before DB read (defense-in-depth) | **P1** |
| `avm-observability/src/audit.rs` | New: `AuditEvent` + Prometheus denial counters | **P1** |
| `tests/integration/memory_scope_isolation.rs` | New: 8 test cases (above) | **P0** |
| `ARCHITECTURE.md` | Update: clarify enforcement section, add signing key to startup | **P1** |

**Estimated effort:**
- P0 items (job_context + scope_acl + gateway validation + tests): **1.5 days**
- P1 items (server-side double-check + audit log + docs): **0.5 day**
- **Total: ~2 days**

---

## Open Question: Agent-scope visibility to parents

The current `Scope Visibility` table in ARCHITECTURE.md says:

> Agent Memory: Readable by parent tenant/project agents (optional, default deny)

**Decision:** Default **DENY**. Parents cannot read agent memories unless explicitly granted via a
future `MemorySharePolicy`. Rationale: an agent's private memory may contain credentials, reasoning
traces, or user-specific context that should not bleed up to parent scopes. If a workflow step
needs to share data upward, it must explicitly write to a project-scoped memory.

This aligns with the principle of least privilege: scope narrows downward (parent → child), not upward.
