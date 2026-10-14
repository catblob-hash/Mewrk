//! File checkpoints: putting the files a conversation's model changed back as
//! they were before one of the user's messages.
//!
//! `write` and `edit` are the only tools whose changes the host sees coming, so
//! they are the only ones tracked. Right before one of them replaces a file on
//! this machine — in a plain workspace, a sandboxed one or an isolated worktree —
//! the file's bytes are kept ([`record_before_write`]); a change a shell command
//! makes, or one on another machine, is not.
//!
//! A change belongs to the user message it was made for, its *anchor*: the last
//! real user message of the run's own timeline, or — for a subagent, a workflow
//! step or a manually run tool, whose requests carry no timeline of the
//! conversation's — the last one the conversation has saved ([`anchor_for_run`],
//! [`anchor_from_store`]). The first change to a file under an anchor is the one
//! kept; later ones under it add nothing, because what a rewind wants is how the
//! file looked before the message, not between two of its edits.
//!
//! The index is the `file_checkpoint` table of the conversation store: a file's
//! path, the hash of what it held (or that it did not exist), and the order the
//! rows were recorded in. The contents are files of their own under
//! `<app data>/file-history/<hash of the conversation id>/<hash of the content>`,
//! which [`crate::workspace_dirs`] removes with the conversation, as it does the
//! spilled tool output beside it. The model's file tools must not reach that
//! directory (`security.rs`), or a backup could be read, or planted.
//!
//! [`rewind`] goes by the order rows were recorded in, not by the timeline,
//! which the user may edit: it takes the first row recorded under any of the
//! rewound messages, and every row from there on. Of each file's rows, the
//! earliest says how the file looked before the first of those changes. Rows are
//! deleted as their file is put back, and kept for a file that could not be, so
//! that rewinding again tries it again.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    api::is_genuine_user_context_id,
    conversation_store::{self, ConversationStore, FileCheckpointRecord, FileCheckpointSlot},
    model::{ContextItem, RunModelRequest},
    state::AppState,
    storage::atomic_write,
    ui_text::{self, ui_text},
    workspace_dirs,
};

/// The largest file a checkpoint keeps. A file above it is changed all the same;
/// the checkpoint then only remembers that it could not keep it.
pub(crate) const MAX_BACKUP_BYTES: u64 = 32 * 1024 * 1024;
/// How many user messages a conversation keeps checkpoints for: the most recent.
pub(crate) const MAX_ANCHORS: usize = 100;

/// What `content_sha256` holds for a file that existed but was not kept.
const NOT_KEPT: &str = "";

/// Recording and cleaning up are one at a time. A content file is written
/// before the row that names it and removed after the last row that did, so two
/// of them interleaving could delete a copy a row is about to point at.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where one call's changes are filed: the conversation, the user message they
/// belong to, and the application data directory the store and the backups live in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CheckpointContext<'a> {
    pub conversation_id: &'a str,
    pub anchor: &'a str,
    pub app_data: &'a Path,
}

#[derive(Clone, Copy)]
struct Limits {
    backup_bytes: u64,
    anchors: usize,
}

const LIMITS: Limits = Limits {
    backup_bytes: MAX_BACKUP_BYTES,
    anchors: MAX_ANCHORS,
};

/// The store beside the application's document, which is where every host
/// reader of a conversation finds it (`history::history_store`).
fn store_of(app_data: &Path) -> Result<Arc<ConversationStore>, String> {
    conversation_store::store_for(&app_data.join("document.v1.json"))
}

// ------------------------------------------------------------------- Anchors

/// The anchor of a run's changes: the id of the user message they belong to.
///
/// A top-level run reads it from its own timeline. A child's request carries a
/// timeline of its own, not the conversation's, so it asks what the conversation
/// has saved. `None` when there is no real user message yet: a change nobody
/// asked for in words cannot be rewound to before anything.
pub(crate) fn anchor_for_run(request: &RunModelRequest) -> Option<String> {
    if request.app_data_path.is_empty() {
        return None;
    }
    if request.subagent_depth == 0 {
        return request.contexts.iter().rev().find_map(|context| match context {
            ContextItem::User { id, .. } if is_genuine_user_context_id(id) => Some(id.clone()),
            _ => None,
        });
    }
    anchor_from_store(Path::new(&request.app_data_path), &request.conversation_id)
}

/// The newest real user message the conversation has saved. What a request with
/// no timeline of the conversation's — a subagent, a manually run tool — files
/// its changes under.
pub(crate) fn anchor_from_store(app_data: &Path, conversation_id: &str) -> Option<String> {
    // A directory that never held a conversation has no anchor to find, and
    // looking must not be what creates its database.
    if !conversation_store::database_path(&app_data.join("document.v1.json")).exists() {
        return None;
    }
    match store_of(app_data)
        .and_then(|store| store.latest_user_context_id(conversation_id, is_genuine_user_context_id))
    {
        Ok(anchor) => anchor,
        Err(error) => {
            eprintln!("无法确定对话 {conversation_id} 的文件检查点锚点：{error}");
            None
        }
    }
}

// ----------------------------------------------------------------- Recording

/// Keeps the bytes `path` holds now, if this is the first change to it under the
/// context's anchor. Called by `write` and `edit` just before they replace the
/// file; `sandboxed` is whether they do so through a verified handle, in which
/// case the copy is read through one too.
///
/// A checkpoint is a convenience, never a reason for a change to fail: whatever
/// goes wrong here is logged and the write goes ahead.
pub(crate) fn record_before_write(
    context: Option<CheckpointContext<'_>>,
    path: &Path,
    sandboxed: bool,
) {
    let Some(context) = context else {
        return;
    };
    if let Err(error) = record(context, path, sandboxed, LIMITS) {
        eprintln!("文件检查点未能记录 {}：{error}", path.display());
    }
}

fn record(
    context: CheckpointContext<'_>,
    path: &Path,
    sandboxed: bool,
    limits: Limits,
) -> Result<(), String> {
    let path_text = path
        .to_str()
        .ok_or_else(|| "路径不是有效的 Unicode".to_owned())?;
    let _serial = serial();
    let store = store_of(context.app_data)?;
    match store.file_checkpoint_slot(context.conversation_id, context.anchor, path_text)? {
        FileCheckpointSlot::NoConversation | FileCheckpointSlot::Taken => return Ok(()),
        FileCheckpointSlot::Free => {}
    }
    let (hash, mode) = match read_original(path, sandboxed, limits.backup_bytes)? {
        // The write will fail on its own; there is nothing to put back.
        Original::NotAFile => return Ok(()),
        Original::Absent => (None, None),
        Original::TooLarge(bytes) => {
            eprintln!(
                "{} 有 {bytes} 字节，超过文件检查点 {} 字节的上限，不留备份",
                path.display(),
                limits.backup_bytes
            );
            (Some(NOT_KEPT.to_owned()), None)
        }
        Original::File { bytes, mode } => {
            let hash = sha256_hex(&bytes);
            save_content(context.app_data, context.conversation_id, &hash, &bytes)?;
            (Some(hash), mode)
        }
    };
    let written = store.put_file_checkpoint(
        context.conversation_id,
        context.anchor,
        path_text,
        hash.as_deref(),
        mode,
        limits.anchors,
    )?;
    drop_unreferenced(
        &store,
        context.app_data,
        context.conversation_id,
        &written.dropped_hashes,
    );
    Ok(())
}

enum Original {
    Absent,
    /// A directory or some other entry that is not a regular file.
    NotAFile,
    TooLarge(u64),
    File { bytes: Vec<u8>, mode: Option<u32> },
}

/// The bytes at `path` as they are on disk, whatever they are: the copy has to be
/// of the file, not of the text `write` happened to be able to decode (which
/// `run_write`'s own diff read cannot be for a file that is not UTF-8 or is over
/// 2 MiB).
fn read_original(path: &Path, sandboxed: bool, limit: u64) -> Result<Original, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Original::Absent)
        }
        Err(error) => return Err(format!("无法检查文件：{error}")),
    };
    if !metadata.is_file() {
        return Ok(Original::NotAFile);
    }
    if metadata.len() > limit {
        return Ok(Original::TooLarge(metadata.len()));
    }
    let mut file = if sandboxed {
        crate::path_guard::open_verified_file(path)?
    } else {
        fs::File::open(path).map_err(|error| format!("无法读取文件：{error}"))?
    };
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    // One byte past the limit tells a file that grew since the stat from one that fits.
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("无法读取文件：{error}"))?;
    if bytes.len() as u64 > limit {
        return Ok(Original::TooLarge(bytes.len() as u64));
    }
    Ok(Original::File {
        bytes,
        mode: mode_of(&metadata),
    })
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

/// Windows keeps no mode to put back: a replaced file keeps its attributes and ACL.
#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A content name is a hash and nothing else, so that a row naming `..` cannot
/// walk out of the conversation's directory.
fn is_content_hash(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Writes a content file unless this content is already there: the name is the
/// hash, so the same text edited twice, or in two files, is kept once.
fn save_content(app_data: &Path, conversation_id: &str, hash: &str, bytes: &[u8]) -> Result<(), String> {
    let directory = workspace_dirs::ensure_file_history_dir(app_data, conversation_id)?;
    let target = directory.join(hash);
    if target.is_file() {
        return Ok(());
    }
    atomic_write(&target, bytes)
}

/// Removes the content files among `candidates` that no row names any more.
fn drop_unreferenced(
    store: &ConversationStore,
    app_data: &Path,
    conversation_id: &str,
    candidates: &[String],
) {
    if candidates.is_empty() {
        return;
    }
    let result = (|| -> Result<(), String> {
        let named = store.file_checkpoint_hashes(conversation_id)?;
        let directory = workspace_dirs::file_history_dir(app_data, conversation_id)?;
        for hash in candidates {
            if is_content_hash(hash) && !named.contains(hash) {
                match fs::remove_file(directory.join(hash)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("无法删除备份 {hash}：{error}")),
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("对话 {conversation_id} 的文件检查点备份未能清理：{error}");
    }
}

// -------------------------------------------------------------------- Rewind

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RewindFilesRequest {
    pub conversation_id: String,
    /// The user message whose sending the files go back to before.
    pub context_id: String,
    /// Only report what would happen.
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RewindAction {
    /// Write the earlier content back.
    Restore,
    /// Remove a file that did not exist then.
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RewindFileEntry {
    pub path: String,
    pub display_path: String,
    pub action: RewindAction,
    /// Why the file is, or will be, left alone; in words for the user.
    pub skipped: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RewindFilesResponse {
    pub files: Vec<RewindFileEntry>,
}

/// What the host knows about the conversation's surroundings, which the store
/// does not.
pub(crate) struct RewindContext<'a> {
    /// The conversation's workspace roots on this machine: what a path is shown
    /// relative to, and the workspace a language server is asked about it in.
    pub roots: &'a [PathBuf],
    /// Other conversations whose model runs may be writing the same checkouts.
    /// Putting files back waits for them (a preview does not).
    pub peers: &'a [String],
}

/// Puts the conversation's files back as they were before the user message
/// `request.context_id`, or reports what that would do.
///
/// The caller holds the storage lock, so no run can get past
/// `trusted_run_request` while this goes on; the check below stops one that is
/// already past it.
pub(crate) fn rewind(
    state: &AppState,
    anchor_file: &Path,
    request: RewindFilesRequest,
    context: &RewindContext<'_>,
) -> Result<RewindFilesResponse, String> {
    let conversation_id = request.conversation_id.as_str();
    if state.conversation_model_run_active(conversation_id) {
        return Err(ui_text!(
            "对话正在运行，不能回退文件；等本轮结束或停止后再试",
            "The conversation is running, so its files cannot be rewound; try again once the turn ends or is stopped"
        ));
    }
    if !request.dry_run
        && context
            .peers
            .iter()
            .any(|peer| state.conversation_model_run_active(peer))
    {
        return Err(ui_text!(
            "同一工作区里还有别的对话正在运行，它可能正在改这些文件；等它结束后再回退",
            "Another conversation is running in this workspace and may be changing these files; rewind once it has finished"
        ));
    }
    let store = conversation_store::store_for(anchor_file)?;
    let conversation = store
        .conversation_shared(conversation_id)?
        .ok_or_else(|| ui_text!("对话不存在", "The conversation does not exist"))?;
    let target = conversation
        .contexts
        .iter()
        .position(|item| matches!(item, ContextItem::User { id, .. } if *id == request.context_id))
        .ok_or_else(|| {
            ui_text!(
                "这条消息已不在对话的时间线上",
                "That message is no longer on the conversation's timeline"
            )
        })?;
    let rewound: HashSet<&str> = conversation.contexts[target..]
        .iter()
        .filter_map(|item| match item {
            ContextItem::User { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    // Putting files back is one at a time with recording, so the rows it reads
    // are the rows it deletes.
    let _serial = (!request.dry_run).then(serial);
    let records = store.file_checkpoints(conversation_id)?;
    let Some(start) = records
        .iter()
        .filter(|record| rewound.contains(record.anchor.as_str()))
        .map(|record| record.seq)
        .min()
    else {
        return Ok(RewindFilesResponse { files: Vec::new() });
    };

    let roots = context
        .roots
        .iter()
        .map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect::<Vec<_>>();
    let directory = workspace_dirs::file_history_dir(
        anchor_file.parent().unwrap_or_else(|| Path::new("")),
        conversation_id,
    )?;
    let mut planned = plan(&records, start, &directory, &roots);
    if request.dry_run {
        return Ok(RewindFilesResponse {
            files: planned.into_iter().filter_map(|planned| planned.entry).collect(),
        });
    }

    for item in &mut planned {
        let Some(entry) = item.entry.as_mut().filter(|entry| entry.skipped.is_none()) else {
            continue;
        };
        let outcome = match &item.work {
            Work::Nothing => continue,
            Work::Restore { bytes, mode } => restore(&item.path, bytes, *mode).map_err(|error| {
                ui_text!("写入失败：{error}", "Could not write the file: {error}")
            }),
            Work::Delete => fs::remove_file(&item.path).map_err(|error| {
                ui_text!("删除失败：{error}", "Could not delete the file: {error}")
            }),
        };
        if let Err(reason) = outcome {
            entry.skipped = Some(reason);
        }
    }

    // A file that is back, or never needed to move, is done with: every row about
    // it goes. A file that was skipped keeps its rows for the next try.
    let finished = planned
        .iter()
        .filter(|item| item.entry.as_ref().map_or(true, |entry| entry.skipped.is_none()))
        .collect::<Vec<_>>();
    let seqs = finished
        .iter()
        .flat_map(|item| item.seqs.iter().copied())
        .collect::<Vec<_>>();
    let dropped = records
        .iter()
        .filter(|record| seqs.contains(&record.seq))
        .filter_map(|record| record.content_sha256.clone())
        .collect::<Vec<_>>();
    if let Err(error) = store.delete_file_checkpoints(conversation_id, &seqs) {
        // The files are already back; rows left behind only offer them again.
        eprintln!("对话 {conversation_id} 的文件检查点记录未能清理：{error}");
    } else {
        drop_unreferenced(
            &store,
            anchor_file.parent().unwrap_or_else(|| Path::new("")),
            conversation_id,
            &dropped,
        );
    }

    let paths = finished
        .iter()
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    state.file_read_state.forget_paths(conversation_id, &paths);
    let restored = finished
        .iter()
        .filter(|item| matches!(item.work, Work::Restore { .. }))
        .map(|item| item.path.as_path())
        .collect::<Vec<_>>();
    tell_language_servers(state, conversation_id, &paths, &restored, &roots);

    Ok(RewindFilesResponse {
        files: planned.into_iter().filter_map(|planned| planned.entry).collect(),
    })
}

/// What one file needs, decided against the disk as it is now.
enum Work {
    /// Already as it was.
    Nothing,
    Restore { bytes: Vec<u8>, mode: Option<u32> },
    Delete,
}

struct Planned {
    path: PathBuf,
    /// `None` when there is nothing to tell the user: the file is as it was.
    entry: Option<RewindFileEntry>,
    work: Work,
    /// Every affected row about this file.
    seqs: Vec<i64>,
}

enum Disk {
    Missing,
    Directory,
    Present,
    Unreadable(String),
}

fn inspect(path: &Path) -> Disk {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Disk::Directory,
        Ok(_) => Disk::Present,
        // Below a path that is a file, nothing exists either.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Disk::Missing
        }
        Err(error) => Disk::Unreadable(error.to_string()),
    }
}

/// One entry per file the rows from `start` on touched, in the order each was
/// first touched.
fn plan(
    records: &[FileCheckpointRecord],
    start: i64,
    directory: &Path,
    roots: &[PathBuf],
) -> Vec<Planned> {
    let mut groups: Vec<(&FileCheckpointRecord, Vec<i64>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for record in records.iter().filter(|record| record.seq >= start) {
        match index.get(record.path.as_str()) {
            Some(&at) => groups[at].1.push(record.seq),
            None => {
                index.insert(record.path.as_str(), groups.len());
                groups.push((record, vec![record.seq]));
            }
        }
    }
    let shown_roots = roots.iter().map(|root| plain(root)).collect::<Vec<_>>();
    groups
        .into_iter()
        .map(|(earliest, seqs)| {
            let path = PathBuf::from(&earliest.path);
            let (action, work, skipped) = decide(earliest, &path, directory);
            let entry = match (&work, &skipped) {
                (Work::Nothing, None) => None,
                _ => Some(RewindFileEntry {
                    path: plain(&path),
                    display_path: display_path(&plain(&path), &shown_roots),
                    action,
                    skipped,
                }),
            };
            Planned {
                path,
                entry,
                work,
                seqs,
            }
        })
        .collect()
}

/// What putting `path` back as `earliest` recorded it comes to, against the
/// disk now: the action, the work, and the reason it cannot be done, if any.
fn decide(
    earliest: &FileCheckpointRecord,
    path: &Path,
    directory: &Path,
) -> (RewindAction, Work, Option<String>) {
    let disk = inspect(path);
    let cannot = |action, reason: String| (action, Work::Nothing, Some(reason));
    let folder = || ui_text!("这个路径现在是一个文件夹", "This path is a folder now");
    match earliest.content_sha256.as_deref() {
        // It did not exist: the file the model created is what goes.
        None => match disk {
            Disk::Missing => (RewindAction::Delete, Work::Nothing, None),
            Disk::Directory => cannot(RewindAction::Delete, folder()),
            Disk::Present => (RewindAction::Delete, Work::Delete, None),
            Disk::Unreadable(error) => cannot(
                RewindAction::Delete,
                ui_text!("无法检查这个路径：{error}", "Could not inspect this path: {error}"),
            ),
        },
        Some(NOT_KEPT) => cannot(
            RewindAction::Restore,
            ui_text!(
                "原文件超过 32 MiB，当时没有备份，无法恢复",
                "The original file was over 32 MiB and was not backed up, so it cannot be restored"
            ),
        ),
        Some(hash) => {
            if let Disk::Directory = disk {
                return cannot(RewindAction::Restore, folder());
            }
            if let Disk::Unreadable(error) = disk {
                return cannot(
                    RewindAction::Restore,
                    ui_text!("无法检查这个路径：{error}", "Could not inspect this path: {error}"),
                );
            }
            let Some(bytes) = load_content(directory, hash) else {
                return cannot(
                    RewindAction::Restore,
                    ui_text!(
                        "备份的文件内容已丢失，无法恢复",
                        "The saved copy of this file is missing, so it cannot be restored"
                    ),
                );
            };
            if matches!(disk, Disk::Present) && holds(path, &bytes) {
                return (RewindAction::Restore, Work::Nothing, None);
            }
            (
                RewindAction::Restore,
                Work::Restore {
                    bytes,
                    mode: earliest.mode,
                },
                None,
            )
        }
    }
}

/// The kept bytes of a content file, when the file is there and is what its
/// name says it is.
fn load_content(directory: &Path, hash: &str) -> Option<Vec<u8>> {
    if !is_content_hash(hash) {
        return None;
    }
    let bytes = fs::read(directory.join(hash)).ok()?;
    (sha256_hex(&bytes) == hash).then_some(bytes)
}

/// Whether the file at `path` already holds `bytes`.
fn holds(path: &Path, bytes: &[u8]) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.len() == bytes.len() as u64)
        && fs::read(path).is_ok_and(|current| current == bytes)
}

fn restore(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<(), String> {
    #[cfg(unix)]
    let existed = fs::symlink_metadata(path).is_ok();
    // Creates the parent directories too: the model may have been the one to
    // create the directory a file sat in, and the user the one to remove it.
    atomic_write(path, bytes)?;
    // A file that was there keeps the mode it has now, as every write of the
    // model's does; one that is made again gets the mode it had.
    #[cfg(unix)]
    if !existed {
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        }
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// A path as people write it: without the `\\?\` prefix the canonical form
/// carries on Windows.
fn plain(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// `path` relative to the workspace root it is inside (forward slashes), or
/// whole when it is inside none. With nested roots the innermost one decides.
fn display_path(path: &str, roots: &[String]) -> String {
    let slashed = path.replace('\\', "/");
    roots
        .iter()
        .filter_map(|root| {
            let root = root.replace('\\', "/");
            let root = root.trim_end_matches('/');
            let head = slashed.get(..root.len())?;
            let same = if cfg!(windows) {
                head.eq_ignore_ascii_case(root)
            } else {
                head == root
            };
            let rest = slashed[root.len()..].strip_prefix('/')?;
            (same && !rest.is_empty()).then(|| rest.to_owned())
        })
        .min_by_key(String::len)
        .unwrap_or_else(|| path.to_owned())
}

/// Tells the language servers that already hold these files what became of
/// them. Only servers that are up are told; nothing is started for a rewind.
fn tell_language_servers(
    state: &AppState,
    conversation_id: &str,
    forgotten: &[PathBuf],
    restored: &[&Path],
    roots: &[PathBuf],
) {
    let registry = &state.lsp_servers;
    if registry.tracks_conversation(conversation_id) {
        for path in forgotten {
            registry.forget_file_diagnostics(conversation_id, path);
        }
    }
    if !registry.has_running_servers() {
        return;
    }
    let language = ui_text::current();
    for &path in restored {
        let workspace = roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .map(PathBuf::as_path)
            .or_else(|| path.parent())
            .unwrap_or(path);
        let configs = crate::lsp_config::servers_for_workspace(workspace, language);
        registry.notify_file_changed(&configs, workspace, path, conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Conversation;

    struct Fixture {
        _directory: tempfile::TempDir,
        app_data: PathBuf,
        anchor_file: PathBuf,
        workspace: PathBuf,
        store: Arc<ConversationStore>,
        state: AppState,
        conversation_id: String,
    }

    fn user(id: &str) -> ContextItem {
        ContextItem::User {
            id: id.into(),
            content: format!("message {id}"),
            images: Vec::new(),
            files: Vec::new(),
            created_at: "2026-10-13T00:00:00Z".into(),
        }
    }

    fn assistant(id: &str) -> ContextItem {
        ContextItem::Assistant {
            id: id.into(),
            content: "done".into(),
            round: None,
            model_turn_id: None,
            interrupted: false,
            sources: Vec::new(),
            created_at: "2026-10-13T00:00:01Z".into(),
        }
    }

    impl Fixture {
        /// A conversation whose timeline is these user messages, each answered.
        fn new(messages: &[&str]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let app_data = fs::canonicalize(directory.path()).unwrap();
            let workspace = app_data.join("workspace");
            fs::create_dir_all(&workspace).unwrap();
            let anchor_file = app_data.join("document.v1.json");
            let store = conversation_store::store_for(&anchor_file).unwrap();
            let document = crate::catalog::default_document();
            let mut conversation: Conversation = document.workspaces[0].conversations[0].clone();
            conversation.id = "conv_checkpoints".into();
            conversation.contexts = messages
                .iter()
                .flat_map(|id| [user(id), assistant(&format!("{id}-answer"))])
                .collect();
            store
                .put_conversation(&document.workspaces[0].id, &conversation)
                .unwrap();
            Self {
                _directory: directory,
                app_data,
                anchor_file,
                workspace,
                store,
                state: AppState::default(),
                conversation_id: conversation.id,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.workspace.join(name)
        }

        fn context<'a>(&'a self, anchor: &'a str) -> CheckpointContext<'a> {
            CheckpointContext {
                conversation_id: &self.conversation_id,
                anchor,
                app_data: &self.app_data,
            }
        }

        /// What `write` does to a file: the checkpoint first, then the change.
        fn write(&self, anchor: &str, name: &str, content: &str) {
            let path = self.path(name);
            record_before_write(Some(self.context(anchor)), &path, false);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
        }

        fn seed(&self, name: &str, content: &str) {
            let path = self.path(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn read(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.path(name)).ok()
        }

        fn rewind(&self, to: &str, dry_run: bool) -> Result<RewindFilesResponse, String> {
            rewind(
                &self.state,
                &self.anchor_file,
                RewindFilesRequest {
                    conversation_id: self.conversation_id.clone(),
                    context_id: to.into(),
                    dry_run,
                },
                &RewindContext {
                    roots: std::slice::from_ref(&self.workspace),
                    peers: &[],
                },
            )
        }

        fn rows(&self) -> Vec<FileCheckpointRecord> {
            self.store.file_checkpoints(&self.conversation_id).unwrap()
        }

        fn history_files(&self) -> Vec<String> {
            let directory = workspace_dirs::file_history_dir(&self.app_data, &self.conversation_id).unwrap();
            let mut names = fs::read_dir(directory)
                .map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    fn files(response: &RewindFilesResponse) -> Vec<(&str, RewindAction, bool)> {
        response
            .files
            .iter()
            .map(|entry| (entry.display_path.as_str(), entry.action, entry.skipped.is_some()))
            .collect()
    }

    #[test]
    fn a_file_is_kept_once_per_user_message() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        fixture.write("u1", "a.txt", "two");

        let rows = fixture.rows();
        assert_eq!(rows.len(), 1, "the second change under the same message adds nothing");
        assert_eq!(rows[0].anchor, "u1");
        assert_eq!(rows[0].path, fixture.path("a.txt").to_str().unwrap());
        assert_eq!(rows[0].content_sha256.as_deref(), Some(sha256_hex(b"zero").as_str()));
        assert_eq!(fixture.history_files(), [sha256_hex(b"zero")]);
        assert_eq!(
            fs::read(workspace_dirs::file_history_dir(&fixture.app_data, &fixture.conversation_id).unwrap().join(sha256_hex(b"zero"))).unwrap(),
            b"zero"
        );
    }

    #[test]
    fn the_bytes_are_kept_whatever_they_are() {
        let fixture = Fixture::new(&["u1"]);
        let path = fixture.path("blob.bin");
        let bytes = [0xff, 0xfe, 0x00, 0x80, b'a'];
        fs::write(&path, bytes).unwrap();
        fixture.write("u1", "blob.bin", "text now");
        assert_eq!(fixture.rewind("u1", false).unwrap().files.len(), 1);
        assert_eq!(fs::read(&path).unwrap(), bytes, "a file that is not UTF-8 comes back byte for byte");
    }

    #[test]
    fn a_file_that_did_not_exist_is_recorded_as_such_and_kept_nowhere() {
        let fixture = Fixture::new(&["u1"]);
        fixture.write("u1", "new/created.txt", "hello");
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content_sha256, None);
        assert!(fixture.history_files().is_empty());
    }

    #[test]
    fn each_user_message_keeps_what_it_found() {
        let fixture = Fixture::new(&["u1", "u2"]);
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        fixture.write("u2", "a.txt", "two");
        let rows = fixture.rows();
        assert_eq!(
            rows.iter().map(|row| row.anchor.as_str()).collect::<Vec<_>>(),
            ["u1", "u2"]
        );
        assert_eq!(rows[0].content_sha256.as_deref(), Some(sha256_hex(b"zero").as_str()));
        assert_eq!(rows[1].content_sha256.as_deref(), Some(sha256_hex(b"one").as_str()));
        assert_eq!(fixture.history_files().len(), 2);
    }

    #[test]
    fn a_change_with_no_context_or_no_conversation_is_not_recorded() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("a.txt", "zero");
        record_before_write(None, &fixture.path("a.txt"), false);
        let stranger = CheckpointContext {
            conversation_id: "conv_nobody",
            anchor: "u1",
            app_data: &fixture.app_data,
        };
        record_before_write(Some(stranger), &fixture.path("a.txt"), false);
        assert!(fixture.rows().is_empty());
        assert!(fixture.history_files().is_empty(), "nothing is kept for a conversation that is not there");
    }

    #[test]
    fn a_file_past_the_size_limit_is_changed_but_not_kept() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("big.txt", "0123456789");
        let path = fixture.path("big.txt");
        let small = Limits {
            backup_bytes: 4,
            anchors: MAX_ANCHORS,
        };
        record(fixture.context("u1"), &path, false, small).expect("a file too large is not an error");
        fs::write(&path, "changed").unwrap();
        // Nothing was copied, and the row says so: a later change under the same
        // message must not take the first change's result for the original.
        assert!(fixture.history_files().is_empty());
        assert_eq!(fixture.rows()[0].content_sha256.as_deref(), Some(NOT_KEPT));
        record(fixture.context("u1"), &path, false, LIMITS).unwrap();
        assert_eq!(fixture.rows().len(), 1);
        assert!(fixture.history_files().is_empty());

        let response = fixture.rewind("u1", false).unwrap();
        assert_eq!(files(&response), [("big.txt", RewindAction::Restore, true)]);
        assert!(response.files[0].skipped.as_deref().unwrap().contains("32 MiB"));
        assert_eq!(fixture.read("big.txt").as_deref(), Some("changed"), "nothing was guessed");
        assert_eq!(fixture.rows().len(), 1, "the row stays: the file is still to be reported");
    }

    #[test]
    fn only_the_most_recent_user_messages_keep_their_checkpoints() {
        let fixture = Fixture::new(&["u1"]);
        let limits = Limits {
            backup_bytes: MAX_BACKUP_BYTES,
            anchors: 3,
        };
        // The first and the fourth message found the same text: the copy the
        // first one drops is wanted by the fourth, which stays.
        let found = ["shared", "f2", "f3", "shared", "f5"];
        for (index, text) in found.iter().enumerate() {
            let name = format!("f{}.txt", index + 1);
            fixture.seed(&name, text);
            record(fixture.context(&format!("m{}", index + 1)), &fixture.path(&name), false, limits)
                .unwrap();
        }
        let rows = fixture.rows();
        assert_eq!(
            rows.iter().map(|row| row.anchor.as_str()).collect::<Vec<_>>(),
            ["m3", "m4", "m5"]
        );
        let mut expected = vec![sha256_hex(b"f3"), sha256_hex(b"shared"), sha256_hex(b"f5")];
        expected.sort();
        assert_eq!(
            fixture.history_files(),
            expected,
            "the copies of the dropped messages go; the one a kept message shares stays"
        );
    }

    #[test]
    fn the_limit_in_force_is_a_hundred_messages() {
        let fixture = Fixture::new(&["u1"]);
        for index in 0..=MAX_ANCHORS {
            let name = format!("f{index}.txt");
            fixture.seed(&name, &format!("text {index}"));
            record_before_write(Some(fixture.context(&format!("m{index}"))), &fixture.path(&name), false);
        }
        let rows = fixture.rows();
        assert_eq!(rows.len(), MAX_ANCHORS);
        assert_eq!(rows[0].anchor, "m1", "the oldest message of the 101 is the one dropped");
        assert_eq!(fixture.history_files().len(), MAX_ANCHORS);
    }

    #[test]
    fn a_modified_file_comes_back_and_a_created_one_goes() {
        let fixture = Fixture::new(&["u1", "u2"]);
        fixture.seed("config.json", "{\"port\":80}");
        fixture.write("u2", "config.json", "{\"port\":8080}");
        fixture.write("u2", "src/new.rs", "fn main() {}");

        let response = fixture.rewind("u2", false).unwrap();
        assert_eq!(
            files(&response),
            [
                ("config.json", RewindAction::Restore, false),
                ("src/new.rs", RewindAction::Delete, false),
            ]
        );
        assert_eq!(fixture.read("config.json").as_deref(), Some("{\"port\":80}"));
        assert!(!fixture.path("src/new.rs").exists());
        assert!(fixture.path("src").is_dir(), "only the file goes, not the directory it made");
        assert!(fixture.rows().is_empty(), "what is done with leaves no rows");
        assert!(fixture.history_files().is_empty(), "nor copies");
        assert_eq!(response.files[0].path, plain(&fixture.path("config.json")));
    }

    #[test]
    fn a_file_the_user_removed_is_made_again() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("deep/dir/a.txt", "original");
        fixture.write("u1", "deep/dir/a.txt", "changed");
        fs::remove_dir_all(fixture.path("deep")).unwrap();
        let response = fixture.rewind("u1", false).unwrap();
        assert_eq!(files(&response), [("deep/dir/a.txt", RewindAction::Restore, false)]);
        assert_eq!(fixture.read("deep/dir/a.txt").as_deref(), Some("original"));
    }

    #[test]
    fn the_earliest_change_at_or_after_the_message_says_how_a_file_looked() {
        let fixture = Fixture::new(&["u1", "u2", "u3"]);
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        fixture.write("u2", "a.txt", "two");
        fixture.write("u3", "a.txt", "three");

        let response = fixture.rewind("u2", false).unwrap();
        assert_eq!(files(&response), [("a.txt", RewindAction::Restore, false)]);
        assert_eq!(fixture.read("a.txt").as_deref(), Some("one"), "before u2 it held what u1 left");
        assert_eq!(fixture.rows().len(), 1, "u1's own row stays");
        assert_eq!(fixture.rows()[0].anchor, "u1");

        assert_eq!(fixture.rewind("u1", false).unwrap().files.len(), 1);
        assert_eq!(fixture.read("a.txt").as_deref(), Some("zero"));
        assert!(fixture.rows().is_empty());
    }

    #[test]
    fn rewinding_to_a_message_whose_messages_changed_nothing_does_nothing() {
        let fixture = Fixture::new(&["u1", "u2"]);
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        // u2 and everything after it changed no file.
        assert!(fixture.rewind("u2", false).unwrap().files.is_empty());
        assert_eq!(fixture.read("a.txt").as_deref(), Some("one"));
        assert_eq!(fixture.rows().len(), 1);
    }

    #[test]
    fn rows_left_by_a_deleted_message_count_in_the_order_they_were_recorded() {
        let fixture = Fixture::new(&["u1", "u2", "u3"]);
        fixture.seed("one.txt", "1");
        fixture.seed("two.txt", "2");
        fixture.seed("three.txt", "3");
        fixture.write("u1", "one.txt", "x");
        fixture.write("u2", "two.txt", "x");
        fixture.write("u3", "three.txt", "x");
        // The user deletes u2 from the timeline; its row stays where it was recorded.
        let mut conversation = (*fixture.store.conversation_shared(&fixture.conversation_id).unwrap().unwrap()).clone();
        conversation.contexts.retain(|item| !matches!(item.id(), "u2" | "u2-answer"));
        fixture.store.put_conversation(&crate::catalog::default_document().workspaces[0].id, &conversation).unwrap();

        // From u3 on, only u3's own change is rewound: u2's row precedes it.
        assert_eq!(files(&fixture.rewind("u3", true).unwrap()), [("three.txt", RewindAction::Restore, false)]);
        // From u1 on, u2's row comes after u1's, so it is rewound too.
        assert_eq!(
            files(&fixture.rewind("u1", true).unwrap()),
            [
                ("one.txt", RewindAction::Restore, false),
                ("two.txt", RewindAction::Restore, false),
                ("three.txt", RewindAction::Restore, false),
            ]
        );
    }

    #[test]
    fn a_message_that_is_not_on_the_timeline_is_refused() {
        let fixture = Fixture::new(&["u1"]);
        assert!(fixture.rewind("never-sent", true).is_err());
        // An answer is not a message the user sent.
        assert!(fixture.rewind("u1-answer", true).is_err());
        let mut request = RewindFilesRequest {
            conversation_id: "conv_nobody".into(),
            context_id: "u1".into(),
            dry_run: true,
        };
        let context = RewindContext { roots: &[], peers: &[] };
        assert!(rewind(&fixture.state, &fixture.anchor_file, request, &context).is_err());
        request = RewindFilesRequest {
            conversation_id: fixture.conversation_id.clone(),
            context_id: "u1".into(),
            dry_run: true,
        };
        assert!(rewind(&fixture.state, &fixture.anchor_file, request, &context).unwrap().files.is_empty());
    }

    #[test]
    fn a_file_already_as_it_was_is_not_listed_but_its_rows_are_done_with() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("kept.txt", "zero");
        fixture.write("u1", "kept.txt", "one");
        fixture.write("u1", "made.txt", "made");
        // The user put both back by hand.
        fixture.seed("kept.txt", "zero");
        fs::remove_file(fixture.path("made.txt")).unwrap();

        assert!(fixture.rewind("u1", true).unwrap().files.is_empty());
        assert_eq!(fixture.rows().len(), 2, "a preview deletes nothing");
        let response = fixture.rewind("u1", false).unwrap();
        assert!(response.files.is_empty());
        assert!(fixture.rows().is_empty());
        assert!(fixture.history_files().is_empty());
    }

    #[test]
    fn a_preview_changes_nothing_and_reports_what_a_rewind_does() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        fixture.write("u1", "b.txt", "made");

        let preview = fixture.rewind("u1", true).unwrap();
        assert_eq!(fixture.read("a.txt").as_deref(), Some("one"));
        assert_eq!(fixture.read("b.txt").as_deref(), Some("made"));
        assert_eq!(fixture.rows().len(), 2);
        assert_eq!(fixture.history_files().len(), 1);

        let done = fixture.rewind("u1", false).unwrap();
        assert_eq!(preview, done, "the same entries before and after");
        assert_eq!(fixture.read("a.txt").as_deref(), Some("zero"));
        assert!(fixture.read("b.txt").is_none());
    }

    #[test]
    fn a_file_whose_saved_copy_is_gone_is_skipped_and_kept_for_another_try() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("a.txt", "zero");
        fixture.seed("b.txt", "zero b");
        fixture.write("u1", "a.txt", "one");
        fixture.write("u1", "b.txt", "one b");
        let directory = workspace_dirs::file_history_dir(&fixture.app_data, &fixture.conversation_id).unwrap();
        fs::remove_file(directory.join(sha256_hex(b"zero"))).unwrap();
        // A copy that is not what its name says counts as lost too.
        fs::write(directory.join(sha256_hex(b"zero b")), "tampered").unwrap();

        let response = fixture.rewind("u1", false).unwrap();
        assert_eq!(
            files(&response),
            [("a.txt", RewindAction::Restore, true), ("b.txt", RewindAction::Restore, true)]
        );
        assert!(response.files[0].skipped.as_deref().unwrap().contains("备份"));
        assert_eq!(fixture.read("a.txt").as_deref(), Some("one"));
        assert_eq!(fixture.read("b.txt").as_deref(), Some("one b"));
        assert_eq!(fixture.rows().len(), 2, "both rows stay");
    }

    #[test]
    fn a_path_that_is_a_folder_now_is_skipped() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("was-file", "text");
        fixture.write("u1", "was-file", "changed");
        fixture.write("u1", "was-new", "made");
        for name in ["was-file", "was-new"] {
            fs::remove_file(fixture.path(name)).unwrap();
            fs::create_dir(fixture.path(name)).unwrap();
        }
        let response = fixture.rewind("u1", false).unwrap();
        assert_eq!(
            files(&response),
            [("was-file", RewindAction::Restore, true), ("was-new", RewindAction::Delete, true)]
        );
        assert!(fixture.path("was-file").is_dir() && fixture.path("was-new").is_dir());
        assert_eq!(fixture.rows().len(), 2);
    }

    #[test]
    fn a_file_that_cannot_be_written_is_skipped_with_the_systems_reason_and_other_files_still_go() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("fine.txt", "zero");
        fixture.seed("dir/a.txt", "zero");
        fixture.write("u1", "dir/a.txt", "one");
        fixture.write("u1", "fine.txt", "one");
        // The directory the file sat in is a file now, so it cannot be made again.
        fs::remove_dir_all(fixture.path("dir")).unwrap();
        fs::write(fixture.path("dir"), "in the way").unwrap();

        let response = fixture.rewind("u1", false).unwrap();
        assert_eq!(
            files(&response),
            [("dir/a.txt", RewindAction::Restore, true), ("fine.txt", RewindAction::Restore, false)]
        );
        assert!(response.files[0].skipped.as_deref().unwrap().len() > 4);
        assert_eq!(fixture.read("fine.txt").as_deref(), Some("zero"));
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1, "only the file that could not be put back keeps its row");
        assert!(rows[0].path.ends_with("a.txt"));
    }

    #[test]
    fn a_running_conversation_refuses_both_the_preview_and_the_rewind() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        let (cancel, _) = fixture.state.begin_model_run("run-1", &fixture.conversation_id).unwrap();
        for dry_run in [true, false] {
            let error = fixture.rewind("u1", dry_run).expect_err("a run is going");
            assert!(error.contains("正在运行"), "{error}");
        }
        assert_eq!(fixture.read("a.txt").as_deref(), Some("one"));
        assert_eq!(fixture.rows().len(), 1);
        fixture.state.finish_model_run("run-1", &cancel);
        assert_eq!(fixture.rewind("u1", false).unwrap().files.len(), 1);
    }

    #[test]
    fn another_conversation_writing_the_same_checkouts_stops_the_rewind_but_not_the_preview() {
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        let (cancel, _) = fixture.state.begin_model_run("run-peer", "conv_peer").unwrap();
        let peers = ["conv_peer".to_owned()];
        let attempt = |dry_run| {
            rewind(
                &fixture.state,
                &fixture.anchor_file,
                RewindFilesRequest {
                    conversation_id: fixture.conversation_id.clone(),
                    context_id: "u1".into(),
                    dry_run,
                },
                &RewindContext {
                    roots: std::slice::from_ref(&fixture.workspace),
                    peers: &peers,
                },
            )
        };
        assert_eq!(attempt(true).unwrap().files.len(), 1);
        assert!(attempt(false).is_err());
        assert_eq!(fixture.read("a.txt").as_deref(), Some("one"));
        fixture.state.finish_model_run("run-peer", &cancel);
        assert_eq!(attempt(false).unwrap().files.len(), 1);
        assert_eq!(fixture.read("a.txt").as_deref(), Some("zero"));
    }

    #[test]
    fn the_models_memory_of_a_file_is_dropped_so_it_has_to_read_it_again() {
        use crate::file_read_state::{FileReadRecord, ScopeRef};
        let fixture = Fixture::new(&["u1"]);
        fixture.state.file_read_state.attach_store(fixture.store.clone());
        fixture.seed("a.txt", "zero");
        fixture.write("u1", "a.txt", "one");
        fixture.seed("other.txt", "untouched");
        let scope = ScopeRef { id: &fixture.conversation_id, parent: None };
        let child_id = crate::file_read_state::child_scope_id(&fixture.conversation_id);
        let child = ScopeRef { id: &child_id, parent: Some(&fixture.conversation_id) };
        let registry = &fixture.state.file_read_state;
        for path in [fixture.path("a.txt"), fixture.path("other.txt")] {
            registry.record(scope, path, FileReadRecord::full_read(1, "one\n".into()));
        }
        assert!(registry.get(child, &fixture.path("a.txt")).is_some());

        // A preview leaves the memory alone.
        fixture.rewind("u1", true).unwrap();
        assert!(registry.get(scope, &fixture.path("a.txt")).is_some());

        fixture.rewind("u1", false).unwrap();
        assert!(registry.get(scope, &fixture.path("a.txt")).is_none());
        assert!(registry.get(child, &fixture.path("a.txt")).is_none());
        assert!(registry.get(scope, &fixture.path("other.txt")).is_some());
        assert!(
            fixture.store.file_read_records(&fixture.conversation_id).unwrap().iter().all(|saved| saved.path != fixture.path("a.txt")),
            "the saved record goes too"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_made_again_gets_the_mode_it_had() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new(&["u1"]);
        fixture.seed("run.sh", "#!/bin/sh\n");
        fs::set_permissions(fixture.path("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fixture.write("u1", "run.sh", "changed");
        fs::remove_file(fixture.path("run.sh")).unwrap();
        fixture.rewind("u1", false).unwrap();
        assert_eq!(
            fs::metadata(fixture.path("run.sh")).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    }

    #[test]
    fn the_saved_conversations_newest_real_message_is_the_anchor_of_a_request_with_no_timeline() {
        let fixture = Fixture::new(&["u1", "u2"]);
        // The host's own user cards are not messages the user sent.
        let mut conversation = (*fixture.store.conversation_shared(&fixture.conversation_id).unwrap().unwrap()).clone();
        conversation.contexts.push(user("ctx_agent-result_late"));
        conversation.contexts.push(user("ctx_hook-continuation_x"));
        fixture.store.put_conversation(&crate::catalog::default_document().workspaces[0].id, &conversation).unwrap();
        assert_eq!(
            anchor_from_store(&fixture.app_data, &fixture.conversation_id).as_deref(),
            Some("u2")
        );
        assert_eq!(anchor_from_store(&fixture.app_data, "conv_nobody"), None);
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(anchor_from_store(empty.path(), &fixture.conversation_id), None);
        assert!(
            !empty.path().join(conversation_store::DATABASE_FILE_NAME).exists(),
            "looking creates no database"
        );
    }

    #[test]
    fn paths_are_shown_relative_to_the_workspace_that_holds_them() {
        let roots = vec!["C:\\work\\app".to_owned(), "C:\\work\\app\\vendor\\lib".to_owned()];
        assert_eq!(display_path("C:\\work\\app\\src\\main.rs", &roots), "src/main.rs");
        assert_eq!(display_path("C:\\work\\app\\vendor\\lib\\a.c", &roots), "a.c", "the innermost root decides");
        assert_eq!(display_path("C:\\elsewhere\\x.txt", &roots), "C:\\elsewhere\\x.txt");
        assert_eq!(display_path("C:\\work\\apple\\x.txt", &roots), "C:\\work\\apple\\x.txt", "a sibling is not inside");
        assert_eq!(display_path("/srv/app/src/a.rs", &["/srv/app/".to_owned()]), "src/a.rs");
        assert_eq!(plain(Path::new(r"\\?\C:\work\a.txt")), r"C:\work\a.txt");
        assert_eq!(plain(Path::new(r"\\?\UNC\host\share\a.txt")), r"\\host\share\a.txt");
    }
}
