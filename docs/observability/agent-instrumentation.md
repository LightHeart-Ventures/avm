# Agent instrumentation

How an agent running on AVM publishes its own traces, metrics and logs.

Agents are opaque processes in any language, so the contract is environment
variables in, JSON out. Nothing here requires an OTel SDK — but if you have one,
it slots straight in.

## 1. What the agent receives

Always:

| Variable | Example |
|---|---|
| `AVM_JOB_ID` | `job_01HQ8...` |
| `AVM_AGENT_ID` | `ag_summarize` |
| `AVM_TENANT_ID` | `t_acme` |
| `AVM_PROJECT_ID` | `b_pipeline` |
| `AVM_SCOPE` | `project` |

Only when the tenant is opted into **`detailed`**:

| Variable | Example |
|---|---|
| `TRACEPARENT` | `00-4bf92f35...-a1b2c3d4e5f60718-01` |
| `OTEL_TRACE_ID` | `4bf92f3577b34da6a3ce929d0e0e4736` |
| `OTEL_SPAN_ID` | `a1b2c3d4e5f60718` (the agent's own span id) |
| `OTEL_PARENT_SPAN_ID` | `00f067aa0ba902b7` (`container.run`) |

At `off` and `basic` **none** of the trace variables are set. Write your agent
so telemetry is optional — absence of `TRACEPARENT` means "do not trace", not
"start your own trace".

## 2. Joining the AVM trace

### With an OTel SDK

Every SDK reads `TRACEPARENT` from the environment when you extract from a
carrier. Python:

```python
import os
from opentelemetry import trace
from opentelemetry.propagate import extract

ctx = extract({"traceparent": os.environ["TRACEPARENT"]}) if "TRACEPARENT" in os.environ else None

tracer = trace.get_tracer("ag_summarize")
with tracer.start_as_current_span("agent.summarize", context=ctx) as span:
    span.set_attribute("input.tokens", n_tokens)
    ...
```

Point the SDK at the same collector AVM uses:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
```

Your spans then appear as children of `container.run` in the same trace.

### Without an SDK

Echo the ids into your own logs and the correlation still works:

```python
import json, os, sys

def log(level, message, **fields):
    print(json.dumps({
        "level": level,
        "message": message,
        "trace_id": os.environ.get("OTEL_TRACE_ID", ""),
        "span_id": os.environ.get("OTEL_SPAN_ID", ""),
        "job_id": os.environ["AVM_JOB_ID"],
        **fields,
    }), file=sys.stderr)

log("info", "loaded model", model="llama-3.1-8b", cache="hit")
```

At `detailed`, AVM captures agent **stderr** and re-emits each line as a log
event correlated to `container.run` (capped at 200 lines per run, so a chatty
agent cannot flood the pipeline). Keep stdout clean — that is your result.

## 3. Publishing custom metrics

Two routes. Pick based on whether you want your metrics in AVM or in your own
backend.

### Route A — the result envelope (no dependencies)

Include a `metrics` object in your JSON result. AVM ingests it into the tenant
metric namespace when the tenant is at `basic` or above.

```json
{
  "output": "…",
  "metrics": {
    "tokens_in": 1843,
    "tokens_out": 512,
    "model_cache_hit": 1,
    "retrieval_latency_ms": 84.2
  }
}
```

Rules:

* Values must be numbers. Non-numeric entries are dropped, not errors.
* Keys are sanitised to `[a-z0-9_]` and prefixed: `tokens_in` becomes
  `avm_agent_tokens_in`.
* Labelled with `tenant_id`, `project_id`, `agent_id`.
* Keep the key set **fixed**. A key derived from input (a user id, a filename)
  is a new metric series per job — that is how you get a metrics bill.

### Route B — OTLP direct (full control)

Export from the agent process to a collector:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
OTEL_SERVICE_NAME=ag_summarize
```

You own the names and the cardinality; AVM does not see or bill these. Use this
when you already have an OTel setup, or want histograms AVM does not model.

Local-sidecar variant: the executor can mount `/var/lib/avm/otel` with a
collector socket, so agents export without network egress. Optional and
deployment-specific — see [otel-setup.md](otel-setup.md).

## 4. Rules that are not negotiable

**Never put payload content in telemetry.** Span attributes, metric labels and
log fields are for structure — counts, durations, outcomes, identifiers. Not
prompts, documents, user data or credentials. AVM scrubs the obvious keys at the
collector (`job.payload`, `authorization`, `api_key`) but that is a backstop,
not a permission.

**Never label a metric with unbounded values.** `model="llama-3.1-8b"` is fine
(bounded set). `user_id=…` or `document_id=…` is not. If you need per-item
detail, it belongs on a *span*, where cardinality costs nothing.

**Exit codes are the contract.** Exit `0` with JSON on stdout for success,
non-zero for failure. AVM maps that to `outcome` on the executor span and to
`avm_tenant_job_count_total{outcome=…}`. Do not signal failure by writing an
error to stdout and exiting 0 — the metrics will say you succeeded.

**Do not assume telemetry is on.** The tenant may be at `off`. Every code path
that touches `TRACEPARENT` or an exporter must no-op cleanly when it is absent.

## 5. Checklist

- [ ] Agent runs correctly with no `OTEL_*` / `TRACEPARENT` variables set.
- [ ] Structured logs go to **stderr**; the result goes to **stdout**.
- [ ] Custom metric keys are a fixed set, defined in code.
- [ ] No payload content, prompts or credentials in any field.
- [ ] Non-zero exit on failure.
- [ ] If you export OTLP yourself, you have your own endpoint configured — the
      AVM default is not guaranteed to be reachable from the agent sandbox.
