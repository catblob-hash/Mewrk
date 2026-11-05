//! A parser for `git diff` text output: upstream's `lib/unifiedDiff.ts`.
//!
//! The host hands back the raw output of `git diff --unified=3`, one string for the whole scope, and the viewer needs it
//! as files, hunks and numbered lines. Nothing here fails: a patch that cannot be understood still names a file, and
//! says so with `incomplete`, because a diff the reader cannot see is a worse failure than a diff the reader can see is
//! partial.

use std::sync::OnceLock;

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffFileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// 1-based line number in the old file; `None` on an addition.
    pub old_line: Option<u32>,
    /// 1-based line number in the new file; `None` on a deletion.
    pub new_line: Option<u32>,
    /// The line's content with the leading marker removed.
    pub text: String,
    /// This line was the last in its file and carried no trailing newline.
    pub no_newline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Whatever followed the closing `@@`: usually the enclosing function.
    pub heading: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    /// Repository-relative path of the new side; for a deletion, of the old side.
    pub path: String,
    /// The old path, set only when it differs from `path` (rename or copy).
    pub old_path: Option<String>,
    pub status: DiffFileStatus,
    pub binary: bool,
    /// True when only the file mode changed and there is no content hunk.
    pub mode_change_only: bool,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    pub hunks: Vec<DiffHunk>,
    pub additions: u32,
    pub deletions: u32,
    /// True when the patch text ran out mid-file, e.g. the host hit its output cap.
    pub incomplete: bool,
}

/// Whether a mode pair changes the kind of object rather than its permissions. The high digits are the object type:
/// 100 regular, 120 symlink, 040 directory, 160 gitlink. Permission bits below them are not a type change.
fn is_type_change(old_mode: Option<&str>, new_mode: Option<&str>) -> bool {
    let (Some(old), Some(new)) = (old_mode, new_mode) else { return false };
    old.get(..old.len().saturating_sub(3)) != new.get(..new.len().saturating_sub(3))
}

/// Unquotes a path the way `git` writes one in a `diff --git` header. Git wraps a path containing unusual bytes in double
/// quotes with C escapes, and writes non-ASCII as octal escapes of the individual UTF-8 bytes. Decoding the escapes one
/// character at a time would therefore produce mojibake, so octal runs are collected as bytes and decoded together.
pub fn unquote_git_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with('"') || !trimmed.ends_with('"') || trimmed.len() < 2 {
        return trimmed.to_owned();
    }
    let body = &trimmed[1..trimmed.len() - 1];
    let mut bytes = Vec::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('\\') => bytes.push(0x5c),
            Some('"') => bytes.push(0x22),
            Some('a') => bytes.push(0x07),
            Some('b') => bytes.push(0x08),
            Some('f') => bytes.push(0x0c),
            Some('n') => bytes.push(0x0a),
            Some('r') => bytes.push(0x0d),
            Some('t') => bytes.push(0x09),
            Some('v') => bytes.push(0x0b),
            Some(d @ '0'..='7') => {
                let mut octal = d.to_digit(8).expect("in range");
                for _ in 0..2 {
                    match chars.peek() {
                        Some(n @ '0'..='7') => {
                            octal = octal * 8 + n.to_digit(8).expect("in range");
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push((octal & 0xff) as u8);
            }
            Some(other) => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => {}
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|_| body.to_owned())
}

/// Strips one `a/` or `b/` prefix from an already-unquoted header path.
fn strip_side_prefix(path: String) -> String {
    if path == "/dev/null" {
        return path;
    }
    if path.starts_with("a/") || path.starts_with("b/") {
        return path[2..].to_owned();
    }
    path
}

fn find_closing_quote(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = from + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == b'"' {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Splits `diff --git <old> <new>` into its two paths. The separator is a space and both paths may contain spaces, so
/// the split is only unambiguous when at least one side is quoted. When neither is, git's own convention is relied on:
/// the two sides differ by their `a/`…`b/` prefixes, and the `---`/`+++` lines below correct any remaining guess.
fn split_header_paths(rest: &str) -> (String, String) {
    if rest.starts_with('"')
        && let Some(end) = find_closing_quote(rest, 0)
    {
        return (rest[..=end].to_owned(), rest[end + 1..].trim().to_owned());
    }
    if let Some(marker) = rest.find(" b/") {
        return (rest[..marker].to_owned(), rest[marker + 1..].to_owned());
    }
    if let Some(quoted) = rest.find(" \"b/") {
        return (rest[..quoted].to_owned(), rest[quoted + 1..].to_owned());
    }
    let mut half = rest.len() / 2;
    while !rest.is_char_boundary(half) {
        half -= 1;
    }
    (rest[..half].trim().to_owned(), rest[half..].trim().to_owned())
}

fn header_path(raw: &str) -> String {
    strip_side_prefix(unquote_git_path(raw))
}

/// The path on a `---`/`+++` line, with git's trailing timestamp column removed.
fn marker_path(raw: &str) -> String {
    if raw.starts_with('"') {
        return match find_closing_quote(raw, 0) {
            Some(end) => header_path(&raw[..=end]),
            None => header_path(raw),
        };
    }
    header_path(raw.split('\t').next().unwrap_or(raw))
}

#[derive(Default)]
struct FileDraft {
    header_old: Option<String>,
    header_new: Option<String>,
    marker_old: Option<String>,
    marker_new: Option<String>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    copy_from: Option<String>,
    copy_to: Option<String>,
    new_file_mode: Option<String>,
    deleted_file_mode: Option<String>,
    old_mode: Option<String>,
    new_mode: Option<String>,
    binary: bool,
    combined: bool,
    hunks: Vec<DiffHunk>,
    additions: u32,
    deletions: u32,
    incomplete: bool,
}

fn settle(draft: FileDraft) -> DiffFile {
    let dev_null = |v: &Option<String>| v.as_deref() == Some("/dev/null");
    let added = draft.new_file_mode.is_some() || dev_null(&draft.marker_old);
    let deleted = draft.deleted_file_mode.is_some() || dev_null(&draft.marker_new);
    let renamed = draft.rename_from.is_some() || draft.rename_to.is_some();
    let copied = draft.copy_from.is_some() || draft.copy_to.is_some();
    let new_side = if deleted { None } else { draft.marker_new.clone().filter(|m| m != "/dev/null").or_else(|| draft.rename_to.clone()).or_else(|| draft.copy_to.clone()).or_else(|| draft.header_new.clone()) };
    let old_side = if added { None } else { draft.marker_old.clone().filter(|m| m != "/dev/null").or_else(|| draft.rename_from.clone()).or_else(|| draft.copy_from.clone()).or_else(|| draft.header_old.clone()) };
    let path = new_side.or_else(|| old_side.clone()).unwrap_or_default();
    let old_path = old_side.filter(|old| *old != path);
    let status = if added {
        DiffFileStatus::Added
    } else if deleted {
        DiffFileStatus::Deleted
    } else if renamed {
        DiffFileStatus::Renamed
    } else if copied {
        DiffFileStatus::Copied
    } else if is_type_change(draft.old_mode.as_deref(), draft.new_mode.as_deref()) {
        DiffFileStatus::TypeChanged
    } else {
        DiffFileStatus::Modified
    };
    DiffFile {
        path,
        old_path,
        status,
        binary: draft.binary,
        mode_change_only: draft.hunks.is_empty() && !draft.binary && draft.old_mode.is_some() && draft.new_mode.is_some() && !added && !deleted,
        old_mode: draft.old_mode.or(draft.deleted_file_mode),
        new_mode: draft.new_mode.or(draft.new_file_mode),
        hunks: draft.hunks,
        additions: draft.additions,
        deletions: draft.deletions,
        incomplete: draft.incomplete || draft.combined,
    }
}

fn hunk_header() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^@@+ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@+ ?(.*)$").expect("valid"))
}

struct OpenHunk {
    hunk: DiffHunk,
    old_cursor: u32,
    new_cursor: u32,
    old_remaining: i64,
    new_remaining: i64,
}

fn close_hunk(draft: &mut FileDraft, open: &mut Option<OpenHunk>) {
    if let Some(o) = open.take() {
        if o.old_remaining > 0 || o.new_remaining > 0 {
            draft.incomplete = true;
        }
        draft.hunks.push(o.hunk);
    }
}

/// Reads a patch into files. The scan is line-oriented and single pass. Structural lines are matched after a trailing
/// `\r` is dropped, but a hunk body line keeps whatever it has: a change from CRLF to LF is a real diff, and stripping it
/// here would render the two sides identical.
///
/// `fallback_path` is a path to trust over the one in the header: `git diff --no-index` names an untracked file by its
/// absolute host path, and a single-file request already knows the repository-relative path it asked for.
pub fn parse_unified_diff(patch: &str, fallback_path: Option<&str>) -> Vec<DiffFile> {
    if patch.is_empty() {
        return Vec::new();
    }
    let raw_lines: Vec<&str> = patch.split('\n').collect();
    let mut files = Vec::new();
    let mut draft: Option<FileDraft> = None;
    let mut open: Option<OpenHunk> = None;

    for (index, raw) in raw_lines.iter().enumerate() {
        let raw = *raw;
        // The final split element after a trailing newline is an artefact, not a line.
        if raw.is_empty() && index == raw_lines.len() - 1 {
            continue;
        }
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(mut d) = draft.take() {
                close_hunk(&mut d, &mut open);
                files.push(settle(d));
            }
            let (left, right) = split_header_paths(rest);
            draft = Some(FileDraft { header_old: Some(header_path(&left)), header_new: Some(header_path(&right)), ..FileDraft::default() });
            continue;
        }
        let Some(d) = draft.as_mut() else { continue };

        if line.starts_with('\\') {
            // "\ No newline at end of file" describes the line before it, which may be the last line of a hunk this
            // parser has already closed on its declared counts.
            let previous = match open.as_mut() {
                Some(o) if !o.hunk.lines.is_empty() => o.hunk.lines.last_mut(),
                _ => d.hunks.last_mut().and_then(|h| h.lines.last_mut()),
            };
            if let Some(previous) = previous {
                previous.no_newline = true;
            }
            continue;
        }

        if open.is_some() {
            let marker = line.chars().next();
            if matches!(marker, Some('+' | '-' | ' ')) || line.is_empty() {
                let o = open.as_mut().expect("checked");
                // An empty body line is an empty context line.
                let text = if line.is_empty() { String::new() } else { raw[1..].to_owned() };
                match marker {
                    Some('+') => {
                        o.hunk.lines.push(DiffLine { kind: DiffLineKind::Addition, old_line: None, new_line: Some(o.new_cursor), text, no_newline: false });
                        o.new_cursor += 1;
                        o.new_remaining -= 1;
                        d.additions += 1;
                    }
                    Some('-') => {
                        o.hunk.lines.push(DiffLine { kind: DiffLineKind::Deletion, old_line: Some(o.old_cursor), new_line: None, text, no_newline: false });
                        o.old_cursor += 1;
                        o.old_remaining -= 1;
                        d.deletions += 1;
                    }
                    _ => {
                        o.hunk.lines.push(DiffLine { kind: DiffLineKind::Context, old_line: Some(o.old_cursor), new_line: Some(o.new_cursor), text, no_newline: false });
                        o.old_cursor += 1;
                        o.new_cursor += 1;
                        o.old_remaining -= 1;
                        o.new_remaining -= 1;
                    }
                }
                if o.old_remaining <= 0 && o.new_remaining <= 0 {
                    close_hunk(d, &mut open);
                }
                continue;
            }
            close_hunk(d, &mut open);
            // Fall through: whatever this line is, it belongs to the file, not the hunk.
        }

        if line.starts_with("@@@") {
            // A combined diff has one column per parent; reading it as a two-sided patch would number every line
            // wrongly, so the file is reported as unreadable.
            d.combined = true;
            continue;
        }
        if let Some(caps) = hunk_header().captures(line) {
            let num = |i: usize, default: u32| caps.get(i).and_then(|m| m.as_str().parse().ok()).unwrap_or(default);
            let (old_start, old_lines, new_start, new_lines) = (num(1, 0), num(2, 1), num(3, 0), num(4, 1));
            let heading = caps.get(5).map_or("", |m| m.as_str()).trim_end().to_owned();
            open = Some(OpenHunk { hunk: DiffHunk { old_start, old_lines, new_start, new_lines, heading, lines: Vec::new() }, old_cursor: old_start, new_cursor: new_start, old_remaining: i64::from(old_lines), new_remaining: i64::from(new_lines) });
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            d.marker_old = Some(marker_path(rest));
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            d.marker_new = Some(marker_path(rest));
        } else if let Some(rest) = line.strip_prefix("new file mode ") {
            d.new_file_mode = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("deleted file mode ") {
            d.deleted_file_mode = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("old mode ") {
            d.old_mode = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("new mode ") {
            d.new_mode = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            d.rename_from = Some(unquote_git_path(rest));
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            d.rename_to = Some(unquote_git_path(rest));
        } else if let Some(rest) = line.strip_prefix("copy from ") {
            d.copy_from = Some(unquote_git_path(rest));
        } else if let Some(rest) = line.strip_prefix("copy to ") {
            d.copy_to = Some(unquote_git_path(rest));
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            // `index`, `similarity index` and the binary payload all fall through here; none of them carry anything the
            // viewer draws.
            d.binary = true;
        }
    }
    if let Some(mut d) = draft.take() {
        close_hunk(&mut d, &mut open);
        files.push(settle(d));
    }
    if let Some(fallback) = fallback_path
        && files.len() == 1
    {
        let only = &mut files[0];
        if only.old_path.as_deref() == Some(only.path.as_str()) {
            only.old_path = Some(fallback.to_owned());
        }
        only.path = fallback.to_owned();
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiffLineKind::{Addition, Context, Deletion};

    const MODIFIED: &str = "diff --git a/src/app.ts b/src/app.ts\nindex 1111111..2222222 100644\n--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,3 +1,3 @@ function main() {\n context\n-removed\n+added\n context2\n";

    fn parse(patch: &str) -> Vec<DiffFile> {
        parse_unified_diff(patch, None)
    }

    fn one(patch: &str) -> DiffFile {
        parse(patch).remove(0)
    }

    fn line(kind: DiffLineKind, old_line: Option<u32>, new_line: Option<u32>, text: &str) -> DiffLine {
        DiffLine { kind, old_line, new_line, text: text.into(), no_newline: false }
    }

    #[test]
    fn reads_one_hunk_of_a_single_file_modification_with_numbered_lines() {
        let file = one(MODIFIED);
        assert_eq!(file.path, "src/app.ts");
        assert_eq!(file.old_path, None);
        assert_eq!(file.status, DiffFileStatus::Modified);
        assert!(!file.binary);
        assert_eq!((file.additions, file.deletions), (1, 1));
        assert!(!file.incomplete);
        let hunk = &file.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines), (1, 3, 1, 3));
        assert_eq!(hunk.heading, "function main() {");
        assert_eq!(hunk.lines, [line(Context, Some(1), Some(1), "context"), line(Deletion, Some(2), None, "removed"), line(Addition, None, Some(2), "added"), line(Context, Some(3), Some(3), "context2")]);
    }

    #[test]
    fn splits_a_two_file_patch_into_two_files_in_order() {
        let files = parse("diff --git a/one.txt b/one.txt\nindex 1111111..2222222 100644\n--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/two.txt b/two.txt\nindex 3333333..4444444 100644\n--- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-alpha\n+beta\n");
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["one.txt", "two.txt"]);
        assert_eq!(files[0].hunks[0].lines[0].text, "old");
        assert_eq!(files[1].hunks[0].lines[1].text, "beta");
    }

    #[test]
    fn reads_added_and_deleted_files() {
        let file = one("diff --git a/new.txt b/new.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n");
        assert_eq!((file.status, file.path.as_str(), file.old_path.clone(), file.new_mode.as_deref()), (DiffFileStatus::Added, "new.txt", None, Some("100644")));
        assert_eq!((file.additions, file.deletions), (1, 0));
        let file = one("diff --git a/old.txt b/old.txt\ndeleted file mode 100644\nindex 1111111..0000000\n--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-goodbye\n");
        assert_eq!((file.status, file.path.as_str(), file.old_path.clone()), (DiffFileStatus::Deleted, "old.txt", None));
        assert_eq!((file.additions, file.deletions), (0, 1));
    }

    #[test]
    fn reads_renames_and_copies() {
        let file = one("diff --git a/old-name.txt b/new-name.txt\nsimilarity index 80%\nrename from old-name.txt\nrename to new-name.txt\nindex 1111111..2222222 100644\n--- a/old-name.txt\n+++ b/new-name.txt\n@@ -1 +1 @@\n-old content\n+new content\n");
        assert_eq!((file.status, file.path.as_str(), file.old_path.as_deref(), file.hunks.len()), (DiffFileStatus::Renamed, "new-name.txt", Some("old-name.txt"), 1));
        let file = one("diff --git a/old-name.txt b/new-name.txt\nsimilarity index 100%\nrename from old-name.txt\nrename to new-name.txt\n");
        assert_eq!((file.status, file.hunks.len()), (DiffFileStatus::Renamed, 0));
        let file = one("diff --git a/original.txt b/copy.txt\nsimilarity index 100%\ncopy from original.txt\ncopy to copy.txt\n");
        assert_eq!((file.status, file.path.as_str(), file.old_path.as_deref(), file.hunks.len()), (DiffFileStatus::Copied, "copy.txt", Some("original.txt"), 0));
    }

    #[test]
    fn flags_mode_changes_and_type_changes() {
        let file = one("diff --git a/script.sh b/script.sh\nold mode 100644\nnew mode 100755\n");
        assert_eq!(file.status, DiffFileStatus::Modified);
        assert!(file.mode_change_only);
        assert_eq!((file.old_mode.as_deref(), file.new_mode.as_deref()), (Some("100644"), Some("100755")));
        let file = one("diff --git a/link b/link\nold mode 120000\nnew mode 100644\n");
        assert_eq!(file.status, DiffFileStatus::TypeChanged);
    }

    #[test]
    fn binary_files_and_a_binary_payload_stay_in_their_file() {
        let file = one("diff --git a/x.png b/x.png\nindex 1111111..2222222 100644\nBinary files a/x.png and b/x.png differ\n");
        assert!(file.binary && file.hunks.is_empty());
        let files = parse("diff --git a/blob.bin b/blob.bin\nindex 1111111..2222222 100644\nGIT binary patch\nliteral 24\nYcmVn#hKq#9F^Rgz}<!{\n\nzcmVnb0jH8m0P2Q5\ndiff --git a/other.txt b/other.txt\nindex 3333333..4444444 100644\n--- a/other.txt\n+++ b/other.txt\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(files.len(), 2);
        assert!(files[0].binary && files[0].hunks.is_empty());
        assert_eq!(files[1].path, "other.txt");
        assert!(!files[1].binary);
        assert_eq!(files[1].hunks[0].lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn a_countless_hunk_header_is_one_line_each_side() {
        let file = one("diff --git a/single.txt b/single.txt\nindex 1111111..2222222 100644\n--- a/single.txt\n+++ b/single.txt\n@@ -4 +4 @@\n-old\n+new\n");
        let hunk = &file.hunks[0];
        assert_eq!((hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines), (4, 1, 4, 1));
        assert!(!file.incomplete);
    }

    #[test]
    fn no_newline_markers_reach_the_line_before_them_even_past_the_hunks_end() {
        let file = one("diff --git a/nl.txt b/nl.txt\nindex 1111111..2222222 100644\n--- a/nl.txt\n+++ b/nl.txt\n@@ -1,2 +1,2 @@\n x\n-y\n\\ No newline at end of file\n+z\n");
        let lines = &file.hunks[0].lines;
        assert!(!lines[0].no_newline);
        assert!(lines[1].no_newline && lines[1].kind == Deletion);
        assert!(!lines[2].no_newline);
        let file = one("diff --git a/nl.txt b/nl.txt\nindex 1111111..2222222 100644\n--- a/nl.txt\n+++ b/nl.txt\n@@ -1,1 +1,2 @@\n x\n+a\n\\ No newline at end of file\n");
        assert!(file.hunks[0].lines[1].no_newline);
        let file = one("diff --git a/nl.txt b/nl.txt\nindex 1111111..2222222 100644\n--- a/nl.txt\n+++ b/nl.txt\n@@ -1,2 +1,2 @@\n-a\n+A\n b\n\\ No newline at end of file\n");
        assert!(file.hunks[0].lines[2].no_newline && file.hunks[0].lines[2].kind == Context);
    }

    #[test]
    fn a_patch_that_stops_mid_hunk_is_incomplete() {
        let file = one("diff --git a/truncated.txt b/truncated.txt\nindex 1111111..2222222 100644\n--- a/truncated.txt\n+++ b/truncated.txt\n@@ -1,5 +1,5 @@\n a\n b\n");
        assert!(file.incomplete);
        assert_eq!(file.hunks[0].lines.len(), 2);
    }

    #[test]
    fn keeps_a_trailing_cr_on_a_body_line_while_still_parsing_the_structure() {
        let file = one("diff --git a/crlf.txt b/crlf.txt\nindex 1111111..2222222 100644\n--- a/crlf.txt\n+++ b/crlf.txt\r\n@@ -1,2 +1,2 @@\n-old\r\n+new\n ctx");
        assert_eq!(file.path, "crlf.txt");
        assert_eq!(file.hunks[0].lines[0], line(Deletion, Some(1), None, "old\r"));
        assert_eq!(file.hunks[0].lines[1].text, "new");
        assert_eq!(file.hunks[0].lines[2].text, "ctx");
    }

    #[test]
    fn refuses_to_number_a_combined_diff() {
        let file = one("diff --git a/merged.txt b/merged.txt\nindex 1111111..2222222 100644\n--- a/merged.txt\n+++ b/merged.txt\n@@@ -1,2 -1,2 +1,2 @@@\n a\n b\n");
        assert!(file.hunks.is_empty() && file.incomplete);
    }

    #[test]
    fn a_fallback_path_replaces_a_single_files_path_and_leaves_several_alone() {
        assert_eq!(parse_unified_diff(MODIFIED, Some("docs/readme.md"))[0].path, "docs/readme.md");
        let two = "diff --git a/one.txt b/one.txt\n--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/two.txt b/two.txt\n--- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-alpha\n+beta\n";
        assert_eq!(parse_unified_diff(two, Some("docs/readme.md"))[0].path, "one.txt");
    }

    #[test]
    fn garbage_is_nothing_and_a_bare_header_still_names_a_file() {
        assert!(parse("").is_empty());
        assert!(parse("hello\nworld").is_empty());
        assert!(parse("@@ -1 +1 @@").is_empty());
        let file = one("diff --git a/x.txt b/x.txt\n");
        assert_eq!((file.path.as_str(), file.status, file.hunks.len()), ("x.txt", DiffFileStatus::Modified, 0));
    }

    #[test]
    fn a_bare_empty_line_in_a_hunk_body_is_an_empty_context_line() {
        let file = one("diff --git a/blanks.txt b/blanks.txt\nindex 1111111..2222222 100644\n--- a/blanks.txt\n+++ b/blanks.txt\n@@ -1,2 +1,3 @@\n+a\n\n ctx\n");
        assert_eq!(file.hunks[0].lines[1], line(Context, Some(1), Some(2), ""));
        assert_eq!(file.hunks[0].lines[2], line(Context, Some(2), Some(3), "ctx"));
    }

    #[test]
    fn unquotes_git_paths() {
        assert_eq!(unquote_git_path("src/lib/app.ts"), "src/lib/app.ts");
        assert_eq!(unquote_git_path("\"a/with space\""), "a/with space");
        assert_eq!(unquote_git_path("\"a/tab\\there\""), "a/tab\there");
        assert_eq!(unquote_git_path("\"a/\\346\\226\\207\\344\\273\\266.txt\""), "a/文件.txt");
    }

    #[test]
    fn uses_quoted_paths_from_real_headers() {
        let file = one("diff --git \"a/with space.txt\" \"b/with space.txt\"\nindex 1111111..2222222 100644\n--- \"a/with space.txt\"\n+++ \"b/with space.txt\"\n@@ -1 +1 @@\n-x\n+y\n");
        assert_eq!(file.path, "with space.txt");
        let file = one("diff --git \"a/\\346\\226\\207\\344\\273\\266.txt\" \"b/\\346\\226\\207\\344\\273\\266.txt\"\nindex 1111111..2222222 100644\n--- \"a/\\346\\226\\207\\344\\273\\266.txt\"\n+++ \"b/\\346\\226\\207\\344\\273\\266.txt\"\n@@ -1 +1 @@\n-x\n+y\n");
        assert_eq!(file.path, "文件.txt");
    }
}
