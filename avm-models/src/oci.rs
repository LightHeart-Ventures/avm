//! OCI artifact client (oras-project `oci-client`).
//!
//! Models are pushed to a registry as OCI **artifacts**: a manifest whose
//! `artifactType` is `application/vnd.avm.model.v1+json` and whose single
//! layer carries the weights.
//!
//! ```text
//! manifest  application/vnd.oci.image.manifest.v1+json
//!   config  application/vnd.avm.model.config.v1+json   { backend, params }
//!   layer   application/vnd.avm.model.weights.v1       <the blob>
//! ```
//!
//! The concrete client is compiled only with `--features oci-registry` so the
//! default workspace build (and CI test run) needs no TLS stack or network.
//! Without the feature the type still exists and every pull returns
//! [`crate::ModelError::RegistryFeatureDisabled`], which keeps call sites and
//! the scheduler/executor wiring identical in both builds.

use async_trait::async_trait;

use crate::store::ArtifactFetcher;
use crate::{ModelError, ModelRef, Result};

/// Media type of the weights layer.
pub const MEDIA_TYPE_WEIGHTS: &str = "application/vnd.avm.model.weights.v1";
/// Media type of the artifact config blob.
pub const MEDIA_TYPE_CONFIG: &str = "application/vnd.avm.model.config.v1+json";
/// `artifactType` advertised on the manifest.
pub const ARTIFACT_TYPE: &str = "application/vnd.avm.model.v1+json";

/// How to authenticate to a registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RegistryAuth {
    /// Public / unauthenticated pulls.
    #[default]
    Anonymous,
    /// Username + password or PAT (e.g. a GHCR token).
    Basic { username: String, password: String },
}

/// Pulls model artifacts from an OCI registry.
#[derive(Debug, Clone, Default)]
pub struct OciArtifactClient {
    auth: RegistryAuth,
    /// Accepted layer media types, in preference order.
    accepted_media_types: Vec<String>,
}

impl OciArtifactClient {
    pub fn new() -> Self {
        Self {
            auth: RegistryAuth::Anonymous,
            accepted_media_types: vec![MEDIA_TYPE_WEIGHTS.to_string()],
        }
    }

    pub fn with_auth(mut self, auth: RegistryAuth) -> Self {
        self.auth = auth;
        self
    }

    /// Accept an additional layer media type (e.g. a vendor GGUF type).
    pub fn accepting(mut self, media_type: impl Into<String>) -> Self {
        self.accepted_media_types.push(media_type.into());
        self
    }

    pub fn auth(&self) -> &RegistryAuth {
        &self.auth
    }

    pub fn accepted_media_types(&self) -> &[String] {
        &self.accepted_media_types
    }

    /// Pull the weights layer for `model`.
    #[cfg(feature = "oci-registry")]
    pub async fn pull_blob(&self, model: &ModelRef) -> Result<Vec<u8>> {
        use oci_client::{secrets::RegistryAuth as OciAuth, Client, Reference};

        let reference: Reference = model
            .oci_reference()
            .parse()
            .map_err(|e| ModelError::InvalidRef(format!("{}: {e}", model.oci_reference())))?;

        let auth = match &self.auth {
            RegistryAuth::Anonymous => OciAuth::Anonymous,
            RegistryAuth::Basic { username, password } => {
                OciAuth::Basic(username.clone(), password.clone())
            }
        };

        let accepted: Vec<&str> = self
            .accepted_media_types
            .iter()
            .map(String::as_str)
            .collect();
        let client = Client::default();
        let image = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| ModelError::Fetch(format!("oci pull {}: {e}", model.oci_reference())))?;

        let layer =
            image.layers.into_iter().next().ok_or_else(|| {
                ModelError::Fetch(format!("no layers in {}", model.oci_reference()))
            })?;
        Ok(layer.data.to_vec())
    }

    /// Pull is unavailable without the `oci-registry` feature.
    #[cfg(not(feature = "oci-registry"))]
    pub async fn pull_blob(&self, model: &ModelRef) -> Result<Vec<u8>> {
        Err(ModelError::RegistryFeatureDisabled(model.oci_reference()))
    }

    /// Push is not wired yet — models are published by `oras push` in CI.
    /// Tracked as a follow-up spike (see IMPLEMENTATION_PLAN.md).
    pub async fn push_blob(&self, model: &ModelRef, _bytes: &[u8]) -> Result<()> {
        Err(ModelError::Unsupported(format!(
            "registry push not implemented; publish {} with `oras push`",
            model.oci_reference()
        )))
    }
}

#[async_trait]
impl ArtifactFetcher for OciArtifactClient {
    async fn fetch(&self, model: &ModelRef) -> Result<Vec<u8>> {
        self.pull_blob(model).await
    }

    fn describe(&self) -> String {
        match &self.auth {
            RegistryAuth::Anonymous => "oci:anonymous".to_string(),
            RegistryAuth::Basic { username, .. } => format!("oci:basic:{username}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn defaults_to_anonymous_and_weights_media_type() {
        let c = OciArtifactClient::new();
        assert_eq!(c.auth(), &RegistryAuth::Anonymous);
        assert_eq!(c.accepted_media_types(), &[MEDIA_TYPE_WEIGHTS.to_string()]);
        assert_eq!(c.describe(), "oci:anonymous");
    }

    #[test]
    fn basic_auth_is_described_without_leaking_the_secret() {
        let c = OciArtifactClient::new().with_auth(RegistryAuth::Basic {
            username: "ci".into(),
            password: "supersecret".into(),
        });
        let d = c.describe();
        assert!(d.contains("ci"));
        assert!(!d.contains("supersecret"));
    }

    #[tokio::test]
    async fn push_is_unsupported_for_now() {
        let model = ModelRef::parse(&format!("oci://ghcr.io/lightheart/m@{D}")).unwrap();
        assert!(matches!(
            OciArtifactClient::new().push_blob(&model, b"x").await,
            Err(ModelError::Unsupported(_))
        ));
    }

    #[cfg(not(feature = "oci-registry"))]
    #[tokio::test]
    async fn pull_without_feature_reports_clearly() {
        let model = ModelRef::parse(&format!("oci://ghcr.io/lightheart/m@{D}")).unwrap();
        assert!(matches!(
            OciArtifactClient::new().fetch(&model).await,
            Err(ModelError::RegistryFeatureDisabled(_))
        ));
    }
}
