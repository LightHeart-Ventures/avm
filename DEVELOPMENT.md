# AVM — Development Guide

Scaffold for the Agent Virtual Machine: a Cargo workspace with a gRPC control
plane, a NATS JetStream job bus, PostgreSQL state and a process-pool executor.

---

## Prerequisites

| Tool | Version | Notes |
|---|---|---|
| Rust | **1.70+** | `rustup toolchain install stable` |
| Docker + Compose | v2 | runs NATS + Postgres locally |
| `protoc` | 3.15+ | **optional** — only needed for gRPC codegen (see below) |
| `sqlx-cli` | 0.8 | optional; `cargo run -p avm-cli -- migrate` works without it |

```bash
cargo --version          # 1.70+
docker compose version
protoc --version         # optional
```

### About `protoc`

`avm-proto/build.rs` compiles `proto/avm_service.proto` with `tonic-build`,
which shells out to `protoc`. **If `protoc` is missing the build still
succeeds** — the build script emits an empty stub module and prints a
`cargo:warning`. Set `AVM_PROTO_STRICT=1` to make missing codegen a hard error
(do this in CI).

```bash
sudo apt-get install -y protobuf-compiler   # Debian/Ubuntu
AVM_PROTO_STRICT=1 cargo build -p avm-proto
```

The queue envelopes (`JobMessage`, `ResultMessage`) are **hand-written serde
types** in `avm-proto/src/types.rs`, so the data path never depends on `protoc`.

---

## 1. Start infrastructure

```bash
docker compose up -d
docker compose ps
```

| Service | Port | Purpose |
|---|---|---|
| `nats` | 4222 (client), 8222 (monitor) | JetStream job/result bus (`-js`) |
| `postgres` | 5432 | durable state, volume `postgres_data` |

Defaults: database `avm`, user `avm`, password `avm`.

```bash
export DATABASE_URL="postgres://avm:avm@localhost:5432/avm"
export NATS_URL="nats://localhost:4222"
```

---

## 2. Apply migrations

Either the built-in runner (no extra tooling):

```bash
cargo run -p avm-cli -- migrate
```

or `sqlx-cli`:

```bash
cargo install sqlx-cli --no-default-features --features rustls,postgres
cargo sqlx migrate run          # reads ./migrations
```

| Migration | Creates |
|---|---|
| `001_create_memories.sql` | `memories` (scope, memory_id, content, tags, TTL) |
| `002_create_jobs.sql` | `jobs` (job_id, status, retry_count, last_error, result) |
| `003_create_audit_logs.sql` | `audit_logs` (append-only audit trail) |
| `004_create_quotas.sql` | `quotas`, `quota_usage` |
| `005_create_indexes.sql` | PKs/uniques + `scope+memory_id` and scheduler indexes |

> Migrations are embedded at compile time via `sqlx::migrate!("../migrations")`,
> so editing SQL requires a rebuild of `avm-storage`.

---

## 3. Build and test

```bash
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Tests in the scaffold are pure unit tests (scope ACL, subject naming, path
traversal guard) and need **neither** Postgres nor NATS.

---

## 4. Run the services

Four binaries, each independently runnable. Use separate terminals.

```bash
# control plane (gRPC :50051) — applies migrations on boot
cargo run -p avm-server

# executor: JetStream pull consumer + process pool
cargo run -p avm-executor -- --pool avm-executor --concurrency 8

# scheduler: requeue/reap/purge reconciler
cargo run -p avm-scheduler -- --tick-secs 5

# MCP gateway (HTTP :8080)
cargo run -p avm-gateway
```

Submit work with the CLI:

```bash
cargo run -p avm-cli -- submit \
  --tenant t_acme --project b_payments \
  --agent ag_pr_reviewer \
  --payload '{"task":"review PR 42"}'

cargo run -p avm-cli -- jobs --tenant t_acme
cargo run -p avm-cli -- job job_<id>
```

### Agent process contract

`avm-executor` forks `$AVM_AGENT_DIR/<agent_id>` (default `/opt/avm/agents`):

* **stdin** — the JSON job payload
* **env** — `AVM_JOB_ID`, `AVM_AGENT_ID`, `AVM_SCOPE`, `AVM_TENANT_ID`, `AVM_PROJECT_ID`
* **stdout** — the result (captured verbatim)
* **exit 0** — success; anything else fails the job and burns a retry

```bash
export AVM_AGENT_DIR=/tmp/avm-agents
mkdir -p "$AVM_AGENT_DIR"
printf '#!/bin/sh\ncat >/dev/null\necho \x27{"ok":true}\x27\n' > "$AVM_AGENT_DIR/ag_pr_reviewer"
chmod +x "$AVM_AGENT_DIR/ag_pr_reviewer"
```

---

## 5. Message flow

```
avm-cli / gRPC ──► avm-server ──► Postgres (jobs: queued)
                        │
                        └──► NATS  avm.jobs.<tenant>.<project>
                                        │
                                 avm-executor (pull consumer, durable)
                                        │ fork+exec agent
                                        ├──► Postgres (succeeded|failed, retry_count)
                                        └──► NATS  avm.results.<tenant>.<project>

avm-scheduler ── every 5s ──► requeue orphaned 'queued', purge expired memories
```

Postgres is written **before** publishing, so a NATS outage leaves a recoverable
`queued` row that the scheduler re-publishes. Delivery is at-least-once;
executors must stay idempotent on `job_id`.

---

## 6. Observability

`avm-observability::init("<service>")` installs a `tracing-subscriber` stack.

```bash
RUST_LOG=debug,avm=trace cargo run -p avm-server
AVM_LOG_FORMAT=json      cargo run -p avm-server   # ndjson logs
```

OTLP export is stubbed behind a non-default feature so the default build does
not pin a fast-moving OTel API:

```bash
cargo build -p avm-observability --features otlp   # wiring still TODO
```

---

## 7. Layout

```
avm/
├── Cargo.toml                 # workspace + shared dependency versions
├── docker-compose.yml         # nats (-js) + postgres:16
├── proto/avm_service.proto    # Tenant/Agent/Job/Memory services + envelopes
├── migrations/                # 001..005
├── avm-proto/                 # generated gRPC + serde queue envelopes
├── avm-server/                # gRPC control plane
├── avm-scheduler/             # reconciler + quota gate
├── avm-executor/              # JetStream consumer + process pool
├── avm-gateway/               # MCP HTTP tool routing
├── avm-storage/               # sqlx pool, memories, jobs
├── avm-queue/                 # JetStream publisher/subscriber
├── avm-observability/         # tracing/OTel bootstrap
└── avm-cli/                   # `avm` operator binary
```

---

## 8. What is stubbed

| Area | State |
|---|---|
| tonic service impls | handlers exist in `avm-server/src/services.rs`; `*Server` wiring is TODO (needs `protoc`) |
| TenantService / AgentService | module stubs only |
| Quota enforcement | `avm_scheduler::quota::Decision` defined; no evaluation yet |
| cgroup/rlimit isolation | `avm_executor::agent_runner::isolation::Limits` defined; not applied |
| MCP local handlers + upstream proxy | routing table works; dispatch returns "not implemented" |
| OTLP exporter | behind `--features otlp`, unimplemented |
| Audit log writes | table + indexes exist; no writer |

---

## Teardown

```bash
docker compose down            # keep volumes
docker compose down -v         # drop postgres_data + nats_data
```
