# A2A + Agent Card

Status: **spec + test-only scaffolding.** Schema id `avm.a2a/v1`.
Types: [`avm-agent`](../avm-agent). Endpoints: [`avm-gateway/src/a2a_router.rs`](../avm-gateway/src/a2a_router.rs).
Protobuf mirror: [`proto/avm_service.proto`](../proto/avm_service.proto) (`AgentCard`, `A2ATask`, `A2AResponse`, `service A2AService`).

This document defines two things:

1. **The Agent Card** — how an AVM agent *advertises itself*.
2. **The A2A task protocol** — how an agent *accepts inbound work* from another agent.

It follows the Linux Foundation A2A convention: a card is served from
`GET /.well-known/agent-card.json`, so an off-the-shelf A2A client can discover
an AVM agent without special-casing. AVM-specific fields (`scope`,
`mcp_servers`, `model_ref`) are additive.

> **Wire format of record is JSON over HTTP.** The protobuf messages exist for
> in-cluster gRPC clients. They mirror the JSON; where they disagree, the JSON
> in this document wins. Nested JSON (`input_schema`, `input`, `output`) is
> carried as a serialized string in protobuf so schema evolution never requires
> a `protoc` round-trip.

---

## 1. Agent Card format

`GET /.well-known/agent-card.json` → `200 application/json`

The document below is the canonical example. It is
[`avm-agent/tests/fixtures/agent-card.example.json`](../avm-agent/tests/fixtures/agent-card.example.json),
asserted field-for-field against `AgentCard::example()` by
`spec_fixtures::documented_card_has_no_undocumented_fields` — if this drifts
from the code, CI fails.

```json
{
  "schema_version": "avm.a2a/v1",
  "agent_id": "ag_pr_reviewer",
  "name": "PR Reviewer",
  "description": "Reviews pull requests for correctness, security, and style.",
  "version": "0.1.0",
  "url": "http://127.0.0.1:8080",
  "scope": {
    "level": "project",
    "tenant_id": "t_lightheart",
    "project_id": "b_avm",
    "agent_id": ""
  },
  "model_ref": {
    "uri": "anthropic://claude-sonnet-4-6",
    "provider": "anthropic",
    "model": "claude-sonnet-4-6"
  },
  "capabilities": [
    {
      "name": "review_pull_request",
      "description": "Review a GitHub pull request and return findings.",
      "input_schema": {
        "type": "object",
        "properties": {
          "repo": { "type": "string" },
          "pr_number": { "type": "integer" }
        },
        "required": ["repo", "pr_number"]
      },
      "output_schema": {
        "type": "object",
        "properties": {
          "findings": { "type": "array", "items": { "type": "string" } },
          "verdict": { "enum": ["approve", "request_changes", "comment"] }
        },
        "required": ["verdict"]
      },
      "tags": ["code", "github"]
    },
    {
      "name": "summarise_diff",
      "description": "Summarise a unified diff in prose."
    }
  ],
  "mcp_servers": [
    {
      "name": "avm-gateway",
      "transport": "http",
      "endpoint": "http://127.0.0.1:8080/mcp",
      "tools": ["avm_memory_read", "avm_memory_write", "avm_job_submit"]
    }
  ],
  "auth_policy": {
    "schemes": [{ "type": "bearer" }, { "type": "mtls" }],
    "required_scopes": ["a2a:submit"],
    "allowed_agents": ["ag_planner"],
    "allow_anonymous": false
  },
  "metadata": {
    "owner": "platform",
    "repo": "LightHeart-Ventures/avm"
  }
}
```

### Field reference

| Field | Required | Notes |
|---|---|---|
| `schema_version` | yes | `avm.a2a/v1`. A consumer MUST reject an unknown **major**; minor bumps are additive-only. |
| `agent_id` | yes | Stable AVM id. Authoritative — `name` is advisory. |
| `name`, `description`, `version` | yes | `version` is the agent implementation's semver, not the schema's. |
| `url` | yes in practice | Base URL the card was served from. A2A paths hang off it; `card_url` / task URL are derived by string join. |
| `scope` | no | The AVM `system → tenant → project → agent` scope the agent runs under. Absent ⇒ `system`. |
| `model_ref` | yes | See below. |
| `capabilities[]` | no | Empty ⇒ the agent accepts only free-text instructions with no capability routing. |
| `mcp_servers[]` | no | Declares dependencies so the scheduler can verify reachability before dispatch. |
| `auth_policy` | yes | Deny-by-default; see §4. |
| `metadata` | no | Free-form string map. Never load-bearing. |

**`model_ref`** — `uri` is the canonical, content-addressable form and is what
the scheduler keys model residency on:

| Form | Example |
|---|---|
| Hosted API | `anthropic://claude-sonnet-4-6` |
| OCI weight artifact | `oci://ghcr.io/lightheart/qwen3-8b@sha256:ab12…` (set `digest`) |
| Node-local | `local://qwen3-8b-q4` |

**`capabilities[]`** — `input_schema` / `output_schema` are JSON-Schema
documents. They are the contract for `A2ATask.context.input` and
`A2AResponse.result.output` respectively. Validation is the **gateway's** job,
not the agent's: a task that fails input validation is rejected with
`invalid_task` before the model is ever invoked, so a malformed caller never
burns tokens.

---

## 2. A2A task invocation protocol

`POST /a2a/task` — body is an `A2ATask`, response is an `A2AResponse`.

### Request

[`avm-agent/tests/fixtures/a2a-task.example.json`](../avm-agent/tests/fixtures/a2a-task.example.json):

```json
{
  "schema_version": "avm.a2a/v1",
  "task_id": "task_9f2c1b7e4a8d4f0b9c3e5a7d1f2b4c60",
  "source_agent": {
    "agent_id": "ag_planner",
    "name": "Planner",
    "card_url": "http://10.0.1.7:8080/.well-known/agent-card.json"
  },
  "target_agent": "ag_pr_reviewer",
  "instructions": "Review PR #42 in LightHeart-Ventures/avm and report blocking issues.",
  "context": {
    "scope": {
      "level": "project",
      "tenant_id": "t_lightheart",
      "project_id": "b_avm",
      "agent_id": ""
    },
    "capability": "review_pull_request",
    "correlation_id": "4bf92f3577b34da6a3ce929d0e0e4736",
    "input": { "repo": "LightHeart-Ventures/avm", "pr_number": 42 }
  },
  "timeout": { "seconds": 300, "deadline": "2026-01-01T00:05:00Z" },
  "created_at": "2026-01-01T00:00:00Z"
}
```

| Field | Notes |
|---|---|
| `task_id` | **Caller-generated idempotency key.** Re-submitting the same `task_id` MUST return the original response, not start a second run. This is what makes a caller's retry safe. |
| `source_agent` | An `AgentRef`. `card_url` lets the receiver fetch the caller's card to verify identity claims out-of-band. |
| `target_agent` | Advisory but checked: a mismatch is `wrong_target` (404), never a silent accept. Catches stale service discovery instead of running the wrong agent's work. |
| `instructions` | Free text. Required and non-blank. |
| `context.scope` | The scope the work executes under. **The callee MUST NOT widen it** — this is how the tenant boundary survives an agent hop. |
| `context.capability` | When set, must be on the callee's card. Enables schema validation and routing. |
| `context.parent_task_id` | Fan-out lineage. |
| `context.correlation_id` | Propagated into OTel spans so one trace spans the whole agent graph. |
| `context.artifacts[]` | Inline `content` or out-of-band `uri` — never both. |
| `timeout` | See §4. |

### Response

`202` when queued, `200` when complete, `4xx`/`5xx` per §4.

```json
{
  "schema_version": "avm.a2a/v1",
  "task_id": "task_9f2c1b7e4a8d4f0b9c3e5a7d1f2b4c60",
  "status": "succeeded",
  "result": {
    "content": "Two blocking issues: unbounded retry loop in publisher.rs; missing scope check on read path.",
    "output": { "verdict": "request_changes", "findings": ["unbounded retry", "missing scope check"] }
  },
  "usage": {
    "input_tokens": 18244,
    "output_tokens": 962,
    "tool_calls": 7,
    "duration_ms": 41230,
    "cost_micro_usd": 68400
  },
  "updated_at": "2026-01-01T00:00:41Z"
}
```

`status` ∈ `accepted` · `running` · `succeeded` · `failed` · `rejected` ·
`timed_out` · `cancelled`. The first two are non-terminal; the rest are final.
`result` is present **iff** `succeeded`; `error` is present **iff** a failure
state. `usage` is zeroed while non-terminal.

`cost_micro_usd` is an integer to keep the wire format exact — floats accumulate
error across a fan-out tree and this number ends up on an invoice.

### Execution modes

| Mode | Behaviour |
|---|---|
| **Synchronous** | Callee runs inline and returns `200` with a terminal status. Appropriate only when the work fits well inside the caller's deadline. |
| **Asynchronous** (target) | Callee validates, enqueues a `JobMessage` on `avm.jobs.<tenant>.<project>`, returns `202` + `accepted`. Caller polls `GET /a2a/task/{task_id}` until terminal. |

The gateway's current implementation is synchronous and echo-only; the async
path lands with executor integration (§6).

---

## 3. Discovery / registration flow

```
┌──────────┐                                        ┌──────────────┐
│ ag_planner│                                       │ ag_pr_reviewer│
└────┬─────┘                                        └──────┬───────┘
     │  1. resolve agent_id → base URL (control plane)     │
     │────────────────────────────────────────────────────▶│
     │  2. GET /.well-known/agent-card.json                │
     │────────────────────────────────────────────────────▶│
     │◀──────────────── AgentCard ─────────────────────────│
     │  3. validate schema_version, match capability       │
     │  4. POST /a2a/task  (+ credential per auth_policy)  │
     │────────────────────────────────────────────────────▶│
     │◀──────── 202 accepted │ 200 succeeded ──────────────│
     │  5. (async) poll GET /a2a/task/{task_id}            │
     │────────────────────────────────────────────────────▶│
```

**Registration.** An agent self-registers its card with the control plane at
startup (`A2AService.RegisterAgentCard`, or the HTTP equivalent). The control
plane stores it scope-keyed and serves resolution for step 1, so callers never
hard-code peer URLs. Registration is an upsert on `(scope, agent_id)`.

**Caching.** Cards are cacheable; honour `ETag`/`Cache-Control`. A caller MUST
re-fetch on `wrong_target` or `unknown_capability` before giving up — those two
errors are the signal that a cached card went stale.

**Trust.** `source_agent.card_url` is a *claim*, not proof. Under `mtls` the
peer certificate SAN is authoritative; under `bearer` the token's subject is.
The allow-list in `auth_policy.allowed_agents` is checked against the
*authenticated* identity, never against the self-reported `agent_id`.

---

## 4. Error handling and timeout semantics

### Error taxonomy

Every failure carries a machine-readable `code`, a human `message`, and an
explicit `retryable` flag. The flag is derived from the code, never set by
hand — a caller implementing backoff needs one bit, not a string match.

| `code` | HTTP | Retryable | Meaning |
|---|---|---|---|
| `unauthenticated` | 401 | no | Credential missing or invalid. |
| `forbidden` | 403 | no | Authenticated, but not permitted. |
| `invalid_task` | 400 | no | Malformed envelope, blank instructions, `timeout.seconds == 0`. |
| `unsupported_schema` | 400 | no | `schema_version` major not implemented. |
| `unknown_capability` | 404 | no | Capability not on the card — re-fetch the card. |
| `wrong_target` | 404 | no | `target_agent` is not this agent — re-fetch the card. |
| `overloaded` | 429 | **yes** | Quota or concurrency cap. Back off. |
| `execution_failed` | 422 | no | The agent ran and failed. Retrying the same input won't help. |
| `upstream_failure` | 502 | **yes** | MCP server, model, or database failed. |
| `timeout` | 504 | **yes** | Deadline elapsed. |
| `internal` | 500 | **yes** | Unclassified. |

The 4xx/no-retry vs 429-5xx/retry split is enforced by test
(`every_error_code_has_a_distinct_http_mapping_class`): a caller can treat
"retryable" and "server's fault" as the same predicate and never be wrong.

A failure maps to a `status`: `timeout` → `timed_out`; any pre-execution
rejection (`unauthenticated`, `forbidden`, `invalid_task`,
`unsupported_schema`, `unknown_capability`, `wrong_target`) → `rejected`;
everything else → `failed`. **`rejected` means nothing ran** — no tokens burned,
no side effects, safe to treat as a no-op.

### Validation order

The receiver checks in this order, cheapest and most-diagnostic first:

1. `schema_version` → `unsupported_schema`
2. envelope well-formedness → `invalid_task`
3. `target_agent` → `wrong_target`
4. `context.capability` on the card → `unknown_capability`
5. credential + allow-list → `unauthenticated` / `forbidden`
6. `context.input` against `input_schema` → `invalid_task`
7. quota → `overloaded`
8. execute

### Timeouts

`timeout.seconds` is the caller's relative budget. `timeout.deadline`, when
present, is an absolute RFC-3339 instant and **always wins**: an absolute
deadline survives queue hops without drifting, a relative budget does not.
Default is 300 s; `0` is invalid.

- The receiver SHOULD compute `deadline = created_at + seconds` on ingest if the
  caller omitted it, and propagate that deadline to every sub-task it spawns.
- A sub-task MUST NOT be given a deadline later than its parent's. Budget only
  ever shrinks down a fan-out tree — this is what stops a deep agent graph from
  outliving the request that started it.
- On expiry the receiver sends `SIGTERM`, waits, then `SIGKILL`, and answers
  `timed_out` / `504`. A task already past its deadline on ingest is rejected
  immediately rather than started.
- Callers SHOULD retry `timeout` / `overloaded` / `upstream_failure` with
  exponential backoff and jitter, reusing the **same `task_id`** so the
  idempotency guarantee applies.

---

## 5. Security notes

- **Deny by default.** `AuthPolicy::default()` is bearer-required, no anonymous,
  no allow-list exemptions. `allow_anonymous: true` is only defensible inside a
  trusted network boundary.
- **Scope never widens.** `context.scope` is a ceiling. An agent handed a
  project scope cannot read tenant-level memory on the caller's behalf.
- **Empty allow-list = any *authenticated* caller**, not any caller. The
  distinction matters: `allowed_agents: []` with `allow_anonymous: false` is
  still closed to the public.
- **Error messages must not leak credentials** — they cross a trust boundary.

---

## 6. Implementation status

| Piece | State |
|---|---|
| `AgentCard`, `A2ATask`, `A2AResponse` + serde | **done** (`avm-agent`) |
| Protobuf mirror + `A2AService` | **done** (spec only — `protoc` absent locally, `build.rs` stubs the module) |
| `GET /.well-known/agent-card.json` | **done, test-only** — serves `AgentCard::example()` |
| `POST /a2a/task` | **done, test-only** — validates, then echoes |
| Auth enforcement (bearer/mTLS) | not started |
| Async dispatch → `avm.jobs.*` + `GET /a2a/task/{id}` | not started |
| Card registration + resolution in the control plane | not started |
| Idempotent replay of a repeated `task_id` | not started |

Next step is auth enforcement, then async dispatch — in that order, because
shipping dispatch before auth would expose an unauthenticated job-submission
path.
