//! A real impaired network: two namespaces joined by a veth pair with netem on both ends.
//!
//! This is packet loss the kernel's TCP actually experiences, with retransmissions, RTO backoff and
//! congestion collapse included. Nothing outside the two namespaces is touched: no firewall rule, no
//! real interface, no routing change, and deleting the namespaces removes all of it.

use std::process::Command;

pub const CLIENT_NS: &str = "lfmc";
pub const SERVER_NS: &str = "lfms";
pub const CLIENT_DEV: &str = "vc0";
pub const SERVER_DEV: &str = "vs0";
pub const CLIENT_IP: &str = "10.77.0.2";
pub const SERVER_IP: &str = "10.77.0.1";
/// A typical path MTU once a VPN or tunnel is involved.
const MTU: &str = "1400";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Loss {
    None,
    /// Independent random loss, in percent. netem's correlation option is deliberately not offered: it
    /// smooths the random sequence, which squeezes its distribution towards the middle, and the loss that
    /// comes out is far below the number configured (5% with 25% correlation measured 0.2%).
    Random { pct: f32 },
    /// Gilbert-Elliott: `p` is good to bad, `r` is bad to good. Mean loss p/(p+r), bursts of 1/r packets.
    Burst { p: f32, r: f32 },
}

/// One direction of the link.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Link {
    pub delay_ms: f32,
    pub jitter_ms: f32,
    /// Use the heavy-tailed distribution, where the occasional packet is delayed far beyond the jitter.
    pub pareto: bool,
    pub loss: Loss,
    pub duplicate_pct: f32,
    pub corrupt_pct: f32,
    pub rate_mbit: Option<u32>,
}

impl Loss {
    /// The long-run loss this setting should produce, in percent.
    pub fn expected_pct(&self) -> f32 {
        match *self {
            Loss::None => 0.0,
            Loss::Random { pct } => pct,
            Loss::Burst { p, r } => p / (p + r) * 100.0,
        }
    }
}

impl Link {
    pub const CLEAN: Link = Link { delay_ms: 0.0, jitter_ms: 0.0, pareto: false, loss: Loss::None, duplicate_pct: 0.0, corrupt_pct: 0.0, rate_mbit: None };

    pub fn netem_args(&self) -> Vec<String> {
        // The queue has to hold everything in flight during the delay, or netem itself becomes the loss.
        let mut a: Vec<String> = vec!["netem".into(), "limit".into(), "200000".into()];
        if self.delay_ms > 0.0 {
            a.push("delay".into());
            a.push(format!("{}ms", self.delay_ms));
            if self.jitter_ms > 0.0 {
                a.push(format!("{}ms", self.jitter_ms));
                if self.pareto {
                    a.push("distribution".into());
                    a.push("pareto".into());
                }
            }
        }
        match self.loss {
            Loss::None => {}
            Loss::Random { pct } => {
                a.push("loss".into());
                a.push(format!("{pct}%"));
            }
            Loss::Burst { p, r } => {
                a.extend(["loss".into(), "gemodel".into(), format!("{p}%"), format!("{r}%")]);
            }
        }
        if self.duplicate_pct > 0.0 {
            a.push("duplicate".into());
            a.push(format!("{}%", self.duplicate_pct));
        }
        if self.corrupt_pct > 0.0 {
            a.push("corrupt".into());
            a.push(format!("{}%", self.corrupt_pct));
        }
        if let Some(rate) = self.rate_mbit {
            a.push("rate".into());
            a.push(format!("{rate}mbit"));
        }
        a
    }
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: &'static str,
    pub summary: &'static str,
    pub link: Link,
    /// Time-varying profiles step through these (duration in seconds, link) in a loop.
    pub schedule: &'static [(f32, Link)],
}

const fn jittery(delay_ms: f32, jitter_ms: f32, loss: Loss) -> Link {
    Link { delay_ms, jitter_ms, pareto: true, loss, duplicate_pct: 0.0, corrupt_pct: 0.0, rate_mbit: None }
}

const FLAP: &[(f32, Link)] = &[
    (8.0, jittery(20.0, 5.0, Loss::Random { pct: 0.5 })),
    (6.0, jittery(150.0, 120.0, Loss::Random { pct: 5.0 })),
    (2.0, Link { loss: Loss::Random { pct: 100.0 }, ..Link::CLEAN }),
    (8.0, jittery(60.0, 40.0, Loss::Random { pct: 2.0 })),
    (4.0, jittery(300.0, 250.0, Loss::Burst { p: 3.0, r: 20.0 })),
];

pub const PROFILES: &[Profile] = &[
    Profile { name: "clean", summary: "no impairment: the cost of TLS and the gateway alone", link: Link::CLEAN, schedule: &[] },
    Profile {
        name: "wan",
        summary: "RTT 40 ms, jitter 5 ms, 0.1% loss: a decent connection",
        link: jittery(20.0, 5.0, Loss::Random { pct: 0.1 }),
        schedule: &[],
    },
    Profile {
        name: "jitter",
        summary: "RTT 100 ms, heavy-tailed jitter 90 ms, 0.5% loss: unstable latency",
        link: jittery(50.0, 90.0, Loss::Random { pct: 0.5 }),
        schedule: &[],
    },
    Profile {
        name: "loss5",
        summary: "RTT 80 ms, jitter 50 ms, 5% loss each way",
        link: jittery(40.0, 50.0, Loss::Random { pct: 5.0 }),
        schedule: &[],
    },
    Profile {
        name: "burst",
        summary: "RTT 80 ms, jitter 60 ms, about 11% loss in bursts of about 8 packets",
        link: jittery(40.0, 60.0, Loss::Burst { p: 1.5, r: 12.0 }),
        schedule: &[],
    },
    Profile {
        name: "loss10",
        summary: "RTT 120 ms, jitter 80 ms, 10% loss each way",
        link: jittery(60.0, 80.0, Loss::Random { pct: 10.0 }),
        schedule: &[],
    },
    Profile {
        name: "loss20",
        summary: "RTT 160 ms, jitter 120 ms, 20% loss each way, 1% duplicates, 0.1% corruption",
        link: Link { delay_ms: 80.0, jitter_ms: 120.0, pareto: true, loss: Loss::Random { pct: 20.0 }, duplicate_pct: 1.0, corrupt_pct: 0.1, rate_mbit: None },
        schedule: &[],
    },
    Profile {
        name: "flap",
        summary: "conditions change every few seconds, with a 2 s total blackout in each 28 s cycle",
        link: Link::CLEAN,
        schedule: FLAP,
    },
];

pub fn profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

/// Runs a privileged command, through `sudo -n` unless already root.
fn run(args: &[&str]) -> Result<String, String> {
    let mut cmd = if is_root() {
        let mut c = Command::new(args[0]);
        c.args(&args[1..]);
        c
    } else {
        let mut c = Command::new("sudo");
        c.arg("-n").args(args);
        c
    };
    let out = cmd.output().map_err(|e| format!("{}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!("{}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn exists() -> bool {
    run(&["ip", "netns", "list"]).is_ok_and(|s| s.lines().any(|l| l.starts_with(CLIENT_NS)))
}

pub fn up() -> Result<(), String> {
    if exists() {
        return Ok(());
    }
    run(&["ip", "netns", "add", CLIENT_NS])?;
    run(&["ip", "netns", "add", SERVER_NS])?;
    run(&["ip", "link", "add", CLIENT_DEV, "netns", CLIENT_NS, "type", "veth", "peer", "name", SERVER_DEV, "netns", SERVER_NS])?;
    for (ns, dev, ip) in [(CLIENT_NS, CLIENT_DEV, CLIENT_IP), (SERVER_NS, SERVER_DEV, SERVER_IP)] {
        run(&["ip", "-n", ns, "link", "set", "lo", "up"])?;
        run(&["ip", "-n", ns, "addr", "add", &format!("{ip}/30"), "dev", dev])?;
        run(&["ip", "-n", ns, "link", "set", dev, "mtu", MTU, "up"])?;
        // With segmentation offload a 64 KB super-packet would be lost or kept as one unit, so a loss
        // setting would mean something very different from what it says.
        run(&["ip", "netns", "exec", ns, "ethtool", "-K", dev, "tso", "off", "gso", "off", "gro", "off", "ufo", "off"]).or_else(|_| {
            run(&["ip", "netns", "exec", ns, "ethtool", "-K", dev, "tso", "off", "gso", "off", "gro", "off"])
        })?;
    }
    Ok(())
}

pub fn down() -> Result<(), String> {
    let _ = run(&["ip", "netns", "del", CLIENT_NS]);
    let _ = run(&["ip", "netns", "del", SERVER_NS]);
    if exists() {
        return Err("namespaces still exist".into());
    }
    Ok(())
}

/// Applies one link setting to both directions.
pub fn apply(link: &Link) -> Result<(), String> {
    let args = link.netem_args();
    for (ns, dev) in [(CLIENT_NS, CLIENT_DEV), (SERVER_NS, SERVER_DEV)] {
        let mut cmd: Vec<&str> = vec!["ip", "netns", "exec", ns, "tc", "qdisc", "replace", "dev", dev, "root"];
        cmd.extend(args.iter().map(String::as_str));
        run(&cmd)?;
    }
    Ok(())
}

pub fn apply_profile(p: &Profile) -> Result<(), String> {
    apply(p.schedule.first().map_or(&p.link, |(_, link)| link))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QdiscCounters {
    pub sent_packets: u64,
    pub dropped: u64,
}

/// Packets sent and dropped by netem since the qdisc was installed. tc's JSON is stable across versions.
pub fn qdisc_counters(client_side: bool) -> Result<QdiscCounters, String> {
    let (ns, dev) = if client_side { (CLIENT_NS, CLIENT_DEV) } else { (SERVER_NS, SERVER_DEV) };
    let out = run(&["ip", "netns", "exec", ns, "tc", "-s", "-j", "qdisc", "show", "dev", dev])?;
    let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| format!("tc json: {e}"))?;
    let q = v.as_array().and_then(|a| a.first()).ok_or("no qdisc")?;
    Ok(QdiscCounters { sent_packets: q["packets"].as_u64().unwrap_or(0), dropped: q["drops"].as_u64().unwrap_or(0) })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct TcpCounters {
    pub in_segs: u64,
    pub out_segs: u64,
    pub retrans_segs: u64,
    pub rto_timeouts: u64,
    pub tlp_probes: u64,
    pub fast_retrans: u64,
    pub estab_resets: u64,
    pub attempt_fails: u64,
}

impl TcpCounters {
    pub fn delta_since(&self, before: &TcpCounters) -> TcpCounters {
        TcpCounters {
            in_segs: self.in_segs.saturating_sub(before.in_segs),
            out_segs: self.out_segs.saturating_sub(before.out_segs),
            retrans_segs: self.retrans_segs.saturating_sub(before.retrans_segs),
            rto_timeouts: self.rto_timeouts.saturating_sub(before.rto_timeouts),
            tlp_probes: self.tlp_probes.saturating_sub(before.tlp_probes),
            fast_retrans: self.fast_retrans.saturating_sub(before.fast_retrans),
            estab_resets: self.estab_resets.saturating_sub(before.estab_resets),
            attempt_fails: self.attempt_fails.saturating_sub(before.attempt_fails),
        }
    }
}

/// Finds `key` in the /proc/net table format, where a header line of names precedes a line of values.
fn proc_table(text: &str, section: &str, key: &str) -> Option<u64> {
    let mut lines = text.lines().filter(|l| l.starts_with(section));
    let names = lines.next()?;
    let values = lines.next()?;
    let index = names.split_whitespace().position(|n| n == key)?;
    values.split_whitespace().nth(index)?.parse().ok()
}

fn parse_tcp_counters(snmp: &str, netstat: &str) -> TcpCounters {
    let s = |k| proc_table(snmp, "Tcp:", k).unwrap_or(0);
    let n = |k| proc_table(netstat, "TcpExt:", k).unwrap_or(0);
    TcpCounters {
        in_segs: s("InSegs"),
        out_segs: s("OutSegs"),
        retrans_segs: s("RetransSegs"),
        rto_timeouts: n("TCPTimeouts"),
        tlp_probes: n("TCPLossProbes"),
        fast_retrans: n("TCPFastRetrans"),
        estab_resets: s("EstabResets"),
        attempt_fails: s("AttemptFails"),
    }
}

/// TCP counters of one namespace.
pub fn tcp_counters(client_side: bool) -> Result<TcpCounters, String> {
    let ns = if client_side { CLIENT_NS } else { SERVER_NS };
    let snmp = run(&["ip", "netns", "exec", ns, "cat", "/proc/net/snmp"])?;
    let netstat = run(&["ip", "netns", "exec", ns, "cat", "/proc/net/netstat"])?;
    Ok(parse_tcp_counters(&snmp, &netstat))
}

/// Creates a blocking socket-making thread inside a namespace. A socket stays in the namespace it was
/// created in whichever thread later uses it, so a single process can straddle two network stacks.
#[cfg(target_os = "linux")]
pub fn in_namespace<T: Send + 'static>(ns: &str, work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    use std::os::fd::AsRawFd;
    let path = format!("/var/run/netns/{ns}");
    let handle = std::thread::spawn(move || -> Result<T, String> {
        let file = std::fs::File::open(&path).map_err(|e| format!("{path}: {e}"))?;
        // SAFETY: the fd is valid for the call, and CLONE_NEWNET only affects this short-lived thread.
        if unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
            return Err(format!("setns {path}: {}", std::io::Error::last_os_error()));
        }
        Ok(work())
    });
    handle.join().map_err(|_| "namespace thread panicked".to_owned())?
}

#[cfg(not(target_os = "linux"))]
pub fn in_namespace<T: Send + 'static>(_: &str, _: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    Err("network namespaces need Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netem_arguments_match_tc_syntax() {
        assert_eq!(Link::CLEAN.netem_args().join(" "), "netem limit 200000");
        let loss20 = profile("loss20").unwrap().link.netem_args().join(" ");
        assert_eq!(loss20, "netem limit 200000 delay 80ms 120ms distribution pareto loss 20% duplicate 1% corrupt 0.1%");
        let burst = profile("burst").unwrap().link.netem_args().join(" ");
        assert_eq!(burst, "netem limit 200000 delay 40ms 60ms distribution pareto loss gemodel 1.5% 12%");
    }

    #[test]
    fn every_profile_is_reachable_by_name() {
        for p in PROFILES {
            assert_eq!(profile(p.name).unwrap().name, p.name);
        }
        assert!(profile("nonsense").is_none());
    }

    #[test]
    fn reads_proc_net_tables() {
        let snmp = "Ip: Forwarding DefaultTTL\nIp: 1 64\nTcp: RtoAlgorithm InSegs OutSegs RetransSegs EstabResets AttemptFails\nTcp: 1 100 200 7 2 1\n";
        let netstat = "TcpExt: SyncookiesSent TCPTimeouts TCPLossProbes TCPFastRetrans\nTcpExt: 0 11 12 13\nIpExt: A\nIpExt: 1\n";
        let c = parse_tcp_counters(snmp, netstat);
        assert_eq!(c, TcpCounters { in_segs: 100, out_segs: 200, retrans_segs: 7, rto_timeouts: 11, tlp_probes: 12, fast_retrans: 13, estab_resets: 2, attempt_fails: 1 });
        let later = TcpCounters { retrans_segs: 10, ..c };
        assert_eq!(later.delta_since(&c).retrans_segs, 3);
    }
}
