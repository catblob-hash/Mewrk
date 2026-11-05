//! `WindowChrome.tsx`, `AppBackdrop.tsx`, `GlassPlate.tsx`, `MewrkIcon.tsx`: the window's own top row, and the brand.

use std::cell::Cell;
use std::time::{Duration, Instant};

use mewrk_client::prelude::*;

/// `--topbar-height`.
pub const TOPBAR_HEIGHT: f32 = 44.0;
const NAV_BUTTON: f32 = 28.0;
const NAV_GAP: f32 = 2.0;
/// `--tile-radius`.
pub const TILE_RADIUS: f32 = 10.0;

/// `--shell-nav-inset`: where `ShellNav` starts: past the traffic lights on macOS, which stay in full screen as well.
pub fn shell_nav_inset() -> f32 {
    if cfg!(target_os = "macos") { 84.0 } else { 10.0 }
}

/// `--shell-nav-closed-width`.
pub fn shell_nav_closed_width() -> f32 {
    4.0 * NAV_BUTTON + 3.0 * NAV_GAP
}

thread_local! {
    static LAST_DRAG_PRESS: Cell<Option<(Instant, f32, f32)>> = const { Cell::new(None) };
}

/// `data-tauri-drag-region`: a press on this element's empty stretches moves the window, and a double click zooms it.
/// Whatever handles presses inside it keeps them, since a press goes to the deepest element that listens.
pub fn drag_region(element: E) -> E {
    element.on_pointer(|app: &mut App, e| {
        if e.phase != Phase::Down {
            return;
        }
        let now = Instant::now();
        let again = LAST_DRAG_PRESS.with(|last| {
            let previous = last.get();
            let again = previous.is_some_and(|(at, x, y)| now.duration_since(at) < Duration::from_millis(400) && (x - e.x).abs() < 4.0 && (y - e.y).abs() < 4.0);
            last.set(if again { None } else { Some((now, e.x, e.y)) });
            again
        });
        if again {
            app.window.zoom = true;
        } else {
            app.window.drag = true;
        }
        repaint();
    })
}

/// `ShellNav`: the sidebar drawer, back and forward through the conversations opened before, and the conversation
/// search. One cluster whichever way the sidebar is: it stays put while the sidebar comes and goes under it, so the
/// drawer button never moves out from under the pointer that just pressed it.
pub fn shell_nav(app: &mut App, sidebar_open: bool, sidebar_width: f32) -> E {
    let inset = shell_nav_inset();
    let can_back = app.conversation_history_target(-1).is_some();
    let can_forward = app.conversation_history_target(1).is_some();
    let width = if sidebar_open { sidebar_width - inset - 12.0 } else { shell_nav_closed_width() };
    let drawer = crate::widgets::tool_button("shell-nav-drawer", "PanelLeft", if sidebar_open { t("收起侧栏", "Collapse sidebar") } else { t("打开侧栏", "Open sidebar") }, NAV_BUTTON, 17.0).aria_expanded(sidebar_open).on_click(|app: &mut App| {
        let state = crate::state::shell(app);
        state.sidebar_closed = !state.sidebar_closed;
        repaint();
    });
    let back_label = t("后退到上一个对话", "Back to the previous conversation");
    let back = if can_back { crate::widgets::tool_button("shell-nav-back", "ArrowLeft", back_label, NAV_BUTTON, 16.0).on_click(|app: &mut App| app.step_conversation_history(-1)) } else { crate::widgets::tool_button_disabled("shell-nav-back", "ArrowLeft", back_label, NAV_BUTTON, 16.0) };
    let forward_label = t("前进到下一个对话", "Forward to the next conversation");
    let forward = if can_forward { crate::widgets::tool_button("shell-nav-forward", "ArrowRight", forward_label, NAV_BUTTON, 16.0).on_click(|app: &mut App| app.step_conversation_history(1)) } else { crate::widgets::tool_button_disabled("shell-nav-forward", "ArrowRight", forward_label, NAV_BUTTON, 16.0) };
    let search = crate::widgets::tool_button("shell-nav-search", "Search", t("搜索对话", "Search conversations"), NAV_BUTTON, 16.0).ml_auto().on_click(|app: &mut App| {
        crate::state::shell(app).search_open = true;
        repaint();
    });
    // The gaps between the buttons belong to the title bar, so they move the window.
    drag_region(row().key("shell-nav").absolute().top(0.0).left(inset).w(width).h(TOPBAR_HEIGHT).items_center().gap(NAV_GAP).child(drawer).child(back).child(forward).child(search))
}

const CAPTION_BUTTON: f32 = 32.0;
const CAPTION_GAP: f32 = 2.0;
const CAPTION_INSET: f32 = 12.0;

/// `--window-caption-width`: what the Windows caption buttons take of the top row's right end.
pub fn window_caption_width() -> f32 {
    if cfg!(windows) { 3.0 * CAPTION_BUTTON + 2.0 * CAPTION_GAP + CAPTION_INSET } else { 0.0 }
}

/// `WindowCaptionControls`: minimize, maximize and close for the frameless Windows window, the top bar's own size of button,
/// with a red close. Over the window's top right corner, above the dialogs' dimming as the system's own would be.
pub fn window_caption(app: &mut App) -> Option<E> {
    if !cfg!(windows) {
        return None;
    }
    let t_ = theme();
    let button = |key: &'static str, name: &'static str, size: f32, label: &'static str| row().key(key).center().size(CAPTION_BUTTON, CAPTION_BUTTON).radius(6.0).hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).shrink0().tooltip(label).child(icon(name, size).color(t_.text_secondary));
    let minimize = button("window-caption-minimize", "Minus", 16.0, t("最小化", "Minimize")).on_click(|app: &mut App| {
        app.window.minimize = true;
        repaint();
    });
    let (name, size, label) = if app.window_maximized { ("Copy", 13.0, t("向下还原", "Restore")) } else { ("Square", 12.0, t("最大化", "Maximize")) };
    let maximize = button("window-caption-maximize", name, size, label).on_click(|app: &mut App| {
        app.window.zoom = true;
        repaint();
    });
    let close = row().key("window-caption-close").center().size(CAPTION_BUTTON, CAPTION_BUTTON).radius(6.0).hover_bg(t_.critical).press_bg(t_.critical).shrink0().tooltip(t("关闭", "Close")).child(icon("X", 16.0).color(t_.text_secondary)).on_click(|app: &mut App| {
        app.window.close = true;
        repaint();
    });
    // The gaps around the buttons belong to the title bar, so they move the window.
    Some(drag_region(row().key("window-caption").absolute().top(0.0).right(0.0).w(window_caption_width()).h(TOPBAR_HEIGHT).pr(CAPTION_INSET).items_center().justify_end().gap(CAPTION_GAP).child(minimize).child(maximize).child(close)))
}

pub use mewrk_ui_common::brand::{BRAND_ORANGE, MARK_HEIGHT, MARK_PATH, MARK_WIDTH};

fn hex_of(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// `MewrkMark`: the mark alone, in a colour of the caller's. `hollow` draws its outline, as an idle status does.
pub fn mewrk_mark(height: f32, color: Color, hollow: bool) -> E {
    let width = height * MARK_WIDTH / MARK_HEIGHT;
    let hex = hex_of(color);
    let key = format!("mewrk-mark-{hex}-{hollow}");
    // The stroke is scaled against the box so it stays a little over a pixel whatever the size, as `non-scaling-stroke` did.
    let paint = if hollow { format!(r#"fill="none" stroke="{hex}" stroke-width="{}""#, 1.15 * MARK_HEIGHT / height) } else { format!(r#"fill="{hex}""#) };
    let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-26 -12 156 156"><path d="{MARK_PATH}" {paint}/></svg>"#);
    let side = height * 156.0 / MARK_HEIGHT;
    custom::<App>(move |_, ui: &mut Ui<'_>, r| ui.svg_icon(&key, &svg, r.x + r.w * 0.5, r.y + r.h * 0.5, side)).size(width, height).shrink0().pass_through()
}

/// `app-loading`: the window before there is a document.
pub fn app_loading() -> E {
    let t_ = theme();
    col().flex1().center().gap(10.0).child(mewrk_mark(30.0, BRAND_ORANGE, false)).child(text(t("正在打开 Mewrk…", "Opening Mewrk…")).ty(FINE).color(t_.text_secondary))
}
