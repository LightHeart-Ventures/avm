# Trace walkthrough: one job, dispatch to completion

An annotated read of a real AVM trace. The point is not the span names — it is
knowing which span to blame when the number is wrong.

Scenario: tenant `t_acme`, project `b_pipeline`, agent `ag_summarize`, dispatched
over HTTP, executed, result published. The tenant is opted into `detailed`.

## The trace

```
gateway.dispatch_job ─────────────────────────────────────────── 4.21s
│ trace_id=4bf92f3577b34da6a3ce929d0e0e4736  span=00f067aa0ba902b7
│ http.route=/jobs  http.method=POST  http.status_code=202
│ tenant_id=t_acme  project_id=b_pipeline  agent_id=ag_summarize
│ job_id=job_01HQ...  instrumentation_level=detailed
│
├─ queue.publish ──────────────────────────────────────── 0.008s
│    subject=avm.jobs  messaging.system=nats
│    messaging.message_body_size=412
│
├─ scheduler.place_job ────────────────────────────────── 0.031s
│  │  strategy=requeue
│  └─ scheduler.node_selection_score ────────────────── 0.002s
│       candidates=1
│
├─ executor.run_container ─────────────────────────────── 4.15s
│  │  executor_id=ex_9f2c  executor_type=process  outcome=succeeded
│  ├─ container.pull_image ──────────────────────────── 0.001s
│  ├─ container.run ─────────────────────────────────── 4.14s
│  │    agent_id=ag_summarize
│  └─ container.cleanup ─────────────────────────────── 0.000s
│
└─ queue.publish_result ───────────────────────────────── 0.009s
     subject=avm.results  outcome=succeeded
```

## Reading it hop by hop

### 1. `gateway.dispatch_job` — the root

Created by the HTTP middleware. Two things happen here that determine the rest
of the trace:

* **Trace identity.** If the caller sent a `traceparent`, the gateway continues
  that trace and this span has a parent outside AVM. Otherwise it mints a root.
  Either way the response echoes `traceparent`, so a client can stitch on.
* **Instrumentation level.** Resolved once from the opt-in store and stamped
  into the job envelope. `instrumentation_level=detailed` on this span is why
  the agent later receives trace context — nothing downstream re-decides.

The span's duration is *not* the job duration: the HTTP request returns `202`
as soon as the job is on JetStream. It is 4.21s here only because this diagram
shows the logical job, not the request. In the real trace the root closes in
~50ms and the executor spans are linked children that finish later.

**If this span is slow:** auth, or the Postgres insert. Look at the child spans,
not the total.

### 2. `queue.publish` — envelope onto JetStream

`messaging.message_body_size` is the wire size of the envelope. It feeds
`avm_queue_message_size_bytes{subject="avm.jobs"}`. A histogram creeping right
means payloads are growing — which matters because the whole envelope is held
in memory per in-flight job.

The subject label is the stream root (`avm.jobs`), not the fully qualified
subject, which would embed tenant and project and blow up cardinality. The
tenant is on the *span*, where high cardinality is free.

**If this span is slow:** NATS is backed up or the stream is at its limit.
Cross-check `avm_queue_depth`.

### 3. `scheduler.place_job` — placement

Only appears for jobs the scheduler touched: a requeue after a crash gap, or a
constrained placement. A straight-through dispatch may not have one at all.

`scheduler.node_selection_score` is a `DEBUG` span — it will be missing unless
`RUST_LOG` includes `avm_scheduler=debug`. It carries the candidate count, which
is the first thing to check when placement picks something surprising.

Note the trace id: a **requeued** job gets a *fresh* root trace, not this one.
Stitching a new execution under a span that already closed would misrepresent
the timeline — so a requeue is honestly a new trace, linked by `job_id`.

**If this span is slow:** quota lookups or a large `queued` backlog.

### 4. `executor.run_container` — the work

The span that usually explains the trace. Three children, deliberately stable
across executor backends even when a backend has nothing to do for one of them:

| Child | What it covers | Typical |
|---|---|---|
| `container.pull_image` | resolving/staging the agent binary or image | ~0 for `process`, seconds for OCI on a cold cache |
| `container.run` | the agent process itself, stdin → stdout | the bulk |
| `container.cleanup` | reaping, closing pipes, unmounting | ~0 |

`outcome` is recorded on span close and is low-cardinality by construction:
`succeeded`, `failed`, `timeout`, `spawn_failed`, `unresolved`. It is the same
string used as the metric label, so a spike in
`avm_executor_container_duration_seconds{outcome="timeout"}` maps directly onto
traces you can open.

Because the tenant is at `detailed`, the agent process received
`TRACEPARENT`, `OTEL_TRACE_ID`, `OTEL_SPAN_ID` and `OTEL_PARENT_SPAN_ID`
pointing at `container.run`. An instrumented agent's own spans appear as
children here. At `off` or `basic` the agent gets none of these, and this
subtree simply ends at `container.run`.

**If `container.run` is slow:** that is the agent, not AVM. Compare against
`avm_tenant_job_duration_seconds` for the same project to see whether it is this
run or every run.

### 5. `queue.publish_result` — the result envelope

Carries `traceparent` back in `ResultMessage`, so a caller polling for the
result can join the original trace rather than starting a new one.

At `detailed`, agent-published custom metrics ride along in the result envelope
and are ingested here — see
[agent-instrumentation.md](agent-instrumentation.md).

## Failure shapes

**Agent timeout.** `container.run` ends at exactly the wall-time limit,
`executor.run_container` has `outcome=timeout`, `queue.publish_result` still
happens with `status=failed`. The result envelope is always published; a missing
`queue.publish_result` means the executor itself died, which is a different
incident.

**Trace stops after `queue.publish`.** The envelope reached JetStream but no
executor consumed it. Check consumer health and `avm_queue_depth` — the job is
not lost, it is waiting.

**Two traces for one `job_id`.** A requeue happened. Expected, not a bug; the
second trace is the execution that counted.

**Trace with no `tenant_id` attributes.** System-scoped job (`Scope::system()`),
typically a scheduler-internal operation.

## Finding a trace

```
# By job
{ job_id = "job_01HQ..." }

# Every failure for one tenant
{ tenant_id = "t_acme" && outcome = "failed" }

# Slow agent runs
{ name = "container.run" && duration > 30s }

# Jobs that never got executed
{ name = "queue.publish" } without { name = "executor.run_container" }
```

The `job_id` attribute is the join key between traces, logs, and the `jobs`
table — every structured log line inside a job span carries it, and so does
every row.
