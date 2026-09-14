# AVM — Agent Virtual Machine

**A multi-tenant Rust runtime for AI agents.** gRPC control plane, NATS JetStream job bus,
PostgreSQL state, process-pool execution, an MCP tool gateway, A2A / Agent Card interop and
OpenTelemetry instrumentation — as a single Cargo workspace.

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)

---

## Why

Running agents in production needs the same primitives a VM gives processes: isolation,
scheduling, quotas, durable state and observability. AVM provides those as infrastructure so
agent code stays plain code.

- **Multi-tenant by construction** — `system → tenant → project → agent` scope hierarchy is
  enforced in storage, on the queue subjects and at the gateway.
- **Durable job bus** — JetStream pull consumers with a bounded process pool; jobs survive
  executor restarts.
- **Typed tool contracts** — JSON-Schema (draft 2020-12) signatures generated from Rust arg
  structs and validated *before* dispatch.
- **Interoperable** — MCP for tools, A2A + Agent Card for agent-to-agent discovery.
- **Observable** — `tracing` everywhere, OTLP export behind a feature flag, Prometheus metrics.

---

## Architecture at a glance

```
                 ┌───────────────┐
   avm-cli ─────►│  avm-server   │  gRPC control plane
                 │ Tenant/Agent/ │  (Tenant, Agent, Job, Memory, A2A)
                 │ Job/Memory/A2A│
                 └───────┬───────┘
                         │  PostgreSQL (avm-storage)
                         ▼
                 ┌───────────────┐      NATS JetStream (avm-queue)
                 │ avm-scheduler │◄────► avm.jobs.<tenant>.<project>
                 │  reconciler   │       avm.results.<tenant>.<project>
                 └───────┬───────┘
                         ▼
                 ┌───────────────┐      ┌──────────────┐
                 │ avm-executor  │◄────►│ avm-gateway  │ HTTP: MCP tools,
                 │ process pool  │      │  MCP + A2A   │ /tools/schema,
                 └───────────────┘      └──────────────┘ /.well-known/agent-card.json
```

### Scope model

```
System   (AVM infra + platform agents)
 └─ Tenant   (t_acme, …)
     └─ Project  (b_payments, …)
         └─ Agent (ag_pr_reviewer, …)
```

Readers see **their own scope plus ancestors** — a project agent reads system + tenant +
project memories, never a sibling's.

---

## Workspace crates

| Crate | Role |
|---|---|
| `avm-server` | gRPC control plane: `TenantService`, `AgentService`, `JobService`, `MemoryService`, `A2AService` |
| `avm-scheduler` | Reconciler: re-publish queued jobs, quota gate, reap stale runs, purge expired memories |
| `avm-executor` | JetStream pull consumer + bounded process pool (fork/exec agents) |
| `avm-gateway` | HTTP MCP tool routing (local + upstream proxy), tool-schema introspection, A2A endpoints |
| `avm-mcp-tools` | JSON-Schema tool signatures: generation (`schemars`), `ManagedToolSet` registry, runtime validation |
| `avm-agent` | Agent Card + A2A protocol types (pure serde, no I/O) |
| `avm-models` | Model distribution: OCI artifacts + content-addressed storage |
| `avm-storage` | PostgreSQL access via `sqlx` + embedded migrations |
| `avm-queue` | NATS JetStream publisher/subscriber |
| `avm-proto` | gRPC codegen (`tonic`/`prost`) + hand-written queue envelopes |
| `avm-observability` | `tracing` setup, structured logging |
| `avm-otel` | Resource detection, W3C propagation, Prometheus metrics, scoped sampling |
| `avm-cli` | Operator CLI `avm` (`migrate`, `submit`, `job`, `jobs`, `memories`) |

---

## Quick start

**Prerequisites:** Rust 1.70+, Docker Compose v2. `protoc` is *optional* — without it
`avm-proto/build.rs` emits a stub module and the build still succeeds (set
`AVM_PROTO_STRICT=1` in CI to make missing codegen a hard error).

```bash
# 1. infrastructure (NATS + Postgres)
docker compose up -d

export DATABASE_URL="postgres://avm:avm@localhost:5432/avm"
export NATS_URL="nats://localhost:4222"

# 2. schema
cargo run -p avm-cli -- migrate

# 3. build + test
cargo build --workspace
cargo test  --workspace
```

| Service | Port | Purpose |
|---|---|---|
| `nats` | 4222 / 8222 | JetStream job + result bus |
| `postgres` | 5432 | durable state |

Submit a job:

```bash
cargo run -p avm-cli -- submit --tenant t_acme --project b_payments --agent ag_demo
cargo run -p avm-cli -- jobs --tenant t_acme
```

Full walkthrough: **[DEVELOPMENT.md](DEVELOPMENT.md)**.

---

## HTTP surface (avm-gateway)

| Endpoint | Purpose |
|---|---|
| `GET /.well-known/agent-card.json` | A2A Agent Card discovery |
| `POST /a2a/task` | A2A task submission |
| `GET /tools/schema` | JSON-Schema signatures for registered tools |
| `POST /tools/validate` | Validate a tool-call payload before dispatch |
| `GET /metrics` | Prometheus metrics (when OTel is enabled) |

---

## Observability

`tracing` + `tracing-subscriber` by default; OTLP export lives behind the `otlp` feature.
A local collector stack ships in `docker-compose.otel.yml`:

```bash
docker compose -f docker-compose.otel.yml up -d
cargo run -p avm-server --features otlp
```

---

## Documentation

| Doc | Contents |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | Components, scope hierarchy, memory store, data plane |
| [DESIGN.md](DESIGN.md) | Design rationale and trade-offs |
| [DEVELOPMENT.md](DEVELOPMENT.md) | Local setup, migrations, running each service |
| [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) | Phased build plan and status |
| [docs/A2A_AGENT_CARD.md](docs/A2A_AGENT_CARD.md) | A2A protocol + Agent Card schema (`avm.a2a/v1`) |

---

## Tech stack

Rust 2021 (MSRV 1.70) · `tokio` · `tonic`/`prost` · `axum` · `sqlx` + PostgreSQL 16 ·
`async-nats` (JetStream) · `schemars`/`jsonschema` · `tracing` + OpenTelemetry · `oci-client`

---

## License

Apache-2.0 — see [Cargo.toml](Cargo.toml). © LightHeart Ventures.
