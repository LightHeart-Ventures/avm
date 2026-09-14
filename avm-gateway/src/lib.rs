//! MCP gateway.
//!
//! Terminates agent MCP tool calls over HTTP and routes each one to a local
//! handler or an upstream MCP server, enforcing the caller's scope.
//!
//! Alongside dispatch it publishes **tool signatures**: every tool the gateway
//! can reach is registered in an [`avm_mcp_tools::ManagedToolSet`] with a
//! JSON-Schema contract, served over `GET /tools/schema` and enforced on
//! `POST /tools/validate`.
//!
//! Every router built here carries the always-on observability layer from
//! [`observability`]: a span and a latency observation per request, plus the
//! `GET /metrics` scrape endpoint and the tenant instrumentation opt-in API.

pub mod mcp_router;
pub mod observability;
pub mod tools_api;

use axum::Router;

pub use avm_mcp_tools::{ManagedToolSet, ToolSchema, ToolSource};
pub use mcp_router::{router, McpRouter, ToolCall, ToolResult};
pub use observability::InstrumentationStore;

/// Gateway runtime configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub listen_addr: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8080".to_string(),
        }
    }
}

/// Full gateway router: MCP dispatch, the tool-schema introspection API, and
/// the observability surface.
///
/// Kept separate from [`mcp_router::router`] so each sub-router owns its own
/// state and neither has to know about the other.
pub fn router_with_tools(mcp: McpRouter, tools: ManagedToolSet) -> Router {
    router_with_observability(mcp, tools, InstrumentationStore::new())
}

/// Same as [`router_with_tools`], with an explicit instrumentation store so a
/// caller (or a test) can seed tenant opt-in rows.
pub fn router_with_observability(
    mcp: McpRouter,
    tools: ManagedToolSet,
    store: InstrumentationStore,
) -> Router {
    mcp_router::router(mcp)
        .merge(tools_api::router(tools))
        .merge(observability::router(store))
        // Outermost layer, so it sees the final status of every route above.
        .layer(axum::middleware::from_fn(observability::trace_requests))
}

/// Default wiring: the gateway's local routes plus their generated schemas.
///
/// Upstream MCP servers are layered on at connect time via
/// [`ManagedToolSet::ingest_listing`] and
/// [`McpRouter::register_upstream`].
pub fn default_router() -> Router {
    router_with_tools(McpRouter::new(), ManagedToolSet::with_builtins())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_routes_and_schemas_agree() {
        let router = McpRouter::new();
        let tools = ManagedToolSet::with_builtins();
        for name in router.tool_names() {
            assert!(
                tools.get(name).is_some(),
                "local route `{name}` has no published JSON-Schema signature"
            );
        }
    }

    #[test]
    fn default_router_builds_with_observability() {
        // Compile-time proof that the metrics + opt-in routes merge cleanly
        // with the MCP and tool-schema routers (duplicate paths would panic).
        let _ = default_router();
    }
}
