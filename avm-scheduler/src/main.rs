//! `avm-scheduler` entrypoint.

use avm_queue::Publisher;
use avm_scheduler::{Scheduler, SchedulerConfig};
use avm_storage::db::{self, DbConfig};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "avm-scheduler", about = "AVM job scheduler / reconciler")]
struct Args {
    #[arg(long, env = "DATABASE_URL", default_value = "postgres://avm:avm@localhost:5432/avm")]
    database_url: String,

    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Reconcile interval in seconds.
    #[arg(long, default_value_t = 5)]
    tick_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let otel = avm_otel::init_otel("avm-scheduler", env!("CARGO_PKG_VERSION"));
    let args = Args::parse();

    let pool = db::connect(&DbConfig { url: args.database_url, ..DbConfig::from_env() }).await?;
    let publisher = Publisher::connect(&args.nats_url).await?;

    let cfg = SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(args.tick_secs),
        ..SchedulerConfig::default()
    };

    tracing::info!(
        tick_secs = args.tick_secs,
        otel_export = otel.export_enabled(),
        "avm-scheduler ready"
    );
    Scheduler::new(pool, publisher, cfg).run().await
}
