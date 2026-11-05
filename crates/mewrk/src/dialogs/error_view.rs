//! `ErrorView`: what the user sees when the host cannot read the document. The host has left the file as it was and copied
//! it aside for diagnosis; starting over is a separate, explicit choice that deletes everything, so it says so and asks first.
//!
//! There is no `App` here, since an `App` is a loaded document, so this draws on a small state of its own and `main.rs` drives
//! it. Pressing Reset only records the wish: the reset runs in a new process, once this one has let go of the data folder.

use fluent::el::*;
use fluent::theme::*;
use fluent::ui::{CursorKind, Key};
use mewrk_client::t;
use std::time::{Duration, Instant};

type F = El<FatalApp>;

#[derive(Debug, Default)]
pub struct FatalApp {
    pub message: String,
    /// Why the last reset did not go through.
    pub reset_error: Option<String>,
    /// Whether a reset could help. It cannot while another Mewrk holds the folder: the document is fine then.
    pub can_reset: bool,
    asking: bool,
    pub reset_requested: bool,
    /// The strip along the top was pressed: the window driver starts the move.
    pub drag_requested: bool,
    /// It was pressed twice over: the window driver zooms the window.
    pub zoom_requested: bool,
    last_strip_press: Option<(Instant, f32, f32)>,
}

impl FatalApp {
    pub fn new(message: String, reset_error: Option<String>, can_reset: bool) -> Self {
        Self { message, reset_error, can_reset, asking: false, reset_requested: false, drag_requested: false, zoom_requested: false, last_strip_press: None }
    }
}

pub fn consequence() -> &'static str {
    t(
        "重置会删除所有项目、对话、附件和临时项目的文件夹，以及全部设置和已保存的 API Key、登录状态与搜索凭据。项目文件夹里的文件不受影响。",
        "Resetting deletes all projects, conversations, attachments and Temporary-project folders, along with every setting and the saved API keys, sign-ins and search credentials. Files in your project folders are not touched.",
    )
}

const CONFIRM_KEY: &str = "error-view-confirm";

fn answer_yes(app: &mut FatalApp) {
    app.asking = false;
    app.reset_requested = true;
}

/// `ConfirmDialog` for this one question, with nothing behind it that could answer another.
fn confirm_reset() -> F {
    let t_ = theme();
    let question = t("删除全部数据并重置 Mewrk？", "Delete everything and reset Mewrk?");
    let header = row().items_center().gap(12.0).pt(9.0).pb(8.0).pl(16.0).pr(10.0).child(text(question).role(Role::Heading).aria_level(2).ty(BODY_SMALL_STRONG).flex1());
    let body = col().px(20.0).pt(4.0).pb(16.0).child(text(consequence()).ty(BODY_SMALL).color(t_.text_secondary));
    let footer = row()
        .justify_end()
        .gap(8.0)
        .px(12.0)
        .pb(12.0)
        .child(button(t("取消", "Cancel")).key("error-view-cancel").on_click(|app: &mut FatalApp| app.asking = false))
        .child(danger_button(t("全部删除并重置", "Delete everything and reset")).key(CONFIRM_KEY).on_click(answer_yes).on_key(|app: &mut FatalApp, press| {
            if matches!(press.key, Key::Enter | Key::Char(' ')) {
                answer_yes(app);
                return true;
            }
            false
        }));
    dialog(420.0, col().child(header).child(body).child(footer), Some(std::rc::Rc::new(|app: &mut FatalApp| app.asking = false))).aria_label(question)
}

/// `data-tauri-drag-region`, as the shell's `drag_region` is: a press moves the window, and a second one on the same spot zooms it.
fn strip_pressed(app: &mut FatalApp, x: f32, y: f32) {
    let now = Instant::now();
    let again = app.last_strip_press.is_some_and(|(at, px, py)| now.duration_since(at) < Duration::from_millis(400) && (px - x).abs() < 4.0 && (py - y).abs() < 4.0);
    app.last_strip_press = if again { None } else { Some((now, x, y)) };
    if again {
        app.zoom_requested = true;
    } else {
        app.drag_requested = true;
    }
}

/// `.fatal-state`, filling the window.
pub fn error_view(app: &mut FatalApp) -> F {
    let t_ = theme();
    let mut view: F = col()
        .key("error-view")
        .w_full()
        .h_full()
        .bg(t_.mica_base)
        .items_center()
        .justify_center()
        .gap(10.0)
        .px(32.0)
        .cursor(CursorKind::Default)
        // `WindowFrame bare`: the page draws no top row of its own, so a strip along the top stands in for one and keeps the
        // window movable.
        .child(div().key("window-drag-strip").absolute().top(0.0).left(0.0).right(0.0).h(44.0).on_pointer(|app: &mut FatalApp, e| {
            if e.phase == Phase::Down {
                strip_pressed(app, e.x, e.y);
            }
        }))
        .child(row().size(48.0, 48.0).center().radius(12.0).bg(t_.critical_bg).child(icon("CircleAlert", 24.0).color(t_.critical)))
        .child(text(t("无法载入 Mewrk", "Unable to load Mewrk")).role(Role::Heading).aria_level(1).ty(SUBTITLE))
        .child(col().max_w(560.0).child(text(app.message.clone()).ty(BODY_SMALL).color(t_.text_secondary).selectable()));
    if app.can_reset {
        let reset = row().items_center().gap(6.0).child(icon("RotateCcw", 15.0).color(Color::WHITE)).child(text(t("重置 Mewrk…", "Reset Mewrk…")).ty(BODY_SMALL_STRONG).color(Color::WHITE));
        view = view
            .child(danger_button("").key("error-view-reset").child(reset).on_click(|app: &mut FatalApp| {
                app.asking = true;
                // `autoFocus`: the user has just asked for this; the answer is one key away.
                focus_request(CONFIRM_KEY);
            }))
            .child(col().max_w(560.0).child(text(consequence()).ty(FINE).color(t_.text_tertiary)));
    }
    if let Some(error) = &app.reset_error {
        view = view.child(col().max_w(560.0).child(text(error.clone()).role(Role::Alert).ty(FINE).color(t_.critical).selectable()));
    }
    if app.asking {
        view = view.child(confirm_reset());
    }
    view.on_key_global(|app: &mut FatalApp, press| {
        if app.asking && press.key == Key::Escape {
            app.asking = false;
            return true;
        }
        false
    })
}
