//! What macOS makes a person grant by hand, for the Permissions page and the sheet of the first launch.
//!
//! No API asks for Full Disk Access: an app can only find out whether it has it, and take the person to the list. The
//! keychain has no app-wide grant either, but every credential here sits under one item (`credential_vault.rs`), so
//! one "Always Allow" comes to the same thing.
//!
//! Both grants name the app's code signature. An ad-hoc signature changes with every build, so a rebuilt Mewrk has
//! neither, whatever System Settings still shows. The page says so when that is what's running.

use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

use serde::Serialize;
use tauri::{AppHandle, Manager};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DiskAccess {
    Granted,
    Denied,
    /// None of the files that tell exists.
    Unknown,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum KeychainAccess {
    Granted,
    /// "Allow" rather than "Always Allow": it works until this process ends.
    ThisLaunchOnly,
    NeedsPermission,
    Refused,
    NotApplicable,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionsSnapshot {
    full_disk_access: DiskAccess,
    keychain: KeychainAccess,
    /// The bundle the grants belong to. `None` for a bare executable, whose grants are whoever started it.
    app_bundle: Option<String>,
    ad_hoc_signed: bool,
    /// The sheet of the first launch has been answered.
    prompted: bool,
}

const PROMPTED_MARKER: &str = "permissions-prompted";

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|error| error.to_string())
}

fn snapshot(data_dir: &Path, keychain: KeychainAccess) -> PermissionsSnapshot {
    PermissionsSnapshot {
        full_disk_access: disk_access(),
        keychain,
        app_bundle: app_bundle(),
        ad_hoc_signed: ad_hoc_signed(),
        prompted: data_dir.join(PROMPTED_MARKER).exists(),
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work).await.map_err(|error| error.to_string())
}

/// Where everything stands. Never raises a dialog.
pub(crate) async fn permissions_status(app: AppHandle) -> Result<PermissionsSnapshot, String> {
    let dir = data_dir(&app)?;
    blocking(move || snapshot(&dir, keychain_access())).await
}

/// The person pressed Allow. Waits for them to answer the keychain's dialog, if macOS raises one.
pub(crate) async fn request_keychain_access(app: AppHandle) -> Result<PermissionsSnapshot, String> {
    let dir = data_dir(&app)?;
    blocking(move || snapshot(&dir, ask_for_keychain())).await
}

/// The sheet of the first launch has been answered, whatever the answer was: the page is there for changing it.
pub(crate) async fn mark_permissions_prompted(app: AppHandle) -> Result<(), String> {
    let dir = data_dir(&app)?;
    blocking(move || std::fs::write(dir.join(PROMPTED_MARKER), b"")).await?.map_err(|error| error.to_string())
}

pub(crate) async fn open_privacy_settings(pane: String) -> Result<(), String> {
    let url = privacy_settings_url(&pane).ok_or_else(|| format!("没有这个隐私设置页面: {pane}"))?;
    blocking(move || open_url(&url)).await?
}

/// The System Settings pane for one of the things Mewrk's `Info.plist` declares. Anything else is refused, since this
/// hands a URL to the system.
fn privacy_settings_url(pane: &str) -> Option<String> {
    let anchor = match pane {
        "fullDiskAccess" => "Privacy_AllFiles",
        "filesAndFolders" => "Privacy_FilesAndFolders",
        "automation" => "Privacy_Automation",
        "microphone" => "Privacy_Microphone",
        "camera" => "Privacy_Camera",
        "localNetwork" => "Privacy_LocalNetwork",
        "bluetooth" => "Privacy_Bluetooth",
        _ => return None,
    };
    Some(format!("x-apple.systempreferences:com.apple.preference.security?{anchor}"))
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) -> Result<(), String> {
    let status = Command::new("open").arg(url).status().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("open {url}: {status}"))
    }
}

#[cfg(not(target_os = "macos"))]
fn open_url(_url: &str) -> Result<(), String> {
    Err("这个系统没有隐私设置页面".to_owned())
}

#[cfg(target_os = "macos")]
fn keychain_access() -> KeychainAccess {
    crate::credential_vault::access()
}

#[cfg(not(target_os = "macos"))]
fn keychain_access() -> KeychainAccess {
    KeychainAccess::NotApplicable
}

#[cfg(target_os = "macos")]
fn ask_for_keychain() -> KeychainAccess {
    crate::credential_vault::request_access()
}

#[cfg(not(target_os = "macos"))]
fn ask_for_keychain() -> KeychainAccess {
    KeychainAccess::NotApplicable
}

/// Files only Full Disk Access opens, and whose opening makes macOS ask nothing. That rules out `~/Library/Containers`:
/// reading another app's container raises a dialog of its own. Not every macOS has the per-user `TCC.db`.
fn protected_files(home: Option<&Path>) -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from("/Library/Application Support/com.apple.TCC/TCC.db")];
    if let Some(home) = home {
        files.push(home.join("Library/Safari/Bookmarks.plist"));
        files.push(home.join("Library/Safari/History.db"));
        files.push(home.join("Library/Application Support/com.apple.TCC/TCC.db"));
    }
    files
}

/// The first file that exists decides: macOS answers them all alike.
fn disk_access_from(files: &[PathBuf]) -> DiskAccess {
    for file in files {
        match std::fs::File::open(file) {
            Ok(_) => return DiskAccess::Granted,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return DiskAccess::Denied,
            Err(_) => {}
        }
    }
    DiskAccess::Unknown
}

fn disk_access() -> DiskAccess {
    if !cfg!(target_os = "macos") {
        return DiskAccess::NotApplicable;
    }
    disk_access_from(&protected_files(std::env::home_dir().as_deref()))
}

/// `Mewrk.app` when the executable is the one inside `Mewrk.app/Contents/MacOS`.
fn bundle_of(executable: &Path) -> Option<&Path> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && bundle.extension()? == "app").then_some(bundle)
}

fn app_bundle() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    bundle_of(&executable).map(|bundle| bundle.to_string_lossy().into_owned())
}

/// From `codesign`, once: the flags the kernel keeps for a running process don't carry `CS_ADHOC`.
#[cfg(target_os = "macos")]
fn ad_hoc_signed() -> bool {
    static AD_HOC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AD_HOC.get_or_init(|| {
        let Ok(executable) = std::env::current_exe() else { return false };
        let Ok(output) = Command::new("/usr/bin/codesign").arg("-dv").arg(executable).output() else { return false };
        names_an_ad_hoc_signature(&String::from_utf8_lossy(&output.stderr))
    })
}

#[cfg(target_os = "macos")]
fn names_an_ad_hoc_signature(codesign_output: &str) -> bool {
    codesign_output.lines().any(|line| line.trim() == "Signature=adhoc")
}

#[cfg(not(target_os = "macos"))]
fn ad_hoc_signed() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_file_that_exists_decides() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let readable = root.path().join("readable");
        std::fs::write(&readable, b"x").unwrap();
        assert_eq!(disk_access_from(&[missing.clone()]), DiskAccess::Unknown);
        assert_eq!(disk_access_from(&[missing.clone(), readable.clone()]), DiskAccess::Granted);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let closed = root.path().join("closed");
            std::fs::write(&closed, b"x").unwrap();
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert_eq!(disk_access_from(&[missing, closed.clone(), readable]), DiskAccess::Denied);
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn nothing_probed_is_a_folder_that_raises_a_dialog_of_its_own() {
        let files = protected_files(Some(Path::new("/Users/someone")));
        assert!(files.contains(&PathBuf::from("/Library/Application Support/com.apple.TCC/TCC.db")));
        assert!(files.iter().all(|file| !file.to_string_lossy().contains("Containers")), "{files:?}");
    }

    #[test]
    fn only_the_panes_mewrk_declares_can_be_opened() {
        assert_eq!(privacy_settings_url("fullDiskAccess").unwrap(), "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles");
        for pane in ["filesAndFolders", "automation", "microphone", "camera", "localNetwork", "bluetooth"] {
            assert!(privacy_settings_url(pane).is_some(), "{pane}");
        }
        assert_eq!(privacy_settings_url("https://example.com"), None);
        assert_eq!(privacy_settings_url(""), None);
    }

    #[test]
    fn a_bundle_is_the_executable_inside_contents_macos() {
        let inside = Path::new("/Applications/Mewrk.app/Contents/MacOS/mewrk");
        assert_eq!(bundle_of(inside), Some(Path::new("/Applications/Mewrk.app")));
        assert_eq!(bundle_of(Path::new("/repo/target/release/mewrk")), None);
        assert_eq!(bundle_of(Path::new("/Applications/Mewrk/Contents/MacOS/mewrk")), None);
        assert_eq!(bundle_of(Path::new("mewrk")), None);
    }

    /// A test binary is linker-signed, which `codesign` reports as ad hoc.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn an_ad_hoc_signature_is_seen() {
        assert!(ad_hoc_signed());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_signature_is_not_taken_for_ad_hoc() {
        assert!(names_an_ad_hoc_signature("Identifier=dev.mewrk.native\nFormat=app bundle\nSignature=adhoc\nTeamIdentifier=not set\n"));
        assert!(!names_an_ad_hoc_signature("Identifier=dev.mewrk.native\nSignature size=9000\nTeamIdentifier=ABCDE12345\n"));
    }

    #[test]
    fn the_page_gets_its_field_names() {
        let snapshot = PermissionsSnapshot { full_disk_access: DiskAccess::NotApplicable, keychain: KeychainAccess::ThisLaunchOnly, app_bundle: None, ad_hoc_signed: true, prompted: false };
        assert_eq!(serde_json::to_value(snapshot).unwrap(), serde_json::json!({ "fullDiskAccess": "notApplicable", "keychain": "thisLaunchOnly", "appBundle": null, "adHocSigned": true, "prompted": false }));
    }
}
