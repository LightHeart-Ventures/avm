# AVM Implementation Plan

Living design doc. One top-level `##` section per subsystem — **append your
section, don't rewrite someone else's.** `ARCHITECTURE.md` describes what AVM
*is*; this file describes how each piece gets built and why it's shaped that
way.

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
