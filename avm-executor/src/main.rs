//! `avm-executor` entrypoint.

use avm_executor::{Executor, ExecutorConfig};
use avm_queue::{Publisher, Subscriber};
use avm_storage::db::{self, DbConfig};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "avm-executor", about = "AVM agent executor / process pool")]
struct Args {
    #[arg(long, env = "DATABASE_URL", default_value = "postgres://avm:avm@localhost:5432/avm")]
    database_url: String,

    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Durable JetStream consumer name (shared across replicas of a pool).
    #[arg(long, env = "AVM_POOL", default_value = "avm-executor")]
    pool: String,

    /// Optional subject filter, e.g. `avm.jobs.t_acme.>`.
    #[arg(long)]
    filter: Option<String>,

    /// Max concurrent agent processes.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let otel = avm_otel::init_otel("avm-executor", env!("CARGO_PKG_VERSION"));
    let args = Args::parse();

    let pool = db::connect(&DbConfig { url: args.database_url, ..DbConfig::from_env() }).await?;
    let publisher = Publisher::connect(&args.nats_url).await?;
    let subscriber = Subscriber::connect(&args.nats_url, &args.pool, args.filter.as_deref()).await?;

    let cfg = ExecutorConfig { max_concurrency: args.concurrency, ..ExecutorConfig::default() };
    tracing::info!(
        pool = %args.pool,
        executor_id = %cfg.executor_id,
        otel_export = otel.export_enabled(),
        "avm-executor ready"
    );

    Executor::new(pool, publisher, subscriber, cfg).run().await
}
