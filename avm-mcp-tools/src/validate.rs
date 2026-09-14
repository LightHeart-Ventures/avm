//! Runtime validation of tool calls against their JSON-Schema signature.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schema::ToolSchema;
use crate::{Result, ToolError};

/// One schema violation, addressed by JSON-Pointer into the arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// JSON-Pointer to the offending value (`""` = the root object).
    pub path: String,
    /// Human-readable reason.
    pub message: String,
}

/// Outcome of validating a single tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// Tool the call targeted.
    pub tool: String,
    /// True when the arguments satisfy the input schema.
    pub valid: bool,
    /// Every violation found (empty when `valid`).
    #[serde(default)]
    pub errors: Vec<ValidationIssue>,
    /// Fingerprint of the schema the call was checked against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_fingerprint: Option<String>,
}

impl ValidationReport {
    /// A passing report.
    pub fn ok(tool: impl Into<String>, fingerprint: Option<String>) -> Self {
        Self {
            tool: tool.into(),
            valid: true,
            errors: Vec::new(),
            schema_fingerprint: fingerprint,
        }
    }

    /// A report carrying a single top-level failure.
    pub fn failed(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            valid: false,
            errors: vec![ValidationIssue {
                path: String::new(),
                message: message.into(),
            }],
            schema_fingerprint: None,
        }
    }
}

/// A compiled JSON-Schema validator for one tool.
///
/// Compilation is the expensive half, so the gateway builds these once per
/// schema revision (keyed by [`ToolSchema::fingerprint`]) and reuses them for
/// every call.
pub struct SchemaValidator {
    tool: String,
    fingerprint: String,
    input: jsonschema::Validator,
}

impl std::fmt::Debug for SchemaValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaValidator")
            .field("tool", &self.tool)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl SchemaValidator {
    /// Compile the input schema of `schema`.
    pub fn compile(schema: &ToolSchema) -> Result<Self> {
        let input = jsonschema::validator_for(&schema.input_schema).map_err(|e| {
            ToolError::InvalidSchema {
                tool: schema.name.clone(),
                source_message: e.to_string(),
            }
        })?;
        Ok(Self {
            tool: schema.name.clone(),
            fingerprint: schema.fingerprint.clone(),
            input,
        })
    }

    /// Tool this validator belongs to.
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Fingerprint of the schema revision this validator was built from.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Fast path: is this argument object acceptable?
    pub fn is_valid(&self, arguments: &Value) -> bool {
        self.input.is_valid(arguments)
    }

    /// Full path: collect every violation.
    pub fn validate(&self, arguments: &Value) -> ValidationReport {
        let errors: Vec<ValidationIssue> = self
            .input
            .iter_errors(arguments)
            .map(|err| ValidationIssue {
                path: err.instance_path.to_string(),
                message: err.to_string(),
            })
            .collect();

        ValidationReport {
            tool: self.tool.clone(),
            valid: errors.is_empty(),
            errors,
            schema_fingerprint: Some(self.fingerprint.clone()),
        }
    }
}

/// One-shot validation of `arguments` against `schema`.
///
/// Compiles the schema on every call — fine for tests and cold paths; use
/// [`SchemaValidator::compile`] (or [`crate::ManagedToolSet`], which caches)
/// on the dispatch path.
pub fn validate_call(schema: &ToolSchema, arguments: &Value) -> Result<ValidationReport> {
    Ok(SchemaValidator::compile(schema)?.validate(arguments))
}

/// Parse an `arguments_json` string the way the wire protocol carries it.
pub fn parse_arguments(arguments_json: &str) -> Result<Value> {
    if arguments_json.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(arguments_json).map_err(|e| ToolError::BadArguments(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ToolSchema, ToolSource};
    use serde_json::json;

    fn read_file_schema() -> ToolSchema {
        ToolSchema::new(
            "read_file",
            Some("Read a file".into()),
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "max_bytes": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            None,
            ToolSource::stdio("fs", "npx fs"),
        )
    }

    #[test]
    fn accepts_valid_arguments() {
        let report = validate_call(&read_file_schema(), &json!({ "path": "/etc/hosts" })).unwrap();
        assert!(report.valid, "{report:?}");
        assert!(report.errors.is_empty());
        assert!(report.schema_fingerprint.is_some());
    }

    #[test]
    fn rejects_missing_required_property() {
        let report = validate_call(&read_file_schema(), &json!({})).unwrap();
        assert!(!report.valid);
        assert!(!report.errors.is_empty());
    }

    #[test]
    fn rejects_wrong_type() {
        let report = validate_call(&read_file_schema(), &json!({ "path": 7 })).unwrap();
        assert!(!report.valid);
        assert_eq!(report.errors[0].path, "/path");
    }

    #[test]
    fn rejects_unknown_property_when_sealed() {
        let report =
            validate_call(&read_file_schema(), &json!({ "path": "/x", "nope": true })).unwrap();
        assert!(!report.valid);
    }

    #[test]
    fn compiled_validator_is_reusable() {
        let schema = read_file_schema();
        let validator = SchemaValidator::compile(&schema).unwrap();
        assert_eq!(validator.tool(), "read_file");
        assert_eq!(validator.fingerprint(), schema.fingerprint);
        assert!(validator.is_valid(&json!({ "path": "/a" })));
        assert!(!validator.is_valid(&json!({ "path": 1 })));
    }

    #[test]
    fn empty_arguments_string_is_an_empty_object() {
        assert_eq!(parse_arguments("   ").unwrap(), json!({}));
        assert!(parse_arguments("{not json").is_err());
    }
}
