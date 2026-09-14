# AVM Observability Architecture

> Status: implemented. Code lives in [`avm-otel/`](../../avm-otel), wired into
> `avm-gateway`, `avm-scheduler`, `avm-executor` and `avm-queue`.

## 1. Why two layers

AVM runs other people's agents on our infrastructure. That splits observability
into two problems that want opposite defaults:

| | Platform telemetry | Tenant telemetry |
|---|---|---|
| Question it answers | "Is AVM healthy? Where did this job stall?" | "Why is *my* agent slow?" |
| Default | **always on**, 100 % sampled | **off** |
| Cardinality | bounded by code (routes, subjects, outcomes) | grows with customers |
| Who reads it | AVM operators | operator, on behalf of one tenant |
| Contains tenant ids | in span attributes only | in **metric labels** too |

Platform telemetry is never sampled away, because the moment you sample it you
lose the ability to answer "what happened to job X" — which is the only question
anyone asks during an incident. It is cheap precisely because it is
low-cardinality: nothing in the platform layer is labelled with a tenant id.

Tenant telemetry is off by default for two independent reasons, and either one
alone would be sufficient:

1. **Cost.** `avm_tenant_job_duration_seconds{tenant_id,project_id}` is one
   histogram per project. At 10 000 projects that is 10 000 × bucket-count
   series, which is how metric bills get to five figures.
2. **Blast radius.** An operator debugging tenant A should not be able to
   accidentally enable, and then read, tenant B's job timing. Opt-in is
   per `(tenant_id, project_id)` and resolution **fails closed** — an unknown
   tenant resolves to `off`, never to a default.

## 2. The three levels

```
off       platform telemetry only. Tenant appears in span attributes
          (needed to debug the platform) but never in a metric label,
          and nothing reaches the agent process.

basic     + tenant/project-labelled metrics
            avm_tenant_job_count_total, avm_tenant_job_duration_seconds

detailed  + trace context injected into the agent process
          + agent stderr captured as correlated log events
          + agent-published custom metrics accepted from the result envelope
```

The level is resolved **once, at the gateway**, and stamped onto the job
envelope (`JobMessage.instrumentation_level`). Every downstream hop reads it
from the envelope rather than re-querying config. That matters for three
reasons: the decision cannot drift mid-job, a replayed JetStream message
reproduces the telemetry it was dispatched with, and the scheduler/executor stay
free of a config dependency they would otherwise have to fail open on.

A requeue by the scheduler resets the level to `off`; it is re-resolved on the
next dispatch. Requeue is the one path that could silently widen instrumentation
beyond what the tenant configured, so it deliberately narrows instead.

## 3. Layer map

```
                    ┌───────────────────────────────────────────┐
                    │                avm-otel                   │
                    │  config · resource · sampler · metrics    │
                    │  propagation · level                      │
                    └───────────────────────────────────────────┘
                        ▲          ▲          ▲          ▲
                   init_otel   init_otel  init_otel  init_otel
                        │          │          │          │
                 ┌──────┴───┐ ┌────┴─────┐ ┌──┴──────┐ ┌─┴───────┐
                 │  gateway │ │ scheduler│ │ executor│ │  queue  │
                 └──────────┘ └──────────┘ └─────────┘ └─────────┘
```

`avm-otel` owns every observability decision so the services cannot disagree
with each other:

| Module | Responsibility |
|---|---|
| `config` | Reads `OTEL_*` env vars into a typed struct |
| `resource` | Detects `service.name`, version, hostname, environment |
| `sampler` | Head-based sampling; platform default is always-on |
| `metrics` | Dependency-free Prometheus registry + the platform families |
| `propagation` | W3C `traceparent` / `tracestate` parse, inject, child, agent env |
| `level` | `InstrumentationLevel` + `InstrumentationConfig` opt-in rules |

## 4. Trace context propagation

Two carriers, one format. Both are W3C Trace Context, because a format that only
works over HTTP is useless the moment a job crosses NATS.

**HTTP (ingress and inter-service):** standard `traceparent` / `tracestate`
headers. The gateway continues an inbound trace when one arrives, and mints a
root when it does not. It always echoes `traceparent` on the response so a
caller can stitch its own client span onto ours.

**NATS (job dispatch and result publish):** the context travels *inside* the
envelope as `JobMessage.traceparent` / `.tracestate`, not as a NATS header.
This is deliberate — envelope fields survive JetStream replay, appear in
`nats stream view`, and mean the trace is recoverable from a stored message
days later.

```
gateway.dispatch_job                      trace_id=abc123  span=aaaa
  ├─ scheduler.place_job                  trace_id=abc123  parent=aaaa
  │    └─ scheduler.node_selection_score  trace_id=abc123  parent=bbbb
  ├─ executor.run_container               trace_id=abc123  parent=aaaa
  │    ├─ container.pull_image            trace_id=abc123  parent=cccc
  │    ├─ container.run                   trace_id=abc123  parent=cccc
  │    └─ container.cleanup               trace_id=abc123  parent=cccc
  └─ queue.publish_result                 trace_id=abc123  parent=cccc
```

Every scoped span carries `tenant_id`, `project_id`, `job_id`, and `agent_id`
when known. See [trace-walkthrough.md](trace-walkthrough.md) for a real one,
annotated.

## 5. Sampling policy

| Scope | Policy | Rationale |
|---|---|---|
| Platform | 100 %, always on | An incident you sampled away is an incident you cannot debug |
| Tenant | configurable ratio, default 1.0 when enabled | Cost control for a chatty tenant |
| Upstream decision | always honoured | A caller that sampled *out* is never resurrected |

Sampling is **head-based**: the decision is made at ingress and encoded in the
`traceparent` sampled flag, so every downstream hop inherits it without needing
to re-decide. The ratio sampler is deterministic on the trace id — the same
trace always gets the same verdict at every service, which is what stops
partial traces from appearing.

Per-tenant ratios **narrow but never widen**: a tenant ratio cannot cause a span
to be sampled that the platform sampler dropped.

Tail-based sampling (e.g. "keep every trace that contains an error") belongs in
the collector, not the SDK, and is deliberately deferred — it needs a collector
in the path, which is optional today.

## 6. Metric families

Platform — always registered, never tenant-labelled:

| Metric | Type | Labels |
|---|---|---|
| `avm_gateway_request_duration_seconds` | histogram | `endpoint`, `status` |
| `avm_scheduler_job_placement_duration_seconds` | histogram | `strategy` |
| `avm_executor_container_duration_seconds` | histogram | `executor_type`, `outcome` |
| `avm_queue_message_size_bytes` | histogram | `subject` |
| `avm_queue_messages_total` | counter | `subject`, `direction` |
| `avm_queue_depth` | gauge | `subject` |

Tenant — only written when the level is `basic` or `detailed`:

| Metric | Type | Labels |
|---|---|---|
| `avm_tenant_job_count_total` | counter | `tenant_id`, `project_id`, `outcome` |
| `avm_tenant_job_duration_seconds` | histogram | `tenant_id`, `project_id` |
| `avm_tenant_model_cache_hit_ratio` | gauge | `tenant_id`, `model_ref` |

Two cardinality rules are enforced in code, not by convention:

* **`endpoint` is the matched route**, not the raw URI. `/jobs/:id` is one
  series; `/jobs/job_abc123` would be one series per job.
* **`subject` is the stream root** (`avm.jobs`, `avm.results`), not the fully
  qualified subject, which embeds tenant and project.

## 7. Threat model

What the telemetry pipeline can and cannot reveal.

**Never emitted, by construction:**

* Job payloads. `JobMessage.payload` is never a span attribute or log field.
* Agent stdout. It goes into the result envelope, not the log pipeline.
  Agent *stderr* is re-emitted only at `detailed`, capped at 200 lines per run.
* Auth tokens, API keys, model weights, memory contents.

**Emitted, and treated as sensitive:**

* `tenant_id` / `project_id` / `agent_id` / `job_id` — structural identifiers.
  They appear in platform span attributes even at `off`, because the platform
  layer is useless without them. Anyone with trace access can therefore infer
  *that* a tenant ran a job, its timing, and its outcome — not *what* it did.
* Timing and outcome distributions, which leak coarse usage patterns.

**Access model:** the OTLP endpoint is operator-controlled and per-deployment.
AVM does not multiplex tenants onto a shared backend; if you need to give a
tenant their own telemetry, point *their* agents at *their* collector via
`OTEL_EXPORTER_OTLP_ENDPOINT` in the agent environment — the platform pipeline
stays separate.

**Trace ids are not secrets and authenticate nothing.** They are generated from
a clock plus a process-local counter, which is enough for uniqueness within a
deployment and is not a CSPRNG. Nothing in AVM should ever treat possession of a
trace id as proof of anything.

**Kill switch:** `OTEL_SDK_DISABLED=true` wins over every other setting,
including a configured endpoint. When no endpoint is configured, the SDK is a
no-op — instruments still record into the local Prometheus registry (so
`/metrics` keeps working) but nothing leaves the process.

## 8. Failure posture

Telemetry never fails a job:

* A collector that is down does not block dispatch or execution — export is
  fire-and-forget, and a failed depth read is logged at `debug` and swallowed.
* An unparseable inbound `traceparent` starts a new root rather than rejecting
  the request.
* A poison job envelope is counted (`direction="poison"`) and terminated rather
  than retried into a loop.

## 9. See also

* [otel-setup.md](otel-setup.md) — configuring Grafana Cloud, Honeycomb, Datadog
* [trace-walkthrough.md](trace-walkthrough.md) — one job, annotated end to end
* [agent-instrumentation.md](agent-instrumentation.md) — custom agent telemetry
