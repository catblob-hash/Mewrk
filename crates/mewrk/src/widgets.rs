//! The small things `Common.tsx` draws: icon buttons, the armed delete, a dialog window.

use std::hash::Hash;

use mewrk_client::model::RunTarget;
use mewrk_client::prelude::*;

/// `IconButton` at a size of the caller's: a square that is only its icon until hovered.
pub fn tool_button(key: impl Hash, name: impl Into<IconRef>, label: impl Into<Arc<str>>, size: f32, icon_size: f32) -> E {
    let t_ = theme();
    row().key(key).center().size(size, size).radius(6.0).hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).shrink0().tooltip(label).child(icon(name, icon_size).color(t_.text_secondary))
}

/// The same, drawn spent: no hover and a disabled colour. A reason is `disabled_reason`, chained on.
pub fn tool_button_disabled(key: impl Hash, name: impl Into<IconRef>, label: impl Into<Arc<str>>, size: f32, icon_size: f32) -> E {
    let t_ = theme();
    row().key(key).center().size(size, size).radius(6.0).shrink0().tooltip(label).disabled(true).child(icon(name, icon_size).color(t_.text_disabled))
}

/// `machineIcon`.
pub fn machine_icon_name(machine: Option<&RunTarget>) -> &'static str {
    match machine {
        None => "Monitor",
        Some(RunTarget::Wsl { .. }) => "SquareTerminal",
        Some(RunTarget::Ssh { .. }) => "Server",
    }
}

/// `Loader`: a turning arc. Being on screen keeps frames coming.
pub fn spinner(size: f32) -> E {
    let t_ = theme();
    let turning = use_state("shell-spinner-epoch", std::time::Instant::now);
    let angle = turning.with(|started| started.elapsed().as_secs_f32()) * 360.0;
    repaint();
    icon("LoaderCircle", size).color(t_.text_secondary).rotate(angle % 360.0)
}

/// `ConfirmDeleteButton`: the first press arms it, the second deletes, and a press anywhere else or Escape disarms it.
#[allow(clippy::too_many_arguments)]
pub fn confirm_delete_button(key: impl Hash + Clone, label: String, confirm_label: String, title: Option<String>, disabled: bool, size: f32, button: f32, on_delete: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    let armed = use_state(("confirm-delete", hash_key(key.clone())), || false);
    let id = hash_key(("confirm-delete-button", hash_key(key)));
    let is_armed = armed.get() && !disabled;
    let current = if is_armed { confirm_label } else { label };
    let mut b: E = row().key(id).center().h(button).radius(6.0).shrink0().tooltip(title.clone().unwrap_or(current));
    if is_armed {
        // Armed, it gives up the fixed width so the word it now carries has room.
        b = b.px(6.0).bg(t_.critical_bg).child(text(t("确认", "Confirm")).ty(FINE_STRONG).color(t_.critical).nowrap());
    } else {
        let fg = if disabled { t_.text_disabled } else { t_.text_secondary };
        b = b.w(button).child(icon("Trash2", size).color(fg));
    }
    if disabled {
        return b.disabled(true).maybe_disabled_reason(title);
    }
    b = b.hover_bg(t_.critical_bg);
    let press = armed.clone();
    b = b.on_click(move |app: &mut App| {
        if press.get() {
            press.set(false);
            on_delete(app);
        } else {
            press.set(true);
        }
    });
    if is_armed {
        // `onBlur`: there is no focus to lose here, so any press outside the button is the blur.
        let blur = armed.clone();
        b = b.child(
            custom(move |_, ui: &mut Ui<'_>, _| {
                if ui.input.pressed
                    && let Some((x, y)) = ui.input.pointer
                    && !rect_of(id).is_some_and(|r| r.contains(x, y))
                {
                    blur.set(false);
                }
            })
            .size(0.0, 0.0)
            .absolute()
            .pass_through(),
        );
        let escape = armed.clone();
        b = b.on_key_global(move |_, press| {
            if press.key == Key::Escape {
                escape.set(false);
                return true;
            }
            false
        });
    }
    b
}

/// `Dialog`: a one-line title and the way out, a scrolling body, and a footer of actions.
pub struct DialogWindow {
    pub key: u64,
    pub title: String,
    pub description: Option<E>,
    pub width: f32,
    pub dismissible: bool,
    pub body: E,
    pub footer: Option<E>,
    pub on_close: Handler<App>,
}

impl DialogWindow {
    pub fn new(key: impl Hash, title: impl Into<String>, width: f32, body: E, on_close: impl Fn(&mut App) + 'static) -> Self {
        Self { key: hash_key(("dialog", key)), title: title.into(), description: None, width, dismissible: true, body, footer: None, on_close: Rc::new(on_close) }
    }

    pub fn description(mut self, description: E) -> Self {
        self.description = Some(description);
        self
    }

    pub fn footer(mut self, footer: E) -> Self {
        self.footer = Some(footer);
        self
    }

    pub fn dismissible(mut self, on: bool) -> Self {
        self.dismissible = on;
        self
    }

    pub fn build(self) -> E {
        dialog_window(self)
    }
}

pub fn dialog_window(d: DialogWindow) -> E {
    let DialogWindow { key, title, description, width, dismissible, body, footer, on_close } = d;
    let (window_w, window_h) = window_size();
    let name = title.clone();
    let lede_key = hash_key(("dialog-lede", key));
    let described = description.is_some();
    let close = on_close.clone();
    // Drawn even where `dismissible` is false: that flag is about closing by accident, not closing on purpose.
    let header = row().key(("dialog-header", key)).shrink0().items_center().gap(12.0).pt(9.0).pb(8.0).pl(16.0).pr(10.0).child(text(title).role(Role::Heading).aria_level(2).ty(BODY_SMALL_STRONG).ellipsis().flex1()).child(icon_button::<App>("X", t("关闭", "Close")).key(("dialog-close", key)).on_click(move |app: &mut App| close(app)));
    let mut scroller: E = col().key(("dialog-body", key)).min_h(0.0).scroll_y().pt(4.0).pb(16.0).px(20.0);
    if let Some(description) = description {
        scroller = scroller.child(description.key(lede_key).mb(14.0));
    }
    scroller = scroller.child(body);
    let inner = col().max_h((window_h - 48.0).min(760.0)).child(header).child(scroller).child(footer.map(|f| row().key(("dialog-footer", key)).shrink0().items_center().justify_end().gap(8.0).pt(6.0).pb(12.0).px(12.0).child(f)));
    let backdrop = dismissible.then(|| on_close.clone());
    dialog(width.min(window_w - 48.0), inner, backdrop).aria_label(name).when(described, |e| e.aria_describedby(lede_key))
}

/// A look for [`dialog_button`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Standard,
    Accent,
    Subtle,
    Danger,
}

/// `.dialog__footer .button`: 28 high, 12 across and 12 px type, a little smaller than Fluent's default so a footer of long
/// labels still fits its window.
pub fn dialog_button(look: Look, label: impl Into<Arc<str>>) -> E {
    let t_ = theme();
    let (base, fg): (E, Color) = match look {
        Look::Standard => (button(""), t_.text_primary),
        Look::Accent => (accent_button(""), t_.text_on_accent),
        Look::Subtle => (subtle_button(""), t_.text_primary),
        Look::Danger => (danger_button(""), Color::WHITE),
    };
    base.h(28.0).px(12.0).child(text(label.into()).ty(CAPTION).color(fg).nowrap())
}

/// A paragraph of a dialog (`confirm-copy`).
pub fn copy(s: impl Into<Arc<str>>) -> E {
    let t_ = theme();
    text(s).ty(CAPTION).color(t_.text_secondary)
}

/// `safe-boundary-note`.
pub fn boundary_note(name: &'static str, s: impl Into<Arc<str>>) -> E {
    let t_ = theme();
    row().gap(8.0).p(10.0).radius(8.0).bg(t_.card_secondary).items_start().child(icon(name, 15.0).color(t_.text_secondary)).child(text(s).ty(FINE).color(t_.text_secondary).flex1())
}

/// A failure line in a dialog (`role="alert"`).
pub fn error_line(s: impl Into<Arc<str>>) -> E {
    let t_ = theme();
    text(s).role(Role::Alert).ty(FINE).color(t_.critical)
}
