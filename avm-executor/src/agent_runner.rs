//! Agent process runner (fork + exec with a wall-clock guard).
//!
//! The agent contract is deliberately dumb so any language works:
//! * the job payload arrives on **stdin** as JSON,
//! * scope is exported as `AVM_*` environment variables,
//! * the agent writes its result to **stdout** and exits 0 on success.

use std::process::Stdio;
use std::time::Duration;

use avm_proto::types::Scope;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// What to execute.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub job_id: String,
    pub agent_id: String,
    pub payload: String,
    pub scope: Scope,
}

/// Successful process result.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Failure modes of a single agent run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("agent binary not resolvable for agent {0}")]
    Unresolved(String),
    #[error("spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("agent exited with code {code}: {stderr}")]
    NonZeroExit { code: i32, stderr: String },
    #[error("agent exceeded wall time of {0}s")]
    Timeout(u64),
}

/// Forks agent processes with a per-run timeout.
#[derive(Debug, Clone)]
pub struct AgentRunner {
    wall_time_sec: u64,
    /// Directory holding agent entrypoints; `AVM_AGENT_DIR` overrides it.
    agent_dir: String,
}

impl AgentRunner {
    pub fn new(wall_time_sec: u64) -> Self {
        Self {
            wall_time_sec,
            agent_dir: std::env::var("AVM_AGENT_DIR").unwrap_or_else(|_| "/opt/avm/agents".into()),
        }
    }

    /// Resolve the executable for an agent id.
    ///
    /// TODO(avm): look this up via `AgentService` (registry) instead of by path.
    pub fn resolve(&self, agent_id: &str) -> Result<String, RunError> {
        if agent_id.is_empty() || agent_id.contains('/') {
            return Err(RunError::Unresolved(agent_id.to_string()));
        }
        Ok(format!(
            "{}/{}",
            self.agent_dir.trim_end_matches('/'),
            agent_id
        ))
    }

    /// Spawn the agent, stream the payload to stdin, collect stdout/stderr.
    pub async fn run(&self, spec: &RunSpec) -> Result<RunOutcome, RunError> {
        let bin = self.resolve(&spec.agent_id)?;

        tracing::info!(
            job_id = %spec.job_id,
            agent_id = %spec.agent_id,
            bin = %bin,
            "spawning agent process"
        );

        let mut child = Command::new(&bin)
            .env("AVM_JOB_ID", &spec.job_id)
            .env("AVM_AGENT_ID", &spec.agent_id)
            .env("AVM_SCOPE", &spec.scope.level)
            .env("AVM_TENANT_ID", &spec.scope.tenant_id)
            .env("AVM_PROJECT_ID", &spec.scope.project_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(spec.payload.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let output = tokio::time::timeout(
            Duration::from_secs(self.wall_time_sec),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| RunError::Timeout(self.wall_time_sec))??;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let code = output.status.code().unwrap_or(-1);

        if code != 0 {
            return Err(RunError::NonZeroExit { code, stderr });
        }

        Ok(RunOutcome {
            stdout,
            stderr,
            exit_code: code,
        })
    }
}

pub mod isolation {
    //! Resource isolation (cgroup v2 / rlimit) applied before exec.
    //!
    //! TODO(avm): apply cgroup v2 cpu.max + memory.max and setrlimit before
    //! exec. Linux-only; gate behind `#[cfg(target_os = "linux")]`.

    /// Requested limits for a single agent process.
    #[derive(Debug, Clone, Copy)]
    pub struct Limits {
        pub cpu_millicores: u32,
        pub memory_bytes: u64,
        pub max_open_files: u32,
    }

    impl Default for Limits {
        fn default() -> Self {
            Self {
                cpu_millicores: 500,
                memory_bytes: 512 * 1024 * 1024,
                max_open_files: 1024,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_path_traversal() {
        let runner = AgentRunner::new(60);
        assert!(runner.resolve("../../bin/sh").is_err());
        assert!(runner.resolve("").is_err());
        assert!(runner.resolve("ag_ok").is_ok());
    }
}
