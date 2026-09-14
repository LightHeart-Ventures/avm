//! W3C Trace Context propagation.
//!
//! One representation is used everywhere in AVM:
//!
//! * **HTTP** — the `traceparent` / `tracestate` headers (gateway ingress).
//! * **NATS** — the same two names as JetStream message headers, so a job
//!   envelope keeps its trace across the queue hop.
//! * **Agent processes** — exported as `OTEL_TRACE_ID`, `OTEL_SPAN_ID`,
//!   `OTEL_PARENT_SPAN_ID` and `TRACEPARENT` environment variables.
//!
//! Spec: <https://www.w3.org/TR/trace-context/>

use serde::{Deserialize, Serialize};

/// Header / env-var names used for propagation.
pub const TRACEPARENT: &str = "traceparent";
pub const TRACESTATE: &str = "tracestate";
pub const ENV_TRACEPARENT: &str = "TRACEPARENT";
pub const ENV_TRACE_ID: &str = "OTEL_TRACE_ID";
pub const ENV_SPAN_ID: &str = "OTEL_SPAN_ID";
pub const ENV_PARENT_SPAN_ID: &str = "OTEL_PARENT_SPAN_ID";

/// A propagated span context: trace id, span id, sampling flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    /// 32 lowercase hex chars.
    pub trace_id: String,
    /// 16 lowercase hex chars — the *current* span, i.e. the parent of whatever
    /// the receiver creates next.
    pub span_id: String,
    /// The `sampled` bit of `trace-flags`.
    pub sampled: bool,
    /// Opaque vendor state, forwarded verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trace_state: String,
}

impl TraceContext {
    /// Build a context from raw ids.
    pub fn new(trace_id: impl Into<String>, span_id: impl Into<String>, sampled: bool) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            sampled,
            trace_state: String::new(),
        }
    }

    /// Start a brand-new root context. `seed` supplies entropy; callers pass a
    /// UUIDv4's bytes (or any 16 random bytes) so the crate needs no RNG dep.
    pub fn root_from_seed(seed: &[u8; 16], sampled: bool) -> Self {
        let trace_id = hex16(seed);
        // Derive a distinct span id from the tail so root spans never reuse the
        // first 8 bytes of the trace id (some backends flag that as suspicious).
        let mut span_bytes = [0u8; 8];
        span_bytes.copy_from_slice(&seed[8..16]);
        span_bytes[0] ^= 0xa5;
        Self::new(trace_id, hex8(&span_bytes), sampled)
    }

    /// The 16 raw trace-id bytes, for deterministic ratio sampling.
    pub fn trace_id_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (i, slot) in out.iter_mut().enumerate() {
            let lo = i * 2;
            *slot = self
                .trace_id
                .get(lo..lo + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
                .unwrap_or(0);
        }
        out
    }

    /// Render the `traceparent` header value (version `00`).
    pub fn to_traceparent(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.trace_id,
            self.span_id,
            if self.sampled { "01" } else { "00" }
        )
    }

    /// Parse a `traceparent` header. Returns `None` for any malformed or
    /// all-zero value, per the spec's "restart the trace" rule.
    pub fn parse_traceparent(value: &str) -> Option<Self> {
        let mut parts = value.trim().split('-');
        let version = parts.next()?;
        let trace_id = parts.next()?;
        let span_id = parts.next()?;
        let flags = parts.next()?;

        if version.len() != 2 || version == "ff" || !is_hex(version) {
            return None;
        }
        if trace_id.len() != 32 || !is_hex(trace_id) || trace_id.bytes().all(|b| b == b'0') {
            return None;
        }
        if span_id.len() != 16 || !is_hex(span_id) || span_id.bytes().all(|b| b == b'0') {
            return None;
        }
        if flags.len() != 2 || !is_hex(flags) {
            return None;
        }
        let sampled = u8::from_str_radix(flags, 16).ok()? & 0x01 == 0x01;
        Some(Self {
            trace_id: trace_id.to_ascii_lowercase(),
            span_id: span_id.to_ascii_lowercase(),
            sampled,
            trace_state: String::new(),
        })
    }

    /// Extract a context from any `(name, value)` header source (HTTP headers,
    /// NATS headers, a map). Header names are matched case-insensitively.
    pub fn extract<'a, I>(headers: I) -> Option<Self>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut ctx: Option<Self> = None;
        let mut state: Option<String> = None;
        for (name, value) in headers {
            match name.to_ascii_lowercase().as_str() {
                TRACEPARENT => ctx = Self::parse_traceparent(value),
                TRACESTATE => state = Some(value.to_string()),
                _ => {}
            }
        }
        ctx.map(|mut c| {
            if let Some(s) = state {
                c.trace_state = s;
            }
            c
        })
    }

    /// The headers to inject on an outbound HTTP request or NATS message.
    pub fn inject(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![(TRACEPARENT, self.to_traceparent())];
        if !self.trace_state.is_empty() {
            out.push((TRACESTATE, self.trace_state.clone()));
        }
        out
    }

    /// Environment variables handed to an instrumented agent process.
    ///
    /// `child_span_id` is the span the executor created for this run; the agent
    /// sees it as its parent.
    pub fn agent_env(&self, child_span_id: &str) -> Vec<(&'static str, String)> {
        vec![
            (ENV_TRACE_ID, self.trace_id.clone()),
            (ENV_SPAN_ID, child_span_id.to_string()),
            (ENV_PARENT_SPAN_ID, self.span_id.clone()),
            (
                ENV_TRACEPARENT,
                format!(
                    "00-{}-{}-{}",
                    self.trace_id,
                    child_span_id,
                    if self.sampled { "01" } else { "00" }
                ),
            ),
        ]
    }

    /// Derive a child context whose current span is `span_id`.
    pub fn child(&self, span_id: impl Into<String>) -> Self {
        Self {
            trace_id: self.trace_id.clone(),
            span_id: span_id.into(),
            sampled: self.sampled,
            trace_state: self.trace_state.clone(),
        }
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex16(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex8(bytes: &[u8; 8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn traceparent_roundtrips() {
        let ctx = TraceContext::parse_traceparent(VALID).expect("valid header");
        assert_eq!(ctx.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(ctx.span_id, "00f067aa0ba902b7");
        assert!(ctx.sampled);
        assert_eq!(ctx.to_traceparent(), VALID);
    }

    #[test]
    fn unsampled_flag_is_preserved() {
        let ctx = TraceContext::parse_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
        )
        .unwrap();
        assert!(!ctx.sampled);
    }

    #[test]
    fn malformed_headers_are_rejected() {
        for bad in [
            "",
            "garbage",
            "00-4bf92f35-00f067aa0ba902b7-01",              // short trace id
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01", // all-zero trace
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01", // all-zero span
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", // forbidden version
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-zz", // bad flags
        ] {
            assert!(TraceContext::parse_traceparent(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn extract_is_case_insensitive_and_keeps_tracestate() {
        let ctx = TraceContext::extract([("TraceParent", VALID), ("TraceState", "vendor=1")])
            .expect("extracted");
        assert_eq!(ctx.trace_state, "vendor=1");
        assert_eq!(ctx.inject().len(), 2);
    }

    #[test]
    fn extract_returns_none_without_traceparent() {
        assert!(TraceContext::extract([("content-type", "application/json")]).is_none());
    }

    #[test]
    fn agent_env_points_the_agent_at_the_executor_span() {
        let ctx = TraceContext::parse_traceparent(VALID).unwrap();
        let env: std::collections::HashMap<_, _> =
            ctx.agent_env("aaaaaaaaaaaaaaaa").into_iter().collect();
        assert_eq!(env[ENV_TRACE_ID], ctx.trace_id);
        assert_eq!(env[ENV_SPAN_ID], "aaaaaaaaaaaaaaaa");
        assert_eq!(env[ENV_PARENT_SPAN_ID], ctx.span_id);
        assert!(env[ENV_TRACEPARENT].ends_with("-01"));
    }

    #[test]
    fn root_context_is_well_formed_and_stable() {
        let seed = [7u8; 16];
        let ctx = TraceContext::root_from_seed(&seed, true);
        assert_eq!(ctx.trace_id.len(), 32);
        assert_eq!(ctx.span_id.len(), 16);
        assert!(TraceContext::parse_traceparent(&ctx.to_traceparent()).is_some());
        assert_eq!(ctx.trace_id_bytes(), seed);
    }

    #[test]
    fn child_keeps_trace_and_sampling() {
        let ctx = TraceContext::parse_traceparent(VALID).unwrap();
        let child = ctx.child("bbbbbbbbbbbbbbbb");
        assert_eq!(child.trace_id, ctx.trace_id);
        assert_eq!(child.sampled, ctx.sampled);
        assert_ne!(child.span_id, ctx.span_id);
    }
}
