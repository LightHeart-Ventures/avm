//! The Agent Card: how an AVM agent advertises itself.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::discovery::SCHEMA_VERSION;

/// A pointer at another agent — the minimum needed to fetch its card.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    /// Stable AVM agent id, e.g. `ag_pr_reviewer`.
    pub agent_id: String,
    /// Human-readable name (advisory; `agent_id` is authoritative).
    #[serde(default)]
    pub name: String,
    /// Absolute URL of the agent's `/.well-known/agent-card.json`.
    #[serde(default)]
    pub card_url: String,
}

impl AgentRef {
    /// Build a ref from an id alone (name/card_url resolved at discovery time).
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            ..Default::default()
        }
    }
}

/// Which model backs the agent.
///
/// `uri` is the canonical, content-addressable form and is what the scheduler
/// keys model residency on, e.g.
/// `oci://ghcr.io/lightheart/qwen3-8b@sha256:ab12…` for a self-hosted weight
/// artifact, or `anthropic://claude-sonnet-4-6` for a hosted API model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// Canonical model URI.
    pub uri: String,
    /// Provider hint: `anthropic` | `openai` | `bedrock` | `oci` | `local`.
    #[serde(default)]
    pub provider: String,
    /// Provider-local model identifier.
    #[serde(default)]
    pub model: String,
    /// Content digest, when the model is distributed as an OCI artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl ModelRef {
    /// A hosted (API) model reference.
    pub fn hosted(provider: impl Into<String>, model: impl Into<String>) -> Self {
        let provider = provider.into();
        let model = model.into();
        Self {
            uri: format!("{provider}://{model}"),
            provider,
            model,
            digest: None,
        }
    }
}

/// One advertised unit of work the agent can perform.
///
/// `input_schema` / `output_schema` are JSON-Schema documents. They are
/// deliberately untyped [`serde_json::Value`] here so this crate does not have
/// to depend on a schema library; validation is the gateway's job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    /// Machine name, e.g. `review_pull_request`.
    pub name: String,
    /// One-line description for humans and planner agents.
    #[serde(default)]
    pub description: String,
    /// JSON-Schema for the `instructions`/`context` this capability expects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    /// JSON-Schema for the result this capability returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Free-form routing tags, e.g. `["code", "github"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl Capability {
    /// A capability with just a name and description.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: None,
            output_schema: None,
            tags: Vec::new(),
        }
    }
}

/// How the gateway reaches an MCP server on the agent's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// Child process speaking MCP over stdio.
    Stdio,
    /// Streamable-HTTP MCP endpoint.
    #[default]
    Http,
    /// Server-sent-events MCP endpoint (legacy).
    Sse,
}

/// An MCP server the agent depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerRef {
    /// Server name as it appears in tool routing.
    pub name: String,
    /// Transport used to reach it.
    #[serde(default)]
    pub transport: McpTransport,
    /// URL (http/sse) or argv-0 command (stdio).
    #[serde(default)]
    pub endpoint: String,
    /// Tools the agent actually uses. Empty = "whatever the server exposes".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

/// Credential shape a caller must present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthScheme {
    /// No credential (only valid inside a trusted network boundary).
    None,
    /// Static bearer token minted by the AVM control plane.
    Bearer,
    /// Mutual TLS; the peer certificate's SAN carries the caller `agent_id`.
    Mtls,
    /// OIDC-issued JWT.
    OidcJwt {
        /// Expected `iss` claim.
        issuer: String,
        /// Expected `aud` claim.
        audience: String,
    },
}

/// Who may submit work to this agent, and how they prove it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthPolicy {
    /// Accepted credential shapes, most-preferred first.
    pub schemes: Vec<AuthScheme>,
    /// Caller must hold every listed scope string.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_scopes: Vec<String>,
    /// Allow-list of caller `agent_id`s. Empty = any authenticated caller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_agents: Vec<String>,
    /// Whether unauthenticated submissions are accepted at all.
    #[serde(default)]
    pub allow_anonymous: bool,
}

impl Default for AuthPolicy {
    /// Deny-by-default: bearer required, no anonymous callers.
    fn default() -> Self {
        Self {
            schemes: vec![AuthScheme::Bearer],
            required_scopes: Vec::new(),
            allowed_agents: Vec::new(),
            allow_anonymous: false,
        }
    }
}

impl AuthPolicy {
    /// True when `caller` is permitted by the allow-list.
    pub fn permits(&self, caller: &str) -> bool {
        self.allowed_agents.is_empty() || self.allowed_agents.iter().any(|a| a == caller)
    }
}

/// The document served at `/.well-known/agent-card.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    /// Schema identifier; see [`crate::discovery::SCHEMA_VERSION`].
    pub schema_version: String,
    /// Stable AVM agent id.
    pub agent_id: String,
    /// Display name.
    pub name: String,
    /// What this agent is for.
    pub description: String,
    /// Semver of the agent implementation (not of the schema).
    pub version: String,
    /// Base URL this card was served from; A2A paths hang off it.
    #[serde(default)]
    pub url: String,
    /// Scope the agent runs under (system → tenant → project → agent).
    #[serde(default)]
    pub scope: avm_proto::types::Scope,
    /// Backing model.
    pub model_ref: ModelRef,
    /// Advertised capabilities.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// MCP servers the agent depends on.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerRef>,
    /// Inbound authorization policy.
    pub auth_policy: AuthPolicy,
    /// Free-form annotations (owner, repo, cost centre, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl AgentCard {
    /// A minimally-valid card.
    pub fn new(agent_id: impl Into<String>, name: impl Into<String>, model_ref: ModelRef) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            agent_id: agent_id.into(),
            name: name.into(),
            description: String::new(),
            version: "0.1.0".to_string(),
            url: String::new(),
            scope: avm_proto::types::Scope::default(),
            model_ref,
            capabilities: Vec::new(),
            mcp_servers: Vec::new(),
            auth_policy: AuthPolicy::default(),
            metadata: BTreeMap::new(),
        }
    }

    /// Does this card advertise `capability`?
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c.name == capability)
    }

    /// The reference form of this card, for use as `A2ATask::source_agent`.
    pub fn as_ref_(&self) -> AgentRef {
        AgentRef {
            agent_id: self.agent_id.clone(),
            name: self.name.clone(),
            card_url: crate::discovery::card_url(&self.url),
        }
    }

    /// Reject a card whose schema major version we do not implement.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported agent card schema {} (expected {SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        if self.agent_id.is_empty() {
            return Err("agent_id must not be empty".to_string());
        }
        if self.model_ref.uri.is_empty() {
            return Err("model_ref.uri must not be empty".to_string());
        }
        Ok(())
    }

    /// The canonical example card. Served by the gateway's test-only endpoint
    /// and used verbatim in `docs/A2A_AGENT_CARD.md`.
    pub fn example() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            agent_id: "ag_pr_reviewer".to_string(),
            name: "PR Reviewer".to_string(),
            description: "Reviews pull requests for correctness, security, and style.".to_string(),
            version: "0.1.0".to_string(),
            url: "http://127.0.0.1:8080".to_string(),
            scope: avm_proto::types::Scope::project("t_lightheart", "b_avm"),
            model_ref: ModelRef::hosted("anthropic", "claude-sonnet-4-6"),
            capabilities: vec![
                Capability {
                    name: "review_pull_request".to_string(),
                    description: "Review a GitHub pull request and return findings.".to_string(),
                    input_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "repo": { "type": "string" },
                            "pr_number": { "type": "integer" }
                        },
                        "required": ["repo", "pr_number"]
                    })),
                    output_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "findings": { "type": "array", "items": { "type": "string" } },
                            "verdict": { "enum": ["approve", "request_changes", "comment"] }
                        },
                        "required": ["verdict"]
                    })),
                    tags: vec!["code".to_string(), "github".to_string()],
                },
                Capability::new("summarise_diff", "Summarise a unified diff in prose."),
            ],
            mcp_servers: vec![McpServerRef {
                name: "avm-gateway".to_string(),
                transport: McpTransport::Http,
                endpoint: "http://127.0.0.1:8080/mcp".to_string(),
                tools: vec![
                    "avm_memory_read".to_string(),
                    "avm_memory_write".to_string(),
                    "avm_job_submit".to_string(),
                ],
            }],
            auth_policy: AuthPolicy {
                schemes: vec![AuthScheme::Bearer, AuthScheme::Mtls],
                required_scopes: vec!["a2a:submit".to_string()],
                allowed_agents: vec!["ag_planner".to_string()],
                allow_anonymous: false,
            },
            metadata: BTreeMap::from([
                ("owner".to_string(), "platform".to_string()),
                ("repo".to_string(), "LightHeart-Ventures/avm".to_string()),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_card_roundtrips_through_json() {
        let card = AgentCard::example();
        let json = serde_json::to_string(&card).unwrap();
        let back: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(card, back);
    }

    #[test]
    fn example_card_is_valid() {
        AgentCard::example().validate().unwrap();
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let mut card = AgentCard::example();
        card.schema_version = "avm.a2a/v9".to_string();
        assert!(card.validate().is_err());
    }

    #[test]
    fn capability_lookup_works() {
        let card = AgentCard::example();
        assert!(card.has_capability("review_pull_request"));
        assert!(!card.has_capability("deploy_to_prod"));
    }

    #[test]
    fn auth_policy_defaults_deny_anonymous() {
        let p = AuthPolicy::default();
        assert!(!p.allow_anonymous);
        assert_eq!(p.schemes, vec![AuthScheme::Bearer]);
        // empty allow-list = any authenticated caller
        assert!(p.permits("ag_anything"));
    }

    #[test]
    fn auth_policy_allow_list_is_enforced() {
        let card = AgentCard::example();
        assert!(card.auth_policy.permits("ag_planner"));
        assert!(!card.auth_policy.permits("ag_stranger"));
    }

    #[test]
    fn auth_scheme_is_internally_tagged() {
        let json = serde_json::to_string(&AuthScheme::OidcJwt {
            issuer: "https://idp.example".into(),
            audience: "avm".into(),
        })
        .unwrap();
        assert!(json.contains("\"type\":\"oidc_jwt\""), "got {json}");
    }

    #[test]
    fn as_ref_builds_a_discoverable_pointer() {
        let r = AgentCard::example().as_ref_();
        assert_eq!(r.agent_id, "ag_pr_reviewer");
        assert_eq!(
            r.card_url,
            "http://127.0.0.1:8080/.well-known/agent-card.json"
        );
    }

    #[test]
    fn model_ref_hosted_builds_uri() {
        let m = ModelRef::hosted("anthropic", "claude-sonnet-4-6");
        assert_eq!(m.uri, "anthropic://claude-sonnet-4-6");
        assert!(m.digest.is_none());
    }
}
