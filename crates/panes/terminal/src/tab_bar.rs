//! `TerminalTabBar`: the terminal pane's title bar, a tab per shell on the shared page strip, and the menu that opens
//! another one. It takes the pane's `header` slot, so the pane draws no title of its own. A double click renames a shell
//! in place. The read-only page, when there is one, leads the strip and stays there.

use std::collections::HashSet;

use mewrk_client::prelude::*;

use crate::menu::{popover_menu, MenuSection};
use mewrk_ui_common::page_tabs::{page_tabs, PageTab, PageTabs};

pub struct TerminalTabDescriptor {
    pub id: String,
    /// Already resolved: the user's name, or the derived one.
    pub label: String,
    /// Hover text; defaults to `label`.
    pub title: Option<String>,
    /// Drawn in place of `label`, at its width: a label that is a path, drawn as one.
    pub content: Option<E>,
    /// The read-only page: a command's output rather than a shell. Pinned to the strip's start, so the caller lists it
    /// first; it takes no name, and closing it ends nothing.
    pub read_only: bool,
}

pub struct TerminalTabBarProps {
    /// Names this bar among all others.
    pub key: String,
    pub tabs: Vec<TerminalTabDescriptor>,
    pub active_id: Option<String>,
    /// Tabs whose shell is being killed; their close control is spent.
    pub closing_ids: HashSet<String>,
    pub on_select: Rc<dyn Fn(&mut App, &str)>,
    pub on_close: Rc<dyn Fn(&mut App, &str)>,
    pub on_rename: Rc<dyn Fn(&mut App, &str, String)>,
    /// Makes the tab order the user's; receives every terminal id in the new order.
    pub on_reorder: Option<Rc<dyn Fn(&mut App, Vec<String>)>>,
    /// The menu `+` opens: where a new terminal can start.
    pub add: Vec<MenuSection>,
}

#[derive(Clone, Default, PartialEq)]
struct Rename {
    id: Option<String>,
    draft: String,
    /// The field has had focus, so losing it means the reader has gone elsewhere.
    focused: bool,
}

pub fn terminal_tab_bar(props: TerminalTabBarProps) -> E {
    let t_ = theme();
    let TerminalTabBarProps { key, tabs, active_id, closing_ids, on_select, on_close, on_rename, on_reorder, add } = props;
    let renaming = use_state(("terminal-rename", key.as_str()), Rename::default);
    let renaming_id = renaming.with(|r| r.id.clone());

    let editor = renaming_id.as_ref().filter(|id| tabs.iter().any(|tab| tab.id == **id)).map(|id| {
        let field_key = ("terminal-rename-field", key.as_str());
        let raw_key = hash_key(("field", field_key));
        let commit: Rc<dyn Fn(&mut App)> = {
            let (renaming, on_rename) = (renaming.clone(), on_rename.clone());
            Rc::new(move |app| {
                let Some(id) = renaming.update(|r| r.id.take()) else { return };
                let draft = renaming.with(|r| r.draft.clone());
                on_rename(app, &id, draft);
            })
        };
        let draft = renaming.with(|r| r.draft.clone());
        let (changed, submit) = (renaming.clone(), commit.clone());
        let input = field(field_key, draft)
            .placeholder(t("重命名终端", "Rename terminal"))
            .ty(CAPTION)
            .on_change(move |_, value| changed.update(|r| r.draft = value))
            .on_submit(move |app, _| submit(app))
            .build()
            .flex1()
            .h(24.0)
            .aria_label(t("重命名终端", "Rename terminal"));
        let (watch, blur) = (renaming.clone(), commit.clone());
        // Blur commits: a leaf that looks, each frame, at whether the field still holds the focus.
        let blur_watch: E = custom(move |app, ui: &mut Ui<'_>, _| {
            let focused = ui.mem.focus == raw_key;
            let was = watch.with(|r| r.focused);
            if focused && !was {
                watch.update(|r| r.focused = true);
            } else if !focused && was {
                blur(app);
            }
        })
        .size(1.0, 1.0)
        .absolute()
        .pass_through();
        let cancel = renaming.clone();
        let editor: E = row()
            .w(128.0)
            .h(24.0)
            .shrink0()
            .on_key_global(move |_, press| {
                if press.key == Key::Escape && cancel.with(|r| r.id.is_some()) {
                    cancel.set(Rename::default());
                    return true;
                }
                false
            })
            .child(input)
            .child(blur_watch);
        (id.clone(), editor)
    });

    let closing_for_tabs = closing_ids;
    let page_tabs_list: Vec<PageTab> = tabs
        .into_iter()
        .map(|tab| PageTab {
            close_disabled: closing_for_tabs.contains(&tab.id),
            icon: tab.read_only.then(|| "Eye".into()),
            pinned: tab.read_only,
            content: tab.content,
            title: tab.title,
            ..PageTab::new(tab.id, tab.label)
        })
        .collect();
    let read_only_ids: HashSet<String> = page_tabs_list.iter().filter(|tab| tab.pinned).map(|tab| tab.id.clone()).collect();
    let label_of: Vec<(String, String, bool)> = page_tabs_list.iter().map(|tab| (tab.id.clone(), tab.label.clone(), tab.pinned)).collect();

    let add_label = t("新建终端", "New terminal");
    let trigger: E = row().center().size(24.0, 24.0).radius(CONTROL_RADIUS).shrink0().hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).child(icon("Plus", 14.0).color(t_.text_secondary));
    let trailing = popover_menu(("terminal-add", key.as_str()), add_label, trigger, add, Align::End);

    // A double click renames a shell in place, starting from what it shows.
    let start_rename: Rc<dyn Fn(&mut App, &str)> = {
        let (renaming, field_owner) = (renaming.clone(), key.clone());
        Rc::new(move |_: &mut App, id: &str| {
            let Some((_, label, read_only)) = label_of.iter().find(|(tab, _, _)| tab == id) else { return };
            // The read-only page is named after the command it shows.
            if *read_only {
                return;
            }
            renaming.set(Rename { id: Some(id.to_owned()), draft: label.clone(), focused: false });
            focus_request(("field", ("terminal-rename-field", field_owner.as_str())));
        })
    };

    page_tabs(PageTabs {
        key: key.clone(),
        tabs: page_tabs_list,
        active_id,
        aria_label: Arc::from(t("终端标签", "Terminal tabs")),
        more_label: Arc::from(t("更多终端", "More terminals")),
        on_select,
        on_close: Some(on_close),
        close_label: Some(Rc::new(move |tab: &PageTab| if read_only_ids.contains(&tab.id) { t("关闭只读终端", "Close the read-only terminal").to_owned() } else { t("关闭终端", "Close terminal").to_owned() })),
        close_on_delete_key: false,
        on_reorder,
        max_tab_width: 180.0,
        on_tab_context_menu: None,
        on_tab_double_click: Some(start_rename),
        editor,
        trailing: Some(trailing),
        trailing_width: 24.0,
    })
}
