//! A popover menu of sections, with flyouts: as much of upstream's `PopoverMenu` as the terminal's `+` asks for. These are the
//! shapes the pane builds; `mewrk_ui_common::popover_menu` draws them, with upstream's placement, arrow keys and Enter.

use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::{PopoverMenu, PopoverMenuItem, PopoverMenuSection, Submenu};

pub struct MenuEntry {
    pub id: String,
    pub label: Arc<str>,
    /// Hover text.
    pub title: Option<Arc<str>>,
    pub icon: Option<&'static str>,
    pub disabled: bool,
    /// Draws the label the way `PathText` does.
    pub is_path: bool,
    pub on_select: Option<Handler<App>>,
    /// Rows opening beside this one, for `submenu: "flyout"`.
    pub children: Vec<MenuEntry>,
}

impl MenuEntry {
    pub fn new(id: impl Into<String>, label: impl Into<Arc<str>>) -> Self {
        Self { id: id.into(), label: label.into(), title: None, icon: None, disabled: false, is_path: false, on_select: None, children: Vec::new() }
    }
}

pub struct MenuSection {
    pub id: String,
    pub label: Option<Arc<str>>,
    pub items: Vec<MenuEntry>,
}

fn shared_item(entry: MenuEntry) -> PopoverMenuItem {
    let mut item = PopoverMenuItem::new(entry.id, entry.label.to_string());
    item.title = entry.title.map(|title| title.to_string());
    item.icon = entry.icon.map(Into::into);
    item.disabled = entry.disabled;
    item.label_is_path = entry.is_path;
    item.on_select = entry.on_select;
    item.children = entry.children.into_iter().map(shared_item).collect();
    item
}

fn shared_section(section: MenuSection) -> PopoverMenuSection {
    let mut shared = PopoverMenuSection::new(section.id).items(section.items.into_iter().map(shared_item));
    shared.label = section.label.map(|label| label.to_string());
    shared
}

/// The trigger and, while open, the menu. `key` names this menu among all others, `label` names the menu and is the trigger's
/// hover text. A row with children opens them beside it: a choice made in two steps.
pub fn popover_menu(key: impl std::hash::Hash, label: &str, trigger: E, sections: Vec<MenuSection>, align: Align) -> E {
    let flyout = sections.iter().any(|section| section.items.iter().any(|item| !item.children.is_empty()));
    let mut menu = PopoverMenu::new(key, label, move |_| trigger).menu_label(label).dense().sections(move || sections.into_iter().map(shared_section).collect());
    if flyout {
        menu = menu.submenu(Submenu::Flyout);
    }
    if align == Align::End { menu.align_end().build() } else { menu.build() }
}
