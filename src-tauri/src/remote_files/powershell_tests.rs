//! The PowerShell forms of `grep` and `read` ([`crate::remote_powershell`]),
//! run for real by the PowerShell this machine has.
//!
//! A local PowerShell stands in for a Windows machine's agent shell the way
//! `tests::LocalBash` stands in for a POSIX one: the scripts are the ones a
//! machine would be sent, the answers are parsed by the same host code, and
//! each result is held to what the host leg says about the same files.
//!
//! Every test returns early, saying so, where there is no PowerShell to run:
//! anywhere but on Windows, whose paths the scripts are written for.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::*;
use crate::shell_backend::{AgentShell, ShellBackend};
use crate::workspace_set::WorkspaceSet;

/// The longest script handed over on the command line. Windows allows 32,767
/// characters for all of it, quoting included.
const COMMAND_LINE_ROOM: usize = 24_000;

/// A PowerShell on this machine, started exactly as a machine's agent starts
/// its agent shell ([`AgentShell::script_argv`]).
struct LocalPowerShell {
    shell: AgentShell,
    /// Send every script on standard input behind the bootstrap, the way a
    /// machine with no agent is sent one that does not fit its login shell's
    /// command line — which, PowerShell's encoding being what it is, is all
    /// of them.
    piped: bool,
}

impl LocalPowerShell {
    /// Each edition this machine has: Windows PowerShell 5.1, PowerShell 7.
    fn all() -> Vec<Self> {
        if !cfg!(windows) {
            println!("not Windows; skipping the PowerShell remote-file tests");
            return Vec::new();
        }
        let found = [ShellBackend::WindowsPowerShell, ShellBackend::Pwsh]
            .into_iter()
            .filter_map(|backend| {
                let program = run_environment::local_powershell_candidates(backend)
                    .into_iter()
                    .next()?;
                Some(Self {
                    shell: AgentShell::new(backend, program),
                    piped: false,
                })
            })
            .collect::<Vec<_>>();
        if found.is_empty() {
            println!("no PowerShell on this machine; skipping the PowerShell remote-file tests");
        }
        found
    }

    fn name(&self) -> &'static str {
        self.shell.backend.id()
    }
}

impl RemoteShell for LocalPowerShell {
    fn run(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        _timeout: Duration,
        _cancel: &CancelSignal,
    ) -> Result<RemoteCommandOutput, String> {
        // A script too long for a command line travels on standard input
        // behind the bootstrap that reads and runs it, as
        // `run_environment::run_remote_script` sends one. A script that reads
        // its own input is never that long.
        let bootstrap;
        let (script, stdin) = if stdin.is_none() && (self.piped || script.len() > COMMAND_LINE_ROOM) {
            bootstrap = crate::remote_powershell::run_script_from_input();
            (bootstrap.as_str(), Some(script.as_bytes()))
        } else {
            (script, stdin)
        };
        let argv = self.shell.script_argv(script);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed to start {}: {error}", self.shell.program))?;
        let writer = match (child.stdin.take(), stdin) {
            (Some(mut pipe), Some(bytes)) => {
                let bytes = bytes.to_vec();
                Some(std::thread::spawn(move || {
                    let _ = pipe.write_all(&bytes);
                }))
            }
            (pipe, _) => {
                drop(pipe);
                None
            }
        };
        let output = child
            .wait_with_output()
            .map_err(|error| format!("failed to read PowerShell's output: {error}"))?;
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        Ok(RemoteCommandOutput {
            status: output.status.code(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn dialect(&self) -> ScriptDialect {
        ScriptDialect::PowerShell
    }
}

fn input(value: Value) -> JsonObject {
    value.as_object().expect("object").clone()
}

fn git_available() -> bool {
    crate::environment_tools::resolve_on_path("git").is_some()
}

/// A temporary workspace on this machine, which is both the host leg's
/// workspace and the "remote" one PowerShell is sent to.
struct Fixture {
    _root: tempfile::TempDir,
    /// The workspace as a conversation records it: forward slashes.
    workspace: String,
    profile: PromptProfile,
    cancel: CancelSignal,
    state: crate::state::AppState,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let directory = root.path().join("ws");
        std::fs::create_dir_all(&directory).expect("workspace");
        Self {
            workspace: directory.to_string_lossy().replace('\\', "/"),
            _root: root,
            profile: PromptProfile::default(),
            cancel: CancelSignal::default(),
            state: crate::state::AppState::default(),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        std::path::Path::new(&self.workspace).join(relative)
    }

    fn write(&self, relative: &str, content: &[u8]) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture parent");
        }
        std::fs::write(path, content).expect("fixture file");
    }

    /// Dates a file `seconds` after the epoch, so the order of a file list
    /// does not hang on when the test wrote what.
    fn date(&self, relative: &str, seconds: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(self.path(relative))
            .expect("fixture file");
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
            .expect("fixture time");
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(&self.workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    /// The host leg's answer to the same call.
    fn host(&self, tool: &str, arguments: &Value) -> crate::model::ToolExecutionResponse {
        crate::tool_executor::execute(
            crate::model::ToolExecutionRequest {
                conversation_id: "conversation-test".into(),
                workspace_path: self.workspace.clone(),
                tool_name: tool.into(),
                input: input(arguments.clone()),
            },
            &self.state,
        )
    }

    /// The workspace as one on a Windows machine whose agent shell is `shell`.
    fn through<'a>(&'a self, shell: &'a LocalPowerShell) -> Machine<'a> {
        let runner = ShellRunner::Ssh {
            host: "windows-test".into(),
            port: 0,
            identity_file: String::new(),
            env: BTreeMap::new(),
            agent_shell: shell.shell.clone(),
        };
        Machine {
            fixture: self,
            shell,
            set: WorkspaceSet::single(self.workspace.clone(), runner),
        }
    }
}

/// One fixture reached through one PowerShell.
struct Machine<'a> {
    fixture: &'a Fixture,
    shell: &'a LocalPowerShell,
    set: WorkspaceSet,
}

impl Machine<'_> {
    fn target(&self, confinement: Confinement) -> RemoteWorkspace<'_> {
        RemoteWorkspace {
            workspace: self.set.primary().expect("one workspace"),
            machine_key: "ssh:windows-test".to_owned(),
            confinement,
            also: Vec::new(),
            sandbox: None,
            profile: &self.fixture.profile,
            cancel: &self.fixture.cancel,
        }
    }

    fn grep(&self, arguments: &Value) -> Result<String, String> {
        grep_with(
            self.shell,
            &self.target(Confinement::Workspace),
            &input(arguments.clone()),
        )
    }

    fn read(&self, arguments: &Value) -> Result<String, String> {
        self.read_counting(arguments, read_text::COUNT_SCAN_LIMIT)
    }

    /// A read that counts the lines of files up to `count_limit` bytes only.
    fn read_counting(&self, arguments: &Value, count_limit: u64) -> Result<String, String> {
        read_counting(
            self.shell,
            &self.target(Confinement::Workspace),
            &input(arguments.clone()),
            None,
            None,
            count_limit,
        )
        .map(|outcome| outcome.output)
    }

    /// The remote answer, which must be the host leg's.
    fn same_grep(&self, arguments: Value) -> String {
        let host = self.fixture.host("grep", &arguments);
        assert!(host.success, "{}: host {arguments}: {}", self.shell.name(), host.output);
        let remote = self
            .grep(&arguments)
            .unwrap_or_else(|error| panic!("{}: {arguments}: {error}", self.shell.name()));
        assert_eq!(remote, host.output, "{}: {arguments}", self.shell.name());
        remote
    }

    fn same_read(&self, arguments: Value) -> String {
        let host = self.fixture.host("read", &arguments);
        assert!(host.success, "{}: host {arguments}: {}", self.shell.name(), host.output);
        let remote = self
            .read(&arguments)
            .unwrap_or_else(|error| panic!("{}: {arguments}: {error}", self.shell.name()));
        assert_eq!(remote, host.output, "{}: {arguments}", self.shell.name());
        remote
    }

    /// The remote refusal, which must be the host leg's.
    fn same_read_refusal(&self, arguments: Value) -> String {
        let host = self.fixture.host("read", &arguments);
        assert!(!host.success, "{}: host {arguments}: {}", self.shell.name(), host.output);
        let remote = match self.read(&arguments) {
            Ok(output) => panic!("{}: {arguments}: read {output}", self.shell.name()),
            Err(error) => error,
        };
        assert_eq!(remote, host.output, "{}: {arguments}", self.shell.name());
        remote
    }

    /// One `grep` script's answer after its header and format line.
    fn grep_payload(&self, path: &str, pattern: &str, request: &GrepRequest<'_>) -> String {
        let target = self.target(Confinement::Workspace);
        let pattern = crate::remote_regex::translate(pattern, true).expect("pattern");
        let output = run_script(
            self.shell,
            &target,
            &grep_script(&target, path, &pattern, request, false).expect("script"),
            None,
            SEARCH_TIMEOUT,
            &ExitWording::new(path).grep(),
        )
        .unwrap_or_else(|error| panic!("{}: {error}", self.shell.name()));
        let (header, rest) = take_header(&output.stdout).expect("header");
        let (format, rest) = take_lines(rest, 1).expect("format");
        assert_eq!(format[0], "null", "{}", self.shell.name());
        // Paths are shown below the workspace here, so an expectation does not
        // have to know where the temporary directory is.
        String::from_utf8_lossy(rest).replace(&format!("{}/", header.root), "")
    }
}

const MAIN_RS: &str = "fn main() {\n    let needle = 1;\n    // a\n    // b\n    let other = needle + 1;\n    // c\n    // d\n    // e\n    // f\n    // g\n    // h\n    let third = needle * 2;\n}\n";

/// Sources, documents, a binary file, a dependency directory and a log, with
/// `needle` in most of them.
fn search_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("src/main.rs", MAIN_RS.as_bytes());
    fixture.write("src/lib.rs", b"pub fn needle() {}\n\npub fn hay() {}\n");
    fixture.write("src/ui/app.tsx", b"export const needle = 1;\nconst x = 2;\n");
    fixture.write("src/ui/deep/view.tsx", b"// needle here\r\n// second\r\n\r\n// needles\r\n");
    fixture.write("src/\u{4e2d}\u{6587}.rs", "// needle \u{4e2d}\u{6587}\n".as_bytes());
    fixture.write("web/index.ts", b"import { needle } from './x';\n");
    fixture.write("docs/guide.md", b"# Guide\n\nThe needle is here.\nNEEDLE again\nA Needle too");
    fixture.write("docs/notes.txt", b"needle in text\n");
    fixture.write("README.md", b"needle top\n");
    fixture.write("a_b.txt", b"needle with an underscore\n");
    fixture.write("ab.txt", b"needle without one\n");
    fixture.write("Zed.txt", b"needle in capitals\n");
    fixture.write("empty.txt", b"");
    fixture.write("data/blob.bin", b"needle\0binary\n");
    fixture.write("node_modules/pkg/index.js", b"needle in a dependency\n");
    fixture.write("debug.log", b"needle in a log\n");
    // Two files the list must not order by when the test happened to run.
    fixture.date("docs/guide.md", 1_600_000_000);
    fixture.date("web/index.ts", 1_500_000_000);
    fixture
}

/// Every call a search fixture is put through, on both legs.
fn search_cases() -> Vec<Value> {
    let mut cases = Vec::new();
    // Each mode, as it is and with each glob.
    for mode in ["files_with_matches", "content", "count"] {
        cases.push(json!({"pattern": "needle", "output_mode": mode}));
        for glob in ["*.rs", "src/**/*.tsx", "*.{ts,md}", "docs/*", "*.none"] {
            cases.push(json!({"pattern": "needle", "output_mode": mode, "glob": glob}));
        }
        cases.push(json!({"pattern": "needle", "output_mode": mode, "case_sensitive": false}));
        cases.push(json!({"pattern": "needle", "output_mode": mode, "path": "src"}));
        cases.push(json!({"pattern": "needle", "output_mode": mode, "path": "src", "glob": "ui/**/*.tsx"}));
        // A file named outright, which is searched whatever the glob says.
        cases.push(json!({"pattern": "needle", "output_mode": mode, "path": "docs/guide.md", "case_sensitive": false}));
        cases.push(json!({"pattern": "needle", "output_mode": mode, "path": "src/main.rs", "glob": "*.md"}));
        cases.push(json!({"pattern": r"\bneedle\b", "output_mode": mode, "path": "src/main.rs", "glob": "*.md"}));
        // A pattern .NET cannot say exactly, so the machine sends lines and
        // the files' times come from a second trip.
        cases.push(json!({"pattern": r"\bneedle\b", "output_mode": mode}));
        // An empty line is one between two newlines, never what follows the
        // last one.
        cases.push(json!({"pattern": "^$", "output_mode": mode}));
        cases.push(json!({"pattern": r"needles?$", "output_mode": mode}));
        cases.push(json!({"pattern": "no such text anywhere", "output_mode": mode}));
        // Pages.
        cases.push(json!({"pattern": "needle", "output_mode": mode, "limit": 2}));
        cases.push(json!({"pattern": "needle", "output_mode": mode, "limit": 2, "offset": 2}));
        cases.push(json!({"pattern": "needle", "output_mode": mode, "limit": 3, "offset": 4}));
        cases.push(json!({"pattern": "needle", "output_mode": mode, "limit": 50, "offset": 500}));
    }
    // Context: windows that overlap, windows with a gap between them, and
    // each side on its own.
    for context in [
        json!({"context": 2}),
        json!({"context": 1}),
        json!({"before": 1}),
        json!({"after": 3}),
        json!({"before": 3, "after": 1}),
        json!({"context": 20}),
        json!({"context": 2, "limit": 2}),
        json!({"context": 2, "limit": 2, "offset": 1}),
        json!({"context": 1, "glob": "*.{rs,md}", "case_sensitive": false}),
    ] {
        let mut arguments = json!({"pattern": "needle", "output_mode": "content"});
        for (key, value) in context.as_object().expect("object") {
            arguments[key] = value.clone();
        }
        cases.push(arguments);
    }
    cases
}

fn check_searches(fixture: &Fixture) {
    // The cases reach both ways a machine answers `files_with_matches` and
    // `count`: in summaries for a pattern .NET says exactly, and in lines for
    // the host to check for one it cannot.
    let exact = |pattern: &str| {
        crate::remote_regex::translate(pattern, true)
            .expect("pattern")
            .dotnet_exact
    };
    assert!(exact("needle") && exact("^$") && !exact(r"\bneedle\b"));
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        for arguments in search_cases() {
            machine.same_grep(arguments);
        }
        // The answers are not all "no match": the fixture is found.
        let listed = machine.same_grep(json!({"pattern": "needle", "glob": "*.{ts,md}"}));
        assert_eq!(listed, "README.md\ndocs/guide.md\nweb/index.ts", "{}", shell.name());
        let counted = machine.same_grep(json!({"pattern": "needle", "output_mode": "count", "glob": "src/**/*.tsx"}));
        assert!(counted.starts_with("src/ui/app.tsx:1\nsrc/ui/deep/view.tsx:2\n"), "{}: {counted}", shell.name());
        let context = machine.same_grep(json!({"pattern": "needle", "path": "src/main.rs", "output_mode": "content", "context": 2}));
        assert_eq!(
            context,
            "src/main.rs-1-fn main() {\nsrc/main.rs:2:    let needle = 1;\nsrc/main.rs-3-    // a\nsrc/main.rs-4-    // b\nsrc/main.rs:5:    let other = needle + 1;\nsrc/main.rs-6-    // c\nsrc/main.rs-7-    // d\n--\nsrc/main.rs-10-    // g\nsrc/main.rs-11-    // h\nsrc/main.rs:12:    let third = needle * 2;\nsrc/main.rs-13-}",
            "{}",
            shell.name()
        );
        let empty = machine.same_grep(json!({"pattern": "^$", "output_mode": "content", "path": "src"}));
        assert_eq!(empty, "src/lib.rs:2:\nsrc/ui/deep/view.tsx:3:", "{}", shell.name());
    }
}

/// The wildcards choose among the files under a directory, by the path below
/// it or by the name, whatever the case of either. A whole call cannot show
/// that — the host applies the glob to whatever comes back — so the script
/// is asked directly.
fn check_wildcards(machine: &Machine<'_>) {
    let wildcards = ["ui/*.tsx".to_owned(), "LIB.rs".to_owned()];
    let request = GrepRequest {
        shape: GrepShape::Counts,
        cap: 100,
        wildcards: Some(&wildcards),
    };
    let mut chosen = machine
        .grep_payload("src", "needle", &request)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    chosen.sort();
    assert_eq!(
        chosen,
        ["src/lib.rs:1", "src/ui/app.tsx:1", "src/ui/deep/view.tsx:2"],
        "{}",
        machine.shell.name()
    );
}

#[test]
fn powershell_greps_a_plain_directory_as_the_host_leg_does() {
    let fixture = search_fixture();
    check_searches(&fixture);
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        // In the walk.
        check_wildcards(&machine);
        // With no Git to ask, the dependency directory is left out by name
        // and the log is searched.
        let listed = machine.same_grep(json!({"pattern": "needle in a"}));
        assert_eq!(listed, "debug.log", "{}", shell.name());
        // The walk is in the host leg's order, byte by byte: capitals before
        // small letters, `_` before them too.
        let ordered = machine.same_grep(json!({"pattern": "needle", "output_mode": "content", "glob": "*.txt", "limit": 3}));
        assert!(
            ordered.starts_with("Zed.txt:1:needle in capitals\na_b.txt:1:needle with an underscore\nab.txt:1:needle without one\n"),
            "{}: {ordered}",
            shell.name()
        );
    }
}

#[test]
fn powershell_greps_a_repository_as_the_host_leg_does() {
    if !git_available() {
        println!("no git on this machine; skipping");
        return;
    }
    let fixture = search_fixture();
    fixture.git(&["init", "-q"]);
    fixture.write(".gitignore", b"node_modules/\n*.log\n");
    // Some of the files are tracked and the rest are not: Git lists both.
    fixture.git(&["add", ".gitignore", "src/main.rs", "docs"]);
    check_searches(&fixture);
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        // In the loop over Git's list.
        check_wildcards(&machine);
        // What Git ignores is not searched, unless it is named outright.
        let ignored = machine.same_grep(json!({"pattern": "needle in a"}));
        assert_eq!(ignored, fixture.profile.text(PromptKey::ToolGrepNoMatch), "{}", shell.name());
        let named = machine.same_grep(json!({"pattern": "needle in a", "path": "node_modules", "output_mode": "content"}));
        assert_eq!(named, "node_modules/pkg/index.js:1:needle in a dependency", "{}", shell.name());
        let named = machine.same_grep(json!({"pattern": "needle in a", "path": "debug.log", "output_mode": "content"}));
        assert_eq!(named, "debug.log:1:needle in a log", "{}", shell.name());
    }
}

/// What the scripts promise the host beyond what a whole call shows: the cap
/// counts every line, a file named outright is searched whatever the
/// wildcards say, and each shape is spelled as `parse_records`,
/// `parse_modified` and `parse_counts` read it.
#[test]
fn the_powershell_grep_script_keeps_its_cap_and_its_shapes() {
    let fixture = search_fixture();
    let modified = 1_700_000_000_u64;
    fixture.date("src/main.rs", modified);
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        let lines = |before, after, cap| GrepRequest {
            shape: GrepShape::Lines { before, after },
            cap,
            wildcards: None,
        };
        // Matches and context alike, each line once, a NUL after the name.
        let all = machine.grep_payload("src/main.rs", "needle", &lines(1, 1, 100));
        assert_eq!(
            all,
            "src/main.rs\01-fn main() {\nsrc/main.rs\02:    let needle = 1;\nsrc/main.rs\03-    // a\nsrc/main.rs\04-    // b\nsrc/main.rs\05:    let other = needle + 1;\nsrc/main.rs\06-    // c\nsrc/main.rs\011-    // h\nsrc/main.rs\012:    let third = needle * 2;\nsrc/main.rs\013-}\n",
            "{}",
            shell.name()
        );
        // The cap stops the answer wherever it falls, on a context line too.
        let cut = machine.grep_payload("src/main.rs", "needle", &lines(1, 1, 4));
        assert_eq!(
            cut,
            "src/main.rs\01-fn main() {\nsrc/main.rs\02:    let needle = 1;\nsrc/main.rs\03-    // a\nsrc/main.rs\04-    // b\n",
            "{}",
            shell.name()
        );
        let bare = machine.grep_payload("src", "needle", &lines(0, 0, 2));
        assert_eq!(bare.lines().count(), 2, "{}: {bare}", shell.name());

        let files = |cap, wildcards| GrepRequest {
            shape: GrepShape::Files,
            cap,
            wildcards,
        };
        let listed = machine.grep_payload("src/main.rs", "needle", &files(100, None));
        assert_eq!(listed, format!("{modified} src/main.rs\n"), "{}", shell.name());
        let capped = machine.grep_payload(".", "needle", &files(3, None));
        assert_eq!(capped.lines().count(), 3, "{}: {capped}", shell.name());

        let counts = |wildcards| GrepRequest {
            shape: GrepShape::Counts,
            cap: 100,
            wildcards,
        };
        let counted = machine.grep_payload("src/main.rs", "needle", &counts(None));
        assert_eq!(counted, "src/main.rs:3\n", "{}", shell.name());
        let mut below = machine
            .grep_payload("src", "needle", &counts(None))
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        below.sort();
        assert_eq!(
            below,
            ["src/lib.rs:1", "src/main.rs:3", "src/ui/app.tsx:1", "src/ui/deep/view.tsx:2", "src/\u{4e2d}\u{6587}.rs:1"],
            "{}",
            shell.name()
        );

        // A file named outright is searched whatever the wildcards say.
        let wildcards = ["*.md".to_owned()];
        let named = machine.grep_payload("src/main.rs", "needle", &counts(Some(&wildcards)));
        assert_eq!(named, "src/main.rs:3\n", "{}", shell.name());
        let named = machine.grep_payload("src/main.rs", "needle", &files(100, Some(&wildcards)));
        assert_eq!(named, format!("{modified} src/main.rs\n"), "{}", shell.name());

        // A pattern .NET refuses exits 2 with what it said.
        let target = machine.target(Confinement::Workspace);
        let script = crate::remote_powershell::grep(
            &powershell(&target).expect("target").expect("a PowerShell machine"),
            ".",
            "(",
            &lines(0, 0, 10),
        );
        let refused = shell
            .run(&script, None, SEARCH_TIMEOUT, &fixture.cancel)
            .expect("run");
        assert_eq!(refused.status, Some(EXIT_BAD_PATTERN), "{}: {}", shell.name(), refused.stderr);
        assert!(!refused.stderr.trim().is_empty(), "{}", shell.name());
    }
}

/// A file something else holds open is named among what was skipped, and the
/// rest is searched.
#[cfg(windows)]
#[test]
fn powershell_grep_reports_a_file_it_cannot_open() {
    use std::os::windows::fs::OpenOptionsExt as _;
    let fixture = Fixture::new();
    fixture.write("open.txt", b"needle here\n");
    fixture.write("locked.txt", b"needle there\n");
    // No sharing at all: nothing else may open the file while this is held.
    let _held = std::fs::File::options()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(fixture.path("locked.txt"))
        .expect("lock");
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        let found = machine
            .grep(&json!({"pattern": "needle", "output_mode": "content"}))
            .unwrap_or_else(|error| panic!("{}: {error}", shell.name()));
        let mut lines = found.lines();
        assert_eq!(lines.next(), Some("open.txt:1:needle here"), "{}: {found}", shell.name());
        let skipped = lines.next().unwrap_or_default();
        assert!(
            skipped.starts_with("[skipped] grep: ") && skipped.contains("/locked.txt: "),
            "{}: {found}",
            shell.name()
        );
    }
}

/// The end of a long answer, for a failure message.
fn tail(text: &str) -> &str {
    let mut start = text.len().saturating_sub(240);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// `count` lines of `width` bytes each, numbered from 1, each ending in
/// `ending` — the last one too only with `closed`.
fn numbered(count: usize, width: usize, ending: &str, closed: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(count * width);
    for number in 1..=count {
        let line = format!("line {number:09}");
        bytes.extend_from_slice(line.as_bytes());
        bytes.resize(bytes.len() + width - line.len() - ending.len(), b'x');
        bytes.extend_from_slice(ending.as_bytes());
    }
    if !closed {
        bytes.truncate(bytes.len() - ending.len());
    }
    bytes
}

fn utf16(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[test]
fn powershell_reads_files_sent_whole_as_the_host_leg_does() {
    let fixture = Fixture::new();
    fixture.write("notes.txt", b"alpha\nbeta\ngamma\ndelta\n");
    fixture.write("dos.txt", b"one\r\ntwo\r\nthree");
    fixture.write("empty.txt", b"");
    fixture.write("latin.txt", b"caf\xe9 au lait\nplain\n");
    fixture.write("wide.txt", &utf16("h\u{e9}llo \u{1f600}\nsecond line\n"));
    fixture.write("blob.bin", b"MZ\x90\0\x03\0\0\0binary");
    fixture.write("sub/it's here.txt", "caf\u{e9} \u{4e2d}\u{6587}\n".as_bytes());
    fixture.write("many.txt", &numbered(3000, 16, "\n", true));
    std::fs::write(fixture.path("../outside.txt"), b"outside\n").expect("outside");
    // A file something is still writing — a log — reads like any other.
    fixture.write("live.log", b"started\nrunning\n");
    let _writer = std::fs::File::options()
        .append(true)
        .open(fixture.path("live.log"))
        .expect("writer");
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        let whole = machine.same_read(json!({"path": "notes.txt"}));
        assert_eq!(whole, "alpha\nbeta\ngamma\ndelta", "{}", shell.name());
        let live = machine.same_read(json!({"path": "live.log"}));
        assert_eq!(live, "started\nrunning", "{}", shell.name());
        let ranged = machine.same_read(json!({"path": "notes.txt", "start_line": 2, "end_line": 3}));
        assert_eq!(ranged, "beta\ngamma", "{}", shell.name());
        machine.same_read(json!({"path": "notes.txt", "start_line": 4}));
        machine.same_read(json!({"path": "dos.txt"}));
        machine.same_read(json!({"path": "dos.txt", "start_line": 3}));
        machine.same_read(json!({"path": "sub/it's here.txt"}));

        let empty = machine.same_read(json!({"path": "empty.txt"}));
        assert_eq!(empty, fixture.profile.text(PromptKey::ToolReadEmptyFile), "{}", shell.name());
        let past = machine.same_read(json!({"path": "notes.txt", "start_line": 99}));
        assert_eq!(
            past,
            fixture.profile.render(PromptKey::ToolReadRangeOutOfBounds, &[("total", "4")]),
            "{}",
            shell.name()
        );

        let lossy = machine.same_read(json!({"path": "latin.txt"}));
        assert!(lossy.starts_with("caf\u{fffd} au lait\nplain"), "{}: {lossy}", shell.name());
        assert!(lossy.ends_with(fixture.profile.text(PromptKey::ToolReadLossyNote)), "{}: {lossy}", shell.name());
        let wide = machine.same_read(json!({"path": "wide.txt"}));
        assert!(wide.starts_with("h\u{e9}llo \u{1f600}\nsecond line"), "{}: {wide}", shell.name());
        assert!(wide.contains("UTF-16LE"), "{}: {wide}", shell.name());
        let binary = machine.same_read_refusal(json!({"path": "blob.bin"}));
        assert_eq!(
            binary,
            fixture.profile.render(PromptKey::ToolReadBinary, &[("path", "blob.bin")]),
            "{}",
            shell.name()
        );

        // More lines than one read returns: the note names the total.
        let many = machine.same_read(json!({"path": "many.txt"}));
        assert!(
            many.ends_with(&fixture.profile.render(
                PromptKey::ToolReadLimit,
                &[("from", "1"), ("to", "2000"), ("total", "3000"), ("next", "2001")]
            )),
            "{}",
            shell.name()
        );
        machine.same_read(json!({"path": "many.txt", "start_line": 2001}));

        // The reserved exit codes read as they do on a POSIX machine.
        let refusal = |arguments: Value| match machine.read(&arguments) {
            Ok(output) => panic!("{}: {arguments}: read {output}", shell.name()),
            Err(error) => error,
        };
        assert_eq!(refusal(json!({"path": "sub"})), "read target is not a file: sub", "{}", shell.name());
        assert_eq!(refusal(json!({"path": "nope.txt"})), "No such file or directory: nope.txt", "{}", shell.name());
        let outside = refusal(json!({"path": "../outside.txt"}));
        assert!(outside.starts_with("../outside.txt is outside workspace 1"), "{}: {outside}", shell.name());
        let reached = read_with(
            &shell,
            &machine.target(Confinement::Machine),
            &input(json!({"path": "../outside.txt"})),
            None,
            None,
        )
        .map(|outcome| outcome.output);
        assert_eq!(reached.as_deref(), Ok("outside"), "{}", shell.name());
    }
}

/// A file past the size sent whole is cut on the machine, by streaming: every
/// range reads as the host leg reads it from the file itself.
#[test]
fn powershell_reads_ranges_of_a_large_file_as_the_host_leg_does() {
    const LINES: usize = 420_000;
    let fixture = Fixture::new();
    // 6.4 MiB of sixteen-byte lines, a hundred-byte kind that fills a read's
    // bytes long before its lines, and CRLF text whose last line is open.
    fixture.write("big.log", &numbered(LINES, 16, "\n", true));
    fixture.write("wide.log", &numbered(66_000, 100, "\n", true));
    fixture.write("dos.log", &numbered(LINES, 16, "\r\n", false));
    // And one whose second line alone is 300,000 bytes.
    let mut giant = b"first\n".to_vec();
    giant.extend_from_slice("\u{e9}".repeat(150_000).as_bytes());
    giant.extend_from_slice(b"\nlast\n");
    giant.extend_from_slice(&numbered(LINES, 16, "\n", true));
    fixture.write("giant.txt", &giant);
    assert!(std::fs::metadata(fixture.path("big.log")).expect("big").len() > MAX_IMAGE_ATTACHMENT_BYTES as u64);
    // Something is still writing the log, as something usually is.
    let _writer = std::fs::File::options()
        .append(true)
        .open(fixture.path("big.log"))
        .expect("writer");
    let line = |number: usize| format!("line {number:09}x");
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        // The start: a read's worth of lines, and the total the machine
        // counted.
        let start = machine.same_read(json!({"path": "big.log"}));
        assert!(start.starts_with(&format!("{}\n{}\n", line(1), line(2))), "{}", shell.name());
        assert!(
            start.ends_with(&format!(
                "{}{}",
                line(2000),
                fixture.profile.render(
                    PromptKey::ToolReadLimit,
                    &[("from", "1"), ("to", "2000"), ("total", &LINES.to_string()), ("next", "2001")]
                )
            )),
            "{}: {}",
            shell.name(),
            tail(&start)
        );
        // The middle.
        let middle = machine.same_read(json!({"path": "big.log", "start_line": 200_000, "end_line": 200_002}));
        assert_eq!(middle, [line(200_000), line(200_001), line(200_002)].join("\n"), "{}", shell.name());
        machine.same_read(json!({"path": "big.log", "start_line": 123_456}));
        machine.same_read(json!({"path": "big.log", "start_line": 300_000, "end_line": 309_000}));
        // The end, and one line either side of it.
        let end = machine.same_read(json!({"path": "big.log", "start_line": LINES - 2}));
        assert_eq!(end, [line(LINES - 2), line(LINES - 1), line(LINES)].join("\n"), "{}", shell.name());
        assert_eq!(machine.same_read(json!({"path": "big.log", "start_line": LINES})), line(LINES), "{}", shell.name());
        machine.same_read(json!({"path": "big.log", "start_line": LINES - 2000}));
        machine.same_read(json!({"path": "big.log", "start_line": LINES - 1999}));
        machine.same_read(json!({"path": "big.log", "start_line": LINES - 5, "end_line": LINES + 5}));
        // Past the end.
        let out_of_range = fixture
            .profile
            .render(PromptKey::ToolReadRangeOutOfBounds, &[("total", &LINES.to_string())]);
        for arguments in [
            json!({"path": "big.log", "start_line": LINES + 1}),
            json!({"path": "big.log", "start_line": LINES * 3, "end_line": LINES * 3 + 10}),
            json!({"path": "big.log", "start_line": u64::MAX}),
            json!({"path": "dos.log", "start_line": LINES + 1}),
        ] {
            assert_eq!(machine.same_read(arguments), out_of_range, "{}", shell.name());
        }

        // Lines that fill a read's bytes first: the machine sends no more
        // than the host can use, and the host says where it stopped.
        let wide = machine.same_read(json!({"path": "wide.log"}));
        assert!(wide.contains("showing lines 1–614 of 66000"), "{}: {}", shell.name(), tail(&wide));
        machine.same_read(json!({"path": "wide.log", "start_line": 615}));
        machine.same_read(json!({"path": "wide.log", "start_line": 65_000}));
        machine.same_read(json!({"path": "wide.log", "start_line": 65_950}));
        machine.same_read(json!({"path": "wide.log", "start_line": 30_000, "end_line": 36_000}));

        // A line longer than any read is left for a read of its own, where
        // it is cut: by the machine to what it may send, and then by the host
        // to what it shows, which is the same text as the host leg shows.
        let before = machine.same_read(json!({"path": "giant.txt"}));
        assert!(before.starts_with("first\n… showing lines 1–1 of "), "{}: {}", shell.name(), tail(&before));
        assert_eq!(
            machine.same_read(json!({"path": "giant.txt", "start_line": 3, "end_line": 4})),
            format!("last\n{}", line(1)),
            "{}",
            shell.name()
        );
        let arguments = json!({"path": "giant.txt", "start_line": 2, "end_line": 2});
        let cut = machine
            .read(&arguments)
            .unwrap_or_else(|error| panic!("{}: {arguments}: {error}", shell.name()));
        let shown = |text: &str| text.chars().take_while(|character| *character == '\u{e9}').count();
        assert_eq!(shown(&cut), shown(&fixture.host("read", &arguments).output), "{}", shell.name());
        assert!(shown(&cut) > 20_000 && cut.len() > shown(&cut) * 2, "{}: {}", shell.name(), tail(&cut));
        assert!(cut.len() < read_text::READ_MAX_BYTES, "{}: {}", shell.name(), cut.len());

        // CRLF, and a last line with no newline after it.
        machine.same_read(json!({"path": "dos.log"}));
        let open = machine.same_read(json!({"path": "dos.log", "start_line": LINES - 1}));
        assert_eq!(open, format!("line {:09}\nline {:09}", LINES - 1, LINES), "{}", shell.name());

        // A file too big to count for a number: the note says so, unless the
        // read reached the end — which the machine finds out by looking for
        // one more line — or started past it, which only a count can answer.
        let uncounted = |arguments: Value| {
            machine
                .read_counting(&arguments, 1024 * 1024)
                .unwrap_or_else(|error| panic!("{}: {arguments}: {error}", shell.name()))
        };
        let limit = |from: usize, to: usize| {
            fixture.profile.render(
                PromptKey::ToolReadLimitUncounted,
                &[("from", &from.to_string()), ("to", &to.to_string()), ("next", &(to + 1).to_string())],
            )
        };
        let head = uncounted(json!({"path": "big.log"}));
        assert!(head.starts_with(&line(1)) && head.ends_with(&format!("{}{}", line(2000), limit(1, 2000))), "{}", shell.name());
        // Two lines left after the read, then one, then none.
        for (first, more) in [(LINES - 2001, true), (LINES - 2000, true), (LINES - 1999, false), (LINES - 10, false)] {
            let read = uncounted(json!({"path": "big.log", "start_line": first}));
            let last = (first + 1999).min(LINES);
            let lines = (first..=last).map(line).collect::<Vec<_>>().join("\n");
            let expected = if more { format!("{lines}{}", limit(first, last)) } else { lines };
            assert_eq!(read, expected, "{}: from {first}", shell.name());
        }
        assert_eq!(
            uncounted(json!({"path": "big.log", "start_line": 1000, "end_line": 1001})),
            [line(1000), line(1001)].join("\n"),
            "{}",
            shell.name()
        );
        assert_eq!(uncounted(json!({"path": "big.log", "start_line": LINES + 1})), out_of_range, "{}", shell.name());
        assert_eq!(uncounted(json!({"path": "dos.log", "start_line": LINES + 7})), out_of_range, "{}", shell.name());
        let wide = uncounted(json!({"path": "wide.log", "start_line": 700}));
        assert!(wide.ends_with(&limit(700, 1313)), "{}: {}", shell.name(), tail(&wide));
    }
}

/// The slice as the script itself answers it, before the host reads anything
/// into it: the line count or `-`, whether the file goes on, and the lines'
/// bytes — one more than `max_bytes` of them at most.
#[test]
fn the_powershell_read_script_cuts_the_slice_it_is_asked_for() {
    const COUNTED: u64 = 64 * 1024 * 1024;
    const UNCOUNTED: u64 = 1024;
    let fixture = Fixture::new();
    fixture.write("closed.log", &numbered(420_000, 16, "\n", true));
    fixture.write("open.log", &numbered(420_000, 16, "\n", false));
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        let answer = |path: &str, first_line: u64, last_line: u64, max_bytes: usize, count_limit: u64| {
            let target = machine.target(Confinement::Workspace);
            let slice = ReadSlice {
                first_line,
                last_line,
                max_bytes,
                count_limit,
            };
            let output = run_script(
                &shell,
                &target,
                &read_script(&target, path, &slice).expect("script"),
                None,
                FILE_TIMEOUT,
                &ExitWording::new(path),
            )
            .unwrap_or_else(|error| panic!("{}: {error}", shell.name()));
            let (_, rest) = take_header(&output.stdout).expect("header");
            let (meta, rest) = take_lines(rest, 2).expect("time and size");
            let size = std::fs::metadata(fixture.path(path)).expect("file").len();
            assert_eq!(meta[1], size.to_string(), "{}", shell.name());
            String::from_utf8_lossy(rest).into_owned()
        };
        let name = shell.name();
        assert_eq!(answer("closed.log", 3, 4, 1000, COUNTED), "slice\n420000\n1\nline 000000003x\nline 000000004x\n", "{name}");
        assert_eq!(answer("closed.log", 3, 4, 1000, UNCOUNTED), "slice\n-\n1\nline 000000003x\nline 000000004x\n", "{name}");
        // Twenty bytes allowed: twenty-one sent.
        assert_eq!(answer("closed.log", 3, 9, 20, COUNTED), "slice\n420000\n1\nline 000000003x\nline ", "{name}");
        // Up to the last line, and up to the one before it.
        assert_eq!(answer("closed.log", 419_999, 420_000, 1000, COUNTED), "slice\n420000\n0\nline 000419999x\nline 000420000x\n", "{name}");
        assert_eq!(answer("closed.log", 419_999, 420_000, 1000, UNCOUNTED), "slice\n-\n0\nline 000419999x\nline 000420000x\n", "{name}");
        assert_eq!(answer("closed.log", 419_999, 419_999, 1000, UNCOUNTED), "slice\n-\n1\nline 000419999x\n", "{name}");
        // A last line with no newline is a line, sent as it is.
        assert_eq!(answer("open.log", 420_000, 420_050, 1000, COUNTED), "slice\n420000\n0\nline 000420000x", "{name}");
        assert_eq!(answer("open.log", 420_000, 420_050, 1000, UNCOUNTED), "slice\n-\n0\nline 000420000x", "{name}");
        assert_eq!(answer("open.log", 419_999, 419_999, 1000, UNCOUNTED), "slice\n-\n1\nline 000419999x\n", "{name}");
        // Past the end the lines are counted whatever the file's size, and
        // none are sent.
        for path in ["closed.log", "open.log"] {
            assert_eq!(answer(path, 420_001, 420_050, 1000, UNCOUNTED), "slice\n420000\n0\n", "{name}: {path}");
            assert_eq!(answer(path, u64::MAX, u64::MAX, 1000, UNCOUNTED), "slice\n420000\n0\n", "{name}: {path}");
        }
        // Where the bytes ran out before the lines did, whether the file
        // goes on is still found by looking.
        assert_eq!(answer("closed.log", 419_990, 419_999, 20, UNCOUNTED), "slice\n-\n1\nline 000419990x\nline ", "{name}");
        assert_eq!(answer("closed.log", 419_990, 420_000, 20, UNCOUNTED), "slice\n-\n0\nline 000419990x\nline ", "{name}");
    }
}

/// A machine with no agent is sent its scripts on standard input, behind a
/// bootstrap that reads and runs them. They answer the same from there, exit
/// codes included, and a script with input of its own (the files' times) is
/// still sent as itself.
#[test]
fn the_powershell_scripts_answer_the_same_from_standard_input() {
    let fixture = search_fixture();
    fixture.write("big.log", &numbered(420_000, 16, "\n", true));
    for mut shell in LocalPowerShell::all() {
        shell.piped = true;
        let machine = fixture.through(&shell);
        for arguments in [
            json!({"pattern": "needle"}),
            json!({"pattern": r"\bneedle\b"}),
            json!({"pattern": "needle", "output_mode": "count", "glob": "src/**/*.tsx"}),
            json!({"pattern": "needle", "output_mode": "content", "context": 1, "glob": "*.rs", "limit": 3}),
        ] {
            machine.same_grep(arguments);
        }
        machine.same_read(json!({"path": "src/main.rs"}));
        machine.same_read(json!({"path": "big.log", "start_line": 400_000, "end_line": 400_010}));
        machine.same_read(json!({"path": "big.log", "start_line": 500_000}));
        let refused = match machine.read(&json!({"path": "src"})) {
            Ok(output) => panic!("{}: read {output}", shell.name()),
            Err(error) => error,
        };
        assert_eq!(refused, "read target is not a file: src", "{}", shell.name());
    }
}

/// What is past the size sent whole and is not text in a byte encoding is
/// told apart on the machine, from the file's first bytes.
#[test]
fn powershell_tells_what_a_large_file_is_before_cutting_it() {
    let size = MAX_IMAGE_ATTACHMENT_BYTES + 4096;
    let fixture = Fixture::new();
    let mut binary = numbered(size / 16, 16, "\n", true);
    binary[5000] = 0;
    fixture.write("big.bin", &binary);
    // A NUL past what is sniffed does not make a file binary.
    let mut late = numbered(size / 16, 16, "\n", true);
    late[100_014] = 0;
    fixture.write("late.txt", &late);
    let mut wide = utf16("first line\n");
    wide.resize(size, 0);
    fixture.write("big16.txt", &wide);
    for (name, signature) in [
        ("big.png", &b"\x89PNG\r\n\x1a\n"[..]),
        ("big.jpg", &b"\xff\xd8\xff\xe0"[..]),
        ("big.gif", &b"GIF89a"[..]),
        ("big.webp", &b"RIFF\x24\0\0\0WEBPVP8 "[..]),
    ] {
        let mut image = signature.to_vec();
        image.resize(size, 0);
        fixture.write(name, &image);
    }
    for shell in LocalPowerShell::all() {
        let machine = fixture.through(&shell);
        let refusal = |arguments: Value| match machine.read(&arguments) {
            Ok(output) => panic!("{}: {arguments}: read {}", shell.name(), &output[..output.len().min(200)]),
            Err(error) => error,
        };
        let binary = machine.same_read_refusal(json!({"path": "big.bin", "start_line": 5}));
        assert_eq!(
            binary,
            fixture.profile.render(PromptKey::ToolReadBinary, &[("path", "big.bin")]),
            "{}",
            shell.name()
        );
        // Read as text, the NUL (in line 6251) is one more character of it.
        let late = machine.same_read(json!({"path": "late.txt", "start_line": 5700, "end_line": 6300}));
        assert!(late.contains("line 000006251\0"), "{}", shell.name());

        let wide = refusal(json!({"path": "big16.txt"}));
        assert!(
            wide.starts_with("big16.txt is UTF-16 text larger than 5 MiB, which cannot be read in ranges on a remote machine."),
            "{}: {wide}",
            shell.name()
        );
        for name in ["big.png", "big.jpg", "big.gif", "big.webp"] {
            assert_eq!(
                refusal(json!({"path": name, "start_line": 1, "end_line": 5})),
                "File exceeds the 5 MiB limit",
                "{}: {name}",
                shell.name()
            );
        }
    }
}
