# AVM Implementation Plan

Living design doc. One top-level `##` section per subsystem — **append your
section, don't rewrite someone else's.** `ARCHITECTURE.md` describes what AVM
*is*; this file describes how each piece gets built and why it's shaped that
way.

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
