//! Model distribution for AVM.
//!
//! Models are shipped as **OCI artifacts** and cached on every node in a
//! content-addressed store rooted at `/var/lib/avm/models/blobs`. Nothing in
//! this crate talks to Postgres or NATS — it is the node-local data path:
//!
//! ```text
//!   oci://ghcr.io/lightheart/qwen3-8b:q4_k_m@sha256:ab12…
//!            │
//!            ├─ [`ModelRef`]              parsed, digest-pinned reference
//!            ├─ [`ArtifactFetcher`]       byte source (OCI registry, local mirror)
//!            └─ [`ContentAddressedStore`] verify → blobs/sha256/ab/ab12… → resident
//! ```
//!
//! Residency is published back to the scheduler as node labels
//! (`model.avm.io/<digest>=resident|cached|absent`) so agent placement can
//! prefer nodes that already hold the weights.
//!
//! # Example
//!
//! ```
//! use avm_models::ModelRef;
//!
//! const URI: &str = "oci://ghcr.io/lightheart/qwen3-8b:q4_k_m@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
//!
//! let model = ModelRef::parse(URI).unwrap();
//! assert_eq!(model.registry, "ghcr.io");
//! assert_eq!(model.short(), "e3b0c44298fc");
//! assert_eq!(model.label_key(), format!("model.avm.io/{}", model.digest));
//!
//! // Unpinned references are rejected: the digest is the contract.
//! assert!(ModelRef::parse("oci://ghcr.io/lightheart/qwen3-8b:q4_k_m").is_err());
//! ```

#![forbid(unsafe_code)]

pub mod gc;
pub mod model_ref;
pub mod oci;
pub mod store;

pub use gc::{GcPolicy, GcReport};
pub use model_ref::{ModelRef, Residency, DIGEST_ALGO, LABEL_PREFIX, MODEL_URI_SCHEME};
pub use oci::OciArtifactClient;
pub use store::{
    gc, list_resident, pull_model, verify_checksum, ArtifactFetcher, ContentAddressedStore,
    LocalDirFetcher, ModelStore, ResidentModel, DEFAULT_STORE_ROOT,
};

/// Failure modes of the model data path.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("invalid model reference: {0}")]
    InvalidRef(String),
    #[error("digest mismatch: expected {expected}, computed {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("model {0} is not resident")]
    NotResident(String),
    #[error("artifact fetch failed: {0}")]
    Fetch(String),
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("operation unsupported: {0}")]
    Unsupported(String),
    #[error(
        "avm-models was built without the `oci-registry` feature; \
         rebuild with `--features oci-registry` to pull from {0}"
    )]
    RegistryFeatureDisabled(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, ModelError>;

impl ModelError {
    pub(crate) fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        ModelError::Io {
            path: path.into(),
            source,
        }
    }
}
