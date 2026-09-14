//! End-to-end trace-context propagation across the AVM hop chain.
//!
//! This exercises the contract the four services rely on, without needing NATS,
//! Postgres or a real OTLP endpoint:
//!
//! ```text
//! gateway.dispatch_job            (root, W3C traceparent minted at ingress)
//!   └─ scheduler.place_job        (carried in the JobMessage envelope)
//!        └─ executor.run_container
//!             └─ container.run    (agent env when tenant opted into detailed)
//!   └─ queue.publish_result       (carried back in the ResultMessage envelope)
//! ```
//!
//! A `MockCollector` stands in for the OTLP backend: every hop records the span
//! it would export, and the assertions are the ones an operator actually cares
//! about — one trace id for the whole journey, correct parent links, and no
//! tenant data escaping when the tenant did not opt in.

use std::sync::{Arc, Mutex};

use avm_otel::propagation::{self, TRACEPARENT};
use avm_otel::{InstrumentationConfig, InstrumentationLevel, TraceContext};

/// One exported span, reduced to what we assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MockSpan {
    name: &'static str,
    trace_id: String,
    span_id: String,
    parent_span_id: String,
    attributes: Vec<(String, String)>,
}

impl MockSpan {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// In-process stand-in for an OTLP collector.
#[derive(Debug, Clone, Default)]
struct MockCollector {
    spans: Arc<Mutex<Vec<MockSpan>>>,
}

impl MockCollector {
    /// Record a span as a child of `parent`, returning the child's context so
    /// the next hop can continue the trace.
    fn record(
        &self,
        name: &'static str,
        parent: &TraceContext,
        attributes: &[(&str, &str)],
    ) -> TraceContext {
        let child = parent.child(propagation::new_span_id());
        self.spans.lock().unwrap().push(MockSpan {
            name,
            trace_id: child.trace_id.clone(),
            span_id: child.span_id.clone(),
            parent_span_id: parent.span_id.clone(),
            attributes: attributes
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        });
        child
    }

    /// Record the root span of a trace (no parent).
    fn record_root(&self, name: &'static str, ctx: &TraceContext, attributes: &[(&str, &str)]) {
        self.spans.lock().unwrap().push(MockSpan {
            name,
            trace_id: ctx.trace_id.clone(),
            span_id: ctx.span_id.clone(),
            parent_span_id: String::new(),
            attributes: attributes
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        });
    }

    fn exported(&self) -> Vec<MockSpan> {
        self.spans.lock().unwrap().clone()
    }

    fn get(&self, name: &str) -> MockSpan {
        self.exported()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("span `{name}` was never exported"))
    }
}

/// Drive one job through all four services at the given instrumentation level.
///
/// Returns the collector plus the environment the agent process would receive.
fn run_journey(level: InstrumentationLevel) -> (MockCollector, Vec<(&'static str, String)>) {
    let collector = MockCollector::default();
    let tenant = "t_acme";
    let project = "b_pipeline";

    // --- hop 1: gateway ingress -------------------------------------------
    // No inbound traceparent, so the gateway mints a root and samples it.
    let root = TraceContext::root_from_seed(&propagation::new_trace_id_seed(), true);
    collector.record_root(
        "gateway.dispatch_job",
        &root,
        &[
            ("tenant_id", tenant),
            ("project_id", project),
            ("instrumentation_level", level.as_str()),
        ],
    );

    // The envelope carries the context onto JetStream as a plain header pair.
    let envelope_headers = root.inject();
    let traceparent = envelope_headers
        .iter()
        .find(|(k, _)| *k == TRACEPARENT)
        .map(|(_, v)| v.clone())
        .expect("dispatch must inject a traceparent");

    // --- hop 2: scheduler ---------------------------------------------------
    let from_envelope =
        TraceContext::parse_traceparent(&traceparent).expect("envelope traceparent parses");
    let placement = collector.record(
        "scheduler.place_job",
        &from_envelope,
        &[("strategy", "requeue")],
    );
    collector.record(
        "scheduler.node_selection_score",
        &placement,
        &[("candidates", "1")],
    );

    // --- hop 3: executor ----------------------------------------------------
    let run = collector.record(
        "executor.run_container",
        &from_envelope,
        &[
            ("tenant_id", tenant),
            ("project_id", project),
            ("executor_type", "process"),
        ],
    );
    let container = collector.record("container.run", &run, &[("agent_id", "ag_summarize")]);

    // Agent env is only populated for `detailed`.
    let agent_env = if level.agent_tracing() {
        container.agent_env(&propagation::new_span_id())
    } else {
        Vec::new()
    };

    // --- hop 4: result publish ---------------------------------------------
    collector.record("queue.publish_result", &run, &[("outcome", "succeeded")]);

    (collector, agent_env)
}

#[test]
fn one_trace_id_spans_the_whole_journey() {
    let (collector, _) = run_journey(InstrumentationLevel::Basic);
    let spans = collector.exported();
    assert_eq!(spans.len(), 6, "every hop exports exactly one span");

    let trace_id = &spans[0].trace_id;
    for span in &spans {
        assert_eq!(
            &span.trace_id, trace_id,
            "span `{}` escaped the trace",
            span.name
        );
    }
}

#[test]
fn parent_links_reproduce_the_documented_shape() {
    let (collector, _) = run_journey(InstrumentationLevel::Basic);

    let root = collector.get("gateway.dispatch_job");
    assert!(root.parent_span_id.is_empty(), "gateway span is the root");

    let place = collector.get("scheduler.place_job");
    assert_eq!(place.parent_span_id, root.span_id);

    let score = collector.get("scheduler.node_selection_score");
    assert_eq!(score.parent_span_id, place.span_id);

    let run = collector.get("executor.run_container");
    assert_eq!(run.parent_span_id, root.span_id);

    let container = collector.get("container.run");
    assert_eq!(container.parent_span_id, run.span_id);

    let publish = collector.get("queue.publish_result");
    assert_eq!(publish.parent_span_id, run.span_id);
}

#[test]
fn identity_attributes_ride_along_every_scoped_span() {
    let (collector, _) = run_journey(InstrumentationLevel::Detailed);
    for name in ["gateway.dispatch_job", "executor.run_container"] {
        let span = collector.get(name);
        assert_eq!(span.attr("tenant_id"), Some("t_acme"), "{name}");
        assert_eq!(span.attr("project_id"), Some("b_pipeline"), "{name}");
    }
}

#[test]
fn only_detailed_reaches_the_agent_process() {
    let (_, off_env) = run_journey(InstrumentationLevel::Off);
    assert!(off_env.is_empty(), "opted-out tenants get no trace env");

    let (_, basic_env) = run_journey(InstrumentationLevel::Basic);
    assert!(
        basic_env.is_empty(),
        "`basic` is metrics-only; the agent stays untouched"
    );

    let (collector, detailed_env) = run_journey(InstrumentationLevel::Detailed);
    let names: Vec<&str> = detailed_env.iter().map(|(k, _)| *k).collect();
    assert!(names.contains(&propagation::ENV_TRACE_ID));
    assert!(names.contains(&propagation::ENV_SPAN_ID));
    assert!(names.contains(&propagation::ENV_PARENT_SPAN_ID));
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case(TRACEPARENT)),
        "agent must also receive a raw W3C traceparent: {names:?}"
    );

    // The agent's trace id is the AVM trace id — that is the whole point.
    let trace_id = detailed_env
        .iter()
        .find(|(k, _)| *k == propagation::ENV_TRACE_ID)
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(trace_id, collector.get("container.run").trace_id);
}

#[test]
fn an_upstream_sampling_decision_is_honoured_end_to_end() {
    // A caller that sampled the trace out sends `-00`; AVM must not resurrect
    // it, or head-based sampling stops meaning anything.
    let parent = TraceContext::parse_traceparent(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
    )
    .expect("valid traceparent");
    assert!(!parent.sampled);

    let child = parent.child(propagation::new_span_id());
    assert!(!child.sampled, "sampling decision must survive the hop");
    assert!(child.to_traceparent().ends_with("-00"));
    assert_eq!(child.trace_id, parent.trace_id);
}

#[test]
fn instrumentation_config_scopes_to_the_right_tenant() {
    let acme = InstrumentationConfig {
        tenant_id: "t_acme".into(),
        project_id: String::new(),
        level: InstrumentationLevel::Detailed,
        sample_ratio: 1.0,
    };
    assert!(acme.matches("t_acme", "b_anything"));
    assert!(!acme.matches("t_other", "b_anything"));

    let scoped = InstrumentationConfig {
        project_id: "b_pipeline".into(),
        ..acme
    };
    assert!(scoped.matches("t_acme", "b_pipeline"));
    assert!(
        !scoped.matches("t_acme", "b_other"),
        "a project-scoped opt-in must not leak to sibling projects"
    );
}
