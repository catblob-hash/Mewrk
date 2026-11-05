//! The file manager on screen (upstream's `FilesPane.tsx`, its JSX). See `pane.rs` for what the handlers here do.

use crate::browser::*;
use crate::icons::file_icon;
use crate::pane::*;
use crate::previews::{error_notice, notice};
use mewrk_ui_common::page_tabs::{page_tabs, PageTab, PageTabs};
use crate::viewer::{file_viewer_body, CLEAR_FRAGMENT};
use crate::viewers::*;
use crate::widgets::*;
use mewrk_client::model::RunTarget;
use mewrk_client::prelude::*;
use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuProps, ContextMenuSection};
use mewrk_ui_common::path_text::{path_text_with, PathTextOptions};
use mewrk_ui_common::popover_menu::{PopoverMenu, PopoverMenuItem, PopoverMenuSection};

const NAME_TY: TypeStyle = CAPTION;

/// Splits `text` at `positions` so the matched characters can be emphasised. `positions` index the whole relative path, so
/// a slice of it is matched by shifting the offsets rather than searching again.
pub fn highlight_spans(text: &str, positions: &[u32], offset: u32, ty: TypeStyle, color: Color, match_color: Color) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < chars.len() {
        let hit = |at: usize| positions.contains(&(at as u32 + offset));
        let start = cursor;
        let matched = hit(cursor);
        while cursor < chars.len() && hit(cursor) == matched {
            cursor += 1;
        }
        let piece: String = chars[start..cursor].iter().collect();
        out.push(Span { text: Arc::from(piece), ty: if matched { TypeStyle { weight: Weight::Semibold, ..ty } } else { ty }, color: Some(if matched { match_color } else { color }) });
    }
    out
}

/// The whole pane, for the shell's `side_pane` to frame. Signature: see the crate root.
pub fn files_pane(app: &mut App, props: FilesPaneProps) -> FilesPaneView {
    let id = props.pane_id.clone();
    sync(app, &props);
    let Some(mut st) = take_pane(app, &id) else { return FilesPaneView { header: div(), menu_sections: Vec::new(), body: div() } };
    let view = build(app, &mut st, &props);
    if CLEAR_FRAGMENT.with(|c| c.replace(false)) {
        st.jump_fragment = None;
    }
    if let Some(found) = crate::code::take_jump_result() {
        st.jump_line = None;
        if !found {
            st.lit_line = None;
        }
    }
    put_pane(app, st);
    view
}

fn build(app: &mut App, st: &mut PaneState, props: &FilesPaneProps) -> FilesPaneView {
    let t_ = theme();
    let id = st.id.clone();
    let active = st.active().cloned();
    let active_file = active.as_ref().and_then(|p| p.file.clone());
    let can_fit = st.can_fit_tree;
    // The tree folds away on request; with a file open it also steps aside when the pane is too narrow for the two.
    let tree_visible = active.is_some() && if active_file.is_none() { st.show_tree } else { can_fit && st.show_tree };
    let tree_is_column = tree_visible && can_fit && active_file.is_some();
    let viewer_visible = active_file.is_some();
    let empty_visible = !viewer_visible && (!tree_visible || can_fit);
    let toggle_disabled = active.is_none() || (active_file.is_some() && !can_fit);

    let mut body = row().key(("files-body", id.as_str())).flex1().min_w(0.0).min_h(0.0).clip();
    if tree_visible {
        body = body.child(tree_column(st, tree_is_column));
    }
    if let Some(file) = &active_file {
        body = body.child(viewer_column(app, st, file, !tree_visible, props));
    }
    if empty_visible {
        body = body.child(placeholder(st, tree_visible, viewer_visible));
    }
    if let Some(text) = st.pending_copy.take() {
        // The clipboard is only within reach while painting.
        body = body.child(custom(move |_, ui, _| ui.clipboard.set(&text)).absolute().left(0.0).top(0.0).size(1.0, 1.0).pass_through());
    }
    if let Some(menu) = &st.menu {
        let close_id = id.clone();
        body = body.child(context_menu(("files", id.as_str(), menu.serial), ContextMenuProps::new(menu.anchor, t("文件操作", "File actions"), move |a: &mut App| {
            if let Some(st) = pane(a, &close_id) {
                st.menu = None;
            }
        }, menu.sections.as_ref().clone())));
    }

    let menu_sections = {
        let (a, b) = (id.clone(), id.clone());
        let show = st.show_tree;
        let mut show_item = PopoverMenuItem::new("show-tree", t("显示文件树", "Show file tree")).checkbox(tree_visible).disabled(toggle_disabled).on(move |app| set_show_tree_preference(app, &a, !show));
        if toggle_disabled && active.is_some() {
            show_item = show_item.description(t("面板太窄，放不下文件树", "The pane is too narrow for the tree"));
        }
        vec![
            PopoverMenuSection::new("files-tree").item(show_item),
            PopoverMenuSection::new("files-refresh").item(PopoverMenuItem::new("refresh", t("刷新", "Refresh")).on(move |app| refresh(app, &b))),
        ]
    };
    FilesPaneView { header: header_row(st, tree_visible, toggle_disabled), menu_sections, body: body.bg(t_.layer) }
}

/// `PopoverMenu` as this pane's two `+`-like controls use it: an icon button whose menu lines up with its end.
fn pane_menu(key: impl std::hash::Hash, button_key: impl std::hash::Hash + Clone + 'static, glyph: &'static str, label: &'static str, sections: Vec<PopoverMenuSection>) -> PopoverMenu {
    PopoverMenu::new(key, label, move |_| tool(button_key, glyph, label, 24.0, 14.0)).menu_label(label).align_end().dense().sections(move || sections)
}

fn header_row(st: &PaneState, tree_visible: bool, toggle_disabled: bool) -> E {
    let t_ = theme();
    let id = st.id.clone();
    let toggle_id = id.clone();
    let show = st.show_tree;
    let title_tip = st.active().map(|p| st.page_title(p));
    let title = text(t("文件", "Files")).ty(CAPTION).color(t_.text_secondary).nowrap().pl(4.0).shrink0().map(|e| match title_tip {
        Some(tip) => e.tooltip(tip),
        None => e,
    });
    let toggle = tool(("files-tree-toggle", id.as_str()), if tree_visible { "PanelLeftClose" } else { "PanelLeftOpen" }, if tree_visible { t("收起文件目录", "Collapse file tree") } else { t("展开文件目录", "Expand file tree") }, 24.0, 14.0).ml(-2.0).disabled(toggle_disabled).aria_expanded(tree_visible).on_click(move |app: &mut App| set_show_tree_preference(app, &toggle_id, !show));
    let mut tab_editor = None;
    let tabs = st.pages.iter().map(|page| {
        let label = st.page_label(page);
        let icon_el: E = match (&page.file, &page.directory) {
            (Some(f), _) => file_icon(&f.path, 14.0),
            (None, Some(_)) => icon("FolderOpen", 14.0).color(t_.text_secondary),
            (None, None) => icon("Monitor", 14.0).color(t_.text_secondary),
        };
        if let Some(renaming) = st.renaming.as_ref().filter(|r| r.tab.as_deref() == Some(page.id.as_str())) {
            tab_editor = Some((page.id.clone(), rename_input(&id, renaming).flex1().mx(3.0)));
        }
        let mut tab = PageTab::new(page.id.clone(), label);
        tab.italic = page.preview;
        tab.title = Some(st.page_title(page));
        tab.leading = Some(icon_el);
        tab.hint = page.file.as_ref().and(page.directory.as_ref()).map(|d| browse_name(&d.path));
        tab
    }).collect();
    let new_page_label = t("新建页面", "New page");
    let add: E = if st.props_workspaces.len() > 1 {
        let add_id = id.clone();
        let sections = workspace_sections(st, Rc::new(move |app, location| {
            let page = make_page(app, Some(location));
            open_page(app, &add_id, page);
        }));
        pane_menu(("files-add-page", id.as_str()), ("files-add-page-btn", id.clone()), "Plus", new_page_label, sections).build()
    } else {
        let add_id = id.clone();
        tool(("files-add-page", id.as_str()), "Plus", new_page_label, 24.0, 14.0).on_click(move |app: &mut App| {
            let Some(home) = pane(app, &add_id).map(|st| st.home_location()) else { return };
            let page = make_page(app, Some(home));
            open_page(app, &add_id, page);
        })
    };
    let (a, b, c, d, e) = (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
    let strip = page_tabs(PageTabs {
        key: format!("files/{id}"),
        tabs,
        active_id: st.active_page.clone(),
        aria_label: Arc::from(t("文件页面", "File pages")),
        more_label: Arc::from(t("更多页面", "More pages")),
        on_select: Rc::new(move |app, page| {
            if let Some(st) = pane(app, &a) {
                st.active_page = Some(page.to_owned());
            }
        }),
        on_close: Some(Rc::new(move |app, page| close_pages(app, &b, |p| p.id == page))),
        close_label: Some(Rc::new(|tab: &PageTab| tf("关闭 {name}", "Close {name}", &[("name", &tab.label)]))),
        close_on_delete_key: true,
        on_reorder: Some(Rc::new(move |app, ids| reorder_pages(app, &c, ids))),
        on_tab_double_click: Some(Rc::new(move |app, page| keep_page(app, &d, page))),
        on_tab_context_menu: Some(Rc::new(move |app, page, (x, y)| {
            let Some(sections) = pane(app, &e).map(|st| tab_menu(st, page)) else { return };
            open_menu(app, &e, ContextMenuAnchor::Point { x, y }, sections);
        })),
        max_tab_width: 180.0,
        editor: tab_editor,
        trailing: Some(add),
        trailing_width: 24.0,
    });
    row().flex1().min_w(0.0).items_center().gap(4.0).child(title).child(toggle).child(strip)
}

fn rename_input(id: &str, renaming: &Renaming) -> E {
    let name = renaming.draft.clone();
    // The name without its extension is what a rename usually changes.
    let select_to = match name.rfind('.') {
        Some(dot) if dot > 0 => dot,
        _ => name.len(),
    };
    let (a, b, c, d) = (id.to_owned(), id.to_owned(), id.to_owned(), id.to_owned());
    let input = field(("files-rename", id), name)
        .ty(CAPTION)
        .select_on_focus_range(0, select_to)
        .on_change(move |app: &mut App, value| {
            if let Some(r) = pane(app, &a).and_then(|st| st.renaming.as_mut()) {
                r.draft = value;
            }
        })
        .on_submit(move |app: &mut App, _| commit_rename(app, &b))
        .on_blur(move |app: &mut App, _| commit_rename(app, &c))
        .on_key(move |app: &mut App, k| {
            if k.key == Key::Escape {
                if let Some(st) = pane(app, &d) {
                    st.renaming = None;
                }
                return true;
            }
            false
        })
        .build()
        .h(20.0)
        .flex1()
        .aria_label(t("新名称", "New name"));
    div().flex1().min_w(0.0).child(input).disabled(renaming.busy)
}

fn row_box(key: impl std::hash::Hash, depth: usize, current: bool, revealed: bool) -> E {
    let t_ = theme();
    let mut r = row().key(key).min_h(ROW_H).h(ROW_H).shrink0().pr(8.0).pl(ROOT_INDENT + depth as f32 * INDENT_PER_LEVEL).items_center().gap(4.0).radius(5.0).hover_bg(t_.subtle_hover);
    if current {
        r = r.bg(t_.subtle_pressed);
    }
    if revealed {
        r = r.bg(t_.accent.with_alpha(0.22));
    }
    r
}

fn message_row(key: impl std::hash::Hash, depth: usize, text_value: &str, failed: bool) -> E {
    let t_ = theme();
    row().key(key).min_h(ROW_H).shrink0().pr(8.0).pl(ROOT_INDENT + depth as f32 * INDENT_PER_LEVEL).items_center().child(text(text_value.to_owned()).ty(NAME_TY).color(if failed { t_.critical } else { t_.text_tertiary }).ellipsis().flex1())
}

fn row_menu_button(id: &str, key: &str, sections: Rc<dyn Fn(&PaneState) -> Vec<ContextMenuSection>>) -> E {
    let pane_id = id.to_owned();
    let button_key = ("files-row-menu", id.to_owned(), key.to_owned());
    let anchor = hash_key(button_key.clone());
    tool(button_key, "MoreVertical", t("文件操作", "File actions"), 18.0, 13.0).reveal_on_parent_hover().aria_haspopup(HasPopup::Menu).on_click(move |app: &mut App| {
        let Some(built) = pane(app, &pane_id).map(|st| sections(st)) else { return };
        let Some(rect) = rect_by_id(anchor) else { return };
        open_menu(app, &pane_id, ContextMenuAnchor::Box { rect, align_end: true }, built);
    })
}

fn entry_row(st: &PaneState, entry: &Rc<BrowseEntry>, depth: usize, expanded: bool, machine: &BrowseMachine, row_focus: Option<&str>) -> E {
    let t_ = theme();
    let id = st.id.clone();
    let key = location_key(machine, &entry.path);
    let location = Location::new(machine, entry.path.clone());
    let directory_row = entry.kind == BrowseEntryKind::Directory;
    let current = st.active().and_then(|p| p.file.as_ref()).is_some_and(|f| f.key() == key);
    let revealed = st.revealed_key.as_deref() == Some(key.as_str());
    let editing = st.renaming.as_ref().filter(|r| r.tab.is_none() && r.path == entry.path);
    let mut r = row_box(("files-row", id.as_str(), key.as_str()), depth, current, revealed).focusable().skip_tab(row_focus != Some(key.as_str())).full_text(format_address(machine, &entry.path, &st.context)).role(Role::TreeItem).aria_level(depth + 1).when(directory_row, |e| e.aria_expanded(expanded)).when(current, |e| e.aria_current(AriaCurrent::True));
    if st.keyboard_ring && st.focused_path.as_deref() == Some(key.as_str()) {
        r = r.border(t_.accent);
    }
    r = r.child(if directory_row { icon("ChevronRight", 14.0).color(t_.text_tertiary).rotate(if expanded { 90.0 } else { 0.0 }) } else { file_icon(&entry.path, 14.0) });
    if let Some(renaming) = editing {
        return r.child(rename_input(&id, renaming));
    }
    let name_color = if entry.kind == BrowseEntryKind::Other { t_.text_secondary } else { t_.text_primary };
    r = r.child(text(entry.name.clone()).ty(NAME_TY).color(name_color).ellipsis().flex_grow(0.0).min_w(0.0));
    if entry.link {
        r = r.child(icon("Link2", 11.0).color(t_.text_tertiary).tooltip(t("链接", "link")));
    }
    r = r.child(spacer());
    {
        let (id, location, kind) = (id.clone(), location.clone(), entry.kind);
        r = r.child(row_menu_button(&st.id, &key, Rc::new(move |st| entry_menu(st, &location, kind))));
        let _ = id;
    }
    {
        let (pane_id, location, key_for_click, machine, path) = (id.clone(), location.clone(), key.clone(), machine.clone(), entry.path.clone());
        let kind = entry.kind;
        r = r.on_click(move |app: &mut App| {
            if directory_row {
                click_directory(app, &pane_id, &key_for_click, location.clone());
                return;
            }
            let Some(st) = pane(app, &pane_id) else { return };
            st.focused_path = Some(key_for_click.clone());
            st.keyboard_ring = false;
            if kind == BrowseEntryKind::Other {
                return;
            }
            let Some(page) = st.active_page.clone() else { return };
            show_file(app, &pane_id, &page, Location::new(&machine, path.clone()), false);
        });
    }
    if !directory_row {
        let (pane_id, location) = (id.clone(), location.clone());
        if entry.kind != BrowseEntryKind::Other {
            r = r.on_double_click(move |app: &mut App| {
                let Some(page) = pane(app, &pane_id).and_then(|st| st.active_page.clone()) else { return };
                show_file(app, &pane_id, &page, location.clone(), true);
            });
        }
    } else {
        let (pane_id, location_for_hover) = (id.clone(), location.clone());
        r = r.on_hover_change(move |app: &mut App, on| hover_directory(app, &pane_id, on.then(|| location_for_hover.clone())));
        // A press is sooner than the click it starts.
        let (pane_id, location) = (id.clone(), location.clone());
        r = r.on_press_capture(move |app: &mut App, _| {
            prefetch_directory(app, &pane_id, &location.machine, &location.path);
            false
        });
    }
    {
        let (pane_id, location, kind, key_for_menu) = (id, location, entry.kind, key);
        r = r.on_context_menu(move |app: &mut App, ev| {
            let Some(st) = pane(app, &pane_id) else { return };
            st.focused_path = Some(key_for_menu.clone());
            let sections = entry_menu(st, &location, kind);
            open_menu(app, &pane_id, ContextMenuAnchor::Point { x: ev.x, y: ev.y }, sections);
        });
    }
    r
}

fn machine_row(st: &PaneState, key: &str, machine: &BrowseMachine, name: &str, detail: Option<&str>, row_focus: Option<&str>) -> E {
    let t_ = theme();
    let id = st.id.clone();
    let glyph = match machine {
        None => "Monitor",
        Some(RunTarget::Wsl { .. }) => "HardDrive",
        Some(RunTarget::Ssh { .. }) => "Server",
    };
    let revealed = st.revealed_key.as_deref() == Some(key);
    let (enter_id, enter_machine) = (id.clone(), machine.clone());
    let (menu_id, menu_machine) = (id.clone(), machine.clone());
    row_box(("files-row", id.as_str(), key), 0, false, revealed).focusable().skip_tab(row_focus != Some(key)).full_text(detail.unwrap_or(name).to_owned()).role(Role::TreeItem).aria_level(1)
        .on_click(move |app: &mut App| navigate(app, &enter_id, Some(Location::new(&enter_machine, "~"))))
        .on_context_menu(move |app: &mut App, ev| open_menu(app, &menu_id, ContextMenuAnchor::Point { x: ev.x, y: ev.y }, machine_menu(&menu_id, &menu_machine)))
        .child(icon(glyph, 14.0).color(t_.text_secondary))
        .child(text(name.to_owned()).ty(NAME_TY).color(t_.text_primary).ellipsis().flex_grow(0.0).min_w(0.0))
        .child(detail.map(|d| text(d.to_owned()).ty(FINE).color(t_.text_tertiary).ellipsis().min_w(0.0)))
}

/// What arrow keys, Enter and Backspace do on the tree.
fn on_tree_key(app: &mut App, id: &str, k: &KeyPress) -> bool {
    let Some(st) = pane(app, id) else { return false };
    let keys = st.navigable_keys();
    let searching = st.searching();
    let directory = st.active().and_then(|p| p.directory.clone());
    let focused = st.focused_path.clone().filter(|f| keys.contains(f)).or_else(|| keys.first().cloned());
    let Some(focused) = focused else { return false };
    let index = keys.iter().position(|x| *x == focused).unwrap_or(0);
    let move_to = |st: &mut PaneState, key: Option<&String>| {
        let Some(key) = key else { return };
        st.focused_path = Some(key.clone());
        st.keyboard_ring = true;
        st.request_reveal(key.clone());
        focus_request(("files-row", st.id.as_str(), key.as_str()));
    };
    let entry = st.rows().into_iter().find_map(|r| match r {
        TreeRow::Entry { entry, expanded, depth } if directory.as_ref().is_some_and(|d| location_key(&d.machine, &entry.path) == focused) => Some((entry, expanded, depth)),
        _ => None,
    });
    match k.key {
        Key::Down => move_to(st, keys.get(index + 1)),
        Key::Up => move_to(st, index.checked_sub(1).and_then(|i| keys.get(i))),
        Key::Right => {
            let (Some((entry, expanded, depth)), Some(directory), false) = (entry, directory, searching) else { return false };
            if entry.kind != BrowseEntryKind::Directory {
                return false;
            }
            if !expanded {
                toggle_directory(app, id, &directory.machine, &entry.path);
            } else {
                let rows = st.rows();
                let at = rows.iter().position(|r| matches!(r, TreeRow::Entry { entry: e, .. } if e.path == entry.path));
                if let Some(TreeRow::Entry { entry: next, depth: d, .. }) = at.and_then(|a| rows.get(a + 1))
                    && *d > depth
                {
                    let next_key = location_key(&directory.machine, &next.path);
                    move_to(st, Some(&next_key));
                }
            }
        }
        Key::Left => {
            let (Some((entry, expanded, _)), Some(directory), false) = (entry, directory, searching) else { return false };
            if entry.kind == BrowseEntryKind::Directory && expanded {
                toggle_directory(app, id, &directory.machine, &entry.path);
            } else if let Some(parent) = parent_path(&entry.path).filter(|p| *p != directory.path) {
                let parent_key = location_key(&directory.machine, &parent);
                move_to(st, Some(&parent_key));
            }
        }
        Key::Enter => activate_focused(app, id, &focused, true),
        Key::Backspace => {
            if searching {
                return false;
            }
            go_up(app, id);
        }
        _ => return false,
    }
    true
}

fn activate_focused(app: &mut App, id: &str, key: &str, keep: bool) {
    let Some(st) = pane(app, id) else { return };
    let page = st.active_page.clone();
    let directory = st.active().and_then(|p| p.directory.clone());
    let path = key_path(key).to_owned();
    if let Some(directory) = directory {
        if st.searching() {
            let kind = match &st.search {
                SearchState::Ready { root, matches, .. } => matches.iter().find(|m| join_path(root, &m.path) == path).map(|m| m.kind),
                _ => None,
            };
            if let Some(kind) = kind {
                activate_match(app, id, &Location::new(&directory.machine, path.clone()), kind, keep);
            }
            return;
        }
        let rows = st.rows();
        let kind = rows.iter().find_map(|r| if let TreeRow::Entry { entry, .. } = r { (entry.path == path).then_some(entry.kind) } else { None });
        match kind {
            Some(BrowseEntryKind::Directory) => toggle_directory(app, id, &directory.machine, &path),
            Some(BrowseEntryKind::File) => {
                if let Some(page) = page {
                    show_file(app, id, &page, Location::new(&directory.machine, path), keep);
                }
            }
            _ => {}
        }
    } else if let Some(machine) = machine_from_key(Some(key)) {
        navigate(app, id, Some(Location::new(&machine, "~")));
    }
}

fn activate_match(app: &mut App, id: &str, location: &Location, kind: BrowseEntryKind, keep: bool) {
    if let Some(st) = pane(app, id) {
        st.focused_path = Some(location.key());
    }
    if kind == BrowseEntryKind::Directory {
        reveal_in_tree(app, id, location.clone());
        toggle_directory(app, id, &location.machine, &location.path);
        return;
    }
    if let Some(page) = pane(app, id).and_then(|st| st.active_page.clone()) {
        show_file(app, id, &page, location.clone(), keep);
    }
}

fn search_view(st: &PaneState, directory: &Location) -> E {
    let t_ = theme();
    let id = st.id.clone();
    let mut list = div().key(("files-search", id.as_str())).flex1().min_w(0.0).min_h(0.0).py(5.0).px(4.0).scroll_y().role(Role::ListBox).aria_label(t("匹配的文件", "Matching files")).on_key({
        let id = id.clone();
        move |app: &mut App, k| on_tree_key(app, &id, k)
    });
    match &st.search {
        SearchState::Error(message) => list = list.child(message_row("search-error", 0, message, true)),
        SearchState::Ready { root, matches, truncated } => {
            if matches.is_empty() {
                list = list.child(message_row("search-none", 0, t("没有匹配的文件", "No matching files"), false));
            }
            let active_file = st.active().and_then(|p| p.file.as_ref()).map(Location::key);
            // One Tab stop in the list, as with the tree below.
            let navigable = st.navigable_keys();
            let match_focus = st.focused_path.clone().filter(|f| navigable.contains(f)).or_else(|| navigable.first().cloned());
            for m in matches.iter() {
                let separator = m.path.rfind('/');
                let parent_path_text = separator.filter(|s| *s > 0).map(|s| &m.path[..s]).unwrap_or("");
                let name_offset = separator.map_or(0, |s| m.path[..=s].chars().count()) as u32;
                let location = Location::new(&directory.machine, join_path(root, &m.path));
                let key = location.key();
                let selected = active_file.as_deref() == Some(key.as_str());
                let (click_id, click_loc, kind) = (id.clone(), location.clone(), m.kind);
                let (dbl_id, dbl_loc) = (id.clone(), location.clone());
                let (menu_id, menu_loc) = (id.clone(), location.clone());
                let glyph: E = if m.kind == BrowseEntryKind::Directory { icon("FolderOpen", 14.0).color(t_.text_secondary) } else { file_icon(&m.path, 14.0) };
                let mut r = row_box(("files-row", id.as_str(), key.as_str()), 0, selected, false).focusable().skip_tab(match_focus.as_deref() != Some(key.as_str())).full_text(m.path.clone()).role(Role::ListBoxOption).aria_selected(selected)
                    .on_click(move |app: &mut App| activate_match(app, &click_id, &click_loc, kind, false))
                    .on_double_click(move |app: &mut App| activate_match(app, &dbl_id, &dbl_loc, kind, true))
                    .on_context_menu(move |app: &mut App, ev| {
                        let Some(sections) = pane(app, &menu_id).map(|st| entry_menu(st, &menu_loc, kind)) else { return };
                        open_menu(app, &menu_id, ContextMenuAnchor::Point { x: ev.x, y: ev.y }, sections);
                    })
                    .child(glyph)
                    .child(spans(highlight_spans(&m.name, &m.positions, name_offset, NAME_TY, t_.text_secondary, t_.text_primary)).key(("files-match", key.as_str())).ellipsis().flex_grow(0.0).min_w(0.0));
                if !parent_path_text.is_empty() {
                    r = r.child(spans(highlight_spans(parent_path_text, &m.positions, 0, FINE, t_.text_tertiary, t_.text_primary)).key(("files-match-dir", key.as_str())).ellipsis().flex_grow(0.0).min_w(0.0));
                }
                let (menu_location, kind_for_menu) = (location.clone(), m.kind);
                r = r.child(spacer()).child(row_menu_button(&id, &key, Rc::new(move |st| entry_menu(st, &menu_location, kind_for_menu))));
                list = list.child(r);
            }
            if *truncated {
                list = list.child(message_row("search-truncated", 0, t("结果过多，只显示了一部分", "Too many results; only some are shown"), false));
            }
        }
        _ => {}
    }
    list
}

fn tree_view(st: &mut PaneState) -> E {
    let id = st.id.clone();
    let directory = st.active().and_then(|p| p.directory.clone());
    if let Some(d) = &directory
        && st.searching()
    {
        return search_view(st, d);
    }
    let root_state = directory.as_ref().and_then(|d| st.listings.get(&d.key())).cloned();
    if let Some(DirectoryState::Error(message)) = &root_state {
        return error_notice(message.clone()).flex1();
    }
    let machine = directory.as_ref().map_or(None, |d| d.machine.clone());
    let rows = st.rows();
    let scroller = ("files-tree", id.clone());
    if st.has_pending_reveal() {
        let keys: Vec<String> = rows.iter().map(|r| match r {
            TreeRow::Entry { entry, .. } => location_key(&machine, &entry.path),
            TreeRow::Machine { key, .. } => key.clone(),
            TreeRow::Message { id, .. } => id.clone(),
        }).collect();
        if let Some(index) = take_pending_reveal(st, &keys) {
            scroll_request(scroller.clone(), ScrollRequest::Reveal { y: 5.0 + index as f32 * ROW_H, h: ROW_H });
        }
    }
    let creating = st.creating.clone().filter(|c| directory.as_ref().is_some_and(|d| same_machine(&c.machine, &d.machine) && c.parent == d.path));
    let extra = usize::from(creating.is_some());
    let range = visible_rows(scroller.clone(), rows.len() + extra, ROW_H, 12);
    let tree_id = id.clone();
    let menu_id = id.clone();
    let mut list = div().key(scroller).flex1().min_w(0.0).min_h(0.0).py(5.0).px(4.0).scroll_y().role(Role::Tree).aria_label(if directory.is_some() { t("文件", "Files") } else { t("机器", "Machines") }).on_key(move |app: &mut App, k| on_tree_key(app, &tree_id, k)).on_context_menu(move |app: &mut App, ev| {
        let Some(sections) = pane(app, &menu_id).and_then(|st| st.active().and_then(|p| p.directory.clone()).map(|d| directory_menu(st, &d))) else { return };
        open_menu(app, &menu_id, ContextMenuAnchor::Point { x: ev.x, y: ev.y }, sections);
    });
    if let Some(c) = creating {
        list = list.child(creating_row(&id, &c));
    }
    list = list.child(div().h(range.start.saturating_sub(extra) as f32 * ROW_H).shrink0());
    // The tree is one Tab stop, the row last moved to, and the arrow keys walk it (`on_tree_key`).
    let navigable = st.navigable_keys();
    let row_focus = st.focused_path.clone().filter(|f| navigable.contains(f)).or_else(|| navigable.first().cloned());
    for i in range.start.saturating_sub(extra)..range.end.saturating_sub(extra).min(rows.len()) {
        list = list.child(match &rows[i] {
            TreeRow::Message { id: message_id, depth, text, failed } => message_row(("files-msg", id.as_str(), message_id.as_str()), *depth, text, *failed),
            TreeRow::Machine { key, machine, name, detail } => machine_row(st, key, machine, name, detail.as_deref(), row_focus.as_deref()),
            TreeRow::Entry { entry, depth, expanded } => entry_row(st, entry, *depth, *expanded, &machine, row_focus.as_deref()),
        });
    }
    list = list.child(div().h(rows.len().saturating_sub(range.end.saturating_sub(extra)) as f32 * ROW_H).shrink0());
    if let Some(DirectoryState::Ready(listing)) = &root_state
        && listing.truncated
    {
        list = list.child(message_row("files-truncated", 0, t("目录里的项目过多，只列出了一部分", "This folder holds too many items; only some are listed"), false));
    }
    list
}

fn creating_row(id: &str, c: &Creating) -> E {
    let t_ = theme();
    let (a, b, d, e) = (id.to_owned(), id.to_owned(), id.to_owned(), id.to_owned());
    let input = field(("files-create", id), c.draft.clone())
        .ty(CAPTION)
        .placeholder(t("新建目录", "New folder"))
        .on_change(move |app: &mut App, value| {
            if let Some(c) = pane(app, &a).and_then(|st| st.creating.as_mut()) {
                c.draft = value;
            }
        })
        .on_submit(move |app: &mut App, _| commit_create(app, &b))
        .on_blur(move |app: &mut App, value| {
            if value.trim().is_empty() {
                if let Some(st) = pane(app, &d) {
                    st.creating = None;
                }
            } else {
                commit_create(app, &d);
            }
        })
        .on_key(move |app: &mut App, k| {
            if k.key == Key::Escape {
                if let Some(st) = pane(app, &e) {
                    st.creating = None;
                }
                return true;
            }
            false
        })
        .build()
        .h(20.0)
        .flex1()
        .aria_label(t("新目录的名称", "Name of the new folder"));
    row().min_h(ROW_H).shrink0().pl(ROOT_INDENT).pr(8.0).items_center().gap(4.0).child(icon("FolderPlus", 14.0).color(t_.text_secondary)).child(div().flex1().min_w(0.0).disabled(c.busy).child(input))
}

/// What the last action said, and a delete asking first: at the foot of the tree, or of whatever the page shows while its
/// tree is put away.
fn notices(st: &PaneState) -> E {
    let t_ = theme();
    let id = st.id.clone();
    let mut out = col().shrink0().min_w(0.0);
    if let Some(message) = &st.action_error {
        let dismiss = id.clone();
        out = out.child(row().role(Role::Alert).items_start().gap(6.0).py(6.0).pl(10.0).pr(4.0).border_top(t_.divider).child(text(message.clone()).ty(FINE).color(t_.critical).flex1()).child(tool(("files-error-dismiss", id.as_str()), "X", t("关闭", "Dismiss"), 18.0, 12.0).on_click(move |app: &mut App| {
            if let Some(st) = pane(app, &dismiss) {
                st.action_error = None;
            }
        })));
    }
    if let Some(confirm) = &st.confirm_delete {
        let name = browse_name(&confirm.path);
        let question = if confirm.machine.is_some() {
            tf("永久删除“{name}”？那台机器没有回收站，删除后无法恢复。", "Delete “{name}” for good? That machine has no Trash to restore it from.", &[("name", &name)])
        } else {
            tf("把“{name}”移到废纸篓？", "Move “{name}” to the Trash?", &[("name", &name)])
        };
        let (cancel, go) = (id.clone(), id.clone());
        let delete_label = if confirm.machine.is_some() { t("永久删除", "Delete") } else { t("移到废纸篓", "Move to Trash") };
        out = out.child(col().role(Role::AlertDialog).aria_label(t("确认删除", "Confirm delete")).gap(8.0).px(10.0).py(8.0).border_top(t_.divider).bg(t_.card_secondary).child(text(question).ty(FINE).color(t_.text_primary)).child(row().justify_end().gap(6.0).child(button(t("取消", "Cancel")).key(("files-delete-cancel", id.as_str())).h(24.0).disabled(confirm.busy).on_click(move |app: &mut App| {
            if let Some(st) = pane(app, &cancel) {
                st.confirm_delete = None;
            }
        })).child(danger_button(delete_label).key(("files-delete-confirm", id.as_str())).h(24.0).disabled(confirm.busy).on_click(move |app: &mut App| commit_delete(app, &go)))));
    }
    out
}

fn tree_column(st: &mut PaneState, aside: bool) -> E {
    let t_ = theme();
    let id = st.id.clone();
    let directory = st.active().and_then(|p| p.directory.clone());
    let address_text = directory.as_ref().map_or(String::new(), |d| format_address(&d.machine, &d.path, &st.context));
    let up_id = id.clone();
    let up = tool(("files-up", id.as_str()), "ChevronLeft", t("上一级", "Up one level"), 22.0, 14.0).disabled(directory.is_none()).on_click(move |app: &mut App| go_up(app, &up_id));
    let mut address = row().h(32.0).shrink0().pl(4.0).pr(6.0).items_center().gap(2.0).border_bottom(t_.divider).child(up);
    let mut address_error = None;
    match &st.address {
        Some(edit) => {
            let (a, b, c, d) = (id.clone(), id.clone(), id.clone(), id.clone());
            let invalid = edit.error.is_some();
            let input = field(("files-address", id.as_str()), edit.text.clone())
                .ty(CAPTION)
                .select_on_focus(true)
                .placeholder(t("路径，或 机器:路径", "A path, or machine:path"))
                .on_change(move |app: &mut App, value| {
                    // `readOnly` while the machine is being asked: what was typed is what it is being asked about.
                    if let Some(st) = pane(app, &a)
                        && !st.address.as_ref().is_some_and(|x| x.busy)
                    {
                        st.address = Some(AddressEdit { text: value, error: None, busy: false });
                    }
                })
                .on_submit(move |app: &mut App, _| submit_address(app, &b))
                .on_blur(move |app: &mut App, _| {
                    if let Some(st) = pane(app, &c)
                        && !st.address.as_ref().is_some_and(|x| x.busy)
                    {
                        st.address = None;
                    }
                })
                .on_key(move |app: &mut App, k| {
                    if k.key == Key::Escape {
                        if let Some(st) = pane(app, &d) {
                            st.address = None;
                        }
                        return true;
                    }
                    false
                })
                .build()
                .h(24.0)
                .flex1()
                .aria_label(t("位置", "Location"))
                .aria_invalid(invalid);
            let mut holder = div().flex1().min_w(0.0).child(input);
            if let Some(error) = edit.error.as_deref().filter(|_| invalid) {
                holder = holder.border(t_.critical).radius(4.0).tooltip(error.to_owned());
            }
            address = address.child(holder);
            if edit.busy {
                address = address.child(spinner(13.0));
            }
            address_error = edit.error.clone();
        }
        None => {
            let start_id = id.clone();
            let shown = address_text.clone();
            let tip = if directory.is_some() { address_text.clone() } else { t("所有机器", "All machines").to_owned() };
            let label = if directory.is_some() { path_text_with(("files-address-path", id.as_str()), address_text.clone(), CAPTION, t_.text_secondary, PathTextOptions { title: Some(String::new()), ..Default::default() }).flex1() } else { text(t("所有机器", "All machines")).ty(CAPTION).color(t_.text_tertiary).ellipsis().flex1() };
            address = address.child(row().key(("files-address-display", id.as_str())).flex1().min_w(0.0).h(24.0).px(6.0).radius(5.0).items_center().hover_bg(t_.subtle_hover).cursor(CursorKind::Text).aria_label(tf("位置：{address}，点按编辑", "Location: {address}; click to edit", &[("address", &tip)])).full_text(tip).on_click(move |app: &mut App| {
                if let Some(st) = pane(app, &start_id) {
                    st.address = Some(AddressEdit { text: shown.clone(), error: None, busy: false });
                }
                focus_request(("field", ("files-address", start_id.as_str())));
            }).child(label));
        }
    }
    let tree = tree_view(st);
    let filter_set = id.clone();
    let filter_esc = id.clone();
    let filter_field = field(("files-filter", id.as_str()), st.filter.clone()).bare().ty(CAPTION).placeholder(t("筛选文件…", "Filter files…")).on_change(move |app: &mut App, value| set_filter(app, &filter_set, value)).on_key(move |app: &mut App, k| {
        if k.key == Key::Escape && pane(app, &filter_esc).is_some_and(|st| !st.filter.is_empty()) {
            set_filter(app, &filter_esc, String::new());
            return true;
        }
        false
    }).build().h(18.0).flex1().aria_label(t("筛选文件", "Filter files"));
    let filter_glyph: E = if matches!(st.search, SearchState::Loading) { spinner(13.0) } else { icon("Search", 13.0).color(t_.text_tertiary) };
    let mut filter_row = row().flex1().min_w(0.0).h_full().pl(8.0).pr(4.0).items_center().gap(6.0).child(row().size(16.0, 16.0).center().shrink0().child(filter_glyph)).child(filter_field).disabled(directory.is_none());
    if !st.filter.is_empty() {
        let clear = id.clone();
        filter_row = filter_row.child(tool(("files-filter-clear", id.as_str()), "X", t("清除筛选", "Clear filter"), 18.0, 12.0).on_click(move |app: &mut App| {
            set_filter(app, &clear, String::new());
            focus_request(("field", ("files-filter", clear.as_str())));
        }));
    }
    let nav_id = id.clone();
    let workspaces = workspace_sections(st, Rc::new(move |app, location| navigate(app, &nav_id, Some(location))));
    let footer = row().h(32.0).shrink0().pr(4.0).items_center().gap(2.0).border_top(t_.divider).child(filter_row).child(pane_menu(("files-workspaces", id.as_str()), ("files-workspaces-btn", id.clone()), "MoreHorizontal", t("工作区", "Workspaces"), workspaces).above().build());
    let mut column = col().min_h(0.0).min_w(0.0).clip();
    column = if aside { column.w(TREE_COLUMN_WIDTH).shrink0().border_right(t_.divider) } else { column.flex1() };
    column.child(address).child(address_error.map(|e| text(e).ty(FINE).color(t_.critical).px(10.0).py(4.0).shrink0().role(Role::Alert))).child(tree).child(notices(st)).child(footer)
}

fn viewer_column(app: &mut App, st: &PaneState, file: &Location, show_notices: bool, props: &FilesPaneProps) -> E {
    let t_ = theme();
    let id = st.id.clone();
    let key = file.key();
    let root = st.document_root(file);
    let relative = relative_path(&root, &file.path).unwrap_or_else(|| file.path.clone());
    let viewer = st.viewers.get(&key).cloned();
    let source_on = st.source_keys.contains(&key);
    let ready_text = matches!(viewer, Some(ViewerState::Ready { binary: false, .. }));
    let can_read_source = has_source_form(&file.path) && ready_text;
    let directory_part = relative[..relative.rfind('/').map_or(0, |i| i + 1)].to_owned();
    let tip = format_address(&file.machine, &file.path, &st.context);
    let path_label = spans(vec![span(directory_part, FINE).color_of(t_.text_tertiary), span(browse_name(&file.path), FINE_STRONG).color_of(t_.text_primary)]).key(("files-viewer-path", id.as_str())).ellipsis().flex1().full_text(tip);
    let mut bar = row().h(32.0).shrink0().pl(8.0).pr(4.0).items_center().gap(2.0).border_bottom(t_.divider).child(path_label);
    if can_read_source {
        let (toggle_id, toggle_key) = (id.clone(), key.clone());
        bar = bar.child(tool_pressed(tool(("files-source", id.as_str()), if source_on { "Eye" } else { "Code2" }, if source_on { t("显示渲染结果", "Show rendered") } else { t("显示源码", "Show source") }, 22.0, 13.0), source_on).on_click(move |app: &mut App| {
            if let Some(st) = pane(app, &toggle_id) {
                toggle_source(st, &toggle_key);
            }
        }));
    }
    let (reload_id, reload_file_loc) = (id.clone(), file.clone());
    let (reveal_id, reveal_loc) = (id.clone(), file.clone());
    let copy_id = id.clone();
    let copy_text_value = if let Some(ViewerState::Ready { content, binary: false, .. }) = &viewer { Some(content.clone()) } else { None };
    let copy_enabled = copy_text_value.is_some();
    let close_id = id.clone();
    bar = bar
        .child(tool(("files-reload", id.as_str()), "RotateCw", t("重新读取文件", "Reload file"), 22.0, 13.0).on_click(move |app: &mut App| reload_file(app, &reload_id, &reload_file_loc)))
        .child(tool(("files-reveal", id.as_str()), "FolderTree", t("在文件树中显示", "Reveal in file tree"), 22.0, 13.0).on_click(move |app: &mut App| reveal_in_tree(app, &reveal_id, reveal_loc.clone())))
        .child(tool(("files-copy", id.as_str()), "Copy", t("复制文件内容", "Copy file contents"), 22.0, 13.0).disabled(!copy_enabled).on_click(move |app: &mut App| {
            if let Some(content) = &copy_text_value {
                copy_text(app, &copy_id, content.to_string());
            }
        }))
        .child(tool(("files-close-file", id.as_str()), "X", t("关闭文件", "Close file"), 22.0, 13.0).on_click(move |app: &mut App| {
            let Some(st) = pane(app, &close_id) else { return };
            if let Some(page) = st.active_page.clone() {
                st.update_page(&page, |p| p.file = None);
            }
        }));
    let truncated = matches!(viewer, Some(ViewerState::Ready { truncated: true, .. })) && needs_text(&file.path);
    let body = file_viewer_body(app, st, file, props.on_mention.clone());
    col().key(("files-viewer", id.as_str())).flex1().min_w(0.0).min_h(0.0).clip().child(bar).child(truncated.then(|| notice(t("文件过大，只显示了开头部分", "This file is too large; only its beginning is shown")))).child(body).child(show_notices.then(|| notices(st)))
}

fn placeholder(st: &PaneState, tree_visible: bool, viewer_visible: bool) -> E {
    let t_ = theme();
    let has_page = st.active().is_some();
    let mut out = col().flex1().min_w(0.0).min_h(0.0).center().gap(4.0).p(24.0)
        .child(row().size(40.0, 40.0).center().border(t_.divider).radius(11.0).child(icon("FolderOpen", 20.0).color(t_.text_tertiary)))
        .child(text(if has_page { t("未选择文件", "No file selected") } else { t("没有打开的页面", "No pages open") }).ty(CAPTION_STRONG).color(t_.text_primary).mt(8.0))
        .child(text(if !has_page { t("用标签栏的 + 打开一个工作区。", "Open a workspace with the + in the tab bar.") } else if tree_visible { t("在文件树中选择一个文件。", "Pick a file in the tree.") } else { t("打开文件树来浏览。", "Show the file tree to browse.") }).ty(FINE).color(t_.text_secondary).text_center().max_w(260.0));
    if !tree_visible && !viewer_visible && (st.action_error.is_some() || st.confirm_delete.is_some()) {
        out = out.child(div().mt(12.0).w_full().child(notices(st)));
    }
    out
}

trait SpanColor {
    fn color_of(self, c: Color) -> Span;
}

impl SpanColor for Span {
    fn color_of(mut self, c: Color) -> Span {
        self.color = Some(c);
        self
    }
}
