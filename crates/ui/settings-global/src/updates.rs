//! `UpdateSettings/index.tsx`: the version, and checking for a newer one.
//!
//! Two things upstream's page reports don't exist in this build, so they aren't drawn.
//!
//! The AI SDK card is about a Node sidecar fetched from a download mirror at startup. The model layer here is compiled into the app, so there
//! is no component with a version or a download of its own: the version card already says all there is to say.
//!
//! The update check asks GitHub for the latest release of `catblob-hash/Mewrk`, and the mirror's `latest.json` when GitHub can't answer. The
//! mirror's manifest still names the last Tauri release, 1.0.3, now in `catblob-hash/Mewrk-previous`. That is not an update of this build: 1.0.0
//! would compare below it, and whenever GitHub couldn't answer the page would say "new version" and send people to a different application's
//! .dmg. Until it has a manifest of its own, `UpdatesPage::release_channel` stays off and the page asks nothing of GitHub. The pipeline stays,
//! tested, for the day it does.

use mewrk_client::prelude::*;
use serde::Deserialize;

use crate::logic::app_update::{AppUpdateCheck, AppUpdateController, AppUpdateDownload, AppUpdateDownloadEvent, AppUpdateState};
use crate::logic::usage::{format_iso_date, format_iso_datetime, now_ms};
use crate::widgets::*;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppVersionInfo {
    pub version: String,
    pub flavor: String,
    #[serde(default)]
    pub development_build: bool,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub repository_url: String,
    #[serde(default)]
    pub releases_url: String,
    #[serde(default)]
    pub executable_dir: String,
}

#[derive(Default)]
pub struct UpdatesPage {
    generation: u64,
    started: bool,
    pub info: Option<AppVersionInfo>,
    pub info_error: Option<String>,
    /// Whether this build has releases to check for: see the top of this file. Nothing but a test turns it on yet.
    pub release_channel: bool,
}

fn page_state(app: &mut App) -> &mut UpdatesPage {
    app.local::<UpdatesPage>()
}

pub fn unmount(app: &mut App) {
    let s = page_state(app);
    let generation = s.generation + 1;
    let release_channel = s.release_channel;
    *s = UpdatesPage { generation, release_channel, ..Default::default() };
}

fn controller(app: &mut App) -> &mut AppUpdateController {
    app.local::<AppUpdateController>()
}

pub fn update_state(app: &mut App) -> AppUpdateState {
    controller(app).state.clone()
}

/// Fetches the latest release. Without `force` a recent successful check is kept and nothing is asked.
pub fn check(app: &mut App, force: bool) {
    let Some(sequence) = controller(app).begin_check(force, now_ms()) else { return };
    repaint();
    app.invoke::<AppUpdateCheck>("check_app_update", json!({}), move |app, result| {
        controller(app).finish_check(sequence, result, now_ms());
        repaint();
    });
}

pub fn download(app: &mut App) {
    let Some((sequence, asset, checksums)) = controller(app).begin_download() else { return };
    repaint();
    let channel = app.open_channel(move |app, event| {
        if let Ok(event) = serde_json::from_value::<AppUpdateDownloadEvent>(event) {
            controller(app).on_download_event(sequence, event);
            repaint();
        }
    });
    let closing = channel.clone();
    app.invoke::<AppUpdateDownload>("download_app_update", json!({ "asset": asset, "checksumsAsset": checksums, "onProgress": channel.arg() }), move |app, result| {
        closing.close();
        controller(app).finish_download(sequence, result);
        repaint();
    });
}

/// A failure means the host never got the request, so the download is still running and the state keeps saying so.
fn cancel_download(app: &mut App) {
    if matches!(controller(app).state, AppUpdateState::Downloading { .. }) {
        app.invoke::<Value>("cancel_app_update_download", json!({}), |_, _| {});
    }
}

fn install(app: &mut App) {
    let Some(path) = controller(app).begin_install() else { return };
    repaint();
    app.invoke::<Value>("install_app_update", json!({ "path": path }), |app, result| {
        let outcome = result.map(|v| v.get("action").and_then(Value::as_str).unwrap_or_default().to_owned());
        controller(app).finish_install(outcome);
        repaint();
    });
}

/// `formatBytes` of the page, with its binary units.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 { format!("{value:.0} {}", units[unit]) } else { format!("{value:.1} {}", units[unit]) }
}

fn flavor_label(info: &AppVersionInfo) -> String {
    match info.flavor.as_str() {
        "installer" => t("安装版", "Installer").to_owned(),
        "portable" => t("便携版", "Portable").to_owned(),
        "store_msix" => t("MSIX（Microsoft Store）", "MSIX (Microsoft Store)").to_owned(),
        "sideloaded_msix" => "MSIX".to_owned(),
        "mac_app" => "macOS".to_owned(),
        _ => t("其他", "Other").to_owned(),
    }
}

/// A Store install is the Store's to update, so the page never checks for it.
fn checks_for_updates(info: Option<&AppVersionInfo>) -> bool {
    info.is_none_or(|i| i.flavor != "store_msix")
}

/// Only the two Windows editions the release ships files for download from inside the app.
fn downloads_updates(info: &AppVersionInfo) -> bool {
    matches!(info.flavor.as_str(), "installer" | "portable")
}

fn release_page_note(info: Option<&AppVersionInfo>) -> &'static str {
    match info.map(|i| i.flavor.as_str()) {
        Some("mac_app") => t("请到发布页下载新的 .dmg。", "Download the new .dmg from the release page."),
        Some("sideloaded_msix") => t("请到发布页下载并安装新的 .msix。", "Download and install the new .msix from the release page."),
        _ => t("请到发布页下载新版本。", "Download the new version from the release page."),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Neutral,
    Success,
    Accent,
    Danger,
}

/// What the card's first line says.
pub fn status_of(state: &AppUpdateState, info: Option<&AppVersionInfo>) -> (Tone, String, String) {
    let chinese = !mewrk_client::i18n::is_english();
    match state {
        AppUpdateState::Idle | AppUpdateState::Checking => (Tone::Neutral, t("正在检查更新…", "Checking for updates…").to_owned(), t("正在向 GitHub Releases 查询最新版本。", "Asking GitHub Releases for the latest version.").to_owned()),
        AppUpdateState::CheckFailed { message } => (Tone::Danger, t("检查更新失败", "Update check failed").to_owned(), message.clone()),
        other => {
            let check = other.check().expect("every other phase has a check");
            if !check.update_available {
                return (Tone::Success, t("已是最新版本", "You are up to date").to_owned(), tf("v{version} 就是最新发布；检查于 {time}。", "v{version} is the latest release; checked {time}.", &[("version", &check.current_version), ("time", &format_iso_datetime(&check.checked_at, chinese))]));
            }
            let published = if check.release.published_at.is_empty() { String::new() } else { tf("，发布于 {date}", ", published {date}", &[("date", &format_iso_date(&check.release.published_at, chinese))]) };
            // An edition that does not download its updates is sent to the release page; one that does and finds none is
            // told the release lacks it.
            let flavor_note = if !check.in_app_install {
                release_page_note(info).to_owned()
            } else if let (Some(info), None) = (info, &check.asset) {
                tf("这次发布没有提供{flavor}的安装文件，请到发布页手动下载。", "This release has no file for the {flavor} flavor; download it from the release page.", &[("flavor", &flavor_label(info))])
            } else {
                String::new()
            };
            (Tone::Accent, tf("发现新版本 v{version}", "Version v{version} is available", &[("version", &check.latest_version)]), tf("当前 v{current}{published}。{flavorNote}", "You have v{current}{published}. {flavorNote}", &[("current", &check.current_version), ("published", &published), ("flavorNote", &flavor_note)]).trim().to_owned())
        }
    }
}

fn version_card(info: Option<&AppVersionInfo>, info_error: Option<&str>, release_channel: bool) -> E {
    let t_ = theme();
    let badge = |s: String, mono: bool, color: Color, ground: Color| row().min_h(18.0).px(6.0).items_center().radius(5.0).bg(ground).child(text(s).ty(if mono { MONO_SMALL } else { FINE }).color(color).nowrap());
    let mut copy = col().flex1().min_w(0.0).child(row().gap(8.0).items_baseline().child(text("Mewrk").ty(BODY_STRONG).nowrap()).child(text(info.map_or_else(|| "—".to_owned(), |i| tf("v{version}", "v{version}", &[("version", &i.version)]))).key("current-version").ty(MONO_BODY).nowrap()));
    if let Some(i) = info {
        // Installer and portable are the two Windows editions; elsewhere the flavor only says no uninstaller was found.
        copy = copy.child(row().mt(6.0).gap(4.0).wrap().items_center().child((i.os == "windows").then(|| badge(flavor_label(i), false, t_.text_secondary, t_.subtle_hover))).child(badge(i.arch.clone(), true, t_.text_secondary, t_.subtle_hover)).child(i.development_build.then(|| badge(t("开发构建", "Development build").to_owned(), false, t_.caution, t_.caution.with_alpha(0.14))))).child(text(i.executable_dir.clone()).ty(MONO_SMALL).color(t_.text_tertiary).mt(8.0).selectable());
    }
    copy = copy.child(info_error.map(|e| error_line(e.to_owned()).mt(8.0)));
    let links = info.map(|i| {
        // A Store install comes from the Store, and this build has no releases of its own: neither points at other downloads.
        col().gap(2.0).items_end().shrink0().child(link("update-source", t("源代码", "Source"), i.repository_url.clone())).child((release_channel && checks_for_updates(Some(i))).then(|| link("update-releases", t("全部发布", "All releases"), i.releases_url.clone())))
    });
    card(None).child(row().gap(12.0).items_start().child(mewrk_ui_common::brand::mewrk_icon(44.0)).child(copy).child(links))
}

/// `update-settings__status-icon`: the glyph on a plate of its tone.
fn status_plate(glyph: E, ground: Color) -> E {
    row().center().size(32.0, 32.0).shrink0().radius(9.0).bg(ground).child(glyph)
}

/// `update-settings__note`: a small line with what it's about before it.
fn note(glyph: Option<E>, line: impl Into<Arc<str>>, color: Color) -> E {
    row().gap(6.0).items_start().child(glyph.map(|g| row().mt(2.0).shrink0().child(g))).child(text(line).ty(FINE).color(color).flex1().min_w(0.0))
}

fn release_notes(check: &AppUpdateCheck) -> E {
    let t_ = theme();
    let notes = check.release.notes.trim().to_owned();
    let body = if notes.is_empty() { help(t("这次发布没有填写说明。", "This release has no notes.")) } else { markdown_view(("release-notes", check.release.tag.as_str()), notes) };
    col().gap(6.0).mt(12.0).pt(10.0).border_top(t_.divider).role(Role::Region).aria_label(t("发布说明", "Release notes")).child(row().items_center().justify_between().gap(8.0).child(text(check.release.name.clone()).ty(CAPTION_STRONG).ellipsis().flex1()).child(link("update-notes-link", t("在 GitHub 上查看", "View on GitHub"), check.release.html_url.clone()))).child(body)
}

/// Markdown laid out with fluent's stream, sized by what it needs at the width it's given.
fn markdown_view(key: impl std::hash::Hash, source: String) -> E {
    let t_ = theme();
    let state = use_state(("markdown", hash_key(key)), || None::<(f32, fluent::md::MdStream)>);
    let palette = fluent::md::Palette { text: t_.text_primary, muted: t_.text_secondary, code: t_.code_bg };
    let (paint_state, source2) = (state.clone(), source.clone());
    let lay_out = move |slot: &mut Option<(f32, fluent::md::MdStream)>, ui: &mut Ui<'_>, width: f32, source: &str| {
        if slot.as_ref().is_none_or(|(w, _)| (*w - width).abs() > 0.5) {
            let mut md = fluent::md::MdStream::new(width * ui.scale, BODY_SMALL, palette);
            md.push(ui.text, source);
            md.finish(ui.text);
            *slot = Some((width, md));
        }
    };
    custom::<App>(move |_, ui, r| {
        let mut slot = paint_state.borrow_mut();
        lay_out(&mut slot, ui, r.w, &source2);
        if let Some((_, md)) = slot.as_mut() {
            ui.doc(&mut md.doc, r.x, r.y);
        }
    })
    .w_full()
    .when(a11y_active(), |e| e.aria_label(source.clone()))
    .measure(move |_, ui, known, avail| {
        let width = known.unwrap_or(if avail.is_finite() { avail } else { 480.0 });
        let mut slot = state.borrow_mut();
        lay_out(&mut slot, ui, width, &source);
        (width, slot.as_ref().map_or(0.0, |(_, md)| md.doc.height() / ui.scale))
    })
}

fn update_card(app: &mut App, info: Option<&AppVersionInfo>) -> E {
    let t_ = theme();
    let state = controller(app).state.clone();
    let (tone, title, detail) = status_of(&state, info);
    let checking = matches!(state, AppUpdateState::Idle | AppUpdateState::Checking);
    let check = state.check().cloned();
    let installer = info.is_some_and(|i| i.flavor == "installer");
    let glyph = match tone {
        Tone::Success => status_plate(icon("CheckCircle2", 18.0).color(t_.success), t_.success_bg),
        Tone::Accent => status_plate(icon("ArrowUpCircle", 18.0).color(t_.accent), t_.accent.with_alpha(0.14)),
        Tone::Danger => status_plate(icon("AlertTriangle", 18.0).color(t_.critical), t_.critical_bg),
        Tone::Neutral if checking => status_plate(spinner(18.0), t_.subtle_hover),
        Tone::Neutral => status_plate(icon("RefreshCw", 18.0).color(t_.text_secondary), t_.subtle_hover),
    };
    let mut actions = row().gap(6.0).items_center().shrink0();
    if matches!(state, AppUpdateState::CheckFailed { .. }) {
        actions = actions.child(small_button(t("重试", "Retry")).key("update-retry").on_click(|app: &mut App| check_forced(app)));
    }
    if let Some(c) = &check
        && c.update_available {
            if let Some(asset) = c.asset.as_ref().filter(|_| matches!(state, AppUpdateState::Checked { .. } | AppUpdateState::DownloadFailed { .. })) {
                actions = actions.child(with_icon(icon("Download", 14.0).color(t_.text_on_accent), tf("下载更新（{size}）", "Download update ({size})", &[("size", &format_bytes(asset.size))]), true).key("update-download").on_click(download));
            }
            if c.asset.is_none() {
                let url = c.release.html_url.clone();
                actions = actions.child(with_icon(icon("ExternalLink", 14.0).color(t_.text_on_accent), t("前往发布页", "Open release page"), true).key("update-release-page").on_click(move |app: &mut App| open_url(app, &url)));
            }
        }
    if matches!(state, AppUpdateState::Downloaded { .. } | AppUpdateState::InstallFailed { .. }) {
        actions = actions.child(with_icon(icon(if installer { "ArrowUpCircle" } else { "FolderOpen" }, 14.0).color(t_.text_on_accent), if installer { t("安装并重启", "Install and restart") } else { t("在文件夹中显示", "Show in folder") }, true).key("update-install").on_click(install));
    }
    let mut body = col().child(row().gap(10.0).items_start().child(glyph).child(col().flex1().min_w(0.0).gap(3.0).pt(2.0).when(tone == Tone::Danger, |e| e.role(Role::Alert)).child(text(title).ty(BODY_SMALL_STRONG)).child(text(detail).ty(CAPTION).color(if tone == Tone::Danger { t_.critical } else { t_.text_secondary }))).child(actions));
    if let AppUpdateState::Downloading { received_bytes, total_bytes, verifying, .. } = &state {
        let percent = if *total_bytes > 0 { (((*received_bytes as f64 / *total_bytes as f64) * 100.0).round() as u64).min(100) } else { 0 };
        let label = if *verifying { t("正在校验…", "Verifying…").to_owned() } else { format!("{} / {} · {percent}%", format_bytes(*received_bytes), format_bytes(*total_bytes)) };
        body = body.child(row().mt(12.0).gap(10.0).items_center().child(progress(Some(percent as f32)).aria_label(t("下载进度", "Download progress"))).child(text(label).ty(MONO_SMALL).color(t_.text_secondary).nowrap().min_w(128.0).text_end()).child(icon_button::<App>("X", t("取消下载", "Cancel download")).key("update-cancel").size(26.0, 26.0).disabled(*verifying).on_click(cancel_download)));
    }
    if let AppUpdateState::DownloadFailed { message, .. } = &state {
        body = body.child(note(Some(icon("AlertTriangle", 12.0).color(t_.critical)), tf("下载失败：{message}", "Download failed: {message}", &[("message", message)]), t_.critical).mt(10.0).role(Role::Alert));
    }
    if let AppUpdateState::Downloaded { download, .. } | AppUpdateState::Installing { download, .. } | AppUpdateState::InstallFailed { download, .. } = &state {
        let size = format_bytes(download.size_bytes);
        let verified = if download.verification == "verified" {
            tf("已下载 {file}（{size}），SHA-256 与发布的校验和一致。", "Downloaded {file} ({size}); SHA-256 matches the published checksum.", &[("file", &download.file_name), ("size", &size)])
        } else {
            tf("已下载 {file}（{size}）。这次发布没有提供校验和，完整性由 HTTPS 保证。", "Downloaded {file} ({size}). This release publishes no checksum; HTTPS is the integrity check.", &[("file", &download.file_name), ("size", &size)])
        };
        let how = if installer {
            t("点击「安装并重启」会启动安装程序并关闭 Mewrk；设置与数据保留，安装完成后自动重新打开。", "“Install and restart” launches the installer and closes Mewrk; settings and data are kept, and the app reopens when it finishes.").to_owned()
        } else {
            tf("便携版需要手动替换：在托盘图标菜单选择「关闭 Mewrk」，把压缩包解压到 {dir} 覆盖旧文件，再重新打开。", "The portable flavor is replaced by hand: choose “Quit Mewrk” from the tray icon's menu, unzip the archive over {dir}, then reopen it.", &[("dir", &info.map_or("", |i| i.executable_dir.as_str()))])
        };
        let mut ready = col().gap(5.0).mt(10.0).pt(10.0).border_top(t_.divider).child(note(Some(icon("ShieldCheck", 12.0).color(t_.text_secondary)), verified, t_.text_secondary)).child(note(None, how, t_.text_secondary));
        if matches!(state, AppUpdateState::Installing { .. }) {
            ready = ready.child(note(Some(spinner(12.0)), if installer { t("正在启动安装程序…", "Starting the installer…") } else { t("正在打开文件夹…", "Opening the folder…") }, t_.text_secondary));
        }
        if let AppUpdateState::InstallFailed { message, .. } = &state {
            ready = ready.child(note(Some(icon("AlertTriangle", 12.0).color(t_.critical)), message.clone(), t_.critical).mt(5.0).role(Role::Alert));
        }
        body = body.child(ready);
    }
    if let Some(c) = check.as_ref().filter(|c| c.update_available) {
        body = body.child(release_notes(c));
    }
    card(None).child(body)
}

fn check_forced(app: &mut App) {
    check(app, true);
}

fn store_updates_card() -> E {
    let t_ = theme();
    card(None).child(row().gap(10.0).items_start().child(status_plate(icon("ShieldCheck", 18.0).color(t_.text_secondary), t_.subtle_hover)).child(col().flex1().min_w(0.0).gap(3.0).pt(2.0).child(text(t("由 Microsoft Store 更新", "Updated by the Microsoft Store")).ty(BODY_SMALL_STRONG)).child(text(t("Store 自动更新，应用内不检查、不下载。", "The Store updates it; the app doesn't check or download.")).ty(CAPTION).color(t_.text_secondary))))
}

/// The page.
pub fn update_settings(app: &mut App) -> E {
    let t_ = theme();
    if !page_state(app).started {
        page_state(app).started = true;
        let generation = page_state(app).generation;
        app.invoke::<AppVersionInfo>("app_version_info", json!({}), move |app, result| {
            if page_state(app).generation != generation {
                return;
            }
            let channel = page_state(app).release_channel;
            match result {
                Ok(info) => {
                    let checks = checks_for_updates(Some(&info));
                    page_state(app).info = Some(info);
                    if channel && checks {
                        check(app, false);
                    }
                }
                Err(e) => {
                    page_state(app).info_error = Some(e);
                    // Without the flavor the check still runs; the host refuses it for a Store install.
                    if channel {
                        check(app, false);
                    }
                }
            }
        });
    }
    let (info, info_error, release_channel) = {
        let s = page_state(app);
        (s.info.clone(), s.info_error.clone(), s.release_channel)
    };
    if !release_channel {
        let description = page_description(t("这个版本没有发布渠道，不检查更新。", "This build has no release channel, so it doesn't check for updates."));
        return page().child(page_heading(Some(page_title(t("版本更新", "Updates"))), description, None)).child(version_card(info.as_ref(), info_error.as_deref(), false));
    }
    let state = controller(app).state.clone();
    let checking = matches!(state, AppUpdateState::Idle | AppUpdateState::Checking);
    let busy = matches!(state, AppUpdateState::Downloading { .. } | AppUpdateState::Installing { .. });
    let in_app = checks_for_updates(info.as_ref());
    let description = if !in_app {
        page_description(t("更新由 Microsoft Store 提供。", "Updates come from the Microsoft Store."))
    } else if info.as_ref().is_none_or(downloads_updates) {
        help_with_docs(t("检查并下载新版本。", "Checks for and downloads new versions."), "settings", "updates")
    } else {
        page_description(match info.as_ref().map(|i| i.flavor.as_str()) {
            Some("mac_app") => t("只检查新版本，请到发布页下载 .dmg。", "Only checks for updates; download the .dmg from the release page."),
            Some("sideloaded_msix") => t("不会自动更新，请到发布页获取新的 .msix。", "Not auto-updated; get the new .msix from the release page."),
            _ => t("只检查新版本，请到发布页下载。", "Only checks for updates; download from the release page."),
        })
    };
    let glyph = if checking { spinner(14.0) } else { icon("RefreshCw", 14.0).color(t_.text_secondary) };
    let action = in_app.then(|| with_icon(glyph, t("检查更新", "Check for updates"), false).key("update-check").disabled(checking || busy).aria_busy(checking).on_click(|app: &mut App| check_forced(app)));
    let card_el = if in_app { update_card(app, info.as_ref()) } else { store_updates_card() };
    page().child(page_heading(Some(page_title(t("版本更新", "Updates"))), description, action)).child(version_card(info.as_ref(), info_error.as_deref(), true)).child(card_el)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::app_update::AppRelease;

    fn check_with(update: bool, in_app: bool, asset: bool) -> AppUpdateCheck {
        AppUpdateCheck { current_version: "1.0.0".into(), latest_version: "1.1.0".into(), update_available: update, release: AppRelease { tag: "v1.1.0".into(), name: "1.1.0".into(), html_url: "https://x".into(), notes: String::new(), published_at: String::new() }, asset: asset.then(|| crate::logic::app_update::AppReleaseAsset { name: "a".into(), download_url: "https://x".into(), size: 2048 }), checksums_asset: None, checked_at: "2026-01-01T00:00:00Z".into(), in_app_install: in_app }
    }

    // UpdateSettings.test.tsx
    #[test]
    fn the_status_tells_each_edition_where_its_update_comes_from() {
        let _language = crate::language_for_test(true);
        let mac = AppVersionInfo { flavor: "mac_app".into(), ..Default::default() };
        let (tone, title, detail) = status_of(&AppUpdateState::Checked { check: check_with(true, false, false) }, Some(&mac));
        assert_eq!(tone, Tone::Accent);
        assert_eq!(title, "Version v1.1.0 is available");
        assert!(detail.contains("Download the new .dmg from the release page."), "{detail}");
        let installer = AppVersionInfo { flavor: "installer".into(), ..Default::default() };
        let (_, _, detail) = status_of(&AppUpdateState::Checked { check: check_with(true, true, false) }, Some(&installer));
        assert!(detail.contains("no file for the Installer flavor"), "{detail}");
        let (tone, title, _) = status_of(&AppUpdateState::Checked { check: check_with(false, true, true) }, Some(&installer));
        assert_eq!((tone, title.as_str()), (Tone::Success, "You are up to date"));
        let (tone, _, detail) = status_of(&AppUpdateState::CheckFailed { message: "offline".into() }, None);
        assert_eq!((tone, detail.as_str()), (Tone::Danger, "offline"));
    }

    #[test]
    fn only_a_store_install_skips_the_check_and_only_two_editions_download() {
        assert!(checks_for_updates(None));
        assert!(!checks_for_updates(Some(&AppVersionInfo { flavor: "store_msix".into(), ..Default::default() })));
        assert!(downloads_updates(&AppVersionInfo { flavor: "portable".into(), ..Default::default() }));
        assert!(!downloads_updates(&AppVersionInfo { flavor: "mac_app".into(), ..Default::default() }));
    }

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(200 * 1024 * 1024), "200 MiB");
    }
}
