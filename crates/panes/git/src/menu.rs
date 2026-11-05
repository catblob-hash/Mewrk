//! A popover menu with what the review pane's rows carry: section labels, descriptions, checks, hints. These are the shapes
//! the pane builds; `mewrk_ui_common::popover_menu` draws them, with upstream's placement, arrow keys and Enter.

use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::{PopoverMenu, PopoverMenuItem, PopoverMenuSection};

#[derive(Clone)]
pub struct MenuItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub hint: Option<String>,
    pub icon: Option<&'static str>,
    pub checked: Option<bool>,
    pub disabled: bool,
    pub on_select: Option<Handler<App>>,
}

impl MenuItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), description: None, hint: None, icon: None, checked: None, disabled: false, on_select: None }
    }

    pub fn on(mut self, f: impl Fn(&mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }
}

#[derive(Clone)]
pub struct MenuSection {
    pub id: String,
    pub label: Option<String>,
    pub items: Vec<MenuItem>,
}

fn shared_section(section: MenuSection) -> PopoverMenuSection {
    let items = section.items.into_iter().map(|item| {
        let mut row = PopoverMenuItem::new(item.id, item.label);
        row.description = item.description;
        row.hint = item.hint;
        row.icon = item.icon.map(Into::into);
        row.checked = item.checked;
        row.disabled = item.disabled;
        row.on_select = item.on_select;
        row
    });
    let mut shared = PopoverMenuSection::new(section.id).items(items);
    shared.label = section.label;
    shared
}

/// The trigger and, while open, the menu it opens. The trigger keeps what it was given and gains the click. Sections are
/// asked for only while the menu is open, so a row of triggers costs nothing until one is used. `label` names the menu, and
/// is the trigger's hover text.
pub fn popover_menu(key: impl std::hash::Hash, label: impl Into<String>, trigger: E, sections: impl FnOnce() -> Vec<MenuSection> + 'static, align: Align) -> E {
    let label = label.into();
    let menu = PopoverMenu::new(key, label.clone(), move |_| trigger).menu_label(label).dense().sections(move || sections().into_iter().map(shared_section).collect());
    if align == Align::End { menu.align_end().build() } else { menu.build() }
}
