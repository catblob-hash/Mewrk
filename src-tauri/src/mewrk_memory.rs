//! Two-tier, plain-Markdown memory.
//!
//! Memory here is exactly what it looks like on disk: a directory of Markdown
//! documents the model maintains itself. There is no owner identity, no
//! database, no CAS version and no revision history — a memory belongs to a
//! *location*, not to a model.
//!
//! ```text
//! ~/.mewrk/                 <project>/.mewrk/
//!   memory/                    memory/
//!     MEMORY.md                  MEMORY.md        <- host-owned index
//!     <topic>.md                 <topic>.md       <- model-owned documents
//! ```
//!
//! Each enabled tier's `MEMORY.md` goes into the conversation context — the two
//! tiers switch on and off independently. Topic documents do not: the model
//! reads them on demand through the read tool. The `MEWRK.md` beside each
//! `memory/` is not memory: it is a standing instruction file, read for every
//! run whether memory is on or not, and only by the instruction loader
//! ([`crate::project_memory`]), so it reaches the model exactly once.
//!
//! `MEMORY.md` is deliberately **not** reachable by any tool. The host owns it
//! and rewrites it from the index descriptions supplied to the create/edit
//! tools, so the model can never desynchronize the index from the documents it
//! describes, and a write can never silently change what the next run loads.

use crate::{
    memory_archive_file::{read_bounded_nofollow, write_all_nofollow},
    model::JsonObject,
    prompt_profile::{PromptKey, PromptProfile},
};
use std::{
    fs,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

/// Filename of the host-owned index inside each tier's `memory/` directory.
pub const INDEX_NAME: &str = "MEMORY.md";
/// Directory holding the index and every topic document.
pub const MEMORY_DIR: &str = "memory";
/// Root directory of a memory tier, under the home directory or the workspace.
pub const MEWRK_DIR: &str = ".mewrk";

/// Largest single document accepted, in bytes. Generous for prose, small
/// enough that a runaway write cannot exhaust the context or the disk.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Largest index accepted. The index is pure pointers, so it stays small.
pub const MAX_INDEX_BYTES: usize = 64 * 1024;
/// Longest accepted document name, including the `.md` suffix.
const MAX_NAME_CHARS: usize = 120;
/// Longest accepted one-line index description.
const MAX_DESCRIPTION_CHARS: usize = 300;

/// Which tier a memory operation addresses.
///
/// The two tiers are independent directories with identical structure. Neither
/// shadows the other: both are loaded, and a document name may exist in both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryTier {
    /// `~/.mewrk` — shared by every workspace on this machine.
    Global,
    /// `<workspace>/.mewrk` — scoped to the open workspace.
    Project,
}

impl MemoryTier {
    pub fn prompt_key(self) -> PromptKey {
        match self {
            Self::Global => PromptKey::MemoryTierGlobal,
            Self::Project => PromptKey::MemoryTierProject,
        }
    }

    /// Human-facing tier label used in errors.
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global memory",
            Self::Project => "project memory",
        }
    }
}

/// A resolved, existing-or-creatable memory tier root.
///
/// Holding one of these means the host — not the model — chose the directory.
/// Tools name documents; they never supply paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryRoot {
    tier: MemoryTier,
    place: MemoryPlace,
}

/// Where a tier's `.mewrk` directory is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MemoryPlace {
    /// On this computer: `~/.mewrk`, or a local workspace's.
    Local(PathBuf),
    /// In the folder of a workspace on another machine, reached through that
    /// machine's shell ([`crate::remote_memory`]).
    Remote(crate::remote_memory::RemoteMemoryPlace),
}

impl MemoryRoot {
    pub fn tier(&self) -> MemoryTier {
        self.tier
    }

    /// The tier's `.mewrk` directory: on this computer, or for a tier on
    /// another machine its path there.
    fn root(&self) -> PathBuf {
        match &self.place {
            MemoryPlace::Local(root) => root.clone(),
            MemoryPlace::Remote(place) => Path::new(&place.root).join(MEWRK_DIR),
        }
    }

    fn remote(&self) -> Option<&crate::remote_memory::RemoteMemoryPlace> {
        match &self.place {
            MemoryPlace::Local(_) => None,
            MemoryPlace::Remote(place) => Some(place),
        }
    }

    /// `<root>/memory`
    pub fn memory_dir(&self) -> PathBuf {
        self.root().join(MEMORY_DIR)
    }

    /// `<root>/memory/MEMORY.md`
    pub fn index_path(&self) -> PathBuf {
        self.memory_dir().join(INDEX_NAME)
    }

    /// `<root>/memory/<name>` for an already-validated document name.
    fn document_path(&self, validated_name: &str) -> PathBuf {
        self.memory_dir().join(validated_name)
    }
}

/// Resolves the global tier at `~/.mewrk`.
///
/// Returns `None` when the platform reports no home directory, which makes
/// global memory unavailable rather than falling back to another location.
pub fn global_root(home: Option<&Path>) -> Option<MemoryRoot> {
    home.map(|home| MemoryRoot {
        tier: MemoryTier::Global,
        place: MemoryPlace::Local(home.join(MEWRK_DIR)),
    })
}

/// Resolves the project tier at `<workspace>/.mewrk`.
///
/// Returns `None` for a workspace with no stable directory (a temporary or
/// unsupported workspace), so project memory fails closed instead of leaking
/// into the global tier.
pub fn project_root(workspace: Option<&Path>) -> Option<MemoryRoot> {
    workspace.map(|workspace| MemoryRoot {
        tier: MemoryTier::Project,
        place: MemoryPlace::Local(workspace.join(MEWRK_DIR)),
    })
}

/// The project tier of a workspace on another machine: `.mewrk` in that
/// workspace's folder there, shared by every conversation of the workspace
/// and still there after any of them is deleted.
pub(crate) fn remote_project_root(place: crate::remote_memory::RemoteMemoryPlace) -> MemoryRoot {
    MemoryRoot {
        tier: MemoryTier::Project,
        place: MemoryPlace::Remote(place),
    }
}

/// Normalizes a model-supplied document name to a safe `<name>.md` leaf.
///
/// The model may write `notes`, `notes.md` or `Notes.MD`; all resolve to the
/// same document. Anything that could escape the memory directory — a path
/// separator, a drive letter, `..`, a NUL, a leading dot — is rejected rather
/// than sanitized, so a rejected name never silently becomes a different one.
pub fn normalize_document_name(raw: &str) -> Result<String, String> {
    let normalized = normalize_named_document(raw, "Memory document")?;
    if normalized.eq_ignore_ascii_case(INDEX_NAME) {
        return Err(format!(
            "{INDEX_NAME} is the host-managed memory index and cannot be read or written directly; provide its description when creating or editing memory"
        ));
    }
    Ok(normalized)
}

/// The rules of [`normalize_document_name`] for any directory of named Markdown
/// documents, with `noun` naming the kind of document in every error. The
/// caller rejects its own host-owned index name.
pub(crate) fn normalize_named_document(raw: &str, noun: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{noun} name must not be empty"));
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "{noun} name must not exceed {MAX_NAME_CHARS} characters"
        ));
    }

    // Strip an optional, case-insensitive `.md` suffix before validating the
    // stem, so `.md` itself (an empty stem) is rejected like any other empty
    // name instead of becoming a dotfile.
    let stem = match trimmed.len().checked_sub(3) {
        Some(cut) if trimmed[cut..].eq_ignore_ascii_case(".md") => &trimmed[..cut],
        _ => trimmed,
    };
    if stem.is_empty() {
        return Err(format!("{noun} name must not be empty"));
    }

    if stem.contains('/') || stem.contains('\\') {
        return Err(format!(
            "{noun} name must be a single file name, without path separators"
        ));
    }
    if stem.contains(':') {
        return Err(format!("{noun} name must not contain a drive or data-stream separator"));
    }
    if stem.contains('\0') {
        return Err(format!("{noun} name must not contain a NUL character"));
    }
    if stem.starts_with('.') {
        return Err(format!("{noun} name must not start with a dot"));
    }
    if stem.chars().any(|character| character.is_control()) {
        return Err(format!("{noun} name must not contain control characters"));
    }
    // `.` and `..` are already excluded by the leading-dot rule; this covers
    // trailing-dot and trailing-space names, which Windows silently truncates
    // and would therefore resolve to a different file than the one named.
    if stem.ends_with('.') || stem.ends_with(' ') {
        return Err(format!("{noun} name must not end with a dot or space"));
    }
    if stem
        .chars()
        .any(|character| matches!(character, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err(format!(
            "{noun} name must not contain reserved characters: < > \" | ? *"
        ));
    }
    // Windows reserved device names (CON, NUL, COM1, and others) resolve as
    // devices rather than files. Reject their case-insensitive stem before any
    // dot suffix.
    let device_stem = stem.split('.').next().unwrap_or(stem);
    let is_reserved_device = matches!(
        device_stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
    ) || {
        let upper = device_stem.to_ascii_uppercase();
        (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0'
    };
    if is_reserved_device {
        return Err(format!("{noun} name must not use a Windows reserved device name (CON, NUL, COM1, and similar names)"));
    }

    Ok(format!("{stem}.md"))
}

/// Validates the one-line index description supplied with a write.
fn normalize_description(raw: &str) -> Result<String, String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err("Memory index description must not be empty; describe what this memory records in one sentence".into());
    }
    if collapsed.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "Memory index description must not exceed {MAX_DESCRIPTION_CHARS} characters; it is only an index entry, so put the full text in the memory document"
        ));
    }
    Ok(collapsed)
}

/// One entry of a tier's index: a document and its one-line description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub description: String,
}

/// Reads a tier's index into entries. A missing or unreadable index is an
/// empty index: memory is best-effort context, never a hard runtime failure.
///
/// Entries whose document file no longer exists are dropped here: an external
/// editor may delete or rename a topic file without touching `MEMORY.md`, and
/// a dangling entry would keep describing a document the read tool cannot
/// open. The cleaned list persists naturally on the next index rewrite.
pub fn read_index(root: &MemoryRoot) -> Vec<IndexEntry> {
    if let Some(place) = root.remote() {
        return crate::remote_memory::snapshot(place, MAX_INDEX_BYTES)
            .map(|snapshot| remote_index_entries(place, &snapshot, None))
            .unwrap_or_default();
    }
    let Ok(bytes) = read_bounded_nofollow(&root.index_path(), MAX_INDEX_BYTES) else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut entries = parse_index(&text);
    entries.retain(|entry| fs::symlink_metadata(root.document_path(&entry.name)).is_ok());
    entries
}

/// Parses the `- [name](name) — description` lines the host writes.
///
/// Unrecognized lines are skipped rather than treated as an error, so a
/// hand-edited index degrades to the entries that are still well-formed.
fn parse_index(text: &str) -> Vec<IndexEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("- [") else {
            continue;
        };
        let Some((label, rest)) = rest.split_once("](") else {
            continue;
        };
        let Some((_target, rest)) = rest.split_once(')') else {
            continue;
        };
        let Ok(name) = normalize_document_name(label) else {
            continue;
        };
        let description = rest
            .trim_start()
            .trim_start_matches('—')
            .trim_start_matches('-')
            .trim()
            .to_owned();
        if entries.iter().any(|entry: &IndexEntry| entry.name == name) {
            continue;
        }
        entries.push(IndexEntry { name, description });
    }
    entries
}

/// Renders entries back into the index document.
fn render_index(tier: MemoryTier, entries: &[IndexEntry]) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(tier.label());
    out.push_str(" index\n\n");
    if entries.is_empty() {
        out.push_str("(no memory documents yet)\n");
        return out;
    }
    out.push_str("<!-- Maintained automatically by Mewrk: descriptions supplied when memory is created or edited are recorded here. -->\n\n");
    for entry in entries {
        out.push_str("- [");
        out.push_str(&entry.name);
        out.push_str("](");
        out.push_str(&entry.name);
        out.push(')');
        if !entry.description.is_empty() {
            out.push_str(" — ");
            out.push_str(&entry.description);
        }
        out.push('\n');
    }
    out
}

/// Rewrites the index so `name` carries `description`, preserving the order of
/// existing entries and appending a genuinely new document at the end.
fn upsert_index_entry(root: &MemoryRoot, name: &str, description: &str) -> Result<(), String> {
    let mut entries = read_index(root);
    match entries.iter_mut().find(|entry| entry.name == name) {
        Some(existing) => existing.description = description.to_owned(),
        None => entries.push(IndexEntry {
            name: name.to_owned(),
            description: description.to_owned(),
        }),
    }
    write_index(root, &entries)
}

fn write_index(root: &MemoryRoot, entries: &[IndexEntry]) -> Result<(), String> {
    let rendered = render_index(root.tier(), entries);
    if rendered.len() > MAX_INDEX_BYTES {
        return Err(format!(
            "Memory index exceeds the {MAX_INDEX_BYTES}-byte limit; delete or merge some memory documents first"
        ));
    }
    ensure_memory_dir(root)?;
    write_all_nofollow(&root.index_path(), rendered.as_bytes(), MAX_INDEX_BYTES)
        .map_err(|_| "Could not write the memory index".to_owned())
}

fn ensure_memory_dir(root: &MemoryRoot) -> Result<(), String> {
    let directory = root.memory_dir();
    // `create_dir_all` succeeds on an existing directory. A pre-existing
    // non-directory (or a link pointing at one) is rejected here, and the
    // no-follow write below rejects a linked document even if this passes.
    if let Ok(metadata) = fs::symlink_metadata(&directory) {
        if !metadata.is_dir() {
            return Err("Memory directory is occupied by a file or link with the same name".into());
        }
    }
    fs::create_dir_all(&directory).map_err(|_| "Could not create the memory directory".to_owned())
}

/// Filename of the mutation lock earlier builds kept inside each tier's
/// `memory/` directory, removed when it is found.
const LEGACY_MUTATION_LOCK_NAME: &str = ".memory.lock";

/// Takes the tier's cross-process mutation lock for one read→rewrite section.
///
/// Two Mewrk instances (for example production and browser-dev) share the
/// same user-level `~/.mewrk` tree, and every mutation here is a
/// read-modify-write of `MEMORY.md` plus a body file. The in-process storage
/// lock cannot see the other process, so concurrent mutations silently lost
/// updates. The lock is blocking — mutations are tiny, so waiting beats
/// failing — and releases when the returned handle drops.
///
/// The lock file lives in the user's cache directory, named after the tier's
/// canonical directory, never inside the tier: project memory sits in the
/// user's repository, where a lock file is an untracked change that `git
/// status` keeps reporting.
fn acquire_mutation_lock(root: &MemoryRoot) -> Result<File, String> {
    use sha2::{Digest, Sha256};

    ensure_memory_dir(root)?;
    let directory = fs::canonicalize(root.memory_dir())
        .map_err(|_| "Could not resolve the memory directory".to_owned())?;
    let locks = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mewrk")
        .join("memory-locks");
    fs::create_dir_all(&locks)
        .map_err(|_| "Could not create the memory mutation lock directory".to_owned())?;
    let digest = Sha256::digest(directory.to_string_lossy().as_bytes());
    let lock_path = locks.join(format!("{}.lock", hex_prefix(&digest)));
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("Memory mutation lock path is not a regular file".into());
        }
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(&lock_path)
        .map_err(|_| "Could not open the memory mutation lock file".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "Could not validate the memory mutation lock file".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Memory mutation lock path is not a regular file".into());
    }
    fs2::FileExt::lock_exclusive(&file)
        .map_err(|_| "Could not acquire the memory mutation lock".to_owned())?;
    // Under the new lock, so no writer of this build is mid-mutation; a
    // regular file only, so a link of that name is left for the user to see.
    let legacy = root.memory_dir().join(LEGACY_MUTATION_LOCK_NAME);
    if fs::symlink_metadata(&legacy).is_ok_and(|metadata| metadata.is_file()) {
        let _ = fs::remove_file(legacy);
    }
    Ok(file)
}

/// The first 16 bytes of a digest as lowercase hex: plenty to keep the lock
/// files of different directories apart, short enough for any filesystem.
fn hex_prefix(digest: &[u8]) -> String {
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Result of reading one memory document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryDocument {
    pub tier: MemoryTier,
    pub name: String,
    pub content: String,
}

/// Reads one topic document by name.
pub fn read_document(root: &MemoryRoot, raw_name: &str) -> Result<MemoryDocument, String> {
    let name = normalize_document_name(raw_name)?;
    let missing = || {
        format!(
            "No memory document named {name} exists in {}",
            root.tier().label()
        )
    };
    let bytes = match root.remote() {
        Some(place) => {
            crate::remote_memory::read(place, &name, MAX_DOCUMENT_BYTES)?
                .ok_or_else(missing)?
                .1
        }
        None => read_bounded_nofollow(&root.document_path(&name), MAX_DOCUMENT_BYTES)
            .map_err(|_| missing())?,
    };
    let content = String::from_utf8(bytes)
        .map_err(|_| format!("Memory document {name} is not valid UTF-8 text"))?;
    Ok(MemoryDocument {
        tier: root.tier(),
        name,
        content,
    })
}

/// Creates a new document and records its index description.
///
/// Refuses to overwrite an existing document: creation and modification stay
/// distinct so an accidental re-create cannot silently discard prior content.
pub fn create_document(
    root: &MemoryRoot,
    raw_name: &str,
    content: &str,
    raw_description: &str,
) -> Result<MemoryDocument, String> {
    let name = normalize_document_name(raw_name)?;
    let description = normalize_description(raw_description)?;
    validate_content(content)?;
    if let Some(place) = root.remote() {
        return create_remote_document(root.tier(), place, name, content, &description);
    }

    // The existence check through index rewrite is a cross-process critical section.
    let _mutation_lock = acquire_mutation_lock(root)?;
    let path = root.document_path(&name);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(format!(
            "{name} already exists in {}; use the edit-memory tool or choose another name",
            root.tier().label()
        ));
    }

    ensure_memory_dir(root)?;
    write_all_nofollow(&path, content.as_bytes(), MAX_DOCUMENT_BYTES)
        .map_err(|_| format!("Could not write memory document {name}"))?;
    // A document the index does not mention is invisible to the next run, so
    // an index failure un-creates the document rather than leaving an
    // orphan the model has no way to discover.
    if let Err(error) = upsert_index_entry(root, &name, &description) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(MemoryDocument {
        tier: root.tier(),
        name,
        content: content.to_owned(),
    })
}

/// Replaces one exact substring in an existing document, and refreshes its
/// index description.
///
/// The match must be unique, mirroring the file-editing tools: a non-unique
/// `old_text` is an ambiguous edit, not a request to change the first hit.
pub fn edit_document(
    root: &MemoryRoot,
    raw_name: &str,
    old_text: &str,
    new_text: &str,
    raw_description: &str,
) -> Result<MemoryDocument, String> {
    let name = normalize_document_name(raw_name)?;
    let description = normalize_description(raw_description)?;
    if old_text.is_empty() {
        return Err(
            "Text to replace must not be empty; use the create-memory tool for a new memory".into(),
        );
    }
    if old_text == new_text {
        return Err(
            "The old and replacement text are identical; there is no change to write".into(),
        );
    }
    if let Some(place) = root.remote() {
        return edit_remote_document(root.tier(), place, name, old_text, new_text, &description);
    }

    // Reading, modifying, and rewriting the body and index is a cross-process critical section.
    let _mutation_lock = acquire_mutation_lock(root)?;
    let existing = read_document(root, &name)?;
    let content = replace_once(&name, &existing.content, old_text, new_text)?;
    // Update the index first. The document already exists and is already
    // listed, so a failure here leaves the prior description in place and the
    // body untouched — consistent, just not yet edited. Writing the body first
    // could instead leave a document whose index line describes the old text.
    upsert_index_entry(root, &name, &description)?;
    write_all_nofollow(
        &root.document_path(&name),
        content.as_bytes(),
        MAX_DOCUMENT_BYTES,
    )
    .map_err(|_| format!("Could not write memory document {name}"))?;
    Ok(MemoryDocument {
        tier: root.tier(),
        name,
        content,
    })
}

/// `content` with its one occurrence of `old_text` replaced, checked as a
/// document.
fn replace_once(name: &str, content: &str, old_text: &str, new_text: &str) -> Result<String, String> {
    let occurrences = content.matches(old_text).count();
    if occurrences == 0 {
        return Err(format!(
            "Could not find the text to replace in memory document {name}"
        ));
    }
    if occurrences > 1 {
        return Err(format!(
            "The text to replace occurs {occurrences} times in memory document {name}; provide a longer unique match"
        ));
    }
    let content = content.replacen(old_text, new_text, 1);
    validate_content(&content)?;
    Ok(content)
}

// ---------------------------------------------------------------------------
// A tier on another machine
// ---------------------------------------------------------------------------
//
// The same create and edit, as optimistic steps over the machine's shell: no
// lock file there, so every write names the file it expects to replace and an
// index that changed underneath is read again and the update retried.

/// How often an index update is retried when the index keeps changing
/// underneath it — another Mewrk writing the same memory at that moment.
const REMOTE_INDEX_ATTEMPTS: usize = 4;

const OCCUPIED: &str = "Memory directory is occupied by a file or link with the same name";

/// The index entries of a snapshot whose documents exist. `created` counts
/// as existing: a document this create just wrote, which a snapshot taken
/// before the write cannot list.
fn remote_index_entries(
    place: &crate::remote_memory::RemoteMemoryPlace,
    snapshot: &crate::remote_memory::Snapshot,
    created: Option<&str>,
) -> Vec<IndexEntry> {
    let crate::remote_memory::Index::Present {
        bytes: Some(bytes), ..
    } = &snapshot.index
    else {
        return Vec::new();
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let same = |left: &str, right: &str| {
        if place.names_ignore_case() {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };
    let mut entries = parse_index(text);
    entries.retain(|entry| {
        created.is_some_and(|created| same(created, &entry.name))
            || snapshot
                .documents
                .iter()
                .any(|document| same(document, &entry.name))
    });
    entries
}

/// Rewrites the remote index so `name` carries `description`, against the
/// index `snapshot` saw; when it changed meanwhile, against a fresh one.
fn upsert_remote_index(
    tier: MemoryTier,
    place: &crate::remote_memory::RemoteMemoryPlace,
    snapshot: &mut crate::remote_memory::Snapshot,
    name: &str,
    description: &str,
) -> Result<(), String> {
    use crate::remote_memory::{Index, WriteError};

    for attempt in 1..=REMOTE_INDEX_ATTEMPTS {
        if snapshot.occupied {
            return Err(OCCUPIED.into());
        }
        if snapshot.index == Index::Unusable {
            return Err("Could not write the memory index".into());
        }
        let mut entries = remote_index_entries(place, snapshot, Some(name));
        match entries.iter_mut().find(|entry| entry.name == name) {
            Some(existing) => existing.description = description.to_owned(),
            None => entries.push(IndexEntry {
                name: name.to_owned(),
                description: description.to_owned(),
            }),
        }
        let rendered = render_index(tier, &entries);
        if rendered.len() > MAX_INDEX_BYTES {
            return Err(format!(
                "Memory index exceeds the {MAX_INDEX_BYTES}-byte limit; delete or merge some memory documents first"
            ));
        }
        match crate::remote_memory::write(place, INDEX_NAME, snapshot.index.expected(), rendered.as_bytes()) {
            Ok(_) => return Ok(()),
            Err(WriteError::Changed) if attempt < REMOTE_INDEX_ATTEMPTS => {
                *snapshot = crate::remote_memory::snapshot(place, MAX_INDEX_BYTES)?;
            }
            Err(WriteError::Changed) => {
                return Err(
                    "The memory index kept changing while it was being updated; try again".into(),
                )
            }
            Err(WriteError::Occupied) => return Err("Could not write the memory index".into()),
            Err(WriteError::Failed(error)) => {
                return Err(format!("Could not write the memory index: {error}"))
            }
        }
    }
    Err("The memory index kept changing while it was being updated; try again".into())
}

fn create_remote_document(
    tier: MemoryTier,
    place: &crate::remote_memory::RemoteMemoryPlace,
    name: String,
    content: &str,
    description: &str,
) -> Result<MemoryDocument, String> {
    use crate::remote_memory::WriteError;

    let lock = crate::remote_memory::mutation_lock(place);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let exists = || {
        format!(
            "{name} already exists in {}; use the edit-memory tool or choose another name",
            tier.label()
        )
    };
    let mut snapshot = crate::remote_memory::snapshot(place, MAX_INDEX_BYTES)?;
    if snapshot.occupied {
        return Err(OCCUPIED.into());
    }
    if snapshot.documents.contains(&name) {
        return Err(exists());
    }
    // `absent`: the write itself refuses a document that appeared since, or
    // anything else standing under that name.
    let fingerprint = match crate::remote_memory::write(
        place,
        &name,
        crate::remote_memory::ABSENT,
        content.as_bytes(),
    ) {
        Ok(fingerprint) => fingerprint,
        Err(WriteError::Changed) => return Err(exists()),
        Err(WriteError::Occupied) => return Err(OCCUPIED.into()),
        Err(WriteError::Failed(error)) => {
            return Err(format!("Could not write memory document {name}: {error}"))
        }
    };
    // As locally: a document the index does not mention is invisible to the
    // next run, so an index that cannot be updated un-creates it.
    if let Err(error) = upsert_remote_index(tier, place, &mut snapshot, &name, description) {
        let _ = crate::remote_memory::remove(place, &name, &fingerprint);
        return Err(error);
    }
    Ok(MemoryDocument {
        tier,
        name,
        content: content.to_owned(),
    })
}

fn edit_remote_document(
    tier: MemoryTier,
    place: &crate::remote_memory::RemoteMemoryPlace,
    name: String,
    old_text: &str,
    new_text: &str,
    description: &str,
) -> Result<MemoryDocument, String> {
    use crate::remote_memory::WriteError;

    let lock = crate::remote_memory::mutation_lock(place);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (fingerprint, bytes) = crate::remote_memory::read(place, &name, MAX_DOCUMENT_BYTES)?
        .ok_or_else(|| {
            format!(
                "No memory document named {name} exists in {}",
                tier.label()
            )
        })?;
    let existing = String::from_utf8(bytes)
        .map_err(|_| format!("Memory document {name} is not valid UTF-8 text"))?;
    let content = replace_once(&name, &existing, old_text, new_text)?;
    // The index first, as locally, then the body against the fingerprint it
    // was read with.
    let mut snapshot = crate::remote_memory::snapshot(place, MAX_INDEX_BYTES)?;
    upsert_remote_index(tier, place, &mut snapshot, &name, description)?;
    match crate::remote_memory::write(place, &name, &fingerprint, content.as_bytes()) {
        Ok(_) => Ok(MemoryDocument {
            tier,
            name,
            content,
        }),
        Err(WriteError::Changed) => Err(format!(
            "Memory document {name} changed while it was being edited; read it again and retry the edit"
        )),
        Err(WriteError::Occupied) => Err(format!("Could not write memory document {name}")),
        Err(WriteError::Failed(error)) => {
            Err(format!("Could not write memory document {name}: {error}"))
        }
    }
}

fn validate_content(content: &str) -> Result<(), String> {
    if content.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "Memory document exceeds the {MAX_DOCUMENT_BYTES}-byte limit; split it into multiple memories"
        ));
    }
    if content.contains('\0') {
        return Err("Memory document must not contain a NUL character".into());
    }
    Ok(())
}

/// One tier's index as the conversation context carries it, or `None` when
/// the tier has no documents.
///
/// Rendered from [`read_index`] rather than sent as the file stands, so a
/// document deleted or renamed outside Mewrk leaves the index the model sees
/// at once, without waiting for a write to rewrite `MEMORY.md`.
pub fn read_index_context(root: &MemoryRoot) -> Option<String> {
    let entries = read_index(root);
    (!entries.is_empty()).then(|| render_index(root.tier(), &entries))
}

/// Delimiters framing the memory block in the conversation context. They let
/// the host recognize and replace its own block without re-parsing prose.
pub const MEMORY_CONTEXT_START: &str = "<mewrk-memory>";
pub const MEMORY_CONTEXT_END: &str = "</mewrk-memory>";

/// Assembles the memory block injected when the conversation enables memory,
/// from each tier's [`read_index_context`].
///
/// Global comes first and project second, so the more specific tier is nearest
/// the conversation. Returns `None` when both tiers are empty, so an enabled
/// toggle with nothing stored costs no context at all.
#[cfg(test)]
pub fn render_memory_context(
    global: Option<&str>,
    project: Option<&str>,
    profile: &PromptProfile,
) -> Option<String> {
    let project = project.map(|index| (profile.text(MemoryTier::Project.prompt_key()).to_owned(), index.to_owned()));
    render_memory_block(global, project.as_slice(), profile)
}

/// [`render_memory_context`] with any number of project indexes, each under
/// its own name: one per workspace of the conversation that has memory.
fn render_memory_block(
    global: Option<&str>,
    projects: &[(String, String)],
    profile: &PromptProfile,
) -> Option<String> {
    if global.is_none() && projects.is_empty() {
        return None;
    }

    let mut out = String::from(MEMORY_CONTEXT_START);
    out.push('\n');
    out.push_str(profile.text(PromptKey::MemoryContextIntro));
    out.push('\n');
    if let Some(index) = global {
        push_tier(&mut out, profile.text(MemoryTier::Global.prompt_key()), index, profile);
    }
    for (name, index) in projects {
        push_tier(&mut out, name, index, profile);
    }
    out.push_str(MEMORY_CONTEXT_END);
    Some(out)
}

fn push_tier(out: &mut String, tier: &str, index: &str, profile: &PromptProfile) {
    out.push('\n');
    out.push_str(&profile.render(PromptKey::MemoryIndexHeading, &[("tier", tier)]));
    out.push_str("\n\n");
    out.push_str(index.trim_end());
    out.push('\n');
}

/// Every memory tool name, both tiers.
///
/// Two verbs per tier, plus read. There is deliberately no list tool (the
/// index is already in context), no search tool (the index is small enough to
/// scan), and no delete tool (removing a memory is the user's call, made on
/// the file itself, not something a model should do mid-turn).
///
/// This is the *broad* list: stripping, redaction and UI exclusion all address
/// memory as one family regardless of which tier a conversation turned on.
/// Granting tools is the opposite — it goes through [`tool_names_for_tier`],
/// because a conversation enables the two tiers independently.
pub const MEMORY_TOOL_NAMES: [&str; 6] = [
    "read_global_memory",
    "read_project_memory",
    "create_global_memory",
    "create_project_memory",
    "edit_global_memory",
    "edit_project_memory",
];

/// The three tools a conversation gains by enabling global memory.
pub const GLOBAL_MEMORY_TOOL_NAMES: [&str; 3] = [
    "read_global_memory",
    "create_global_memory",
    "edit_global_memory",
];

/// The three tools a conversation gains by enabling project memory.
pub const PROJECT_MEMORY_TOOL_NAMES: [&str; 3] = [
    "read_project_memory",
    "create_project_memory",
    "edit_project_memory",
];

/// The exact three tools of one tier. Grant paths use this instead of the
/// six-name list so an enabled tier can never drag the other one along.
pub fn tool_names_for_tier(tier: MemoryTier) -> [&'static str; 3] {
    match tier {
        MemoryTier::Global => GLOBAL_MEMORY_TOOL_NAMES,
        MemoryTier::Project => PROJECT_MEMORY_TOOL_NAMES,
    }
}

/// True when `tool_name` is one of the six memory tools.
pub fn is_memory_tool(tool_name: &str) -> bool {
    MEMORY_TOOL_NAMES.contains(&tool_name)
}

/// The tier a memory tool addresses, or `None` if it is not a memory tool.
pub fn tool_tier(tool_name: &str) -> Option<MemoryTier> {
    if !is_memory_tool(tool_name) {
        return None;
    }
    Some(if tool_name.contains("_global_") {
        MemoryTier::Global
    } else {
        MemoryTier::Project
    })
}

/// Which tiers the conversation turned on.
///
/// Deliberately distinct from *availability*: an enabled tier can still be
/// unresolvable (no home directory, no workspace on disk), and a disabled tier
/// is refused even though its directory is sitting right there. Keeping the two
/// apart is what lets an error say which of the two reasons applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryTierAccess {
    pub global: bool,
    pub project: bool,
}

impl MemoryTierAccess {
    /// Both tiers open.
    pub const ALL: Self = Self {
        global: true,
        project: true,
    };

    pub fn allows(self, tier: MemoryTier) -> bool {
        match tier {
            MemoryTier::Global => self.global,
            MemoryTier::Project => self.project,
        }
    }
}

impl Default for MemoryTierAccess {
    fn default() -> Self {
        Self::ALL
    }
}

/// Host-resolved roots for one run. A tier absent here is unavailable for the
/// whole run, and its tools report that instead of writing somewhere else.
#[derive(Clone, Debug, Default)]
pub struct MemoryRoots {
    pub global: Option<MemoryRoot>,
    /// The project tier of each of the conversation's workspaces, in the
    /// conversation's order: every workspace keeps its own project memory in
    /// its own folder. Empty when no workspace has a directory.
    pub projects: Vec<ProjectMemory>,
    /// Which tiers this run is allowed to touch at all.
    pub access: MemoryTierAccess,
}

/// One workspace's project memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectMemory {
    /// The conversation's number for the workspace: what the project memory
    /// tools' `workspace` argument names.
    pub workspace: u32,
    /// The workspace's folder as the model was told it, for the memory
    /// block's heading.
    pub path: String,
    pub root: MemoryRoot,
}

impl MemoryRoots {
    /// Resolves both tiers from the trusted home and workspace directories,
    /// recording which of them the conversation actually enabled.
    #[cfg(test)]
    pub fn resolve_enabled(
        home: Option<&Path>,
        workspace: Option<&Path>,
        access: MemoryTierAccess,
    ) -> Self {
        Self {
            global: global_root(home),
            projects: workspace
                .and_then(|workspace| {
                    project_root(Some(workspace)).map(|root| ProjectMemory {
                        workspace: 1,
                        path: workspace.to_string_lossy().into_owned(),
                        root,
                    })
                })
                .into_iter()
                .collect(),
            access,
        }
    }

    /// Workspace 1's project tier, or the first workspace's that has one.
    pub fn project(&self) -> Option<&MemoryRoot> {
        self.projects.first().map(|project| &project.root)
    }

    /// The project tier of workspace `member` (`None`: the first).
    fn project_of(&self, member: Option<u32>) -> Option<&MemoryRoot> {
        match member {
            None => self.project(),
            Some(member) => self
                .projects
                .iter()
                .find(|project| project.workspace == member)
                .map(|project| &project.root),
        }
    }

    fn tier(&self, tier: MemoryTier, member: Option<u32>) -> Result<&MemoryRoot, String> {
        // A disabled tier fails before availability is even consulted, so the
        // message never blames a missing directory for a switch the user left
        // off.
        if !self.access.allows(tier) {
            return Err(format!(
                "{} is not enabled for this conversation; enable it in conversation settings and try again",
                tier.label()
            ));
        }
        match tier {
            MemoryTier::Global => self
                .global
                .as_ref()
                .ok_or_else(|| "Global memory is unavailable because the host did not resolve a user home directory".to_owned()),
            MemoryTier::Project => match (self.project_of(member), member) {
                (Some(root), _) => Ok(root),
                (None, None) => Err("Project memory is unavailable because this conversation is not bound to a workspace directory on disk".to_owned()),
                (None, Some(member)) => Err(format!(
                    "There is no project memory of workspace {member}. This conversation's project memories are those of workspaces {}.",
                    self.projects
                        .iter()
                        .map(|project| project.workspace.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            },
        }
    }

    /// Builds the always-on memory block for the conversation context.
    ///
    /// Only enabled tiers are read. A disabled tier contributes no index, so
    /// turning a tier off is observable in the context, not just in the tool
    /// list.
    ///
    /// With more than one workspace, each workspace's project index stands
    /// under its own heading, naming the workspace by its number and folder.
    /// The indexes of workspaces on other machines are read there, all at
    /// once.
    pub fn render_context(&self, profile: &PromptProfile) -> Option<String> {
        let global = self
            .global
            .as_ref()
            .filter(|_| self.access.allows(MemoryTier::Global))
            .and_then(read_index_context);
        let projects = if self.access.allows(MemoryTier::Project) {
            let indexes = std::thread::scope(|scope| {
                self.projects
                    .iter()
                    .map(|project| scope.spawn(|| read_index_context(&project.root)))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|worker| worker.join().ok().flatten())
                    .collect::<Vec<_>>()
            });
            let several = self.projects.len() > 1;
            self.projects
                .iter()
                .zip(indexes)
                .filter_map(|(project, index)| {
                    let name = if several {
                        profile.render(
                            PromptKey::MemoryTierProjectOfWorkspace,
                            &[
                                ("workspace", &project.workspace.to_string()),
                                ("path", &project.path),
                            ],
                        )
                    } else {
                        profile.text(MemoryTier::Project.prompt_key()).to_owned()
                    };
                    index.map(|index| (name, index))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        render_memory_block(global.as_deref(), &projects, profile)
    }
}

/// Executes one memory tool call.
///
/// Every argument is a plain document name or text; the tier comes from the
/// tool name and the directory comes from `roots`. No caller-supplied path
/// ever reaches the filesystem.
pub fn execute_tool(
    roots: &MemoryRoots,
    tool_name: &str,
    input: &JsonObject,
    profile: &PromptProfile,
) -> Result<String, String> {
    let tier = tool_tier(tool_name).ok_or_else(|| format!("Unknown memory tool: {tool_name}"))?;
    // A project memory tool names the workspace whose memory it means when the
    // conversation has more than one; absent is workspace 1's.
    let member = match tier {
        MemoryTier::Global => None,
        MemoryTier::Project => crate::tool_executor::workspace_argument(input)?,
    };
    let root = roots.tier(tier, member)?;

    match tool_name {
        "read_global_memory" | "read_project_memory" => {
            let document = read_document(root, &required_text(input, "name")?)?;
            Ok(document.content)
        }
        "create_global_memory" | "create_project_memory" => {
            let document = create_document(
                root,
                &required_text(input, "name")?,
                &required_text(input, "content")?,
                &required_text(input, "description")?,
            )?;
            Ok(profile.render(
                PromptKey::MemoryCreated,
                &[
                    ("tier", profile.text(tier.prompt_key())),
                    ("name", &document.name),
                ],
            ))
        }
        "edit_global_memory" | "edit_project_memory" => {
            let document = edit_document(
                root,
                &required_text(input, "name")?,
                &required_text(input, "old_text")?,
                &required_text(input, "new_text")?,
                &required_text(input, "description")?,
            )?;
            Ok(profile.render(
                PromptKey::MemoryUpdated,
                &[
                    ("tier", profile.text(tier.prompt_key())),
                    ("name", &document.name),
                ],
            ))
        }
        other => Err(format!("Unknown memory tool: {other}")),
    }
}

fn required_text(input: &JsonObject, field: &str) -> Result<String, String> {
    match input.get(field) {
        Some(serde_json::Value::String(value)) => Ok(value.clone()),
        Some(serde_json::Value::Null) | None => Err(format!("Missing argument: {field}")),
        Some(_) => Err(format!("Argument {field} must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "mewrk-memory-{}-{label}-{nonce}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn global(&self) -> MemoryRoot {
            global_root(Some(&self.root.join("home"))).unwrap()
        }

        fn project(&self) -> MemoryRoot {
            project_root(Some(&self.root.join("workspace"))).unwrap()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn roots_resolve_to_the_documented_two_tier_layout() {
        let tree = TempTree::new("layout");
        let global = tree.global();
        assert_eq!(global.tier(), MemoryTier::Global);
        assert_eq!(
            global.index_path(),
            tree.root.join("home/.mewrk/memory/MEMORY.md")
        );

        let project = tree.project();
        assert_eq!(project.tier(), MemoryTier::Project);
        assert_eq!(
            project.index_path(),
            tree.root.join("workspace/.mewrk/memory/MEMORY.md")
        );

        // A missing home or an unstable workspace makes that tier unavailable
        // rather than silently redirecting it somewhere writable.
        assert!(global_root(None).is_none());
        assert!(project_root(None).is_none());
    }

    #[test]
    fn document_names_accept_bare_stems_and_reject_every_escape() {
        assert_eq!(normalize_document_name("notes").unwrap(), "notes.md");
        assert_eq!(normalize_document_name("notes.md").unwrap(), "notes.md");
        assert_eq!(normalize_document_name(" notes.MD ").unwrap(), "notes.md");
        assert_eq!(normalize_document_name("café").unwrap(), "café.md");

        for rejected in [
            "",
            "   ",
            ".md",
            "..",
            ".",
            ".hidden",
            "a/b",
            "a\\b",
            "../escape",
            "C:notes",
            "notes:stream",
            "notes.",
            "notes .md",
            "no<te",
            "no|te",
            "no*te",
        ] {
            assert!(
                normalize_document_name(rejected).is_err(),
                "{rejected:?} must be rejected"
            );
        }
        assert!(normalize_document_name("notes\0.md").is_err());
        assert!(normalize_document_name(&"a".repeat(MAX_NAME_CHARS + 1)).is_err());
    }

    #[test]
    fn the_index_is_never_addressable_as_a_document() {
        // The host owns MEMORY.md. No tool may name it under any casing or
        // with the suffix omitted, so the model cannot rewrite the index out
        // of step with the documents it points at.
        for spelling in ["MEMORY.md", "memory.md", "MEMORY", "Memory.MD", " memory "] {
            let error = normalize_document_name(spelling).unwrap_err();
            assert!(error.contains(INDEX_NAME), "{spelling:?}: {error}");
        }
    }

    #[test]
    fn create_writes_the_document_and_records_its_index_description() {
        let tree = TempTree::new("create");
        let root = tree.global();

        let created = create_document(
            &root,
            "build",
            "cargo test requires the project's bundled environment.",
            "build command",
        )
        .unwrap();
        assert_eq!(created.name, "build.md");
        assert_eq!(created.tier, MemoryTier::Global);
        assert_eq!(
            fs::read_to_string(root.memory_dir().join("build.md")).unwrap(),
            "cargo test requires the project's bundled environment."
        );

        let index = fs::read_to_string(root.index_path()).unwrap();
        assert!(index.contains("- [build.md](build.md) — build command"));

        // Re-creating must not clobber; the model is told to edit instead.
        let error =
            create_document(&root, "build", "other content", "other description").unwrap_err();
        assert!(error.contains("already exists"));
        assert_eq!(
            fs::read_to_string(root.memory_dir().join("build.md")).unwrap(),
            "cargo test requires the project's bundled environment."
        );
    }

    #[test]
    fn create_requires_a_usable_index_description() {
        let tree = TempTree::new("description");
        let root = tree.global();
        assert!(create_document(&root, "a", "body", "   ").is_err());
        assert!(
            create_document(&root, "a", "body", &"x".repeat(MAX_DESCRIPTION_CHARS + 1)).is_err()
        );
        // A failed write leaves nothing behind, index included.
        assert!(!root.memory_dir().join("a.md").exists());
        assert!(read_index(&root).is_empty());
    }

    #[test]
    fn edit_replaces_a_unique_match_and_refreshes_the_description() {
        let tree = TempTree::new("edit");
        let root = tree.project();
        create_document(&root, "notes", "The port is 3000.\nOther content.", "port").unwrap();

        let edited = edit_document(&root, "notes", "3000", "4000", "development port").unwrap();
        assert_eq!(edited.content, "The port is 4000.\nOther content.");
        let index = fs::read_to_string(root.index_path()).unwrap();
        assert!(index.contains("- [notes.md](notes.md) — development port"));
        assert!(!index.contains("— port\n"));
        // One entry per document, no duplicate line from the second write.
        assert_eq!(read_index(&root).len(), 1);
    }

    #[test]
    fn edit_refuses_ambiguous_missing_and_empty_matches() {
        let tree = TempTree::new("edit-guard");
        let root = tree.project();
        create_document(
            &root,
            "notes",
            "same paragraph\nsame paragraph",
            "duplicate",
        )
        .unwrap();

        let ambiguous = edit_document(&root, "notes", "same paragraph", "replacement", "duplicate")
            .unwrap_err();
        assert!(ambiguous.contains("occurs 2 times"), "{ambiguous}");
        assert!(edit_document(&root, "notes", "missing", "x", "duplicate").is_err());
        assert!(edit_document(&root, "notes", "", "x", "duplicate").is_err());
        assert!(edit_document(
            &root,
            "notes",
            "same paragraph",
            "same paragraph",
            "duplicate"
        )
        .is_err());
        assert!(edit_document(&root, "missing", "a", "b", "duplicate").is_err());

        // Every rejected edit left the document byte-identical.
        assert_eq!(
            read_document(&root, "notes").unwrap().content,
            "same paragraph\nsame paragraph"
        );
    }

    #[test]
    fn reading_a_missing_document_names_the_tier_without_leaking_a_path() {
        let tree = TempTree::new("read-missing");
        let root = tree.global();
        let error = read_document(&root, "absent").unwrap_err();
        assert!(error.contains("absent.md"));
        assert!(error.contains("global memory"));
        assert!(!error.contains(".mewrk"));
    }

    #[test]
    fn oversized_documents_are_refused() {
        let tree = TempTree::new("oversize");
        let root = tree.global();
        let huge = "x".repeat(MAX_DOCUMENT_BYTES + 1);
        assert!(create_document(&root, "big", &huge, "too large").is_err());
        assert!(!root.memory_dir().join("big.md").exists());
    }

    #[test]
    fn index_parsing_round_trips_and_skips_malformed_lines() {
        let entries = parse_index(
            "# Global memory index\n\n\
             - [a.md](a.md) — first entry\n\
             arbitrary prose\n\
             - entry without a link\n\
             - [b.md](b.md) second entry\n\
             - [a.md](a.md) — duplicate is ignored\n\
             - [../escape.md](../escape.md) — invalid name is ignored\n",
        );
        assert_eq!(
            entries,
            vec![
                IndexEntry {
                    name: "a.md".into(),
                    description: "first entry".into()
                },
                IndexEntry {
                    name: "b.md".into(),
                    description: "second entry".into()
                },
            ]
        );

        let rendered = render_index(MemoryTier::Global, &entries);
        assert_eq!(parse_index(&rendered), entries);
    }

    #[test]
    fn context_carries_each_index_but_never_topic_bodies_or_mewrk_md() {
        let tree = TempTree::new("context");
        let global = tree.global();
        let project = tree.project();
        let profile = PromptProfile::builtin_english();
        fs::create_dir_all(global.memory_dir()).unwrap();
        fs::create_dir_all(project.memory_dir()).unwrap();
        // The instruction loader sends these; the memory block must not send
        // them a second time.
        fs::write(tree.root.join("home/.mewrk/MEWRK.md"), "global standing instruction").unwrap();
        fs::write(tree.root.join("workspace/.mewrk/MEWRK.md"), "project standing instruction").unwrap();
        create_document(&global, "g", "global memory body", "global entry").unwrap();
        create_document(&project, "p", "project memory body", "project entry").unwrap();

        let global_index = read_index_context(&global);
        let project_index = read_index_context(&project);
        let rendered =
            render_memory_context(global_index.as_deref(), project_index.as_deref(), &profile)
                .unwrap();

        assert!(rendered.starts_with(MEMORY_CONTEXT_START));
        assert!(rendered.ends_with(MEMORY_CONTEXT_END));
        assert!(rendered.contains("Below is your long-term memory."));
        assert!(rendered.contains("## Global memory · MEMORY.md"));
        assert!(rendered.contains("## Project memory · MEMORY.md"));
        assert!(rendered.contains("- [g.md](g.md) — global entry"));
        assert!(rendered.contains("- [p.md](p.md) — project entry"));
        assert!(!rendered.contains("standing instruction"));
        assert!(!rendered.contains("MEWRK.md"));

        // Topic bodies stay on disk until the model reads them by name.
        assert!(!rendered.contains("global memory body"));
        assert!(!rendered.contains("project memory body"));

        // Global is injected before project, so the more specific tier sits
        // closest to the conversation.
        assert!(rendered.find("global entry").unwrap() < rendered.find("project entry").unwrap());
    }

    #[test]
    fn context_is_absent_when_nothing_is_stored() {
        let tree = TempTree::new("empty-context");
        let profile = PromptProfile::builtin_english();
        assert_eq!(read_index_context(&tree.global()), None);
        assert!(render_memory_context(None, None, &profile).is_none());

        // An index that lists nothing describes nothing worth the context.
        let root = tree.global();
        fs::create_dir_all(root.memory_dir()).unwrap();
        fs::write(root.index_path(), render_index(MemoryTier::Global, &[])).unwrap();
        assert_eq!(read_index_context(&root), None);
    }

    #[test]
    fn a_document_deleted_outside_mewrk_leaves_the_index_at_once() {
        let tree = TempTree::new("deleted-outside");
        let root = tree.project();
        create_document(&root, "kept", "kept body", "kept entry").unwrap();
        create_document(&root, "gone", "gone body", "gone entry").unwrap();

        fs::remove_file(root.memory_dir().join("gone.md")).unwrap();

        let index = read_index_context(&root).unwrap();
        assert!(index.contains("- [kept.md](kept.md) — kept entry"));
        assert!(!index.contains("gone"));
        // MEMORY.md itself is untouched until the next write rewrites it.
        let on_disk = fs::read_to_string(root.index_path()).unwrap();
        assert!(on_disk.contains("gone entry"));
    }

    #[test]
    fn writing_memory_leaves_no_lock_file_in_the_tier() {
        let tree = TempTree::new("no-lock-in-tier");
        let root = tree.project();
        fs::create_dir_all(root.memory_dir()).unwrap();
        // One an earlier build left behind goes on the next write.
        fs::write(root.memory_dir().join(LEGACY_MUTATION_LOCK_NAME), "").unwrap();

        create_document(&root, "notes", "body", "entry").unwrap();
        edit_document(&root, "notes", "body", "new body", "entry").unwrap();

        let mut names = fs::read_dir(root.memory_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["MEMORY.md", "notes.md"]);
    }

    #[test]
    fn the_two_tiers_are_independent_namespaces() {
        let tree = TempTree::new("tiers");
        let global = tree.global();
        let project = tree.project();

        create_document(&global, "notes", "global version", "global").unwrap();
        create_document(&project, "notes", "project version", "project").unwrap();

        assert_eq!(
            read_document(&global, "notes").unwrap().content,
            "global version"
        );
        assert_eq!(
            read_document(&project, "notes").unwrap().content,
            "project version"
        );

        edit_document(
            &project,
            "notes",
            "project version",
            "updated project version",
            "project",
        )
        .unwrap();
        assert_eq!(
            read_document(&global, "notes").unwrap().content,
            "global version"
        );
    }

    #[test]
    fn a_hand_written_index_survives_a_model_write() {
        let tree = TempTree::new("hand-index");
        let root = tree.global();
        fs::create_dir_all(root.memory_dir()).unwrap();
        fs::write(
            root.index_path(),
            "# Hand-written index\n\n- [existing.md](existing.md) — user description\n",
        )
        .unwrap();
        fs::write(root.memory_dir().join("existing.md"), "existing content").unwrap();

        create_document(&root, "fresh", "new content", "new entry").unwrap();
        let entries = read_index(&root);
        assert_eq!(
            entries,
            vec![
                IndexEntry {
                    name: "existing.md".into(),
                    description: "user description".into()
                },
                IndexEntry {
                    name: "fresh.md".into(),
                    description: "new entry".into()
                },
            ]
        );
    }

    fn input(pairs: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(key, value)| {
                (
                    (*key).to_owned(),
                    serde_json::Value::String((*value).to_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn tool_names_map_to_exactly_one_tier_each() {
        assert_eq!(MEMORY_TOOL_NAMES.len(), 6);
        for name in MEMORY_TOOL_NAMES {
            assert!(is_memory_tool(name), "{name}");
        }
        assert_eq!(tool_tier("read_global_memory"), Some(MemoryTier::Global));
        assert_eq!(tool_tier("create_global_memory"), Some(MemoryTier::Global));
        assert_eq!(tool_tier("edit_global_memory"), Some(MemoryTier::Global));
        assert_eq!(tool_tier("read_project_memory"), Some(MemoryTier::Project));
        assert_eq!(
            tool_tier("create_project_memory"),
            Some(MemoryTier::Project)
        );
        assert_eq!(tool_tier("edit_project_memory"), Some(MemoryTier::Project));

        // The per-tier trios partition the broad list exactly: every name
        // belongs to its own tier and to no other, so granting one tier can
        // never widen into the other.
        for tier in [MemoryTier::Global, MemoryTier::Project] {
            for name in tool_names_for_tier(tier) {
                assert!(MEMORY_TOOL_NAMES.contains(&name), "{name}");
                assert_eq!(tool_tier(name), Some(tier), "{name}");
            }
        }
        assert_eq!(
            GLOBAL_MEMORY_TOOL_NAMES.len() + PROJECT_MEMORY_TOOL_NAMES.len(),
            MEMORY_TOOL_NAMES.len()
        );

        // Retired tools from the model-owned SQLite era must not resolve.
        for retired in [
            "memory_list",
            "memory_read",
            "memory_search",
            "memory_upsert",
            "memory_delete",
        ] {
            assert!(!is_memory_tool(retired), "{retired}");
            assert_eq!(tool_tier(retired), None);
        }
    }

    #[test]
    fn a_disabled_tier_contributes_no_context_and_refuses_its_own_tools() {
        let tree = TempTree::new("tier-switches");
        let home = tree.root.join("home");
        let workspace = tree.root.join("workspace");
        let profile = PromptProfile::builtin_english();
        for root in [tree.global(), tree.project()] {
            fs::create_dir_all(root.memory_dir()).unwrap();
        }
        create_document(&tree.global(), "g", "global body", "global entry").unwrap();
        create_document(&tree.project(), "p", "project body", "project entry").unwrap();

        let global_only = MemoryRoots::resolve_enabled(
            Some(&home),
            Some(&workspace),
            MemoryTierAccess {
                global: true,
                project: false,
            },
        );
        let rendered = global_only.render_context(&profile).unwrap();
        assert!(rendered.contains("- [g.md](g.md) — global entry"));
        // Not one byte of the disabled tier's index.
        assert!(!rendered.contains("project entry"));
        assert!(!rendered.contains("p.md"));
        for name in PROJECT_MEMORY_TOOL_NAMES {
            let error = execute_tool(
                &global_only,
                name,
                &input(&[
                    ("name", "p"),
                    ("content", "x"),
                    ("description", "x"),
                    ("old_text", "project body"),
                    ("new_text", "replacement"),
                ]),
                &profile,
            )
            .unwrap_err();
            assert!(
                error.contains("project memory is not enabled"),
                "{name}: {error}"
            );
        }
        assert_eq!(
            execute_tool(
                &global_only,
                "read_global_memory",
                &input(&[("name", "g")]),
                &profile,
            )
            .unwrap(),
            "global body"
        );

        // The mirror image, and then both off: no block at all.
        let project_only = MemoryRoots::resolve_enabled(
            Some(&home),
            Some(&workspace),
            MemoryTierAccess {
                global: false,
                project: true,
            },
        );
        let rendered = project_only.render_context(&profile).unwrap();
        assert!(rendered.contains("- [p.md](p.md) — project entry"));
        assert!(!rendered.contains("global entry"));
        let error = execute_tool(
            &project_only,
            "read_global_memory",
            &input(&[("name", "g")]),
            &profile,
        )
        .unwrap_err();
        assert!(error.contains("global memory is not enabled"), "{error}");

        let none = MemoryRoots::resolve_enabled(
            Some(&home),
            Some(&workspace),
            MemoryTierAccess {
                global: false,
                project: false,
            },
        );
        assert!(none.render_context(&profile).is_none());
        for name in MEMORY_TOOL_NAMES {
            assert!(
                execute_tool(&none, name, &input(&[("name", "g")]), &profile).is_err(),
                "{name}"
            );
        }

        // A disabled tier is refused even though its directory exists, and the
        // message says so rather than blaming a missing directory.
        let unavailable = MemoryRoots::resolve_enabled(None, None, MemoryTierAccess::ALL);
        assert!(unavailable
            .tier(MemoryTier::Global, None)
            .unwrap_err()
            .contains("unavailable"));
    }

    #[test]
    fn the_dispatcher_round_trips_create_read_and_edit_per_tier() {
        let tree = TempTree::new("dispatch");
        let roots = MemoryRoots::resolve_enabled(
            Some(&tree.root.join("home")),
            Some(&tree.root.join("workspace")),
            MemoryTierAccess::ALL,
        );
        let profile = PromptProfile::builtin_english();

        for (create, read, edit, tier) in [
            (
                "create_global_memory",
                "read_global_memory",
                "edit_global_memory",
                "Global memory",
            ),
            (
                "create_project_memory",
                "read_project_memory",
                "edit_project_memory",
                "Project memory",
            ),
        ] {
            assert_eq!(
                execute_tool(
                    &roots,
                    create,
                    &input(&[
                        ("name", "build"),
                        ("content", "port 3000"),
                        ("description", "build"),
                    ]),
                    &profile,
                )
                .unwrap(),
                format!("Created build.md in {tier} and recorded its index description.")
            );
            assert_eq!(
                execute_tool(&roots, read, &input(&[("name", "build")]), &profile).unwrap(),
                "port 3000"
            );

            assert_eq!(
                execute_tool(
                    &roots,
                    edit,
                    &input(&[
                        ("name", "build.md"),
                        ("old_text", "3000"),
                        ("new_text", "4000"),
                        ("description", "build port"),
                    ]),
                    &profile,
                )
                .unwrap(),
                format!("Updated build.md in {tier} and refreshed its index description.")
            );
            assert_eq!(
                execute_tool(&roots, read, &input(&[("name", "build")]), &profile).unwrap(),
                "port 4000"
            );
        }

        // Both tiers now hold their own build.md, and the context shows both
        // index entries without either document body.
        let rendered = roots.render_context(&profile).unwrap();
        assert_eq!(
            rendered
                .matches("- [build.md](build.md) — build port")
                .count(),
            2
        );
        assert!(!rendered.contains("port 4000"));
    }

    #[test]
    fn an_unavailable_tier_fails_closed_instead_of_using_the_other_one() {
        let tree = TempTree::new("unavailable");
        let profile = PromptProfile::builtin_english();
        // No workspace: project memory is unavailable for the whole run.
        let roots = MemoryRoots::resolve_enabled(
            Some(&tree.root.join("home")),
            None,
            MemoryTierAccess::ALL,
        );

        let error = execute_tool(
            &roots,
            "create_project_memory",
            &input(&[("name", "a"), ("content", "b"), ("description", "c")]),
            &profile,
        )
        .unwrap_err();
        assert!(error.contains("Project memory is unavailable"), "{error}");
        assert!(execute_tool(
            &roots,
            "read_project_memory",
            &input(&[("name", "a")]),
            &profile,
        )
        .is_err());

        // Nothing leaked into the global tier.
        assert!(execute_tool(
            &roots,
            "read_global_memory",
            &input(&[("name", "a")]),
            &profile,
        )
        .is_err());
        assert!(roots.render_context(&profile).is_none());

        // And the symmetric case: no home means no global tier.
        let project_only = MemoryRoots::resolve_enabled(
            None,
            Some(&tree.root.join("workspace")),
            MemoryTierAccess::ALL,
        );
        let error = execute_tool(
            &project_only,
            "read_global_memory",
            &input(&[("name", "a")]),
            &profile,
        )
        .unwrap_err();
        assert!(error.contains("Global memory is unavailable"), "{error}");
    }

    #[test]
    fn the_dispatcher_rejects_missing_and_mistyped_arguments() {
        let tree = TempTree::new("arguments");
        let roots = MemoryRoots::resolve_enabled(
            Some(&tree.root.join("home")),
            None,
            MemoryTierAccess::ALL,
        );
        let profile = PromptProfile::builtin_english();

        assert!(execute_tool(&roots, "read_global_memory", &input(&[]), &profile).is_err());
        // create requires a description so the index can never go stale.
        assert!(execute_tool(
            &roots,
            "create_global_memory",
            &input(&[("name", "a"), ("content", "b")]),
            &profile,
        )
        .is_err());

        let mut mistyped = serde_json::Map::new();
        mistyped.insert("name".into(), serde_json::Value::from(7));
        let error = execute_tool(&roots, "read_global_memory", &mistyped, &profile).unwrap_err();
        assert!(error.contains("must be a string"), "{error}");

        assert!(execute_tool(&roots, "memory_upsert", &input(&[]), &profile).is_err());
    }

    #[test]
    fn no_tool_can_address_the_host_owned_index() {
        let tree = TempTree::new("index-guard");
        let roots = MemoryRoots::resolve_enabled(
            Some(&tree.root.join("home")),
            None,
            MemoryTierAccess::ALL,
        );
        let profile = PromptProfile::builtin_english();
        execute_tool(
            &roots,
            "create_global_memory",
            &input(&[
                ("name", "real"),
                ("content", "content"),
                ("description", "description"),
            ]),
            &profile,
        )
        .unwrap();
        let index_before = fs::read_to_string(roots.global.as_ref().unwrap().index_path()).unwrap();

        for spelling in ["MEMORY.md", "memory", "Memory.MD"] {
            assert!(execute_tool(
                &roots,
                "read_global_memory",
                &input(&[("name", spelling)]),
                &profile,
            )
            .is_err());
            assert!(execute_tool(
                &roots,
                "create_global_memory",
                &input(&[
                    ("name", spelling),
                    ("content", "hijack"),
                    ("description", "hijack")
                ]),
                &profile,
            )
            .is_err());
            assert!(execute_tool(
                &roots,
                "edit_global_memory",
                &input(&[
                    ("name", spelling),
                    ("old_text", "real"),
                    ("new_text", "hijack"),
                    ("description", "hijack"),
                ]),
                &profile,
            )
            .is_err());
        }

        assert_eq!(
            fs::read_to_string(roots.global.as_ref().unwrap().index_path()).unwrap(),
            index_before
        );
    }

    /// Every workspace keeps its own project memory: the memory block lists
    /// each workspace's index under its own heading, and the tools act on the
    /// memory of the workspace they name — workspace 1's when they name none.
    #[test]
    fn each_workspace_keeps_its_own_project_memory() {
        let tree = TempTree::new("workspaces");
        let first = project_root(Some(&tree.root.join("first"))).unwrap();
        let second = project_root(Some(&tree.root.join("second"))).unwrap();
        let roots = MemoryRoots {
            global: None,
            projects: vec![
                ProjectMemory { workspace: 1, path: "/work/first".into(), root: first.clone() },
                ProjectMemory { workspace: 2, path: "/work/second".into(), root: second.clone() },
            ],
            access: MemoryTierAccess::ALL,
        };
        let profile = PromptProfile::builtin_english();
        let create = |input: serde_json::Value| {
            execute_tool(&roots, "create_project_memory", input.as_object().unwrap(), &profile)
        };
        create(serde_json::json!({"name": "a", "content": "first body", "description": "first entry"})).unwrap();
        create(serde_json::json!({"name": "b", "content": "second body", "description": "second entry", "workspace": 2})).unwrap();
        assert_eq!(read_document(&first, "a").unwrap().content, "first body");
        assert_eq!(read_document(&second, "b").unwrap().content, "second body");
        assert!(read_document(&first, "b").is_err());
        let error = create(serde_json::json!({"name": "c", "content": "x", "description": "x", "workspace": 3})).unwrap_err();
        assert!(error.contains("workspace 3"), "{error}");

        let context = roots.render_context(&profile).unwrap();
        assert!(context.contains("Project memory of workspace 1 (/work/first)"), "{context}");
        assert!(context.contains("Project memory of workspace 2 (/work/second)"), "{context}");
        assert!(context.contains("first entry") && context.contains("second entry"), "{context}");
        let read = serde_json::json!({"name": "b", "workspace": 2});
        assert_eq!(
            execute_tool(&roots, "read_project_memory", read.as_object().unwrap(), &profile).unwrap(),
            "second body"
        );
    }
}
