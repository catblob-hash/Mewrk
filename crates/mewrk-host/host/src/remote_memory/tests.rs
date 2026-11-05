use super::*;
use crate::mewrk_memory::{
    create_document, edit_document, execute_tool, read_document, read_index, remote_project_root,
    MemoryRoots, MemoryTierAccess,
};
use crate::prompt_profile::PromptProfile;
use crate::remote_files::tests::LocalBash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// -- answers ----------------------------------------------------------------

#[test]
fn a_snapshot_reads_the_index_bytes_and_the_document_names() {
    let mut bytes = b"mewrk-memory 1\nready\nindex\n1700000000 123 9\n9\n".to_vec();
    bytes.extend_from_slice(b"# idx\nE\n\n");
    bytes.extend_from_slice(b"a.md\nb.md\nE\n");
    let snapshot = parse_snapshot(&bytes).unwrap();
    assert!(!snapshot.occupied);
    assert_eq!(
        snapshot.index,
        Index::Present {
            fingerprint: "1700000000 123 9".into(),
            bytes: Some(b"# idx\nE\n\n".to_vec()),
        }
    );
    assert_eq!(
        snapshot.documents.into_iter().collect::<Vec<_>>(),
        ["a.md", "b.md"]
    );

    let empty = parse_snapshot(b"mewrk-memory 1\nempty\nE\n").unwrap();
    assert_eq!(empty.index, Index::Absent);
    assert_eq!(empty.index.expected(), ABSENT);
    assert!(parse_snapshot(b"mewrk-memory 1\noccupied\nE\n").unwrap().occupied);
    let large = parse_snapshot(b"mewrk-memory 1\nready\nlarge\n17 1 2\nE\n").unwrap();
    assert_eq!(large.index.expected(), "17 1 2");
    assert!(parse_snapshot(&bytes[..30]).is_err());
    assert!(parse_snapshot(b"mewrk-memory 1\nready\nindex\n1;rm -rf\n0\nE\n").is_err());
}

#[test]
fn every_operand_reaches_the_scripts_quoted() {
    let write = posix_write("it's.md", "17 2 3");
    assert!(write.contains("D=\"$M\"/'it'\\''s.md'"), "{write}");
    assert!(write.contains("FP='17 2 3'"), "{write}");
    for code in ["exit 67", "exit 69", "exit 70"] {
        assert!(write.contains(code), "{code}");
    }
    assert!(posix_read("a.md", 10).contains("exit 66"));
    assert!(posix_read("a.md", 10).contains("exit 68"));
    assert!(fingerprint_is_sane("1700000000 4294967295 12"));
    assert!(!fingerprint_is_sane("1'; rm -rf / #"));

    let ps = crate::remote_powershell::memory_write("C:/w/it's", "it\u{2019}s.md", "17 abc");
    assert!(ps.contains("Native-Path 'C:/w/it''s'"), "{ps}");
    assert!(ps.contains("'it\u{2019}\u{2019}s.md'"), "{ps}");
    assert!(ps.contains("$MewrkExpected = '17 abc'"), "{ps}");
    for code in ["Quit 67", "Quit 69", "Quit 70"] {
        assert!(ps.contains(code), "{code}");
    }
    let ps = crate::remote_powershell::memory_read("C:/w", "a.md", 10);
    assert!(ps.contains("Quit 66") && ps.contains("Quit 68"), "{ps}");
    let ps = crate::remote_powershell::memory_snapshot("C:/w", 65536);
    for state in ["'occupied'", "'empty'", "'ready'", "'absent'", "'other'", "'index'", "'large'", "'E'"] {
        assert!(ps.contains(&format!("Out-Line {state}")), "{state}");
    }
    assert!(ps.contains("-le 65536"));
    let ps = crate::remote_powershell::memory_remove("C:/w", "a.md", "17 abc");
    assert!(ps.contains("-ne '17 abc'"), "{ps}");
}

/// PowerShell names ignore case: nothing a memory script adds may assign the
/// helpers' prologue variables in any spelling.
#[test]
fn the_powershell_memory_scripts_keep_clear_of_the_prologue_variables() {
    let assigned = regex::Regex::new(r"(?im)^\s*\$(c|t|root|req)\s*=").unwrap();
    for script in [
        crate::remote_powershell::memory_snapshot("C:/w", 10),
        crate::remote_powershell::memory_read("C:/w", "a.md", 10),
        crate::remote_powershell::memory_write("C:/w", "a.md", "absent"),
        crate::remote_powershell::memory_remove("C:/w", "a.md", "1 a"),
    ] {
        let body = &script[script.find("$MewrkTier =").expect("prologue")..];
        assert!(!assigned.is_match(body), "{body}");
    }
}

// -- the scripts, for real --------------------------------------------------

/// A workspace on a machine played by a local POSIX shell.
struct Workspace {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn place(&self, shell: Arc<dyn RemoteShell + Send + Sync>) -> RemoteMemoryPlace {
        RemoteMemoryPlace {
            shell,
            machine: "ssh:memory-test".into(),
            label: "memory-test".into(),
            root: self.root.to_string_lossy().into_owned(),
        }
    }

    fn memory(&self, name: &str) -> PathBuf {
        self.root.join(".mewrk/memory").join(name)
    }
}

fn bash() -> Option<Arc<dyn RemoteShell + Send + Sync>> {
    LocalBash::find().map(|shell| Arc::new(shell) as Arc<dyn RemoteShell + Send + Sync>)
}

#[test]
fn project_memory_on_another_machine_is_written_read_and_listed_there() {
    let Some(shell) = bash() else { return };
    let workspace = Workspace::new();
    let root = remote_project_root(workspace.place(shell.clone()));

    let created = create_document(&root, "build", "cargo test needs the env.", "build command").unwrap();
    assert_eq!(created.name, "build.md");
    assert_eq!(
        std::fs::read_to_string(workspace.memory("build.md")).unwrap(),
        "cargo test needs the env."
    );
    let index = std::fs::read_to_string(workspace.memory("MEMORY.md")).unwrap();
    assert!(index.contains("- [build.md](build.md) — build command"), "{index}");
    assert!(create_document(&root, "build", "again", "again")
        .unwrap_err()
        .contains("already exists"));

    edit_document(&root, "build", "needs the env", "needs the bundled env", "build command, edited").unwrap();
    assert_eq!(
        read_document(&root, "build").unwrap().content,
        "cargo test needs the bundled env."
    );
    // Another conversation of the same workspace shares it.
    let other = remote_project_root(workspace.place(shell));
    let entries = read_index(&other);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].description, "build command, edited");

    let roots = MemoryRoots {
        global: None,
        projects: vec![crate::mewrk_memory::ProjectMemory {
            workspace: 1,
            path: "/srv/app".into(),
            root: other,
        }],
        access: MemoryTierAccess::ALL,
    };
    let profile = PromptProfile::default();
    let context = roots.render_context(&profile).unwrap();
    assert!(context.contains("[build.md](build.md) — build command, edited"), "{context}");
    let input = serde_json::json!({ "name": "build" });
    assert_eq!(
        execute_tool(&roots, "read_project_memory", input.as_object().unwrap(), &profile).unwrap(),
        "cargo test needs the bundled env."
    );

    // A document deleted on the machine leaves the index the model sees.
    std::fs::remove_file(workspace.memory("build.md")).unwrap();
    assert!(roots.render_context(&profile).is_none());
    assert!(read_document(roots.project().unwrap(), "build")
        .unwrap_err()
        .contains("No memory document named build.md"));
}

/// Runs another writer's change just before the scripts that write `name`,
/// as many times as asked: a second Mewrk, or a person, at that moment.
struct Interfering {
    inner: Arc<dyn RemoteShell + Send + Sync>,
    name: &'static str,
    times: AtomicUsize,
    change: Box<dyn Fn() + Send + Sync>,
}

impl RemoteShell for Interfering {
    fn run(
        &self,
        script: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
        cancel: &CancelSignal,
    ) -> Result<RemoteCommandOutput, String> {
        let writes = script.contains(&format!("D=\"$M\"/'{}'", self.name)) && script.contains("FP=");
        if writes
            && self
                .times
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
                .is_ok()
        {
            (self.change)();
        }
        self.inner.run(script, stdin, timeout, cancel)
    }
}

fn interfering(
    inner: Arc<dyn RemoteShell + Send + Sync>,
    name: &'static str,
    times: usize,
    change: impl Fn() + Send + Sync + 'static,
) -> Arc<dyn RemoteShell + Send + Sync> {
    Arc::new(Interfering {
        inner,
        name,
        times: AtomicUsize::new(times),
        change: Box::new(change),
    })
}

fn other_writer_adds(memory: &Path, name: &str) -> impl Fn() + Send + Sync + 'static {
    let memory = memory.to_path_buf();
    let name = name.to_owned();
    move || {
        std::fs::write(memory.join(&name), "theirs").unwrap();
        let index = memory.join("MEMORY.md");
        let mut text = std::fs::read_to_string(&index).unwrap_or_default();
        text.push_str(&format!("- [{name}]({name}) — added elsewhere {}\n", text.len()));
        std::fs::write(index, text).unwrap();
    }
}

#[test]
fn an_index_that_changed_underneath_is_read_again_and_both_entries_kept() {
    let Some(shell) = bash() else { return };
    let workspace = Workspace::new();
    create_document(&remote_project_root(workspace.place(shell.clone())), "first", "one", "first").unwrap();
    let memory = workspace.root.join(".mewrk/memory");
    let shell = interfering(shell, "MEMORY.md", 1, other_writer_adds(&memory, "theirs.md"));
    let root = remote_project_root(workspace.place(shell));

    create_document(&root, "mine", "two", "mine").unwrap();
    let names = read_index(&root)
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["first.md", "theirs.md", "mine.md"]);
}

#[test]
fn a_create_whose_index_never_settles_takes_its_document_back() {
    let Some(shell) = bash() else { return };
    let workspace = Workspace::new();
    let memory = workspace.root.join(".mewrk/memory");
    let shell = interfering(shell, "MEMORY.md", usize::MAX, other_writer_adds(&memory, "theirs.md"));
    let root = remote_project_root(workspace.place(shell));

    let error = create_document(&root, "mine", "two", "mine").unwrap_err();
    assert!(error.contains("kept changing"), "{error}");
    assert!(!workspace.memory("mine.md").exists(), "the document went with the index");
    assert!(workspace.memory("theirs.md").exists(), "never someone else's");
}

#[test]
fn an_edit_of_a_document_changed_since_it_was_read_is_refused() {
    let Some(shell) = bash() else { return };
    let workspace = Workspace::new();
    create_document(&remote_project_root(workspace.place(shell.clone())), "notes", "alpha beta", "notes").unwrap();
    let document = workspace.memory("notes.md");
    let shell = interfering(shell, "notes.md", 1, move || {
        std::fs::write(&document, "alpha beta, and a line someone added").unwrap();
    });
    let root = remote_project_root(workspace.place(shell));

    let error = edit_document(&root, "notes", "beta", "gamma", "notes").unwrap_err();
    assert!(error.contains("changed while it was being edited"), "{error}");
    assert_eq!(
        std::fs::read_to_string(workspace.memory("notes.md")).unwrap(),
        "alpha beta, and a line someone added"
    );
}

// A Unix symlink.
#[cfg(unix)]
#[test]
fn a_linked_memory_folder_is_refused_and_never_followed() {
    let Some(shell) = bash() else { return };
    let workspace = Workspace::new();
    let elsewhere = workspace.root.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(workspace.root.join(".mewrk")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, workspace.root.join(".mewrk/memory")).unwrap();
    std::fs::write(elsewhere.join("MEMORY.md"), "- [x.md](x.md) — outside\n").unwrap();
    std::fs::write(elsewhere.join("x.md"), "outside").unwrap();
    let root = remote_project_root(workspace.place(shell));

    assert!(read_index(&root).is_empty());
    assert!(read_document(&root, "x").is_err());
    let error = create_document(&root, "y", "y", "y").unwrap_err();
    assert!(error.contains("occupied"), "{error}");
    assert!(!elsewhere.join("y.md").exists());
}
