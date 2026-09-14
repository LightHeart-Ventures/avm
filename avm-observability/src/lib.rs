//! Observability bootstrap shared by every AVM binary.
//!
//! ```no_run
//! avm_observability::init("avm-server");
//! ```

pub mod tracing;

pub use tracing::{init, init_with, TelemetryConfig};
