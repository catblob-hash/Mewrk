//! `TimelineBlock.tsx`: one uninterrupted stretch of work, drawn as its rows and nothing else. Tool calls, reasoning and
//! hook records are different kinds of record with the same shape on screen, so they share one row.

use mewrk_client::model::ContextItem;
use mewrk_client::model_capabilities::is_encrypted_reasoning;
use mewrk_client::prelude::*;
use mewrk_client::runs::context_tokens::{estimate_context_tokens, format_compact_token_count};
use mewrk_client::runs::StreamingToolPhase;
use mewrk_client::task_tools::is_preview_tool_name;
use mewrk_pane_files::links::PathOpenRequest;
use mewrk_pane_tasks::context::{LiveState, SubagentViewStatus};
use mewrk_pane_tasks::subagents::{agent_timeline_route_id, agent_timeline_run_status};
use mewrk_ui_common::markdown::{markdown_content, MarkdownOptions};

use crate::drag::{BLOCK_PAD_BOTTOM, BLOCK_PAD_TOP};
use crate::frame::{hash_id, Frame, ToolFacts};
use crate::inline_text_editor::{inline_text_editor_parts, InsertKind, TextEditorProps};
use crate::inline_tool_editor::{inline_tool_editor_parts, ToolEditorProps};
use crate::item::{Item, ToolItem};
use crate::kit::{action_button, badge, chevron, copy_button, docs_link, ty};
use crate::nodes::{reasoning_line, BlockEntry, EntryKind};
use crate::props::OpenInsert;
use crate::reveal::stream_reveal_text;
use crate::tool_detail::tool_detail_renderer;
use crate::tool_view::{get_tool_presentation, summarize_block_kinds, tool_row_name, tool_summary_kind, SummaryKind, TitleSubject, ToolPresentation, ToolViewFamily};
use crate::workflow_detail::workflow_run_detail;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTone {
    Success,
    Error,
    Running,
    Ready,
    Announced,
    Halted,
    Neutral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowStatus {
    pub tone: RowTone,
    pub label: String,
    /// Drawn at the end of the row. Only a row whose state the reader has to act on while it is still changing asks for one.
    pub spinner: bool,
}

fn status(tone: RowTone, label: &str) -> RowStatus {
    RowStatus { tone, label: label.to_owned(), spinner: false }
}

/// What a row reports about itself on the right: its own share of the figure a block totals. A record that projects to
/// nothing reports nothing rather than a zero.
fn token_stat(tokens: u64) -> Option<String> {
    (tokens > 0).then(|| tf("{tokens} token", "{tokens} tokens", &[("tokens", &format_compact_token_count(tokens))]))
}

/// `toolRowStatus`. An agent-run row cannot read its state off `result.success`: the call only acknowledges that the
/// protocol accepted it, so a completed spawn may still own a live background child.
fn tool_row_status(item: &ToolItem<'_>, family: ToolViewFamily, live: Option<&LiveState>) -> RowStatus {
    if item.streaming && !matches!(item.stream_status, Some(StreamingToolPhase::Completed | StreamingToolPhase::Running)) {
        if item.stream_status == Some(StreamingToolPhase::Ready) {
            return status(RowTone::Ready, t("等待执行", "Waiting to run"));
        }
        return status(RowTone::Announced, t("准备参数", "Preparing arguments"));
    }
    if family == ToolViewFamily::AgentRun {
        return match agent_timeline_run_status(&item.task_ref(live)) {
            SubagentViewStatus::Running => status(RowTone::Running, t("运行中", "Running")),
            SubagentViewStatus::Completed => status(RowTone::Success, t("完成", "Completed")),
            SubagentViewStatus::Stopped => status(RowTone::Halted, t("已停止", "Stopped")),
            SubagentViewStatus::RoundLimit => status(RowTone::Halted, t("已达轮次上限", "Round limit reached")),
            SubagentViewStatus::Failed => status(RowTone::Error, t("已失败", "Failed")),
            _ => status(RowTone::Error, t("已中断", "Interrupted")),
        };
    }
    if item.streaming && item.stream_status != Some(StreamingToolPhase::Completed) {
        return status(RowTone::Running, t("执行中", "Running"));
    }
    if item.result.success { status(RowTone::Success, t("完成", "Completed")) } else { status(RowTone::Error, t("失败", "Failed")) }
}

fn hook_row_status(execution_status: &str) -> RowStatus {
    match execution_status {
        "running" => status(RowTone::Running, t("执行中", "Running")),
        "blocked" => status(RowTone::Error, t("已阻止", "Blocked")),
        "failed" => status(RowTone::Halted, t("错误（未阻断）", "Error (not blocking)")),
        _ => status(RowTone::Success, t("完成", "Completed")),
    }
}

/// The mark on a reply or reasoning an error or Stop cut off. Such a fragment is kept for the reader but never sent to the
/// model; saving an edit of it makes it an ordinary card again.
pub fn interrupted_badge() -> E {
    badge(
        t("已打断", "Interrupted"),
        t("被错误或停止打断：只留在时间线上，不会发送给模型；编辑并保存后会作为普通卡片发送", "Cut off by an error or Stop: kept on the timeline but not sent to the model; edit and save it to send it as an ordinary card"),
    )
}

pub struct RowSpec {
    pub context_id: String,
    pub index: usize,
    pub icon: &'static str,
    pub name: String,
    /// What the name slot shows when it is more than text.
    pub label: Option<E>,
    /// Figures that belong to the name and stay whole when it is cut short: a change's line counts.
    pub figures: Option<E>,
    pub line: Option<String>,
    /// The line is still being written: what each commit adds sweeps in.
    pub line_streaming: bool,
    pub stat: Option<String>,
    pub status: Option<RowStatus>,
    /// What the row is called, for the tooltip: the row shows an identifier, not a sentence.
    pub accessible_name: String,
    pub badge: Option<E>,
    pub actions: Option<E>,
    /// The actions are an editor's: they stay up, rather than appear on hover.
    pub actions_pinned: bool,
    pub expandable: bool,
    pub expanded: bool,
    pub on_toggle: Option<Handler<App>>,
    /// Where a row that has no body of its own sends the click instead.
    pub on_open_panel: Option<Handler<App>>,
    pub read_only: bool,
    pub on_open_insert: Option<OpenInsert>,
    /// Built only when open: a closed row keeps nothing of its body, Markdown and diffs being the expensive part of a transcript.
    pub body: Option<E>,
}

/// The shared row: a marker, a name, one line of the record itself, and a body that opens under it. The controls are laid
/// over the end of the header rather than given a column: reserving space for controls nobody is reaching for would
/// shorten every line on screen to make room for the rare hover.
pub fn timeline_row(f: &Frame<'_>, spec: RowSpec) -> E {
    let t_ = theme();
    let RowSpec { context_id, index, icon: icon_name, name, label, figures, line, line_streaming, stat, status, accessible_name, badge: row_badge, actions, actions_pinned, expandable, expanded, on_toggle, on_open_panel, read_only, on_open_insert, body } = spec;
    let key = f.unit_key(&context_id);
    let tone = status.as_ref().map_or(RowTone::Neutral, |status| status.tone);
    let selected = f.selected.contains(&context_id);
    let (name_color, line_color) = match tone {
        RowTone::Error => (t_.critical, t_.critical),
        RowTone::Halted => (t_.caution, t_.text_tertiary),
        _ => (t_.text_secondary, t_.text_tertiary),
    };
    let has_line = line.is_some();
    let has_actions = actions.is_some();

    // A button upstream: a click on it leaves the focus there, inside the row, where the row's keys answer.
    let details_key = ("tl-row-details", f.salt, context_id.as_str());
    let summary_key = ("tl-row-summary", f.salt, context_id.as_str());
    let mut summary = row().key(summary_key).flex1().min_h(30.0).pl(3.0).pr(5.0).py(1.0).items_center().gap(6.0).radius(6.0).focusable().tooltip(accessible_name.clone()).role(Role::Button);
    // The row says what happened, and the identifier it says it about is only its tooltip, so the name has to carry both. The figures stay
    // out: a token count that re-announces itself every flush is worse than not hearing it at all.
    if a11y_active() {
        summary = summary.aria_label(format!("{name} · {accessible_name}")).aria_expanded(expandable && expanded);
        if expandable {
            summary = summary.aria_controls(details_key);
        } else if on_open_panel.is_some() {
            summary = summary.aria_haspopup(HasPopup::Dialog);
        } else {
            summary = summary.aria_disabled(true);
        }
    }
    summary = summary.child(row().size(18.0, 18.0).center().shrink0().child(icon(icon_name, 14.0).color(name_color)));
    summary = summary.child(match label {
        Some(label) => label,
        None => text(name.clone())
            .ty(ty::ROW)
            .color(name_color)
            .ellipsis()
            .with_style(|s| {
                s.flex_grow = 0.0;
                if has_line {
                    // The line carries the record, so a long name gives way first.
                    s.max_size.width = taffy::prelude::percent(0.4f32);
                }
            }),
    });
    summary = summary.child(figures);
    if let Some(line) = line {
        let view = if line_streaming { stream_reveal_text(("tl-row-line", f.salt, context_id.as_str()), &line, ty::ROW, line_color).min_w(0.0).shrink0() } else { text(line).ty(ty::ROW).color(line_color).ellipsis().with_style(|s| s.flex_grow = 0.0) };
        summary = summary.child(view);
    }
    if expandable {
        // Against the end of the row's own words: the arrow belongs to what it opens.
        summary = summary.child(chevron(expanded, 14.0, t_.text_tertiary).reveal_unless(expanded));
    }
    // An editor's controls stay up over the end of the row, so what they would be read over goes.
    let stat = stat.filter(|_| !actions_pinned);
    let row_badge = row_badge.filter(|_| !actions_pinned);
    if let Some(stat) = stat {
        // Laid over by the controls while they show, it steps aside, so nothing is read through a button.
        let figure = text(stat).ty(ty::FIGURE).color(t_.text_tertiary).nowrap().pl(10.0).shrink0();
        summary = summary.child(spacer()).child(if has_actions && !actions_pinned { figure.conceal_on_hover_up(2) } else { figure });
    }
    if expandable {
        if let Some(on_toggle) = on_toggle {
            summary = summary.on_click(move |app| on_toggle(app));
        }
    } else if let Some(on_open_panel) = on_open_panel {
        summary = summary.on_click(move |app| on_open_panel(app));
    }

    let mut header = row().w_full().min_w(0.0).items_center().radius(6.0).hover_bg(t_.subtle_hover).child(summary);
    if let Some(actions) = actions {
        let bar = row().absolute().top(3.0).right(1.0).items_center().gap(1.0).radius(6.0).child(actions);
        header = header.child(if actions_pinned { bar } else { bar.reveal_on_parent_hover() });
    }
    header = header.child(row_badge);
    if let Some(status) = &status
        && status.spinner
    {
        header = header.child(row().size(16.0, 16.0).center().shrink0().tooltip(status.label.clone()).role(Role::Status).aria_label(status.label.clone()).child(mewrk_pane_tasks::widgets::spinner(12.0, t_.accent)));
    }

    let busy = matches!(tone, RowTone::Running | RowTone::Ready | RowTone::Announced);
    let mut article = col().key(key).w_full().min_w(0.0).shrink0().role(Role::ListItem).aria_busy(busy).child(header);
    if selected {
        article = article.bg(t_.selection).radius(6.0);
    }
    if f.dimmed.contains(&context_id) {
        article = article.opacity(0.6);
    }
    if expandable && expanded {
        article = article.child(col().key(details_key).mt(2.0).mr(3.0).mb(8.0).ml(6.0).min_w(0.0).role(Role::Region).aria_labelledby(summary_key).child(body));
    }
    let mut article = f.picked_cursor(&context_id, f.slid(&context_id, article));
    if !read_only {
        article = article.focusable();
        if let Some(open) = on_open_insert {
            let keyboard = open.clone();
            article = article
                .on_context_menu(move |app, event| {
                    let after = event.y > event.rect.y + event.rect.h * 0.5;
                    open(app, event, index + usize::from(after));
                })
                .on_key(move |app, press| {
                    if press.shift && press.key == Key::F(10) {
                        if let Some(r) = rect_of(key) {
                            keyboard(app, PointerEvent { phase: Phase::Down, x: r.x + 32.0, y: r.bottom(), dx: 0.0, dy: 0.0, rect: r }, index + 1);
                        }
                        return true;
                    }
                    false
                });
        }
    }
    article
}

/// The workspace number a call named, or `None` for the conversation's first.
fn workspace_of(item: &ToolItem<'_>) -> Option<u32> {
    let value = item.input.get("workspace")?.as_u64()?;
    (value > 0).then_some(value as u32)
}

/// `fileRequest`: how a row's file opens: in a page of its own in the file pane, where the call's workspace has it.
fn file_request(item: &ToolItem<'_>, subject: &TitleSubject, path_base_dir: Option<&str>) -> Option<PathOpenRequest> {
    let path = subject.path.as_ref()?;
    let workspace = workspace_of(item);
    Some(PathOpenRequest {
        path: path.clone(),
        base_dir: if matches!(workspace, None | Some(1)) { path_base_dir.map(str::to_owned) } else { None },
        line: subject.line,
        workspace,
        new_page: true,
        ..PathOpenRequest::default()
    })
}

/// How long the pointer rests on a file name before the app looks for where it is.
const FILE_PREFETCH_DWELL: std::time::Duration = std::time::Duration::from_millis(60);

/// `ToolTitle`: a row's title with its subject set apart: a file is a link into the file pane, a pattern reads as code.
/// The link sits inside the row's disclosure, and being the deepest handler its click stops there.
fn tool_title(key: u64, subject: &TitleSubject, request: Option<PathOpenRequest>, color: Color) -> E {
    let t_ = theme();
    // Centred, not on a baseline: its pieces share one line height, and Taffy lays out a baseline row's children while it only sizes
    // the row. A frame laid out twice keeps what that wrote, which leaves the name inside the link at its min-content width, nothing.
    let mut title = row().items_center().min_w(0.0).shrink0().with_style(|s| s.flex_shrink = 1.0);
    // A run's width leaves out its trailing spaces, so the space that sets the subject apart is a margin.
    let piece = |s: &str| text(s.trim().to_owned()).ty(ty::ROW).color(color).nowrap().shrink0();
    if !subject.before.is_empty() {
        title = title.child(piece(&subject.before).when(subject.before.ends_with(char::is_whitespace), |e| e.mr(4.0)));
    }
    let icon_view = subject.path.as_deref().map(|path| div().mr(3.0).shrink0().self_center().child(mewrk_pane_files::icons::file_icon(path, 13.0)));
    if let Some(request) = request {
        let (prefetch, open) = (request.clone(), request);
        let path_tip = subject.path.clone().unwrap_or_default();
        let hover_key = key;
        title = title.child(
            row()
                .key(("tl-file", key))
                .items_center()
                .min_w(0.0)
                .radius(3.0)
                .hover_bg(t_.subtle_hover)
                .full_text(path_tip)
                .child(icon_view)
                .child(text(subject.text.clone()).key(("tl-file-name", key)).ty(ty::ROW).color(t_.accent_text).ellipsis().with_style(|s| s.flex_grow = 0.0))
                .on_hover_change(move |app: &mut App, on| {
                    let dwelling = use_state(("tl-file-dwell", hover_key), || None::<mewrk_client::timers::TimerId>);
                    if let Some(timer) = dwelling.borrow_mut().take() {
                        app.clear_timer(timer);
                    }
                    if on {
                        let request = prefetch.clone();
                        let timer = app.set_timer(FILE_PREFETCH_DWELL, move |app| mewrk_pane_files::path_open::prefetch_path(app, &request));
                        dwelling.set(Some(timer));
                    }
                })
                .on_click(move |app: &mut App| {
                    let mut request = open.clone();
                    request.anchor = rect_of(("tl-file", key)).map(|r| [r.x, r.y, r.right(), r.bottom()]);
                    mewrk_pane_files::path_open::open_path(app, &request);
                }),
        );
    } else if subject.code {
        title = title.child(row().px(4.0).radius(3.0).bg(t_.code_bg).min_w(0.0).child(text(subject.text.clone()).ty(ty::ROW_CODE).color(color).ellipsis().with_style(|s| s.flex_grow = 0.0)));
    } else {
        title = title.child(icon_view).child(text(subject.text.clone()).ty(ty::ROW).color(color).ellipsis().with_style(|s| s.flex_grow = 0.0));
    }
    if !subject.after.is_empty() {
        title = title.child(piece(&subject.after).when(subject.after.starts_with(char::is_whitespace), |e| e.ml(4.0)));
    }
    title
}

/// A change's added and removed lines, then what a search found; nothing for any other row.
fn tool_figures(presentation: &ToolPresentation) -> Option<E> {
    let t_ = theme();
    if presentation.counts.is_none() && presentation.note.is_none() {
        return None;
    }
    let mut figures = row().items_baseline().gap(4.0).shrink0();
    if let Some((additions, deletions)) = presentation.counts {
        // `−` is the typographic minus the review pane uses; a hyphen reads as a dash.
        figures = figures.child(text(format!("+{additions}")).ty(TypeStyle { weight: Weight::Semibold, ..ty::FIGURE }).color(t_.success).nowrap()).child(text(format!("−{deletions}")).ty(ty::FIGURE).color(t_.critical).nowrap());
    }
    if let Some(note) = &presentation.note {
        figures = figures.child(text(note.clone()).ty(ty::FIGURE).color(t_.text_tertiary).nowrap());
    }
    Some(figures)
}

/// Whether what a card's facts were derived from has changed. The output's address and length stand in for its text:
/// a card that changes gets a new string, and hashing a megabyte of output every frame is what this cache is here to avoid. The
/// rest can change under an output that does not: a child settling on a spawn's record, a rewritten argument with the output kept,
/// a descriptor that loaded after the row was first drawn and names an MCP tool in the user's words.
fn facts_signature(item: &ToolItem<'_>, explanation: Option<&str>, error: Option<&str>, descriptor: Option<&mewrk_client::model::ToolDescriptor>) -> u64 {
    hash_key((
        (item.result.output.as_ptr() as usize, item.result.output.len(), item.result.success, item.result.diff.as_ref().map_or(0, String::len)),
        (item.streaming, item.stream_status.map(crate::item::stream_status_name), item.live.map(|live| std::sync::Arc::as_ptr(live) as usize)),
        (item.subagent.map(|run| (run.status as u8, run.updates.len())), item.attestation, item.notice),
        descriptor.map(|descriptor| descriptor.label.as_str()),
        explanation,
        error,
        mewrk_client::i18n::is_english(),
    ))
}

fn tool_facts(f: &Frame<'_>, item: &ToolItem<'_>, whole: &ContextItem, descriptor: Option<&mewrk_client::model::ToolDescriptor>) -> Rc<ToolFacts> {
    let (explanation, error) = f.explanations.get(item.id).map_or((None, None), |(a, b)| (a.as_deref(), b.as_deref()));
    let signature = facts_signature(item, explanation, error, descriptor);
    let key = hash_id(item.id);
    if let Some((held, facts)) = f.shared.borrow().facts.get(&key)
        && *held == signature
    {
        return facts.clone();
    }
    let presentation = get_tool_presentation(item, descriptor, explanation, error);
    let live: Option<LiveState> = item.live.map(|live| LiveState::from(&**live));
    let task_ref = item.task_ref(live.as_ref());
    let status = tool_row_status(item, presentation.family, live.as_ref());
    let agent_route = (presentation.family == ToolViewFamily::AgentRun).then(|| agent_timeline_route_id(&task_ref));
    let facts = Rc::new(ToolFacts { tokens: estimate_context_tokens(whole), presentation, status, agent_route });
    let mut state = f.shared.borrow_mut();
    if state.facts.len() > 4096 {
        state.facts.clear();
    }
    state.facts.insert(key, (signature, facts.clone()));
    facts
}

fn tool_row(f: &Frame<'_>, entry: BlockEntry) -> E {
    let index = entry.index;
    let context = f.contexts.get(index);
    let item = context.tool().expect("a tool entry holds a tool card");
    let tools = f.props.tools.clone();
    let descriptor = tools.iter().find(|tool| tool.name == item.tool_name);
    let facts = tool_facts(f, &item, context.item, descriptor);
    let workflow_view = (entry.kind == EntryKind::Workflow).then(|| f.props.workflow_run_by_call.get(item.id).cloned()).flatten();
    let presentation = &facts.presentation;
    let transient = item.streaming;
    let editing = f.editing_context_id == Some(item.id);
    let expand_key = hash_id(item.id);
    // Every row starts closed, a failed one included: its line already turns red, and a body that opened on its own would
    // push the rest of the round down the page in the middle of reading it.
    let expanded = f.expanded(expand_key, false);
    // An open editor owns the body, and the row stays open when the editor goes.
    if editing && !expanded {
        f.shared.borrow_mut().expanded.insert(expand_key, true);
    }
    let agent_route = facts.agent_route.as_deref();
    let subagent_route = f.props.on_open_subagent.as_ref().and(agent_route);
    let read_only_surface = f.read_only;
    // A run of its own is read in its own panel, not in a drawer under the line. Those rows carry no disclosure at all.
    let on_open_panel: Option<Handler<App>> = if editing {
        None
    } else if let (Some(view), Some(open), false) = (&workflow_view, &f.props.on_open_workflow_run, read_only_surface) {
        let (open, id) = (open.clone(), view.id.clone());
        Some(Rc::new(move |app| open(app, &id)))
    } else if let (Some(route), Some(open)) = (subagent_route, &f.props.on_open_subagent) {
        let (open, id) = (open.clone(), route.to_owned());
        Some(Rc::new(move |app| open(app, &id)))
    } else {
        None
    };
    let expandable = on_open_panel.is_none() && (!transient || item.stream_status != Some(StreamingToolPhase::Announced));
    let open = expanded || editing;
    // Most rows say what happened, in words. A block can hold several runs that would all read "ran a subagent", so those
    // take the name that addresses the run; a note carries no subject beyond what it says, so it takes its own text.
    let name = if let (EntryKind::Workflow, Some(view)) = (entry.kind, &workflow_view) {
        view.name.clone()
    } else if let Some(route) = agent_route.filter(|route| *route != item.id) {
        route.to_owned()
    } else if presentation.family == ToolViewFamily::AgentNote && presentation.target.as_deref().is_some_and(|target| !target.is_empty()) {
        presentation.target.clone().unwrap_or_default()
    } else {
        presentation.title.clone()
    };
    let named = presentation.subject.is_some() && name == presentation.title;
    let token = token_stat(facts.tokens);
    let stat = match &workflow_view {
        Some(view) => {
            let agents = tf("{count} 个代理", "{count} agents", &[("count", &view.step_count)]);
            Some(token.map_or(agents.clone(), |token| format!("{agents} · {token}")))
        }
        None => token,
    };
    let row_status = facts.status.clone();
    let name_color = match row_status.tone {
        RowTone::Error => theme().critical,
        RowTone::Halted => theme().caution,
        _ => theme().text_secondary,
    };
    let label = if named { presentation.subject.as_ref().map(|subject| tool_title(hash_key((f.salt, item.id)), subject, file_request(&item, subject, f.props.path_base_dir.as_deref()), name_color)) } else { None };
    let accessible_name = [Some(tool_row_name(&item, descriptor)), presentation.target.clone(), Some(row_status.label.clone())].into_iter().flatten().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" · ");

    // Preview tools and those of a remote-control screen are calls on a live page or device, not something a timeline can
    // re-attest; a card carrying a child transcript the host refuses to re-attest; a timeline with no way to commit a
    // rewritten card withholds the pencil for the same reason.
    let allow_edit = !is_preview_tool_name(item.tool_name) && item.subagent.is_none() && (f.props.on_save_tool_edit.is_some() || f.props.on_save_tool.is_some());
    let actions = if !f.mutation_read_only && !transient && !editing {
        Some(row_actions(
            f,
            item.id,
            allow_edit,
            tf("编辑工具调用 {title}", "Edit tool call {title}", &[("title", &presentation.title)]),
            tf("删除工具调用 {title}", "Delete tool call {title}", &[("title", &presentation.title)]),
        ))
    } else {
        None
    };

    let mut actions_pinned = false;
    let mut pinned_actions = None;
    let body: Option<E> = if !open {
        None
    } else if let (true, Some(on_cancel), true) = (editing, f.props.on_cancel_edit.clone(), f.props.on_save_tool_edit.is_some() || f.props.on_save_tool.is_some()) {
        let parts = inline_tool_editor_parts(("tl-tool-edit", f.salt, item.id), ToolEditorProps { item: context.item.clone(), descriptor: descriptor.cloned(), inserting: false, on_cancel, on_run: f.props.on_save_tool.clone(), on_save: f.props.on_save_tool_edit.clone() });
        actions_pinned = true;
        pinned_actions = Some(parts.actions);
        Some(parts.body)
    } else if let Some(view) = &workflow_view {
        // Reached only where the row owns no panel to send the click to, so the body states the run instead.
        Some(workflow_run_detail(("tl-workflow", item.id), view))
    } else {
        Some(tool_detail_renderer(item.id, &item, descriptor))
    };
    let shared = f.shared.clone();
    let on_toggle: Handler<App> = Rc::new(move |_| {
        if editing {
            return;
        }
        let mut state = shared.borrow_mut();
        let now = state.expanded.get(&expand_key).copied().unwrap_or(false);
        state.expanded.insert(expand_key, !now);
        repaint();
    });
    timeline_row(
        f,
        RowSpec {
            context_id: item.id.to_owned(),
            index,
            icon: presentation.icon,
            name,
            label,
            figures: tool_figures(presentation),
            line: None,
            line_streaming: false,
            stat,
            status: Some(row_status),
            accessible_name,
            badge: None,
            actions: if actions_pinned { pinned_actions } else { actions },
            actions_pinned,
            expandable,
            expanded: open,
            on_toggle: Some(on_toggle),
            on_open_panel,
            read_only: f.mutation_read_only,
            on_open_insert: Some(f.open_insert.clone()),
            body,
        },
    )
}

/// The pencil and the bin of a row, delete always last.
fn row_actions(f: &Frame<'_>, id: &str, allow_edit: bool, edit_label: String, delete_label: String) -> E {
    let mut bar = row().items_center().gap(1.0);
    if allow_edit && let Some(on_edit) = f.props.on_edit.clone() {
        let (source, id_) = (f.props.clone(), id.to_owned());
        bar = bar.child(action_button(("tl-edit", f.salt, id), "Pencil", edit_label, move |app| {
            if let Some(item) = find_item(app, &source, &id_) {
                on_edit(app, &item);
            }
        }));
    }
    let on_delete = f.props.on_delete.clone();
    let (source, id_) = (f.props.clone(), id.to_owned());
    bar.child(action_button(("tl-delete", f.salt, id), "Trash2", delete_label, move |app| {
        if let (Some(on_delete), Some(item)) = (&on_delete, find_item(app, &source, &id_)) {
            on_delete(app, &item);
        }
    }))
}

/// A handler is told about a whole context, read at click time: copying every visible card every frame for a click that
/// mostly never comes would be the most expensive thing in the frame.
pub fn find_item(app: &mut App, props: &crate::props::ConversationViewProps, id: &str) -> Option<ContextItem> {
    if let Some(conversation) = &props.conversation_id {
        if mewrk_client::drafts::is_draft_conversation_id(conversation) {
            return app.timeline_contexts_of(conversation).into_iter().find(|item| item.id() == id);
        }
        if let Some(found) = app.conversation(conversation).and_then(|conversation| conversation.contexts.iter().find(|item| item.id() == id)).cloned() {
            return Some(found);
        }
    }
    props.contexts.iter().find(|context| context.id() == id).map(|context| context.item.clone())
}

fn reasoning_row(f: &Frame<'_>, entry: BlockEntry) -> E {
    let index = entry.index;
    let context = f.contexts.get(index);
    let item = context.reasoning().expect("a reasoning entry holds a reasoning card");
    let content = item.content;
    let streaming = item.streaming;
    // Prefers the card's `form`, falling back only to the legacy empty-body heuristic.
    let encrypted = is_encrypted_reasoning(item.form, Some(content));
    let editing = f.editing_context_id == Some(item.id);
    let editable = !f.mutation_read_only && !streaming;
    let expand_key = hash_id(item.id);
    // The appearance preference decides, streaming or not, until the reader says otherwise.
    let expanded = f.expanded(expand_key, !f.collapse_reasoning);
    if editing && !expanded {
        f.shared.borrow_mut().expanded.insert(expand_key, true);
    }
    let expandable = !content.is_empty() || editing;
    let open = expandable && (expanded || editing);
    let token_text = item.tokens.filter(|tokens| *tokens > 0).map(|tokens| format!("{} tokens", format_compact_token_count(tokens)));
    // A provider that reports no reasoning-token usage leaves the row without a figure of its own, so the shared estimate stands in.
    let stat = token_text.clone().or_else(|| token_stat(estimate_context_tokens(context.item)));
    let line = reasoning_line(content, streaming);
    let has_line = line.is_some();

    let mut actions_bar = None;
    let mut actions_pinned = false;
    let body = if !open {
        None
    } else if let (true, Some(on_cancel), Some(on_save)) = (editing, f.props.on_cancel_edit.clone(), f.props.on_save_text.clone()) {
        let parts = inline_text_editor_parts(("tl-reasoning-edit", f.salt, item.id), TextEditorProps { kind: InsertKind::Reasoning, content: content.to_owned(), images: Vec::new(), files: Vec::new(), show_kind: false, image_input: None, on_cancel, on_save, on_add_attachments: None });
        actions_bar = Some(parts.bar);
        actions_pinned = true;
        let note = item.replay.map(|_| {
            // The provider's signed payload is what those protocols replay, so what is typed here never reaches them.
            row().ml(13.0).mb(6.0).gap(4.0).items_center().wrap().child(text(t("带签名的思考：部分协议会忽略编辑。", "Signed reasoning: some protocols ignore edits.")).ty(ty::NOTE).color(theme().text_tertiary)).child(docs_link("context", "edit"))
        });
        Some(col().child(note).child(parts.body))
    } else {
        Some(prose(f, ("tl-reasoning", item.id), content, streaming))
    };
    let actions = if actions_pinned {
        actions_bar
    } else if editing || (f.mutation_read_only && (streaming || content.is_empty())) {
        None
    } else {
        // Copying changes nothing, so a read-only or running timeline keeps it; a thought still being written has no settled text to copy yet.
        let mut bar = row().items_center().gap(1.0);
        if !streaming && !content.is_empty() {
            bar = bar.child(copy_button(("tl-copy", f.salt, item.id), content, t("复制思考过程", "Copy reasoning"), false));
        }
        if !f.mutation_read_only {
            // Encrypted reasoning cannot be edited: its body never reached the client, so anything typed here would be
            // fabricated history the next round is asked to believe.
            bar = bar.child(row_actions(f, item.id, editable && !encrypted, t("编辑上下文", "Edit context").to_owned(), t("删除上下文", "Delete context").to_owned()));
        }
        Some(bar)
    };
    let shared = f.shared.clone();
    let default_open = !f.collapse_reasoning;
    let on_toggle: Handler<App> = Rc::new(move |_| {
        if editing {
            return;
        }
        let mut state = shared.borrow_mut();
        let now = state.expanded.get(&expand_key).copied().unwrap_or(default_open);
        state.expanded.insert(expand_key, !now);
        repaint();
    });
    timeline_row(
        f,
        RowSpec {
            context_id: item.id.to_owned(),
            index,
            icon: "BrainCircuit",
            name: "think".to_owned(),
            label: None,
            figures: None,
            // A record with no body to quote has its token count as the whole of what it can say for itself.
            line: line.or_else(|| token_text.clone()),
            line_streaming: streaming && has_line,
            stat: if has_line { stat } else { None },
            status: streaming.then(|| RowStatus { tone: RowTone::Running, label: t("正在思考", "Thinking").to_owned(), spinner: true }),
            accessible_name: if streaming {
                t("正在思考", "Thinking")
            } else if encrypted {
                t("加密思考", "Encrypted reasoning")
            } else {
                t("思考过程", "Reasoning")
            }
            .to_owned(),
            badge: item.interrupted.then(interrupted_badge),
            actions,
            actions_pinned,
            expandable,
            expanded: open,
            on_toggle: Some(on_toggle),
            on_open_panel: None,
            read_only: f.mutation_read_only,
            on_open_insert: Some(f.open_insert.clone()),
            body,
        },
    )
}

/// `.timeline-row__prose`.
pub fn prose(f: &Frame<'_>, key: impl std::hash::Hash, content: &str, streaming: bool) -> E {
    let t_ = theme();
    col().pt(2.0).pb(4.0).pl(13.0).border_left(t_.divider).min_w(0.0).when(streaming, |e| e.aria_live(Live::Polite)).child(markdown_content(
        key,
        content,
        MarkdownOptions { streaming, linkify_paths: true, render_html: true, path_base_dir: f.props.path_base_dir.clone(), single_dollar_math: f.single_dollar_math, ty: ty::PROSE, on_open_path: Some(open_path_handler()), ..MarkdownOptions::default() },
    ))
}

pub fn open_path_handler() -> Rc<dyn Fn(&mut App, mewrk_ui_common::markdown::PathLink)> {
    Rc::new(|app, link| {
        let request = PathOpenRequest { path: link.path, base_dir: link.base_dir, line: link.line, anchor: Some([link.anchor.x, link.anchor.y, link.anchor.right(), link.anchor.bottom()]), ..PathOpenRequest::default() };
        mewrk_pane_files::path_open::open_path(app, &request);
    })
}

fn hook_row(f: &Frame<'_>, entry: BlockEntry) -> E {
    let t_ = theme();
    let index = entry.index;
    let context = f.contexts.get(index);
    let item = context.system().expect("a hook entry holds a system card");
    let execution = item.hook_execution.expect("a hook entry has its execution");
    let row_status = hook_row_status(&execution.status);
    let content = item.content;
    let expand_key = hash_id(item.id);
    let expandable = !content.is_empty();
    let open = expandable && f.expanded(expand_key, false);
    let shared = f.shared.clone();
    let on_toggle: Handler<App> = Rc::new(move |_| {
        let mut state = shared.borrow_mut();
        let now = state.expanded.get(&expand_key).copied().unwrap_or(false);
        state.expanded.insert(expand_key, !now);
        repaint();
    });
    let running = execution.status == "running";
    let body = open.then(|| col().key(("tl-hook-output", f.salt, item.id)).max_h(240.0).scroll_y().pt(2.0).pb(4.0).pl(13.0).border_left(t_.divider).when(running, |e| e.aria_live(Live::Polite)).child(text(content.to_owned()).ty(ty::OUTPUT).color(t_.text_secondary).selectable()));
    timeline_row(
        f,
        RowSpec {
            context_id: item.id.to_owned(),
            index,
            icon: "Webhook",
            name: if execution.hook_name.is_empty() { execution.event.clone() } else { execution.hook_name.clone() },
            label: None,
            figures: None,
            line: None,
            line_streaming: false,
            stat: token_stat(estimate_context_tokens(context.item)),
            accessible_name: tf("{event} 钩子 · {status}", "{event} hook · {status}", &[("event", &execution.event), ("status", &row_status.label)]),
            status: Some(row_status),
            badge: execution.context_injected.then(|| {
                badge(
                    t("已加入模型上下文", "Added to model context"),
                    t(
                        "钩子补充的上下文以宿主通知的形式在当前工具结果之后送达模型（“送达了钩子补充的上下文”）；这条记录本身只留在本机",
                        "The hook's additional context reaches the model as a host notice after the current tool results (\u{201c}Delivered context from a hook\u{201d}); this record itself stays on this machine",
                    ),
                )
            }),
            actions: None,
            actions_pinned: false,
            expandable,
            expanded: open,
            on_toggle: Some(on_toggle),
            on_open_panel: None,
            read_only: f.mutation_read_only,
            on_open_insert: Some(f.open_insert.clone()),
            body,
        },
    )
}

/// `summarizeBlockKinds` over a block's entries: its name for assistive tech, which reads the rows as a list.
pub fn block_summary(f: &Frame<'_>, entries: &[BlockEntry]) -> String {
    let kinds: Vec<SummaryKind> = entries
        .iter()
        .map(|entry| match entry.kind {
            EntryKind::Reasoning => SummaryKind::Reasoning,
            EntryKind::Hook => SummaryKind::Hook,
            _ => f.contexts.get(entry.index).tool().map_or(SummaryKind::Other, |item| tool_summary_kind(&item)),
        })
        .collect();
    summarize_block_kinds(&kinds)
}

/// One uninterrupted stretch of work. There is no heading over the rows and no way to fold them away: each row is one line
/// already, so a block is as tall as the work it records.
pub fn timeline_block(f: &Frame<'_>, entries: &[BlockEntry], insertion_index: Option<usize>) -> E {
    timeline_rows(f, entries, insertion_index, true)
}

/// A block, or with no padding the block a row lifted out of one rides in.
pub fn timeline_rows(f: &Frame<'_>, entries: &[BlockEntry], insertion_index: Option<usize>, padded: bool) -> E {
    let t_ = theme();
    let first = entries.iter().map(|entry| entry.index).min().unwrap_or(0);
    let after = entries.iter().map(|entry| entry.index).max().unwrap_or(0) + 1;
    let first_id = f.contexts.get(first).id();
    let mut block = col().key(("tl-block", f.salt, first_id)).w_full().min_w(0.0).when(a11y_active(), |e| e.role(Role::List).aria_label(block_summary(f, entries)));
    if padded {
        block = block.pt(BLOCK_PAD_TOP).pb(BLOCK_PAD_BOTTOM);
    }
    if !f.mutation_read_only {
        let open = f.open_insert.clone();
        let keyboard = f.open_insert.clone();
        let (salt, block_id) = (f.salt, first_id.to_owned());
        block = block
            .on_context_menu(move |app, event| {
                let at = if event.y > event.rect.y + event.rect.h * 0.5 { after } else { first };
                open(app, event, at);
            })
            .on_key(move |app, press| {
                if press.shift && press.key == Key::F(10)
                    && let Some(r) = rect_of(("tl-block", salt, block_id.as_str()))
                {
                    keyboard(app, PointerEvent { phase: Phase::Down, x: r.x + 32.0, y: r.bottom(), dx: 0.0, dy: 0.0, rect: r }, after);
                    return true;
                }
                false
            });
    }
    for entry in entries {
        let context: Item<'_> = f.contexts.get(entry.index);
        if f.lifted.contains(context.id()) {
            continue;
        }
        // Copies docked here take room among the rows; they are drawn over it.
        if let Some((at, height)) = f.dock_inside
            && at == entry.index
        {
            block = block.child(div().h(height).shrink0());
        }
        if !f.mutation_read_only && insertion_index == Some(entry.index) {
            block = block.child(insertion_line(&t_));
        }
        block = block.child(match entry.kind {
            EntryKind::Reasoning => reasoning_row(f, *entry),
            EntryKind::Hook => hook_row(f, *entry),
            _ => tool_row(f, *entry),
        });
    }
    block
}

/// `.insertion-line`: no height of its own, so drawing it moves nothing.
pub fn insertion_line(t_: &Theme) -> E {
    div().h(0.0).w_full().shrink0().raised().child(div().w_full().h(2.0).bg(t_.accent).absolute().top(-1.0).left(0.0).right(0.0))
}
