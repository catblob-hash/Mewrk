//! `SandboxSettings/index.tsx`: the sandbox one workspace's commands run in, asked of the agent on that machine.

use mewrk_client::model::{RunTarget, SandboxNetworkMode, SandboxSettings};
use mewrk_client::prelude::*;
use mewrk_client::workspaces::run_env_key;
use serde::Deserialize;

use crate::widgets::*;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSupport {
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub setup: bool,
}

#[derive(Default)]
pub struct SandboxPageState {
    /// What was asked for, so an equal target rebuilt on each render asks once.
    pub asked_for: Option<String>,
    pub support: Option<SandboxSupport>,
    pub loading: bool,
    pub ignores_case: bool,
    pub setting_up: bool,
    pub setup_error: Option<String>,
    pub token: u64,
}

fn page_state(app: &mut App) -> &mut SandboxPageState {
    app.local::<SandboxPageState>()
}

pub fn unmount(app: &mut App) {
    let token = page_state(app).token + 1;
    *page_state(app) = SandboxPageState { token, ..Default::default() };
}

fn ask_ignores_case(app: &mut App, machine: Option<RunTarget>, path: String, token: u64) {
    app.invoke::<Option<bool>>("workspace_sandbox_ignores_case", json!({ "machine": machine, "path": path }), move |app, result| {
        // Nothing to point out when the directory cannot be asked.
        if page_state(app).token == token {
            page_state(app).ignores_case = matches!(result, Ok(Some(true)));
        }
    });
}

fn ask_support(app: &mut App, machine: Option<RunTarget>, path: String) {
    let s = page_state(app);
    s.token += 1;
    let token = s.token;
    s.loading = true;
    s.support = None;
    s.ignores_case = false;
    let (m2, p2) = (machine.clone(), path);
    app.invoke::<Option<SandboxSupport>>("machine_sandbox_support", json!({ "machine": machine }), move |app, result| {
        let s = page_state(app);
        if s.token != token {
            return;
        }
        s.loading = false;
        let support = match result {
            Ok(v) => v.unwrap_or_default(),
            Err(e) => SandboxSupport { backend: String::new(), available: false, detail: e, setup: false },
        };
        // Only bubblewrap protects a file by its name, so only there does a directory that ignores case weaken it.
        let bubblewrap = support.available && support.backend == "bubblewrap";
        s.support = Some(support);
        if bubblewrap {
            ask_ignores_case(app, m2, p2, token);
        }
    });
}

fn backend_name(backend: &str) -> String {
    match backend {
        "seatbelt" => "Seatbelt".to_owned(),
        "bubblewrap" => "bubblewrap".to_owned(),
        "srt-win" => t("Windows 沙箱", "Windows sandbox").to_owned(),
        other => other.to_owned(),
    }
}

/// The status line, led by the machine's name: the answer is that machine's, whichever computer shows it.
pub fn status_text(machine_name: &str, loading: bool, support: Option<&SandboxSupport>) -> String {
    let what = if loading {
        t("正在检查……", "Checking…").to_owned()
    } else {
        match support {
            None => t("浏览器预览中无法检查。", "Cannot be checked in the browser preview.").to_owned(),
            Some(s) if s.available => tf("可以使用：{backend}", "Available: {backend}", &[("backend", &backend_name(&s.backend))]),
            Some(s) => {
                let detail = if s.detail.is_empty() { backend_name(&s.backend) } else { s.detail.clone() };
                tf("不可用：{detail}", "Not available: {detail}", &[("detail", &detail)])
            }
        }
    };
    format!("{machine_name} · {what}")
}

fn list_field(key: &'static str, scope: u64, label: &str, description: E, value: &[String], placeholder: &str, first: bool, on_change: impl Fn(&mut App, Vec<String>) + 'static) -> E {
    let t_ = theme();
    // Parsed on every keystroke, so it can't render the parse: a blank line being typed between two rules would vanish.
    let draft = use_state(("sandbox-list-draft", key, scope), || None::<String>);
    let shown = draft.get().unwrap_or_else(|| value.join("\n"));
    let change_draft = draft.clone();
    let field_el = field::<App>(("sandbox-list", key, scope), shown)
        .multiline(144.0, 144.0)
        .ty(MONO_BODY)
        .placeholder(placeholder.to_owned())
        .on_change(move |app, v| {
            let parsed = v.split('\n').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect();
            change_draft.set(Some(v));
            on_change(app, parsed);
        })
        .on_blur(move |_, _| draft.set(None))
        .build()
        .aria_label(label);
    col().min_h(48.0).py(9.0).gap(9.0).when(!first, |e| e.border_top(t_.divider)).child(col().gap(2.0).child(text(label.to_owned()).ty(BODY_SMALL)).child(description)).child(field_el)
}

/// The workspace's sandbox. `on_change` gets the whole value, since it goes into a keyed table of the document.
pub fn sandbox_settings(app: &mut App, machine: Option<RunTarget>, machine_name: &str, path: &str, settings: Option<SandboxSettings>, on_change: Rc<dyn Fn(&mut App, SandboxSettings)>) -> E {
    let t_ = theme();
    let current = settings.unwrap_or_default();
    let ask_key = format!("{}|{path}", run_env_key(machine.as_ref()));
    let scope = hash_key(&ask_key);
    if page_state(app).asked_for.as_deref() != Some(ask_key.as_str()) {
        page_state(app).asked_for = Some(ask_key);
        ask_support(app, machine.clone(), path.to_owned());
    }
    let s = page_state(app);
    let (loading, support, ignores_case, setting_up, setup_error) = (s.loading, s.support.clone(), s.ignores_case, s.setting_up, s.setup_error.clone());
    let status = status_text(machine_name, loading, support.as_ref());
    let unavailable = support.as_ref().is_some_and(|s| !s.available);
    // Only this computer's setup can be started from here: an SSH machine's needs an administrator at that machine.
    let needs_setup = unavailable && support.as_ref().is_some_and(|s| s.setup) && machine.is_none();

    let toggled = {
        let (current, on_change) = (current.clone(), on_change.clone());
        move |app: &mut App| {
            let mut next = current.clone();
            next.enabled = !next.enabled;
            on_change(app, next);
        }
    };
    let row_with = |copy: E, control: Option<E>| row().min_h(48.0).py(9.0).gap(22.0).items_center().justify_between().child(copy.flex1()).child(control);
    let mut first = col().child(row_with(col().gap(2.0).child(text(t("在沙箱中运行命令", "Run commands in the sandbox")).ty(BODY_SMALL)).child(text(status).ty(FINE).color(if unavailable { t_.critical } else { t_.text_secondary })), Some(labeled_switch("sandbox-enabled", current.enabled, false, toggled).aria_label(t("在沙箱中运行命令", "Run commands in the sandbox")))));
    if ignores_case {
        first = first.child(col().py(9.0).gap(2.0).border_top(t_.divider).child(text(t("这个目录不区分大小写", "This directory ignores case")).ty(BODY_SMALL)).child(col().role(Role::Note).child(text(t(
            "它所在的文件系统不区分文件名的大小写（WSL 里的 Windows 磁盘就是这样）。Linux 沙箱只能按确切的名字保护文件，所以在这里，.envrc、.mcp.json、.git/config 这类会在沙箱外执行的文件保护不完整，沙箱里的命令可能改到它们；受保护的目录（.mewrk、.vscode、.git/hooks）不受影响，其余限制照常。要完整保护，把工作区放到 Linux 自己的文件系统里，例如 WSL 的主目录下。",
            "Its file system does not tell file names apart by case (Windows drives in WSL work this way). The Linux sandbox can protect a file only by its exact name, so here the files that run outside the sandbox, such as .envrc, .mcp.json and .git/config, are not fully protected: sandboxed commands may change them. Protected directories (.mewrk, .vscode, .git/hooks) are unaffected, and every other limit still applies. For full protection, keep the workspace on a Linux file system, such as your home directory in WSL.",
        )).ty(FINE).color(t_.caution))));
    }
    if needs_setup {
        let set_up = with_icon(icon("ShieldCheck", 12.0).color(t_.text_on_accent), if setting_up { t("正在设置……", "Setting up…") } else { t("设置", "Set up") }, true).key("sandbox-setup").disabled(setting_up).on_click(|app: &mut App| {
            let s = page_state(app);
            s.setting_up = true;
            s.setup_error = None;
            app.invoke::<SandboxSupport>("setup_local_sandbox", json!({}), |app, result| {
                let s = page_state(app);
                s.setting_up = false;
                match result {
                    Ok(next) => s.support = Some(next),
                    Err(e) => s.setup_error = Some(e),
                }
            });
        });
        first = first.child(row_with(col().gap(2.0).child(text(t("设置 Windows 沙箱", "Set up the Windows sandbox")).ty(BODY_SMALL)).child(help_with_docs(t("一次性设置，需要管理员批准。", "One-time setup; needs administrator approval."), "security", "sandbox")).child(setup_error.map(error_line)), Some(set_up)).border_top(t_.divider));
    }
    let bullet = |s: &'static str| row().gap(6.0).items_start().child(text("•").ty(FINE).color(t_.text_tertiary).nowrap()).child(text(s).ty(FINE).color(t_.text_secondary).flex1());
    first = first.child(
        col()
            .py(9.0)
            .gap(4.0)
            .border_top(t_.divider)
            .child(text(t("沙箱里的命令", "What sandboxed commands can do")).ty(BODY_SMALL))
            .child(bullet(t("只能写这个工作区、临时目录和包缓存。", "Write only to this workspace, temp folders and package caches.")))
            .child(bullet(t("读不到 SSH 密钥、令牌等凭据。", "Cannot read credentials such as SSH keys and tokens.")))
            .child(bullet(t("只能经代理联网，遵循下面的网络规则。", "Reach the network only through a proxy, by the rules below.")))
            .child(bullet(t("看不到沙箱外的进程。", "Cannot see processes outside the sandbox.")))
            .child(help_with_docs(t("适用于 shell 命令、后台命令和文件工具。", "Applies to shell commands, background commands and file tools."), "security", "sandbox")),
    );

    let modes = [SandboxNetworkMode::Allowlist, SandboxNetworkMode::Open, SandboxNetworkMode::Off];
    let mode_options: Vec<Arc<str>> = vec![Arc::from(t("只允许白名单", "Allowlist only")), Arc::from(t("任意公网主机", "Any public host")), Arc::from(t("不联网", "No network"))];
    let mode_index = modes.iter().position(|m| *m == current.network.mode).unwrap_or(0);
    let mode_select = {
        let (current, on_change) = (current.clone(), on_change.clone());
        select::<App>("sandbox-network-mode", mode_options, mode_index, move |app, i| {
            let mut next = current.clone();
            next.network.mode = modes[i];
            on_change(app, next);
        })
        .min_w(160.0)
        .aria_label(t("网络", "Network"))
    };
    let mut network = col().child(row_with(col().gap(2.0).child(text(t("网络", "Network")).ty(BODY_SMALL)).child(help(t("放行一个主机，就放行经它的一切。", "Allowing a host allows everything done through it."))), Some(mode_select)));
    if current.network.mode == SandboxNetworkMode::Allowlist {
        let (c1, f1) = (current.clone(), on_change.clone());
        network = network.child(list_field("allow", scope, t("白名单", "Allowlist"), help_with_docs(t("一行一个主机，可通配子域、可加端口。", "One host per line; subdomain wildcards and ports allowed."), "security", "sandbox"), &current.network.allow, "github.com\n*.npmjs.org\nlocalhost:5432", false, move |app, allow| {
            let mut next = c1.clone();
            next.network.allow = allow;
            f1(app, next);
        }));
        let (c2, f2) = (current.clone(), on_change.clone());
        network = network.child(row().justify_end().child(with_icon(icon("RotateCcw", 12.0).color(t_.text_secondary), t("恢复默认白名单", "Restore the default allowlist"), false).key("sandbox-restore-allow").on_click(move |app: &mut App| {
            let mut next = c2.clone();
            next.network.allow = SandboxSettings::default().network.allow;
            f2(app, next);
        })));
    }
    if current.network.mode != SandboxNetworkMode::Off {
        let (c3, f3) = (current.clone(), on_change.clone());
        network = network.child(list_field("deny", scope, t("黑名单", "Blocklist"), help(t("任何模式下都不放行，先于白名单判断。", "Never allowed, in any mode; checked before the allowlist.")), &current.network.deny, "*.example.com", false, move |app, deny| {
            let mut next = c3.clone();
            next.network.deny = deny;
            f3(app, next);
        }));
    }
    let (c4, f4, c5, f5) = (current.clone(), on_change.clone(), current.clone(), on_change);
    let paths = col()
        .child(list_field("writable", scope, t("额外可写目录", "Further writable directories"), help(t("工作区所在机器上的绝对路径或 ~ 路径。", "Absolute or ~ paths on the workspace's machine.")), &current.writable, "~/shared-data", true, move |app, writable| {
            let mut next = c4.clone();
            next.writable = writable;
            f4(app, next);
        }))
        .child(list_field("deny-read", scope, t("额外禁读路径", "Further unreadable paths"), help(t("内置凭据位置之外，沙箱也读不到这些。", "Also unreadable, beyond the built-in credential locations.")), &current.deny_read, "~/secrets", false, move |app, deny_read| {
            let mut next = c5.clone();
            next.deny_read = deny_read;
            f5(app, next);
        }));
    col().child(card(None).mt(0.0).child(first)).child(card(None).child(network)).child(card(None).child(paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    // SandboxSettings.test.tsx
    #[test]
    fn the_status_leads_with_the_machine_and_says_what_it_can_do() {
        let _language = crate::language_for_test(true);
        assert_eq!(status_text("This machine", true, None), "This machine · Checking…");
        let seatbelt = SandboxSupport { backend: "seatbelt".into(), available: true, detail: String::new(), setup: false };
        assert_eq!(status_text("This machine", false, Some(&seatbelt)), "This machine · Available: Seatbelt");
        let refused = SandboxSupport { backend: "bubblewrap".into(), available: false, detail: "WSL 1".into(), setup: false };
        assert_eq!(status_text("WSL: Ubuntu", false, Some(&refused)), "WSL: Ubuntu · Not available: WSL 1");
        let unnamed = SandboxSupport { backend: "srt-win".into(), available: false, detail: String::new(), setup: true };
        assert_eq!(status_text("X", false, Some(&unnamed)), "X · Not available: Windows sandbox");
    }
}
