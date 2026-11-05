//! `GlobalSettings.tsx` and `GlobalSettingsNavigation.tsx`: the window, its rail of pages, and which page is up.

use mewrk_client::prelude::*;

use crate::logic::session::SettingsSession;
use crate::widgets::*;
use crate::{appearance, dependencies, permissions_page, providers, shortcuts_page, updates, usage_page, web_search_page};

/// The pages, and the retired ids that still lead to one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettingsView {
    Appearance,
    Providers,
    SearchProviders,
    Shortcuts,
    Usage,
    Dependencies,
    Updates,
    Permissions,
}

impl SettingsView {
    /// Retired view IDs remain valid entry points and redirect to active pages. Execution environments were retired as
    /// a page: a machine's settings open from the gear beside it, and environment variables belong to each workspace.
    pub fn from_wire(id: &str) -> Self {
        match id {
            "web_search" | "search_providers" => Self::SearchProviders,
            "advanced" | "memory" | "general" | "execution_environments" | "appearance" => Self::Appearance,
            "hooks" | "capability_catalog" | "agents" | "conversation_presets" | "mcp" | "skills" | "providers" => Self::Providers,
            "shortcuts" => Self::Shortcuts,
            "usage" => Self::Usage,
            "dependencies" => Self::Dependencies,
            "updates" => Self::Updates,
            "permissions" => Self::Permissions,
            _ => Self::Appearance,
        }
    }

    pub fn key(self) -> u64 {
        self as u64 + 1
    }
}

/// One opening of the window: closing it ends the session and whatever pages asked the host in it.
#[derive(Default)]
pub struct WindowState {
    pub view: Option<SettingsView>,
    pub session: SettingsSession,
}

/// Opens the window on a page, the way `openGlobalSettings(view)` does.
pub fn open(app: &mut App, view: SettingsView) {
    let window = app.local::<WindowState>();
    if window.view.is_none() {
        window.session = SettingsSession::default();
    }
    window.view = Some(view);
    repaint();
}

/// Closes it. Every page's state goes with it, and answers still on their way find themselves stale.
pub fn close(app: &mut App) {
    let view = app_view(app);
    leave_page(app, view);
    *app.local::<WindowState>() = WindowState::default();
    repaint();
}

pub fn is_open(app: &mut App) -> bool {
    app.local::<WindowState>().view.is_some()
}

pub fn current_view(app: &mut App) -> Option<SettingsView> {
    app.local::<WindowState>().view
}

fn app_view(app: &mut App) -> Option<SettingsView> {
    app.local::<WindowState>().view
}

/// What React does when a page unmounts: its state goes, and its running work no longer has anywhere to report.
fn leave_page(app: &mut App, view: Option<SettingsView>) {
    match view {
        Some(SettingsView::Providers) => providers::unmount(app),
        Some(SettingsView::SearchProviders) => web_search_page::unmount(app),
        Some(SettingsView::Appearance) => appearance::unmount(app),
        Some(SettingsView::Shortcuts) => shortcuts_page::unmount(app),
        Some(SettingsView::Usage) => usage_page::unmount(app),
        Some(SettingsView::Dependencies) => dependencies::unmount(app),
        Some(SettingsView::Updates) => updates::unmount(app),
        Some(SettingsView::Permissions) | None => {}
    }
}

pub fn select_view(app: &mut App, next: SettingsView) {
    let previous = app_view(app);
    if previous == Some(next) {
        return;
    }
    leave_page(app, previous);
    app.local::<WindowState>().view = Some(next);
    repaint();
}

fn navigation(view: SettingsView) -> E {
    let window_title = t("全局设置", "Global settings");
    settings_nav(
        t("全局设置分类", "Global settings categories"),
        window_title,
        vec![
            (Some(t("提供商", "Providers")), vec![(SettingsView::Providers, "Cloud", t("模型提供商", "Model providers")), (SettingsView::SearchProviders, "Search", t("搜索提供商", "Search providers"))]),
            (Some(t("偏好", "Preferences")), vec![(SettingsView::Appearance, "Palette", t("外观", "Appearance"))]),
            (Some(t("效率", "Efficiency")), vec![(SettingsView::Shortcuts, "Command", t("快捷键", "Keyboard shortcuts")), (SettingsView::Usage, "ChartColumn", t("用量统计", "Usage statistics"))]),
            (Some(t("系统", "System")), vec![(SettingsView::Dependencies, "Terminal", t("环境依赖", "Dependencies")), (SettingsView::Updates, "Download", t("版本更新", "Updates")), (SettingsView::Permissions, "ShieldCheck", t("权限", "Permissions"))]),
        ],
        view,
        SettingsView::key,
        select_view,
    )
}

/// The window's body: the rail and the selected page. For a caller with a window of its own to put it in.
pub fn global_settings_body(app: &mut App, view: SettingsView) -> E {
    let content = match view {
        SettingsView::Appearance => appearance::appearance_settings(app),
        SettingsView::Providers => providers::api_provider_settings(app),
        SettingsView::SearchProviders => web_search_page::web_search_settings(app),
        SettingsView::Shortcuts => shortcuts_page::shortcut_settings(app),
        SettingsView::Usage => usage_page::usage_settings(app),
        SettingsView::Dependencies => dependencies::dependency_settings(app),
        SettingsView::Updates => updates::update_settings(app),
        SettingsView::Permissions => permissions_page::permissions_settings(app),
    };
    // A rail page scrolls its own columns; every other page scrolls as one.
    let scrolling = !matches!(view, SettingsView::Providers | SettingsView::SearchProviders);
    let body = if scrolling { col().key(("settings-content", view.key())).flex1().min_w(0.0).min_h(0.0).scroll_y().child(content) } else { col().key(("settings-content", view.key())).flex1().min_w(0.0).min_h(0.0).child(content) };
    settings_layout(t("全局设置", "Global settings"), navigation(view), body).bg(theme().card_secondary).child(clipboard_pump())
}

/// The whole window: `<Dialog sidebar width="1040px"><GlobalSettings/></Dialog>`. `None` while it's closed.
pub fn global_settings_window(app: &mut App) -> Option<E> {
    let view = app_view(app)?;
    let (width, height) = settings_window_size();
    let close_x = icon_button::<App>("X", tf("关闭 {title}", "Close {title}", &[("title", &t("全局设置", "Global settings"))])).key("settings-close").absolute().top(8.0).right(12.0).on_click(close);
    let body = col().h(height).w_full().child(global_settings_body(app, view)).child(close_x);
    Some(dialog(width, body, Some(Rc::new(close))).aria_label(t("全局设置", "Global settings")))
}
