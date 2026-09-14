//! Garbage collection policy for the node-local blob cache.
//!
//! Strategy: **LRU by last access, bounded by a hard byte ceiling**, with two
//! escape hatches that keep a busy node from evicting live weights:
//!
//! * `pinned` — digests currently served by a running model server; never evicted.
//! * `min_age` — a blob younger than this is never evicted, so a pull that is
//!   racing a placement decision cannot be reaped before first use.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Tunables for [`crate::store::ContentAddressedStore::gc`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcPolicy {
    /// Hard ceiling for the blob directory. Eviction runs until the store is
    /// at or below this size.
    pub max_bytes: u64,
    /// Start evicting once usage crosses this fraction of `max_bytes`.
    pub high_watermark: f64,
    /// Blobs newer than this are never evicted.
    pub min_age: Duration,
    /// Digests that must never be evicted (in-use weights).
    pub pinned: Vec<String>,
    /// Report what would be evicted without deleting anything.
    pub dry_run: bool,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            max_bytes: 200 * 1024 * 1024 * 1024, // 200 GiB
            high_watermark: 0.85,
            min_age: Duration::from_secs(900),
            pinned: Vec::new(),
            dry_run: false,
        }
    }
}

impl GcPolicy {
    /// Byte level at which eviction kicks in.
    pub fn trigger_bytes(&self) -> u64 {
        let hw = self.high_watermark.clamp(0.0, 1.0);
        (self.max_bytes as f64 * hw) as u64
    }

    /// True when `used` has crossed the high watermark.
    pub fn should_collect(&self, used: u64) -> bool {
        used > self.trigger_bytes()
    }

    pub fn is_pinned(&self, digest: &str) -> bool {
        self.pinned.iter().any(|d| d == digest)
    }
}

/// Outcome of one GC pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    /// Bytes in the store before the pass.
    pub bytes_before: u64,
    /// Bytes in the store after the pass (equals `bytes_before` on a dry run).
    pub bytes_after: u64,
    /// Digests evicted (or that *would* be evicted on a dry run).
    pub evicted: Vec<String>,
    /// Digests skipped because they were pinned or too young.
    pub retained_pinned: Vec<String>,
}

impl GcReport {
    pub fn bytes_reclaimed(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_gates_collection() {
        let p = GcPolicy {
            max_bytes: 1000,
            high_watermark: 0.8,
            ..GcPolicy::default()
        };
        assert_eq!(p.trigger_bytes(), 800);
        assert!(!p.should_collect(799));
        assert!(p.should_collect(801));
    }

    #[test]
    fn pinned_digests_are_recognised() {
        let p = GcPolicy {
            pinned: vec!["sha256:aa".into()],
            ..GcPolicy::default()
        };
        assert!(p.is_pinned("sha256:aa"));
        assert!(!p.is_pinned("sha256:bb"));
    }

    #[test]
    fn report_computes_reclaimed_bytes() {
        let r = GcReport {
            bytes_before: 100,
            bytes_after: 40,
            ..GcReport::default()
        };
        assert_eq!(r.bytes_reclaimed(), 60);
    }
}
