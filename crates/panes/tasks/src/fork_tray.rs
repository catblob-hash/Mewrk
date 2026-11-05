//! `components/ForkRequestTray.tsx`, and what `App.tsx` kept for it: the pending cards and the answered decisions.

use std::collections::HashMap;

use mewrk_client::prelude::*;
pub use mewrk_client::runs::PendingForkRequest;

use fluent::Weight;

use crate::task_container::ForkDecisionRecord;
use crate::util::utf16_len;
use crate::widgets::ts;

#[derive(Default)]
pub struct ForkStore {
    /// Oldest first; the tray renders the newest card at the top.
    pub requests: Vec<PendingForkRequest>,
    /// Keyed by the source conversation. Model-invisible: the host records them for the task bar alone.
    pub decisions: HashMap<String, Vec<ForkDecisionRecord>>,
    subscribed: bool,
}

impl ForkStore {
    pub fn open_request(&mut self, request: PendingForkRequest) {
        if !self.requests.iter().any(|r| r.fork_id == request.fork_id) {
            self.requests.push(request);
        }
    }

    pub fn close_request(&mut self, fork_id: &str) {
        self.requests.retain(|r| r.fork_id != fork_id);
    }

    /// A reload and the live event can both carry the same decision.
    pub fn append_decision(&mut self, decision: ForkDecisionRecord) {
        let existing = self.decisions.entry(decision.source_conversation_id.clone()).or_default();
        if !existing.iter().any(|r| r.fork_id == decision.fork_id) {
            existing.push(decision);
        }
    }

    /// One `AppPushEvent`. A retraction carries no decision; an answered card carries the row the task bar shows.
    /// Starting the child's first run stays with the shell, which sees the same event.
    pub fn apply_event(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("forkRequested") => {
                if let Ok(request) = serde_json::from_value(event.clone()) {
                    self.open_request(request);
                }
            }
            Some("forkResolved") => {
                if let Some(id) = event.get("forkId").and_then(Value::as_str) {
                    self.close_request(id);
                }
                if let Some(decision) = event.get("decision").filter(|d| !d.is_null())
                    && let Ok(decision) = serde_json::from_value(decision.clone())
                {
                    self.append_decision(decision);
                }
            }
            _ => {}
        }
    }
}

pub fn install(app: &mut App) {
    if std::mem::replace(&mut app.local::<ForkStore>().subscribed, true) {
        return;
    }
    app.on_push_event(|app, event| {
        app.local::<ForkStore>().apply_event(event);
        repaint();
    });
}

/// Cards belong to no run; re-listing them after a reload keeps a request from hiding until its 30-minute expiry.
pub fn list_pending_fork_requests(app: &App) {
    app.invoke::<Vec<PendingForkRequest>>("list_pending_fork_requests", json!({}), |app, result| {
        // Failure is non-fatal: a later push can still show the card.
        for request in result.unwrap_or_default() {
            app.local::<ForkStore>().open_request(request);
        }
        repaint();
    });
}

/// Host rows: re-read whenever a conversation becomes active, so a card answered elsewhere still shows up here.
pub fn list_fork_decisions(app: &App, conversation_id: &str) {
    let id = conversation_id.to_owned();
    app.invoke::<Vec<ForkDecisionRecord>>("list_fork_decisions", json!({ "conversationId": conversation_id }), move |app, result| {
        // Advisory: a failed read keeps whatever was already shown.
        if let Ok(records) = result {
            app.local::<ForkStore>().decisions.insert(id, records);
            repaint();
        }
    });
}

/// Answers one card. It comes down at once; the host publishes `forkResolved` for the outcome.
pub fn decide_fork_request(app: &mut App, fork_id: &str, approved: bool) {
    app.local::<ForkStore>().close_request(fork_id);
    app.invoke::<Value>("resolve_fork_request", json!({ "forkId": fork_id, "approved": approved }), |_, result| {
        if let Err(e) = result {
            eprintln!("{}: {e}", t("分叉请求未能提交给宿主", "The fork decision could not be delivered to the host"));
        }
    });
}

/// Prompts longer than this get an expand toggle; the preview is clamped to five lines.
const PROMPT_TOGGLE_CHARS: usize = 240;
const PROMPT_TY: fluent::TypeStyle = ts(11.5, 18.0, Weight::Regular);

pub type ForkDecide = Rc<dyn Fn(&mut App, &str, bool)>;
pub type ForkOpenSource = Rc<dyn Fn(&mut App, &str, &str)>;

/// Non-blocking stack of fork requests, pinned to the window's top-right corner. It never traps focus and never covers
/// the app: the tray itself is pointer-transparent, only the cards are not. Place it as a child of the window's root,
/// lifted with `overlay_at`: that is what `useFloatingSurface` did, since a native browser page under an overlay makes room.
pub fn fork_request_tray(requests: &[PendingForkRequest], caption_height: f32, on_decide: ForkDecide, on_open_source: Option<ForkOpenSource>) -> Option<E> {
    if requests.is_empty() {
        return None;
    }
    let c = theme();
    let (window_w, window_h) = window_size();
    let width = 360f32.min(window_w - 24.0);
    let mut tray = col().key("fork-request-tray").absolute().top(caption_height + 12.0).right(12.0).w(width).gap(8.0).pass_through().role(Role::Region).aria_label(t("分叉请求", "Fork requests")).aria_live(Live::Polite);
    for request in requests.iter().rev() {
        let id = request.fork_id.as_str();
        let expanded = use_state(("fork-card-expanded", id), || false);
        let is_expanded = expanded.get();
        let source_label = tf("来自：{title}", "From: {title}", &[("title", &request.source_title)]);
        let source: E = match on_open_source.clone() {
            Some(open) => {
                let (workspace, conversation) = (request.workspace_id.clone(), request.source_conversation_id.clone());
                div().key(("fork-card-source", id)).self_start().on_click(move |app| open(app, &workspace, &conversation)).child(text(source_label).ty(FINE).color(c.accent_text).ellipsis())
            }
            None => text(source_label).ty(FINE).color(c.text_secondary).ellipsis(),
        };
        let prompt = text(request.prompt.clone()).ty(PROMPT_TY).key(("fork-card-prompt", id, is_expanded));
        let prompt: E = if is_expanded { col().key(("fork-card-scroll", id)).mt(7.0).max_h(window_h * 0.4).scroll_y().child(prompt) } else { col().mt(7.0).max_h(PROMPT_TY.line * 5.0).clip().child(prompt) };
        let decide = |approved: bool| {
            let (on_decide, id) = (on_decide.clone(), request.fork_id.clone());
            move |app: &mut App| on_decide(app, &id, approved)
        };
        let mut card = col().key(("fork-card", id)).py(12.0).px(14.0).radius(12.0).bg(c.flyout).border(c.flyout_stroke).on_click(|_| {}).role(Role::GenericContainer).cursor(CursorKind::Default)
            .child(row().items_center().gap(6.0).child(icon("GitFork", 13.0).color(c.text_primary)).child(text(t("模型请求分叉会话", "The model wants to fork this conversation")).ty(CAPTION_STRONG).ellipsis()))
            .child(div().mt(4.0).child(source))
            .child(prompt);
        if utf16_len(&request.prompt) > PROMPT_TOGGLE_CHARS {
            let toggle = expanded.clone();
            card = card.child(div().key(("fork-card-toggle", id)).self_start().mt(2.0).on_click(move |_| toggle.update(|v| *v = !*v)).child(text(if is_expanded { t("收起", "Show less") } else { t("展开", "Show more") }).ty(FINE).color(c.accent_text).nowrap()));
        }
        card = card.child(
            row()
                .justify_end()
                .gap(8.0)
                .mt(10.0)
                .child(button(t("拒绝", "Deny")).key(("fork-card-deny", id)).on_click(decide(false)))
                .child(accent_button(t("批准", "Approve")).key(("fork-card-approve", id)).on_click(decide(true))),
        );
        tray = tray.child(card);
    }
    Some(tray)
}
