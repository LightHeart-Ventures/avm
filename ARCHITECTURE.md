# AVM Architecture — Rust Implementation

**Multi-tenant, multi-scope agent runtime on bare metal/VM. gRPC control plane, process-pool execution, embedded memory store, OTel instrumentation.**

---

## Core Components

### Control Plane (gRPC)

| Service | Responsibility |
|---------|---|
| `TenantService` | Tenant CRUD, quota mgmt, billing |
| `AgentService` | Agent registration, versioning, discovery |
| `JobService` | Job submission, polling, cancellation |
| `WorkflowService` | Workflow definition, scheduling, execution |
| `MemoryService` | Read/write memories (system/tenant/project/agent scoped) |

### Distributed Coordinator (Raft Consensus)

**Multi-host deployments use **Raft consensus** for automatic coordinator election.** Each AVM runtime is a Raft node; the elected leader acts as the **Coordinator** — the single source of truth for:

| Responsibility |
|---|
| **Agent Registry** — where each agent lives (agent_id → host_id + gRPC addr) |
| **Cross-host Routing** — agent-to-agent RPC forwarding via coordinator |
| **Global Memory** — distributed KV store for shared agent state |
| **Workload Placement** — which host runs which new job (round-robin, capacity-aware) |
| **Status Collection** — aggregates telemetry from all runtimes |

**Raft Implementation:**
- Each runtime runs a Raft node (embedded, ~1000 LOC)
- Consensus on every write: agent registration, job placement, memory updates
- **State Machine:**
  ```
  type CoordinatorState {
    agent_registry: HashMap<agent_id, (host_id, gRPC_addr)>,
    memory_store: HashMap<(scope, scope_id, memory_id), Memory>,
    job_queue: PriorityQueue<Job>,  // or each host manages its own, coordinator just tracks capacity
    host_status: HashMap<host_id, (last_heartbeat, capacity_free, running_agents)>,
  }
  ```
- **Heartbeat:** each runtime → coordinator every 5s (or pull-based polling)
- **Leader election:** timeout-triggered new election if leader fails

**Agent-to-Agent Comms (Example):**
1. Agent on `host-1` calls `invoke_agent(agent_id="ag_task_executor", task=...)`
2. `host-1` runtime → Coordinator: "Where is `ag_task_executor`?"
3. Coordinator responds: "On `host-2` at `grpc://host-2:50051`"
4. `host-1` runtime connects directly to `host-2` runtime's gRPC server
5. `host-2` routes to its local agent process pool

**Fallback:** if coordinator is unreachable, runtimes can cache the agent registry locally and use stale data (eventual consistency); writes block until quorum is reachable.

### Data Plane

| Component | Responsibility |
|---------|---|
| **Job Scheduler** | Queue + priority; route to process pool; enforce quotas |
| **Process Pool** | Spawn agent processes; resource isolation (cgroups/ulimit) |
| **MCP Gateway** | Route tool calls (local + remote); agent-to-agent invocation |
| **Memory Store** | Embedded key-value (RocksDB or SQLite); scope-based access control |
| **Secrets Manager** | Per-scope encrypted credential injection |
| **AI Provider Router** | Multiplex to Claude/GPT-4/Ollama; cost tracking |
| **OTel Collector** | In-process span/metric buffering → Prometheus/Jaeger |

### Scope Hierarchy

```
System (AVM infra + platform agents)
  └─► Tenant (t_acme, t_competitor, ...)
        └─► Project (b_payments, b_infra, ...)
              └─► Agent (ag_pr_reviewer, ag_task_executor, ...)
```

**Memory scope isolation:**
- System agents see only system memories.
- Tenant agents see system + tenant memories.
- Project agents see system + tenant + project memories.
- Agent-scoped memories are agent-specific within their scope.

---

## Memory Store (Embedded)

### Schema

```
Memories (RocksDB or SQLite)
  PK: (scope, scope_id, memory_id)
  - scope: "system" | "tenant" | "project" | "agent"
  - scope_id: "" | "t_acme" | "b_payments" | "ag_xyz"
  - memory_id: "memory_abc123" or "agent_memory:key"
  - content: string (max 32 KB)
  - tags: []string (for filtering)
  - created_at: timestamp
  - updated_at: timestamp
  - ttl_seconds: optional (auto-expire)
  - owner_id: agent or user who created it
```

### Access Pattern

```
Agent(scope=project, scope_id=b_payments, agent_id=ag_pr_reviewer)
  → MemoryService.Read(scope="project", scope_id="b_payments", memory_id="...")
    ✓ Can read memories in: system, tenant (parent), project (self)
    ✗ Cannot read: agent (other), project (sibling), tenant (other)
```

### Example: Project-Scoped Memory (PR Review Checklist)

```proto
message Memory {
  string memory_id = 1;
  string scope = 2;              // "project"
  string scope_id = 3;           // "b_payments"
  string content = 4;            // "Security: check for SQL injection, XXS, auth bypass..."
  repeated string tags = 5;      // ["pr_review", "checklist", "v2"]
  string created_at = 6;
  string owner_id = 7;           // "ag_pr_reviewer" or "u_alice"
}
```

Agent stores on first run:
```
POST /memories
{
  "scope": "project",
  "scope_id": "b_payments",
  "content": "Security: check for SQL injection, XXS, auth bypass. Performance: O(n)? DB indexes? Memory leaks?",
  "tags": ["pr_review", "checklist", "v2"],
  "ttl_seconds": 2592000  // 30 days, auto-refresh on update
}
```

Agent retrieves on subsequent runs:
```
GET /memories?scope=project&scope_id=b_payments&tags=pr_review,checklist
→ [Memory { memory_id: "mem_abc", content: "Security: ...", tags: [...] }]
```

### Scope Visibility

```
System Memory (scope="system", scope_id="")
  ✓ Readable by: system agents
  ✓ Readable by: tenant agents (inherited)
  ✓ Readable by: project agents (inherited)
  ✓ Readable by: agents in all projects

Tenant Memory (scope="tenant", scope_id="t_acme")
  ✓ Readable by: tenant agents in t_acme
  ✓ Readable by: project agents in t_acme's projects
  ✗ Readable by: agents in t_competitor

Project Memory (scope="project", scope_id="b_payments")
  ✓ Readable by: project agents in b_payments
  ✓ Readable by: parent tenant agents
  ✗ Readable by: agents in b_infra (sibling project)

Agent Memory (scope="agent", scope_id="ag_pr_reviewer")
  ✓ Readable by: ag_pr_reviewer only
  ✗ Readable by: ag_task_executor (sibling agent)
  ✓ Readable by: parent tenant/project agents (optional, default deny)
```

---

## Process Pool Execution

### Agent Startup

```bash
$ avm_agent_run \
    --job-id job_abc123 \
    --agent-id ag_pr_reviewer \
    --scope project \
    --scope-id b_payments \
    --tenant-id t_acme \
    --prompt "You are a code reviewer..." \
    --tools "atum_get_project_task,atum_update_project_task,avm_invoke_agent" \
    --mcp-server-addr localhost:9090 \
    --model claude-3-5-sonnet \
    < task.json
```

### Environment Injection

```bash
# Credentials (decrypted from Secrets Manager)
export GITHUB_TOKEN="ghp_..."
export ATUM_API_KEY="atum_key_..."
export AWS_ACCESS_KEY_ID="AKIA..."

# Scope metadata
export AVM_SCOPE="project"
export AVM_SCOPE_ID="b_payments"
export AVM_TENANT_ID="t_acme"
export AVM_AGENT_ID="ag_pr_reviewer"
export AVM_JOB_ID="job_abc123"

# Memory service endpoint
export AVM_MEMORY_ENDPOINT="http://localhost:9091/memories"

# Resource limits (enforced by cgroups/ulimit)
ulimit -v 2000000  # 2GB virtual
ulimit -t 600      # 10min wall time
```

### Agent Process Lifecycle

```
1. Spawn: fork + exec(agent binary)
2. Setup: Agent reads stdin (task.json), connects to MCP gateway
3. Loop: 
   - Agent calls tool (e.g., "atum_get_project_task")
   - MCP Gateway intercepts, routes to Atum/GitHub/avm_invoke_agent
   - Tool result returned via stdout
   - Agent processes, calls next tool
4. Exit: Agent writes result.json to stdout, exits(0)
5. Cleanup: 
   - Capture exit code, stdout, stderr
   - Persist job result
   - Emit metrics (tokens, latency, cost)
   - Cleanup: kill cgroup, release resources
```

### Resource Isolation (Linux cgroups v2)

```
/sys/fs/cgroup/jobs/job_abc123/
  ├─ memory.max = 2000000000     # 2GB
  ├─ cpu.max = 60000:100000      # 60% CPU
  ├─ pids.max = 10               # max 10 processes
  └─ io.max = "...max_io_ops..." # disk I/O throttle
```

---

## MCP Gateway (Embedded)

### Local MCP Tools

Agents call standard tools; gateway routes them:

```
Tool Call
  → "avm_invoke_agent" 
    → JobService.SubmitJob (scoped invocation)
    → Wait for result or return job_id (async)
  
  → "avm_list_agents"
    → AgentService.ListAgents (filter by scope)
  
  → "avm_get_memory"
    → MemoryService.Read (scope-aware ACL)
  
  → "avm_upsert_memory"
    → MemoryService.Write (scope-aware ACL)
```

### Remote MCP Forwarding

```
Tool Call
  → "atum_get_project_task"
    → MCP Gateway.Forward(
        scope_id="b_payments",  // enforce project_id in URL
        tool="atum_get_project_task",
        params=...
      )
    → HTTP POST to Atum API (with injected ATUM_PROJECT_ID header)
  
  → "gh_create_pull_request"
    → MCP Gateway.Forward(
        scope_id="b_payments",
        tool="gh_create_pull_request",
        params=...
      )
    → gRPC call to GitHub service (with OAuth token from Secrets)
```

### AI Provider Routing

```
Agent.call(model="claude-3-5-sonnet", messages=[...], tools=[...])
  → AIRouter.SelectProvider(
      tenant_id="t_acme",
      model="claude-3-5-sonnet",
      job_id="job_abc123",
      estimated_tokens=5000
    )
  → Check quota: t_acme has 9,995,000 tokens left ✓
  → Check cost: Sonnet = $3/1M input, GPT-4 = $15/1M
    → Route to Claude (cheaper)
  → Call Anthropic API (retry + fallback to GPT-4 if rate-limited)
  → Return completion + token count
  → Deduct from tenant/project quota
  → Emit cost metric
```

---

## Execution Layer: Container-Based Isolation

**Each agent job runs inside a dedicated OCI container** (Docker/Podman). This provides full isolation: CPU limits, memory bounds, filesystem sandbox, network isolation, and process limits — preventing one runaway agent from starving others or compromising the host.

### Container Lifecycle

1. **Create** — before job: `podman run --name job_abc123 --cpus=0.6 --memory=512m --pids-limit=10 --network=none --user=1000 job_image:latest`
2. **Wait** — for container exit or 900s timeout; stream stdout → results, capture exit code
3. **Destroy** — `podman rm -f job_abc123`; emit cleanup metric

### Agent Image Requirements

- Minimal Alpine-based image (~50 MB)
- Agent binary reads job from stdin, writes results to stdout
- Non-root user (uid=1000), no sudo
- Exit cleanly on SIGTERM

### Future: Kubernetes

Containers → **K8s Job** is 1 pod with 1 container. Executor's `max_concurrency` becomes DaemonSet + HPA. Same agent image works on-prem or cloud.

---

## Observability (OTel)

### Instrumentation Points

```
┌─────────────────────────────┐
│ span: "job_abc123"          │
│ attributes:                 │
│   job_id: "job_abc123"      │
│   agent_id: "ag_pr_reviewer"│
│   scope: "project"          │
│   scope_id: "b_payments"    │
│   tenant_id: "t_acme"       │
│   status: "running"         │
│ events:                     │
│   - "job.started"           │
│   - "job.completed"         │
└─────────────────────────────┘
    │
    ├─ span: "tool_call:atum_get_project_task"
    │   attributes:
    │     tool_name: "atum_get_project_task"
    │     latency_ms: 45
    │     params: { project_id: "b_payments" }
    │     result_tokens: 200
    │
    ├─ span: "tool_call:avm_invoke_agent"
    │   attributes:
    │     target_agent_id: "ag_task_executor"
    │     target_scope: "project"
    │     latency_ms: 2300
    │     result: { status: "completed", ... }
    │
    └─ span: "ai_provider_call"
        attributes:
          provider: "anthropic"
          model: "claude-3-5-sonnet"
          input_tokens: 5240
          output_tokens: 2150
          cost_usd: 0.0237
          latency_ms: 1500
```

### Metrics (Prometheus)

```
avm_job_duration_seconds{tenant_id, agent_id, scope, status}
avm_job_tokens_total{tenant_id, agent_id, scope, model}
avm_job_cost_usd{tenant_id, agent_id, provider}
avm_tool_call_duration_seconds{tool_name, provider, status}
avm_quota_usage_tokens{tenant_id, project_id, period}
avm_agent_invocation_count{source_agent, target_agent, scope}
avm_memory_size_bytes{scope, scope_id}
avm_scheduler_queue_depth{pool_id}
```

### Logging (Structured JSON)

```json
{
  "timestamp": "2025-09-14T12:34:56Z",
  "level": "INFO",
  "logger": "job_executor",
  "message": "job completed",
  "job_id": "job_abc123",
  "agent_id": "ag_pr_reviewer",
  "tenant_id": "t_acme",
  "scope": "project",
  "scope_id": "b_payments",
  "status": "completed",
  "duration_ms": 4200,
  "input_tokens": 5240,
  "output_tokens": 2150,
  "cost_usd": 0.0237
}
```

---

## Deployment Model

### Single-Machine Deployment

```
bare metal / VM
  ├─ AVM Server (port 9090, gRPC)
  ├─ Scheduler + Job Queue
  ├─ Process Pool (up to 32 agents concurrently)
  ├─ MCP Gateway
  ├─ Memory Store (RocksDB / SQLite)
  ├─ Secrets Manager
  ├─ OTel Collector (buffered)
  └─ Prometheus Exporter (port 9091)
```

### Scale-Out (Future)

- Multiple AVM nodes share:
  - Central state store (PostgreSQL / DynamoDB)
  - Distributed job queue (Redis / NATS)
  - Shared secrets manager (Vault / AWS Secrets Manager)
  - Shared memory store (PostgreSQL)
- Scheduler distributes jobs across nodes
- OTel → Central Prometheus/Jaeger

---

## Rust Crate Structure

```
avm/
├─ Cargo.toml (workspace)
├─ crates/
│  ├─ avm-proto/          # proto definitions + generated code
│  ├─ avm-server/         # gRPC server + control plane
│  │  ├─ tenant_service.rs
│  │  ├─ agent_service.rs
│  │  ├─ job_service.rs
│  │  ├─ workflow_service.rs
│  │  ├─ memory_service.rs
│  │  └─ main.rs
│  ├─ avm-scheduler/      # job scheduler + queue
│  ├─ avm-executor/       # process pool + agent execution
│  │  ├─ agent_runner.rs  # fork + exec
│  │  ├─ cgroup.rs        # resource isolation
│  │  └─ main.rs
│  ├─ avm-gateway/        # MCP gateway
│  │  ├─ mcp_router.rs
│  │  ├─ ai_provider_router.rs
│  │  └─ secrets_injector.rs
│  ├─ avm-memory/         # embedded store
│  │  ├─ store.rs         # RocksDB/SQLite wrapper
│  │  ├─ scope_acl.rs     # access control
│  │  └─ expiration.rs    # TTL enforcement
│  ├─ avm-observability/  # OTel instrumentation
│  │  ├─ tracing.rs
│  │  ├─ metrics.rs
│  │  └─ logging.rs
│  └─ avm-cli/            # CLI tool
│     ├─ tenant_cmd.rs
│     ├─ agent_cmd.rs
│     ├─ job_cmd.rs
│     └─ main.rs
└─ tests/
   ├─ integration/
   │  ├─ tenant_workflow.rs
   │  ├─ agent_invocation.rs
   │  ├─ memory_scope_isolation.rs
   │  └─ quota_enforcement.rs
   └─ benches/
      └─ scheduler_throughput.rs
```

### Key Rust Dependencies

```toml
# Async runtime
tokio = "1.0"
tonic = "0.12"  # gRPC
prost = "0.13"  # Protobuf

# Storage
rocksdb = "0.22"  # or sqlite via rusqlite
tokio-util = { version = "0.7", features = ["codec"] }

# Secrets + crypto
sodiumoxide = "0.2"  # NaCl for encryption
thiserror = "1.0"

# Observability
opentelemetry = "0.24"
opentelemetry-jaeger-trace = "0.23"
opentelemetry-prometheus = "0.16"
tracing = "0.1"
tracing-subscriber = "0.3"

# HTTP + parsing
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# Process management
nix = "0.29"  # cgroups + ulimit
subprocess = "0.12"

# Config
config = "0.14"
toml = "0.8"

# Testing
mockito = "1.0"
proptest = "1.5"
```

---

## Startup Sequence

```
1. Load config (avm.toml)
   - Bind address, scheduler pool size, memory store path
   - OTel endpoint, Prometheus scrape port
   - Default agent image registry

2. Initialize secrets manager
   - Load encryption key from env (AVM_SECRETS_KEY)
   - Connect to encrypted store

3. Initialize memory store
   - Open RocksDB at DATA_DIR/memories
   - Run schema migrations
   - Load scope ACLs

4. Initialize scheduler
   - Create job queue (in-memory or Redis)
   - Spin up N worker threads (default 8)
   - Pre-allocate cgroups for process pool

5. Initialize MCP gateway
   - Load Atum API endpoint + credentials
   - Load GitHub OAuth app credentials
   - Initialize AI provider clients (Claude SDK, OpenAI SDK)

6. Initialize OTel
   - Configure Jaeger exporter or stdout
   - Configure Prometheus exporter (port 9091)
   - Set up structured logging (tracing-subscriber)

7. Start gRPC server
   - Bind to :9090
   - Register services: TenantService, AgentService, JobService, etc.
   - Listen for connections

8. Log "AVM ready" with git commit, version, config summary
```

---

## First Milestones (Rust-Native)

| Phase | Goal | Crates Involved |
|-------|------|---|
| **M1** | Proto + gRPC server skeleton | avm-proto, avm-server |
| **M2** | Tenant + Agent CRUD | avm-server, avm-memory |
| **M3** | Job submission + scheduler | avm-server, avm-scheduler |
| **M4** | Process pool + agent execution | avm-executor, nix crate |
| **M5** | MCP gateway (local + remote) | avm-gateway, reqwest |
| **M6** | AI provider routing | avm-gateway, anthropic SDK |
| **M7** | Memory store (scoped ACL) | avm-memory, rocksdb |
| **M8** | OTel instrumentation | avm-observability, opentelemetry |
| **M9** | CLI tool | avm-cli |
| **M10** | Integration tests + docs | tests/, DEVELOPMENT.md |

---

## Open Questions

1. **Memory store backend**: RocksDB (embedded, fast, no ops) or SQLite (easier backups, ACID)? Start with SQLite, migrate to RocksDB if perf matters.
2. **Process pool or container pool**: Start with process pool (simpler cgroups isolation). Container pool (Podman/runc) is future work.
3. **Secrets encryption**: Use AES-256-GCM via sodiumoxide, or delegate to system keyring? Start with in-process, plan for Vault later.
4. **Distributed mode**: Single-machine first, plan for PostgreSQL state share + Redis job queue in M11+.
