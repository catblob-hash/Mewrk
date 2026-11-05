//! Failure classification. Whether to retry depends on what failed, so kinds stay explicit all the way up.

use std::fmt;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FailKind {
    Connect,
    ConnectTimeout,
    Tls,
    Upload,
    FirstByteTimeout,
    Stall,
    Reset,
    /// The connection ended cleanly at the HTTP level, but no finish reason was ever seen.
    Truncated,
    Http(u16),
    /// The server reported a failure inside the stream.
    Server(u16),
    Protocol,
    Cancelled,
}

impl FailKind {
    pub fn retryable(self) -> bool {
        match self {
            FailKind::Cancelled => false,
            // These describe the request, and sending it again would only get the same answer.
            FailKind::Http(code) | FailKind::Server(code) => matches!(code, 0 | 408 | 425 | 429 | 500..=599),
            _ => true,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FailKind::Connect => "connect",
            FailKind::ConnectTimeout => "connect-timeout",
            FailKind::Tls => "tls",
            FailKind::Upload => "upload-stall",
            FailKind::FirstByteTimeout => "first-byte-timeout",
            FailKind::Stall => "stall",
            FailKind::Reset => "reset",
            FailKind::Truncated => "truncated",
            FailKind::Http(_) => "http",
            FailKind::Server(_) => "server",
            FailKind::Protocol => "protocol",
            FailKind::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetError {
    pub kind: FailKind,
    pub detail: String,
}

impl NetError {
    pub fn new(kind: FailKind, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into() }
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.kind.label(), self.detail)
    }
}
impl std::error::Error for NetError {}

/// Decorrelated jitter: retries spread out on their own, so a link drop that kills 128 streams at once
/// does not come back as 128 handshakes in the same instant.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 16, base: Duration::from_millis(100), cap: Duration::from_secs(6) }
    }
}

pub struct Backoff {
    prev: Duration,
    policy: RetryPolicy,
    rng: fastrand::Rng,
}

impl Backoff {
    pub fn new(policy: RetryPolicy, seed: u64) -> Self {
        Self { prev: policy.base, rng: fastrand::Rng::with_seed(seed), policy }
    }

    pub fn next_delay(&mut self) -> Duration {
        let lo = self.policy.base.as_millis() as u64;
        let hi = (self.prev.as_millis() as u64 * 3).max(lo + 1);
        let next = Duration::from_millis(self.rng.u64(lo..=hi)).min(self.policy.cap);
        self.prev = next;
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_request_errors_are_final() {
        assert!(FailKind::Stall.retryable());
        assert!(FailKind::Tls.retryable());
        assert!(FailKind::Http(503).retryable());
        assert!(FailKind::Http(429).retryable());
        assert!(!FailKind::Http(400).retryable());
        assert!(!FailKind::Server(400).retryable());
        assert!(!FailKind::Cancelled.retryable());
    }

    #[test]
    fn backoff_stays_within_bounds_and_spreads_out() {
        let policy = RetryPolicy::default();
        let mut delays = std::collections::BTreeSet::new();
        for seed in 0..128 {
            let mut backoff = Backoff::new(policy.clone(), seed);
            for _ in 0..12 {
                let d = backoff.next_delay();
                assert!(d >= policy.base && d <= policy.cap, "{d:?}");
            }
            delays.insert(Backoff::new(policy.clone(), seed).next_delay());
        }
        assert!(delays.len() > 32, "first retries barely spread: {}", delays.len());
    }
}
