//! Agent builds for machines other than this computer, from Mewrk's channel,
//! fetched the first time a machine needs one.
//!
//! The installer carries only the agent for this computer's own platform, which
//! its sandboxed commands run through. An SSH machine, a WSL distribution or an
//! emulated Windows on Arm needs another build, published for this host's agent
//! source ([`remote_agent::SOURCE_ID`]) at
//! `remote-agent/<source id>/<triple>.json`. Builds are keyed by the source,
//! not by Mewrk's version, so a release whose agent did not change shares its
//! builds with the previous one, and a machine is only ever given an agent this
//! host speaks for.
//!
//! A fetched build lands in `<components>/remote-agent/<source id>/<triple>/`,
//! one of the directories `remote_link` finds builds in, which still reads the
//! source identity out of every executable it is given
//! (`remote_link::Build::read`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{fetch_pointer_from, install_files_from};
use super::Pointer;
use crate::ui_text::ui_text;

/// The pointer's `component`, and the channel directory it is under.
const COMPONENT: &str = "remote-agent";
/// The Windows sandbox's helper, which a Windows build carries beside its agent.
const SANDBOX_HELPER: &str = "srt-win.exe";
/// A fetch's own directory beside the builds, while it puts one together.
const STAGING_PREFIX: &str = ".partial-";
/// A build a fetch moved out of the way to put another in its place.
const ASIDE_PREFIX: &str = ".replaced-";

/// Tells apart the directories of fetches running in this process.
static ATTEMPT: AtomicU64 = AtomicU64::new(0);

/// `<components>/remote-agent/<this host's agent source id>`: laid out as
/// `<triple>/mewrk-remote[.exe]`, one of `remote_link`'s agent directories.
pub fn cache_dir() -> Option<PathBuf> {
    super::root().map(|root| root.join(COMPONENT).join(remote_agent::SOURCE_ID))
}

/// Fetches the build for the first of `triples` the channel has for this
/// host's agent source into [`cache_dir`]. `Ok(Some(triple))` once installed,
/// `Ok(None)` when none of them is published.
pub fn fetch(triples: &[&str]) -> Result<Option<String>, String> {
    let cache = cache_dir().ok_or_else(|| {
        ui_text!("组件目录还没有设定", "The components folder has not been set up")
    })?;
    fetch_into(&super::channel_base(), &cache, triples)
}

/// Removes builds of other agent sources, which belong to other versions of
/// Mewrk, and what fetches the last run of Mewrk did not finish left behind.
/// Called at startup, before any fetch can be running.
pub fn prune_other_sources() {
    let Some(root) = super::root() else {
        return;
    };
    super::prune_dir(&root.join(COMPONENT), |name| name == remote_agent::SOURCE_ID);
    if let Some(cache) = cache_dir() {
        super::prune_dir(&cache, |name| !name.starts_with(STAGING_PREFIX) && !name.starts_with(ASIDE_PREFIX));
    }
}

/// [`fetch`] from the channel at `base` into `cache`.
fn fetch_into(base: &str, cache: &Path, triples: &[&str]) -> Result<Option<String>, String> {
    let pointer_dir = format!("{COMPONENT}/{}", remote_agent::SOURCE_ID);
    for triple in triples {
        let key = format!("{pointer_dir}/{triple}.json");
        let Some(pointer) = fetch_pointer_from(base, &key, COMPONENT, triple)? else {
            continue;
        };
        check_pointer(&pointer, triple)?;
        install(base, &pointer_dir, cache, triple, &pointer)?;
        eprintln!("[components] fetched the {triple} agent build");
        return Ok(Some((*triple).to_owned()));
    }
    Ok(None)
}

/// What an agent's pointer must say beyond [`Pointer::validate`]: this host's
/// agent source, and exactly the files `remote_link` installs on a machine —
/// the agent alone, or on Windows the agent and then the sandbox helper.
fn check_pointer(pointer: &Pointer, triple: &str) -> Result<(), String> {
    let source = pointer.extra.get("source").and_then(serde_json::Value::as_str);
    if source != Some(remote_agent::SOURCE_ID) {
        return Err(format!(
            "the {triple} agent pointer is for agent source {source:?}, not this Mewrk's {}",
            &remote_agent::SOURCE_ID[..12]
        ));
    }
    let expected = build_files(triple);
    let listed: Vec<&str> = pointer.files.iter().map(|file| file.name.as_str()).collect();
    if listed != expected {
        return Err(format!("the {triple} agent pointer lists {listed:?}, not {expected:?}"));
    }
    Ok(())
}

/// The files of a build for `triple`, in the order its pointer lists them.
fn build_files(triple: &str) -> Vec<String> {
    let agent = agent_binary(triple);
    if triple.contains("-windows-") {
        vec![agent, SANDBOX_HELPER.to_owned()]
    } else {
        vec![agent]
    }
}

/// The agent's executable name in a build for `triple`, as `remote_link`
/// looks for it.
fn agent_binary(triple: &str) -> String {
    if triple.contains("-windows-") {
        format!("{}.exe", remote_agent::AGENT_BINARY)
    } else {
        remote_agent::AGENT_BINARY.to_owned()
    }
}

/// Whether `dir` holds every file of `pointer`, checked.
fn is_complete(dir: &Path, pointer: &Pointer) -> bool {
    pointer
        .files
        .iter()
        .all(|file| super::is_installed(&dir.join(&file.name), file.unpacked_size, &file.unpacked_sha256))
}

/// Installs `pointer`'s files as `<cache>/<triple>/`, whole: they are put
/// together in a directory of this attempt's own beside it, which is renamed
/// into place, so `remote_link`, which may look at any time, never finds an
/// agent without the sandbox helper meant to sit beside it, and two fetches of
/// one build (for machines of two platforms that both run it) never write into
/// each other's files. A fetch that fails leaves nothing; the next one starts
/// over.
fn install(base: &str, pointer_dir: &str, cache: &Path, triple: &str, pointer: &Pointer) -> Result<(), String> {
    let dest = cache.join(triple);
    if is_complete(&dest, pointer) {
        return Ok(());
    }
    let attempt = format!("{triple}-{}-{}", std::process::id(), ATTEMPT.fetch_add(1, Ordering::Relaxed));
    let staging = cache.join(format!("{STAGING_PREFIX}{attempt}"));
    let aside = cache.join(format!("{ASIDE_PREFIX}{attempt}"));
    let result = install_files_from(base, pointer_dir, pointer, &staging, &AtomicBool::new(false), &mut |_| {})
        .and_then(|()| place(&staging, &dest, &aside, pointer));
    // Gone once placed; otherwise what the failure left.
    let _ = fs::remove_dir_all(&staging);
    result
}

/// Renames `staging` to `dest` — only into a free name: a build already there
/// is never removed from under whoever may have just placed it or be reading
/// it. One that is complete is kept (another fetch placed it first); another
/// build of the triple (an older publication, a damaged one) is first renamed
/// to `aside`, and removed once the new one is in place.
fn place(staging: &Path, dest: &Path, aside: &Path, pointer: &Pointer) -> Result<(), String> {
    let place_failed = |error: std::io::Error| {
        let dest = dest.display();
        ui_text!("无法放置 {dest}: {error}", "Could not move {dest} into place: {error}")
    };
    let into_free_name = || {
        super::with_patience(|| {
            if dest.exists() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            fs::rename(staging, dest)
        })
    };
    match into_free_name() {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(place_failed(error)),
        Err(_) if is_complete(dest, pointer) => return Ok(()),
        Err(_) => {}
    }
    super::with_patience(|| fs::rename(dest, aside)).map_err(place_failed)?;
    let placed = match into_free_name() {
        Ok(()) => Ok(()),
        Err(_) if is_complete(dest, pointer) => Ok(()),
        Err(error) => Err(place_failed(error)),
    };
    if placed.is_err() && !dest.exists() {
        // The build that was there is still one of this source: better than none.
        let _ = fs::rename(aside, dest);
    }
    // One still in use stays until the next start prunes it.
    let _ = fs::remove_dir_all(aside);
    placed
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::aisdk::test_channel::{packed, serve};
    use super::*;

    const MUSL: &str = "x86_64-unknown-linux-musl";
    const GNU: &str = "x86_64-unknown-linux-gnu";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";

    /// A published agent build of `files` (name, bytes) for `triple`, made from
    /// agent source `source`.
    fn publish(triple: &str, source: &str, files: &[(&str, &[u8])]) -> HashMap<String, Vec<u8>> {
        let dir = format!("remote-agent/{}", remote_agent::SOURCE_ID);
        let mut served = HashMap::new();
        let mut listed = Vec::new();
        for (name, bytes) in files {
            let (file, gz) = packed(triple, name, bytes);
            served.insert(format!("{dir}/{}", file.path), gz);
            listed.push(file);
        }
        let pointer = serde_json::json!({
            "schema": 1,
            "component": "remote-agent",
            "triple": triple,
            "version": "1.2.4",
            "source": source,
            "files": listed,
        });
        served.insert(format!("{dir}/{triple}.json"), serde_json::to_vec(&pointer).unwrap());
        served
    }

    #[test]
    fn the_first_published_triple_is_installed() {
        let (base, asked) = serve(publish(GNU, remote_agent::SOURCE_ID, &[("mewrk-remote", b"gnu agent")]));
        let cache = tempfile::tempdir().unwrap();
        assert_eq!(fetch_into(&base, cache.path(), &[MUSL, GNU]), Ok(Some(GNU.to_owned())));
        assert_eq!(fs::read(cache.path().join(GNU).join("mewrk-remote")).unwrap(), b"gnu agent");
        assert!(!cache.path().join(MUSL).exists());
        let names: Vec<String> = fs::read_dir(cache.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [GNU], "nothing is left beside the build");
        // Fetching it again finds it in place.
        let before = asked.lock().unwrap().len();
        assert_eq!(fetch_into(&base, cache.path(), &[GNU]), Ok(Some(GNU.to_owned())));
        assert_eq!(asked.lock().unwrap().len(), before + 1, "only the pointer is asked for");
    }

    #[test]
    fn a_windows_build_brings_the_sandbox_helper_beside_the_agent() {
        let files: &[(&str, &[u8])] = &[("mewrk-remote.exe", b"windows agent"), ("srt-win.exe", b"helper")];
        let (base, _) = serve(publish(WINDOWS, remote_agent::SOURCE_ID, files));
        let cache = tempfile::tempdir().unwrap();
        assert_eq!(fetch_into(&base, cache.path(), &[WINDOWS]), Ok(Some(WINDOWS.to_owned())));
        assert_eq!(fs::read(cache.path().join(WINDOWS).join("mewrk-remote.exe")).unwrap(), b"windows agent");
        assert_eq!(fs::read(cache.path().join(WINDOWS).join("srt-win.exe")).unwrap(), b"helper");
    }

    #[test]
    fn a_build_of_another_source_or_shape_is_refused() {
        let cache = tempfile::tempdir().unwrap();
        let other = "0".repeat(64);
        let (base, _) = serve(publish(GNU, &other, &[("mewrk-remote", b"other agent")]));
        assert!(fetch_into(&base, cache.path(), &[GNU]).is_err());
        let (base, _) = serve(publish(GNU, remote_agent::SOURCE_ID, &[("main.js", b"not an agent")]));
        assert!(fetch_into(&base, cache.path(), &[GNU]).is_err());
        assert!(!cache.path().join(GNU).exists());
    }

    #[test]
    fn a_pointer_lists_exactly_the_files_a_machine_is_given() {
        let cache = tempfile::tempdir().unwrap();
        let refused: [(&str, &[(&str, &[u8])]); 4] = [
            // A Windows agent without its sandbox helper, or with the two swapped.
            (WINDOWS, &[("mewrk-remote.exe", b"agent")]),
            (WINDOWS, &[("srt-win.exe", b"helper"), ("mewrk-remote.exe", b"agent")]),
            // Anything beside a Linux agent.
            (GNU, &[("mewrk-remote", b"agent"), ("srt-win.exe", b"helper")]),
            (GNU, &[("mewrk-remote", b"agent"), ("run.sh", b"#!/bin/sh")]),
        ];
        for (triple, files) in refused {
            let (base, asked) = serve(publish(triple, remote_agent::SOURCE_ID, files));
            let error = fetch_into(&base, cache.path(), &[triple]).unwrap_err();
            assert!(error.contains("lists"), "{error}");
            assert!(!asked.lock().unwrap().iter().any(|path| path.ends_with(".gz")), "nothing is downloaded");
            assert!(!cache.path().join(triple).exists());
        }
    }

    #[test]
    fn fetches_of_one_build_at_once_leave_one_build() {
        let (base, _) = serve(publish(GNU, remote_agent::SOURCE_ID, &[("mewrk-remote", b"gnu agent")]));
        let cache = tempfile::tempdir().unwrap();
        let fetches: Vec<_> = (0..4)
            .map(|_| {
                let (base, cache) = (base.clone(), cache.path().to_path_buf());
                std::thread::spawn(move || fetch_into(&base, &cache, &[GNU]))
            })
            .collect();
        for fetch in fetches {
            assert_eq!(fetch.join().unwrap(), Ok(Some(GNU.to_owned())));
        }
        assert_eq!(fs::read(cache.path().join(GNU).join("mewrk-remote")).unwrap(), b"gnu agent");
        let names: Vec<String> = fs::read_dir(cache.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [GNU], "no fetch leaves its own directory behind");
    }

    #[test]
    fn a_build_in_place_is_kept_when_complete_and_set_aside_when_not() {
        let (base, _) = serve(publish(GNU, remote_agent::SOURCE_ID, &[("mewrk-remote", b"gnu agent")]));
        let cache = tempfile::tempdir().unwrap();
        fetch_into(&base, cache.path(), &[GNU]).unwrap();
        let pointer: Pointer = {
            let files = publish(GNU, remote_agent::SOURCE_ID, &[("mewrk-remote", b"gnu agent")]);
            let key = format!("remote-agent/{}/{GNU}.json", remote_agent::SOURCE_ID);
            serde_json::from_slice(&files[&key]).unwrap()
        };
        let dest = cache.path().join(GNU);
        let aside = cache.path().join(".replaced-test");

        // Another fetch placed the same build first: it stays, and this one's is left.
        let staging = cache.path().join(".partial-test");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("mewrk-remote"), b"gnu agent").unwrap();
        place(&staging, &dest, &aside, &pointer).unwrap();
        assert!(staging.exists(), "the build in place was not replaced");
        assert_eq!(fs::read(dest.join("mewrk-remote")).unwrap(), b"gnu agent");

        // What is in place is another build of the triple: replaced, through a name of its own.
        fs::write(dest.join("mewrk-remote"), b"an older agent").unwrap();
        place(&staging, &dest, &aside, &pointer).unwrap();
        assert!(!staging.exists() && !aside.exists());
        assert_eq!(fs::read(dest.join("mewrk-remote")).unwrap(), b"gnu agent");
    }

    #[test]
    fn nothing_published_is_none() {
        let (base, asked) = serve(HashMap::new());
        let cache = tempfile::tempdir().unwrap();
        assert_eq!(fetch_into(&base, cache.path(), &[MUSL, GNU]), Ok(None));
        assert_eq!(asked.lock().unwrap().len(), 2, "every triple is asked for, in order");
        assert!(asked.lock().unwrap()[0].ends_with(&format!("{MUSL}.json")));
    }
}
