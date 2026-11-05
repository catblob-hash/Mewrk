//! What upstream got from JavaScript's own library: `Date.parse`, UTF-16 lengths and whitespace rules, so that
//! previews cut and rows sort exactly where they did.

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// `Date.parse`: RFC 3339, a local date-time without an offset, or a date alone (which JavaScript reads as UTC).
pub fn parse_time(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Some(at.timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(at) = NaiveDateTime::parse_from_str(value, format) {
            return Local.from_local_datetime(&at).earliest().map(|d| d.timestamp_millis());
        }
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

/// An unparsable or absent stamp reads as the epoch, as upstream's `timestamp` has it.
pub fn timestamp(value: Option<&str>) -> i64 {
    value.and_then(parse_time).unwrap_or(0)
}

/// `/\s/` in JavaScript: Unicode white space plus the byte order mark.
pub fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `s.replace(/\s+/g, " ").trim()`.
pub fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for c in s.chars() {
        if is_js_space(c) {
            pending = true;
        } else {
            if pending && !out.is_empty() {
                out.push(' ');
            }
            pending = false;
            out.push(c);
        }
    }
    out
}

/// `s.length`.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, n)`, keeping a pair whole rather than leaving half of it behind.
pub fn utf16_prefix(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

/// `s.slice(-n)`.
pub fn utf16_suffix(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices().rev() {
        units += c.len_utf16();
        if units > n {
            return &s[i + c.len_utf8()..];
        }
    }
    s
}

/// Compact wall time: "9s", "1m24s", "3m02s", "1h04m". `lib/elapsed.ts`.
pub fn format_elapsed(ms: i64) -> String {
    let total = (ms / 1000).max(0);
    if total < 60 {
        return format!("{total}s");
    }
    let (minutes, seconds) = (total / 60, total % 60);
    if minutes < 60 {
        return format!("{minutes}m{seconds:02}s");
    }
    format!("{}h{:02}m", minutes / 60, minutes % 60)
}

/// `x.toFixed(1)` on the decimal-looking value JavaScript would print. Rust rounds the same binary value the same way.
pub fn to_fixed(v: f64, digits: usize) -> String {
    format!("{v:.digits$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_date_parse_does() {
        assert_eq!(parse_time("1970-01-01T00:00:10Z"), Some(10_000));
        assert_eq!(parse_time("not a date"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("1970-01-02"), Some(86_400_000));
        assert_eq!(parse_time("1970-01-01T00:00:01.5Z"), Some(1500));
    }

    #[test]
    fn collapses_and_trims_like_the_regexp() {
        assert_eq!(collapse_spaces("  a\n\t b\u{3000}c  "), "a b c");
        assert_eq!(collapse_spaces(""), "");
    }

    #[test]
    fn utf16_slices_count_code_units() {
        assert_eq!(utf16_len("a😀"), 3);
        assert_eq!(utf16_prefix("a😀b", 3), "a😀");
        assert_eq!(utf16_prefix("a😀b", 2), "a");
        assert_eq!(utf16_suffix("abcd", 2), "cd");
    }

    #[test]
    fn elapsed_matches_upstream_spelling() {
        assert_eq!(format_elapsed(9_000), "9s");
        assert_eq!(format_elapsed(84_000), "1m24s");
        assert_eq!(format_elapsed(182_000), "3m02s");
        assert_eq!(format_elapsed(3_840_000), "1h04m");
        assert_eq!(format_elapsed(-5), "0s");
    }
}
