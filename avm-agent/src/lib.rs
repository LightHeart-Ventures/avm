//! `avm-agent` — the agent-facing data model for AVM.
//!
//! This crate carries the types that describe *an agent* rather than the
//! runtime that executes it. Three concerns live here:
//!
//! * [`card`] — how an agent **advertises itself**. An [`AgentCard`] is the
//!   JSON document served at [`discovery::AGENT_CARD_PATH`]
//!   (`/.well-known/agent-card.json`), following the Linux Foundation A2A
//!   convention.
//! * [`a2a`] — how an agent **accepts inbound work**. An [`A2ATask`] is POSTed
//!   to [`discovery::A2A_TASK_PATH`] (`/a2a/task`) and answered with an
//!   [`A2AResponse`].
//! * [`a2a_policy`] — **who may call it**: the A2A trust policy carried on
//!   every Agent Card, plus the [`AgentScope`] triple that tenant/project
//!   boundaries are evaluated against. The gateway enforces it in
//!   `avm_gateway::security::validate_a2a_dispatch`.
//!
//! The policy is not a separate document: [`AgentCard::a2a_policy`] is part of
//! the card, and carries `#[serde(default)]` so cards written before the field
//! existed still deserialize — to a deny-all policy, which is the safe default.
//!
//! Like [`avm_proto::types`], these are hand-written serde structs rather than
//! `prost`-generated ones: the A2A data path is JSON over HTTP, so the wire
//! format must stay human-readable and must not drag a `protoc` build
//! dependency into the gateway. The protobuf mirrors in
//! `proto/avm_service.proto` exist for gRPC clients that prefer them.
//!
//! ```
//! use avm_agent::{AgentCard, A2ATask, A2AResponse, TaskStatus};
//!
//! let card = AgentCard::example();
//! let json = serde_json::to_string_pretty(&card).unwrap();
//! assert!(json.contains("\"schema_version\""));
//!
//! let task = A2ATask::new("ag_planner", "Summarise the open PRs.");
//! let reply = A2AResponse::accepted(&task.task_id);
//! assert_eq!(reply.status, TaskStatus::Accepted);
//! ```

pub mod a2a;
pub mod a2a_policy;
pub mod card;
pub mod discovery;

pub use a2a::{
    A2AError, A2AResponse, A2ATask, Artifact, ErrorCode, TaskContext, TaskResult, TaskStatus,
    TaskTimeout, Usage,
};
pub use a2a_policy::{A2APolicy, AgentScope, TrustDefault};
pub use card::{
    AgentCard, AgentRef, AuthPolicy, AuthScheme, Capability, McpServerRef, McpTransport, ModelRef,
};
pub use discovery::{A2A_TASK_PATH, AGENT_CARD_PATH, SCHEMA_VERSION};
