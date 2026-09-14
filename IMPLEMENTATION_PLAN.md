# AVM Implementation Plan: Container-Based Execution

## Overview

Move from hypothetical cgroup-based isolation (cgroups v2 + systemd-run) to **production-grade OCI container isolation** (Docker/Podman) for agent job execution. This enables security, scalability, and Kubernetes readiness.

---

## Phase 1: Executor Foundation (Weeks 1–2)

### Goal
Build the **Executor** subsystem to spawn, manage, and observe container-based agent jobs.

### Scope

#### 1.1 Container Runtime Selection
- **Decision:** Podman (rootless by default, no daemon, OCI-compliant)
- **Rationale:** Kubernetes-native, single-binary deployment, good for multi-tenant isolation
- **Fallback:** Docker (if Podman not available; requires daemon, but compatible OCI image format)

**Implementation:**
```go
// executor/runtime/runtime.go
type ContainerRuntime interface {
    Run(ctx context.Context, req *RunRequest) (*RunResult, error)  // Create + Wait
    Stop(ctx context.Context, jobID string) error                   // Send SIGTERM
    Cleanup(ctx context.Context, jobID string) error                // podman rm -f
}

type PodmanRuntime struct {
    binary     string  // /usr/bin/podman
    timeout    time.Duration
}
```

#### 1.2 Executor Service
- Manage **ExecutorConfig** (max_concurrency, memory_limit, cpu_limit, timeout)
- Spin up a **job queue** + **worker pool** (default 10 workers)
- Each worker: Run container, capture stdout/stderr, emit metrics

**Implementation:**
```go
// executor/executor.go
type Executor struct {
    runtime     ContainerRuntime
    config      ExecutorConfig
    jobQueue    chan *Job
    workers     []*Worker
    metrics     MetricsCollector
}

func (e *Executor) Submit(ctx context.Context, job *Job) (string, error) {
    // Enqueue + return job ID immediately
}

func (e *Executor) Poll(ctx context.Context, jobID string) (*JobStatus, error) {
    // Return status: PENDING, RUNNING, SUCCEEDED, FAILED, TIMEOUT
}
```

#### 1.3 Job Input/Output Contract
- **Input:** JSON payload on stdin (job_id, task, tenant_id, agent_id)
- **Output:** JSON result on stdout (status, output_text, exit_code, duration_ms)
- **Error handling:** Non-zero exit code = failure (agent logs errors to stdout)

**Agent Binary Interface (pseudocode):**
```bash
#!/bin/bash
# /agent/bin/run (agent binary inside container)
set -o pipefail

read -r JOB_JSON  # Read job from stdin

TASK=$(echo "$JOB_JSON" | jq -r '.task')
TENANT_ID=$(echo "$JOB_JSON" | jq -r '.tenant_id')

# Execute work (pseudocode; real agent calls LLM, runs tools, etc.)
OUTPUT=$(run_agent_logic "$TASK" "$TENANT_ID")
EXIT_CODE=$?

# Write result to stdout
jq -n --arg status "$([ $EXIT_CODE -eq 0 ] && echo SUCCEEDED || echo FAILED)" \
      --arg output "$OUTPUT" \
      --arg exit_code "$EXIT_CODE" \
      '{status: $status, output_text: $output, exit_code: ($exit_code | tonumber)}'

exit $EXIT_CODE
```

#### 1.4 Basic Observability
- Emit **executor.job.{submit, start, complete, timeout}** OTel events
- Log job duration, memory peak, CPU, exit code
- No tracing inside the container yet (Phase 2)

**Metrics:**
```
executor.jobs.submitted_total (counter)
executor.jobs.running (gauge)
executor.jobs.succeeded_total (counter)
executor.jobs.failed_total (counter)
executor.jobs.timeout_total (counter)
executor.job.duration_ms (histogram)
executor.container.memory_peak_bytes (gauge)
executor.container.cpu_throttle_ms (gauge)
```

---

## Phase 2: Agent Image Build & Registry (Weeks 3–4)

### Goal
Build agent container images per tenant, push to per-tenant registries, and enable image caching.

### Scope

#### 2.1 Agent Dockerfile
Create minimal agent image:

```dockerfile
# Dockerfile
FROM alpine:3.21
RUN apk add --no-cache ca-certificates curl jq
COPY agent /agent/bin/
RUN chmod +x /agent/bin/run
USER 1000
ENTRYPOINT ["/agent/bin/run"]
```

- **Size target:** ~60 MB (Alpine 3.21 base ~25 MB + agent binary ~10 MB)
- **Multi-stage build:** Compile agent outside container, copy binary only
- **Health check:** None (K8s will add probes later)

#### 2.2 Registry Setup
- **Per-tenant registry:** ECR, Harbor, or local Docker daemon (configurable)
- **Image naming:** `REGISTRY/TENANT_ID/agent:VERSION` (e.g., `ecr.aws/my-tenant/agent:v1.0.0`)
- **Authentication:** Credentials stored in executor config (YAML or env)

**Implementation:**
```go
// executor/registry/registry.go
type Registry interface {
    Push(ctx context.Context, image string, tarball io.Reader) error
    Pull(ctx context.Context, image string) (*Image, error)
}

type ECRRegistry struct {
    aws *ecr.Client
}

type LocalRegistry struct {
    daemon ContainerRuntime
}
```

#### 2.3 CI Build Pipeline
- **Trigger:** On agent binary release (git tag `v*`)
- **Job:** Build Dockerfile, tag with version + `latest`, push to per-tenant registry
- **Caching:** Layer caching via registry (if supported) or local daemon

**Build Script (executor/scripts/build-agent-image.sh):**
```bash
#!/bin/bash
AGENT_VERSION=$1
TENANT_ID=$2
REGISTRY=$3

podman build \
  --tag "$REGISTRY/$TENANT_ID/agent:$AGENT_VERSION" \
  --tag "$REGISTRY/$TENANT_ID/agent:latest" \
  -f executor/Dockerfile \
  .

podman push "$REGISTRY/$TENANT_ID/agent:$AGENT_VERSION"
podman push "$REGISTRY/$TENANT_ID/agent:latest"
```

#### 2.4 Image Caching Strategy
- **Pull policy:** `IfNotPresent` (use local layer cache; pull if missing)
- **Layer caching:** Register layers with SHA256 before pulling
- **Expiry:** Keep only last 3 versions locally; GC on pod startup

---

## Phase 3: Security & Resource Limits (Weeks 5–6)

### Goal
Enforce CPU, memory, process limits, and user isolation per job.

### Scope

#### 3.1 Container Resource Limits
- **CPU:** `--cpus=0.6` (60% of 1 core; configurable per job)
- **Memory:** `--memory=512m` (hard limit; configurable; includes swap)
- **Swap:** `--memory-swap=512m` (cap swap to same as memory to prevent spill)
- **OOM behavior:** `--oom-kill-disable=false` (kill container on OOM, not host)

**Implementation:**
```go
// executor/container_config.go
type ContainerLimits struct {
    CPULimitFraction float64       // 0.6 = 60% of 1 core
    MemoryMB         int64         // 512 MB default
    MaxProcesses     int64         // 10 (pid limit)
}

func (c *ContainerLimits) ToPodmanArgs() []string {
    return []string{
        fmt.Sprintf("--cpus=%v", c.CPULimitFraction),
        fmt.Sprintf("--memory=%dm", c.MemoryMB),
        fmt.Sprintf("--pids-limit=%d", c.MaxProcesses),
        "--memory-swap=512m",
        "--oom-kill-disable=false",
    }
}
```

#### 3.2 User & Namespace Isolation
- **User:** Non-root uid (uid=1000, gid=1000) inside container; mapped to separate host user
- **Network:** `--network=none` (loopback only; for future bridges)
- **IPC:** `--ipc=private` (no cross-container shared memory)
- **UTS:** `--uts=private` (unique hostname per container)

**Implementation:**
```go
// executor/pod.go
func (e *Executor) buildRunCommand(job *Job, imageName string) []string {
    args := []string{"run", "--rm"}
    args = append(args, "--name", job.ID)
    args = append(args, "--user", "1000:1000")
    args = append(args, "--network", "none")
    args = append(args, "--ipc", "private")
    args = append(args, "--uts", "private")
    args = append(args, "--security-opt", "no-new-privileges:true")
    args = append(args, job.Limits.ToPodmanArgs()...)
    args = append(args, imageName)
    return args
}
```

#### 3.3 Filesystem Isolation
- **Root:** Read-only overlay FS (agent cannot modify /app, /etc, /usr, /lib)
- **/tmp:** `--tmpfs /tmp:noexec,nodev,noexec` (in-memory, no exec)
- **Volumes:** No host volumes (future: use `--volume` for sandboxed mounts if needed)

**Implementation:**
```go
// executor/pod.go
args = append(args,
    "--read-only",  // RO root
    "--tmpfs", "/tmp:size=256m,noexec,nodev",  // In-memory /tmp
)
```

#### 3.4 Capability Dropping
- Drop all Linux capabilities: `--cap-drop=all`
- Add only required capabilities (none for agent jobs; future: add CAP_NET_BIND_SERVICE if needed)

**Implementation:**
```go
// executor/pod.go
args = append(args,
    "--cap-drop=all",  // Drop everything
    "--security-opt", "apparmor=unconfined",  // Allow default AppArmor (strict profile later)
)
```

---

## Phase 4: Advanced Features (Weeks 7–8)

### Goal
Support dynamic limits, job preemption, and K8s integration prep.

### Scope

#### 4.1 Per-Job Custom Limits
- Allow agent to specify CPU, memory, timeout at submission time
- Validate against tenant quota before scheduling

**Implementation:**
```go
// executor/api.go
type JobRequest struct {
    AgentID     string
    Task        string
    TenantID    string
    CPULimitFraction float64  // Optional; override default
    MemoryMB    int64         // Optional; override default
    TimeoutSecs int64         // Optional; override default (900s)
}
```

#### 4.2 Job Preemption
- If job is stuck (no output for 60s), send SIGTERM
- If after 30s it's still running, send SIGKILL
- Emit preemption metric + incident log

**Implementation:**
```go
// executor/worker.go
func (w *Worker) runWithPreemption(ctx context.Context, job *Job) *JobResult {
    // Start container
    // Poll stdout every 5s; if stalled for 60s, SIGTERM
    // Poll every 5s; if stalled for 30s post-SIGTERM, SIGKILL
    // Emit metric on preemption
}
```

#### 4.3 K8s Job Bridge (Planning)
- **No implementation yet**, but define the interface:
  - `KubernetesExecutor implements ContainerRuntime`
  - Spawns `Job` + `Pod` resources instead of calling `podman run`
  - Scales via DaemonSet + HPA
  - Same job lifecycle (Run, Poll, Cleanup)

**Interface (for future):**
```go
// executor/runtime/kubernetes.go
type KubernetesRuntime struct {
    clientset *kubernetes.Clientset
    namespace string  // e.g., "agent-jobs"
}

func (kr *KubernetesRuntime) Run(ctx context.Context, req *RunRequest) (*RunResult, error) {
    // Create K8s Job
    // Poll pod status
    // Return result
}
```

---

## Phase 5: Deployment & Testing (Weeks 9–10)

### Goal
End-to-end testing, deployment guide, and operational runbook.

### Scope

#### 5.1 Integration Tests
- **Unit tests:** Container limits, user mapping, capability dropping
- **Integration tests:** 
  - Submit job → container spawns → executes agent code → exits cleanly
  - Job timeout → SIGTERM → cleanup
  - Job OOM → container killed → status = FAILED
  - Concurrent jobs → no interference, resource isolation verified

**Test Framework:**
```go
// executor/executor_test.go
func TestContainerResourceLimits(t *testing.T) {
    executor := NewExecutor(config)
    job := &Job{ID: "test-1", Task: "sleep 10", Limits: &ContainerLimits{MemoryMB: 256}}
    jobID, _ := executor.Submit(context.Background(), job)
    status, _ := executor.Poll(context.Background(), jobID)
    // Assert status = SUCCEEDED after ~1s
}

func TestContainerTimeout(t *testing.T) {
    executor := NewExecutor(config)
    job := &Job{ID: "test-2", Task: "sleep 1000", TimeoutSecs: 2}
    jobID, _ := executor.Submit(context.Background(), job)
    time.Sleep(3 * time.Second)
    status, _ := executor.Poll(context.Background(), jobID)
    // Assert status = TIMEOUT
}
```

#### 5.2 Deployment Guide
- **Prerequisites:** Podman or Docker installed, per-tenant registry access
- **Executor startup:** Load config, initialize runtime, spin up worker pool
- **Health checks:** Verify runtime availability before accepting jobs
- **Observability:** Export metrics to OTel collector, logs to stdout

**Deployment Checklist:**
- [ ] Podman/Docker installed and accessible
- [ ] Per-tenant registries configured in executor config
- [ ] Agent image built and pushed to registries
- [ ] OTel collector endpoint reachable
- [ ] Executor logs scraped by log aggregator
- [ ] Metrics endpoint exposed on :9090/metrics

#### 5.3 Operational Runbook
- **Monitoring:** Executor metrics (jobs.running, job.duration_ms, memory_peak_bytes)
- **Alerting:** Job failure rate > 5%, job timeout rate > 1%, executor unavailability
- **Troubleshooting:** 
  - Job stuck? Check pod logs: `podman logs job_abc123`
  - High memory? Adjust `MemoryMB` limit; check agent code for leaks
  - Timeout frequently? Increase `TimeoutSecs`; check task complexity

---

## Milestones & Success Criteria

| Phase | Milestone | Success Criteria |
|---|---|---|
| 1 | Executor + job queue | Can submit/poll jobs; jobs execute and complete cleanly |
| 2 | Agent images + registry | Per-tenant images build on CI; executor pulls and caches locally |
| 3 | Security & limits | CPU, memory, user, capability limits enforced; no host compromise |
| 4 | Advanced features | Per-job limits work; preemption on stall; K8s interface defined |
| 5 | Deployment + tests | E2E tests pass; deployment guide + runbook written; monitored in prod |

---

## Risk Mitigation

| Risk | Mitigation |
|---|---|
| **Container runtime unavailable** | Fallback to cgroup-based executor (Phase 1 only); emit alert |
| **Registry unreachable** | Local layer cache; retry with exponential backoff; alert on repeated failures |
| **Agent binary crash inside container** | Non-zero exit code → FAILED status; agent logs still available in stdout |
| **OOM kill container** | Pre-set memory limit; job fails with FAILED status; not host OOM |
| **Resource starvation** | Per-job limits + global concurrency cap; backpressure on queue |
| **K8s integration delay** | Podman executor works standalone; K8s bridge in Phase 4 (optional) |

---

## Out of Scope (for now)

- GPU support (future: `--gpus` flag)
- Persistent volumes (jobs are ephemeral)
- Network bridges (jobs are isolated)
- Multi-tenant cgroup sharing (each job gets full allocation)
- Custom AppArmor/SELinux profiles (use defaults for now)

---

## Questions for Review

1. **Podman vs. Docker?** Podman is preferred (rootless, no daemon), but fallback to Docker OK?
2. **Registry strategy?** Per-tenant ECR? Or single Harbor instance with RBAC?
3. **Resource defaults?** 0.6 CPU, 512 MB memory, 10 max processes — reasonable?
4. **Preemption behavior?** SIGTERM after 60s inactivity, SIGKILL after 30s more — OK?
5. **K8s timeline?** Build foundation now (Phase 1–3); K8s bridge in Phase 4 (optional for v1.0)?

---

## Next Steps

1. **Review & approval** of this plan
2. **Spike:** Test Podman setup, container isolation (resource limits, user mapping)
3. **Phase 1 start:** Implement Executor + job queue
4. **Weekly syncs:** Track progress, unblock dependencies

