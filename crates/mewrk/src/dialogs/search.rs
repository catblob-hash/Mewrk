//! `ConversationSearch.tsx`: finds a conversation by title or project, across every project. An empty query lists the most
//! recently updated first, so the palette doubles as a jump list; every word of a query has to appear, in any order, in the
//! title or the project's name.

use mewrk_client::drafts::visible_conversations;
use mewrk_client::model::Workspace;
use mewrk_client::prelude::*;
use mewrk_client::workspaces::is_temporary_workspace;

use crate::state::shell;

/// How many rows the palette lists; a longer query narrows it rather than scrolling on.
pub const MAX_RESULTS: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchEntry {
    pub workspace_id: String,
    pub conversation_id: String,
    pub title: String,
    pub project_name: String,
    pub updated_at: String,
    haystack: String,
}

pub fn search_entries(workspaces: &[Workspace], unloaded: &std::collections::HashSet<String>) -> Vec<SearchEntry> {
    let mut entries = Vec::new();
    for workspace in workspaces {
        let project_name = if is_temporary_workspace(workspace) { t("临时项目", "Temporary project").to_owned() } else { workspace.name.clone() };
        for conversation in visible_conversations(&workspace.conversations, unloaded) {
            entries.push(SearchEntry { workspace_id: workspace.id.clone(), conversation_id: conversation.id.clone(), title: conversation.title.clone(), project_name: project_name.clone(), updated_at: conversation.updated_at.clone(), haystack: format!("{}\n{project_name}", conversation.title).to_lowercase() });
        }
    }
    entries
}

/// `results`.
pub fn search_results(entries: &[SearchEntry], query: &str) -> Vec<SearchEntry> {
    let lowered = query.to_lowercase();
    let terms: Vec<&str> = lowered.split_whitespace().collect();
    let mut matched: Vec<SearchEntry> = if terms.is_empty() {
        let mut all = entries.to_vec();
        all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        all
    } else {
        entries.iter().filter(|entry| terms.iter().all(|term| entry.haystack.contains(term))).cloned().collect()
    };
    matched.truncate(MAX_RESULTS);
    matched
}

fn close(app: &mut App) {
    shell(app).search_open = false;
    repaint();
}

/// A modal over the whole window, like `Dialog`: the built-in browser's native page has to step aside rather than paint
/// over it, which any modal layer does.
pub fn conversation_search(app: &mut App) -> Option<E> {
    if !shell(app).search_open {
        return None;
    }
    let t_ = theme();
    let (window_w, window_h) = window_size();
    let query = use_state("conversation-search-query", String::new);
    let cursor = use_state("conversation-search-cursor", || 0usize);
    let opened = use_state("conversation-search-opened", || false);
    // What held the focus gets it back when the palette goes.
    // Read out here: `use_state` holds the runtime while it runs what it is given.
    let holder = focused_id();
    let previous = use_state("conversation-search-previous", move || holder).get();
    let close = move |app: &mut App| {
        close(app);
        focus_request_id(previous);
    };
    if !opened.get() {
        opened.set(true);
        focus_request(("field", "conversation-search"));
    }
    let active = app.active_conversation_id();
    let entries = search_entries(&app.doc.workspaces, &app.unloaded);
    let results = search_results(&entries, &query.get());
    let current = cursor.get().min(results.len().saturating_sub(1));

    // Keys come through the field, since it holds the focus for as long as the palette is up.
    let choose = {
        let results = results.clone();
        Rc::new(move |app: &mut App, index: usize| {
            if let Some(entry) = results.get(index) {
                app.select_conversation(&entry.workspace_id, &entry.conversation_id);
                close(app);
            }
        })
    };
    // `aria-activedescendant`: the field keeps the focus, so where the arrows land is said aloud.
    let spoken: Vec<String> = if a11y_active() { results.iter().map(|entry| entry.title.clone()).collect() } else { Vec::new() };
    let (typed, reset, moved, count) = (query.clone(), cursor.clone(), cursor.clone(), results.len());
    let submit = choose.clone();
    let submit_cursor = cursor.clone();
    let input = field::<App>("conversation-search", query.get())
        .ty(ty_input())
        .bare()
        .placeholder(t("按标题或项目搜索对话…", "Search conversations by title or project…"))
        .on_change(move |_, value| {
            typed.set(value);
            // Every new query starts from its best match.
            reset.set(0);
        })
        .on_key(move |app, press| match press.key {
            Key::Escape => {
                close(app);
                true
            }
            Key::Down | Key::Up if count > 0 => {
                let step: isize = if press.key == Key::Down { 1 } else { -1 };
                let next = (moved.get().min(count - 1) as isize + step).rem_euclid(count as isize) as usize;
                moved.set(next);
                if let Some(title) = spoken.get(next) {
                    announce(title.clone());
                }
                scroll_request("conversation-search-results", ScrollRequest::Reveal { y: next as f32 * ROW_H, h: ROW_H });
                true
            }
            _ => false,
        })
        .on_submit(move |app, _| submit(app, submit_cursor.get().min(count.saturating_sub(1))))
        .build()
        .role(Role::EditableComboBox)
        .aria_label(t("搜索对话", "Search conversations"))
        .aria_expanded(!results.is_empty())
        .aria_controls("conversation-search-results")
        .flex1()
        .h(18.0);
    let search_field = row().h(44.0).shrink0().items_center().gap(9.0).px(14.0).border_bottom(t_.divider).child(icon("Search", 15.0).color(t_.text_tertiary)).child(input);

    let mut list: E = col().key("conversation-search-results").role(Role::ListBox).aria_label(t("对话", "Conversations")).min_h(0.0).scroll_y().p(5.0);
    for (index, entry) in results.iter().enumerate() {
        let is_current = index == current;
        let is_open = active.as_deref() == Some(entry.conversation_id.as_str());
        let hover = cursor.clone();
        let pick = choose.clone();
        let mut result: E = row()
            .key(("conversation-search-result", entry.workspace_id.as_str(), entry.conversation_id.as_str()))
            .role(Role::ListBoxOption)
            .skip_tab(true)
            .aria_selected(is_current)
            .h(ROW_H)
            .shrink0()
            .items_center()
            .gap(12.0)
            .px(10.0)
            .radius(7.0)
            // Keep the focus in the field, so typing carries on after a click that misses.
            .keep_focus()
            .child(text(entry.title.clone()).ty(if is_open { CAPTION_STRONG } else { CAPTION }).ellipsis().flex1())
            .child(text(entry.project_name.clone()).ty(FINE).color(t_.text_tertiary).ellipsis().with_style(|s| s.flex_shrink = 1.0).max_w(200.0))
            .on_hover_change(move |_, on| {
                if on && hover.get() != index {
                    hover.set(index);
                }
            })
            .on_click(move |app: &mut App| pick(app, index));
        if is_current {
            result = result.bg(t_.subtle_hover);
        }
        list = list.child(result);
    }

    let mut panel: E = col().key("conversation-search").w((window_w - 48.0).min(560.0)).max_h((window_h - 128.0).min(520.0)).bg(t_.flyout).border(t_.flyout_stroke).radius(12.0).clip().keep_focus().on_click(|_| {}).role(Role::GenericContainer).child(search_field);
    if results.is_empty() {
        let message = if entries.is_empty() { t("还没有对话", "No conversations yet") } else { t("没有匹配的对话", "No matching conversations") };
        panel = panel.child(text(message).ty(CAPTION).color(t_.text_tertiary).py(18.0).px(16.0));
    } else {
        panel = panel.child(list);
    }
    Some(col().key("conversation-search-backdrop").aria_label(t("搜索对话", "Search conversations")).items_center().pt(64.0).pb(24.0).px(24.0).on_click(close).child(panel).modal_layer(Some(Rc::new(close))))
}

const ROW_H: f32 = 32.0;

fn ty_input() -> fluent::TypeStyle {
    fluent::TypeStyle { size: 13.5, line: 18.0, weight: fluent::Weight::Regular, mono: false, italic: false, serif: false }
}
