//! Head-based trace sampling.
//!
//! The sampling decision is taken once, at ingress (the gateway), and then
//! *inherited* by every downstream span through the W3C trace-context
//! `sampled` flag. That keeps a trace all-or-nothing: you never get a job with
//! the scheduler span present but the executor span silently dropped.
//!
//! Tail-based sampling is deliberately out of scope here — it belongs in the
//! collector, where the whole trace is visible.

/// Head-based sampler, mirroring the `OTEL_TRACES_SAMPLER` spec values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SamplerSpec {
    /// Sample every trace. AVM's platform default.
    AlwaysOn,
    /// Sample nothing.
    AlwaysOff,
    /// Sample a fixed ratio in `[0.0, 1.0]`.
    TraceIdRatio(f64),
    /// Respect an upstream decision; use `inner` when there is no parent.
    ParentBased(&'static SamplerSpec),
}

const ALWAYS_ON: SamplerSpec = SamplerSpec::AlwaysOn;
const ALWAYS_OFF: SamplerSpec = SamplerSpec::AlwaysOff;

impl Default for SamplerSpec {
    /// Platform default: **100 % sampled**. AVM's own control plane is
    /// always-on so security and performance debugging never hit a sampling
    /// blind spot; per-tenant ratios are applied on top of this.
    fn default() -> Self {
        SamplerSpec::ParentBased(&ALWAYS_ON)
    }
}

impl SamplerSpec {
    /// Parse `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG`.
    pub fn from_env() -> Self {
        let name = std::env::var("OTEL_TRACES_SAMPLER").unwrap_or_default();
        let arg = std::env::var("OTEL_TRACES_SAMPLER_ARG")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(1.0);
        Self::parse(&name, arg)
    }

    /// Parse an explicit sampler name plus its argument.
    pub fn parse(name: &str, arg: f64) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "always_off" => Self::AlwaysOff,
            "always_on" => Self::AlwaysOn,
            "traceidratio" => Self::TraceIdRatio(clamp_ratio(arg)),
            "parentbased_always_off" => Self::ParentBased(&ALWAYS_OFF),
            "parentbased_traceidratio" => {
                // Leaked-free equivalent: a ratio parent-based sampler is
                // represented by the ratio itself plus parent inheritance,
                // which `should_sample` already honours.
                Self::TraceIdRatio(clamp_ratio(arg))
            }
            // "parentbased_always_on" and anything unrecognised.
            _ => Self::default(),
        }
    }

    /// The effective ratio this sampler applies when there is no parent.
    pub fn ratio(&self) -> f64 {
        match self {
            Self::AlwaysOn => 1.0,
            Self::AlwaysOff => 0.0,
            Self::TraceIdRatio(r) => *r,
            Self::ParentBased(inner) => inner.ratio(),
        }
    }

    /// Decide whether a trace is sampled.
    ///
    /// * `parent_sampled` — the upstream decision, when a `traceparent` arrived.
    /// * `trace_id` — the 16-byte trace id, used for deterministic ratio
    ///   sampling so every service in the path reaches the *same* verdict.
    pub fn should_sample(&self, parent_sampled: Option<bool>, trace_id: &[u8; 16]) -> bool {
        if let Self::ParentBased(inner) = self {
            return match parent_sampled {
                Some(decision) => decision,
                None => inner.should_sample(None, trace_id),
            };
        }
        // Non-parent-based samplers still respect an explicit upstream "no",
        // otherwise a trace would be half-recorded.
        if parent_sampled == Some(false) {
            return false;
        }
        match self {
            Self::AlwaysOn => true,
            Self::AlwaysOff => false,
            Self::TraceIdRatio(ratio) => trace_id_ratio_hit(trace_id, *ratio),
            Self::ParentBased(_) => unreachable!("handled above"),
        }
    }

    /// Human-readable name matching the `OTEL_TRACES_SAMPLER` vocabulary.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AlwaysOn => "always_on",
            Self::AlwaysOff => "always_off",
            Self::TraceIdRatio(_) => "traceidratio",
            Self::ParentBased(inner) => match inner {
                Self::AlwaysOff => "parentbased_always_off",
                _ => "parentbased_always_on",
            },
        }
    }
}

fn clamp_ratio(r: f64) -> f64 {
    if r.is_nan() {
        return 1.0;
    }
    r.clamp(0.0, 1.0)
}

/// Deterministic ratio test: take the low 8 bytes of the trace id as a
/// big-endian u64 and compare against `ratio * u64::MAX`. Identical inputs give
/// identical verdicts in every service, which is what keeps a trace whole.
fn trace_id_ratio_hit(trace_id: &[u8; 16], ratio: f64) -> bool {
    if ratio >= 1.0 {
        return true;
    }
    if ratio <= 0.0 {
        return false;
    }
    let mut tail = [0u8; 8];
    tail.copy_from_slice(&trace_id[8..16]);
    let value = u64::from_be_bytes(tail);
    let threshold = (ratio * (u64::MAX as f64)) as u64;
    value < threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(byte: u8) -> [u8; 16] {
        let mut t = [0u8; 16];
        t[15] = byte;
        t
    }

    #[test]
    fn platform_default_is_always_on() {
        let s = SamplerSpec::default();
        assert_eq!(s.ratio(), 1.0);
        assert!(s.should_sample(None, &tid(1)));
    }

    #[test]
    fn parent_decision_wins_for_parentbased() {
        let s = SamplerSpec::default();
        assert!(!s.should_sample(Some(false), &tid(1)));
        assert!(s.should_sample(Some(true), &tid(1)));
    }

    #[test]
    fn always_off_drops_everything() {
        assert!(!SamplerSpec::AlwaysOff.should_sample(None, &tid(1)));
        assert!(!SamplerSpec::AlwaysOff.should_sample(Some(true), &tid(1)));
    }

    #[test]
    fn ratio_is_deterministic_for_a_trace_id() {
        let s = SamplerSpec::TraceIdRatio(0.5);
        let id = tid(7);
        assert_eq!(s.should_sample(None, &id), s.should_sample(None, &id));
    }

    #[test]
    fn ratio_zero_and_one_are_absolute() {
        assert!(SamplerSpec::TraceIdRatio(1.0).should_sample(None, &tid(9)));
        assert!(!SamplerSpec::TraceIdRatio(0.0).should_sample(None, &tid(9)));
    }

    #[test]
    fn ratio_clamps_out_of_range_input() {
        assert_eq!(SamplerSpec::parse("traceidratio", 4.2).ratio(), 1.0);
        assert_eq!(SamplerSpec::parse("traceidratio", -1.0).ratio(), 0.0);
    }

    #[test]
    fn unknown_sampler_falls_back_to_platform_default() {
        assert_eq!(SamplerSpec::parse("mystery", 1.0).as_str(), "parentbased_always_on");
    }
}
