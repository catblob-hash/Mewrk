//! `ShortcutSettings/index.tsx`: view and customize application shortcuts. Documents store only overrides.

use std::collections::BTreeMap;
use std::time::Duration;

use mewrk_client::model::ShortcutPreference;
use mewrk_client::prelude::*;
use mewrk_client::shortcuts::*;

use crate::logic::store;
use crate::widgets::*;

type Shortcuts = BTreeMap<String, ShortcutPreference>;

#[derive(Default)]
pub struct ShortcutsState {
    pub search_open: bool,
    pub query: String,
    /// `None` is all groups.
    pub group: Option<ShortcutGroup>,
    pub recording: Option<&'static str>,
    pub pending: Vec<String>,
    pub conflict: Option<(&'static str, &'static str)>,
    pub conflict_timer: Option<mewrk_client::timers::TimerId>,
}

fn page(app: &mut App) -> &mut ShortcutsState {
    app.local::<ShortcutsState>()
}

pub fn unmount(app: &mut App) {
    if let Some(timer) = page(app).conflict_timer.take() {
        app.clear_timer(timer);
    }
    *page(app) = ShortcutsState::default();
}

pub fn update_preference(shortcuts: &Shortcuts, command: &ShortcutCommand, preference: ShortcutPreference) -> Shortcuts {
    let mut next = shortcuts.clone();
    let fallback = default_shortcut_binding(command);
    if preference.enabled == command.default_enabled && preference.binding == fallback {
        next.remove(command.id);
    } else {
        next.insert(command.id.to_owned(), preference);
    }
    next
}

fn show_conflict(app: &mut App, command: &'static str, conflicting: &'static str) {
    if let Some(timer) = page(app).conflict_timer.take() {
        app.clear_timer(timer);
    }
    page(app).conflict = Some((command, conflicting));
    let timer = app.set_timer(Duration::from_secs(2), |app| {
        let p = page(app);
        p.conflict = None;
        p.conflict_timer = None;
    });
    page(app).conflict_timer = Some(timer);
}

fn cancel_recording(app: &mut App) {
    let p = page(app);
    p.recording = None;
    p.pending.clear();
}

fn set_shortcuts(app: &mut App, next: Shortcuts) {
    store::change(app, move |doc| doc.global_settings.shortcuts = next);
}

/// What a key does while recording. Answers whether it was taken, which is every key.
pub fn handle_recording_key(app: &mut App, command: &'static ShortcutCommand, press: &KeyPress) -> bool {
    let input = KeyInput::from_key_press(*press);
    if is_ime_key_event(&input) {
        return true;
    }
    if press.key == Key::Escape && !press.cmd && !press.ctrl && !press.alt && !press.shift {
        cancel_recording(app);
        return true;
    }
    let Some(candidate) = binding_from_event(&input) else {
        let mut modifiers = Vec::new();
        for (held, token) in [(input.ctrl, "Control"), (input.alt, "Alt"), (input.shift, "Shift"), (input.meta, "Meta")] {
            if held {
                modifiers.push(token.to_owned());
            }
        }
        page(app).pending = order_binding(&modifiers);
        return true;
    };
    let ordered = order_binding(&candidate);
    page(app).pending = ordered.clone();
    if !is_valid_binding(&ordered) {
        return true;
    }
    let shortcuts = app.doc.global_settings.shortcuts.clone();
    if let Some(conflict) = find_conflict(&shortcuts, command.id, &ordered) {
        show_conflict(app, command.id, conflict.id);
        return true;
    }
    set_shortcuts(app, update_preference(&shortcuts, command, ShortcutPreference { binding: ordered, enabled: true }));
    page(app).conflict = None;
    cancel_recording(app);
    true
}

fn handle_enabled_change(app: &mut App, command: &'static ShortcutCommand, enabled: bool) {
    let shortcuts = app.doc.global_settings.shortcuts.clone();
    let resolved = resolve_shortcut(&shortcuts, command);
    if enabled
        && let Some(conflict) = find_conflict(&shortcuts, command.id, &resolved.binding) {
            show_conflict(app, command.id, conflict.id);
            return;
        }
    set_shortcuts(app, update_preference(&shortcuts, command, ShortcutPreference { enabled, ..resolved }));
}

/// Enables or disables what's listed. The complete proposed state is checked before anything is written, so a bulk update
/// is never partial.
pub fn set_visible_enabled(app: &mut App, visible: &[&'static ShortcutCommand], enabled: bool) {
    let shortcuts = app.doc.global_settings.shortcuts.clone();
    let mut proposed = shortcuts;
    for command in visible {
        let resolved = resolve_shortcut(&proposed, command);
        if resolved.binding.is_empty() {
            continue;
        }
        proposed = update_preference(&proposed, command, ShortcutPreference { enabled, ..resolved });
    }
    if enabled {
        let enabled_commands: Vec<&'static ShortcutCommand> = SHORTCUT_COMMANDS.iter().filter(|c| resolve_shortcut(&proposed, c).enabled).collect();
        for (i, left) in enabled_commands.iter().enumerate() {
            for right in &enabled_commands[i + 1..] {
                let (left_visible, right_visible) = (visible.iter().any(|c| c.id == left.id), visible.iter().any(|c| c.id == right.id));
                if !left_visible && !right_visible {
                    continue;
                }
                if !bindings_conflict(&resolve_shortcut(&proposed, left).binding, &resolve_shortcut(&proposed, right).binding) {
                    continue;
                }
                let (target, other) = if left_visible { (left.id, right.id) } else { (right.id, left.id) };
                show_conflict(app, target, other);
                return;
            }
        }
    }
    set_shortcuts(app, proposed);
}

/// `onBlur={cancelRecording}`: paints nothing, and notices when focus has left the recorder.
fn recording_blur_watch(id: &'static str) -> E {
    let node = hash_key(("shortcut-recording", id));
    custom::<App>(move |app, ui, _| {
        if ui.mem.focus != node {
            cancel_recording(app);
        }
    })
    .absolute()
    .size(0.0, 0.0)
    .pass_through()
}

fn caps_row(caps: Vec<String>) -> E {
    row().gap(3.0).items_center().children(caps.into_iter().map(keycap))
}

/// Which commands the search and the group filter leave.
pub fn visible_commands(shortcuts: &Shortcuts, query: &str, group: Option<ShortcutGroup>) -> (Vec<&'static ShortcutCommand>, Vec<&'static ShortcutCommand>) {
    let needle = query.trim().to_lowercase();
    let matches: Vec<&'static ShortcutCommand> = SHORTCUT_COMMANDS
        .iter()
        .filter(|command| {
            if needle.is_empty() {
                return true;
            }
            let caps = shortcut_key_caps(&resolve_shortcut(shortcuts, command).binding, None);
            let searchable = format!("{} {}", caps.join(" "), caps.join("+")).to_lowercase();
            command.label().to_lowercase().contains(&needle) || searchable.contains(&needle)
        })
        .collect();
    let visible = matches.iter().copied().filter(|c| group.is_none_or(|g| c.group == g)).collect();
    (matches, visible)
}

/// The page.
pub fn shortcut_settings(app: &mut App) -> E {
    let t_ = theme();
    let shortcuts = app.doc.global_settings.shortcuts.clone();
    let p = page(app);
    let (query, group, search_open) = (p.query.clone(), p.group, p.search_open);
    let (recording, pending, conflict) = (p.recording, p.pending.clone(), p.conflict);
    let (matches, visible) = visible_commands(&shortcuts, &query, group);
    let count_of = |g: ShortcutGroup| matches.iter().filter(|c| c.group == g).count();
    // A group the search emptied falls back to all of them.
    if let Some(g) = group
        && count_of(g) == 0 {
            page(app).group = None;
        }

    // At `max-width: 760px` the actions take a line of their own under the title, with the search filling it.
    let narrow = window_size().0 <= 760.0;
    let search = if search_open || !query.is_empty() {
        row()
            .min_h(30.0)
            .when(narrow, |e| e.flex1().min_w(0.0))
            .when(!narrow, |e| e.w(190.0))
            .items_center()
            .gap(5.0)
            .pl(8.0)
            .radius(CONTROL_RADIUS)
            .bg(t_.control_fill)
            .border(t_.control_stroke)
            .child(icon("Search", 13.0).color(t_.text_tertiary))
            .child(
                field::<App>("shortcut-search", query.clone())
                    .bare()
                    .placeholder(t("搜索快捷键", "Search shortcuts"))
                    .on_change(|app, v| page(app).query = v)
                    .build()
                    .flex1()
                    .h(18.0)
                    .aria_label(t("搜索快捷键", "Search shortcuts"))
                    .on_key(|app: &mut App, k: &KeyPress| {
                        if k.key != Key::Escape {
                            return false;
                        }
                        let p = page(app);
                        p.query.clear();
                        p.search_open = false;
                        true
                    }),
            )
            .child(icon_button::<App>("X", t("收起搜索", "Collapse search")).key("shortcut-search-close").size(24.0, 24.0).on_click(|app: &mut App| {
                let p = page(app);
                p.query.clear();
                p.search_open = false;
            }))
    } else {
        icon_button::<App>("Search", t("搜索快捷键", "Search shortcuts")).key("shortcut-search-open").size(30.0, 30.0).on_click(|app: &mut App| {
            page(app).search_open = true;
            focus_request(("field", "shortcut-search"));
        })
    };

    let mut group_options: Vec<Arc<str>> = vec![Arc::from(tf("全部（{count}）", "All ({count})", &[("count", &matches.len())]))];
    let mut group_values: Vec<Option<ShortcutGroup>> = vec![None];
    for g in SHORTCUT_GROUP_ORDER {
        let count = count_of(g);
        if count > 0 {
            group_options.push(Arc::from(tf("{name}（{count}）", "{name} ({count})", &[("name", &g.label()), ("count", &count)])));
            group_values.push(Some(g));
        }
    }
    let selected_group = group_values.iter().position(|v| *v == group).unwrap_or(0);
    let group_select = select::<App>("shortcut-group", group_options, selected_group, move |app, i| page(app).group = group_values[i]).w(154.0).aria_label(t("筛选快捷键分组", "Filter shortcut groups"));

    let menu_open = use_state("shortcut-menu-open", || false);
    let is_menu_open = menu_open.get();
    let toggle = menu_open.clone();
    let mut more = icon_button::<App>("MoreHorizontal", t("更多快捷键操作", "More shortcut actions")).key("shortcut-more").size(30.0, 30.0).aria_haspopup(HasPopup::Menu).aria_expanded(is_menu_open).on_click(move |_: &mut App| toggle.update(|v| *v = !*v));
    if is_menu_open {
        let closer = menu_open.clone();
        let close: Handler<App> = Rc::new(move |_| closer.set(false));
        let (enable_list, disable_list) = (visible.clone(), visible.clone());
        let rows = vec![
            MenuItem::new(t("全部启用", "Enable all"), move |app| set_visible_enabled(app, &enable_list, true)).icon("Check"),
            MenuItem::new(t("全部停用", "Disable all"), move |app| set_visible_enabled(app, &disable_list, false)).icon("X"),
            MenuItem::new(t("重置为默认值", "Reset to defaults"), |app| {
                cancel_recording(app);
                set_shortcuts(app, Shortcuts::new());
            })
            .icon("RotateCcw")
            .danger()
            .separated(),
        ];
        let dismiss = close.clone();
        more = more.child(menu_panel(rows, close).key("shortcut-menu").popover("shortcut-more", Side::Below, Align::End, 4.0, move |a| dismiss(a)));
    }
    let actions = row().gap(6.0).items_center().shrink0().child(search).child(group_select).child(more);

    let hovered_row = use_state("shortcut-hovered-row", || None::<&'static str>);
    let mut card_el = col().mt(12.0).radius(OVERLAY_RADIUS + 2.0).border(t_.card_stroke).bg(t_.card).clip();
    if visible.is_empty() {
        card_el = card_el.child(row().min_h(128.0).p(24.0).center().child(text(t("这个分组里没有快捷键", "There are no shortcuts in this group")).ty(CAPTION).color(t_.text_tertiary)));
    }
    for (index, &command) in visible.iter().enumerate() {
        let resolved = resolve_shortcut(&shortcuts, command);
        let label = command.label();
        let empty = resolved.binding.is_empty();
        let is_recording = recording == Some(command.id);
        let conflict_with = conflict.filter(|(c, _)| *c == command.id).and_then(|(_, other)| shortcut_command(other)).map(|c| c.label());

        let undo = is_shortcut_modified(&shortcuts, command).then(|| {
            icon_button::<App>("RotateCcw", tf("撤销「{name}」的修改", "Undo changes to “{name}”", &[("name", &label)])).key(("shortcut-undo", command.id)).size(23.0, 23.0).ml(-4.0).on_click(move |app: &mut App| {
                let mut next = app.doc.global_settings.shortcuts.clone();
                next.remove(command.id);
                set_shortcuts(app, next);
            })
        });
        let binding: E = if is_recording {
            let caps: Vec<String> = pending.iter().map(|tk| key_token_label(tk, None)).collect();
            row()
                .key(("shortcut-recording", command.id))
                .w(224.0)
                .min_h(32.0)
                .py(3.0)
                .px(7.0)
                .gap(8.0)
                .items_center()
                .justify_between()
                .radius(7.0)
                .border(t_.accent)
                .bg(t_.accent.with_alpha(0.12))
                .tooltip(tf("为「{name}」录制快捷键", "Record shortcut for “{name}”", &[("name", &label)]))
                .role(Role::Button)
                .aria_label(tf("为「{name}」录制快捷键", "Record shortcut for “{name}”", &[("name", &label)]))
                .on_key(move |app: &mut App, k: &KeyPress| handle_recording_key(app, command, k))
                .child(text(t("按下快捷键", "Press shortcut")).ty(CAPTION).color(t_.text_secondary).nowrap())
                .child((!caps.is_empty()).then(|| caps_row(caps)))
                .child(recording_blur_watch(command.id))
        } else {
            let tip = if command.editable { t("点击后按下新的组合键", "Click, then press a new key combination") } else { t("此快捷键由应用保留，不能修改", "This shortcut is reserved by the application") };
            row()
                .key(("shortcut-binding", command.id))
                .min_w(112.0)
                .min_h(30.0)
                .py(3.0)
                .px(8.0)
                .gap(4.0)
                .items_center()
                .justify_end()
                .radius(7.0)
                .border(t_.card_stroke)
                .hover_bg(t_.subtle_hover)
                .when(!command.editable, |e| e.opacity(0.5))
                .tooltip(tip)
                .when(!command.editable, |e| e.disabled_reason(tip))
                .when(a11y_active(), |e| e.aria_label(if command.editable { tf("修改「{name}」的快捷键", "Change shortcut for “{name}”", &[("name", &label)]) } else { tf("「{name}」的快捷键不可修改", "The shortcut for “{name}” cannot be changed", &[("name", &label)]) }))
                .disabled(!command.editable)
                .on_click(move |app: &mut App| {
                    let p = page(app);
                    p.conflict = None;
                    p.pending.clear();
                    p.recording = Some(command.id);
                    focus_request(("shortcut-recording", command.id));
                })
                .child(if empty { text(t("按下快捷键", "Press shortcut")).ty(CAPTION).color(t_.text_tertiary).nowrap() } else { caps_row(shortcut_key_caps(&resolved.binding, None)) })
        };
        let enabled = resolved.enabled;
        let switch_el = labeled_switch(("shortcut-enabled", command.id), enabled, empty, move |app| handle_enabled_change(app, command, !enabled)).when(a11y_active(), |e| e.aria_label(tf("启用「{name}」", "Enable “{name}”", &[("name", &label)]))).when(empty, |e| e.disabled_reason(t("先绑定一个组合键才能改启用状态", "Bind a key combination before changing its enabled state")));
        card_el = card_el.child(
            row()
                .key(("shortcut-row", command.id))
                .min_h(52.0)
                .py(8.0)
                .px(12.0)
                .gap(12.0)
                .items_center()
                .when(index > 0, |e| e.border_top(t_.divider))
                // Disabled rows remain interactive for recording, undo and toggling; hovering one brings it most of the way back.
                .when(!enabled, |e| e.opacity(if hovered_row.get() == Some(command.id) { 0.82 } else { 0.56 }))
                .on_hover_change({
                    let hovered_row = hovered_row.clone();
                    move |_: &mut App, on| {
                        if on {
                            hovered_row.set(Some(command.id));
                        } else if hovered_row.get() == Some(command.id) {
                            hovered_row.set(None);
                        }
                    }
                })
                .child(row().flex1().min_w(0.0).items_center().gap(5.0).child(undo).child(text(label).ty(BODY_SMALL).ellipsis()))
                .child(col().w(224.0).shrink0().items_end().gap(3.0).child(binding).child(conflict_with.map(|name| text(tf("与「{name}」冲突", "Conflicts with “{name}”", &[("name", &name)])).ty(FINE).color(t_.critical).ellipsis().max_w(224.0).role(Role::Alert))))
                .child(row().w(40.0).min_h(30.0).shrink0().center().child(switch_el)),
        );
    }
    let title = page_title(t("快捷键", "Keyboard shortcuts"));
    let description = page_description(t("查看和自定义应用快捷键。", "View and customize application shortcuts."));
    let heading = if narrow { col().gap(8.0).pb(8.0).child(page_heading(Some(title), description, None).min_h(0.0)).child(actions.w_full()) } else { page_heading(Some(title), description, Some(actions)) };
    crate::widgets::page().child(heading).child(card_el)
}
