//! `avm-gateway` entrypoint.

use std::sync::Arc;

use avm_gateway::{a2a, A2AState, ManagedToolSet, McpRouter, StaticCardRegistry};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "avm-gateway", about = "AVM MCP gateway")]
struct Args {
    #[arg(long, env = "AVM_GATEWAY_ADDR", default_value = "0.0.0.0:8080")]
    listen_addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Platform telemetry is always on: traces are 100 % sampled and metrics are
    // always registered. Export is a no-op until OTEL_EXPORTER_OTLP_ENDPOINT is
    // set, so a laptop run costs an atomic add per instrument.
    let otel = avm_otel::init_otel("avm-gateway", env!("CARGO_PKG_VERSION"));
    let args = Args::parse();

    // Built-in tool signatures are generated from their argument structs;
    // upstream MCP servers are ingested into this set as they connect.
    let tools = ManagedToolSet::with_builtins();
    tracing::info!(tools = tools.len(), "tool schema catalog ready");

    // Agent Card registry backing A2A scope validation. Empty at boot: cards
    // are registered as agents come up, and an unresolved target is denied
    // rather than waved through (see `a2a::A2AState::authorize`).
    let cards = Arc::new(StaticCardRegistry::new());

    let app = avm_gateway::router_with_tools(McpRouter::new(), tools)
        .merge(a2a::router(A2AState::new(cards)));
    let listener = tokio::net::TcpListener::bind(&args.listen_addr).await?;

    tracing::info!(
        addr = %args.listen_addr,
        otel_export = otel.export_enabled(),
        sampler = otel.sampler().as_str(),
        "avm-gateway listening"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
