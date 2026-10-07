//! The instruction files of a workspace on another machine.
//!
//! A workspace on a WSL distribution or an SSH machine reads its instructions
//! the way a local folder does: the `MEWRK.md`, `.mewrk/MEWRK.md`,
//! `.mewrk/rules/` and `MEWRK.local.md` of every folder from the Git
//! repository's top level down to the workspace, what they import, and the
//! files of a subfolder once the model reads something inside it. Those rules
//! live in [`crate::project_memory`], a loader hardened against a local
//! filesystem — verified opens, canonical identities, an import graph. Rather
//! than teach all of that a second filesystem, this module reproduces on this
//! computer exactly the files the loader would read on the machine, a
//! *mirror*, and runs the loader on it unchanged.
//!
//! The mirror holds a machine's files at the place their paths name: the
//! machine's `/home/u/repo/MEWRK.md` is `<tree>/home/u/repo/MEWRK.md`, a
//! Windows machine's `C:/src/app` is `<tree>/C/src/app`. A symbolic link there
//! is a symbolic link here, to the mirrored place of what it resolves to, so
//! canonical paths, labels and deduplication come out as they would on the
//! machine — and so does which files an `@` line reached, which the
//! conversation treats as files of its workspaces
//! ([`crate::workspace_set::InstructionImports`]). A link this computer cannot
//! create is left out and told as a skipped file — never replaced by a copy of
//! its target, which would put the target somewhere it is not.
//!
//! The files come from scripts run through the machine's own shell, in the
//! POSIX dialect here and the PowerShell dialect in
//! [`crate::remote_powershell::instruction_probe`]: one probe for the folders a
//! run starts with, the same probe for the subfolders a `read` reaches, and a
//! fetch of exact import paths. Each answers in counted records, because a
//! file may hold any bytes. An answer says what is there and, through its
//! scopes, what is no longer there, so each run brings exactly the files it
//! reads up to date — in place, one rename per file, so a run reading the same
//! mirror never sees a file missing that the machine still has. Nothing is
//! cleared wholesale: another conversation on the same machine may be reading
//! the mirror at that moment.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs, io,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    cancel::CancelSignal,
    project_memory::{self, ProjectMemoryOptions, ProjectMemoryReport, SkipReason},
    remote_files::{RemoteShell, SHELL_HELPERS},
    run_environment::{self, quote_remote_path, sh_single_quote, ShellRunner},
    shell_backend::ScriptDialect,
    workspace_set::ResolvedWorkspace,
};

/// The first line of every answer, in either dialect.
pub(crate) const HEADER: &str = "mewrk-instructions 1";

/// How long a run waits for one answer from the machine. The run cannot
/// start without its instructions, and the machine is its workspace's.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// How deep a rules folder is walked. The loader itself has no depth limit,
/// only an entry budget; this keeps a link-free but absurdly deep tree from
/// costing the answer more than the entries would.
const MAX_RULE_DEPTH: usize = 32;

/// The most file bytes one answer carries. Past it a file is reported by size
/// alone and reproduced as too large: the loader stops at 1 MiB of
/// instructions in all, so a run never needs more than this.
const MAX_ANSWER_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// The most import paths one fetch asks for.
const MAX_FETCH_PATHS: usize = 512;

/// The folder inside a machine's mirror directory that stands for its
/// filesystem root, beside the file naming the machine.
const TREE: &str = "tree";
const MACHINE_FILE: &str = "machine.json";

/// How a machine spells its paths: `/home/u` on POSIX, `C:/Users/u` on a
/// Windows machine whose scripts run in PowerShell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathFlavour {
    Posix,
    Windows,
}

impl PathFlavour {
    fn of(dialect: ScriptDialect) -> Self {
        match dialect {
            ScriptDialect::Posix => Self::Posix,
            ScriptDialect::PowerShell => Self::Windows,
        }
    }
}

/// Where one machine's instruction files are reproduced on this computer,
/// and how its paths map onto that place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMirror {
    /// Canonical, so the loader's canonical identities start with it.
    tree: PathBuf,
    flavour: PathFlavour,
}

impl RemoteMirror {
    pub(crate) fn new(tree: PathBuf, flavour: PathFlavour) -> Self {
        Self { tree, flavour }
    }

    /// Whether `path` is inside the reproduction.
    pub(crate) fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.tree)
    }

    /// The place on this computer of `remote`, an absolute path on the
    /// machine, normalized the way the machine reads it. `None` for a path
    /// that is not absolute there, or holds a name this computer's filesystem
    /// cannot carry.
    pub(crate) fn local_path(&self, remote: &str) -> Option<PathBuf> {
        let mut path = self.tree.clone();
        for component in remote_components(remote, self.flavour)? {
            if !host_name_is_valid(&component) {
                return None;
            }
            path.push(component);
        }
        Some(path)
    }

    /// The machine's path for a place inside the reproduction.
    pub(crate) fn remote_path(&self, local: &Path) -> Option<String> {
        let components = local
            .strip_prefix(&self.tree)
            .ok()?
            .components()
            .map(|component| match component {
                Component::Normal(name) => name.to_str().map(str::to_owned),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        join_remote(&components, self.flavour)
    }

    /// Where `import`, written in the reproduced file `source`, leads: by the
    /// machine's rules, absolute paths included, and never out of the
    /// reproduction — `..` stops at the machine's root as it does there.
    pub(crate) fn resolve_import(&self, source: &Path, import: &str) -> Option<PathBuf> {
        let source = self.remote_path(source)?;
        let import = match self.flavour {
            PathFlavour::Posix => import.to_owned(),
            PathFlavour::Windows => import.replace('\\', "/"),
        };
        let joined = match self.flavour {
            PathFlavour::Posix if import.starts_with('/') => import,
            PathFlavour::Windows if import.starts_with("//") || has_drive(&import) => import,
            // Rooted but driveless: the root of the drive the source is on.
            PathFlavour::Windows if import.starts_with('/') => {
                format!("{}{import}", source.get(..2)?)
            }
            _ => {
                let parent = source.rsplit_once('/').map_or("", |(parent, _)| parent);
                format!("{parent}/{import}")
            }
        };
        self.local_path(&joined)
    }
}

fn has_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// A remote absolute path's components, `.` and `..` resolved lexically and
/// never above the root. A Windows path starts with its drive letter
/// (upper-cased) or, for a share, `UNC`, the server and the share.
fn remote_components(remote: &str, flavour: PathFlavour) -> Option<Vec<String>> {
    let (mut components, fixed, rest) = match flavour {
        PathFlavour::Posix => (Vec::new(), 0, remote.strip_prefix('/')?.to_owned()),
        PathFlavour::Windows => {
            let path = remote.replace('\\', "/");
            if has_drive(&path) {
                let drive = path[..1].to_ascii_uppercase();
                (vec![drive], 1, path[2..].to_owned())
            } else {
                let share = path.strip_prefix("//")?;
                let mut parts = share.splitn(3, '/');
                let server = parts.next().filter(|part| !part.is_empty())?;
                let name = parts.next().filter(|part| !part.is_empty())?;
                let rest = parts.next().unwrap_or_default().to_owned();
                (vec!["UNC".to_owned(), server.to_owned(), name.to_owned()], 3, rest)
            }
        }
    };
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if components.len() > fixed {
                    components.pop();
                }
            }
            name => components.push(name.to_owned()),
        }
    }
    Some(components)
}

/// The inverse of [`remote_components`].
fn join_remote(components: &[String], flavour: PathFlavour) -> Option<String> {
    match flavour {
        PathFlavour::Posix => Some(format!("/{}", components.join("/"))),
        PathFlavour::Windows => match components.first().map(String::as_str) {
            Some("UNC") if components.len() >= 3 => Some(format!("//{}", components[1..].join("/"))),
            Some(drive) if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() => {
                Some(format!("{drive}:/{}", components[1..].join("/")))
            }
            _ => None,
        },
    }
}

/// Whether this computer's filesystem can hold a file of this name exactly.
fn host_name_is_valid(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return false;
    }
    if cfg!(windows) {
        if name.ends_with(['.', ' '])
            || name.chars().any(|character| {
                character.is_control()
                    || matches!(character, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
            })
        {
            return false;
        }
        let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0');
        if device {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Where the mirrors live
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    static TEST_MIRROR_BASE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Every machine's mirror sits in its own folder under this one, in the
/// user's cache directory: it is a copy that every run rebuilds what it reads
/// of, and losing it costs only the next run a little time.
fn mirror_base() -> PathBuf {
    #[cfg(test)]
    if let Some(base) = TEST_MIRROR_BASE.with(|base| base.borrow().clone()) {
        return base;
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mewrk")
        .join("remote-instructions")
}

/// Points this thread's mirrors at `base` until the guard drops.
#[cfg(test)]
pub(crate) fn use_mirror_base(base: &Path) -> MirrorBaseGuard {
    TEST_MIRROR_BASE.with(|slot| *slot.borrow_mut() = Some(base.to_path_buf()));
    MirrorBaseGuard
}

#[cfg(test)]
pub(crate) struct MirrorBaseGuard;

/// A machine's mirror, opened as a run opens it.
#[cfg(test)]
pub(crate) fn open_test_mirror(machine: &str, label: &str, flavour: PathFlavour) -> RemoteMirror {
    open_mirror(machine, label, flavour).expect("the mirror opens")
}

#[cfg(test)]
impl Drop for MirrorBaseGuard {
    fn drop(&mut self) {
        TEST_MIRROR_BASE.with(|slot| *slot.borrow_mut() = None);
    }
}

/// A machine's folder name: readable, and kept apart from every other
/// machine's by a digest of its identity.
fn machine_directory_name(machine: &str) -> String {
    let readable = machine
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect::<String>();
    let digest = Sha256::digest(machine.as_bytes())
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{readable}-{digest}")
}

/// What the mirror folder records about its machine, for showing a mirrored
/// path as the machine's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MachineRecord {
    label: String,
    flavour: PathFlavour,
}

/// Opens (creating as needed) the mirror of `machine`.
fn open_mirror(machine: &str, label: &str, flavour: PathFlavour) -> io::Result<RemoteMirror> {
    let directory = mirror_base().join(machine_directory_name(machine));
    let tree = directory.join(TREE);
    fs::create_dir_all(&tree)?;
    let record = serde_json::to_vec(&MachineRecord {
        label: label.to_owned(),
        flavour,
    })
    .map_err(io::Error::other)?;
    let path = directory.join(MACHINE_FILE);
    if fs::read(&path).ok().as_deref() != Some(record.as_slice()) {
        write_file(&path, &record)?;
    }
    Ok(RemoteMirror::new(fs::canonicalize(&tree)?, flavour))
}

/// A path inside some machine's mirror, as that machine names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MirroredPath {
    /// The machine, as people know it ([`crate::remote_capabilities::machine_label`]).
    pub label: String,
    pub flavour: PathFlavour,
    /// The path's components on the machine: for a Windows machine the first
    /// is the drive letter, or `UNC` followed by the server and the share.
    pub components: Vec<String>,
}

impl MirroredPath {
    /// The path as the machine spells it, without the machine.
    pub(crate) fn path(&self) -> Option<String> {
        join_remote(&self.components, self.flavour)
    }

    /// `machine:path`, with the machine's own spelling of the path.
    pub(crate) fn display(&self, components: &[String]) -> String {
        let path = join_remote(components, self.flavour)
            .unwrap_or_else(|| components.join("/"));
        format!("{}:{path}", self.label)
    }
}

/// Which machine's file `path` is a reproduction of, if it is one. Anything a
/// person sees about an instruction file of a remote workspace goes through
/// here, so it names the machine and its path rather than this computer's
/// cache.
pub(crate) fn locate(path: &Path) -> Option<MirroredPath> {
    let base = mirror_base();
    let base = fs::canonicalize(&base).unwrap_or(base);
    let mut components = path.strip_prefix(&base).ok()?.components();
    let Some(Component::Normal(machine)) = components.next() else {
        return None;
    };
    if components.next() != Some(Component::Normal(TREE.as_ref())) {
        return None;
    }
    let record: MachineRecord =
        serde_json::from_slice(&fs::read(base.join(machine).join(MACHINE_FILE)).ok()?).ok()?;
    let components = components
        .map(|component| match component {
            Component::Normal(name) => name.to_str().map(str::to_owned),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(MirroredPath {
        label: record.label,
        flavour: record.flavour,
        components,
    })
}

/// One materialization of a mirror at a time in this process, per machine:
/// two runs bringing the same files up to date would otherwise trip over each
/// other's half-written entries. Reading the mirror needs no lock.
fn mirror_lock(tree: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(tree.to_path_buf())
        .or_default()
        .clone()
}

// ---------------------------------------------------------------------------
// One run's instructions on a machine
// ---------------------------------------------------------------------------

/// A workspace on another machine, as reading its instructions needs it.
#[derive(Clone)]
pub(crate) struct RemotePlace {
    /// The machine's transport. A trait object so the tests can stand a local
    /// shell in for the machine.
    pub shell: Arc<dyn RemoteShell + Send + Sync>,
    /// `run_environment::env_key` of the machine: which mirror, and which read
    /// records belong to it.
    pub machine: String,
    pub label: String,
    /// The workspace root, spelled as the workspace records it.
    pub root: String,
    /// For a run in the conversation's worktree, the registered folder it was
    /// checked out from, whose personal `MEWRK.local.md` the run also reads.
    pub project_folder: Option<String>,
}

impl RemotePlace {
    pub(crate) fn of_workspace(workspace: &ResolvedWorkspace) -> Self {
        Self {
            shell: Arc::new(workspace.runner.clone()),
            machine: run_environment::env_key(workspace.machine.as_ref()),
            label: crate::remote_capabilities::machine_label(&workspace.runner),
            root: workspace.root.clone(),
            project_folder: workspace
                .is_worktree
                .then(|| workspace.env_path.clone())
                .filter(|folder| folder != &workspace.root),
        }
    }

    fn flavour(&self) -> PathFlavour {
        PathFlavour::of(self.shell.dialect())
    }

    fn unreadable(&self, reason: &str) -> String {
        crate::ui_text::ui_text!(
            "无法读取 {} 上工作区 {} 的指令文件：{reason}",
            "Could not read the instruction files of workspace {} on {}: {reason}",
            self.label,
            self.root
        )
    }
}

/// The bounds one run's answers keep to, from the loader's own limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The loader's per-file limit; a larger file is reported by size alone.
    pub max_file: u64,
    /// File bytes one answer may carry in all.
    pub budget: u64,
    /// Rules-folder entries one answer may list.
    pub entries: usize,
    pub depth: usize,
}

impl Limits {
    fn of(options: &ProjectMemoryOptions) -> Self {
        Self {
            max_file: options.limits.max_file_bytes as u64,
            budget: MAX_ANSWER_FILE_BYTES,
            entries: project_memory::rule_scan_limit(&options.limits),
            depth: MAX_RULE_DEPTH,
        }
    }
}

/// What one script is asked.
pub(crate) enum Job<'a> {
    /// Enter the workspace root, find the repository's top level above it,
    /// and report every folder from there down to the root, then the project
    /// folder's `MEWRK.local.md`.
    Startup {
        root: &'a str,
        project_folder: Option<&'a str>,
    },
    /// Report these canonical folders, in order, stopping at the first that
    /// is no longer a folder by that canonical name.
    Folders(&'a [String]),
    /// Report these exact absolute paths, and every link along each.
    Paths(&'a [String]),
}

/// One run's instruction files on a machine: where they are reproduced, and
/// what this run has brought over so far.
pub(crate) struct RemoteInstructions {
    place: RemotePlace,
    mirror: RemoteMirror,
    limits: Limits,
    /// The workspace root's canonical path on the machine.
    root: String,
    /// Import paths brought up to date this run.
    fetched: HashSet<String>,
    /// Folders whose candidates were brought up to date this run.
    scanned: HashSet<String>,
    /// Files the mirror could not hold, not yet told.
    skipped: Vec<(PathBuf, SkipReason)>,
}

impl RemoteInstructions {
    /// Brings over the files a run starts with — every candidate from the
    /// repository's top level down to the root, the project folder's local
    /// file, and everything they import — and points `options` at their
    /// reproduction. Fails, naming the machine and the workspace, when the
    /// machine cannot be read: a run without its instructions would look like
    /// one whose project has none.
    pub(crate) fn prepare(
        place: RemotePlace,
        options: &mut ProjectMemoryOptions,
    ) -> Result<Self, String> {
        check_operand(&place.root).map_err(|reason| place.unreadable(&reason))?;
        if let Some(folder) = &place.project_folder {
            check_operand(folder).map_err(|reason| place.unreadable(&reason))?;
        }
        let mirror = open_mirror(&place.machine, &place.label, place.flavour())
            .map_err(|error| place.unreadable(&error.to_string()))?;
        let limits = Limits::of(options);
        let answer = probe(
            &place,
            &Job::Startup {
                root: &place.root,
                project_folder: place.project_folder.as_deref(),
            },
            &limits,
        )?;
        let (Some(root), Some(top)) = (answer.root.clone(), answer.top.clone()) else {
            return Err(place.unreadable(&incomplete()));
        };
        let mut this = Self {
            place,
            mirror,
            limits,
            root,
            fetched: HashSet::new(),
            scanned: HashSet::new(),
            skipped: Vec::new(),
        };
        this.apply(&answer.records)?;
        let reproduced = |remote: &str| {
            this.mirror.local_path(remote).ok_or_else(|| {
                this.place.unreadable(&format!("{remote} cannot be named on this computer"))
            })
        };
        options.workspace_root = reproduced(&this.root)?;
        options.ancestor_floor = Some(reproduced(&top)?);
        options.project_folder = answer
            .project
            .as_deref()
            .and_then(|folder| this.mirror.local_path(folder));
        options.remote = Some(this.mirror.clone());
        this.scanned.extend(folders_between(&top, &this.root, this.mirror.flavour));
        this.fetch_imports(options, project_memory::discover_project_memory)?;
        Ok(this)
    }

    /// The reproduced place of a file the model just read on the machine,
    /// after bringing over the folders between the workspace root and it, so
    /// the loader's nested discovery and path-scoped rules can follow the read
    /// as they do a local one. `None` for a read on another machine, or of a
    /// file outside the workspace.
    pub(crate) fn load_for_read(
        &mut self,
        options: &ProjectMemoryOptions,
        report: &ProjectMemoryReport,
        machine: &str,
        canonical: &str,
    ) -> Result<Option<PathBuf>, String> {
        if machine != self.place.machine {
            return Ok(None);
        }
        let Some(relative) = strip_root(canonical, &self.root, self.mirror.flavour) else {
            return Ok(None);
        };
        let Some(identity) = self.mirror.local_path(canonical) else {
            return Ok(None);
        };
        let mut folders = Vec::new();
        let mut current = self.root.clone();
        let names = relative
            .split('/')
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        for name in names.iter().take(names.len().saturating_sub(1)) {
            current = join_child(&current, name);
            if !self.scanned.contains(&current) {
                folders.push(current.clone());
            }
        }
        if !folders.is_empty() {
            let answer = probe(&self.place, &Job::Folders(&folders), &self.limits)?;
            self.apply(&answer.records)?;
            self.scanned.extend(folders);
            self.fetch_imports(options, |options| {
                let mut report = report.clone();
                project_memory::discover_nested_project_memory_for_mirror(
                    options,
                    &mut report,
                    &identity,
                );
                report
            })?;
        }
        Ok(Some(identity))
    }

    /// The files the mirror could not hold since the last call, labelled the
    /// way the loader labels its own skips.
    pub(crate) fn take_skipped(
        &mut self,
        options: &ProjectMemoryOptions,
    ) -> Vec<project_memory::SkippedInstructionFile> {
        self.skipped
            .drain(..)
            .map(|(path, reason)| project_memory::skipped_file(options, &path, reason))
            .collect()
    }

    /// Brings every import of the reproduced files up to date, following
    /// imports of imports as deep as the loader would.
    ///
    /// `discover` runs the loader on what is reproduced so far. Its report is
    /// used only to learn which files are imported; the run's own discovery
    /// comes after. An import already brought over this run is not asked for
    /// again.
    fn fetch_imports(
        &mut self,
        options: &ProjectMemoryOptions,
        discover: impl Fn(&ProjectMemoryOptions) -> ProjectMemoryReport,
    ) -> Result<(), String> {
        for _ in 0..=project_memory::MAX_IMPORT_HOPS {
            let report = discover(options);
            let wanted = project_memory::import_targets(&report, options)
                .into_iter()
                .filter_map(|target| self.mirror.remote_path(&target))
                .filter(|remote| !self.fetched.contains(remote))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .take(MAX_FETCH_PATHS)
                .collect::<Vec<_>>();
            if wanted.is_empty() {
                return Ok(());
            }
            for path in &wanted {
                check_operand(path).map_err(|reason| self.place.unreadable(&reason))?;
            }
            let answer = probe(&self.place, &Job::Paths(&wanted), &self.limits)?;
            self.fetched.extend(wanted);
            self.apply(&answer.records)?;
        }
        Ok(())
    }

    fn apply(&mut self, records: &[Record]) -> Result<(), String> {
        let lock = mirror_lock(&self.mirror.tree);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let skipped = materialize(&self.mirror, records, self.limits.max_file)
            .map_err(|error| self.place.unreadable(&error))?;
        self.skipped.extend(skipped);
        Ok(())
    }
}

/// `path` relative to `root` on the machine, or `None` when it lies outside.
fn strip_root<'a>(path: &'a str, root: &str, flavour: PathFlavour) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    let head = path.get(..root.len())?;
    let same = match flavour {
        PathFlavour::Posix => head == root,
        PathFlavour::Windows => head.eq_ignore_ascii_case(root),
    };
    if !same {
        return None;
    }
    let rest = &path[root.len()..];
    (rest.is_empty() || rest.starts_with('/')).then_some(rest)
}

fn join_child(directory: &str, name: &str) -> String {
    if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

/// The folders from `top` down to `root`, both included.
fn folders_between(top: &str, root: &str, flavour: PathFlavour) -> Vec<String> {
    let mut folders = vec![top.to_owned()];
    let Some(relative) = strip_root(root, top, flavour) else {
        return folders;
    };
    let mut current = top.to_owned();
    for name in relative.split('/').filter(|name| !name.is_empty()) {
        current = join_child(&current, name);
        folders.push(current.clone());
    }
    folders
}

/// An operand that goes into a script. Quoting makes it inert; this keeps
/// out what quoting cannot carry.
fn check_operand(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.chars().any(char::is_control) {
        return Err("a path is empty or contains control characters".to_owned());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The scripts
// ---------------------------------------------------------------------------

/// Runs one job on the machine and reads its answer, asking once more when
/// the answer comes back cut short — a file that changed size between being
/// measured and being sent.
fn probe(place: &RemotePlace, job: &Job<'_>, limits: &Limits) -> Result<Answer, String> {
    let script = match place.shell.dialect() {
        ScriptDialect::Posix => posix_script(job, limits),
        ScriptDialect::PowerShell => crate::remote_powershell::instruction_probe(job, limits),
    };
    let mut failure = incomplete();
    for _ in 0..2 {
        let output = place
            .shell
            .run(&script, None, RUN_TIMEOUT, &CancelSignal::default())
            .map_err(|error| place.unreadable(&error))?;
        if output.status != Some(0) {
            let reason = run_environment::legible_remote_reply(&output.stderr)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("exit {:?}", output.status));
            return Err(place.unreadable(&reason));
        }
        match parse(&output.stdout) {
            Ok(answer) => return Ok(answer),
            Err(error) => failure = error,
        }
    }
    Err(place.unreadable(&failure))
}

/// The shell functions every POSIX job shares.
///
/// `item PATH KIND` reports one candidate: `S KIND PATH` first — this path's
/// reproduction is to be exactly what follows — then, for a link, `L` with
/// its fully resolved target and the target reported in turn, else what the
/// path holds. The kinds are `f` (an instruction file), `d` (a `.mewrk`
/// folder: its `MEWRK.md` and its `rules`) and `t` (a rules tree, walked
/// whole). In a rules tree only Markdown files and links named like them are
/// reported, as the loader only reads those.
///
/// Every function's scratch variables are globals (POSIX sh has no `local`),
/// so none is read again after a call that could have reassigned it.
fn posix_functions(limits: &Limits) -> String {
    format!(
        r#"nl='
'
budget={budget}
left={entries}
j() {{ case $1 in */) printf '%s%s' "$1" "$2" ;; *) printf '%s/%s' "$1" "$2" ;; esac; }}
emit_file() {{
  if [ ! -r "$1" ]; then printf 'U\n%s\n' "$1"; return 0; fi
  _n=$(digits "$(fsize "$1")")
  if [ "$_n" -le {max_file} ] && [ "$_n" -le "$budget" ]; then
    printf 'F\n%s\n%s\n' "$1" "$_n"
    cat -- "$1"
    budget=$((budget - _n))
  else
    printf 'B\n%s\n%s\n' "$1" "$_n"
  fi
}}
item() {{
  printf 'S\n%s\n%s\n' "$2" "$1"
  if [ -L "$1" ]; then
    _t=$(canon "$1") || return 0
    case $_t in ''|"$1"|*"$nl"*) return 0 ;; esac
    printf 'L\n%s\n%s\n' "$1" "$_t"
    printf 'S\n%s\n%s\n' "$2" "$_t"
    holds "$_t" "$2"
  else
    holds "$1" "$2"
  fi
}}
holds() {{
  case $2 in
  f) if [ -f "$1" ]; then emit_file "$1"; fi ;;
  d) if [ -d "$1" ]; then printf 'D\n%s\n' "$1"; item "$(j "$1" MEWRK.md)" f; item "$(j "$1" rules)" t; fi ;;
  t) if [ -d "$1" ]; then walk "$1" 0; fi ;;
  esac
}}
walk() {{
  printf 'D\n%s\n' "$1"
  [ "$2" -lt {depth} ] || return 0
  for _e in "$1"/* "$1"/.[!.]* "$1"/..?*; do
    [ -e "$_e" ] || [ -L "$_e" ] || continue
    case ${{_e##*/}} in *"$nl"*) continue ;; esac
    [ "$left" -gt 0 ] || return 0
    left=$((left - 1))
    if [ -L "$_e" ]; then
      case ${{_e##*/}} in .[mM][dD]) continue ;; *.[mM][dD]) ;; *) continue ;; esac
      _t=$(canon "$_e") || continue
      case $_t in ''|"$_e"|*"$nl"*) continue ;; esac
      printf 'L\n%s\n%s\nS\nf\n%s\n' "$_e" "$_t" "$_t"
      if [ -f "$_t" ]; then emit_file "$_t"; fi
    elif [ -d "$_e" ]; then
      walk "$_e" $(($2 + 1))
    elif [ -f "$_e" ]; then
      case ${{_e##*/}} in .[mM][dD]) ;; *.[mM][dD]) emit_file "$_e" ;; esac
    fi
  done
}}
scan() {{
  printf 'D\n%s\n' "$1"
  item "$(j "$1" MEWRK.md)" f
  item "$(j "$1" MEWRK.local.md)" f
  item "$(j "$1" .mewrk)" d
}}
fetch() {{
  _rest=${{1#/}}
  _cur=/
  while [ -n "$_rest" ]; do
    _name=${{_rest%%/*}}
    case $_rest in */*) _rest=${{_rest#*/}} ;; *) _rest= ;; esac
    [ -n "$_name" ] || continue
    _next=$(j "$_cur" "$_name")
    if [ -L "$_next" ]; then
      printf 'S\nf\n%s\n' "$_next"
      _t=$(canon "$_next") || return 0
      case $_t in ''|"$_next"|*"$nl"*) return 0 ;; esac
      printf 'L\n%s\n%s\n' "$_next" "$_t"
      _cur=$_t
    elif [ -d "$_next" ]; then
      printf 'D\n%s\n' "$_next"
      _cur=$_next
    elif [ -n "$_rest" ]; then
      printf 'S\nf\n%s\n' "$_next"
      return 0
    else
      _cur=$_next
    fi
  done
  printf 'S\nf\n%s\n' "$_cur"
  if [ -f "$_cur" ]; then emit_file "$_cur"; elif [ -d "$_cur" ]; then printf 'D\n%s\n' "$_cur"; fi
}}
"#,
        budget = limits.budget,
        entries = limits.entries,
        max_file = limits.max_file,
        depth = limits.depth,
    )
}

/// The POSIX form of a job. See [`crate::remote_powershell::instruction_probe`]
/// for the PowerShell one; both write the same records:
///
/// ```text
/// mewrk-instructions 1
/// R <root>   T <top level>   P <project folder>     (a startup job only)
/// D <folder>                                        a real folder
/// F <file> <size> <bytes>                           a regular file
/// B <file> <size>                                   one too large to bring
/// U <file>                                          one that cannot be read
/// L <link> <target>                                 a link and where it leads
/// S <f|d|t> <path>                                  a scope (see posix_functions)
/// E
/// ```
///
/// each field on a line of its own. Every path but a link's own is canonical,
/// and a link's sits in a canonical folder.
fn posix_script(job: &Job<'_>, limits: &Limits) -> String {
    let mut script = String::with_capacity(8192);
    // Walking a rules folder needs globbing, which zsh's `sh` emulation
    // starts with off (its `-f`); every operand is quoted, so it is safe on.
    script.push_str("set +f\n");
    if let Job::Startup { root, .. } = job {
        script.push_str(&format!(
            "cd -- {} 2>/dev/null || {{ printf '%s\\n' 'cannot enter the workspace root' >&2; exit 64; }}\nR=$(pwd -P)\n",
            quote_remote_path(root.trim())
        ));
    }
    script.push_str(SHELL_HELPERS);
    script.push_str(&posix_functions(limits));
    script.push_str(&format!("printf '%s\\n' '{HEADER}'\n"));
    match job {
        Job::Startup { project_folder, .. } => {
            script.push_str(
                r#"printf 'R\n%s\n' "$R"
T=
d=$R
while :; do
  if [ -e "$d/.git" ] || [ -L "$d/.git" ]; then T=$d; break; fi
  p=$(dirname -- "$d")
  [ "$p" != "$d" ] || break
  d=$p
done
[ -n "$T" ] || T=$R
printf 'T\n%s\n' "$T"
cur=$T
scan "$cur"
rest=${R#"$T"}
rest=${rest#/}
while [ -n "$rest" ]; do
  name=${rest%%/*}
  case $rest in */*) rest=${rest#*/} ;; *) rest= ;; esac
  cur=$(j "$cur" "$name")
  scan "$cur"
done
"#,
            );
            if let Some(folder) = project_folder {
                script.push_str(&format!(
                    "if cd -- {} 2>/dev/null; then P=$(pwd -P); printf 'P\\n%s\\nD\\n%s\\n' \"$P\" \"$P\"; item \"$(j \"$P\" MEWRK.local.md)\" f; fi\n",
                    quote_remote_path(folder.trim())
                ));
            }
        }
        Job::Folders(folders) => {
            script.push_str("for d in");
            for folder in folders.iter() {
                script.push(' ');
                script.push_str(&sh_single_quote(folder));
            }
            script.push_str(
                "; do\n  [ -d \"$d\" ] && [ ! -L \"$d\" ] && [ \"$(cd -- \"$d\" 2>/dev/null && pwd -P)\" = \"$d\" ] || break\n  scan \"$d\"\ndone\n",
            );
        }
        Job::Paths(paths) => {
            script.push_str("for p in");
            for path in paths.iter() {
                script.push(' ');
                script.push_str(&sh_single_quote(path));
            }
            script.push_str("; do fetch \"$p\"; done\n");
        }
    }
    script.push_str("printf 'E\\n'\n");
    script
}

// ---------------------------------------------------------------------------
// Reading an answer
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeKind {
    /// An instruction file: whatever is not reported there is gone.
    File,
    /// A `.mewrk` folder: gone unless reported, its other contents left be.
    Folder,
    /// A rules tree: gone unless reported, and inside it whatever is not
    /// reported is gone too.
    Tree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    Dir(String),
    File { path: String, bytes: Vec<u8> },
    Large { path: String, size: u64 },
    Unreadable(String),
    Link { path: String, target: String },
    Scope { kind: ScopeKind, path: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Answer {
    root: Option<String>,
    top: Option<String>,
    project: Option<String>,
    records: Vec<Record>,
}

/// The answer was cut short or does not follow the format.
fn incomplete() -> String {
    "the machine's answer was incomplete (a file may have changed while it was read); try again"
        .to_owned()
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

fn parse(bytes: &[u8]) -> Result<Answer, String> {
    let mut cursor = Cursor { rest: bytes };
    if cursor.line()? != HEADER {
        return Err(incomplete());
    }
    let mut answer = Answer::default();
    loop {
        let tag = cursor.line()?;
        let record = match tag.as_str() {
            "E" => return Ok(answer),
            "R" => {
                answer.root = Some(cursor.line()?);
                continue;
            }
            "T" => {
                answer.top = Some(cursor.line()?);
                continue;
            }
            "P" => {
                answer.project = Some(cursor.line()?);
                continue;
            }
            "D" => Record::Dir(cursor.line()?),
            "F" => {
                let path = cursor.line()?;
                let size = cursor.count()?;
                Record::File {
                    path,
                    bytes: cursor.bytes(size)?,
                }
            }
            "B" => {
                let path = cursor.line()?;
                Record::Large {
                    path,
                    size: cursor.count()?,
                }
            }
            "U" => Record::Unreadable(cursor.line()?),
            "L" => {
                let path = cursor.line()?;
                Record::Link {
                    path,
                    target: cursor.line()?,
                }
            }
            "S" => {
                let kind = match cursor.line()?.as_str() {
                    "f" => ScopeKind::File,
                    "d" => ScopeKind::Folder,
                    "t" => ScopeKind::Tree,
                    _ => return Err(incomplete()),
                };
                Record::Scope {
                    kind,
                    path: cursor.line()?,
                }
            }
            _ => return Err(incomplete()),
        };
        answer.records.push(record);
    }
}

// ---------------------------------------------------------------------------
// Bringing the mirror up to date
// ---------------------------------------------------------------------------

/// Applies one answer to the mirror: folders, files and links first, each put
/// in place by a rename, then the scopes, which remove what the machine no
/// longer has. Returns the files left out — a name or link this computer
/// cannot hold, a file the machine would not let be read — each with its
/// reproduced place (or just its name) for the label.
fn materialize(
    mirror: &RemoteMirror,
    records: &[Record],
    max_file: u64,
) -> Result<Vec<(PathBuf, SkipReason)>, String> {
    let mut skipped = Vec::new();
    let mut reported = HashSet::new();
    let mut folders = HashSet::new();
    let mut ensured = HashSet::new();
    let mut links = Vec::new();
    let mut scopes = Vec::new();
    let failed = |path: &Path, error: io::Error| {
        format!("could not reproduce {} here: {error}", path.display())
    };
    let left_out = |remote: &str| PathBuf::from(remote.rsplit('/').next().unwrap_or(remote));
    for record in records {
        match record {
            Record::Dir(remote) => {
                let Some(local) = mirror.local_path(remote) else {
                    continue;
                };
                ensure_real_dir(mirror, &local, &mut ensured).map_err(|error| failed(&local, error))?;
                folders.insert(local.clone());
                reported.insert(local);
            }
            Record::File { path, bytes } => {
                let Some(local) = mirror.local_path(path) else {
                    skipped.push((left_out(path), SkipReason::Unreadable));
                    continue;
                };
                ensure_parent(mirror, &local, &mut ensured).map_err(|error| failed(&local, error))?;
                write_file(&local, bytes).map_err(|error| failed(&local, error))?;
                reported.insert(local);
            }
            Record::Large { path, size } => {
                let Some(local) = mirror.local_path(path) else {
                    skipped.push((left_out(path), SkipReason::TooLarge));
                    continue;
                };
                ensure_parent(mirror, &local, &mut ensured).map_err(|error| failed(&local, error))?;
                // Only its size is ever looked at: the loader refuses a file
                // over its limit before reading a byte. Sparse, so it costs
                // nothing.
                write_placeholder(&local, (*size).max(max_file + 1))
                    .map_err(|error| failed(&local, error))?;
                reported.insert(local);
            }
            Record::Unreadable(path) => {
                // Not reported, so its scope removes any older copy.
                skipped.push((
                    mirror.local_path(path).unwrap_or_else(|| left_out(path)),
                    SkipReason::Unreadable,
                ));
            }
            Record::Link { path, target } => links.push((path, target)),
            Record::Scope { kind, path } => scopes.push((*kind, path)),
        }
    }
    // Links last: on Windows a link to a folder is a different kind of link,
    // so its target has to be in place first.
    for (path, target) in links {
        let Some(local) = mirror.local_path(path) else {
            skipped.push((left_out(path), SkipReason::Unreadable));
            continue;
        };
        let Some(local_target) = mirror.local_path(target) else {
            skipped.push((local, SkipReason::Unreadable));
            continue;
        };
        ensure_parent(mirror, &local, &mut ensured).map_err(|error| failed(&local, error))?;
        match make_link(&local, &local_target) {
            Ok(()) => {
                reported.insert(local);
            }
            Err(_) => {
                remove_entry(&local).map_err(|error| failed(&local, error))?;
                skipped.push((local, SkipReason::Unreadable));
            }
        }
    }
    let mut ancestors = HashSet::new();
    for path in &reported {
        for ancestor in path.ancestors().skip(1) {
            if ancestor == mirror.tree || !mirror.contains(ancestor) {
                break;
            }
            if !ancestors.insert(ancestor.to_path_buf()) {
                break;
            }
        }
    }
    let prune = Prune {
        reported: &reported,
        folders: &folders,
        ancestors: &ancestors,
    };
    for (kind, path) in scopes {
        let Some(local) = mirror.local_path(path) else {
            continue;
        };
        prune.scope(kind, &local).map_err(|error| failed(&local, error))?;
    }
    Ok(skipped)
}

/// What one answer reported, for removing what it did not.
struct Prune<'a> {
    reported: &'a HashSet<PathBuf>,
    folders: &'a HashSet<PathBuf>,
    /// Folders some reported path lies in.
    ancestors: &'a HashSet<PathBuf>,
}

impl Prune<'_> {
    fn scope(&self, kind: ScopeKind, path: &Path) -> io::Result<()> {
        if self.reported.contains(path) {
            if kind == ScopeKind::Tree && self.folders.contains(path) {
                self.tree(path)?;
            }
            return Ok(());
        }
        if self.ancestors.contains(path) {
            return Ok(());
        }
        remove_entry(path)
    }

    fn tree(&self, directory: &Path) -> io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let child = entry?.path();
            if is_temporary(&child) {
                continue;
            }
            let kept = self.reported.contains(&child) || self.ancestors.contains(&child);
            if !kept {
                remove_entry(&child)?;
            } else if fs::symlink_metadata(&child).is_ok_and(|metadata| metadata.is_dir()) {
                self.tree(&child)?;
            }
        }
        Ok(())
    }
}

/// Makes every folder from the tree down to `path` a real folder, replacing
/// a link or file that stands where the machine has a folder. Every reported
/// folder is canonical, so no component of it is a link on the machine.
fn ensure_real_dir(
    mirror: &RemoteMirror,
    path: &Path,
    ensured: &mut HashSet<PathBuf>,
) -> io::Result<()> {
    if ensured.contains(path) {
        return Ok(());
    }
    let relative = path
        .strip_prefix(&mirror.tree)
        .map_err(|_| io::Error::other("outside the mirror"))?;
    let mut current = mirror.tree.clone();
    for component in relative.components() {
        current.push(component);
        if ensured.contains(&current) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                remove_entry(&current)?;
                fs::create_dir(&current)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(error) = fs::create_dir(&current) {
                    if !fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.is_dir()) {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
        ensured.insert(current.clone());
    }
    Ok(())
}

fn ensure_parent(
    mirror: &RemoteMirror,
    path: &Path,
    ensured: &mut HashSet<PathBuf>,
) -> io::Result<()> {
    match path.parent() {
        Some(parent) if parent != mirror.tree => ensure_real_dir(mirror, parent, ensured),
        _ => Ok(()),
    }
}

/// Removes whatever is at `path` — a link itself, never what it leads to.
fn remove_entry(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::remove_file(path).or_else(|_| fs::remove_dir(path))
        }
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

const TEMPORARY_PREFIX: &str = ".mewrk-mirror-";

fn is_temporary(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(TEMPORARY_PREFIX))
}

/// A fresh name beside `path` to build its replacement under.
fn temporary_beside(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    path.with_file_name(format!(
        "{TEMPORARY_PREFIX}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Puts `replacement` (already built beside it) in `path`'s place. A folder
/// standing there goes first, and on Windows a link too, which a rename
/// there does not replace.
fn replace_with(replacement: &Path, path: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() || (cfg!(windows) && metadata.file_type().is_symlink()) {
            remove_entry(path)?;
        }
    }
    fs::rename(replacement, path).inspect_err(|_| {
        let _ = remove_entry(replacement);
    })
}

/// Writes `bytes` to `path` unless it already holds exactly them.
fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file() && metadata.len() == bytes.len() as u64
    }) && fs::read(path).is_ok_and(|existing| existing == bytes)
    {
        return Ok(());
    }
    let temporary = temporary_beside(path);
    fs::write(&temporary, bytes).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })?;
    replace_with(&temporary, path)
}

fn write_placeholder(path: &Path, size: u64) -> io::Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == size) {
        return Ok(());
    }
    let temporary = temporary_beside(path);
    fs::File::create(&temporary)
        .and_then(|file| file.set_len(size))
        .inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })?;
    replace_with(&temporary, path)
}

fn make_link(path: &Path, target: &Path) -> io::Result<()> {
    if fs::read_link(path).is_ok_and(|existing| existing == target) {
        return Ok(());
    }
    let temporary = temporary_beside(path);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &temporary)?;
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, &temporary)?;
        } else {
            std::os::windows::fs::symlink_file(target, &temporary)?;
        }
    }
    replace_with(&temporary, path)
}

#[cfg(test)]
mod tests;
