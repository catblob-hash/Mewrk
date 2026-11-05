//! Measurements of one run, and how they are rendered.

use std::collections::BTreeMap;

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

pub struct Hist(Histogram<u64>);

impl Hist {
    pub fn new() -> Self {
        // Up to an hour in microseconds with 3 significant digits, which keeps percentiles honest at both ends.
        Self(Histogram::new_with_bounds(1, 3_600_000_000, 3).expect("valid histogram bounds"))
    }

    pub fn record(&mut self, v: u64) {
        self.0.saturating_record(v.max(1));
    }

    fn summary_with(&self, div: f64) -> Summary {
        if self.0.is_empty() {
            return Summary::default();
        }
        let q = |p: f64| self.0.value_at_quantile(p) as f64 / div;
        Summary {
            n: self.0.len(),
            mean: self.0.mean() / div,
            p50: q(0.50),
            p90: q(0.90),
            p95: q(0.95),
            p99: q(0.99),
            p999: q(0.999),
            max: self.0.max() as f64 / div,
        }
    }

    pub fn summary_ms(&self) -> Summary {
        self.summary_with(1000.0)
    }

    pub fn summary_scaled(&self, div: f64) -> Summary {
        self.summary_with(div)
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug)]
pub struct Summary {
    pub n: u64,
    pub mean: f64,
    pub p50: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct RunReport {
    pub label: String,
    pub url: String,
    pub scenario: String,
    pub concurrency: usize,
    pub jobs: u64,
    pub http2: bool,
    pub ok: u64,
    pub failed: u64,
    pub attempts: u64,
    pub resumes: u64,
    pub tokens: u64,
    pub usage_completion: u64,
    pub wall_secs: f64,
    pub tokens_per_sec: f64,
    pub ttft_ms: Summary,
    pub gap_ms: Summary,
    pub tcp_ms: Summary,
    pub tls_ms: Summary,
    pub stream_tok_s: Summary,
    pub ttft_by_kind: BTreeMap<String, Summary>,
    pub gap_by_kind: BTreeMap<String, Summary>,
    pub gaps_over_1s: u64,
    pub gaps_over_5s: u64,
    pub retry_causes: BTreeMap<String, u64>,
    pub final_failures: BTreeMap<String, u64>,
    pub failure_samples: Vec<String>,
    pub checked: u64,
    pub correct: u64,
    pub deviations: Vec<String>,
    #[serde(default)]
    pub model_deviations: u64,
    #[serde(default)]
    pub splice_errors: u64,
    #[serde(default)]
    pub extra_after_complete: u64,
    #[serde(default)]
    pub slowest: Vec<String>,
    pub tool_runs: u64,
    pub tool_errors: u64,
    pub tool_rewinds: u64,
    pub dials: u64,
    pub hedges: u64,
    pub hedge_wins: u64,
    pub tls_full: u64,
    pub tls_resumed: u64,
    pub reused: u64,
    pub stale_retries: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub client_cpu_secs: f64,
    pub client_peak_rss_mib: f64,
    pub cpu_us_per_token: f64,
    pub slots_busy_after: Option<u32>,
    pub slots_drain_secs: f64,
    #[serde(default)]
    pub net: Option<NetDelta>,
}

/// What the network actually did during a run, read from the kernel on both sides of the link.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct NetDelta {
    pub profile: String,
    pub netem_sent_c2s: u64,
    pub netem_dropped_c2s: u64,
    pub netem_sent_s2c: u64,
    pub netem_dropped_s2c: u64,
    pub client_out_segs: u64,
    pub client_retrans: u64,
    pub client_rto: u64,
    pub server_out_segs: u64,
    pub server_retrans: u64,
    pub server_rto: u64,
    pub server_tlp: u64,
    /// Loss the profile promises, or `None` for a schedule that changes over time.
    #[serde(default)]
    pub expected_loss_pct: Option<f32>,
}

impl NetDelta {
    pub fn measured_loss_pct(&self) -> f64 {
        let (sent, dropped) = (self.netem_sent_c2s + self.netem_sent_s2c, self.netem_dropped_c2s + self.netem_dropped_s2c);
        percent(dropped, sent + dropped)
    }

    /// A run under a loss rate other than the one on its label would make the whole result a lie.
    pub fn loss_warning(&self) -> Option<String> {
        let expected = self.expected_loss_pct? as f64;
        let measured = self.measured_loss_pct();
        let packets = self.netem_sent_c2s + self.netem_sent_s2c;
        // Below this sample size, sampling noise alone explains a large relative error.
        if packets < 2000 {
            return None;
        }
        let tolerance = (expected * 0.2).max(0.3);
        ((measured - expected).abs() > tolerance).then(|| format!("{}: configured {expected:.1}% loss but measured {measured:.2}%", self.profile))
    }
}

impl RunReport {
    pub fn one_line(&self) -> String {
        format!(
            "{} {} c={} ok={}/{} attempts={} resumes={} | {:.0} tok/s ({:.1}/stream) | TTFT p50 {:.0} p99 {:.0} max {:.0} ms | gap p50 {:.1} p99 {:.0} p99.9 {:.0} max {:.0} ms, >1s {} | correct {}/{} | dials {} hedge {}/{} reused {} stale {} | cpu {:.2}s {:.1}us/tok rss {:.0}MiB | slots busy {:?} {:.1}s | {:.1}s",
            self.label,
            self.scenario,
            self.concurrency,
            self.ok,
            self.jobs,
            self.attempts,
            self.resumes,
            self.tokens_per_sec,
            self.stream_tok_s.p50,
            self.ttft_ms.p50,
            self.ttft_ms.p99,
            self.ttft_ms.max,
            self.gap_ms.p50,
            self.gap_ms.p99,
            self.gap_ms.p999,
            self.gap_ms.max,
            self.gaps_over_1s,
            self.correct,
            self.checked,
            self.dials,
            self.hedge_wins,
            self.hedges,
            self.reused,
            self.stale_retries,
            self.client_cpu_secs,
            self.cpu_us_per_token,
            self.client_peak_rss_mib,
            self.slots_busy_after,
            self.slots_drain_secs,
            self.wall_secs,
        )
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 }
}

/// A Markdown table over a whole matrix.
pub fn markdown(reports: &[RunReport]) -> String {
    let mut out = String::new();
    out.push_str("| profile | scenario | ok | attempts | resumes | tok/s | stream tok/s | TTFT p50 / p99 ms | gap p99 / p99.9 / max ms | gaps >1 s | correct | dials | netem loss c→s / s→c | TCP retrans s→c | slots after |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for r in reports {
        let net = r.net.clone().unwrap_or_default();
        let total_c2s = net.netem_sent_c2s + net.netem_dropped_c2s;
        let total_s2c = net.netem_sent_s2c + net.netem_dropped_s2c;
        out.push_str(&format!(
            "| {} | {} | {}/{} | {} | {} | {:.0} | {:.1} | {:.0} / {:.0} | {:.0} / {:.0} / {:.0} | {} | {} | {} | {:.1}% / {:.1}% | {} ({:.1}%) | {} |\n",
            net.profile,
            r.scenario,
            r.ok,
            r.jobs,
            r.attempts,
            r.resumes,
            r.tokens_per_sec,
            r.stream_tok_s.p50,
            r.ttft_ms.p50,
            r.ttft_ms.p99,
            r.gap_ms.p99,
            r.gap_ms.p999,
            r.gap_ms.max,
            r.gaps_over_1s,
            if r.checked > 0 { format!("{}/{}", r.correct, r.checked) } else { "-".into() },
            r.dials,
            percent(net.netem_dropped_c2s, total_c2s),
            percent(net.netem_dropped_s2c, total_s2c),
            net.server_retrans,
            percent(net.server_retrans, net.server_out_segs),
            r.slots_busy_after.map_or("?".into(), |b| b.to_string()),
        ));
    }
    out
}
