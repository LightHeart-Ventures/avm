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
> | Architecture Decisions | `docs/adr/` (below) |

---

## Architecture Decisions

Two decisions taken after the spikes below were written **supersede parts of
them**. The ADRs are authoritative; the spike sections are kept for their
reasoning and are annotated in place where they now conflict.

| ADR | Decision | Supersedes |
|---|---|---|
| [ADR-0001](docs/adr/0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md) | Per-node embedded **SQLite** (WAL) is the store, the durable **event log** and the transactional outbox. **gRPC** is the only intra-cluster transport. **NATS/JetStream is removed; Postgres is removed.** | Every reference below to NATS subjects/accounts, JetStream, and Postgres as the cluster-wide store |
| [ADR-0002](docs/adr/0002-container-isolated-agents.md) | Agents run as **OCI containers**, not forked subprocesses. The `AVM_*` env + stdin/stdout job contract is unchanged — only the sandbox changes. | `fork/exec` agent execution in `avm-executor/src/agent_runner.rs`; makes defence layer 3 enforceable rather than conventional |

**Two consequences that need the operator's attention before the port:**

1. **RLS has no SQLite equivalent.** Defence layer 4 below is
   `migrations/007_add_agent_isolation_rls.sql` — Postgres Row-Level
   Security. SQLite has no RLS. Layer 4 must become a **process-level hard
   invariant** in `avm-storage` (every query takes a `Scope`; the connection
   pool is private to the module and never exported). This changes the
   Phase 4 row of the phased rollout and must be decided **before** the port,
   not after. See ADR-0001 § "Migration hazard".
2. **The agent runtime tier is still open** (`runc` / `runsc` / Firecracker).
   ADR-0002 recommends abstracting a `Sandbox` trait now and defaulting to
   rootless `runc`, with the tier as a per-tenant policy knob.

> **Numbering correction:** several passages below cite
> `migrations/006_add_agent_isolation_rls.sql`. The RLS migration is
> **`007_add_agent_isolation_rls.sql`** (`006` is `006_create_models.sql`),
> and migrations live at the repo root `migrations/`, not `avm-storage/migrations/`.
> Corrected in place.

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
| 2 | **Transport** | ~~Per-tenant NATS accounts + subject ACLs~~ → **superseded by [ADR-0001](docs/adr/0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md)**: no broker exists to bypass; gRPC `EventBus` peer identity is mTLS, and scope filtering is server-side | Subscribing to another tenant's stream; forging results | Phase 2 |
| 3 | **Container** | Kubernetes NetworkPolicies (`docs/network-policies/`) — **now actually binding**, see [ADR-0002](docs/adr/0002-container-isolated-agents.md) | Direct Pod-to-Pod traffic that bypasses the gateway entirely | Phase 3 |
| 4 | **Data** | Postgres RLS (`migrations/007_add_agent_isolation_rls.sql`) — ⚠️ **no SQLite equivalent**; becomes a process-level invariant in `avm-storage` under [ADR-0001](docs/adr/0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md) | A missing `WHERE tenant_id = …`, or a leaked DB credential | Phase 4 |

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

> ⚠️ **Superseded in mechanism by [ADR-0001](docs/adr/0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md).**
> The *goal* below — fail-closed tenant isolation that survives a missing
> `WHERE` predicate — still stands and is non-negotiable. The *mechanism*
> does not: SQLite has no row-level security. Layer 4 must be re-expressed as
> a process-level invariant in `avm-storage`. **Decide this before the port.**

`migrations/007_add_agent_isolation_rls.sql` enables and **forces** row-level
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
| **2** | ~~Per-tenant NATS accounts, subject ACLs, NSC credential lifecycle~~ → **dropped**; gRPC `EventBus` with mTLS peer identity + server-side scope filtering ([ADR-0001](docs/adr/0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md)) | Follow-on spike | A subscriber reads scopes it does not own |
| **3** | Kubernetes NetworkPolicies rolled out per tenant namespace — **prerequisite: containerised agents** ([ADR-0002](docs/adr/0002-container-isolated-agents.md)); add `allow-gateway-egress-only.yaml` as the default agent netpol | Follow-on spike | Gateway is bypassable via a direct Pod dial |
| **4** | ⚠️ **Changed by [ADR-0001](docs/adr/0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md).** ~~Postgres RLS, `avm_app` / `avm_migrator` roles, `SET LOCAL`~~ → scope-typed query API in `avm-storage` with a private connection pool, enforced by API shape + tests | Follow-on spike — **blocks the SQLite port** | A single missing predicate leaks rows, with no database-level backstop |

Deliberate ordering: Phase 1 first because it is the only layer that carries
policy semantics and the only one that produces an audit trail — it is what
tells you whether Phases 2–4 would have blocked anything real. Phases 2–4 then
close the bypasses in ascending order of blast radius.

**Rollout discipline.** Phases 2–4 each change a boundary that currently
permits traffic. Enable per tenant namespace, run the verification probes in
the respective README (each is written so the *expected* result is a failure),
and only then widen. Every one of these will surface an undocumented path
something was quietly relying on — finding those is the point.

---

## Model distribution: OCI artifacts + content-addressed storage

**Status:** design landed, node-local data path implemented (`avm-models`),
scheduler/executor wiring implemented, registry client behind a feature flag.

### 1. Why OCI artifacts

Model weights are large, immutable, and shared by many agents. That is exactly
the shape of a container layer, so we reuse the container ecosystem instead of
inventing a transport:

| Need | What OCI gives us |
|---|---|
| Immutability | Content-addressed digests, end to end |
| Dedup | Same digest = same bytes, cached once per node |
| Auth / rate limits | Existing registry auth (GHCR PAT, ECR IAM) |
| Provenance | Manifest annotations, cosign signatures, SBOMs |
| Mirroring | Pull-through caches, air-gapped `oras copy` |

Artifact shape:

```text
manifest   application/vnd.oci.image.manifest.v1+json
  artifactType  application/vnd.avm.model.v1+json
  config        application/vnd.avm.model.config.v1+json   { backend, params, license }
  layer[0]      application/vnd.avm.model.weights.v1       <the weights blob>
```

Rust client: **`oci-client`** — the oras-project crate
(`github.com/oras-project/rust-oci-client`). The bare `oras` crate name on
crates.io is an unreleased `0.0.1` placeholder and is deliberately not used.

### 2. Model reference format

```text
oci://<registry>/<repository>[:<tag>]@sha256:<64 lowercase hex>

oci://ghcr.io/lightheart/qwen3-8b:q4_k_m@sha256:e3b0c442…b855
oci://registry.local:5000/models/phi4@sha256:1111…1111
```

Rules enforced by `ModelRef::parse`:

* The `@sha256:…` digest is **mandatory** — unpinned refs are rejected at parse
  time, so nothing downstream can be surprised by a moving tag.
* The tag is advisory provenance; the digest is the identity. A `:` after the
  last `/` is a tag, a `:` inside the host is a port.
* Only `sha256` today; the `algo:hex` split leaves room for `sha512`.
* `ModelRef` carries a `backend` hint (`llama.cpp` | `vllm` | `tgi`) used to
  pick a model-server image, and `size_bytes` for GC accounting.

Agents reference models by this URI in their Agent Card (`model_ref`), which
keeps the A2A surface unchanged: the card carries a string, the scheduler
resolves it to a digest and scores residency.

### 3. Content-addressed storage layout

Store root `/var/lib/avm/models` (override per node):

```text
/var/lib/avm/models/
  blobs/sha256/<aa>/<full-hex>   immutable weights, bind-mounted ro into model servers
  meta/<full-hex>.json           ModelRef + pulled_at + last_access + verified
  tmp/<full-hex>.part            staging for in-flight pulls
```

Invariants:

* **Atomic publish.** Bytes are staged in `tmp/`, hashed, and only then
  `rename(2)`d into `blobs/`. A blob visible under `blobs/` is always complete —
  a crashed pull leaves garbage in `tmp/`, never a torn blob.
* **Verify before publish.** `commit_bytes` refuses a digest mismatch, so a
  corrupt or MITM'd artifact can never become resident.
* **Two-char shard** (`blobs/sha256/ab/ab12…`) keeps directory fan-out sane.
* **Metadata is a sidecar, not a lock.** Losing `meta/` degrades residency to
  `absent` and triggers a re-pull; it never corrupts the blob.
* `blobs/` is the only path mounted into containers, always read-only. A model
  server can never mutate weights.

### 4. Pull / cache / residency flow

```text
scheduler ──placement──▶ executor(node)
                            │ 1. ModelStore::pull(model_ref)
                            │      meta hit + blob present ─▶ touch, done (no network)
                            │      miss ─▶ ArtifactFetcher::fetch
                            │             OciArtifactClient  (registry)
                            │             LocalDirFetcher    (air-gapped mirror)
                            │ 2. sha256 verify ─▶ tmp/ ─▶ rename ─▶ blobs/
                            │ 3. Postgres: model_pulls (checksum_ok, duration_ms)
                            │ 4. Postgres: model_placements (status, serving, endpoint)
                            └ 5. publish node label model.avm.io/<digest>=resident
```

Residency ladder:

| State | Meaning |
|---|---|
| `resident` | Blob on disk **and** digest verified |
| `cached` | Blob on disk, verification deferred (cheap hot path) |
| `absent` | Node must pull |
| `pulling` / `failed` | Transient states recorded in Postgres only |

Postgres (migration `006_create_models.sql`) is the cluster-wide view:

* `models` — catalogue keyed by digest (registry, repo, tag, size, backend).
* `model_pulls` — append-only pull + checksum audit trail per node.
* `model_placements` — current `(digest, node_id)` residency, `serving` flag and
  endpoint; the scheduler's residency input and what node labels are rebuilt
  from after an executor restart.

### 5. GC strategy

`GcPolicy` = **LRU by `last_access`, bounded by a hard byte ceiling**:

* `max_bytes` — hard ceiling; eviction runs until usage is at or below it.
* `high_watermark` (default `0.85`) — the level that *triggers* a pass, so GC
  does not thrash at the boundary.
* `min_age` (default 15 min) — a freshly pulled blob is never evicted, which
  kills the race where GC reaps weights a pending placement is about to use.
* `pinned` — digests with a live model server; never evicted.
* `dry_run` — report the eviction set without deleting (what `avm models gc
  --dry-run` prints).

Eviction order is oldest-access-first; every pass returns a `GcReport`
(`bytes_before`, `bytes_after`, `evicted`, `retained_pinned`) which is logged and
mirrored into `model_pulls` as `evicted` rows so cache churn is measurable.

### 6. Scheduling and affinity

Placement is a **soft-constraint scorer** (`avm-scheduler::placement`), not a
bin-packer:

```text
score = residency_weight · residency(node, digest)    resident 100 / cached 40 / absent 0
      + affinity_weight  · model_server_live(node)    +50 when a server already serves it
      + spread_weight    · free_slot_fraction(node)   ×20, keeps the cluster from hot-spotting
      - pull_penalty     · would_cold_pull(node)      −25, cold pulls cost minutes
```

* **Residency is soft.** A node without the weights stays *feasible* — it just
  loses to one that has them. This avoids the deadlock where a brand-new digest
  is unschedulable everywhere.
* **Hard constraints** are the only source of infeasibility: `required_labels`
  (e.g. `gpu.avm.io/kind=a100`), insufficient free slots, cordoned node.
* **Model affinity** co-schedules an agent with a live model server on the same
  node (`serving_digests`), so inference is a loopback call rather than a
  cross-node hop.
* Every `PlacementScore` keeps its components (`residency_score`,
  `affinity_score`, `spread_score`, `pull_penalty`) plus a rejection `reason`, so
  `avm scheduler explain` can show why a node won or lost.

Node labels are the contract between executor and scheduler:

```text
model.avm.io/sha256:<hex> = resident | cached | absent
```

### 7. Model servers (`kind: ModelServer`)

`ExecutorKind::ModelServer` runs an inference container that mounts the blob
store read-only:

```text
--mount type=bind,src=/var/lib/avm/models/blobs,dst=/models,ro
--model /models/sha256/e3/e3b0c442…b855
```

* Weights are **never** baked into the server image — one image serves any
  digest the node holds.
* **Multiple model servers per node are allowed.** How many, and whether a
  single multi-model server is preferable, is deliberately left open (see
  follow-ups); nothing in the design assumes one server per node.
* After `/health` passes, the executor publishes `model.avm.io/<digest>=resident`
  and writes `model_placements(serving=true, endpoint=…)`.
* Env handed to the container: `AVM_MODEL_DIR`, `AVM_MODEL_SERVER`,
  `AVM_MODEL_PORT`, `AVM_MODEL_DIGESTS`, `AVM_MODEL_BACKEND`.

### 8. Feature gating and hermetic tests

`avm-models` compiles the real registry client only under
`--features oci-registry`; the default build has no TLS/HTTP dependency and the
whole crate is unit-testable offline via `LocalDirFetcher`. Without the feature
`OciArtifactClient::pull_blob` returns `ModelError::RegistryFeatureDisabled`, so
call sites, the scheduler and the executor compile identically either way.

```bash
cargo test  --workspace                            # hermetic, no network
cargo check -p avm-models --features oci-registry  # real registry client
```

### 9. Follow-ups (deliberately out of scope here)

| Item | Why deferred |
|---|---|
| Model-server fan-out policy (one vs. many per node) | Needs a separate spike; nothing here assumes a single server |
| `oras push` from Rust | Publishing runs in CI today; pull is the hot path |
| Streaming / chunked pulls with resume | Current fetcher buffers; fine for the sizes we ship first |
| Cosign signature + SBOM verification at pull time | Slots in as another `commit_bytes` precondition |
| Pull-through registry mirror per rack | Bandwidth optimisation, not correctness |
| Proactive pre-warm (pull on placement *intent*) | Wants placement telemetry first |

---

## Tool Schema & Validation

**Crate:** `avm-mcp-tools` · **Endpoints:** `GET /tools/schema`,
`GET /tools/schema/:name`, `POST /tools/validate` · **Proto:** `ToolService`,
`ToolDefinition`, `ToolCall`, `ToolCallValidation`

### Problem

An agent handed a task needs to answer two questions before it can act:

1. *What can I call?* — the set of tools reachable through the gateway, and the
   shape of each one's arguments.
2. *Is this call well-formed?* — cheaply, **before** paying for a dispatch, a
   process spawn, or a round-trip to an upstream MCP server.

Today `avm-gateway` routes tool calls but publishes no contract: `ToolCall`
carries `arguments` as an opaque `serde_json::Value`, and the only way to learn
a tool was called wrong is to call it wrong. That is fine for a human at a
terminal and useless for an autonomous peer doing A2A task dispatch.

### Design

We publish **JSON-Schema as-is** (draft 2020-12). No AVM-specific schema
language, no IDL of our own: MCP servers already describe their tools in
JSON-Schema, every agent framework can already read it, and `schemars` /
`jsonschema` give us generation and validation for free.

```
                 ┌──────────────────────────────────────────┐
 Rust arg struct │  #[derive(JsonSchema)] MemoryReadArgs     │
   (builtins)    └───────────────┬──────────────────────────┘
                                 │ to_json_schema::<T>()
                                 ▼
 MCP tools/list  ──────►  from_mcp_definition()  ──────►  ToolSchema
 (stdio | http)                                              │
                                                             ▼
                                                     ManagedToolSet
                                                    (name → schema +
                                                  compiled validator)
                                                       │        │
                             GET /tools/schema ────────┘        └──── POST /tools/validate
                                                                       assert_callable()
```

#### Types

| Type | Role |
|---|---|
| `ToolSchema` | one signature: `name`, `description`, `input_schema`, `output_schema`, `source`, `version`, `fingerprint` |
| `ToolSource` | provenance — server id + transport (`builtin` / `stdio` / `http`) + endpoint |
| `ManagedToolSet` | the registry: every callable tool, with its compiled validator cached |
| `SchemaValidator` | a compiled `jsonschema::Validator` for one tool's input schema |
| `ValidationReport` | `{ tool, valid, errors[{path, message}], schema_fingerprint }` |
| `Compatibility` | `Identical` / `BackwardCompatible` / `Breaking` |

#### Tool schema format

`GET /tools/schema` returns:

```json
{
  "catalog_version": "avm.tools/v1",
  "dialect": "https://json-schema.org/draft/2020-12/schema",
  "tools": [
    {
      "name": "avm_memory_read",
      "description": "Read a memory visible to the caller's scope.",
      "input_schema": {
        "title": "MemoryReadArgs",
        "type": "object",
        "properties": {
          "memory_id":         { "description": "Memory key to resolve.", "type": "string" },
          "include_inherited": { "description": "Also search ancestor scopes.", "type": "boolean", "default": false }
        },
        "required": ["memory_id"]
      },
      "source_id": "builtin",
      "transport": "builtin",
      "fingerprint": "fnv1a64:3f1c9a0b77e2d415"
    }
  ]
}
```

`input_schema` is always an **object schema** — MCP tool arguments are a named
map, never a positional list. Servers that omit `"type": "object"` on an
otherwise-object schema are normalised on ingest; argument-less tools get
`{"type":"object","properties":{},"additionalProperties":false}`.

`output_schema` is emitted only when the server advertises one. Most MCP servers
today do not, so agents must treat it as best-effort.

`fingerprint` is an FNV-1a hash over the canonical (key-sorted) JSON form of the
contract. It is a **change detector, not a checksum** — it lets an agent cache a
schema and notice when the contract moved, without diffing documents. Key order
and whitespace do not affect it.

#### Validation flow

Validation happens at up to three points, cheapest first:

| # | Where | Trigger | Failure mode |
|---|---|---|---|
| 1 | **Agent-side** | agent has cached `/tools/schema` | agent fixes its own call, no network |
| 2 | **`POST /tools/validate`** | agent is unsure, or the schema is uncached | `422` with per-field `{path, message}`; no dispatch, no cost |
| 3 | **Gateway dispatch gate** | every call reaching `McpRouter` | `ManagedToolSet::assert_callable()` refuses before spawn/proxy |

Step 3 is the one that matters for safety: **validation is enforced at the
gateway, not merely offered.** Steps 1–2 exist so a well-behaved agent never
reaches step 3 with a bad call. Compilation is the expensive half of
`jsonschema`, so validators are compiled once per schema revision (keyed by
`fingerprint`) and reused for every call.

`POST /tools/validate` accepts either shape:

```jsonc
{ "tool_name": "avm_job_status", "arguments": { "job_id": "job_1" } }        // structured
{ "tool_name": "avm_job_status", "arguments_json": "{\"job_id\":\"job_1\"}" } // wire form (ToolCall)
```

and answers:

```json
{ "tool_name": "avm_job_status", "valid": false, "dispatchable": false,
  "errors": [{ "path": "/job_id", "message": "42 is not of type \"string\"" }],
  "schema_fingerprint": "fnv1a64:..." }
```

Status codes: `200` valid · `422` known tool, bad arguments · `404` unknown tool.
`dispatchable` is deliberately separate from `valid` — a call can be
schema-valid against a tool the gateway can no longer reach.

#### MCP integration

Tool definitions are *scanned*, never hand-maintained:

1. **Built-ins.** The gateway's four local tools derive `JsonSchema` on the very
   argument structs their handlers deserialize (`avm_mcp_tools::builtin`).
   Change the struct → the published schema and its fingerprint change with it.
   A unit test asserts every route in `McpRouter` has a published signature, so
   a new local tool cannot ship undocumented.
2. **Upstream MCPs.** On connect, the gateway issues `tools/list` over the
   existing connection — stdio child process or streamable HTTP, no new
   transport — and feeds the response to `ManagedToolSet::ingest_listing(source,
   listing)`. Both `inputSchema` (MCP spec camelCase) and `input_schema` are
   accepted.
3. **Refresh.** Re-ingesting from the *same* `source_id` replaces that server's
   schemas. A *different* source claiming a live name is rejected
   (`ToolError::DuplicateTool`) — that is a routing collision, not an update.
4. **Disconnect.** `remove_source(id)` drops every tool a server contributed, so
   the catalog never advertises an unreachable tool.
5. **Failure isolation.** A malformed entry fails the whole listing; a
   half-registered server never serves traffic.

#### Versioning strategy

Two independent version axes, and neither is a guess:

**Catalog envelope** — `catalog_version: "avm.tools/v1"`. Bumped only when the
`/tools/schema` response *shape* changes. Additive fields do not bump it.

**Per-tool contract** — every schema carries a `fingerprint` and an optional
server-declared `version`. `ToolSchema::compatibility(&previous)` classifies a
change mechanically:

| Change | Verdict |
|---|---|
| identical contract | `Identical` |
| added **optional** property | `BackwardCompatible` |
| description / title / default changed | `BackwardCompatible` |
| added **required** property | `Breaking` |
| removed a property | `Breaking` |
| changed a property's `type` | `Breaking` |

The rule is one-directional and caller-centric: *a change is breaking iff a
previously-valid call could now fail.* Backward-compatible revisions hot-swap
silently. Breaking ones are logged, and the intended policy (not yet enforced —
see below) is to require the upstream server to declare a new `version` before
the gateway will swap the schema under live callers.

**Agent caching contract:** cache per-tool keyed by `fingerprint`; re-fetch
`/tools/schema` when a validation verdict returns an unfamiliar
`schema_fingerprint`. Agents must not assume a name→schema binding outlives a
fingerprint change.

### Testing

46 tests across the workspace, all green. The ones that carry the design:

- schema generation from a Rust type produces the expected `required` set
- camelCase/snake_case MCP ingest, `"type"` normalisation, argument-less tools
- fingerprint is key-order independent
- all six compatibility verdicts above
- validation accepts/rejects: missing required, wrong type, sealed-object extras
- registry: duplicate-source rejection, same-source refresh, disconnect cleanup
- endpoints: catalog shape, single-tool lookup + `404`, `200`/`422`/`404` verdicts
- **cross-check:** every `McpRouter` local route has a published signature

### Deliberately out of scope

Called out so the next person doesn't mistake absence for oversight:

- **Enforcement of the breaking-change policy.** `Compatibility` is computed and
  available; the gateway does not yet *refuse* a breaking hot-swap. Wiring that
  in needs a place to persist the previous revision — one line once the catalog
  has a store.
- **Live `tools/list` scanning.** `ingest_listing` is the seam and is fully
  tested; calling it from the upstream connect path lands with the MCP client
  work, not here.
- **Output-schema validation.** We publish `output_schema` when a server gives
  one, but do not validate responses against it. Few servers advertise one; the
  validator is already there when they do.
- **Cryptographic schema signing.** `fingerprint` answers "did this change?",
  not "who says so?" Trust in an upstream MCP is a connection-level concern.

---

## A2A + Agent Card

**Full spec: [`docs/A2A_AGENT_CARD.md`](docs/A2A_AGENT_CARD.md).** This section
is the summary; the doc is authoritative.

### Scope

How an AVM agent advertises itself (the **Agent Card**) and how it accepts
inbound work from another agent (the **A2A task protocol**). Aligned with the
Linux Foundation A2A convention — the card is served from
`GET /.well-known/agent-card.json` so third-party A2A clients interoperate
without special-casing. Schema id: `avm.a2a/v1`.

### Agent Card format

A JSON document describing one agent: identity (`agent_id`, `name`,
`description`, `version`, `url`), the AVM `scope` it runs under, its backing
`model_ref`, its advertised `capabilities[]` (each with JSON-Schema
`input_schema` / `output_schema`), the `mcp_servers[]` it depends on, and its
inbound `auth_policy`.

`model_ref.uri` is canonical and content-addressable —
`anthropic://claude-sonnet-4-6` for a hosted model, `oci://…@sha256:…` for a
self-hosted weight artifact — which is what lets the scheduler key model
residency off the card.

The example document lives at
`avm-agent/tests/fixtures/agent-card.example.json` and is asserted
field-for-field against `AgentCard::example()` in CI, so the published spec
cannot drift from the code.

### A2A task invocation

`POST /a2a/task` carries an `A2ATask`; the reply is an `A2AResponse`.

- `task_id` is a **caller-generated idempotency key** — a replayed id returns
  the original response rather than starting a second run, which is what makes
  retry safe.
- `context.scope` is a ceiling the callee must not widen: the tenant boundary
  survives the agent hop.
- `context.capability` opts the task into JSON-Schema validation at the gateway,
  so a malformed caller is rejected before any tokens are spent.
- `timeout.deadline` (absolute, RFC-3339) beats `timeout.seconds` (relative)
  because an absolute deadline survives queue hops without drifting. Budget only
  ever shrinks down a fan-out tree.
- Response carries `usage` (tokens, tool calls, duration, `cost_micro_usd` as an
  integer — this number ends up on an invoice).

Two execution modes: synchronous (`200` + terminal status) and asynchronous
(`202` + `accepted`, work enqueued on `avm.jobs.<tenant>.<project>`, caller
polls). Only the synchronous echo path exists today.

### Discovery / registration

An agent self-registers its card with the control plane at startup
(`A2AService.RegisterAgentCard`), stored scope-keyed as an upsert on
`(scope, agent_id)`. Callers resolve `agent_id` → base URL through the control
plane, fetch the card, match a capability, then submit. Cards are cacheable;
`wrong_target` and `unknown_capability` are the two errors that mean "your
cached card is stale — re-fetch".

`source_agent.card_url` is a claim, not proof: the allow-list is checked against
the *authenticated* identity (mTLS SAN or bearer subject), never the
self-reported id.

### Error handling / timeouts

Eleven error codes, each with a fixed HTTP status and a derived `retryable`
flag. The split is enforced by test: 4xx ⇒ caller's fault, never retryable;
429/5xx ⇒ ours, retryable. A caller needs exactly one bit to implement backoff.

`rejected` (pre-execution failure) is distinct from `failed` (the agent ran and
failed) — `rejected` guarantees nothing ran, no side effects, safe to treat as a
no-op. Validation runs cheapest-and-most-diagnostic first: schema → envelope →
target → capability → auth → input schema → quota → execute.

On timeout: `SIGTERM`, wait, `SIGKILL`, answer `timed_out`/`504`. A task already
past its deadline on ingest is rejected rather than started.

### What landed

| Piece | State |
|---|---|
| `avm-agent` crate: `AgentCard`, `A2ATask`, `A2AResponse` + serde | done |
| Protobuf mirror in `proto/avm_service.proto` + `service A2AService` | spec only (`protoc` absent locally; `build.rs` stubs the module) |
| `GET /.well-known/agent-card.json` | test-only — serves the example card |
| `POST /a2a/task` | test-only — validates the envelope, then echoes |
| Spec-fixture contract tests (doc ⇄ code) | done |

### Deferred

Auth enforcement (bearer/mTLS), async dispatch onto the job bus plus
`GET /a2a/task/{id}`, card registration/resolution in the control plane, and
idempotent replay of a repeated `task_id`. Auth lands **before** dispatch —
shipping dispatch first would expose an unauthenticated job-submission path.
---
## Observability & OpenTelemetry

**Crate:** `avm-otel` · **Endpoints:** `GET /metrics`,
`POST /config/instrumentation`, `GET /config/instrumentation`,
`DELETE /config/instrumentation` · **Docs:**
[`docs/observability/otel-architecture.md`](docs/observability/otel-architecture.md),
[`otel-setup.md`](docs/observability/otel-setup.md),
[`trace-walkthrough.md`](docs/observability/trace-walkthrough.md),
[`agent-instrumentation.md`](docs/observability/agent-instrumentation.md)

### Problem

AVM dispatches someone else's code, on someone else's behalf, onto a node the
tenant never sees. When a job is slow, stuck, or wrong, the three questions are
always the same: *where did the time go*, *which hop dropped it*, and *was it
us or the agent*. None of those are answerable from per-service logs, because
the interesting unit of work — a job — crosses gateway → scheduler → queue →
executor → agent and back.

The second problem is the opposite of the first: an operator who instruments
*everything* in enough detail to debug one tenant has, by construction, built a
cross-tenant surveillance system. Detail and isolation pull against each other,
so the split has to be structural rather than a matter of discipline.

### Design

Two tiers, and the boundary between them is the whole point.

```
 ┌─ platform tier ──────────────── always on, 100% sampled ─────────────┐
 │  span names, timings, outcomes, structural ids (job_id, node_id)     │
 │  no tenant labels on platform metric series                          │
 └─────────────────────────────────────────────────────────────────────┘
 ┌─ tenant tier ─────────────── opt-in per tenant/project ──────────────┐
 │  off → basic → detailed                                             │
 │  basic:    tenant/project-labelled metric series + scoped sampling   │
 │  detailed: + W3C context injected into the agent process            │
 └─────────────────────────────────────────────────────────────────────┘
```

`avm-otel` is the single seam. Every service calls `init_otel()` at startup and
gets the same resource attributes, the same sampler, the same Prometheus
registry, and the same propagator — so a span emitted by the executor and a
span emitted by the gateway agree on what a `tenant_id` is without either
service owning the convention.

**Modules and why each exists:**

| module | responsibility |
|---|---|
| `config` | env parsing — `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SDK_DISABLED`, `OTEL_TRACES_SAMPLER`, `OTEL_RESOURCE_ATTRIBUTES`, protocol |
| `resource` | auto-detected service identity: name, version from `CARGO_PKG_VERSION`, hostname, pid; explicit attrs override detection |
| `propagation` | W3C `traceparent`/`tracestate` parse + emit, child derivation, NATS header carrier, agent env carrier |
| `sampler` | head-based decision at ingress; `always_on`, `always_off`, `traceidratio`, `parentbased_*` |
| `metrics` | dependency-free Prometheus exposition — counters, gauges, histograms, label escaping, name sanitisation |
| `level` | the `off`/`basic`/`detailed` ladder, fail-closed parsing, tenant-vs-project row resolution |
| `fields` | the naming contract: dotted lowercase span names, snake_case identity fields |

**Trace shape.** One trace id spans the journey; the parent links reproduce the
shape documented in the walkthrough and asserted in the integration test:

```
gateway.dispatch_job
  ├─ scheduler.place_job
  │    └─ scheduler.node_selection_score
  ├─ executor.run_container
  │    ├─ container.pull_image
  │    └─ container.run
  └─ queue.publish_result
```

Context rides HTTP headers inbound and NATS message headers internally, so the
hop that most systems lose — the async queue hand-off — is the one that is
explicitly carried on `JobMessage` (`traceparent`, `tracestate`,
`instrumentation_level`) rather than inferred.

**Sampling.** Platform traces are 100% sampled: an operator debugging AVM
itself cannot be told the interesting trace was the one that got dropped. The
tenant tier narrows from there and **never widens** — a tenant ratio can only
subtract from the platform decision, and an upstream `sampled=0` is honoured
end to end. Both properties are tests, not comments.

**Metric hierarchy.** Platform families (`avm_gateway_request_duration_seconds`,
`avm_scheduler_job_placement_duration_seconds`,
`avm_executor_container_duration_seconds`, `avm_queue_message_size_bytes`,
`avm_queue_depth`) carry no tenant label at all — so the always-on tier cannot
leak tenant cardinality even by accident. Tenant families
(`avm_tenant_job_count_total`, `avm_tenant_job_duration_seconds`,
`avm_tenant_model_cache_hit_ratio`) only ever get a series when that tenant is
at `basic` or above; `off` emits nothing, which is the single most important
test in the suite.

### Threat model

The architecture doc carries this in full; the short version, because it is the
reason for the tier split:

- **What logging reveals.** Structural facts only — job ids, tenant/project
  ids, timings, outcomes, image refs, node ids. Never job payloads, prompts,
  agent stdout content, auth tokens, model weights, or env values. Agent
  stdout is correlated, not captured, unless the tenant asks for it.
- **Who can access it.** Platform telemetry goes to the operator's backend and
  is operator-visible by definition. Tenant-labelled series exist only for
  tenants that opted in, so a compromised dashboard cannot enumerate tenants
  that never enabled instrumentation.
- **Cardinality as a DoS surface.** Every label value that touches a metric
  name is sanitised and the outcome labels are a closed set — an agent cannot
  inflate the registry by returning novel outcome strings.
- **Kill switch.** `OTEL_SDK_DISABLED=true` wins over a configured endpoint,
  and an unset endpoint makes the whole thing a no-op rather than an error —
  telemetry can never be the reason a job fails to run.

### Testing

100+ tests across the workspace, all green. The ones that carry the design:

- `off_emits_no_tenant_series` — opt-in is real, in both `avm-otel` and the executor
- `tenant_ratio_narrows_but_never_widens`, `sampling_respects_an_upstream_drop`
- `one_trace_id_spans_the_whole_journey`, `parent_links_reproduce_the_documented_shape`
- `only_detailed_reaches_the_agent_process`, `detailed_without_a_sampled_parent_injects_nothing`
- `requeue_does_not_widen_instrumentation` — a retry cannot escalate a tenant's level
- `outcome_labels_are_low_cardinality`, `label_values_are_escaped`, `names_are_sanitized`
- `init_is_a_noop_without_an_endpoint`, `sdk_disabled_wins_over_a_configured_endpoint`
- `observability_fields_are_optional_on_the_wire` — old producers still parse

### Deliberately out of scope

- **Tail-based sampling.** Head-based only. Tail sampling belongs in the
  collector, where it can see the whole trace; the collector config in
  `deploy/otel/` is the place it lands, and nothing here blocks it.
- **A shipped collector sidecar.** `docker-compose.otel.yml` runs collector +
  Jaeger + Prometheus for local dev. Per-node sidecar deployment is an ops
  decision, not a code one.
- **Log export over OTLP.** Logs are structured and trace-correlated through
  `tracing`; shipping them as OTLP log records is a collector receiver away and
  deliberately not a second export path in-process.
- **Agent SDK.** Agents publish custom metrics via the result envelope or their
  own OTLP endpoint. We define the contract, not a client library.
