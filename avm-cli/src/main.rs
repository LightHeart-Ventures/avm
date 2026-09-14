//! `avm` — operator CLI for the AVM control plane.

use avm_proto::types::{JobMessage, Scope};
use avm_queue::Publisher;
use avm_storage::{
    db::{self, DbConfig},
    jobs::{self, NewJob},
    memories,
};
use chrono::Utc;
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "avm", version, about = "AVM command-line tool")]
struct Cli {
    #[arg(long, env = "DATABASE_URL", default_value = "postgres://avm:avm@localhost:5432/avm", global = true)]
    database_url: String,

    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222", global = true)]
    nats_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply pending SQL migrations.
    Migrate,
    /// Submit a job and publish it to JetStream.
    Submit {
        #[arg(long)]
        tenant: String,
        #[arg(long, default_value = "")]
        project: String,
        #[arg(long)]
        agent: String,
        /// JSON payload handed to the agent on stdin.
        #[arg(long, default_value = "{}")]
        payload: String,
        #[arg(long, default_value_t = 100)]
        priority: i32,
    },
    /// Show one job.
    Job {
        job_id: String,
    },
    /// List recent jobs for a tenant.
    Jobs {
        #[arg(long, default_value = "")]
        tenant: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// List memories visible to a scope.
    Memories {
        #[arg(long)]
        tenant: String,
        #[arg(long, default_value = "")]
        project: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    avm_observability::init("avm-cli");
    let cli = Cli::parse();
    let pool = db::connect(&DbConfig { url: cli.database_url.clone(), ..DbConfig::from_env() }).await?;

    match cli.command {
        Command::Migrate => {
            db::migrate(&pool).await?;
            println!("migrations applied");
        }
        Command::Submit { tenant, project, agent, payload, priority } => {
            let scope = if project.is_empty() {
                Scope::tenant(&tenant)
            } else {
                Scope::project(&tenant, &project)
            };
            let job_id = format!("job_{}", Uuid::new_v4().simple());

            jobs::create(
                &pool,
                &NewJob {
                    job_id: job_id.clone(),
                    scope: scope.clone(),
                    agent_id: agent.clone(),
                    payload: serde_json::from_str(&payload)?,
                    priority,
                    max_retries: 3,
                    idempotency_key: None,
                },
            )
            .await?;

            Publisher::connect(&cli.nats_url)
                .await?
                .publish_job(&JobMessage {
                    job_id: job_id.clone(),
                    scope,
                    agent_id: agent,
                    payload,
                    created_at: Utc::now().to_rfc3339(),
                })
                .await?;

            println!("{job_id}");
        }
        Command::Job { job_id } => {
            let row = jobs::get(&pool, &job_id).await?;
            println!(
                "{}\t{}\tretries={}/{}\t{}",
                row.job_id,
                row.status,
                row.retry_count,
                row.max_retries,
                row.last_error.unwrap_or_default()
            );
        }
        Command::Jobs { tenant, status, limit } => {
            let scope = Scope::tenant(&tenant);
            for row in jobs::list(&pool, &scope, status.as_deref(), limit).await? {
                println!("{}\t{}\t{}\t{}", row.job_id, row.status, row.agent_id, row.created_at);
            }
        }
        Command::Memories { tenant, project, limit } => {
            let scope = if project.is_empty() {
                Scope::tenant(&tenant)
            } else {
                Scope::project(&tenant, &project)
            };
            for row in memories::list_visible(&pool, &scope, &[], limit).await? {
                println!("{}\t{}\t{}", row.memory_id, row.scope, row.updated_at);
            }
        }
    }

    Ok(())
}
