//! Serde mirrors of the model-distribution messages in
//! `proto/avm_service.proto` (`Model`, `ModelPlacement`).
//!
//! Same rationale as [`crate::types`]: the control plane speaks gRPC, but the
//! queue and the node-label path ship JSON, and `protoc` is not a build
//! requirement for the data path.

use serde::{Deserialize, Serialize};

/// Wire mirror of `avm.v1.Model`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    /// `sha256:<64 hex>` — authoritative content digest.
    pub digest: String,
    /// Registry host the artifact came from, e.g. `ghcr.io`.
    #[serde(default)]
    pub registry: String,
    /// Artifact size in bytes (`0` when unknown).
    #[serde(default)]
    pub size_bytes: i64,
    /// Serving backend hint: `llama.cpp` | `vllm` | `tgi`.
    #[serde(default)]
    pub backend: String,
}

impl Model {
    /// Hex portion of the digest.
    pub fn digest_hex(&self) -> &str {
        self.digest
            .split_once(':')
            .map(|(_, h)| h)
            .unwrap_or(&self.digest)
    }

    /// Node-label key advertising this model.
    pub fn label_key(&self) -> String {
        format!("model.avm.io/{}", self.digest)
    }
}

/// Wire mirror of `avm.v1.ModelPlacement` — where a model lives right now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPlacement {
    pub node_id: String,
    /// `resident` | `cached` | `absent` | `pulling` | `failed`
    pub status: String,
}

impl ModelPlacement {
    pub fn new(node_id: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            status: status.into(),
        }
    }

    /// True when the node can serve the model without a pull.
    pub fn is_servable(&self) -> bool {
        matches!(self.status.as_str(), "resident" | "cached")
    }
}

/// Placement status constants (mirrors the SQL CHECK constraint on
/// `model_placements.status`).
pub mod placement_status {
    pub const RESIDENT: &str = "resident";
    pub const CACHED: &str = "cached";
    pub const ABSENT: &str = "absent";
    pub const PULLING: &str = "pulling";
    pub const FAILED: &str = "failed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_roundtrips_and_derives_label() {
        let m = Model {
            digest: "sha256:abc123".into(),
            registry: "ghcr.io".into(),
            size_bytes: 4_700_000_000,
            backend: "llama.cpp".into(),
        };
        let decoded: Model = serde_json::from_slice(&serde_json::to_vec(&m).unwrap()).unwrap();
        assert_eq!(decoded, m);
        assert_eq!(decoded.digest_hex(), "abc123");
        assert_eq!(decoded.label_key(), "model.avm.io/sha256:abc123");
    }

    #[test]
    fn placement_servability() {
        assert!(ModelPlacement::new("node-1", placement_status::RESIDENT).is_servable());
        assert!(ModelPlacement::new("node-1", placement_status::CACHED).is_servable());
        assert!(!ModelPlacement::new("node-1", placement_status::PULLING).is_servable());
        assert!(!ModelPlacement::new("node-1", placement_status::ABSENT).is_servable());
    }
}
