# Configuring an OTel backend for AVM

AVM speaks plain OTLP and exports Prometheus text. There is no vendor SDK in the
tree, so any OTLP-compatible backend works by setting environment variables —
nothing is recompiled to switch vendors.

## 1. Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | *(unset)* | OTLP collector/backend URL. **Unset ⇒ no export** (local metrics still work). |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` | `grpc` (port 4317) or `http/protobuf` (port 4318) |
| `OTEL_EXPORTER_OTLP_HEADERS` | *(unset)* | `key=value,key2=value2` — this is where vendor API keys go |
| `OTEL_SDK_DISABLED` | `false` | Kill switch. `true` beats every other setting. |
| `OTEL_SERVICE_NAME` | crate name | Overrides the detected service name |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on` | `always_on`, `always_off`, `traceidratio`, `parentbased_*` |
| `OTEL_TRACES_SAMPLER_ARG` | `1.0` | Ratio for the `traceidratio` samplers |
| `OTEL_RESOURCE_ATTRIBUTES` | *(unset)* | `key=value` pairs merged onto the resource, e.g. `deployment.environment=prod,region=us-east-2` |
| `RUST_LOG` | `info` | Log filter (`tracing_subscriber` EnvFilter syntax) |

Sanity check: start any service and read the startup line.

```
INFO avm_otel: otel initialised service.name=avm-gateway service.version=0.1.0
     otlp_endpoint=Some("http://localhost:4317") sampler="parentbased_always_on"
     sdk_disabled=false export_enabled=true
```

`export_enabled=false` with an endpoint set means `OTEL_SDK_DISABLED` is on.

## 2. Local development

```bash
docker compose -f docker-compose.otel.yml up -d

export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
export OTEL_RESOURCE_ATTRIBUTES=deployment.environment=dev
export RUST_LOG=info,avm_scheduler=debug

cargo run -p avm-gateway
```

* Traces → <http://localhost:16686> (Jaeger), service `avm-gateway`
* Metrics → <http://localhost:9090> (Prometheus), try `avm_gateway_request_duration_seconds_bucket`
* Collector internals → <http://localhost:55679/debug/tracez>

Tear down with `docker compose -f docker-compose.otel.yml down -v`.

## 3. Grafana Cloud

Grafana Cloud OTLP uses HTTP with basic auth; the token is the instance-id and
API key, base64-encoded.

```bash
AUTH=$(printf '%s:%s' "$GRAFANA_INSTANCE_ID" "$GRAFANA_API_KEY" | base64 -w0)

export OTEL_EXPORTER_OTLP_ENDPOINT="https://otlp-gateway-prod-us-east-0.grafana.net/otlp"
export OTEL_EXPORTER_OTLP_PROTOCOL="http/protobuf"
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic ${AUTH}"
export OTEL_RESOURCE_ATTRIBUTES="deployment.environment=prod,region=us-east-2"
```

Note the endpoint has **no** `/v1/traces` suffix — the base URL is correct; the
signal path is appended by the exporter.

## 4. Honeycomb

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="https://api.honeycomb.io"
export OTEL_EXPORTER_OTLP_PROTOCOL="http/protobuf"
export OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=${HONEYCOMB_API_KEY}"
```

Honeycomb derives the dataset from `service.name`, so each AVM service lands in
its own dataset. Useful starting query: `GROUP BY name WHERE trace.trace_id = …`.

## 5. Datadog

Datadog does not accept OTLP directly at the intake — run the Datadog Agent (or
the collector's `datadog` exporter) and point AVM at that.

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://datadog-agent:4317"
export OTEL_RESOURCE_ATTRIBUTES="deployment.environment=prod,service.version=0.1.0"
```

Enable OTLP ingest on the agent side:

```yaml
otlp_config:
  receiver:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
```

## 6. Prometheus without any OTel backend

Every service exposes `GET /metrics` in Prometheus text format, independent of
OTLP export. If you only want metrics, set nothing at all and scrape directly:

```yaml
scrape_configs:
  - job_name: avm
    metrics_path: /metrics
    static_configs:
      - targets: ['avm-gateway:8080']
```

This path has no dependency on a collector, and keeps working with
`OTEL_SDK_DISABLED=true`.

## 7. Per-tenant instrumentation

Opt a tenant in at runtime — no restart, no config file.

```bash
# Enable metrics for one project
curl -XPOST localhost:8080/config/instrumentation \
  -H 'content-type: application/json' \
  -d '{"tenant_id":"t_acme","project_id":"b_pipeline","level":"basic"}'

# Full detail (agent trace context + stderr capture) at 10% sampling
curl -XPOST localhost:8080/config/instrumentation \
  -H 'content-type: application/json' \
  -d '{"tenant_id":"t_acme","level":"detailed","sample_ratio":0.1}'

# Inspect current state
curl localhost:8080/config/instrumentation

# Turn it back off (level "off" removes the row)
curl -XPOST localhost:8080/config/instrumentation \
  -H 'content-type: application/json' \
  -d '{"tenant_id":"t_acme","level":"off"}'
```

Resolution rules:

* A project-scoped row beats a tenant-wide row for that project.
* A tenant-wide row (`project_id` empty) applies to every project of the tenant.
* An unknown tenant resolves to `off` — resolution fails closed.

`detailed` is a debugging mode. It injects trace context into agent processes
and re-emits agent stderr into the log pipeline; leave it on for one tenant
while you are looking, then turn it off.

## 8. Production checklist

- [ ] `OTEL_EXPORTER_OTLP_ENDPOINT` points at a collector you control, not the
      vendor directly — you want a buffer when the vendor has an incident.
- [ ] `OTEL_EXPORTER_OTLP_HEADERS` comes from a secret store, not a Dockerfile.
- [ ] `OTEL_RESOURCE_ATTRIBUTES` sets `deployment.environment` — otherwise dev
      and prod traces land in the same view.
- [ ] Platform sampler left at `parentbased_always_on`. If cost is a problem,
      reduce *tenant* ratios, not platform sampling.
- [ ] Scrape `/metrics` even when OTLP is configured; it is the fallback that
      survives a collector outage.
- [ ] `OTEL_SDK_DISABLED=true` is documented as the incident kill switch.
