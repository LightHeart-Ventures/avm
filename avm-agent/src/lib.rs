//! `avm-agent` — the agent-facing data model for AVM.
//!
//! This crate carries the types that describe *an agent* rather than the
//! runtime that executes it:
//!
//! * [`a2a_policy`] — the A2A (agent-to-agent) trust policy attached to every
//!   Agent Card, plus the scope triple that tenant/project boundaries are
//!   evaluated against.
//!
//! The A2A + Agent Card spike owns the richer transport-level types
//! (`A2ATask`, `A2AResponse`, discovery endpoints). This module deliberately
//! keeps the *security* surface separate and self-contained so the two can
//! land independently.

pub mod a2a_policy;

pub use a2a_policy::{A2APolicy, AgentCard, AgentScope, TrustDefault};
