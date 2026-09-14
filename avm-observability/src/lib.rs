//! Observability bootstrap shared by every AVM binary.
//!
//! This crate is now a **thin compatibility shim** over [`avm_otel`], which
//! owns the real OpenTelemetry bootstrap (traces, metrics, logs, propagation,
//! sampling). New code should depend on `avm-otel` directly; this crate keeps
//! the historical `avm_observability::init("avm-x")` entry point working.
//!
//! ```no_run
//! avm_observability::init("avm-server");
//! ```

pub mod tracing;

pub use tracing::{init, init_with, TelemetryConfig};

// Re-exports so downstream crates can reach the real API through the shim.
pub use avm_otel::{
    encode_prometheus, fields, init_otel, init_otel_with, level, metrics, propagation, registry,
    resource, sampler, InstrumentationConfig, InstrumentationLevel, Otel, OtelConfig, Registry,
    Resource, SamplerSpec, TraceContext,
};
