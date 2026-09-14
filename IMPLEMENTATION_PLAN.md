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
