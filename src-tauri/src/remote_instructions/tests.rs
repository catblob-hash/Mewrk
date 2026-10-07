use super::*;
use crate::project_memory::discover_project_memory;
use crate::remote_files::tests::LocalBash;

fn mirror(flavour: PathFlavour) -> (tempfile::TempDir, RemoteMirror) {
    let directory = tempfile::tempdir().unwrap();
    let tree = fs::canonicalize(directory.path()).unwrap().join(TREE);
    fs::create_dir_all(&tree).unwrap();
    (directory, RemoteMirror::new(tree, flavour))
}

// -- paths ------------------------------------------------------------------

#[test]
fn a_posix_path_lands_under_the_tree_and_comes_back_unchanged() {
    let (_directory, mirror) = mirror(PathFlavour::Posix);
    let local = mirror.local_path("/home/u/repo/MEWRK.md").unwrap();
    assert_eq!(local, mirror.tree.join("home/u/repo/MEWRK.md"));
    assert_eq!(
        mirror.remote_path(&local).as_deref(),
        Some("/home/u/repo/MEWRK.md")
    );
    assert_eq!(mirror.remote_path(&mirror.tree).as_deref(), Some("/"));
    // `..` is the machine's: it stops at the machine's root, never above the
    // tree.
    assert_eq!(
        mirror.local_path("/home/../../../etc/./x.md").unwrap(),
        mirror.tree.join("etc/x.md")
    );
    assert_eq!(mirror.local_path("relative/x.md"), None);
    assert_eq!(mirror.remote_path(Path::new("/elsewhere/x.md")), None);
}

#[test]
fn a_windows_path_is_filed_under_its_drive_letter_or_its_share() {
    let (_directory, mirror) = mirror(PathFlavour::Windows);
    let local = mirror.local_path("c:\\Users\\dev\\app\\MEWRK.md").unwrap();
    assert_eq!(local, mirror.tree.join("C/Users/dev/app/MEWRK.md"));
    assert_eq!(
        mirror.remote_path(&local).as_deref(),
        Some("C:/Users/dev/app/MEWRK.md")
    );
    assert_eq!(
        mirror.local_path("C:/a/../../b").unwrap(),
        mirror.tree.join("C/b"),
        "`..` stops at the drive's root"
    );
    let share = mirror.local_path("//server/share/x/../y.md").unwrap();
    assert_eq!(share, mirror.tree.join("UNC/server/share/y.md"));
    assert_eq!(
        mirror.remote_path(&share).as_deref(),
        Some("//server/share/y.md")
    );
    assert_eq!(mirror.local_path("/no/drive"), None);
    assert_eq!(mirror.local_path("relative"), None);
}

#[test]
fn an_import_resolves_by_the_machines_rules_and_stays_in_the_tree() {
    let (_directory, posix) = mirror(PathFlavour::Posix);
    let source = posix.local_path("/home/u/repo/MEWRK.md").unwrap();
    assert_eq!(
        posix.resolve_import(&source, "docs/style.md").unwrap(),
        posix.tree.join("home/u/repo/docs/style.md")
    );
    assert_eq!(
        posix.resolve_import(&source, "/etc/shared.md").unwrap(),
        posix.tree.join("etc/shared.md"),
        "an absolute import is the machine's path, not this computer's"
    );
    assert_eq!(
        posix
            .resolve_import(&source, "../../../../../../outside.md")
            .unwrap(),
        posix.tree.join("outside.md")
    );

    let (_directory, windows) = mirror(PathFlavour::Windows);
    let source = windows.local_path("C:/src/app/MEWRK.md").unwrap();
    assert_eq!(
        windows.resolve_import(&source, "..\\docs\\x.md").unwrap(),
        windows.tree.join("C/src/docs/x.md")
    );
    assert_eq!(
        windows.resolve_import(&source, "D:/shared/x.md").unwrap(),
        windows.tree.join("D/shared/x.md")
    );
    assert_eq!(
        windows.resolve_import(&source, "/rooted.md").unwrap(),
        windows.tree.join("C/rooted.md"),
        "a driveless rooted import is on the source's drive"
    );
}

#[test]
fn the_loader_resolves_imports_of_a_reproduced_file_by_the_machines_rules() {
    let (_directory, mirror) = mirror(PathFlavour::Posix);
    let workspace = mirror.local_path("/srv/app").unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("MEWRK.md"), "See @/etc/team.md and @notes.md.").unwrap();
    let mut options = ProjectMemoryOptions::new(&workspace);
    options.remote = Some(mirror.clone());
    let targets = project_memory::discover_project_memory(&options);
    let targets = project_memory::import_targets(&targets, &options);
    assert_eq!(
        targets,
        [mirror.tree.join("etc/team.md"), workspace.join("notes.md")]
    );
}

#[test]
fn folders_between_the_top_level_and_the_root_include_both() {
    assert_eq!(
        folders_between("/home/u/repo", "/home/u/repo/apps/web", PathFlavour::Posix),
        ["/home/u/repo", "/home/u/repo/apps", "/home/u/repo/apps/web"]
    );
    assert_eq!(folders_between("/", "/srv", PathFlavour::Posix), ["/", "/srv"]);
    assert_eq!(
        folders_between("C:/", "C:/src", PathFlavour::Windows),
        ["C:/", "C:/src"]
    );
    assert_eq!(strip_root("/srv/app/x.md", "/srv/app", PathFlavour::Posix), Some("/x.md"));
    assert_eq!(strip_root("/srv/apple", "/srv/app", PathFlavour::Posix), None);
    assert_eq!(
        strip_root("c:/Src/App/x.md", "C:/src/app", PathFlavour::Windows),
        Some("/x.md")
    );
}

// -- answers ----------------------------------------------------------------

fn answer(records: &[&[u8]]) -> Vec<u8> {
    let mut bytes = format!("{HEADER}\n").into_bytes();
    for record in records {
        bytes.extend_from_slice(record);
    }
    bytes.extend_from_slice(b"E\n");
    bytes
}

#[test]
fn an_answer_carries_any_bytes_and_a_cut_one_is_refused() {
    let bytes = answer(&[
        b"R\n/srv/app\nT\n/srv\n",
        b"D\n/srv/app\n",
        b"F\n/srv/app/MEWRK.md\n6\nE\nF\n\xff\n",
        b"B\n/srv/app/huge.md\n999999\n",
        b"U\n/srv/app/secret.md\n",
        b"L\n/srv/app/.mewrk\n/srv/shared\n",
        b"S\nt\n/srv/app/.mewrk/rules\n",
    ]);
    let parsed = parse(&bytes).unwrap();
    assert_eq!(parsed.root.as_deref(), Some("/srv/app"));
    assert_eq!(parsed.top.as_deref(), Some("/srv"));
    assert_eq!(parsed.project, None);
    assert_eq!(
        parsed.records,
        [
            Record::Dir("/srv/app".into()),
            Record::File {
                path: "/srv/app/MEWRK.md".into(),
                bytes: b"E\nF\n\xff\n".to_vec(),
            },
            Record::Large {
                path: "/srv/app/huge.md".into(),
                size: 999_999,
            },
            Record::Unreadable("/srv/app/secret.md".into()),
            Record::Link {
                path: "/srv/app/.mewrk".into(),
                target: "/srv/shared".into(),
            },
            Record::Scope {
                kind: ScopeKind::Tree,
                path: "/srv/app/.mewrk/rules".into(),
            },
        ]
    );
    let cut = &bytes[..bytes.len() - 20];
    assert!(parse(cut).is_err());
    assert!(parse(b"something else\nE\n").is_err());
}

// -- the mirror -------------------------------------------------------------

#[test]
fn an_answer_puts_files_and_links_in_place_and_its_scopes_remove_what_is_gone() {
    let (_directory, mirror) = mirror(PathFlavour::Posix);
    let local = |path: &str| mirror.local_path(path).unwrap();
    // What an earlier run left: a rule since deleted, a stale `.mewrk` that
    // was a link, an unreadable file's old copy, and a file left in a rules
    // tree that is not Markdown at all.
    fs::create_dir_all(local("/srv/app/.mewrk/rules/old")).unwrap();
    fs::write(local("/srv/app/.mewrk/rules/old/gone.md"), "gone").unwrap();
    fs::write(local("/srv/app/MEWRK.local.md"), "stale").unwrap();
    fs::write(local("/srv/app/secret.md"), "old secret").unwrap();
    fs::create_dir_all(local("/srv/other")).unwrap();

    let records = parse(&answer(&[
        b"D\n/srv/app\n",
        b"S\nf\n/srv/app/MEWRK.md\nF\n/srv/app/MEWRK.md\n5\nhello",
        b"S\nf\n/srv/app/MEWRK.local.md\n",
        b"S\nf\n/srv/app/secret.md\nU\n/srv/app/secret.md\n",
        b"S\nd\n/srv/app/.mewrk\nD\n/srv/app/.mewrk\n",
        b"S\nf\n/srv/app/.mewrk/MEWRK.md\n",
        b"S\nt\n/srv/app/.mewrk/rules\nD\n/srv/app/.mewrk/rules\n",
        b"F\n/srv/app/.mewrk/rules/kept.md\n4\nkept",
        b"B\n/srv/app/.mewrk/rules/huge.md\n10\n",
        b"L\n/srv/app/.mewrk/rules/linked.md\n/srv/shared/linked.md\nS\nf\n/srv/shared/linked.md\n",
        b"F\n/srv/shared/linked.md\n6\nlinked",
    ]))
    .unwrap()
    .records;
    let skipped = materialize(&mirror, &records, 8).unwrap();

    assert_eq!(fs::read_to_string(local("/srv/app/MEWRK.md")).unwrap(), "hello");
    assert!(!local("/srv/app/MEWRK.local.md").exists(), "gone on the machine");
    assert!(!local("/srv/app/secret.md").exists(), "unreadable there");
    assert_eq!(skipped, [(local("/srv/app/secret.md"), SkipReason::Unreadable)]);
    assert!(!local("/srv/app/.mewrk/rules/old").exists());
    assert_eq!(
        fs::read_to_string(local("/srv/app/.mewrk/rules/kept.md")).unwrap(),
        "kept"
    );
    assert_eq!(
        fs::metadata(local("/srv/app/.mewrk/rules/huge.md")).unwrap().len(),
        10,
        "too large to bring: a placeholder of its size, past the per-file limit"
    );
    assert_eq!(
        fs::read_link(local("/srv/app/.mewrk/rules/linked.md")).unwrap(),
        local("/srv/shared/linked.md")
    );
    assert_eq!(
        fs::read_to_string(local("/srv/app/.mewrk/rules/linked.md")).unwrap(),
        "linked"
    );
    assert!(local("/srv/other").exists(), "nothing outside a scope is touched");
}

// Unix symlinks and permission bits.
#[cfg(unix)]
#[test]
fn a_stale_link_where_the_machine_now_has_a_folder_is_replaced_by_one() {
    let (_directory, mirror) = mirror(PathFlavour::Posix);
    let local = |path: &str| mirror.local_path(path).unwrap();
    fs::create_dir_all(local("/srv/elsewhere")).unwrap();
    fs::create_dir_all(local("/srv/app")).unwrap();
    std::os::unix::fs::symlink(local("/srv/elsewhere"), local("/srv/app/docs")).unwrap();

    let records = parse(&answer(&[
        b"D\n/srv/app/docs\n",
        b"S\nf\n/srv/app/docs/guide.md\nF\n/srv/app/docs/guide.md\n5\nguide",
    ]))
    .unwrap()
    .records;
    materialize(&mirror, &records, 100).unwrap();
    let docs = fs::symlink_metadata(local("/srv/app/docs")).unwrap();
    assert!(docs.is_dir() && !docs.file_type().is_symlink());
    assert!(!local("/srv/elsewhere/guide.md").exists());
    assert_eq!(fs::read_to_string(local("/srv/app/docs/guide.md")).unwrap(), "guide");
}

#[test]
fn a_name_this_computer_cannot_hold_is_told_as_skipped() {
    let (_directory, mirror) = mirror(PathFlavour::Posix);
    let records = vec![Record::File {
        path: "/srv/app/.mewrk/rules/a\0b.md".into(),
        bytes: b"x".to_vec(),
    }];
    let skipped = materialize(&mirror, &records, 100).unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].1, SkipReason::Unreadable);
}

#[test]
fn a_mirrored_path_is_shown_as_the_machines_own() {
    let base = tempfile::tempdir().unwrap();
    let _base = use_mirror_base(base.path());
    let posix = open_mirror("ssh:m-1", "dev@box", PathFlavour::Posix).unwrap();
    let windows = open_mirror("ssh:m-2", "winbox", PathFlavour::Windows).unwrap();
    let wsl = open_mirror("wsl:Ubuntu", "WSL (Ubuntu)", PathFlavour::Posix).unwrap();

    let located = locate(&posix.local_path("/home/u/repo/x.md").unwrap()).unwrap();
    assert_eq!(located.label, "dev@box");
    assert_eq!(located.components, ["home", "u", "repo", "x.md"]);
    assert_eq!(located.display(&located.components), "dev@box:/home/u/repo/x.md");
    let located = locate(&windows.local_path("C:/Users/dev/x.md").unwrap()).unwrap();
    assert_eq!(
        located.display(&located.components),
        "winbox:C:/Users/dev/x.md"
    );
    let located = locate(&wsl.local_path("/srv/x.md").unwrap()).unwrap();
    assert_eq!(located.display(&located.components), "WSL (Ubuntu):/srv/x.md");
    assert_eq!(locate(base.path()), None);
    assert_eq!(locate(Path::new("/srv/x.md")), None);
    // Two machines never share a folder, whatever their names.
    assert_ne!(machine_directory_name("ssh:a/b"), machine_directory_name("ssh:a_b"));
}

// -- the scripts, for real --------------------------------------------------

/// A machine played by a local POSIX shell, its filesystem a temporary
/// folder: the scripts run exactly as they would over SSH.
struct Machine {
    _directory: tempfile::TempDir,
    _base: MirrorBaseGuard,
    /// The machine's filesystem, canonical, as `pwd -P` reports it.
    root: PathBuf,
    shell: Arc<LocalBash>,
}

impl Machine {
    fn new() -> Option<Self> {
        Self::in_directory(tempfile::tempdir().unwrap())
    }

    /// A machine whose paths are short: the loader's secret guard reads a
    /// long mixed-case path written into an instruction file — the system
    /// temporary folder's, on macOS — as a token.
    fn with_short_paths() -> Option<Self> {
        Self::in_directory(tempfile::tempdir_in("/tmp").unwrap())
    }

    fn in_directory(directory: tempfile::TempDir) -> Option<Self> {
        let shell = LocalBash::find()?;
        let root = fs::canonicalize(directory.path()).unwrap();
        fs::create_dir_all(root.join("machine")).unwrap();
        fs::create_dir_all(root.join("mirrors")).unwrap();
        let base = use_mirror_base(&root.join("mirrors"));
        Some(Self {
            _directory: directory,
            _base: base,
            root: root.join("machine"),
            shell: Arc::new(shell),
        })
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn remote(&self, relative: &str) -> String {
        self.path(relative).to_string_lossy().into_owned()
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn place(&self, root: &str) -> RemotePlace {
        RemotePlace {
            shell: self.shell.clone(),
            machine: "ssh:test-machine".into(),
            label: "test-machine".into(),
            root: self.remote(root),
            project_folder: None,
        }
    }
}

fn source_contents(report: &ProjectMemoryReport) -> Vec<String> {
    report
        .sources
        .iter()
        .map(|source| source.content.trim().to_owned())
        .collect()
}

// Unix symlinks and permission bits.
#[cfg(unix)]
#[test]
fn a_remote_workspace_reads_its_own_instruction_files_up_to_its_top_level() {
    let Some(machine) = Machine::new() else { return };
    machine.write("MEWRK.md", "above the repository");
    fs::create_dir_all(machine.path("repo/.git")).unwrap();
    machine.write("repo/MEWRK.md", "repository rules");
    machine.write("repo/shared/team.md", "team conventions");
    machine.write("repo/app/MEWRK.md", "app rules, see @docs/guide.md");
    machine.write("repo/app/docs/guide.md", "the guide, and @deeper.md");
    machine.write("repo/app/docs/deeper.md", "deeper still");
    machine.write("repo/app/MEWRK.local.md", "personal notes");
    machine.write("repo/app/.mewrk/MEWRK.md", "dot mewrk rules");
    machine.write(
        "repo/app/.mewrk/rules/style.md",
        "---\npaths:\n  - \"src/**/*.rs\"\n---\nrust style",
    );
    machine.write("repo/app/.mewrk/rules/nested/always.md", "always rule");
    machine.write("repo/app/.mewrk/rules/notes.txt", "not a rule");
    std::os::unix::fs::symlink(
        machine.path("repo/shared/team.md"),
        machine.path("repo/app/.mewrk/rules/team.md"),
    )
    .unwrap();

    let mut options = ProjectMemoryOptions::new("");
    let mut remote = RemoteInstructions::prepare(machine.place("repo/app"), &mut options).unwrap();
    let mirror = options.remote.clone().unwrap();
    assert_eq!(
        mirror.remote_path(&options.workspace_root).as_deref(),
        Some(machine.remote("repo/app").as_str())
    );
    assert_eq!(
        options.ancestor_floor.as_deref().and_then(|floor| mirror.remote_path(floor)),
        Some(machine.remote("repo"))
    );

    let report = discover_project_memory(&options);
    let contents = source_contents(&report);
    for expected in [
        "repository rules",
        "app rules, see @docs/guide.md",
        "the guide, and @deeper.md",
        "deeper still",
        "personal notes",
        "dot mewrk rules",
        "rust style",
        "always rule",
    ] {
        assert!(contents.iter().any(|content| content == expected), "{expected}: {contents:?}");
    }
    assert!(!contents.iter().any(|content| content == "above the repository"));
    assert!(!contents.iter().any(|content| content == "not a rule"));
    // The linked rule resolves outside the workspace and is read there, as a
    // local link out of the workspace is.
    assert!(contents.contains(&"team conventions".to_owned()), "{contents:?}");
    // Model-visible labels stay relative.
    assert!(report
        .sources
        .iter()
        .all(|source| !source.safe_label().contains("mirrors")));
    assert!(remote.take_skipped(&options).is_empty());
}

#[test]
fn a_second_run_sees_what_changed_on_the_machine_and_not_what_was_deleted() {
    let Some(machine) = Machine::new() else { return };
    machine.write("app/MEWRK.md", "first version, see @extra.md");
    machine.write("app/extra.md", "extra, first");
    machine.write("app/.mewrk/rules/old.md", "old rule");

    let mut options = ProjectMemoryOptions::new("");
    RemoteInstructions::prepare(machine.place("app"), &mut options).unwrap();
    let first = source_contents(&discover_project_memory(&options));
    assert!(first.contains(&"extra, first".to_owned()), "{first:?}");
    assert!(first.contains(&"old rule".to_owned()));

    machine.write("app/MEWRK.md", "second version, see @extra.md");
    machine.write("app/extra.md", "extra, second and longer");
    fs::remove_file(machine.path("app/.mewrk/rules/old.md")).unwrap();

    let mut options = ProjectMemoryOptions::new("");
    RemoteInstructions::prepare(machine.place("app"), &mut options).unwrap();
    let second = source_contents(&discover_project_memory(&options));
    assert!(second.contains(&"second version, see @extra.md".to_owned()), "{second:?}");
    assert!(second.contains(&"extra, second and longer".to_owned()), "{second:?}");
    assert!(!second.contains(&"old rule".to_owned()));
    assert!(!second.iter().any(|content| content.contains("first")));
}

#[test]
fn an_absolute_import_is_read_on_the_machine() {
    let Some(machine) = Machine::with_short_paths() else { return };
    machine.write("outside/team.md", "from elsewhere on the machine");
    let absolute = machine.remote("outside/team.md");
    machine.write("app/MEWRK.md", &format!("Follow @{absolute}"));

    let mut options = ProjectMemoryOptions::new("");
    RemoteInstructions::prepare(machine.place("app"), &mut options).unwrap();
    let mirror = options.remote.clone().unwrap();
    let report = discover_project_memory(&options);
    assert!(source_contents(&report).contains(&"from elsewhere on the machine".to_owned()));
    // It is one of the files the instructions import, named by the machine's
    // own path for it.
    let imported = report
        .imported_files()
        .filter_map(|path| mirror.remote_path(path))
        .collect::<Vec<_>>();
    assert_eq!(imported, [absolute]);
}

#[test]
fn reading_a_file_in_a_subfolder_brings_that_folders_instructions() {
    let Some(machine) = Machine::new() else { return };
    machine.write("app/MEWRK.md", "app rules");
    machine.write("app/pkg/deep/MEWRK.md", "nested package rules, see @more.md");
    machine.write("app/pkg/deep/more.md", "more nested");
    machine.write("app/pkg/deep/main.rs", "fn main() {}");

    let mut options = ProjectMemoryOptions::new("");
    let mut remote = RemoteInstructions::prepare(machine.place("app"), &mut options).unwrap();
    let mut report = discover_project_memory(&options);
    assert!(!source_contents(&report).contains(&"more nested".to_owned()));

    let identity = remote
        .load_for_read(
            &options,
            &report,
            "ssh:test-machine",
            &machine.remote("app/pkg/deep/main.rs"),
        )
        .unwrap()
        .unwrap();
    project_memory::discover_nested_project_memory_for_mirror(&options, &mut report, &identity)
        .unwrap();
    let loaded = project_memory::sources_for_read_path(&report, &identity, &BTreeSet::new());
    let contents = loaded
        .iter()
        .map(|source| source.content.trim())
        .collect::<Vec<_>>();
    assert_eq!(contents, ["nested package rules, see @more.md", "more nested"]);

    // Another machine's read, or a file outside the workspace, brings nothing.
    assert_eq!(
        remote
            .load_for_read(&options, &report, "wsl:Other", &machine.remote("app/pkg/deep/main.rs"))
            .unwrap(),
        None
    );
    assert_eq!(
        remote
            .load_for_read(&options, &report, "ssh:test-machine", &machine.remote("elsewhere/x.rs"))
            .unwrap(),
        None
    );
}

#[test]
fn a_worktree_also_reads_its_project_folders_local_file() {
    let Some(machine) = Machine::new() else { return };
    fs::create_dir_all(machine.path("repo/.git")).unwrap();
    machine.write("repo/MEWRK.local.md", "personal, untracked");
    machine.write("repo/MEWRK.md", "main checkout");
    machine.write("repo/.mewrk/worktrees/c1/.git", "gitdir: elsewhere");
    machine.write("repo/.mewrk/worktrees/c1/MEWRK.md", "worktree checkout");

    let mut place = machine.place("repo/.mewrk/worktrees/c1");
    place.project_folder = Some(machine.remote("repo"));
    let mut options = ProjectMemoryOptions::new("");
    RemoteInstructions::prepare(place, &mut options).unwrap();
    let contents = source_contents(&discover_project_memory(&options));
    assert_eq!(contents, ["worktree checkout", "personal, untracked"]);
}

// Unix symlinks and permission bits.
#[cfg(unix)]
#[test]
fn a_file_the_machine_will_not_let_be_read_is_told_by_its_workspace_label() {
    use std::os::unix::fs::PermissionsExt;

    let Some(machine) = Machine::new() else { return };
    machine.write("app/.mewrk/rules/secret.md", "hidden");
    let secret = machine.path("app/.mewrk/rules/secret.md");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&secret).is_ok() {
        // Running as a user every file is readable to.
        return;
    }
    let mut options = ProjectMemoryOptions::new("");
    let prepared = RemoteInstructions::prepare(machine.place("app"), &mut options);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let mut remote = prepared.unwrap();
    assert_eq!(
        remote.take_skipped(&options),
        [project_memory::SkippedInstructionFile {
            label: Some("workspace:.mewrk/rules/secret.md".into()),
            reason: SkipReason::Unreadable,
        }]
    );
    assert!(remote.take_skipped(&options).is_empty(), "told once");
}

#[test]
fn a_machine_that_cannot_be_read_fails_naming_it_and_the_workspace() {
    let Some(machine) = Machine::new() else { return };
    let mut options = ProjectMemoryOptions::new("");
    let error = match RemoteInstructions::prepare(machine.place("missing"), &mut options) {
        Ok(_) => panic!("a missing root must fail"),
        Err(error) => error,
    };
    assert!(error.contains("test-machine"), "{error}");
    assert!(error.contains("missing"), "{error}");
    assert!(error.contains("cannot enter the workspace root"), "{error}");
}

/// Instructions and project memory of a workspace on a real Linux machine
/// over SSH through the agent, with bash and with the machine's own `sh`
/// (dash on Debian and Ubuntu) as the agent shell: GNU `realpath`, `stat` and
/// `cksum`. Set `MEWRK_E2E_SSH_HOST` (and `MEWRK_E2E_SSH_PORT`,
/// `MEWRK_E2E_SSH_KEY` as needed) and run with `--ignored`.
#[test]
#[ignore]
fn over_real_ssh_a_workspace_reads_its_instructions_and_keeps_its_memory_there() {
    use crate::mewrk_memory::{create_document, edit_document, read_index, remote_project_root};
    use crate::shell_backend::{AgentShell, ShellBackend};

    let host = std::env::var("MEWRK_E2E_SSH_HOST").expect("MEWRK_E2E_SSH_HOST");
    let port = std::env::var("MEWRK_E2E_SSH_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(0);
    let identity_file = std::env::var("MEWRK_E2E_SSH_KEY").unwrap_or_default();
    let app_data = tempfile::tempdir().unwrap();
    crate::remote_link::install(app_data.path(), Vec::new(), None);
    let mirrors = tempfile::tempdir().unwrap();
    let _base = use_mirror_base(mirrors.path());
    let cancel = CancelSignal::default();
    let timeout = Duration::from_secs(60);
    for backend in [ShellBackend::Bash, ShellBackend::Sh] {
        let runner = ShellRunner::Ssh {
            agent_shell: AgentShell::new(backend, backend.id()),
            host: host.clone(),
            port,
            identity_file: identity_file.clone(),
            env: Default::default(),
        };
        let top = "/tmp/mewrk-e2e-instructions";
        let setup = runner
            .run(
                &format!(
                    "rm -rf {top} && mkdir -p {top}/repo/.git {top}/repo/shared {top}/repo/app/.mewrk/rules/sub {top}/repo/app/pkg && cd {top}/repo && \\
                     printf 'above\\n' > {top}/MEWRK.md && printf 'repository rules\\n' > MEWRK.md && \\
                     printf 'team conventions\\n' > shared/team.md && \\
                     printf 'app rules, see @docs/guide.md\\n' > app/MEWRK.md && mkdir -p app/docs && \\
                     printf 'the guide\\n' > app/docs/guide.md && printf 'always rule\\n' > app/.mewrk/rules/sub/always.md && \\
                     ln -s ../../../shared/team.md app/.mewrk/rules/team.md && \\
                     printf 'package rules\\n' > app/pkg/MEWRK.md && printf 'fn main() {{}}\\n' > app/pkg/main.rs"
                ),
                None,
                timeout,
                &cancel,
            )
            .unwrap();
        assert_eq!(setup.status, Some(0), "{backend:?}: {}", setup.stderr);
        let root = format!("{top}/repo/app");
        let shell: Arc<dyn RemoteShell + Send + Sync> = Arc::new(runner.clone());
        let place = RemotePlace {
            shell: shell.clone(),
            machine: "ssh:e2e".into(),
            label: host.clone(),
            root: root.clone(),
            project_folder: None,
        };

        let mut options = ProjectMemoryOptions::new("");
        let mut remote = RemoteInstructions::prepare(place, &mut options).unwrap();
        let report = discover_project_memory(&options);
        let contents = source_contents(&report);
        for expected in ["repository rules", "app rules, see @docs/guide.md", "the guide", "always rule"] {
            assert!(contents.contains(&expected.to_owned()), "{backend:?}: {expected}: {contents:?}");
        }
        assert!(!contents.contains(&"above".to_owned()), "{backend:?}");
        assert!(contents.contains(&"team conventions".to_owned()), "{backend:?}: {contents:?}");
        let mut report = report;
        let identity = remote
            .load_for_read(&options, &report, "ssh:e2e", &format!("{root}/pkg/main.rs"))
            .unwrap()
            .unwrap();
        project_memory::discover_nested_project_memory_for_mirror(&options, &mut report, &identity)
            .unwrap();
        let nested = project_memory::sources_for_read_path(&report, &identity, &BTreeSet::new())
            .iter()
            .map(|source| source.content.clone())
            .collect::<Vec<_>>();
        assert_eq!(nested, ["package rules\n"], "{backend:?}");

        let memory = remote_project_root(crate::remote_memory::RemoteMemoryPlace {
            shell: shell.clone(),
            machine: "ssh:e2e".into(),
            label: host.clone(),
            root: root.clone(),
        });
        create_document(&memory, "build", "cargo test needs the env.", "build command").unwrap();
        edit_document(&memory, "build", "needs the env", "needs the bundled env", "build, edited").unwrap();
        let entries = read_index(&memory);
        assert_eq!(entries.len(), 1, "{backend:?}");
        assert_eq!(entries[0].description, "build, edited");
        let on_machine = runner
            .run(&format!("cat {root}/.mewrk/memory/build.md"), None, timeout, &cancel)
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&on_machine.stdout),
            "cargo test needs the bundled env.",
            "{backend:?}"
        );

        let cleaned = runner.run(&format!("rm -rf {top}"), None, timeout, &cancel).unwrap();
        assert_eq!(cleaned.status, Some(0));
    }
}

// -- the PowerShell dialect -------------------------------------------------

fn limits() -> Limits {
    Limits {
        max_file: 262_144,
        budget: 4_194_304,
        entries: 2048,
        depth: 32,
    }
}

#[test]
fn the_powershell_probe_quotes_every_operand_and_writes_the_posix_records() {
    let startup = crate::remote_powershell::instruction_probe(
        &Job::Startup {
            root: "C:/Users/dev/it's",
            project_folder: Some("C:/Users/dev/main"),
        },
        &limits(),
    );
    assert!(startup.contains("Native-Path 'C:/Users/dev/it''s'"), "{startup}");
    assert!(startup.contains("Native-Path 'C:/Users/dev/main'"), "{startup}");
    assert!(startup.contains(&format!("Out-Line '{HEADER}'")));
    for record in ["'R'", "'T'", "'P'", "'D'", "'F'", "'B'", "'U'", "'L'", "'S'", "'E'"] {
        assert!(startup.contains(&format!("Out-Line {record}")), "{record}");
    }
    for candidate in ["'MEWRK.md'", "'MEWRK.local.md'", "'.mewrk'", "'rules'"] {
        assert!(startup.contains(candidate), "{candidate}");
    }
    assert!(startup.contains("Quit 64"));
    assert!(startup.contains("-le 262144"));

    let folders = crate::remote_powershell::instruction_probe(
        &Job::Folders(&["C:/w/a".to_owned(), "C:/w/a/b'c".to_owned()]),
        &limits(),
    );
    assert!(folders.contains("@('C:/w/a', 'C:/w/a/b''c')"), "{folders}");
    assert!(!folders.contains("Set-Location"), "a folder job enters nothing");

    let paths = crate::remote_powershell::instruction_probe(
        &Job::Paths(&["C:/x/\u{2019}y.md".to_owned()]),
        &limits(),
    );
    assert!(paths.contains("'C:/x/\u{2019}\u{2019}y.md'"), "{paths}");
    assert!(paths.contains("Fetch-Path $MewrkPath"));
}

/// PowerShell names ignore case, so a script must not reuse the helpers'
/// prologue variables, nor assign the loop variable of one function where
/// another reads it.
#[test]
fn the_powershell_probe_keeps_clear_of_the_prologue_variables() {
    let assigned = regex::Regex::new(r"(?im)^\s*\$(c|t|root|req)\s*=").unwrap();
    for job in [
        Job::Startup {
            root: "C:/w",
            project_folder: None,
        },
        Job::Folders(&["C:/w".to_owned()]),
        Job::Paths(&["C:/w/x.md".to_owned()]),
    ] {
        let script = crate::remote_powershell::instruction_probe(&job, &limits());
        let body = &script[script.find("$global:MewrkBudget").expect("the probe's own part")..];
        assert!(!assigned.is_match(body), "{body}");
    }
}

#[test]
fn the_posix_probe_quotes_every_operand() {
    let script = posix_script(
        &Job::Startup {
            root: "/srv/it's",
            project_folder: Some("~/main"),
        },
        &limits(),
    );
    assert!(script.contains("cd -- '/srv/it'\\''s'"), "{script}");
    assert!(script.contains("cd -- ~/'main'"), "{script}");
    let script = posix_script(&Job::Paths(&["/a b/$(x).md".to_owned()]), &limits());
    assert!(script.contains("for p in '/a b/$(x).md'; do"), "{script}");
}
