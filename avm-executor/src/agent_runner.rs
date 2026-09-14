//! Agent runner: resolves an agent id to an OCI image and runs it in a
//! [`Sandbox`](crate::sandbox::Sandbox).
//!
//! The agent contract is unchanged and deliberately dumb so any language works:
//! * the job payload arrives on **stdin** as JSON,
//! * scope is exported as `AVM_*` environment variables,
//! * the agent writes its result to **stdout** and exits 0 on success.
//!
//! What changed is the box, not the contract. Agents used to be `fork`/`exec`ed
//! from `<agent_dir>/<agent_id>` — a filesystem path built from a job field —
//! while model servers already ran as OCI containers. Now both go through the
//! sandbox abstraction, the default being an OCI container with a read-only
//! rootfs, dropped capabilities, explicit limits and no network beyond the
//! gateway. The fork/exec path survives as
//! [`ProcessSandbox`](crate::sandbox::ProcessSandbox) behind `AVM_SANDBOX=process`
//! for local development.
//!
//! When the job's instrumentation level is `detailed`, the runner also exports
//! W3C trace context (`TRACEPARENT`, `OTEL_TRACE_ID`, `OTEL_SPAN_ID`,
//! `OTEL_PARENT_SPAN_ID`) so an instrumented agent's own spans join the AVM
//! trace, and it captures the agent's stderr as correlated log events.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use avm_otel::propagation;
use avm_otel::{InstrumentationLevel, TraceContext};
use avm_proto::types::Scope;

use crate::model_server::{Mount, CONTAINER_BLOB_DIR, HOST_BLOB_DIR};
use crate::sandbox::{
    self, ImageRef, NetworkMode, ResourceLimits, Sandbox, SandboxError, SandboxKind, SandboxSpec,
    DEFAULT_PULL_TIMEOUT_SEC,
};

/// Registry used to build an image reference when an agent has no explicit
/// mapping. Overridable with `AVM_AGENT_REGISTRY`.
pub const DEFAULT_AGENT_REGISTRY: &str = "ghcr.io/lightheart/agents";
/// Runtime network an agent joins when it is handed a gateway credential.
pub const DEFAULT_GATEWAY_NETWORK: &str = "avm-gateway";
/// Max length of an agent id. Anything longer is a bug or an attack.
pub const MAX_AGENT_ID_LEN: usize = 128;

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
    /// Gateway base URL injected as `AVM_GATEWAY_URL`.
    pub gateway_url: Option<String>,
    /// Job-scoped bearer token injected as `AVM_GATEWAY_TOKEN`.
    ///
    /// TODO(avm): minting and verifying this token (audience = `job_id`,
    /// TTL = the job's wall time) is gateway work and is deliberately **not**
    /// done here — no token format or crypto scheme is invented in the
    /// executor. The caller injects the value; if it is absent, neither
    /// gateway variable is set and the agent simply has no network identity.
    pub gateway_token: Option<String>,
}

impl RunSpec {
    /// Minimal spec: no trace context, platform telemetry only, no gateway
    /// credential.
    pub fn new(
        job_id: impl Into<String>,
        agent_id: impl Into<String>,
        payload: impl Into<String>,
        scope: Scope,
    ) -> Self {
        Self {
            job_id: job_id.into(),
            agent_id: agent_id.into(),
            payload: payload.into(),
            scope,
            trace: None,
            instrumentation: InstrumentationLevel::Off,
            gateway_url: None,
            gateway_token: None,
        }
    }

    /// Attach the job-scoped gateway credential (builder style).
    pub fn with_gateway(mut self, url: impl Into<String>, token: impl Into<String>) -> Self {
        self.gateway_url = Some(url.into());
        self.gateway_token = Some(token.into());
        self
    }

    /// Environment variables carrying trace context into the agent.
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

    /// The full environment contract handed to the sandbox.
    ///
    /// The five `AVM_*` scope variables are exactly what fork/exec set, byte
    /// for byte. Gateway variables appear **only** when a token was supplied.
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("AVM_JOB_ID".to_string(), self.job_id.clone());
        env.insert("AVM_AGENT_ID".to_string(), self.agent_id.clone());
        env.insert("AVM_SCOPE".to_string(), self.scope.level.clone());
        env.insert("AVM_TENANT_ID".to_string(), self.scope.tenant_id.clone());
        env.insert("AVM_PROJECT_ID".to_string(), self.scope.project_id.clone());

        for (key, value) in self.trace_env() {
            env.insert(key.to_string(), value);
        }

        if let (Some(url), Some(token)) = (&self.gateway_url, &self.gateway_token) {
            env.insert("AVM_GATEWAY_URL".to_string(), url.clone());
            env.insert("AVM_GATEWAY_TOKEN".to_string(), token.clone());
        }
        env
    }

    /// Pod/container labels. These are what
    /// `docs/network-policies/allow-gateway-egress-only.yaml` selects on.
    pub fn labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("avm.io/workload".to_string(), "agent".to_string()),
            ("avm.io/job".to_string(), self.job_id.clone()),
            ("avm.io/agent".to_string(), self.agent_id.clone()),
            ("avm.io/tenant".to_string(), self.scope.tenant_id.clone()),
            ("avm.io/project".to_string(), self.scope.project_id.clone()),
        ])
    }
}

/// Successful run result.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Time spent staging the image (zero when already resident, and always
    /// zero on the fork/exec path).
    pub pull_duration: Duration,
    /// Time spent running the agent.
    pub run_duration: Duration,
}

/// Failure modes of a single agent run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("agent image not resolvable for agent {0}")]
    Unresolved(String),
    #[error("spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("agent exited with code {code}: {stderr}")]
    NonZeroExit { code: i32, stderr: String },
    #[error("agent exceeded wall time of {0}s")]
    Timeout(u64),
    #[error("sandbox failure: {0}")]
    Sandbox(#[source] SandboxError),
}

impl From<SandboxError> for RunError {
    fn from(err: SandboxError) -> Self {
        match err {
            // Preserve the historical timeout error shape/label.
            SandboxError::Timeout(d) => RunError::Timeout(d.as_secs()),
            other => RunError::Sandbox(other),
        }
    }
}

impl RunError {
    /// Low-cardinality outcome label for metrics.
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::Unresolved(_) => "unresolved",
            Self::Spawn(_) => "spawn_failed",
            Self::NonZeroExit { .. } => "failed",
            Self::Timeout(_) => "timeout",
            Self::Sandbox(err) => err.outcome(),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution: agent id -> OCI reference (never a filesystem path)
// ---------------------------------------------------------------------------

/// Maps an agent id to the image that implements it.
///
/// The return type is the whole point: an [`ImageRef`], not a path. There is no
/// longer a string interpolated into a filesystem location, so the traversal
/// surface the old `contains('/')` guard was defending is gone by construction.
pub trait AgentResolver: Send + Sync + std::fmt::Debug {
    fn resolve(&self, agent_id: &str) -> Result<ImageRef, RunError>;
}

/// Reject anything that is not a plain agent id.
///
/// Kept (and tightened) even though the path is gone: an id ends up in a
/// container name, a label value and a registry path, none of which want
/// arbitrary bytes.
pub fn validate_agent_id(agent_id: &str) -> Result<(), RunError> {
    let bad = agent_id.is_empty()
        || agent_id.len() > MAX_AGENT_ID_LEN
        || agent_id.starts_with('.')
        || agent_id.starts_with('-')
        || agent_id.contains("..")
        || !agent_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if bad {
        return Err(RunError::Unresolved(agent_id.to_string()));
    }
    Ok(())
}

/// Config-map backed resolver: an explicit `agent_id -> oci://…` table, with a
/// registry-plus-tag fallback for ids that have no entry.
///
/// TODO(avm): replace with a registry-backed resolver that asks `AgentService`
/// for the digest-pinned image of an agent revision. The trait boundary exists
/// so that is one impl swap, and callers do not change.
#[derive(Debug, Clone)]
pub struct ConfigMapResolver {
    images: BTreeMap<String, String>,
    registry: String,
    tag: String,
}

impl ConfigMapResolver {
    pub fn new(registry: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            images: BTreeMap::new(),
            registry: registry.into(),
            tag: tag.into(),
        }
    }

    /// Explicit mapping wins over the fallback. Prefer digest-pinned values.
    pub fn with_image(mut self, agent_id: impl Into<String>, image: impl Into<String>) -> Self {
        self.images.insert(agent_id.into(), image.into());
        self
    }

    /// Reads `AVM_AGENT_IMAGES` (`id=oci://…,id2=oci://…`), `AVM_AGENT_REGISTRY`
    /// and `AVM_AGENT_TAG`.
    pub fn from_env() -> Self {
        let registry =
            std::env::var("AVM_AGENT_REGISTRY").unwrap_or_else(|_| DEFAULT_AGENT_REGISTRY.into());
        let tag = std::env::var("AVM_AGENT_TAG").unwrap_or_else(|_| "latest".into());
        let mut resolver = Self::new(registry, tag);
        if let Ok(raw) = std::env::var("AVM_AGENT_IMAGES") {
            for entry in raw.split(',') {
                if let Some((id, image)) = entry.split_once('=') {
                    let (id, image) = (id.trim(), image.trim());
                    if !id.is_empty() && !image.is_empty() {
                        resolver.images.insert(id.to_string(), image.to_string());
                    }
                }
            }
        }
        resolver
    }
}

impl AgentResolver for ConfigMapResolver {
    fn resolve(&self, agent_id: &str) -> Result<ImageRef, RunError> {
        validate_agent_id(agent_id)?;
        let uri = match self.images.get(agent_id) {
            Some(uri) => uri.clone(),
            None => format!(
                "oci://{}/{}:{}",
                self.registry.trim_end_matches('/'),
                agent_id,
                self.tag
            ),
        };
        let image =
            ImageRef::parse(&uri).map_err(|_| RunError::Unresolved(agent_id.to_string()))?;
        if !image.is_digest_pinned() {
            tracing::warn!(
                agent_id,
                image = %image,
                "agent image is not digest-pinned; residency labels and cache sharing are unavailable"
            );
        }
        Ok(image)
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// Runs agents in a sandbox with a per-run wall-clock guard and an independent
/// image-pull budget.
#[derive(Clone)]
pub struct AgentRunner {
    wall_time_sec: u64,
    pull_timeout_sec: u64,
    limits: ResourceLimits,
    resolver: Arc<dyn AgentResolver>,
    sandbox: Arc<dyn Sandbox>,
    kind: SandboxKind,
    /// Host directory of the content-addressed blob store, mounted read-only.
    host_blob_dir: String,
    gateway_network: String,
}

impl std::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunner")
            .field("wall_time_sec", &self.wall_time_sec)
            .field("pull_timeout_sec", &self.pull_timeout_sec)
            .field("limits", &self.limits)
            .field("resolver", &self.resolver)
            .field("sandbox", &self.kind.as_str())
            .finish()
    }
}

impl AgentRunner {
    /// Default runner: OCI sandbox on the `runc` tier unless `AVM_SANDBOX` /
    /// `AVM_SANDBOX_RUNTIME` say otherwise.
    pub fn new(wall_time_sec: u64) -> Self {
        let kind = SandboxKind::from_env();
        let runtime = sandbox::SandboxRuntime::from_env();
        Self {
            wall_time_sec,
            pull_timeout_sec: std::env::var("AVM_PULL_TIMEOUT_SEC")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PULL_TIMEOUT_SEC),
            limits: ResourceLimits::default(),
            resolver: Arc::new(ConfigMapResolver::from_env()),
            sandbox: sandbox::build(kind, runtime),
            kind,
            host_blob_dir: std::env::var("AVM_MODEL_BLOB_DIR")
                .unwrap_or_else(|_| HOST_BLOB_DIR.to_string()),
            gateway_network: std::env::var("AVM_GATEWAY_NETWORK")
                .unwrap_or_else(|_| DEFAULT_GATEWAY_NETWORK.to_string()),
        }
    }

    /// Explicit wiring, for tests and for callers that build their own sandbox.
    pub fn with_sandbox(mut self, kind: SandboxKind, sandbox: Arc<dyn Sandbox>) -> Self {
        self.kind = kind;
        self.sandbox = sandbox;
        self
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn AgentResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Backend label for `avm_executor_container_duration_seconds`.
    pub fn executor_type(&self) -> &'static str {
        self.sandbox.backend()
    }

    /// Resolve the **image** for an agent id.
    ///
    /// Returns an OCI reference, never a filesystem path.
    pub fn resolve(&self, agent_id: &str) -> Result<ImageRef, RunError> {
        self.resolver.resolve(agent_id)
    }

    /// Build the sandbox spec for a run. Pure — the hardening contract is
    /// asserted in unit tests without any runtime present.
    pub fn sandbox_spec(&self, spec: &RunSpec) -> Result<SandboxSpec, RunError> {
        let image = self.resolve(&spec.agent_id)?;

        let network = if spec.gateway_token.is_some() && spec.gateway_url.is_some() {
            NetworkMode::GatewayOnly {
                network: self.gateway_network.clone(),
            }
        } else {
            // No credential, no network. The default posture is silence.
            NetworkMode::None
        };

        let mut sspec = SandboxSpec::new(container_name(&spec.job_id), &spec.agent_id, image);
        sspec.env = spec.env();
        sspec.labels = spec.labels();
        sspec.stdin = spec.payload.clone();
        sspec.limits = self.limits;
        sspec.mounts = vec![Mount::read_only(
            self.host_blob_dir.clone(),
            CONTAINER_BLOB_DIR,
        )];
        sspec.network = network;
        sspec.wall_timeout = Duration::from_secs(self.wall_time_sec);
        sspec.pull_timeout = Duration::from_secs(self.pull_timeout_sec);
        sspec.runtime = sandbox::SandboxRuntime::from_env();
        Ok(sspec)
    }

    /// Run the agent: stage the image, stream the payload to stdin, collect
    /// stdout/stderr.
    pub async fn run(&self, spec: &RunSpec) -> Result<RunOutcome, RunError> {
        let sandbox_spec = self.sandbox_spec(spec)?;

        let span = tracing::info_span!(
            "container.run",
            otel.kind = "internal",
            job_id = %spec.job_id,
            agent_id = %spec.agent_id,
            tenant_id = %spec.scope.tenant_id,
            project_id = %spec.scope.project_id,
            instrumentation_level = spec.instrumentation.as_str(),
            sandbox = self.kind.as_str(),
            image = %sandbox_spec.image,
        );
        let _entered = span.enter();

        tracing::info!(
            job_id = %spec.job_id,
            agent_id = %spec.agent_id,
            image = %sandbox_spec.image,
            sandbox = self.kind.as_str(),
            "starting agent sandbox"
        );

        let outcome = self.sandbox.run(&sandbox_spec).await?;

        let stdout = outcome.stdout;
        let stderr = outcome.stderr;
        let code = outcome.exit_code;

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

        tracing::debug!(
            job_id = %spec.job_id,
            pull_ms = outcome.pull_duration.as_millis() as u64,
            run_ms = outcome.run_duration.as_millis() as u64,
            "sandbox timing"
        );

        if code != 0 {
            return Err(RunError::NonZeroExit { code, stderr });
        }

        Ok(RunOutcome {
            stdout,
            stderr,
            exit_code: code,
            pull_duration: outcome.pull_duration,
            run_duration: outcome.run_duration,
        })
    }
}

/// Container name for a job. Deterministic, so teardown can always find it.
fn container_name(job_id: &str) -> String {
    let sanitized: String = job_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("avm-{sanitized}")
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

/// Resource isolation, now expressed as a runtime spec.
///
/// Kept as a compatibility shim: the limits it described are applied by
/// [`ResourceLimits`](crate::sandbox::ResourceLimits) through the container
/// runtime (`--memory`, `--cpus`, `--pids-limit`) rather than by hand-written
/// cgroup writes before exec.
pub mod isolation {
    /// Requested limits for a single agent workload.
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

    impl From<Limits> for crate::sandbox::ResourceLimits {
        fn from(l: Limits) -> Self {
            crate::sandbox::ResourceLimits {
                memory_bytes: l.memory_bytes,
                cpu_millis: l.cpu_millicores,
                pids: 128,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn spec() -> RunSpec {
        RunSpec::new("job_1", "ag_ok", "{}", Scope::system())
    }

    fn runner() -> AgentRunner {
        AgentRunner::new(60).with_resolver(Arc::new(
            ConfigMapResolver::new("ghcr.io/lightheart/agents", "latest").with_image(
                "ag_ok",
                format!("oci://ghcr.io/lightheart/agents/ag_ok@{D}"),
            ),
        ))
    }

    // -- resolution ---------------------------------------------------------

    #[test]
    fn resolve_returns_an_oci_reference_not_a_path() {
        let image = runner().resolve("ag_ok").unwrap();
        assert!(image.uri().starts_with("oci://"));
        assert!(image.is_digest_pinned());
        assert_eq!(image.digest(), Some(D));
        assert!(!image.uri().contains("/opt/avm/agents"));
    }

    #[test]
    fn resolve_falls_back_to_the_registry_template() {
        let image = runner().resolve("summarizer").unwrap();
        assert_eq!(
            image.uri(),
            "oci://ghcr.io/lightheart/agents/summarizer:latest"
        );
    }

    #[test]
    fn resolve_rejects_path_traversal_and_junk_ids() {
        let r = runner();
        for bad in [
            "",
            "../../bin/sh",
            "..",
            "a/b",
            "/etc/passwd",
            "ag ok",
            "ag\nok",
            "-flag",
            ".hidden",
            "ag;rm -rf /",
        ] {
            assert!(
                r.resolve(bad).is_err(),
                "resolve should have rejected `{bad}`"
            );
        }
        assert!(r.resolve(&"a".repeat(MAX_AGENT_ID_LEN + 1)).is_err());
        assert!(r.resolve("ag_ok").is_ok());
    }

    // -- spec construction --------------------------------------------------

    #[test]
    fn env_contract_matches_the_fork_exec_contract_exactly() {
        let mut s = spec();
        s.scope.tenant_id = "t-1".into();
        s.scope.project_id = "p-1".into();

        let env = s.env();
        assert_eq!(env.get("AVM_JOB_ID").map(String::as_str), Some("job_1"));
        assert_eq!(env.get("AVM_AGENT_ID").map(String::as_str), Some("ag_ok"));
        assert_eq!(
            env.get("AVM_SCOPE").map(String::as_str),
            Some(s.scope.level.as_str())
        );
        assert_eq!(env.get("AVM_TENANT_ID").map(String::as_str), Some("t-1"));
        assert_eq!(env.get("AVM_PROJECT_ID").map(String::as_str), Some("p-1"));
    }

    #[test]
    fn gateway_vars_appear_only_with_a_token() {
        let plain = spec();
        assert!(!plain.env().contains_key("AVM_GATEWAY_URL"));
        assert!(!plain.env().contains_key("AVM_GATEWAY_TOKEN"));

        // A URL without a token is not enough — no half-credentials.
        let mut url_only = spec();
        url_only.gateway_url = Some("http://avm-gateway:8080".into());
        assert!(!url_only.env().contains_key("AVM_GATEWAY_URL"));

        let credentialed = spec().with_gateway("http://avm-gateway:8080", "tok_abc");
        let env = credentialed.env();
        assert_eq!(
            env.get("AVM_GATEWAY_URL").map(String::as_str),
            Some("http://avm-gateway:8080")
        );
        assert_eq!(
            env.get("AVM_GATEWAY_TOKEN").map(String::as_str),
            Some("tok_abc")
        );
    }

    #[test]
    fn sandbox_spec_is_hardened_and_limited() {
        let r = runner();
        let sspec = r.sandbox_spec(&spec()).unwrap();
        let joined = sspec.run_args().unwrap().join(" ");

        assert!(joined.contains("--read-only"));
        assert!(joined.contains("--tmpfs /tmp:rw,noexec,nosuid,size=64m"));
        assert!(joined.contains("--cap-drop ALL"));
        assert!(joined.contains("--security-opt no-new-privileges"));
        assert!(joined.contains("--memory 536870912"));
        assert!(joined.contains("--cpus 0.500"));
        assert!(joined.contains("--pids-limit 128"));
        // Model blobs are mounted read-only, exactly as for model servers.
        assert!(joined.contains(&format!(
            "--mount type=bind,src={HOST_BLOB_DIR},dst={CONTAINER_BLOB_DIR},ro"
        )));
        // And the env contract survived into the argv.
        assert!(joined.contains("--env AVM_JOB_ID=job_1"));
        assert!(joined.contains("--env AVM_AGENT_ID=ag_ok"));
        // Pod labels the NetworkPolicy selects on.
        assert!(joined.contains("--label avm.io/workload=agent"));
    }

    #[test]
    fn network_is_off_unless_a_gateway_credential_is_injected() {
        let r = runner();
        assert_eq!(
            r.sandbox_spec(&spec()).unwrap().network,
            NetworkMode::None,
            "default posture must be no network at all"
        );

        let credentialed = spec().with_gateway("http://avm-gateway:8080", "tok_abc");
        assert_eq!(
            r.sandbox_spec(&credentialed).unwrap().network,
            NetworkMode::GatewayOnly {
                network: DEFAULT_GATEWAY_NETWORK.to_string()
            }
        );
    }

    #[test]
    fn pull_and_wall_budgets_are_separate_fields() {
        let r = runner();
        let sspec = r.sandbox_spec(&spec()).unwrap();
        assert_eq!(sspec.wall_timeout, Duration::from_secs(60));
        assert_eq!(
            sspec.pull_timeout,
            Duration::from_secs(DEFAULT_PULL_TIMEOUT_SEC)
        );
        assert_ne!(sspec.wall_timeout, sspec.pull_timeout);
    }

    #[test]
    fn container_names_are_sanitized() {
        assert_eq!(container_name("job_1"), "avm-job_1");
        assert_eq!(container_name("job/1;rm"), "avm-job-1-rm");
    }

    // -- preserved behaviour ------------------------------------------------

    #[test]
    fn trace_env_is_empty_without_opt_in() {
        let mut s = spec();
        s.trace = Some(TraceContext::root_from_seed(&[7u8; 16], true));
        // Level `off` and `basic` must not leak trace ids into the process.
        assert!(s.trace_env().is_empty());
        s.instrumentation = InstrumentationLevel::Basic;
        assert!(s.trace_env().is_empty());
        assert!(!s.env().contains_key(propagation::ENV_TRACE_ID));
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
            RunError::NonZeroExit {
                code: 2,
                stderr: "boom".into()
            }
            .outcome(),
            "failed"
        );
        assert_eq!(
            RunError::from(SandboxError::PullTimeout(Duration::from_secs(1))).outcome(),
            "pull_timeout"
        );
        // A sandbox timeout keeps the historical `timeout` shape.
        let mapped = RunError::from(SandboxError::Timeout(Duration::from_secs(42)));
        assert!(matches!(mapped, RunError::Timeout(42)));
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
        assert_eq!(
            registry.value("avm_agent_tokens_used", &labels),
            Some(1234.0)
        );
        assert_eq!(
            registry.value("avm_agent_cache_hit_ratio", &labels),
            Some(0.5)
        );
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

    // -- the demoted fork/exec path ----------------------------------------

    #[tokio::test]
    async fn process_sandbox_still_runs_an_agent() {
        // The dev/test path: fork/exec `/bin/cat`, payload in, payload out.
        let runner = AgentRunner::new(30)
            .with_sandbox(
                SandboxKind::Process,
                Arc::new(sandbox::ProcessSandbox::new("/bin")),
            )
            .with_resolver(Arc::new(
                ConfigMapResolver::new("ghcr.io/lightheart/agents", "latest")
                    .with_image("cat", format!("oci://ghcr.io/lightheart/agents/cat@{D}")),
            ));
        assert_eq!(runner.executor_type(), "process");

        let s = RunSpec::new("job_cat", "cat", "{\"hello\":\"world\"}", Scope::system());
        let outcome = runner.run(&s).await.expect("cat should succeed");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "{\"hello\":\"world\"}");
        assert_eq!(outcome.pull_duration, Duration::ZERO);
    }

    #[tokio::test]
    async fn process_sandbox_enforces_the_wall_clock() {
        let runner = AgentRunner::new(0)
            .with_sandbox(
                SandboxKind::Process,
                Arc::new(sandbox::ProcessSandbox::new("/bin")),
            )
            .with_resolver(Arc::new(ConfigMapResolver::new(
                "ghcr.io/lightheart/agents",
                "latest",
            )));

        // wall_time 0 → the timeout fires immediately. `/bin/yes` never exits on
        // its own, so the only way out of this call is the wall clock.
        let s = RunSpec::new("job_yes", "yes", "", Scope::system());
        let err = runner.run(&s).await.unwrap_err();
        assert!(matches!(err, RunError::Timeout(0)), "got {err:?}");
    }
}
