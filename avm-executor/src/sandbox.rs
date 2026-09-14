//! The [`Sandbox`] abstraction: how the executor actually runs untrusted code.
//!
//! AVM's premise is running code it did not write. Until this module existed,
//! agents were `fork`/`exec`ed into the executor's own namespaces (see the
//! `ProcessSandbox` below, preserved for local development) while model
//! servers already ran as OCI containers — the executor contradicted itself.
//!
//! A sandbox takes a [`SandboxSpec`] (image, env, stdin, limits, mounts,
//! network mode, two independent timeouts) and returns a [`SandboxOutcome`]
//! (stdout, stderr, exit code, **pull duration and run duration separately**).
//! The agent contract is untouched: payload on stdin, `AVM_*` in the
//! environment, result on stdout, exit 0 on success. Only the box moved.
//!
//! ```text
//!   SandboxSpec ─▶ Sandbox::run ─┬─▶ OciSandbox      (default, container)
//!                                └─▶ ProcessSandbox  (AVM_SANDBOX=process, dev only)
//! ```
//!
//! Isolation **tier** is a config knob, not an architectural commitment:
//! [`SandboxRuntime::Runc`] is implemented today; `runsc` (gVisor) and
//! `firecracker` are enum variants that fail loudly with
//! [`SandboxError::UnsupportedRuntime`] so adding them later is one impl plus
//! a config change, never a rewrite.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::model_server::Mount;

/// Default budget for staging the image before the job's own clock starts.
pub const DEFAULT_PULL_TIMEOUT_SEC: u64 = 120;
/// Default SIGTERM → SIGKILL grace period on teardown.
pub const DEFAULT_STOP_GRACE_SEC: u64 = 10;
/// Scheme every agent/model artifact reference carries.
pub const OCI_URI_SCHEME: &str = "oci://";
/// Container-runtime CLIs probed, in preference order, when
/// `AVM_CONTAINER_CLI` is unset. Rootless podman first: it needs no
/// root-equivalent daemon socket.
pub const CLI_CANDIDATES: [&str; 2] = ["podman", "docker"];

// ---------------------------------------------------------------------------
// Image references
// ---------------------------------------------------------------------------

/// A parsed `oci://registry/repo[:tag][@sha256:…]` reference.
///
/// This type exists so an agent id can never again be interpolated into a
/// filesystem path: the resolver's return type is an image reference, so the
/// path-traversal surface is gone by construction rather than by validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    uri: String,
    reference: String,
    digest: Option<String>,
}

impl ImageRef {
    /// Parse a canonical `oci://…` reference. A digest is strongly preferred
    /// (and required for residency labels) but a tag is accepted so a
    /// development registry without content addressing still works.
    pub fn parse(uri: &str) -> Result<Self, SandboxError> {
        let uri = uri.trim();
        let rest = uri
            .strip_prefix(OCI_URI_SCHEME)
            .ok_or_else(|| SandboxError::InvalidImage(format!("missing `oci://` scheme: {uri}")))?;
        if rest.is_empty() {
            return Err(SandboxError::InvalidImage(format!(
                "empty reference: {uri}"
            )));
        }
        if rest.contains(char::is_whitespace) {
            return Err(SandboxError::InvalidImage(format!(
                "whitespace in reference: {uri}"
            )));
        }

        let (locator, digest) = match rest.rsplit_once('@') {
            Some((locator, digest)) => {
                avm_models::model_ref::validate_digest(digest)
                    .map_err(|e| SandboxError::InvalidImage(e.to_string()))?;
                (locator, Some(digest.to_string()))
            }
            None => (rest, None),
        };

        let (registry, repository) = locator
            .split_once('/')
            .ok_or_else(|| SandboxError::InvalidImage(format!("missing repository path: {uri}")))?;
        if registry.is_empty() || repository.is_empty() {
            return Err(SandboxError::InvalidImage(format!(
                "empty registry or repository: {uri}"
            )));
        }

        Ok(Self {
            uri: uri.to_string(),
            reference: rest.to_string(),
            digest,
        })
    }

    /// The canonical `oci://…` form.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// The scheme-less form a container runtime CLI expects.
    pub fn oci_reference(&self) -> &str {
        &self.reference
    }

    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    pub fn is_digest_pinned(&self) -> bool {
        self.digest.is_some()
    }

    /// `agent.avm.io/<digest>` — the residency label analogous to
    /// `model.avm.io/<digest>`, so the scheduler can prefer nodes that already
    /// hold this agent image. `None` for an unpinned (tag-only) reference.
    pub fn label_key(&self) -> Option<String> {
        self.digest
            .as_deref()
            .map(|d| format!("{}{}", avm_models::artifact::AGENT_LABEL_PREFIX, d))
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.uri)
    }
}

// ---------------------------------------------------------------------------
// Runtime tiers
// ---------------------------------------------------------------------------

/// Isolation tier. Per-tenant policy knob, not a build-time decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxRuntime {
    /// Rootless OCI runtime (the default). Namespaces + cgroups + seccomp.
    #[default]
    Runc,
    /// gVisor — syscall interception in userspace. Not implemented here.
    Runsc,
    /// Firecracker microVM — a real kernel boundary. Not implemented here.
    Firecracker,
}

impl SandboxRuntime {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxRuntime::Runc => "runc",
            SandboxRuntime::Runsc => "runsc",
            SandboxRuntime::Firecracker => "firecracker",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "runc" | "" => Some(SandboxRuntime::Runc),
            "runsc" | "gvisor" => Some(SandboxRuntime::Runsc),
            "firecracker" | "fc" => Some(SandboxRuntime::Firecracker),
            _ => None,
        }
    }

    /// `AVM_SANDBOX_RUNTIME`, defaulting to rootless `runc`.
    pub fn from_env() -> Self {
        std::env::var("AVM_SANDBOX_RUNTIME")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }

    /// The `--runtime` value handed to the CLI, or a loud error naming exactly
    /// what is missing.
    pub fn runtime_flag(&self) -> Result<&'static str, SandboxError> {
        match self {
            SandboxRuntime::Runc => Ok("runc"),
            SandboxRuntime::Runsc => Err(SandboxError::UnsupportedRuntime {
                runtime: "runsc",
                detail: "gVisor tier requested: install the `runsc` OCI runtime, register it with \
                         the container CLI (`--runtime runsc`), and implement pull/teardown for \
                         it in OciSandbox. Tracked as a follow-up."
                    .to_string(),
            }),
            SandboxRuntime::Firecracker => Err(SandboxError::UnsupportedRuntime {
                runtime: "firecracker",
                detail: "Firecracker tier requested: needs a microVM supervisor \
                         (firecracker-containerd or a jailer-managed VMM), a kernel + rootfs \
                         image pair, and a vsock stdio bridge. Not a CLI drop-in. Tracked as a \
                         follow-up."
                    .to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Spec / outcome / error
// ---------------------------------------------------------------------------

/// Hard resource ceilings applied by the runtime, not by hand-rolled cgroup
/// writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    pub memory_bytes: u64,
    pub cpu_millis: u32,
    pub pids: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 512 * 1024 * 1024,
            cpu_millis: 500,
            pids: 128,
        }
    }
}

/// Network posture of the sandbox.
///
/// The default is [`NetworkMode::None`]. When an agent is handed a gateway
/// credential it gets [`NetworkMode::GatewayOnly`] — a dedicated runtime
/// network whose only reachable endpoint is the gateway. The authoritative
/// enforcement in Kubernetes is `docs/network-policies/allow-gateway-egress-only.yaml`;
/// the runtime network is the node-local half of the same rule.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetworkMode {
    #[default]
    None,
    GatewayOnly {
        network: String,
    },
    /// Dev escape hatch. Never a default, never for tenant workloads.
    Host,
}

impl NetworkMode {
    pub fn flag(&self) -> String {
        match self {
            NetworkMode::None => "none".to_string(),
            NetworkMode::GatewayOnly { network } => network.clone(),
            NetworkMode::Host => "host".to_string(),
        }
    }
}

/// Everything a sandbox needs to run one workload.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// Container name — also the teardown handle.
    pub name: String,
    /// Agent id, used by the dev-only [`ProcessSandbox`] and for logging.
    pub agent_id: String,
    pub image: ImageRef,
    pub env: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    pub stdin: String,
    pub limits: ResourceLimits,
    pub mounts: Vec<Mount>,
    pub network: NetworkMode,
    /// Budget for the workload itself.
    pub wall_timeout: Duration,
    /// Budget for staging the image. **Separate on purpose**: a cold pull must
    /// not be able to consume the whole job budget.
    pub pull_timeout: Duration,
    /// SIGTERM → SIGKILL grace on teardown.
    pub stop_grace: Duration,
    pub runtime: SandboxRuntime,
}

impl SandboxSpec {
    pub fn new(name: impl Into<String>, agent_id: impl Into<String>, image: ImageRef) -> Self {
        Self {
            name: name.into(),
            agent_id: agent_id.into(),
            image,
            env: BTreeMap::new(),
            labels: BTreeMap::new(),
            stdin: String::new(),
            limits: ResourceLimits::default(),
            mounts: Vec::new(),
            network: NetworkMode::None,
            wall_timeout: Duration::from_secs(900),
            pull_timeout: Duration::from_secs(DEFAULT_PULL_TIMEOUT_SEC),
            stop_grace: Duration::from_secs(DEFAULT_STOP_GRACE_SEC),
            runtime: SandboxRuntime::default(),
        }
    }

    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin = stdin.into();
        self
    }

    /// `<cli> pull <reference>`.
    pub fn pull_args(&self) -> Vec<String> {
        vec!["pull".to_string(), self.image.oci_reference().to_string()]
    }

    /// The full `run` argument vector, hardening flags included.
    ///
    /// Pure and deterministic (env/labels come from `BTreeMap`s) so the
    /// hardening contract is unit-testable without a container runtime.
    pub fn run_args(&self) -> Result<Vec<String>, SandboxError> {
        let runtime = self.runtime.runtime_flag()?;
        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-i".into(),
            "--name".into(),
            self.name.clone(),
            "--runtime".into(),
            runtime.into(),
            // --- hardening ------------------------------------------------
            "--read-only".into(),
            "--tmpfs".into(),
            "/tmp:rw,noexec,nosuid,size=64m".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            // --- limits ---------------------------------------------------
            "--memory".into(),
            self.limits.memory_bytes.to_string(),
            "--cpus".into(),
            format!("{:.3}", self.limits.cpu_millis as f64 / 1000.0),
            "--pids-limit".into(),
            self.limits.pids.to_string(),
            // --- network --------------------------------------------------
            "--network".into(),
            self.network.flag(),
        ];

        for mount in &self.mounts {
            args.push("--mount".into());
            args.push(mount.to_arg());
        }
        for (key, value) in &self.labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        // TODO(avm): the job-scoped gateway token is passed as `--env`, which is
        // visible in the node's process table. Move to `--env-file` (or the
        // runtime's secret mount) when the gateway minting work lands.
        for (key, value) in &self.env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }

        args.push(self.image.oci_reference().to_string());
        Ok(args)
    }

    /// `<cli> stop --time <grace> <name>` — SIGTERM, then the runtime sends
    /// SIGKILL once the grace period expires.
    pub fn stop_args(&self) -> Vec<String> {
        vec![
            "stop".into(),
            "--time".into(),
            self.stop_grace.as_secs().to_string(),
            self.name.clone(),
        ]
    }

    /// `<cli> kill --signal KILL <name>` — the unconditional backstop.
    pub fn kill_args(&self) -> Vec<String> {
        vec![
            "kill".into(),
            "--signal".into(),
            "KILL".into(),
            self.name.clone(),
        ]
    }
}

/// Result of one sandboxed run.
#[derive(Debug, Clone)]
pub struct SandboxOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Time spent staging the image. Zero when it was already resident —
    /// which is exactly the number the warm-pool / residency work needs.
    pub pull_duration: Duration,
    /// Time spent running the workload.
    pub run_duration: Duration,
}

impl SandboxOutcome {
    pub fn total_duration(&self) -> Duration {
        self.pull_duration + self.run_duration
    }
}

/// Failure modes of a sandboxed run.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox runtime `{runtime}` is not implemented: {detail}")]
    UnsupportedRuntime {
        runtime: &'static str,
        detail: String,
    },
    #[error("invalid image reference: {0}")]
    InvalidImage(String),
    #[error(
        "no container runtime CLI found (tried {0}); set AVM_CONTAINER_CLI or AVM_SANDBOX=process"
    )]
    NoRuntimeCli(String),
    #[error("image pull exceeded {}ms", .0.as_millis())]
    PullTimeout(Duration),
    #[error("image pull failed: {0}")]
    Pull(String),
    #[error("sandbox exceeded wall time of {}ms", .0.as_millis())]
    Timeout(Duration),
    #[error("sandbox io error: {0}")]
    Io(#[from] std::io::Error),
}

impl SandboxError {
    /// Low-cardinality outcome label for metrics.
    pub fn outcome(&self) -> &'static str {
        match self {
            SandboxError::UnsupportedRuntime { .. } => "unsupported_runtime",
            SandboxError::InvalidImage(_) => "unresolved",
            SandboxError::NoRuntimeCli(_) => "no_runtime",
            SandboxError::PullTimeout(_) => "pull_timeout",
            SandboxError::Pull(_) => "pull_failed",
            SandboxError::Timeout(_) => "timeout",
            SandboxError::Io(_) => "spawn_failed",
        }
    }
}

/// Apply the **pull** budget to `fut`. Separate function (and separate error)
/// from [`with_wall_timeout`] so the two budgets can be asserted independently.
pub async fn with_pull_timeout<F, T>(spec: &SandboxSpec, fut: F) -> Result<T, SandboxError>
where
    F: Future<Output = Result<T, SandboxError>>,
{
    match tokio::time::timeout(spec.pull_timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(SandboxError::PullTimeout(spec.pull_timeout)),
    }
}

/// Apply the **wall-time** budget to `fut`.
pub async fn with_wall_timeout<F, T>(spec: &SandboxSpec, fut: F) -> Result<T, SandboxError>
where
    F: Future<Output = Result<T, SandboxError>>,
{
    match tokio::time::timeout(spec.wall_timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(SandboxError::Timeout(spec.wall_timeout)),
    }
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Runs one workload to completion under some isolation boundary.
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, SandboxError>;

    /// Backend label for `avm_executor_container_duration_seconds`.
    fn backend(&self) -> &'static str;
}

/// Which implementation the executor uses. `AVM_SANDBOX`, default `oci`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxKind {
    #[default]
    Oci,
    /// fork/exec. **Development and tests only** — no isolation whatsoever.
    Process,
}

impl SandboxKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxKind::Oci => "oci",
            SandboxKind::Process => "process",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "oci" | "container" | "" => Some(SandboxKind::Oci),
            "process" | "fork" | "exec" => Some(SandboxKind::Process),
            _ => None,
        }
    }

    pub fn from_env() -> Self {
        std::env::var("AVM_SANDBOX")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }
}

/// Build the sandbox named by `kind`.
pub fn build(kind: SandboxKind, runtime: SandboxRuntime) -> std::sync::Arc<dyn Sandbox> {
    match kind {
        SandboxKind::Oci => std::sync::Arc::new(OciSandbox::from_env(runtime)),
        SandboxKind::Process => {
            tracing::warn!(
                "AVM_SANDBOX=process: agents run fork/exec with NO isolation. \
                 Development only — never in a multi-tenant deployment."
            );
            std::sync::Arc::new(ProcessSandbox::from_env())
        }
    }
}

// ---------------------------------------------------------------------------
// OciSandbox — the real one
// ---------------------------------------------------------------------------

/// Runs the workload as an OCI container by shelling out to a runtime CLI.
///
/// Deliberately a CLI dependency and not a Docker-daemon socket: a socket
/// handle is root-equivalent on the node, which is the wrong shape for a
/// multi-tenant executor. Rootless `podman` is preferred when present;
/// `docker` is the fallback (see the PR body for the caveat).
#[derive(Debug, Clone)]
pub struct OciSandbox {
    cli: Option<String>,
    runtime: SandboxRuntime,
}

impl OciSandbox {
    pub fn new(cli: impl Into<String>, runtime: SandboxRuntime) -> Self {
        Self {
            cli: Some(cli.into()),
            runtime,
        }
    }

    /// `AVM_CONTAINER_CLI`, else the first of [`CLI_CANDIDATES`] on `PATH`.
    pub fn from_env(runtime: SandboxRuntime) -> Self {
        let cli = std::env::var("AVM_CONTAINER_CLI")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(find_cli);
        Self { cli, runtime }
    }

    pub fn cli(&self) -> Option<&str> {
        self.cli.as_deref()
    }

    pub fn runtime(&self) -> SandboxRuntime {
        self.runtime
    }

    fn cli_or_err(&self) -> Result<&str, SandboxError> {
        self.cli
            .as_deref()
            .ok_or_else(|| SandboxError::NoRuntimeCli(CLI_CANDIDATES.join(", ")))
    }

    async fn pull(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
        let cli = self.cli_or_err()?.to_string();
        let args = spec.pull_args();
        with_pull_timeout(spec, async move {
            let output = Command::new(&cli).args(&args).output().await?;
            if output.status.success() {
                Ok(())
            } else {
                Err(SandboxError::Pull(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ))
            }
        })
        .await
    }

    /// SIGTERM → grace → SIGKILL. `kill_on_drop` semantics do not survive the
    /// move to containers: the child we spawned is the CLI, not the workload.
    async fn teardown(&self, spec: &SandboxSpec) {
        let Ok(cli) = self.cli_or_err() else { return };
        let _ = Command::new(cli)
            .args(spec.stop_args())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        let _ = Command::new(cli)
            .args(spec.kill_args())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

#[async_trait]
impl Sandbox for OciSandbox {
    async fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, SandboxError> {
        // Fail before doing any work if the requested tier has no impl.
        let _ = spec.runtime.runtime_flag()?;
        let cli = self.cli_or_err()?.to_string();

        tracing::debug!(
            agent_id = %spec.agent_id,
            image = %spec.image,
            "container.pull_image"
        );
        let pull_started = Instant::now();
        let pull_result = self.pull(spec).await;
        let pull_duration = pull_started.elapsed();
        pull_result?;

        let args = spec.run_args()?;
        let run_started = Instant::now();
        let mut child = Command::new(&cli)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(spec.stdin.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let waited = with_wall_timeout(spec, async {
            child.wait_with_output().await.map_err(SandboxError::Io)
        })
        .await;

        let output = match waited {
            Ok(output) => output,
            Err(err) => {
                tracing::warn!(name = %spec.name, "container.cleanup: SIGTERM then SIGKILL");
                self.teardown(spec).await;
                return Err(err);
            }
        };

        Ok(SandboxOutcome {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
            pull_duration,
            run_duration: run_started.elapsed(),
        })
    }

    fn backend(&self) -> &'static str {
        "oci"
    }
}

/// First [`CLI_CANDIDATES`] entry present on `PATH`.
fn find_cli() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for candidate in CLI_CANDIDATES {
        for dir in std::env::split_paths(&path) {
            let full = dir.join(candidate);
            if is_executable(&full) {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                meta.is_file() && meta.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                meta.is_file()
            }
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// ProcessSandbox — preserved, demoted
// ---------------------------------------------------------------------------

/// The original fork/exec path.
///
/// **Development and test only.** It provides no filesystem, network, or
/// resource isolation: the agent inherits the executor's namespaces exactly as
/// it did before containers. Kept so a laptop with no container runtime can
/// still run the stack (`AVM_SANDBOX=process`), and so the historical tests
/// keep a home. Never the default, never for tenant workloads.
#[derive(Debug, Clone)]
pub struct ProcessSandbox {
    agent_dir: String,
}

impl ProcessSandbox {
    pub fn new(agent_dir: impl Into<String>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
        }
    }

    /// `AVM_AGENT_DIR`, default `/opt/avm/agents`.
    pub fn from_env() -> Self {
        Self::new(std::env::var("AVM_AGENT_DIR").unwrap_or_else(|_| "/opt/avm/agents".into()))
    }

    pub fn agent_dir(&self) -> &str {
        &self.agent_dir
    }

    /// The binary this sandbox would exec. `agent_id` is validated by the
    /// resolver before it ever reaches here.
    pub fn binary_path(&self, agent_id: &str) -> String {
        format!("{}/{}", self.agent_dir.trim_end_matches('/'), agent_id)
    }
}

#[async_trait]
impl Sandbox for ProcessSandbox {
    async fn run(&self, spec: &SandboxSpec) -> Result<SandboxOutcome, SandboxError> {
        let bin = self.binary_path(&spec.agent_id);
        let run_started = Instant::now();

        let mut cmd = Command::new(&bin);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(spec.stdin.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let output = with_wall_timeout(spec, async {
            child.wait_with_output().await.map_err(SandboxError::Io)
        })
        .await?;

        Ok(SandboxOutcome {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
            // Nothing is staged: fork/exec is the ~1ms path this work trades
            // away for isolation.
            pull_duration: Duration::ZERO,
            run_duration: run_started.elapsed(),
        })
    }

    fn backend(&self) -> &'static str {
        "process"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn image() -> ImageRef {
        ImageRef::parse(&format!("oci://ghcr.io/lightheart/agents/summarizer@{D}")).unwrap()
    }

    fn spec() -> SandboxSpec {
        SandboxSpec::new("avm-job_1", "summarizer", image())
    }

    #[test]
    fn image_ref_parses_digest_and_tag_forms() {
        let pinned = image();
        assert!(pinned.is_digest_pinned());
        assert_eq!(pinned.digest(), Some(D));
        assert_eq!(
            pinned.oci_reference(),
            format!("ghcr.io/lightheart/agents/summarizer@{D}")
        );
        assert_eq!(
            pinned.label_key().as_deref(),
            Some(format!("agent.avm.io/{D}").as_str())
        );

        let tagged = ImageRef::parse("oci://ghcr.io/lightheart/agents/dev:latest").unwrap();
        assert!(!tagged.is_digest_pinned());
        assert_eq!(tagged.label_key(), None);
    }

    #[test]
    fn image_ref_rejects_junk() {
        for bad in [
            "",
            "ghcr.io/x/y",               // no scheme
            "oci://",                    // empty
            "oci://noslash",             // no repository
            "oci:///repo",               // empty registry
            "oci://ghcr.io/x y",         // whitespace
            "oci://ghcr.io/x@sha256:zz", // bad digest
            "oci://ghcr.io/x@md5:abc",   // wrong algo
        ] {
            assert!(
                ImageRef::parse(bad).is_err(),
                "should have rejected `{bad}`"
            );
        }
    }

    #[test]
    fn runc_is_implemented_and_the_other_tiers_fail_loudly() {
        assert_eq!(SandboxRuntime::parse("runc"), Some(SandboxRuntime::Runc));
        assert_eq!(SandboxRuntime::parse("gvisor"), Some(SandboxRuntime::Runsc));
        assert_eq!(
            SandboxRuntime::parse("firecracker"),
            Some(SandboxRuntime::Firecracker)
        );
        assert_eq!(SandboxRuntime::parse("qemu"), None);
        assert_eq!(SandboxRuntime::default(), SandboxRuntime::Runc);

        assert_eq!(SandboxRuntime::Runc.runtime_flag().unwrap(), "runc");

        for tier in [SandboxRuntime::Runsc, SandboxRuntime::Firecracker] {
            let err = tier.runtime_flag().unwrap_err();
            match err {
                SandboxError::UnsupportedRuntime { runtime, detail } => {
                    assert_eq!(runtime, tier.as_str());
                    assert!(!detail.is_empty(), "must name what is needed");
                }
                other => panic!("expected UnsupportedRuntime, got {other:?}"),
            }
            assert_eq!(
                tier.runtime_flag().unwrap_err().outcome(),
                "unsupported_runtime"
            );
        }
    }

    #[test]
    fn unsupported_tier_fails_before_any_args_are_built() {
        let mut s = spec();
        s.runtime = SandboxRuntime::Runsc;
        assert!(matches!(
            s.run_args(),
            Err(SandboxError::UnsupportedRuntime { .. })
        ));
    }

    #[test]
    fn run_args_carry_every_hardening_flag() {
        let args = spec().run_args().unwrap();
        let joined = args.join(" ");

        assert!(joined.contains("--read-only"));
        assert!(joined.contains("--tmpfs /tmp:rw,noexec,nosuid,size=64m"));
        assert!(joined.contains("--cap-drop ALL"));
        assert!(joined.contains("--security-opt no-new-privileges"));
        assert!(joined.contains("--runtime runc"));
        assert!(joined.contains("--network none"), "no network by default");
        assert!(joined.ends_with(&format!("ghcr.io/lightheart/agents/summarizer@{D}")));
    }

    #[test]
    fn run_args_apply_limits_and_mounts() {
        let mut s = spec();
        s.limits = ResourceLimits {
            memory_bytes: 1024 * 1024 * 1024,
            cpu_millis: 1500,
            pids: 64,
        };
        s.mounts = vec![Mount::read_only("/var/lib/avm/models/blobs", "/models")];
        let joined = s.run_args().unwrap().join(" ");

        assert!(joined.contains("--memory 1073741824"));
        assert!(joined.contains("--cpus 1.500"));
        assert!(joined.contains("--pids-limit 64"));
        assert!(joined.contains("--mount type=bind,src=/var/lib/avm/models/blobs,dst=/models,ro"));
    }

    #[test]
    fn gateway_network_mode_names_the_runtime_network() {
        let mut s = spec();
        s.network = NetworkMode::GatewayOnly {
            network: "avm-gateway".into(),
        };
        assert!(s
            .run_args()
            .unwrap()
            .join(" ")
            .contains("--network avm-gateway"));
    }

    #[test]
    fn teardown_is_sigterm_then_sigkill() {
        let s = spec();
        assert_eq!(
            s.stop_args(),
            vec!["stop", "--time", "10", "avm-job_1"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            s.kill_args(),
            vec!["kill", "--signal", "KILL", "avm-job_1"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn pull_and_wall_budgets_are_enforced_independently() {
        let mut s = spec();
        s.pull_timeout = Duration::from_millis(40);
        s.wall_timeout = Duration::from_secs(30);

        // A slow pull blows the pull budget even though the wall budget is huge.
        let err = with_pull_timeout(&s, async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok::<(), SandboxError>(())
        })
        .await
        .unwrap_err();
        assert!(matches!(err, SandboxError::PullTimeout(_)));
        assert_eq!(err.outcome(), "pull_timeout");

        // …and the wall budget is untouched by it.
        assert!(with_wall_timeout(&s, async { Ok::<(), SandboxError>(()) })
            .await
            .is_ok());

        // Inverted: a fast pull, a slow run.
        let mut s2 = spec();
        s2.pull_timeout = Duration::from_secs(30);
        s2.wall_timeout = Duration::from_millis(40);
        assert!(with_pull_timeout(&s2, async { Ok::<(), SandboxError>(()) })
            .await
            .is_ok());
        let err = with_wall_timeout(&s2, async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok::<(), SandboxError>(())
        })
        .await
        .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout(_)));
        assert_eq!(err.outcome(), "timeout");
    }

    #[test]
    fn sandbox_kind_defaults_to_oci() {
        assert_eq!(SandboxKind::default(), SandboxKind::Oci);
        assert_eq!(SandboxKind::parse("process"), Some(SandboxKind::Process));
        assert_eq!(SandboxKind::parse("oci"), Some(SandboxKind::Oci));
        assert_eq!(SandboxKind::parse("nope"), None);
        assert_eq!(SandboxKind::Oci.as_str(), "oci");
    }

    #[test]
    fn process_sandbox_is_reachable_but_not_default() {
        let p = ProcessSandbox::new("/opt/avm/agents/");
        assert_eq!(p.binary_path("summarizer"), "/opt/avm/agents/summarizer");
        assert_eq!(p.backend(), "process");
        assert_eq!(
            OciSandbox::new("podman", SandboxRuntime::Runc).backend(),
            "oci"
        );
    }

    #[tokio::test]
    async fn missing_cli_is_reported_clearly() {
        let sandbox = OciSandbox {
            cli: None,
            runtime: SandboxRuntime::Runc,
        };
        let err = sandbox.run(&spec()).await.unwrap_err();
        assert!(matches!(err, SandboxError::NoRuntimeCli(_)));
    }

    /// Requires a working container runtime + network. Not run in CI.
    #[tokio::test]
    #[ignore = "requires a container runtime (podman/docker) and registry access"]
    async fn oci_sandbox_round_trips_stdin_to_stdout() {
        let sandbox = OciSandbox::from_env(SandboxRuntime::Runc);
        let mut s = SandboxSpec::new(
            format!("avm-test-{}", std::process::id()),
            "cat",
            ImageRef::parse("oci://docker.io/library/busybox:latest").unwrap(),
        );
        s.stdin = "{\"hello\":\"world\"}".into();
        s.wall_timeout = Duration::from_secs(60);

        let outcome = sandbox.run(&s).await.expect("container run");
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.contains("hello"));
    }
}
