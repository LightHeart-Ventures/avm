# avm-models

Node-local model distribution: OCI artifact pulls, content-addressed storage,
residency reporting and LRU garbage collection.

| Piece | What it does |
|---|---|
| `ModelRef` | Digest-pinned reference, `oci://registry/repo[:tag]@sha256:<hex>` |
| `ModelStore` | Async `pull` / `push` / `verify` / `list_resident` / `residency` / `gc` |
| `ContentAddressedStore` | SHA-256 CAS under `/var/lib/avm/models/blobs` |
| `OciArtifactClient` | Registry client (oras-project `oci-client`), feature `oci-registry` |
| `LocalDirFetcher` | Air-gapped mirror / test byte source |
| `GcPolicy` / `GcReport` | LRU-by-access eviction with pins, min-age and a hard byte ceiling |

## Layout

```text
/var/lib/avm/models/
  blobs/sha256/<aa>/<full-hex>   immutable weights (bind-mounted read-only into model servers)
  meta/<full-hex>.json           ModelRef + pulled_at + last_access + verified
  tmp/<hex>.part                 staging; renamed into blobs/ only after the digest matches
```

## Features

`oci-registry` pulls in `oci-client` (TLS + HTTP). It is **off by default** so
`cargo test --workspace` stays hermetic:

```bash
cargo test  -p avm-models                          # hermetic, no network
cargo check -p avm-models --features oci-registry  # real registry client
```

Without the feature, `OciArtifactClient::pull_blob` returns
`ModelError::RegistryFeatureDisabled` — call sites compile identically either way.
