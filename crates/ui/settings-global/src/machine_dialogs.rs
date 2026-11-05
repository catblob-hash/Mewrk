//! `MachineDialogs.tsx`: a machine's settings, opened from the gear beside it wherever machines are listed. An SSH machine
//! is edited or deleted here; this machine and a WSL distribution are found, not registered, so theirs says what they are.
//! Environment variables and the sandbox are never here: they belong to each workspace.

use mewrk_client::machine_shells::{effective_agent_shell, machine_os_label, shell_backend_label, ShellBackend};
use mewrk_client::model::{RunTarget, SshMachineConfig};
use mewrk_client::prelude::*;
use mewrk_client::workspaces::run_env_key;
use serde::Deserialize;

use mewrk_ui_common::path_text::{path_text_with, PathTextOptions};

use crate::logic::codex_provider::create_id;
use crate::logic::workspace_env::validate_ssh_machine;
use crate::widgets::*;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WslDistro {
    pub name: String,
    pub version: u32,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Default)]
struct DialogState {
    /// Counts the dialogs opened, so a probe still out when its dialog closes has nobody left to tell.
    mount: u64,
    probing: bool,
    probe_error: Option<String>,
    distros: Option<Vec<WslDistro>>,
    pending_delete: bool,
    agent_shell: Option<ShellBackend>,
    name: Option<String>,
    host: Option<String>,
    port: Option<String>,
    identity: Option<String>,
    error: Option<String>,
}

fn dialog_state(app: &mut App) -> &mut DialogState {
    app.local::<DialogState>()
}

/// Whether this is the first frame of this dialog on screen, which is when upstream's component mounts and its state starts over.
/// `use_state` forgets what a view stopped asking for, so closing a dialog is what makes the next one new.
fn just_opened(key: &str) -> bool {
    let seen = use_state(("machine-dialog-open", key), || false);
    !std::mem::replace(&mut *seen.borrow_mut(), true)
}

fn probe(app: &mut App, machine: Option<RunTarget>) {
    let s = dialog_state(app);
    s.probing = true;
    s.probe_error = None;
    let mount = s.mount;
    app.probe_machine_shells(machine, move |app, result| {
        let s = dialog_state(app);
        if s.mount != mount {
            return;
        }
        s.probing = false;
        if let Err(e) = result {
            s.probe_error = Some(e);
        }
    });
}

/// A machine's shell backends: a button that probes again, the agent-shell choice to its right, and what the last probe found, each
/// heading on a line of its own and each shell its name over its path.
fn machine_shells_section(app: &mut App, machine: Option<RunTarget>, agent_shell: Option<(Option<ShellBackend>, Rc<dyn Fn(&mut App, ShellBackend)>)>) -> E {
    let t_ = theme();
    let key = run_env_key(machine.as_ref());
    let probed = app.machine_shells().get(&key).cloned();
    let s = dialog_state(app);
    let (probing, error) = (s.probing, s.probe_error.clone());
    let found: Vec<ShellBackend> = probed.as_ref().map_or_else(Vec::new, |p| p.shells.iter().map(|s| s.backend).collect());
    let machine_probe = machine.clone();
    let probe_button = button::<App>(if probing { t("正在探测…", "Probing…") } else { t("重新探测 shell", "Probe shells again") }).key("machine-probe").disabled(probing).on_click(move |app: &mut App| probe(app, machine_probe.clone()));
    let agent = agent_shell.map(|(chosen, on_change)| {
        let (options, selected): (Vec<Arc<str>>, usize) = if found.is_empty() {
            (vec![Arc::from(chosen.map_or(t("先探测", "Probe first"), shell_backend_label))], 0)
        } else {
            (found.iter().map(|b| Arc::from(shell_backend_label(*b))).collect(), chosen.and_then(|c| found.iter().position(|b| *b == c)).unwrap_or(0))
        };
        let values = found.clone();
        row()
            .gap(7.0)
            .items_center()
            .child(text(t("代理 shell", "Agent shell")).ty(FINE).color(t_.text_secondary).nowrap())
            .child(
                select::<App>("machine-agent-shell", options, selected, move |app, i| {
                    if let Some(v) = values.get(i) {
                        on_change(app, *v);
                    }
                })
                .min_w(118.0)
                .h(28.0)
                .disabled(probing || found.is_empty())
                .aria_label(t("代理 shell", "Agent shell"))
                .tooltip(t("这台机器上的代理用它运行 Mewrk 自己的脚本：远程文件工具与语言服务器。", "The shell this machine's agent runs Mewrk's own scripts in: the remote file tools and language servers.")),
            )
    });
    let mut section = col().gap(9.0).mt(14.0).pt(12.0).border_top(t_.divider).role(Role::Region).aria_label(t("Shell 后端", "Shell backends")).child(row().wrap().gap(10.0).items_center().child(probe_button).child(agent));
    section = if let Some(e) = error {
        section.child(error_line(e))
    } else if let Some(p) = probed {
        let shells: E = if p.shells.is_empty() {
            text(t("没有 Mewrk 能在这个系统上使用的 shell", "No shell Mewrk can use on this system")).ty(CAPTION)
        } else {
            col().gap(6.0).children(p.shells.iter().map(|s| {
                col().gap(1.0).min_w(0.0).tooltip(s.path.clone()).child(text(shell_backend_label(s.backend)).ty(CAPTION)).child(path_text_with(("machine-shell-path", shell_backend_label(s.backend)), s.path.clone(), MONO_SMALL, t_.text_tertiary, PathTextOptions { title: Some(String::new()), ..PathTextOptions::default() }))
            }))
        };
        let heading = |label: &'static str| text(label).ty(CAPTION).color(t_.text_secondary);
        section.child(col().gap(3.0).child(heading(t("系统", "System"))).child(text(machine_os_label(p.os)).ty(CAPTION)).child(heading(t("可用 shell", "Shells")).mt(6.0)).child(shells))
    } else {
        section.child(help(t("还没有探测结果，可以现在探测。", "Not probed yet; you can probe now.")))
    };
    section
}

pub struct MachineSettingsProps {
    /// `None` is this computer.
    pub machine: Option<RunTarget>,
    pub ssh_machines: Vec<SshMachineConfig>,
    pub projects: usize,
    pub conversations: usize,
    pub on_set_wsl_agent_shell: Rc<dyn Fn(&mut App, ShellBackend)>,
    pub on_save_ssh_machine: Rc<dyn Fn(&mut App, SshMachineConfig)>,
    pub on_delete_ssh_machine: Rc<dyn Fn(&mut App, String)>,
    pub on_close: Rc<dyn Fn(&mut App)>,
}

fn ssh_field(key: &'static str, label: &str, value: String, placeholder: &str, set: fn(&mut DialogState, String)) -> E {
    let t_ = theme();
    col()
        .gap(4.0)
        .child(text(label.to_owned()).ty(FINE).color(t_.text_secondary).nowrap())
        .child(field::<App>(("ssh", key), value).placeholder(placeholder.to_owned()).ty(CAPTION).on_change(move |app, v| {
            let s = dialog_state(app);
            set(s, v);
            s.error = None;
        }).build().h(28.0).aria_label(label))
}

/// The SSH machine dialog: creating (`machine: None`) or editing.
pub fn ssh_machine_dialog(app: &mut App, machine: Option<&SshMachineConfig>, in_use_note: Option<String>, machine_count: usize, on_save: Rc<dyn Fn(&mut App, SshMachineConfig)>, on_delete: Option<Rc<dyn Fn(&mut App)>>, on_close: Rc<dyn Fn(&mut App)>) -> E {
    let target = machine.map(|m| RunTarget::Ssh { machine_id: m.id.clone() });
    if just_opened(machine.map_or("new", |m| m.id.as_str())) {
        // What the dialog would record, unless a choice is made here: the shell in effect as it opens.
        let probes = app.machine_shells().clone();
        let opening = target.as_ref().and_then(|tg| effective_agent_shell(Some(tg), &app.doc.assets.execution_environments, &probes));
        let s = dialog_state(app);
        let mount = s.mount + 1;
        *s = DialogState { mount, agent_shell: opening, ..DialogState::default() };
    }
    let s = dialog_state(app);
    let name = s.name.clone().unwrap_or_else(|| machine.map_or_else(String::new, |m| m.name.clone()));
    let host = s.host.clone().unwrap_or_else(|| machine.map_or_else(String::new, |m| m.host.clone()));
    let port = s.port.clone().unwrap_or_else(|| machine.filter(|m| m.port != 0).map_or_else(String::new, |m| m.port.to_string()));
    let identity = s.identity.clone().unwrap_or_else(|| machine.map_or_else(String::new, |m| m.identity_file.clone()));
    let error = s.error.clone();
    let chosen_shell = s.agent_shell.clone();
    let existing = machine.cloned();
    let creating = existing.is_none();
    let form = col().gap(9.0).child(ssh_field("name", t("名称", "Name"), name.clone(), t("开发机", "devbox"), |s, v| s.name = Some(v))).child(ssh_field("host", t("主机", "Host"), host.clone(), "user@hostname", |s, v| s.host = Some(v))).child(ssh_field("port", t("端口（空 = 22）", "Port (empty = 22)"), port.clone(), "22", |s, v| s.port = Some(v))).child(ssh_field("identity", t("身份文件（可选）", "Identity file (optional)"), identity.clone(), "~/.ssh/id_ed25519", |s, v| s.identity = Some(v)));
    // A probe that arrives while the dialog is open fills in the choice it would record, unless one was made here.
    let shells = target.map(|tg| {
        let effective = {
            let probes = app.machine_shells().clone();
            effective_agent_shell(Some(&tg), &app.doc.assets.execution_environments, &probes)
        };
        let value = chosen_shell.or(effective);
        let f: Rc<dyn Fn(&mut App, ShellBackend)> = Rc::new(|app: &mut App, v| dialog_state(app).agent_shell = Some(v));
        machine_shells_section(app, Some(tg), Some((value, f)))
    });
    let save = {
        let on_save = on_save.clone();
        let existing = existing.clone();
        move |app: &mut App| {
            match validate_ssh_machine(&name, &host, &identity, &port, creating, machine_count) {
                Err(e) => dialog_state(app).error = Some(e),
                Ok(parsed_port) => {
                    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    // The host's own backend type isn't reachable from here, so the record goes through its wire form.
                    let previous = existing.as_ref().and_then(|m| serde_json::to_value(m).ok());
                    let agent = chosen_shell.and_then(|b| serde_json::to_value(b).ok()).or_else(|| previous.as_ref().and_then(|p| p.get("agentShell").cloned()));
                    let mut record = json!({
                        "id": existing.as_ref().map_or_else(|| create_id("sshm"), |m| m.id.clone()),
                        "name": name.trim(),
                        "host": host.trim(),
                        "port": parsed_port,
                        "identityFile": identity.trim(),
                        "createdAt": existing.as_ref().map_or_else(|| now.clone(), |m| m.created_at.clone()),
                        "updatedAt": now,
                    });
                    if let Some(agent) = agent {
                        record["agentShell"] = agent;
                    }
                    let Ok(saved) = serde_json::from_value::<SshMachineConfig>(record) else { return };
                    on_save(app, saved);
                }
            }
        }
    };
    let close_cancel = on_close.clone();
    let danger = theme().critical;
    // Delete stands at the far end of the row from the other two: `margin-right: auto`, which here would take the row's gap with it.
    let footer = row()
        .flex1()
        .gap(8.0)
        .child(on_delete.map(|f| button::<App>("").key("ssh-delete").child(text(t("删除", "Delete")).ty(BODY_SMALL).color(danger).nowrap()).when(in_use_note.is_some(), |e| e.tooltip(in_use_note.clone().unwrap_or_default())).on_click(move |a: &mut App| f(a))))
        .child(div().flex1())
        .child(button::<App>(t("取消", "Cancel")).key("ssh-cancel").on_click(move |a: &mut App| close_cancel(a)))
        .child(accent_button::<App>(t("保存", "Save")).key("ssh-save").on_click(save));
    let body = col().child(form).child(shells).child(error.map(error_line));
    modal("ssh-machine", if creating { t("添加 SSH 机器", "Add SSH machine") } else { t("配置 SSH 机器", "Configure SSH machine") }, Some(help_with_docs(t("通过 OpenSSH 连接，Mewrk 不保存认证信息。", "Connects through OpenSSH; Mewrk stores no credentials."), "workspaces", "add-ssh")), 460.0, body, Some(footer), move |a| on_close(a))
}

/// The window.
pub fn machine_settings_dialog(app: &mut App, props: MachineSettingsProps) -> E {
    let MachineSettingsProps { machine, ssh_machines, projects, conversations, on_set_wsl_agent_shell, on_save_ssh_machine, on_delete_ssh_machine, on_close } = props;
    let in_use = projects > 0 || conversations > 0;
    let usage_text = if in_use { tf("{projects} 个项目、{conversations} 个对话在这台机器上有工作区", "{projects} projects and {conversations} conversations have workspaces on this machine", &[("projects", &projects), ("conversations", &conversations)]) } else { t("还没有工作区在这台机器上", "No workspace is on this machine yet").to_owned() };
    let distro_name = match &machine {
        Some(RunTarget::Wsl { distro }) => Some(distro.clone()),
        _ => None,
    };
    let ssh = match &machine {
        Some(RunTarget::Ssh { machine_id }) => ssh_machines.iter().find(|m| &m.id == machine_id).cloned(),
        _ => None,
    };
    // An SSH machine's dialog starts itself over, delete's confirmation aside; the rest is this window's.
    let window_key = run_env_key(machine.as_ref());
    if just_opened(&format!("window:{window_key}")) {
        let s = dialog_state(app);
        let mount = s.mount + 1;
        *s = DialogState { mount, ..DialogState::default() };
        // Distributions are machine state, so they are enumerated live rather than stored.
        if distro_name.is_some() {
            app.invoke::<Vec<WslDistro>>("list_wsl_distros", json!({}), move |app, result| {
                let s = dialog_state(app);
                if s.mount == mount
                    && let Ok(list) = result
                {
                    s.distros = Some(list);
                }
            });
        }
    }
    if let Some(ssh) = ssh {
        if dialog_state(app).pending_delete {
            let (keep_a, keep_b) = (on_close.clone(), ssh.id.clone());
            let delete = on_delete_ssh_machine.clone();
            let footer = row().gap(8.0).child(button::<App>(t("取消", "Cancel")).key("ssh-delete-cancel").on_click(|app: &mut App| dialog_state(app).pending_delete = false)).child(danger_button::<App>(t("删除", "Delete")).key("ssh-delete-confirm").on_click(move |a: &mut App| {
                dialog_state(a).pending_delete = false;
                delete(a, keep_b.clone());
                keep_a(a);
            }));
            let copy = if in_use {
                tf(
                    "{projects} 个项目、{conversations} 个对话仍在使用这台机器。删除后，它们在这台机器上的工作区（包括项目的第一个工作区）都会显示「已删除的机器」并停止工作，直到在「编辑项目…」里或重新附加时为它们重新选择机器和目录；这些工作区的环境变量和沙箱设置也会一并删除。",
                    "{projects} projects and {conversations} conversations still use this machine. Their workspaces on it, a project's first workspace included, will show Deleted machine and stop working until a machine and directory are chosen for them again in Edit project… or by attaching them again; those workspaces' environment variables and sandbox settings are deleted too.",
                    &[("projects", &projects), ("conversations", &conversations)],
                )
            } else {
                t("没有项目或对话在使用这台机器。", "No project or conversation uses this machine.").to_owned()
            };
            return modal("ssh-delete", &tf("删除 SSH 机器“{name}”？", "Delete SSH machine “{name}”?", &[("name", &ssh.name)]), None, 420.0, text(copy).ty(CAPTION).color(theme().text_secondary), Some(footer), |app| dialog_state(app).pending_delete = false);
        }
        let save_close = on_close.clone();
        let save = on_save_ssh_machine.clone();
        let on_save: Rc<dyn Fn(&mut App, SshMachineConfig)> = Rc::new(move |a, m| {
            save(a, m);
            save_close(a);
        });
        let on_delete: Rc<dyn Fn(&mut App)> = Rc::new(|a| dialog_state(a).pending_delete = true);
        return ssh_machine_dialog(app, Some(&ssh), in_use.then_some(usage_text), ssh_machines.len(), on_save, Some(on_delete), on_close);
    }
    let title = match &machine {
        None => t("本机", "This machine").to_owned(),
        Some(RunTarget::Wsl { distro }) => distro.clone(),
        Some(RunTarget::Ssh { .. }) => t("已删除的机器", "Deleted machine").to_owned(),
    };
    let description = match &machine {
        None => t("运行 Mewrk 的这台电脑。", "The computer running Mewrk."),
        Some(RunTarget::Wsl { .. }) => t("这台电脑上的一个 WSL 发行版。", "A WSL distribution on this computer."),
        Some(RunTarget::Ssh { .. }) => t("这台 SSH 机器已删除，其上的工作区需重新选择才能使用。", "This SSH machine was deleted; choose another for its workspaces."),
    };
    let distro = distro_name.as_ref().and_then(|name| dialog_state(app).distros.as_ref().and_then(|l| l.iter().find(|d| &d.name == name).cloned()));
    let kind = match &machine {
        None => t("本机", "This machine").to_owned(),
        Some(RunTarget::Wsl { .. }) => match &distro {
            Some(d) if d.is_default => tf("WSL {version} · 默认", "WSL {version} · default", &[("version", &d.version)]),
            Some(d) => tf("WSL {version}", "WSL {version}", &[("version", &d.version)]),
            None => "WSL".to_owned(),
        },
        Some(RunTarget::Ssh { .. }) => "SSH".to_owned(),
    };
    let shells = matches!(machine, None | Some(RunTarget::Wsl { .. })).then(|| {
        let chosen = {
            let probes = app.machine_shells().clone();
            effective_agent_shell(machine.as_ref(), &app.doc.assets.execution_environments, &probes)
        };
        let agent = matches!(machine, Some(RunTarget::Wsl { .. })).then(|| (chosen, on_set_wsl_agent_shell));
        machine_shells_section(app, machine.clone(), agent)
    });
    let close_footer = on_close.clone();
    let t_ = theme();
    // `.machine-settings__facts`: a column of names as wide as the longest, and what they say beside them.
    let fact_name = |label: &'static str| text(label).ty(CAPTION).color(t_.text_secondary).nowrap();
    let facts = row().gap(14.0).child(col().shrink0().gap(7.0).child(fact_name(t("类型", "Kind"))).child(fact_name(t("工作区", "Workspaces")))).child(col().flex1().min_w(0.0).gap(7.0).child(text(kind).ty(CAPTION)).child(text(usage_text).ty(CAPTION)));
    let note = col().mt(14.0).py(9.0).px(10.0).radius(8.0).bg(t_.card_secondary).child(help_with_docs(t("环境变量和沙箱属于工作区，在工作区旁的齿轮里设置。", "Environment variables and the sandbox belong to a workspace; set them from its gear."), "workspaces", "environment"));
    let body = col().child(facts).child(shells).child(note);
    modal("machine-settings", &title, Some(help(description)), 420.0, body, Some(button::<App>(t("关闭", "Close")).key("machine-close").on_click(move |a: &mut App| close_footer(a))), move |a| on_close(a))
}
