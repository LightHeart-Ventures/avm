# AVM Implementation Plan

Living document. Each section is written by the spike that owns it; sections
are additive and can land independently.

> **Section ownership**
> | Section | Owner spike |
> |---|---|
> | A2A + Agent Card | `feat/a2a-agent-card` |
> | JSON-Schema tool signatures | tool-introspection spike |
> | OCI model distribution | model-distribution spike |
> | Network Isolation & A2A Security | this document (below) |

---

## Network Isolation & A2A Security

### Executive summary

AVM is multi-tenant, and agents are arbitrary user-supplied code. We therefore
assume **every agent container is potentially hostile** and design so that no
single control failure produces a cross-tenant breach.

Isolation is enforced at four independent layers. Each assumes the others may
have failed:

| # | Layer | Mechanism | Blocks | Phase |
|---|---|---|---|---|
| 1 | **API** | `validate_a2a_dispatch()` in `avm-gateway/src/security.rs` | An agent asking the platform to route work to an agent it may not reach | **Phase 1 — this PR** |
| 2 | **Queue** | Per-tenant NATS accounts + subject ACLs (`docs/nats/`) | Subscribing to another tenant's streams; forging results; writing a peer's A2A subject | Phase 2 |
| 3 | **Container** | Kubernetes NetworkPolicies (`docs/network-policies/`) | Direct Pod-to-Pod traffic that bypasses the gateway entirely | Phase 3 |
| 4 | **Data** | Postgres RLS (`migrations/006_add_agent_isolation_rls.sql`) | A missing `WHERE tenant_id = …`, or a leaked DB credential | Phase 4 |

Only layer 1 understands *policy* (trusted peers, intra-project opt-in).
Layers 2–4 are blunt boundaries whose job is to make the sanctioned path the
**only reachable** path, so that layer 1 cannot be routed around.

Two invariants hold at every layer:

1. **Tenant boundaries are hard.** No configuration value anywhere in the
   system permits cross-tenant communication. There is deliberately no
   `allow_cross_tenant` field to set.
2. **Intra-project A2A is deny-by-default.** Co-location in a project is not
   consent. The callee must opt in explicitly.

### Threat model

| Threat | Layer that stops it |
|---|---|
| Agent calls `POST /a2a/task` targeting another tenant's agent | 1 (gateway) |
| Agent calls a sibling agent in its own project without being invited | 1 (gateway) |
| Agent opens a raw socket to a sibling Pod's IP | 3 (NetworkPolicy) |
| Agent connects to NATS and subscribes to `avm.jobs.>` | 2 (subject ACL) |
| Agent publishes onto a peer's `avm.a2a.…` subject to skip the gateway | 2 (deny-pub on `avm.a2a.>`) |
| Application bug drops the tenant predicate from a query | 4 (RLS) |
| Stolen DB credential used from outside the cluster | 4 (RLS) + 3 (egress deny) |
| Stolen NATS credential | 2 (per-tenant account; revocation) |
| Agent exfiltrates data to the public internet | 3 (default-deny egress) |

Explicitly **out of scope** for this design: isolation *within* a single Pod
(sidecars share a network namespace), side-channel attacks between co-tenant
nodes, and supply-chain compromise of the agent image itself.

---

### 1. NATS tenant isolation (Phase 2)

**One NATS account per tenant.** Accounts — not subject prefixes — are the
hard isolation primitive in NATS: subjects do not cross an account boundary at
all. A shared account with prefix conventions is one typo away from a
cross-tenant leak; an account boundary is not.

Subject layout:

```
avm.jobs.<tenant>.<project>              scheduler → executor
avm.results.<tenant>.<project>           executor  → scheduler
avm.a2a.<tenant>.<project>.<agent>       gateway   → agent
avm.gateway.<tenant>.dispatch            agent     → gateway
```

Per-agent users are scoped tighter still. The load-bearing rule:

- agent users **may subscribe** only to their own `avm.a2a.<t>.<p>.<agent>`;
- agent users are **denied publish on `avm.a2a.>` entirely**.

That second rule is what makes the gateway unavoidable at the queue layer — an
agent has no way to hand work to a peer except by asking the gateway, where
layer 1 evaluates it.

Accounts declare `exports: []` and `imports: []`. Any export added here is a
cross-tenant channel and must be treated as a defect.

Credentials are NSC/JWT with `--expiry`, so rotation is routine rather than an
incident. Generation, rotation, revocation and the isolation-verification
probes are in `docs/nats/README.md`.

### 2. A2A scope validation (Phase 1 — implemented in this PR)

Every agent publishes an Agent Card carrying an `A2APolicy`
(`avm-agent/src/a2a_policy.rs`):

```rust
pub struct A2APolicy {
    pub default: TrustDefault,       // Deny (default) | Allow
    pub trusted_peers: Vec<String>,  // agent IDs allowed to call us
    pub allow_intra_project: bool,   // default false
    pub allow_cross_project: bool,   // default false
}
```

`A2APolicy::default()` is the closed policy. A card that omits the field
deserializes to deny-all — the failure mode of a forgotten config is *closed*,
not open.

`validate_a2a_dispatch()` (`avm-gateway/src/security.rs`) evaluates in this
fixed order and returns on the first failure. Order matters: the hardest
boundary is checked first so a cross-tenant attempt can never be masked by a
permissive peer list.

| # | Check | Failure |
|---|---|---|
| 0 | Card describes the routed target | `AuthError::CardTargetMismatch` |
| 0b | Source == target (self-dispatch) | *allowed, short-circuits* |
| 1 | `source.tenant_id == target.tenant_id` | `ScopeError::CrossTenantDenied` |
| 2 | Different project ⇒ `allow_cross_project` | `ScopeError::CrossProjectDenied` |
| 3 | Same project ⇒ `allow_intra_project` | `ScopeError::IntraProjectDenied` |
| 4 | Caller in `trusted_peers`, else `default == Allow` | `AuthError::PeerNotTrusted` |

Rule 1 consults no policy field at all — it is unconditional.

**Auditing.** Every call emits exactly one `SecurityEvent` through a
`SecurityAudit` sink, on the allow path as well as the deny path. There is no
silent branch. The default `TracingAudit` writes a structured event on target
`avm.security.a2a` (`info` on allow, `warn` on deny) which the OTel pipeline
exports; deployments should additionally persist to `audit_logs` (migration
`003`). All denials surface as HTTP 403 with no distinction between "not
permitted" and "does not exist" — we do not leak the existence of other
tenants' agents.

`evaluate()` is exposed as a pure, side-effect-free variant for admission
dry-runs.

Wiring: `POST /a2a/task` calls `validate_a2a_dispatch()` **before** any job is
published to NATS, and returns 403 on `Err`.

### 3. Container network policies (Phase 3)

Default-deny egress *and* ingress on every Pod labelled
`avm.io/workload: agent`, then whitelist exactly: DNS, `avm-gateway:8080`,
`avm-server:50051`, `nats:4222`, `postgres:5432`. Agent-to-agent Pod traffic
is dropped by the CNI, so an agent that ignores the gateway has nowhere to go.

Ingress to agents is permitted only from the gateway, which means every task
an agent receives has necessarily passed `validate_a2a_dispatch()`.

An opt-in per-pair exception template exists
(`docs/network-policies/allow-peer-communication.yaml`) for the rare case that
needs a direct data path. It is annotated with owner and review date, and
requires the tenant label to match on both sides. Its real cost: direct peer
traffic is invisible to the gateway and therefore **absent from the A2A audit
trail**. That is why the default is off.

Hard prerequisite: a CNI that actually enforces NetworkPolicy. An unenforced
policy is indistinguishable from an enforced one until an incident, so the
rollout runbook includes a deliberate connectivity test that must fail.

### 4. Postgres RLS (Phase 4)

`migrations/006_add_agent_isolation_rls.sql` enables and **forces** row-level
security on the isolated tables, with a policy of the shape:

```sql
USING (avm_current_tenant() IS NOT NULL AND tenant_id = avm_current_tenant())
```

`avm_current_tenant()` reads `current_setting('app.tenant_id', true)`. The
`IS NOT NULL` guard makes an unset GUC yield **zero rows** — the failure mode
is fail-closed. `WITH CHECK` mirrors `USING`, so a write cannot plant a row
into another tenant either. Where a `project_id` column exists the policy
narrows further, while keeping tenant-scoped rows (`project_id = ''`) visible
as ancestors of the project scope, consistent with AVM's scope-inheritance
model.

Callers must issue `SET LOCAL app.tenant_id = …` at transaction start —
`SET LOCAL`, not `SET`, so a pooled connection cannot leak identity to the
next checkout. `avm-storage` owns this.

Two roles: `avm_app` (runtime, subject to RLS) and `avm_migrator`
(`BYPASSRLS`, for migrations and the scheduler's purge job).

The migration applies to whichever of `agent_state` / `agent_memory` /
`execution_logs` / `memories` / `jobs` / `audit_logs` exist, so it is correct
both against today's schema and after the A2A spike's table renames.

### 5. Proto surface

`proto/avm_service.proto` gains `SecurityEvent`, `ScopeError`, `AuthError` and
the `A2AError` wrapper, so denials are typed on the wire and the audit record
has a schema rather than being free-form JSON.

---

### Phased rollout

| Phase | Scope | Status | Risk if skipped |
|---|---|---|---|
| **1** | Gateway scope validation + Agent Card `A2APolicy` + audit + typed errors | **This PR** | Any agent can dispatch to any other agent |
| **2** | Per-tenant NATS accounts, subject ACLs, NSC credential lifecycle | Follow-on spike | Gateway is bypassable via a direct NATS publish |
| **3** | Kubernetes NetworkPolicies rolled out per tenant namespace | Follow-on spike | Gateway is bypassable via a direct Pod dial |
| **4** | Postgres RLS enabled, `avm_app` / `avm_migrator` roles, `SET LOCAL` in `avm-storage` | Follow-on spike | A single missing predicate leaks rows |

Deliberate ordering: Phase 1 first because it is the only layer that carries
policy semantics and the only one that produces an audit trail — it is what
tells you whether Phases 2–4 would have blocked anything real. Phases 2–4 then
close the bypasses in ascending order of blast radius.

**Rollout discipline.** Phases 2–4 each change a boundary that currently
permits traffic. Enable per tenant namespace, run the verification probes in
the respective README (each is written so the *expected* result is a failure),
and only then widen. Every one of these will surface an undocumented path
something was quietly relying on — finding those is the point.
