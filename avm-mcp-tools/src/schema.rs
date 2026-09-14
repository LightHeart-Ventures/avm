//! Tool schema representation, generation and ingestion.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{Result, ToolError};

/// How the gateway reaches the server that owns a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTransport {
    /// Handled inside the gateway process.
    Builtin,
    /// MCP server spawned as a child process, framed over stdio.
    Stdio,
    /// MCP server reached over streamable HTTP.
    Http,
}

impl ToolTransport {
    /// Lowercase wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolTransport::Builtin => "builtin",
            ToolTransport::Stdio => "stdio",
            ToolTransport::Http => "http",
        }
    }
}

/// Provenance of a tool schema: which MCP server it came from and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSource {
    /// Stable server id (`filesystem`, `atum`, `builtin`, ...).
    pub id: String,
    /// Transport used to reach the server.
    pub transport: ToolTransport,
    /// Command line (stdio) or URL (http). Empty for builtins.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
}

impl ToolSource {
    /// A tool implemented by the gateway itself.
    pub fn builtin() -> Self {
        Self {
            id: "builtin".into(),
            transport: ToolTransport::Builtin,
            endpoint: String::new(),
        }
    }

    /// A tool served by a stdio MCP child process.
    pub fn stdio(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            transport: ToolTransport::Stdio,
            endpoint: command.into(),
        }
    }

    /// A tool served by an HTTP MCP endpoint.
    pub fn http(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            transport: ToolTransport::Http,
            endpoint: url.into(),
        }
    }
}

/// A single tool signature: name plus its JSON-Schema input/output contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name as callable through the gateway.
    pub name: String,
    /// Human description surfaced to agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON-Schema describing the argument object. Always an object schema.
    pub input_schema: Value,
    /// JSON-Schema describing the result, when the server advertises one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Where this tool lives.
    pub source: ToolSource,
    /// Server-declared version string, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Stable fingerprint of `input_schema` + `output_schema`.
    ///
    /// Changes iff the contract changes; agents cache schemas against it.
    pub fingerprint: String,
}

impl ToolSchema {
    /// Build a schema from parts, computing the fingerprint.
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        input_schema: Value,
        output_schema: Option<Value>,
        source: ToolSource,
    ) -> Self {
        let name = name.into();
        let fingerprint = fingerprint(&input_schema, output_schema.as_ref());
        Self {
            name,
            description,
            input_schema,
            output_schema,
            source,
            version: None,
            fingerprint,
        }
    }

    /// Attach a server-declared version string.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Properties declared on the input schema (empty when untyped).
    pub fn properties(&self) -> Vec<&str> {
        self.input_schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Required properties on the input schema.
    pub fn required(&self) -> Vec<&str> {
        self.input_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }

    /// Classify the change from `previous` to `self`.
    ///
    /// See `IMPLEMENTATION_PLAN.md` — this is the machine half of the
    /// versioning strategy; the gateway refuses to hot-swap a schema whose
    /// change is [`Compatibility::Breaking`] without a version bump.
    pub fn compatibility(&self, previous: &ToolSchema) -> Compatibility {
        if self.fingerprint == previous.fingerprint {
            return Compatibility::Identical;
        }

        let old_props = object_properties(&previous.input_schema);
        let new_props = object_properties(&self.input_schema);

        // Removing or retyping a property breaks existing callers.
        for (name, old_ty) in &old_props {
            match new_props.get(name) {
                None => return Compatibility::Breaking,
                Some(new_ty) if new_ty != old_ty => return Compatibility::Breaking,
                Some(_) => {}
            }
        }

        let old_required = previous.required();
        for req in self.required() {
            // A newly-required property breaks callers that omitted it.
            if !old_required.contains(&req) {
                return Compatibility::Breaking;
            }
        }

        Compatibility::BackwardCompatible
    }
}

/// Result of comparing two revisions of the same tool schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    /// Byte-identical contract.
    Identical,
    /// Existing valid calls remain valid (new optional properties, doc changes).
    BackwardCompatible,
    /// Previously-valid calls may now fail. Requires a version bump.
    Breaking,
}

/// Generate a JSON-Schema for a native Rust argument type.
///
/// Used for the gateway's built-in tools so their signatures cannot drift from
/// the structs the handlers actually deserialize.
pub fn to_json_schema<T: schemars::JsonSchema>() -> Value {
    let settings = schemars::gen::SchemaSettings::draft2019_09().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let root = generator.into_root_schema_for::<T>();
    serde_json::to_value(root).unwrap_or_else(|_| json!({ "type": "object" }))
}

/// Parse one entry of an MCP `tools/list` response into a [`ToolSchema`].
///
/// Accepts the MCP spec's camelCase (`inputSchema`) and the snake_case variant
/// some servers emit. A missing `inputSchema` is tolerated and normalised to a
/// permissive object schema — MCP allows argument-less tools.
pub fn from_mcp_definition(source: ToolSource, def: &Value) -> Result<ToolSchema> {
    let obj = def
        .as_object()
        .ok_or_else(|| ToolError::MalformedDefinition("tool entry is not an object".into()))?;

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::MalformedDefinition("tool entry has no `name`".into()))?
        .to_string();

    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty());

    let input_schema = pick(obj, &["inputSchema", "input_schema"])
        .cloned()
        .map(normalize_input_schema)
        .unwrap_or_else(empty_object_schema);

    if !input_schema.is_object() {
        return Err(ToolError::MalformedDefinition(format!(
            "tool `{name}` has a non-object inputSchema"
        )));
    }

    let output_schema = pick(obj, &["outputSchema", "output_schema"]).cloned();

    let version = pick(obj, &["version"])
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut schema = ToolSchema::new(name, description, input_schema, output_schema, source);
    schema.version = version;
    Ok(schema)
}

fn pick<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| obj.get(*k))
}

/// MCP servers occasionally omit `"type": "object"` on an object schema.
fn normalize_input_schema(mut schema: Value) -> Value {
    if let Some(map) = schema.as_object_mut() {
        if !map.contains_key("type") && map.contains_key("properties") {
            map.insert("type".into(), json!("object"));
        }
    }
    schema
}

fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn object_properties(schema: &Value) -> std::collections::BTreeMap<String, String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .map(|(k, v)| {
                    let ty = v
                        .get("type")
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    (k.clone(), ty)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Stable, order-independent fingerprint of a tool contract.
///
/// FNV-1a over the canonical (key-sorted) JSON form. Not a cryptographic
/// digest — it exists to answer "did this contract change?", not to
/// authenticate it.
pub fn fingerprint(input: &Value, output: Option<&Value>) -> String {
    let mut canonical = String::new();
    canonicalize(input, &mut canonical);
    if let Some(out) = output {
        canonical.push('|');
        canonicalize(out, &mut canonical);
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn canonicalize(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for k in keys {
                out.push_str(k);
                out.push(':');
                canonicalize(&map[k], out);
                out.push(',');
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for item in items {
                canonicalize(item, out);
                out.push(',');
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)] // exists to be reflected over, not read
    struct Args {
        /// Memory key.
        memory_id: String,
        #[serde(default)]
        include_inherited: bool,
    }

    #[test]
    fn generates_object_schema_from_rust_type() {
        let schema = to_json_schema::<Args>();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("memory_id").is_some());
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(required.contains(&"memory_id"));
    }

    #[test]
    fn parses_camel_case_mcp_definition() {
        let def = json!({
            "name": "read_file",
            "description": "Read a file",
            "inputSchema": {
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        });
        let schema = from_mcp_definition(ToolSource::stdio("fs", "npx fs"), &def).unwrap();
        assert_eq!(schema.name, "read_file");
        assert_eq!(schema.input_schema["type"], "object", "type is normalised");
        assert_eq!(schema.required(), vec!["path"]);
        assert_eq!(schema.source.transport, ToolTransport::Stdio);
    }

    #[test]
    fn rejects_nameless_definition() {
        let err =
            from_mcp_definition(ToolSource::builtin(), &json!({ "description": "x" })).unwrap_err();
        assert!(matches!(err, ToolError::MalformedDefinition(_)));
    }

    #[test]
    fn argument_less_tool_gets_empty_object_schema() {
        let schema =
            from_mcp_definition(ToolSource::builtin(), &json!({ "name": "ping" })).unwrap();
        assert_eq!(schema.input_schema["type"], "object");
        assert!(schema.required().is_empty());
    }

    #[test]
    fn fingerprint_is_key_order_independent() {
        let a = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        let b = json!({ "properties": { "a": { "type": "string" } }, "type": "object" });
        assert_eq!(fingerprint(&a, None), fingerprint(&b, None));
    }

    fn schema_with(input: Value) -> ToolSchema {
        ToolSchema::new("t", None, input, None, ToolSource::builtin())
    }

    #[test]
    fn adding_optional_property_is_backward_compatible() {
        let old = schema_with(json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["a"]
        }));
        let new = schema_with(json!({
            "type": "object",
            "properties": { "a": { "type": "string" }, "b": { "type": "number" } },
            "required": ["a"]
        }));
        assert_eq!(new.compatibility(&old), Compatibility::BackwardCompatible);
        assert_eq!(old.compatibility(&old), Compatibility::Identical);
    }

    #[test]
    fn newly_required_property_is_breaking() {
        let old = schema_with(json!({
            "type": "object",
            "properties": { "a": { "type": "string" } }
        }));
        let new = schema_with(json!({
            "type": "object",
            "properties": { "a": { "type": "string" }, "b": { "type": "number" } },
            "required": ["b"]
        }));
        assert_eq!(new.compatibility(&old), Compatibility::Breaking);
    }

    #[test]
    fn removing_or_retyping_a_property_is_breaking() {
        let old = schema_with(json!({
            "type": "object",
            "properties": { "a": { "type": "string" } }
        }));
        let removed = schema_with(json!({ "type": "object", "properties": {} }));
        let retyped = schema_with(json!({
            "type": "object",
            "properties": { "a": { "type": "number" } }
        }));
        assert_eq!(removed.compatibility(&old), Compatibility::Breaking);
        assert_eq!(retyped.compatibility(&old), Compatibility::Breaking);
    }
}

// ---------------------------------------------------------------------
// Wire conversions
// ---------------------------------------------------------------------

impl From<&ToolSchema> for avm_proto::tools::ToolDefinition {
    fn from(schema: &ToolSchema) -> Self {
        Self {
            name: schema.name.clone(),
            json_schema: schema.input_schema.to_string(),
            description: schema.description.clone().unwrap_or_default(),
            output_json_schema: schema
                .output_schema
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            source_id: schema.source.id.clone(),
            transport: schema.source.transport.as_str().to_string(),
            version: schema.version.clone().unwrap_or_default(),
            fingerprint: schema.fingerprint.clone(),
        }
    }
}

impl From<ToolSchema> for avm_proto::tools::ToolDefinition {
    fn from(schema: ToolSchema) -> Self {
        Self::from(&schema)
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    #[test]
    fn tool_schema_converts_to_proto_definition() {
        let schema = ToolSchema::new(
            "read_file",
            Some("Read a file".into()),
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            Some(json!({ "type": "string" })),
            ToolSource::http("fs", "https://x.invalid/mcp"),
        )
        .with_version("1.2.0");

        let def = avm_proto::tools::ToolDefinition::from(&schema);
        assert_eq!(def.name, "read_file");
        assert_eq!(def.transport, "http");
        assert_eq!(def.source_id, "fs");
        assert_eq!(def.version, "1.2.0");
        assert_eq!(def.fingerprint, schema.fingerprint);

        let reparsed: Value = serde_json::from_str(&def.json_schema).unwrap();
        assert_eq!(reparsed, schema.input_schema);
        assert_eq!(def.output_json_schema, "{\"type\":\"string\"}");
    }
}
