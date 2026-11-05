//! `components/SubagentPanel.tsx` and `StreamedSubagentPanel` from `StreamedConversationView.tsx`.

use std::collections::HashMap;

use mewrk_client::model::SubagentRunRecord;
use mewrk_client::prelude::*;

use fluent::Weight;

use crate::context::{Ctx, SubagentViewStatus};
use crate::subagents::*;
use crate::widgets::*;
use crate::workflow_panel::workflow_step_record;

fn status_meta(status: SubagentViewStatus) -> (&'static str, &'static str) {
    use SubagentViewStatus::*;
    match status {
        Running => (t("工作中", "Working"), "LoaderCircle"),
        Completed => (t("已完成", "Completed"), "CircleCheck"),
        Interrupted => (t("已中断", "Interrupted"), "CircleSlash"),
        Failed => (t("已失败", "Failed"), "CircleX"),
        Stopped => (t("已停止", "Stopped"), "CircleSlash"),
        RoundLimit => (t("已达轮次上限", "Round limit reached"), "CircleAlert"),
    }
}

/// `(ink, wash)` of a status. A deliberate stop reads muted; a ceiling is a caution, not a failure.
fn status_colors(status: SubagentViewStatus) -> (Color, Color) {
    use SubagentViewStatus::*;
    let c = theme();
    match status {
        Running => (c.accent_text, c.attention_bg),
        Completed => (c.success, c.success_bg),
        Interrupted | Stopped => (c.text_secondary, c.card_secondary),
        Failed => (c.critical, c.critical_bg),
        RoundLimit => (c.caution, c.caution_bg),
    }
}

fn status_icon(status: SubagentViewStatus, size: f32, color: Color) -> E {
    if status == SubagentViewStatus::Running {
        return spinner(size, color);
    }
    icon(status_meta(status).1, size).color(color)
}

pub fn subagent_status_badge(status: SubagentViewStatus) -> E {
    let (ink, wash) = status_colors(status);
    row().items_center().gap(4.0).py(2.0).px(6.0).radius(999.0).bg(wash).shrink0().child(status_icon(status, 12.0, ink)).child(text(status_meta(status).0).ty(ts(8.5, 12.0, Weight::Semibold)).color(ink).nowrap())
}

fn tone_color(tone: SubagentAvatarTone) -> Color {
    // Colours stay Fluent, but six agents still have to read as six: one hue per tone, as upstream's avatars have.
    let dark = theme().dark;
    let pick = |light: u32, night: u32| Color::rgb(if dark { night } else { light });
    match tone {
        SubagentAvatarTone::Amber => pick(0xC77A1F, 0xE3A65B),
        SubagentAvatarTone::Green => pick(0x287A38, 0x6CCB5F),
        SubagentAvatarTone::Mint => pick(0x279070, 0x4FC9A8),
        SubagentAvatarTone::Blue => pick(0x3769BB, 0x6CB6FF),
        SubagentAvatarTone::Violet => pick(0x6254AA, 0xB4A7F0),
        SubagentAvatarTone::Rose => pick(0xAA4F6A, 0xF08FAA),
    }
}

fn subagent_avatar(agent: &SubagentView, compact: bool) -> E {
    let (box_size, glyph) = if compact { (20.0, 12.0) } else { (30.0, 15.0) };
    row().center().size(box_size, box_size).shrink0().tooltip(agent.label.clone()).aria_hidden(true).child(icon("Bot", glyph).color(tone_color(subagent_avatar_tone(&agent.id))))
}

/// The subagent pane's title bar: a tab per agent whose transcript was opened, each marked with how its agent is doing.
/// A tab is only a view: closing one stops nothing.
pub fn subagent_tab_bar(agents: &[Rc<SubagentView>], active_id: Option<&str>, on_select: impl Fn(&mut App, &str) + 'static, on_close: impl Fn(&mut App, &str) + 'static, on_reorder: impl Fn(&mut App, Vec<String>) + 'static) -> E {
    let c = theme();
    let tabs = agents
        .iter()
        .map(|agent| {
            let (status, _) = status_meta(agent.status);
            let ink = match agent.status {
                SubagentViewStatus::Running => c.accent_text,
                SubagentViewStatus::Failed => c.critical,
                SubagentViewStatus::RoundLimit => c.caution,
                _ => c.text_secondary,
            };
            PageTab { id: agent.id.clone(), label: agent.label.clone(), title: Some(format!("{} · {status}", agent.label)), icon: Some(status_icon(agent.status, 12.0, ink)) }
        })
        .collect();
    page_tabs("subagents", tabs, active_id, Rc::new(on_select), PageTabsOptions { aria_label: t("子代理标签", "Subagent tabs"), more_label: t("更多子代理", "More subagents"), on_close: Some(Rc::new(on_close)), close_on_delete_key: true, on_reorder: Some(Rc::new(on_reorder)) })
}

/// Parent link and direct children of one agent, so a search group reads as a tree. A step's parent is a run, which is
/// a script with no transcript to go back to.
fn subagent_group_nav(agent: &SubagentView, agents: &[Rc<SubagentView>], on_select: Rc<dyn Fn(&mut App, &str)>) -> Option<E> {
    let c = theme();
    let parent = agent.parent_id.as_ref().and_then(|id| agents.iter().find(|v| &v.id == id)).filter(|o| !o.workflow_run);
    let children: Vec<&Rc<SubagentView>> = agent.child_ids.iter().filter_map(|id| agents.iter().find(|v| &v.id == id)).collect();
    if parent.is_none() && children.is_empty() {
        return None;
    }
    let chip = |key: &str, id: &str, tip: String| {
        let (select, id) = (on_select.clone(), id.to_owned());
        row().key(("subagent-nav", key.to_owned())).items_center().gap(5.0).py(3.0).px(8.0).radius(999.0).bg(c.card).border(c.card_stroke).hover_bg(c.subtle_hover).min_w(0.0).tooltip(tip.clone()).aria_label(tip).on_click(move |app| select(app, &id))
    };
    let mut nav = row().wrap().items_center().gap(5.0).shrink0().pt(6.0).pb(6.0).pl(13.0).pr(9.0).border_bottom(c.divider).bg(c.card_secondary).role(Role::Navigation).aria_label(t("子代理组结构", "Subagent group structure"));
    if let Some(parent) = parent {
        nav = nav.child(chip(&format!("parent:{}", parent.id), &parent.id, tf("返回上级子代理 {label}", "Back to parent subagent {label}", &[("label", &parent.label)])).child(icon("ChevronLeft", 12.0).color(c.text_secondary)).child(text(parent.label.clone()).ty(ts(10.5, 15.0, Weight::Regular)).color(c.text_secondary).ellipsis()));
    }
    for child in children {
        let ink = if child.status == SubagentViewStatus::Running { c.accent_text } else { c.text_secondary };
        nav = nav.child(chip(&format!("child:{}", child.id), &child.id, tf("打开子代理 {label}", "Open subagent {label}", &[("label", &child.label)])).child(subagent_avatar(child, true)).child(text(child.label.clone()).ty(ts(10.5, 15.0, Weight::Regular)).color(ink).ellipsis()).child(subagent_status_badge(child.status)));
    }
    Some(nav)
}

/// What the transcript element needs. Drawn by the chat agent's timeline, with `editable: false`.
pub struct TranscriptArgs<'a> {
    pub contexts: &'a [Ctx],
    pub streaming: bool,
    pub path_base_dir: Option<&'a str>,
    pub aria_label: String,
}

/// TODO(chat): the shell passes the timeline here. `subagentChangeSpans` is that side's too, computed from `contexts`.
pub type TranscriptView<'a> = &'a dyn Fn(&mut App, TranscriptArgs<'_>) -> E;

pub struct SubagentPanelProps<'a> {
    pub agent: Rc<SubagentView>,
    /// Whole flat agent tree, so a search-group page can walk to its neighbours.
    pub agents: Vec<Rc<SubagentView>>,
    pub on_select_agent: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// Leaves the transcript and returns the main area to the conversation.
    pub on_close: Option<Rc<dyn Fn(&mut App)>>,
    /// Drops the panel's own header, for a host that already draws the title and status.
    pub chromeless: bool,
    pub path_base_dir: Option<String>,
    pub transcript: TranscriptView<'a>,
}

pub fn subagent_panel(app: &mut App, p: SubagentPanelProps<'_>) -> E {
    let c = theme();
    let agent = &p.agent;
    let mut section = col().key(("subagent-panel", agent.id.as_str())).w_full().h_full().min_w(0.0).clip().role(Role::Region);
    if !p.chromeless {
        section = section.aria_labelledby(("subagent-title", agent.id.as_str()));
        let lead: E = match p.on_close.clone() {
            Some(close) => row().key("subagent-back").center().size(31.0, 31.0).radius(7.0).shrink0().hover_bg(c.subtle_hover).tooltip(t("返回对话", "Back to the conversation")).on_click(move |app| close(app)).child(icon("ArrowLeft", 16.0).color(c.text_secondary)),
            None => icon("Bot", 17.0).color(c.text_secondary),
        };
        section = section.child(
            row()
                .items_center()
                .gap(9.0)
                .h(56.0)
                .shrink0()
                .pt(7.0)
                .pb(6.0)
                .pl(13.0)
                .pr(9.0)
                .border_bottom(c.divider)
                .child(lead)
                .child(col().flex1().child(text(t("只读终端", "Read-only terminal")).ty(ts(8.5, 12.0, Weight::Regular)).color(c.text_tertiary).nowrap()).child(text(agent.label.clone()).key(("subagent-title", agent.id.as_str())).ty(ts(13.0, 19.0, Weight::Semibold)).ellipsis()))
                .child(subagent_status_badge(agent.status)),
        );
    }
    if let Some(on_select) = p.on_select_agent.clone()
        && let Some(nav) = subagent_group_nav(agent, &p.agents, on_select)
    {
        section = section.child(nav);
    }
    let body = (p.transcript)(
        app,
        TranscriptArgs {
            contexts: &agent.contexts,
            streaming: agent.status == SubagentViewStatus::Running,
            path_base_dir: p.path_base_dir.as_deref(),
            aria_label: tf("{label}只读终端", "{label} read-only terminal", &[("label", &agent.label)]),
        },
    );
    section.child(row().flex1().clip().child(body))
}

/// Loaded workflow step bodies, keyed `conversation/run/index`. `None` is a read that found nothing, cached so it is
/// not asked for again.
#[derive(Default)]
pub struct ExternalStepBodies {
    pub bodies: HashMap<String, Option<SubagentRunRecord>>,
    loading: Vec<String>,
}

pub fn external_body_key(conversation_id: &str, r: &ExternalStepBodyRef) -> String {
    format!("{conversation_id}/{}/{}", r.run_id, r.step_index)
}

/// The effect in `App.tsx`: when the open view is an externalized step, fetch its body once. Feed it the tree the views
/// were derived from, already grafted, so a step that has its body yields no coordinates and starts no second read.
pub fn ensure_external_step_body(app: &mut App, conversation_id: &str, contexts: &[Ctx], view: &SubagentView) {
    if view.kind != mewrk_client::model::SubagentRunKind::WorkflowStep {
        return;
    }
    let Some(reference) = find_external_step_body_ref(contexts, &view.call_ids) else { return };
    let key = external_body_key(conversation_id, &reference);
    let store = app.local::<ExternalStepBodies>();
    if store.bodies.contains_key(&key) || store.loading.contains(&key) {
        return;
    }
    store.loading.push(key.clone());
    workflow_step_record(app, conversation_id, &reference.run_id, reference.step_index, move |app, result| {
        let store = app.local::<ExternalStepBodies>();
        store.loading.retain(|k| *k != key);
        // A failed read stays unanswered, so opening the step again asks again.
        if let Ok(record) = result {
            store.bodies.insert(key, record);
        }
        repaint();
    });
}

/// `useRenderedContexts`: the conversation's contexts with the active run's stream merged in, and the revision that says
/// when they changed. `persisted_revision` is whatever the shell bumps when `persisted` changes.
pub fn rendered_contexts(app: &mut App, conversation_id: &str, persisted: &[mewrk_client::model::ContextItem], persisted_revision: u64) -> (Vec<Ctx>, u64) {
    match app.run_view(conversation_id) {
        Some(run) => (run.rendered_contexts(persisted).map(Ctx::from).collect(), persisted_revision.wrapping_mul(1 << 20).wrapping_add(run.revision)),
        None => (persisted.iter().cloned().map(Ctx::from).collect(), persisted_revision.wrapping_mul(1 << 20)),
    }
}

pub struct StreamedSubagentPanelProps<'a> {
    pub conversation_id: String,
    /// See [`rendered_contexts`].
    pub rendered_contexts: &'a [Ctx],
    /// Bumped whenever `rendered_contexts` changes, which is what keeps a full re-derivation off every frame.
    pub revision: u64,
    pub selected_subagent_id: Option<String>,
    pub on_select_agent: Rc<dyn Fn(&mut App, &str)>,
    pub on_close: Rc<dyn Fn(&mut App)>,
    pub chromeless: bool,
    pub path_base_dir: Option<String>,
    /// A confirmation card that must stay visible and answerable on this page.
    pub dock: Option<E>,
    pub transcript: TranscriptView<'a>,
}

struct DerivedViews {
    revision: Option<(u64, usize)>,
    views: Vec<Rc<SubagentView>>,
}

/// The subagent drawer: the rendered timeline with every loaded step body grafted back, derived into complete views.
pub fn streamed_subagent_panel(app: &mut App, p: StreamedSubagentPanelProps<'_>) -> Option<E> {
    let memo = use_state(("streamed-subagent-views", p.conversation_id.as_str()), || DerivedViews { revision: None, views: Vec::new() });
    let bodies_seen = app.local::<ExternalStepBodies>().bodies.len();
    if memo.borrow().revision != Some((p.revision, bodies_seen)) {
        let conversation = p.conversation_id.clone();
        let bodies = std::mem::take(&mut app.local::<ExternalStepBodies>().bodies);
        let source = graft_external_step_bodies(p.rendered_contexts, &|r| bodies.get(&external_body_key(&conversation, r)).cloned().flatten());
        let views = derive_subagent_views(&source, &subagent_view_messages());
        app.local::<ExternalStepBodies>().bodies = bodies;
        let mut m = memo.borrow_mut();
        m.views = views;
        m.revision = Some((p.revision, bodies_seen));
    }
    let views = memo.borrow().views.clone();
    let agent = find_openable_subagent_view(&views, p.selected_subagent_id.as_deref())?.clone();
    ensure_external_step_body(app, &p.conversation_id, p.rendered_contexts, &agent);
    let panel = subagent_panel(
        app,
        SubagentPanelProps { agent, agents: views, on_select_agent: Some(p.on_select_agent), on_close: Some(p.on_close), chromeless: p.chromeless, path_base_dir: p.path_base_dir, transcript: p.transcript },
    );
    Some(col().flex1().min_w(0.0).child(panel).child(p.dock))
}
