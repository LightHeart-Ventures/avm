# ADR-0002: Container-isolated agents supersede fork/exec

- **Status:** Accepted
- **Date:** 2026-02-14
- **Supersedes:** the process-pool execution model (`ARCHITECTURE.md` §Process Pool Execution; `ARCHITECTURE.md` Open Question 2)
- **Affects:** `avm-executor`, `avm-models`, `avm-gateway`, `proto/`, `docs/network-policies/`
- **Related:** [ADR-0001 — Embedded SQLite + gRPC](0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md)

---

## Context

### The codebase currently contradicts itself

Two workload types live in the same crate and are isolated by two different
mechanisms.

| Workload | File | Mechanism |
|---|---|---|
| Model server | `avm-executor/src/model_server.rs` | OCI container. `ExecutorKind::Oci` / `ExecutorKind::ModelServer` (`:31`–`:37`), `container_args()` (`:163`), blob store bind-mounted read-only from `HOST_BLOB_DIR` (`:24`) |
| Agent | `avm-executor/src/agent_runner.rs` | **`Command::new(&bin)` at `:151`** — a forked subprocess against a filesystem path |

The agent path resolves a binary by string concatenation:

```rust
// avm-executor/src/agent_runner.rs:113
pub fn resolve(&self, agent_id: &str) -> Result<String, RunError> {
    if agent_id.is_empty() || agent_id.contains('/') {   // :114 — path-traversal guard
        return Err(RunError::Unresolved(agent_id.to_string()));
    }
    Ok(format!("{}/{}", self.agent_dir.trim_end_matches('/'), agent_id))
}
```

The function that resolves *weights* — the thing that is merely data — goes
through a digest-pinned OCI reference with checksum verification
(`avm-models::ModelRef`, which **rejects an unpinned reference outright**).
The function that resolves *arbitrary user-supplied code* does
`format!("{dir}/{id}")` and guards it with a `contains('/')` check. That is
backwards, and it is the motivating evidence for this decision.

The tree already knows where it is going. `agent_runner.rs:124`–`:127` opens a
tracing span literally named **`container.pull_image`** around a step that
today pulls nothing — the vocabulary was written for containers before the
implementation was.

### What is not yet built

There is **no `cgroup.rs`**. Resource isolation is a documented stub:

```rust
// avm-executor/src/agent_runner.rs:261
/// Resource isolation (cgroup v2 / rlimit) applied before exec.
    //! TODO(avm): apply cgroup v2 cpu.max + memory.max and setrlimit before …
```

So the cost of moving to containers is *lower* than it looks: there is no
hand-rolled cgroup implementation to throw away, only a TODO that the runtime
spec answers directly.

### The security argument

`IMPLEMENTATION_PLAN.md` §Network Isolation states the premise plainly:
"agents are arbitrary user-supplied code… **assume every agent container is
potentially hostile**." The word *container* is already in the threat model.
Layer 3 of the four-layer defence is Kubernetes NetworkPolicies
(`docs/network-policies/block-intra-project-a2a.yaml`).

A NetworkPolicy binds to a Pod's network namespace. A forked subprocess shares
the executor's network namespace. **Under fork/exec, layer 3 does not apply to
agents at all** — "no direct inter-agent RPC by default" is a *convention*
that nothing enforces. An agent can open a socket to a sibling and the
gateway's `validate_a2a_dispatch()` (`avm-gateway/src/security.rs:286`) never
sees it. The threat-model row "Agent opens a raw socket to a sibling Pod's IP
→ stopped by layer 3" is, today, false.

## Decision

**Agents run in containers, not forked subprocesses.**

### The contract survives; only the sandbox changes

This is deliberately **not** a redesign of the agent interface. The existing
contract — `AVM_JOB_ID` / `AVM_AGENT_ID` / `AVM_SCOPE` / `AVM_TENANT_ID` /
`AVM_PROJECT_ID` as environment (`agent_runner.rs:152`–`:156`), payload on
stdin, result on stdout — is correct and stays. It is now delivered as
**container environment and PID-1 stdio** instead of subprocess environment
and pipe stdio. Agents that work today work unchanged inside a container.

Re-host the contract. Do not rewrite it.

### What changes

| Concern | fork/exec (today) | Container (decided) |
|---|---|---|
| **Resolution** | `resolve()` returns a filesystem path, `agent_dir/{agent_id}` (`agent_runner.rs:113`) | Returns an **OCI ref** — `agent_id → oci://registry/repo@sha256:…` — looked up via the `AgentService` registry (the `TODO(avm)` already on `:112` says exactly this) |
| **Path-traversal guard** | `contains('/')` check (`:114`) | **Moot.** No filesystem path is interpolated, so there is no traversal to guard. The guard is deleted, not ported |
| **Distribution** | Binary pre-staged on the node out of band | Reuses `ContentAddressedStore` from `avm-models`. An agent is just another OCI artifact — same blob dir, same `model.avm.io/<digest>` residency labels, same LRU `gc()` |
| **Resource limits** | `TODO(avm)` stub at `:261`; no `cgroup.rs` exists | Runtime spec: `memory`, `cpus`, `pids`. The runtime enforces it; we declare it |
| **Termination** | `kill_on_drop(true)` (`:160`) | SIGTERM → grace period → SIGKILL, **plus a separate image-pull timeout** |
| **Filesystem** | Full host filesystem, executor's uid | Read-only rootfs + `tmpfs /tmp` + read-only model-blob mount (the pattern `model_server.rs` already uses) |

The **separate pull timeout** is not a detail. `wall_time_sec` currently wraps
the entire run (`agent_runner.rs:121`+). If a cold image pull is inside that
budget, a 90-second pull against a 120-second wall time leaves the agent 30
seconds and the failure reports as "agent timed out" — a misleading error that
will be debugged as an agent bug. Pull and execute get separate budgets and
separate error variants.

### The win — stated sharply

Under fork/exec, "no direct inter-agent RPC by default" was documentation.
Under containers, `docs/network-policies/block-intra-project-a2a.yaml`
**actually binds**.

Layer 3 of the defence-in-depth model moves **from documentation to
enforcement**. That is the entire point of this ADR; everything else is
consequence.

---

## The new problem containers create — and its resolution

A container does not share the executor's process space. Stdio is the only
inherited channel. That is sufficient for "read payload, do work, write
result" and **insufficient** for an agent that needs MCP tools, memory reads,
or mid-run event emission — all of which a real agent needs.

**Resolution:** inject `AVM_GATEWAY_URL` plus a **job-scoped bearer token**:

- audience = `job_id`
- TTL = the job's wall time
- minted per run, at dispatch, never reused

The network policy **denies all egress except `gateway:port`**. That single
hole is the agent's entire world. Everything — MCP tool calls, memory reads,
A2A dispatch, event emission — funnels through the gateway, where
`validate_a2a_dispatch()` already sits and already audits via `SecurityEvent`.

The token bounds the blast radius of exfiltration: a stolen agent token is
useless after the job ends and useless for any other job.

### Where this ADR meets ADR-0001

An agent emitting an event mid-run calls `POST /events` on the gateway with
its job token. The gateway verifies → appends to the local SQLite log →
returns `202`.

That is **the exact same ingress path** an inbound GitHub webhook takes in
[ADR-0001](0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md#ingress-specifics):
verify before parse, append, `202`, no work in the request path.

**One code path, two callers.** The difference between "a GitHub PR opened"
and "an agent finished step three" is the verifier and the `source` field —
not the machinery. Neither decision requires the other, but together the agent
becomes a first-class event source without a second ingestion path.

---

## ⚠️ Open decision — the runtime tier (surfaced, not settled)

This ADR does **not** choose a container runtime. It requires that the choice
be made behind an abstraction.

| Tier | Isolation boundary | Cost | Concern |
|---|---|---|---|
| **Docker / containerd socket** | Namespaces | ~0 | **Weak for multi-tenant.** The executor holds a root-equivalent handle to the host daemon. Socket access is host compromise |
| **Rootless podman / youki** | Namespaces + user namespace per executor | Low | No daemon, no root handle. Namespace escapes remain kernel-surface bugs |
| **gVisor `runsc`** | Syscall interception in userspace | ~10–20% syscall overhead | Materially reduced kernel attack surface; some syscalls unimplemented |
| **Firecracker microVM** | Real kernel boundary | ~125 ms boot, heavier per-instance | Strongest. Highest resource floor |

**Recommendation to record:** abstract a **`Sandbox` trait now**
(`runc | runsc | firecracker`), default to **rootless runc**, and make the
tier a **per-tenant policy knob**.

The reasoning is about cost of change, not about which tier is best today.
Every tier has the same logical interface — start with a spec, stream stdio,
enforce limits, terminate. If that interface is extracted at the start, adding
Firecracker is an implementation of an existing trait. If it is not, the first
tenant who needs hard isolation triggers a rewrite of the executor.

AVM's premise is *running code the operator did not write*. The hard-isolation
tier must be reachable by configuration, not by a migration. **This needs the
operator's sign-off before `avm-executor` work begins.**

---

## Cost to eat

| | fork/exec | Container |
|---|---|---|
| Cold start | ~1 ms | 100 ms – 2 s, **plus pull** |

Honest framing: for short agent runs this is a 100–2000× regression in startup
latency, and no amount of tuning makes a container as cheap as `fork()`.

Mitigations, both of which already exist in the tree:

- **Residency labels and affinity scoring.** `avm-scheduler/src/placement.rs`
  already scores nodes `resident 100 / cached 40 / absent 0` and applies a
  pull penalty (`:275`–`:299`). Point the same machinery at **agent digests**
  as well as model digests and placement prefers nodes that already hold the
  image. This is a parameter change, not new infrastructure.
- **Warm pools keyed on `(tenant, agent_digest)`.** A pre-started container
  amortizes the cold start across runs. Keyed on the tenant because a warm
  container reused across tenants would reintroduce exactly the boundary this
  ADR exists to create.

---

## Crate deltas

| Crate | Change |
|---|---|
| `avm-executor` | `agent_runner.rs` drives the OCI executor instead of `Command::new`. A shared **`Sandbox` trait** is extracted, common to `agent_runner.rs` and `model_server.rs`. The `cgroup` TODO (`:261`) folds into the runtime spec rather than being implemented |
| `avm-models` | Generalize `ContentAddressedStore` from *models* to *artifacts* (agents **and** models). `ModelRef` grows a sibling or a type parameter; the digest-pinning invariant is preserved verbatim |
| `avm-gateway` | `POST /events` with job-token auth; job-token minting and verification |
| `proto/` | `AgentCard` (`proto/avm_service.proto:530`) gains **`image_ref`** (OCI digest) alongside the existing `model_ref` (`:538`) |
| `docs/network-policies/` | New **`allow-gateway-egress-only.yaml`** as the default agent NetworkPolicy — deny all egress except gateway:port |

---

## Consequences

### Gains

- **Layer 3 of the isolation model becomes real.** The NetworkPolicies in
  `docs/network-policies/` bind to agents instead of describing an intention.
- **The executor stops contradicting itself.** One sandbox abstraction for
  model servers and agents.
- **Agent distribution inherits the model data path** — digest-pinned,
  checksum-verified, content-addressed, GC'd, residency-aware. Deleting
  `format!("{dir}/{id}")` removes an entire class of resolution bug.
- **Resource limits become declarative** and are enforced by a runtime that
  does this for a living, rather than by code we would have had to write.
- **Supply chain gets a handle.** An agent identified by `sha256:…` can be
  signed, scanned and attested. A path under `/opt/avm/agents` cannot.
- **The agent's network surface becomes exactly one endpoint**, and that
  endpoint already has the policy gate and the audit trail.

### Losses — stated plainly

- **Cold start regresses by 2–3 orders of magnitude.** Mitigable, not
  eliminable. Latency-sensitive workloads will feel it.
- **A container runtime becomes a hard node dependency.** "Run the executor"
  now means "run the executor on a node with a working runtime and a reachable
  registry."
- **Registry availability enters the critical path.** A registry outage stops
  cold-start agent runs. Content-addressed caching bounds this to *cold*
  starts only, which is why residency-aware placement matters more than it did
  for models.
- **Debugging gets a layer.** `strace` on a child process is direct; the same
  investigation inside a container is a step removed, and one step further
  under `runsc` or Firecracker.
- **Disk pressure rises.** Agent images are larger than agent binaries and
  share the blob store with model weights. The LRU `gc()` policy in
  `avm-models` now arbitrates between agents and models — a policy question
  that did not previously exist.
- **The `contains('/')` guard's deletion must be deliberate.** It is safe to
  delete *only* once resolution genuinely goes through the registry. Deleting
  it while any filesystem path remains would be a regression, so the two
  changes land together or not at all.

---

## Options rejected

| Option | Why not |
|---|---|
| Keep fork/exec, implement the `cgroup.rs` TODO properly | Buys resource limits and buys **nothing** on network isolation — the shared network namespace is the actual problem, and cgroups do not touch it |
| Keep fork/exec, add seccomp + namespace isolation by hand | This is writing a container runtime. Worse than an existing one, with the same cold-start benefit that is not worth the maintenance |
| WASM sandbox for agents | Excellent isolation, very fast cold start, and it **forecloses the product premise**: agents are arbitrary user code, frequently Python with native dependencies. A compelling future *additional* tier, not a replacement |
| Containers for untrusted agents only, fork/exec for first-party | Two execution paths, two security models, and the first-party path becomes the one an attacker targets. The inconsistency this ADR exists to remove, reintroduced by policy |
