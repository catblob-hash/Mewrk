//! The message cards of `ContextStream.tsx`: `ContextCard`, `TimelineInsertEditor`, `FoldableUserText`, `BranchNavigation`,
//! `TurnErrorNotice`.

use mewrk_client::auto_compact::compaction_title;
use mewrk_client::conversation_branches::ContextBranchNavigation;
use mewrk_client::model::ContextItem;
use mewrk_client::prelude::*;
use mewrk_client::runs::context_tokens::{estimate_context_tokens, format_compact_token_count};
use mewrk_client::runs::image_short_ids::text_without_appended_image_placeholders;
use mewrk_client::runs::{ConversationTurnError, ConversationTurnStatus};
use mewrk_pane_browser::selected_element::strip_selected_element_blocks;
use mewrk_ui_common::markdown::{markdown_content, MarkdownOptions};

use crate::frame::{hash_id, Frame};
use crate::image_strip::image_strip;
use crate::inline_text_editor::{inline_text_editor, inline_text_editor_parts, InsertKind, TextEditorProps};
use crate::inline_tool_editor::{draft_tool_context, inline_tool_editor_parts, ToolEditorProps};
use crate::item::Item;
use crate::kit::{action_button, action_button_disabled, badge, copy_button};
use crate::nodes::{first_prose_line, safe_source_href, source_hostname};
use crate::orchestration::is_host_authored_user_context;
use crate::props::TimelineEditorState;
use crate::rows::{find_item, insertion_line, interrupted_badge, open_path_handler, prose, timeline_row, RowSpec};
use crate::tool_view::{get_tool_presentation, tool_row_name};
use crate::waiting::{stream_waiting_indicator, WaitingProps};

/// `--timeline-actions-inset`.
const ACTIONS_INSET: f32 = 1.0;
/// `--context-actions-width`: three 24 px buttons and their gaps.
const ACTIONS_WIDTH: f32 = 74.0;
/// How many lines a long message shows while folded.
const FOLDED_USER_MESSAGE_LINES: f32 = 10.0;
/// A message only this much longer than its folded height shows whole: folding would hide next to nothing.
const USER_MESSAGE_FOLD_SLACK_LINES: f32 = 3.0;

/// A user message's text. One long enough to push the conversation out of view waits folded to its first lines under
/// "Show more". Wrapping decides what is long, so it is measured, not counted.
fn foldable_user_text(f: &Frame<'_>, id: &str, line: f32, content: E, source: &str) -> E {
    let t_ = theme();
    let inner_key = hash_key(("tl-fold-inner", f.salt, id));
    let expand_key = hash_key(("fold", id));
    // Until it has been laid out once the newlines are the best guess there is, and a wrong one only costs a frame.
    let whole_height = rect_of(inner_key).map(|r| r.h).unwrap_or_else(|| (source.split('\n').count() as f32) * line);
    let foldable = whole_height > line * (FOLDED_USER_MESSAGE_LINES + USER_MESSAGE_FOLD_SLACK_LINES);
    let expanded = f.expanded(expand_key, false);
    let folded = foldable && !expanded;
    let body = content.key(inner_key).shrink0();
    let mut view = col().min_w(0.0);
    view = if folded {
        view.child(
            div()
                .max_h(line * FOLDED_USER_MESSAGE_LINES)
                .clip()
                .min_w(0.0)
                .child(body)
                // `.context-card__content--folded`: fading out at the cut.
                .child(div().absolute().left(0.0).right(0.0).bottom(0.0).h(line * 2.0).gradient(t_.card.fade(0.0), t_.card).pass_through()),
        )
    } else {
        view.child(body)
    };
    if foldable {
        let shared = f.shared.clone();
        let toggle_key = hash_key(("tl-fold", f.salt, id));
        view = view.child(
            row()
                .key(toggle_key)
                .self_start()
                .items_center()
                .gap(3.0)
                .mt(5.0)
                .ml(-4.0)
                .pl(3.0)
                .pr(6.0)
                .py(1.0)
                .radius(5.0)
                .hover_bg(t_.subtle_hover)
                .aria_expanded(expanded)
                .aria_controls(inner_key)
                .child(icon(if expanded { "ChevronUp" } else { "ChevronDown" }, 13.0).color(t_.text_secondary))
                .child(text(if expanded { t("收起", "Show less") } else { t("展开", "Show more") }).ty(TypeStyle { size: 12.0, line: 18.0, weight: Weight::Regular, mono: false, italic: false, serif: false }).color(t_.text_secondary).nowrap())
                .on_click(move |_: &mut App| {
                    let mut state = shared.borrow_mut();
                    state.expanded.insert(expand_key, !expanded);
                    // Folding a message read to its end leaves the reader below it; bring the toggle, and the message's end, back into view.
                    if expanded {
                        state.pending_reveal = Some((toggle_key, 2));
                    }
                    repaint();
                }),
        );
    }
    view
}

/// Which of a message's branches is on screen. It is state, not an action, so it stays under the bubble where reading it
/// never needs a hover.
fn branch_navigation(f: &Frame<'_>, fork_id: &str, navigation: &ContextBranchNavigation) -> E {
    let t_ = theme();
    let previous = navigation.active_index.checked_sub(1).and_then(|at| navigation.branch_ids.get(at)).cloned();
    let next = navigation.branch_ids.get(navigation.active_index + 1).cloned();
    let reason = f.props.branch_switch_disabled_reason.clone();
    let button = |name: &'static str, label: &'static str, target: Option<String>, slot: &'static str| -> E {
        let key = ("tl-branch", slot, f.salt, fork_id.to_owned());
        let tip = reason.clone().unwrap_or_else(|| label.to_owned());
        match (target, reason.is_some(), f.props.on_select_branch.clone()) {
            (Some(branch), false, Some(select)) => {
                let fork = fork_id.to_owned();
                action_button(key, name, tip, move |app| select(app, &fork, &branch)).aria_label(label)
            }
            _ => action_button_disabled(key, name, tip).aria_label(label).maybe_disabled_reason(reason.clone()),
        }
    };
    row()
        .key(("tl-branches", f.salt, fork_id))
        .absolute()
        .bottom(-27.0)
        .right(ACTIONS_INSET - 1.0)
        .h(24.0)
        .items_center()
        .gap(1.0)
        .role(Role::Group)
        .aria_label(t("此消息的分支", "Branches from this message"))
        .child(button("ChevronLeft", t("上一个分支", "Previous branch"), previous, "previous"))
        .child(row().min_w(29.0).center().child(text(format!("{} / {}", navigation.active_index + 1, navigation.branch_ids.len())).ty(TypeStyle { size: 10.0, line: 14.0, weight: Weight::Regular, mono: false, italic: false, serif: false }).color(t_.text_tertiary).nowrap().aria_live(Live::Polite)))
        .child(button("ChevronRight", t("下一个分支", "Next branch"), next, "next"))
}

/// `ContextActions`: whatever else a message offers first, then copy, then edit, and delete last, so delete is always the
/// rightmost button. Copying changes nothing, so it stays on a timeline that is read-only or busy running.
#[allow(clippy::too_many_arguments)]
fn context_actions(f: &Frame<'_>, id: &str, leading: Option<E>, copy_text_: Option<&str>, copy_label: &str, copy_disabled: bool, show_edit: bool, show_delete: bool, mutation_disabled: bool) -> E {
    let mut bar = row().items_center().gap(1.0);
    bar = bar.child(leading);
    if let Some(text_) = copy_text_.filter(|text_| !text_.is_empty()) {
        bar = bar.child(copy_button(("tl-copy", f.salt, id), text_, copy_label.to_owned(), copy_disabled));
    }
    let item_handler = |handler: Option<crate::props::OnContext>, slot: &'static str, name: &'static str, label: &'static str| -> E {
        let key = (slot, f.salt, id.to_owned());
        match (handler, mutation_disabled) {
            (Some(handler), false) => {
                let (props, id) = (f.props.clone(), id.to_owned());
                action_button(key, name, label, move |app| {
                    if let Some(item) = find_item(app, &props, &id) {
                        handler(app, &item);
                    }
                })
            }
            _ => action_button_disabled(key, name, label),
        }
    };
    if show_edit && f.props.on_edit.is_some() {
        bar = bar.child(item_handler(f.props.on_edit.clone(), "tl-edit", "Pencil", t("编辑上下文", "Edit context")));
    }
    if show_delete && f.props.on_delete.is_some() {
        bar = bar.child(item_handler(f.props.on_delete.clone(), "tl-delete", "Trash2", t("删除上下文", "Delete context")));
    }
    bar
}

/// `ContextCard`.
pub fn context_card(f: &Frame<'_>, index: usize) -> E {
    let t_ = theme();
    let context = f.contexts.get(index);
    let item = context.item;
    let id = item.id();
    let unit = f.unit_key(id);
    let editing = f.editing_context_id == Some(id);
    let editing_in_place = editing && f.props.on_cancel_edit.is_some() && f.props.on_save_text.is_some();
    let show_mutation_actions = !f.read_only && !editing_in_place;

    // A reply or a message has no card shell to sit in while being written, so both creating one from the context menu and
    // a pencil edit show the bare editor.
    if editing_in_place && let ContextItem::Assistant { content, .. } | ContextItem::User { content, .. } = item {
        let (images, files, kind) = match item {
            ContextItem::User { images, files, .. } => (images.clone(), files.clone(), InsertKind::User),
            _ => (Vec::new(), Vec::new(), InsertKind::Assistant),
        };
        return col().key(unit).w_full().child(inline_text_editor(
            ("tl-text-edit", f.salt, id),
            TextEditorProps {
                kind,
                content: content.clone(),
                images,
                files,
                show_kind: false,
                image_input: f.props.attachment_image_input,
                on_cancel: f.props.on_cancel_edit.clone().expect("checked"),
                on_save: f.props.on_save_text.clone().expect("checked"),
                on_add_attachments: if kind == InsertKind::User { f.props.on_add_attachments.clone() } else { None },
            },
        ));
    }

    let mut card = match item {
        ContextItem::System { content, local_only, native_compaction, .. } => system_card(f, index, content, *local_only, native_compaction.as_deref(), editing_in_place, show_mutation_actions),
        ContextItem::User { content, images, files, .. } => user_card(f, id, item, content, images, files),
        ContextItem::Assistant { content, interrupted, sources, .. } => assistant_card(f, id, context, content, *interrupted, sources, show_mutation_actions),
        _ => col(),
    };
    card = card.key(unit);
    // A message is an `<article>`, a system prompt a row that says so itself.
    if !matches!(item, ContextItem::System { .. }) {
        card = card.role(Role::Article).aria_busy(context.streaming);
    }
    // The system prompt's own row lights up for it, so the card around that row does not.
    if !matches!(item, ContextItem::System { .. }) && f.selected.contains(id) {
        card = card.bg(t_.selection);
    }
    if f.dimmed.contains(id) {
        card = card.opacity(0.6);
    }
    card = f.picked_cursor(id, f.slid(id, card));
    if !f.mutation_read_only && !editing_in_place {
        let open = f.open_insert.clone();
        let keyboard = f.open_insert.clone();
        card = card
            .focusable()
            .on_context_menu(move |app, event| {
                let after = event.y > event.rect.y + event.rect.h * 0.5;
                open(app, event, index + usize::from(after));
            })
            .on_key(move |app, press| {
                if press.shift && press.key == Key::F(10) {
                    if let Some(r) = rect_of(unit) {
                        keyboard(app, PointerEvent { phase: Phase::Down, x: r.x + 32.0, y: r.bottom(), dx: 0.0, dy: 0.0, rect: r }, index + 1);
                    }
                    return true;
                }
                false
            });
    }
    card
}

#[allow(clippy::too_many_arguments)]
fn system_card(f: &Frame<'_>, index: usize, content: &str, local_only: bool, compaction: Option<&mewrk_client::model::NativeCompaction>, editing_in_place: bool, show_mutation_actions: bool) -> E {
    let id = f.contexts.get(index).id();
    let context = f.contexts.get(index);
    if let Some(compaction) = compaction {
        // A native compaction is a host record of its own, not a system prompt: it reads as what happened, and only deletes.
        let mut bar = None;
        if show_mutation_actions {
            bar = Some(context_actions(f, id, None, None, "", false, false, true, f.mutation_read_only));
        }
        return col().w_full().child(timeline_row(
            f,
            RowSpec {
                context_id: format!("{id}"),
                index,
                icon: "Shrink",
                name: compaction_title(compaction),
                label: None,
                figures: None,
                line: None,
                line_streaming: false,
                stat: None,
                status: None,
                accessible_name: t("原生压缩", "Native compaction").to_owned(),
                badge: None,
                actions: bar,
                actions_pinned: false,
                expandable: false,
                expanded: false,
                on_toggle: None,
                on_open_panel: None,
                read_only: f.mutation_read_only,
                on_open_insert: Some(f.open_insert.clone()),
                body: None,
            },
        ));
    }
    let expand_key = hash_id(id);
    let prompt_expanded = f.expanded(expand_key, !f.collapse_reasoning);
    let prompt_editing = editing_in_place;
    let has_content = !content.is_empty();
    let open = (has_content || prompt_editing) && (prompt_expanded || prompt_editing);
    let mut actions_pinned = false;
    let mut actions = None;
    let body = if prompt_editing {
        let parts = inline_text_editor_parts(("tl-system-edit", f.salt, id), TextEditorProps { kind: InsertKind::System, content: content.to_owned(), images: Vec::new(), files: Vec::new(), show_kind: false, image_input: None, on_cancel: f.props.on_cancel_edit.clone().expect("checked"), on_save: f.props.on_save_text.clone().expect("checked"), on_add_attachments: None });
        actions = Some(parts.bar);
        actions_pinned = true;
        Some(parts.body)
    } else if open {
        Some(prose(f, ("tl-system", id), content, false))
    } else {
        None
    };
    if !actions_pinned && (show_mutation_actions || has_content) {
        let mut bar = row().items_center().gap(1.0);
        if has_content {
            bar = bar.child(copy_button(("tl-copy", f.salt, id), content, t("复制系统提示词", "Copy system prompt"), false));
        }
        if show_mutation_actions {
            bar = bar.child(context_actions(f, id, None, None, "", false, true, true, f.mutation_read_only));
        }
        actions = Some(bar);
    }
    let shared = f.shared.clone();
    let default_open = !f.collapse_reasoning;
    let on_toggle: Handler<App> = Rc::new(move |_| {
        if prompt_editing {
            return;
        }
        let mut state = shared.borrow_mut();
        let now = state.expanded.get(&expand_key).copied().unwrap_or(default_open);
        state.expanded.insert(expand_key, !now);
        repaint();
    });
    let tokens = format_compact_token_count(estimate_context_tokens(context.item));
    col().w_full().child(timeline_row(
        f,
        RowSpec {
            context_id: id.to_owned(),
            index,
            icon: "Shield",
            name: "system".to_owned(),
            label: None,
            figures: None,
            line: first_prose_line(content).map(str::to_owned),
            line_streaming: false,
            stat: Some(tf("{tokens} token", "{tokens} tokens", &[("tokens", &tokens)])),
            status: None,
            accessible_name: t("系统提示词", "System prompt").to_owned(),
            badge: local_only.then(|| badge(t("仅本地", "Local only"), t("只保存在本地时间线，不会发送给模型", "Saved only in the local timeline and not sent to the model"))),
            actions,
            actions_pinned,
            expandable: has_content || prompt_editing,
            expanded: open,
            on_toggle: Some(on_toggle),
            on_open_panel: None,
            read_only: f.mutation_read_only,
            on_open_insert: Some(f.open_insert.clone()),
            body,
        },
    ))
}

fn user_card(f: &Frame<'_>, id: &str, item: &ContextItem, content: &str, images: &[mewrk_client::model::ImageAttachment], files: &[mewrk_client::model::FileAttachment]) -> E {
    let t_ = theme();
    // A picked element expands into a prompt block the user never typed, and an attached image gets the `[Image #N]` the
    // model cites it by. Both expansions belong to the model, so the transcript shows the message without them.
    let stripped = strip_selected_element_blocks(content);
    let display = text_without_appended_image_placeholders(&stripped, images);
    let branch = f.props.branch_navigations.get(id);
    let mut card = col()
        .self_end()
        // `.context-card--user { width: auto; max-width: 78%; min-width: min(100%, actions + 32px) }`.
        .max_w(f.column_w * 0.78)
        .min_w(ACTIONS_WIDTH + 32.0)
        .pt(9.0)
        .px(12.0)
        .pb(11.0)
        .radius(10.0)
        .bg(t_.card)
        .border(t_.card_stroke)
        .when(branch.is_some(), |e| e.mb(26.0));
    if !images.is_empty() || !files.is_empty() {
        card = card.child(div().mb(if display.is_empty() { 0.0 } else { 8.0 }).child(image_strip(("tl-images", f.salt, id), images, files)));
    }
    if !display.is_empty() {
        let ty_ = f.message_ty;
        let body: E = if f.render_user_markdown {
            // Only model output is scanned for paths and has its HTML rendered. User text keeps rendering exactly what was typed.
            markdown_content(("tl-user", id), &display, MarkdownOptions { linkify_paths: false, render_html: false, path_base_dir: f.props.path_base_dir.clone(), single_dollar_math: f.single_dollar_math, ty: ty_, ..MarkdownOptions::default() }).on_context_menu(card_menu_passthrough(f, id))
        } else {
            text(display.clone()).ty(ty_).color(t_.text_primary).selectable()
        };
        card = card.child(foldable_user_text(f, id, ty_.line, body, &display));
    }
    if !content.is_empty() || !f.read_only {
        let mut leading = row().items_center().gap(1.0);
        let mut any_leading = false;
        if !f.read_only {
            if let Some(on_branch) = f.props.on_branch_from.clone() {
                any_leading = true;
                let reason = f.props.branch_from_disabled_reason.clone();
                let key = ("tl-branch-from", f.salt, id);
                let named = t("从此消息分支", "Branch from this message");
                leading = leading.child(match reason {
                    Some(reason) => action_button_disabled(key, "GitBranch", reason.clone()).aria_label(named).disabled_reason(reason),
                    None => {
                        let (props, id_) = (f.props.clone(), id.to_owned());
                        action_button(key, "GitBranch", t("在新对话中继续这条消息", "Continue this message in a new conversation"), move |app| {
                            if let Some(item) = find_item(app, &props, &id_) {
                                on_branch(app, &item);
                            }
                        })
                        .aria_label(named)
                    }
                });
            }
            // A note the host wrote in the user's place is not a message the user sent, so there is nothing to rewind to.
            if let (Some(on_rewind), false) = (f.props.on_rewind_to.clone(), is_host_authored_user_context(item)) {
                any_leading = true;
                let key = ("tl-rewind", f.salt, id);
                let reason = f.props.rewind_disabled_reason.clone();
                let named = t("回退到此消息之前", "Rewind to before this message");
                leading = leading.child(if f.mutation_read_only || reason.is_some() {
                    action_button_disabled(key, "Undo2", t("把对话和文件退回到这条消息发出之前", "Put the conversation and files back to before this message was sent")).aria_label(named).maybe_disabled_reason(reason)
                } else {
                    let (props, id_) = (f.props.clone(), id.to_owned());
                    action_button(key, "Undo2", t("把对话和文件退回到这条消息发出之前", "Put the conversation and files back to before this message was sent"), move |app| {
                        if let Some(item) = find_item(app, &props, &id_) {
                            on_rewind(app, &item);
                        }
                    })
                    .aria_label(named)
                });
            }
        }
        let bar = context_actions(f, id, any_leading.then_some(leading), Some(content), t("复制用户消息", "Copy user message"), false, !f.read_only, !f.read_only, f.mutation_read_only);
        // Laid over the corner, level with the message's first line, on a ground of its own so the words go under it.
        card = card.child(row().absolute().top(4.0).right(ACTIONS_INSET - 1.0).radius(6.0).bg(t_.flyout).border(t_.flyout_stroke).reveal_on_parent_hover().child(bar));
    }
    if let Some(navigation) = branch {
        card = card.child(branch_navigation(f, id, navigation));
    }
    card
}

/// A right click on a message's text is a right click on the message: the leaf that draws it keeps pointer events to itself.
fn card_menu_passthrough(f: &Frame<'_>, id: &str) -> impl Fn(&mut App, PointerEvent) + 'static {
    let open = f.open_insert.clone();
    let shared = f.shared.clone();
    let id = id.to_owned();
    let unit = f.unit_key(&id);
    move |app, event| {
        let index = shared.borrow().context_index.get(&hash_id(&id)).copied().unwrap_or(0) as usize;
        let card = rect_of(unit).unwrap_or(event.rect);
        let after = event.y > card.y + card.h * 0.5;
        open(app, event, index + usize::from(after));
    }
}

fn assistant_card(f: &Frame<'_>, id: &str, context: Item<'_>, content: &str, interrupted: bool, sources: &[mewrk_client::model::ContextSource], show_mutation_actions: bool) -> E {
    let t_ = theme();
    let streaming = context.streaming;
    let mut card = col().w_full().pt(2.0).radius(10.0).min_w(0.0);
    // `.context-card--assistant > * { max-width: min(86%, 100% - actions - inset - 10px) }`.
    let column = f.column_w;
    let narrowed = move |e: E| e.w((column * 0.86).min(column - ACTIONS_WIDTH - ACTIONS_INSET - 10.0).max(120.0)).min_w(0.0);
    if interrupted {
        card = card.child(row().mb(4.0).child(interrupted_badge()));
    }
    let body = markdown_content(("tl-assistant", id), content, MarkdownOptions { streaming, linkify_paths: true, render_html: true, path_base_dir: f.props.path_base_dir.clone(), single_dollar_math: f.single_dollar_math, ty: f.message_ty, on_open_path: Some(open_path_handler()), ..MarkdownOptions::default() });
    let body = if f.mutation_read_only { body } else { body.on_context_menu(card_menu_passthrough(f, id)) };
    card = card.child(narrowed(col().when(streaming, |e| e.aria_live(Live::Polite)).child(body)));
    if !sources.is_empty() {
        let mut chips = row().wrap().gap(4.0).mt(6.0).role(Role::Navigation).aria_label(t("引用来源", "Cited sources"));
        for source in sources {
            let label = source.title.as_deref().map(str::trim).filter(|title| !title.is_empty()).map(str::to_owned).or_else(|| source_hostname(source.url.as_deref())).unwrap_or_else(|| source.id.clone());
            let href = safe_source_href(source.url.as_deref());
            let mut chip = row().key(("tl-source", f.salt, id, source.id.as_str())).items_center().max_w(260.0).px(8.0).py(2.0).radius(999.0).border(t_.card_stroke).tooltip(href.clone().unwrap_or_else(|| source.id.clone()));
            chip = chip.child(text(label).ty(TypeStyle { size: 11.0, line: 15.0, weight: Weight::Regular, mono: false, italic: false, serif: false }).color(if href.is_some() { t_.accent_text } else { t_.text_secondary }).ellipsis());
            // Citations without a trusted URL are listed too; omitting them hides part of what the model cited.
            if let Some(href) = href {
                chip = chip.hover_bg(t_.subtle_hover).role(Role::Link).on_click(move |app: &mut App| app.invoke_logged("open_external_url", json!({ "url": href })));
            }
            chips = chips.child(chip);
        }
        card = card.child(narrowed(chips));
    }
    // A message names itself by which side it sits on, so it carries no title; its controls surface only while the pointer is on it.
    if show_mutation_actions || !content.is_empty() {
        let bar = context_actions(f, id, None, Some(content), t("复制模型回复", "Copy model reply"), streaming, show_mutation_actions, show_mutation_actions, f.mutation_read_only || streaming);
        card = card.child(row().absolute().top(0.0).right(ACTIONS_INSET).radius(6.0).bg(t_.flyout).border(t_.flyout_stroke).reveal_on_parent_hover().child(bar));
    }
    card
}

/// `TimelineInsertEditor`: a context being placed, drawn in the shell it will have once it exists. Placing a context and
/// correcting one are the same act on the same surface, so they are the same editor inside the same card or row.
pub fn timeline_insert_editor(f: &Frame<'_>) -> Option<E> {
    let TimelineEditorState::Insert { kind, index, tool_name } = f.props.editor.as_ref()? else { return None };
    let on_cancel = f.props.on_cancel_edit.clone()?;
    let (kind, index) = (*kind, *index);
    let slot = |e: E| col().w_full().py(4.0).shrink0().child(e);
    let draft_row = |spec: RowSpec| timeline_row(f, spec);
    let bare = |context_id: &str, icon: &'static str, name: String, accessible: String, actions: E, body: E| RowSpec {
        context_id: context_id.to_owned(),
        index,
        icon,
        name,
        label: None,
        figures: None,
        line: None,
        line_streaming: false,
        stat: None,
        status: None,
        accessible_name: accessible,
        badge: None,
        actions: Some(actions),
        actions_pinned: true,
        expandable: true,
        expanded: true,
        on_toggle: None,
        on_open_panel: None,
        // Not a context of its own yet, so it is not something to right-click an insertion around.
        read_only: true,
        on_open_insert: None,
        body: Some(body),
    };
    if kind == InsertKind::Tool {
        let tool_name = tool_name.as_deref()?;
        // A placed call has to be committable somewhere, by either route: running it writes it down, and so does writing it down by hand.
        if f.props.on_save_tool.is_none() && f.props.on_save_tool_edit.is_none() {
            return None;
        }
        let descriptor = f.props.tools.iter().find(|tool| tool.name == tool_name);
        let draft = draft_tool_context(tool_name);
        let draft_item = Item::settled(&draft);
        let tool = draft_item.tool().expect("a draft tool card");
        let presentation = get_tool_presentation(&tool, descriptor, None, None);
        let name = tool_row_name(&tool, descriptor);
        // A different tool is a different set of arguments, so the editor starts over.
        let parts = inline_tool_editor_parts(("tl-tool-insert", tool_name), ToolEditorProps { item: draft.clone(), descriptor: descriptor.cloned(), inserting: true, on_cancel, on_run: f.props.on_save_tool.clone(), on_save: f.props.on_save_tool_edit.clone() });
        let label = descriptor.map_or(tool_name.to_owned(), |descriptor| descriptor.label.clone());
        let spec = bare(draft.id(), presentation.icon, name, tf("添加工具调用 {label}", "Add tool call {label}", &[("label", &label)]), parts.actions, parts.body);
        return Some(slot(draft_row(spec)));
    }
    let on_save = f.props.on_save_text.clone()?;
    let props = TextEditorProps {
        kind,
        content: String::new(),
        images: Vec::new(),
        files: Vec::new(),
        show_kind: kind == InsertKind::User,
        image_input: f.props.attachment_image_input,
        on_cancel,
        on_save,
        on_add_attachments: if kind == InsertKind::User { f.props.on_add_attachments.clone() } else { None },
    };
    let key = ("tl-text-insert", kind as u8);
    // Reasoning and the system prompt live inside rows, so they are edited as rows; a reply has no shell of its own even while being written.
    Some(slot(match kind {
        InsertKind::Reasoning => {
            let parts = inline_text_editor_parts(key, props);
            draft_row(bare("draft", "BrainCircuit", "think".to_owned(), t("添加思考字段", "Add a reasoning field").to_owned(), parts.bar, parts.body))
        }
        InsertKind::System => {
            let parts = inline_text_editor_parts(key, props);
            draft_row(bare("draft", "Shield", "system".to_owned(), t("添加系统提示词", "Add a system prompt").to_owned(), parts.bar, parts.body))
        }
        _ => inline_text_editor(key, props),
    }))
}

/// `TurnErrorNotice`: a run failure, read where it happened. It is a rendered notice rather than a context: it is never
/// persisted into the conversation and never reaches the model, so the timeline explains the gap without inventing history.
fn turn_error_notice(f: &Frame<'_>, error: &ConversationTurnError, retryable: bool, key: &str, follows_changes: bool) -> E {
    let t_ = theme();
    let origin = [error.provider_name.as_str(), error.model_name.as_str()].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" · ");
    let title = if origin.is_empty() { t("模型请求失败", "The model request failed").to_owned() } else { origin };
    let mut notice = row().key(("tl-turn-error", key)).w_full().items_center().gap(7.0).pt(7.0).pb(7.0).pl(9.0).pr(7.0).mt(if follows_changes { 16.0 } else { 0.0 }).radius(8.0).bg(t_.critical_bg).border(t_.critical.fade(0.35)).role(Role::Alert);
    notice = notice.child(icon("CircleAlert", 15.0).color(t_.critical));
    notice = notice.child(
        col()
            .flex1()
            .child(text(title).ty(TypeStyle { size: 10.0, line: 14.0, weight: Weight::Semibold, mono: false, italic: false, serif: false }).color(t_.critical))
            .child(text(error.message.clone()).ty(TypeStyle { size: 9.0, line: 13.0, weight: Weight::Regular, mono: false, italic: false, serif: false }).color(t_.text_secondary).mt(1.0).selectable()),
    );
    if retryable && let Some(retry) = f.props.on_retry_turn_error.clone() {
        notice = notice.child(
            row()
                .key(("tl-turn-retry", key))
                .h(25.0)
                .px(8.0)
                .items_center()
                .radius(6.0)
                .hover_bg(t_.subtle_hover)
                .child(text(t("重试", "Retry")).ty(TypeStyle { size: 9.5, line: 13.0, weight: Weight::Semibold, mono: false, italic: false, serif: false }).color(t_.critical).nowrap())
                .on_click(move |app| retry(app)),
        );
    }
    if let Some(dismiss) = f.props.on_dismiss_turn_error.clone() {
        notice = notice.child(action_button(("tl-turn-dismiss", key), "X", t("关闭模型错误", "Dismiss model error"), move |app| dismiss(app)));
    }
    notice
}

/// What a round still draws for itself: the sign that it is still running, and the notice saying why it stopped.
pub fn turn_notices(f: &Frame<'_>, turn: &mewrk_client::runs::ConversationTurn, follows_changes: bool) -> Vec<E> {
    let mut out = Vec::new();
    if f.props.streaming && turn.status == ConversationTurnStatus::Running {
        out.push(waiting_indicator(f, &format!("turn:{}", turn.id)));
    }
    if let Some(error) = &turn.error {
        let retryable = f.props.retryable_turn_request_id.as_deref() == Some(turn.request_id.as_str());
        // `.turn-changes + .turn-error`: the card sits between two slots, and what follows it takes that space back.
        let follows_changes = follows_changes && out.is_empty();
        out.push(turn_error_notice(f, error, retryable, &turn.id, follows_changes));
    }
    out
}

/// `StreamWaitingIndicator` over the timeline's tail: only live calls are narrated, so nothing above the last user message counts.
pub fn waiting_indicator(f: &Frame<'_>, key: &str) -> E {
    let count = f.contexts.count();
    let mut start = count;
    while start > 0 {
        let item = f.contexts.get(start - 1);
        if matches!(item.item, ContextItem::User { .. }) && !item.streaming {
            break;
        }
        start -= 1;
    }
    let tail: Vec<Item<'_>> = (start..count).map(|index| f.contexts.get(index)).collect();
    stream_waiting_indicator(("tl-waiting", key), &tail, WaitingProps { tools: f.props.tools.clone(), thinking: f.props.thinking.clone(), retry_notice: f.props.retry_notice.clone(), running_task_count: f.props.running_task_count, on_open_tasks: f.props.on_open_tasks.clone() })
}

/// The stock empty notice.
pub fn stock_empty_state(mutation_read_only: bool) -> E {
    crate::kit::empty_state(Some(mewrk_ui_common::brand::mewrk_icon_bare(44.0)), t("这段对话还没有消息", "This conversation has no messages yet"), (!mutation_read_only).then_some(t("在上下文之间右键，可精确插入新内容", "Right-click between contexts to insert content precisely")))
}

/// `.insertion-line`.
pub fn insertion_line_view() -> E {
    insertion_line(&theme())
}
