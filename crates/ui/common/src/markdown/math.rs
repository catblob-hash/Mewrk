//! Math in Markdown: `normalizeMathDelimiters` from `MarkdownContent.tsx`, and the pass that turns what it leaves into something
//! pulldown-cmark carries through. What a formula looks like is `tex.rs` and `math_layout.rs`.

use std::borrow::Cow;

/// Opens and closes the source of inline math inside a code span, so pulldown-cmark treats it as opaque text without its
/// own idea of where math ends. From the private use area: no reply has any business containing these.
pub(super) const MATH_OPEN: char = '\u{E000}';
pub(super) const MATH_CLOSE: char = '\u{E001}';

fn backslash_is_escaped(bytes: &[u8], index: usize) -> bool {
    bytes[..index].iter().rev().take_while(|b| **b == b'\\').count() % 2 == 1
}

/// JavaScript's `\s`.
fn js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn find_byte(bytes: &[u8], from: usize, byte: u8) -> Option<usize> {
    bytes[from..].iter().position(|b| *b == byte).map(|at| at + from)
}

/// Where a single `$` at `open` closes as inline math, or `None` when it is text. remark-math pairs dollars the way it
/// pairs backticks, so "$4.50 vs $4.75" became a formula. This takes Pandoc's rule for the pair it would make: the opener
/// is followed by a non-space, the closer is preceded by a non-space and not followed by a digit. The search ends at a
/// code span, `$$` or a blank line.
fn single_dollar_close(content: &str, open: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    if content[open + 1..].chars().next().is_none_or(js_space) {
        return None;
    }
    let mut cursor = open + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'`' => return None,
            b'\n' => {
                let line_end = find_byte(bytes, cursor + 1, b'\n').unwrap_or(bytes.len());
                if content[cursor + 1..line_end].trim().is_empty() {
                    return None;
                }
            }
            b'$' if !backslash_is_escaped(bytes, cursor) => {
                let before = content[..cursor].chars().next_back();
                if bytes.get(cursor + 1) == Some(&b'$') || before.is_some_and(js_space) || bytes.get(cursor + 1).is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                return Some(cursor);
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

/// The fence a line opens or closes: its marker, how long it is and how much of the line it and its indent take.
fn fence_on_line(line: &str) -> Option<(u8, usize, usize)> {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take(4).take_while(|b| **b == b' ').count();
    if indent > 3 {
        return None;
    }
    let marker = *bytes.get(indent)?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let length = bytes[indent..].iter().take_while(|b| **b == marker).count();
    (length >= 3).then_some((marker, length, indent + length))
}

/// remark-math understands dollar delimiters. Models also commonly emit the LaTeX-style `\(...\)` and `\[...\]` forms, so
/// normalize those while leaving fenced and inline code untouched. A single `$` that does not open or close inline math
/// (see `single_dollar_close`) is escaped, so prices stay text.
pub fn normalize_math_delimiters(content: &str) -> String {
    normalized(content).into_owned()
}

pub(super) fn normalized(content: &str) -> Cow<'_, str> {
    // Most replies have none of this, and a stream asks ten times a second.
    let bytes = content.as_bytes();
    let special = bytes.iter().enumerate().any(|(i, b)| *b == b'$' || (*b == b'\\' && matches!(bytes.get(i + 1), Some(b'(' | b')' | b'[' | b']'))));
    if !special {
        return Cow::Borrowed(content);
    }
    let mut result = String::with_capacity(content.len() + 16);
    let mut cursor = 0;
    let mut inline_ticks = 0usize;
    let mut fence: Option<(u8, usize)> = None;
    let mut line_start = true;
    let mut close_fence_at_line_end = false;
    let mut math_close: Option<usize> = None;

    while cursor < bytes.len() {
        if line_start && inline_ticks == 0 {
            let line_end = find_byte(bytes, cursor, b'\n').unwrap_or(bytes.len());
            let line = &content[cursor..line_end];
            if let Some((marker, length, taken)) = fence_on_line(line) {
                match fence {
                    None => fence = Some((marker, length)),
                    Some((open_marker, open_length)) => {
                        if open_marker == marker && length >= open_length && line[taken..].trim().is_empty() {
                            close_fence_at_line_end = true;
                        }
                    }
                }
            }
        }

        let character = bytes[cursor];
        if fence.is_none() && character == b'`' {
            let run = bytes[cursor..].iter().take_while(|b| **b == b'`').count();
            if inline_ticks == 0 {
                inline_ticks = run;
            } else if inline_ticks == run {
                inline_ticks = 0;
            }
            result.push_str(&content[cursor..cursor + run]);
            cursor += run;
            line_start = false;
            continue;
        }

        if fence.is_none() && inline_ticks == 0 && character == b'\\' && !backslash_is_escaped(bytes, cursor) {
            match bytes.get(cursor + 1) {
                Some(b'(' | b')') => {
                    result.push('$');
                    cursor += 2;
                    line_start = false;
                    continue;
                }
                Some(b'[' | b']') => {
                    result.push_str("\n$$\n");
                    cursor += 2;
                    line_start = false;
                    continue;
                }
                _ => {}
            }
        }

        if fence.is_none() && inline_ticks == 0 && character == b'$' && !backslash_is_escaped(bytes, cursor) {
            if bytes.get(cursor + 1) == Some(&b'$') {
                result.push_str("\n$$\n");
                cursor += 2;
                line_start = false;
                continue;
            }
            if math_close == Some(cursor) {
                math_close = None;
            } else if math_close.is_none() {
                math_close = single_dollar_close(content, cursor);
                if math_close.is_none() {
                    result.push('\\');
                }
            }
            result.push('$');
            cursor += 1;
            line_start = false;
            continue;
        }

        if character == b'\n' {
            result.push('\n');
            cursor += 1;
            line_start = true;
            if close_fence_at_line_end {
                fence = None;
                close_fence_at_line_end = false;
            }
            continue;
        }

        // A run with nothing to look at goes across whole, which is most of a long reply.
        let run_end = bytes[cursor + 1..].iter().position(|b| matches!(*b, b'`' | b'\\' | b'$' | b'\n')).map_or(bytes.len(), |at| at + cursor + 1);
        result.push_str(&content[cursor..run_end]);
        cursor = run_end;
        line_start = false;
    }
    Cow::Owned(result)
}

/// What is left to do after `normalized`, which remark-math would do next: a `$$` line opens display math, which runs to
/// the next one or to the end of the text and may hold blank lines, and becomes the ```` ```math ```` fence upstream treats
/// the same. Inline `$x$` (when `single_dollar` allows it) becomes a code span carrying the source between sentinels.
pub(super) fn prepare_math(normalized: &str, single_dollar: bool) -> Cow<'_, str> {
    if !normalized.contains('$') {
        return Cow::Borrowed(normalized);
    }
    let bytes = normalized.as_bytes();
    let mut out = String::with_capacity(normalized.len() + 32);
    let mut fence: Option<(u8, usize)> = None;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let line_end = find_byte(bytes, cursor, b'\n').unwrap_or(bytes.len());
        let line = &normalized[cursor..line_end];
        let next = (line_end + 1).min(bytes.len());
        if let Some((marker, length, taken)) = fence_on_line(line) {
            match fence {
                None => fence = Some((marker, length)),
                Some((open_marker, open_length)) => {
                    if open_marker == marker && length >= open_length && line[taken..].trim().is_empty() {
                        fence = None;
                    }
                }
            }
            out.push_str(&normalized[cursor..next]);
            cursor = next;
            continue;
        }
        if fence.is_some() {
            out.push_str(&normalized[cursor..next]);
            cursor = next;
            continue;
        }
        if line.trim() == "$$" && line.len() - line.trim_start().len() <= 3 {
            cursor = write_display_math(normalized, next, &mut out);
            continue;
        }
        cursor = write_prose_line(normalized, cursor, single_dollar, &mut out);
    }
    Cow::Owned(out)
}

/// The fence for display math whose body starts at `body_start`, up to the closing `$$` line or the end of the text: a
/// stream shows the formula so far. Answers where to go on from.
fn write_display_math(text: &str, body_start: usize, out: &mut String) -> usize {
    let bytes = text.as_bytes();
    let mut scan = body_start;
    let (mut body_end, mut resume) = (bytes.len(), bytes.len());
    while scan < bytes.len() {
        let end = find_byte(bytes, scan, b'\n').unwrap_or(bytes.len());
        if text[scan..end].trim() == "$$" {
            body_end = scan;
            resume = (end + 1).min(bytes.len());
            break;
        }
        scan = end + 1;
    }
    let body = text[body_start..body_end].trim_end_matches('\n');
    let ticks = "`".repeat(longest_tick_run(body).max(2) + 1);
    out.push_str(&ticks);
    out.push_str("math\n");
    out.push_str(body);
    if !body.is_empty() {
        out.push('\n');
    }
    out.push_str(&ticks);
    out.push('\n');
    resume
}

fn longest_tick_run(text: &str) -> usize {
    let (mut best, mut run) = (0, 0);
    for b in text.bytes() {
        run = if b == b'`' { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// Whether the line starting at `at` has nothing on it, where neither a code span nor inline math can reach past.
fn blank_line_at(text: &str, at: usize) -> bool {
    let end = find_byte(text.as_bytes(), at, b'\n').unwrap_or(text.len());
    text[at..end].trim().is_empty()
}

/// The closing run of a code span opened by `run` backticks before `from`, within its paragraph.
fn code_span_close(text: &str, from: usize, run: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'`' => {
                let length = bytes[at..].iter().take_while(|b| **b == b'`').count();
                if length == run {
                    return Some(at);
                }
                at += length;
            }
            b'\n' if blank_line_at(text, at + 1) => return None,
            _ => at += 1,
        }
    }
    None
}

/// The `$` that closes one at `open`, within its paragraph. `normalized` escaped every dollar that has none, except where
/// its own count of backticks was off, and there this simply finds nothing.
fn math_close(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = open + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'$' if !backslash_is_escaped(bytes, at) => return Some(at),
            b'\n' if blank_line_at(text, at + 1) => return None,
            _ => at += 1,
        }
    }
    None
}

/// Copies prose from `start` to the end of its line, code spans and inline math whole even where they cross into the next
/// line. Answers where the next line starts.
fn write_prose_line(text: &str, start: usize, single_dollar: bool, out: &mut String) -> usize {
    let bytes = text.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() {
        let special = bytes[cursor..].iter().position(|b| matches!(*b, b'`' | b'$' | b'\n')).map_or(bytes.len(), |at| at + cursor);
        out.push_str(&text[cursor..special]);
        cursor = special;
        match bytes.get(cursor) {
            None => return cursor,
            Some(b'\n') => {
                out.push('\n');
                return cursor + 1;
            }
            Some(b'`') => {
                let run = bytes[cursor..].iter().take_while(|b| **b == b'`').count();
                let end = code_span_close(text, cursor + run, run).map_or(cursor + run, |close| close + run);
                out.push_str(&text[cursor..end]);
                cursor = end;
            }
            Some(_) => {
                let close = (single_dollar && !backslash_is_escaped(bytes, cursor)).then(|| math_close(text, cursor)).flatten();
                match close {
                    Some(close) => {
                        out.push('`');
                        out.push(MATH_OPEN);
                        // A line break inside math is a space, and must not start a list or a quote on the way through.
                        out.extend(text[cursor + 1..close].chars().map(|c| if c == '\n' { ' ' } else { c }));
                        out.push(MATH_CLOSE);
                        out.push('`');
                        cursor = close + 1;
                    }
                    None => {
                        out.push('$');
                        cursor += 1;
                    }
                }
            }
        }
    }
    cursor
}
