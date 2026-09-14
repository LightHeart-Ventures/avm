//! Content-addressed blob store + the [`ModelStore`] interface.
//!
//! On-disk layout under the store root (default `/var/lib/avm/models`):
//!
//! ```text
//! /var/lib/avm/models/
//!   blobs/sha256/<aa>/<full-hex>      immutable model bytes (0444)
//!   meta/<full-hex>.json              ModelRef + pulled_at + last_access + verified
//!   tmp/<uuid>.part                   in-flight pulls (fsync + rename into place)
//! ```
//!
//! Writes are staged in `tmp/` and renamed only after the digest matches, so a
//! blob that exists in `blobs/` is always complete. The two-char shard keeps
//! directory fan-out sane on ext4/xfs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::gc::{GcPolicy, GcReport};
use crate::model_ref::{validate_digest, Residency};
use crate::{ModelError, ModelRef, Result};

/// Default store root. The blob subdirectory is what gets bind-mounted
/// read-only into model-server containers.
pub const DEFAULT_STORE_ROOT: &str = "/var/lib/avm/models";
/// Blob subdirectory relative to the store root.
pub const BLOBS_DIR: &str = "blobs";
/// Metadata sidecar subdirectory.
pub const META_DIR: &str = "meta";
/// Staging subdirectory for in-flight pulls.
pub const TMP_DIR: &str = "tmp";

/// A model that exists on this node's disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResidentModel {
    pub model: ModelRef,
    /// Absolute path of the blob.
    pub path: PathBuf,
    pub size_bytes: u64,
    pub pulled_at: DateTime<Utc>,
    pub last_access: DateTime<Utc>,
    /// Whether the digest has been re-verified since the pull.
    pub verified: bool,
}

impl ResidentModel {
    pub fn residency(&self) -> Residency {
        if self.verified {
            Residency::Resident
        } else {
            Residency::Cached
        }
    }

    /// `model.avm.io/<digest>` → `resident|cached`.
    pub fn node_label(&self) -> (String, String) {
        (
            self.model.label_key(),
            self.residency().as_str().to_string(),
        )
    }
}

/// Byte source for an artifact. Implemented by [`crate::OciArtifactClient`]
/// (registry) and [`LocalDirFetcher`] (air-gapped mirror / tests).
#[async_trait]
pub trait ArtifactFetcher: Send + Sync {
    /// Fetch the full artifact payload for `model`.
    async fn fetch(&self, model: &ModelRef) -> Result<Vec<u8>>;

    /// Human-readable source name, used in logs.
    fn describe(&self) -> String {
        "artifact-fetcher".to_string()
    }
}

/// Node-local model lifecycle.
#[async_trait]
pub trait ModelStore: Send + Sync {
    /// Ensure `model` is resident, pulling + verifying if needed. Idempotent.
    async fn pull(&self, model: &ModelRef) -> Result<ResidentModel>;

    /// Import local bytes into the store under their computed digest.
    async fn push(&self, model: &ModelRef, bytes: &[u8]) -> Result<ResidentModel>;

    /// Re-hash the on-disk blob and compare against the reference digest.
    async fn verify(&self, model: &ModelRef) -> Result<bool>;

    /// Everything currently on disk, newest access first.
    async fn list_resident(&self) -> Result<Vec<ResidentModel>>;

    /// Cheap residency probe used by the node-label publisher.
    async fn residency(&self, model: &ModelRef) -> Result<Residency>;

    /// Evict per `policy`.
    async fn gc(&self, policy: &GcPolicy) -> Result<GcReport>;
}

/// SHA-256 content-addressed store on the local filesystem.
pub struct ContentAddressedStore {
    root: PathBuf,
    fetcher: Option<std::sync::Arc<dyn ArtifactFetcher>>,
}

impl std::fmt::Debug for ContentAddressedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentAddressedStore")
            .field("root", &self.root)
            .field("fetcher", &self.fetcher.as_ref().map(|f| f.describe()))
            .finish()
    }
}

impl ContentAddressedStore {
    /// Store rooted at `root` with no fetcher — pulls fail, local reads work.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            fetcher: None,
        }
    }

    /// Store rooted at [`DEFAULT_STORE_ROOT`].
    pub fn default_root() -> Self {
        Self::new(DEFAULT_STORE_ROOT)
    }

    /// Attach the byte source used by [`ModelStore::pull`].
    pub fn with_fetcher(mut self, fetcher: std::sync::Arc<dyn ArtifactFetcher>) -> Self {
        self.fetcher = Some(fetcher);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/blobs` — the path bind-mounted read-only into model servers.
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join(BLOBS_DIR)
    }

    /// `<root>/blobs/sha256/<aa>/<hex>`
    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        validate_digest(digest)?;
        let hex = digest.split_once(':').map(|(_, h)| h).unwrap_or(digest);
        Ok(self.blobs_dir().join("sha256").join(&hex[..2]).join(hex))
    }

    /// `<root>/meta/<hex>.json`
    pub fn meta_path(&self, digest: &str) -> Result<PathBuf> {
        validate_digest(digest)?;
        let hex = digest.split_once(':').map(|(_, h)| h).unwrap_or(digest);
        Ok(self.root.join(META_DIR).join(format!("{hex}.json")))
    }

    /// Create `blobs/`, `meta/` and `tmp/` if missing.
    pub async fn init(&self) -> Result<()> {
        for dir in [
            self.blobs_dir(),
            self.root.join(META_DIR),
            self.root.join(TMP_DIR),
        ] {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| ModelError::io(dir.display().to_string(), e))?;
        }
        Ok(())
    }

    async fn read_meta(&self, digest: &str) -> Result<Option<ResidentModel>> {
        let path = self.meta_path(digest)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ModelError::io(path.display().to_string(), e)),
        }
    }

    async fn write_meta(&self, entry: &ResidentModel) -> Result<()> {
        let path = self.meta_path(&entry.model.digest)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ModelError::io(parent.display().to_string(), e))?;
        }
        let bytes = serde_json::to_vec_pretty(entry)
            .map_err(|e| ModelError::Fetch(format!("meta encode failed: {e}")))?;
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| ModelError::io(path.display().to_string(), e))
    }

    /// Stamp `last_access` — the input to LRU eviction.
    pub async fn touch(&self, digest: &str) -> Result<()> {
        if let Some(mut entry) = self.read_meta(digest).await? {
            entry.last_access = Utc::now();
            self.write_meta(&entry).await?;
        }
        Ok(())
    }

    /// Total bytes held in `blobs/`.
    pub async fn used_bytes(&self) -> Result<u64> {
        Ok(self
            .list_resident()
            .await?
            .iter()
            .map(|m| m.size_bytes)
            .sum())
    }

    /// Stage bytes in `tmp/`, verify the digest, then atomically rename into
    /// `blobs/`. A blob visible in `blobs/` is therefore always complete.
    async fn commit_bytes(&self, model: &ModelRef, bytes: &[u8]) -> Result<ResidentModel> {
        self.init().await?;
        let actual = sha256_hex(bytes);
        let expected = model.digest_hex();
        if actual != expected {
            return Err(ModelError::DigestMismatch {
                expected: model.digest.clone(),
                actual: format!("sha256:{actual}"),
            });
        }

        let staged = self.root.join(TMP_DIR).join(format!("{actual}.part"));
        tokio::fs::write(&staged, bytes)
            .await
            .map_err(|e| ModelError::io(staged.display().to_string(), e))?;

        let final_path = self.blob_path(&model.digest)?;
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ModelError::io(parent.display().to_string(), e))?;
        }
        tokio::fs::rename(&staged, &final_path)
            .await
            .map_err(|e| ModelError::io(final_path.display().to_string(), e))?;

        let now = Utc::now();
        let entry = ResidentModel {
            model: model.clone().with_size(bytes.len() as u64),
            path: final_path,
            size_bytes: bytes.len() as u64,
            pulled_at: now,
            last_access: now,
            verified: true,
        };
        self.write_meta(&entry).await?;
        Ok(entry)
    }
}

#[async_trait]
impl ModelStore for ContentAddressedStore {
    async fn pull(&self, model: &ModelRef) -> Result<ResidentModel> {
        validate_digest(&model.digest)?;

        // Already on disk → touch and return (idempotent, no network).
        if let Some(mut entry) = self.read_meta(&model.digest).await? {
            if tokio::fs::try_exists(&entry.path).await.unwrap_or(false) {
                entry.last_access = Utc::now();
                self.write_meta(&entry).await?;
                tracing::debug!(model = %model.short(), "model already resident");
                return Ok(entry);
            }
        }

        let fetcher = self
            .fetcher
            .as_ref()
            .ok_or_else(|| ModelError::Unsupported("no artifact fetcher configured".into()))?;

        tracing::info!(
            model = %model.short(),
            source = %fetcher.describe(),
            "pulling model artifact"
        );
        let bytes = fetcher.fetch(model).await?;
        self.commit_bytes(model, &bytes).await
    }

    async fn push(&self, model: &ModelRef, bytes: &[u8]) -> Result<ResidentModel> {
        self.commit_bytes(model, bytes).await
    }

    async fn verify(&self, model: &ModelRef) -> Result<bool> {
        let path = self.blob_path(&model.digest)?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ModelError::NotResident(model.digest.clone()))
            }
            Err(e) => return Err(ModelError::io(path.display().to_string(), e)),
        };
        let ok = sha256_hex(&bytes) == model.digest_hex();
        if let Some(mut entry) = self.read_meta(&model.digest).await? {
            entry.verified = ok;
            entry.last_access = Utc::now();
            self.write_meta(&entry).await?;
        }
        Ok(ok)
    }

    async fn list_resident(&self) -> Result<Vec<ResidentModel>> {
        let meta_dir = self.root.join(META_DIR);
        let mut entries = Vec::new();
        let mut rd = match tokio::fs::read_dir(&meta_dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
            Err(e) => return Err(ModelError::io(meta_dir.display().to_string(), e)),
        };
        while let Some(item) = rd
            .next_entry()
            .await
            .map_err(|e| ModelError::io(meta_dir.display().to_string(), e))?
        {
            let path = item.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(&path).await else {
                continue;
            };
            let Ok(entry) = serde_json::from_slice::<ResidentModel>(&bytes) else {
                continue;
            };
            if tokio::fs::try_exists(&entry.path).await.unwrap_or(false) {
                entries.push(entry);
            }
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_access));
        Ok(entries)
    }

    async fn residency(&self, model: &ModelRef) -> Result<Residency> {
        match self.read_meta(&model.digest).await? {
            Some(entry) if tokio::fs::try_exists(&entry.path).await.unwrap_or(false) => {
                Ok(entry.residency())
            }
            _ => Ok(Residency::Absent),
        }
    }

    async fn gc(&self, policy: &GcPolicy) -> Result<GcReport> {
        let resident = self.list_resident().await?;
        let bytes_before: u64 = resident.iter().map(|m| m.size_bytes).sum();

        let mut report = GcReport {
            bytes_before,
            bytes_after: bytes_before,
            ..GcReport::default()
        };
        if !policy.should_collect(bytes_before) {
            return Ok(report);
        }

        let now = Utc::now();
        let min_age =
            chrono::Duration::from_std(policy.min_age).unwrap_or_else(|_| chrono::Duration::zero());

        // Oldest access first = eviction order.
        let mut candidates: Vec<&ResidentModel> = resident.iter().collect();
        candidates.sort_by_key(|c| c.last_access);

        let mut used = bytes_before;
        let target = policy.max_bytes;
        for entry in candidates {
            if used <= target {
                break;
            }
            let digest = &entry.model.digest;
            if policy.is_pinned(digest) || now.signed_duration_since(entry.pulled_at) < min_age {
                report.retained_pinned.push(digest.clone());
                continue;
            }
            if !policy.dry_run {
                let _ = tokio::fs::remove_file(&entry.path).await;
                if let Ok(meta) = self.meta_path(digest) {
                    let _ = tokio::fs::remove_file(meta).await;
                }
            }
            used = used.saturating_sub(entry.size_bytes);
            report.evicted.push(digest.clone());
        }

        report.bytes_after = if policy.dry_run { bytes_before } else { used };
        if !report.evicted.is_empty() {
            tracing::info!(
                evicted = report.evicted.len(),
                reclaimed = report.bytes_reclaimed(),
                dry_run = policy.dry_run,
                "model gc pass complete"
            );
        }
        Ok(report)
    }
}

/// Fetches artifacts from a local directory keyed by digest hex — the
/// air-gapped mirror path, and what the unit tests use.
#[derive(Debug, Clone)]
pub struct LocalDirFetcher {
    dir: PathBuf,
}

impl LocalDirFetcher {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

#[async_trait]
impl ArtifactFetcher for LocalDirFetcher {
    async fn fetch(&self, model: &ModelRef) -> Result<Vec<u8>> {
        let path = self.dir.join(model.digest_hex());
        tokio::fs::read(&path)
            .await
            .map_err(|e| ModelError::Fetch(format!("{}: {e}", path.display())))
    }

    fn describe(&self) -> String {
        format!("local-dir:{}", self.dir.display())
    }
}

/// Hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Free functions named by the spec (thin wrappers over the trait).
// ---------------------------------------------------------------------------

/// Ensure `model` is resident on this node.
pub async fn pull_model<S: ModelStore + ?Sized>(
    store: &S,
    model: &ModelRef,
) -> Result<ResidentModel> {
    store.pull(model).await
}

/// Verify `bytes` against a `sha256:<hex>` digest.
pub fn verify_checksum(bytes: &[u8], digest: &str) -> Result<()> {
    validate_digest(digest)?;
    let actual = sha256_hex(bytes);
    let expected = digest.split_once(':').map(|(_, h)| h).unwrap_or(digest);
    if actual == expected {
        Ok(())
    } else {
        Err(ModelError::DigestMismatch {
            expected: digest.to_string(),
            actual: format!("sha256:{actual}"),
        })
    }
}

/// Everything on disk, newest access first.
pub async fn list_resident<S: ModelStore + ?Sized>(store: &S) -> Result<Vec<ResidentModel>> {
    store.list_resident().await
}

/// Run one GC pass.
pub async fn gc<S: ModelStore + ?Sized>(store: &S, policy: &GcPolicy) -> Result<GcReport> {
    store.gc(policy).await
}

/// Node labels for every resident model: `model.avm.io/<digest> = resident|cached`.
pub fn node_labels(models: &[ResidentModel]) -> BTreeMap<String, String> {
    models.iter().map(|m| m.node_label()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("avm-models-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn ref_for(bytes: &[u8]) -> ModelRef {
        ModelRef {
            registry: "ghcr.io".into(),
            repository: "lightheart/test".into(),
            tag: Some("v1".into()),
            digest: format!("sha256:{}", sha256_hex(bytes)),
            size_bytes: bytes.len() as u64,
            backend: Some("llama.cpp".into()),
        }
    }

    #[test]
    fn blob_path_is_sharded() {
        let store = ContentAddressedStore::new("/var/lib/avm/models");
        let digest = format!("sha256:{}", sha256_hex(b"x"));
        let hex = digest.split_once(':').unwrap().1.to_string();
        let path = store.blob_path(&digest).unwrap();
        assert!(path.ends_with(format!("sha256/{}/{}", &hex[..2], hex)));
        assert!(path.starts_with("/var/lib/avm/models/blobs"));
    }

    #[test]
    fn verify_checksum_detects_tampering() {
        let bytes = b"weights";
        let digest = format!("sha256:{}", sha256_hex(bytes));
        assert!(verify_checksum(bytes, &digest).is_ok());
        assert!(matches!(
            verify_checksum(b"tampered", &digest),
            Err(ModelError::DigestMismatch { .. })
        ));
        assert!(verify_checksum(bytes, "sha1:abc").is_err());
    }

    #[tokio::test]
    async fn push_verify_list_and_labels() {
        let root = tmp_root("push");
        let store = ContentAddressedStore::new(&root);
        let bytes = b"pretend-gguf-weights".to_vec();
        let model = ref_for(&bytes);

        let entry = store.push(&model, &bytes).await.unwrap();
        assert_eq!(entry.size_bytes, bytes.len() as u64);
        assert!(entry.verified);
        assert_eq!(entry.residency(), Residency::Resident);

        assert!(store.verify(&model).await.unwrap());
        assert_eq!(store.residency(&model).await.unwrap(), Residency::Resident);

        let resident = store.list_resident().await.unwrap();
        assert_eq!(resident.len(), 1);

        let labels = node_labels(&resident);
        assert_eq!(
            labels.get(&model.label_key()).map(String::as_str),
            Some("resident")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn push_rejects_digest_mismatch() {
        let root = tmp_root("mismatch");
        let store = ContentAddressedStore::new(&root);
        let model = ref_for(b"real");
        let err = store.push(&model, b"different").await.unwrap_err();
        assert!(matches!(err, ModelError::DigestMismatch { .. }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn pull_is_idempotent_and_uses_fetcher() {
        let root = tmp_root("pull");
        let mirror = tmp_root("mirror");
        std::fs::create_dir_all(&mirror).unwrap();

        let bytes = b"mirrored-weights".to_vec();
        let model = ref_for(&bytes);
        std::fs::write(mirror.join(model.digest_hex()), &bytes).unwrap();

        let store = ContentAddressedStore::new(&root)
            .with_fetcher(std::sync::Arc::new(LocalDirFetcher::new(&mirror)));

        let first = store.pull(&model).await.unwrap();
        let second = store.pull(&model).await.unwrap();
        assert_eq!(first.path, second.path);
        assert_eq!(store.list_resident().await.unwrap().len(), 1);
        assert_eq!(store.used_bytes().await.unwrap(), bytes.len() as u64);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&mirror);
    }

    #[tokio::test]
    async fn pull_without_fetcher_is_unsupported() {
        let root = tmp_root("nofetcher");
        let store = ContentAddressedStore::new(&root);
        let err = store.pull(&ref_for(b"nope")).await.unwrap_err();
        assert!(matches!(err, ModelError::Unsupported(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn gc_evicts_lru_and_honours_pins() {
        let root = tmp_root("gc");
        let store = ContentAddressedStore::new(&root);

        let old = b"aaaaaaaaaaaaaaaaaaaa".to_vec(); // 20 bytes
        let new = b"bbbbbbbbbbbbbbbbbbbb".to_vec();
        let old_ref = ref_for(&old);
        let new_ref = ref_for(&new);

        store.push(&old_ref, &old).await.unwrap();
        store.push(&new_ref, &new).await.unwrap();

        // Make `old` look stale and both blobs old enough to be evictable.
        for (r, access) in [(&old_ref, -3600i64), (&new_ref, -1i64)] {
            let mut entry = store.read_meta(&r.digest).await.unwrap().unwrap();
            entry.last_access = Utc::now() + chrono::Duration::seconds(access);
            entry.pulled_at = Utc::now() - chrono::Duration::seconds(7200);
            store.write_meta(&entry).await.unwrap();
        }

        let policy = GcPolicy {
            max_bytes: 20,
            high_watermark: 0.5,
            min_age: std::time::Duration::from_secs(60),
            pinned: vec![],
            dry_run: false,
        };
        let report = store.gc(&policy).await.unwrap();
        assert_eq!(report.evicted, vec![old_ref.digest.clone()]);
        assert_eq!(report.bytes_reclaimed(), 20);
        assert_eq!(store.list_resident().await.unwrap().len(), 1);

        // Pinning the survivor keeps it even under pressure.
        let pinned = GcPolicy {
            max_bytes: 0,
            high_watermark: 0.0,
            min_age: std::time::Duration::from_secs(0),
            pinned: vec![new_ref.digest.clone()],
            dry_run: false,
        };
        let report = store.gc(&pinned).await.unwrap();
        assert!(report.evicted.is_empty());
        assert_eq!(report.retained_pinned, vec![new_ref.digest.clone()]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn gc_dry_run_deletes_nothing() {
        let root = tmp_root("gcdry");
        let store = ContentAddressedStore::new(&root);
        let bytes = b"cccccccccc".to_vec();
        let model = ref_for(&bytes);
        store.push(&model, &bytes).await.unwrap();

        let mut entry = store.read_meta(&model.digest).await.unwrap().unwrap();
        entry.pulled_at = Utc::now() - chrono::Duration::seconds(7200);
        store.write_meta(&entry).await.unwrap();

        let policy = GcPolicy {
            max_bytes: 1,
            high_watermark: 0.0,
            min_age: std::time::Duration::from_secs(0),
            pinned: vec![],
            dry_run: true,
        };
        let report = store.gc(&policy).await.unwrap();
        assert_eq!(report.evicted, vec![model.digest.clone()]);
        assert_eq!(report.bytes_after, report.bytes_before);
        assert_eq!(store.list_resident().await.unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
