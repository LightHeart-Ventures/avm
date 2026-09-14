//! JSON-Schema tool signatures for AVM.
//!
//! Agents need two things before they can safely call a tool:
//!
//! 1. **Introspection** — "what tools exist, and what arguments do they take?"
//! 2. **Validation** — "is this specific call well-formed *before* I dispatch it?"
//!
//! This crate answers both using JSON-Schema as-is (no bespoke schema
//! language). Schemas arrive from two directions:
//!
//! * **Generated** from native Rust argument types via [`schema::to_json_schema`]
//!   (`schemars`) — used for the gateway's built-in tools.
//! * **Ingested** from an MCP server's `tools/list` response via
//!   [`schema::from_mcp_definition`] — used for every upstream stdio/HTTP MCP.
//!
//! Both land in a [`ManagedToolSet`], which the gateway serves over
//! `GET /tools/schema` and validates against on `POST /tools/validate`.
//!
//! ```
//! use avm_mcp_tools::{ManagedToolSet, ToolSource};
//! use serde_json::json;
//!
//! let mut set = ManagedToolSet::new();
//! set.ingest_listing(
//!     ToolSource::stdio("filesystem", "npx -y @modelcontextprotocol/server-filesystem"),
//!     &json!({
//!         "tools": [{
//!             "name": "read_file",
//!             "description": "Read a file",
//!             "inputSchema": {
//!                 "type": "object",
//!                 "properties": { "path": { "type": "string" } },
//!                 "required": ["path"]
//!             }
//!         }]
//!     }),
//! )
//! .unwrap();
//!
//! let report = set.validate_call("read_file", &json!({ "path": "/etc/hosts" }));
//! assert!(report.valid);
//!
//! let bad = set.validate_call("read_file", &json!({}));
//! assert!(!bad.valid);
//! ```

#![deny(missing_docs)]

pub mod builtin;
pub mod registry;
pub mod schema;
pub mod validate;

pub use registry::{ManagedToolSet, ToolCatalog, ToolCatalogEntry};
pub use schema::{
    from_mcp_definition, to_json_schema, Compatibility, ToolSchema, ToolSource, ToolTransport,
};
pub use validate::{validate_call, SchemaValidator, ValidationIssue, ValidationReport};

/// JSON-Schema dialect AVM publishes and validates against.
pub const SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Version of the `/tools/schema` envelope itself (not of any individual tool).
pub const CATALOG_VERSION: &str = "avm.tools/v1";

/// Errors raised while building or querying a tool set.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// An MCP `tools/list` entry was missing a required field or malformed.
    #[error("malformed MCP tool definition: {0}")]
    MalformedDefinition(String),

    /// A schema failed to compile into a validator.
    #[error("invalid JSON-Schema for tool `{tool}`: {source_message}")]
    InvalidSchema {
        /// Tool the schema belongs to.
        tool: String,
        /// Compiler message.
        source_message: String,
    },

    /// A second tool tried to claim a name already taken by another source.
    #[error("duplicate tool `{tool}`: already registered by `{existing_source}`")]
    DuplicateTool {
        /// Conflicting tool name.
        tool: String,
        /// Source id that already owns the name.
        existing_source: String,
    },

    /// A call referenced a tool the gateway does not expose.
    #[error("unknown tool `{0}`")]
    UnknownTool(String),

    /// `arguments_json` was not parseable JSON.
    #[error("arguments are not valid JSON: {0}")]
    BadArguments(String),
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, ToolError>;
