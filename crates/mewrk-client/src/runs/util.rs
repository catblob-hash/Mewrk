//! `new Date().toISOString()`, `Date.parse` and `createId` (lib/id.ts), which the run code leans on throughout.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// `Date.now()`.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    iso_from_ms(now_ms())
}

/// `new Date(ms).toISOString()`.
pub fn iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// `Date.parse` for the forms the host and the renderer write: RFC 3339 with any fraction, or a bare date.
/// `None` is JavaScript's `NaN`.
pub fn parse_iso(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (y, m, d) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, m, d) * 86_400_000;
    if bytes.len() == 10 {
        return Some(ms);
    }
    if bytes[10] != b'T' && bytes[10] != b' ' {
        return None;
    }
    let (hh, mm) = (number(11..13)?, number(14..16)?);
    if bytes.get(13) != Some(&b':') || bytes.get(16) != Some(&b':') || hh > 24 || mm > 59 {
        return None;
    }
    let ss = number(17..19)?;
    if ss > 60 {
        return None;
    }
    ms += (hh * 3600 + mm * 60 + ss) * 1000;
    let mut at = 19;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(|b| b.is_ascii_digit()) {
            at += 1;
        }
        if at == start {
            return None;
        }
        let fraction = &text[start..at];
        let padded = format!("{fraction:0<3}");
        ms += padded[..3].parse::<i64>().ok()?;
    }
    match bytes.get(at) {
        None | Some(b'Z') | Some(b'z') if at + 1 >= bytes.len() => Some(ms),
        Some(sign @ (b'+' | b'-')) => {
            let oh = number(at + 1..at + 3)?;
            let om = if bytes.get(at + 3) == Some(&b':') { number(at + 4..at + 6)? } else { number(at + 3..at + 5)? };
            let offset = (oh * 60 + om) * 60_000;
            Some(if *sign == b'+' { ms - offset } else { ms + offset })
        }
        _ => None,
    }
}

// Howard Hinnant's civil calendar algorithms, the usual way to do this without a date crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `createId(prefix)`: `<prefix>_` and a random UUID, unique across processes.
pub fn create_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded from the OS once per process; mixing the clock and a counter makes each draw distinct.
    let hash = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_i64(now_ms());
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.finish()
    };
    let (a, b) = (hash(0x9e37), hash(0x79b9));
    format!(
        "{prefix}_{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a & 0x0fff,
        0x8000 | (b >> 48) as u16 & 0x3fff,
        b & 0xffff_ffff_ffff,
    )
}

/// `Array.from(text).length` compared with a limit, without counting a whole long string.
pub fn exceeds_chars(text: &str, limit: usize) -> bool {
    text.chars().nth(limit).is_some()
}

/// `text.slice(0, n)`: JavaScript counts UTF-16 units, and a pair cut in half never reaches a title here because
/// the result is only ever displayed; a whole code point is kept instead.
pub fn slice_utf16(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in text.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip_like_javascript() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(parse_iso("2026-08-04T00:00:01.000Z"), Some(1_785_801_601_000));
        assert_eq!(iso_from_ms(1_785_801_601_000), "2026-08-04T00:00:01.000Z");
        assert_eq!(parse_iso("2026-08-04T00:00:14.000Z").unwrap() - parse_iso("2026-08-04T00:00:01.000Z").unwrap(), 13_000);
        assert_eq!(parse_iso("2026-08-04T08:00:01+08:00"), Some(1_785_801_601_000));
        assert_eq!(parse_iso("2026-08-04T00:00:01.123456Z"), Some(1_785_801_601_123));
        assert_eq!(parse_iso("2026-08-04"), Some(1_785_801_600_000));
        assert_eq!(parse_iso("keep"), None);
        assert_eq!(parse_iso(""), None);
    }

    #[test]
    fn ids_are_prefixed_and_distinct() {
        let (a, b) = (create_id("run"), create_id("run"));
        assert!(a.starts_with("run_"));
        assert_ne!(a, b);
        assert_eq!(a.len(), "run_".len() + 36);
    }

    #[test]
    fn utf16_slice_counts_units() {
        assert_eq!(slice_utf16("abcdef", 3), "abc");
        assert_eq!(slice_utf16("你好世界", 2), "你好");
        assert_eq!(slice_utf16("😀😀", 3), "😀");
    }
}
