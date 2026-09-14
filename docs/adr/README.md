# Architecture Decision Records

Decisions that changed the shape of AVM, recorded in the
[Michael Nygard format](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
(Title / Status / Context / Decision / Consequences).

An ADR is **immutable once accepted**. A decision is revised by writing a new
ADR that supersedes the old one, not by editing history. The superseded record
stays, with a pointer forward — the reasoning that was correct at the time is
part of the argument for why it changed.

## Index

| ADR | Title | Status | Supersedes | Affects |
|---|---|---|---|---|
| [0001](0001-embedded-sqlite-and-grpc-replace-postgres-and-nats.md) | Embedded SQLite + gRPC replace Postgres + NATS | Accepted | — | `avm-queue` → `avm-events`, `avm-storage`, `avm-gateway`, `avm-scheduler`, `proto/` |
| [0002](0002-container-isolated-agents.md) | Container-isolated agents supersede fork/exec | Accepted | Process-pool execution (`ARCHITECTURE.md` §Process Pool Execution) | `avm-executor`, `avm-models`, `avm-gateway`, `proto/`, `docs/network-policies/` |

## Relationship between the two

They meet at one seam, deliberately. ADR-0002 gives a containerized agent a
single network hole — the gateway. An agent emitting an event mid-run posts to
`POST /events`, which is the **same ingress path** an inbound GitHub webhook
takes in ADR-0001: verify → append to the local SQLite log → `202`. One code
path, two callers.

## Status vocabulary

| Status | Meaning |
|---|---|
| **Proposed** | Written, not yet agreed. Do not build against it. |
| **Accepted** | Agreed. The tree is expected to converge on it. |
| **Superseded by NNNN** | Was accepted; a later ADR replaced it. Kept for the reasoning. |
| **Deprecated** | No longer applies and nothing replaced it. |

An ADR may be **Accepted** while the code still contradicts it — that gap is
the work item, and each ADR names the crates that have to move.

## Adding one

1. Next free number, zero-padded to four: `NNNN-kebab-case-title.md`.
2. Nygard template. Context is the interesting part — record the forces, the
   options rejected, and what was *not* known at the time.
3. Consequences get **both** columns. An ADR with only upside is a sales
   document, not a decision record.
4. Add a row here.
