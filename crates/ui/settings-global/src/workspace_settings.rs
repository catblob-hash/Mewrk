//! `WorkspaceSettings.tsx`: a workspace's settings, opened from the gear beside it, named after the workspace. Its one page
//! is the environment its commands run in. Changes apply as they are made.

use std::collections::BTreeMap;

use mewrk_client::model::{RunTarget, SandboxSettings, SshMachineConfig};
use mewrk_client::prelude::*;
use mewrk_client::workspaces::{workspace_location_title, workspace_machine_label};

use crate::logic::workspace_env::{env_text_error, format_env_text, parse_env_text};
use crate::sandbox;
use crate::widgets::*;

pub struct WorkspaceSettingsProps {
    /// The directory's label, which names the window.
    pub name: String,
    /// `None` is this computer.
    pub machine: Option<RunTarget>,
    pub path: String,
    pub ssh_machines: Vec<SshMachineConfig>,
    pub vars: BTreeMap<String, String>,
    /// Absent when the workspace's sandbox has never been set: off.
    pub sandbox: Option<SandboxSettings>,
    pub on_change_vars: Rc<dyn Fn(&mut App, BTreeMap<String, String>)>,
    pub on_change_sandbox: Rc<dyn Fn(&mut App, SandboxSettings)>,
    pub on_close: Rc<dyn Fn(&mut App)>,
}

/// A workspace's variables, one `KEY=value` per line. Each draft that parses is saved, one that does not keeps the last
/// that did, and the problem is shown once the field is left rather than at every keystroke of a half-typed line.
fn environment_variables_card(scope: u64, vars: &BTreeMap<String, String>, on_change: Rc<dyn Fn(&mut App, BTreeMap<String, String>)>) -> E {
    let text_state = use_state(("env-text", scope), || format_env_text(vars));
    let problem = use_state(("env-problem", scope), || None::<String>);
    let shown = use_state(("env-shown", scope), || false);
    let (t_change, p_change, s_change, s_blur) = (text_state.clone(), problem.clone(), shown.clone(), shown.clone());
    let field_el = field::<App>(("env-input", scope), text_state.get())
        .multiline(160.0, 320.0)
        .ty(MONO_BODY)
        .placeholder("API_KEY=value\nHTTP_PROXY=http://127.0.0.1:7890")
        .on_change(move |app, next| {
            let parsed = parse_env_text(&next);
            let message = env_text_error(&parsed);
            t_change.set(next);
            s_change.set(false);
            if message.is_none() {
                on_change(app, parsed.vars);
            }
            p_change.set(message);
        })
        .on_blur(move |_, _| s_blur.set(true))
        .build()
        .aria_label(t("环境变量", "Environment variables"))
        .aria_invalid(shown.get() && problem.get().is_some());
    let error = (shown.get()).then(|| problem.get()).flatten().map(|p| error_line(tf("{problem}。在改好之前，保存的仍是上一版。", "{problem}. Until it is fixed, the last version that was valid stays saved.", &[("problem", &p)])));
    card(None).mt(0.0).gap(9.0).child(help_with_docs(t("每行一条 KEY=value，用于这个工作区的命令。", "One KEY=value per line, set for this workspace's commands."), "workspaces", "environment")).child(field_el).child(error)
}

/// The window.
pub fn workspace_settings_dialog(app: &mut App, props: WorkspaceSettingsProps) -> E {
    let WorkspaceSettingsProps { name, machine, path, ssh_machines, vars, sandbox: sandbox_settings, on_change_vars, on_change_sandbox, on_close } = props;
    let machine_name = workspace_machine_label(machine.as_ref(), &ssh_machines).unwrap_or_else(|| t("本机", "This machine").to_owned());
    let location = workspace_location_title(&path, machine.as_ref(), &ssh_machines);
    let scope = hash_key(&location);
    let section = |s: &'static str, top: f32| text(s).ty(BODY_SMALL_STRONG).mt(top).mb(8.0).nowrap();
    let page_el = page()
        .child(page_heading(Some(page_title(t("环境", "Environment"))), page_description(tf("{location} 里的命令怎样运行：沙箱和环境变量。", "How commands run in {location}: sandbox and environment variables.", &[("location", &location)])), None))
        .child(section(t("沙箱", "Sandbox"), 4.0))
        .child(sandbox::sandbox_settings(app, machine, &machine_name, &path, sandbox_settings, on_change_sandbox))
        .child(section(t("环境变量", "Environment variables"), 22.0))
        .child(environment_variables_card(scope, &vars, on_change_vars));
    let close = on_close.clone();
    let nav = settings_nav(t("工作区设置分类", "Workspace settings categories"), &name, vec![(None, vec![(0u8, "Container", t("环境", "Environment"))])], 0u8, |v| v as u64 + 1, |_, _| {});
    let (width, height) = settings_window_size();
    let x = icon_button::<App>("X", tf("关闭 {title}", "Close {title}", &[("title", &name)])).key("workspace-settings-close").absolute().top(8.0).right(12.0).on_click(move |a: &mut App| close(a));
    let body = col().h(height).w_full().child(settings_layout(t("工作区设置", "Workspace settings"), nav, col().key(("workspace-settings-page", scope)).flex1().min_w(0.0).min_h(0.0).scroll_y().child(page_el)).bg(theme().card_secondary)).child(x);
    dialog(width, body, Some(Rc::new(move |a: &mut App| on_close(a)))).aria_label(name)
}
