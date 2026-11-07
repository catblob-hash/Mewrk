//! `PathText.tsx` and path detection: `lib/pathDisplay.ts` (how a path gives way in the middle), `lib/remarkPathLinks.ts`
//! (paths in prose and code spans), `lib/documentLinks.ts` (what a link's `href` names), `lib/externalLinks.ts`
//! (which addresses go to the system browser).
//!
//! Everything but the two elements is a plain function, so the transcript, the file pane and any row that draws a path
//! agree on what a path is.

use mewrk_client::prelude::*;
use regex::Regex;
use std::hash::Hash;
use std::sync::LazyLock;

// ---------------------------------------------------------------- what a link's href names

#[derive(Clone, Debug, PartialEq)]
pub struct LocalLinkTarget {
    /// The file, exactly as written but decoded; empty for a link to a heading in the same document.
    pub path: String,
    /// The line named by `:12` or `#L12`, or `None` when the link names none.
    pub line: Option<u32>,
    /// A fragment that is not a line reference: the heading a link jumps to.
    pub fragment: Option<String>,
}

static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]+:").expect("regex"));
static LINE_FRAGMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^L?(\d+)(?:C\d+)?(?:-L?\d+(?:C\d+)?)?$").expect("regex"));
static LINE_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":(\d+)(?::\d+)?$").expect("regex"));
static FILE_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^file:(?://(?:localhost)?)?").expect("regex"));

/// `decodeURIComponent`; a stray `%` is part of the name, not the start of an escape.
pub(crate) fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
            let Some(byte) = hex else { return value.to_owned() };
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_owned())
}

fn positive_line(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|l| *l > 0)
}

/// Takes a link's `href` apart, or returns `None` when it is somebody else's to follow, or names nothing at all.
pub fn local_link_target(href: &str) -> Option<LocalLinkTarget> {
    let mut raw = href.trim().to_owned();
    if raw.is_empty() {
        return None;
    }
    if raw.to_lowercase().starts_with("file:") {
        raw = FILE_PREFIX.replace(&raw, "").into_owned();
        // `file:///C:/x` carries the drive after the root slash.
        let b = raw.as_bytes();
        if b.len() > 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' && matches!(b[3], b'/' | b'\\') {
            raw.remove(0);
        }
    } else if SCHEME.is_match(&raw) || raw.starts_with("//") {
        return None;
    }
    let (before_hash, fragment) = match raw.find('#') {
        Some(h) => (&raw[..h], Some(decode(&raw[h + 1..]))),
        None => (raw.as_str(), None),
    };
    let mut path = decode(before_hash.split('?').next().unwrap_or(""));
    let mut line = None;
    if let Some(c) = LINE_SUFFIX.captures(&path) {
        let at = c.get(0).expect("whole").start();
        // `C:` alone is a drive, not a line; a suffix needs a path in front of it.
        let drive = at == 1 && path.as_bytes()[0].is_ascii_alphabetic();
        if at > 0 && !drive {
            line = positive_line(&c[1]);
            path.truncate(at);
        }
    }
    let mut heading = None;
    if let Some(f) = fragment.filter(|f| !f.is_empty()) {
        match LINE_FRAGMENT.captures(&f).filter(|_| !path.is_empty()) {
            Some(c) => line = positive_line(&c[1]),
            None => heading = Some(f),
        }
    }
    if path.is_empty() && heading.is_none() {
        return None;
    }
    Some(LocalLinkTarget { path, line, fragment: heading })
}

/// The anchor a heading answers to: lowercased, punctuation dropped, spaces hyphenated.
pub fn heading_slug(text: &str) -> String {
    static STRIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}\s_-]").expect("regex"));
    let lower = text.trim().to_lowercase();
    STRIP.replace_all(&lower, "").chars().map(|c| if c.is_whitespace() { '-' } else { c }).collect()
}

/// Hand-written tables of contents often fold the runs GitHub's slugs keep: `a--b` as `a-b`.
pub fn folded_slug(slug: &str) -> String {
    static RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-{2,}").expect("regex"));
    RUNS.replace_all(slug, "-").into_owned()
}

/// Whether a fragment names a heading with this text.
pub fn slug_matches(heading: &str, fragment: &str) -> bool {
    let wanted = heading_slug(&decode(fragment));
    let slug = heading_slug(heading);
    !wanted.is_empty() && (slug == wanted || folded_slug(&slug) == folded_slug(&wanted))
}

/// An address the system browser should get, in the form the host command takes, or `None`. Matches the host's
/// `external_open::parse_external_url`: only http(s), with a host. Upstream reaches this through `new URL`, so what comes
/// back is what that would serialise: the scheme and host lowercased, a root path filled in, spaces encoded.
pub fn external_http_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let split = trimmed.find("://")?;
    let scheme = trimmed[..split].to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &trimmed[split + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let (user, host_port) = match authority.rfind('@') {
        Some(at) => (&authority[..=at], &authority[at + 1..]),
        None => ("", authority),
    };
    if host_port.is_empty() || host_port.contains(char::is_whitespace) {
        return None;
    }
    let default_port = if scheme == "http" { ":80" } else { ":443" };
    let host_port = host_port.strip_suffix(default_port).unwrap_or(host_port);
    if host_port.is_empty() || host_port.starts_with(':') {
        return None;
    }
    let mut out = format!("{scheme}://{user}{}", host_port.to_lowercase());
    if !tail.starts_with('/') {
        out.push('/');
    }
    for c in tail.chars() {
        match c {
            c if c.is_ascii_control() => {}
            ' ' => out.push_str("%20"),
            c => out.push(c),
        }
    }
    Some(out)
}

// ---------------------------------------------------------------- paths in prose

#[derive(Clone, Debug, PartialEq)]
pub enum PathRun {
    Text(String),
    Path { display: String, target: String, line: Option<u32> },
}

/// Longest path accepted; matches the host's own bound.
const MAX_PATH_LENGTH: usize = 400;

/// Characters that terminate a path match in prose. Full-width punctuation ends a Chinese sentence and never appears in
/// a file name, while ASCII `.` and `,` do and are trimmed afterwards instead.
fn prose_stop(c: char) -> bool {
    c.is_whitespace() || "\"'`<>|*?，。、；：！？（）【】《》「」『』…“”‘’".contains(c)
}

fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Where an absolute path starting at byte `i` of `s` would end, if one starts there. The POSIX branch requires a
/// complete first segment so that `and/or`, `24/7` and `5.0/6.0` cannot match.
fn absolute_at(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    let body_from = if first.is_ascii_alphabetic() {
        let (_, second) = chars.next()?;
        let (at, third) = chars.next()?;
        if second != ':' || !matches!(third, '\\' | '/') {
            return None;
        }
        at + third.len_utf8()
    } else if first == '/' {
        let mut seen = false;
        let mut ok = false;
        for (_, c) in chars.by_ref() {
            if word(c) || ".@+~-".contains(c) {
                seen = true;
            } else {
                ok = c == '/' && seen;
                break;
            }
        }
        if !ok {
            return None;
        }
        1
    } else {
        return None;
    };
    let end = rest[body_from..].find(prose_stop).map_or(rest.len(), |e| body_from + e);
    Some(i + end)
}

/// Drops trailing punctuation that belongs to the sentence rather than the path. Brackets go only when unbalanced, so
/// `C:\Program Files (x86)\node.exe` survives while `(see /usr/bin/env)` does not keep its closing parenthesis.
fn trim_sentence_punctuation(value: &str) -> &str {
    let mut result = value;
    while let Some(last) = result.chars().last() {
        if ".,;:!?。，、；：！？".contains(last) {
            result = &result[..result.len() - last.len_utf8()];
            continue;
        }
        let opener = match last {
            ')' => '(',
            ']' => '[',
            '}' => '{',
            '）' => '（',
            '】' => '【',
            '》' => '《',
            _ => break,
        };
        if result.matches(last).count() > result.matches(opener).count() {
            result = &result[..result.len() - last.len_utf8()];
            continue;
        }
        break;
    }
    result
}

fn separators(value: &str) -> usize {
    value.chars().filter(|c| matches!(c, '/' | '\\')).count()
}

/// Builds the display and host-facing forms, or rejects the candidate.
fn to_path(raw: &str) -> Option<(String, String, Option<u32>)> {
    static URL_LIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").expect("regex"));
    let display = trim_sentence_punctuation(raw);
    if display.is_empty() || display.chars().count() > MAX_PATH_LENGTH {
        return None;
    }
    // A network or device path must not be offered at all; the host rejects it too.
    if display.starts_with("//") || display.starts_with("\\\\") || URL_LIKE.is_match(display) {
        return None;
    }
    let (target, line) = match LINE_SUFFIX.captures(display) {
        // `foo.ts:12:5` names a line and a column; only the line is somewhere to go.
        Some(c) => (&display[..c.get(0).expect("whole").start()], positive_line(&c[1])),
        None => (display, None),
    };
    if target.is_empty() || separators(target) == 0 {
        return None;
    }
    Some((display.to_owned(), target.to_owned(), line))
}

/// Splits a prose run into plain text and absolute-path segments.
pub fn split_path_runs(value: &str) -> Vec<PathRun> {
    let mut runs = Vec::new();
    let mut consumed = 0;
    let mut at = 0;
    while at < value.len() {
        let c = value[at..].chars().next().expect("in bounds");
        let step = c.len_utf8();
        if !(c == '/' || c.is_ascii_alphabetic()) {
            at += step;
            continue;
        }
        // Without this guard the drive-letter branch matches `s:/` inside `https://`, and the POSIX branch the tail of
        // any unlinked URL.
        let previous = value[..at].chars().last();
        if previous.is_some_and(|p| matches!(p, ':' | '/' | '\\') || word(p)) {
            at += step;
            continue;
        }
        let Some(end) = absolute_at(value, at) else {
            at += step;
            continue;
        };
        let Some((display, target, line)) = to_path(&value[at..end]) else {
            at += step;
            continue;
        };
        if at > consumed {
            runs.push(PathRun::Text(value[consumed..at].to_owned()));
        }
        consumed = at + display.len();
        runs.push(PathRun::Path { display, target, line });
        at = consumed;
    }
    if consumed < value.len() {
        runs.push(PathRun::Text(value[consumed..].to_owned()));
    }
    runs
}

/// Matches a whole inline-code span as a path, as `(display, target, line)`. Relative paths are accepted here because
/// backticks are the signal that the author meant a path rather than prose.
pub fn inline_code_path(value: &str) -> Option<(String, String, Option<u32>)> {
    static EXTENSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[A-Za-z][A-Za-z0-9]{0,7}$").expect("regex"));
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_PATH_LENGTH || trimmed.contains(['"', '\'', '`', '<', '>', '|', '*', '?', '\n', '\r', '\t']) {
        return None;
    }
    let candidate = to_path(trimmed)?;
    let target = &candidate.1;
    let b = target.as_bytes();
    let windows_absolute = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\');
    let absolute = windows_absolute || (target.starts_with('/') && separators(target) >= 2);
    // A relative path containing a space is far more often a command line than a path.
    if !absolute && target.contains(' ') {
        return None;
    }
    if absolute || target.starts_with("./") || target.starts_with(".\\") || target.starts_with("../") || target.starts_with("..\\") {
        return Some(candidate);
    }
    // The extension must start with a letter, or `5.0/6.0` reads as a path.
    if EXTENSION.is_match(target) || separators(target) >= 2 {
        return Some(candidate);
    }
    None
}

/// Matches the host's own rule so a relative path is recognized identically.
pub fn is_absolute_path(value: &str) -> bool {
    let b = value.as_bytes();
    value.starts_with('/') || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
}

// ---------------------------------------------------------------- how a path gives way

/// How a path is shortened when it is drawn somewhere too narrow for it (`lib/pathDisplay.ts`): it gives way in the
/// middle, a whole directory at a time, keeping its first and last names; then those two names in their own middles.
pub fn elide_path(path: &str, fits: &mut dyn FnMut(&str) -> bool) -> String {
    const ELLIPSIS: &str = "…";
    if fits(path) {
        return path.to_owned();
    }
    // Tokens alternate name, separator-run, name, ...; runs and all come back out exactly as they went in.
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_sep = false;
    for c in path.chars() {
        let sep = matches!(c, '/' | '\\');
        if sep != in_sep {
            tokens.push(std::mem::take(&mut current));
            in_sep = sep;
        }
        current.push(c);
    }
    tokens.push(current);
    // Names sit at even indices; a trailing separator is followed by an empty name.
    if in_sep {
        tokens.push(String::new());
    }
    let named: Vec<usize> = tokens.iter().enumerate().filter(|(i, t)| i % 2 == 0 && !t.is_empty()).map(|(i, _)| i).collect();
    if named.is_empty() {
        return path.to_owned();
    }
    let count = named.len();
    let compose = |head: usize, tail: usize, replaced: &[(usize, String)]| -> String {
        let part = |from: usize, to: usize| -> String {
            (from..to).map(|i| replaced.iter().find(|(r, _)| *r == i).map_or(tokens[i].as_str(), |(_, s)| s.as_str())).collect()
        };
        if head + tail >= count {
            return part(0, tokens.len());
        }
        let head_end = named[head - 1];
        let tail_start = named[count - tail];
        format!("{}{}{ELLIPSIS}{}{}", part(0, head_end + 1), tokens[head_end + 1], tokens[tail_start - 1], part(tail_start, tokens.len()))
    };
    let (mut head, mut tail) = if count >= 3 { (1, 1) } else { (count, 0) };
    if count >= 3 && fits(&compose(head, tail, &[])) {
        let mut grew = true;
        while grew {
            grew = false;
            if head + tail + 1 < count && fits(&compose(head, tail + 1, &[])) {
                tail += 1;
                grew = true;
            }
            if head + tail + 1 < count && fits(&compose(head + 1, tail, &[])) {
                head += 1;
                grew = true;
            }
        }
        return compose(head, tail, &[]);
    }
    fn shorten_name(name: &str, keep: usize) -> String {
        let chars: Vec<char> = name.chars().collect();
        if keep >= chars.len() {
            return name.to_owned();
        }
        let (start, end) = (keep.div_ceil(2), keep / 2);
        let tail: String = if end > 0 { chars[chars.len() - end..].iter().collect() } else { String::new() };
        format!("{}{ELLIPSIS}{tail}", chars[..start].iter().collect::<String>())
    }
    let (first, last) = (named[0], named[count - 1]);
    let mut replaced: Vec<(usize, String)> = Vec::new();
    let shorten = |index: usize, replaced: &mut Vec<(usize, String)>, fits: &mut dyn FnMut(&str) -> bool| -> bool {
        let name = tokens[index].clone();
        let length = name.chars().count();
        let attempt = |keep: usize, replaced: &[(usize, String)], fits: &mut dyn FnMut(&str) -> bool| {
            let mut with = replaced.to_vec();
            with.push((index, shorten_name(&name, keep)));
            fits(&compose(head, tail, &with))
        };
        let mut found = None;
        if length > 1 && attempt(1, replaced, fits) {
            let (mut low, mut high) = (1, length - 1);
            while low < high {
                let middle = (low + high).div_ceil(2);
                if attempt(middle, replaced, fits) { low = middle } else { high = middle - 1 }
            }
            found = Some(low);
        }
        replaced.push((index, shorten_name(&name, found.unwrap_or(1))));
        found.is_some()
    };
    if first != last && shorten(first, &mut replaced, fits) {
        return compose(head, tail, &replaced);
    }
    shorten(last, &mut replaced, fits);
    compose(head, tail, &replaced)
}

// ---------------------------------------------------------------- the elements

/// What `PathText` takes beyond the path. `Default` draws the path alone, with the whole of it as its hover text.
#[derive(Clone, Default)]
pub struct PathTextOptions {
    /// Text kept whole in front of the path, such as the shell a command ran in.
    pub prefix: String,
    /// What a reader takes for its description and hover shows when the path is cut; `None` is the whole of what is drawn. `Some("")` says nothing to a reader,
    /// and a cut path still shows itself whole on hover.
    pub title: Option<String>,
}

#[derive(Default)]
struct Fitted {
    path: Arc<str>,
    prefix: String,
    room: f32,
    /// The size and weight it was measured at, which a change of the text scale also moves.
    measured: (u32, u32),
    shown: String,
    cut: bool,
}

/// A path that gives way in the middle when it does not fit. Every absolute path the app draws goes through here, so they
/// all shorten the same way. It asks for the whole path's width, so what is drawn never changes the width it is fitted to.
pub fn path_text(key: impl Hash, path: impl Into<Arc<str>>, ty: TypeStyle, color: Color) -> E {
    path_text_with(key, path, ty, color, PathTextOptions::default())
}

pub fn path_text_with(key: impl Hash, path: impl Into<Arc<str>>, ty: TypeStyle, color: Color, options: PathTextOptions) -> E {
    let id = hash_key(("path-text", hash_key(key)));
    let path: Arc<str> = path.into();
    let fitted = use_state(id, Fitted::default);
    let full = format!("{}{path}", options.prefix);
    let title = options.title.clone().unwrap_or_else(|| full.clone());
    let spoken = a11y_active().then(|| full.clone());
    let whole = full.clone();
    let measured = full;
    let prefix = options.prefix;
    let leaf = custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let scale = (ui.scale.to_bits(), ty.size.to_bits());
        let mut f = fitted.borrow_mut();
        // Half a pixel of slack for the rounding between measuring and layout. Fitting asks the text cache a few times,
        // which is worth keeping off a frame that nothing changed in.
        if f.path != path || f.prefix != prefix || f.room != r.w || f.measured != scale {
            let room = r.w - ui.measure(&prefix, &ty) - 0.5;
            let shown = elide_path(&path, &mut |text| ui.measure(text, &ty) <= room);
            *f = Fitted { path: path.clone(), prefix: prefix.clone(), room: r.w, measured: scale, cut: shown != *path, shown: format!("{prefix}{shown}") };
        }
        // `text-overflow` stays on as the fallback for wherever a measurement is off by a hair.
        if f.cut {
            ui.line_was_cut(r, &whole);
        }
        ui.label_ellipsis(r.x, r.y + (r.h - ty.line) * 0.5, r.w, &f.shown, &ty, color);
    })
    .measure_key((measured.as_str(), ty))
    .measure(move |_: &mut App, ui: &mut Ui<'_>, known: Option<f32>, avail: f32| {
        let natural = ui.measure(&measured, &ty).ceil();
        (known.unwrap_or(natural.min(avail)), ty.line)
    })
    .min_w(0.0)
    // The text is inside buttons and rows all over the app, and a custom leaf keeps a press to itself.
    .pass_through()
    // What is drawn may be shortened, and what is read is the whole path.
    .map(|leaf| match spoken {
        Some(words) => leaf.aria_label(words),
        None => leaf,
    });
    div().key(id).min_w(0.0).full_text(title).child(leaf)
}

/// A path drawn as the transcript draws a detected one: link-coloured, giving way in the middle, opening on a click.
pub fn path_link_el(key: impl Hash, path: impl Into<Arc<str>>, ty: TypeStyle, on_open: impl Fn(&mut App) + 'static) -> E {
    path_text(key, path, ty, theme().accent_text).cursor(CursorKind::Hand).on_click(on_open)
}
