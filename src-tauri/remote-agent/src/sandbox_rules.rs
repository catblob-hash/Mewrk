//! What a cell may read and write, resolved on the machine it runs on.
//!
//! The host's [`SandboxPolicy`] names the conversation's workspaces and any
//! extra paths the user configured. Everything a policy cannot turn off is
//! added here, where the machine's own home, `PATH` and agent directories are
//! known:
//!
//! * **Credentials are unreadable**: SSH and GPG keys, cloud and registry
//!   credentials, password stores, browser profiles, keychains, and Mewrk's
//!   own directories. The threat is code in the sandbox that reads a secret
//!   and sends it somewhere the network policy allows.
//! * **Whatever runs outside the sandbox later is unwritable**, even inside a
//!   workspace: git hooks and git config, Mewrk's `.mewrk` project
//!   configuration (its hooks, MCP servers, language servers and launch
//!   commands run on the host), editor task files, `direnv`'s `.envrc`, shell
//!   startup files, autostart entries, every directory on `PATH`, and the
//!   agent itself. Writing any of them would be a way out of the sandbox that
//!   waits for the user to open a terminal or commit.
//!
//! Paths are canonicalized — Seatbelt matches the resolved path of the file a
//! process opens, and a bind mount follows symlinks — so `/tmp` on macOS is
//! `/private/tmp` here. A path that does not exist yet is resolved as far as
//! it exists and the rest appended.
//!
//! The agent turns the rules into a cell's sandbox (`agent::sandbox`). The
//! host reads the same rules for its own file tools on a sandboxed workspace
//! of this computer, which act in its process rather than in a cell:
//! [`Rules::reads`] and [`Rules::refuses_write`] are the rules as Seatbelt
//! applies them, the backend that expresses every one of them, so a tool and
//! a command in one workspace meet the same sandbox. That is why this module
//! is part of the library rather than of the agent alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::protocol::SandboxPolicy;

/// The operating system a cell runs on, which decides the built-in lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// What the agent knows about the machine and the cell it is about to start.
#[derive(Clone, Debug)]
pub struct Facts {
    pub os: Os,
    pub home: PathBuf,
    /// The agent's own directory (`~/.mewrk/remote` unless overridden).
    pub agent_root: Option<PathBuf>,
    /// The running agent's executable, which is what a cell runs. `None`
    /// where no agent's cells are in question: the host's file tools with no
    /// build of the agent on its computer.
    pub agent_exe: Option<PathBuf>,
    pub run_dir: Option<PathBuf>,
    /// Directories on the `PATH` sessions would get, and on the agent's own.
    pub path_dirs: Vec<PathBuf>,
    /// The cell's own directories, created by the caller: its private
    /// temporary directory and the package-manager caches it may fill (see
    /// [`cache_env`]). None when the rules are read for something other than
    /// a cell — the host's file tools — which has no directories of its own.
    pub own: Vec<PathBuf>,
}

/// The resolved rules, all absolute and canonical.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    /// Directories the cell may write, besides the device files every
    /// process needs.
    pub writable: Vec<PathBuf>,
    /// Paths the cell may not read. They need not exist.
    pub deny_read: Vec<PathBuf>,
    /// Paths inside [`Self::deny_read`] that are readable after all.
    pub readable: Vec<PathBuf>,
    /// Paths that stay read-only inside writable directories. They need not
    /// exist: Seatbelt refuses to create them, and on Linux the caller decides
    /// which missing ones get a placeholder.
    pub deny_write: Vec<PathBuf>,
    /// Names protected at any depth under a writable directory, as relative
    /// paths (`.git/hooks`). Seatbelt matches them by pattern; bubblewrap
    /// can only protect the ones it finds (see [`find_protected`]).
    pub protected_names: Vec<&'static str>,
    /// Writable directories that are not inside a git repository, where a
    /// `HEAD` file would turn the directory itself into one (see
    /// [`BARE_REPOSITORY_MARKERS`]).
    pub bare_roots: Vec<PathBuf>,
}

/// Why the rules keep a path from being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
    /// It is in none of the writable directories.
    Outside,
    /// It is a credential store, something outside the sandbox runs (a shell
    /// startup file, a directory on `PATH`, the agent), or a path the policy
    /// keeps read-only.
    Protected,
    /// It is in a writable directory, but git, Mewrk, an editor or `direnv`
    /// would run or obey it outside the sandbox: a protected name, `.git`
    /// itself, or a `HEAD` that would make its directory a repository.
    Executed,
}

impl Rules {
    /// Whether the rules let `path` — absolute and canonical — be read.
    pub fn reads(&self, path: &Path) -> bool {
        !self.deny_read.iter().any(|denied| within(path, denied))
            || self.readable.iter().any(|allowed| within(path, allowed))
    }

    /// Why the rules do not let `path` — absolute and canonical, the part
    /// that does not exist yet appended — be written, or `None` when they do.
    ///
    /// This is the reading the agent's Seatbelt profile gives the rules:
    /// writable directories as subpaths, then the read-only paths
    /// beneath them, then the protected names matched at any depth. Names are
    /// matched without regard to case on the systems whose file systems
    /// usually ignore it, where `.MEWRK` is the `.mewrk` the host reads —
    /// as Seatbelt itself matches on such a volume.
    pub fn refuses_write(&self, path: &Path) -> Option<WriteRefusal> {
        let Some(root) = self.writable.iter().find(|root| within(path, root)) else {
            return Some(WriteRefusal::Outside);
        };
        if self.deny_write.iter().any(|denied| within(path, denied)) {
            return Some(WriteRefusal::Protected);
        }
        let names: Vec<String> = relative_names(path, root);
        let same = |name: &str, expected: &str| {
            if CASE_INSENSITIVE {
                name.eq_ignore_ascii_case(expected)
            } else {
                name == expected
            }
        };
        let git = names.iter().position(|name| same(name, ".git"));
        // `.git` itself: a directory to rename away, a file to point
        // elsewhere.
        if names.last().is_some_and(|name| same(name, ".git")) {
            return Some(WriteRefusal::Executed);
        }
        for protected in &self.protected_names {
            let parts: Vec<&str> = protected.split('/').collect();
            if let Some(inside_git) = protected.strip_prefix(".git/") {
                // What git runs or obeys, wherever it sits in a git directory
                // (`.git/worktrees/x/config`, `.git/modules/m/hooks`).
                if let Some(git) = git {
                    if names[git + 1..].iter().any(|name| same(name, inside_git)) {
                        return Some(WriteRefusal::Executed);
                    }
                }
                continue;
            }
            if names
                .windows(parts.len())
                .any(|window| window.iter().zip(&parts).all(|(name, part)| same(name, part)))
            {
                return Some(WriteRefusal::Executed);
            }
        }
        if let Some(last) = names.last() {
            let in_git = git.is_some_and(|git| git + 1 < names.len());
            if !in_git && BARE_REPOSITORY_MARKERS.iter().any(|marker| same(last, marker)) {
                return Some(WriteRefusal::Executed);
            }
        }
        None
    }
}

/// Whether file names usually ignore case here: macOS and Windows format
/// their volumes that way.
const CASE_INSENSITIVE: bool = cfg!(any(target_os = "macos", windows));

/// Whether `path` is `root` or inside it, both canonical.
fn within(path: &Path, root: &Path) -> bool {
    if !CASE_INSENSITIVE {
        return path.starts_with(root);
    }
    let mut path = path.components();
    root.components().all(|part| {
        path.next().is_some_and(|candidate| {
            candidate
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&part.as_os_str().to_string_lossy())
        })
    })
}

/// The names of `path` below `root`, which contains it.
fn relative_names(path: &Path, root: &Path) -> Vec<String> {
    path.components()
        .skip(root.components().count())
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect()
}

/// Relative to the home directory, on every system: credentials, keys and
/// the private state of programs that hold them.
const SECRET_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".config/gh",
    ".config/hub",
    ".config/glab-cli",
    ".npmrc",
    ".yarnrc",
    ".pypirc",
    ".pip/pip.conf",
    ".config/pip/pip.conf",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".gem/credentials",
    ".m2/settings.xml",
    ".gradle/gradle.properties",
    ".terraform.d/credentials.tfrc.json",
    ".vault-token",
    ".password-store",
    ".pki",
    ".claude.json",
    ".claude/.credentials.json",
    ".codex/auth.json",
    ".config/github-copilot",
    ".config/op",
    ".1password",
    ".mewrk",
    ".local/share/keyrings",
    ".local/share/com.mewrk.app",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/microsoft-edge",
    ".config/vivaldi",
    ".thunderbird",
];

/// macOS keeps keychains, cookies and application state under `Library`.
const SECRET_MACOS_PATHS: &[&str] = &[
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    "Library/Mail",
    "Library/Messages",
    "Library/Application Support/com.mewrk.app",
    "Library/Application Support/Claude",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Chromium",
    "Library/Application Support/Firefox",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Arc",
    "Library/Application Support/Vivaldi",
    "Library/Application Support/1Password",
    "Library/Group Containers/2BUA8C4S2C.com.1password",
];

/// Windows keeps application state under `AppData`: Mewrk's own — its data,
/// credentials vault, and the components it runs (the AI SDK sidecar, the
/// Claude Code CLI) — is in both of its halves.
const SECRET_WINDOWS_PATHS: &[&str] = &[
    "AppData/Local/com.mewrk.app",
    "AppData/Roaming/com.mewrk.app",
];

/// Inside an unreadable directory, what the cell still needs to read: the
/// skills a model may run a script from.
const READABLE_HOME_PATHS: &[&str] = &[".mewrk/skills"];

/// Relative to the home directory: files something outside the sandbox reads
/// as instructions — shells at login, git on every command, the desktop at
/// login, editors when they open — plus every credential store, whose
/// configuration can name a program to run (`credential_process`,
/// `credential.helper`).
const EXECUTED_HOME_PATHS: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".config/fish",
    ".config/nushell",
    ".config/powershell",
    ".inputrc",
    ".tmux.conf",
    ".config/tmux",
    ".vimrc",
    ".config/nvim",
    ".emacs",
    ".emacs.d",
    ".gitconfig",
    ".config/git",
    ".pam_environment",
    ".xprofile",
    ".xinitrc",
    ".xsession",
    ".config/autostart",
    ".config/systemd",
    ".config/environment.d",
    ".local/share/applications",
    ".local/bin",
    ".vscode",
    ".vscode-server",
    ".cursor",
    ".config/Code",
    ".claude",
    ".codex",
    ".mewrk",
    "Library/LaunchAgents",
    "Library/Application Support/Code",
];

/// Names protected wherever they appear under a writable directory: what git,
/// Mewrk, editors and `direnv` run on their own when the user next works in
/// the project outside the sandbox.
pub const PROTECTED_NAMES: &[&str] = &[
    ".git/hooks",
    ".git/config",
    ".git/config.worktree",
    ".git/commondir",
    ".mewrk",
    ".vscode",
    ".idea",
    ".claude",
    ".cursor",
    ".mcp.json",
    ".envrc",
    ".devcontainer",
];

/// Protected names that, missing at a workspace's root, are held by an empty
/// placeholder while a cell runs, on the backends that can only protect what
/// exists (bubblewrap, srt-win): without one, code in the sandbox could create
/// `.envrc` or `.vscode/tasks.json` for the user's own tools to run later.
/// `(name, is a directory, what a file placeholder holds)`. A placeholder
/// still as it was made is removed when the cell ends.
pub const ROOT_PLACEHOLDERS: &[(&str, bool, &str)] = &[
    (".mewrk", true, ""),
    (".vscode", true, ""),
    (".idea", true, ""),
    (".claude", true, ""),
    (".cursor", true, ""),
    (".devcontainer", true, ""),
    (".envrc", false, ""),
    // Valid and empty, so a client that reads it finds no servers rather
    // than an error.
    (".mcp.json", false, "{\"mcpServers\":{}}"),
];

/// The same inside an existing `.git`. An empty `commondir` names the git
/// directory itself and an empty `config.worktree` sets nothing, so git
/// behaves as if neither were there.
pub const GIT_PLACEHOLDERS: &[(&str, bool, &str)] = &[
    ("hooks", true, ""),
    ("config.worktree", false, ""),
    ("commondir", false, ""),
];

/// What makes a directory a git repository by itself. Git treats a directory
/// holding `HEAD`, `objects` and `refs` as a bare repository and honours its
/// `config` — including `core.fsmonitor`, a command — when the user runs git
/// there, so a writable directory that is not in a repository must not be
/// able to become one. Refusing `HEAD` is enough; the others are ordinary
/// names in many projects.
pub const BARE_REPOSITORY_MARKERS: &[&str] = &["HEAD"];

/// Resolves the rules for one cell.
pub fn resolve(policy: &SandboxPolicy, facts: &Facts) -> Result<Rules, String> {
    let home = canonical(&facts.home);
    let mut writable = BTreeSet::new();
    for entry in &policy.writable {
        let path = absolute(entry, &facts.home)?;
        let path = canonical(&path);
        if !path.is_dir() {
            return Err(format!(
                "The sandbox cannot make {} writable: it is not a directory on this machine",
                path.display()
            ));
        }
        if path.parent().is_none() {
            return Err("The sandbox cannot make the whole file system writable".into());
        }
        writable.insert(path);
    }
    let own: Vec<PathBuf> = facts.own.iter().map(|own| canonical(own)).collect();
    writable.extend(own.iter().cloned());

    let mut deny_read = BTreeSet::new();
    for relative in SECRET_HOME_PATHS {
        deny_read.insert(under(&home, relative));
    }
    if facts.os == Os::MacOs {
        for relative in SECRET_MACOS_PATHS {
            deny_read.insert(under(&home, relative));
        }
    }
    if facts.os == Os::Windows {
        for relative in SECRET_WINDOWS_PATHS {
            deny_read.insert(under(&home, relative));
        }
    }
    if let Some(root) = &facts.agent_root {
        deny_read.insert(canonical(root));
    }
    if let Some(run_dir) = &facts.run_dir {
        deny_read.insert(canonical(run_dir));
    }
    for entry in &policy.deny_read {
        deny_read.insert(canonical(&absolute(entry, &facts.home)?));
    }

    let mut readable = BTreeSet::new();
    for relative in READABLE_HOME_PATHS {
        readable.insert(under(&home, relative));
    }
    // The cell is the agent's own executable; it has to be able to start it.
    if let Some(exe) = &facts.agent_exe {
        if let Some(directory) = canonical(exe).parent() {
            readable.insert(directory.to_path_buf());
        }
    }
    for entry in &policy.readable {
        readable.insert(canonical(&absolute(entry, &facts.home)?));
    }
    // A workspace the user chose inside an unreadable directory is still the
    // workspace.
    for root in &writable {
        if deny_read.iter().any(|denied| root.starts_with(denied) && root != denied) {
            readable.insert(root.clone());
        }
    }

    let mut deny_write = BTreeSet::new();
    for relative in EXECUTED_HOME_PATHS {
        deny_write.insert(under(&home, relative));
    }
    // Credential stores are written as well as read by programs outside the
    // sandbox; a planted config is as good as a stolen key.
    deny_write.extend(deny_read.iter().cloned());
    for directory in &facts.path_dirs {
        if directory.is_absolute() {
            deny_write.insert(canonical(directory));
        }
    }
    if let Some(root) = &facts.agent_root {
        deny_write.insert(canonical(root));
    }
    if let Some(exe) = &facts.agent_exe {
        deny_write.insert(canonical(exe));
    }
    for entry in &policy.deny_write {
        deny_write.insert(canonical(&absolute(entry, &facts.home)?));
    }
    // A cell's own directories are its own, whatever list they happen to fall
    // under (a cache directory under `~/.cache` on a `PATH`, say).
    deny_write.retain(|path| !own.iter().any(|own| path.starts_with(own)));
    deny_read.retain(|path| !own.iter().any(|own| path.starts_with(own)));

    let bare_roots = writable
        .iter()
        .filter(|root| !own.contains(root) && !inside_git_repository(root))
        .cloned()
        .collect();

    Ok(Rules {
        writable: prune_nested(writable),
        deny_read: deny_read.into_iter().collect(),
        readable: readable.into_iter().collect(),
        deny_write: deny_write.into_iter().collect(),
        protected_names: PROTECTED_NAMES.to_vec(),
        bare_roots,
    })
}

/// `path` as an absolute path of this machine, with a leading `~` expanded.
fn absolute(path: &str, home: &Path) -> Result<PathBuf, String> {
    let expanded = expand_home(path.trim(), home);
    if !expanded.is_absolute() {
        return Err(format!("The sandbox needs absolute paths, not {path}"));
    }
    Ok(expanded)
}

/// Expands a leading `~` against `home`, the only expansion the agent does:
/// everything else in a path is taken literally.
pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| {
        if cfg!(windows) {
            path.strip_prefix("~\\")
        } else {
            None
        }
    }) {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// `base` joined with `relative`, one of the `/`-separated names above, in
/// the machine's own separators.
pub fn under(base: &Path, relative: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    path.extend(relative.split('/'));
    path
}

/// The real path of `path`: symlinks resolved as far as the path exists, the
/// rest appended as written (with `.` and `..` folded).
pub fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return strip_verbatim(real);
    }
    let mut missing = Vec::new();
    let mut cursor = normalize(path);
    loop {
        if let Ok(real) = std::fs::canonicalize(&cursor) {
            let mut out = strip_verbatim(real);
            for part in missing.iter().rev() {
                out.push(part);
            }
            return normalize(&out);
        }
        match (cursor.file_name().map(|name| name.to_os_string()), cursor.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name);
                cursor = parent.to_path_buf();
            }
            _ => return normalize(path),
        }
    }
}

/// Windows' `canonicalize` answers in the `\\?\C:\…` form, which no rule or
/// tool expects.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Drops writable directories inside other writable directories: one rule
/// covers them.
fn prune_nested(paths: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !kept.iter().any(|outer| path.starts_with(outer)) {
            kept.push(path);
        }
    }
    kept
}

fn inside_git_repository(directory: &Path) -> bool {
    directory
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

/// Existing paths under `roots` that match a protected name, down to `depth`
/// levels, skipping directories that hold dependencies or build output. For
/// the backends that cannot protect a name by pattern.
pub fn find_protected(roots: &[PathBuf], names: &[&str], depth: usize) -> Vec<PathBuf> {
    const SKIP: &[&str] = &["node_modules", "target", "dist", "build", ".venv", "venv", "__pycache__"];
    let mut found = Vec::new();
    let mut queue: Vec<(PathBuf, usize)> = roots.iter().map(|root| (root.clone(), 0)).collect();
    let mut visited = 0usize;
    while let Some((directory, level)) = queue.pop() {
        visited += 1;
        if visited > 20_000 {
            break;
        }
        for name in names {
            let candidate = under(&directory, name);
            if std::fs::symlink_metadata(&candidate).is_ok() {
                found.push(candidate);
            }
        }
        if level >= depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || SKIP.contains(&name.as_ref()) {
                continue;
            }
            queue.push((entry.path(), level + 1));
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Whether an environment variable's name says it holds a secret. Matched by
/// the words of the name (`OPENAI_API_KEY` is `OPENAI`, `API`, `KEY`), so
/// `GIT_AUTHOR_NAME` or `KEYTIMEOUT` are left alone.
pub fn is_secret_env_name(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "TOKEN",
        "TOKENS",
        "SECRET",
        "SECRETS",
        "PASSWORD",
        "PASSWORDS",
        "PASSWD",
        "PASSPHRASE",
        "CREDENTIAL",
        "CREDENTIALS",
        "COOKIE",
        "COOKIES",
        "APIKEY",
        "KEY",
        "KEYS",
        "PAT",
        "AUTH",
    ];
    const NAMES: &[&str] = &[
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "GPG_AGENT_INFO",
        "GNUPGHOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "KRB5CCNAME",
        "LD_PRELOAD",
        "LD_AUDIT",
        "DOCKER_HOST",
        "VAULT_ADDR",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "AWS_PROFILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_CONFIG_FILE",
        "KUBECONFIG",
    ];
    // Words run together: `PGPASSWORD`, `GITHUBTOKEN`.
    const SUFFIXES: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "PASSWD", "APIKEY"];
    let upper = name.to_ascii_uppercase();
    if SUFFIXES.iter().any(|suffix| upper.ends_with(suffix)) {
        return true;
    }
    if NAMES.contains(&upper.as_str())
        || upper.starts_with("DYLD_")
        || upper.starts_with("MEWRK_")
        || upper.starts_with("VSCODE_IPC")
        || upper.starts_with("SSH_")
    {
        return true;
    }
    upper
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| WORDS.contains(&word))
}

/// Where package managers keep their caches inside the sandbox. Their usual
/// caches are outside it and unwritable — deliberately: a package unpacked
/// into `~/.cargo/registry` by code in the sandbox would be built outside it
/// the next time the user runs `cargo`. The cell gets caches of its own
/// instead, shared only with cells working on the same directories.
pub fn cache_env(cache_dir: &Path) -> BTreeMap<String, String> {
    let at = |relative: &str| cache_dir.join(relative).to_string_lossy().into_owned();
    [
        ("XDG_CACHE_HOME", at("xdg")),
        ("npm_config_cache", at("npm")),
        ("npm_config_store_dir", at("pnpm-store")),
        ("YARN_CACHE_FOLDER", at("yarn")),
        ("BUN_INSTALL_CACHE_DIR", at("bun")),
        ("PIP_CACHE_DIR", at("pip")),
        ("UV_CACHE_DIR", at("uv")),
        ("POETRY_CACHE_DIR", at("poetry")),
        ("GOMODCACHE", at("go/mod")),
        ("GOCACHE", at("go/build")),
        ("CARGO_HOME", at("cargo")),
        ("GRADLE_USER_HOME", at("gradle")),
        ("DENO_DIR", at("deno")),
        ("COREPACK_HOME", at("corepack")),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(home: &Path, tmp: &Path) -> Facts {
        Facts {
            os: Os::Linux,
            home: home.to_path_buf(),
            agent_root: Some(home.join(".mewrk/remote")),
            agent_exe: Some(home.join(".mewrk/remote/bin/tag/mewrk-remote")),
            run_dir: Some(home.join(".mewrk/remote/run")),
            path_dirs: vec![home.join(".local/bin"), PathBuf::from("/usr/bin")],
            own: vec![tmp.join("cell"), tmp.join("cache")],
        }
    }

    #[test]
    fn credentials_are_unreadable_and_what_runs_outside_is_unwritable() {
        let home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let workspace = home.path().join("project");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(tmp.path().join("cell")).unwrap();
        std::fs::create_dir_all(tmp.path().join("cache")).unwrap();
        let policy = SandboxPolicy {
            writable: vec!["~/project".into()],
            ..SandboxPolicy::default()
        };
        let rules = resolve(&policy, &facts(home.path(), tmp.path())).unwrap();
        let home = canonical(home.path());
        assert!(rules.writable.contains(&canonical(&workspace)));
        assert!(rules.writable.contains(&canonical(&tmp.path().join("cell"))));
        for secret in [".ssh", ".aws", ".mewrk", ".config/gh"] {
            assert!(rules.deny_read.contains(&home.join(secret)), "{secret}");
        }
        assert!(rules.readable.contains(&home.join(".mewrk/skills")));
        assert!(rules.readable.contains(&home.join(".mewrk/remote/bin/tag")));
        for executed in [".bashrc", ".gitconfig", ".local/bin", ".ssh", ".mewrk/remote"] {
            assert!(rules.deny_write.contains(&home.join(executed)), "{executed}");
        }
        assert!(rules.deny_write.contains(&PathBuf::from("/usr/bin")) || !Path::new("/usr/bin").exists());
        // In a repository already: git itself protects the directory.
        assert!(rules.bare_roots.is_empty());
    }

    /// Mewrk's application data — what it runs, and its vault — is under
    /// `AppData` on Windows, `Library/Application Support` on macOS: each
    /// system's own list hides it, and only that system's.
    #[test]
    fn each_system_hides_mewrks_own_application_data() {
        let home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let resolved = |os| {
            let facts = Facts { os, ..facts(home.path(), tmp.path()) };
            resolve(&SandboxPolicy::default(), &facts).unwrap()
        };
        let home = canonical(home.path());
        let windows = [
            under(&home, "AppData/Local/com.mewrk.app"),
            under(&home, "AppData/Roaming/com.mewrk.app"),
        ];
        let macos = under(&home, "Library/Application Support/com.mewrk.app");
        let rules = resolved(Os::Windows);
        for path in &windows {
            assert!(rules.deny_read.contains(path), "{}", path.display());
        }
        assert!(!rules.deny_read.contains(&macos));
        let rules = resolved(Os::MacOs);
        assert!(rules.deny_read.contains(&macos));
        assert!(windows.iter().all(|path| !rules.deny_read.contains(path)));
        let rules = resolved(Os::Linux);
        assert!(rules.deny_read.contains(&home.join(".local/share/com.mewrk.app")));
        assert!(windows.iter().all(|path| !rules.deny_read.contains(path)));
    }

    #[test]
    fn a_policy_cannot_make_the_root_or_a_missing_directory_writable() {
        let home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let facts = facts(home.path(), tmp.path());
        let root = SandboxPolicy {
            writable: vec!["/".into()],
            ..SandboxPolicy::default()
        };
        assert!(resolve(&root, &facts).is_err());
        let missing = SandboxPolicy {
            writable: vec![home.path().join("nope").to_string_lossy().into_owned()],
            ..SandboxPolicy::default()
        };
        assert!(resolve(&missing, &facts).is_err());
        let relative = SandboxPolicy {
            writable: vec!["project".into()],
            ..SandboxPolicy::default()
        };
        assert!(resolve(&relative, &facts).is_err());
    }

    #[test]
    fn a_directory_outside_any_repository_cannot_become_one() {
        let home = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let workspace = home.path().join("scratch");
        std::fs::create_dir_all(&workspace).unwrap();
        let policy = SandboxPolicy {
            writable: vec![workspace.to_string_lossy().into_owned()],
            ..SandboxPolicy::default()
        };
        let rules = resolve(&policy, &facts(home.path(), tmp.path())).unwrap();
        assert_eq!(rules.bare_roots, vec![canonical(&workspace)]);
    }

    #[test]
    fn a_path_is_read_and_written_the_way_seatbelt_reads_the_rules() {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("project");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(home.path().join(".mewrk/skills")).unwrap();
        let policy = SandboxPolicy {
            writable: vec!["~/project".into()],
            ..SandboxPolicy::default()
        };
        // No directories of its own: the rules as the host's file tools read them.
        let mut facts = facts(home.path(), home.path());
        facts.own.clear();
        let rules = resolve(&policy, &facts).unwrap();
        let home = canonical(home.path());
        let workspace = canonical(&workspace);

        assert!(rules.reads(&workspace.join("src/main.rs")));
        assert!(rules.reads(&home.join("notes.txt")));
        assert!(!rules.reads(&home.join(".ssh/id_ed25519")));
        assert!(!rules.reads(&home.join(".mewrk")));
        assert!(rules.reads(&home.join(".mewrk/skills/x/SKILL.md")));

        assert_eq!(rules.refuses_write(&workspace.join("src/new/main.rs")), None);
        assert_eq!(rules.refuses_write(&workspace.join(".git/index")), None);
        assert_eq!(rules.refuses_write(&home.join("notes.txt")), Some(WriteRefusal::Outside));
        assert_eq!(rules.refuses_write(&PathBuf::from("/etc/hosts")), Some(WriteRefusal::Outside));
        for executed in [
            ".git",
            ".git/hooks/pre-commit",
            ".git/config",
            ".git/worktrees/x/config",
            ".mewrk/launch.json",
            "packages/app/.vscode/tasks.json",
            "deep/er/.envrc",
            ".mcp.json",
            "HEAD",
            "sub/HEAD",
        ] {
            assert_eq!(
                rules.refuses_write(&workspace.join(executed)),
                Some(WriteRefusal::Executed),
                "{executed}"
            );
        }
        // Git's own `HEAD`, and a name that only contains a protected one.
        assert_eq!(rules.refuses_write(&workspace.join(".git/HEAD")), None);
        assert_eq!(rules.refuses_write(&workspace.join("my.envrc.md")), None);
        if CASE_INSENSITIVE {
            assert_eq!(
                rules.refuses_write(&workspace.join(".MEWRK/launch.json")),
                Some(WriteRefusal::Executed)
            );
        }
    }

    #[test]
    fn what_outside_runs_stays_read_only_inside_a_writable_directory() {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("project");
        std::fs::create_dir_all(workspace.join("bin")).unwrap();
        let policy = SandboxPolicy {
            writable: vec!["~".into()],
            deny_write: vec!["~/project/keep".into()],
            ..SandboxPolicy::default()
        };
        let mut facts = facts(home.path(), home.path());
        facts.own.clear();
        facts.path_dirs = vec![workspace.join("bin")];
        let rules = resolve(&policy, &facts).unwrap();
        let home = canonical(home.path());
        let workspace = canonical(&workspace);
        for protected in [
            home.join(".bashrc"),
            home.join(".ssh/config"),
            home.join(".mewrk/remote/run/x"),
            workspace.join("bin/tool"),
            workspace.join("keep/file"),
        ] {
            assert_eq!(
                rules.refuses_write(&protected),
                Some(WriteRefusal::Protected),
                "{}",
                protected.display()
            );
        }
        assert_eq!(rules.refuses_write(&workspace.join("src/lib.rs")), None);
    }

    #[test]
    fn a_missing_path_is_resolved_through_its_existing_parent() {
        let base = tempfile::tempdir().unwrap();
        let real = canonical(base.path());
        assert_eq!(canonical(&base.path().join("a/../b/c")), real.join("b/c"));
    }

    #[test]
    fn protected_names_are_found_in_nested_projects_but_not_in_dependencies() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git/hooks")).unwrap();
        std::fs::create_dir_all(root.path().join("packages/app/.vscode")).unwrap();
        std::fs::create_dir_all(root.path().join("node_modules/x/.vscode")).unwrap();
        let found = find_protected(&[root.path().to_path_buf()], PROTECTED_NAMES, 3);
        assert!(found.contains(&root.path().join(".git/hooks")));
        assert!(found.contains(&root.path().join("packages/app/.vscode")));
        assert!(!found.iter().any(|path| path.starts_with(root.path().join("node_modules"))));
    }

    #[test]
    fn secret_variables_are_recognized_by_their_words() {
        for secret in [
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "NPM_TOKEN",
            "PGPASSWORD",
            "DB_PASSWORD",
            "SSH_AUTH_SOCK",
            "DBUS_SESSION_BUS_ADDRESS",
            "MEWRK_BROWSER_DEV_TOKEN",
            "DYLD_INSERT_LIBRARIES",
            "HF_TOKEN",
            "GH_PAT",
        ] {
            assert!(is_secret_env_name(secret), "{secret}");
        }
        for plain in ["PATH", "HOME", "GIT_AUTHOR_NAME", "KEYTIMEOUT", "LANG", "TERM", "CARGO_HOME", "SHELL"] {
            assert!(!is_secret_env_name(plain), "{plain}");
        }
    }
}
