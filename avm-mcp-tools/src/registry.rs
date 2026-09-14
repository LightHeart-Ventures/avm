//! `ManagedToolSet`: the gateway's registry of every callable tool signature.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schema::{from_mcp_definition, ToolSchema, ToolSource};
use crate::validate::{parse_arguments, SchemaValidator, ValidationReport};
use crate::{Result, ToolError, CATALOG_VERSION};

/// Registry of tool schemas plus their compiled validators.
///
/// Populated at gateway startup (built-ins) and whenever an MCP connection is
/// established or refreshed (`tools/list` → [`ManagedToolSet::ingest_listing`]).
#[derive(Default)]
pub struct ManagedToolSet {
    tools: BTreeMap<String, ToolSchema>,
    validators: BTreeMap<String, SchemaValidator>,
}

impl std::fmt::Debug for ManagedToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedToolSet")
            .field("tools", &self.names())
            .finish()
    }
}

impl ManagedToolSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// A set preloaded with the gateway's built-in tool signatures.
    pub fn with_builtins() -> Self {
        let mut set = Self::new();
        for schema in crate::builtin::builtin_tools() {
            // Built-ins are generated from Rust types; they cannot collide.
            let _ = set.register(schema);
        }
        set
    }

    /// Register (or replace) a tool schema, compiling its validator.
    ///
    /// Replacing a name owned by a *different* source is rejected — that is a
    /// routing collision between two MCP servers, not a schema update.
    pub fn register(&mut self, schema: ToolSchema) -> Result<()> {
        if let Some(existing) = self.tools.get(&schema.name) {
            if existing.source.id != schema.source.id {
                return Err(ToolError::DuplicateTool {
                    tool: schema.name.clone(),
                    existing_source: existing.source.id.clone(),
                });
            }
        }

        let validator = SchemaValidator::compile(&schema)?;
        self.validators.insert(schema.name.clone(), validator);
        self.tools.insert(schema.name.clone(), schema);
        Ok(())
    }

    /// Ingest a full MCP `tools/list` response from one server.
    ///
    /// Returns the names accepted. A malformed entry fails the whole listing so
    /// a half-registered server never serves traffic.
    pub fn ingest_listing(&mut self, source: ToolSource, listing: &Value) -> Result<Vec<String>> {
        let entries = listing
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::MalformedDefinition("listing has no `tools` array".into()))?;

        let parsed: Vec<ToolSchema> = entries
            .iter()
            .map(|def| from_mcp_definition(source.clone(), def))
            .collect::<Result<Vec<_>>>()?;

        let mut accepted = Vec::with_capacity(parsed.len());
        for schema in parsed {
            let name = schema.name.clone();
            self.register(schema)?;
            accepted.push(name);
        }
        Ok(accepted)
    }

    /// Drop every tool contributed by a source (used when an MCP disconnects).
    pub fn remove_source(&mut self, source_id: &str) -> usize {
        let doomed: Vec<String> = self
            .tools
            .iter()
            .filter(|(_, s)| s.source.id == source_id)
            .map(|(n, _)| n.clone())
            .collect();
        for name in &doomed {
            self.tools.remove(name);
            self.validators.remove(name);
        }
        doomed.len()
    }

    /// Look up one tool signature.
    pub fn get(&self, name: &str) -> Option<&ToolSchema> {
        self.tools.get(name)
    }

    /// Sorted tool names.
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// True when no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Iterate over every signature.
    pub fn iter(&self) -> impl Iterator<Item = &ToolSchema> {
        self.tools.values()
    }

    /// Validate arguments for `tool` using the cached validator.
    ///
    /// Unknown tools produce a failing report rather than an error so the
    /// gateway can answer `POST /tools/validate` uniformly.
    pub fn validate_call(&self, tool: &str, arguments: &Value) -> ValidationReport {
        match self.validators.get(tool) {
            Some(v) => v.validate(arguments),
            None => ValidationReport::failed(tool, format!("unknown tool `{tool}`")),
        }
    }

    /// Validate a wire-form call (`arguments_json` string).
    pub fn validate_wire_call(&self, tool: &str, arguments_json: &str) -> ValidationReport {
        match parse_arguments(arguments_json) {
            Ok(args) => self.validate_call(tool, &args),
            Err(e) => ValidationReport::failed(tool, e.to_string()),
        }
    }

    /// Gate used on the dispatch path: `Ok(())` or the reason to refuse.
    pub fn assert_callable(&self, tool: &str, arguments: &Value) -> Result<()> {
        if !self.tools.contains_key(tool) {
            return Err(ToolError::UnknownTool(tool.to_string()));
        }
        let report = self.validate_call(tool, arguments);
        if report.valid {
            Ok(())
        } else {
            Err(ToolError::BadArguments(
                report
                    .errors
                    .iter()
                    .map(|e| e.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            ))
        }
    }

    /// Serializable catalog served by `GET /tools/schema`.
    pub fn to_catalog(&self) -> ToolCatalog {
        ToolCatalog {
            catalog_version: CATALOG_VERSION,
            dialect: crate::SCHEMA_DIALECT,
            tools: self
                .tools
                .values()
                .map(|s| ToolCatalogEntry {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    input_schema: s.input_schema.clone(),
                    output_schema: s.output_schema.clone(),
                    source_id: s.source.id.clone(),
                    transport: s.source.transport.as_str(),
                    version: s.version.clone(),
                    fingerprint: s.fingerprint.clone(),
                })
                .collect(),
        }
    }

    /// Proto-facing view: one [`avm_proto::tools::ToolDefinition`] per tool.
    pub fn to_tool_definitions(&self) -> Vec<avm_proto::tools::ToolDefinition> {
        self.tools
            .values()
            .map(avm_proto::tools::ToolDefinition::from)
            .collect()
    }
}

/// One entry in the `GET /tools/schema` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCatalogEntry {
    /// Callable tool name.
    pub name: String,
    /// Human description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON-Schema for the argument object.
    pub input_schema: Value,
    /// JSON-Schema for the result, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// MCP server id that owns the tool.
    pub source_id: String,
    /// `builtin` | `stdio` | `http`.
    pub transport: &'static str,
    /// Server-declared version, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Contract fingerprint for client-side caching.
    pub fingerprint: String,
}

/// Full `GET /tools/schema` envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCatalog {
    /// Version of this envelope format.
    pub catalog_version: &'static str,
    /// JSON-Schema dialect the entries are expressed in.
    pub dialect: &'static str,
    /// Every tool the gateway can dispatch.
    pub tools: Vec<ToolCatalogEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listing() -> Value {
        json!({
            "tools": [
                {
                    "name": "read_file",
                    "description": "Read a file",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    }
                },
                {
                    "name": "write_file",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "path": { "type": "string" }, "body": { "type": "string" } },
                        "required": ["path", "body"]
                    }
                }
            ]
        })
    }

    #[test]
    fn ingests_a_listing() {
        let mut set = ManagedToolSet::new();
        let names = set
            .ingest_listing(ToolSource::stdio("fs", "npx fs"), &listing())
            .unwrap();
        assert_eq!(names, vec!["read_file", "write_file"]);
        assert_eq!(set.len(), 2);
        assert_eq!(set.names(), vec!["read_file", "write_file"]);
    }

    #[test]
    fn validates_through_the_registry() {
        let mut set = ManagedToolSet::new();
        set.ingest_listing(ToolSource::http("fs", "https://x.invalid/mcp"), &listing())
            .unwrap();

        assert!(
            set.validate_call("read_file", &json!({ "path": "/a" }))
                .valid
        );
        assert!(!set.validate_call("read_file", &json!({})).valid);

        let unknown = set.validate_call("nope", &json!({}));
        assert!(!unknown.valid);
        assert!(unknown.errors[0].message.contains("unknown tool"));
    }

    #[test]
    fn wire_form_arguments_are_parsed() {
        let mut set = ManagedToolSet::new();
        set.ingest_listing(ToolSource::stdio("fs", "npx fs"), &listing())
            .unwrap();
        assert!(
            set.validate_wire_call("read_file", r#"{"path":"/a"}"#)
                .valid
        );
        assert!(!set.validate_wire_call("read_file", "{oops").valid);
    }

    #[test]
    fn assert_callable_is_the_dispatch_gate() {
        let mut set = ManagedToolSet::new();
        set.ingest_listing(ToolSource::stdio("fs", "npx fs"), &listing())
            .unwrap();
        assert!(set
            .assert_callable("read_file", &json!({ "path": "/a" }))
            .is_ok());
        assert!(matches!(
            set.assert_callable("ghost", &json!({})),
            Err(ToolError::UnknownTool(_))
        ));
        assert!(matches!(
            set.assert_callable("read_file", &json!({})),
            Err(ToolError::BadArguments(_))
        ));
    }

    #[test]
    fn colliding_sources_are_rejected() {
        let mut set = ManagedToolSet::new();
        set.ingest_listing(ToolSource::stdio("fs", "npx fs"), &listing())
            .unwrap();
        let err = set
            .ingest_listing(
                ToolSource::http("other", "https://y.invalid/mcp"),
                &listing(),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::DuplicateTool { .. }));
    }

    #[test]
    fn same_source_may_refresh_its_own_schema() {
        let mut set = ManagedToolSet::new();
        let src = ToolSource::stdio("fs", "npx fs");
        set.ingest_listing(src.clone(), &listing()).unwrap();
        set.ingest_listing(src, &listing()).unwrap();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn disconnecting_a_source_drops_its_tools() {
        let mut set = ManagedToolSet::with_builtins();
        let builtin_count = set.len();
        set.ingest_listing(ToolSource::stdio("fs", "npx fs"), &listing())
            .unwrap();
        assert_eq!(set.remove_source("fs"), 2);
        assert_eq!(set.len(), builtin_count);
    }

    #[test]
    fn catalog_round_trips_and_covers_builtins() {
        let set = ManagedToolSet::with_builtins();
        assert!(!set.is_empty());
        let catalog = set.to_catalog();
        assert_eq!(catalog.catalog_version, CATALOG_VERSION);
        let encoded = serde_json::to_string(&catalog).unwrap();
        assert!(encoded.contains("avm_memory_read"));
        assert!(catalog
            .tools
            .iter()
            .all(|t| t.input_schema["type"] == "object"));
    }

    #[test]
    fn exports_proto_tool_definitions() {
        let set = ManagedToolSet::with_builtins();
        let defs = set.to_tool_definitions();
        assert_eq!(defs.len(), set.len());
        assert!(defs.iter().any(|d| d.name == "avm_job_submit"));
        // json_schema is carried as a string on the wire.
        let parsed: Value = serde_json::from_str(&defs[0].json_schema).unwrap();
        assert_eq!(parsed["type"], "object");
    }
}
