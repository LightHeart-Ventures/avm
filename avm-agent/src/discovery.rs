//! Well-known paths and schema identifiers for A2A discovery.

/// Schema identifier stamped on every Agent Card and A2A envelope.
///
/// Consumers MUST reject a document whose `schema_version` major version they
/// do not understand; minor bumps are additive-only.
pub const SCHEMA_VERSION: &str = "avm.a2a/v1";

/// Path an agent serves its [`crate::AgentCard`] from.
///
/// Matches the Linux Foundation A2A well-known location so off-the-shelf A2A
/// clients can discover an AVM agent without special-casing.
pub const AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// Path an agent accepts inbound [`crate::A2ATask`] submissions on.
pub const A2A_TASK_PATH: &str = "/a2a/task";

/// Join a base URL with a well-known path, collapsing a duplicate slash.
///
/// ```
/// use avm_agent::discovery::{card_url, task_url};
/// assert_eq!(card_url("https://a.example/"), "https://a.example/.well-known/agent-card.json");
/// assert_eq!(task_url("https://a.example"), "https://a.example/a2a/task");
/// ```
pub fn card_url(base: &str) -> String {
    join(base, AGENT_CARD_PATH)
}

/// Absolute URL of an agent's task endpoint. See [`card_url`].
pub fn task_url(base: &str) -> String {
    join(base, A2A_TASK_PATH)
}

fn join(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_never_double_slash() {
        assert_eq!(
            card_url("https://agents.avm.io/reviewer/"),
            "https://agents.avm.io/reviewer/.well-known/agent-card.json"
        );
        assert_eq!(
            task_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/a2a/task"
        );
    }

    #[test]
    fn well_known_path_matches_a2a_convention() {
        assert_eq!(AGENT_CARD_PATH, "/.well-known/agent-card.json");
    }
}
