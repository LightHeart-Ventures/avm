//! Tool introspection and pre-dispatch validation endpoints.
//!
//! * `GET  /tools/schema`        — full JSON-Schema catalog of every callable tool
//! * `GET  /tools/schema/:name`  — one tool's signature
//! * `POST /tools/validate`      — check a tool call *before* dispatching it
//!
//! These are what an A2A peer reads to learn what it may call: an agent fetches
//! `/tools/schema` once, caches per-tool by `fingerprint`, and validates
//! locally or via `/tools/validate` before spending a dispatch.

use std::sync::Arc;

use avm_mcp_tools::{ManagedToolSet, ToolCatalog};
use avm_proto::types::Scope;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

/// Request body for `POST /tools/validate`.
///
/// Accepts either a structured `arguments` object or the wire-form
/// `arguments_json` string carried by `avm_proto::tools::ToolCall`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ValidateRequest {
    /// Tool to check.
    pub tool_name: String,
    /// Arguments as JSON.
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    /// Arguments as a serialized JSON string (wire form).
    #[serde(default)]
    pub arguments_json: Option<String>,
    /// Caller scope, when the executor supplies it.
    #[serde(default)]
    pub scope: Option<Scope>,
}

/// Response body for `POST /tools/validate`.
#[derive(Debug, Clone, Serialize)]
pub struct ValidateResponse {
    /// Tool the call targeted.
    pub tool_name: String,
    /// True when the arguments satisfy the input schema.
    pub valid: bool,
    /// True when the gateway would accept this call for dispatch.
    pub dispatchable: bool,
    /// Violations found (empty when valid).
    pub errors: Vec<avm_mcp_tools::ValidationIssue>,
    /// Fingerprint of the schema revision the check used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_fingerprint: Option<String>,
}

/// Sub-router exposing the tool schema API. Merge it into the gateway router.
pub fn router(tools: ManagedToolSet) -> Router {
    let state = Arc::new(tools);
    Router::new()
        .route("/tools/schema", get(tools_schema))
        .route("/tools/schema/:name", get(tool_schema_one))
        .route("/tools/validate", post(validate))
        .with_state(state)
}

/// `GET /tools/schema`
pub async fn tools_schema(State(tools): State<Arc<ManagedToolSet>>) -> Json<ToolCatalog> {
    Json(tools.to_catalog())
}

/// `GET /tools/schema/:name`
pub async fn tool_schema_one(
    State(tools): State<Arc<ManagedToolSet>>,
    Path(name): Path<String>,
) -> Result<Json<avm_mcp_tools::ToolSchema>, (StatusCode, Json<serde_json::Value>)> {
    match tools.get(&name) {
        Some(schema) => Ok(Json(schema.clone())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("unknown tool `{name}`") })),
        )),
    }
}

/// `POST /tools/validate`
pub async fn validate(
    State(tools): State<Arc<ManagedToolSet>>,
    Json(req): Json<ValidateRequest>,
) -> (StatusCode, Json<ValidateResponse>) {
    let report = match (&req.arguments, &req.arguments_json) {
        (Some(args), _) => tools.validate_call(&req.tool_name, args),
        (None, Some(raw)) => tools.validate_wire_call(&req.tool_name, raw),
        (None, None) => tools.validate_call(&req.tool_name, &serde_json::json!({})),
    };

    let known = tools.get(&req.tool_name).is_some();
    let status = if report.valid {
        StatusCode::OK
    } else if known {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::NOT_FOUND
    };

    (
        status,
        Json(ValidateResponse {
            tool_name: report.tool,
            valid: report.valid,
            dispatchable: report.valid && known,
            errors: report.errors,
            schema_fingerprint: report.schema_fingerprint,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> State<Arc<ManagedToolSet>> {
        State(Arc::new(ManagedToolSet::with_builtins()))
    }

    #[tokio::test]
    async fn schema_endpoint_lists_every_builtin() {
        let Json(catalog) = tools_schema(state()).await;
        assert_eq!(catalog.catalog_version, avm_mcp_tools::CATALOG_VERSION);
        assert_eq!(catalog.dialect, avm_mcp_tools::SCHEMA_DIALECT);
        let names: Vec<&str> = catalog.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"avm_memory_read"));
        assert!(names.contains(&"avm_job_submit"));
        assert!(catalog.tools.iter().all(|t| !t.fingerprint.is_empty()));
    }

    #[tokio::test]
    async fn single_tool_lookup() {
        let ok = tool_schema_one(state(), Path("avm_job_status".into())).await;
        assert!(ok.is_ok());
        let missing = tool_schema_one(state(), Path("ghost".into())).await;
        assert_eq!(missing.err().unwrap().0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn validate_accepts_a_well_formed_call() {
        let (status, Json(body)) = validate(
            state(),
            Json(ValidateRequest {
                tool_name: "avm_job_status".into(),
                arguments: Some(json!({ "job_id": "job_1" })),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.valid && body.dispatchable);
        assert!(body.errors.is_empty());
    }

    #[tokio::test]
    async fn validate_rejects_bad_arguments() {
        let (status, Json(body)) = validate(
            state(),
            Json(ValidateRequest {
                tool_name: "avm_job_status".into(),
                arguments: Some(json!({ "job_id": 42 })),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!body.valid && !body.dispatchable);
        assert_eq!(body.errors[0].path, "/job_id");
    }

    #[tokio::test]
    async fn validate_accepts_the_wire_form() {
        let (status, Json(body)) = validate(
            state(),
            Json(ValidateRequest {
                tool_name: "avm_job_status".into(),
                arguments_json: Some(r#"{"job_id":"job_1"}"#.into()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.valid);
    }

    #[tokio::test]
    async fn unknown_tool_is_not_found() {
        let (status, Json(body)) = validate(
            state(),
            Json(ValidateRequest {
                tool_name: "ghost".into(),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.dispatchable);
    }
}
