//! `avm-server` entrypoint: gRPC control plane.

use avm_queue::Publisher;
use avm_server::{AppState, ServerConfig};
use avm_storage::db::{self, DbConfig};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "avm-server", about = "AVM gRPC control plane")]
struct Args {
    /// Address to bind the gRPC listener to.
    #[arg(long, env = "AVM_LISTEN_ADDR", default_value = "0.0.0.0:50051")]
    listen_addr: String,

    /// Postgres connection string.
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "postgres://avm:avm@localhost:5432/avm"
    )]
    database_url: String,

    /// NATS endpoint.
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Apply pending migrations on startup.
    #[arg(long, default_value_t = true)]
    migrate: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    avm_observability::init("avm-server");
    let args = Args::parse();

    let cfg = ServerConfig {
        listen_addr: args.listen_addr,
        database_url: args.database_url,
        nats_url: args.nats_url,
        run_migrations: args.migrate,
    };

    let pool = db::connect(&DbConfig {
        url: cfg.database_url.clone(),
        ..DbConfig::from_env()
    })
    .await?;

    if cfg.run_migrations {
        db::migrate(&pool).await?;
    }

    let publisher = Publisher::connect(&cfg.nats_url).await?;
    let _state = AppState::new(pool, publisher);

    tracing::info!(addr = %cfg.listen_addr, "avm-server ready");

    // TODO(avm): serve the generated tonic services once protoc codegen is on:
    //   tonic::transport::Server::builder()
    //       .add_service(JobServiceServer::new(JobSvc::new(_state.clone())))
    //       .serve(cfg.listen_addr.parse()?)
    //       .await?;
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutdown signal received");
    Ok(())
}
