//! Prometheus-compatible metric registry.
//!
//! A small, dependency-free implementation of the three instrument types AVM
//! needs — counter, gauge, histogram — with a text-exposition renderer so any
//! Prometheus/OTel collector can scrape `GET /metrics`. When an OTLP endpoint
//! is configured the same registry is what the `otlp` feature reads from, so
//! call sites never change based on the export path.
//!
//! Naming follows the Prometheus conventions: `avm_<component>_<thing>_<unit>`,
//! seconds and bytes as base units, `_total` suffix on counters.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Metric names — platform (always exported)
// ---------------------------------------------------------------------------

/// Gateway HTTP request latency, labelled by `endpoint` and `status`.
pub const GATEWAY_REQUEST_DURATION: &str = "avm_gateway_request_duration_seconds";
/// Scheduler placement latency, labelled by `strategy`.
pub const SCHEDULER_PLACEMENT_DURATION: &str = "avm_scheduler_job_placement_duration_seconds";
/// Executor container/process wall time, labelled by `executor_type`, `outcome`.
pub const EXECUTOR_CONTAINER_DURATION: &str = "avm_executor_container_duration_seconds";
/// Queue message size, labelled by `subject`.
pub const QUEUE_MESSAGE_SIZE: &str = "avm_queue_message_size_bytes";
/// Queue depth, labelled by `subject`.
pub const QUEUE_DEPTH: &str = "avm_queue_depth";
/// Queue messages published/consumed, labelled by `subject`, `direction`.
pub const QUEUE_MESSAGES: &str = "avm_queue_messages_total";
/// Model/image pull time, labelled by `model_ref`.
pub const MODEL_PULL_DURATION: &str = "avm_model_pull_duration_seconds";
/// Model cache lookups, labelled by `result` (`hit` | `miss`).
pub const MODEL_CACHE_LOOKUPS: &str = "avm_model_cache_lookups_total";

// ---------------------------------------------------------------------------
// Metric names — tenant / project scoped (opt-in only)
// ---------------------------------------------------------------------------

/// Jobs seen for a tenant, labelled by `tenant_id`, `project_id`, `outcome`.
pub const TENANT_JOB_COUNT: &str = "avm_tenant_job_count_total";
/// Job wall time per tenant, labelled by `tenant_id`, `project_id`.
pub const TENANT_JOB_DURATION: &str = "avm_tenant_job_duration_seconds";
/// Model cache hit ratio per tenant, labelled by `tenant_id`, `model_ref`.
pub const TENANT_MODEL_CACHE_HIT_RATIO: &str = "avm_tenant_model_cache_hit_ratio";

/// Default latency buckets (seconds): sub-ms to ~2 min, log-ish spacing.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// Default size buckets (bytes): 64 B to 4 MiB.
pub const SIZE_BUCKETS: &[f64] = &[
    64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
];

/// Instrument kind, as rendered in the `# TYPE` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// One label set (sorted, so identical sets collapse to one series).
type Labels = BTreeMap<String, String>;

#[derive(Debug, Clone)]
enum Series {
    Scalar(f64),
    Histogram { buckets: Vec<f64>, counts: Vec<u64>, sum: f64, count: u64 },
}

#[derive(Debug, Clone)]
struct Family {
    kind: MetricKind,
    help: String,
    buckets: Vec<f64>,
    series: BTreeMap<Vec<(String, String)>, Series>,
}

/// A process-wide metric registry.
#[derive(Debug, Default)]
pub struct Registry {
    families: Mutex<BTreeMap<String, Family>>,
    constant_labels: Mutex<Vec<(String, String)>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Labels attached to every series (typically the resource identity).
    pub fn set_constant_labels(&self, labels: Vec<(String, String)>) {
        *self.constant_labels.lock().expect("registry poisoned") = labels;
    }

    /// Declare a family up front so `HELP`/`TYPE` are correct even before the
    /// first observation. Optional — instruments auto-register on first use.
    pub fn register(&self, name: &str, kind: MetricKind, help: &str, buckets: &[f64]) {
        let mut families = self.families.lock().expect("registry poisoned");
        families.entry(name.to_string()).or_insert_with(|| Family {
            kind,
            help: help.to_string(),
            buckets: buckets.to_vec(),
            series: BTreeMap::new(),
        });
    }

    /// Increment a counter by `delta`.
    pub fn counter_add(&self, name: &str, labels: &[(&str, &str)], delta: f64) {
        self.with_scalar(name, MetricKind::Counter, labels, |v| *v += delta);
    }

    /// Increment a counter by one.
    pub fn counter_inc(&self, name: &str, labels: &[(&str, &str)]) {
        self.counter_add(name, labels, 1.0);
    }

    /// Set a gauge to an absolute value.
    pub fn gauge_set(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.with_scalar(name, MetricKind::Gauge, labels, |v| *v = value);
    }

    /// Add (or subtract, with a negative delta) to a gauge.
    pub fn gauge_add(&self, name: &str, labels: &[(&str, &str)], delta: f64) {
        self.with_scalar(name, MetricKind::Gauge, labels, |v| *v += delta);
    }

    /// Record a histogram observation using the family's buckets.
    pub fn histogram_observe(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.histogram_observe_with(name, labels, value, LATENCY_BUCKETS);
    }

    /// Record a histogram observation, specifying buckets for first use.
    pub fn histogram_observe_with(
        &self,
        name: &str,
        labels: &[(&str, &str)],
        value: f64,
        buckets: &[f64],
    ) {
        let key = label_key(labels);
        let mut families = self.families.lock().expect("registry poisoned");
        let family = families.entry(name.to_string()).or_insert_with(|| Family {
            kind: MetricKind::Histogram,
            help: format!("{name} (auto-registered)"),
            buckets: buckets.to_vec(),
            series: BTreeMap::new(),
        });
        let family_buckets = family.buckets.clone();
        let series = family.series.entry(key).or_insert_with(|| Series::Histogram {
            buckets: family_buckets.clone(),
            counts: vec![0; family_buckets.len()],
            sum: 0.0,
            count: 0,
        });
        if let Series::Histogram { buckets, counts, sum, count } = series {
            for (i, bound) in buckets.iter().enumerate() {
                if value <= *bound {
                    counts[i] += 1;
                }
            }
            *sum += value;
            *count += 1;
        }
    }

    fn with_scalar(
        &self,
        name: &str,
        kind: MetricKind,
        labels: &[(&str, &str)],
        f: impl FnOnce(&mut f64),
    ) {
        let key = label_key(labels);
        let mut families = self.families.lock().expect("registry poisoned");
        let family = families.entry(name.to_string()).or_insert_with(|| Family {
            kind,
            help: format!("{name} (auto-registered)"),
            buckets: Vec::new(),
            series: BTreeMap::new(),
        });
        let series = family.series.entry(key).or_insert(Series::Scalar(0.0));
        if let Series::Scalar(v) = series {
            f(v);
        }
    }

    /// Current value of a scalar series (test/introspection helper).
    pub fn value(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let families = self.families.lock().expect("registry poisoned");
        match families.get(name)?.series.get(&label_key(labels))? {
            Series::Scalar(v) => Some(*v),
            Series::Histogram { count, .. } => Some(*count as f64),
        }
    }

    /// Number of distinct series in a family (test/introspection helper).
    pub fn series_count(&self, name: &str) -> usize {
        let families = self.families.lock().expect("registry poisoned");
        families.get(name).map(|f| f.series.len()).unwrap_or(0)
    }

    /// Render the whole registry in Prometheus text exposition format v0.0.4.
    pub fn encode_prometheus(&self) -> String {
        let families = self.families.lock().expect("registry poisoned");
        let constant = self.constant_labels.lock().expect("registry poisoned").clone();
        let mut out = String::new();

        for (name, family) in families.iter() {
            out.push_str(&format!("# HELP {name} {}\n", family.help));
            out.push_str(&format!("# TYPE {name} {}\n", family.kind.as_str()));
            for (labels, series) in &family.series {
                match series {
                    Series::Scalar(v) => {
                        out.push_str(&format!(
                            "{name}{} {}\n",
                            render_labels(labels, &constant, None),
                            fmt_f64(*v)
                        ));
                    }
                    Series::Histogram { buckets, counts, sum, count } => {
                        for (bound, c) in buckets.iter().zip(counts) {
                            out.push_str(&format!(
                                "{name}_bucket{} {}\n",
                                render_labels(labels, &constant, Some(("le", &fmt_f64(*bound)))),
                                c
                            ));
                        }
                        out.push_str(&format!(
                            "{name}_bucket{} {}\n",
                            render_labels(labels, &constant, Some(("le", "+Inf"))),
                            count
                        ));
                        out.push_str(&format!(
                            "{name}_sum{} {}\n",
                            render_labels(labels, &constant, None),
                            fmt_f64(*sum)
                        ));
                        out.push_str(&format!(
                            "{name}_count{} {}\n",
                            render_labels(labels, &constant, None),
                            count
                        ));
                    }
                }
            }
        }
        out
    }
}

/// The process-wide registry every AVM service records into.
pub fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::new)
}

/// Render the process registry for a `/metrics` handler.
pub fn encode_prometheus() -> String {
    registry().encode_prometheus()
}

/// Normalise an arbitrary key into a legal Prometheus metric/label name.
pub fn sanitize_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if out.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(true) {
        out.insert(0, '_');
    }
    out
}

fn label_key(labels: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut map: Labels = Labels::new();
    for (k, v) in labels {
        map.insert(sanitize_name(k), v.to_string());
    }
    map.into_iter().collect()
}

fn render_labels(
    labels: &[(String, String)],
    constant: &[(String, String)],
    extra: Option<(&str, &str)>,
) -> String {
    let mut parts: Vec<String> = constant
        .iter()
        .chain(labels.iter())
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{}\"", escape(v)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn fmt_f64(v: f64) -> String {
    if v.is_infinite() {
        return if v.is_sign_positive() { "+Inf".into() } else { "-Inf".into() };
    }
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_per_label_set() {
        let r = Registry::new();
        r.counter_inc(TENANT_JOB_COUNT, &[("tenant_id", "t_a")]);
        r.counter_inc(TENANT_JOB_COUNT, &[("tenant_id", "t_a")]);
        r.counter_inc(TENANT_JOB_COUNT, &[("tenant_id", "t_b")]);
        assert_eq!(r.value(TENANT_JOB_COUNT, &[("tenant_id", "t_a")]), Some(2.0));
        assert_eq!(r.value(TENANT_JOB_COUNT, &[("tenant_id", "t_b")]), Some(1.0));
        assert_eq!(r.series_count(TENANT_JOB_COUNT), 2);
    }

    #[test]
    fn label_order_does_not_split_a_series() {
        let r = Registry::new();
        r.counter_inc("x_total", &[("a", "1"), ("b", "2")]);
        r.counter_inc("x_total", &[("b", "2"), ("a", "1")]);
        assert_eq!(r.series_count("x_total"), 1);
        assert_eq!(r.value("x_total", &[("a", "1"), ("b", "2")]), Some(2.0));
    }

    #[test]
    fn gauges_set_and_add() {
        let r = Registry::new();
        r.gauge_set(QUEUE_DEPTH, &[("subject", "avm.jobs.t_a.b_1")], 12.0);
        r.gauge_add(QUEUE_DEPTH, &[("subject", "avm.jobs.t_a.b_1")], -2.0);
        assert_eq!(r.value(QUEUE_DEPTH, &[("subject", "avm.jobs.t_a.b_1")]), Some(10.0));
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let r = Registry::new();
        r.register(
            GATEWAY_REQUEST_DURATION,
            MetricKind::Histogram,
            "request latency",
            LATENCY_BUCKETS,
        );
        for v in [0.002, 0.2, 3.0] {
            r.histogram_observe(GATEWAY_REQUEST_DURATION, &[("endpoint", "/mcp/call")], v);
        }
        let text = r.encode_prometheus();
        assert!(text.contains("# TYPE avm_gateway_request_duration_seconds histogram"));
        assert!(text.contains("avm_gateway_request_duration_seconds_count{endpoint=\"/mcp/call\"} 3"));
        assert!(text.contains("le=\"+Inf\""));
        // 0.002 lands in the 0.005 bucket and every wider one.
        assert!(text.contains("le=\"0.005\"} 1"));
    }

    #[test]
    fn constant_labels_are_prefixed_onto_every_series() {
        let r = Registry::new();
        r.set_constant_labels(vec![("service_name".into(), "avm-gateway".into())]);
        r.counter_inc("avm_x_total", &[("k", "v")]);
        let text = r.encode_prometheus();
        assert!(text.contains("avm_x_total{service_name=\"avm-gateway\",k=\"v\"} 1"), "{text}");
    }

    #[test]
    fn label_values_are_escaped() {
        let r = Registry::new();
        r.counter_inc("avm_x_total", &[("k", "a\"b\\c")]);
        assert!(r.encode_prometheus().contains("k=\"a\\\"b\\\\c\""));
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("service.name"), "service_name");
        assert_eq!(sanitize_name("9lives"), "_9lives");
        assert_eq!(sanitize_name("ok_name"), "ok_name");
    }

    #[test]
    fn exposition_has_help_and_type_lines() {
        let r = Registry::new();
        r.register(QUEUE_DEPTH, MetricKind::Gauge, "pending messages", &[]);
        r.gauge_set(QUEUE_DEPTH, &[("subject", "s")], 1.0);
        let text = r.encode_prometheus();
        assert!(text.contains("# HELP avm_queue_depth pending messages"));
        assert!(text.contains("# TYPE avm_queue_depth gauge"));
    }
}
