//! Language servers: spawning them, speaking the LSP base protocol to them,
//! and keeping the ones a workspace needs alive between calls.
//!
//! The process machinery is the same shape as [`crate::preview_servers`] — a
//! registry behind one mutex, a Windows job object so the whole tree dies
//! together, stderr drained so the child never blocks on a full pipe. What is
//! new is the framing: every existing pipe in this repo is newline-delimited
//! JSON, and LSP is `Content-Length: N\r\n\r\n<body>`, so the codec below is
//! written fresh rather than borrowed from [`crate::mcp`].
//!
//! Servers start lazily, on the first request for a file whose extension they
//! claim — or on the first edit of such a file, in the background, so the edit
//! never waits for it — and are keyed by `(machine, root, name)` so every
//! conversation in a workspace shares one `rust-analyzer` rather than paying its
//! index cost again. At most one start per key is in flight at any moment: a
//! second caller, whether an edit or a navigation call, joins it.
//!
//! A workspace on another machine gets its server *there*: the process is
//! started through that machine's shell transport — the same `wsl.exe` or
//! `ssh` invocation the remote file tools use — and speaks the base protocol
//! over the wrapper's pipes. That is how Claude Code has it when it runs on a
//! remote: the language server is a child of whatever runs where the code is,
//! never a host-side process guessing at files it cannot open. Only the
//! wrapper lives in this process; the paths, the `rootUri` and every
//! `file://` URI are the remote machine's.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Map, Value};

use crate::{
    lsp_config::LspServerConfig,
    run_environment::ShellRunner,
    tool_executor::{kill_shell_child, ShellChild, ShellJob},
};

/// Where a language server runs: in this process, or on the machine a
/// workspace lives on.
#[derive(Clone, Debug)]
pub enum ServerHost {
    Local,
    Remote {
        /// The transport to that machine, with its variable table.
        runner: ShellRunner,
        /// `run_environment::env_key` of the machine — its identity everywhere
        /// else in the host, and the first component of every key here.
        machine_key: String,
    },
}

impl ServerHost {
    /// The machine component of a [`ServerRoot`]: `local` for this process,
    /// the machine's environment key otherwise.
    pub fn machine_key(&self) -> String {
        match self {
            Self::Local => crate::run_environment::env_key(None),
            Self::Remote { machine_key, .. } => machine_key.clone(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }
}

/// The directory one server indexes, on the machine it indexes it.
///
/// Two machines can each have a `/srv/app`; a diagnostics fan-out or a registry
/// lookup that compared paths alone would hand one machine's compile errors to
/// a conversation working on the other.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServerRoot {
    pub machine: String,
    pub path: PathBuf,
}

impl ServerRoot {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self {
            machine: crate::run_environment::env_key(None),
            path: path.into(),
        }
    }

    pub fn new(host: &ServerHost, path: impl Into<PathBuf>) -> Self {
        Self {
            machine: host.machine_key(),
            path: path.into(),
        }
    }
}

/// Largest message body accepted from a server, matching the source's 32 MiB
/// cap. A server that declares more has desynchronized from the protocol.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Largest header block accepted before the blank line.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Stderr kept per server for the settings row. Never reaches the model.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Diagnostics volume limits, ported from the source: at most this many per
/// file and this many in total in one injection, and this many fingerprints
/// remembered per conversation so the same problem is not reported twice.
const MAX_DIAGNOSTICS_PER_FILE: usize = 10;
const MAX_DIAGNOSTICS_TOTAL: usize = 30;
const MAX_DELIVERED_FINGERPRINTS: usize = 500;
/// Longest rendered diagnostics body, after which it is cut with a marker.
const MAX_DIAGNOSTICS_CHARS: usize = 4_000;

/// JSON-RPC error code a server returns when the document changed under a
/// request. The source retries these three times with exponential backoff.
const CONTENT_MODIFIED: i64 = -32801;
const CONTENT_MODIFIED_RETRIES: u32 = 3;
const CONTENT_MODIFIED_BACKOFF: Duration = Duration::from_millis(500);

/// Wall clock one LSP request gets.
///
/// The source applies **no** per-request timeout: its tool call is driven by an
/// abort signal and a user who can press escape. Mewrk runs builtin tools in a
/// synchronous call slot, so an unbounded request would wedge the turn on any
/// server that simply never answers. This deadline is therefore a deliberate
/// deviation, not an omission.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Wall clock one outbound frame gets before the caller gives up on the pipe.
///
/// Distinct from [`REQUEST_TIMEOUT`]: that one bounds the *answer*, this one
/// bounds the *ask*. A server that stopped draining its stdin would otherwise
/// block the writer forever with no deadline anywhere.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a `shutdown`/`exit` handshake is given before the tree is killed.
const STOP_GRACE: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Base-protocol framing
// ---------------------------------------------------------------------------

/// Reads one `Content-Length`-framed message body.
///
/// Returns `Ok(None)` at a clean end of stream. Any other shape is an error:
/// the source kills the process on a protocol violation rather than trying to
/// resynchronize, because a server writing logs to stdout will never recover.
fn read_message(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = Vec::new();
        let read = reader
            .take((MAX_HEADER_BYTES - header_bytes + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("Failed to read from the language server: {error}"))?;
        if read == 0 {
            return if content_length.is_none() && header_bytes == 0 {
                Ok(None)
            } else {
                Err("The language server closed its output mid-message".into())
            };
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(
                "The language server sent a header block larger than the protocol allows".into(),
            );
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            break;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            return Err(
                "The language server sent non-protocol output in the header block — its stdout is desynchronized (logs on stdout instead of stderr?)"
                    .into(),
            );
        };
        let Some((name, value)) = text.split_once(':') else {
            return Err(
                "The language server sent non-protocol output in the header block — its stdout is desynchronized (logs on stdout instead of stderr?)"
                    .into(),
            );
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = content_length else {
        return Err(
            "The language server sent a header block without a Content-Length — its stdout is desynchronized from the base protocol"
                .into(),
        );
    };
    if length > MAX_BODY_BYTES {
        return Err(
            "The language server declared a message larger than the size limit — refusing to buffer it".into(),
        );
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("Failed to read the language server's message body: {error}"))?;
    Ok(Some(body))
}

/// Frames one message for the base protocol: `Content-Length` then the body.
fn encode_message(payload: &Value) -> Result<Vec<u8>, String> {
    let body = serde_json::to_vec(payload)
        .map_err(|error| format!("Could not encode an LSP message: {error}"))?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    Ok(frame)
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// One problem a server reported, already flattened into what the injection
/// prints.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub severity: String,
    pub line: u64,
    pub character: u64,
    pub message: String,
    pub code: String,
    pub source: String,
}

/// `publishDiagnostics` severity integers, as the source maps them.
fn severity_name(value: Option<&Value>) -> String {
    match value.and_then(Value::as_u64) {
        Some(2) => "Warning",
        Some(3) => "Info",
        Some(4) => "Hint",
        _ => "Error",
    }
    .to_owned()
}

/// The latest published set for one file. LSP replaces the whole set per URI,
/// so this is assignment, never accumulation.
#[derive(Clone, Debug, Default)]
struct FileDiagnostics {
    diagnostics: Vec<Diagnostic>,
}

/// What one conversation is owed.
#[derive(Debug, Default)]
struct ConversationLedger {
    /// The roots this conversation has reached a language server in. A publish
    /// from any other root is not its business — without this, a conversation
    /// in one project would be handed another project's compile errors.
    roots: BTreeSet<ServerRoot>,
    pending: BTreeSet<String>,
    delivered: VecDeque<Delivered>,
}

/// One problem this conversation has been shown. The file is kept beside the
/// fingerprint (which already folds the file in, but one-way) so an edit can
/// make a single file's problems reportable again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Delivered {
    file: u64,
    fingerprint: u64,
}

/// Published diagnostics, and who has already been told about them.
///
/// A conversation has to register before it is owed anything: without that, a
/// conversation that never touched a language server would open with a backlog
/// of problems from somebody else's turn.
#[derive(Debug, Default)]
struct DiagnosticsLedger {
    current: HashMap<String, FileDiagnostics>,
    /// Insertion order of `current`, so the oldest file is evicted first.
    order: VecDeque<String>,
    conversations: HashMap<String, ConversationLedger>,
    /// Insertion order of `conversations`, same reason.
    conversation_order: VecDeque<String>,
}

/// Files whose latest diagnostics are kept. A session that touches more than
/// this many files drops the oldest rather than growing for its whole life.
const MAX_TRACKED_FILES: usize = 512;
/// Conversations tracked at once, evicted oldest-first.
const MAX_TRACKED_CONVERSATIONS: usize = 64;

impl DiagnosticsLedger {
    fn register(&mut self, conversation_id: &str, root: &ServerRoot) {
        if !self.conversations.contains_key(conversation_id) {
            self.conversation_order.push_back(conversation_id.to_owned());
            while self.conversation_order.len() > MAX_TRACKED_CONVERSATIONS {
                if let Some(oldest) = self.conversation_order.pop_front() {
                    self.conversations.remove(&oldest);
                }
            }
        }
        self.conversations
            .entry(conversation_id.to_owned())
            .or_default()
            .roots
            .insert(root.clone());
    }

    fn publish(&mut self, root: &ServerRoot, uri: String, diagnostics: Vec<Diagnostic>) {
        let empty = diagnostics.is_empty();
        if !self.current.contains_key(&uri) {
            self.order.push_back(uri.clone());
            while self.order.len() > MAX_TRACKED_FILES {
                if let Some(oldest) = self.order.pop_front() {
                    self.current.remove(&oldest);
                    for ledger in self.conversations.values_mut() {
                        ledger.pending.remove(&oldest);
                    }
                }
            }
        }
        self.current
            .insert(uri.clone(), FileDiagnostics { diagnostics });
        // An empty publish is how a server says "this file is clean now". It
        // clears the stored set but is not itself news, so it is not queued.
        if empty {
            for ledger in self.conversations.values_mut() {
                ledger.pending.remove(&uri);
            }
            return;
        }
        for ledger in self.conversations.values_mut() {
            if ledger.roots.contains(root) {
                ledger.pending.insert(uri.clone());
            }
        }
    }

    /// Drains what `conversation_id` has not been shown, applying the source's
    /// dedup and volume limits.
    fn take_new(&mut self, conversation_id: &str) -> Vec<(String, Vec<Diagnostic>)> {
        let Some(ledger) = self.conversations.get_mut(conversation_id) else {
            return Vec::new();
        };
        let pending = std::mem::take(&mut ledger.pending);
        let mut out: Vec<(String, Vec<Diagnostic>)> = Vec::new();
        let mut total = 0usize;
        for uri in pending {
            if total >= MAX_DIAGNOSTICS_TOTAL {
                break;
            }
            let Some(file) = self.current.get(&uri) else {
                continue;
            };
            let file_key = hash_file(&uri);
            let mut fresh = Vec::new();
            for diagnostic in &file.diagnostics {
                if fresh.len() >= MAX_DIAGNOSTICS_PER_FILE || total + fresh.len() >= MAX_DIAGNOSTICS_TOTAL
                {
                    break;
                }
                let fingerprint = fingerprint_diagnostic(&uri, diagnostic);
                if ledger
                    .delivered
                    .iter()
                    .any(|shown| shown.fingerprint == fingerprint)
                {
                    continue;
                }
                ledger.delivered.push_back(Delivered {
                    file: file_key,
                    fingerprint,
                });
                while ledger.delivered.len() > MAX_DELIVERED_FINGERPRINTS {
                    ledger.delivered.pop_front();
                }
                fresh.push(diagnostic.clone());
            }
            if !fresh.is_empty() {
                total += fresh.len();
                out.push((uri, fresh));
            }
        }
        out
    }

    /// Makes `conversation_id`'s problems in one file reportable again after
    /// the file was edited: what it was already shown is forgotten, and what
    /// was queued for it (published for the text as it was) is dropped.
    ///
    /// Without this, a problem that is still there after an edit is the same
    /// fingerprint as the one reported before it and is never mentioned again —
    /// the model fixes one error, the other it was told about stays out of
    /// sight. Only this conversation's records are touched: another
    /// conversation in the same workspace did not make the edit, and what it
    /// has or has not been told is its own, and so is the file's stored set,
    /// which the server replaces by publishing again.
    fn forget_file(&mut self, conversation_id: &str, uri: &str) {
        let Some(ledger) = self.conversations.get_mut(conversation_id) else {
            return;
        };
        ledger.pending.remove(uri);
        let file_key = hash_file(uri);
        ledger.delivered.retain(|shown| shown.file != file_key);
    }
}

fn hash_file(uri: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    uri.hash(&mut hasher);
    hasher.finish()
}

fn fingerprint_diagnostic(uri: &str, diagnostic: &Diagnostic) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    uri.hash(&mut hasher);
    diagnostic.severity.hash(&mut hasher);
    diagnostic.line.hash(&mut hasher);
    diagnostic.character.hash(&mut hasher);
    diagnostic.message.hash(&mut hasher);
    diagnostic.code.hash(&mut hasher);
    hasher.finish()
}

/// Renders what [`LspRegistry::take_new_diagnostics`] returned into the block
/// the model reads. Verbatim from the source, including the wrapper tag, the
/// per-line shape and the 4000-character cut.
pub fn render_diagnostics(files: &[(String, Vec<Diagnostic>)]) -> String {
    let body = files
        .iter()
        .map(|(uri, diagnostics)| {
            let name = uri.rsplit('/').next().unwrap_or(uri.as_str());
            let lines = diagnostics
                .iter()
                .map(|diagnostic| {
                    let code = if diagnostic.code.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", diagnostic.code)
                    };
                    let source = if diagnostic.source.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", diagnostic.source)
                    };
                    format!(
                        "  {} [Line {}:{}] {}{code}{source}",
                        diagnostic.severity,
                        diagnostic.line + 1,
                        diagnostic.character + 1,
                        diagnostic.message
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("{name}:\n{lines}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let body = if body.chars().count() > MAX_DIAGNOSTICS_CHARS {
        let kept: String = body
            .chars()
            .take(MAX_DIAGNOSTICS_CHARS - 12)
            .collect::<String>();
        format!("{kept}…[truncated]")
    } else {
        body
    };
    format!("<new-diagnostics>The following new diagnostic issues were detected:\n\n{body}</new-diagnostics>")
}

// ---------------------------------------------------------------------------
// One live server
// ---------------------------------------------------------------------------

/// One outbound message and the slot its writer reports back through.
struct WriteRequest {
    bytes: Vec<u8>,
    reply: SyncSender<Result<(), String>>,
}

/// A running language server and everything needed to talk to it.
pub struct Connection {
    child: Mutex<ShellChild>,
    /// Behind a lock because the reader thread holds an `Arc<Connection>`, and
    /// a raw Windows job handle is `Send` but not `Sync`.
    job: Mutex<ShellJob>,
    /// Writes go to a dedicated thread rather than a `Mutex<ChildStdin>`.
    ///
    /// A server that stops draining its stdin makes `write_all` block forever.
    /// Under a mutex that would wedge the caller *and* the reader thread, which
    /// needs the same pipe to answer `workspace/configuration` — the two would
    /// deadlock with no deadline anywhere. A channel lets every sender give up
    /// on its own schedule and leaves exactly one thread blocked on the pipe.
    writes: SyncSender<WriteRequest>,
    next_id: AtomicI64,
    alive: Arc<AtomicBool>,
    pending: Arc<Mutex<HashMap<i64, SyncSender<Result<Value, String>>>>>,
    /// `uri` to the version last sent, which is also the open-document set.
    documents: Mutex<BTreeMap<String, i64>>,
    stderr: Arc<Mutex<String>>,
    /// Why the reader stopped, when it stopped for a reason worth reporting.
    failure: Arc<Mutex<Option<String>>>,
    shutdown_timeout: Duration,
    /// Fires once the stderr drain reaches end of file, so a failure report
    /// can wait for the server's last words rather than race the drain.
    stderr_done: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Connection {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn send(&self, payload: &Value) -> Result<(), String> {
        if !self.is_alive() {
            return Err(self.failure_reason());
        }
        let bytes = encode_message(payload)?;
        let (reply, written) = mpsc::sync_channel(1);
        self.writes
            .try_send(WriteRequest { bytes, reply })
            .map_err(|_| {
                // The writer is either gone or still blocked on a previous
                // frame the server never drained.
                format!(
                    "{} (its input pipe is not being read)",
                    self.failure_reason()
                )
            })?;
        match written.recv_timeout(WRITE_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err("The language server stopped reading its input".to_owned())
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.failure_reason()),
        }
    }

    fn failure_reason(&self) -> String {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| "The language server is no longer running".to_owned())
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, sender);
        let sent = self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        if let Err(error) = sent {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return Err(error);
        }
        let outcome = match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "The language server did not answer '{method}' within {} seconds",
                timeout.as_secs()
            )),
            Err(RecvTimeoutError::Disconnected) => Err(self.failure_reason()),
        };
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        outcome
    }

    /// Sends a request, retrying the one error the source retries.
    pub fn request_with_retry(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut attempt = 0u32;
        loop {
            match self.request(method, params.clone(), REQUEST_TIMEOUT) {
                Err(error)
                    if error.starts_with(CONTENT_MODIFIED_MARKER)
                        && attempt < CONTENT_MODIFIED_RETRIES =>
                {
                    thread::sleep(CONTENT_MODIFIED_BACKOFF * 2u32.pow(attempt));
                    attempt += 1;
                }
                other => return other.map_err(strip_marker),
            }
        }
    }

    /// Makes sure the server has `path` open at `text`, opening it the first
    /// time and sending an incremental full-text change afterwards.
    ///
    /// The document map is held across the notification on purpose. Releasing
    /// it first lets two concurrent calls assign versions 1 and 2 and then
    /// write them in the other order, so the server sees `didChange` for a
    /// document it was never told to open.
    pub fn sync_document(&self, path: &Path, language_id: &str, text: &str) -> Result<(), String> {
        let uri = path_to_uri(path);
        let mut documents = self
            .documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let known = documents.get(&uri).copied();
        let version = known.unwrap_or(0) + 1;
        let sent = match known {
            None => self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }
                }),
            ),
            Some(_) => self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }],
                }),
            ),
        };
        // Only record the version the server was actually told about, so a
        // failed write does not leave this side believing the file is open.
        if sent.is_ok() {
            documents.insert(uri, version);
        }
        sent
    }

    /// Whether this server already holds `path`.
    pub fn has_document(&self, path: &Path) -> bool {
        self.documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&path_to_uri(path))
    }

    /// The last lines the server wrote to stderr, for the settings row. Never
    /// reaches the model.
    pub fn diagnostics_log(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// Prefix the reader puts on a `ContentModified` failure so the retry loop can
/// recognize it without a second error type crossing the channel.
const CONTENT_MODIFIED_MARKER: &str = "\u{1}content-modified\u{1}";

/// Strips the retry marker before a failure reaches the model.
fn strip_marker(error: String) -> String {
    error
        .strip_prefix(CONTENT_MODIFIED_MARKER)
        .map(str::to_owned)
        .unwrap_or(error)
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Identity of one server instance: the root it indexes — machine included —
/// and the name it was configured under. Two workspaces get two
/// `rust-analyzer` processes; two conversations in one workspace share one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ServerKey {
    root: ServerRoot,
    name: String,
}

/// What the registry knows about one server: how it was configured, the process
/// if there is one, the start under way if there is one, and how the last
/// attempts went.
///
/// The accounting follows Claude Code's. `crashes` counts *consecutive*
/// crashes — a process that ended abnormally — and a start that completes the
/// handshake clears it. A start that merely fails (the command is not there,
/// `initialize` timed out or was refused) is not a crash: it leaves the entry
/// in the error state, counts for nothing, and the next use tries again.
struct ServerEntry {
    config: LspServerConfig,
    connection: Option<Arc<Connection>>,
    /// The start in progress. At most one per entry: the second caller joins it
    /// instead of spawning another process.
    starting: Option<Arc<StartInFlight>>,
    crashes: u32,
    /// Why the entry is in the error state — the last crash or failed start.
    /// `None` when the server never ran, was stopped, or exited cleanly.
    last_error: Option<String>,
}

impl ServerEntry {
    fn new(config: &LspServerConfig) -> Self {
        Self {
            config: config.clone(),
            connection: None,
            starting: None,
            crashes: 0,
            last_error: None,
        }
    }
}

#[derive(Default)]
struct Registry {
    servers: BTreeMap<ServerKey, ServerEntry>,
    diagnostics: DiagnosticsLedger,
}

/// Text of a document, read at the moment it is about to be sent rather than
/// when the edit happened, so a start that took seconds opens the file as it is
/// now and not as it was when the start was requested.
pub type TextSource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// A file an edit touched while its server was still starting, to be opened as
/// soon as the server is up.
#[derive(Clone)]
struct QueuedDocument {
    path: PathBuf,
    language_id: String,
    read_text: TextSource,
}

/// One start in progress, shared by everyone who needs the server meanwhile.
struct StartInFlight {
    /// The configuration the process is being started from; a caller whose
    /// entry has since changed must not mistake this start for its own.
    config: LspServerConfig,
    state: Mutex<StartState>,
    done: Condvar,
}

#[derive(Default)]
struct StartState {
    /// Documents to open once the server is up, one per path, newest request
    /// last. Pushed while the registry lock is held and drained under it, so
    /// none is added after the drain.
    queued: Vec<QueuedDocument>,
    outcome: Option<Result<Arc<Connection>, String>>,
}

/// How long a caller that joined a start keeps waiting past the start's own
/// `initialize` deadline before giving up on the starter itself.
const START_JOIN_SLACK: Duration = Duration::from_secs(30);

impl StartInFlight {
    fn new(config: &LspServerConfig) -> Arc<Self> {
        Arc::new(Self {
            config: config.clone(),
            state: Mutex::new(StartState::default()),
            done: Condvar::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, StartState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn queue(&self, document: QueuedDocument) {
        let mut state = self.state();
        // The newest edit's text source wins: the older one would read the same
        // file, but the queue should say what was last asked for.
        state.queued.retain(|queued| queued.path != document.path);
        state.queued.push(document);
    }

    fn take_queue(&self) -> Vec<QueuedDocument> {
        std::mem::take(&mut self.state().queued)
    }

    fn complete(&self, outcome: Result<Arc<Connection>, String>) {
        let mut state = self.state();
        // The first report stands; a starter that unwinds after having
        // reported must not rewrite it.
        if state.outcome.is_none() {
            state.outcome = Some(outcome);
        }
        drop(state);
        self.done.notify_all();
    }

    /// Blocks until the starter reports, which it does after at most the
    /// `initialize` deadline plus the time to reap a failed process.
    fn wait(&self) -> Result<Arc<Connection>, String> {
        let deadline = Instant::now()
            + Duration::from_millis(self.config.startup_timeout_millis)
            + START_JOIN_SLACK;
        let mut state = self.state();
        loop {
            if let Some(outcome) = &state.outcome {
                return outcome.clone();
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "Timed out waiting for the language server '{}' to start",
                    self.config.name
                ));
            }
            state = self
                .done
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

/// Why a start did not produce a server, and whether the *process* is to blame.
#[derive(Debug)]
struct StartFailure {
    message: String,
    /// The process came up and then ended abnormally before the handshake
    /// finished. That is a crash and is counted as one; a command that is not
    /// there, an `initialize` that timed out or was refused is not.
    crashed: bool,
}

impl From<String> for StartFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            crashed: false,
        }
    }
}

/// What a caller asking for a server finds, decided under the registry lock.
enum Begin {
    /// A live connection.
    Live(Arc<Connection>),
    /// A start is under way for this configuration; the caller's document, if
    /// it had one, is queued behind it.
    Joined(Arc<StartInFlight>),
    /// A start is under way, but for a configuration this caller has since
    /// replaced. Wait for it to be over, then ask again.
    Superseded(Arc<StartInFlight>),
    /// This caller owns a new start (and its document, if any, is queued on it).
    Own(Arc<StartInFlight>),
    /// Nothing may be started, and why.
    Refused(String),
}

/// Background edit tasks allowed at once. An edit's start is best effort: a
/// burst past this is dropped rather than turned into a thread per call.
const MAX_BACKGROUND_TASKS: usize = 16;

/// Host-owned language servers. Cloneable handle over one registry, like
/// [`crate::preview_servers::PreviewServerRegistry`].
#[derive(Clone, Default)]
pub struct LspRegistry {
    inner: Arc<Mutex<Registry>>,
    /// Background tasks running now, against [`MAX_BACKGROUND_TASKS`].
    background: Arc<AtomicUsize>,
}

/// Counts one background task for as long as it lives, including when it
/// unwinds.
struct BackgroundSlot(Arc<AtomicUsize>);

impl Drop for BackgroundSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl LspRegistry {
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Notes that `conversation_id` is now owed diagnostics. Called the first
    /// time a conversation reaches a language server, so a conversation that
    /// never uses one is never handed a backlog.
    pub fn register_conversation(&self, conversation_id: &str, root: &ServerRoot) {
        self.lock().diagnostics.register(conversation_id, root);
    }

    /// Drains the diagnostics `conversation_id` has not been shown.
    pub fn take_new_diagnostics(&self, conversation_id: &str) -> Vec<(String, Vec<Diagnostic>)> {
        self.lock().diagnostics.take_new(conversation_id)
    }

    /// Whether any server is currently up.
    ///
    /// The edit hook asks this before resolving configuration off disk: with no
    /// server running there is nobody to notify, and a `write` should not pay
    /// two file reads to discover that.
    pub fn has_running_servers(&self) -> bool {
        self.lock().servers.values().any(|entry| {
            entry
                .connection
                .as_ref()
                .is_some_and(|connection| connection.is_alive())
        })
    }

    /// Picks the server that claims `path`, out of the ones configured for this
    /// run. The first configuration claiming an extension wins, as in the
    /// source; `configs` therefore arrives in precedence order.
    pub fn config_for_path<'a>(
        configs: &'a [LspServerConfig],
        path: &Path,
    ) -> Option<(&'a LspServerConfig, String)> {
        let extension = path
            .extension()
            .map(|value| crate::lsp_config::normalize_extension(&value.to_string_lossy()))?;
        configs.iter().find_map(|config| {
            config
                .extension_to_language
                .get(&extension)
                .map(|language| (config, language.clone()))
        })
    }

    /// Returns a live server for `path`, starting it if this is the first call
    /// that needs it, together with the language id its configuration gives the
    /// file and the root that server indexes. `workspace` is the root a server
    /// with no `workspaceFolder` of its own is started in, and `host` is the
    /// machine both of them are on.
    ///
    /// The root is handed back rather than left implicit because the diagnostics
    /// ledger fans out by it: a caller that registers under `workspace` while
    /// the server publishes under its configured `workspaceFolder` is registered
    /// for a root nothing ever publishes to.
    pub fn connection_for(
        &self,
        host: &ServerHost,
        configs: &[LspServerConfig],
        workspace: &Path,
        path: &Path,
    ) -> Result<(Arc<Connection>, String, ServerRoot), String> {
        let Some((config, language_id)) = Self::config_for_path(configs, path) else {
            return Err(no_server_for(path));
        };
        let root = ServerRoot::new(host, server_root(config, workspace));
        let connection = self.ensure_started(host, config, &root)?;
        Ok((connection, language_id, root))
    }

    /// Returns a live connection for `config` rooted at `root`, starting or
    /// restarting as needed.
    ///
    /// A start already under way for the same configuration is joined, not
    /// repeated: two callers never each spawn a process and race to keep the
    /// better one, which is what spawning outside the lock used to allow.
    fn ensure_started(
        &self,
        host: &ServerHost,
        config: &LspServerConfig,
        root: &ServerRoot,
    ) -> Result<Arc<Connection>, String> {
        loop {
            match self.begin(config, root, None) {
                Begin::Live(connection) => return Ok(connection),
                Begin::Refused(reason) => return Err(reason),
                Begin::Joined(start) => return start.wait(),
                Begin::Superseded(start) => {
                    let _ = start.wait();
                }
                Begin::Own(start) => return self.run_start(host, config, root, start, false),
            }
        }
    }

    /// Decides what a caller that needs the server for `config` at `root`
    /// does, under the registry lock; see [`decide`].
    ///
    /// Connections that decision retires — a process found dead, or one started
    /// from a configuration since replaced — are stopped here, after the lock
    /// is released and before the caller starts anything, so a replacement
    /// never runs beside the server it replaces.
    fn begin(
        &self,
        config: &LspServerConfig,
        root: &ServerRoot,
        document: Option<QueuedDocument>,
    ) -> Begin {
        let key = ServerKey {
            root: root.clone(),
            name: config.name.clone(),
        };
        let mut retired = Vec::new();
        let mut registry = self.lock();
        let outcome = decide(&mut registry, &key, config, document, &mut retired);
        drop(registry);
        for connection in retired {
            stop_connection(&connection);
        }
        outcome
    }

    /// Runs the start `start` stands for, as the one caller entitled to, and
    /// reports it to everyone who joined.
    ///
    /// Spawning and the `initialize` handshake happen outside the registry
    /// lock: a cold server takes seconds, and holding the lock would stall every
    /// other conversation's language server too. What keeps that from being a
    /// race is `start`: it is already recorded on the entry, so nobody else
    /// spawns meanwhile.
    ///
    /// `drain_inline` is whether documents queued behind the start are opened on
    /// this thread — a background edit, which has nothing else to do — or on a
    /// new one, because a navigation call that happened to be the starter must
    /// not wait for other callers' files.
    fn run_start(
        &self,
        host: &ServerHost,
        config: &LspServerConfig,
        root: &ServerRoot,
        start: Arc<StartInFlight>,
        drain_inline: bool,
    ) -> Result<Arc<Connection>, String> {
        let key = ServerKey {
            root: root.clone(),
            name: config.name.clone(),
        };
        let mut guard = StartGuard {
            registry: self,
            key: key.clone(),
            start: Arc::clone(&start),
            reported: false,
        };
        let started = start_server(host, config, root, Arc::downgrade(&self.inner));
        let result = self.finish_start(&key, &start, started, drain_inline);
        guard.reported = true;
        result
    }

    /// Records how a start went and wakes whoever joined it.
    fn finish_start(
        &self,
        key: &ServerKey,
        start: &Arc<StartInFlight>,
        started: Result<Arc<Connection>, StartFailure>,
        drain_inline: bool,
    ) -> Result<Arc<Connection>, String> {
        // A process nobody will use, to be stopped once the lock is released.
        let mut orphan: Option<Arc<Connection>> = None;
        let (result, queued) = {
            let mut registry = self.lock();
            let entry = registry.servers.get_mut(key).filter(|entry| {
                entry
                    .starting
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, start))
            });
            match entry {
                Some(entry) => {
                    entry.starting = None;
                    // Taken under the lock that every push is made under, so
                    // nothing joins after this.
                    let queued = start.take_queue();
                    // The entry may have been moved to another configuration
                    // while this one was starting; the outcome then says
                    // nothing about it.
                    let current = entry.config.launches_like(&start.config);
                    let result = match started {
                        Ok(connection) if current => {
                            entry.connection = Some(Arc::clone(&connection));
                            // A start that completed the handshake ends a run
                            // of crashes: the count is of *consecutive* ones.
                            entry.crashes = 0;
                            entry.last_error = None;
                            Ok(connection)
                        }
                        Ok(connection) => {
                            orphan = Some(connection);
                            Err(format!(
                                "The language server '{}' was reconfigured while it was starting",
                                start.config.name
                            ))
                        }
                        Err(failure) => {
                            // Not latched: a start that failed is retried the
                            // next time the server is needed. Only a process
                            // that ended abnormally counts toward giving up.
                            if current {
                                if failure.crashed {
                                    entry.crashes += 1;
                                }
                                entry.last_error = Some(failure.message.clone());
                            }
                            Err(failure.message)
                        }
                    };
                    (result, queued)
                }
                None => {
                    // `stop_all` ran while this was starting, or the entry has
                    // been replaced by one with a start of its own. Either way
                    // there is no place to put a process that came up.
                    start.take_queue();
                    let result = match started {
                        Ok(connection) => {
                            orphan = Some(connection);
                            Err(format!(
                                "The language server '{}' was stopped while it was starting",
                                start.config.name
                            ))
                        }
                        Err(failure) => Err(failure.message),
                    };
                    (result, Vec::new())
                }
            }
        };
        if let Some(orphan) = orphan {
            stop_connection(&orphan);
        }
        start.complete(result.clone());
        if let (Ok(connection), false) = (&result, queued.is_empty()) {
            if drain_inline {
                open_queued(connection, &queued);
            } else {
                let connection = Arc::clone(connection);
                thread::spawn(move || open_queued(&connection, &queued));
            }
        }
        result
    }

    /// Runs `task` on its own thread, for work that must not delay the call
    /// that caused it. Dropped, with a line on stderr, when too many such tasks
    /// are already running: what an edit asks of a language server is best
    /// effort, and the next edit asks again.
    pub fn in_background(&self, task: impl FnOnce(&LspRegistry) + Send + 'static) {
        if self.background.fetch_add(1, Ordering::SeqCst) >= MAX_BACKGROUND_TASKS {
            self.background.fetch_sub(1, Ordering::SeqCst);
            eprintln!("Skipped a language server update after an edit: too many are already running");
            return;
        }
        let slot = BackgroundSlot(Arc::clone(&self.background));
        let registry = self.clone();
        let spawned = thread::Builder::new()
            .name("lsp-after-edit".into())
            .spawn(move || {
                let _slot = slot;
                task(&registry);
            });
        if let Err(error) = spawned {
            // The closure, and the slot inside it, went with the failed spawn.
            eprintln!("Could not start a language server update after an edit: {error}");
        }
    }

    /// Blocks until every background task has finished, for tests that assert
    /// on what an edit did (or did not) set in motion.
    #[cfg(test)]
    pub(crate) fn wait_for_background_tasks(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.background.load(Ordering::SeqCst) > 0 {
            assert!(Instant::now() < deadline, "background tasks did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Claude Code's `changeFile`, for a file the model just wrote, minus the
    /// part [`Self::notify_file_changed`] already did: make sure the server
    /// that claims the file is up — starting it if it is not — and have it hold
    /// the file as it is on disk now.
    ///
    /// Meant for a background thread ([`Self::in_background`]): the start can
    /// take as long as a cold `rust-analyzer` takes. The caller has already
    /// decided that starting a server on an edit is allowed here; this does not
    /// ask again. Nothing is reported: a failure is a line on stderr, and the
    /// next edit or the next navigation call tries again.
    ///
    /// The conversation is registered for diagnostics *before* anything is
    /// started, because the first `publishDiagnostics` of a freshly opened
    /// document can arrive before this function returns, and a publish is
    /// fanned out only to conversations that are registered when it lands.
    #[allow(clippy::too_many_arguments)]
    pub fn open_after_edit(
        &self,
        host: &ServerHost,
        config: &LspServerConfig,
        language_id: &str,
        root: &ServerRoot,
        path: &Path,
        conversation_id: &str,
        read_text: TextSource,
    ) {
        // `diagnostics: false` suppresses only the reporting half: the document
        // is opened all the same, or the server answers navigation from nothing.
        if config.diagnostics {
            self.register_conversation(conversation_id, root);
        }
        let document = QueuedDocument {
            path: path.to_path_buf(),
            language_id: language_id.to_owned(),
            read_text,
        };
        // A reconfiguration under way is waited out, a few times at most.
        for _ in 0..3 {
            match self.begin(config, root, Some(document.clone())) {
                Begin::Live(connection) => {
                    // A document the server already held was re-synced by the
                    // edit hook before this ran; this is for one it did not.
                    if !connection.has_document(path) {
                        open_queued(&connection, std::slice::from_ref(&document));
                    }
                    return;
                }
                // Queued behind a start somebody else is running; its starter
                // opens the file when the server is up.
                Begin::Joined(_) => return,
                // Crashed out or stopped for good. A navigation call reports
                // why; saying it again on every edit would only be noise.
                Begin::Refused(_) => return,
                Begin::Superseded(start) => {
                    let _ = start.wait();
                }
                Begin::Own(start) => {
                    if let Err(error) = self.run_start(host, config, root, start, true) {
                        eprintln!(
                            "The language server '{}' did not start after an edit of {}: {error}",
                            config.name,
                            path.display()
                        );
                    }
                    return;
                }
            }
        }
    }

    /// Makes `conversation_id`'s diagnostics for the file at `path` reportable
    /// again: what it was shown for the file is forgotten, and what was queued
    /// for it is dropped. Called after every edit, whether or not a server
    /// holds the file, as Claude Code does.
    pub fn forget_file_diagnostics(&self, conversation_id: &str, path: &Path) {
        self.lock()
            .diagnostics
            .forget_file(conversation_id, &path_to_uri(path));
    }

    /// Whether the diagnostics ledger has a record of `conversation_id`. An
    /// edit hook asks before it resolves a path to forget a file's records:
    /// for a conversation that never reached a language server there are none.
    pub fn tracks_conversation(&self, conversation_id: &str) -> bool {
        self.lock()
            .diagnostics
            .conversations
            .contains_key(conversation_id)
    }

    /// Tells the server that claims `path` that it changed on disk, so both the
    /// diagnostics injected next round and the next navigation call describe the
    /// file as it is now.
    ///
    /// This is the part of an edit's effect that is cheap enough to do inline:
    /// it only ever re-syncs a server that is already up and already holds the
    /// document. Starting a server, or opening a document it does not hold yet,
    /// is [`Self::open_after_edit`]'s job, on a background thread and only where
    /// the caller has established that it may.
    ///
    /// Silent on every failure: an edit must not fail because a language server
    /// is unhappy.
    pub fn notify_file_changed(
        &self,
        configs: &[LspServerConfig],
        workspace: &Path,
        path: &Path,
        conversation_id: &str,
    ) {
        let Some((config, language_id)) = Self::config_for_path(configs, path) else {
            return;
        };
        let root = ServerRoot::local(server_root(config, workspace));
        // `diagnostics: false` suppresses only the reporting half. The document
        // still has to be re-synced, or the server keeps answering navigation
        // from the text as it was before this edit.
        if config.diagnostics {
            self.register_conversation(conversation_id, &root);
        }
        // Only a server that is already up; a cold start, where one is allowed,
        // is the background path's.
        let connection = {
            let registry = self.lock();
            registry
                .servers
                .get(&ServerKey {
                    root,
                    name: config.name.clone(),
                })
                .and_then(|entry| entry.connection.clone())
        };
        let Some(connection) = connection else {
            return;
        };
        if !connection.is_alive() || !connection.has_document(path) {
            // Nothing to re-sync: the next call opens it at its current text.
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        resync(&connection, path, &language_id, &text);
    }

    /// The remote counterpart of [`Self::notify_file_changed`]: a file on
    /// `machine_key` changed, and whichever live server there holds it is told.
    ///
    /// No configuration is resolved for this. A server that is up already knows
    /// which extensions it claims, and the document map says whether it holds
    /// this one; reading two `lsp.json` files off another machine to learn what
    /// the registry can answer from memory would be a round trip per `write`.
    /// `read_text` is asked for the new contents only once a holder is found,
    /// so a write on a machine with no live server costs nothing extra.
    pub fn notify_remote_file_changed(
        &self,
        machine_key: &str,
        path: &Path,
        conversation_id: &str,
        read_text: &dyn Fn() -> Option<String>,
    ) {
        let holders: Vec<(Arc<Connection>, String, ServerRoot, bool)> = {
            let registry = self.lock();
            registry
                .servers
                .iter()
                .filter(|(key, _)| key.root.machine == machine_key)
                .filter_map(|(key, entry)| {
                    let connection = entry.connection.clone()?;
                    let (_, language_id) =
                        Self::config_for_path(std::slice::from_ref(&entry.config), path)?;
                    Some((connection, language_id, key.root.clone(), entry.config.diagnostics))
                })
                .collect()
        };
        let mut text: Option<String> = None;
        for (connection, language_id, root, diagnostics) in holders {
            if !connection.is_alive() || !connection.has_document(path) {
                continue;
            }
            if diagnostics {
                self.register_conversation(conversation_id, &root);
            }
            if text.is_none() {
                text = read_text();
            }
            let Some(text) = text.as_deref() else {
                return;
            };
            resync(&connection, path, &language_id, text);
        }
    }

    /// Stops every server. Called on app exit and on document reset.
    pub fn stop_all(&self) {
        let entries: Vec<Arc<Connection>> = {
            let mut registry = self.lock();
            let connections = registry
                .servers
                .values_mut()
                .filter_map(|entry| entry.connection.take())
                .collect();
            registry.servers.clear();
            registry.diagnostics = DiagnosticsLedger::default();
            connections
        };
        for connection in entries {
            stop_connection(&connection);
        }
    }
}

/// What a caller that needs a server finds in its entry, and what that makes
/// it do. Runs under the registry lock and therefore does nothing slow: a
/// process to retire is handed back for the caller to stop afterwards.
///
/// `document` is the file an edit wants opened. It is queued on whichever start
/// the caller ends up joining or owning, and left alone otherwise.
fn decide(
    registry: &mut Registry,
    key: &ServerKey,
    config: &LspServerConfig,
    document: Option<QueuedDocument>,
    retired: &mut Vec<Arc<Connection>>,
) -> Begin {
    let entry = registry
        .servers
        .entry(key.clone())
        .or_insert_with(|| ServerEntry::new(config));

    // `lsp.json` is read on every call, so an entry that changed since this
    // server started is the configuration the user wants now: the running
    // server (started from the old one) is stopped and a fresh one started from
    // the new one, and a failure recorded against the old one no longer says
    // anything about the new.
    if !entry.config.launches_like(config) {
        retired.extend(entry.connection.take());
        entry.config = config.clone();
        entry.crashes = 0;
        entry.last_error = None;
    }

    // A live connection is consulted before anything recorded about failures:
    // one call's start can fail while another's succeeded, and a recorded
    // reason must never hide a server that is up and answering.
    if let Some(connection) = entry
        .connection
        .as_ref()
        .filter(|connection| connection.is_alive())
    {
        return Begin::Live(Arc::clone(connection));
    }

    // The process went away since the last call. How it went decides what it
    // costs: ending abnormally is a crash and counts toward the budget; a clean
    // exit is the server stopping of its own accord and costs nothing.
    if let Some(dead) = entry.connection.take() {
        if exited_abnormally(&dead) {
            entry.crashes += 1;
            entry.last_error = Some(dead.failure_reason());
        } else {
            entry.last_error = None;
        }
        retired.push(dead);
    }

    if let Some(start) = &entry.starting {
        if start.config.launches_like(config) {
            if let Some(document) = document {
                start.queue(document);
            }
            return Begin::Joined(Arc::clone(start));
        }
        return Begin::Superseded(Arc::clone(start));
    }

    // Whether another start is worth trying. Both refusals are Claude Code's,
    // in its order, and both are recomputed from the counters on every call
    // rather than recorded: the entry changing (or the app restarting) is what
    // lifts them, and the counters are what a changed entry resets.
    if entry.last_error.is_some() && entry.crashes > 0 && !entry.config.restart_on_crash {
        return Begin::Refused(format!(
            "The language server '{}' stopped and restartOnCrash is false",
            config.name
        ));
    }
    if entry.crashes > entry.config.max_restarts {
        return Begin::Refused(format!(
            "The language server '{}' exceeded max crash recovery attempts ({})",
            config.name, entry.config.max_restarts
        ));
    }

    let start = StartInFlight::new(config);
    if let Some(document) = document {
        start.queue(document);
    }
    entry.starting = Some(Arc::clone(&start));
    Begin::Own(start)
}

/// Whether a connection found dead ended the way a crash does.
///
/// Status 0 is the server choosing to stop and is not one. Everything else is:
/// a non-zero code, a signal, and also a process that is still there with its
/// output closed or whose status cannot be had, because a server that has
/// stopped speaking is not behaving as asked.
fn exited_abnormally(connection: &Connection) -> bool {
    let Ok(mut child) = connection.child.try_lock() else {
        return true;
    };
    // The reader marked the connection dead on end of file, which can be a
    // moment before the operating system reports the exit; a process that is
    // still there after that moment is not behaving.
    let status = match child.try_wait() {
        Ok(Some(status)) => Some(status),
        Ok(None) => child.wait_timeout(EXIT_SETTLE).ok().flatten(),
        Err(_) => None,
    };
    !status.is_some_and(|status| status.success())
}

/// Releases a start's slot if its starter unwinds before reporting, so that
/// nobody waits on it forever and the next caller can start again.
struct StartGuard<'a> {
    registry: &'a LspRegistry,
    key: ServerKey,
    start: Arc<StartInFlight>,
    reported: bool,
}

impl Drop for StartGuard<'_> {
    fn drop(&mut self) {
        if self.reported {
            return;
        }
        let interrupted = "The language server's start was interrupted".to_owned();
        if let Some(entry) = self.registry.lock().servers.get_mut(&self.key) {
            if entry
                .starting
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.start))
            {
                entry.starting = None;
                entry.last_error = Some(interrupted.clone());
            }
        }
        self.start.complete(Err(interrupted));
    }
}

/// Opens each queued document on `connection` at the text its source returns
/// now, then tells the server it was saved. A document whose text cannot be
/// read any more (deleted since, too large) is skipped.
fn open_queued(connection: &Connection, documents: &[QueuedDocument]) {
    for document in documents {
        let Some(text) = (document.read_text)() else {
            continue;
        };
        resync(connection, &document.path, &document.language_id, &text);
    }
}

/// The directory one server indexes: its own `workspaceFolder` when the entry
/// names one, the conversation's workspace otherwise.
///
/// Every caller has to agree on this, not just the one that starts the process:
/// the registry key, the `rootUri` in `initialize`, and the root the diagnostics
/// ledger fans out by are all this same value.
pub(crate) fn server_root(config: &LspServerConfig, workspace: &Path) -> PathBuf {
    if config.workspace_folder.trim().is_empty() {
        workspace.to_path_buf()
    } else {
        PathBuf::from(config.workspace_folder.trim())
    }
}

/// Re-sends a document the server already holds, then tells it the file was
/// saved. Failures are the server's problem, not the edit's.
fn resync(connection: &Connection, path: &Path, language_id: &str, text: &str) {
    let _ = connection.sync_document(path, language_id, text);
    let _ = connection.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": path_to_uri(path) } }),
    );
}

/// The sentence a call gets when nothing is configured for the file it named.
fn no_server_for(path: &Path) -> String {
    let extension = path
        .extension()
        .map(|value| crate::lsp_config::normalize_extension(&value.to_string_lossy()))
        .unwrap_or_else(|| "(none)".to_owned());
    format!(
        "No LSP server available for file type: {extension}. Configure one in .mewrk/lsp.json under \"lspServers\", or install a language server Mewrk already knows (rust-analyzer, typescript-language-server, pyright, gopls, clangd, lua-language-server, bash-language-server)."
    )
}

/// `file://` URI for an absolute path, in the spelling the source produces:
/// forward slashes, a leading slash before a Windows drive letter.
///
/// Windows' extended-length spellings are normalized away first, and that part
/// is a deliberate deviation. The source never meets them: Node's
/// `path.resolve` does not produce `\\?\`. Mewrk's path guard hands this
/// function `fs::canonicalize`'s answer, which on Windows always is
/// `\\?\C:\…` — and for a network location `\\?\UNC\server\share\…`. Replacing
/// the backslashes alone turns the first into `//?/C:/…`, which already starts
/// with a slash, so the leading-slash branch below does not fire and the result
/// is `file:////?/C:/…`: four slashes, an empty authority, a URI no server can
/// turn back into a path. A real `rust-analyzer` answers every request naming
/// one with `-32603 url is not a file`, which is every `lsp` call on Windows.
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    // `\\?\UNC\server\share` and `\\server\share` name the same location, and
    // both reach a URI the same way: the host belongs in the authority.
    let text = match text.strip_prefix("//?/") {
        Some(rest) => match strip_ascii_case_insensitive_prefix(rest, "UNC/") {
            Some(share) => format!("//{share}"),
            None => rest.to_owned(),
        },
        None => text,
    };
    // `//server/share/…` is already authority-and-path, `/home/…` needs the
    // empty authority spelled out, and `C:/…` needs it plus the root slash.
    let prefix = if text.starts_with("//") {
        "file:"
    } else if text.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    let mut encoded = String::with_capacity(text.len() + prefix.len());
    encoded.push_str(prefix);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' | b':' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

/// A `file://` URI back to a path. Mirrors the source's decoder, including the
/// leading-slash strip in front of a Windows drive letter.
pub fn uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return percent_decode(uri);
    };
    // An authority means a network location. Dropping its two slashes with the
    // scheme would turn `file://server/share/x` into the relative path
    // `server/share/x`, which resolves against whatever the process's current
    // directory happens to be.
    if !rest.starts_with('/') {
        return format!("//{}", percent_decode(rest));
    }
    let decoded = percent_decode(rest);
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        decoded[1..].to_owned()
    } else {
        decoded
    }
}

/// `\\?\UNC\` is spelled in either case by different Windows APIs.
fn strip_ascii_case_insensitive_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Spawns one server and completes the `initialize`/`initialized` handshake.
fn start_server(
    host: &ServerHost,
    config: &LspServerConfig,
    root: &ServerRoot,
    sink: std::sync::Weak<Mutex<Registry>>,
) -> Result<Arc<Connection>, StartFailure> {
    // A command that is not there fails to spawn at all: not a crash, since no
    // process ever ran.
    let child = match host {
        ServerHost::Local => ShellChild::Local(spawn_local(config, &root.path)?),
        ServerHost::Remote { runner, .. } => spawn_remote(runner, config, &root.path)?,
    };
    attach_server(child, host, config, root, sink)
}

/// Takes a freshly spawned server (or the wrapper relaying one), wires its
/// pipes, and completes the handshake. Separate from the spawn so the remote
/// leg's script can be driven through any shell in a test.
fn attach_server(
    mut child: ShellChild,
    host: &ServerHost,
    config: &LspServerConfig,
    root: &ServerRoot,
    sink: std::sync::Weak<Mutex<Registry>>,
) -> Result<Arc<Connection>, StartFailure> {
    // Before it can spawn grandchildren of its own. A server the agent runs
    // is already its own process group on the machine; nothing here owns it.
    let job = ShellJob::create();
    if let ShellChild::Local(local) = &child {
        job.assign(local);
    }

    let stdin = child
        .take_stdin()
        .ok_or_else(|| "Could not acquire the language server's stdin".to_owned())?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| "Could not acquire the language server's stdout".to_owned())?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| "Could not acquire the language server's stderr".to_owned())?;

    let (writes, write_requests) = mpsc::sync_channel::<WriteRequest>(1);
    thread::spawn(move || {
        let mut stdin = stdin;
        while let Ok(request) = write_requests.recv() {
            let result = stdin
                .write_all(&request.bytes)
                .and_then(|()| stdin.flush())
                .map_err(|error| format!("Failed to write to the language server: {error}"));
            let failed = result.is_err();
            let _ = request.reply.send(result);
            if failed {
                return;
            }
        }
    });

    let connection = Arc::new(Connection {
        child: Mutex::new(child),
        job: Mutex::new(job),
        writes,
        next_id: AtomicI64::new(1),
        alive: Arc::new(AtomicBool::new(true)),
        pending: Arc::new(Mutex::new(HashMap::new())),
        documents: Mutex::new(BTreeMap::new()),
        stderr: Arc::new(Mutex::new(String::new())),
        failure: Arc::new(Mutex::new(None)),
        shutdown_timeout: Duration::from_millis(config.shutdown_timeout_millis),
        stderr_done: Mutex::new(None),
    });

    let stderr_done = spawn_stderr_drain(
        stderr,
        Arc::clone(&connection.stderr),
        matches!(host, ServerHost::Remote { runner: ShellRunner::Wsl { .. }, .. }),
    );
    *connection
        .stderr_done
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stderr_done);
    spawn_reader(
        stdout,
        Arc::clone(&connection.alive),
        Arc::clone(&connection.pending),
        Arc::clone(&connection.failure),
        Arc::downgrade(&connection),
        config.clone(),
        root.clone(),
        sink,
    );

    match handshake(&connection, host, config, &root.path) {
        Ok(()) => Ok(connection),
        Err(error) => {
            // Asked before the process is stopped, because stopping it makes
            // every process look like one that ended: what matters is whether
            // it ended by itself first. A server that never answered
            // `initialize`, or answered it with an error, is still running here.
            let ended_by_itself = exit_before_stop(&connection);
            stop_connection(&connection);
            // Whatever the server (or the wrapper that could not reach it)
            // said on stderr is the reason more often than the timeout is: a
            // command not on the remote PATH, an SSH key the machine refused.
            // The process is gone, so its stderr is at end of file; give the
            // drain a moment to have read it.
            let done = connection
                .stderr_done
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(done) = done {
                let _ = done.recv_timeout(STDERR_SETTLE);
            }
            let tail = last_stderr_line(&connection.diagnostics_log());
            Err(StartFailure {
                message: if tail.is_empty() {
                    error
                } else {
                    format!("{error} ({tail})")
                },
                // Not "the process died" but "the process is what failed": a
                // wrapper that could not reach its machine, or a launch script
                // that refused, died too, and neither says anything about the
                // server.
                crashed: ended_by_itself.is_some_and(|exit| {
                    counts_as_crash(exit, !host.is_local())
                }),
            })
        }
    }
}

/// How a process that ended on its own ended, as far as a crash count cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitKind {
    /// Status 0.
    Clean,
    /// A non-zero code.
    Code(i32),
    /// Killed by a signal, or a status that could not be had.
    Abnormal,
}

/// Exit codes of a remote launch that mean "never reached the server": the
/// launch script's own refusals (`cd` failed, command not found — see
/// [`remote_launch_script`]) and `ssh`'s 255 for a connection it could not
/// make. Counting those as the server crashing would use up the restart budget
/// on a machine that was merely unreachable.
const REMOTE_LAUNCH_REFUSALS: [i32; 3] = [64, 127, 255];

/// Whether a process that ended on its own during start-up is to be counted as
/// having crashed. `wrapped` is whether what ran is a wrapper relaying the
/// server from another machine.
fn counts_as_crash(exit: ExitKind, wrapped: bool) -> bool {
    match exit {
        ExitKind::Clean => false,
        ExitKind::Code(code) => !(wrapped && REMOTE_LAUNCH_REFUSALS.contains(&code)),
        ExitKind::Abnormal => true,
    }
}

/// `None` while the process is still running — a timeout, or a server that
/// refused `initialize` — and otherwise how it ended on its own.
///
/// A reader that has seen end of file marks the connection dead before the
/// operating system has necessarily finished reporting the exit, so a dead
/// connection is given a moment to settle; a status that never arrives is
/// abnormal.
fn exit_before_stop(connection: &Connection) -> Option<ExitKind> {
    let mut child = connection
        .child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let status: Option<ExitStatus> = match child.try_wait() {
        Ok(Some(status)) => Some(status),
        _ if connection.is_alive() => return None,
        _ => child.wait_timeout(EXIT_SETTLE).ok().flatten(),
    };
    Some(match status {
        Some(status) if status.success() => ExitKind::Clean,
        Some(status) => status.code().map_or(ExitKind::Abnormal, ExitKind::Code),
        None => ExitKind::Abnormal,
    })
}

/// How long a connection whose output closed is given to report how its
/// process ended.
const EXIT_SETTLE: Duration = Duration::from_millis(200);

/// How long a failed start waits for the stderr drain to finish after the
/// process is gone.
const STDERR_SETTLE: Duration = Duration::from_millis(500);

/// The last non-empty line a server wrote to stderr, cut to a length that fits
/// in an error sentence.
fn last_stderr_line(log: &str) -> String {
    const MAX_TAIL_CHARS: usize = 240;
    let line = log
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() > MAX_TAIL_CHARS {
        let kept: String = line.chars().take(MAX_TAIL_CHARS).collect();
        format!("{kept}…")
    } else {
        line.to_owned()
    }
}

/// Starts the server as a child of this process, in `root` when it exists.
fn spawn_local(config: &LspServerConfig, root: &Path) -> Result<Child, String> {
    let resolved = crate::environment_tools::resolve_on_path(&config.command)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| config.command.clone());
    let mut process = Command::new(&resolved);
    process
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if root.is_dir() {
        process.current_dir(root);
    }
    for (name, value) in &config.env {
        process.env(name, value);
    }
    // A language server needs the developer's toolchain environment, so this
    // inherits rather than using the MCP allowlist — but the harness secrets
    // every child spawn strips are still stripped.
    for name in crate::child_environment::private_child_environment_names() {
        process.env_remove(&name);
    }
    process.env_remove(crate::child_environment::DEV_APPLICATION_PATH_ENVIRONMENT_NAME);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session, so the negative-pid group kill reaches the
        // proc-macro and build-script children a server starts.
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    process.spawn().map_err(|error| {
        format!(
            "Could not start the language server '{}' ({}): {error}",
            config.name, config.command
        )
    })
}

/// Starts the server on the machine `runner` reaches, in `root` there.
///
/// What this process holds is the `wsl.exe` or `ssh` wrapper; the server is its
/// grandchild on the other side, and the wrapper relays both pipes verbatim, so
/// the base protocol is spoken through it unchanged. The wrapper's own
/// environment table is applied by the transport ([`ShellRunner`]), the entry's
/// `env` by the script — and `exec` makes the server the process the wrapper
/// is talking to, so `shutdown`/`exit` reach it and its end of the pipe closes
/// when it goes.
fn spawn_remote(
    runner: &ShellRunner,
    config: &LspServerConfig,
    root: &Path,
) -> Result<ShellChild, String> {
    let script = match runner.script_dialect() {
        crate::shell_backend::ScriptDialect::Posix => remote_launch_script(config, root)?,
        crate::shell_backend::ScriptDialect::PowerShell => remote_launch_powershell(config, root)?,
    };
    let failed = |error: String| {
        format!(
            "Could not start the language server '{}' ({}) on the remote machine: {error}",
            config.name, config.command
        )
    };
    // An SSH machine the agent serves runs the server itself: the server then
    // belongs to the machine's agent, not to an SSH session, and a dropped
    // link pauses its conversation instead of ending it.
    let argv = runner
        .agent_shell()
        .cloned()
        .unwrap_or_default()
        .script_argv(&script);
    if let Some(spawned) = crate::remote_link::spawn(
        runner,
        argv,
        None,
        remote_agent::protocol::StdinMode::Pipe,
        "lsp",
    ) {
        return spawned
            .map(|child| ShellChild::Remote {
                child,
                killed: false,
            })
            .map_err(failed);
    }
    crate::run_environment::spawn_remote_script(runner, &script, true)
        .map(ShellChild::Local)
        .map_err(failed)
}

/// The script that starts one configured server on a remote machine.
///
/// Every fragment is single-quoted: the command, its arguments and the entry's
/// environment all come from an `lsp.json` a repository may have shipped, and
/// none of them may rewrite the script. A command that is not on the remote
/// PATH is reported by name before anything is executed, because the wrapper
/// exiting with `127` would otherwise read as "the language server exited".
pub fn remote_launch_script(config: &LspServerConfig, root: &Path) -> Result<String, String> {
    use crate::run_environment::{quote_remote_path, sh_single_quote};

    fn check(text: &str, label: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err(format!("The language server's {label} is empty"));
        }
        if text.chars().any(char::is_control) {
            return Err(format!(
                "The language server's {label} contains control characters"
            ));
        }
        Ok(())
    }

    let root = root.to_string_lossy();
    check(&root, "workspace root")?;
    check(&config.command, "command")?;
    let mut script = String::with_capacity(512);
    script.push_str(&format!(
        "cd -- {} || exit 64\n",
        quote_remote_path(root.trim())
    ));
    script.push_str(&format!(
        "command -v {cmd} >/dev/null 2>&1 || {{ printf '%s\\n' {message} >&2; exit 127; }}\n",
        cmd = sh_single_quote(&config.command),
        message = sh_single_quote(&format!(
            "{}: command not found on the remote machine's PATH",
            config.command
        )),
    ));
    script.push_str("exec ");
    if !config.env.is_empty() {
        script.push_str("env ");
        for (name, value) in &config.env {
            crate::run_environment::validate_env_var_name(name)?;
            // Empty is a legal value (`RUST_LOG=`); only control characters
            // cannot be quoted into safety.
            if value.chars().any(char::is_control) {
                return Err(
                    "The language server's environment values contain control characters".into(),
                );
            }
            script.push_str(&sh_single_quote(&format!("{name}={value}")));
            script.push(' ');
        }
    }
    script.push_str(&sh_single_quote(&config.command));
    for argument in &config.args {
        if argument.chars().any(char::is_control) {
            return Err("The language server's arguments contain control characters".into());
        }
        script.push(' ');
        script.push_str(&sh_single_quote(argument));
    }
    script.push('\n');
    Ok(script)
}

/// [`remote_launch_script`] for a Windows machine whose agent shell is
/// PowerShell, with the same checks on everything a repository's `lsp.json`
/// supplies and the same exit codes (64, 127).
pub fn remote_launch_powershell(config: &LspServerConfig, root: &Path) -> Result<String, String> {
    let root = root.to_string_lossy();
    for (text, label) in [(root.as_ref(), "workspace root"), (config.command.as_str(), "command")] {
        if text.trim().is_empty() {
            return Err(format!("The language server's {label} is empty"));
        }
        if text.chars().any(char::is_control) {
            return Err(format!("The language server's {label} contains control characters"));
        }
    }
    let mut env = Vec::with_capacity(config.env.len());
    for (name, value) in &config.env {
        crate::run_environment::validate_env_var_name(name)?;
        if value.chars().any(char::is_control) {
            return Err("The language server's environment values contain control characters".into());
        }
        env.push((name.clone(), value.clone()));
    }
    if config.args.iter().any(|argument| argument.chars().any(char::is_control)) {
        return Err("The language server's arguments contain control characters".into());
    }
    Ok(crate::remote_powershell::lsp_launch(
        root.trim(),
        &config.command,
        &config.args,
        &env,
    ))
}

fn handshake(
    connection: &Connection,
    host: &ServerHost,
    config: &LspServerConfig,
    root: &Path,
) -> Result<(), String> {
    let root_uri = path_to_uri(root);
    let name = root
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    // `processId` is the parent a server may watch and exit with. This
    // process is that parent only when the server runs here; on another
    // machine the number would name whatever happens to hold that pid there.
    let process_id = if host.is_local() {
        json!(std::process::id())
    } else {
        Value::Null
    };
    let params = json!({
        "processId": process_id,
        "clientInfo": { "name": "Mewrk", "version": env!("CARGO_PKG_VERSION") },
        "initializationOptions": config
            .initialization_options
            .clone()
            .unwrap_or(Value::Object(Map::new())),
        "workspaceFolders": [{ "uri": root_uri, "name": name }],
        "rootPath": root.to_string_lossy(),
        "rootUri": root_uri,
        "capabilities": client_capabilities(config),
    });
    connection
        .request(
            "initialize",
            params,
            Duration::from_millis(config.startup_timeout_millis),
        )
        .map_err(|error| {
            format!(
                "The language server '{}' failed to initialize: {}",
                config.name,
                strip_marker(error)
            )
        })?;
    connection.notify("initialized", json!({}))?;
    if let Some(settings) = &config.settings {
        let _ = connection.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": settings }),
        );
    }
    Ok(())
}

/// The client capabilities literal, ported from the source.
///
/// Every `dynamicRegistration: false` and `workspaceFolders: false` here is
/// load-bearing: they are what keep a server from sending the client requests
/// this client does not implement.
fn client_capabilities(config: &LspServerConfig) -> Value {
    json!({
        "workspace": {
            "configuration": config.settings.is_some(),
            "workspaceFolders": false,
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": true,
            },
            "publishDiagnostics": {
                "relatedInformation": true,
                "tagSupport": { "valueSet": [1, 2] },
                "versionSupport": false,
                "codeDescriptionSupport": true,
                "dataSupport": false,
            },
            "hover": {
                "dynamicRegistration": false,
                "contentFormat": ["markdown", "plaintext"],
            },
            "definition": { "dynamicRegistration": false, "linkSupport": true },
            "references": { "dynamicRegistration": false },
            "implementation": { "dynamicRegistration": false, "linkSupport": true },
            "documentSymbol": {
                "dynamicRegistration": false,
                "hierarchicalDocumentSymbolSupport": true,
            },
            "callHierarchy": { "dynamicRegistration": false },
        },
        "workspace_symbol": { "dynamicRegistration": false },
        "general": { "positionEncodings": ["utf-16"] },
    })
}

/// Drains stderr into the settings-row log. `wsl` marks a `wsl.exe` wrapper,
/// whose own complaints (a distribution that is not there) arrive as UTF-16LE
/// even though everything the server itself writes is UTF-8. The returned
/// receiver fires when the pipe reaches end of file.
fn spawn_stderr_drain(
    stderr: Box<dyn Read + Send>,
    sink: Arc<Mutex<String>>,
    wsl: bool,
) -> mpsc::Receiver<()> {
    let (done, finished) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        // A `wsl.exe` complaint is UTF-16LE, and a `\n` byte falls in the
        // middle of its code units: decoding line by line would leave every
        // line after the first misaligned. Its stream is kept raw and decoded
        // whole each time instead, cut at an even offset so the alignment
        // survives the trim.
        let mut raw: Vec<u8> = Vec::new();
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => {
                    let _ = done.send(());
                    return;
                }
                Ok(_) => {}
            }
            let mut log = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if wsl {
                raw.extend_from_slice(&line);
                if raw.len() > MAX_STDERR_BYTES {
                    let cut = (raw.len() - MAX_STDERR_BYTES) & !1;
                    raw.drain(..cut);
                }
                *log = crate::run_environment::decode_wsl_output(&raw);
                continue;
            }
            log.push_str(&String::from_utf8_lossy(&line));
            if log.len() > MAX_STDERR_BYTES {
                let cut = log.len() - MAX_STDERR_BYTES;
                let boundary = (cut..log.len())
                    .find(|index| log.is_char_boundary(*index))
                    .unwrap_or(log.len());
                *log = log[boundary..].to_owned();
            }
        }
    });
    finished
}

#[allow(clippy::too_many_arguments)]
fn spawn_reader(
    stdout: Box<dyn Read + Send>,
    alive: Arc<AtomicBool>,
    pending: Arc<Mutex<HashMap<i64, SyncSender<Result<Value, String>>>>>,
    failure: Arc<Mutex<Option<String>>>,
    connection: std::sync::Weak<Connection>,
    config: LspServerConfig,
    root: ServerRoot,
    sink: std::sync::Weak<Mutex<Registry>>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let stop = |reason: Option<String>| {
            alive.store(false, Ordering::SeqCst);
            if let Some(reason) = reason {
                *failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
            }
            let message = failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .unwrap_or_else(|| "The language server stopped".to_owned());
            // Every waiter learns at once rather than each timing out alone.
            let mut waiting = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (_, sender) in waiting.drain() {
                let _ = sender.send(Err(message.clone()));
            }
        };
        loop {
            let message = match read_message(&mut reader) {
                Ok(Some(body)) => body,
                Ok(None) => {
                    stop(Some(format!(
                        "The language server '{}' exited",
                        config.name
                    )));
                    return;
                }
                Err(error) => {
                    stop(Some(error));
                    return;
                }
            };
            let Ok(value) = serde_json::from_slice::<Value>(&message) else {
                // A body that is not JSON is a protocol violation, not a
                // recoverable hiccup: the stream offset is no longer trusted.
                stop(Some(format!(
                    "The language server '{}' sent a message that is not valid JSON",
                    config.name
                )));
                return;
            };
            let Some(object) = value.as_object() else {
                continue;
            };
            let id = object.get("id");
            let method = object.get("method").and_then(Value::as_str);
            match (id, method) {
                // A response to something this client asked.
                (Some(id), None) => {
                    let Some(id) = id.as_i64() else { continue };
                    let sender = pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&id)
                        .cloned();
                    let Some(sender) = sender else { continue };
                    let payload = if let Some(error) = object.get("error") {
                        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                        let text = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        if code == CONTENT_MODIFIED {
                            Err(format!("{CONTENT_MODIFIED_MARKER}{text}"))
                        } else {
                            Err(text.to_owned())
                        }
                    } else {
                        Ok(object.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = sender.send(payload);
                }
                // A request from the server. Answering is not optional: a
                // server left waiting on `client/registerCapability` never
                // finishes starting.
                (Some(id), Some(method)) => {
                    let Some(connection) = connection.upgrade() else {
                        return;
                    };
                    let reply = answer_server_request(method, object.get("params"), &config, &root.path);
                    let payload = match reply {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err(message) => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": message },
                        }),
                    };
                    let _ = connection.send(&payload);
                }
                // A notification.
                (None, Some("textDocument/publishDiagnostics")) => {
                    if !config.diagnostics {
                        continue;
                    }
                    let Some(params) = object.get("params") else {
                        continue;
                    };
                    let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                        continue;
                    };
                    let diagnostics = params
                        .get("diagnostics")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().map(parse_diagnostic).collect::<Vec<_>>())
                        .unwrap_or_default();
                    // The registry that owns this server, not a process-wide
                    // one: a weak handle so a reader thread can never keep a
                    // dead registry (or a test's) alive.
                    if let Some(registry) = sink.upgrade() {
                        registry
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .diagnostics
                            .publish(&root, uri.to_owned(), diagnostics);
                    }                }
                // Everything else a server volunteers — log messages, progress
                // — is noise this client has no surface for.
                (None, Some(_)) | (None, None) => {}
            }
        }
    });
}

fn parse_diagnostic(value: &Value) -> Diagnostic {
    let start = value.get("range").and_then(|range| range.get("start"));
    Diagnostic {
        severity: severity_name(value.get("severity")),
        line: start
            .and_then(|start| start.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        character: start
            .and_then(|start| start.get("character"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        code: match value.get("code") {
            Some(Value::String(code)) => code.clone(),
            Some(Value::Number(code)) => code.to_string(),
            _ => String::new(),
        },
        source: value
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

/// Answers the server-to-client requests a real server sends.
///
/// The source keeps this set small by advertising `dynamicRegistration: false`
/// everywhere, but `client/registerCapability` and the progress-token requests
/// arrive anyway; a `-32601` to those is what makes `rust-analyzer` hang.
fn answer_server_request(
    method: &str,
    params: Option<&Value>,
    config: &LspServerConfig,
    root: &Path,
) -> Result<Value, String> {
    match method {
        "workspace/configuration" => {
            let items = params
                .and_then(|params| params.get("items"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(Value::Array(
                items
                    .iter()
                    .map(|item| {
                        let section = item.get("section").and_then(Value::as_str);
                        settings_section(config.settings.as_ref(), section)
                    })
                    .collect(),
            ))
        }
        "workspace/workspaceFolders" => Ok(json!([{
            "uri": path_to_uri(root),
            "name": root
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }])),
        // Accepted and ignored: this client has no dynamic capability table,
        // and a server that registers one only needs to be told it landed.
        "client/registerCapability" | "client/unregisterCapability" => Ok(Value::Null),
        // Progress reporting has no surface here, but refusing the token makes
        // some servers treat startup as failed.
        "window/workDoneProgress/create" => Ok(Value::Null),
        // This client never applies server-authored edits: the model edits
        // through the `write` and `edit` tools, which are approved.
        "workspace/applyEdit" => Ok(json!({ "applied": false })),
        "window/showMessageRequest" => Ok(Value::Null),
        other => Err(format!("Mewrk's LSP client does not implement {other}")),
    }
}

/// Reads one dotted section out of a server's `settings` object, as the source
/// does for `workspace/configuration`.
fn settings_section(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(settings) = settings else {
        return Value::Null;
    };
    let Some(section) = section.filter(|value| !value.is_empty()) else {
        return settings.clone();
    };
    let mut cursor = settings;
    for part in section.split('.') {
        match cursor.get(part) {
            Some(next) => cursor = next,
            None => return Value::Null,
        }
    }
    cursor.clone()
}

/// Asks a server to shut down, then makes sure it and its children are gone.
///
/// The graceful half is bounded by the entry's `shutdownTimeout`; the kill is
/// not optional, because a language server that ignores `exit` still holds the
/// inherited stdout handle and keeps a reader thread alive forever.
fn stop_connection(connection: &Connection) {
    if connection.is_alive() {
        let _ = connection.request("shutdown", Value::Null, connection.shutdown_timeout);
        let _ = connection.notify("exit", Value::Null);
    }
    connection.alive.store(false, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + STOP_GRACE;
    let mut child = connection
        .child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let job = connection
        .job
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    kill_shell_child(&mut child, &job);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_framed_message_round_trips_through_the_reader() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(body);
        let mut reader = BufReader::new(Cursor::new(framed));
        let read = read_message(&mut reader)
            .expect("the frame parses")
            .expect("there is a message");
        assert_eq!(read, body);
        assert!(
            read_message(&mut reader).expect("a clean end is not an error").is_none(),
            "end of stream is None, not an error"
        );
    }

    #[test]
    fn headers_are_case_insensitive_and_extra_headers_are_ignored() {
        let body = b"{}";
        let mut framed =
            b"content-type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: 2\r\n\r\n"
                .to_vec();
        framed.extend_from_slice(body);
        let mut reader = BufReader::new(Cursor::new(framed));
        assert_eq!(
            read_message(&mut reader).expect("parses").expect("present"),
            body
        );
    }

    #[test]
    fn a_header_block_without_a_content_length_is_a_protocol_violation() {
        let framed = b"Content-Type: text/plain\r\n\r\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(framed));
        let error = read_message(&mut reader).expect_err("a frame with no length cannot be read");
        assert!(error.contains("Content-Length"), "{error}");
    }

    #[test]
    fn a_server_logging_to_stdout_is_reported_rather_than_resynchronized() {
        let framed = b"info: starting up\r\n\r\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(framed));
        let error = read_message(&mut reader).expect_err("plain text is not a header block");
        assert!(error.contains("desynchronized"), "{error}");
    }

    #[test]
    fn two_messages_in_one_buffer_are_read_in_order() {
        let mut framed = Vec::new();
        for body in [br#"{"id":1}"#.as_slice(), br#"{"id":2}"#.as_slice()] {
            framed.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
            framed.extend_from_slice(body);
        }
        let mut reader = BufReader::new(Cursor::new(framed));
        assert_eq!(
            read_message(&mut reader).unwrap().unwrap(),
            br#"{"id":1}"#.to_vec()
        );
        assert_eq!(
            read_message(&mut reader).unwrap().unwrap(),
            br#"{"id":2}"#.to_vec()
        );
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn a_windows_path_uri_round_trips() {
        let uri = path_to_uri(Path::new("C:\\Users\\me\\src\\main.rs"));
        assert_eq!(uri, "file:///C:/Users/me/src/main.rs");
        assert_eq!(uri_to_path(&uri), "C:/Users/me/src/main.rs");
    }

    /// The path guard hands navigation `fs::canonicalize`'s answer, and on
    /// Windows that is always the extended-length spelling. Encoding it
    /// literally produced `file:////?/C:/…`, which `rust-analyzer` rejects with
    /// `-32603 url is not a file` — every `lsp` call on Windows, not an edge
    /// case.
    #[test]
    fn a_verbatim_windows_path_loses_its_extended_length_prefix() {
        let uri = path_to_uri(Path::new("\\\\?\\C:\\Users\\me\\src\\main.rs"));
        assert_eq!(uri, "file:///C:/Users/me/src/main.rs");
        assert_eq!(uri_to_path(&uri), "C:/Users/me/src/main.rs");
        assert!(!uri.starts_with("file:////"), "{uri}");
    }

    /// The same file reached through the guard and through the edit hook must
    /// produce one URI, because the document map is keyed by it.
    #[test]
    fn the_verbatim_and_plain_spellings_of_one_file_agree() {
        assert_eq!(
            path_to_uri(Path::new("\\\\?\\C:\\work\\a.rs")),
            path_to_uri(Path::new("C:\\work\\a.rs"))
        );
    }

    #[test]
    fn a_network_path_keeps_its_host_in_the_authority() {
        let uri = path_to_uri(Path::new("\\\\build01\\share\\src\\main.rs"));
        assert_eq!(uri, "file://build01/share/src/main.rs");
        assert_eq!(uri_to_path(&uri), "//build01/share/src/main.rs");
        // Canonicalizing a network path yields the verbatim UNC spelling, which
        // has to land on the same URI as the plain one.
        assert_eq!(
            path_to_uri(Path::new("\\\\?\\UNC\\build01\\share\\src\\main.rs")),
            uri
        );
    }

    #[test]
    fn a_posix_path_uri_round_trips_and_spaces_are_encoded() {
        let uri = path_to_uri(Path::new("/home/me/my project/main.rs"));
        assert_eq!(uri, "file:///home/me/my%20project/main.rs");
        assert_eq!(uri_to_path(&uri), "/home/me/my project/main.rs");
    }

    #[test]
    fn a_configuration_request_is_answered_section_by_section() {
        let settings = json!({ "rust-analyzer": { "cargo": { "allFeatures": true } } });
        assert_eq!(
            settings_section(Some(&settings), Some("rust-analyzer.cargo")),
            json!({ "allFeatures": true })
        );
        assert_eq!(
            settings_section(Some(&settings), Some("missing.section")),
            Value::Null
        );
        assert_eq!(settings_section(Some(&settings), None), settings);
        assert_eq!(settings_section(None, Some("anything")), Value::Null);
    }

    fn test_config() -> LspServerConfig {
        LspServerConfig {
            id: "id".into(),
            name: "test".into(),
            description: String::new(),
            command: "server".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            extension_to_language: [(".rs".to_owned(), "rust".to_owned())]
                .into_iter()
                .collect(),
            initialization_options: None,
            settings: None,
            workspace_folder: String::new(),
            startup_timeout_millis: 1_000,
            shutdown_timeout_millis: 1_000,
            restart_on_crash: true,
            max_restarts: 3,
            diagnostics: true,
        }
    }

    #[test]
    fn capability_registration_is_acknowledged_rather_than_refused() {
        let config = test_config();
        let root = Path::new("/tmp");
        for method in [
            "client/registerCapability",
            "client/unregisterCapability",
            "window/workDoneProgress/create",
            "window/showMessageRequest",
        ] {
            assert!(
                answer_server_request(method, None, &config, root).is_ok(),
                "{method} must be answered, not refused: a server waiting on it never finishes starting"
            );
        }
        assert_eq!(
            answer_server_request("workspace/applyEdit", None, &config, root).unwrap(),
            json!({ "applied": false })
        );
        assert!(
            answer_server_request("workspace/somethingElse", None, &config, root).is_err(),
            "an unknown request still gets a method-not-found"
        );
    }

    #[test]
    fn the_extension_table_picks_the_first_configuration_that_claims_a_file() {
        let mut first = test_config();
        first.name = "first".into();
        let mut second = test_config();
        second.name = "second".into();
        let configs = vec![first, second];
        let (chosen, language) =
            LspRegistry::config_for_path(&configs, Path::new("/src/main.rs")).expect("claimed");
        assert_eq!(chosen.name, "first");
        assert_eq!(language, "rust");
        assert!(
            LspRegistry::config_for_path(&configs, Path::new("/src/main.py")).is_none(),
            "an unclaimed extension resolves to nothing"
        );
        assert!(
            LspRegistry::config_for_path(&configs, Path::new("/src/MAIN.RS")).is_some(),
            "extension matching is case-insensitive"
        );
    }

    fn project_root() -> PathBuf {
        PathBuf::from("/work/project")
    }

    fn other_root() -> PathBuf {
        PathBuf::from("/work/other")
    }

    fn local(path: PathBuf) -> ServerRoot {
        ServerRoot::local(path)
    }

    fn diagnostic(message: &str, line: u64) -> Diagnostic {
        Diagnostic {
            severity: "Error".into(),
            line,
            character: 0,
            message: message.into(),
            code: String::new(),
            source: "test".into(),
        }
    }

    #[test]
    fn a_conversation_is_owed_nothing_until_it_registers() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.publish(&local(project_root()), "file:///a.rs".into(), vec![diagnostic("before", 1)]);
        ledger.register("c1", &local(project_root()));
        assert!(
            ledger.take_new("c1").is_empty(),
            "a conversation must not open with somebody else's backlog"
        );
        ledger.publish(&local(project_root()), "file:///a.rs".into(), vec![diagnostic("after", 2)]);
        assert_eq!(ledger.take_new("c1").len(), 1);
    }

    #[test]
    fn the_same_problem_is_reported_once() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("c1", &local(project_root()));
        ledger.publish(&local(project_root()), "file:///a.rs".into(), vec![diagnostic("same", 1)]);
        assert_eq!(ledger.take_new("c1").len(), 1);
        ledger.publish(&local(project_root()), "file:///a.rs".into(), vec![diagnostic("same", 1)]);
        assert!(
            ledger.take_new("c1").is_empty(),
            "an unchanged republish is not news"
        );
    }

    #[test]
    fn an_empty_publish_clears_the_file_without_becoming_news() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("c1", &local(project_root()));
        ledger.publish(&local(project_root()), "file:///a.rs".into(), vec![diagnostic("broken", 1)]);
        ledger.publish(&local(project_root()), "file:///a.rs".into(), Vec::new());
        assert!(
            ledger.take_new("c1").is_empty(),
            "a file that became clean has nothing to report"
        );
    }

    #[test]
    fn volume_limits_cap_one_injection() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("c1", &local(project_root()));
        for file in 0..5 {
            let diagnostics = (0..20)
                .map(|index| diagnostic(&format!("problem {file}-{index}"), index))
                .collect();
            ledger.publish(&local(project_root()), format!("file:///f{file}.rs"), diagnostics);
        }
        let taken = ledger.take_new("c1");
        let total: usize = taken.iter().map(|(_, items)| items.len()).sum();
        assert!(total <= MAX_DIAGNOSTICS_TOTAL, "{total} exceeded the total cap");
        for (uri, items) in &taken {
            assert!(
                items.len() <= MAX_DIAGNOSTICS_PER_FILE,
                "{uri} exceeded the per-file cap"
            );
        }
    }

    #[test]
    fn two_conversations_are_owed_independently() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("c1", &local(project_root()));
        ledger.register("c2", &local(project_root()));
        ledger.publish(&local(project_root()), "file:///a.rs".into(), vec![diagnostic("shared", 1)]);
        assert_eq!(ledger.take_new("c1").len(), 1);
        assert_eq!(
            ledger.take_new("c2").len(),
            1,
            "draining one conversation must not consume another's"
        );
    }

    #[test]
    fn a_conversation_is_not_told_about_another_projects_problems() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("here", &local(project_root()));
        ledger.register("there", &local(other_root()));
        ledger.publish(
            &local(other_root()),
            "file:///work/other/src/main.rs".into(),
            vec![diagnostic("broken over there", 1)],
        );
        assert!(
            ledger.take_new("here").is_empty(),
            "a conversation in one project must never be handed another project's compile errors"
        );
        assert_eq!(ledger.take_new("there").len(), 1);
    }

    /// A server entry that names its own `workspaceFolder` publishes under that
    /// folder, not under the conversation's workspace. Registering the
    /// conversation under the workspace left it subscribed to a root nothing
    /// ever published to, so that server's problems were dropped in silence.
    #[test]
    fn a_server_with_its_own_workspace_folder_is_registered_under_the_root_it_publishes_to() {
        let mut config = test_config();
        config.workspace_folder = other_root().to_string_lossy().into_owned();
        let workspace = project_root();
        let root = server_root(&config, &workspace);
        assert_eq!(root, other_root(), "the entry's own folder wins");

        let mut ledger = DiagnosticsLedger::default();
        ledger.register("c1", &local(root.clone()));
        ledger.publish(&local(root.clone()), "file:///work/other/src/main.rs".into(), vec![diagnostic("broken", 1)]);
        assert_eq!(
            ledger.take_new("c1").len(),
            1,
            "a configured workspaceFolder must not swallow the server's diagnostics"
        );
    }

    #[test]
    fn a_server_without_a_workspace_folder_is_rooted_at_the_conversations_workspace() {
        let config = test_config();
        assert_eq!(server_root(&config, &project_root()), project_root());
    }

    #[test]
    fn a_conversation_that_works_in_two_projects_is_owed_both() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("both", &local(project_root()));
        ledger.register("both", &local(other_root()));
        ledger.publish(&local(project_root()), "file:///a.rs".into(), vec![diagnostic("a", 1)]);
        ledger.publish(&local(other_root()), "file:///b.rs".into(), vec![diagnostic("b", 1)]);
        assert_eq!(ledger.take_new("both").len(), 2);
    }

    #[test]
    fn tracked_files_and_conversations_are_evicted_rather_than_growing_forever() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("c1", &local(project_root()));
        for index in 0..(MAX_TRACKED_FILES + 50) {
            ledger.publish(
                &local(project_root()),
                format!("file:///f{index}.rs"),
                vec![diagnostic("problem", 1)],
            );
        }
        assert!(
            ledger.current.len() <= MAX_TRACKED_FILES,
            "a long session must not accumulate every file it ever saw: {}",
            ledger.current.len()
        );

        for index in 0..(MAX_TRACKED_CONVERSATIONS + 10) {
            ledger.register(&format!("conversation-{index}"), &local(project_root()));
        }
        assert!(
            ledger.conversations.len() <= MAX_TRACKED_CONVERSATIONS,
            "conversation ledgers are evicted oldest-first: {}",
            ledger.conversations.len()
        );
    }

    #[test]
    fn the_rendered_block_matches_the_sources_shape() {
        let rendered = render_diagnostics(&[(
            "file:///c/src/main.rs".into(),
            vec![Diagnostic {
                severity: "Error".into(),
                line: 9,
                character: 4,
                message: "cannot find value `x`".into(),
                code: "E0425".into(),
                source: "rustc".into(),
            }],
        )]);
        assert_eq!(
            rendered,
            "<new-diagnostics>The following new diagnostic issues were detected:\n\nmain.rs:\n  Error [Line 10:5] cannot find value `x` [E0425] (rustc)</new-diagnostics>"
        );
    }

    #[test]
    fn a_long_block_is_cut_with_the_sources_marker() {
        let diagnostics = (0..2_000)
            .map(|index| diagnostic(&format!("a fairly long diagnostic message {index}"), index))
            .collect::<Vec<_>>();
        let rendered = render_diagnostics(&[("file:///a.rs".into(), diagnostics)]);
        assert!(rendered.contains("…[truncated]"), "long bodies are cut");
    }

    #[test]
    fn severity_integers_map_to_the_sources_names() {
        assert_eq!(severity_name(Some(&json!(1))), "Error");
        assert_eq!(severity_name(Some(&json!(2))), "Warning");
        assert_eq!(severity_name(Some(&json!(3))), "Info");
        assert_eq!(severity_name(Some(&json!(4))), "Hint");
        assert_eq!(
            severity_name(None),
            "Error",
            "an absent severity defaults to Error, as in the source"
        );
    }

    /// `/srv/app` on a WSL distribution and `/srv/app` on an SSH machine are
    /// two projects. A ledger keyed by path alone would hand the conversation
    /// working on one the other's compile errors.
    #[test]
    fn the_same_path_on_two_machines_is_two_roots() {
        let wsl = ServerRoot {
            machine: "wsl:Ubuntu".into(),
            path: PathBuf::from("/srv/app"),
        };
        let ssh = ServerRoot {
            machine: "ssh:m1".into(),
            path: PathBuf::from("/srv/app"),
        };
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("on-wsl", &wsl);
        ledger.register("on-ssh", &ssh);
        ledger.publish(&ssh, "file:///srv/app/main.rs".into(), vec![diagnostic("there", 1)]);
        assert!(
            ledger.take_new("on-wsl").is_empty(),
            "a publish from one machine is not news on another"
        );
        assert_eq!(ledger.take_new("on-ssh").len(), 1);
    }

    /// The remote launch script is the one place a repository-shipped
    /// `lsp.json` reaches a shell: every fragment of it is single-quoted, the
    /// server is `exec`ed so the wrapper's pipes are its pipes, and a command
    /// that is not there is named before anything runs.
    #[test]
    fn the_remote_launch_script_quotes_everything_and_execs_the_server() {
        let mut config = test_config();
        config.command = "rust-analyzer".into();
        config.args = vec!["--log-file".into(), "/tmp/it's here.log".into()];
        config.env = [("RA_LOG".to_owned(), "error".to_owned())].into_iter().collect();
        let script = remote_launch_script(&config, Path::new("/home/dev/app")).unwrap();
        assert_eq!(
            script,
            "cd -- '/home/dev/app' || exit 64\n\
             command -v 'rust-analyzer' >/dev/null 2>&1 || { printf '%s\\n' 'rust-analyzer: command not found on the remote machine'\\''s PATH' >&2; exit 127; }\n\
             exec env 'RA_LOG=error' 'rust-analyzer' '--log-file' '/tmp/it'\\''s here.log'\n"
        );

        // A `~`-spelled root stays expandable, as the file tools spell it.
        let script = remote_launch_script(&config, Path::new("~/app")).unwrap();
        assert!(script.starts_with("cd -- ~/'app' || exit 64\n"), "{script}");

        // Control characters cannot be quoted into safety and are refused.
        config.args = vec!["--x\n; rm -rf /".into()];
        assert!(remote_launch_script(&config, Path::new("/home/dev/app")).is_err());
        config.args.clear();
        config.env = [("BAD NAME".to_owned(), "x".to_owned())].into_iter().collect();
        assert!(remote_launch_script(&config, Path::new("/home/dev/app")).is_err());
    }

    #[test]
    fn the_stderr_tail_is_the_last_non_empty_line_cut_short() {
        assert_eq!(last_stderr_line("a\nb\n\n  \n"), "b");
        assert_eq!(last_stderr_line(""), "");
        let long = "x".repeat(300);
        let tail = last_stderr_line(&long);
        assert_eq!(tail.chars().count(), 241);
        assert!(tail.ends_with('…'));
    }

    /// A language server that speaks the base protocol over stdio, written in
    /// Node so the test needs nothing this repository does not already need.
    /// It answers `initialize`, `hover` (with its working directory and one
    /// environment variable, so the launch script's `cd` and `env` are
    /// observable), `shutdown` and `exit`, and writes one line to stderr.
    const FAKE_SERVER: &str = r#"
let buffer = Buffer.alloc(0);
function send(payload) {
  const body = Buffer.from(JSON.stringify(payload), "utf8");
  process.stdout.write(`Content-Length: ${body.length}\r\n\r\n`);
  process.stdout.write(body);
}
function handle(message) {
  if (message.method === "initialize") {
    send({ jsonrpc: "2.0", id: message.id, result: { capabilities: {}, serverInfo: { name: "fake" } } });
  } else if (message.method === "textDocument/hover") {
    const value = `cwd=${process.cwd()} env=${process.env.FAKE_LSP || ""}`;
    send({ jsonrpc: "2.0", id: message.id, result: { contents: { kind: "plaintext", value } } });
  } else if (message.method === "shutdown") {
    send({ jsonrpc: "2.0", id: message.id, result: null });
  } else if (message.method === "exit") {
    process.exit(0);
  }
}
process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    const headerEnd = buffer.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;
    const match = /Content-Length:\s*(\d+)/i.exec(buffer.slice(0, headerEnd).toString("utf8"));
    if (!match) process.exit(3);
    const start = headerEnd + 4;
    const length = Number(match[1]);
    if (buffer.length < start + length) return;
    const body = JSON.parse(buffer.slice(start, start + length).toString("utf8"));
    buffer = buffer.slice(start + length);
    handle(body);
  }
});
process.stdin.on("end", () => process.exit(0));
process.stderr.write("fake server up\n");
"#;

    /// A local Bash running the remote launch script, standing in for the
    /// `wsl.exe`/`ssh` wrapper exactly as the remote file tools' tests do.
    fn spawn_through_local_bash(script: &str) -> Option<Child> {
        let bash = crate::run_environment::local_bash_candidates().into_iter().next()?;
        Command::new(bash)
            .args(["--noprofile", "--norc", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()
    }

    fn fake_host() -> ServerHost {
        ServerHost::Remote {
            runner: ShellRunner::Ssh {
                agent_shell: Default::default(),
                host: "fake".into(),
                port: 0,
                identity_file: String::new(),
                env: BTreeMap::new(),
            },
            machine_key: "ssh:fake".into(),
        }
    }

    /// The whole remote leg short of the machine: the launch script starts the
    /// server through a shell, the wrapper's pipes carry the protocol, the
    /// entry's `env` and the root's `cd` reach the server, the handshake
    /// completes with a `null` `processId`, documents sync, requests are
    /// answered, stderr is kept, and `shutdown`/`exit` end the process.
    #[test]
    fn a_remote_launch_script_drives_a_server_through_a_shell() {
        let Some(node) = crate::environment_tools::resolve_on_path("node") else {
            eprintln!("skipped: node is not on PATH");
            return;
        };
        let temp = tempfile::tempdir().expect("temp dir");
        let server = temp.path().join("fake-lsp.js");
        std::fs::write(&server, FAKE_SERVER).expect("fake server written");
        let root = temp.path().to_string_lossy().replace('\\', "/");

        let mut config = test_config();
        config.command = node.to_string_lossy().replace('\\', "/");
        config.args = vec![server.to_string_lossy().replace('\\', "/")];
        config.env = [("FAKE_LSP".to_owned(), "yes".to_owned())].into_iter().collect();
        let script = remote_launch_script(&config, Path::new(&root)).expect("launch script");
        let Some(child) = spawn_through_local_bash(&script) else {
            eprintln!("skipped: no local bash");
            return;
        };

        let host = fake_host();
        let server_root = ServerRoot::new(&host, PathBuf::from(&root));
        let connection = attach_server(
            ShellChild::Local(child),
            &host,
            &config,
            &server_root,
            std::sync::Weak::new(),
        )
        .expect("the handshake completes through the shell");
        assert!(connection.is_alive());

        let document = PathBuf::from(format!("{root}/main.rs"));
        connection
            .sync_document(&document, "rust", "fn main() {}\n")
            .expect("didOpen reaches the server");
        assert!(connection.has_document(&document));
        let answer = connection
            .request_with_retry(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": path_to_uri(&document) },
                    "position": { "line": 0, "character": 3 },
                }),
            )
            .expect("hover is answered");
        let value = answer["contents"]["value"].as_str().unwrap_or_default();
        assert!(value.contains("env=yes"), "the entry's env reached the server: {value}");
        let cwd = value
            .split_once("cwd=")
            .and_then(|(_, rest)| rest.split_once(" env="))
            .map(|(cwd, _)| cwd.replace('\\', "/"))
            .unwrap_or_default();
        let leaf = Path::new(&root)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(
            cwd.to_ascii_lowercase()
                .ends_with(&leaf.to_ascii_lowercase()),
            "the script's cd put the server in the root: {cwd} vs {root}"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !connection.diagnostics_log().contains("fake server up") {
            assert!(std::time::Instant::now() < deadline, "stderr is drained");
            thread::sleep(Duration::from_millis(20));
        }

        stop_connection(&connection);
        assert!(!connection.is_alive());
        let mut child = connection.child.lock().unwrap();
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "shutdown/exit ended the process behind the shell"
        );
    }

    /// An `lsp.json` edit takes effect on the next call: a running server whose
    /// entry changed is stopped and started again from the new entry, and an
    /// unchanged entry — or one whose description alone changed — keeps the
    /// server it has. A start that failed is not held against the entry in the
    /// meantime: it is simply tried again.
    #[test]
    fn a_changed_entry_restarts_its_server() {
        let Some(node) = crate::environment_tools::resolve_on_path("node") else {
            eprintln!("skipped: node is not on PATH");
            return;
        };
        let temp = tempfile::tempdir().expect("temp dir");
        let server = temp.path().join("fake-lsp.js");
        std::fs::write(&server, FAKE_SERVER).expect("fake server written");
        let registry = LspRegistry::default();
        let root = ServerRoot::local(temp.path());

        let mut broken = test_config();
        broken.command = "definitely-not-a-language-server-xyz".into();
        let first = registry
            .ensure_started(&ServerHost::Local, &broken, &root)
            .err()
            .expect("a missing command does not start");
        let again = registry
            .ensure_started(&ServerHost::Local, &broken, &root)
            .err()
            .expect("and does not start the second time either");
        assert_eq!(first, again);

        let mut fixed = test_config();
        fixed.command = node.to_string_lossy().into_owned();
        fixed.args = vec![server.to_string_lossy().into_owned()];
        let started = registry
            .ensure_started(&ServerHost::Local, &fixed, &root)
            .expect("the fixed entry starts");
        let reused = registry
            .ensure_started(&ServerHost::Local, &fixed, &root)
            .expect("still running");
        assert!(Arc::ptr_eq(&started, &reused));
        let reworded = LspServerConfig {
            description: "only the words changed".into(),
            ..fixed.clone()
        };
        let reused = registry
            .ensure_started(&ServerHost::Local, &reworded, &root)
            .expect("still running");
        assert!(Arc::ptr_eq(&started, &reused));

        let mut changed = fixed.clone();
        changed.env = [("FAKE_LSP".to_owned(), "new".to_owned())].into_iter().collect();
        let restarted = registry
            .ensure_started(&ServerHost::Local, &changed, &root)
            .expect("the changed entry starts");
        assert!(!Arc::ptr_eq(&started, &restarted));
        assert!(!started.is_alive(), "the server of the old entry was stopped");
        assert!(restarted.is_alive());
        stop_connection(&restarted);
    }

    /// A command the machine does not have is the most likely remote failure,
    /// and the script names it on stderr before exiting — which is what the
    /// handshake failure hands back, instead of "the language server exited".
    #[test]
    fn a_missing_remote_command_is_named_in_the_failure() {
        let mut config = test_config();
        config.command = "definitely-not-a-language-server-xyz".into();
        config.args.clear();
        config.startup_timeout_millis = 5_000;
        let script = remote_launch_script(&config, Path::new("/")).expect("launch script");
        let Some(child) = spawn_through_local_bash(&script) else {
            eprintln!("skipped: no local bash");
            return;
        };
        let host = fake_host();
        let Err(error) = attach_server(
            ShellChild::Local(child),
            &host,
            &config,
            &ServerRoot::new(&host, PathBuf::from("/")),
            std::sync::Weak::new(),
        ) else {
            panic!("a missing command cannot complete the handshake");
        };
        assert!(
            error
                .message
                .contains("command not found on the remote machine's PATH"),
            "{}",
            error.message
        );
        assert!(
            !error.crashed,
            "a launch script that refused (exit 127) is not the server crashing"
        );
    }

    // -----------------------------------------------------------------------
    // The diagnostics ledger after an edit
    // -----------------------------------------------------------------------

    /// A problem that is still there after an edit has the same fingerprint as
    /// the one reported before it. Forgetting the file is what lets it be
    /// reported again.
    #[test]
    fn an_edit_makes_a_files_problems_reportable_again() {
        let mut ledger = DiagnosticsLedger::default();
        ledger.register("c1", &local(project_root()));
        let root = local(project_root());
        ledger.publish(&root, "file:///a.rs".into(), vec![diagnostic("still broken", 1)]);
        assert_eq!(ledger.take_new("c1").len(), 1);
        ledger.publish(&root, "file:///a.rs".into(), vec![diagnostic("still broken", 1)]);
        assert!(ledger.take_new("c1").is_empty(), "unchanged, so not news");

        ledger.forget_file("c1", "file:///a.rs");
        ledger.publish(&root, "file:///a.rs".into(), vec![diagnostic("still broken", 1)]);
        assert_eq!(
            ledger.take_new("c1").len(),
            1,
            "after the edit the same problem is reported once more"
        );
    }

    /// What was queued for the file describes text that no longer exists.
    #[test]
    fn an_edit_drops_what_was_queued_for_the_file() {
        let mut ledger = DiagnosticsLedger::default();
        let root = local(project_root());
        ledger.register("c1", &root);
        ledger.publish(&root, "file:///a.rs".into(), vec![diagnostic("from the old text", 1)]);
        ledger.forget_file("c1", "file:///a.rs");
        assert!(ledger.take_new("c1").is_empty());
    }

    /// The records are per conversation and per file: another conversation did
    /// not make the edit, and another file did not change.
    #[test]
    fn forgetting_a_file_touches_neither_other_files_nor_other_conversations() {
        let mut ledger = DiagnosticsLedger::default();
        let root = local(project_root());
        ledger.register("editor", &root);
        ledger.register("bystander", &root);
        for uri in ["file:///a.rs", "file:///b.rs"] {
            ledger.publish(&root, uri.into(), vec![diagnostic("broken", 1)]);
        }
        assert_eq!(ledger.take_new("editor").len(), 2);
        assert_eq!(ledger.take_new("bystander").len(), 2);

        ledger.forget_file("editor", "file:///a.rs");
        for uri in ["file:///a.rs", "file:///b.rs"] {
            ledger.publish(&root, uri.into(), vec![diagnostic("broken", 1)]);
        }
        let editor = ledger.take_new("editor");
        assert_eq!(
            editor.iter().map(|(uri, _)| uri.as_str()).collect::<Vec<_>>(),
            ["file:///a.rs"],
            "only the edited file is reportable again"
        );
        assert!(
            ledger.take_new("bystander").is_empty(),
            "the bystander was told already, and did not edit anything"
        );
        // A conversation the ledger does not know is not an error.
        ledger.forget_file("nobody", "file:///a.rs");
    }

    #[test]
    fn the_registry_forgets_a_file_by_its_path() {
        let registry = LspRegistry::default();
        let root = local(project_root());
        assert!(!registry.tracks_conversation("c1"));
        registry.register_conversation("c1", &root);
        assert!(registry.tracks_conversation("c1"));
        let path = project_root().join("src").join("main.rs");
        let uri = path_to_uri(&path);
        registry.lock().diagnostics.publish(&root, uri, vec![diagnostic("broken", 1)]);
        registry.forget_file_diagnostics("c1", &path);
        assert!(registry.take_new_diagnostics("c1").is_empty());
    }

    // -----------------------------------------------------------------------
    // What counts as a crash
    // -----------------------------------------------------------------------

    #[test]
    fn only_an_abnormal_exit_counts_as_a_crash() {
        // Wrapped or not, a server that exits cleanly did not crash, and one
        // that dies with an ordinary failure code or a signal did.
        for wrapped in [false, true] {
            assert!(!counts_as_crash(ExitKind::Clean, wrapped));
            assert!(counts_as_crash(ExitKind::Code(1), wrapped));
            assert!(counts_as_crash(ExitKind::Code(139), wrapped));
            assert!(counts_as_crash(ExitKind::Abnormal, wrapped));
        }
        // Through a wrapper, the launch script's refusals and ssh's own
        // failure are the machine being out of reach, not the server dying.
        for code in [64, 127, 255] {
            assert!(!counts_as_crash(ExitKind::Code(code), true), "{code}");
            assert!(counts_as_crash(ExitKind::Code(code), false), "{code}");
        }
    }

    // -----------------------------------------------------------------------
    // Starting servers: one at a time, in the background, and failing cleanly
    // -----------------------------------------------------------------------

    /// A language server that also records what it is told, so the tests can
    /// see how many processes were started and which documents they opened.
    /// Behaviour is chosen by environment variables:
    ///
    /// * `FAKE_LSP_LOG` — file every start and document event is appended to.
    /// * `FAKE_LSP_DELAY_MS` — how long `initialize` takes to answer.
    /// * `FAKE_LSP_PUBLISH` — publish one error for every document opened.
    /// * `FAKE_LSP_MARKER` — a file whose absence marks the first start.
    /// * `FAKE_LSP_MODE` — `crash-on-start` (exit 1 before answering),
    ///   `crash-first-start`, `reject-first-initialize` (answer the first
    ///   `initialize` with an error), `die-on-hover` (exit 1),
    ///   `exit-clean-on-hover` (exit 0).
    const RECORDING_SERVER: &str = r#"
const fs = require("fs");
const log = process.env.FAKE_LSP_LOG;
const mode = process.env.FAKE_LSP_MODE || "";
const delay = Number(process.env.FAKE_LSP_DELAY_MS || 0);
const marker = process.env.FAKE_LSP_MARKER || "";
function record(line) { if (log) fs.appendFileSync(log, line + "\n"); }
function firstStart() {
  if (!marker || fs.existsSync(marker)) return false;
  fs.writeFileSync(marker, "1");
  return true;
}
record("start");
if (mode === "crash-on-start") process.exit(1);
if (mode === "crash-first-start" && firstStart()) process.exit(1);
let buffer = Buffer.alloc(0);
function send(payload) {
  const body = Buffer.from(JSON.stringify(payload), "utf8");
  process.stdout.write(`Content-Length: ${body.length}\r\n\r\n`);
  process.stdout.write(body);
}
function handle(message) {
  const params = message.params || {};
  switch (message.method) {
    case "initialize":
      if (mode === "reject-first-initialize" && firstStart()) {
        send({ jsonrpc: "2.0", id: message.id, error: { code: -32603, message: "refused" } });
        return;
      }
      setTimeout(() => send({ jsonrpc: "2.0", id: message.id, result: { capabilities: {} } }), delay);
      break;
    case "textDocument/didOpen": {
      const document = params.textDocument;
      record("open " + document.uri + " " + JSON.stringify(document.text));
      if (process.env.FAKE_LSP_PUBLISH) {
        send({
          jsonrpc: "2.0",
          method: "textDocument/publishDiagnostics",
          params: {
            uri: document.uri,
            diagnostics: [{
              range: { start: { line: 0, character: 0 }, end: { line: 0, character: 1 } },
              severity: 1, message: "broken", source: "fake",
            }],
          },
        });
      }
      break;
    }
    case "textDocument/didChange":
      record("change " + params.textDocument.uri + " " + JSON.stringify(params.contentChanges[0].text));
      break;
    case "textDocument/didSave":
      record("save " + params.textDocument.uri);
      break;
    case "textDocument/hover":
      if (mode === "die-on-hover") process.exit(1);
      if (mode === "exit-clean-on-hover") process.exit(0);
      send({ jsonrpc: "2.0", id: message.id, result: null });
      break;
    case "shutdown":
      send({ jsonrpc: "2.0", id: message.id, result: null });
      break;
    case "exit":
      process.exit(0);
  }
}
process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    const headerEnd = buffer.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;
    const match = /Content-Length:\s*(\d+)/i.exec(buffer.slice(0, headerEnd).toString("utf8"));
    if (!match) process.exit(3);
    const start = headerEnd + 4;
    const length = Number(match[1]);
    if (buffer.length < start + length) return;
    const body = JSON.parse(buffer.slice(start, start + length).toString("utf8"));
    buffer = buffer.slice(start + length);
    handle(body);
  }
});
process.stdin.on("end", () => process.exit(0));
"#;

    /// A temporary workspace with the recording server in it.
    struct Rig {
        _temp: tempfile::TempDir,
        node: PathBuf,
        script: PathBuf,
        log: PathBuf,
        workspace: PathBuf,
    }

    fn rig() -> Option<Rig> {
        let Some(node) = crate::environment_tools::resolve_on_path("node") else {
            eprintln!("skipped: node is not on PATH");
            return None;
        };
        let temp = tempfile::tempdir().expect("temp dir");
        // Resolved where a temporary directory can sit behind a link (macOS),
        // but not on Windows, where canonical paths are `\\?\` verbatim
        // spellings that Node cannot open the server's log through.
        let workspace = if cfg!(windows) {
            temp.path().to_path_buf()
        } else {
            std::fs::canonicalize(temp.path()).expect("workspace resolves")
        };
        let script = workspace.join("recording-lsp.js");
        std::fs::write(&script, RECORDING_SERVER).expect("fake server written");
        let log = workspace.join("server.log");
        Some(Rig {
            _temp: temp,
            node,
            script,
            log,
            workspace,
        })
    }

    impl Rig {
        fn config(&self, env: &[(&str, &str)]) -> LspServerConfig {
            let mut config = test_config();
            config.command = self.node.to_string_lossy().into_owned();
            config.args = vec![self.script.to_string_lossy().into_owned()];
            config.startup_timeout_millis = 10_000;
            config.shutdown_timeout_millis = 1_000;
            config.env = env
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .chain([(
                    "FAKE_LSP_LOG".to_owned(),
                    self.log.to_string_lossy().into_owned(),
                )])
                .collect();
            config
        }

        fn root(&self) -> ServerRoot {
            ServerRoot::local(self.workspace.clone())
        }

        /// A file in the workspace, written with `text`.
        fn file(&self, name: &str, text: &str) -> PathBuf {
            let path = self.workspace.join(name);
            std::fs::write(&path, text).expect("file written");
            path
        }

        fn lines(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn starts(&self) -> usize {
            self.lines().iter().filter(|line| *line == "start").count()
        }

        /// The documents the servers were told to open, as `(uri, text)`.
        fn opened(&self) -> Vec<(String, String)> {
            self.lines()
                .iter()
                .filter_map(|line| line.strip_prefix("open "))
                .filter_map(|rest| rest.split_once(' '))
                .map(|(uri, text)| {
                    (
                        uri.to_owned(),
                        serde_json::from_str::<String>(text).expect("the recorded text is JSON"),
                    )
                })
                .collect()
        }

        fn state(&self, registry: &LspRegistry, config: &LspServerConfig) -> (u32, Option<String>, bool) {
            let key = ServerKey {
                root: self.root(),
                name: config.name.clone(),
            };
            let guard = registry.lock();
            let entry = guard.servers.get(&key).expect("an entry exists");
            (entry.crashes, entry.last_error.clone(), entry.starting.is_some())
        }
    }

    fn text_of(path: &Path) -> TextSource {
        let path = path.to_path_buf();
        Arc::new(move || std::fs::read_to_string(&path).ok())
    }

    fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    const PATIENCE: Duration = Duration::from_secs(20);

    /// The core of the feature: an edit brings the server up without waiting
    /// for it, and the file it opens is the file as it is when the server is
    /// ready, not as it was when the edit asked.
    #[test]
    fn an_edit_starts_the_server_in_the_background_and_opens_the_file_as_it_is_by_then() {
        let Some(rig) = rig() else { return };
        let config = rig.config(&[("FAKE_LSP_DELAY_MS", "400"), ("FAKE_LSP_PUBLISH", "1")]);
        let file = rig.file("a.rs", "fn a() {}\n");
        let registry = LspRegistry::default();

        let asked = Instant::now();
        let (task_config, root, task_file) = (config.clone(), rig.root(), file.clone());
        registry.in_background(move |registry| {
            registry.open_after_edit(
                &ServerHost::Local,
                &task_config,
                "rust",
                &root,
                &task_file,
                "c1",
                text_of(&task_file),
            );
        });
        assert!(
            asked.elapsed() < Duration::from_millis(300),
            "the caller does not wait for the server: {:?}",
            asked.elapsed()
        );
        assert!(!registry.has_running_servers(), "the server is not up yet");

        // The file changes again while the server is still coming up.
        std::fs::write(&file, "fn a() { 2 }\n").unwrap();
        wait_until("the file is opened", PATIENCE, || !rig.opened().is_empty());
        assert_eq!(
            rig.opened(),
            vec![(path_to_uri(&file), "fn a() { 2 }\n".to_owned())],
            "opened at the text on disk once the server was ready"
        );
        assert!(rig.lines().contains(&format!("save {}", path_to_uri(&file))));
        assert_eq!(rig.starts(), 1);

        // The conversation was registered before the start, so the first
        // publish of the freshly opened document is its own.
        let mut delivered = Vec::new();
        wait_until("the first diagnostics arrive", PATIENCE, || {
            delivered = registry.take_new_diagnostics("c1");
            !delivered.is_empty()
        });
        assert_eq!(delivered[0].1[0].message, "broken");
        registry.stop_all();
    }

    /// A second edit, a third, and a navigation call that all arrive while the
    /// server is starting join that start instead of each spawning a process.
    #[test]
    fn edits_and_a_navigation_call_during_a_start_share_one_process() {
        let Some(rig) = rig() else { return };
        let config = rig.config(&[("FAKE_LSP_DELAY_MS", "600")]);
        let registry = LspRegistry::default();
        let files: Vec<PathBuf> = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|name| rig.file(name, &format!("// {name}\n")))
            .collect();

        for file in &files[..2] {
            let (config, root, file) = (config.clone(), rig.root(), file.clone());
            registry.in_background(move |registry| {
                registry.open_after_edit(
                    &ServerHost::Local,
                    &config,
                    "rust",
                    &root,
                    &file,
                    "c1",
                    text_of(&file),
                );
            });
        }
        wait_until("a start is under way", PATIENCE, || {
            registry
                .lock()
                .servers
                .values()
                .any(|entry| entry.starting.is_some())
        });

        // A navigation call for the third file, and an edit of it, mid-start.
        let (config_for_edit, root, file) = (config.clone(), rig.root(), files[2].clone());
        registry.in_background(move |registry| {
            registry.open_after_edit(
                &ServerHost::Local,
                &config_for_edit,
                "rust",
                &root,
                &file,
                "c1",
                text_of(&file),
            );
        });
        let (connection, language_id, _) = registry
            .connection_for(&ServerHost::Local, &[config.clone()], &rig.workspace, &files[2])
            .expect("the call joins the start in flight");
        assert_eq!(language_id, "rust");
        assert!(connection.is_alive());

        wait_until("all three files are opened", PATIENCE, || rig.opened().len() >= 3);
        assert_eq!(rig.starts(), 1, "one process for all of them: {:?}", rig.lines());
        let mut opened: Vec<String> = rig.opened().into_iter().map(|(uri, _)| uri).collect();
        opened.sort();
        let mut expected: Vec<String> = files.iter().map(|file| path_to_uri(file)).collect();
        expected.sort();
        assert_eq!(opened, expected);
        registry.stop_all();
    }

    /// The old race: several calls reach the spawn at once and each starts a
    /// process, keeping one and stopping the rest. Now exactly one is started
    /// and everyone is handed it.
    #[test]
    fn simultaneous_callers_are_handed_one_connection() {
        let Some(rig) = rig() else { return };
        let config = rig.config(&[("FAKE_LSP_DELAY_MS", "300")]);
        let registry = LspRegistry::default();
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (registry, config, root, barrier) =
                    (registry.clone(), config.clone(), rig.root(), Arc::clone(&barrier));
                thread::spawn(move || {
                    barrier.wait();
                    registry
                        .ensure_started(&ServerHost::Local, &config, &root)
                        .expect("the server starts")
                })
            })
            .collect();
        let connections: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
        assert!(connections.windows(2).all(|pair| Arc::ptr_eq(&pair[0], &pair[1])));
        assert_eq!(rig.starts(), 1);
        registry.stop_all();
    }

    /// Edits are not only for servers that are down: a server that is up but
    /// does not hold the file yet is given it, while one that does hold it is
    /// left to the quick re-sync the edit hook already did.
    #[test]
    fn an_edit_gives_a_running_server_the_file_it_does_not_hold() {
        let Some(rig) = rig() else { return };
        let config = rig.config(&[]);
        let registry = LspRegistry::default();
        let held = rig.file("held.rs", "// held\n");
        let new = rig.file("new.rs", "// new\n");
        let (connection, _, _) = registry
            .connection_for(&ServerHost::Local, &[config.clone()], &rig.workspace, &held)
            .unwrap();
        connection.sync_document(&held, "rust", "// held\n").unwrap();

        for file in [&held, &new] {
            registry.open_after_edit(
                &ServerHost::Local,
                &config,
                "rust",
                &rig.root(),
                file,
                "c1",
                text_of(file),
            );
        }
        wait_until("the new file is opened", PATIENCE, || {
            rig.opened().iter().any(|(uri, _)| *uri == path_to_uri(&new))
        });
        let opens_of_held = rig
            .opened()
            .iter()
            .filter(|(uri, _)| *uri == path_to_uri(&held))
            .count();
        assert_eq!(opens_of_held, 1, "a document the server holds is not opened twice");
        assert!(
            !rig.lines().iter().any(|line| line.starts_with("change ")),
            "and not re-sent here either: {:?}",
            rig.lines()
        );
        assert_eq!(rig.starts(), 1);
        registry.stop_all();
    }

    /// `diagnostics: false` turns off only the reporting half: the document is
    /// opened all the same, and the conversation is not registered for what the
    /// server publishes.
    #[test]
    fn an_entry_without_diagnostics_still_opens_the_file_but_registers_nobody() {
        let Some(rig) = rig() else { return };
        let mut config = rig.config(&[("FAKE_LSP_PUBLISH", "1")]);
        config.diagnostics = false;
        let file = rig.file("a.rs", "fn a() {}\n");
        let registry = LspRegistry::default();
        registry.open_after_edit(
            &ServerHost::Local,
            &config,
            "rust",
            &rig.root(),
            &file,
            "c1",
            text_of(&file),
        );
        wait_until("the file is opened", PATIENCE, || rig.opened().len() == 1);
        assert!(!registry.tracks_conversation("c1"), "nothing to be owed");
        assert!(registry.take_new_diagnostics("c1").is_empty());
        registry.stop_all();
    }

    /// A start that fails is not held against the entry: the next use tries
    /// again, however many times it has failed, and it is never mistaken for a
    /// crash.
    #[test]
    fn a_start_that_fails_is_retried_and_never_counted_as_a_crash() {
        let Some(rig) = rig() else { return };
        let mut config = rig.config(&[]);
        config.command = "definitely-not-a-language-server-xyz".into();
        config.max_restarts = 1;
        let registry = LspRegistry::default();
        for attempt in 0..6 {
            let error = registry
                .ensure_started(&ServerHost::Local, &config, &rig.root())
                .err()
                .expect("a missing command does not start");
            assert!(error.contains("Could not start the language server"), "attempt {attempt}: {error}");
            assert!(!error.contains("exceeded"), "attempt {attempt}: {error}");
        }
        let (crashes, last_error, starting) = rig.state(&registry, &config);
        assert_eq!(crashes, 0, "a command that is not there never ran, so it never crashed");
        assert!(last_error.is_some(), "the entry is in the error state");
        assert!(!starting);

        // Once the command is there the very next use starts it.
        let mut fixed = rig.config(&[]);
        fixed.max_restarts = 1;
        assert!(registry.ensure_started(&ServerHost::Local, &fixed, &rig.root()).is_ok());
        registry.stop_all();
    }

    /// An `initialize` the server refuses is a failed start, not a crash, and
    /// the next use gets a fresh try — which succeeds and clears the error.
    #[test]
    fn a_refused_initialize_is_retried_on_the_next_use() {
        let Some(rig) = rig() else { return };
        let marker = rig.workspace.join("marker");
        let config = rig.config(&[
            ("FAKE_LSP_MODE", "reject-first-initialize"),
            ("FAKE_LSP_MARKER", &marker.to_string_lossy()),
        ]);
        let registry = LspRegistry::default();
        let error = registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .err()
            .expect("the first initialize is refused");
        assert!(error.contains("failed to initialize"), "{error}");
        let (crashes, last_error, _) = rig.state(&registry, &config);
        assert_eq!(crashes, 0);
        assert!(last_error.is_some_and(|reason| reason.contains("refused")));

        registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .expect("the second try is made, and works");
        let (crashes, last_error, _) = rig.state(&registry, &config);
        assert_eq!((crashes, last_error), (0, None));
        assert_eq!(rig.starts(), 2);
        registry.stop_all();
    }

    /// A process that dies while starting is a crash, counted; the count is of
    /// consecutive crashes; once it exceeds `maxRestarts` nothing more is
    /// tried — until the entry is changed, which starts the count over.
    #[test]
    fn consecutive_crashes_at_start_up_use_up_the_budget_and_an_entry_change_restores_it() {
        let Some(rig) = rig() else { return };
        let mut config = rig.config(&[("FAKE_LSP_MODE", "crash-on-start")]);
        config.max_restarts = 2;
        let registry = LspRegistry::default();

        for attempt in 1..=3 {
            let error = registry
                .ensure_started(&ServerHost::Local, &config, &rig.root())
                .err()
                .expect("the server crashes before it answers");
            assert!(!error.contains("exceeded"), "attempt {attempt}: {error}");
            assert_eq!(rig.state(&registry, &config).0, attempt, "crashes after attempt {attempt}");
        }
        assert_eq!(rig.starts(), 3);

        for _ in 0..2 {
            let error = registry
                .ensure_started(&ServerHost::Local, &config, &rig.root())
                .err()
                .expect("the budget is spent");
            assert!(
                error.contains("exceeded max crash recovery attempts (2)"),
                "{error}"
            );
        }
        assert_eq!(rig.starts(), 3, "nothing is spawned once the budget is spent");

        // `lsp.json` changed: the count starts over and the server is tried.
        let mut changed = config.clone();
        changed.env.insert("EXTRA".into(), "1".into());
        let error = registry
            .ensure_started(&ServerHost::Local, &changed, &rig.root())
            .err()
            .expect("it still crashes");
        assert!(!error.contains("exceeded"), "{error}");
        assert_eq!(rig.starts(), 4);
        assert_eq!(rig.state(&registry, &changed).0, 1);
    }

    /// A start that completes the handshake ends the run of crashes.
    #[test]
    fn a_successful_start_clears_the_crash_count() {
        let Some(rig) = rig() else { return };
        let marker = rig.workspace.join("marker");
        let config = rig.config(&[
            ("FAKE_LSP_MODE", "crash-first-start"),
            ("FAKE_LSP_MARKER", &marker.to_string_lossy()),
        ]);
        let registry = LspRegistry::default();
        registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .err()
            .expect("the first start crashes");
        assert_eq!(rig.state(&registry, &config).0, 1);
        registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .expect("the second start works");
        assert_eq!(rig.state(&registry, &config).0, 0, "consecutive, so a good start resets it");
        registry.stop_all();
    }

    /// Dying after a good start is a crash too, found the next time the server
    /// is needed. With `restartOnCrash: false` that is the end of it; with it
    /// on, the server is brought back.
    #[test]
    fn a_server_that_died_is_restarted_only_if_the_entry_allows_it() {
        let Some(rig) = rig() else { return };
        let mut config = rig.config(&[("FAKE_LSP_MODE", "die-on-hover")]);
        config.restart_on_crash = false;
        let registry = LspRegistry::default();
        let hover = || {
            json!({
                "textDocument": { "uri": "file:///x.rs" },
                "position": { "line": 0, "character": 0 },
            })
        };

        let connection = registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .expect("a first start does not depend on restartOnCrash");
        let _ = connection.request_with_retry("textDocument/hover", hover());
        wait_until("the server is seen to have died", PATIENCE, || !connection.is_alive());
        let error = registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .err()
            .expect("restartOnCrash is false");
        assert!(error.contains("restartOnCrash is false"), "{error}");
        assert_eq!(rig.starts(), 1, "no second process");
        assert_eq!(rig.state(&registry, &config).0, 1);

        // The same death under an entry that allows a restart.
        let mut allowed = rig.config(&[("FAKE_LSP_MODE", "die-on-hover")]);
        allowed.restart_on_crash = true;
        let registry = LspRegistry::default();
        let first = registry
            .ensure_started(&ServerHost::Local, &allowed, &rig.root())
            .unwrap();
        let _ = first.request_with_retry("textDocument/hover", hover());
        wait_until("the server is seen to have died", PATIENCE, || !first.is_alive());
        let second = registry
            .ensure_started(&ServerHost::Local, &allowed, &rig.root())
            .expect("the server is brought back");
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(rig.state(&registry, &allowed).0, 0, "and the good start cleared the count");
        registry.stop_all();
    }

    /// A server that exits with status 0 chose to stop. That is not a crash,
    /// so it neither counts nor trips `restartOnCrash: false`.
    #[test]
    fn a_clean_exit_is_not_a_crash() {
        let Some(rig) = rig() else { return };
        let mut config = rig.config(&[("FAKE_LSP_MODE", "exit-clean-on-hover")]);
        config.restart_on_crash = false;
        let registry = LspRegistry::default();
        let first = registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .unwrap();
        let _ = first.request_with_retry(
            "textDocument/hover",
            json!({ "textDocument": { "uri": "file:///x.rs" }, "position": { "line": 0, "character": 0 } }),
        );
        wait_until("the server is seen to have exited", PATIENCE, || !first.is_alive());
        let second = registry
            .ensure_started(&ServerHost::Local, &config, &rig.root())
            .expect("a clean exit is simply started again");
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(rig.state(&registry, &config).0, 0);
        registry.stop_all();
    }

    /// A start that failed after one edit is simply tried again by the next
    /// one: nothing about the failure is remembered against the entry.
    #[test]
    fn a_failed_start_after_an_edit_is_retried_by_the_next_edit() {
        let Some(rig) = rig() else { return };
        let marker = rig.workspace.join("marker");
        let config = rig.config(&[
            ("FAKE_LSP_MODE", "reject-first-initialize"),
            ("FAKE_LSP_MARKER", &marker.to_string_lossy()),
            ("FAKE_LSP_DELAY_MS", "0"),
        ]);
        let file = rig.file("a.rs", "fn a() {}\n");
        let registry = LspRegistry::default();

        // Edit 1 fails its start (the first initialize is refused).
        registry.open_after_edit(
            &ServerHost::Local,
            &config,
            "rust",
            &rig.root(),
            &file,
            "c1",
            text_of(&file),
        );
        assert!(!registry.has_running_servers());
        let (crashes, last_error, starting) = rig.state(&registry, &config);
        assert_eq!((crashes, last_error.is_some(), starting), (0, true, false));

        // Edit 2 tries again, and this time it works and opens the file.
        registry.open_after_edit(
            &ServerHost::Local,
            &config,
            "rust",
            &rig.root(),
            &file,
            "c1",
            text_of(&file),
        );
        assert!(registry.has_running_servers());
        wait_until("the file is opened", PATIENCE, || rig.opened().len() == 1);
        assert_eq!(rig.starts(), 2);
        registry.stop_all();
    }

    /// The pool of background tasks is bounded, and a slot is given back
    /// whether the task returns or panics.
    #[test]
    fn background_tasks_are_bounded_and_always_release_their_slot() {
        let registry = LspRegistry::default();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..MAX_BACKGROUND_TASKS {
            let (release, ran) = (Arc::clone(&release), Arc::clone(&ran));
            registry.in_background(move |_| {
                ran.fetch_add(1, Ordering::SeqCst);
                let (lock, signal) = &*release;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = signal.wait(open).unwrap();
                }
            });
        }
        wait_until("every task is running", PATIENCE, || {
            ran.load(Ordering::SeqCst) == MAX_BACKGROUND_TASKS
        });
        let overflow = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&overflow);
        registry.in_background(move |_| flag.store(true, Ordering::SeqCst));
        assert_eq!(
            registry.background.load(Ordering::SeqCst),
            MAX_BACKGROUND_TASKS,
            "the refused task did not take a slot"
        );
        {
            let (lock, signal) = &*release;
            *lock.lock().unwrap() = true;
            signal.notify_all();
        }
        wait_until("the slots are given back", PATIENCE, || {
            registry.background.load(Ordering::SeqCst) == 0
        });
        assert!(!overflow.load(Ordering::SeqCst), "the task past the bound never ran");

        registry.in_background(|_| panic!("a task that fails"));
        wait_until("a panicking task gives its slot back", PATIENCE, || {
            registry.background.load(Ordering::SeqCst) == 0
        });
    }

    /// `stop_all` while a server is still starting: the process that comes up
    /// afterwards has nowhere to live and is stopped, not leaked.
    #[test]
    fn a_server_that_finishes_starting_after_stop_all_is_stopped() {
        let Some(rig) = rig() else { return };
        let config = rig.config(&[("FAKE_LSP_DELAY_MS", "500")]);
        let file = rig.file("a.rs", "fn a() {}\n");
        let registry = LspRegistry::default();
        let (task_config, root, task_file) = (config.clone(), rig.root(), file.clone());
        registry.in_background(move |registry| {
            registry.open_after_edit(
                &ServerHost::Local,
                &task_config,
                "rust",
                &root,
                &task_file,
                "c1",
                text_of(&task_file),
            );
        });
        wait_until("the start is under way", PATIENCE, || {
            registry
                .lock()
                .servers
                .values()
                .any(|entry| entry.starting.is_some())
        });
        registry.stop_all();
        wait_until("the task is over", PATIENCE, || {
            registry.background.load(Ordering::SeqCst) == 0
        });
        assert!(!registry.has_running_servers());
        assert!(rig.opened().is_empty(), "nothing was opened on a server nobody owns");
    }

    /// The local edit path, end to end through [`crate::lsp::open_after_local_edit`]:
    /// a file nothing claims starts nothing; a refused gate starts nothing; an
    /// allowed one starts the server and opens the file.
    #[test]
    fn the_local_edit_path_starts_a_server_only_for_a_claimed_file_and_an_open_gate() {
        let Some(rig) = rig() else { return };
        let config = rig.config(&[]);
        let registry = LspRegistry::default();
        let file = rig.file("a.rs", "fn a() {}\n");
        let notes = rig.file("notes.md", "# notes\n");
        let asked = std::cell::Cell::new(0);
        let allow = || {
            asked.set(asked.get() + 1);
            true
        };
        let deny = || false;
        let configs = [config];

        crate::lsp::open_after_local_edit(&registry, &configs, &rig.workspace, &notes, "c1", &allow);
        assert_eq!(asked.get(), 0, "no server claims .md, so nobody is asked");
        crate::lsp::open_after_local_edit(&registry, &configs, &rig.workspace, &file, "c1", &deny);
        assert_eq!(rig.starts(), 0, "a refusing gate starts nothing");
        assert!(!registry.tracks_conversation("c1"));

        crate::lsp::open_after_local_edit(&registry, &configs, &rig.workspace, &file, "c1", &allow);
        assert_eq!(asked.get(), 1);
        wait_until("the file is opened", PATIENCE, || !rig.opened().is_empty());
        assert_eq!(rig.opened(), vec![(path_to_uri(&file), "fn a() {}\n".to_owned())]);
        assert!(registry.tracks_conversation("c1"));
        registry.stop_all();
    }
}
