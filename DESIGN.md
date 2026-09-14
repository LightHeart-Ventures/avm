# AVM — Agent Virtual Machine

**Multi-tenant runtime for scalable, isolated AI agent execution with integrated MCP gateways, AI provider routing, secrets management, and observability.**

## Architecture

### Control Plane

| Component | Responsibility |
|-----------|---|
| **Tenant API** (gRPC + HTTP/1.1) | Tenant CRUD, agent registration, workflow submission, task polling |
| **Scheduler** | Routes jobs to agent pools; respects quotas, priorities, and placement policies |
| **Secrets Manager** | Per-tenant encrypted credential store; injection at agent startup |
| **State Machine** | Event-sourced workflow state; checkpoints for resumption after crash |

### Data Plane

| Component | Responsibility |
|-----------|---|
| **Agent Pool** | Process or container-per-agent; resource isolation (RAM, CPU, wall time) |
| **MCP Gateway** | Translate agent MCP calls to local tool execution or remote server forwarding |
| **AI Provider Router** | Multiplex agent requests (Claude, GPT-4, local Ollama, etc.) with failover |
| **Message Bus** | Async job queue + agent-to-agent publish/subscribe (NATS or gRPC streams) |

### Observability

- **Tracing**: OTel spans for every step (scheduler → agent exec → provider call)
- **Logging**: Structured JSON logs (agent stdout/stderr + runtime events)
- **Metrics**: Prometheus counters (agent runs, latency p50/p95/p99, errors, quota usage)

---

## API Surface (Control Plane)

### Tenant Management

```proto
service TenantService {
  rpc CreateTenant(CreateTenantReq) returns (Tenant);
  rpc GetTenant(GetTenantReq) returns (Tenant);
  rpc UpdateTenantQuotas(UpdateQuotasReq) returns (Tenant);
  rpc ListTenants(ListTenantsReq) returns (ListTenantsResp); // admin-only
}

message Tenant {
  string tenant_id = 1;         // e.g., "t_abc123"
  string name = 2;
  map<string, string> labels = 3; // team, environment, billing_code
  QuotaConfig quotas = 4;
  string created_at = 5;        // ISO-8601 UTC
}

message QuotaConfig {
  uint32 max_concurrent_agents = 1;
  uint32 max_daily_token_count = 2;
  double max_monthly_spend_usd = 3;
  repeated string allowed_models = 4; // ["claude-sonnet", "claude-opus", "local"]
}
```

### Agent Lifecycle

```proto
service AgentService {
  rpc RegisterAgent(RegisterAgentReq) returns (Agent);
  rpc GetAgent(GetAgentReq) returns (Agent);
  rpc ListAgents(ListAgentsReq) returns (ListAgentsResp);
  rpc UpdateAgent(UpdateAgentReq) returns (Agent);
  rpc DeregisterAgent(DeregisterAgentReq) returns (Empty);
}

message Agent {
  string agent_id = 1;          // e.g., "ag_xyz789"
  string tenant_id = 2;
  string name = 3;
  string image = 4;             // container image or local entrypoint
  string prompt = 5;            // system prompt
  repeated string tools = 6;    // ["atum_get_project_task", "atum_memory_upsert"]
  string model = 7;             // "claude-sonnet" | "gpt-4o" | "local-ollama:7b"
  AgentPlacement placement = 8;
  string status = 9;            // "registered", "running", "failed", "quarantined"
  string registered_at = 10;
}

message AgentPlacement {
  string pool_id = 1;           // e.g., "pool-high-memory", "pool-gpu"
  int32 cpu_millicores = 2;     // 500 = 0.5 CPU
  int64 memory_bytes = 3;       // 2GB = 2_000_000_000
  int32 wall_time_sec = 4;      // max execution time per job
}
```

### Job Submission & Polling

```proto
service JobService {
  rpc SubmitJob(SubmitJobReq) returns (Job);
  rpc GetJob(GetJobReq) returns (Job);
  rpc ListJobs(ListJobsReq) returns (ListJobsResp);
  rpc PollJobResult(PollJobResultReq) returns (stream JobUpdate); // server push
  rpc CancelJob(CancelJobReq) returns (Empty);
}

message SubmitJobReq {
  string tenant_id = 1;
  string agent_id = 2;
  string task = 3;               // "Analyze the PR at https://github.com/..."
  map<string, string> context = 4; // { "project_id": "b_123", "card_id": "card_456" }
  int32 priority = 5;            // 0=low, 100=critical
  string callback_url = 6;       // optional webhook for completion
}

message Job {
  string job_id = 1;            // e.g., "job_abc123"
  string tenant_id = 2;
  string agent_id = 3;
  string task = 4;
  string status = 5;            // "pending", "running", "completed", "failed", "cancelled"
  JobResult result = 6;         // populated on completion
  string submitted_at = 7;
  string started_at = 8;
  string completed_at = 9;
}

message JobResult {
  string synthesis = 1;         // agent's final output
  repeated ToolCall tool_calls = 2;
  int32 input_tokens = 3;
  int32 output_tokens = 4;
  string error = 5;             // if status=="failed"
}

message ToolCall {
  string tool_name = 1;
  map<string, string> params = 2;
  string result = 3;            // JSON-stringified tool output
  int32 latency_ms = 4;
}
```

### Workflow Orchestration

```proto
service WorkflowService {
  rpc DefineWorkflow(DefineWorkflowReq) returns (Workflow);
  rpc GetWorkflow(GetWorkflowReq) returns (Workflow);
  rpc TriggerWorkflow(TriggerWorkflowReq) returns (WorkflowRun);
  rpc GetWorkflowRun(GetWorkflowRunReq) returns (WorkflowRun);
}

message Workflow {
  string workflow_id = 1;
  string tenant_id = 2;
  string name = 3;
  string trigger = 4;           // "manual" | "scheduled:0 9 * * ?" | "event:card.moved"
  repeated WorkflowStep steps = 5;
}

message WorkflowStep {
  string step_id = 1;
  string type = 2;              // "agent", "branch", "aggregate"
  string agent_id = 3;          // if type=="agent"
  string condition = 4;         // if type=="branch"
  repeated string inputs = 5;   // step ids this depends on
}

message WorkflowRun {
  string run_id = 1;
  string workflow_id = 2;
  string status = 3;            // "pending", "running", "completed", "failed"
  map<string, JobResult> step_results = 4;
  string started_at = 5;
  string completed_at = 6;
}
```

---

## Data Plane: Message Flow

### Happy Path: Job Submission → Execution → Result

```
Client (tenant-api) 
  → SubmitJob RPC
      → Scheduler receives job
      → Allocate slot in agent pool (check quotas, priority, placement)
      → Inject secrets (SSH keys, API tokens, etc.)
      → Start agent process with (prompt, task, context)
          → Agent loads system prompt
          → Agent loop: read stdin, call MCP tools, emit stdout
              → MCP Gateway intercepts tool calls
              → Route to local tool or remote server (Atum APIs, GitHub, AWS)
              → Return result to agent stdin
          → Agent finishes, writes result.json to stdout, exits
      → Capture result.json (synthesis, tool_calls, tokens, latency)
      → Audit log: tokens consumed, model used, cost
      → Return JobResult to client
```

### Error Handling: Crash Recovery

```
Agent crashes (SIGSEGV, timeout, OOM)
  → Runtime captures stack trace, stderr, partial stdout
  → Persist checkpoint (last completed tool call, step index)
  → Emit error event: agent_crash_detected
  → If job is retryable (no side-effect-only tool calls):
      → Requeue job with checkpoint (resume from last good step)
      → Increment retry counter (max 3)
  → If max retries exceeded OR job is non-retryable:
      → Mark job as failed
      → Escalate (page oncall, file incident)
```

---

## Implementation Phases

| Phase | Goal | Owner | ETA |
|-------|------|-------|-----|
| **1A** | Proto + server skeleton (gRPC Tenant/Agent/Job services) | — | Week 1 |
| **1B** | Agent Pool (process-per-agent, resource limits) | — | Week 2 |
| **1C** | Scheduler (quota enforcement, priority queue) | — | Week 3 |
| **2A** | MCP Gateway (local tool routing + Atum API forwarding) | — | Week 4 |
| **2B** | AI Provider Router (claude, gpt-4, fallback) | — | Week 5 |
| **2C** | Secrets Manager (encrypted per-tenant store) | — | Week 5 |
| **3A** | Event Sourcing + Workflow Engine | — | Week 6 |
| **3B** | OTel integration + dashboards | — | Week 7 |
| **4** | CLI + docs | — | Week 8 |

---

## Open Decisions

1. **Agent placement**: Container (Podman/Docker) vs. process pool? Containers are heavier but stricter isolation.
2. **State store**: PostgreSQL (transactional) vs. DynamoDB (serverless) vs. Redis (fast but ephemeral)?
3. **Message bus**: NATS (simple) vs. gRPC streams (less infra) vs. Kafka (heavy but proven)?
4. **MCP forwarding**: Should agents call remote MCP servers directly, or proxy through the gateway?
5. **Cost attribution**: Per-agent per-model rates? Blended tenant billing? Usage-based discount tiers?
