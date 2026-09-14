//! Agent process runner (fork + exec with a wall-clock guard).
//!
//! The agent contract is deliberately dumb so any language works:
//! * the job payload arrives on **stdin** as JSON,
//! * scope is exported as `AVM_*` environment variables,
//! * the agent writes its result to **stdout** and exits 0 on success.
//!
//! When the job's instrumentation level is `detailed`, the runner also exports
//! W3C trace context (`TRACEPARENT`, `OTEL_TRACE_ID`, `OTEL_SPAN_ID`,
//! `OTEL_PARENT_SPAN_ID`) so an instrumented agent's own spans join the AVM
//! trace, and it captures the agent's stderr as correlated log events.

use std::process::Stdio;
use std::time::Duration;

use avm_otel::propagation;
use avm_otel::{InstrumentationLevel, TraceContext};
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
    /// Parent trace context, when the dispatching trace was sampled.
    pub trace: Option<TraceContext>,
    /// Instrumentation depth resolved at the gateway.
    pub instrumentation: InstrumentationLevel,
}

impl RunSpec {
    /// Minimal spec: no trace context, platform telemetry only.
    pub fn new(job_id: impl Into<String>, agent_id: impl Into<String>, payload: impl Into<String>, scope: Scope) -> Self {
        Self {
            job_id: job_id.into(),
            agent_id: agent_id.into(),
            payload: payload.into(),
            scope,
            trace: None,
            instrumentation: InstrumentationLevel::Off,
        }
    }

    /// Environment variables carrying trace context into the agent process.
    ///
    /// Empty unless the level is `detailed` **and** a sampled parent exists —
    /// a tenant that did not opt in never sees AVM trace ids in its process
    /// environment.
    pub fn trace_env(&self) -> Vec<(&'static str, String)> {
        match (&self.trace, self.instrumentation.agent_tracing()) {
            (Some(ctx), true) => ctx.agent_env(&propagation::new_span_id()),
            _ => Vec::new(),
        }
    }
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

impl RunError {
    /// Low-cardinality outcome label for metrics.
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Unresolved(_) => "unresolved",
            Self::Spawn(_) => "spawn_failed",
            Self::NonZeroExit { .. } => "failed",
            Self::Timeout(_) => "timeout",
        }
    }
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
        Ok(format!("{}/{}", self.agent_dir.trim_end_matches('/'), agent_id))
    }

    /// Spawn the agent, stream the payload to stdin, collect stdout/stderr.
    pub async fn run(&self, spec: &RunSpec) -> Result<RunOutcome, RunError> {
        let bin = self.resolve(&spec.agent_id)?;

        // `container.pull_image` in the trace vocabulary: resolving/staging the
        // agent image or binary before it runs.
        let pull = tracing::debug_span!(
            "container.pull_image",
            job_id = %spec.job_id,
            agent_id = %spec.agent_id,
        );
        drop(pull);

        let span = tracing::info_span!(
            "container.run",
            otel.kind = "internal",
            job_id = %spec.job_id,
            agent_id = %spec.agent_id,
            tenant_id = %spec.scope.tenant_id,
            project_id = %spec.scope.project_id,
            instrumentation_level = spec.instrumentation.as_str(),
        );
        let _entered = span.enter();

        tracing::info!(
            job_id = %spec.job_id,
            agent_id = %spec.agent_id,
            bin = %bin,
            "spawning agent process"
        );

        let mut cmd = Command::new(&bin);
        cmd.env("AVM_JOB_ID", &spec.job_id)
            .env("AVM_AGENT_ID", &spec.agent_id)
            .env("AVM_SCOPE", &spec.scope.level)
            .env("AVM_TENANT_ID", &spec.scope.tenant_id)
            .env("AVM_PROJECT_ID", &spec.scope.project_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        for (key, value) in spec.trace_env() {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn()?;

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

        // Agent output is only re-emitted as AVM log events when the tenant
        // opted into `detailed`. Otherwise it stays in the result envelope.
        if spec.instrumentation.agent_tracing() {
            for line in stderr.lines().take(MAX_CAPTURED_LINES) {
                tracing::info!(
                    job_id = %spec.job_id,
                    agent_id = %spec.agent_id,
                    stream = "stderr",
                    message = %line,
                    "agent log"
                );
            }
        }

        // Route A of the agent metric contract: a `metrics` object in the
        // result envelope is ingested into the tenant namespace. Only from
        // `basic` upward — at `off` the agent's numbers are ignored entirely.
        if spec.instrumentation.tenant_metrics() {
            ingest_agent_metrics(&stdout, spec);
        }

        // `container.cleanup`: the child is reaped by `wait_with_output`, the
        // pipes close on drop. Recorded so the trace shape is stable across
        // executor backends that do have teardown work.
        let _cleanup = tracing::debug_span!("container.cleanup", job_id = %spec.job_id).entered();

        if code != 0 {
            return Err(RunError::NonZeroExit { code, stderr });
        }

        Ok(RunOutcome { stdout, stderr, exit_code: code })
    }
}

/// Cap on agent log lines re-emitted per run, so a chatty agent cannot flood
/// the log pipeline.
const MAX_CAPTURED_LINES: usize = 200;

/// Cap on distinct metric keys accepted from one agent run. An agent that
/// derives keys from its input would otherwise mint a metric series per job.
const MAX_AGENT_METRICS: usize = 32;

/// Parse `{"metrics": {...}}` out of an agent's stdout and record each numeric
/// entry as `avm_agent_<key>`, labelled with the job's identity.
///
/// Deliberately forgiving: stdout that is not JSON, or has no `metrics` object,
/// is not an error — most agents publish nothing. Non-numeric values are
/// dropped rather than rejected, so one bad field cannot cost an agent its
/// whole metric set.
fn ingest_agent_metrics(stdout: &str, spec: &RunSpec) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return;
    };
    let Some(entries) = value.get("metrics").and_then(|m| m.as_object()) else {
        return;
    };

    let registry = avm_otel::metrics::registry();
    let labels = [
        ("tenant_id", spec.scope.tenant_id.as_str()),
        ("project_id", spec.scope.project_id.as_str()),
        ("agent_id", spec.agent_id.as_str()),
    ];

    for (key, raw) in entries.iter().take(MAX_AGENT_METRICS) {
        let Some(number) = raw.as_f64() else { continue };
        let name = format!("avm_agent_{}", avm_otel::metrics::sanitize_name(key));
        registry.register(
            &name,
            avm_otel::metrics::MetricKind::Gauge,
            "custom metric published by an agent result envelope",
            &[],
        );
        registry.gauge_set(&name, &labels, number);
    }
}

/// Resource isolation (cgroup v2 / rlimit) applied before exec.
pub mod isolation {
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
            Self { cpu_millicores: 500, memory_bytes: 512 * 1024 * 1024, max_open_files: 1024 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec::new("job_1", "ag_ok", "{}", Scope::system())
    }

    #[test]
    fn resolve_rejects_path_traversal() {
        let runner = AgentRunner::new(60);
        assert!(runner.resolve("../../bin/sh").is_err());
        assert!(runner.resolve("").is_err());
        assert!(runner.resolve("ag_ok").is_ok());
    }

    #[test]
    fn trace_env_is_empty_without_opt_in() {
        let mut s = spec();
        s.trace = Some(TraceContext::root_from_seed(&[7u8; 16], true));
        // Level `off` and `basic` must not leak trace ids into the process.
        assert!(s.trace_env().is_empty());
        s.instrumentation = InstrumentationLevel::Basic;
        assert!(s.trace_env().is_empty());
    }

    #[test]
    fn detailed_injects_w3c_context() {
        let mut s = spec();
        let parent = TraceContext::root_from_seed(&[7u8; 16], true);
        s.trace = Some(parent.clone());
        s.instrumentation = InstrumentationLevel::Detailed;

        let env = s.trace_env();
        let names: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        assert!(names.contains(&propagation::ENV_TRACE_ID));
        assert!(names.contains(&propagation::ENV_SPAN_ID));
        assert!(names.contains(&propagation::ENV_PARENT_SPAN_ID));

        let trace_id = env
            .iter()
            .find(|(k, _)| *k == propagation::ENV_TRACE_ID)
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(trace_id, parent.trace_id, "agent must join the AVM trace");
    }

    #[test]
    fn detailed_without_a_sampled_parent_injects_nothing() {
        let mut s = spec();
        s.instrumentation = InstrumentationLevel::Detailed;
        assert!(s.trace_env().is_empty());
    }

    #[test]
    fn outcome_labels_are_low_cardinality() {
        assert_eq!(RunError::Timeout(60).outcome(), "timeout");
        assert_eq!(
            RunError::NonZeroExit { code: 2, stderr: "boom".into() }.outcome(),
            "failed"
        );
    }

    #[test]
    fn agent_metrics_are_ingested_from_the_result_envelope() {
        let mut s = spec();
        s.agent_id = "metric-emitter".into();
        s.scope.tenant_id = "t-ingest".into();
        s.scope.project_id = "p-ingest".into();

        ingest_agent_metrics(
            r#"{"output":"ok","metrics":{"tokens.used":1234,"cache.hit_ratio":0.5}}"#,
            &s,
        );

        let labels = [
            ("tenant_id", "t-ingest"),
            ("project_id", "p-ingest"),
            ("agent_id", "metric-emitter"),
        ];
        let registry = avm_otel::metrics::registry();
        assert_eq!(registry.value("avm_agent_tokens_used", &labels), Some(1234.0));
        assert_eq!(registry.value("avm_agent_cache_hit_ratio", &labels), Some(0.5));
    }

    #[test]
    fn non_json_and_non_numeric_agent_output_is_ignored() {
        let mut s = spec();
        s.agent_id = "noisy".into();
        s.scope.tenant_id = "t-noisy".into();

        // Neither of these may panic, and neither may mint a series.
        ingest_agent_metrics("not json at all", &s);
        ingest_agent_metrics(r#"{"metrics":{"note":"hello"}}"#, &s);

        let registry = avm_otel::metrics::registry();
        assert_eq!(registry.series_count("avm_agent_note"), 0);
    }
}
