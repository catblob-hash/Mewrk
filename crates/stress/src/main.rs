//! Load generator and test matrix for the agent runtime.
//!
//! `run` drives one workload from wherever it is started. `matrix` builds the impaired network, starts the
//! gateway and re-executes `run` inside the client namespace for every profile and scenario.

mod matrix;
mod report;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_rt::media::MediaInput;
use agent_rt::{run_agent, AgentConfig, Calc, Cancel, Event, FailKind, Message, RetryPolicy, Role, Runtime, RuntimeConfig, Sink, ToolRegistry, Transport, TransportConfig};
use parking_lot::Mutex;

use report::{Hist, RunReport};

const USAGE: &str = "\
stress run --url URL [--ca PEM] [--model NAME] [-c N] [-n N] [--scenario text|image|video|mixed|agent|count]
           [--max-tokens N] [--prompt-words N] [--thinking on|off] [--image PATH] [--video PATH]
           [--http2] [--h2-conns N] [--stall-secs N] [--max-attempts N] [--media-inflight N] [--no-hedge]
           [--preconnect N] [--no-slots-check] [--label L] [--json PATH]
stress matrix --out DIR --profiles a,b,c --scenarios x,y,z [--model NAME] [-c N] [--image PATH] [--video PATH]
              [--gateway-args 'extra args'] [--http2] [--stall-secs N] [--media-inflight N] [--repeat N]";

pub struct Args(pub Vec<String>);

impl Args {
    pub fn value(&self, name: &str) -> Option<&str> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).map(String::as_str)
    }
    pub fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
    pub fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.value(name) {
            Some(v) => v.parse().map_err(|_| format!("{name}: bad value {v}")),
            None => Ok(default),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scenario {
    Text,
    Image,
    Video,
    Mixed,
    Agent,
    Count,
}

impl Scenario {
    fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "text" => Scenario::Text,
            "image" => Scenario::Image,
            "video" => Scenario::Video,
            "mixed" => Scenario::Mixed,
            "agent" => Scenario::Agent,
            "count" => Scenario::Count,
            _ => return Err(format!("unknown scenario {s}")),
        })
    }
}

/// Per-job measurements. One task writes and nothing reads until it ends, so the lock is never contended.
#[derive(Default)]
struct JobState {
    started: Option<Instant>,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    gaps_us: Vec<u32>,
    tokens: u64,
    text: String,
    keep_text: bool,
    tcp_ms: Vec<f32>,
    tls_ms: Vec<f32>,
    reused: u32,
    tool_runs: u32,
    tool_errors: u32,
    tool_rewinds: u32,
    retry_delay: Duration,
    ttfb: Vec<Duration>,
    attempt_reused: bool,
}

struct JobSink(Mutex<JobState>);

impl Sink for JobSink {
    fn event(&self, _: u32, event: Event<'_>) {
        let mut s = self.0.lock();
        match event {
            Event::Attempt { .. } => {
                if s.started.is_none() {
                    s.started = Some(Instant::now());
                }
            }
            Event::Headers { ttfb } => s.ttfb.push(ttfb),
            Event::Connected(t) => {
                s.attempt_reused = t.reused;
                if t.reused {
                    s.reused += 1;
                } else {
                    s.tcp_ms.push(t.tcp_ms);
                    s.tls_ms.push(t.tls_ms);
                }
            }
            Event::Reasoning(_) | Event::ToolArgs { .. } => token(&mut s),
            Event::Text(t) => {
                token(&mut s);
                if s.keep_text {
                    s.text.push_str(t);
                }
            }
            Event::ToolResult { is_error, .. } => {
                s.tool_runs += 1;
                s.tool_errors += is_error as u32;
                // The pause around a tool call is a new model turn. It says nothing about stream pacing.
                s.last_token = None;
            }
            Event::ToolRewind => s.tool_rewinds += 1,
            Event::Retry { delay, .. } => s.retry_delay += delay,
            _ => {}
        }
    }
}

#[inline]
fn token(s: &mut JobState) {
    let now = Instant::now();
    match s.last_token {
        Some(last) => s.gaps_us.push(now.duration_since(last).as_micros().min(u32::MAX as u128) as u32),
        None => {
            s.first_token.get_or_insert(now);
        }
    }
    s.last_token = Some(now);
    s.tokens += 1;
}

const WORDS: &[&str] = &[
    "system", "agent", "tool", "call", "graph", "node", "cache", "memory", "thread", "worker", "queue", "stream", "token", "batch", "index", "vector", "kernel", "buffer",
];

fn prompt_words(rng: &mut fastrand::Rng, n: usize) -> String {
    let mut out = String::with_capacity(n * 7);
    for i in 0..n {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(WORDS[rng.usize(..WORDS.len())]);
    }
    out
}

struct Workload {
    scenario: Scenario,
    max_tokens: u32,
    prompt_words: usize,
    thinking: Option<bool>,
    image: Option<Arc<MediaInput>>,
    video: Option<Arc<MediaInput>>,
    count_to: u32,
    model: Arc<str>,
}

#[derive(Default)]
struct JobOutcome {
    ok: bool,
    fail: Option<FailKind>,
    fail_detail: String,
    attempts: u32,
    resumes: u32,
    tokens: u64,
    usage_completion: u64,
    fail_counts: Vec<(FailKind, u32)>,
    ttft: Option<Duration>,
    active: Option<Duration>,
    gaps_us: Vec<u32>,
    tcp_ms: Vec<f32>,
    tls_ms: Vec<f32>,
    reused: u32,
    tool_runs: u32,
    tool_errors: u32,
    tool_rewinds: u32,
    /// `None` when the scenario has nothing to check.
    correct: Option<bool>,
    deviation: Option<String>,
    /// The model declined or wandered off. The transport delivered that faithfully, so it is not a fault here.
    model_deviation: bool,
    splice_error: bool,
    extra_after_complete: bool,
    kind: &'static str,
    index: u64,
    ttfb: Vec<Duration>,
    last_reused: bool,
}

fn digits_only(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

#[derive(Debug, PartialEq, Eq)]
enum CountCheck {
    Exact,
    /// A number repeated or skipped. This is the one that would mean a resume damaged the text.
    SpliceError { expected: u32, got: u32 },
    /// All of 1..=n came through and then something else. A cut right at the end of an answer asks the model to continue
    /// something already complete, and it may oblige. The sequence itself is intact.
    ExtraAfterComplete(String),
    /// The model stopped counting, as in "1 2 3 ... 60".
    Abbreviated(String),
}

/// Verifies 1..=n appear in order, which is the check that can be run against a real model and not only the mock.
fn check_count(text: &str, n: u32) -> CountCheck {
    let mut expected = 1;
    for word in text.split_whitespace() {
        let Ok(got) = word.trim_matches(|c: char| !c.is_ascii_digit()).parse::<u32>() else {
            let message = format!("non-number {word:?} after {}", expected - 1);
            return if expected > n { CountCheck::ExtraAfterComplete(message) } else { CountCheck::Abbreviated(message) };
        };
        if got != expected {
            return if expected > n { CountCheck::ExtraAfterComplete(format!("{got} after {n}")) } else { CountCheck::SpliceError { expected, got } };
        }
        expected += 1;
    }
    if expected <= n {
        return CountCheck::Abbreviated(format!("stopped after {}", expected - 1));
    }
    CountCheck::Exact
}

async fn one_job(rt: Arc<Runtime>, w: Arc<Workload>, index: u64) -> JobOutcome {
    let mut rng = fastrand::Rng::with_seed(index.wrapping_mul(0x9E37_79B9).wrapping_add(17));
    let mut cfg = AgentConfig::new(w.model.clone());
    cfg.max_tokens = w.max_tokens;
    cfg.seed = Some(index);
    cfg.thinking = w.thinking;
    cfg.max_steps = 1;

    let scenario = match w.scenario {
        // In the mix, the same 70/20/10 split a busy multi-user deployment might see.
        Scenario::Mixed => match index % 10 {
            0 => Scenario::Video,
            1 | 2 => Scenario::Image,
            _ => Scenario::Text,
        },
        s => s,
    };

    let mut expected_answer = None;
    let (kind, conversation) = match scenario {
        Scenario::Text | Scenario::Mixed => {
            let prompt = format!("#{index} {}\nContinue this list with more words.", prompt_words(&mut rng, w.prompt_words));
            ("text", vec![Message::text(Role::User, prompt)])
        }
        Scenario::Image => {
            let media = w.image.clone().expect("--image is required");
            cfg.max_tokens = cfg.max_tokens.min(64);
            ("image", vec![Message::user_with_media(format!("#{index} Describe this image briefly."), vec![media])])
        }
        Scenario::Video => {
            let media = w.video.clone().expect("--video is required");
            cfg.max_tokens = cfg.max_tokens.min(64);
            ("video", vec![Message::user_with_media(format!("#{index} Describe this video briefly."), vec![media])])
        }
        Scenario::Agent => {
            let (a, b) = (rng.u32(1000..99_999), rng.u32(100..9_999));
            cfg.tools = ToolRegistry::new().with(Calc);
            cfg.max_steps = 4;
            cfg.max_tokens = 256;
            cfg.thinking = Some(false);
            expected_answer = Some((a as u64 * b as u64).to_string());
            (
                "agent",
                vec![
                    Message::text(Role::System, "You are a careful assistant. For any arithmetic you must call the calc tool, then report the number it returned."),
                    Message::text(Role::User, format!("What is {a} * {b}?")),
                ],
            )
        }
        Scenario::Count => {
            cfg.thinking = Some(false);
            cfg.max_tokens = w.count_to * 4;
            let prompt = format!("Count from 1 to {} separated by single spaces. Output only the numbers, nothing else.", w.count_to);
            ("count", vec![Message::text(Role::User, prompt)])
        }
    };

    let sink = JobSink(Mutex::new(JobState { keep_text: matches!(scenario, Scenario::Count | Scenario::Agent), ..Default::default() }));
    let mut conversation = conversation;
    let result = run_agent(&rt, &cfg, index as u32, &mut conversation, &sink, &Cancel::new()).await;
    let state = sink.0.into_inner();

    let mut out = JobOutcome {
        kind,
        tokens: state.tokens,
        gaps_us: state.gaps_us,
        tcp_ms: state.tcp_ms,
        tls_ms: state.tls_ms,
        reused: state.reused,
        tool_runs: state.tool_runs,
        tool_errors: state.tool_errors,
        tool_rewinds: state.tool_rewinds,
        index,
        ttfb: state.ttfb,
        last_reused: state.attempt_reused,
        ttft: state.started.zip(state.first_token).map(|(a, b)| b.duration_since(a)),
        active: state.first_token.zip(state.last_token).map(|(a, b)| b.duration_since(a)),
        ..Default::default()
    };
    let summary = match result {
        Ok(summary) => {
            out.ok = true;
            summary
        }
        Err((error, summary)) => {
            out.fail = Some(error.kind);
            out.fail_detail = error.detail;
            summary
        }
    };
    out.attempts = summary.attempts;
    out.resumes = summary.resumes;
    out.usage_completion = summary.usage_completion as u64;
    out.fail_counts = summary.fail_counts;

    if out.ok {
        match scenario {
            Scenario::Count => {
                let check = check_count(&state.text, w.count_to);
                out.correct = Some(check == CountCheck::Exact);
                match check {
                    CountCheck::Exact => {}
                    CountCheck::SpliceError { expected, got } => {
                        out.splice_error = true;
                        out.deviation = Some(format!("expected {expected}, got {got}, after {} resumes", out.resumes));
                    }
                    CountCheck::ExtraAfterComplete(m) => {
                        out.extra_after_complete = true;
                        out.deviation = Some(m);
                    }
                    CountCheck::Abbreviated(m) => {
                        out.model_deviation = true;
                        out.deviation = Some(m);
                    }
                }
            }
            Scenario::Agent => {
                let answer = expected_answer.expect("set above");
                let correct = digits_only(&state.text).contains(&answer);
                out.correct = Some(correct && out.tool_runs >= 1 && out.tool_errors == 0);
                if out.correct != Some(true) {
                    // Never having called the tool, or doing arithmetic with it and then misreporting, is the model's.
                    out.model_deviation = out.tool_errors == 0;
                    out.deviation = Some(format!("wanted {answer}, tools {}/{} err, said {:?}", out.tool_runs, out.tool_errors, state.text.chars().take(80).collect::<String>()));
                }
            }
            _ => {}
        }
    }
    out
}

fn read_ca(path: Option<&str>) -> Result<Option<Vec<u8>>, String> {
    path.map(|p| std::fs::read(p).map_err(|e| format!("{p}: {e}"))).transpose()
}

fn cpu_seconds() -> f64 {
    // SAFETY: rusage is plain old data, and getrusage fully initialises it on success.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0.0;
        }
        let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
        t(usage.ru_utime) + t(usage.ru_stime)
    }
}

fn peak_rss_mib() -> f64 {
    // SAFETY: as in cpu_seconds.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0.0;
        }
        // Kilobytes on Linux, bytes on macOS.
        if cfg!(target_os = "macos") { usage.ru_maxrss as f64 / 1048576.0 } else { usage.ru_maxrss as f64 / 1024.0 }
    }
}

/// Checks the server gave every slot back. A slot held by a request nobody is reading anymore is capacity
/// quietly lost, and it is the failure mode that makes retries under loss spiral.
pub async fn drain_slots(control: &Arc<Transport>) -> (Option<u32>, f64) {
    let started = Instant::now();
    let mut last = None;
    while started.elapsed() < Duration::from_secs(45) {
        if let Ok(v) = control.get_json("/slots").await {
            let busy = v.as_array().map_or(0, |a| a.iter().filter(|s| s["is_processing"].as_bool() == Some(true)).count()) as u32;
            last = Some(busy);
            if busy == 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    (last, started.elapsed().as_secs_f64())
}

fn run(args: &Args) -> Result<RunReport, String> {
    let url = args.value("--url").ok_or(USAGE)?.to_owned();
    let scenario = Scenario::parse(args.value("--scenario").unwrap_or("text"))?;
    let concurrency: usize = args.num("-c", 128)?;
    let jobs: u64 = args.num("-n", concurrency as u64)?;
    let ca_pem = read_ca(args.value("--ca"))?;
    let thinking = match args.value("--thinking") {
        Some("on") => Some(true),
        Some("off") => Some(false),
        None => None,
        Some(other) => return Err(format!("--thinking: {other}")),
    };
    let load_media = |flag: &str, needed: bool| -> Result<Option<Arc<MediaInput>>, String> {
        match args.value(flag) {
            Some(p) => MediaInput::from_path_blocking(Path::new(p), agent_rt::media::DEFAULT_MAX_IMAGE_EDGE).map(|m| Some(Arc::new(m))).map_err(|e| e.0),
            None if needed => Err(format!("{flag} is required by this scenario")),
            None => Ok(None),
        }
    };
    let image = load_media("--image", matches!(scenario, Scenario::Image | Scenario::Mixed))?;
    let video = load_media("--video", matches!(scenario, Scenario::Video | Scenario::Mixed))?;
    let workload = Arc::new(Workload {
        scenario,
        max_tokens: args.num("--max-tokens", 96)?,
        prompt_words: args.num("--prompt-words", 64)?,
        thinking,
        image,
        video,
        count_to: args.num("--count-to", 100)?,
        model: args.value("--model").unwrap_or("LFM3-VL-2B").into(),
    });
    let http2 = args.flag("--http2");
    let transport_cfg = TransportConfig {
        base_url: url.clone(),
        ca_pem: ca_pem.clone(),
        http2,
        h2_conns: args.num("--h2-conns", 4)?,
        hedge_after: if args.flag("--no-hedge") { None } else { Some(Duration::from_millis(args.num("--hedge-ms", 1200)?)) },
        ..Default::default()
    };
    let control_url = url.trim_end_matches('/').trim_end_matches("/v1").to_owned();
    let runtime_config = RuntimeConfig {
        max_concurrency: concurrency,
        max_media_inflight: args.num("--media-inflight", concurrency)?,
        stall_timeout: Duration::from_secs(args.num("--stall-secs", 8)?),
        retry: RetryPolicy { max_attempts: args.num("--max-attempts", 16)?, ..Default::default() },
    };

    let tokio_rt = tokio::runtime::Builder::new_multi_thread().worker_threads(args.num("--threads", 4)?).enable_all().build().map_err(|e| e.to_string())?;
    let label = args.value("--label").unwrap_or("run").to_owned();
    let preconnect: usize = args.num("--preconnect", 0)?;
    let check_slots = !args.flag("--no-slots-check");

    tokio_rt.block_on(async move {
        let transport = Transport::new(transport_cfg)?;
        let control = Transport::new(TransportConfig { base_url: control_url, ca_pem, hedge_after: Some(Duration::from_millis(1200)), ..Default::default() })?;
        if preconnect > 0 {
            let ready = transport.preconnect(preconnect).await;
            eprintln!("preconnected {ready}/{preconnect}");
        }
        let rt = Runtime::new(transport.clone(), runtime_config.clone());
        let cpu_before = cpu_seconds();
        let wall = Instant::now();
        let mut set = tokio::task::JoinSet::new();
        for index in 0..jobs {
            set.spawn(one_job(rt.clone(), workload.clone(), index + 1));
        }

        let mut report = RunReport { label, url, scenario: format!("{scenario:?}").to_lowercase(), concurrency, jobs, http2, ..Default::default() };
        let (mut ttft, mut gaps, mut tcp, mut tls) = (Hist::new(), Hist::new(), Hist::new(), Hist::new());
        let mut per_stream = Hist::new();
        let mut ttft_by_kind: BTreeMap<&'static str, Hist> = BTreeMap::new();
        let mut gaps_by_kind: BTreeMap<&'static str, Hist> = BTreeMap::new();
        let mut progress = 0u64;
        let mut slowest: Vec<(Duration, String)> = Vec::new();
        while let Some(done) = set.join_next().await {
            let job = done.map_err(|e| format!("job panicked: {e}"))?;
            progress += 1;
            if progress.is_multiple_of(64) {
                eprintln!("[{}] {progress}/{jobs} finished, {:.1}s", report.label, wall.elapsed().as_secs_f64());
            }
            report.ok += job.ok as u64;
            report.failed += !job.ok as u64;
            report.attempts += job.attempts as u64;
            report.resumes += job.resumes as u64;
            report.tokens += job.tokens;
            report.usage_completion += job.usage_completion;
            report.reused += job.reused as u64;
            report.tool_runs += job.tool_runs as u64;
            report.tool_errors += job.tool_errors as u64;
            report.tool_rewinds += job.tool_rewinds as u64;
            if let Some(kind) = job.fail {
                *report.final_failures.entry(format!("{}:{}", kind.label(), status_of(kind))).or_default() += 1;
                if report.failure_samples.len() < 5 {
                    report.failure_samples.push(format!("{}: {}", kind.label(), job.fail_detail.chars().take(200).collect::<String>()));
                }
            }
            for (kind, n) in &job.fail_counts {
                *report.retry_causes.entry(kind.label().to_owned()).or_default() += *n as u64;
            }
            if let Some(correct) = job.correct {
                report.checked += 1;
                report.correct += correct as u64;
                report.model_deviations += job.model_deviation as u64;
                report.splice_errors += job.splice_error as u64;
                report.extra_after_complete += job.extra_after_complete as u64;
                if let (false, Some(d)) = (correct, job.deviation.clone())
                    && report.deviations.len() < 8 {
                        report.deviations.push(d);
                    }
            }
            // The slowest few jobs, with enough detail to tell which phase ate the time.
            if let Some(t) = job.ttft {
                slowest.push((t, format!(
                    "#{} {} ttft {:.1}s attempts {} ttfb {:?} tcp {:?} tls {:?} reused_last {} causes {:?}",
                    job.index,
                    job.kind,
                    t.as_secs_f64(),
                    job.attempts,
                    job.ttfb.iter().map(|d| (d.as_secs_f64() * 10.0).round() / 10.0).collect::<Vec<_>>(),
                    job.tcp_ms.iter().map(|v| v.round()).collect::<Vec<_>>(),
                    job.tls_ms.iter().map(|v| v.round()).collect::<Vec<_>>(),
                    job.last_reused,
                    job.fail_counts,
                )));
                slowest.sort_by_key(|entry| std::cmp::Reverse(entry.0));
                slowest.truncate(4);
            }
            if let Some(t) = job.ttft {
                ttft.record(t.as_micros() as u64);
                ttft_by_kind.entry(job.kind).or_insert_with(Hist::new).record(t.as_micros() as u64);
            }
            if let (Some(active), true) = (job.active, job.tokens > 1) {
                let rate = (job.tokens - 1) as f64 / active.as_secs_f64().max(1e-6);
                per_stream.record((rate * 1000.0) as u64);
            }
            let by_kind = gaps_by_kind.entry(job.kind).or_insert_with(Hist::new);
            for &g in &job.gaps_us {
                gaps.record(g as u64);
                by_kind.record(g as u64);
                report.gaps_over_1s += (g > 1_000_000) as u64;
                report.gaps_over_5s += (g > 5_000_000) as u64;
            }
            for &ms in &job.tcp_ms {
                tcp.record((ms * 1000.0) as u64);
            }
            for &ms in &job.tls_ms {
                tls.record((ms * 1000.0) as u64);
            }
        }
        report.slowest = slowest.into_iter().map(|(_, s)| s).collect();
        report.wall_secs = wall.elapsed().as_secs_f64();
        report.client_cpu_secs = cpu_seconds() - cpu_before;
        report.client_peak_rss_mib = peak_rss_mib();
        report.tokens_per_sec = report.tokens as f64 / report.wall_secs;
        report.cpu_us_per_token = report.client_cpu_secs * 1e6 / report.tokens.max(1) as f64;
        report.ttft_ms = ttft.summary_ms();
        report.gap_ms = gaps.summary_ms();
        report.tcp_ms = tcp.summary_ms();
        report.tls_ms = tls.summary_ms();
        report.stream_tok_s = per_stream.summary_scaled(1000.0);
        report.ttft_by_kind = ttft_by_kind.iter().map(|(k, h)| (k.to_string(), h.summary_ms())).collect();
        report.gap_by_kind = gaps_by_kind.iter().map(|(k, h)| (k.to_string(), h.summary_ms())).collect();
        let s = &transport.stats;
        let g = |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
        report.dials = g(&s.dials);
        report.hedges = g(&s.hedges);
        report.hedge_wins = g(&s.hedge_wins);
        report.tls_full = g(&s.tls_full);
        report.tls_resumed = g(&s.tls_resumed);
        report.stale_retries = g(&s.stale_retries);
        report.bytes_up = g(&s.bytes_up);
        report.bytes_down = g(&s.bytes_down);
        if check_slots {
            let (busy, drain) = drain_slots(&control).await;
            report.slots_busy_after = busy;
            report.slots_drain_secs = drain;
        }
        Ok(report)
    })
}

fn status_of(kind: FailKind) -> u16 {
    match kind {
        FailKind::Http(c) | FailKind::Server(c) => c,
        _ => 0,
    }
}

fn main() {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let command = raw.remove(0);
    let args = Args(raw);
    let result = match command.as_str() {
        "run" => run(&args).and_then(|report| {
            println!("{}", report.one_line());
            if let Some(path) = args.value("--json") {
                std::fs::write(path, serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?).map_err(|e| format!("{path}: {e}"))?;
            }
            Ok(())
        }),
        "matrix" => matrix::run(&args),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_check_finds_every_kind_of_damage() {
        assert_eq!(check_count("1 2 3 4", 4), CountCheck::Exact);
        assert_eq!(check_count("1 2 3 3 4", 4), CountCheck::SpliceError { expected: 4, got: 3 }, "duplicate at a splice");
        assert_eq!(check_count("1 2 4", 4), CountCheck::SpliceError { expected: 3, got: 4 }, "text lost at a splice");
        assert!(matches!(check_count("1 2", 4), CountCheck::Abbreviated(_)));
        assert!(matches!(check_count("1 2 3 ... 60", 60), CountCheck::Abbreviated(_)));
        assert!(matches!(check_count("1 2 I'm", 2), CountCheck::ExtraAfterComplete(_)), "the sequence is whole, then more");
    }
}
