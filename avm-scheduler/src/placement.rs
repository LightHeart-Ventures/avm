//! Model-aware placement: node labels, scoring and affinity.
//!
//! Placement is a soft-constraint scorer, not a bin-packer. Every candidate
//! node is scored and the best feasible one wins:
//!
//! ```text
//! score = residency_weight · residency(node, model)     resident 100 / cached 40 / absent 0
//!       + affinity_weight  · model_server_live(node)    a server is already serving the digest
//!       + spread_weight    · free_slot_fraction(node)   keep the cluster from hot-spotting
//!       - pull_penalty     · would_have_to_pull(node)   cold pulls are measured in minutes
//! ```
//!
//! Residency is *soft*: a node with free capacity and no copy of the weights is
//! still feasible — it just loses to a node that already has them. Hard
//! constraints (`required_labels`, capacity) are the only way to make a node
//! infeasible.

use std::collections::BTreeMap;

use avm_models::model_ref::{Residency, LABEL_PREFIX};
use serde::{Deserialize, Serialize};

/// A single `key=value` node label.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeLabel {
    pub key: String,
    pub value: String,
}

impl NodeLabel {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    /// `model.avm.io/<digest> = resident|cached|absent`
    pub fn model(digest: &str, residency: Residency) -> Self {
        Self::new(format!("{LABEL_PREFIX}{digest}"), residency.as_str())
    }

    /// The digest this label advertises, if it is a model label.
    pub fn model_digest(&self) -> Option<&str> {
        self.key.strip_prefix(LABEL_PREFIX)
    }

    /// The residency this label advertises, if it is a model label.
    pub fn residency(&self) -> Option<Residency> {
        self.model_digest().map(|_| Residency::parse(&self.value))
    }
}

impl std::fmt::Display for NodeLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

/// A candidate node as the scheduler sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeState {
    pub node_id: String,
    /// All labels, including `model.avm.io/*` published by the executor.
    #[serde(default)]
    pub labels: Vec<NodeLabel>,
    /// Total agent slots.
    pub capacity_slots: u32,
    /// Slots currently in use.
    pub used_slots: u32,
    /// Digests with a **live model server** on this node.
    #[serde(default)]
    pub serving_digests: Vec<String>,
    /// Node is draining / cordoned — never place new work.
    #[serde(default)]
    pub cordoned: bool,
}

impl NodeState {
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            capacity_slots: 1,
            ..Default::default()
        }
    }

    pub fn with_capacity(mut self, capacity: u32, used: u32) -> Self {
        self.capacity_slots = capacity;
        self.used_slots = used;
        self
    }

    pub fn with_label(mut self, label: NodeLabel) -> Self {
        self.labels.push(label);
        self
    }

    pub fn with_model(mut self, digest: &str, residency: Residency) -> Self {
        self.labels.push(NodeLabel::model(digest, residency));
        self
    }

    /// Mark a live model server for `digest` (implies residency).
    pub fn serving(mut self, digest: impl Into<String>) -> Self {
        self.serving_digests.push(digest.into());
        self
    }

    pub fn free_slots(&self) -> u32 {
        self.capacity_slots.saturating_sub(self.used_slots)
    }

    /// Fraction of capacity still free, `0.0..=1.0`.
    pub fn free_fraction(&self) -> f64 {
        if self.capacity_slots == 0 {
            0.0
        } else {
            self.free_slots() as f64 / self.capacity_slots as f64
        }
    }

    /// Residency of `digest` on this node, from its labels.
    pub fn residency_of(&self, digest: &str) -> Residency {
        self.labels
            .iter()
            .find(|l| l.model_digest() == Some(digest))
            .map(|l| Residency::parse(&l.value))
            .unwrap_or(Residency::Absent)
    }

    /// True when a model server for `digest` is already live here.
    pub fn is_serving(&self, digest: &str) -> bool {
        self.serving_digests.iter().any(|d| d == digest)
    }

    pub fn label_map(&self) -> BTreeMap<&str, &str> {
        self.labels
            .iter()
            .map(|l| (l.key.as_str(), l.value.as_str()))
            .collect()
    }

    fn satisfies(&self, required: &NodeLabel) -> bool {
        self.labels
            .iter()
            .any(|l| l.key == required.key && l.value == required.value)
    }
}

/// What the scheduler is trying to place.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlacementRequest {
    pub agent_id: String,
    /// Digest of the model the agent needs (from its Agent Card `model_ref`).
    #[serde(default)]
    pub model_digest: Option<String>,
    /// Hard constraints — a node missing any of these is infeasible.
    #[serde(default)]
    pub required_labels: Vec<NodeLabel>,
    /// Slots this placement consumes.
    #[serde(default = "one")]
    pub slots: u32,
}

fn one() -> u32 {
    1
}

impl PlacementRequest {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            slots: 1,
            ..Default::default()
        }
    }

    /// Co-schedule with the model server holding `digest` (soft constraint).
    pub fn wanting_model(mut self, digest: impl Into<String>) -> Self {
        self.model_digest = Some(digest.into());
        self
    }

    pub fn requiring(mut self, label: NodeLabel) -> Self {
        self.required_labels.push(label);
        self
    }
}

/// Scoring weights. Defaults are tuned so residency dominates load spread but
/// never overrides a hard constraint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreWeights {
    pub residency: f64,
    /// Bonus when a model server for the digest is already live on the node.
    pub affinity: f64,
    /// Weight on free capacity — spreads load across equally-resident nodes.
    pub spread: f64,
    /// Penalty applied when the node would have to cold-pull the weights.
    pub pull_penalty: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            residency: 1.0,
            affinity: 50.0,
            spread: 20.0,
            pull_penalty: 25.0,
        }
    }
}

/// Per-node placement score with its components kept for observability —
/// `avm scheduler explain` prints these verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementScore {
    pub node_id: String,
    pub feasible: bool,
    /// Why the node was rejected, when `!feasible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub residency: Residency,
    pub residency_score: f64,
    pub affinity_score: f64,
    pub spread_score: f64,
    pub pull_penalty: f64,
    pub total: f64,
}

impl PlacementScore {
    fn infeasible(node_id: &str, reason: impl Into<String>) -> Self {
        Self {
            node_id: node_id.to_string(),
            feasible: false,
            reason: Some(reason.into()),
            residency: Residency::Absent,
            residency_score: 0.0,
            affinity_score: 0.0,
            spread_score: 0.0,
            pull_penalty: 0.0,
            total: f64::MIN,
        }
    }

    /// True when placing here requires a cold pull.
    pub fn requires_pull(&self) -> bool {
        self.feasible && self.residency == Residency::Absent
    }
}

/// Score one node for one request.
pub fn score_node(node: &NodeState, req: &PlacementRequest, w: &ScoreWeights) -> PlacementScore {
    if node.cordoned {
        return PlacementScore::infeasible(&node.node_id, "node cordoned");
    }
    if node.free_slots() < req.slots {
        return PlacementScore::infeasible(
            &node.node_id,
            format!(
                "insufficient capacity: {} free < {} requested",
                node.free_slots(),
                req.slots
            ),
        );
    }
    if let Some(missing) = req.required_labels.iter().find(|l| !node.satisfies(l)) {
        return PlacementScore::infeasible(
            &node.node_id,
            format!("missing required label {missing}"),
        );
    }

    let (residency, affinity_score, pull_penalty) = match req.model_digest.as_deref() {
        Some(digest) => {
            let residency = node.residency_of(digest);
            let affinity = if node.is_serving(digest) {
                w.affinity
            } else {
                0.0
            };
            let penalty = if residency == Residency::Absent {
                w.pull_penalty
            } else {
                0.0
            };
            (residency, affinity, penalty)
        }
        // No model requirement: residency is irrelevant, don't bias placement.
        None => (Residency::Absent, 0.0, 0.0),
    };

    let residency_score = match req.model_digest {
        Some(_) => residency.score() as f64 * w.residency,
        None => 0.0,
    };
    let spread_score = node.free_fraction() * w.spread;
    let total = residency_score + affinity_score + spread_score - pull_penalty;

    PlacementScore {
        node_id: node.node_id.clone(),
        feasible: true,
        reason: None,
        residency,
        residency_score,
        affinity_score,
        spread_score,
        pull_penalty,
        total,
    }
}

/// Score every node, best first. Infeasible nodes are included (with a reason)
/// so the scheduler can explain why a placement failed.
pub fn rank_nodes(
    nodes: &[NodeState],
    req: &PlacementRequest,
    w: &ScoreWeights,
) -> Vec<PlacementScore> {
    let mut scored: Vec<PlacementScore> = nodes.iter().map(|n| score_node(n, req, w)).collect();
    scored.sort_by(|a, b| {
        b.feasible
            .cmp(&a.feasible)
            .then(
                b.total
                    .partial_cmp(&a.total)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.node_id.cmp(&b.node_id))
    });
    scored
}

/// Best feasible node, or `None` when nothing fits.
pub fn select_node(
    nodes: &[NodeState],
    req: &PlacementRequest,
    w: &ScoreWeights,
) -> Option<PlacementScore> {
    rank_nodes(nodes, req, w).into_iter().find(|s| s.feasible)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn model_label_roundtrips() {
        let l = NodeLabel::model(D1, Residency::Resident);
        assert_eq!(l.key, format!("model.avm.io/{D1}"));
        assert_eq!(l.value, "resident");
        assert_eq!(l.model_digest(), Some(D1));
        assert_eq!(l.residency(), Some(Residency::Resident));
        assert_eq!(l.to_string(), format!("model.avm.io/{D1}=resident"));

        let other = NodeLabel::new("gpu.avm.io/kind", "a100");
        assert_eq!(other.model_digest(), None);
    }

    #[test]
    fn residency_beats_free_capacity() {
        let resident = NodeState::new("n-resident")
            .with_capacity(10, 9) // nearly full
            .with_model(D1, Residency::Resident);
        let empty = NodeState::new("n-empty").with_capacity(10, 0); // wide open, no weights

        let req = PlacementRequest::new("ag_x").wanting_model(D1);
        let w = ScoreWeights::default();
        let winner = select_node(&[empty.clone(), resident.clone()], &req, &w).unwrap();

        assert_eq!(winner.node_id, "n-resident");
        assert_eq!(winner.residency, Residency::Resident);
        assert!(!winner.requires_pull());
        assert!(score_node(&empty, &req, &w).requires_pull());
    }

    #[test]
    fn residency_order_is_resident_cached_absent() {
        let req = PlacementRequest::new("ag_x").wanting_model(D1);
        let w = ScoreWeights::default();
        let nodes = vec![
            NodeState::new("n-absent").with_capacity(4, 0),
            NodeState::new("n-cached")
                .with_capacity(4, 0)
                .with_model(D1, Residency::Cached),
            NodeState::new("n-resident")
                .with_capacity(4, 0)
                .with_model(D1, Residency::Resident),
        ];
        let ranked = rank_nodes(&nodes, &req, &w);
        let order: Vec<&str> = ranked.iter().map(|s| s.node_id.as_str()).collect();
        assert_eq!(order, vec!["n-resident", "n-cached", "n-absent"]);
    }

    #[test]
    fn affinity_prefers_a_live_model_server() {
        let req = PlacementRequest::new("ag_x").wanting_model(D1);
        let w = ScoreWeights::default();
        let serving = NodeState::new("n-serving")
            .with_capacity(4, 2)
            .with_model(D1, Residency::Resident)
            .serving(D1);
        let idle = NodeState::new("n-idle")
            .with_capacity(4, 2)
            .with_model(D1, Residency::Resident);

        let winner = select_node(&[idle, serving], &req, &w).unwrap();
        assert_eq!(winner.node_id, "n-serving");
        assert_eq!(winner.affinity_score, w.affinity);
    }

    #[test]
    fn spread_breaks_ties_between_equally_resident_nodes() {
        let req = PlacementRequest::new("ag_x").wanting_model(D1);
        let w = ScoreWeights::default();
        let busy = NodeState::new("n-busy")
            .with_capacity(10, 8)
            .with_model(D1, Residency::Resident);
        let free = NodeState::new("n-free")
            .with_capacity(10, 1)
            .with_model(D1, Residency::Resident);
        assert_eq!(
            select_node(&[busy, free], &req, &w).unwrap().node_id,
            "n-free"
        );
    }

    #[test]
    fn residency_is_soft_absent_nodes_stay_feasible() {
        let req = PlacementRequest::new("ag_x").wanting_model(D2);
        let w = ScoreWeights::default();
        let only = NodeState::new("n-only")
            .with_capacity(2, 0)
            .with_model(D1, Residency::Resident);
        let pick = select_node(&[only], &req, &w).unwrap();
        assert_eq!(pick.node_id, "n-only");
        assert!(pick.requires_pull());
    }

    #[test]
    fn hard_constraints_make_nodes_infeasible() {
        let w = ScoreWeights::default();
        let gpu = NodeLabel::new("gpu.avm.io/kind", "a100");
        let req = PlacementRequest::new("ag_x")
            .wanting_model(D1)
            .requiring(gpu.clone());

        let cpu_node = NodeState::new("n-cpu")
            .with_capacity(4, 0)
            .with_model(D1, Residency::Resident);
        assert!(select_node(&[cpu_node], &req, &w).is_none());

        let full = NodeState::new("n-full")
            .with_capacity(1, 1)
            .with_label(gpu.clone());
        let cordoned = {
            let mut n = NodeState::new("n-cordoned")
                .with_capacity(4, 0)
                .with_label(gpu);
            n.cordoned = true;
            n
        };
        let ranked = rank_nodes(&[full, cordoned], &req, &w);
        assert!(ranked.iter().all(|s| !s.feasible));
        assert!(ranked
            .iter()
            .any(|s| s.reason.as_deref() == Some("node cordoned")));
    }

    #[test]
    fn requests_without_a_model_ignore_residency() {
        let w = ScoreWeights::default();
        let req = PlacementRequest::new("ag_no_model");
        let a = NodeState::new("n-a")
            .with_capacity(4, 3)
            .with_model(D1, Residency::Resident);
        let b = NodeState::new("n-b").with_capacity(4, 0);
        let winner = select_node(&[a, b], &req, &w).unwrap();
        assert_eq!(winner.node_id, "n-b"); // pure spread
        assert_eq!(winner.residency_score, 0.0);
        assert_eq!(winner.pull_penalty, 0.0);
    }
}
