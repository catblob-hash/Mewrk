//! Project memory of a workspace on another machine.
//!
//! The project memory of a workspace on a WSL distribution or an SSH machine
//! lives where a local workspace's does: in the workspace folder's
//! `.mewrk/memory/`, on that machine, shared by every conversation of the
//! workspace. [`crate::mewrk_memory`] keeps every rule — document names, the
//! index, what creating and editing mean; this module only carries the four
//! operations those rules need over the machine's shell, in the POSIX dialect
//! here and the PowerShell one in [`crate::remote_powershell`]: a snapshot of
//! the index and the documents, reading one document, a compare-and-swap
//! write, and removing a document the host itself just wrote.
//!
//! Nothing is locked on the machine. A lock file there would be an untracked
//! change in the user's repository, and one left behind by a process that
//! died would block every later write. Writes are optimistic instead: each
//! names the fingerprint of the file it expects to replace (or `absent`), and
//! a file that changed since is refused rather than overwritten. Within this
//! process a mutex per workspace keeps conversations from racing each other.
//!
//! As everywhere else, a link or anything but a real folder standing where
//! `.mewrk` or `.mewrk/memory` should be, or a link in place of a document,
//! is refused, never followed.

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use crate::{
    cancel::CancelSignal,
    remote_files::{RemoteShell, SHELL_HELPERS},
    run_environment::{self, quote_remote_path, sh_single_quote, RemoteCommandOutput},
    shell_backend::ScriptDialect,
    workspace_set::ResolvedWorkspace,
};

const HEADER: &str = "mewrk-memory 1";

/// One memory operation moves a few kilobytes; a machine that takes longer
/// than this is not answering.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Exit codes the scripts reserve, as the file tools reserve them.
const EXIT_ROOT_MISSING: i32 = 64;
const EXIT_NOT_FOUND: i32 = 66;
const EXIT_OCCUPIED: i32 = 67;
const EXIT_TOO_LARGE: i32 = 68;
const EXIT_CHANGED: i32 = 69;

/// The expected fingerprint of a file that must not exist yet.
pub(crate) const ABSENT: &str = "absent";

/// A workspace on another machine whose `.mewrk/memory/` is a memory tier.
#[derive(Clone)]
pub(crate) struct RemoteMemoryPlace {
    /// The machine's transport. A trait object so the tests can stand a local
    /// shell in for the machine.
    pub shell: Arc<dyn RemoteShell + Send + Sync>,
    /// `run_environment::env_key` of the machine.
    pub machine: String,
    /// The machine as people know it, for messages.
    pub label: String,
    /// The workspace folder, spelled as the workspace records it.
    pub root: String,
}

impl RemoteMemoryPlace {
    /// The project memory of `workspace`: its registered folder's, so a
    /// conversation's worktree on that machine shares it with the project.
    pub(crate) fn of_workspace(workspace: &ResolvedWorkspace) -> Self {
        Self {
            shell: Arc::new(workspace.runner.clone()),
            machine: run_environment::env_key(workspace.machine.as_ref()),
            label: crate::remote_capabilities::machine_label(&workspace.runner),
            root: if workspace.is_worktree {
                workspace.env_path.clone()
            } else {
                workspace.root.clone()
            },
        }
    }

    fn dialect(&self) -> ScriptDialect {
        self.shell.dialect()
    }

    /// Whether the machine's file names ignore case, as a Windows machine's
    /// do.
    pub(crate) fn names_ignore_case(&self) -> bool {
        self.dialect() == ScriptDialect::PowerShell
    }
}

impl fmt::Debug for RemoteMemoryPlace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteMemoryPlace")
            .field("machine", &self.machine)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl PartialEq for RemoteMemoryPlace {
    fn eq(&self, other: &Self) -> bool {
        self.machine == other.machine && self.root == other.root
    }
}

impl Eq for RemoteMemoryPlace {}

/// The one mutex of a workspace's memory in this process.
pub(crate) fn mutation_lock(place: &RemoteMemoryPlace) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<(String, String), Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry((place.machine.clone(), place.root.clone()))
        .or_default()
        .clone()
}

/// What a tier holds right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// `.mewrk` or `.mewrk/memory` is a link or not a folder.
    pub occupied: bool,
    pub index: Index,
    /// Regular, unlinked `*.md` files in `.mewrk/memory/`, the index aside.
    pub documents: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Index {
    Absent,
    /// The index and its fingerprint. `None` bytes for one over the size
    /// limit, which reads as empty, as a local one does.
    Present {
        fingerprint: String,
        bytes: Option<Vec<u8>>,
    },
    /// A link or something other than a file stands in its place.
    Unusable,
}

impl Index {
    /// What a write replacing it has to expect.
    pub(crate) fn expected(&self) -> &str {
        match self {
            Self::Present { fingerprint, .. } => fingerprint,
            Self::Absent | Self::Unusable => ABSENT,
        }
    }
}

/// Why a write did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WriteError {
    /// The file is not the one expected: it changed, or appeared, or went.
    Changed,
    /// A link, or something other than a folder or a file, stands in the way.
    Occupied,
    Failed(String),
}

pub(crate) fn snapshot(place: &RemoteMemoryPlace, max_index: usize) -> Result<Snapshot, String> {
    let script = match place.dialect() {
        ScriptDialect::Posix => format!("{}{}", posix_prologue(place)?, posix_snapshot(max_index)),
        ScriptDialect::PowerShell => {
            check_operand(&place.root)?;
            crate::remote_powershell::memory_snapshot(&place.root, max_index)
        }
    };
    let mut failure = String::new();
    // Once more when the answer comes back cut short: the index changed size
    // between being measured and being sent.
    for _ in 0..2 {
        let output = run(place, &script, None)?;
        if output.status != Some(0) {
            return Err(failed(place, &output));
        }
        match parse_snapshot(&output.stdout) {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => failure = error,
        }
    }
    Err(unreachable(place, &failure))
}

/// One document's fingerprint and bytes, or `None` when there is no such
/// document — a link in its place included.
pub(crate) fn read(
    place: &RemoteMemoryPlace,
    name: &str,
    max_bytes: usize,
) -> Result<Option<(String, Vec<u8>)>, String> {
    let script = match place.dialect() {
        ScriptDialect::Posix => format!(
            "{}{}",
            posix_prologue(place)?,
            posix_read(name, max_bytes)
        ),
        ScriptDialect::PowerShell => {
            check_operand(&place.root)?;
            crate::remote_powershell::memory_read(&place.root, name, max_bytes)
        }
    };
    let output = run(place, &script, None)?;
    match output.status {
        Some(0) => {}
        Some(EXIT_NOT_FOUND) => return Ok(None),
        Some(EXIT_TOO_LARGE) => {
            return Err(format!(
                "Memory document {name} exceeds the {max_bytes}-byte limit"
            ))
        }
        _ => return Err(failed(place, &output)),
    }
    let mut cursor = Cursor {
        rest: &output.stdout,
    };
    let fingerprint = cursor.line()?;
    let size = cursor.count()?;
    let bytes = cursor.bytes(size)?;
    if !fingerprint_is_sane(&fingerprint) || !cursor.rest.is_empty() {
        return Err(unreachable(place, &incomplete()));
    }
    Ok(Some((fingerprint, bytes)))
}

/// Puts `content` in `.mewrk/memory/<name>` if that file is still what
/// `expected` says — its fingerprint, or [`ABSENT`] — creating the folders it
/// needs, and returns the new fingerprint.
pub(crate) fn write(
    place: &RemoteMemoryPlace,
    name: &str,
    expected: &str,
    content: &[u8],
) -> Result<String, WriteError> {
    if !fingerprint_is_sane(expected) {
        return Err(WriteError::Failed(
            "the expected fingerprint is not one a machine reports".to_owned(),
        ));
    }
    let script = match place.dialect() {
        ScriptDialect::Posix => posix_prologue(place)
            .map(|prologue| format!("{prologue}{}", posix_write(name, expected)))
            .map_err(WriteError::Failed)?,
        ScriptDialect::PowerShell => {
            check_operand(&place.root).map_err(WriteError::Failed)?;
            crate::remote_powershell::memory_write(&place.root, name, expected)
        }
    };
    let output = run(place, &script, Some(content)).map_err(WriteError::Failed)?;
    match output.status {
        Some(0) => {}
        Some(EXIT_CHANGED) => return Err(WriteError::Changed),
        Some(EXIT_OCCUPIED) => return Err(WriteError::Occupied),
        _ => return Err(WriteError::Failed(failed(place, &output))),
    }
    let fingerprint = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !fingerprint_is_sane(&fingerprint) {
        return Err(WriteError::Failed(unreachable(place, &incomplete())));
    }
    Ok(fingerprint)
}

/// Removes `.mewrk/memory/<name>` if it is still exactly the file whose
/// fingerprint is given — the host undoing its own write, never deleting
/// someone else's.
pub(crate) fn remove(place: &RemoteMemoryPlace, name: &str, fingerprint: &str) -> Result<(), String> {
    if !fingerprint_is_sane(fingerprint) {
        return Err("the fingerprint is not one a machine reports".to_owned());
    }
    let script = match place.dialect() {
        ScriptDialect::Posix => format!(
            "{}{}",
            posix_prologue(place)?,
            posix_remove(name, fingerprint)
        ),
        ScriptDialect::PowerShell => {
            check_operand(&place.root)?;
            crate::remote_powershell::memory_remove(&place.root, name, fingerprint)
        }
    };
    let output = run(place, &script, None)?;
    if output.status == Some(0) {
        Ok(())
    } else {
        Err(failed(place, &output))
    }
}

fn run(
    place: &RemoteMemoryPlace,
    script: &str,
    stdin: Option<&[u8]>,
) -> Result<RemoteCommandOutput, String> {
    place
        .shell
        .run(script, stdin, TIMEOUT, &CancelSignal::default())
        .map_err(|error| unreachable(place, &error))
}

fn unreachable(place: &RemoteMemoryPlace, reason: &str) -> String {
    format!(
        "Could not reach the project memory of workspace {} on {}: {reason}",
        place.root, place.label
    )
}

fn failed(place: &RemoteMemoryPlace, output: &RemoteCommandOutput) -> String {
    let reason = match output.status {
        Some(EXIT_ROOT_MISSING) => "the workspace folder cannot be entered".to_owned(),
        Some(EXIT_OCCUPIED) => {
            "the memory folder is occupied by a file or link with the same name".to_owned()
        }
        status => run_environment::legible_remote_reply(&output.stderr)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("exit {status:?}")),
    };
    unreachable(place, &reason)
}

fn check_operand(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.chars().any(char::is_control) {
        return Err("The workspace folder is empty or contains control characters".to_owned());
    }
    Ok(())
}

/// A fingerprint goes back into the next script as a quoted operand; it is
/// machine output, but only digits, letters and spaces are let through.
fn fingerprint_is_sane(fingerprint: &str) -> bool {
    !fingerprint.is_empty()
        && fingerprint.len() <= 128
        && fingerprint
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == ' ')
}

// ---------------------------------------------------------------------------
// The POSIX scripts
// ---------------------------------------------------------------------------

/// Enters the workspace and names the tier: `$W` is `.mewrk`, `$M` its
/// `memory`. `kind` answers 0 (nothing), 1 (a real folder), 2 (a link),
/// 3 (a regular file) or 4 (anything else); `fp` is a file's fingerprint,
/// its modification second beside `cksum`'s checksum and size. Globbing is
/// switched on, which zsh's `sh` emulation starts with off (its `-f`), for
/// listing the documents; every operand is quoted.
fn posix_prologue(place: &RemoteMemoryPlace) -> Result<String, String> {
    check_operand(&place.root)?;
    Ok(format!(
        r#"set +f
cd -- {root} 2>/dev/null || {{ printf '%s\n' 'cannot enter the workspace root' >&2; exit 64; }}
R=$(pwd -P)
{SHELL_HELPERS}W="$R/.mewrk"
M="$W/memory"
nl='
'
kind() {{ if [ -L "$1" ]; then echo 2; elif [ -d "$1" ]; then echo 1; elif [ -f "$1" ]; then echo 3; elif [ -e "$1" ]; then echo 4; else echo 0; fi; }}
fp() {{ printf '%s %s\n' "$(digits "$(mtime "$1")")" "$(cksum < "$1")"; }}
"#,
        root = quote_remote_path(place.root.trim()),
    ))
}

/// The snapshot: `occupied`, `empty` (no memory folder yet) or `ready`, then
/// for a ready tier the index — `absent`, `other`, `index` with fingerprint,
/// size and bytes, or `large` with only the fingerprint — and the document
/// names, one per line, until `E`.
fn posix_snapshot(max_index: usize) -> String {
    format!(
        r#"printf '%s\n' '{HEADER}'
w=$(kind "$W"); m=0
[ "$w" != 1 ] || m=$(kind "$M")
case "$w$m" in 00|10) printf 'empty\nE\n'; exit 0 ;; 11) ;; *) printf 'occupied\nE\n'; exit 0 ;; esac
printf 'ready\n'
I="$M/MEMORY.md"
case $(kind "$I") in
0) printf 'absent\n' ;;
3) n=$(digits "$(fsize "$I")")
   if [ "$n" -le {max_index} ]; then printf 'index\n%s\n%s\n' "$(fp "$I")" "$n"; cat -- "$I"; else printf 'large\n%s\n' "$(fp "$I")"; fi ;;
*) printf 'other\n' ;;
esac
for f in "$M"/*.md; do
  [ "$(kind "$f")" = 3 ] || continue
  n=${{f##*/}}
  case $n in *"$nl"*|MEMORY.md) continue ;; esac
  printf '%s\n' "$n"
done
printf 'E\n'
"#
    )
}

/// One document: 66 when there is no such document (or a link stands in
/// for it or for a folder on the way), 68 when it is too large, otherwise
/// its fingerprint, size and bytes.
fn posix_read(name: &str, max_bytes: usize) -> String {
    format!(
        r#"D="$M"/{name}
[ "$(kind "$W")" = 1 ] && [ "$(kind "$M")" = 1 ] && [ "$(kind "$D")" = 3 ] || exit 66
n=$(digits "$(fsize "$D")")
[ "$n" -le {max_bytes} ] || exit 68
printf '%s\n%s\n' "$(fp "$D")" "$n"
cat -- "$D"
"#,
        name = sh_single_quote(name),
    )
}

/// The compare-and-swap write: 67 when a link or a non-folder stands where a
/// folder goes, or anything but a regular file where the document is, 69
/// when the document is not what `expected` says, 70 when writing fails;
/// otherwise the new fingerprint. Standard input goes to a temporary file
/// beside the document and is renamed into place.
fn posix_write(name: &str, expected: &str) -> String {
    format!(
        r#"D="$M"/{name}
FP={expected}
mk() {{ case $(kind "$1") in 1) return 0 ;; 0) ;; *) exit 67 ;; esac; mkdir -- "$1" 2>/dev/null; [ "$(kind "$1")" = 1 ] || exit 70; }}
mk "$W"
mk "$M"
k=$(kind "$D")
if [ "$FP" = absent ]; then
  [ "$k" = 0 ] || exit 69
else
  [ "$k" != 0 ] || exit 69
  [ "$k" = 3 ] || exit 67
  [ "$(fp "$D")" = "$FP" ] || exit 69
fi
T="$M/.mewrk-write.$$"
cat > "$T" || {{ rm -f -- "$T"; exit 70; }}
mv -f -- "$T" "$D" || {{ rm -f -- "$T"; exit 70; }}
fp "$D"
"#,
        name = sh_single_quote(name),
        expected = sh_single_quote(expected),
    )
}

fn posix_remove(name: &str, fingerprint: &str) -> String {
    format!(
        r#"D="$M"/{name}
case "$(kind "$W")$(kind "$M")" in *2*) exit 67 ;; esac
case $(kind "$D") in 0) exit 0 ;; 3) ;; *) exit 67 ;; esac
[ "$(fp "$D")" = {fingerprint} ] || exit 69
rm -f -- "$D"
"#,
        name = sh_single_quote(name),
        fingerprint = sh_single_quote(fingerprint),
    )
}

// ---------------------------------------------------------------------------
// Reading answers
// ---------------------------------------------------------------------------

fn incomplete() -> String {
    "the machine's answer was incomplete; try again".to_owned()
}

struct Cursor<'a> {
    rest: &'a [u8],
}

impl Cursor<'_> {
    fn line(&mut self) -> Result<String, String> {
        let end = self
            .rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(incomplete)?;
        let line = String::from_utf8(self.rest[..end].to_vec()).map_err(|_| incomplete())?;
        self.rest = &self.rest[end + 1..];
        Ok(line)
    }

    fn count(&mut self) -> Result<u64, String> {
        self.line()?.trim().parse().map_err(|_| incomplete())
    }

    fn bytes(&mut self, count: u64) -> Result<Vec<u8>, String> {
        let count = usize::try_from(count).map_err(|_| incomplete())?;
        if self.rest.len() < count {
            return Err(incomplete());
        }
        let bytes = self.rest[..count].to_vec();
        self.rest = &self.rest[count..];
        Ok(bytes)
    }
}

fn parse_snapshot(bytes: &[u8]) -> Result<Snapshot, String> {
    let mut cursor = Cursor { rest: bytes };
    if cursor.line()? != HEADER {
        return Err(incomplete());
    }
    let mut snapshot = Snapshot {
        occupied: false,
        index: Index::Absent,
        documents: BTreeSet::new(),
    };
    match cursor.line()?.as_str() {
        "empty" => {}
        "occupied" => snapshot.occupied = true,
        "ready" => {
            snapshot.index = match cursor.line()?.as_str() {
                "absent" => Index::Absent,
                "other" => Index::Unusable,
                "index" => {
                    let fingerprint = cursor.line()?;
                    let size = cursor.count()?;
                    Index::Present {
                        fingerprint,
                        bytes: Some(cursor.bytes(size)?),
                    }
                }
                "large" => Index::Present {
                    fingerprint: cursor.line()?,
                    bytes: None,
                },
                _ => return Err(incomplete()),
            };
            if let Index::Present { fingerprint, .. } = &snapshot.index {
                if !fingerprint_is_sane(fingerprint) {
                    return Err(incomplete());
                }
            }
            loop {
                let name = cursor.line()?;
                if name == "E" {
                    return Ok(snapshot);
                }
                snapshot.documents.insert(name);
            }
        }
        _ => return Err(incomplete()),
    }
    (cursor.line()? == "E")
        .then_some(snapshot)
        .ok_or_else(incomplete)
}

#[cfg(test)]
mod tests;
