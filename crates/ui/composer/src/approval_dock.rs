//! The card a tool or a plan waits on: upstream's `ToolApprovalDock.tsx`.

use mewrk_client::prelude::*;
use mewrk_client::runs::{PendingToolPrompt, PromptKind, ToolPromptDecision};
use mewrk_ui_common::popover_menu::probe::say;
use std::collections::HashMap;
use std::time::Instant;

/// Position of the shown card inside its conversation's pending stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DockStack {
    /// 0-based index of the card currently shown.
    pub index: usize,
    /// Pending cards in this conversation, the shown one included.
    pub total: usize,
}

#[derive(Default)]
struct CardState {
    decided: Option<ToolPromptDecision>,
    feedback: String,
}

/// One card answers once. Kept per prompt: a new prompt id arrives with its own fresh decision state, never inheriting the
/// previous card's spent guard.
#[derive(Default)]
struct Cards {
    states: HashMap<String, CardState>,
    /// The card up now and when it came. One flipped to has just arrived, as a keyed remount has upstream: focus goes to its
    /// default action, and it slides in.
    shown: Option<(String, Instant)>,
}

pub fn allow_key(prompt_id: &str) -> u64 {
    hash_key(("approval-allow", prompt_id))
}

pub fn always_key(prompt_id: &str) -> u64 {
    hash_key(("approval-always", prompt_id))
}

pub fn deny_key(prompt_id: &str) -> u64 {
    hash_key(("approval-deny", prompt_id))
}

/// What the card's reply box is keyed by.
pub struct FeedbackField(u64);

impl FeedbackField {
    /// The element's key, for finding its rectangle.
    pub fn field_id(&self) -> u64 {
        hash_key(("field", self.0))
    }
}

impl std::hash::Hash for FeedbackField {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

pub fn feedback_field_key(prompt_id: &str) -> FeedbackField {
    FeedbackField(hash_key(("approval-feedback", prompt_id)))
}

fn title_key(prompt_id: &str) -> u64 {
    hash_key(("approval-title", prompt_id))
}

pub fn pager_key(previous: bool) -> u64 {
    hash_key(("dock-pager", previous))
}

/// `decide`: spends the card's guard, so a double click can't spend the prompt id twice.
fn decide(app: &mut App, conversation_id: &str, prompt_id: &str, decision: ToolPromptDecision, feedback: Option<&str>) {
    let card = app.local::<Cards>().states.entry(prompt_id.to_owned()).or_default();
    if card.decided.is_some() {
        return;
    }
    card.decided = Some(decision);
    app.decide_tool_prompt(conversation_id, prompt_id, decision, feedback);
}

pub fn status_text(plan_exit: bool, decided: Option<ToolPromptDecision>) -> Option<&'static str> {
    let decided = decided?;
    Some(if plan_exit {
        if decided == ToolPromptDecision::Deny { t("已发送意见，模型会修改计划后再来请你批准", "Feedback sent; the model will revise the plan and ask again") } else { t("已批准，模型开始实施", "Approved; the model is implementing it") }
    } else {
        match decided {
            ToolPromptDecision::Deny => t("已拒绝", "Denied"),
            ToolPromptDecision::AllowAlways => t("本对话内的同类调用将不再询问", "Calls of this kind will no longer be asked in this conversation"),
            ToolPromptDecision::AllowOnce => t("已允许", "Allowed"),
        }
    })
}

/// The stack's pager: draws only for two or more cards.
pub(crate) fn pager(conversation_id: &str, stack: Option<DockStack>, previous_label: &'static str, next_label: &'static str) -> Option<E> {
    let stack = stack.filter(|stack| stack.total > 1)?;
    let t_ = theme();
    let count = format!("{}/{}", stack.index + 1, stack.total);
    say(&count);
    say(previous_label);
    say(next_label);
    let step = |previous: bool, name: &'static str, label: &'static str, enabled: bool| -> E {
        let delta: isize = if previous { -1 } else { 1 };
        let conversation = conversation_id.to_owned();
        let button: E = row().key(pager_key(previous)).center().size(18.0, 18.0).radius(4.0).tooltip(label).child(icon(name, 12.0).color(if enabled { t_.text_secondary } else { t_.text_disabled }));
        if enabled { button.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| app.navigate_tool_prompt(&conversation, delta)) } else { button.disabled(true).on_click(|_: &mut App| {}) }
    };
    Some(row().items_center().gap(2.0).shrink0().child(step(true, "ChevronLeft", previous_label, stack.index > 0)).child(text(count).ty(FINE_STRONG).color(t_.text_tertiary).min_w(26.0).text_center()).child(step(false, "ChevronRight", next_label, stack.index + 1 < stack.total)))
}

fn action_button(key: u64, label: &str, leading: Option<&'static str>, trailing: Option<&'static str>, color: Color, enabled: bool, on_click: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    say(label);
    let fg = if enabled { color } else { t_.text_disabled };
    let button: E = row().key(key).items_center().gap(6.0).min_h(30.0).px(6.0).radius(5.0).shrink0().child(leading.map(|name| icon(name, 14.0).color(fg))).child(text(label.to_owned()).ty(FINE_STRONG).color(fg).nowrap()).child(trailing.map(|name| icon(name, 14.0).color(fg)));
    if enabled { button.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).on_click(on_click) } else { button.disabled(true).on_click(|_: &mut App| {}) }
}

/// `.tool-approval-dock-slide-in` and its question card twin: `.18s ease-out`, rising 7 px as it fades in, each time a card mounts.
pub(crate) fn arriving(card: E, since: Instant) -> E {
    const SECONDS: f32 = 0.18;
    let progress = since.elapsed().as_secs_f32() / SECONDS;
    if progress >= 1.0 || fluent::reduced_motion() {
        return card;
    }
    repaint();
    let eased = 1.0 - (1.0 - progress) * (1.0 - progress);
    card.top(7.0 * (1.0 - eased)).opacity(eased)
}

/// `.tool-approval-dock__footer`, a grid of `minmax(90px, 1fr) auto auto`, or `1fr auto auto` with less between them on a narrow
/// window. The first action stands at the start with the free room after it; with only two actions the last column is empty, and
/// the gap before it still stands.
fn footer(first: E, second: E, third: Option<E>) -> E {
    let narrow = crate::narrow_window();
    let start = row().flex1().min_w(if narrow { 0.0 } else { 90.0 }).child(first);
    row().mt(10.0).min_h(36.0).items_center().gap(if narrow { 8.0 } else { 12.0 }).child(start).child(second).child(third.unwrap_or_else(div))
}

/// The card, or nothing when no call is waiting. A question card is answered in the question dock, not with a decision.
pub fn tool_approval_dock(app: &mut App, conversation_id: &str, pending: Option<&PendingToolPrompt>, stack: Option<DockStack>) -> Option<E> {
    if !app.local::<Cards>().states.is_empty() {
        let live: std::collections::HashSet<String> = app.runs().tool_prompts.values().flatten().map(|prompt| prompt.prompt_id.clone()).collect();
        app.local::<Cards>().states.retain(|id, _| live.contains(id));
    }
    // Nothing waiting here, or a question up instead: the next card to show has arrived. With a stack and no card it is only that
    // another surface draws it, and that surface's call is the one that counts.
    if pending.map_or(stack.is_none(), |prompt| prompt.kind == PromptKind::Question) {
        app.local::<Cards>().shown = None;
    }
    let pending = pending.filter(|prompt| prompt.kind != PromptKind::Question)?;
    let t_ = theme();
    let prompt_id = pending.prompt_id.clone();
    let plan_exit = pending.kind == PromptKind::PlanExit;
    let (decided, feedback, arrived) = {
        let cards = app.local::<Cards>();
        let arrived = match &cards.shown {
            Some((shown, since)) if *shown == prompt_id => *since,
            _ => {
                // Focus goes to the safe default on arrival, so Enter answers the card the eye just landed on.
                focus_request(allow_key(&prompt_id));
                let now = Instant::now();
                cards.shown = Some((prompt_id.clone(), now));
                now
            }
        };
        let card = cards.states.entry(prompt_id.clone()).or_default();
        (card.decided, card.feedback.clone(), arrived)
    };
    let locked = decided.is_some();

    let heading = if plan_exit { t("计划已就绪，批准或提意见", "Plan ready: approve it or send feedback") } else { t("需要你的确认", "Your approval is needed") };
    say(heading);
    let mut title: E = row().flex1().min_w(0.0).items_baseline().gap(8.0).child(text(heading).key(title_key(&prompt_id)).role(Role::Heading).aria_level(2).ty(FINE_STRONG).color(t_.text_secondary).nowrap());
    if !plan_exit {
        let detail = match &pending.requester {
            Some(requester) => tf("{requester} · {label}", "{requester} · {label}", &[("requester", requester), ("label", &pending.label)]),
            None => pending.label.clone(),
        };
        say(&detail);
        title = title.child(text(detail).ty(FINE).color(t_.text_tertiary).ellipsis().min_w(0.0));
    }
    let mut header: E = row().min_h(32.0).items_center().gap(7.0).child(row().center().size(20.0, 20.0).shrink0().child(icon("ShieldAlert", 15.0).color(t_.caution))).child(title);
    header = header.child(pager(conversation_id, stack, t("上一条待确认", "Previous pending approval"), t("下一条待确认", "Next pending approval")));
    // A plan card is a decision about the conversation, not about one call: it has no risk level to report.
    if !plan_exit {
        let risk = tf("风险 {level}", "{level} risk", &[("level", &pending.risk_level)]);
        say(&risk);
        header = header.child(text(risk).ty(FINE_STRONG).color(t_.text_tertiary).nowrap().shrink0());
    }

    say(&pending.summary);
    // A command or a list of addresses is shown whole, its line breaks kept; past about eight lines it scrolls instead of pushing
    // the composer off screen.
    let summary: E = div().key(("approval-summary", prompt_id.as_str())).mt(9.0).max_h(12.8 * MONO_BODY.size).scroll_y().child(text(pending.summary.clone()).ty(MONO_BODY).color(t_.text_primary).selectable());
    let reason = if plan_exit { t("计划全文见左侧计划页面", "The full plan is shown on the plan page").to_owned() } else { pending.reason.clone() };
    say(&reason);
    let mut card: E = col().key(("tool-approval-dock", prompt_id.as_str())).role(Role::Dialog).aria_labelledby(title_key(&prompt_id)).w_full().max_w(820.0).mx_auto().mt(10.0).mb(2.0).shrink0().child(header).child(summary).child(text(reason).ty(FINE).color(t_.text_tertiary).mt(5.0));
    // Plan cards are always mandatory; saying so would only be noise.
    if !plan_exit && pending.mandatory {
        let always_asked = t("任何安全层级都会询问这一条。", "Asked at every security level.");
        say(always_asked);
        card = card.child(text(always_asked).ty(FINE).color(t_.text_tertiary).mt(5.0));
    }

    let conversation = conversation_id.to_owned();
    let answer = move |decision: ToolPromptDecision, feedback: Option<String>| {
        let (conversation, prompt) = (conversation.clone(), prompt_id.clone());
        move |app: &mut App| decide(app, &conversation, &prompt, decision, feedback.as_deref())
    };
    let prompt_id = pending.prompt_id.clone();

    // A plan card has no refusal: the user approves, or writes what should change and the model revises the plan and asks again.
    // The security level is not part of the answer: it stays whatever the user picked.
    if plan_exit {
        let writer = prompt_id.clone();
        let reply = field::<App>(feedback_field_key(&prompt_id), feedback.clone())
            .multiline(54.0, 120.0)
            .ty(CAPTION)
            .placeholder(t("有要改的地方？写在这里发给模型", "Anything to change? Write it here for the model"))
            .on_change(move |app: &mut App, value| {
                // Typing in it is not yet an answer: the guard stays unspent until the user sends it or approves.
                app.local::<Cards>().states.entry(writer.clone()).or_default().feedback = value;
                repaint();
            })
            .build()
            .aria_label(t("修改意见", "Plan feedback"));
        let reply = if locked { reply.disabled(true) } else { reply };
        card = card.child(div().mt(8.0).child(reply));
        let trimmed = feedback.trim().to_owned();
        let send = action_button(always_key(&prompt_id), t("发送意见", "Send feedback"), None, None, t_.text_secondary, !locked && !trimmed.is_empty(), answer(ToolPromptDecision::Deny, Some(trimmed)));
        let approve = action_button(allow_key(&prompt_id), t("批准", "Approve"), None, Some("Check"), t_.text_primary, !locked, answer(ToolPromptDecision::AllowOnce, None));
        card = card.child(footer(send, allow_on_enter(approve, !locked, answer(ToolPromptDecision::AllowOnce, None)), None));
    } else {
        let deny = action_button(deny_key(&prompt_id), t("拒绝", "Deny"), Some("X"), None, t_.text_secondary, !locked, answer(ToolPromptDecision::Deny, None));
        let always = pending.allow_always_offered.then(|| action_button(always_key(&prompt_id), t("总是允许", "Always allow"), None, None, t_.text_secondary, !locked, answer(ToolPromptDecision::AllowAlways, None)));
        let allow = action_button(allow_key(&prompt_id), t("允许", "Allow"), None, Some("Check"), t_.text_primary, !locked, answer(ToolPromptDecision::AllowOnce, None));
        let allow = allow_on_enter(allow, !locked, answer(ToolPromptDecision::AllowOnce, None));
        card = card.child(match always {
            Some(always) => footer(deny, always, Some(allow)),
            None => footer(deny, allow, None),
        });
    }
    card = card.child(div().min_h(14.0).aria_live(Live::Polite).when_some(status_text(plan_exit, decided), |e, status| {
        say(status);
        e.child(text(status).ty(FINE).color(t_.text_tertiary))
    }));
    Some(arriving(card, arrived))
}

/// The default action holds the focus when the card arrives, and Enter presses what holds the focus.
fn allow_on_enter(button: E, enabled: bool, on_enter: impl Fn(&mut App) + 'static) -> E {
    if !enabled {
        return button;
    }
    button.focusable().on_key(move |app: &mut App, press: &KeyPress| {
        if press.key == Key::Enter {
            on_enter(app);
            return true;
        }
        false
    })
}

trait WhenSome: Sized {
    fn when_some<T>(self, value: Option<T>, f: impl FnOnce(Self, T) -> Self) -> Self;
}

impl WhenSome for E {
    fn when_some<T>(self, value: Option<T>, f: impl FnOnce(Self, T) -> Self) -> Self {
        match value {
            Some(value) => f(self, value),
            None => self,
        }
    }
}
