//! Digest-pinned model references.
//!
//! Canonical URI form:
//!
//! ```text
//! oci://<registry>/<repository>[:<tag>]@sha256:<64 hex>
//! ```
//!
//! The tag is advisory (human breadcrumb); the **digest is authoritative**.
//! Two refs with the same digest are the same bytes regardless of registry,
//! which is what makes the node-local cache safely shareable across tenants.

use serde::{Deserialize, Serialize};

use crate::{ModelError, Result};

/// URI scheme for model artifacts.
pub const MODEL_URI_SCHEME: &str = "oci://";
/// Only SHA-256 content addressing is supported today.
pub const DIGEST_ALGO: &str = "sha256";
/// Node-label namespace used to advertise residency to the scheduler.
pub const LABEL_PREFIX: &str = "model.avm.io/";

/// Where a model sits relative to a node.
///
/// * `Resident` — blob is on local disk **and** verified against its digest.
/// * `Cached`   — blob is on local disk but unverified (e.g. partial pull, or
///   verification deferred to keep the hot path cheap).
/// * `Absent`   — the node would have to pull it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Residency {
    Absent,
    Cached,
    Resident,
}

impl Residency {
    /// Lowercase wire form used in node labels and in Postgres.
    pub fn as_str(&self) -> &'static str {
        match self {
            Residency::Resident => "resident",
            Residency::Cached => "cached",
            Residency::Absent => "absent",
        }
    }

    /// Parse the wire form; unknown values degrade to [`Residency::Absent`].
    pub fn parse(s: &str) -> Residency {
        match s {
            "resident" => Residency::Resident,
            "cached" => Residency::Cached,
            _ => Residency::Absent,
        }
    }

    /// Placement weight — higher is better. Ordering is what the scheduler
    /// actually cares about: `resident > cached > absent`.
    pub fn score(&self) -> i32 {
        match self {
            Residency::Resident => 100,
            Residency::Cached => 40,
            Residency::Absent => 0,
        }
    }
}

impl std::fmt::Display for Residency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A digest-pinned reference to a model artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// Registry host, e.g. `ghcr.io`.
    pub registry: String,
    /// Repository path, e.g. `lightheart/qwen3-8b`.
    pub repository: String,
    /// Advisory tag, e.g. `q4_k_m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Authoritative content digest, `sha256:<64 hex>`.
    pub digest: String,
    /// Artifact size in bytes; `0` when not yet known (pre-pull).
    #[serde(default)]
    pub size_bytes: u64,
    /// Backend hint the scheduler uses to pick a model server image,
    /// e.g. `llama.cpp`, `vllm`, `tgi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
}

impl ModelRef {
    /// Parse a canonical `oci://…@sha256:…` reference.
    pub fn parse(uri: &str) -> Result<Self> {
        let uri = uri.trim();
        let rest = uri.strip_prefix(MODEL_URI_SCHEME).ok_or_else(|| {
            ModelError::InvalidRef(format!("missing `{MODEL_URI_SCHEME}` scheme: {uri}"))
        })?;

        let (locator, digest) = rest
            .rsplit_once('@')
            .ok_or_else(|| ModelError::InvalidRef(format!("missing `@sha256:` digest: {uri}")))?;
        validate_digest(digest)?;

        let (registry, repo_and_tag) = locator
            .split_once('/')
            .ok_or_else(|| ModelError::InvalidRef(format!("missing repository path: {uri}")))?;
        if registry.is_empty() {
            return Err(ModelError::InvalidRef(format!("empty registry: {uri}")));
        }

        // A ':' after the last '/' is a tag; a ':' inside the host is a port.
        let (repository, tag) = match repo_and_tag.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => (repo, Some(tag.to_string())),
            _ => (repo_and_tag, None),
        };
        if repository.is_empty() {
            return Err(ModelError::InvalidRef(format!("empty repository: {uri}")));
        }

        Ok(ModelRef {
            registry: registry.to_string(),
            repository: repository.to_string(),
            tag,
            digest: digest.to_string(),
            size_bytes: 0,
            backend: None,
        })
    }

    /// Build a digest-only reference (used for cache lookups where the origin
    /// registry is irrelevant).
    pub fn from_digest(digest: impl Into<String>) -> Result<Self> {
        let digest = digest.into();
        validate_digest(&digest)?;
        Ok(ModelRef {
            registry: String::new(),
            repository: String::new(),
            tag: None,
            digest,
            size_bytes: 0,
            backend: None,
        })
    }

    /// Render back to the canonical URI.
    pub fn to_uri(&self) -> String {
        let tag = self
            .tag
            .as_deref()
            .map(|t| format!(":{t}"))
            .unwrap_or_default();
        format!(
            "{MODEL_URI_SCHEME}{}/{}{}@{}",
            self.registry, self.repository, tag, self.digest
        )
    }

    /// `<registry>/<repo>@<digest>` — the form an OCI client expects.
    pub fn oci_reference(&self) -> String {
        format!("{}/{}@{}", self.registry, self.repository, self.digest)
    }

    /// Hex portion of the digest (no `sha256:` prefix).
    pub fn digest_hex(&self) -> &str {
        self.digest
            .split_once(':')
            .map(|(_, hex)| hex)
            .unwrap_or(&self.digest)
    }

    /// First 12 hex chars — the human-facing short id.
    pub fn short(&self) -> &str {
        let hex = self.digest_hex();
        &hex[..hex.len().min(12)]
    }

    /// Node-label key advertising this model: `model.avm.io/sha256:<hex>`.
    pub fn label_key(&self) -> String {
        format!("{LABEL_PREFIX}{}", self.digest)
    }

    /// Attach a backend hint (builder style).
    pub fn with_backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = Some(backend.into());
        self
    }

    /// Attach a known size (builder style).
    pub fn with_size(mut self, size_bytes: u64) -> Self {
        self.size_bytes = size_bytes;
        self
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_uri())
    }
}

impl std::str::FromStr for ModelRef {
    type Err = ModelError;
    fn from_str(s: &str) -> Result<Self> {
        ModelRef::parse(s)
    }
}

/// Reject anything that is not `sha256:<64 lowercase hex>`.
pub fn validate_digest(digest: &str) -> Result<()> {
    let (algo, hex) = digest
        .split_once(':')
        .ok_or_else(|| ModelError::InvalidRef(format!("digest must be `algo:hex`: {digest}")))?;
    if algo != DIGEST_ALGO {
        return Err(ModelError::InvalidRef(format!(
            "unsupported digest algorithm: {algo}"
        )));
    }
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(ModelError::InvalidRef(format!(
            "digest must be 64 lowercase hex chars: {digest}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn parses_full_reference() {
        let r = ModelRef::parse(&format!("oci://ghcr.io/lightheart/qwen3-8b:q4_k_m@{D}")).unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "lightheart/qwen3-8b");
        assert_eq!(r.tag.as_deref(), Some("q4_k_m"));
        assert_eq!(r.digest, D);
        assert_eq!(r.short(), "e3b0c44298fc");
        assert_eq!(r.label_key(), format!("model.avm.io/{D}"));
    }

    #[test]
    fn parses_untagged_and_ported_registry() {
        let r = ModelRef::parse(&format!("oci://registry.local:5000/models/phi4@{D}")).unwrap();
        assert_eq!(r.registry, "registry.local:5000");
        assert_eq!(r.repository, "models/phi4");
        assert_eq!(r.tag, None);
    }

    #[test]
    fn roundtrips_through_uri() {
        let uri = format!("oci://ghcr.io/lightheart/qwen3-8b:q4_k_m@{D}");
        assert_eq!(ModelRef::parse(&uri).unwrap().to_uri(), uri);
    }

    #[test]
    fn rejects_unpinned_or_malformed_refs() {
        assert!(ModelRef::parse("ghcr.io/foo/bar@sha256:deadbeef").is_err()); // no scheme
        assert!(ModelRef::parse("oci://ghcr.io/foo/bar:latest").is_err()); // no digest
        assert!(ModelRef::parse("oci://ghcr.io/foo/bar@md5:abc").is_err()); // wrong algo
        assert!(ModelRef::parse(&format!("oci://ghcr.io@{D}")).is_err()); // no repo
        assert!(ModelRef::parse("oci://ghcr.io/foo/bar@sha256:ABC").is_err()); // short/upper
    }

    #[test]
    fn residency_orders_resident_first() {
        assert!(Residency::Resident > Residency::Cached);
        assert!(Residency::Cached > Residency::Absent);
        assert_eq!(Residency::parse("resident"), Residency::Resident);
        assert_eq!(Residency::parse("nonsense"), Residency::Absent);
        assert!(Residency::Resident.score() > Residency::Cached.score());
    }

    #[test]
    fn oci_reference_drops_tag() {
        let r = ModelRef::parse(&format!("oci://ghcr.io/lightheart/q:t@{D}")).unwrap();
        assert_eq!(r.oci_reference(), format!("ghcr.io/lightheart/q@{D}"));
    }
}
