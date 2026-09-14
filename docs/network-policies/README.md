# Kubernetes NetworkPolicies — container-layer isolation

Layer 3 of AVM's defence-in-depth. See `IMPLEMENTATION_PLAN.md`
§ *Network Isolation & A2A Security* for the whole model; this directory is
the operator-facing half of **Phase 3**.

## What each layer protects against

The four layers are independent. Each one assumes the others may have failed.

| Layer | Mechanism | Threat it stops | Threat it does **not** stop |
|---|---|---|---|
| 1. Gateway | `validate_a2a_dispatch()` in `avm-gateway/src/security.rs` | An agent asking the platform to route work to an agent it is not allowed to reach | An agent that bypasses the gateway and dials a Pod directly |
| 2. NATS ACLs | Per-tenant credentials, subject-scoped (`docs/nats/`) | A compromised agent subscribing to or publishing on another tenant's job subjects | Direct Pod-to-Pod traffic that never touches NATS |
| 3. **NetworkPolicy** | **This directory** | **Direct Pod-to-Pod traffic between agents; egress to anything that is not a sanctioned platform service** | **Anything inside a single Pod; a CNI that does not enforce policy** |
| 4. Postgres RLS | `migrations/006_add_agent_isolation_rls.sql` | A query with a missing `WHERE tenant_id = …`, or a leaked DB credential reading another tenant's rows | Damage done before the query reaches Postgres |

The honest summary: layer 1 is the only one that understands *policy*
(trusted peers, intra-project opt-in). Layers 2–4 are blunt boundaries that
make the sanctioned path the only reachable path.

## Files

| File | Purpose |
|---|---|
| `block-intra-project-a2a.yaml` | Default-deny egress + ingress for agent Pods, then whitelist DNS, gateway, control plane, NATS, Postgres. **Apply this first.** |
| `allow-gateway-egress-only.yaml` | **The default agent policy.** Deny all egress from an agent Pod except DNS and the gateway service/port. Applied to every agent container by default. |
| `allow-peer-communication.yaml` | Templated, annotated opt-in exception for one caller → one callee. Same tenant only. |

## The default agent policy: `allow-gateway-egress-only.yaml`

This is the policy every agent workload gets unless an operator deliberately
widens it. It denies **all** egress from Pods labelled
`avm.io/workload: agent`, then re-admits exactly two destinations:

1. **DNS** (`kube-system`, UDP/TCP 53) — without it nothing resolves.
2. **The gateway** (`avm-gateway.avm-system`, TCP 8080) — the single sanctioned
   way for an agent to reach MCP tools, memory, mid-run event emits, and other
   agents.

Everything else — other agent Pods, the control plane, NATS, Postgres, the
public internet, the cloud metadata endpoint — is denied. An agent that wants
to talk to another agent must ask the gateway, where
`validate_a2a_dispatch()` applies policy and writes an audit record.

### Why this only became real with containers

Under the previous `fork/exec` executor the agent ran as a child process of
the executor, inside the executor's Pod and network namespace. "Agents do not
make direct inter-agent RPC calls" was a **convention** — nothing stopped an
agent binary from opening a socket to any address the executor could reach,
and a NetworkPolicy selecting `avm.io/workload: agent` had no Pod of its own
to select.

With agents running as OCI containers (`avm-executor/src/sandbox.rs`), each
agent gets its own network namespace and its own labelled workload identity.
That is what turns this file from documentation into **enforcement**, and it
is what makes `block-intra-project-a2a.yaml` meaningful: the deny is now
applied at a boundary the agent cannot step around.

The sandbox reinforces the same posture one layer lower: `NetworkMode` defaults
to `None` (no network at all) and is only widened to `GatewayOnly` when a
job-scoped gateway credential is injected. Container-level network mode and
cluster-level NetworkPolicy are two independent expressions of the same rule —
if the CNI silently ignores policy, the container still has no route.


## Prerequisites

1. **A CNI that enforces NetworkPolicy** — Calico, Cilium, Antrea, or a
   managed equivalent. Flannel (without an add-on) silently ignores these
   objects, in which case applying them buys you nothing. Verify, do not
   assume.
2. **Namespace labels.** The rules select platform services by
   `kubernetes.io/metadata.name: avm-system`. Kubernetes ≥ 1.21 sets this
   label automatically; on older clusters add it by hand.
3. **Pod labels on agent workloads**, applied by the executor:
   - `avm.io/workload: agent`
   - `avm.io/tenant: <tenant-id>`
   - `avm.io/project: <project-id>`
   - `avm.io/agent: <agent-id>`

## Applying

```sh
# 1. Baseline: deny everything, allow platform services.
kubectl -n <tenant-namespace> apply -f block-intra-project-a2a.yaml

# 2. Verify the deny actually bites (this MUST fail/time out).
kubectl -n <tenant-namespace> exec deploy/<agent-a> -- \
  timeout 5 nc -vz <agent-b-pod-ip> 8081

# 3. Verify the gateway is still reachable (this MUST succeed).
kubectl -n <tenant-namespace> exec deploy/<agent-a> -- \
  timeout 5 nc -vz avm-gateway.avm-system.svc.cluster.local 8080
```

Step 2 is not optional. A NetworkPolicy that is not enforced looks exactly
like one that is, right up until an incident.

## When to use the peer exception

Reach for `allow-peer-communication.yaml` only when **all** of these hold:

- The two agents are in the **same tenant** (cross-tenant is never permitted,
  at any layer, for any reason).
- The traffic genuinely cannot go through the gateway — bulk streaming, a
  non-HTTP sidecar protocol, or a hard latency floor. "It is more convenient"
  does not qualify.
- The callee's Agent Card already grants the caller access
  (`allow_intra_project: true` **and** the caller in `trusted_peers`), so the
  network posture and the policy posture agree.
- The exception has a named owner and a review date, recorded in the
  annotations on the object.

Direct peer traffic is invisible to `validate_a2a_dispatch()`, so **it does
not appear in the A2A audit trail.** That is the real cost of the exception,
and it is why the default is off.

## Rollout order

Apply the baseline to a single non-production tenant namespace first, run the
connectivity checks above, then widen. Rolling this out cluster-wide in one
step will take down any agent that was quietly relying on a path nobody
documented — which is precisely the traffic you are trying to find.
