//! `avm-gateway` entrypoint.

use avm_gateway::{app, A2AState, ManagedToolSet, McpRouter};
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

    // Built-in tool signatures are generated from their argument structs;
    // upstream MCP servers are ingested into this set as they connect.
    let tools = ManagedToolSet::with_builtins();
    tracing::info!(tools = tools.len(), "tool schema catalog ready");

    let app = app(McpRouter::new(), tools, A2AState::default());
    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;

    tracing::info!(addr = %args.listen_addr, "avm-gateway listening");
    axum::serve(listener, app).await?;
    Ok(())
}
