//! Serde mirrors of the tool-signature messages in `proto/avm_service.proto`.
//!
//! Same rationale as [`crate::types`]: these travel over HTTP/NATS as JSON, so
//! keeping hand-written mirrors removes `protoc` from the data path while the
//! `.proto` remains the gRPC contract of record.

use serde::{Deserialize, Serialize};

use crate::types::Scope;

/// A tool signature as published by the gateway.
///
/// `json_schema` carries a JSON-Schema **document encoded as a string** — the
/// wire format stays a plain proto `string` rather than a nested `Struct`, so
/// schemas pass through unmodified (no field-ordering or number-precision
/// surprises from protobuf `Value` round-trips).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Callable tool name.
    pub name: String,
    /// JSON-Schema document for the argument object, serialized.
    pub json_schema: String,
    /// Human description.
    #[serde(default)]
    pub description: String,
    /// JSON-Schema document for the result, serialized. Empty when unknown.
    #[serde(default)]
    pub output_json_schema: String,
    /// MCP server id that owns this tool (`builtin` for gateway-local tools).
    #[serde(default)]
    pub source_id: String,
    /// `builtin` | `stdio` | `http`.
    #[serde(default)]
    pub transport: String,
    /// Server-declared version string, when present.
    #[serde(default)]
    pub version: String,
    /// Stable fingerprint of the contract; changes iff the schema changes.
    #[serde(default)]
    pub fingerprint: String,
}

/// An agent's intent to invoke a tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool to invoke.
    pub tool_name: String,
    /// Arguments as a serialized JSON object.
    #[serde(default)]
    pub arguments_json: String,
    /// Caller scope, injected by the executor.
    #[serde(default)]
    pub scope: Scope,
    /// Optional correlation id so a validation verdict can be traced to a call.
    #[serde(default)]
    pub call_id: String,
}

impl ToolCall {
    /// Build a call from a name and an already-serialized argument object.
    pub fn new(tool_name: impl Into<String>, arguments_json: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments_json: arguments_json.into(),
            ..Default::default()
        }
    }
}

/// Verdict returned by `POST /tools/validate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallValidation {
    /// Tool the call targeted.
    pub tool_name: String,
    /// True when the arguments satisfy the tool's input schema.
    pub valid: bool,
    /// Human-readable violations (empty when `valid`).
    #[serde(default)]
    pub errors: Vec<String>,
    /// Fingerprint of the schema revision used for the check.
    #[serde(default)]
    pub schema_fingerprint: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_round_trips() {
        let def = ToolDefinition {
            name: "read_file".into(),
            json_schema: r#"{"type":"object"}"#.into(),
            description: "Read a file".into(),
            transport: "stdio".into(),
            source_id: "fs".into(),
            fingerprint: "fnv1a64:deadbeefdeadbeef".into(),
            ..Default::default()
        };
        let encoded = serde_json::to_vec(&def).unwrap();
        let decoded: ToolDefinition = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, def);
    }

    #[test]
    fn tool_call_defaults_to_empty_scope() {
        let call = ToolCall::new("read_file", r#"{"path":"/a"}"#);
        assert_eq!(call.tool_name, "read_file");
        assert!(call.scope.level.is_empty());
        let decoded: ToolCall =
            serde_json::from_str(r#"{"tool_name":"x","arguments_json":"{}"}"#).unwrap();
        assert_eq!(decoded.tool_name, "x");
    }
}
