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
//! It also carries the agent's A2A surface: Agent Card discovery
//! ([`a2a_router`]) plus agent-to-agent dispatch (`POST /a2a/task`), which is
//! authorized before it is routed — see [`security::validate_a2a_dispatch`].

pub mod a2a;
pub mod a2a_router;
pub mod mcp_router;
pub mod security;
pub mod tools_api;

use axum::Router;

pub use a2a::{
    A2AErrorBody, A2ARejection, A2AState, A2ATaskAccepted, A2ATaskRequest, CardResolver,
    StaticCardRegistry,
};
pub use a2a_router::CardState;
pub use avm_mcp_tools::{ManagedToolSet, ToolSchema, ToolSource};
pub use mcp_router::{router, McpRouter, ToolCall, ToolResult};
pub use security::{
    validate_a2a_dispatch, A2ADispatch, AuthError, ScopeError, SecurityAudit, SecurityError,
    SecurityEvent, TracingAudit,
};

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

/// MCP dispatch plus the tool-schema introspection API.
///
/// Kept separate from [`mcp_router::router`] so each sub-router owns its own
/// state and neither has to know about the other.
pub fn router_with_tools(mcp: McpRouter, tools: ManagedToolSet) -> Router {
    mcp_router::router(mcp).merge(tools_api::router(tools))
}

/// The full gateway application: MCP tool routing, tool schemas, and the A2A
/// surface (Agent Card discovery + inbound tasks).
pub fn app(
    mcp: McpRouter,
    tools: ManagedToolSet,
    a2a: A2AState,
    cards: CardState,
) -> Router {
    router_with_tools(mcp, tools)
        .merge(a2a::router(a2a))
        .merge(a2a_router::router(cards))
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
    fn app_merges_all_surfaces() {
        let _ = app(
            McpRouter::new(),
            ManagedToolSet::with_builtins(),
            A2AState::new(std::sync::Arc::new(StaticCardRegistry::new())),
            CardState::default(),
        );
    }
}
