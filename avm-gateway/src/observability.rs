//! Gateway-side observability.
//!
//! Three things live here, all of them ingress concerns:
//!
//! 1. [`trace_requests`] — the always-on platform middleware. Every inbound
//!    request gets a span, the W3C trace context is continued (or started),
//!    and `avm_gateway_request_duration_seconds` is observed.
//! 2. [`metrics_handler`] — `GET /metrics`, Prometheus text exposition of the
//!    process registry owned by `avm-otel`.
//! 3. [`InstrumentationStore`] + `/config/instrumentation` — the tenant /
//!    project **opt-in** surface. Nothing tenant-labelled is emitted until an
//!    operator writes a row here.
//!
//! Privacy: the middleware labels series with the *matched route* rather than
//! the raw URI, so path parameters (which can carry tenant or job identifiers)
//! never become metric cardinality or span attributes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use avm_otel::propagation::{self, TRACEPARENT, TRACESTATE};
use avm_otel::{metrics, InstrumentationConfig, InstrumentationLevel, TraceContext};
use axum::{
    extract::{MatchedPath, Query, Request, State},
    http::{header::HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

/// Label used when a request does not match any route (404s, malformed URIs).
const UNMATCHED: &str = "unmatched";

// ---------------------------------------------------------------------------
// Instrumentation opt-in store
// ---------------------------------------------------------------------------

/// In-memory projection of the `avm_config` instrumentation rows.
///
/// The gateway is the only component that needs to *resolve* a level: once
/// resolved it is stamped onto the job envelope and every downstream hop reads
/// it from there. Backed by a map today; the persistent `avm_config` table
/// replaces the map without changing this API.
#[derive(Debug, Clone, Default)]
pub struct InstrumentationStore {
    inner: Arc<Mutex<BTreeMap<(String, String), InstrumentationConfig>>>,
}

impl InstrumentationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the row for `(tenant_id, project_id)`.
    pub fn upsert(&self, cfg: InstrumentationConfig) -> InstrumentationConfig {
        let cfg = cfg.normalized();
        self.inner
            .lock()
            .expect("instrumentation store poisoned")
            .insert((cfg.tenant_id.clone(), cfg.project_id.clone()), cfg.clone());
        cfg
    }

    /// Most specific match wins: an exact `(tenant, project)` row, else the
    /// tenant-wide row, else `off`. Fail-closed by construction — an unknown
    /// tenant can never resolve to anything but platform-only telemetry.
    pub fn resolve(&self, tenant_id: &str, project_id: &str) -> InstrumentationConfig {
        let map = self.inner.lock().expect("instrumentation store poisoned");
        map.get(&(tenant_id.to_string(), project_id.to_string()))
            .or_else(|| map.get(&(tenant_id.to_string(), String::new())))
            .cloned()
            .unwrap_or_else(|| InstrumentationConfig::off(tenant_id))
    }

    /// Every configured row, tenant-major.
    pub fn list(&self) -> Vec<InstrumentationConfig> {
        self.inner
            .lock()
            .expect("instrumentation store poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Drop a row, returning whether one existed.
    pub fn remove(&self, tenant_id: &str, project_id: &str) -> bool {
        self.inner
            .lock()
            .expect("instrumentation store poisoned")
            .remove(&(tenant_id.to_string(), project_id.to_string()))
            .is_some()
    }
}

/// `GET /config/instrumentation` query.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResolveQuery {
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub project_id: String,
}

/// `GET /config/instrumentation` response.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedInstrumentation {
    #[serde(flatten)]
    pub config: InstrumentationConfig,
    /// True when tenant/project-labelled series are permitted.
    pub tenant_metrics: bool,
    /// True when agent processes receive trace context.
    pub agent_tracing: bool,
}

impl From<InstrumentationConfig> for ResolvedInstrumentation {
    fn from(config: InstrumentationConfig) -> Self {
        Self {
            tenant_metrics: config.level.tenant_metrics(),
            agent_tracing: config.level.agent_tracing(),
            config,
        }
    }
}

async fn get_instrumentation(
    State(store): State<InstrumentationStore>,
    Query(q): Query<ResolveQuery>,
) -> impl IntoResponse {
    if q.tenant_id.is_empty() {
        return Json(
            store
                .list()
                .into_iter()
                .map(ResolvedInstrumentation::from)
                .collect::<Vec<_>>(),
        )
        .into_response();
    }
    Json(ResolvedInstrumentation::from(
        store.resolve(&q.tenant_id, &q.project_id),
    ))
    .into_response()
}

async fn post_instrumentation(
    State(store): State<InstrumentationStore>,
    Json(cfg): Json<InstrumentationConfig>,
) -> impl IntoResponse {
    if cfg.tenant_id.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "tenant_id is required").into_response();
    }
    let saved = store.upsert(cfg);
    tracing::info!(
        tenant_id = %saved.tenant_id,
        project_id = %saved.project_id,
        level = %saved.level,
        sample_ratio = saved.sample_ratio,
        "instrumentation level updated"
    );
    (StatusCode::OK, Json(ResolvedInstrumentation::from(saved))).into_response()
}

// ---------------------------------------------------------------------------
// Metrics endpoint
// ---------------------------------------------------------------------------

/// `GET /metrics` — Prometheus text exposition (OpenMetrics-compatible).
pub async fn metrics_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        metrics::encode_prometheus(),
    )
}

// ---------------------------------------------------------------------------
// Request middleware
// ---------------------------------------------------------------------------

/// Always-on platform middleware: span + latency histogram + trace context.
///
/// Runs for every request regardless of tenant opt-in — platform telemetry is
/// not sampled away, because it is what an operator debugs an incident with.
pub async fn trace_requests(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = req.method().as_str().to_owned();
    let endpoint = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| UNMATCHED.to_owned());

    // Continue an upstream trace when one arrives, otherwise start a root.
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_owned(), s.to_owned())))
        .collect();
    let inbound = TraceContext::extract(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let ctx = match inbound {
        Some(parent) => parent.child(propagation::new_span_id()),
        None => TraceContext::root_from_seed(&propagation::new_trace_id_seed(), true),
    };

    let span = tracing::info_span!(
        "gateway.http_request",
        otel.kind = "server",
        http.method = %method,
        http.route = %endpoint,
        trace_id = %ctx.trace_id,
        span_id = %ctx.span_id,
        http.status_code = tracing::field::Empty,
    );
    let _entered = span.enter();

    let mut response = next.run(req).await;
    let status = response.status().as_u16();
    span.record("http.status_code", status);

    let elapsed = started.elapsed().as_secs_f64();
    let status_label = status.to_string();
    metrics::registry().histogram_observe(
        metrics::GATEWAY_REQUEST_DURATION,
        &[("endpoint", endpoint.as_str()), ("status", status_label.as_str())],
        elapsed,
    );

    // Echo the context so a caller can stitch its own client span onto ours.
    if let Ok(v) = HeaderValue::from_str(&ctx.to_traceparent()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(TRACEPARENT), v);
    }
    if !ctx.trace_state.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&ctx.trace_state) {
            response
                .headers_mut()
                .insert(HeaderName::from_static(TRACESTATE), v);
        }
    }

    if status >= 500 {
        tracing::error!(http.route = %endpoint, status, elapsed_s = elapsed, "request failed");
    } else {
        tracing::debug!(http.route = %endpoint, status, elapsed_s = elapsed, "request served");
    }

    response
}

/// Routes owned by this module: the scrape endpoint and the opt-in API.
pub fn router(store: InstrumentationStore) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .route(
            "/config/instrumentation",
            get(get_instrumentation).post(post_instrumentation),
        )
        .with_state(store)
}

/// Convenience alias so `post` stays referenced when the route list changes.
#[allow(dead_code)]
fn _post_marker() -> axum::routing::MethodRouter<InstrumentationStore> {
    post(post_instrumentation)
}

/// Level resolution helper used by the job-dispatch path.
///
/// Returns the level to stamp on the envelope together with the sampling ratio
/// the gateway should apply to this tenant's traces.
pub fn resolve_for_dispatch(
    store: &InstrumentationStore,
    tenant_id: &str,
    project_id: &str,
) -> (InstrumentationLevel, f64) {
    let cfg = store.resolve(tenant_id, project_id);
    (cfg.level, cfg.sample_ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_tenant_resolves_to_off() {
        let store = InstrumentationStore::new();
        let (level, ratio) = resolve_for_dispatch(&store, "t_unknown", "b_any");
        assert_eq!(level, InstrumentationLevel::Off);
        assert_eq!(ratio, 1.0);
        assert!(!level.tenant_metrics(), "off must not emit tenant series");
    }

    #[test]
    fn project_row_beats_tenant_row() {
        let store = InstrumentationStore::new();
        store.upsert(InstrumentationConfig {
            tenant_id: "t_acme".into(),
            project_id: String::new(),
            level: InstrumentationLevel::Basic,
            sample_ratio: 0.1,
        });
        store.upsert(InstrumentationConfig {
            tenant_id: "t_acme".into(),
            project_id: "b_hot".into(),
            level: InstrumentationLevel::Detailed,
            sample_ratio: 1.0,
        });

        assert_eq!(
            store.resolve("t_acme", "b_cold").level,
            InstrumentationLevel::Basic
        );
        assert_eq!(
            store.resolve("t_acme", "b_hot").level,
            InstrumentationLevel::Detailed
        );
        // Another tenant is untouched by either row.
        assert_eq!(
            store.resolve("t_other", "b_hot").level,
            InstrumentationLevel::Off
        );
    }

    #[test]
    fn remove_reverts_to_off() {
        let store = InstrumentationStore::new();
        store.upsert(InstrumentationConfig {
            tenant_id: "t_acme".into(),
            project_id: String::new(),
            level: InstrumentationLevel::Detailed,
            sample_ratio: 1.0,
        });
        assert!(store.remove("t_acme", ""));
        assert_eq!(store.resolve("t_acme", "").level, InstrumentationLevel::Off);
    }
}
