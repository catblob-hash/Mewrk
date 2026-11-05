//! The Permissions page, and the sheet of the first launch that asks for the same two things.
//!
//! Upstream has neither: its macOS build lets the system ask. Here a read of the keychain never raises a dialog on its
//! own (`credential_vault.rs`), and Full Disk Access has no dialog to raise, so this is where they get asked for.

use mewrk_client::prelude::*;
use serde::Deserialize;

use crate::widgets::*;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiskAccess {
    Granted,
    Denied,
    #[default]
    Unknown,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum KeychainAccess {
    Granted,
    ThisLaunchOnly,
    #[default]
    NeedsPermission,
    Refused,
    NotApplicable,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsSnapshot {
    pub full_disk_access: DiskAccess,
    pub keychain: KeychainAccess,
    #[serde(default)]
    pub app_bundle: Option<String>,
    #[serde(default)]
    pub ad_hoc_signed: bool,
    #[serde(default)]
    pub prompted: bool,
}

impl PermissionsSnapshot {
    /// Whether this system has anything to grant at all.
    pub fn applies(&self) -> bool {
        self.full_disk_access != DiskAccess::NotApplicable || self.keychain != KeychainAccess::NotApplicable
    }

    pub fn all_granted(&self) -> bool {
        matches!(self.full_disk_access, DiskAccess::Granted | DiskAccess::NotApplicable) && matches!(self.keychain, KeychainAccess::Granted | KeychainAccess::NotApplicable)
    }
}

/// Not reset with the page: the sheet shows the same answers, and a page opened later has them at once.
#[derive(Default)]
pub struct PermissionsState {
    pub snapshot: Option<PermissionsSnapshot>,
    pub started: bool,
    /// Waiting on the keychain's dialog.
    pub asking_keychain: bool,
    pub first_launch_open: bool,
    pub error: Option<String>,
    request: u64,
}

pub fn state(app: &mut App) -> &mut PermissionsState {
    app.local::<PermissionsState>()
}

fn apply(app: &mut App, request: u64, result: Result<PermissionsSnapshot, String>) {
    let s = state(app);
    if s.request != request {
        return;
    }
    match result {
        Ok(snapshot) => {
            s.snapshot = Some(snapshot);
            s.error = None;
        }
        Err(e) => s.error = Some(e),
    }
    repaint();
}

/// Asks where everything stands. This never raises a dialog.
pub fn refresh(app: &mut App) {
    let s = state(app);
    s.started = true;
    s.request += 1;
    let request = s.request;
    app.invoke::<PermissionsSnapshot>("permissions_status", json!({}), move |app, result| apply(app, request, result));
}

/// Back from System Settings, most likely: the switch may have moved.
pub fn window_refocused(app: &mut App) {
    if state(app).started {
        refresh(app);
    }
}

/// Once the window is up: the sheet, unless it has been answered or there is nothing to ask for.
pub fn first_launch(app: &mut App) {
    let s = state(app);
    s.started = true;
    s.request += 1;
    let request = s.request;
    app.invoke::<PermissionsSnapshot>("permissions_status", json!({}), move |app, result| {
        let answered = result.as_ref().map_or(true, |snapshot| snapshot.prompted || !snapshot.applies());
        let nothing_to_ask = result.as_ref().is_ok_and(PermissionsSnapshot::all_granted);
        apply(app, request, result);
        if answered {
            return;
        }
        if nothing_to_ask {
            mark_prompted(app);
        } else {
            state(app).first_launch_open = true;
        }
    });
}

fn mark_prompted(app: &mut App) {
    app.invoke_logged("mark_permissions_prompted", json!({}));
}

fn close_first_launch(app: &mut App) {
    state(app).first_launch_open = false;
    mark_prompted(app);
    repaint();
}

fn ask_for_keychain(app: &mut App) {
    let s = state(app);
    s.asking_keychain = true;
    s.request += 1;
    let request = s.request;
    repaint();
    app.invoke::<PermissionsSnapshot>("request_keychain_access", json!({}), move |app, result| {
        state(app).asking_keychain = false;
        apply(app, request, result);
    });
}

fn open_pane(app: &mut App, pane: &str) {
    app.invoke_logged("open_privacy_settings", json!({ "pane": pane }));
}

fn reveal_app(app: &mut App, path: String) {
    app.invoke_logged("reveal_path_in_file_manager", json!({ "path": path }));
}

fn status_pill(icon_name: &'static str, label: &str, color: Color, background: Color) -> E {
    row().h(20.0).px(7.0).gap(4.0).items_center().radius(4.0).shrink0().bg(background).child(icon(icon_name, 11.0).color(color)).child(text(label.to_owned()).ty(FINE_STRONG).color(color).nowrap())
}

fn granted_pill(label: &str) -> E {
    let t_ = theme();
    status_pill("Check", label, t_.success, t_.success_bg)
}

fn missing_pill(label: &str) -> E {
    let t_ = theme();
    status_pill("AlertTriangle", label, t_.caution, t_.caution_bg)
}

fn refused_pill(label: &str) -> E {
    let t_ = theme();
    status_pill("X", label, t_.critical, t_.critical_bg)
}

fn controls() -> E {
    row().gap(8.0).items_center().wrap().justify_end()
}

fn disk_row(snapshot: &PermissionsSnapshot, place: &'static str) -> E {
    let granted = snapshot.full_disk_access == DiskAccess::Granted;
    let pill = match snapshot.full_disk_access {
        DiskAccess::Granted => granted_pill(t("已开启", "On")),
        DiskAccess::Denied => missing_pill(t("未开启", "Off")),
        _ => missing_pill(t("无法确认", "Can't tell")),
    };
    let purpose = help(t("让 Mewrk 和它启动的程序读写任何位置的项目，macOS 不再逐个文件夹询问。", "Lets Mewrk and the programs it starts reach projects anywhere, without macOS asking folder by folder."));
    let steps = (!granted).then(|| help(t("在系统设置的列表中打开 Mewrk；列表里没有就点 +，或把 Mewrk 拖进去。开启后 macOS 可能要求重新打开 Mewrk。", "Turn Mewrk on in the list in System Settings. If it isn't listed, press + or drag Mewrk in. macOS may ask to reopen Mewrk afterwards.")));
    let reveal = snapshot.app_bundle.clone().filter(|_| !granted).map(|bundle| small_subtle_button(t("在访达中显示", "Show in Finder")).key(("permission-reveal", place)).on_click(move |app: &mut App| reveal_app(app, bundle.clone())));
    let open = small_button(t("打开系统设置", "Open System Settings")).key(("permission-disk", place)).on_click(|app: &mut App| open_pane(app, "fullDiskAccess"));
    setting_row(t("完全磁盘访问", "Full Disk Access"), Some(col().gap(2.0).child(purpose).child(steps)), controls().child(pill).child(reveal).child(open), false)
}

fn keychain_row(snapshot: &PermissionsSnapshot, asking: bool, place: &'static str) -> E {
    let pill = match snapshot.keychain {
        KeychainAccess::Granted => granted_pill(t("已允许", "Allowed")),
        KeychainAccess::ThisLaunchOnly => missing_pill(t("仅本次允许", "This launch only")),
        KeychainAccess::Refused => refused_pill(t("已拒绝", "Refused")),
        _ => missing_pill(t("未允许", "Not allowed")),
    };
    let purpose = help(t("所有 API Key 都由一把主密钥保护，所以只需要允许一次。", "One master key protects every API key, so this only needs allowing once."));
    let steps = match snapshot.keychain {
        KeychainAccess::Granted | KeychainAccess::NotApplicable => None,
        KeychainAccess::Refused => Some(help(t("macOS 没有放行。如果系统弹窗没有出现，通常是 Mewrk 运行中被重新构建过：退出并重新打开后再试。", "macOS did not let it through. If no system dialog appeared, Mewrk was probably rebuilt while running: quit, reopen and try again."))),
        _ => Some(help(t("点「允许」后，在系统弹窗中输入登录密码并选择「始终允许」。只选「允许」的话，下次启动还会再问。", "Press Allow, then enter your login password in the system dialog and choose “Always Allow”. Plain “Allow” asks again on the next launch."))),
    };
    let action = (!matches!(snapshot.keychain, KeychainAccess::Granted | KeychainAccess::NotApplicable)).then(|| {
        let label = if snapshot.keychain == KeychainAccess::NeedsPermission { t("允许", "Allow") } else { t("重新允许", "Allow again") };
        let glyph = asking.then(|| spinner(12.0));
        small_accent_button(label).key(("permission-keychain", place)).disabled(asking).aria_busy(asking).on_click(ask_for_keychain).child(glyph)
    });
    setting_row(t("钥匙串", "Keychain"), Some(col().gap(2.0).child(purpose).child(steps)), controls().child(pill).child(action), false)
}

/// The two rows that need the person, for the page and the sheet alike.
fn granted_by_hand(app: &mut App, place: &'static str) -> Option<(E, E)> {
    let s = state(app);
    let asking = s.asking_keychain;
    let snapshot = s.snapshot.clone()?;
    Some((first_row(disk_row(&snapshot, place)), keychain_row(&snapshot, asking, place)))
}

/// What's different about this copy of the app, in the words that explain what the rows say.
fn build_notes(snapshot: &PermissionsSnapshot) -> Vec<E> {
    let mut notes = Vec::new();
    if snapshot.ad_hoc_signed {
        notes.push(help(t("这个版本是临时签名（ad hoc）的：每次重新构建后，macOS 都把它当作新应用。钥匙串要重新允许一次；完全磁盘访问要在系统设置里移除 Mewrk 再重新添加，即使开关还显示为打开。", "This build is ad hoc signed, so macOS treats every rebuild as a new app. The keychain needs allowing again, and Full Disk Access needs Mewrk removed and added back in System Settings, even if its switch still shows on.")));
    }
    if snapshot.app_bundle.is_none() {
        notes.push(help(t("Mewrk 现在是直接运行的可执行文件，不是 Mewrk.app。从终端启动时，完全磁盘访问算在那个终端上。", "Mewrk is running as a bare executable, not Mewrk.app. Started from a terminal, Full Disk Access is that terminal's.")));
    }
    notes
}

fn on_demand_row(title: &str, description: &str, pane: &'static str, first: bool) -> E {
    let open = small_button(t("打开", "Open")).key(("permission-pane", pane)).aria_label(tf("在系统设置中打开{title}", "Open {title} in System Settings", &[("title", &title)])).on_click(move |app: &mut App| open_pane(app, pane));
    let row = setting_row(title.to_owned(), Some(help(description.to_owned())), open, false);
    if first {
        first_row(row)
    } else {
        row
    }
}

/// The page.
pub fn permissions_settings(app: &mut App) -> E {
    if !state(app).started {
        refresh(app);
    }
    let heading = page_heading(Some(page_title(t("权限", "Permissions"))), page_description(t("Mewrk 和它启动的程序用到的系统权限。系统只允许在系统设置里更改它们。", "The system permissions Mewrk and the programs it starts use. The system only lets them change in System Settings.")), None);
    let rows = granted_by_hand(app, "page");
    let s = state(app);
    let snapshot = s.snapshot.clone();
    let error = s.error.clone().map(error_line);
    let mut page = page().child(heading).child(error);
    let Some(snapshot) = snapshot else { return page };
    if !snapshot.applies() {
        return page.child(card(None).child(help(t("这个系统不需要为 Mewrk 单独授权。", "This system has nothing to grant Mewrk."))));
    }
    if let Some((disk, keychain)) = rows {
        page = page.child(card(Some(t("需要你授权", "Needs you"))).child(disk).child(keychain));
    }
    let notes = build_notes(&snapshot);
    if !notes.is_empty() {
        page = page.child(col().mt(10.0).px(14.0).gap(6.0).children(notes));
    }
    page.child(
        card(Some(t("用到时才询问", "Asked for when needed")))
            .child(help(t("程序第一次用到时 macOS 会询问，之后可以在系统设置里更改。开启完全磁盘访问后，文件和文件夹不再询问。", "macOS asks the first time a program needs one, and they can be changed in System Settings afterwards. With Full Disk Access on, it no longer asks about files and folders.")).mb(6.0))
            .child(on_demand_row(t("文件和文件夹", "Files and Folders"), t("桌面、文稿、下载、外接和网络宗卷。", "Desktop, Documents, Downloads, removable and network volumes."), "filesAndFolders", true))
            .child(on_demand_row(t("自动化", "Automation"), t("终端里的脚本控制其他应用。", "Scripts in the terminal controlling other apps."), "automation", false))
            .child(on_demand_row(t("麦克风", "Microphone"), t("你在 Mewrk 中运行的程序使用麦克风。", "Programs you run in Mewrk using the microphone."), "microphone", false))
            .child(on_demand_row(t("摄像头", "Camera"), t("你在 Mewrk 中运行的程序使用摄像头。", "Programs you run in Mewrk using the camera."), "camera", false))
            .child(on_demand_row(t("本地网络", "Local Network"), t("SSH 主机、Ollama 这样的模型服务和开发服务器。", "SSH machines, model servers such as Ollama, and development servers."), "localNetwork", false))
            .child(on_demand_row(t("蓝牙", "Bluetooth"), t("你在 Mewrk 中运行的程序使用蓝牙。", "Programs you run in Mewrk using Bluetooth."), "bluetooth", false)),
    )
}

/// The sheet of the first launch. `None` once it has been answered.
pub fn first_launch_dialog(app: &mut App) -> Option<E> {
    if !state(app).first_launch_open {
        return None;
    }
    let (disk, keychain) = granted_by_hand(app, "sheet")?;
    let snapshot = state(app).snapshot.clone()?;
    let all_granted = snapshot.all_granted();
    let description = help(t("Mewrk 要运行你的项目和代理工具，也要保管 API Key。给它这两项权限后，macOS 就不会再一个一个地询问。", "Mewrk runs your projects and agent tools, and keeps your API keys. With these two, macOS stops asking one thing at a time."));
    let body = col().child(card(None).mt(0.0).child(disk).child(keychain)).children(build_notes(&snapshot).into_iter().map(|note| note.mt(8.0))).child(help(t("之后可以在「全局设置 → 权限」中管理。", "You can manage these later under Global settings → Permissions.")).mt(12.0));
    let finish = if all_granted { accent_button::<App>(t("完成", "Done")) } else { button::<App>(t("稍后", "Later")) };
    Some(modal("permissions-first-launch", t("启用 Mewrk 需要的权限", "Enable the permissions Mewrk needs"), Some(description), 620.0, body, Some(finish.key("permissions-first-launch-done").on_click(close_first_launch)), close_first_launch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(full_disk_access: DiskAccess, keychain: KeychainAccess) -> PermissionsSnapshot {
        PermissionsSnapshot { full_disk_access, keychain, ..Default::default() }
    }

    #[test]
    fn a_one_time_allow_or_an_unknown_disk_is_not_all_granted() {
        assert!(snapshot(DiskAccess::Granted, KeychainAccess::Granted).all_granted());
        assert!(!snapshot(DiskAccess::Granted, KeychainAccess::ThisLaunchOnly).all_granted());
        assert!(!snapshot(DiskAccess::Unknown, KeychainAccess::Granted).all_granted());
        assert!(!snapshot(DiskAccess::Denied, KeychainAccess::Granted).all_granted());
    }

    #[test]
    fn a_system_without_these_has_nothing_to_ask_for() {
        let elsewhere = snapshot(DiskAccess::NotApplicable, KeychainAccess::NotApplicable);
        assert!(!elsewhere.applies());
        assert!(elsewhere.all_granted());
        assert!(snapshot(DiskAccess::Denied, KeychainAccess::NotApplicable).applies());
    }

    #[test]
    fn the_host_s_answer_reads_as_these_types() {
        let answer: PermissionsSnapshot = serde_json::from_value(json!({ "fullDiskAccess": "denied", "keychain": "thisLaunchOnly", "appBundle": "/Applications/Mewrk.app", "adHocSigned": true, "prompted": false })).unwrap();
        assert_eq!(answer.full_disk_access, DiskAccess::Denied);
        assert_eq!(answer.keychain, KeychainAccess::ThisLaunchOnly);
        assert_eq!(answer.app_bundle.as_deref(), Some("/Applications/Mewrk.app"));
        assert!(answer.ad_hoc_signed);
    }
}
