//! `SidePane.tsx`: one pseudo-window on the right: a 32 px title bar, a close ×, and a body.

use mewrk_client::prelude::*;

use crate::menu::{MenuSection, PopoverMenu};
use crate::widgets::tool_button;

pub struct SidePane {
    pub id: String,
    pub title: String,
    /// Takes over the title bar: a pane whose own chrome is a title bar, the browser's toolbar, puts it here and the title
    /// is never drawn.
    pub header: Option<E>,
    /// A second row under the title bar, for chrome the pane owns but that does not belong in the window title.
    pub subheader: Option<E>,
    /// Pane-specific controls, placed before the pane's own settings menu.
    pub trailing: Option<E>,
    /// Pane-specific menu rows. Given any, the title bar grows a `⋮` settings menu.
    pub menu_sections: Vec<MenuSection>,
    pub expanded: bool,
    /// Present only for panes that can be shown alone over the workspace.
    pub on_toggle_expand: Option<Handler<App>>,
    pub on_close: Handler<App>,
    /// Any pointer entry into the pane; the parent marks it focused.
    pub on_focus: Option<Handler<App>>,
}

const CONTROL: f32 = 24.0;

/// The body's key, for whatever needs its rectangle.
pub fn side_pane_body_key(id: &str) -> u64 {
    hash_key(("side-pane-body", id))
}

pub fn side_pane(p: SidePane, body: E) -> E {
    let t_ = theme();
    let SidePane { id, title, header, subheader, trailing, menu_sections, expanded, on_toggle_expand, on_close, on_focus } = p;
    let mut section: E = col().key(("side-pane", id.as_str())).role(Role::Region).aria_label(title.as_str()).w_full().h_full().min_w(0.0).min_h(0.0);
    let mut controls: E = row().key(("side-pane-controls", id.as_str())).shrink0().items_center().gap(2.0).child(trailing);
    if !menu_sections.is_empty() {
        let label = tf("{name} 设置", "{name} settings", &[("name", &title)]);
        let trigger_id = id.clone();
        let trigger_label = label.clone();
        controls = controls.child(PopoverMenu::new(("side-pane-menu", id.as_str()), label.clone(), move |_| tool_button(("side-pane-menu-trigger", trigger_id.as_str()), "MoreVertical", trigger_label, CONTROL, 14.0)).menu_label(label).sections(move || menu_sections).align_end().dense().build());
    }
    if let Some(toggle) = on_toggle_expand {
        controls = controls.child(tool_button(("side-pane-expand", id.as_str()), if expanded { "Minimize2" } else { "Maximize2" }, if expanded { t("折叠", "Collapse") } else { t("展开", "Expand") }, CONTROL, 13.0).aria_pressed(expanded).on_click(move |app: &mut App| toggle(app)));
    }
    // A pane about to close keeps the focus where it is until the click lands, so a terminal never flashes a change.
    controls = controls.child(tool_button(("side-pane-close", id.as_str()), "X", t("关闭面板", "Close pane"), CONTROL, 14.0).keep_focus().on_click(move |app: &mut App| on_close(app)));

    let lead: E = match header {
        Some(header) => row().flex1().min_w(0.0).items_center().child(header),
        None => text(title).ty(CAPTION).color(t_.text_secondary).ellipsis().flex1().pl(4.0),
    };
    let head = row().key(("side-pane-header", id.as_str())).h(32.0).shrink0().px(4.0).items_center().justify_between().gap(8.0).child(lead).child(controls);
    section = section.child(head);
    if let Some(subheader) = subheader {
        section = section.child(row().key(("side-pane-subheader", id.as_str())).min_h(32.0).shrink0().px(8.0).items_center().gap(4.0).bg(t_.card_secondary).border_top(t_.divider).child(subheader));
    }
    section = section.child(col().key(side_pane_body_key(&id)).flex1().min_w(0.0).min_h(0.0).clip().child(body));
    if let Some(focus) = on_focus {
        section = section.on_press_capture(move |app: &mut App, _| {
            focus(app);
            false
        });
    }
    section
}
