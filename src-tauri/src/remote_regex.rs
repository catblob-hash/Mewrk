//! Rust-regex patterns rendered for the remote `grep` legs.
//!
//! The host leg matches each line with the `regex` crate. A remote machine has
//! no such crate, so its leg hands the pattern to whatever engine is there: GNU
//! `grep -P`, a POSIX `grep -E` (macOS, BusyBox), or .NET's `Regex` under
//! PowerShell. Those dialects disagree with Rust and with each other on
//! escapes, classes, case folding, word boundaries and even on which patterns
//! are legal, so passing the model's pattern through verbatim made one tool
//! call mean different things on different machines. This module parses the
//! pattern with the parser the host leg uses and writes it out again in each
//! remote dialect, so the model only ever has to speak Rust's syntax.
//!
//! Every rendering accepts a superset of the lines the Rust regex accepts. The
//! host re-checks each line a remote returns with the real matcher, so an
//! extra line costs only transfer and a share of the remote's line cap, while
//! a missing line would be a wrong answer that nothing downstream can repair.
//! Where a dialect can say exactly what Rust means the rendering is exact, and
//! the `*_exact` flags record when it could not be.
//!
//! The renderings assume the lines each remote engine sees:
//!
//! * `pcre` and `ere` run under `LC_ALL=C` on the raw bytes of a line, which
//!   the host decodes with `String::from_utf8_lossy` instead. Non-ASCII text is
//!   therefore spelled as UTF-8 byte sequences, and a class that holds U+FFFD
//!   also accepts the invalid byte runs the host decodes to it. grep strips
//!   only the `\n`, so a CRLF line still ends in the `\r` that `str::lines`
//!   drops: end anchors allow it, and no class may consume it. That reading
//!   cannot tell a CRLF line from a last line that really ends in a bare `\r`,
//!   and treats both as CRLF.
//! * `dotnet` runs on the line decoded to UTF-16 with CRLF already folded, so
//!   it sees the host's characters, astral ones as surrogate pairs.
//!
//! Case-insensitivity is folded into the parsed classes, so no rendering may
//! be run with an engine's own case-insensitive flag.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{Class, Hir, HirKind, Look};
use regex_syntax::utf8::Utf8Sequences;
use regex_syntax::ParserBuilder;

/// One `grep` pattern rendered for each remote engine, with the host's matcher.
pub(crate) struct RemotePattern {
    /// For GNU `grep -P` under `LC_ALL=C`, without `-i`. Printable ASCII with
    /// no single quote, so it can sit inside a single-quoted shell word.
    pub pcre: String,
    /// Whether `pcre` accepts exactly the host's lines rather than a
    /// superset; likewise `ere_exact` and `dotnet_exact`. The grep leg does
    /// not consult them — it cannot tell which engine a machine ran and
    /// re-checks every line anyway — but the tests hold each rendering to
    /// the claim.
    #[cfg_attr(not(test), allow(dead_code))]
    pub pcre_exact: bool,
    /// For POSIX `grep -E` under `LC_ALL=C`, without `-i`. Raw bytes, because
    /// bracket expressions take no escapes; never a newline or a NUL. Pass it
    /// through [`ere_printf_argument`].
    pub ere: Vec<u8>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub ere_exact: bool,
    /// For .NET `Regex` with `RegexOptions.CultureInvariant` and nothing else.
    /// Printable ASCII with no single quote, so it can sit inside a
    /// single-quoted PowerShell string.
    pub dotnet: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub dotnet_exact: bool,
    /// The host's own matcher, built exactly as the local leg builds it, for
    /// re-checking remote lines.
    pub regex: Regex,
}

/// Renders `pattern` for every remote engine.
///
/// The pattern is validated by the same `RegexBuilder` call the local leg
/// makes, so a bad pattern fails with the very `regex::Error` the local leg
/// would report, before any remote shell is involved.
pub(crate) fn translate(
    pattern: &str,
    case_sensitive: bool,
) -> Result<RemotePattern, regex::Error> {
    translate_from(pattern, case_sensitive, &LEVELS)
}

/// [`translate`], trying only `levels`; tests start lower to reach the
/// widened forms.
fn translate_from(
    pattern: &str,
    case_sensitive: bool,
    levels: &[Level],
) -> Result<RemotePattern, regex::Error> {
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()?;
    let mut faithful = true;
    let node = match parser(case_sensitive).parse(pattern) {
        Ok(hir) => lower(&hir, &mut faithful),
        // `regex` has just parsed the same text with the same settings, so this
        // cannot happen; were the two ever to disagree, matching every line
        // still leaves the answer to the host's re-check.
        Err(_) => {
            faithful = false;
            Node::Empty
        }
    };
    let (pcre, pcre_exact) = render(&node, Dialect::Pcre, levels);
    let (ere, ere_exact) = render(&node, Dialect::Ere, levels);
    let (dotnet, dotnet_exact) = render(&node, Dialect::Dotnet, levels);
    Ok(RemotePattern {
        pcre: String::from_utf8_lossy(&pcre).into_owned(),
        pcre_exact: pcre_exact && faithful,
        ere,
        ere_exact: ere_exact && faithful,
        dotnet: String::from_utf8_lossy(&dotnet).into_owned(),
        dotnet_exact: dotnet_exact && faithful,
        regex,
    })
}

/// A single-quoted POSIX `printf` format that prints `ere` byte for byte.
///
/// Bracket expressions hold raw high and control bytes, which no quoting
/// survives intact across every shell and transport. Every byte other than an
/// ASCII letter or digit becomes a `\ooo` escape, so `%`, `\`, quotes and
/// non-ASCII are all inert; `MEWRK_RX=$(printf '...')` then restores the
/// pattern exactly, since the pattern never ends in the newline `$(...)` strips.
pub(crate) fn ere_printf_argument(ere: &[u8]) -> String {
    let mut out = String::with_capacity(printf_len(ere));
    out.push('\'');
    for &byte in ere {
        if byte.is_ascii_alphanumeric() {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "\\{byte:03o}");
        }
    }
    out.push('\'');
    out
}

fn printf_len(ere: &[u8]) -> usize {
    2 + ere
        .iter()
        .map(|byte| if byte.is_ascii_alphanumeric() { 1 } else { 4 })
        .sum::<usize>()
}

/// The `regex` crate's own syntax settings, spelled out: its builder applies
/// exactly these to `regex_syntax`, and only the case flag varies per call.
fn parser(case_sensitive: bool) -> regex_syntax::Parser {
    ParserBuilder::new()
        .case_insensitive(!case_sensitive)
        .multi_line(false)
        .dot_matches_new_line(false)
        .crlf(false)
        .line_terminator(b'\n')
        .swap_greed(false)
        .ignore_whitespace(false)
        .unicode(true)
        .utf8(true)
        .nest_limit(250)
        .octal(false)
        .build()
}

// ---------------------------------------------------------------------------
// The line-level form of a pattern.

/// Inclusive scalar-value ranges, sorted, disjoint and free of surrogates.
type Ranges = Vec<(u32, u32)>;
/// Inclusive byte ranges, sorted and disjoint.
type ByteSet = Vec<(u8, u8)>;
/// One alternative of a multi-byte character: a byte set per position.
type ByteSeq = Vec<ByteSet>;

const LF: u32 = 0x0A;
const CR: u32 = 0x0D;
const REPLACEMENT: u32 = 0xFFFD;
const MAX_CHAR: u32 = 0x10FFFF;

/// A pattern as it matters for matching one line.
///
/// Lines never hold `\n`, so it is removed from every class (a part that needs
/// it can never match), and the multi-line anchors are the text anchors.
/// Captures, names and laziness change nothing for `is_match` and are gone.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    /// Matches the empty string.
    Empty,
    /// Matches nothing.
    Never,
    /// One character from the set.
    Chars(Ranges),
    /// One raw byte from the set; only the ERE word-edge stand-ins use it.
    Byte(ByteSet),
    Look(Look),
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        sub: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

impl Node {
    fn nullable(&self) -> bool {
        match self {
            Node::Empty | Node::Look(_) => true,
            Node::Never | Node::Chars(_) | Node::Byte(_) => false,
            Node::Seq(items) => items.iter().all(Node::nullable),
            Node::Alt(items) => items.iter().any(Node::nullable),
            Node::Repeat { sub, min, .. } => *min == 0 || sub.nullable(),
        }
    }

    fn zero_width(&self) -> bool {
        match self {
            Node::Empty | Node::Look(_) => true,
            Node::Never | Node::Chars(_) | Node::Byte(_) => false,
            Node::Seq(items) | Node::Alt(items) => items.iter().all(Node::zero_width),
            Node::Repeat { sub, .. } => sub.zero_width(),
        }
    }

    /// Whether an end anchor occurs; ERE spells it by consuming the `\r`.
    fn has_end(&self) -> bool {
        match self {
            Node::Look(look) => *look == Look::End,
            Node::Empty | Node::Never | Node::Chars(_) | Node::Byte(_) => false,
            Node::Seq(items) | Node::Alt(items) => items.iter().any(Node::has_end),
            Node::Repeat { sub, .. } => sub.has_end(),
        }
    }
}

/// Lowers parsed HIR; clears `faithful` if any part had to be widened.
fn lower(hir: &Hir, faithful: &mut bool) -> Node {
    match hir.kind() {
        HirKind::Empty => Node::Empty,
        HirKind::Literal(literal) => match std::str::from_utf8(&literal.0) {
            Ok(text) => seq(text
                .chars()
                .map(|c| chars(vec![(u32::from(c), u32::from(c))]))
                .collect()),
            Err(_) => seq(literal
                .0
                .iter()
                .map(|&byte| byte_class(&[(byte, byte)], faithful))
                .collect()),
        },
        HirKind::Class(Class::Unicode(class)) => chars(
            class
                .ranges()
                .iter()
                .map(|range| (u32::from(range.start()), u32::from(range.end())))
                .collect(),
        ),
        HirKind::Class(Class::Bytes(class)) => {
            let ranges: ByteSet = class
                .ranges()
                .iter()
                .map(|range| (range.start(), range.end()))
                .collect();
            byte_class(&ranges, faithful)
        }
        HirKind::Look(look) => Node::Look(match look {
            Look::StartLF => Look::Start,
            Look::EndLF => Look::End,
            other => *other,
        }),
        HirKind::Repetition(rep) => repeat(lower(&rep.sub, faithful), rep.min, rep.max),
        HirKind::Capture(capture) => lower(&capture.sub, faithful),
        HirKind::Concat(items) => seq(items.iter().map(|item| lower(item, faithful)).collect()),
        HirKind::Alternation(items) => {
            alt(items.iter().map(|item| lower(item, faithful)).collect())
        }
    }
}

/// A byte class from `(?-u:...)`. A `&str` regex only admits ASCII ones; a
/// high byte, should one appear, stands for any non-ASCII character.
fn byte_class(ranges: &[(u8, u8)], faithful: &mut bool) -> Node {
    let mut set: Ranges = ranges
        .iter()
        .filter(|range| range.0 < 0x80)
        .map(|&(start, end)| (u32::from(start), u32::from(end.min(0x7F))))
        .collect();
    if ranges.iter().any(|range| range.1 >= 0x80) {
        *faithful = false;
        set.push((0x80, MAX_CHAR));
    }
    chars(set)
}

fn chars(set: Ranges) -> Node {
    let set = subtract(&subtract(&normalize(set), 0xD800, 0xDFFF), LF, LF);
    if set.is_empty() {
        Node::Never
    } else {
        Node::Chars(set)
    }
}

fn seq(items: Vec<Node>) -> Node {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Node::Empty => {}
            Node::Never => return Node::Never,
            Node::Seq(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    match out.len() {
        0 => Node::Empty,
        1 => out.pop().unwrap_or(Node::Empty),
        _ => Node::Seq(out),
    }
}

fn alt(items: Vec<Node>) -> Node {
    let mut out: Vec<Node> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Node::Never => {}
            Node::Alt(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    out.dedup();
    match out.len() {
        0 => Node::Never,
        1 => out.pop().unwrap_or(Node::Never),
        _ => Node::Alt(out),
    }
}

fn repeat(sub: Node, min: u32, max: Option<u32>) -> Node {
    if max == Some(0) {
        return Node::Empty;
    }
    match sub {
        Node::Never if min == 0 => Node::Empty,
        Node::Never => Node::Never,
        // Repeating an assertion asserts it once, or not at all; spelling that
        // directly keeps quantified anchors out of dialects that reject them.
        sub if sub.zero_width() => {
            if min == 0 {
                Node::Empty
            } else {
                sub
            }
        }
        sub if min == 1 && max == Some(1) => sub,
        sub => Node::Repeat {
            sub: Box::new(sub),
            min,
            max,
        },
    }
}

// ---------------------------------------------------------------------------
// Range arithmetic.

fn normalize(mut set: Ranges) -> Ranges {
    set.sort_unstable();
    let mut out: Ranges = Vec::with_capacity(set.len());
    for (start, end) in set {
        if let Some(last) = out.last_mut() {
            if start <= last.1.saturating_add(1) {
                last.1 = last.1.max(end);
                continue;
            }
        }
        out.push((start, end));
    }
    out
}

fn normalize_bytes(mut set: ByteSet) -> ByteSet {
    set.sort_unstable();
    let mut out: ByteSet = Vec::with_capacity(set.len());
    for (start, end) in set {
        if let Some(last) = out.last_mut() {
            if u16::from(start) <= u16::from(last.1) + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        out.push((start, end));
    }
    out
}

fn subtract(set: &[(u32, u32)], lo: u32, hi: u32) -> Ranges {
    let mut out = Vec::with_capacity(set.len() + 1);
    for &(start, end) in set {
        if end < lo || start > hi {
            out.push((start, end));
            continue;
        }
        if start < lo {
            out.push((start, lo - 1));
        }
        if end > hi {
            out.push((hi + 1, end));
        }
    }
    out
}

fn intersect(set: &[(u32, u32)], lo: u32, hi: u32) -> Ranges {
    set.iter()
        .filter(|range| range.1 >= lo && range.0 <= hi)
        .map(|&(start, end)| (start.max(lo), end.min(hi)))
        .collect()
}

fn intersect_sets(a: &[(u32, u32)], b: &[(u32, u32)]) -> Ranges {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let start = a[i].0.max(b[j].0);
        let end = a[i].1.min(b[j].1);
        if start <= end {
            out.push((start, end));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn contains(set: &[(u32, u32)], c: u32) -> bool {
    set.iter().any(|range| range.0 <= c && c <= range.1)
}

fn size(set: &[(u32, u32)]) -> u64 {
    set.iter()
        .map(|range| u64::from(range.1 - range.0) + 1)
        .sum()
}

/// Whether `set` holds every character of `lo..=hi` (surrogates are not characters).
fn covers(set: &[(u32, u32)], lo: u32, hi: u32) -> bool {
    size(&intersect(set, lo, hi)) == size(&subtract(&[(lo, hi)], 0xD800, 0xDFFF))
}

fn to_bytes(set: &[(u32, u32)]) -> ByteSet {
    set.iter()
        .filter(|range| range.0 <= 0xFF)
        .map(|&(start, end)| (start as u8, end.min(0xFF) as u8))
        .collect()
}

fn byte_complement(set: &[(u8, u8)], universe: &[(u8, u8)]) -> ByteSet {
    let wide = |s: &[(u8, u8)]| -> Ranges {
        s.iter()
            .map(|&(a, b)| (u32::from(a), u32::from(b)))
            .collect()
    };
    let mut rest = wide(universe);
    for &(start, end) in &wide(set) {
        rest = subtract(&rest, start, end);
    }
    to_bytes(&rest)
}

fn bytes_contain(set: &[(u8, u8)], byte: u8) -> bool {
    set.iter().any(|range| range.0 <= byte && byte <= range.1)
}

/// The UTF-8 spellings of `set` (all non-ASCII), merged where they differ
/// only in their last position.
fn utf8_sequences(set: &[(u32, u32)]) -> Vec<ByteSeq> {
    let mut out: Vec<ByteSeq> = Vec::new();
    for &(start, end) in set {
        let (Some(start), Some(end)) = (char::from_u32(start), char::from_u32(end)) else {
            continue;
        };
        for sequence in Utf8Sequences::new(start, end) {
            let units: ByteSeq = sequence
                .as_slice()
                .iter()
                .map(|range| vec![(range.start, range.end)])
                .collect();
            // UTF-8 keeps code point order, so spellings that differ only in
            // their last byte arrive back to back.
            if let Some(last) = out.last_mut() {
                let n = units.len();
                if last.len() == n && n > 1 && last[..n - 1] == units[..n - 1] {
                    let mut tail = last[n - 1].clone();
                    tail.extend(units[n - 1].iter().copied());
                    last[n - 1] = normalize_bytes(tail);
                    continue;
                }
            }
            out.push(units);
        }
    }
    out
}

/// Merges spellings that agree after their first position, so `[\xe1-\xec]`
/// and `[\xee\xef]` leads sharing continuations become one alternative. The
/// search is quadratic, and past a few hundred spellings the class is too
/// large to spell out anyway.
fn merge_heads(sequences: &[ByteSeq]) -> Vec<ByteSeq> {
    if sequences.len() > 512 {
        return sequences.to_vec();
    }
    let mut merged: Vec<ByteSeq> = Vec::with_capacity(sequences.len());
    for sequence in sequences {
        if sequence.is_empty() {
            continue;
        }
        if let Some(existing) = merged
            .iter_mut()
            .find(|other| other.len() == sequence.len() && other[1..] == sequence[1..])
        {
            let mut head = existing[0].clone();
            head.extend(sequence[0].iter().copied());
            existing[0] = normalize_bytes(head);
        } else {
            merged.push(sequence.clone());
        }
    }
    merged
}

/// Any well-formed multi-byte character, by lead byte alone. On valid UTF-8 it
/// matches exactly one character, which keeps counted repetitions honest.
fn coarse_utf8() -> Vec<ByteSeq> {
    let cont = vec![(0x80, 0xBF)];
    vec![
        vec![vec![(0xC2, 0xDF)], cont.clone()],
        vec![vec![(0xE0, 0xEF)], cont.clone(), cont.clone()],
        vec![vec![(0xF0, 0xF4)], cont.clone(), cont.clone(), cont],
    ]
}

/// A surrogate-pair spelling: a range of high surrogates, each followed by
/// one of the low-surrogate ranges.
type SurrogatePairs = ((u16, u16), Vec<(u16, u16)>);

/// Surrogate-pair spellings of astral `set`.
fn utf16_sequences(set: &[(u32, u32)]) -> Vec<SurrogatePairs> {
    fn split(c: u32) -> (u16, u16) {
        let v = c - 0x10000;
        ((0xD800 + (v >> 10)) as u16, (0xDC00 + (v & 0x3FF)) as u16)
    }
    let mut out: Vec<SurrogatePairs> = Vec::new();
    let mut push = |high: (u16, u16), low: (u16, u16)| {
        if let Some((last_high, lows)) = out.last_mut() {
            if *last_high == high {
                lows.push(low);
                return;
            }
            if lows.as_slice() == [(0xDC00, 0xDFFF)]
                && low == (0xDC00, 0xDFFF)
                && last_high.1 + 1 == high.0
            {
                last_high.1 = high.1;
                return;
            }
        }
        out.push((high, vec![low]));
    };
    for &(start, end) in set {
        let (high_start, low_start) = split(start);
        let (high_end, low_end) = split(end);
        if high_start == high_end {
            push((high_start, high_start), (low_start, low_end));
            continue;
        }
        let mut full_start = high_start;
        if low_start != 0xDC00 {
            push((high_start, high_start), (low_start, 0xDFFF));
            full_start += 1;
        }
        let full_end = if low_end == 0xDFFF {
            high_end
        } else {
            high_end - 1
        };
        if full_start <= full_end {
            push((full_start, full_end), (0xDC00, 0xDFFF));
        }
        if low_end != 0xDFFF {
            push((high_end, high_end), (0xDC00, low_end));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Rendering.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    Pcre,
    Ere,
    Dotnet,
}

impl Dialect {
    /// The largest repetition count the engine accepts: PCRE's compile-time
    /// limit, and POSIX's `RE_DUP_MAX` floor, which BSD and BusyBox enforce.
    fn max_count(self) -> u32 {
        match self {
            Dialect::Pcre => 65_535,
            Dialect::Ere => 255,
            Dialect::Dotnet => 1_000_000,
        }
    }
}

/// The patterns travel inside a shell script or an environment variable, so
/// each stays well under the 32 KiB that Windows allows a command line.
const MAX_TEXT: usize = 16 * 1024;
/// PCRE copies a counted group once per repetition into compiled code that,
/// in common builds, may not exceed 64 KiB; grep's DFA expands counts the same
/// way. The weight estimates that expanded size.
const MAX_WEIGHT: u64 = 32 * 1024;

/// How much precision a rendering may spend.
#[derive(Clone, Copy, Debug)]
struct Level {
    /// Most bytes of pattern text a class's non-ASCII part may take spelled
    /// out exactly; past it the part widens to any non-ASCII character. This
    /// also bounds the alternation PCRE tries at every position: `\d` spelled
    /// out fits the first level, `\w` or `\pL` (some 12 KiB) never do.
    budget: usize,
    /// Whether counted repetitions are widened to `*` and `+`.
    loose_counts: bool,
}

const LEVELS: [Level; 5] = [
    Level {
        budget: 2048,
        loose_counts: false,
    },
    Level {
        budget: 512,
        loose_counts: false,
    },
    Level {
        budget: 128,
        loose_counts: false,
    },
    Level {
        budget: 0,
        loose_counts: false,
    },
    Level {
        budget: 0,
        loose_counts: true,
    },
];

/// Renders `node`, giving up precision until the result fits.
fn render(node: &Node, dialect: Dialect, levels: &[Level]) -> (Vec<u8>, bool) {
    for &level in levels {
        if let Some(rendered) = attempt(node, dialect, level) {
            return rendered;
        }
    }
    // Still too large: keep a prefix of each top-level branch. A line holding
    // a match of the whole branch holds a match of its prefix, so this stays a
    // superset. Each level gets a binary search for its longest prefix that
    // fits, and the longest prefix wins, at the finest level that reaches it.
    let longest = match node {
        Node::Seq(items) => items.len(),
        Node::Alt(branches) => branches
            .iter()
            .map(|branch| match branch {
                Node::Seq(items) => items.len(),
                _ => 1,
            })
            .max()
            .unwrap_or(1),
        _ => 1,
    };
    let mut best: Option<(usize, Vec<u8>)> = None;
    for &level in levels {
        let floor = best.as_ref().map_or(1, |(keep, _)| keep + 1);
        let (mut low, mut high) = (floor, longest.saturating_sub(1));
        // A level that cannot fit even one item more than the best so far
        // has nothing to add.
        if best.is_some() && low <= high {
            match attempt(&truncate(node, low), dialect, level) {
                Some((text, _)) => {
                    best = Some((low, text));
                    low += 1;
                }
                None => continue,
            }
        }
        while low <= high {
            let keep = low + (high - low) / 2;
            match attempt(&truncate(node, keep), dialect, level) {
                Some((text, _)) => {
                    best = Some((keep, text));
                    low = keep + 1;
                }
                None => high = keep - 1,
            }
        }
    }
    // Without even a one-item prefix, a pattern that matches every line hands
    // the whole decision to the host's re-check.
    (best.map_or_else(|| b"^".to_vec(), |(_, text)| text), false)
}

fn truncate(node: &Node, keep: usize) -> Node {
    match node {
        Node::Seq(items) if items.len() > keep => seq(items[..keep].to_vec()),
        Node::Alt(branches) => alt(branches
            .iter()
            .map(|branch| truncate(branch, keep))
            .collect()),
        other => other.clone(),
    }
}

fn attempt(node: &Node, dialect: Dialect, level: Level) -> Option<(Vec<u8>, bool)> {
    let mut edges_exact = true;
    let edged;
    let node = if dialect == Dialect::Ere {
        edged = ere_edges(node, &mut edges_exact);
        &edged
    } else {
        node
    };
    let mut emitter = Emitter {
        dialect,
        level,
        exact: edges_exact,
        define: false,
        spelled: HashMap::new(),
    };
    let frag = emitter.root(node);
    let length = if dialect == Dialect::Ere {
        printf_len(&frag.text)
    } else {
        frag.text.len()
    };
    let heavy = dialect != Dialect::Dotnet && frag.weight > MAX_WEIGHT;
    (length <= MAX_TEXT && !heavy).then_some((frag.text, emitter.exact))
}

/// How a fragment binds, for deciding where groups are needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Prec {
    Empty,
    /// Takes a quantifier as it stands.
    Atom,
    /// Already quantified; another quantifier would change its meaning.
    Quantified,
    Concat,
    Alt,
}

#[derive(Clone)]
struct Frag {
    text: Vec<u8>,
    prec: Prec,
    /// Estimated expanded size; brackets count as the 32-byte bitmaps PCRE
    /// compiles them to.
    weight: u64,
}

impl Frag {
    fn empty() -> Frag {
        Frag {
            text: Vec::new(),
            prec: Prec::Empty,
            weight: 0,
        }
    }

    fn new(text: impl Into<Vec<u8>>, prec: Prec) -> Frag {
        let text = text.into();
        let brackets = text.iter().filter(|&&byte| byte == b'[').count() as u64;
        let weight = text.len() as u64 + 32 * brackets;
        Frag { text, prec, weight }
    }
}

struct Emitter {
    dialect: Dialect,
    level: Level,
    /// Cleared by every over-approximation.
    exact: bool,
    /// Whether the PCRE rendering calls the subroutines of [`pcre_define`].
    define: bool,
    /// Exact UTF-8 spellings by non-ASCII range set, since a pattern tends to
    /// repeat its classes and spelling a large one out is the costly step.
    spelled: HashMap<Ranges, Option<Vec<Frag>>>,
}

/// What surrounds a node in the whole pattern.
#[derive(Clone, Copy, Debug)]
struct Ctx {
    /// Everything after the node can match empty, so the node may consume a
    /// line's last byte.
    tail: bool,
    /// An end anchor may already have matched on this path.
    after_end: bool,
    /// What comes next may begin with a byte run.
    follow_run: bool,
}

impl Emitter {
    fn root(&mut self, node: &Node) -> Frag {
        let at_root = Ctx {
            tail: true,
            after_end: false,
            follow_run: false,
        };
        let frag = self.node(node, at_root);
        // An empty pattern matches every line; `^` says so in every dialect,
        // where an empty `-e ''` is not universally accepted.
        let frag = if frag.prec == Prec::Empty {
            Frag::new("^", Prec::Atom)
        } else {
            frag
        };
        if self.dialect != Dialect::Pcre || !self.define {
            return frag;
        }
        let mut frag = if frag.prec == Prec::Alt {
            self.group(frag)
        } else {
            frag
        };
        let define = Frag::new(pcre_define(), Prec::Atom);
        frag.text.extend_from_slice(&define.text);
        frag.weight += define.weight;
        frag.prec = Prec::Concat;
        frag
    }

    fn node(&mut self, node: &Node, ctx: Ctx) -> Frag {
        match node {
            Node::Empty => Frag::empty(),
            Node::Never => self.never(),
            Node::Chars(set) => match self.dialect {
                Dialect::Pcre => self.pcre_chars(set, ctx.tail),
                Dialect::Ere => self.ere_chars(set, ctx.tail),
                Dialect::Dotnet => self.dotnet_chars(set),
            },
            Node::Byte(set) => self.raw_byte(set),
            Node::Look(look) => self.look(*look, ctx),
            Node::Seq(items) => self.seq(items, ctx),
            Node::Alt(items) => {
                let mut parts = Vec::with_capacity(items.len());
                let mut optional = false;
                for item in items {
                    let frag = self.node(item, ctx);
                    if frag.prec == Prec::Empty {
                        optional = true;
                    } else if !parts.iter().any(|part: &Frag| part.text == frag.text) {
                        parts.push(frag);
                    }
                }
                if parts.is_empty() {
                    return Frag::empty();
                }
                // An empty branch is not portable ERE; `(x)?` says the same.
                let body = self.alternation(parts);
                if optional {
                    self.quantify(body, 0, Some(1))
                } else {
                    body
                }
            }
            Node::Repeat { sub, min, max } => self.repeat(sub, *min, *max, ctx),
        }
    }

    fn seq(&mut self, items: &[Node], ctx: Ctx) -> Frag {
        // Walk backwards to learn, for each item, whether the rest of the
        // pattern can match empty and whether a byte run can come next.
        let mut contexts = vec![ctx; items.len()];
        let (mut rest_nullable, mut run_next) = (ctx.tail, ctx.follow_run);
        for (index, item) in items.iter().enumerate().rev() {
            contexts[index].tail = rest_nullable;
            contexts[index].follow_run = run_next;
            let nullable = item.nullable();
            run_next = self.may_start_run(item) || (nullable && run_next);
            rest_nullable = rest_nullable && nullable;
        }
        let mut parts = Vec::with_capacity(items.len());
        let mut ended = ctx.after_end;
        for (item, mut item_ctx) in items.iter().zip(contexts) {
            item_ctx.after_end = ended;
            parts.push(self.node(item, item_ctx));
            ended = ended || item.has_end();
        }
        self.concat(parts)
    }

    fn repeat(&mut self, sub: &Node, mut min: u32, mut max: Option<u32>, ctx: Ctx) -> Frag {
        // Each widening below accepts every count the original accepts.
        if self.level.loose_counts && max != Some(1) && (min > 1 || max.is_some()) {
            self.exact = false;
            min = min.min(1);
            max = None;
        }
        let limit = self.dialect.max_count();
        if min > limit {
            self.exact = false;
            min = limit;
            max = None;
        }
        if max.is_some_and(|max| max > limit) {
            self.exact = false;
            max = None;
        }
        // Two byte runs side by side could split one character between
        // them, so only a run that nothing run-like follows may be one.
        if max.is_none() && min <= 1 && !ctx.follow_run {
            if let Node::Chars(set) = sub {
                if let Some(run) = self.run(set, ctx.tail) {
                    return self.quantify(run, min, None);
                }
            }
        }
        let again = max != Some(1);
        let sub_ctx = Ctx {
            tail: ctx.tail,
            after_end: ctx.after_end || (again && sub.has_end()),
            follow_run: ctx.follow_run || (again && self.may_start_run(sub)),
        };
        let frag = self.node(sub, sub_ctx);
        self.quantify(frag, min, max)
    }

    /// Whether `node` can begin by consuming bytes from inside a character:
    /// a class repeated without bound may be rendered as a byte run, and the
    /// ERE edge stand-ins match single raw bytes. "No ASCII word byte next"
    /// counts too, since it holds inside a character as well and would let a
    /// run before it stop there.
    fn may_start_run(&self, node: &Node) -> bool {
        match node {
            Node::Byte(_) | Node::Look(Look::WordEndHalfAscii) => true,
            Node::Repeat { sub, min, max } => {
                let unbounded = max.is_none() || (self.level.loose_counts && *max != Some(1));
                let short = *min <= 1 || self.level.loose_counts;
                let candidate = matches!(sub.as_ref(), Node::Chars(set)
                    if unbounded && short && !intersect(set, 0x80, MAX_CHAR).is_empty());
                candidate || self.may_start_run(sub)
            }
            Node::Seq(items) => {
                for item in items {
                    if self.may_start_run(item) {
                        return true;
                    }
                    if !item.nullable() {
                        return false;
                    }
                }
                false
            }
            Node::Alt(items) => items.iter().any(|item| self.may_start_run(item)),
            Node::Empty | Node::Never | Node::Chars(_) | Node::Look(_) => false,
        }
    }

    // -- Assembly ----------------------------------------------------------

    fn group(&self, frag: Frag) -> Frag {
        let open: &[u8] = if self.dialect == Dialect::Ere {
            b"("
        } else {
            b"(?:"
        };
        let mut text = Vec::with_capacity(frag.text.len() + open.len() + 1);
        text.extend_from_slice(open);
        text.extend_from_slice(&frag.text);
        text.push(b')');
        Frag {
            text,
            prec: Prec::Atom,
            weight: frag.weight + 4,
        }
    }

    fn concat(&self, parts: Vec<Frag>) -> Frag {
        let mut parts: Vec<Frag> = parts
            .into_iter()
            .filter(|part| part.prec != Prec::Empty)
            .collect();
        if parts.len() <= 1 {
            return parts.pop().unwrap_or_else(Frag::empty);
        }
        let mut text = Vec::new();
        let mut weight = 0u64;
        for part in parts {
            let part = if part.prec == Prec::Alt {
                self.group(part)
            } else {
                part
            };
            text.extend_from_slice(&part.text);
            weight = weight.saturating_add(part.weight);
        }
        Frag {
            text,
            prec: Prec::Concat,
            weight,
        }
    }

    fn alternation(&self, mut parts: Vec<Frag>) -> Frag {
        if parts.len() <= 1 {
            return parts.pop().unwrap_or_else(Frag::empty);
        }
        let mut text = Vec::new();
        let mut weight = 0u64;
        for (index, part) in parts.into_iter().enumerate() {
            if index > 0 {
                text.push(b'|');
            }
            text.extend_from_slice(&part.text);
            weight = weight.saturating_add(part.weight + 1);
        }
        Frag {
            text,
            prec: Prec::Alt,
            weight,
        }
    }

    fn quantify(&self, frag: Frag, min: u32, max: Option<u32>) -> Frag {
        if frag.prec == Prec::Empty || (min == 1 && max == Some(1)) {
            return frag;
        }
        let mut frag = if frag.prec == Prec::Atom {
            frag
        } else {
            self.group(frag)
        };
        let suffix = match (min, max) {
            (0, None) => "*".to_owned(),
            (1, None) => "+".to_owned(),
            (0, Some(1)) => "?".to_owned(),
            (min, None) => format!("{{{min},}}"),
            (min, Some(max)) if min == max => format!("{{{min}}}"),
            (min, Some(max)) => format!("{{{min},{max}}}"),
        };
        // .NET runs counted loops in place; the others copy the operand.
        let copies = if self.dialect == Dialect::Dotnet {
            1
        } else {
            u64::from(max.unwrap_or(min.saturating_add(1)).max(1))
        };
        frag.text.extend_from_slice(suffix.as_bytes());
        frag.weight = frag
            .weight
            .saturating_mul(copies)
            .saturating_add(suffix.len() as u64);
        frag.prec = Prec::Quantified;
        frag
    }

    fn never(&self) -> Frag {
        match self.dialect {
            // `^` after a character is an anchor in ERE, so this cannot match.
            Dialect::Ere => Frag::new("x^", Prec::Concat),
            Dialect::Pcre | Dialect::Dotnet => Frag::new("(?!)", Prec::Atom),
        }
    }

    fn widen(&mut self) {
        self.exact = false;
    }

    // -- Characters --------------------------------------------------------

    /// The exact UTF-8 alternatives for a class's non-ASCII part, or `None`
    /// when they would exceed the level's budget.
    fn utf8_exact(&mut self, non_ascii: &[(u32, u32)]) -> Option<Vec<Frag>> {
        if let Some(known) = self.spelled.get(non_ascii) {
            return known.clone();
        }
        let spelled = self.spell_utf8(non_ascii);
        self.spelled.insert(non_ascii.to_vec(), spelled.clone());
        spelled
    }

    fn spell_utf8(&self, non_ascii: &[(u32, u32)]) -> Option<Vec<Frag>> {
        if covers(non_ascii, 0x80, MAX_CHAR) {
            // Any character at all costs some 150 bytes spelled out; only the
            // last levels trade that for the shorter widened form.
            return (self.level.budget > 0)
                .then(|| self.trie(&utf8_sequences(&[(0x80, 0xD7FF), (0xE000, MAX_CHAR)])));
        }
        let cost = |alternatives: &[Frag]| -> usize {
            alternatives
                .iter()
                .map(|frag| match self.dialect {
                    Dialect::Ere => printf_len(&frag.text),
                    _ => frag.text.len(),
                })
                .sum()
        };
        let alternatives = self.trie(&utf8_sequences(non_ascii));
        // Widening a class that is already shorter spelled out gains nothing.
        let spent = cost(&alternatives);
        (spent <= self.level.budget || spent <= cost(&self.coarse())).then_some(alternatives)
    }

    /// The non-ASCII part of a byte-dialect class: spelled out while it fits
    /// the budget, else widened. The flag tells whether the result already
    /// covers the invalid runs the host decodes to U+FFFD.
    fn utf8_part(&mut self, non_ascii: &[(u32, u32)]) -> (Vec<Frag>, bool) {
        match self.utf8_exact(non_ascii) {
            Some(alternatives) => (alternatives, false),
            None => {
                self.widen();
                (
                    self.coarse(),
                    self.dialect == Dialect::Ere && self.level.budget == 0,
                )
            }
        }
    }

    /// A widened non-ASCII part. Matching any multi-byte character by its
    /// lead byte keeps character counts right on valid text. ERE's last
    /// levels settle for any one to four high bytes, which is shorter and also
    /// spans every invalid run; it splits a run of high bytes in many ways,
    /// which costs a DFA nothing but would send PCRE's backtracking past its
    /// limit, so PCRE never uses it.
    fn coarse(&self) -> Vec<Frag> {
        if self.dialect != Dialect::Ere || self.level.budget > 0 {
            return self.trie(&coarse_utf8());
        }
        vec![Frag::new(b"[\x80-\xff]{1,4}".to_vec(), Prec::Quantified)]
    }

    /// UTF-8 spellings as a trie on their leading byte sets, so a byte that
    /// starts none of them fails after one comparison per distinct lead.
    fn trie(&self, sequences: &[ByteSeq]) -> Vec<Frag> {
        let sequences = merge_heads(sequences);
        let mut groups: Vec<(&ByteSet, Vec<&[ByteSet]>)> = Vec::new();
        for sequence in &sequences {
            let Some((head, rest)) = sequence.split_first() else {
                continue;
            };
            match groups.iter_mut().find(|group| group.0 == head) {
                Some(group) => group.1.push(rest),
                None => groups.push((head, vec![rest])),
            }
        }
        groups
            .into_iter()
            .map(|(head, rests)| {
                if let [rest] = rests.as_slice() {
                    let mut chain = Vec::with_capacity(rest.len() + 1);
                    chain.push(head.clone());
                    chain.extend(rest.iter().cloned());
                    return self.byte_seq(&chain);
                }
                // Every spelling under one lead set has the same length.
                let children: Vec<ByteSeq> = rests
                    .iter()
                    .filter(|rest| !rest.is_empty())
                    .map(|rest| rest.to_vec())
                    .collect();
                let head = self.byte_seq(std::slice::from_ref(head));
                if children.is_empty() {
                    return head;
                }
                let children = self.alternation(self.trie(&children));
                self.concat(vec![head, children])
            })
            .collect()
    }

    fn pcre_chars(&mut self, set: &[(u32, u32)], tail: bool) -> Frag {
        let ascii = to_bytes(&intersect(set, 0, 0x7F));
        let non_ascii = intersect(set, 0x80, MAX_CHAR);
        let mut alternatives = Vec::new();
        if !ascii.is_empty() {
            let bracket = self.pcre_bracket(&ascii);
            alternatives.push(if tail && bytes_contain(&ascii, b'\r') {
                // The line's last `\r` is the CRLF terminator, not content.
                self.concat(vec![Frag::new(PCRE_CR_GUARD, Prec::Atom), bracket])
            } else {
                bracket
            });
        }
        if !non_ascii.is_empty() {
            let replacement = contains(set, REPLACEMENT);
            self.define |= replacement;
            if covers(&non_ascii, 0x80, MAX_CHAR) {
                // Every non-ASCII character, U+FFFD's invalid runs included.
                alternatives.push(Frag::new("(?2)", Prec::Atom));
            } else if let Some(spelled) = self.utf8_exact(&non_ascii) {
                alternatives.extend(spelled);
                if replacement {
                    alternatives.push(Frag::new("(?1)", Prec::Atom));
                }
            } else {
                self.widen();
                if replacement {
                    alternatives.push(Frag::new("(?2)", Prec::Atom));
                } else {
                    alternatives.extend(self.coarse());
                }
            }
        }
        self.alternation(alternatives)
    }

    fn ere_chars(&mut self, set: &[(u32, u32)], tail: bool) -> Frag {
        // NUL cannot be written into an argument; grep -I skips the binary
        // files that hold it, as the host skips files with an early NUL.
        let mut bytes = to_bytes(&intersect(set, 0x01, 0x7F));
        let non_ascii = intersect(set, 0x80, MAX_CHAR);
        if tail && contains(set, CR) {
            // ERE has no lookahead to keep the CRLF terminator unconsumed.
            self.widen();
        }
        let mut sequences = Vec::new();
        if !non_ascii.is_empty() {
            let (part, covers_invalid) = self.utf8_part(&non_ascii);
            if contains(set, REPLACEMENT) && !covers_invalid {
                // The host reads each invalid run of one to three bytes as one
                // U+FFFD. Without lookaround those runs can only be
                // over-covered: any single high byte, or a lead byte of a
                // three- or four-byte form with one or two continuations.
                self.widen();
                bytes.push((0x80, 0xFF));
                let cont = vec![(0x80, 0xBF)];
                let mut forms = vec![
                    vec![vec![(0xE0, 0xF4)], cont.clone()],
                    vec![vec![(0xF0, 0xF4)], cont.clone(), cont.clone()],
                ];
                if covers(&non_ascii, 0x80, MAX_CHAR) {
                    // Any character at all: every lead with its continuations.
                    forms = vec![
                        vec![vec![(0xC2, 0xF4)], cont.clone()],
                        vec![vec![(0xE0, 0xF4)], cont.clone(), cont.clone()],
                        vec![vec![(0xF0, 0xF4)], cont.clone(), cont.clone(), cont],
                    ];
                } else {
                    sequences = part;
                }
                sequences.extend(self.trie(&forms));
            } else {
                sequences = part;
            }
        }
        let mut alternatives = Vec::new();
        let bytes = normalize_bytes(bytes);
        if !bytes.is_empty() {
            alternatives.push(self.ere_bracket(&bytes));
        }
        alternatives.extend(sequences);
        if alternatives.is_empty() {
            return self.never();
        }
        self.alternation(alternatives)
    }

    /// The BMP part of a .NET class, spelled out while it fits the budget.
    fn dotnet_bmp_exact(&self, non_ascii: &[(u32, u32)]) -> bool {
        if non_ascii.is_empty() || covers(non_ascii, 0x80, 0xFFFF) {
            return true;
        }
        let mut members = String::new();
        dotnet_members(non_ascii, &mut members);
        // `\u0080-\uD7FF\uE000-\uFFFF` is the widened spelling.
        members.len() <= self.level.budget.max(26)
    }

    /// The astral part of a .NET class as surrogate pairs, or `None` past the budget.
    fn dotnet_astral_exact(&self, astral: &[(u32, u32)]) -> Option<Vec<Frag>> {
        if covers(astral, 0x10000, MAX_CHAR) {
            return Some(vec![Frag::new(DOTNET_ANY_ASTRAL, Prec::Concat)]);
        }
        let pairs: Vec<Frag> = utf16_sequences(astral)
            .iter()
            .map(|(high, lows)| dotnet_pair(*high, lows))
            .collect();
        let cost: usize = pairs.iter().map(|pair| pair.text.len() + 1).sum();
        (cost <= self.level.budget.max(DOTNET_ANY_ASTRAL.len())).then_some(pairs)
    }

    fn dotnet_chars(&mut self, set: &[(u32, u32)]) -> Frag {
        let mut alternatives = Vec::new();
        let bmp = intersect(set, 0, 0xFFFF);
        if !bmp.is_empty() {
            let bmp = if self.dotnet_bmp_exact(&intersect(&bmp, 0x80, 0xFFFF)) {
                bmp
            } else {
                self.widen();
                let mut wide = intersect(&bmp, 0, 0x7F);
                wide.extend([(0x80, 0xD7FF), (0xE000, 0xFFFF)]);
                normalize(wide)
            };
            alternatives.push(dotnet_bracket(&bmp));
        }
        let astral = intersect(set, 0x10000, MAX_CHAR);
        if !astral.is_empty() {
            match self.dotnet_astral_exact(&astral) {
                Some(pairs) => alternatives.extend(pairs),
                None => {
                    self.widen();
                    alternatives.push(Frag::new(DOTNET_ANY_ASTRAL, Prec::Concat));
                }
            }
        }
        self.alternation(alternatives)
    }

    /// A class repeated without an upper bound, as one bracket over bytes (or
    /// UTF-16 units) when its non-ASCII part is all of non-ASCII, exactly or
    /// by widening. Between two character boundaries any run of such units is
    /// a run of whole characters, so counting units instead cannot mislead.
    fn run(&mut self, set: &[(u32, u32)], tail: bool) -> Option<Frag> {
        let non_ascii = intersect(set, 0x80, MAX_CHAR);
        if non_ascii.is_empty() {
            return None;
        }
        let full = covers(&non_ascii, 0x80, MAX_CHAR);
        match self.dialect {
            Dialect::Pcre | Dialect::Ere => {
                if !full {
                    if self.utf8_exact(&non_ascii).is_some() {
                        return None;
                    }
                    self.widen();
                }
                let floor = if self.dialect == Dialect::Ere {
                    0x01
                } else {
                    0x00
                };
                let mut bytes = to_bytes(&intersect(set, floor, 0x7F));
                bytes.push((0x80, 0xFF));
                let bytes = normalize_bytes(bytes);
                if self.dialect == Dialect::Ere {
                    if tail && contains(set, CR) {
                        self.widen();
                    }
                    return Some(self.ere_bracket(&bytes));
                }
                let bracket = self.pcre_bracket(&bytes);
                Some(if tail && contains(set, CR) {
                    let guarded = self.concat(vec![Frag::new(PCRE_CR_GUARD, Prec::Atom), bracket]);
                    self.group(guarded)
                } else {
                    bracket
                })
            }
            Dialect::Dotnet => {
                if !full {
                    if self.dotnet_bmp_exact(&intersect(&non_ascii, 0x80, 0xFFFF)) {
                        return None;
                    }
                    self.widen();
                }
                Some(dotnet_unit_bracket(&intersect(set, 0, 0x7F)))
            }
        }
    }

    fn raw_byte(&mut self, set: &[(u8, u8)]) -> Frag {
        match self.dialect {
            Dialect::Ere => self.ere_bracket(set),
            Dialect::Pcre => self.pcre_bracket(set),
            Dialect::Dotnet => {
                let mut wide: Ranges = set
                    .iter()
                    .filter(|range| range.0 < 0x80)
                    .map(|&(a, b)| (u32::from(a), u32::from(b.min(0x7F))))
                    .collect();
                if set.iter().any(|range| range.1 >= 0x80) {
                    wide.push((0x80, MAX_CHAR));
                }
                let wide = subtract(&subtract(&normalize(wide), 0xD800, 0xDFFF), LF, LF);
                self.dotnet_chars(&wide)
            }
        }
    }

    /// One position of a multi-byte spelling per set, runs of equal sets
    /// counted with `{n}`.
    fn byte_seq(&self, sequence: &[ByteSet]) -> Frag {
        let mut text = Vec::new();
        let mut brackets = 0u64;
        let mut index = 0;
        while index < sequence.len() {
            let mut run = 1;
            while index + run < sequence.len() && sequence[index + run] == sequence[index] {
                run += 1;
            }
            let unit = match self.dialect {
                Dialect::Ere => self.ere_bracket(&sequence[index]),
                _ => self.pcre_bracket(&sequence[index]),
            };
            if unit.text.first() == Some(&b'[') {
                brackets += 1;
            }
            text.extend_from_slice(&unit.text);
            if run > 1 {
                text.extend_from_slice(format!("{{{run}}}").as_bytes());
            }
            index += run;
        }
        let prec = if sequence.len() == 1 {
            Prec::Atom
        } else {
            Prec::Concat
        };
        let weight = text.len() as u64 + 32 * brackets;
        Frag { text, prec, weight }
    }

    // -- Byte brackets -----------------------------------------------------

    /// A PCRE bracket over bytes, positive or negated, whichever is shorter.
    fn pcre_bracket(&self, set: &[(u8, u8)]) -> Frag {
        if let [(start, end)] = set {
            if start == end {
                let mut text = Vec::new();
                pcre_literal(*start, &mut text);
                return Frag::new(text, Prec::Atom);
            }
        }
        let render = |set: &[(u8, u8)], negated: bool| {
            let mut text = vec![b'['];
            if negated {
                text.push(b'^');
            }
            for &(start, end) in set {
                pcre_member(start, &mut text);
                if end > start {
                    if end > start + 1 {
                        text.push(b'-');
                    }
                    pcre_member(end, &mut text);
                }
            }
            text.push(b']');
            text
        };
        let positive = render(set, false);
        let complement = byte_complement(set, &[(0x00, 0xFF)]);
        if complement.is_empty() {
            return Frag::new(positive, Prec::Atom);
        }
        // Never `.`: which bytes end a line for it is a PCRE build option,
        // and some builds (MSYS's among them) leave out `\r` as well.
        let negated = render(&complement, true);
        Frag::new(
            if negated.len() < positive.len() {
                negated
            } else {
                positive
            },
            Prec::Atom,
        )
    }

    /// An ERE bracket over bytes (never `\n` or NUL), positive or negated.
    ///
    /// Brackets take no escapes, so `]` goes first, `-` last, `^` anywhere but
    /// first, and `[` just before those two so it never precedes `.`, `:` or
    /// `=`. Ranges compare byte values in the C locale.
    fn ere_bracket(&self, set: &[(u8, u8)]) -> Frag {
        if let [(start, end)] = set {
            if start == end {
                let mut text = Vec::new();
                ere_literal(*start, &mut text);
                return Frag::new(text, Prec::Atom);
            }
        }
        let universe = [(0x01, 0x09), (0x0B, 0xFF)];
        let complement = byte_complement(set, &universe);
        if complement.is_empty() {
            return Frag::new(".", Prec::Atom);
        }
        let positive = ere_bracket_text(set, false);
        let negated = ere_bracket_text(&complement, true);
        Frag::new(
            if negated.len() < positive.len() {
                negated
            } else {
                positive
            },
            Prec::Atom,
        )
    }

    // -- Assertions --------------------------------------------------------

    fn look(&mut self, look: Look, ctx: Ctx) -> Frag {
        let (text, exact): (&str, bool) = match self.dialect {
            Dialect::Pcre => pcre_look(look),
            Dialect::Dotnet => dotnet_look(look),
            Dialect::Ere => match look {
                // Once `\r?$` has consumed the terminator, `^` would never
                // match where Rust's start anchor does; drop it instead.
                Look::Start | Look::StartLF if !ctx.after_end => ("^", true),
                Look::End | Look::EndLF => ("\r?$", true),
                // ERE has no other assertion; dropping one only widens.
                _ => ("", false),
            },
        };
        // "No ASCII word byte before" also holds between the bytes of one
        // character, and a byte run after it may start there; Rust only ever
        // asks at character boundaries.
        if !exact || (look == Look::WordStartHalfAscii && ctx.follow_run) {
            self.widen();
        }
        if text.is_empty() {
            return Frag::empty();
        }
        Frag::new(text, Prec::Concat)
    }
}

/// The end of a PCRE line, past an optional CRLF `\r`. `$` would do on most
/// builds, but whether it also matches before a final newline, and whether a
/// bare `\r` counts as one, depend on grep's flags and PCRE's build options;
/// "no byte but `\n` follows" means the same everywhere.
const PCRE_END: &str = r"(?=\r?(?![^\n]))";
/// Keeps a class from consuming the CRLF `\r` that ends a line.
const PCRE_CR_GUARD: &str = r"(?!\r(?![^\n]))";

/// PCRE in byte mode, where `\w` and `\b` are ASCII-only (the C locale's tables).
fn pcre_look(look: Look) -> (&'static str, bool) {
    match look {
        Look::Start | Look::StartLF => ("^", true),
        Look::End | Look::EndLF => (PCRE_END, true),
        Look::StartCRLF => ("(?:^|(?<=\\r))", true),
        Look::EndCRLF => ("(?![^\\n\\r])", true),
        Look::WordAscii => ("\\b", true),
        // Exact at character boundaries, but a pattern made only of
        // assertions can also be tried between the bytes of one character.
        Look::WordAsciiNegate => ("\\B", false),
        // Rust's Unicode word set is too large to spell; next to a non-ASCII
        // byte the assertion is simply allowed.
        Look::WordUnicode => ("(?:\\b|(?<=[\\x80-\\xff])|(?=[\\x80-\\xff]))", false),
        Look::WordUnicodeNegate => ("(?:\\B|(?<=[\\x80-\\xff])|(?=[\\x80-\\xff]))", false),
        Look::WordStartAscii => ("\\b(?=\\w)", true),
        Look::WordEndAscii => ("\\b(?<=\\w)", true),
        Look::WordStartUnicode => ("(?<!\\w)(?=[\\w\\x80-\\xff])", false),
        Look::WordEndUnicode => ("(?<=[\\w\\x80-\\xff])(?!\\w)", false),
        Look::WordStartHalfAscii => ("(?<!\\w)", true),
        Look::WordEndHalfAscii => ("(?!\\w)", true),
        Look::WordStartHalfUnicode => ("(?<!\\w)", false),
        Look::WordEndHalfUnicode => ("(?!\\w)", false),
    }
}

/// .NET's `\w` is neither ASCII nor Rust's Unicode set, so ASCII assertions
/// are spelled with explicit lookaround; .NET's `\b` is used only where both
/// sides are ASCII and the non-ASCII cases are allowed anyway.
fn dotnet_look(look: Look) -> (&'static str, bool) {
    match look {
        Look::Start | Look::StartLF => ("^", true),
        Look::End | Look::EndLF => ("$", true),
        Look::StartCRLF => ("(?:^|(?<=\\r))", true),
        Look::EndCRLF => ("(?=\\r|$)", true),
        Look::WordAscii => (
            "(?:(?<=[0-9A-Z_a-z])(?![0-9A-Z_a-z])|(?<![0-9A-Z_a-z])(?=[0-9A-Z_a-z]))",
            true,
        ),
        // Exact at character boundaries; see the PCRE counterpart.
        Look::WordAsciiNegate => (
            "(?:(?<=[0-9A-Z_a-z])(?=[0-9A-Z_a-z])|(?<![0-9A-Z_a-z])(?![0-9A-Z_a-z]))",
            false,
        ),
        Look::WordUnicode => (
            "(?:\\b|(?<=[^\\u0000-\\u007F])|(?=[^\\u0000-\\u007F]))",
            false,
        ),
        Look::WordUnicodeNegate => (
            "(?:\\B|(?<=[^\\u0000-\\u007F])|(?=[^\\u0000-\\u007F]))",
            false,
        ),
        Look::WordStartAscii => ("(?<![0-9A-Z_a-z])(?=[0-9A-Z_a-z])", true),
        Look::WordEndAscii => ("(?<=[0-9A-Z_a-z])(?![0-9A-Z_a-z])", true),
        Look::WordStartUnicode => ("(?<![0-9A-Z_a-z])(?=[0-9A-Z_a-z\\u0080-\\uFFFF])", false),
        Look::WordEndUnicode => ("(?<=[0-9A-Z_a-z\\u0080-\\uFFFF])(?![0-9A-Z_a-z])", false),
        Look::WordStartHalfAscii => ("(?<![0-9A-Z_a-z])", true),
        Look::WordEndHalfAscii => ("(?![0-9A-Z_a-z])", true),
        Look::WordStartHalfUnicode => ("(?<![0-9A-Z_a-z])", false),
        Look::WordEndHalfUnicode => ("(?![0-9A-Z_a-z])", false),
    }
}

/// Any byte run the host's lossy decoding turns into one U+FFFD, as PCRE
/// group 1: each maximal invalid subpart, the same split `from_utf8_lossy`
/// makes. Every branch needs the bytes around it to be ill-formed, so it never
/// matches inside valid UTF-8, and a class holding U+FFFD stays exact. A lone
/// continuation byte is a subpart only when no lead byte within reach still
/// expects it. The leading lookahead lets an ASCII byte fail at once.
const PCRE_INVALID_UTF8: &str = concat!(
    r"(?=[\x80-\xff])(?:[\xc0\xc1\xf5-\xff]",
    r"|[\xc2-\xdf\xe1-\xec\xee\xef\xf1-\xf3](?![\x80-\xbf])",
    r"|\xe0(?![\xa0-\xbf])|\xed(?![\x80-\x9f])|\xf0(?![\x90-\xbf])|\xf4(?![\x80-\x8f])",
    r"|(?:\xe0[\xa0-\xbf]|[\xe1-\xec\xee\xef][\x80-\xbf]|\xed[\x80-\x9f]",
    r"|\xf0[\x90-\xbf]|[\xf1-\xf3][\x80-\xbf]|\xf4[\x80-\x8f])(?![\x80-\xbf])",
    r"|(?:\xf0[\x90-\xbf]|[\xf1-\xf3][\x80-\xbf]|\xf4[\x80-\x8f])[\x80-\xbf](?![\x80-\xbf])",
    r"|(?<!\xe0[\xa0-\xbf]|[\xe1-\xec\xee\xef][\x80-\xbf]|\xed[\x80-\x9f]",
    r"|\xf0[\x90-\xbf]|[\xf1-\xf3][\x80-\xbf]|\xf4[\x80-\x8f])",
    r"(?<!\xf0[\x90-\xbf][\x80-\xbf]|[\xf1-\xf3][\x80-\xbf]{2}|\xf4[\x80-\x8f][\x80-\xbf])",
    r"(?:(?<![\xc2-\xdf\xe1-\xef\xf1-\xf4])[\x80-\x8f]",
    r"|(?<![\xc2-\xdf\xe1-\xf3])[\x90-\x9f]",
    r"|(?<![\xc2-\xec\xee-\xf3])[\xa0-\xbf]))",
);

/// Every non-ASCII character, as PCRE group 2: each well-formed multi-byte
/// spelling, or an invalid run (group 1) that the host reads as U+FFFD.
/// Exactly one length matches wherever it matches, so a repeated class that
/// calls it never gives PCRE two ways to split the same bytes.
const PCRE_ANY_NON_ASCII: &str = concat!(
    r"[\xc2-\xdf][\x80-\xbf]|\xe0[\xa0-\xbf][\x80-\xbf]|[\xe1-\xec\xee\xef][\x80-\xbf]{2}",
    r"|\xed[\x80-\x9f][\x80-\xbf]|\xf0[\x90-\xbf][\x80-\xbf]{2}|[\xf1-\xf3][\x80-\xbf]{3}",
    r"|\xf4[\x80-\x8f][\x80-\xbf]{2}|(?1)",
);

/// The subroutines a PCRE rendering calls as `(?1)` and `(?2)`, appended once;
/// the renderings use no capturing groups of their own, so the numbers hold.
fn pcre_define() -> String {
    format!("(?(DEFINE)({PCRE_INVALID_UTF8})({PCRE_ANY_NON_ASCII}))")
}

const DOTNET_ANY_ASTRAL: &str = r"[\uD800-\uDBFF][\uDC00-\uDFFF]";

// ---------------------------------------------------------------------------
// Escaping.

fn pcre_literal(byte: u8, out: &mut Vec<u8>) {
    match byte {
        b'\t' => out.extend_from_slice(b"\\t"),
        b'\n' => out.extend_from_slice(b"\\n"),
        b'\r' => out.extend_from_slice(b"\\r"),
        b'\\' | b'^' | b'$' | b'.' | b'[' | b']' | b'|' | b'(' | b')' | b'?' | b'*' | b'+'
        | b'{' | b'}' => {
            out.push(b'\\');
            out.push(byte);
        }
        b'\'' => out.extend_from_slice(b"\\x27"),
        0x20..=0x7E => out.push(byte),
        _ => out.extend_from_slice(format!("\\x{byte:02x}").as_bytes()),
    }
}

fn pcre_member(byte: u8, out: &mut Vec<u8>) {
    match byte {
        b'\t' => out.extend_from_slice(b"\\t"),
        b'\n' => out.extend_from_slice(b"\\n"),
        b'\r' => out.extend_from_slice(b"\\r"),
        b'\\' | b']' | b'[' | b'^' | b'-' => {
            out.push(b'\\');
            out.push(byte);
        }
        b'\'' => out.extend_from_slice(b"\\x27"),
        0x20..=0x7E => out.push(byte),
        _ => out.extend_from_slice(format!("\\x{byte:02x}").as_bytes()),
    }
}

fn ere_literal(byte: u8, out: &mut Vec<u8>) {
    if b".[\\()*+?{|^$".contains(&byte) {
        out.push(b'\\');
    }
    out.push(byte);
}

fn ere_bracket_text(set: &[(u8, u8)], negated: bool) -> Vec<u8> {
    let specials = *b"][^-";
    let has = |byte: u8| bytes_contain(set, byte);
    let mut plain: Ranges = set
        .iter()
        .map(|&(a, b)| (u32::from(a), u32::from(b)))
        .collect();
    for special in specials {
        plain = subtract(&plain, u32::from(special), u32::from(special));
    }
    let plain = to_bytes(&plain);
    if !negated && !has(b']') && plain.is_empty() && !has(b'[') && has(b'^') {
        // Only `^` and `-` are left, and a leading `^` would negate.
        return b"[-^]".to_vec();
    }
    let mut text = vec![b'['];
    if negated {
        text.push(b'^');
    }
    if has(b']') {
        text.push(b']');
    }
    for &(start, end) in &plain {
        text.push(start);
        if end > start {
            if end > start + 1 {
                text.push(b'-');
            }
            text.push(end);
        }
    }
    if has(b'[') {
        text.push(b'[');
    }
    if has(b'^') {
        text.push(b'^');
    }
    if has(b'-') {
        text.push(b'-');
    }
    text.push(b']');
    text
}

fn dotnet_unit(unit: u32, out: &mut String) {
    let _ = write!(out, "\\u{unit:04X}");
}

fn dotnet_literal(c: u32, out: &mut String) {
    match c {
        0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x5F => out.push(char::from(c as u8)),
        0x27 => dotnet_unit(c, out),
        // A backslash before any other printable ASCII is a literal in .NET;
        // before a word character it would be an unknown escape.
        0x20..=0x7E => {
            out.push('\\');
            out.push(char::from(c as u8));
        }
        0x10000.. => {
            let v = c - 0x10000;
            dotnet_unit(0xD800 + (v >> 10), out);
            dotnet_unit(0xDC00 + (v & 0x3FF), out);
        }
        _ => dotnet_unit(c, out),
    }
}

fn dotnet_member(c: u32, out: &mut String) {
    match c {
        0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x5F => out.push(char::from(c as u8)),
        // `-[` would start a .NET class subtraction.
        0x5C | 0x5D | 0x5B | 0x5E | 0x2D => {
            out.push('\\');
            out.push(char::from(c as u8));
        }
        0x27 => dotnet_unit(c, out),
        0x20..=0x7E => out.push(char::from(c as u8)),
        _ => dotnet_unit(c, out),
    }
}

fn dotnet_members(set: &[(u32, u32)], out: &mut String) {
    for &(start, end) in set {
        dotnet_member(start, out);
        if end > start {
            if end > start + 1 {
                out.push('-');
            }
            dotnet_member(end, out);
        }
    }
}

/// A .NET bracket over BMP characters (never surrogates), positive or
/// negated. A negated one must also exclude the surrogates, or it would match
/// half of an astral character.
fn dotnet_bracket(set: &[(u32, u32)]) -> Frag {
    if let [(start, end)] = set {
        if start == end {
            let mut text = String::new();
            dotnet_literal(*start, &mut text);
            return Frag::new(text, Prec::Atom);
        }
    }
    let mut positive = String::from("[");
    dotnet_members(set, &mut positive);
    positive.push(']');
    let mut complement: Ranges = vec![(0x00, 0x09), (0x0B, 0xD7FF), (0xE000, 0xFFFF)];
    for &(start, end) in set {
        complement = subtract(&complement, start, end);
    }
    let mut negated = String::from("[^");
    dotnet_members(&complement, &mut negated);
    negated.push_str("\\n\\uD800-\\uDFFF]");
    Frag::new(
        if negated.len() < positive.len() {
            negated
        } else {
            positive
        },
        Prec::Atom,
    )
}

/// A .NET bracket over UTF-16 units: `ascii` plus every non-ASCII unit.
fn dotnet_unit_bracket(ascii: &[(u32, u32)]) -> Frag {
    let mut complement: Ranges = vec![(0x00, 0x09), (0x0B, 0x7F)];
    for &(start, end) in ascii {
        complement = subtract(&complement, start, end);
    }
    if complement.is_empty() {
        return Frag::new(".", Prec::Atom);
    }
    let mut positive = String::from("[");
    dotnet_members(ascii, &mut positive);
    positive.push_str("\\u0080-\\uFFFF]");
    let mut negated = String::from("[^");
    dotnet_members(&complement, &mut negated);
    negated.push_str("\\n]");
    Frag::new(
        if negated.len() < positive.len() {
            negated
        } else {
            positive
        },
        Prec::Atom,
    )
}

fn dotnet_pair(high: (u16, u16), lows: &[(u16, u16)]) -> Frag {
    let mut text = String::new();
    if high.0 == high.1 {
        dotnet_unit(u32::from(high.0), &mut text);
    } else {
        text.push('[');
        dotnet_unit(u32::from(high.0), &mut text);
        text.push('-');
        dotnet_unit(u32::from(high.1), &mut text);
        text.push(']');
    }
    if let [(start, end)] = lows {
        if start == end {
            dotnet_unit(u32::from(*start), &mut text);
            return Frag::new(text, Prec::Concat);
        }
    }
    text.push('[');
    for &(start, end) in lows {
        dotnet_unit(u32::from(start), &mut text);
        if end > start {
            text.push('-');
            dotnet_unit(u32::from(end), &mut text);
        }
    }
    text.push(']');
    Frag::new(text, Prec::Concat)
}

// ---------------------------------------------------------------------------
// ERE word edges.

/// ERE has no assertions besides `^` and `$`, but one at either end of the
/// pattern can still be honoured by consuming the neighbouring byte, which an
/// unanchored search is free to do: `\bfoo` becomes `(^|[^0-9A-Z_a-z])foo`
/// and a trailing `(?mR)$` becomes `(\r?$|\r)`. When the class beside the
/// assertion lies wholly on one side of the word boundary, the assertion may
/// also turn out to always hold or never hold. Elsewhere it is dropped. Rust's
/// Unicode word set is approximated by "ASCII word or any non-ASCII byte", so
/// a Unicode assertion kept this way is no longer exact.
fn ere_edges(node: &Node, exact: &mut bool) -> Node {
    let at_edge = |items: &[Node]| {
        items.first().is_some_and(is_edge_look) || items.last().is_some_and(is_edge_look)
    };
    let wants = match node {
        Node::Seq(items) => at_edge(items),
        Node::Alt(branches) => branches
            .iter()
            .any(|branch| matches!(branch, Node::Seq(items) if at_edge(items))),
        _ => false,
    };
    if !wants {
        return node.clone();
    }
    let Some(unicode_word) = unicode_word_class() else {
        return node.clone();
    };
    let words = Words {
        unicode: unicode_word.clone(),
        ascii: vec![(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)],
    };
    match node {
        Node::Seq(items) => seq(words.edges(items, exact)),
        Node::Alt(branches) => alt(branches
            .iter()
            .map(|branch| match branch {
                Node::Seq(items) => seq(words.edges(items, exact)),
                other => other.clone(),
            })
            .collect()),
        other => other.clone(),
    }
}

fn is_edge_look(node: &Node) -> bool {
    matches!(
        node,
        Node::Look(
            Look::StartCRLF
                | Look::EndCRLF
                | Look::WordAscii
                | Look::WordUnicode
                | Look::WordStartAscii
                | Look::WordEndAscii
                | Look::WordStartUnicode
                | Look::WordEndUnicode
                | Look::WordStartHalfAscii
                | Look::WordEndHalfAscii
                | Look::WordStartHalfUnicode
                | Look::WordEndHalfUnicode
        )
    )
}

/// Rust's Unicode word characters, parsed once from `\w`.
fn unicode_word_class() -> Option<&'static Ranges> {
    static WORD: OnceLock<Option<Ranges>> = OnceLock::new();
    WORD.get_or_init(|| {
        let hir = ParserBuilder::new().build().parse(r"\w").ok()?;
        match hir.kind() {
            HirKind::Class(Class::Unicode(class)) => Some(subtract(
                &normalize(
                    class
                        .ranges()
                        .iter()
                        .map(|range| (u32::from(range.start()), u32::from(range.end())))
                        .collect(),
                ),
                0xD800,
                0xDFFF,
            )),
            _ => None,
        }
    })
    .as_ref()
}

struct Words {
    unicode: Ranges,
    ascii: Ranges,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Word,
    NonWord,
}

/// What becomes of an assertion at the edge of the pattern.
enum Edge {
    /// The neighbouring byte must be on this side of the boundary;
    /// `faithful` when that is all the assertion requires.
    Consume(Side, bool),
    /// The neighbour's class already guarantees the assertion.
    Holds,
    /// The neighbour's class contradicts the assertion.
    Fails,
    Drop,
}

impl Words {
    fn edges(&self, items: &[Node], exact: &mut bool) -> Vec<Node> {
        let mut items = items.to_vec();
        let n = items.len();
        if n < 2 {
            return items;
        }
        if let Node::Look(look) = items[0] {
            let edge = if look == Look::StartCRLF {
                // After a `\r` or at the start: consume the `\r`.
                items[0] = alt(vec![
                    Node::Look(Look::Start),
                    Node::Byte(vec![(b'\r', b'\r')]),
                ]);
                None
            } else {
                Some(self.leading(look, edge_set(&items[1])))
            };
            if let Some(edge) = edge {
                let aligned = aligned(&items[1]);
                items[0] = self.resolve(edge, look, true, aligned, &items[0], exact);
            }
        }
        if let Node::Look(look) = items[n - 1] {
            let edge = if look == Look::EndCRLF {
                // Before a `\r` or at the end: consume the `\r`.
                items[n - 1] = alt(vec![
                    Node::Look(Look::End),
                    Node::Byte(vec![(b'\r', b'\r')]),
                ]);
                None
            } else {
                Some(self.trailing(look, edge_set(&items[n - 2])))
            };
            if let Some(edge) = edge {
                let aligned = aligned(&items[n - 2]);
                items[n - 1] = self.resolve(edge, look, false, aligned, &items[n - 1], exact);
            }
        }
        items
    }

    /// `aligned`: the neighbour can only start (or end) at a character
    /// boundary, so a stand-in byte beside it is a whole neighbouring byte.
    fn resolve(
        &self,
        edge: Edge,
        look: Look,
        leading: bool,
        aligned: bool,
        item: &Node,
        exact: &mut bool,
    ) -> Node {
        match edge {
            Edge::Consume(side, faithful) => {
                // A non-word stand-in also takes a byte from inside a
                // character, which only matters beside a byte run.
                *exact &= faithful && !is_unicode_look(look) && (side == Side::Word || aligned);
                self.stand_in(side, look, leading)
            }
            Edge::Holds => Node::Empty,
            Edge::Fails => Node::Never,
            Edge::Drop => item.clone(),
        }
    }

    fn side_of(&self, set: Option<&Ranges>, unicode: bool) -> Option<Side> {
        let set = set?;
        let words = if unicode { &self.unicode } else { &self.ascii };
        let shared = size(&intersect_sets(set, words));
        if shared == size(set) {
            Some(Side::Word)
        } else if shared == 0 {
            Some(Side::NonWord)
        } else {
            None
        }
    }

    /// An assertion before the first character, whose class is `next`.
    fn leading(&self, look: Look, next: Option<&Ranges>) -> Edge {
        let next = self.side_of(next, is_unicode_look(look));
        match look {
            Look::WordAscii | Look::WordUnicode => match next {
                Some(Side::Word) => Edge::Consume(Side::NonWord, true),
                Some(Side::NonWord) => Edge::Consume(Side::Word, true),
                None => Edge::Drop,
            },
            Look::WordStartAscii | Look::WordStartUnicode => match next {
                Some(Side::NonWord) => Edge::Fails,
                next => Edge::Consume(Side::NonWord, next.is_some()),
            },
            Look::WordEndAscii | Look::WordEndUnicode => match next {
                Some(Side::Word) => Edge::Fails,
                next => Edge::Consume(Side::Word, next.is_some()),
            },
            Look::WordStartHalfAscii | Look::WordStartHalfUnicode => {
                Edge::Consume(Side::NonWord, true)
            }
            Look::WordEndHalfAscii | Look::WordEndHalfUnicode => match next {
                Some(Side::Word) => Edge::Fails,
                Some(Side::NonWord) => Edge::Holds,
                None => Edge::Drop,
            },
            _ => Edge::Drop,
        }
    }

    /// An assertion after the last character, whose class is `previous`.
    fn trailing(&self, look: Look, previous: Option<&Ranges>) -> Edge {
        let previous = self.side_of(previous, is_unicode_look(look));
        match look {
            Look::WordAscii | Look::WordUnicode => match previous {
                Some(Side::Word) => Edge::Consume(Side::NonWord, true),
                Some(Side::NonWord) => Edge::Consume(Side::Word, true),
                None => Edge::Drop,
            },
            Look::WordEndAscii | Look::WordEndUnicode => match previous {
                Some(Side::NonWord) => Edge::Fails,
                previous => Edge::Consume(Side::NonWord, previous.is_some()),
            },
            Look::WordStartAscii | Look::WordStartUnicode => match previous {
                Some(Side::Word) => Edge::Fails,
                previous => Edge::Consume(Side::Word, previous.is_some()),
            },
            Look::WordEndHalfAscii | Look::WordEndHalfUnicode => Edge::Consume(Side::NonWord, true),
            Look::WordStartHalfAscii | Look::WordStartHalfUnicode => match previous {
                Some(Side::Word) => Edge::Fails,
                Some(Side::NonWord) => Edge::Holds,
                None => Edge::Drop,
            },
            _ => Edge::Drop,
        }
    }

    /// A non-word neighbour is the line's edge or any byte outside ASCII
    /// word characters (a non-ASCII character's bytes are all high); a word
    /// neighbour is an ASCII word byte, or for Unicode any high byte.
    fn stand_in(&self, side: Side, look: Look, leading: bool) -> Node {
        let ascii_word: ByteSet = vec![(b'0', b'9'), (b'A', b'Z'), (b'_', b'_'), (b'a', b'z')];
        match side {
            Side::NonWord => {
                let non_word = byte_complement(&ascii_word, &[(0x01, 0x09), (0x0B, 0xFF)]);
                let edge = Node::Look(if leading { Look::Start } else { Look::End });
                alt(vec![edge, Node::Byte(non_word)])
            }
            Side::Word => {
                let mut word = ascii_word;
                if is_unicode_look(look) {
                    word.push((0x80, 0xFF));
                }
                Node::Byte(word)
            }
        }
    }
}

fn is_unicode_look(look: Look) -> bool {
    matches!(
        look,
        Look::WordUnicode
            | Look::WordUnicodeNegate
            | Look::WordStartUnicode
            | Look::WordEndUnicode
            | Look::WordStartHalfUnicode
            | Look::WordEndHalfUnicode
    )
}

/// Whether a node can only begin and end at character boundaries: a class,
/// or an ASCII class repeated (a repeated non-ASCII class may become a run).
fn aligned(node: &Node) -> bool {
    match node {
        Node::Chars(_) => true,
        Node::Repeat { sub, .. } => {
            matches!(sub.as_ref(), Node::Chars(set) if intersect(set, 0x80, MAX_CHAR).is_empty())
        }
        _ => false,
    }
}

/// The characters a node consumes first (or last): a class, repeated or not.
fn edge_set(node: &Node) -> Option<&Ranges> {
    match node {
        Node::Chars(set) => Some(set),
        Node::Repeat { sub, min, .. } if *min >= 1 => match sub.as_ref() {
            Node::Chars(set) => Some(set),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every well-formed multi-byte character, as the PCRE and ERE renderings
    /// spell a class that holds all of non-ASCII.
    const ANY_UTF8: &str = concat!(
        r"[\xc2-\xdf][\x80-\xbf]|\xe0[\xa0-\xbf][\x80-\xbf]|[\xe1-\xec\xee\xef][\x80-\xbf]{2}",
        r"|\xed[\x80-\x9f][\x80-\xbf]|\xf0[\x90-\xbf][\x80-\xbf]{2}|[\xf1-\xf3][\x80-\xbf]{3}",
        r"|\xf4[\x80-\x8f][\x80-\xbf]{2}",
    );
    /// Any multi-byte character by lead byte alone, for widened classes.
    const COARSE_UTF8: &str =
        r"[\xc2-\xdf][\x80-\xbf]|[\xe0-\xef][\x80-\xbf]{2}|[\xf0-\xf4][\x80-\xbf]{3}";
    /// The ERE stand-in for any character at all, U+FFFD's byte runs included.
    const ERE_ANY: &[u8] =
        b"[\xc2-\xf4][\x80-\xbf]|[\xe0-\xf4][\x80-\xbf]{2}|[\xf0-\xf4][\x80-\xbf]{3}";
    const DOTNET_ANY: &str = r"[^\n\uD800-\uDFFF]|[\uD800-\uDBFF][\uDC00-\uDFFF]";

    fn define() -> String {
        pcre_define()
    }

    #[test]
    fn the_any_character_subroutine_is_the_spelled_out_range() {
        let emitter = Emitter {
            dialect: Dialect::Pcre,
            level: LEVELS[0],
            exact: true,
            define: false,
            spelled: HashMap::new(),
        };
        let all = emitter.trie(&utf8_sequences(&[(0x80, 0xD7FF), (0xE000, MAX_CHAR)]));
        let spelled: Vec<String> = all
            .iter()
            .map(|frag| String::from_utf8_lossy(&frag.text).into_owned())
            .collect();
        assert_eq!(spelled.join("|"), ANY_UTF8);
        assert_eq!(PCRE_ANY_NON_ASCII, format!("{ANY_UTF8}|(?1)"));
    }

    #[track_caller]
    fn pin(
        pattern: &str,
        case_sensitive: bool,
        pcre: (&str, bool),
        ere: (&[u8], bool),
        dotnet: (&str, bool),
    ) {
        let remote = translate(pattern, case_sensitive).expect("valid pattern");
        assert_eq!(
            (remote.pcre.as_str(), remote.pcre_exact),
            pcre,
            "pcre for {pattern:?}"
        );
        assert_eq!(
            (
                String::from_utf8_lossy(&remote.ere).as_ref(),
                remote.ere_exact
            ),
            (String::from_utf8_lossy(ere.0).as_ref(), ere.1),
            "ere for {pattern:?}"
        );
        assert_eq!(remote.ere, ere.0, "ere bytes for {pattern:?}");
        assert_eq!(
            (remote.dotnet.as_str(), remote.dotnet_exact),
            dotnet,
            "dotnet for {pattern:?}"
        );
    }

    /// Patterns every dialect spells alike.
    #[track_caller]
    fn same_everywhere(pattern: &str, case_sensitive: bool, rendered: &str) {
        pin(
            pattern,
            case_sensitive,
            (rendered, true),
            (rendered.as_bytes(), true),
            (rendered, true),
        );
    }

    #[test]
    fn plain_patterns_pass_through() {
        same_everywhere("TODO|FIXME", true, "TODO|FIXME");
        same_everywhere("x{2,5}", true, "x{2,5}");
        same_everywhere("[[:alpha:]]", true, "[A-Za-z]");
        same_everywhere("[a-z&&[^aeiou]]", true, "[b-df-hj-np-tv-z]");
        same_everywhere("(?m)^x", true, "^x");
    }

    #[test]
    fn captures_names_and_laziness_drop_out() {
        same_everywhere("(?P<n>ab)", true, "ab");
        // Laziness cannot change whether a line matches.
        same_everywhere("a+?", true, "a+");
        // An empty branch becomes an optional group, which ERE accepts.
        same_everywhere("(?:a|)", true, "a?");
    }

    #[test]
    fn literal_specials_are_escaped_per_dialect() {
        pin(
            r"\.\*\?\+\(\)\[\]\{\}\|\^\$\\",
            true,
            (r"\.\*\?\+\(\)\[\]\{\}\|\^\$\\", true),
            (br"\.\*\?\+\(\)\[]\{}\|\^\$\\", true),
            (r"\.\*\?\+\(\)\[\]\{\}\|\^\$\\", true),
        );
        // ERE takes `}` as it stands; the others escape both braces.
        pin(
            r"interface\{\}",
            true,
            (r"interface\{\}", true),
            (br"interface\{}", true),
            (r"interface\{\}", true),
        );
        // No rendering holds a single quote, so each fits in a quoted word.
        pin("'", true, (r"\x27", true), (b"'", true), (r"\u0027", true));
    }

    #[test]
    fn brackets_keep_their_specials_literal() {
        pin(
            r"[]\[^-]",
            true,
            (r"[\-\[\]\^]", true),
            (b"[][^-]", true),
            (r"[\-\[\]\^]", true),
        );
        // A leading `^` would negate an ERE bracket.
        pin(
            "[-^]",
            true,
            (r"[\-\^]", true),
            (b"[-^]", true),
            (r"[\-\^]", true),
        );
    }

    #[test]
    fn case_insensitivity_is_folded_into_classes() {
        same_everywhere("(?i)abc", true, "[Aa][Bb][Cc]");
        pin(
            "Straße",
            false,
            (
                r"(?:[Ss]|\xc5\xbf)[Tt][Rr][Aa](?:\xc3\x9f|\xe1\xba\x9e)[Ee]",
                true,
            ),
            (
                b"([Ss]|\xc5\xbf)[Tt][Rr][Aa](\xc3\x9f|\xe1\xba\x9e)[Ee]",
                true,
            ),
            (r"[Ss\u017F][Tt][Rr][Aa][\u00DF\u1E9E][Ee]", true),
        );
        pin(
            "ǅ",
            false,
            (r"\xc7[\x84-\x86]", true),
            (b"\xc7[\x84-\x86]", true),
            (r"[\u01C4-\u01C6]", true),
        );
        // Unicode folding reaches the Kelvin sign.
        pin(
            "k",
            false,
            (r"[Kk]|\xe2\x84\xaa", true),
            (b"[Kk]|\xe2\x84\xaa", true),
            (r"[Kk\u212A]", true),
        );
    }

    #[test]
    fn any_character_is_spelled_per_encoding() {
        let pcre = format!(r"(?:(?!\r(?![^\n]))[^\n\x80-\xff]|(?2)){}", define());
        pin(
            ".",
            true,
            (&pcre, true),
            (&concat_bytes(&[b".|", ERE_ANY]), false),
            (DOTNET_ANY, true),
        );
        let pcre = format!(r"a(?:[^\n\x80-\xff]|(?2))b*c{}", define());
        pin(
            "a.b*c",
            true,
            (&pcre, true),
            (&concat_bytes(&[b"a(.|", ERE_ANY, b")b*c"]), false),
            (&format!("a(?:{DOTNET_ANY})b*c"), true),
        );
        // A run between two characters is a run of whole characters.
        pin(
            "foo.*bar",
            true,
            (r"foo[^\n]*bar", true),
            (b"foo.*bar", true),
            ("foo.*bar", true),
        );
        // Side by side, two runs could split one character between them.
        pin(
            ".+.+",
            true,
            (
                &format!(
                    r"(?:[^\n\x80-\xff]|(?2))+(?:(?!\r(?![^\n]))[^\n])+{}",
                    define()
                ),
                true,
            ),
            (&concat_bytes(&[b"(.|", ERE_ANY, b")+.+"]), false),
            (&format!("(?:{DOTNET_ANY})+.+"), true),
        );
    }

    #[test]
    fn negated_classes_keep_non_ascii() {
        let pcre = format!(r"(?:(?!\r(?![^\n]))[^\na-z\x80-\xff]|(?2)){}", define());
        pin(
            "[^a-z]",
            true,
            (&pcre, true),
            (&concat_bytes(&[b"[^a-z]|", ERE_ANY]), false),
            (
                r"[^a-z\n\uD800-\uDFFF]|[\uD800-\uDBFF][\uDC00-\uDFFF]",
                true,
            ),
        );
        // A class that holds U+FFFD also takes the invalid bytes the host
        // decodes to it; in .NET the decoder has already made it U+FFFD.
        pin(
            r"\x{FFFD}",
            true,
            (&format!(r"(?:\xef\xbf\xbd|(?1)){}", define()), true),
            (
                b"[\x80-\xff]|\xef\xbf\xbd|[\xe0-\xf4][\x80-\xbf]|[\xf0-\xf4][\x80-\xbf]{2}",
                false,
            ),
            (r"\uFFFD", true),
        );
    }

    #[test]
    fn perl_classes_are_unicode_aware() {
        pin(
            r"\s",
            true,
            (
                concat!(
                    r"(?!\r(?![^\n]))[\t\x0b-\r ]|\xc2[\x85\xa0]|\xe1\x9a\x80",
                    r"|\xe2(?:\x80[\x80-\x8a\xa8\xa9\xaf]|\x81\x9f)|\xe3\x80{2}",
                ),
                true,
            ),
            (
                b"[\t\x0b-\r ]|\xc2[\x85\xa0]|\xe1\x9a\x80|\xe2(\x80[\x80-\x8a\xa8\xa9\xaf]|\x81\x9f)|\xe3\x80{2}",
                false,
            ),
            (
                r"[\u0009\u000B-\u000D \u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]",
                true,
            ),
        );
        // Unicode `\w` is far too large to spell out, so its non-ASCII part
        // widens to every non-ASCII byte.
        pin(
            r"\w+",
            true,
            (r"[0-9A-Z_a-z\x80-\xff]+", false),
            (b"[0-9A-Z_a-z\x80-\xff]+", false),
            (r"[0-9A-Z_a-z\u0080-\uFFFF]+", false),
        );
        let digits = translate(r"\d+", true).unwrap();
        assert!(digits.pcre_exact && digits.ere_exact && digits.dotnet_exact);
        assert!(digits
            .pcre
            .starts_with(r"(?:[0-9]|\xd9[\xa0-\xa9]|\xdb[\xb0-\xb9]|"));
        assert!(digits.pcre.ends_with(r"|\x97[\xb1-\xba])))+"));
        assert!(digits
            .ere
            .starts_with(b"([0-9]|\xd9[\xa0-\xa9]|\xdb[\xb0-\xb9]|"));
        assert!(digits.ere.ends_with(b"|\x97[\xb1-\xba])))+"));
        assert!(digits
            .dotnet
            .starts_with(r"(?:[0-9\u0660-\u0669\u06F0-\u06F9"));
        assert!(digits.dotnet.ends_with(r"|\uD83E[\uDFF0-\uDFF9])+"));
    }

    #[test]
    fn script_classes_are_spelled_out_while_small() {
        pin(
            r"\p{Greek}",
            true,
            (
                concat!(
                    r"\xcd[\xb0-\xb3\xb5-\xb7\xba-\xbd\xbf]|\xce[\x84\x86\x88-\x8a\x8c\x8e-\xa1",
                    r"\xa3-\xbf]|\xcf[\x80-\xa1\xb0-\xbf]|\xe1(?:\xb4[\xa6-\xaa]|\xb5[\x9d-\xa1",
                    r"\xa6-\xaa]|\xb6\xbf|\xbc[\x80-\x95\x98-\x9d\xa0-\xbf]|\xbd[\x80-\x85\x88",
                    r"-\x8d\x90-\x97\x99\x9b\x9d\x9f-\xbd]|\xbe[\x80-\xb4\xb6-\xbf]|\xbf[^\x00",
                    r"-\x7f\x85\x94\x95\x9c\xb0\xb1\xb5\xbf-\xff])|\xe2\x84\xa6|\xea\xad\xa5|",
                    r"\xf0(?:\x90(?:\x85[\x80-\xbf]|\x86[\x80-\x8e\xa0])|\x9d(?:\x88[\x80-\xbf]",
                    r"|\x89[\x80-\x85]))",
                ),
                true,
            ),
            (
                concat_bytes(&[
                    b"\xcd[\xb0-\xb3\xb5-\xb7\xba-\xbd\xbf]|\xce[\x84\x86\x88-\x8a\x8c\x8e-\xa1",
                    b"\xa3-\xbf]|\xcf[\x80-\xa1\xb0-\xbf]|\xe1(\xb4[\xa6-\xaa]|\xb5[\x9d-\xa1",
                    b"\xa6-\xaa]|\xb6\xbf|\xbc[\x80-\x95\x98-\x9d\xa0-\xbf]|\xbd[\x80-\x85\x88",
                    b"-\x8d\x90-\x97\x99\x9b\x9d\x9f-\xbd]|\xbe[\x80-\xb4\xb6-\xbf]|\xbf[\x80",
                    b"-\x84\x86-\x93\x96-\x9b\x9d-\xaf\xb2-\xb4\xb6-\xbe])|\xe2\x84\xa6|\xea",
                    b"\xad\xa5|\xf0(\x90(\x85[\x80-\xbf]|\x86[\x80-\x8e\xa0])|\x9d(\x88[\x80-",
                    b"\xbf]|\x89[\x80-\x85]))",
                ])
                .as_slice(),
                true,
            ),
            (
                concat!(
                    r"[\u0370-\u0373\u0375-\u0377\u037A-\u037D\u037F\u0384\u0386\u0388-\u038A",
                    r"\u038C\u038E-\u03A1\u03A3-\u03E1\u03F0-\u03FF\u1D26-\u1D2A\u1D5D-\u1D61",
                    r"\u1D66-\u1D6A\u1DBF\u1F00-\u1F15\u1F18-\u1F1D\u1F20-\u1F45\u1F48-\u1F4D",
                    r"\u1F50-\u1F57\u1F59\u1F5B\u1F5D\u1F5F-\u1F7D\u1F80-\u1FB4\u1FB6-\u1FC4",
                    r"\u1FC6-\u1FD3\u1FD6-\u1FDB\u1FDD-\u1FEF\u1FF2-\u1FF4\u1FF6-\u1FFE\u2126",
                    r"\uAB65]|\uD800[\uDD40-\uDD8E\uDDA0]|\uD834[\uDE00-\uDE45]",
                ),
                true,
            ),
        );
        let han = translate(r"\p{Han}+", true).unwrap();
        assert!(han.pcre_exact && han.ere_exact && han.dotnet_exact);
        assert!(han
            .pcre
            .starts_with(r"(?:\xe2(?:\xba[\x80-\x99\x9b-\xbf]|\xbb[\x80-\xb3]|"));
        assert!(han
            .pcre
            .ends_with(r"\xb2(?:[\x80-\x8d][\x80-\xbf]|\x8e[\x80-\xaf])))+"));
        assert!(han
            .dotnet
            .starts_with(r"(?:[\u2E80-\u2E99\u2E9B-\u2EF3\u2F00-\u2FD5\u3005"));
        assert!(han.dotnet.ends_with(r"|\uD888[\uDC00-\uDFAF])+"));
    }

    #[test]
    fn astral_characters_become_byte_runs_or_surrogate_pairs() {
        pin(
            r"\x{1F600}",
            true,
            (r"\xf0\x9f\x98\x80", true),
            (b"\xf0\x9f\x98\x80", true),
            (r"\uD83D\uDE00", true),
        );
        pin(
            "[😀-🙏]",
            true,
            (r"\xf0\x9f(?:\x98[\x80-\xbf]|\x99[\x80-\x8f])", true),
            (b"\xf0\x9f(\x98[\x80-\xbf]|\x99[\x80-\x8f])", true),
            (r"\uD83D[\uDE00-\uDE4F]", true),
        );
    }

    #[test]
    fn counted_repetition_respects_each_engine_limit() {
        // POSIX only promises counts up to 255.
        pin(
            "x{300}",
            true,
            ("x{300}", true),
            (b"x{255,}", false),
            ("x{300}", true),
        );
    }

    #[test]
    fn end_anchors_allow_the_crlf_terminator() {
        pin(
            "foo$",
            true,
            (r"foo(?=\r?(?![^\n]))", true),
            (b"foo\r?$", true),
            ("foo$", true),
        );
        pin(
            r"\Afoo\z",
            true,
            (r"^foo(?=\r?(?![^\n]))", true),
            (b"^foo\r?$", true),
            ("^foo$", true),
        );
        // A class must not take the terminator itself: PCRE says so with a
        // lookahead, ERE cannot.
        pin(
            r"^\s*$",
            true,
            (
                concat!(
                    r"^(?:(?!\r(?![^\n]))[\t\x0b-\r ]|\xc2[\x85\xa0]|\xe1\x9a\x80",
                    r"|\xe2(?:\x80[\x80-\x8a\xa8\xa9\xaf]|\x81\x9f)|\xe3\x80{2})*",
                    r"(?=\r?(?![^\n]))",
                ),
                true,
            ),
            (
                b"^([\t\x0b-\r ]|\xc2[\x85\xa0]|\xe1\x9a\x80|\xe2(\x80[\x80-\x8a\xa8\xa9\xaf]|\x81\x9f)|\xe3\x80{2})*\r?$",
                false,
            ),
            (
                r"^[\u0009\u000B-\u000D \u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]*$",
                true,
            ),
        );
        pin(
            "(?mR)^a$",
            true,
            (r"(?:^|(?<=\r))a(?![^\n\r])", true),
            (b"(^|\r)a(\r?$|\r)", true),
            (r"(?:^|(?<=\r))a(?=\r|$)", true),
        );
    }

    #[test]
    fn word_boundaries_are_exact_for_ascii_and_widened_for_unicode() {
        pin(
            r"\bfoo\b",
            true,
            (
                r"(?:\b|(?<=[\x80-\xff])|(?=[\x80-\xff]))foo(?:\b|(?<=[\x80-\xff])|(?=[\x80-\xff]))",
                false,
            ),
            (b"(^|[^0-9A-Z_a-z])foo(\r?$|[^0-9A-Z_a-z])", false),
            (
                concat!(
                    r"(?:\b|(?<=[^\u0000-\u007F])|(?=[^\u0000-\u007F]))foo",
                    r"(?:\b|(?<=[^\u0000-\u007F])|(?=[^\u0000-\u007F]))",
                ),
                false,
            ),
        );
        pin(
            r"(?-u:\b)foo(?-u:\b)",
            true,
            (r"\bfoo\b", true),
            (b"(^|[^0-9A-Z_a-z])foo(\r?$|[^0-9A-Z_a-z])", true),
            (
                concat!(
                    r"(?:(?<=[0-9A-Z_a-z])(?![0-9A-Z_a-z])|(?<![0-9A-Z_a-z])(?=[0-9A-Z_a-z]))foo",
                    r"(?:(?<=[0-9A-Z_a-z])(?![0-9A-Z_a-z])|(?<![0-9A-Z_a-z])(?=[0-9A-Z_a-z]))",
                ),
                true,
            ),
        );
        pin(
            r"\<w",
            true,
            (r"(?<!\w)(?=[\w\x80-\xff])w", false),
            (b"(^|[^0-9A-Z_a-z])w", false),
            (r"(?<![0-9A-Z_a-z])(?=[0-9A-Z_a-z\u0080-\uFFFF])w", false),
        );
        // A start of word before a space can never hold.
        let never = translate(r"\<\s", true).unwrap();
        assert_eq!(
            (never.ere.as_slice(), never.ere_exact),
            (b"x^".as_slice(), true)
        );
        // "No ASCII word byte before" also holds inside a character, where a
        // byte run after it could start, and "none after" where a run before
        // it could stop, so the run before it stays character by character.
        pin(
            r"(?-u:\b{start-half})[^a]+x",
            true,
            (r"(?<!\w)[^\na]+x", false),
            (b"(^|[^0-9A-Z_a-z])[^a]+x", false),
            (r"(?<![0-9A-Z_a-z])[^a\n]+x", false),
        );
        let stop = translate(r"[^b]+(?-u:\b{end-half})", true).unwrap();
        assert!(stop.pcre_exact && stop.dotnet_exact);
        assert!(stop
            .pcre
            .starts_with(r"(?:(?!\r(?![^\n]))[^\nb\x80-\xff]|(?2))+(?!\w)"));
        assert_eq!(
            stop.dotnet,
            r"(?:[^b\n\uD800-\uDFFF]|[\uD800-\uDBFF][\uDC00-\uDFFF])+(?![0-9A-Z_a-z])"
        );
        // Away from the pattern's edges ERE has no way to say it.
        pin(
            r"a\bb",
            true,
            (r"a(?:\b|(?<=[\x80-\xff])|(?=[\x80-\xff]))b", false),
            (b"ab", false),
            (
                r"a(?:\b|(?<=[^\u0000-\u007F])|(?=[^\u0000-\u007F]))b",
                false,
            ),
        );
    }

    #[test]
    fn impossible_patterns_never_match() {
        for pattern in [r"[^\x00-\x{10FFFF}]", r"a\nb"] {
            pin(pattern, true, ("(?!)", true), (b"x^", true), ("(?!)", true));
        }
    }

    #[test]
    fn errors_are_the_local_legs_errors() {
        for pattern in ["(", "a{", r"\p{Nope}", "[z-a]", r"(?-u:\xff)"] {
            let local = RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .unwrap_err();
            let remote = translate(pattern, false).err().expect("rejected");
            assert_eq!(remote.to_string(), local.to_string(), "{pattern:?}");
        }
    }

    #[test]
    fn the_host_matcher_is_the_local_legs_matcher() {
        let remote = translate("STRASSE|ẞ", false).unwrap();
        assert!(remote.regex.is_match("strasse"));
        assert!(remote.regex.is_match("ß"));
        assert!(!translate("abc", true).unwrap().regex.is_match("ABC"));
    }

    #[test]
    fn printf_argument_escapes_everything_but_letters_and_digits() {
        assert_eq!(
            ere_printf_argument(b"a1%\\'\"$\xe9\r Z"),
            r"'a1\045\134\047\042\044\351\015\040Z'"
        );
    }

    #[test]
    fn oversized_renderings_fall_back_to_a_prefix() {
        // 2500 two-byte characters do not fit 16 KiB as `\xHH` pairs (nor as
        // printf octal), so only a prefix of the literal is searched for.
        let long = "é".repeat(2500);
        let remote = translate(&long, true).unwrap();
        assert!(!remote.pcre_exact && !remote.ere_exact);
        assert!(remote.pcre.len() <= MAX_TEXT && remote.pcre.len() > MAX_TEXT / 2);
        assert_eq!(remote.pcre, r"\xc3\xa9".repeat(remote.pcre.len() / 8));
        assert!(printf_len(&remote.ere) <= MAX_TEXT);
        assert_eq!(remote.ere, "é".repeat(remote.ere.len() / 2).into_bytes());
        // .NET spells the same text in half the room.
        assert_eq!(remote.dotnet, r"\u00E9".repeat(2500));
        assert!(remote.dotnet_exact);
    }

    #[test]
    fn oversized_counts_and_classes_widen() {
        // Copied out 1000 times, PCRE's compiled pattern would overflow.
        let remote = translate(".{1000}", true).unwrap();
        assert_eq!(
            (remote.pcre.as_str(), remote.pcre_exact),
            (r"(?:(?!\r(?![^\n]))[^\n])+", false)
        );
        assert_eq!(remote.dotnet, format!("(?:{DOTNET_ANY}){{1000}}"));
        assert!(remote.dotnet_exact);
        // Many large classes give up their spelled-out non-ASCII parts.
        let remote = translate(&r"\p{Lu}\p{Ll}".repeat(8), true).unwrap();
        assert!(!remote.pcre_exact && !remote.dotnet_exact);
        assert!(remote.pcre.contains(COARSE_UTF8));
        for remote in [
            translate(&r"[\p{Greek}\p{Cyrillic}\p{Han}]{2}".repeat(40), false).unwrap(),
            translate(&r"\b\w+\b-".repeat(100), true).unwrap(),
            translate(&"(?:x{255}){40}".repeat(3), true).unwrap(),
        ] {
            assert!(remote.pcre.len() <= MAX_TEXT);
            assert!(printf_len(&remote.ere) <= MAX_TEXT);
            assert!(remote.dotnet.len() <= MAX_TEXT);
        }
    }

    fn concat_bytes(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// Runs every rendering through the engine it is written for and checks
    /// that each accepts every line the host's regex accepts, and nothing
    /// more when it claims to be exact.
    mod differential {
        use std::collections::BTreeSet;
        use std::fmt::Write as _;
        use std::fs;
        use std::path::Path;
        use std::process::Command;

        use regex::Regex;

        use super::super::{ere_printf_argument, translate, translate_from, RemotePattern, LEVELS};

        /// Lines a remote grep meets: code, punctuation, several scripts,
        /// astral characters, CRLF endings (the trailing `\r` here), tabs,
        /// empty lines, controls and invalid UTF-8.
        const CORPUS: &[&[u8]] = &[
            b"fn main() {",
            b"    let x = foo_bar(42);",
            b"interface{} TODO: FIXME later",
            b"if (a+b) * c == d? {e} [f] |g| ^h$ \\i",
            b"#include <stdio.h>",
            b"price: $100.50 (approx.)",
            b"foobar foo bar barfoo",
            b"a+?b x{2,5} xxxxx x xx",
            "Stra\u{df}e STRASSE strasse stra\u{df}e \u{1e9e}".as_bytes(),
            "\u{1c5}ungla \u{1c6} \u{1c4}".as_bytes(),
            "caf\u{e9} na\u{ef}ve r\u{e9}sum\u{e9} \u{dc}n\u{ef}c\u{f6}d\u{e9}".as_bytes(),
            "\u{3a9}\u{3bc}\u{3ad}\u{3b3}\u{3b1} \u{3b1}\u{3bb}\u{3c6}\u{3b1} \u{391}\u{39b}\u{3a6}\u{391}".as_bytes(),
            "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{43c}\u{438}\u{440}".as_bytes(),
            "\u{4e2d}\u{6587}\u{5b57}\u{7b26} \u{6f22}\u{5b57} \u{65e5}\u{672c}\u{8a9e}".as_bytes(),
            "emoji \u{1f600} test \u{1f389}\u{1f389}".as_bytes(),
            "mixed: abc\u{4e2d}\u{6587}def 123".as_bytes(),
            b"tab\tseparated\tvalues",
            b"trailing spaces   ",
            b"",
            b"   ",
            b"\t",
            b"CRLF line\r",
            b"another one\r",
            b" \r",
            b"\r",
            b"x\t\r",
            "kelvin \u{212a} sign and long \u{17f}".as_bytes(),
            "digits \u{660}\u{661}\u{662} and \u{ff10}\u{ff11} fullwidth".as_bytes(),
            "zero\u{200d}width joiner".as_bytes(),
            b"under_score snake_case CamelCase",
            b"x",
            b"xx",
            b"xxx",
            b"TODO",
            b"todo",
            b"a",
            b"ab",
            b"aaa",
            "\u{e9}".as_bytes(),
            "\u{df}".as_bytes(),
            "\u{1f600}".as_bytes(),
            "\u{fffd} replacement".as_bytes(),
            b"invalid \xff byte",
            b"latin1 caf\xe9 here",
            b"\xe9t\xe9",
            b"truncated \xe2\x82 end",
            b"truncated4 \xf0\x9f\x98",
            b"lone \x80\x80 conts",
            b"overlong \xc0\xaf \xe0\x80\xaf",
            b"surrogate \xed\xa0\x80 cesu",
            b"tail lead \xc3",
            b"\xc3\xa9\xa9 extra cont",
            b"control \x01\x02 chars",
            b"del \x7f end",
            b"   leading spaces",
            b"a\rb mid CR",
            "word-boundary\u{2014}em dash".as_bytes(),
            b"quote ' and \" and ` backtick",
            b"percent % and backslash \\ and caret ^",
            b"1234-56-78",
            b"2024-01-02",
            b"$HOME/path/to/file.txt",
            b"ends with dollar$",
            b"<w> \\<w word",
            "\u{3b1}\u{3b2}\u{3b3} abc".as_bytes(),
            b"[]-^ :=. [.x.] [:a:]",
            "\u{2163} roman \u{24b6} circled".as_bytes(),
            "a\u{301} combining".as_bytes(),
            "\u{3000}ideographic space\u{3000}".as_bytes(),
            "nbsp\u{a0}here\u{a0}".as_bytes(),
            "a\u{e9}x".as_bytes(),
            "\u{e9}b".as_bytes(),
            "a\u{1f600}x \u{4e2d}b".as_bytes(),
        ];

        /// Hand-picked patterns aimed at each translation rule.
        const PATTERNS: &[&str] = &[
            r"a.b*c",
            r"\d+",
            r"\w+",
            r"\s",
            r"\bfoo\b",
            r"(?i)abc",
            "Straße",
            "ǅ",
            r"[^a-z]",
            r".",
            r"a+?",
            r"x{2,5}",
            r"x{300}",
            r"interface\{\}",
            r"TODO|FIXME",
            r"(?:a|)",
            r"^\s*$",
            r"\p{Greek}",
            r"\p{Han}+",
            r"[[:alpha:]]",
            r"[a-z&&[^aeiou]]",
            r"(?P<n>ab)",
            r"\x{1F600}",
            r"foo$",
            r"\Afoo\z",
            r"(?m)^x",
            r"\<w",
            r"[^\x00-\x{10FFFF}]",
            r"\d",
            r"k",
            r"foo.*bar",
            r"\W",
            r"\S+",
            r"\x{FFFD}",
            r"[-^]",
            r"[]\[^-]",
            r"'",
            r"a\nb",
            r"(?-u:\b)x(?-u:\B)",
            r"\b{start-half}x\b{end-half}",
            r"[😀-🙏]",
            r"\pL",
            r"\p{Lu}",
            r"\s+$",
            r"a|^b$|\bc",
            r"\s$",
            r"\r",
            r"\r$",
            r"a\rb",
            r"^$",
            r"$^",
            r"(?m)$^",
            r"^",
            r"$",
            r"x*",
            r"(?:)",
            r"\b",
            r"\B",
            r"(?-u:\B)",
            r"(?-u:\b)",
            r"\b{start}",
            r"\b{end}",
            r"(?mR)^b",
            r"(?mR)a$",
            r"(?mR)^",
            r"(?s).",
            r".{3}",
            r"^.{1,2}$",
            r"^.$",
            r"^..$",
            r"^...$",
            r"\x{FFFD}{2}",
            r"[^a]{3}",
            r"\W\W",
            r"é+",
            r"(?i)É",
            r"(?i)ß",
            r"(?i)K",
            r"(?i)ſ",
            r"\p{Greek}+",
            r"[α-ω]+",
            r"\p{Cyrillic}{2,}",
            r"[一-龥]",
            r"\p{Han}{2}",
            "😀",
            r"[😀🎉]",
            r"\p{Emoji}",
            r"(?:😀|🎉){2}",
            r".😀",
            r"\d{4}-\d{2}-\d{2}",
            r"\$\d+\.\d+",
            r"^\s+",
            r"\t",
            r"[[:punct:]]+",
            r"[[:^alpha:]]",
            r"[\x00-\x1f]",
            r"\x7f",
            r"[^\x00-\x7f]+",
            r"[\x{80}-\x{10FFFF}]",
            r"\\",
            r"'",
            "\"",
            r"`",
            r"%",
            r"\^",
            r"\$",
            r"\{\}",
            r"a{0}",
            r"a{0,0}b",
            r"(a|b|)c",
            r"(|a)b",
            r"((a))*",
            r"(?:^|x)y",
            r"(?:x|$)",
            r"a$|^b",
            r"\Aa",
            r"a\z",
            r"(?:\b)+x",
            r"\bTODO\b",
            r"\bfoo",
            r"foo\b",
            r"-\b",
            r"\b-",
            r"\Bo\B",
            r"\<f",
            r"o\>",
            r"\bé",
            r"é\b",
            r"\b中",
            r"(?i)\bstrasse\b",
            r"[ab-]",
            r"[]a]",
            r"[\[\]]",
            r"[\^]",
            r"[a^]",
            r"[-a]",
            r"[.]",
            r"[:]",
            r"[=]",
            r"[\[.]",
            r"[\[:]",
            r"[\[=]",
            r"[\[.x]",
            r"x{255}",
            r"x{256}",
            r"(?:ab){300}",
            r"x{2,300}",
            r"\w+\s*=\s*\w+",
            r"^\s*#",
            r"[ \t]+$",
            r"\.txt$",
            r"/path/",
            r"\(a\+b\)",
            r"\|g\|",
            r"\[f\]",
            r"<stdio\.h>",
            r"\bx{2}\b",
            r"(?i)todo",
            r"(?i)ǆ",
            r"(?i)Ω",
            r"(?i)ω",
            r"(?i)[a-z]+é",
            r"\p{Lu}\p{Ll}+",
            r"\pN+",
            r"\D+",
            r"\S\s\S",
            r"[\s\S]",
            r"(?s:.)+",
            r"(?-u:\w)+",
            r"(?-u:\s)",
            r"(?-u:[^a])",
            r"(?u:\w)+",
            r"\x01",
            r"[\x01-\x02]+",
            r"\x{2014}",
            r"\p{Pd}",
            r"\p{Zs}",
            r"\p{Mn}",
            r"\u{301}",
            r"a\u{301}",
            r"\x{3000}+",
            r"\x{a0}",
            r"\b\w+\b",
            r"\B\w+\B",
            r"\w\b\W",
            r"\W\b\w",
            r"(?-u:\W)+",
            r"[^\w\s]+",
            r"^[^\s]+$",
            r"(?:a|b)*?c",
            r"x??y",
            r"(?U)a+",
            r"(?x) a b # comment",
            r"[\p{Greek}\p{Cyrillic}]+",
            r"[^\p{L}\s]+",
            r"\p{Latin}+",
            r"\P{Latin}+",
            r"\b{start-half}\w",
            r"\w\b{end-half}",
            r"\b{end}x",
            r"x\b{start}",
            r"(?-u:\b{start-half})[^a]+x",
            r"[^b]+(?-u:\b{end-half})",
            r"(?-u:\b{start-half})[^a]",
            r"(?-u:\b)[^b]+(?-u:\b)",
            r"(?-u:\b{end-half})[^b]+x",
            r"[^x]+(?-u:\b{start-half})b",
            r"(?-u:\B)[^a]+",
            r"[^a]+(?-u:\B)b",
        ];

        /// Classes, atoms and escapes the random generator draws from.
        const ATOMS: &[&str] = &[
            "a",
            "b",
            "x",
            "o",
            "T",
            "f",
            "1",
            "2",
            " ",
            "-",
            ".",
            "_",
            ":",
            r"\$",
            r"\.",
            r"\(",
            r"\)",
            r"\[",
            r"\{",
            r"\}",
            r"\|",
            r"\\",
            r"\^",
            r"\*",
            "'",
            "%",
            "é",
            "ß",
            "Ω",
            "ж",
            "中",
            "😀",
            "\u{fffd}",
            r"\t",
            r"\r",
            r"[a-z]",
            r"[^a-z]",
            r"[A-Fa-f0-9]",
            r"[[:alpha:]]",
            r"[[:punct:]]",
            r"[^[:space:]]",
            r"[é-ü]",
            r"[α-ω]",
            r"[^\s]",
            r"\d",
            r"\D",
            r"\w",
            r"\W",
            r"\s",
            r"\S",
            ".",
            r"\pL",
            r"\p{Greek}",
            r"\p{Han}",
            r"\P{L}",
            r"\p{Lu}",
            r"\pN",
            r"[\x{1F600}-\x{1F64F}]",
            r"[^\x00-\x7F]",
            r"[\t ]",
            r"\x{FFFD}",
            r"[^a]",
            r"(?s:.)",
            r"(?-u:\w)",
            r"[a-z&&[^aeiou]]",
            r"[\r\n]",
            r"[]]",
            r"[-]",
            r"[\^x]",
            r"[\[.]",
            r"(?i:k)",
            r"(?i:s)",
            r"(?i:ß)",
        ];

        const LOOKS: &[&str] = &[
            "^",
            "$",
            r"\A",
            r"\z",
            r"\b",
            r"\B",
            r"\<",
            r"\>",
            r"\b{start-half}",
            r"\b{end-half}",
            r"(?-u:\b)",
            r"(?-u:\B)",
            "(?m:^)",
            "(?m:$)",
            "(?mR:^)",
            "(?mR:$)",
        ];

        const QUANTIFIERS: &[&str] = &[
            "*", "+", "?", "{2}", "{1,3}", "{0,2}", "{2,}", "*?", "+?", "??", "{1,2}?", "{3}",
        ];

        struct Rng(u64);

        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            }

            fn below(&mut self, n: usize) -> usize {
                (self.next() % n as u64) as usize
            }

            fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
                items[self.below(items.len())]
            }
        }

        fn random_pattern(rng: &mut Rng, depth: usize) -> String {
            let branches = 1 + rng.below(if depth == 0 { 3 } else { 2 });
            (0..branches)
                .map(|_| {
                    let items = 1 + rng.below(4);
                    (0..items)
                        .map(|_| random_item(rng, depth))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("|")
        }

        fn random_item(rng: &mut Rng, depth: usize) -> String {
            let roll = rng.below(10);
            if roll == 0 && depth < 2 {
                let open = rng.pick(&["(", "(?:", "(?i:", "(?-i:", "(?s:", "(?-u:"]);
                let item = format!("{open}{})", random_pattern(rng, depth + 1));
                return if rng.below(3) == 0 {
                    item + rng.pick(QUANTIFIERS)
                } else {
                    item
                };
            }
            if roll == 1 {
                return rng.pick(LOOKS).to_owned();
            }
            let atom = rng.pick(ATOMS).to_owned();
            if rng.below(4) == 0 {
                atom + rng.pick(QUANTIFIERS)
            } else {
                atom
            }
        }

        struct Probes {
            word: Regex,
            space: Regex,
            digit: Regex,
        }

        /// A pattern cut from a corpus line, so the host matches at least that
        /// line: every character becomes something that accepts it, with
        /// assertions sprinkled in that may or may not hold.
        fn derived_pattern(rng: &mut Rng, line: &str, probes: &Probes) -> String {
            let chars: Vec<char> = line.chars().collect();
            if chars.is_empty() {
                return "^$".to_owned();
            }
            let start = rng.below(chars.len());
            let len = 1 + rng.below((chars.len() - start).min(8));
            let mut out = String::new();
            if start == 0 && rng.below(2) == 0 {
                out.push('^');
            }
            for &c in &chars[start..start + len] {
                if rng.below(6) == 0 {
                    out.push_str(rng.pick(&[
                        r"\b",
                        r"\B",
                        r"\<",
                        r"\>",
                        r"(?-u:\b)",
                        r"\b{start-half}",
                        r"\b{end-half}",
                    ]));
                }
                let text = c.to_string();
                let escaped = regex::escape(&text);
                let piece = match rng.below(10) {
                    0 => ".".to_owned(),
                    1 if probes.word.is_match(&text) => r"\w".to_owned(),
                    1 => r"\W".to_owned(),
                    2 if probes.space.is_match(&text) => r"\s".to_owned(),
                    2 => r"\S".to_owned(),
                    3 if probes.digit.is_match(&text) => r"\d".to_owned(),
                    3 => r"\D".to_owned(),
                    4 => {
                        let other = if c == 'q' { "z" } else { "q" };
                        format!("[^{other}]")
                    }
                    5 => format!("(?i:{escaped})"),
                    6 => format!("[{escaped}-{escaped}]"),
                    _ => escaped,
                };
                out.push_str(&piece);
                if rng.below(5) == 0 {
                    out.push_str(rng.pick(&["+", "*", "?", "{1}", "{0,3}", "+?", "{1,}"]));
                }
            }
            if start + len == chars.len() && rng.below(2) == 0 {
                out.push('$');
            }
            out
        }

        struct Case {
            pattern: String,
            case_sensitive: bool,
            /// Rendered from this level on, so the widened forms get checked.
            level: usize,
            remote: RemotePattern,
        }

        #[derive(Default)]
        struct Tally {
            cases: usize,
            exact_cases: usize,
            host_hits: usize,
            remote_hits: usize,
            false_positives: usize,
            exact_false_positives: usize,
            non_matching_pairs: usize,
        }

        /// Matched line numbers per case, from `@@`-delimited grep output.
        fn parse_grep(output: &[u8], cases: usize) -> (Vec<[BTreeSet<usize>; 2]>, Vec<String>) {
            let mut hits: Vec<[BTreeSet<usize>; 2]> = (0..cases)
                .map(|_| [BTreeSet::new(), BTreeSet::new()])
                .collect();
            let mut errors = Vec::new();
            let mut current: Option<(usize, usize)> = None;
            for line in output.split(|&b| b == b'\n') {
                let text = String::from_utf8_lossy(line);
                if let Some(rest) = text.strip_prefix("@@ rc ") {
                    let rc = rest.trim();
                    if rc != "0" && rc != "1" {
                        errors.push(format!("{current:?}: exit {rc}"));
                    }
                    continue;
                }
                if let Some(rest) = text.strip_prefix("@@ ") {
                    let mut parts = rest.split(' ');
                    let index: usize = parts.next().unwrap().parse().unwrap();
                    let dialect = if parts.next() == Some("P") { 0 } else { 1 };
                    current = Some((index, dialect));
                    continue;
                }
                if line.is_empty() {
                    continue;
                }
                let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
                match (
                    current,
                    text[digits.len()..].starts_with(':'),
                    digits.parse(),
                ) {
                    (Some((index, dialect)), true, Ok(number)) => {
                        hits[index][dialect].insert(number);
                    }
                    _ => errors.push(format!("{current:?}: {text}")),
                }
            }
            (hits, errors)
        }

        const DOTNET_PROBE: &str = r#"param([string]$Corpus, [string]$Patterns, [string]$Out)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Text.RegularExpressions;
public static class MewrkRemoteRegexProbe {
    public static void Run(string corpus, string patterns, string output) {
        var utf8 = new UTF8Encoding(false);
        string text = File.ReadAllText(corpus, utf8).Replace("\r\n", "\n");
        string[] lines = text.Split('\n');
        int count = lines.Length;
        if (count > 0 && lines[count - 1].Length == 0) count--;
        var sb = new StringBuilder();
        string[] pats = File.ReadAllLines(patterns, utf8);
        for (int i = 0; i < pats.Length; i++) {
            sb.Append(i);
            try {
                var rx = new Regex(pats[i], RegexOptions.CultureInvariant, TimeSpan.FromSeconds(5));
                sb.Append(" OK");
                for (int j = 0; j < count; j++) {
                    try { if (rx.IsMatch(lines[j])) { sb.Append(' ').Append(j + 1); } }
                    catch (RegexMatchTimeoutException) { sb.Append(" T").Append(j + 1); }
                }
            } catch (ArgumentException e) {
                sb.Append(" ERR ").Append(e.Message.Replace('\n', ' ').Replace('\r', ' '));
            }
            sb.Append('\n');
        }
        File.WriteAllText(output, sb.ToString(), utf8);
    }
}
'@
[MewrkRemoteRegexProbe]::Run($Corpus, $Patterns, $Out)
"#;

        /// Matched line numbers per case, and how many lines timed out. A
        /// backtracking engine can take polynomial time where Rust's takes
        /// linear time, so the production script, like this probe, bounds
        /// each match and hands a line that ran out of time to the host's
        /// re-check, which keeps the superset intact.
        fn run_dotnet(
            dir: &Path,
            cases: &[Case],
        ) -> (Vec<BTreeSet<usize>>, Vec<BTreeSet<usize>>, Vec<String>) {
            let mut patterns = String::new();
            for case in cases {
                patterns.push_str(&case.remote.dotnet);
                patterns.push('\n');
            }
            fs::write(dir.join("dotnet-patterns.txt"), patterns).unwrap();
            fs::write(dir.join("probe.ps1"), DOTNET_PROBE).unwrap();
            let shell =
                std::env::var("MEWRK_TEST_POWERSHELL").unwrap_or_else(|_| "powershell.exe".into());
            let output = Command::new(shell)
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(dir.join("probe.ps1"))
                .arg("-Corpus")
                .arg(dir.join("dotnet-corpus.txt"))
                .arg("-Patterns")
                .arg(dir.join("dotnet-patterns.txt"))
                .arg("-Out")
                .arg(dir.join("dotnet-out.txt"))
                .output()
                .expect("PowerShell runs");
            assert!(
                output.status.success(),
                "PowerShell failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = fs::read_to_string(dir.join("dotnet-out.txt")).unwrap();
            let mut hits = vec![BTreeSet::new(); cases.len()];
            let mut timeouts = vec![BTreeSet::new(); cases.len()];
            let mut errors = Vec::new();
            for line in text.lines() {
                let mut parts = line.split(' ');
                let index: usize = parts.next().unwrap().parse().unwrap();
                match parts.next() {
                    Some("OK") => {
                        for part in parts {
                            let timed_out = part.strip_prefix('T');
                            match timed_out.unwrap_or(part).parse() {
                                Ok(number) => {
                                    hits[index].insert(number);
                                    if timed_out.is_some() {
                                        timeouts[index].insert(number);
                                    }
                                }
                                Err(_) => errors.push(format!("case {index}: {part}")),
                            }
                        }
                    }
                    _ => errors.push(format!("case {index}: {line}")),
                }
            }
            (hits, timeouts, errors)
        }

        #[test]
        #[ignore = "runs GNU grep -P and -E through sh and printf, and .NET through PowerShell"]
        fn remote_engines_accept_every_line_the_host_accepts() {
            let dir =
                std::env::temp_dir().join(format!("mewrk-remote-regex-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();

            // Long lines give a backtracking engine room to go exponential,
            // should a rendering let it split the same bytes several ways.
            let mut raw_lines: Vec<Vec<u8>> = CORPUS.iter().map(|line| line.to_vec()).collect();
            raw_lines.push("\u{4e2d}\u{6587}".repeat(120).into_bytes());
            raw_lines.push("caf\u{e9} \u{1f600} x-y_z; ".repeat(25).into_bytes());
            raw_lines.push(b"\xe9\x80\xbf\xff\xc3".repeat(60));
            let mut corpus = Vec::new();
            for line in &raw_lines {
                corpus.extend_from_slice(line);
                corpus.push(b'\n');
            }
            fs::write(dir.join("corpus.txt"), &corpus).unwrap();
            let host_lines: Vec<String> = String::from_utf8_lossy(&corpus)
                .lines()
                .map(str::to_owned)
                .collect();
            assert_eq!(host_lines.len(), raw_lines.len());
            // .NET reads the host's decoding, so its decoder's own handling of
            // invalid UTF-8 stays out of the comparison; CRLF is kept for the
            // probe to fold, as the production script does.
            let mut dotnet_corpus = String::new();
            for (raw, line) in raw_lines.iter().zip(&host_lines) {
                dotnet_corpus.push_str(line);
                if raw.ends_with(b"\r") {
                    dotnet_corpus.push('\r');
                }
                dotnet_corpus.push('\n');
            }
            fs::write(dir.join("dotnet-corpus.txt"), dotnet_corpus).unwrap();

            let probes = Probes {
                word: Regex::new(r"^\w$").unwrap(),
                space: Regex::new(r"^\s$").unwrap(),
                digit: Regex::new(r"^\d$").unwrap(),
            };
            let mut rng = Rng(0x6d65_7772_6b21);
            let mut patterns: Vec<String> = PATTERNS.iter().map(|p| (*p).to_owned()).collect();
            let mut generated = 0;
            while generated < 400 {
                let pattern = random_pattern(&mut rng, 0);
                if regex::Regex::new(&pattern).is_ok() {
                    patterns.push(pattern);
                    generated += 1;
                }
            }
            let mut derived = 0;
            while derived < 400 {
                let line = &host_lines[rng.below(host_lines.len())];
                let pattern = derived_pattern(&mut rng, line, &probes);
                if regex::Regex::new(&pattern).is_ok() {
                    patterns.push(pattern);
                    derived += 1;
                }
            }

            let mut cases = Vec::new();
            for (index, pattern) in patterns.iter().enumerate() {
                for (case_sensitive, level) in [(true, 0), (false, 0), (true, 3), (false, 4)] {
                    // Every hand-picked pattern and every fourth generated one
                    // is also rendered as the widest levels would.
                    if level > 0 && index >= PATTERNS.len() && index % 4 != 0 {
                        continue;
                    }
                    let rendered = if level == 0 {
                        translate(pattern, case_sensitive)
                    } else {
                        translate_from(pattern, case_sensitive, &LEVELS[level..])
                    };
                    if let Ok(remote) = rendered {
                        assert!(
                            remote
                                .pcre
                                .bytes()
                                .all(|b| (0x20..0x7F).contains(&b) && b != b'\''),
                            "pcre for {pattern:?} is not quote-free printable ASCII"
                        );
                        assert!(
                            remote
                                .dotnet
                                .bytes()
                                .all(|b| (0x20..0x7F).contains(&b) && b != b'\''),
                            "dotnet for {pattern:?} is not quote-free printable ASCII"
                        );
                        assert!(!remote.ere.contains(&b'\n') && !remote.ere.contains(&0));
                        cases.push(Case {
                            pattern: pattern.clone(),
                            case_sensitive,
                            level,
                            remote,
                        });
                    }
                }
            }

            // MEWRK_TEST_CASE=<n> names case n of a failing run instead of running it.
            if let Some(index) = std::env::var("MEWRK_TEST_CASE")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
            {
                let case: &Case = &cases[index];
                println!(
                    "case {index}: {:?} cs={} level {}",
                    case.pattern, case.case_sensitive, case.level
                );
                return;
            }
            let mut script = String::new();
            for (index, case) in cases.iter().enumerate() {
                let _ = writeln!(
                    script,
                    "echo '@@ {index} P'; \
                     LC_ALL=C grep -U -a -n -P -e '{}' corpus.txt 2>&1; echo \"@@ rc $?\"",
                    case.remote.pcre
                );
                let _ = writeln!(
                    script,
                    "echo '@@ {index} E'; \
                     LC_ALL=C grep -U -a -n -E -e \"$(printf {})\" corpus.txt 2>&1; echo \"@@ rc $?\"",
                    ere_printf_argument(&case.remote.ere)
                );
            }
            fs::write(dir.join("grep.sh"), script).unwrap();
            let shell = std::env::var("MEWRK_TEST_SH").unwrap_or_else(|_| "sh".into());
            let output = Command::new(shell)
                .arg("grep.sh")
                .current_dir(&dir)
                .output()
                .expect("sh runs");
            let (grep_hits, mut errors) = parse_grep(&output.stdout, cases.len());
            let (dotnet_hits, dotnet_timeouts, dotnet_errors) = run_dotnet(&dir, &cases);
            errors.extend(dotnet_errors);

            let names = ["pcre", "ere", "dotnet"];
            let mut tallies: [[Tally; 3]; 2] = Default::default();
            let mut failures = Vec::new();
            for (index, case) in cases.iter().enumerate() {
                let host: BTreeSet<usize> = host_lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| case.remote.regex.is_match(line))
                    .map(|(number, _)| number + 1)
                    .collect();
                let remote = [
                    &grep_hits[index][0],
                    &grep_hits[index][1],
                    &dotnet_hits[index],
                ];
                let exact = [
                    case.remote.pcre_exact,
                    case.remote.ere_exact,
                    case.remote.dotnet_exact,
                ];
                for dialect in 0..3 {
                    let tally = &mut tallies[usize::from(case.level > 0)][dialect];
                    tally.cases += 1;
                    tally.host_hits += host.len();
                    tally.remote_hits += remote[dialect].len();
                    tally.non_matching_pairs += host_lines.len() - host.len();
                    let missing: Vec<_> = host.difference(remote[dialect]).collect();
                    // A line handed over on a timeout says nothing about the
                    // rendering, so it does not count against an exact claim.
                    let no_timeouts = BTreeSet::new();
                    let timed_out = if dialect == 2 {
                        &dotnet_timeouts[index]
                    } else {
                        &no_timeouts
                    };
                    let extra: Vec<_> = remote[dialect]
                        .difference(&host)
                        .filter(|number| !timed_out.contains(number))
                        .collect();
                    tally.false_positives += extra.len();
                    if !extra.is_empty() && std::env::var_os("MEWRK_TEST_VERBOSE").is_some() {
                        println!(
                            "fp {} {:?} cs={} extra {:?}",
                            names[dialect], case.pattern, case.case_sensitive, extra
                        );
                    }
                    if exact[dialect] {
                        tally.exact_cases += 1;
                        tally.exact_false_positives += extra.len();
                    }
                    let rendered = match dialect {
                        0 => case.remote.pcre.clone(),
                        1 => ere_printf_argument(&case.remote.ere),
                        _ => case.remote.dotnet.clone(),
                    };
                    if !missing.is_empty() || (exact[dialect] && !extra.is_empty()) {
                        failures.push(format!(
                            "{} {:?} (case_sensitive={}, level {}): \
                             missing {:?}, extra {:?}{}\n    {}",
                            names[dialect],
                            case.pattern,
                            case.case_sensitive,
                            case.level,
                            missing,
                            extra,
                            if exact[dialect] {
                                " [claimed exact]"
                            } else {
                                ""
                            },
                            &rendered[..rendered.len().min(400)],
                        ));
                    }
                }
            }

            println!(
                "{} patterns, {} cases, {} corpus lines",
                patterns.len(),
                cases.len(),
                host_lines.len()
            );
            for (group, label) in ["as translated", "from widest levels"].iter().enumerate() {
                println!("{label}:");
                for (dialect, tally) in tallies[group].iter().enumerate() {
                    println!(
                        "  {:6}: {} exact of {} cases; host hits {}, remote hits {}, \
                         false positives {} ({:.2}% of non-matching lines, {:.2}% of remote hits), \
                         on exact cases {}",
                        names[dialect],
                        tally.exact_cases,
                        tally.cases,
                        tally.host_hits,
                        tally.remote_hits,
                        tally.false_positives,
                        100.0 * tally.false_positives as f64
                            / tally.non_matching_pairs.max(1) as f64,
                        100.0 * tally.false_positives as f64 / tally.remote_hits.max(1) as f64,
                        tally.exact_false_positives,
                    );
                }
            }
            println!(
                ".NET matches that ran out of time and went to the host: {}",
                dotnet_timeouts.iter().map(BTreeSet::len).sum::<usize>()
            );
            for error in &errors {
                println!("engine error: {error}");
            }
            for failure in &failures {
                println!("FAIL {failure}");
            }
            if errors.is_empty() && failures.is_empty() {
                let _ = fs::remove_dir_all(&dir);
            } else {
                println!("inputs kept in {}", dir.display());
            }
            assert!(errors.is_empty(), "{} engine errors", errors.len());
            assert!(failures.is_empty(), "{} failures", failures.len());
        }
    }
}
