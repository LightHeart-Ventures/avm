//! Artifact kinds on top of the content-addressed store.
//!
//! The store started life as a *model* cache, but nothing in it is
//! model-specific: it verifies a digest, writes `blobs/sha256/<aa>/<hex>`,
//! stamps `last_access` and evicts LRU. Agent images are the same shape —
//! digest-pinned OCI artifacts that a node either holds or must pull — so they
//! share the blob directory, the residency labels and `gc()`.
//!
//! This module is **additive**: [`crate::ContentAddressedStore`],
//! [`crate::ModelRef`] and the `model.avm.io/<digest>` labels are untouched, so
//! the scheduler's residency-weighted placement scoring keeps working exactly
//! as before. What is new is a [`ArtifactKind`] discriminator and the
//! analogous `agent.avm.io/<digest>` label namespace.

use std::collections::BTreeMap;

use crate::model_ref::{validate_digest, Residency, LABEL_PREFIX};
use crate::store::ResidentModel;
use crate::{ModelRef, Result};

/// Node-label namespace advertising a resident **agent image**.
pub const AGENT_LABEL_PREFIX: &str = "agent.avm.io/";

/// A digest-pinned artifact reference. Same wire form for both kinds, so the
/// parser, the store and the GC are shared.
pub type ArtifactRef = ModelRef;

/// An artifact that exists on this node's disk.
pub type ResidentArtifact = ResidentModel;

/// The content-addressed store, named for what it actually holds.
pub type ArtifactStore = crate::store::ContentAddressedStore;

/// What a blob in the store *is*. Only the label namespace differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArtifactKind {
    /// Model weights — labelled `model.avm.io/<digest>`.
    Model,
    /// An agent container image — labelled `agent.avm.io/<digest>`.
    AgentImage,
}

impl ArtifactKind {
    /// Node-label namespace for this kind.
    pub fn label_prefix(&self) -> &'static str {
        match self {
            ArtifactKind::Model => LABEL_PREFIX,
            ArtifactKind::AgentImage => AGENT_LABEL_PREFIX,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Model => "model",
            ArtifactKind::AgentImage => "agent-image",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "model" => Some(ArtifactKind::Model),
            "agent-image" | "agent" => Some(ArtifactKind::AgentImage),
            _ => None,
        }
    }

    /// `<prefix><digest>` — the node-label key advertising this artifact.
    pub fn label_key(&self, digest: &str) -> Result<String> {
        validate_digest(digest)?;
        Ok(format!("{}{}", self.label_prefix(), digest))
    }
}

/// Split a node label into its kind and digest, or `None` if it is not an AVM
/// residency label.
pub fn parse_label_key(key: &str) -> Option<(ArtifactKind, &str)> {
    if let Some(digest) = key.strip_prefix(AGENT_LABEL_PREFIX) {
        return Some((ArtifactKind::AgentImage, digest));
    }
    key.strip_prefix(LABEL_PREFIX)
        .map(|digest| (ArtifactKind::Model, digest))
}

/// Residency label for one artifact, under the namespace of `kind`.
pub fn node_label_for(kind: ArtifactKind, artifact: &ResidentArtifact) -> (String, String) {
    (
        format!("{}{}", kind.label_prefix(), artifact.model.digest),
        artifact.residency().as_str().to_string(),
    )
}

/// Residency labels for every artifact of `kind` on this node.
pub fn node_labels_for(
    kind: ArtifactKind,
    artifacts: &[ResidentArtifact],
) -> BTreeMap<String, String> {
    artifacts.iter().map(|a| node_label_for(kind, a)).collect()
}

/// Residency of `digest` as reported by a label map, for either kind.
pub fn residency_from_labels(
    labels: &BTreeMap<String, String>,
    kind: ArtifactKind,
    digest: &str,
) -> Residency {
    labels
        .get(&format!("{}{}", kind.label_prefix(), digest))
        .map(|v| Residency::parse(v))
        .unwrap_or(Residency::Absent)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn model_labels_are_unchanged() {
        // The scheduler's existing contract must not move.
        assert_eq!(ArtifactKind::Model.label_prefix(), "model.avm.io/");
        assert_eq!(
            ArtifactKind::Model.label_key(D).unwrap(),
            format!("model.avm.io/{D}")
        );
        let m = ModelRef::from_digest(D).unwrap();
        assert_eq!(m.label_key(), ArtifactKind::Model.label_key(D).unwrap());
    }

    #[test]
    fn agent_images_get_their_own_namespace() {
        assert_eq!(ArtifactKind::AgentImage.label_prefix(), "agent.avm.io/");
        assert_eq!(
            ArtifactKind::AgentImage.label_key(D).unwrap(),
            format!("agent.avm.io/{D}")
        );
        assert!(ArtifactKind::AgentImage.label_key("sha256:nope").is_err());
    }

    #[test]
    fn labels_round_trip_through_the_parser() {
        assert_eq!(
            parse_label_key(&format!("model.avm.io/{D}")),
            Some((ArtifactKind::Model, D))
        );
        assert_eq!(
            parse_label_key(&format!("agent.avm.io/{D}")),
            Some((ArtifactKind::AgentImage, D))
        );
        assert_eq!(parse_label_key("kubernetes.io/hostname"), None);
        assert_eq!(ArtifactKind::parse("agent"), Some(ArtifactKind::AgentImage));
        assert_eq!(ArtifactKind::parse("nope"), None);
    }

    #[test]
    fn residency_lookup_is_kind_scoped() {
        let mut labels = BTreeMap::new();
        labels.insert(format!("model.avm.io/{D}"), "resident".to_string());
        assert_eq!(
            residency_from_labels(&labels, ArtifactKind::Model, D),
            Residency::Resident
        );
        // Same digest, different kind → absent. Namespaces do not bleed.
        assert_eq!(
            residency_from_labels(&labels, ArtifactKind::AgentImage, D),
            Residency::Absent
        );
    }
}
