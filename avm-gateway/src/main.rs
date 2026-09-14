//! `avm-gateway` entrypoint.

use std::sync::Arc;

use avm_gateway::{a2a, mcp_router, A2AState, McpRouter, StaticCardRegistry};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "avm-gateway", about = "AVM MCP gateway")]
struct Args {
    #[arg(long, env = "AVM_GATEWAY_ADDR", default_value = "0.0.0.0:8080")]
    listen_addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    avm_observability::init("avm-gateway");
    let args = Args::parse();

    // Agent Card registry backing A2A scope validation. Empty at boot: cards
    // are registered as agents come up, and an unresolved target is denied
    // rather than waved through (see `a2a::A2AState::authorize`).
    let cards = Arc::new(StaticCardRegistry::new());

    let app = mcp_router::router(McpRouter::new()).merge(a2a::router(A2AState::new(cards)));
    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;

    tracing::info!(addr = %args.listen_addr, "avm-gateway listening");
    axum::serve(listener, app).await?;
    Ok(())
}
