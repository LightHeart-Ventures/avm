//! Model-server workloads (`kind: ModelServer`).
//!
//! A model server is an OCI container that serves inference over HTTP for one
//! or more **resident** models. The executor does not copy weights into the
//! image — it bind-mounts the node's content-addressed blob store read-only:
//!
//! ```text
//!   /var/lib/avm/models/blobs  ->  /models  (ro)
//!   --model /models/sha256/ab/ab12…        (digest-addressed path)
//! ```
//!
//! After the server reports healthy the executor publishes node labels
//! (`model.avm.io/<digest>=resident`) so the scheduler can co-schedule agents
//! that need those weights. Multiple model servers per node are explicitly
//! allowed — fan-out policy is a separate spike.

use std::collections::BTreeMap;

use avm_models::model_ref::Residency;
use avm_models::ModelRef;
use serde::{Deserialize, Serialize};

/// Host path of the blob store mounted into model servers.
pub const HOST_BLOB_DIR: &str = "/var/lib/avm/models/blobs";
/// Mount point inside the container.
pub const CONTAINER_BLOB_DIR: &str = "/models";

/// What an executor knows how to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ExecutorKind {
    /// fork/exec an agent binary (see [`crate::agent_runner`]).
    Process,
    /// A generic OCI container workload.
    Oci,
    /// An OCI container serving model inference — mounts the blob store.
    ModelServer,
}

impl ExecutorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutorKind::Process => "Process",
            ExecutorKind::Oci => "Oci",
            ExecutorKind::ModelServer => "ModelServer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Process" => Some(ExecutorKind::Process),
            "Oci" => Some(ExecutorKind::Oci),
            "ModelServer" => Some(ExecutorKind::ModelServer),
            _ => None,
        }
    }

    /// True when the workload needs the model blob mount.
    pub fn needs_model_mount(&self) -> bool {
        matches!(self, ExecutorKind::ModelServer)
    }
}

/// A bind mount handed to the container runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

impl Mount {
    pub fn read_only(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: true,
        }
    }

    /// `--mount type=bind,src=…,dst=…,ro` argument form.
    pub fn to_arg(&self) -> String {
        let ro = if self.read_only { ",ro" } else { "" };
        format!("type=bind,src={},dst={}{}", self.source, self.target, ro)
    }
}

/// Declarative spec for a `kind: ModelServer` workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelServerSpec {
    /// Instance name, e.g. `ms-qwen3-8b`.
    pub name: String,
    /// Server image, e.g. `ghcr.io/ggml-org/llama.cpp:server`.
    pub image: String,
    /// Models this instance serves. More than one is allowed.
    pub models: Vec<ModelRef>,
    /// Port the server listens on inside the container.
    pub port: u16,
    /// Extra arguments appended verbatim to the container command.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Host blob directory (overridable for tests / non-default roots).
    #[serde(default = "default_blob_dir")]
    pub host_blob_dir: String,
}

fn default_blob_dir() -> String {
    HOST_BLOB_DIR.to_string()
}

impl ModelServerSpec {
    pub fn new(name: impl Into<String>, image: impl Into<String>, model: ModelRef) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            models: vec![model],
            port: 8081,
            extra_args: Vec::new(),
            host_blob_dir: default_blob_dir(),
        }
    }

    pub fn kind(&self) -> ExecutorKind {
        ExecutorKind::ModelServer
    }

    /// Serve an additional model from the same instance.
    pub fn serving_also(mut self, model: ModelRef) -> Self {
        self.models.push(model);
        self
    }

    pub fn on_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Always exactly one mount: the blob store, read-only. Weights are never
    /// writable from inside a model server.
    pub fn mounts(&self) -> Vec<Mount> {
        vec![Mount::read_only(
            self.host_blob_dir.clone(),
            CONTAINER_BLOB_DIR,
        )]
    }

    /// In-container path of a model blob: `/models/sha256/<aa>/<hex>`.
    pub fn container_model_path(&self, model: &ModelRef) -> String {
        let hex = model.digest_hex();
        format!("{CONTAINER_BLOB_DIR}/sha256/{}/{}", &hex[..2], hex)
    }

    /// Full `docker run`-style argument vector.
    pub fn container_args(&self) -> Vec<String> {
        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            self.name.clone(),
        ];
        for mount in self.mounts() {
            args.push("--mount".into());
            args.push(mount.to_arg());
        }
        args.push("-p".into());
        args.push(format!("{p}:{p}", p = self.port));
        args.push(self.image.clone());
        for model in &self.models {
            args.push("--model".into());
            args.push(self.container_model_path(model));
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }

    /// Environment handed to the container.
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("AVM_MODEL_DIR".into(), CONTAINER_BLOB_DIR.into());
        env.insert("AVM_MODEL_SERVER".into(), self.name.clone());
        env.insert("AVM_MODEL_PORT".into(), self.port.to_string());
        env.insert(
            "AVM_MODEL_DIGESTS".into(),
            self.models
                .iter()
                .map(|m| m.digest.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        if let Some(backend) = self.models.first().and_then(|m| m.backend.clone()) {
            env.insert("AVM_MODEL_BACKEND".into(), backend);
        }
        env
    }

    /// Labels to publish once the server is healthy. Served models are
    /// `resident` by definition — the executor verified the digest before
    /// starting the container.
    pub fn node_labels(&self) -> BTreeMap<String, String> {
        self.models
            .iter()
            .map(|m| (m.label_key(), Residency::Resident.as_str().to_string()))
            .collect()
    }

    /// Endpoint other workloads on the node dial.
    pub fn endpoint(&self, node_host: &str) -> String {
        format!("http://{node_host}:{}", self.port)
    }

    /// Health probe path (llama.cpp / vLLM / TGI all expose `/health`).
    pub fn health_path(&self) -> &'static str {
        "/health"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn model() -> ModelRef {
        ModelRef::parse(&format!("oci://ghcr.io/lightheart/qwen3-8b:q4_k_m@{D}"))
            .unwrap()
            .with_backend("llama.cpp")
    }

    #[test]
    fn kind_roundtrips_and_gates_the_mount() {
        assert_eq!(
            ExecutorKind::parse("ModelServer"),
            Some(ExecutorKind::ModelServer)
        );
        assert_eq!(ExecutorKind::parse("nope"), None);
        assert!(ExecutorKind::ModelServer.needs_model_mount());
        assert!(!ExecutorKind::Process.needs_model_mount());
        assert!(!ExecutorKind::Oci.needs_model_mount());
    }

    #[test]
    fn blob_mount_is_always_read_only() {
        let spec = ModelServerSpec::new("ms-qwen", "ghcr.io/ggml-org/llama.cpp:server", model());
        let mounts = spec.mounts();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, "/var/lib/avm/models/blobs");
        assert_eq!(mounts[0].target, "/models");
        assert!(mounts[0].read_only);
        assert!(mounts[0].to_arg().ends_with(",ro"));
    }

    #[test]
    fn container_args_point_at_the_digest_path() {
        let spec = ModelServerSpec::new("ms-qwen", "img:tag", model())
            .on_port(9001)
            .with_args(["--ctx-size", "8192"]);
        let args = spec.container_args();
        let joined = args.join(" ");

        assert!(joined.contains("--mount type=bind,src=/var/lib/avm/models/blobs,dst=/models,ro"));
        assert!(joined.contains("-p 9001:9001"));
        assert!(joined.contains(
            "/models/sha256/e3/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        assert!(joined.ends_with("--ctx-size 8192"));
    }

    #[test]
    fn multiple_models_per_server_are_supported() {
        let second = ModelRef::from_digest(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let spec =
            ModelServerSpec::new("ms-multi", "img:tag", model()).serving_also(second.clone());

        let labels = spec.node_labels();
        assert_eq!(labels.len(), 2);
        assert_eq!(
            labels.get(&model().label_key()).map(String::as_str),
            Some("resident")
        );
        assert_eq!(
            labels.get(&second.label_key()).map(String::as_str),
            Some("resident")
        );
        assert_eq!(
            spec.container_args()
                .iter()
                .filter(|a| *a == "--model")
                .count(),
            2
        );
    }

    #[test]
    fn env_and_endpoint_describe_the_instance() {
        let spec = ModelServerSpec::new("ms-qwen", "img:tag", model()).on_port(8081);
        let env = spec.env();
        assert_eq!(
            env.get("AVM_MODEL_DIR").map(String::as_str),
            Some("/models")
        );
        assert_eq!(
            env.get("AVM_MODEL_BACKEND").map(String::as_str),
            Some("llama.cpp")
        );
        assert_eq!(env.get("AVM_MODEL_DIGESTS").map(String::as_str), Some(D));
        assert_eq!(spec.endpoint("node-1"), "http://node-1:8081");
        assert_eq!(spec.health_path(), "/health");
    }
}
