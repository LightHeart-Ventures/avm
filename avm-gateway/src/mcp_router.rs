//! Tool routing for MCP calls.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

/// An inbound MCP tool invocation.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// Caller scope, injected by the executor when it spawns the agent.
    #[serde(default)]
    pub scope: avm_proto::types::Scope,
}

/// Result handed back to the agent.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub tool: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where a tool name resolves to.
#[derive(Debug, Clone)]
pub enum Route {
    /// Handled inside the gateway (memory reads, job submission, ...).
    Local,
    /// Proxied to an upstream MCP server.
    Upstream { url: String },
}

/// Name → destination table.
#[derive(Debug, Clone, Default)]
pub struct McpRouter {
    routes: HashMap<String, Route>,
}

impl McpRouter {
    pub fn new() -> Self {
        let mut routes = HashMap::new();
        for local in [
            "avm_memory_read",
            "avm_memory_write",
            "avm_job_submit",
            "avm_job_status",
        ] {
            routes.insert(local.to_string(), Route::Local);
        }
        Self { routes }
    }

    /// Register an upstream MCP server for a tool name.
    pub fn register_upstream(&mut self, tool: impl Into<String>, url: impl Into<String>) {
        self.routes
            .insert(tool.into(), Route::Upstream { url: url.into() });
    }

    /// Resolve a tool name.
    pub fn resolve(&self, tool: &str) -> Option<&Route> {
        self.routes.get(tool)
    }

    /// Every tool this gateway exposes.
    pub fn tool_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.routes.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// Dispatch a call.
    ///
    /// TODO(avm): implement the local handlers against `avm_storage` and the
    /// upstream proxy over streamable-HTTP MCP.
    pub async fn dispatch(&self, call: ToolCall) -> ToolResult {
        match self.resolve(&call.tool) {
            Some(Route::Local) => ToolResult {
                tool: call.tool,
                ok: false,
                content: None,
                error: Some("local tool handler not implemented yet".into()),
            },
            Some(Route::Upstream { url }) => ToolResult {
                tool: call.tool,
                ok: false,
                content: None,
                error: Some(format!("upstream proxy to {url} not implemented yet")),
            },
            None => ToolResult {
                tool: call.tool.clone(),
                ok: false,
                content: None,
                error: Some(format!("unknown tool {}", call.tool)),
            },
        }
    }
}

/// Build the axum router.
pub fn router(mcp: McpRouter) -> Router {
    let state = Arc::new(mcp);
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/mcp/tools", get(list_tools))
        .route("/mcp/call", post(call_tool))
        .with_state(state)
}

async fn list_tools(State(mcp): State<Arc<McpRouter>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "tools": mcp.tool_names() }))
}

async fn call_tool(
    State(mcp): State<Arc<McpRouter>>,
    Json(call): Json<ToolCall>,
) -> Json<ToolResult> {
    Json(mcp.dispatch(call).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_tools_are_registered() {
        let r = McpRouter::new();
        assert!(matches!(r.resolve("avm_memory_read"), Some(Route::Local)));
        assert!(r.resolve("nope").is_none());
    }

    #[test]
    fn upstream_registration_wins() {
        let mut r = McpRouter::new();
        r.register_upstream("atum_list_projects", "https://example.invalid/mcp");
        assert!(matches!(
            r.resolve("atum_list_projects"),
            Some(Route::Upstream { .. })
        ));
    }
}
