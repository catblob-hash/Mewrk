//! `ClaudeAgentLoginPanel.tsx` and `ClaudeAgentComponentPanel.tsx`: the user's own Claude Code, which Mewrk finds rather
//! than installs, and its login.

use mewrk_client::model::ApiProvider;
use mewrk_client::prelude::*;
use serde::{Deserialize, Serialize};

use super::{set_provider_enabled, state};
use crate::logic::codex_provider::CLAUDE_AGENT_LEGAL_URL;
use crate::logic::store;
use crate::shell::WindowState;
use crate::widgets::*;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAgentLoginStatus {
    pub signed_in: bool,
    #[serde(default)]
    pub auth_method: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub config_dir: String,
    #[serde(default)]
    pub login_command: String,
}

/// `claude_agent_cli_status`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCliStatus {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub problem: Option<String>,
    #[serde(default)]
    pub install_command: String,
}

impl ClaudeCliStatus {
    /// Whether a step could run with it.
    pub fn usable(&self) -> bool {
        self.path.is_some() && self.problem.is_none()
    }
}

#[derive(Default)]
pub struct LoginState {
    pub for_provider: Option<String>,
    pub status: Option<ClaudeAgentLoginStatus>,
    pub pending: bool,
    pub error: Option<String>,
    pub copied: bool,
    pub request_token: u64,
    /// A re-check that turns "signed out" into "signed in" enables the provider; an already-signed-in first read is not
    /// a transition and must leave a deliberately disabled row alone.
    pub last_status: Option<ClaudeAgentLoginStatus>,
    /// False while the first read is still running: it answers for any refresh asked meanwhile.
    pub settled: bool,
    pub seen_revision: u64,
}

fn login_session_key(provider_id: &str) -> String {
    format!("claude-agent:login:{provider_id}")
}

const CLI_KEY: &str = "claude-agent:cli";

fn provider_of(app: &App, id: &str) -> Option<ApiProvider> {
    app.doc.assets.api_providers.iter().find(|p| p.id == id).cloned()
}

fn show_login_status(app: &mut App, provider_id: &str, next: ClaudeAgentLoginStatus) {
    let enable_when_signed_in = state(app).claude_cli.fresh_install;
    let l = &mut state(app).claude_login;
    let previous = l.last_status.replace(next.clone());
    l.status = Some(next.clone());
    if next.signed_in && previous.map_or(enable_when_signed_in, |p| !p.signed_in) {
        set_provider_enabled(app, provider_id, true);
    }
}

/// Asks the host now. `blank` drops the panel to "loading" first (Re-check and Retry); otherwise it keeps showing what it
/// has and a failure changes nothing (the refreshes on focus and on a different CLI).
fn read_now(app: &mut App, provider_id: String, blank: bool) {
    let s = state(app);
    let generation = s.generation;
    let l = &mut s.claude_login;
    l.request_token += 1;
    let token = l.request_token;
    if blank {
        l.status = None;
        l.pending = false;
        l.error = None;
        l.copied = false;
    }
    let current = move |app: &mut App| {
        let s = state(app);
        s.generation == generation && s.claude_login.request_token == token
    };
    store::flush(app, move |app, flushed| {
        if !current(app) {
            return;
        }
        if let Err(e) = flushed {
            let l = &mut state(app).claude_login;
            if blank {
                l.error = Some(e);
            }
            l.settled = true;
            return;
        }
        let Some(provider) = provider_of(app, &provider_id) else { return };
        let id = provider_id.clone();
        app.invoke::<ClaudeAgentLoginStatus>("claude_agent_login_status", json!({ "provider": provider }), move |app, result| {
            if !current(app) {
                return;
            }
            match result {
                Ok(next) => {
                    state(app).claude_login.error = None;
                    if let Ok(v) = serde_json::to_value(&next) {
                        app.local::<WindowState>().session.put(&login_session_key(&id), v);
                    }
                    show_login_status(app, &id, next);
                }
                Err(e) => {
                    if blank {
                        state(app).claude_login.error = Some(e);
                    }
                }
            }
            state(app).claude_login.settled = true;
        });
    });
}

fn start_login(app: &mut App, provider_id: &str) {
    let cached: Option<ClaudeAgentLoginStatus> = app.local::<WindowState>().session.peek(&login_session_key(provider_id)).and_then(|v| serde_json::from_value(v.clone()).ok());
    let revision = state(app).claude_cli.revision;
    let l = &mut state(app).claude_login;
    l.for_provider = Some(provider_id.to_owned());
    l.pending = false;
    l.error = None;
    l.copied = false;
    l.seen_revision = revision;
    if let Some(cached) = cached {
        l.last_status = Some(cached.clone());
        l.status = Some(cached);
        l.settled = true;
        return;
    }
    l.status = None;
    l.settled = false;
    read_now(app, provider_id.to_owned(), false);
}

fn open_login(app: &mut App, provider_id: String) {
    let s = state(app);
    let generation = s.generation;
    let l = &mut s.claude_login;
    l.request_token += 1;
    let token = l.request_token;
    l.pending = true;
    l.error = None;
    let current = move |app: &mut App| {
        let s = state(app);
        s.generation == generation && s.claude_login.request_token == token
    };
    store::flush(app, move |app, flushed| {
        if !current(app) {
            return;
        }
        if let Err(e) = flushed {
            let l = &mut state(app).claude_login;
            l.error = Some(e);
            l.pending = false;
            return;
        }
        let Some(provider) = provider_of(app, &provider_id) else { return };
        app.invoke::<Value>("claude_agent_open_login", json!({ "provider": provider }), move |app, result| {
            if !current(app) {
                return;
            }
            let l = &mut state(app).claude_login;
            l.pending = false;
            if let Err(e) = result {
                l.error = Some(e);
            }
        });
    });
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    let first: String = chars.next().map_or_else(String::new, |c| c.to_uppercase().collect());
    format!("{first}{}", chars.as_str())
}

/// Which login the CLI is using. `console` is billed per token, which must not read the same as a subscription.
fn auth_method_label(status: &ClaudeAgentLoginStatus) -> String {
    if status.auth_method == "console" {
        return t("Console（API 计费）", "Console (API billing)").to_owned();
    }
    status.subscription_type.as_deref().map_or_else(String::new, title_case)
}

fn compliance() -> E {
    row().wrap().items_center().gap(4.0).child(help(t("Mewrk 复用你本机 Claude Code 的登录状态，仅供本人使用。", "Mewrk reuses the Claude Code login on this machine, for your own use only."))).child(link("claude-legal", t("Claude Code 使用条款", "Claude Code terms"), CLAUDE_AGENT_LEGAL_URL.to_owned()))
}

fn login_panel(app: &mut App, provider: &ApiProvider) -> E {
    let t_ = theme();
    let id = provider.id.clone();
    if state(app).claude_login.for_provider.as_deref() != Some(id.as_str()) {
        start_login(app, &id);
    }
    // The panel above moves this when it finds a different CLI: the one that answers has changed.
    let revision = state(app).claude_cli.revision;
    if state(app).claude_login.seen_revision != revision {
        state(app).claude_login.seen_revision = revision;
        if state(app).claude_login.settled {
            read_now(app, id.clone(), false);
        }
    }
    let l = &state(app).claude_login;
    let (status, pending, error, copied) = (l.status.clone(), l.pending, l.error.clone(), l.copied);
    let recheck_id = id.clone();
    let recheck = with_icon(icon("RefreshCw", 12.0).color(t_.text_secondary), t("重新检查", "Re-check"), false).key("claude-recheck").on_click(move |app: &mut App| read_now(app, recheck_id.clone(), true));
    let Some(status) = status else {
        let failed = error.is_some();
        let retry_id = id;
        return col()
            .gap(6.0)
            .child(help(if failed { t("读取 Claude Code 登录状态失败。", "Could not read the Claude Code sign-in status.") } else { t("正在读取登录状态…", "Loading sign-in status…") }))
            .child(error.map(error_line))
            .child(failed.then(|| {
                col().gap(6.0).child(help(t("Mewrk 用的是上面找到的 Claude Code；读不到通常是安装不完整（可以重新安装），或者本机 ~/.claude 不可读。", "Mewrk uses the Claude Code found above; a failure here usually means an incomplete install (reinstalling fixes it), or that ~/.claude is unreadable."))).child(row().child(small_button(t("重试", "Retry")).key("claude-retry").on_click(move |app: &mut App| read_now(app, retry_id.clone(), true))))
            }))
            .child(compliance());
    };
    if status.signed_in {
        let parts: Vec<String> = [status.email.clone().unwrap_or_default(), status.org_name.clone().unwrap_or_default(), auth_method_label(&status)].into_iter().filter(|s| !s.is_empty()).collect();
        return col()
            .gap(6.0)
            .child(row().gap(8.0).items_start().child(icon("CheckCircle2", 16.0).color(t_.success)).child(col().flex1().min_w(0.0).gap(2.0).child(text(t("已登录 Claude Code", "Signed in to Claude Code")).ty(CAPTION_STRONG)).child(help(parts.join(" · ")))))
            .child(row().child(recheck))
            .child(error.map(error_line))
            .child(compliance());
    }
    let login_id = id;
    let command = status.login_command.clone();
    let copy_cmd = command.clone();
    let glyph = if pending { spinner(12.0) } else { icon("TerminalSquare", 12.0).color(t_.text_on_accent) };
    col()
        .gap(6.0)
        .child(row().gap(8.0).items_start().child(icon("CircleAlert", 16.0).color(t_.text_secondary)).child(col().flex1().min_w(0.0).gap(2.0).child(text(t("使用本机 Claude Code 的登录", "Uses the Claude Code login on this machine")).ty(CAPTION_STRONG)).child(help_with_docs(t("在终端里登录，或复制命令到任意终端运行。", "Sign in from a terminal, or copy the command and run it anywhere."), "providers", "claude-agent"))))
        .child(
            row()
                .gap(6.0)
                .wrap()
                .child(with_icon(glyph, t("打开终端登录", "Open a terminal to sign in"), true).key("claude-open-login").disabled(pending).on_click(move |app: &mut App| open_login(app, login_id.clone())))
                .child(with_icon(icon("Copy", 12.0).color(t_.text_secondary), if copied { t("已复制", "Copied") } else { t("复制命令", "Copy command") }, false).key("claude-copy").on_click(move |app: &mut App| {
                    state(app).claude_login.error = None;
                    copy_to_clipboard(copy_cmd.clone());
                    state(app).claude_login.copied = true;
                }))
                .child(recheck),
        )
        .child(text(command).ty(MONO_SMALL).color(t_.text_tertiary).selectable())
        .child(error.map(error_line))
        .child(compliance())
}

#[derive(Default)]
pub struct CliState {
    pub status: Option<ClaudeCliStatus>,
    pub started: bool,
    pub read_error: Option<String>,
    pub checking: bool,
    pub copied: bool,
    /// Moves each time the look-up finds a different CLI, or a different version of it.
    pub revision: u64,
    /// A usable CLI turned up while this panel was up: a login found then enables the row.
    pub fresh_install: bool,
    issued: u64,
    applied: u64,
}

fn show_cli(app: &mut App, next: ClaudeCliStatus) {
    if let Ok(v) = serde_json::to_value(&next) {
        app.local::<WindowState>().session.put(CLI_KEY, v);
    }
    let c = &mut state(app).claude_cli;
    if let Some(previous) = &c.status {
        if !previous.usable() && next.usable() {
            c.fresh_install = true;
        }
        if previous.usable() && (previous.path != next.path || previous.version != next.version) {
            c.revision += 1;
        }
    }
    c.status = Some(next);
}

/// Looks for the CLI. Answers are applied in the order their requests started. `visible` is Re-check: it shows the wait and
/// says what went wrong, where a refresh on focus keeps what is there.
fn look_up(app: &mut App, visible: bool) {
    let s = state(app);
    let generation = s.generation;
    let c = &mut s.claude_cli;
    c.issued += 1;
    let sequence = c.issued;
    if visible {
        c.checking = true;
        c.read_error = None;
        c.copied = false;
    }
    app.invoke::<ClaudeCliStatus>("claude_agent_cli_status", json!({}), move |app, result| {
        if state(app).generation != generation {
            return;
        }
        let c = &mut state(app).claude_cli;
        if visible {
            c.checking = false;
        }
        if sequence < c.applied {
            return;
        }
        c.applied = sequence;
        match result {
            Ok(next) => {
                state(app).claude_cli.read_error = None;
                show_cli(app, next);
            }
            Err(e) => {
                if visible || state(app).claude_cli.status.is_none() {
                    state(app).claude_cli.read_error = Some(e);
                }
            }
        }
    });
}

/// The first time the panel is shown in a session: what was last found, at once, and a look-up that replaces it.
fn start_cli(app: &mut App) {
    state(app).claude_cli.started = true;
    let cached: Option<ClaudeCliStatus> = app.local::<WindowState>().session.peek(CLI_KEY).and_then(|v| serde_json::from_value(v.clone()).ok());
    if let Some(cached) = cached {
        state(app).claude_cli.status = Some(cached);
    }
    look_up(app, false);
}

pub fn on_refocus(app: &mut App) {
    // Quiet: coming back from a terminal the install ran in looks again, and a failure changes nothing.
    look_up(app, false);
    // Coming back from the terminal the sign-in ran in asks again.
    let l = &state(app).claude_login;
    if l.settled
        && let Some(id) = l.for_provider.clone() {
            read_now(app, id, false);
        }
}

fn cli_panel(app: &mut App) -> E {
    let t_ = theme();
    if !state(app).claude_cli.started {
        start_cli(app);
    }
    let c = &state(app).claude_cli;
    let (status, read_error, checking, copied) = (c.status.clone(), c.read_error.clone(), c.checking, c.copied);
    let glyph = if checking || (status.is_none() && read_error.is_none()) {
        spinner(16.0)
    } else if status.as_ref().is_some_and(ClaudeCliStatus::usable) {
        icon("CheckCircle2", 16.0).color(t_.success)
    } else {
        icon("CircleAlert", 16.0).color(t_.caution)
    };
    let mut copy = col().flex1().min_w(0.0).gap(3.0).child(text("Claude Code").ty(CAPTION_STRONG).nowrap());
    match &status {
        None => copy = copy.child(help(if read_error.is_some() { t("查找 Claude Code 失败。", "Could not look for Claude Code.") } else { t("正在查找 Claude Code…", "Looking for Claude Code…") })),
        Some(found) => match &found.path {
            None => copy = copy.child(help_with_docs(t("没有找到 claude 命令。安装 Claude Code 后重新检测，再登录。", "The claude command was not found. Install Claude Code, Re-check, then sign in."), "providers", "claude-agent")),
            Some(path) => {
                copy = copy
                    .child(fact_row(t("版本", "Version"), text(found.version.clone().unwrap_or_else(|| "—".to_owned())).ty(FINE).color(t_.text_secondary)))
                    .child(fact_row(t("位置", "Location"), text(path.clone()).ty(MONO_SMALL).color(t_.text_tertiary).selectable()));
            }
        },
    }
    let mut panel = col().gap(6.0).child(row().gap(8.0).items_start().child(glyph).child(copy));
    if let Some(problem) = status.as_ref().and_then(|s| s.problem.clone()) {
        panel = panel.child(error_line(problem));
    }
    panel = panel.child(read_error.map(error_line));
    let missing = status.as_ref().filter(|s| s.path.is_none());
    let install = missing.map(|s| s.install_command.clone());
    let recheck_glyph = if checking { spinner(12.0) } else { icon("RefreshCw", 12.0).color(t_.text_secondary) };
    let copy_command = install.clone();
    panel = panel.child(
        row()
            .gap(6.0)
            .wrap()
            .child(copy_command.map(|command| {
                with_icon(icon("Copy", 12.0).color(t_.text_secondary), if copied { t("已复制", "Copied") } else { t("复制安装命令", "Copy install command") }, false).key("claude-copy-install").on_click(move |app: &mut App| {
                    copy_to_clipboard(command.clone());
                    state(app).claude_cli.copied = true;
                })
            }))
            .child(with_icon(recheck_glyph, t("重新检测", "Re-check"), false).key("claude-check").disabled(checking).on_click(|app: &mut App| look_up(app, true))),
    );
    if let Some(command) = install {
        panel = panel.child(text(command).ty(MONO_SMALL).color(t_.text_tertiary).selectable());
    }
    panel
}

/// The CLI, and under it the sign-in, which needs one.
pub fn claude_agent_panels(app: &mut App, provider: &ApiProvider) -> E {
    let cli = cli_panel(app);
    let usable = state(app).claude_cli.status.as_ref().is_some_and(ClaudeCliStatus::usable);
    col().gap(16.0).child(cli).child(usable.then(|| login_panel(app, provider)))
}

/// `formatBytes` of `FilePreview/format`.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        return format!("{bytes} B");
    }
    if b < KB * KB {
        return format!("{:.1} KB", b / KB);
    }
    if b < KB * KB * KB {
        return format!("{:.1} MB", b / KB / KB);
    }
    format!("{:.2} GB", b / KB / KB / KB)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cli_is_usable_only_when_it_was_found_and_nothing_is_wrong_with_it() {
        let found = |problem: Option<&str>| ClaudeCliStatus { path: Some("/x/claude".into()), problem: problem.map(str::to_owned), ..Default::default() };
        assert!(found(None).usable());
        assert!(!found(Some("too old")).usable());
        assert!(!ClaudeCliStatus::default().usable());
    }
}
