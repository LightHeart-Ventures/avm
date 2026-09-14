# ADR-0001: Embedded SQLite + gRPC replace Postgres + NATS

- **Status:** Accepted
- **Date:** 2026-02-14
- **Affects:** `avm-queue` (→ `avm-events`), `avm-storage`, `avm-gateway`, `avm-scheduler`, `proto/`
- **Related:** [ADR-0002 — Container-isolated agents](0002-container-isolated-agents.md)

---

## Context

AVM as originally specified runs two pieces of shared infrastructure:

- **PostgreSQL 16** via `sqlx` — memories, jobs, audit logs, quotas, models
  (`migrations/001`–`007`), with Row-Level Security as the Phase-4 tenant
  isolation layer.
- **NATS JetStream** via `async-nats` — the job bus
  (`avm.jobs.<tenant>.<project>` / `avm.results.<tenant>.<project>`), with
  per-tenant accounts and subject ACLs as the Phase-2 isolation layer
  (`avm-queue/src/publisher.rs`, `avm-queue/src/subscriber.rs`).

Three forces pushed against that shape.

**1. Two durable stores means a dual write.** A job transitions to `succeeded`
in Postgres and a `job.succeeded` message is published to NATS. Those are two
systems, two failure domains, and no shared transaction. The standard fix is a
transactional outbox: write the event into a Postgres table inside the state
transaction, then have a relay drain that table into NATS. That is correct and
it is also a whole subsystem — a relay process, a cursor, a poison-message
policy, at-least-once dedup on the consumer, and a permanent class of bug
("the outbox drifted") that only manifests under partial failure. We had not
built it yet. The choice was therefore to build it, or to remove the need for
it.

**2. AVM is not a single logical database.** A scope (`t_x/b_y/ag_z`) lives on
exactly one node at a time — that is already true of the scheduler's placement
model (`avm-scheduler/src/placement.rs`). A shared Postgres gives global
ordering and cross-node queries that the workload does not actually ask for,
in exchange for an operational dependency on every node's hot path.

**3. Two operational dependencies is the ops budget of a product AVM does not
have yet.** Every deployment needs a Postgres and a NATS cluster before a
single agent runs. Both are excellent; neither is free to run correctly
(backups, failover, NSC credential rotation, JetStream storage tuning).

At the same time, gRPC is already load-bearing: `avm-proto` /
`proto/avm_service.proto`, tonic + prost, and the OTel work
(PR #5) already propagates W3C trace context over it. mTLS, flow control,
streaming and trace propagation are wired.

We also already ship an embedded, node-local, content-addressed data path
(`avm-models`) with no dependency on Postgres or NATS at all — proof the
node-local shape works for the parts of AVM that most needed to be fast.

## Decision

1. **Per-node embedded SQLite (WAL mode) replaces the shared Postgres
   instance.** Each node owns its own database file.
2. **gRPC is the only intra-cluster transport. NATS/JetStream is removed.**

The node's SQLite file **is** the durable event log and the transactional
outbox. A state change and the event it emits are written in **one SQLite
transaction**. This does not *solve* the dual-write problem; it deletes the
category — there is no second system to drift from.

### Event-log schema

```sql
CREATE TABLE events (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,  -- per-node monotonic offset
  event_id   TEXT NOT NULL UNIQUE,               -- uuidv7, idempotency key
  ts         TEXT NOT NULL,
  scope      TEXT NOT NULL,                      -- t_x/b_y/ag_z
  type       TEXT NOT NULL,                      -- job.succeeded, webhook.github.pull_request
  source     TEXT NOT NULL,
  subject    TEXT,
  payload    BLOB NOT NULL,
  trace_id   TEXT
);
CREATE INDEX events_scope_seq ON events(scope, seq);
CREATE TABLE subscriptions (sub_id TEXT PRIMARY KEY, kind TEXT, filter TEXT, cursor_seq INTEGER, backoff_until TEXT);
CREATE TABLE triggers      (trigger_id TEXT PRIMARY KEY, scope TEXT, event_glob TEXT, action TEXT);
CREATE TABLE deliveries    (event_id TEXT, sub_id TEXT, attempt INT, status TEXT, last_error TEXT, PRIMARY KEY(event_id,sub_id));
```

`seq` is the offset. Everything downstream — subscribers, the egress
dispatcher, cross-cluster bridges — is a **cursor over `seq`**, which makes
"resume where you left off" the same mechanism in all four planes rather than
four different mechanisms. `event_id` (uuidv7) is the idempotency key carried
end to end, so at-least-once delivery is safe at every hop.

`trace_id` is a column, not an afterthought: the OTel work already puts a W3C
trace context on every gRPC hop, and an event that cannot be correlated to the
job that produced it is an event you cannot debug.

---

## The event bridge — four planes

| Plane | Mechanism | Contract |
|---|---|---|
| **Ingress** | `POST /hooks/{tenant}/{project}/{hook_id}` on `avm-gateway` | Verify signature **before** parsing → append to log → return `202`. Never do work in the request path. |
| **Intra-cluster** | gRPC server-streaming `EventBus.Subscribe(from_seq)` | At-least-once, resumable by offset, flow control for free from HTTP/2. |
| **Egress** | Dispatcher reads the local log at a cursor, matches `triggers` | HMAC-SHA256-signed `POST`, `X-AVM-Signature: t=…,v1=…` (GitHub/Stripe style), exponential backoff, dead-letter, idempotency key = `event_id`. |
| **Cross-cluster** | Bidirectional gRPC `Bridge(stream Envelope)` over mTLS | Same cursor machinery, plus scope translation and a loop guard. |

The same log, read four ways. No plane has a private durability story.

### Proto sketch

```proto
service EventBus {
  rpc Publish  (stream Event)      returns (PublishAck);
  rpc Subscribe(SubscribeRequest)  returns (stream Event);  // {scope_filter, type_globs, from_seq}
  rpc Ack      (AckRequest)        returns (Empty);
  rpc Bridge   (stream Envelope)   returns (stream Envelope);
}
message Envelope { Event event = 1; string origin_cluster = 2; repeated string path = 3; } // path[] = loop guard
```

### Ingress specifics

- **Verify before parse.** A bad signature must never allocate a JSON parse.
  The signature check reads raw bytes; only a verified body is deserialized.
  This is the difference between a rate-limit problem and a memory-pressure
  problem when someone points a script at the hook URL.
- **Pluggable verifiers** — `github` / `stripe` / `slack` / `generic-hmac` —
  with secrets resolved from the sealed store, never from the request.
- **Dedup on the provider delivery-id** (`X-GitHub-Delivery` and equivalents)
  via a unique index. Providers retry; retries must be free.
- **Store the raw body**, size-capped and TTL'd, so a delivery can be
  re-driven locally without asking the provider to resend. The provider's
  retry window is shorter than our debugging window.
- **Normalize to a CloudEvents-compatible canonical envelope** —
  `type: "webhook.github.pull_request"`, `source: "gh:owner/repo"`, `scope`
  derived from the URL path. Providers differ; everything downstream of
  ingress sees one shape.

### Triggers unify cron, webhook and job-completion into one primitive

There is no separate cron subsystem, no separate webhook subsystem and no
separate job-completion hook. There is one table and one matcher.

| Event | Action | Note |
|---|---|---|
| `webhook.github.pull_request` | `job{agent_id}` | GitHub PR starts an agent run |
| `schedule.tick` | `job{}` | Nightly digest. The scheduler emits a **synthetic** `schedule.tick`; cron is just an event source |
| `job.succeeded` | `webhook{url, secret_ref}` | Outbound notification, signed, retried, dead-lettered |
| `job.succeeded` | `a2a{agent_id}` | Routes through the existing `validate_a2a_dispatch()` — the A2A policy gate is not bypassed by being event-driven |
| any glob | `bridge{peer}` | Forward to a peer cluster, subject to the discipline below |

The payoff is that cron inherits retry/audit/replay from the event log for
free, and A2A dispatch keeps exactly one policy chokepoint
(`avm-gateway/src/security.rs`) whether the caller is an agent or a trigger.

### Cross-cluster discipline

1. **Scope translation is MANDATORY.** `t_acme@cluster-a` is *not*
   `t_acme@cluster-b` unless explicitly mapped. A bridged event is rewritten
   into the local namespace or rejected. It is never implicitly trusted. A
   shared tenant-id string across two clusters is a coincidence, not an
   identity.
2. **Loop prevention via `path[]`**, BGP-style: each cluster appends its id;
   an envelope whose `path[]` already contains us is dropped.
3. **Default-deny**, matching the A2A posture already established in
   `avm-agent/src/a2a_policy.rs`. Bridging is opt-in per
   `(peer, scope_glob, type_glob)`. There is deliberately no "bridge
   everything" setting.
4. **Every bridged envelope emits an audit record**, reusing the existing
   `SecurityEvent` pattern from `avm-gateway/src/security.rs` rather than
   inventing a second audit shape.

---

## ⚠️ Migration hazard — RLS has no SQLite equivalent

**This requires a decision before the port, not after it.**

`migrations/007_add_agent_isolation_rls.sql` is Postgres **Row-Level
Security**: it `FORCE`s RLS on the isolated tables with

```sql
USING (avm_current_tenant() IS NOT NULL AND tenant_id = avm_current_tenant())
```

and splits roles into `avm_app` (subject to RLS) and `avm_migrator`
(`BYPASSRLS`). Its whole value is that it is enforced **below** the
application: a missing `WHERE tenant_id = …` in Rust yields zero rows instead
of another tenant's rows, and a leaked credential used from outside the
cluster is still fenced.

**SQLite has no RLS.** There is no database-level backstop to inherit. Layer 4
of the defense-in-depth model does not port — it must be **rebuilt as a
process-level invariant** in `avm-storage`:

- Every query function takes a `Scope` parameter. Not optional, not defaulted.
- The raw connection pool is **private to the module** and never exported.
  `pub(crate)` at the widest. If another crate can get a `SqlitePool`, the
  invariant is gone.
- Scope predicates are applied by the repository layer, not by callers.
  Callers cannot express an unscoped query because the API does not have one.
- The compile-time guarantee replaces a runtime one: a missed predicate should
  be a type error, not a leak.

This is strictly weaker than RLS against a *leaked database file* — an
attacker with the SQLite file has everything on that node. It is arguably
stronger against the more common failure (a forgotten predicate), because the
API makes the unscoped query unwriteable rather than merely unreturned.

**Consequence for the rollout plan:** the Phase-4 row of the network-isolation
phased rollout in `IMPLEMENTATION_PLAN.md` no longer reads "Postgres RLS
enabled, `avm_app` / `avm_migrator` roles, `SET LOCAL` in `avm-storage`". It
becomes process-level scope enforcement in `avm-storage`, and its risk-if-
skipped is *higher*, because there is no second line of defence behind it.

---

## Crate deltas

| Crate | Change |
|---|---|
| `avm-queue` → **`avm-events`** | Rename. NATS publisher/subscriber replaced by a gRPC `EventBus` client + server, plus cursor management. |
| `avm-storage` | `sqlx` `postgres` feature → `sqlite`. Migrations 001–006 port near-verbatim. **007 (RLS) is replaced** by process-level scope enforcement. |
| `avm-gateway` | Gains `hooks.rs` (ingress + pluggable verifiers) and `bridge.rs` (peer streams). |
| `avm-scheduler` | Owns the egress dispatcher loop; emits the synthetic `schedule.tick`. |
| `proto/` | Gains `Event`, `Envelope`, `EventBus`, `Trigger`. |

Docs that describe the removed infrastructure — `docs/nats/`, and layer 2 of
the isolation model — describe a component that no longer exists. They are
retained as the record of why per-tenant NATS accounts were the right answer
*for NATS*, and annotated as superseded.

---

## Consequences

### Gains

- **No broker to operate.** No JetStream storage tuning, no NSC credential
  lifecycle, no account/subject-ACL matrix to get right per tenant.
- **The outbox is free and atomic.** State change + event in one transaction.
  The dual-write bug class is structurally absent, not carefully avoided.
- **Replay from any offset.** Debugging, backfill, and a new subscriber
  catching up are the same operation: `Subscribe(from_seq)`.
- **gRPC brings mTLS, flow control and trace-context propagation already
  wired.** Backpressure is HTTP/2 flow control rather than a consumer-lag
  metric and a tuning knob.
- **Ingest latency is one local fsync.** No network hop to acknowledge a
  webhook.
- **One fewer identity system.** Peer identity is the mTLS certificate, the
  same identity gRPC already uses.

### Losses — stated plainly

- **NO global ordering.** `seq` is per-node and monotonic *only within a
  node*. There is no cluster-wide total order and there will not be one.
  This is acceptable **because a scope lives on one node at a time** — within
  a scope, ordering holds, and cross-scope ordering was never a guarantee
  anything relied on. Any future feature that wants a global order must
  introduce its own sequencer; it does not get one for free.
- **Node disk loss forfeits unflushed events.** Postgres survived the loss of
  an application node. SQLite on that node does not. This is the single
  largest regression in this ADR and it is a real one.
- **A slow subscriber grows the local log.** Retention cannot be a simple age
  or size cap; it needs a floor at the **slowest live cursor**, plus an
  explicit policy for a cursor that has stopped advancing (alert, then
  forcibly abandon — never silently truncate under a live reader).
- **Cross-node fan-in needs a gateway aggregator.** "All events for tenant X"
  is no longer one query. It is a fan-out to the nodes holding that tenant's
  scopes and a merge, with no total order to merge on (merge by `ts`, accept
  that it is approximate).
- **Operational muscle memory is lost.** `psql` against a shared database is a
  well-understood debugging tool. N SQLite files across N nodes is not, and
  tooling for it has to be built.

### Durability mitigation — recommendation, not a settled decision

Two options for the disk-loss regression:

| Option | Mechanism | Cost |
|---|---|---|
| **A. Tail replication** | Replicate the log tail to the scheduler node before acking an append | Every ingest takes a network round trip; reintroduces a distributed-consensus-shaped problem |
| **B. Sender retry as the durability edge** | Accept the loss; rely on the producer to retry | Free. Bounded by the producer's retry policy |

**Recommendation for v1: option B.** GitHub and Stripe both retry with
backoff over hours — that *is* the standard contract for webhook delivery, and
building replication to beat a guarantee the sender already provides is
premature. Internally-generated events (`job.succeeded`, `schedule.tick`)
belong to a job whose state is on the same disk, so they are lost together and
are recoverable by re-reconciling the job.

Option A becomes correct the first time a node is **actually lost** and the
loss is shown to have mattered. This is a recommendation for the operator to
accept or reject, not a decision already taken — and the ack path should be
written so that inserting tail replication later is a change in one function,
not a redesign.

---

## Options rejected

| Option | Why not |
|---|---|
| Keep Postgres + NATS, build a proper transactional outbox | Correct, and a whole subsystem to build and operate, to reach a property SQLite gives for free by having one store |
| Keep Postgres, drop NATS (poll the outbox over gRPC) | Removes the broker but keeps the shared-database operational dependency and the cross-node hot-path latency |
| Drop Postgres, keep NATS JetStream as the log | JetStream *is* a good log. It is also the operational dependency we were trying to remove, and it cannot share a transaction with node-local state |
| Embedded key-value store (RocksDB/sled) instead of SQLite | No SQL, no ad-hoc queries, weaker migration story. SQLite's WAL mode, ACID guarantees and `.dump`-based backups are the reason it wins for a store that humans will need to inspect |
