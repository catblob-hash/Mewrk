//! `CodexLoginPanel.tsx`: signing in with a ChatGPT account.

use std::time::Duration;

use mewrk_client::model::ApiProvider;
use mewrk_client::prelude::*;
use serde::Deserialize;

use super::{set_provider_enabled, state};
use crate::logic::codex_provider::CODEX_SIGN_IN_CANCELLED_MESSAGES;
use crate::logic::store;
use crate::widgets::*;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexOauthAccount {
    pub account_id: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan_type: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexOauthStatus {
    pub signed_in: bool,
    pub signing_in: bool,
    #[serde(default)]
    pub account: Option<CodexOauthAccount>,
}

#[derive(Default)]
pub struct CodexState {
    pub for_provider: Option<String>,
    pub status: Option<CodexOauthStatus>,
    pub pending: bool,
    pub error: Option<String>,
    pub request_token: u64,
    /// The last status shown: a poll that sees "waiting" turn into "signed in" enables the provider, which matters when
    /// the page came back mid-flow and lost the sign-in call.
    pub last_status: Option<CodexOauthStatus>,
    pub poll: Option<mewrk_client::timers::TimerId>,
}

/// What the panel unmounting does: the next time it's drawn it starts over, and nothing asks the host meanwhile.
pub fn reset(app: &mut App) {
    stop_polling(app);
    let c = &mut state(app).codex;
    let request_token = c.request_token + 1;
    *c = CodexState { request_token, ..Default::default() };
}

fn stop_polling(app: &mut App) {
    if let Some(timer) = state(app).codex.poll.take() {
        app.clear_timer(timer);
    }
}

fn current(app: &mut App, generation: u64, token: u64) -> bool {
    let s = state(app);
    s.generation == generation && s.codex.request_token == token
}

fn apply_status(app: &mut App, provider_id: &str, next: CodexOauthStatus) {
    let c = &mut state(app).codex;
    let previous = c.last_status.replace(next.clone());
    c.status = Some(next.clone());
    if previous.is_some_and(|p| p.signing_in && !p.signed_in) && next.signed_in {
        set_provider_enabled(app, provider_id, true);
    }
}

fn provider_of(app: &App, id: &str) -> Option<ApiProvider> {
    app.doc.assets.api_providers.iter().find(|p| p.id == id).cloned()
}

/// Serialized: the next poll is scheduled only after this one answered, so two reads can never land out of order.
fn refresh(app: &mut App, provider_id: String, generation: u64, token: u64) {
    store::flush(app, move |app, flushed| {
        if !current(app, generation, token) {
            return;
        }
        if let Err(e) = flushed {
            state(app).codex.error = Some(e);
            return;
        }
        let Some(provider) = provider_of(app, &provider_id) else { return };
        let id = provider_id.clone();
        app.invoke::<CodexOauthStatus>("codex_oauth_status", json!({ "provider": provider }), move |app, result| {
            if !current(app, generation, token) {
                return;
            }
            match result {
                Ok(next) => {
                    let again = next.signing_in;
                    apply_status(app, &id, next);
                    if again {
                        let id2 = id.clone();
                        let timer = app.set_timer(Duration::from_secs(2), move |app| refresh(app, id2, generation, token));
                        state(app).codex.poll = Some(timer);
                    }
                }
                Err(e) => state(app).codex.error = Some(e),
            }
        });
    });
}

/// `useEffect` on the provider's identity, and the Retry button.
fn restart(app: &mut App, provider_id: &str) {
    stop_polling(app);
    let s = state(app);
    let generation = s.generation;
    let c = &mut s.codex;
    c.for_provider = Some(provider_id.to_owned());
    c.request_token += 1;
    let token = c.request_token;
    c.last_status = None;
    c.status = None;
    c.pending = false;
    c.error = None;
    refresh(app, provider_id.to_owned(), generation, token);
}

fn begin_sign_in(app: &mut App, provider_id: String) {
    stop_polling(app);
    let s = state(app);
    let generation = s.generation;
    let c = &mut s.codex;
    c.request_token += 1;
    let token = c.request_token;
    c.pending = true;
    c.error = None;
    if let Some(status) = c.status.as_mut() {
        status.signing_in = true;
    }
    store::flush(app, move |app, flushed| {
        if !current(app, generation, token) {
            return;
        }
        let fail = move |app: &mut App, message: String| {
            let c = &mut state(app).codex;
            if !CODEX_SIGN_IN_CANCELLED_MESSAGES.contains(&message.as_str()) {
                c.error = Some(message);
            }
            if let Some(status) = c.status.as_mut() {
                status.signing_in = false;
            }
            c.pending = false;
        };
        if let Err(e) = flushed {
            return fail(app, e);
        }
        let Some(provider) = provider_of(app, &provider_id) else { return };
        let id = provider_id.clone();
        app.invoke::<CodexOauthStatus>("codex_oauth_sign_in", json!({ "provider": provider }), move |app, result| {
            if !current(app, generation, token) {
                return;
            }
            match result {
                Ok(next) => {
                    let c = &mut state(app).codex;
                    c.last_status = Some(next.clone());
                    c.status = Some(next.clone());
                    c.pending = false;
                    if next.signed_in {
                        set_provider_enabled(app, &id, true);
                    }
                }
                Err(e) => fail(app, e),
            }
        });
    });
}

fn cancel_sign_in(app: &mut App, provider_id: String) {
    stop_polling(app);
    let s = state(app);
    let generation = s.generation;
    let c = &mut s.codex;
    c.request_token += 1;
    let token = c.request_token;
    c.pending = false;
    c.error = None;
    store::flush(app, move |app, flushed| {
        let fail = move |app: &mut App, e: String| {
            if current(app, generation, token) {
                state(app).codex.error = Some(e);
            }
        };
        if let Err(e) = flushed {
            return fail(app, e);
        }
        let Some(provider) = provider_of(app, &provider_id) else { return };
        let body = json!({ "provider": provider });
        let id = provider_id.clone();
        let status_body = body.clone();
        app.invoke::<Value>("codex_oauth_cancel_sign_in", body, move |app, result| {
            if let Err(e) = result {
                return fail(app, e);
            }
            app.invoke::<CodexOauthStatus>("codex_oauth_status", status_body, move |app, result| {
                if !current(app, generation, token) {
                    return;
                }
                // A callback that raced the cancel may have completed the sign-in; the host's answer is authoritative.
                match result {
                    Ok(next) => apply_status(app, &id, next),
                    Err(e) => state(app).codex.error = Some(e),
                }
            });
        });
    });
}

fn sign_out(app: &mut App, provider_id: String) {
    stop_polling(app);
    let s = state(app);
    let generation = s.generation;
    let c = &mut s.codex;
    c.request_token += 1;
    let token = c.request_token;
    c.pending = true;
    c.error = None;
    store::flush(app, move |app, flushed| {
        if !current(app, generation, token) {
            return;
        }
        if let Err(e) = flushed {
            let c = &mut state(app).codex;
            c.error = Some(e);
            c.pending = false;
            return;
        }
        let Some(provider) = provider_of(app, &provider_id) else { return };
        let id = provider_id.clone();
        app.invoke::<CodexOauthStatus>("codex_oauth_sign_out", json!({ "provider": provider }), move |app, result| {
            if !current(app, generation, token) {
                return;
            }
            state(app).codex.pending = false;
            match result {
                Ok(next) => {
                    let c = &mut state(app).codex;
                    c.last_status = Some(next.clone());
                    c.status = Some(next);
                    set_provider_enabled(app, &id, false);
                }
                Err(e) => state(app).codex.error = Some(e),
            }
        });
    });
}

fn plan_label(plan: Option<&str>) -> String {
    let Some(plan) = plan.filter(|p| !p.is_empty()) else { return String::new() };
    let mut chars = plan.chars();
    let first: String = chars.next().map_or_else(String::new, |c| c.to_uppercase().collect());
    format!("{first}{}", chars.as_str())
}

/// `.codex-login`: a bordered box, green once signed in.
fn panel(signed_in: bool) -> E {
    let t_ = theme();
    col().gap(8.0).p(10.0).radius(8.0).border(if signed_in { t_.success } else { t_.card_stroke }).bg(t_.card)
}

pub fn codex_login_panel(app: &mut App, provider: &ApiProvider) -> E {
    let t_ = theme();
    if state(app).codex.for_provider.as_deref() != Some(provider.id.as_str()) {
        restart(app, &provider.id);
    }
    let c = &state(app).codex;
    let (status, pending, error) = (c.status.clone(), c.pending, c.error.clone());
    let id = provider.id.clone();
    let Some(status) = status else {
        let retry_id = id.clone();
        let failed = error.is_some();
        return panel(false)
            .child(help(if failed { t("读取登录状态失败。", "Could not read the sign-in status.") } else { t("正在读取登录状态…", "Loading sign-in status…") }))
            .child(error.map(error_line))
            .child(failed.then(|| row().child(small_button(t("重试", "Retry")).key("codex-retry").on_click(move |app: &mut App| restart(app, &retry_id)))));
    };
    if status.signed_in {
        let account = status.account.clone();
        let mut parts: Vec<String> = Vec::new();
        if let Some(a) = &account {
            if let Some(email) = a.email.as_ref().filter(|e| !e.is_empty()) {
                parts.push(email.clone());
            }
            let plan = plan_label(a.plan_type.as_deref());
            if !plan.is_empty() {
                parts.push(plan);
            }
            if !a.account_id.is_empty() {
                parts.push(a.account_id.clone());
            }
        }
        let out_id = id.clone();
        let out_glyph = pending.then(|| spinner(12.0));
        return panel(true)
            .child(row().gap(8.0).items_start().child(icon("CheckCircle2", 16.0).color(t_.success)).child(col().flex1().min_w(0.0).gap(2.0).child(text(t("已登录 ChatGPT", "Signed in to ChatGPT")).ty(CAPTION_STRONG)).child(help(parts.join(" · ")))))
            .child(row().child(small_subtle_button(t("退出登录", "Sign out")).key("codex-sign-out").gap(4.0).child(out_glyph).disabled(pending).on_click(move |app: &mut App| sign_out(app, out_id.clone()))))
            .child(error.map(error_line));
    }
    let signing_in = pending || status.signing_in;
    let (in_id, cancel_id) = (id.clone(), id);
    let glyph = if signing_in { spinner(12.0) } else { icon("LogIn", 12.0).color(t_.text_on_accent) };
    panel(false)
        .child(row().gap(8.0).items_start().child(icon("CircleAlert", 16.0).color(t_.critical)).child(col().flex1().min_w(0.0).gap(2.0).child(text(t("使用 ChatGPT 账号登录", "Sign in with your ChatGPT account")).ty(CAPTION_STRONG)).child(help_with_docs(t("用 ChatGPT 订阅额度调用 Codex 模型。", "Uses your ChatGPT subscription for Codex models."), "providers", "codex"))))
        .child(
            row()
                .gap(6.0)
                .child(with_icon(glyph, if signing_in { t("等待浏览器授权…", "Waiting for browser authorization…") } else { t("登录 ChatGPT", "Sign in with ChatGPT") }, true).key("codex-sign-in").disabled(signing_in).on_click(move |app: &mut App| begin_sign_in(app, in_id.clone())))
                .child(signing_in.then(|| small_button(t("取消", "Cancel")).key("codex-cancel").on_click(move |app: &mut App| cancel_sign_in(app, cancel_id.clone())))),
        )
        .child(error.map(error_line))
}
