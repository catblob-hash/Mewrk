//! Small conversions every previewer needs and none of them owns (upstream's `FilePreview/format.ts`), and the text
//! helpers beside them: `lib/frontMatter.ts`.

use regex::Regex;
use std::sync::LazyLock;

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KB", "MB", "GB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 { format!("{} {}", value.round() as u64, units[unit]) } else { format!("{value:.1} {}", units[unit]) }
}

/// `m:ss`, or `h:mm:ss` past an hour.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).floor() as u64;
    let (hours, minutes, rest) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 { format!("{hours}:{minutes:02}:{rest:02}") } else { format!("{minutes}:{rest:02}") }
}

fn payload(source: &str) -> &str {
    source.find(',').map_or(source, |c| &source[c + 1..])
}

/// The bytes a base64 `data:` URL carries.
pub fn data_url_bytes(source: &str) -> Option<Vec<u8>> {
    base64_simd::STANDARD.decode_to_vec(payload(source).trim()).ok()
}

/// How many bytes a base64 `data:` URL stands for, without decoding it.
pub fn data_url_byte_length(source: &str) -> u64 {
    let p = payload(source);
    let padding = if p.ends_with("==") { 2 } else if p.ends_with('=') { 1 } else { 0 };
    (p.len() as u64 * 3 / 4).saturating_sub(padding)
}

pub fn base64_encode(bytes: &[u8]) -> String {
    base64_simd::STANDARD.encode_to_string(bytes)
}

/// Text as a base64 `data:` URL.
pub fn text_data_url(text: &str, media_type: &str) -> String {
    format!("data:{media_type};base64,{}", base64_encode(text.as_bytes()))
}

/// The metadata block a Markdown document may open with.
#[derive(Clone, Debug, PartialEq)]
pub struct FrontMatter {
    pub language: &'static str,
    /// The block's contents, without its fences.
    pub source: String,
    /// Everything after the block.
    pub body: String,
}

static FRONT_MATTER: LazyLock<(Regex, Regex)> = LazyLock::new(|| {
    (
        Regex::new(r"^\u{FEFF}?---[ \t]*\r?\n((?s:.*?))\r?\n---[ \t]*(?:\r?\n|\z)").expect("regex"),
        Regex::new(r"^\u{FEFF}?\+\+\+[ \t]*\r?\n((?s:.*?))\r?\n\+\+\+[ \t]*(?:\r?\n|\z)").expect("regex"),
    )
});

pub fn split_front_matter(content: &str) -> Option<FrontMatter> {
    let (yaml, toml) = &*FRONT_MATTER;
    let (m, language) = yaml.captures(content).map(|c| (c, "yaml")).or_else(|| toml.captures(content).map(|c| (c, "toml")))?;
    Some(FrontMatter { language, source: m[1].to_owned(), body: content[m.get(0)?.end()..].to_owned() })
}

/// Delimited text: `lib/csv.ts`. RFC 4180 quoting is honoured; everything else is taken as written, because a viewer
/// that refuses a slightly malformed file shows less than one that reads it the way a spreadsheet would.
#[derive(Clone, Debug, PartialEq)]
pub struct DelimitedTable {
    pub rows: Vec<Vec<String>>,
    pub truncated: bool,
}

/// The candidate that appears the same non-zero number of times on the most of a file's first lines.
pub fn guess_delimiter(text: &str) -> char {
    let lines: Vec<&str> = text.split('\n').take(20).map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.trim().is_empty()).collect();
    let (mut best, mut best_score) = (',', -1i64);
    for candidate in [',', '\t', ';', '|'] {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(candidate).count()).collect();
        let first = counts.first().copied().unwrap_or(0);
        if first == 0 {
            continue;
        }
        let score = counts.iter().filter(|c| **c == first).count() as i64 * 100 + first as i64;
        if score > best_score {
            best = candidate;
            best_score = score;
        }
    }
    best
}

pub fn parse_delimited(text: &str, delimiter: char, max_rows: usize) -> DelimitedTable {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.strip_prefix('\u{FEFF}').unwrap_or(text).chars().peekable();
    let end_row = |row: &mut Vec<String>, field: &mut String, rows: &mut Vec<Vec<String>>| {
        row.push(std::mem::take(field));
        // A trailing newline, or a blank line between records, is not a row of one empty cell.
        if !(row.len() == 1 && row[0].is_empty()) {
            rows.push(std::mem::take(row));
        } else {
            row.clear();
        }
    };
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        if c == '"' && field.is_empty() {
            quoted = true;
        } else if c == delimiter {
            row.push(std::mem::take(&mut field));
        } else if c == '\r' || c == '\n' {
            end_row(&mut row, &mut field, &mut rows);
            if rows.len() >= max_rows {
                return DelimitedTable { rows, truncated: true };
            }
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !row.is_empty() {
        end_row(&mut row, &mut field, &mut rows);
    }
    DelimitedTable { rows, truncated: false }
}

static NUMERIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-+]?(?:\d{1,3}(?:,\d{3})+|\d+)?(?:\.\d+)?(?:[eE][-+]?\d+)?%?$").expect("regex"));

/// Whether every non-empty value in a column reads as a number, so it can be right-aligned.
pub fn numeric_columns(rows: &[Vec<String>], from: usize) -> Vec<bool> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    (0..width)
        .map(|column| {
            let (mut seen, mut numeric) = (0, true);
            for row in rows.iter().skip(from) {
                let value = row.get(column).map_or("", |v| v.trim());
                if value.is_empty() {
                    continue;
                }
                seen += 1;
                if !(NUMERIC.is_match(value) && value.chars().any(|c| c.is_ascii_digit())) {
                    numeric = false;
                    break;
                }
            }
            numeric && seen > 0
        })
        .collect()
}

/// Terminal colours, read into styled runs: `lib/ansi.ts`. Only SGR is interpreted; every other sequence is removed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnsiRun {
    pub text: String,
    /// 0-15 named, or RGB for 256-colour and true-colour codes.
    pub foreground: Option<AnsiColor>,
    pub background: Option<AnsiColor>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnsiColor {
    Named(u8),
    Rgb(u8, u8, u8),
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[([0-9;]*)([A-Za-z])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Za-z0-9]|\x1b[=>]").expect("regex"));

fn extended(index: u32) -> Option<AnsiColor> {
    let level = |c: u32| if c == 0 { 0 } else { (55 + c * 40) as u8 };
    match index {
        0..=15 => Some(AnsiColor::Named(index as u8)),
        16..=231 => {
            let v = index - 16;
            Some(AnsiColor::Rgb(level(v / 36), level((v / 6) % 6), level(v % 6)))
        }
        232..=255 => {
            let g = (8 + (index - 232) * 10) as u8;
            Some(AnsiColor::Rgb(g, g, g))
        }
        _ => None,
    }
}

pub fn parse_ansi(input: &str) -> Vec<AnsiRun> {
    let mut runs: Vec<AnsiRun> = Vec::new();
    let mut state = AnsiRun::default();
    let mut cursor = 0;
    let push = |runs: &mut Vec<AnsiRun>, state: &AnsiRun, text: &str| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = runs.last_mut()
            && last.foreground == state.foreground && last.background == state.background && last.bold == state.bold && last.italic == state.italic && last.underline == state.underline
        {
            last.text.push_str(text);
        } else {
            runs.push(AnsiRun { text: text.to_owned(), ..state.clone() });
        }
    };
    for c in ANSI.captures_iter(input) {
        let m = c.get(0).expect("whole match");
        push(&mut runs, &state, &input[cursor..m.start()]);
        cursor = m.end();
        if c.get(2).map(|f| f.as_str()) != Some("m") {
            continue;
        }
        let params: Vec<u32> = c[1].split(';').map(|p| p.parse().unwrap_or(0)).collect();
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => state = AnsiRun::default(),
                1 => state.bold = true,
                3 => state.italic = true,
                4 => state.underline = true,
                22 => state.bold = false,
                23 => state.italic = false,
                24 => state.underline = false,
                30..=37 => state.foreground = Some(AnsiColor::Named((p - 30) as u8)),
                39 => state.foreground = None,
                40..=47 => state.background = Some(AnsiColor::Named((p - 40) as u8)),
                49 => state.background = None,
                90..=97 => state.foreground = Some(AnsiColor::Named((p - 90 + 8) as u8)),
                100..=107 => state.background = Some(AnsiColor::Named((p - 100 + 8) as u8)),
                38 | 48 => {
                    let colour = match params.get(i + 1) {
                        Some(5) => {
                            let c = params.get(i + 2).and_then(|v| extended(*v));
                            i += 2;
                            c
                        }
                        Some(2) => {
                            let c = match (params.get(i + 2), params.get(i + 3), params.get(i + 4)) {
                                (Some(r), Some(g), Some(b)) => Some(AnsiColor::Rgb(*r as u8, *g as u8, *b as u8)),
                                _ => None,
                            };
                            i += 4;
                            c
                        }
                        _ => None,
                    };
                    if p == 38 { state.foreground = colour } else { state.background = colour }
                }
                _ => {}
            }
            i += 1;
        }
    }
    push(&mut runs, &state, &input[cursor..]);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_sizes_and_durations() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(4096), "4.0 KB");
        assert_eq!(format_bytes(200 * 1024), "200 KB");
        assert_eq!(format_duration(65.2), "1:05");
        assert_eq!(format_duration(3700.0), "1:01:40");
    }

    #[test]
    fn counts_a_data_urls_bytes_without_decoding_it() {
        let url = text_data_url("hello", "text/plain");
        assert_eq!(data_url_byte_length(&url), 5);
        assert_eq!(data_url_bytes(&url).unwrap(), b"hello");
    }

    #[test]
    fn splits_yaml_and_toml_front_matter() {
        let f = split_front_matter("---\ntitle: x\n---\n# Body\n").unwrap();
        assert_eq!((f.language, f.source.as_str(), f.body.as_str()), ("yaml", "title: x", "# Body\n"));
        assert_eq!(split_front_matter("+++\na = 1\n+++\nb").unwrap().language, "toml");
        assert_eq!(split_front_matter("# no\n---\n"), None);
    }

    #[test]
    fn reads_delimited_text_the_way_a_spreadsheet_would() {
        assert_eq!(guess_delimiter("a;b;c\n1;2;3\n"), ';');
        assert_eq!(guess_delimiter("a\tb\n1\t2\n"), '\t');
        let t = parse_delimited("a,\"b,\"\"c\"\n\n1,\"x\ny\"\n", ',', usize::MAX);
        assert_eq!(t.rows, vec![vec!["a".to_owned(), "b,\"c".to_owned()], vec!["1".to_owned(), "x\ny".to_owned()]]);
        assert!(parse_delimited("1\n2\n3\n", ',', 2).truncated);
        let rows = vec![vec!["n".to_owned(), "s".to_owned()], vec!["1,000".to_owned(), "x".to_owned()], vec!["-2.5%".to_owned(), "1".to_owned()]];
        assert_eq!(numeric_columns(&rows, 1), [true, false]);
    }

    #[test]
    fn reads_sgr_colours_and_removes_every_other_sequence() {
        let runs = parse_ansi("a\x1b[0;31mred\x1b[0m\x1b[2Kb\x1b[38;5;196mc\x1b[38;2;1;2;3md");
        assert_eq!(runs[0].text, "a");
        assert_eq!(runs[1], AnsiRun { text: "red".into(), foreground: Some(AnsiColor::Named(1)), ..Default::default() });
        assert_eq!(runs[2].text, "b");
        assert_eq!(runs[3].foreground, Some(AnsiColor::Rgb(255, 0, 0)));
        assert_eq!(runs[4].foreground, Some(AnsiColor::Rgb(1, 2, 3)));
    }
}
