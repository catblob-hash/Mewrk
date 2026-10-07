//! The sandbox, over the file tools of a sandboxed workspace on this computer.
//!
//! A sandboxed workspace's commands run in the conversation's cell, where the
//! operating system holds them to the sandbox's rules
//! ([`remote_agent::sandbox_rules`]): nothing that holds credentials is
//! readable, nothing outside the writable directories is writable, and nothing
//! in them that runs outside the sandbox later — git hooks and config,
//! `.mewrk`, editor settings, `.envrc` — is writable either. The file tools
//! (`ls`, `grep`, `find`, `read`, `write`, `edit`) are held to the same rules,
//! so a tool reads and writes what a command in the same workspace could and
//! nothing more.
//!
//! On a WSL distribution or an SSH machine the tools are scripts, and they run
//! in the cell itself ([`crate::remote_files`]). On this computer they act in
//! Mewrk's own process ([`crate::tool_executor`]), so the host applies the
//! rules to them here: resolved with this computer's facts the way the agent
//! here resolves them for its cells, and read the way its Seatbelt profile
//! reads them ([`Rules::reads`], [`Rules::refuses_write`]).
//!
//! Like the cell, this ranks before the security level: no level, approval,
//! "always allow" or hook widens it, and a call it refuses is refused before
//! anything asks about it ([`refusal`]). Every check is made on a canonical
//! path, and what is then read or written is opened through a handle proved to
//! be that path ([`crate::path_guard::open_verified_file`],
//! [`crate::path_guard::write_file_verified`]), because the workspace's own
//! commands can rearrange its directories between a check and an open.
//!
//! A file the conversation's own tools saved on the host for the model to read
//! back ([`crate::tool_output`]) is not the workspace's, and is not confined:
//! it holds output the model was already shown.

use std::path::{Path, PathBuf};

use remote_agent::{
    protocol::SandboxSpec,
    sandbox_rules::{self, Facts, Os, Rules, WriteRefusal},
};

use crate::{
    model::ToolExecutionRequest,
    path_guard::{resolve_existing_with_scope, resolve_for_write_with_scope, ExecutionScope},
    workspace_set::ResolvedWorkspace,
};

/// The file tools the sandbox confines.
pub(crate) const FILE_TOOLS: &[&str] = &["ls", "grep", "find", "read", "write", "edit"];

/// The tools of [`FILE_TOOLS`] that write.
fn writes(tool_name: &str) -> bool {
    matches!(tool_name, "write" | "edit")
}

/// One sandboxed workspace's rules, as its file tools meet them.
pub(crate) struct FileSandbox {
    rules: Rules,
    /// The workspace's number, which every refusal names.
    workspace: u32,
}

impl FileSandbox {
    /// The sandbox of `workspace`, when it is a sandboxed workspace on this
    /// computer; `None` when it is not sandboxed or not here. Rules that
    /// cannot be resolved — a writable directory that is gone — are an error,
    /// never no sandbox.
    pub(crate) fn of(workspace: &ResolvedWorkspace) -> Result<Option<Self>, String> {
        match (&workspace.sandbox, workspace.is_local()) {
            (Some(sandbox), true) => Self::local(sandbox, workspace).map(Some),
            _ => Ok(None),
        }
    }

    fn local(sandbox: &SandboxSpec, workspace: &ResolvedWorkspace) -> Result<Self, String> {
        let home = dirs::home_dir().ok_or("This account has no home directory")?;
        // The `PATH` a command here gets: this process's, which is the login
        // shell's (`child_environment::adopt_login_shell_path`), and whatever
        // the workspace's variables put in front of it.
        let path_dirs = [
            workspace.runner.env().get("PATH").map(std::ffi::OsString::from),
            std::env::var_os("PATH"),
        ]
        .into_iter()
        .flatten()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .collect();
        let facts = Facts {
            os: Os::current(),
            home,
            // The agent here serves this process over its standard streams,
            // with no directory or socket of its own.
            agent_root: None,
            run_dir: None,
            agent_exe: crate::remote_link::local_agent_executable(),
            path_dirs,
            own: Vec::new(),
        };
        let rules = sandbox_rules::resolve(&sandbox.policy, &facts).map_err(|error| {
            format!(
                "The sandbox of workspace {} could not be set up: {error}",
                workspace.index
            )
        })?;
        Ok(Self {
            rules,
            workspace: workspace.index,
        })
    }

    /// A sandbox with these rules, for tests that cannot use this account's
    /// own home.
    #[cfg(test)]
    pub(crate) fn with_rules(rules: Rules, workspace: u32) -> Self {
        Self { rules, workspace }
    }

    /// Whether the rules let `path` — canonical — be read.
    pub(crate) fn reads(&self, path: &Path) -> bool {
        self.rules.reads(&plain(path))
    }

    /// Whether an entry a search or listing walked onto may be shown or
    /// opened: its canonical path is readable. One that cannot be resolved is
    /// not.
    pub(crate) fn reads_entry(&self, path: &Path) -> bool {
        std::fs::canonicalize(path).is_ok_and(|canonical| self.reads(&canonical))
    }

    /// `Ok` when the rules let `path` — canonical — be read.
    pub(crate) fn check_read(&self, path: &Path) -> Result<(), String> {
        if self.reads(path) {
            return Ok(());
        }
        Err(format!(
            "The sandbox of workspace {} does not allow reading {}: it keeps credentials and private application data, and the paths its settings name, out of reach of everything in the workspace. {}",
            self.workspace,
            path.display(),
            NO_WIDENING
        ))
    }

    /// `Ok` when the rules let `path` — canonical, the part that does not
    /// exist yet appended — be written.
    pub(crate) fn check_write(&self, path: &Path) -> Result<(), String> {
        let Some(refusal) = self.rules.refuses_write(&plain(path)) else {
            return Ok(());
        };
        let why = match refusal {
            WriteRefusal::Outside => format!(
                "only {} can be written",
                self.rules
                    .writable
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            WriteRefusal::Protected => "it keeps credential stores, shell startup files, the directories on PATH, Mewrk's agent and the paths its settings name read-only".to_owned(),
            WriteRefusal::Executed => "something outside the sandbox would run or obey it — git hooks and config, .git itself, .mewrk, editor settings, .envrc, .mcp.json, or a HEAD that would make its directory a repository".to_owned(),
        };
        Err(format!(
            "The sandbox of workspace {} does not allow writing {}: {why}. {}",
            self.workspace,
            path.display(),
            NO_WIDENING
        ))
    }
}

/// What every refusal adds, so the model neither retries nor asks for an
/// approval that cannot help.
const NO_WIDENING: &str =
    "No security level, approval or hook can allow it; only the user can change the workspace's sandbox settings.";

/// `path` as the rules spell paths: without Windows' `\\?\` prefix, which
/// `fs::canonicalize` adds there and the rules strip.
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// The sandbox's refusal of a file tool call on a sandboxed workspace of this
/// computer, decided before anything asks about the call: the path it names,
/// resolved the way the tool will resolve it, is one the tool could not read
/// or write. `None` lets the call go on — to the security level, and then to
/// the tool, which checks again on what it actually opens. A path that does
/// not resolve is left to the tool to report.
pub(crate) fn refusal(workspace: &ResolvedWorkspace, request: &ToolExecutionRequest) -> Option<String> {
    let sandbox = match FileSandbox::of(workspace) {
        Ok(Some(sandbox)) => sandbox,
        Ok(None) => return None,
        Err(error) => return Some(error),
    };
    let path = request
        .input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(".");
    let root = Path::new(&workspace.root);
    // Where the path leads, not whether the call may go there: the security
    // level answers that afterwards.
    let unrestricted = ExecutionScope::Unrestricted;
    if request.tool_name == "write" {
        let target = resolve_for_write_with_scope(root, path, &unrestricted).ok()?;
        return sandbox.check_write(&target).err();
    }
    let target = resolve_existing_with_scope(root, path, &unrestricted).ok()?;
    if writes(&request.tool_name) {
        sandbox.check_write(&target).err()
    } else {
        sandbox.check_read(&target).err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_environment::ShellRunner;

    fn sandboxed(root: &Path) -> ResolvedWorkspace {
        ResolvedWorkspace {
            index: 1,
            machine: None,
            root: root.to_string_lossy().into_owned(),
            runner: ShellRunner::Local {
                env: Default::default(),
            },
            os: None,
            shells: Vec::new(),
            machine_label: String::new(),
            sandbox: Some(SandboxSpec {
                cell: "conversation-c1".into(),
                policy: remote_agent::protocol::SandboxPolicy {
                    writable: vec![root.to_string_lossy().into_owned()],
                    ..Default::default()
                },
            }),
            env_path: root.to_string_lossy().into_owned(),
            is_worktree: false,
        }
    }

    fn call(tool_name: &str, path: &str) -> ToolExecutionRequest {
        let mut input = serde_json::Map::new();
        input.insert("path".into(), path.into());
        ToolExecutionRequest {
            conversation_id: "c1".into(),
            workspace_path: String::new(),
            tool_name: tool_name.into(),
            input,
        }
    }

    #[test]
    fn a_workspace_that_is_not_sandboxed_or_not_here_has_no_file_sandbox() {
        let root = tempfile::tempdir().unwrap();
        let mut workspace = sandboxed(root.path());
        workspace.sandbox = None;
        assert!(FileSandbox::of(&workspace).unwrap().is_none());
        let mut remote = sandboxed(root.path());
        remote.machine = Some(crate::model::RunTarget::Wsl {
            distro: "Ubuntu".into(),
        });
        assert!(FileSandbox::of(&remote).unwrap().is_none());
    }

    #[test]
    fn calls_are_refused_where_the_sandbox_would_refuse_a_command() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join("notes.md"), "hello").unwrap();
        let workspace = sandboxed(root.path());

        for allowed in [
            call("read", "notes.md"),
            call("ls", "."),
            call("write", "src/new.rs"),
            call("edit", "notes.md"),
            // Not there: the tool says so itself.
            call("read", "missing.md"),
        ] {
            assert_eq!(refusal(&workspace, &allowed), None, "{}", allowed.tool_name);
        }
        for refused in [
            call("write", ".mewrk/launch.json"),
            call("write", ".git/hooks/pre-commit"),
            call("write", ".envrc"),
            call("write", "../outside.txt"),
        ] {
            let refusal = refusal(&workspace, &refused).expect("refused");
            assert!(
                refusal.starts_with("The sandbox of workspace 1 does not allow writing"),
                "{refusal}"
            );
        }
        let home = dirs::home_dir().unwrap();
        if home.join(".ssh").is_dir() {
            let secret = home.join(".ssh").to_string_lossy().into_owned();
            let refusal = refusal(&workspace, &call("ls", &secret)).expect("refused");
            assert!(refusal.contains("does not allow reading"), "{refusal}");
        }
    }

    #[test]
    fn a_sandbox_whose_writable_directory_is_gone_refuses_rather_than_lapses() {
        let root = tempfile::tempdir().unwrap();
        let mut workspace = sandboxed(root.path());
        if let Some(sandbox) = workspace.sandbox.as_mut() {
            sandbox.policy.writable = vec![root.path().join("gone").to_string_lossy().into_owned()];
        }
        assert!(FileSandbox::of(&workspace).is_err());
        let refusal = refusal(&workspace, &call("read", ".")).expect("refused");
        assert!(refusal.contains("could not be set up"), "{refusal}");
    }
}
