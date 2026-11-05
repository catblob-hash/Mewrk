//! `lib/appUpdateController.ts`: the update pipeline, check → download → install.
//!
//! It lives outside the Updates page because the host allows one download at a time and keeps running it whether or not
//! the page is on screen. Upstream's promises become `begin_*` and `finish_*` pairs; the page's verbs call the host in
//! between, which leaves this a plain state machine a test can step through.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppReleaseAsset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppRelease {
    pub tag: String,
    pub name: String,
    pub html_url: String,
    pub notes: String,
    pub published_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateCheck {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release: AppRelease,
    pub asset: Option<AppReleaseAsset>,
    pub checksums_asset: Option<AppReleaseAsset>,
    pub checked_at: String,
    pub in_app_install: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateDownload {
    pub path: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub verification: String,
    pub flavor: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AppUpdateDownloadEvent {
    #[serde(rename_all = "camelCase")]
    Progress { received_bytes: u64, total_bytes: u64 },
    Verifying,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum AppUpdateState {
    #[default]
    Idle,
    Checking,
    CheckFailed { message: String },
    Checked { check: AppUpdateCheck },
    Downloading { check: AppUpdateCheck, received_bytes: u64, total_bytes: u64, verifying: bool },
    DownloadFailed { check: AppUpdateCheck, message: String },
    Downloaded { check: AppUpdateCheck, download: AppUpdateDownload },
    Installing { check: AppUpdateCheck, download: AppUpdateDownload },
    InstallFailed { check: AppUpdateCheck, download: AppUpdateDownload, message: String },
}

impl AppUpdateState {
    pub fn check(&self) -> Option<&AppUpdateCheck> {
        match self {
            Self::Idle | Self::Checking | Self::CheckFailed { .. } => None,
            Self::Checked { check } | Self::Downloading { check, .. } | Self::DownloadFailed { check, .. } | Self::Downloaded { check, .. } | Self::Installing { check, .. } | Self::InstallFailed { check, .. } => Some(check),
        }
    }

    fn holds_download(&self) -> bool {
        matches!(self, Self::Downloaded { .. } | Self::InstallFailed { .. })
    }
}

/// The host's cancellation message, in each language it may have worded it in: a cancel is not a failure to show.
pub const DOWNLOAD_CANCELLED_MESSAGES: &[&str] = &["下载已取消", "The download was cancelled"];

pub fn is_download_cancelled(message: &str) -> bool {
    DOWNLOAD_CANCELLED_MESSAGES.iter().any(|c| message.contains(c))
}

/// A check younger than this is reused when the page mounts; the refresh button always refetches.
pub const CHECK_REUSE_WINDOW_MS: i64 = 10 * 60 * 1000;

#[derive(Default)]
pub struct AppUpdateController {
    pub state: AppUpdateState,
    checked_at: i64,
    check_sequence: u64,
    download_sequence: u64,
    in_flight_check: bool,
    previous: Option<AppUpdateState>,
}

impl AppUpdateController {
    fn busy(&self) -> bool {
        matches!(self.state, AppUpdateState::Downloading { .. } | AppUpdateState::Installing { .. })
    }

    /// Asks to start a check. `Some(sequence)` means call the host and hand the answer to `finish_check`.
    pub fn begin_check(&mut self, force: bool, now: i64) -> Option<u64> {
        if self.busy() || self.in_flight_check {
            return None;
        }
        let reusable = !matches!(self.state, AppUpdateState::Idle | AppUpdateState::Checking | AppUpdateState::CheckFailed { .. }) && now - self.checked_at < CHECK_REUSE_WINDOW_MS;
        if !force && reusable {
            return None;
        }
        self.check_sequence += 1;
        self.previous = Some(self.state.clone());
        // A finished download stays visible while the check runs; only its outcome decides.
        if !self.state.holds_download() {
            self.state = AppUpdateState::Checking;
        }
        self.in_flight_check = true;
        Some(self.check_sequence)
    }

    pub fn finish_check(&mut self, sequence: u64, result: Result<AppUpdateCheck, String>, now: i64) {
        self.in_flight_check = false;
        // The user may have pressed Install while the check was out; a result must never take the page out of it.
        if sequence != self.check_sequence || self.busy() {
            return;
        }
        let previous = self.previous.take().unwrap_or_default();
        match result {
            Ok(check) => {
                self.checked_at = now;
                let kept = match &previous {
                    AppUpdateState::Downloaded { check: old, download } | AppUpdateState::InstallFailed { check: old, download, .. } => (old.release.tag == check.release.tag && download.file_name == check.asset.as_ref().map_or("", |a| a.name.as_str())).then(|| download.clone()),
                    _ => None,
                };
                self.state = match kept {
                    Some(download) => AppUpdateState::Downloaded { check, download },
                    None => AppUpdateState::Checked { check },
                };
            }
            Err(message) => {
                // A downloaded file stays installable when the network is not there to re-check it.
                self.state = if previous.holds_download() { previous } else { AppUpdateState::CheckFailed { message } };
            }
        }
    }

    /// Starts downloading what the current check selected. `Some` carries the arguments for the host.
    pub fn begin_download(&mut self) -> Option<(u64, AppReleaseAsset, Option<AppReleaseAsset>)> {
        let check = match &self.state {
            AppUpdateState::Checked { check } | AppUpdateState::DownloadFailed { check, .. } => check.clone(),
            _ => return None,
        };
        let asset = check.asset.clone()?;
        self.download_sequence += 1;
        let checksums = check.checksums_asset.clone();
        self.state = AppUpdateState::Downloading { check, received_bytes: 0, total_bytes: asset.size, verifying: false };
        Some((self.download_sequence, asset, checksums))
    }

    fn download_current(&self, sequence: u64) -> bool {
        sequence == self.download_sequence && matches!(self.state, AppUpdateState::Downloading { .. })
    }

    pub fn on_download_event(&mut self, sequence: u64, event: AppUpdateDownloadEvent) {
        if !self.download_current(sequence) {
            return;
        }
        if let AppUpdateState::Downloading { check, received_bytes, total_bytes, verifying } = &mut self.state {
            match event {
                AppUpdateDownloadEvent::Progress { received_bytes: r, total_bytes: t } => {
                    *received_bytes = r;
                    *total_bytes = if t != 0 { t } else { check.asset.as_ref().map_or(0, |a| a.size) };
                }
                AppUpdateDownloadEvent::Verifying => *verifying = true,
            }
        }
    }

    pub fn finish_download(&mut self, sequence: u64, result: Result<AppUpdateDownload, String>) {
        if !self.download_current(sequence) {
            return;
        }
        let AppUpdateState::Downloading { check, .. } = std::mem::take(&mut self.state) else { return };
        self.state = match result {
            Ok(download) => AppUpdateState::Downloaded { check, download },
            Err(message) if is_download_cancelled(&message) => AppUpdateState::Checked { check },
            Err(message) => AppUpdateState::DownloadFailed { check, message },
        };
    }

    /// Hands the downloaded file over. `Some(path)` means call the host and hand the answer to `finish_install`.
    pub fn begin_install(&mut self) -> Option<String> {
        let (check, download) = match &self.state {
            AppUpdateState::Downloaded { check, download } | AppUpdateState::InstallFailed { check, download, .. } => (check.clone(), download.clone()),
            _ => return None,
        };
        let path = download.path.clone();
        self.state = AppUpdateState::Installing { check, download };
        Some(path)
    }

    /// `revealed`: the archive is still there to reveal again. A launched installer is taking the app down, so nothing
    /// changes on screen.
    pub fn finish_install(&mut self, result: Result<String, String>) {
        let AppUpdateState::Installing { check, download } = &self.state else { return };
        let (check, download) = (check.clone(), download.clone());
        match result {
            Ok(action) if action == "revealed" => self.state = AppUpdateState::Downloaded { check, download },
            Ok(_) => {}
            Err(message) => self.state = AppUpdateState::InstallFailed { check, download, message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> AppReleaseAsset {
        AppReleaseAsset { name: name.into(), download_url: format!("https://x/{name}"), size: 100 }
    }

    fn check(tag: &str, with_asset: bool) -> AppUpdateCheck {
        AppUpdateCheck { current_version: "1.0.0".into(), latest_version: "1.1.0".into(), update_available: true, release: AppRelease { tag: tag.into(), name: tag.into(), html_url: "https://x".into(), notes: "n".into(), published_at: String::new() }, asset: with_asset.then(|| asset("a.exe")), checksums_asset: None, checked_at: "2026-01-01T00:00:00Z".into(), in_app_install: true }
    }

    fn download() -> AppUpdateDownload {
        AppUpdateDownload { path: "/tmp/a.exe".into(), file_name: "a.exe".into(), size_bytes: 100, sha256: "x".into(), verification: "verified".into(), flavor: "installer".into() }
    }

    fn checked(c: &mut AppUpdateController, now: i64) {
        let seq = c.begin_check(true, now).unwrap();
        c.finish_check(seq, Ok(check("v1", true)), now);
    }

    // appUpdateController.test.ts
    #[test]
    fn a_recent_check_is_reused_until_forced() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 1000);
        assert!(c.begin_check(false, 2000).is_none());
        assert!(c.begin_check(false, 1000 + CHECK_REUSE_WINDOW_MS).is_some());
    }

    #[test]
    fn a_check_already_out_is_not_asked_again() {
        let mut c = AppUpdateController::default();
        assert!(c.begin_check(false, 0).is_some());
        assert_eq!(c.state, AppUpdateState::Checking);
        assert!(c.begin_check(true, 0).is_none());
    }

    #[test]
    fn a_failed_check_says_why_and_a_retry_runs() {
        let mut c = AppUpdateController::default();
        let seq = c.begin_check(false, 0).unwrap();
        c.finish_check(seq, Err("offline".into()), 0);
        assert_eq!(c.state, AppUpdateState::CheckFailed { message: "offline".into() });
        assert!(c.begin_check(false, 1).is_some());
    }

    #[test]
    fn a_download_runs_from_checked_to_downloaded_with_progress() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let (seq, a, _) = c.begin_download().unwrap();
        assert_eq!(a.name, "a.exe");
        c.on_download_event(seq, AppUpdateDownloadEvent::Progress { received_bytes: 50, total_bytes: 0 });
        assert!(matches!(c.state, AppUpdateState::Downloading { received_bytes: 50, total_bytes: 100, verifying: false, .. }));
        c.on_download_event(seq, AppUpdateDownloadEvent::Verifying);
        assert!(matches!(c.state, AppUpdateState::Downloading { verifying: true, .. }));
        c.finish_download(seq, Ok(download()));
        assert!(matches!(c.state, AppUpdateState::Downloaded { .. }));
    }

    #[test]
    fn a_cancel_settles_back_to_checked_and_a_failure_is_shown() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let (seq, ..) = c.begin_download().unwrap();
        c.finish_download(seq, Err("The download was cancelled".into()));
        assert!(matches!(c.state, AppUpdateState::Checked { .. }));
        let (seq, ..) = c.begin_download().unwrap();
        c.finish_download(seq, Err("disk full".into()));
        assert!(matches!(&c.state, AppUpdateState::DownloadFailed { message, .. } if message == "disk full"));
        assert!(c.begin_download().is_some(), "a failed download can be tried again");
    }

    #[test]
    fn a_check_never_interrupts_a_download() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let _ = c.begin_download().unwrap();
        assert!(c.begin_check(true, 1).is_none());
    }

    #[test]
    fn a_superseded_download_settling_late_is_dropped() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let (old, ..) = c.begin_download().unwrap();
        c.finish_download(old, Err("boom".into()));
        let (new, ..) = c.begin_download().unwrap();
        c.finish_download(old, Ok(download()));
        assert!(matches!(c.state, AppUpdateState::Downloading { .. }));
        c.on_download_event(old, AppUpdateDownloadEvent::Verifying);
        assert!(matches!(c.state, AppUpdateState::Downloading { verifying: false, .. }));
        c.finish_download(new, Ok(download()));
        assert!(matches!(c.state, AppUpdateState::Downloaded { .. }));
    }

    #[test]
    fn a_finished_download_survives_a_check_of_the_same_release_and_a_lost_network() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let (seq, ..) = c.begin_download().unwrap();
        c.finish_download(seq, Ok(download()));
        let s = c.begin_check(true, 1).unwrap();
        assert!(matches!(c.state, AppUpdateState::Downloaded { .. }), "stays visible while the check runs");
        c.finish_check(s, Ok(check("v1", true)), 1);
        assert!(matches!(c.state, AppUpdateState::Downloaded { .. }));
        let s = c.begin_check(true, 2).unwrap();
        c.finish_check(s, Err("offline".into()), 2);
        assert!(matches!(c.state, AppUpdateState::Downloaded { .. }));
    }

    #[test]
    fn a_check_finding_another_release_drops_the_stale_file() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let (seq, ..) = c.begin_download().unwrap();
        c.finish_download(seq, Ok(download()));
        let s = c.begin_check(true, 1).unwrap();
        c.finish_check(s, Ok(check("v2", true)), 1);
        assert!(matches!(c.state, AppUpdateState::Checked { .. }));
    }

    #[test]
    fn install_stays_on_a_launched_installer_and_returns_on_a_reveal() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let (seq, ..) = c.begin_download().unwrap();
        c.finish_download(seq, Ok(download()));
        assert_eq!(c.begin_install().as_deref(), Some("/tmp/a.exe"));
        // A result arriving during install must not take the page out of it.
        c.finish_install(Ok("installer_launched".into()));
        assert!(matches!(c.state, AppUpdateState::Installing { .. }));
        c.finish_install(Ok("revealed".into()));
        assert!(matches!(c.state, AppUpdateState::Downloaded { .. }));
        let _ = c.begin_install().unwrap();
        c.finish_install(Err("nope".into()));
        assert!(matches!(&c.state, AppUpdateState::InstallFailed { message, .. } if message == "nope"));
        assert!(c.begin_install().is_some(), "a failed install can be tried again");
    }

    #[test]
    fn a_check_result_cannot_take_the_page_out_of_installing() {
        let mut c = AppUpdateController::default();
        checked(&mut c, 0);
        let (seq, ..) = c.begin_download().unwrap();
        c.finish_download(seq, Ok(download()));
        let s = c.begin_check(true, 1).unwrap();
        let _ = c.begin_install().unwrap();
        c.finish_check(s, Err("offline".into()), 2);
        assert!(matches!(c.state, AppUpdateState::Installing { .. }));
    }

    #[test]
    fn no_asset_means_nothing_to_download() {
        let mut c = AppUpdateController::default();
        let s = c.begin_check(true, 0).unwrap();
        c.finish_check(s, Ok(check("v1", false)), 0);
        assert!(c.begin_download().is_none());
    }
}
