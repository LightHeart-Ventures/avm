# AVM Implementation Plan

Living document. Each section is owned by the workstream that lands it; sections
are additive so parallel workstreams do not collide.

---

## Model distribution: OCI artifacts + content-addressed storage

**Status:** design landed, node-local data path implemented (`avm-models`),
scheduler/executor wiring implemented, registry client behind a feature flag.

### 1. Why OCI artifacts

Model weights are large, immutable, and shared by many agents. That is exactly
the shape of a container layer, so we reuse the container ecosystem instead of
inventing a transport:

| Need | What OCI gives us |
|---|---|
| Immutability | Content-addressed digests, end to end |
| Dedup | Same digest = same bytes, cached once per node |
| Auth / rate limits | Existing registry auth (GHCR PAT, ECR IAM) |
| Provenance | Manifest annotations, cosign signatures, SBOMs |
| Mirroring | Pull-through caches, air-gapped `oras copy` |

Artifact shape:

```text
manifest   application/vnd.oci.image.manifest.v1+json
  artifactType  application/vnd.avm.model.v1+json
  config        application/vnd.avm.model.config.v1+json   { backend, params, license }
  layer[0]      application/vnd.avm.model.weights.v1       <the weights blob>
```

Rust client: **`oci-client`** — the oras-project crate
(`github.com/oras-project/rust-oci-client`). The bare `oras` crate name on
crates.io is an unreleased `0.0.1` placeholder and is deliberately not used.

### 2. Model reference format

```text
oci://<registry>/<repository>[:<tag>]@sha256:<64 lowercase hex>

oci://ghcr.io/lightheart/qwen3-8b:q4_k_m@sha256:e3b0c442…b855
oci://registry.local:5000/models/phi4@sha256:1111…1111
```

Rules enforced by `ModelRef::parse`:

* The `@sha256:…` digest is **mandatory** — unpinned refs are rejected at parse
  time, so nothing downstream can be surprised by a moving tag.
* The tag is advisory provenance; the digest is the identity. A `:` after the
  last `/` is a tag, a `:` inside the host is a port.
* Only `sha256` today; the `algo:hex` split leaves room for `sha512`.
* `ModelRef` carries a `backend` hint (`llama.cpp` | `vllm` | `tgi`) used to
  pick a model-server image, and `size_bytes` for GC accounting.

Agents reference models by this URI in their Agent Card (`model_ref`), which
keeps the A2A surface unchanged: the card carries a string, the scheduler
resolves it to a digest and scores residency.

### 3. Content-addressed storage layout

Store root `/var/lib/avm/models` (override per node):

```text
/var/lib/avm/models/
  blobs/sha256/<aa>/<full-hex>   immutable weights, bind-mounted ro into model servers
  meta/<full-hex>.json           ModelRef + pulled_at + last_access + verified
  tmp/<full-hex>.part            staging for in-flight pulls
```

Invariants:

* **Atomic publish.** Bytes are staged in `tmp/`, hashed, and only then
  `rename(2)`d into `blobs/`. A blob visible under `blobs/` is always complete —
  a crashed pull leaves garbage in `tmp/`, never a torn blob.
* **Verify before publish.** `commit_bytes` refuses a digest mismatch, so a
  corrupt or MITM'd artifact can never become resident.
* **Two-char shard** (`blobs/sha256/ab/ab12…`) keeps directory fan-out sane.
* **Metadata is a sidecar, not a lock.** Losing `meta/` degrades residency to
  `absent` and triggers a re-pull; it never corrupts the blob.
* `blobs/` is the only path mounted into containers, always read-only. A model
  server can never mutate weights.

### 4. Pull / cache / residency flow

```text
scheduler ──placement──▶ executor(node)
                            │ 1. ModelStore::pull(model_ref)
                            │      meta hit + blob present ─▶ touch, done (no network)
                            │      miss ─▶ ArtifactFetcher::fetch
                            │             OciArtifactClient  (registry)
                            │             LocalDirFetcher    (air-gapped mirror)
                            │ 2. sha256 verify ─▶ tmp/ ─▶ rename ─▶ blobs/
                            │ 3. Postgres: model_pulls (checksum_ok, duration_ms)
                            │ 4. Postgres: model_placements (status, serving, endpoint)
                            └ 5. publish node label model.avm.io/<digest>=resident
```

Residency ladder:

| State | Meaning |
|---|---|
| `resident` | Blob on disk **and** digest verified |
| `cached` | Blob on disk, verification deferred (cheap hot path) |
| `absent` | Node must pull |
| `pulling` / `failed` | Transient states recorded in Postgres only |

Postgres (migration `006_create_models.sql`) is the cluster-wide view:

* `models` — catalogue keyed by digest (registry, repo, tag, size, backend).
* `model_pulls` — append-only pull + checksum audit trail per node.
* `model_placements` — current `(digest, node_id)` residency, `serving` flag and
  endpoint; the scheduler's residency input and what node labels are rebuilt
  from after an executor restart.

### 5. GC strategy

`GcPolicy` = **LRU by `last_access`, bounded by a hard byte ceiling**:

* `max_bytes` — hard ceiling; eviction runs until usage is at or below it.
* `high_watermark` (default `0.85`) — the level that *triggers* a pass, so GC
  does not thrash at the boundary.
* `min_age` (default 15 min) — a freshly pulled blob is never evicted, which
  kills the race where GC reaps weights a pending placement is about to use.
* `pinned` — digests with a live model server; never evicted.
* `dry_run` — report the eviction set without deleting (what `avm models gc
  --dry-run` prints).

Eviction order is oldest-access-first; every pass returns a `GcReport`
(`bytes_before`, `bytes_after`, `evicted`, `retained_pinned`) which is logged and
mirrored into `model_pulls` as `evicted` rows so cache churn is measurable.

### 6. Scheduling and affinity

Placement is a **soft-constraint scorer** (`avm-scheduler::placement`), not a
bin-packer:

```text
score = residency_weight · residency(node, digest)    resident 100 / cached 40 / absent 0
      + affinity_weight  · model_server_live(node)    +50 when a server already serves it
      + spread_weight    · free_slot_fraction(node)   ×20, keeps the cluster from hot-spotting
      - pull_penalty     · would_cold_pull(node)      −25, cold pulls cost minutes
```

* **Residency is soft.** A node without the weights stays *feasible* — it just
  loses to one that has them. This avoids the deadlock where a brand-new digest
  is unschedulable everywhere.
* **Hard constraints** are the only source of infeasibility: `required_labels`
  (e.g. `gpu.avm.io/kind=a100`), insufficient free slots, cordoned node.
* **Model affinity** co-schedules an agent with a live model server on the same
  node (`serving_digests`), so inference is a loopback call rather than a
  cross-node hop.
* Every `PlacementScore` keeps its components (`residency_score`,
  `affinity_score`, `spread_score`, `pull_penalty`) plus a rejection `reason`, so
  `avm scheduler explain` can show why a node won or lost.

Node labels are the contract between executor and scheduler:

```text
model.avm.io/sha256:<hex> = resident | cached | absent
```

### 7. Model servers (`kind: ModelServer`)

`ExecutorKind::ModelServer` runs an inference container that mounts the blob
store read-only:

```text
--mount type=bind,src=/var/lib/avm/models/blobs,dst=/models,ro
--model /models/sha256/e3/e3b0c442…b855
```

* Weights are **never** baked into the server image — one image serves any
  digest the node holds.
* **Multiple model servers per node are allowed.** How many, and whether a
  single multi-model server is preferable, is deliberately left open (see
  follow-ups); nothing in the design assumes one server per node.
* After `/health` passes, the executor publishes `model.avm.io/<digest>=resident`
  and writes `model_placements(serving=true, endpoint=…)`.
* Env handed to the container: `AVM_MODEL_DIR`, `AVM_MODEL_SERVER`,
  `AVM_MODEL_PORT`, `AVM_MODEL_DIGESTS`, `AVM_MODEL_BACKEND`.

### 8. Feature gating and hermetic tests

`avm-models` compiles the real registry client only under
`--features oci-registry`; the default build has no TLS/HTTP dependency and the
whole crate is unit-testable offline via `LocalDirFetcher`. Without the feature
`OciArtifactClient::pull_blob` returns `ModelError::RegistryFeatureDisabled`, so
call sites, the scheduler and the executor compile identically either way.

```bash
cargo test  --workspace                            # hermetic, no network
cargo check -p avm-models --features oci-registry  # real registry client
```

### 9. Follow-ups (deliberately out of scope here)

| Item | Why deferred |
|---|---|
| Model-server fan-out policy (one vs. many per node) | Needs a separate spike; nothing here assumes a single server |
| `oras push` from Rust | Publishing runs in CI today; pull is the hot path |
| Streaming / chunked pulls with resume | Current fetcher buffers; fine for the sizes we ship first |
| Cosign signature + SBOM verification at pull time | Slots in as another `commit_bytes` precondition |
| Pull-through registry mirror per rack | Bandwidth optimisation, not correctness |
| Proactive pre-warm (pull on placement *intent*) | Wants placement telemetry first |
