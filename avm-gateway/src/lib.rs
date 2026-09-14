//! MCP gateway.
//!
//! Terminates agent MCP tool calls over HTTP and routes each one to a local
//! handler or an upstream MCP server, enforcing the caller's scope.

pub mod a2a;
pub mod mcp_router;
pub mod security;

pub use a2a::{
    A2AErrorBody, A2ARejection, A2AState, A2ATaskAccepted, A2ATaskRequest, CardResolver,
    StaticCardRegistry,
};
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
