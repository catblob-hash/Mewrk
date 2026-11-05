//! Runs every scenario under every network profile, recording what the kernel saw alongside what the
//! client measured. A loss figure nobody verified is not a test condition, only a hope.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chaos::netlab;

use crate::report::{markdown, NetDelta, RunReport};
use crate::Args;

const GATEWAY_PORT: u16 = 8443;

struct Gateway(Child);

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = Command::new("sudo").args(["-n", "kill", "-TERM", &self.0.id().to_string()]).status();
        let _ = self.0.wait();
    }
}

/// Removes the lab even when a run fails half way through.
struct LabGuard;

impl Drop for LabGuard {
    fn drop(&mut self) {
        if let Err(e) = netlab::down() {
            eprintln!("netlab down: {e}");
        }
    }
}

fn sibling(name: &str) -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(me.with_file_name(name))
}

fn start_gateway(out: &Path, extra: &str, no_thin: bool) -> Result<Gateway, String> {
    let certs = out.join("certs");
    let mut args: Vec<String> = vec![
        "-n".into(),
        sibling("chaosgw")?.display().to_string(),
        "serve".into(),
        "--netns".into(),
        netlab::SERVER_NS.into(),
        "--listen".into(),
        format!("{}:{GATEWAY_PORT}", netlab::SERVER_IP),
        "--upstream".into(),
        "127.0.0.1:8080".into(),
        "--cert-dir".into(),
        certs.display().to_string(),
        "--hosts".into(),
        format!("{},lfm3.test", netlab::SERVER_IP),
        "--drop-to-sudo-user".into(),
    ];
    // Linear RTOs for thin streams, on unless a run is comparing against them.
    if !no_thin {
        args.push("--thin".into());
    }
    args.extend(extra.split_whitespace().map(str::to_owned));
    let log = std::fs::File::create(out.join("gateway.log")).map_err(|e| e.to_string())?;
    let child = Command::new("sudo").args(&args).stdin(Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log).spawn().map_err(|e| format!("gateway: {e}"))?;
    let gateway = Gateway(child);
    // Wait until the certificate exists and the socket answers, from inside the server namespace.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if certs.join("ca.pem").exists() {
            std::thread::sleep(Duration::from_millis(200));
            return Ok(gateway);
        }
    }
    Err("gateway did not come up, see gateway.log".into())
}

/// Steps a time-varying profile through its schedule until told to stop.
fn start_flap(profile: &'static netlab::Profile) -> Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
    if profile.schedule.is_empty() {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || {
        'outer: loop {
            for (secs, link) in profile.schedule {
                if let Err(e) = netlab::apply(link) {
                    eprintln!("flap: {e}");
                }
                let mut left = *secs;
                while left > 0.0 {
                    if stop2.load(Ordering::SeqCst) {
                        break 'outer;
                    }
                    std::thread::sleep(Duration::from_secs_f32(left.min(0.1)));
                    left -= 0.1;
                }
            }
        }
    });
    Some((stop, handle))
}

#[derive(Clone, Copy)]
struct Counters {
    q_c: netlab::QdiscCounters,
    q_s: netlab::QdiscCounters,
    t_c: netlab::TcpCounters,
    t_s: netlab::TcpCounters,
}

fn counters() -> Result<Counters, String> {
    Ok(Counters { q_c: netlab::qdisc_counters(true)?, q_s: netlab::qdisc_counters(false)?, t_c: netlab::tcp_counters(true)?, t_s: netlab::tcp_counters(false)? })
}

fn delta(profile: &str, before: Counters, after: Counters) -> NetDelta {
    let client = after.t_c.delta_since(&before.t_c);
    let server = after.t_s.delta_since(&before.t_s);
    // `tc replace` during a flap resets the counters, so a smaller value after simply means "since the reset".
    let since = |a: u64, b: u64| if a >= b { a - b } else { a };
    NetDelta {
        profile: profile.to_owned(),
        netem_sent_c2s: since(after.q_c.sent_packets, before.q_c.sent_packets),
        netem_dropped_c2s: since(after.q_c.dropped, before.q_c.dropped),
        netem_sent_s2c: since(after.q_s.sent_packets, before.q_s.sent_packets),
        netem_dropped_s2c: since(after.q_s.dropped, before.q_s.dropped),
        client_out_segs: client.out_segs,
        client_retrans: client.retrans_segs,
        client_rto: client.rto_timeouts,
        server_out_segs: server.out_segs,
        server_retrans: server.retrans_segs,
        server_rto: server.rto_timeouts,
        server_tlp: server.tlp_probes,
        expected_loss_pct: None,
    }
}

/// Asks llama-server directly whether every slot came back. This has to stay off the impaired link: the
/// `/slots` document is large, and fetching it at 11% burst loss once took 172 s, measuring only the fetch.
fn drain_slots_directly() -> Result<(Option<u32>, f64), String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let control = agent_rt::Transport::new(agent_rt::TransportConfig { base_url: "http://127.0.0.1:8080".into(), ..Default::default() })?;
        Ok(crate::drain_slots(&control).await)
    })
}

fn scenario_args(scenario: &str, args: &Args, concurrency: usize) -> Result<Vec<String>, String> {
    let c = concurrency.to_string();
    let image = args.value("--image");
    let video = args.value("--video");
    let mut out: Vec<String> = vec!["-c".into(), c.clone()];
    let want = |path: Option<&str>, flag: &str| -> Result<Vec<String>, String> {
        let p = path.ok_or_else(|| format!("{flag} is required by scenario {scenario}"))?;
        Ok(vec![flag.into(), p.into()])
    };
    match scenario {
        // Two rounds, so the second runs on connections and caches the first left behind.
        "text" => out.extend(["-n".into(), (concurrency * 2).to_string(), "--scenario".into(), "text".into(), "--max-tokens".into(), "96".into()]),
        "image" => {
            out.extend(["-n".into(), c, "--scenario".into(), "image".into(), "--max-tokens".into(), "48".into()]);
            out.extend(want(image, "--image")?);
        }
        "video" => {
            out.extend(["-n".into(), c, "--scenario".into(), "video".into(), "--max-tokens".into(), "48".into()]);
            out.extend(want(video, "--video")?);
        }
        "mixed" => {
            out.extend(["-n".into(), (concurrency * 2).to_string(), "--scenario".into(), "mixed".into(), "--max-tokens".into(), "64".into()]);
            out.extend(want(image, "--image")?);
            out.extend(want(video, "--video")?);
        }
        // A large clip, which is about upload and encoding more than about tokens, so far fewer at a time.
        "bigvideo" => {
            out = vec!["-c".into(), "8".into(), "-n".into(), "8".into(), "--scenario".into(), "video".into(), "--max-tokens".into(), "48".into()];
            out.extend(want(args.value("--big-video"), "--video").map(|mut v| {
                v[0] = "--video".into();
                v
            })?);
        }
        "agent" => out.extend(["-n".into(), c, "--scenario".into(), "agent".into()]),
        "count" => out.extend(["-n".into(), c, "--scenario".into(), "count".into(), "--count-to".into(), "60".into()]),
        other => return Err(format!("unknown scenario {other}")),
    }
    Ok(out)
}

pub fn run(args: &Args) -> Result<(), String> {
    let out = PathBuf::from(args.value("--out").ok_or("--out is required")?);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let out = std::fs::canonicalize(&out).map_err(|e| e.to_string())?;
    let profiles: Vec<&'static netlab::Profile> = args
        .value("--profiles")
        .unwrap_or("clean,wan,jitter,loss5,burst,loss10,loss20,flap")
        .split(',')
        .map(|n| netlab::profile(n).ok_or_else(|| format!("unknown profile {n}")))
        .collect::<Result<_, _>>()?;
    let scenarios: Vec<String> = args.value("--scenarios").unwrap_or("text,image,video,mixed,agent,count").split(',').map(str::to_owned).collect();
    let concurrency: usize = args.num("-c", 128)?;
    let repeat: u32 = args.num("--repeat", 1)?;
    let user = std::env::var("USER").map_err(|_| "USER is not set")?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;

    netlab::up()?;
    let _lab = LabGuard;
    netlab::apply(&netlab::Link::CLEAN)?;
    let _gateway = start_gateway(&out, args.value("--gateway-args").unwrap_or(""), args.flag("--no-thin"))?;
    let ca = out.join("certs/ca.pem");
    let url = format!("https://{}:{GATEWAY_PORT}/v1", netlab::SERVER_IP);

    let mut reports: Vec<RunReport> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for profile in &profiles {
        eprintln!("=== profile {}: {}", profile.name, profile.summary);
        netlab::apply_profile(profile)?;
        let flap = start_flap(profile);
        for scenario in &scenarios {
            for round in 0..repeat {
                let label = format!("{}-{scenario}-{round}", profile.name);
                let json = out.join(format!("{label}.json"));
                let before = counters()?;
                let mut cmd = Command::new("sudo");
                cmd.args(["-n", "ip", "netns", "exec", netlab::CLIENT_NS, "sudo", "-n", "-u", &user]);
                cmd.arg(&me).arg("run").args(["--url", &url]).arg("--ca").arg(&ca);
                cmd.args(["--label", &label, "--no-slots-check"]).arg("--json").arg(&json);
                cmd.args(scenario_args(scenario, args, concurrency)?);
                for pass in ["--model", "--stall-secs", "--media-inflight", "--max-attempts", "--h2-conns", "--hedge-ms"] {
                    if let Some(v) = args.value(pass) {
                        cmd.args([pass, v]);
                    }
                }
                for pass in ["--http2", "--no-hedge"] {
                    if args.flag(pass) {
                        cmd.arg(pass);
                    }
                }
                let status = cmd.status().map_err(|e| format!("run: {e}"))?;
                let after = counters()?;
                if !status.success() {
                    eprintln!("{label}: exited with {status}");
                    continue;
                }
                let text = std::fs::read_to_string(&json).map_err(|e| format!("{}: {e}", json.display()))?;
                let mut report: RunReport = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                let (busy, drain_secs) = drain_slots_directly()?;
                report.slots_busy_after = busy;
                report.slots_drain_secs = drain_secs;
                let mut net = delta(profile.name, before, after);
                net.expected_loss_pct = profile.schedule.is_empty().then(|| profile.link.loss.expected_pct());
                if let Some(warning) = net.loss_warning() {
                    eprintln!("WARNING {warning}");
                    warnings.push(warning);
                }
                report.net = Some(net);
                std::fs::write(&json, serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                reports.push(report);
                std::fs::write(out.join("matrix.md"), markdown(&reports)).map_err(|e| e.to_string())?;
                std::fs::write(out.join("matrix.json"), serde_json::to_vec_pretty(&reports).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            }
        }
        if let Some((stop, handle)) = flap {
            stop.store(true, Ordering::SeqCst);
            let _ = handle.join();
        }
    }
    println!("{}", markdown(&reports));
    if !warnings.is_empty() {
        return Err(format!("the network did not match its profile:\n{}", warnings.join("\n")));
    }
    Ok(())
}
