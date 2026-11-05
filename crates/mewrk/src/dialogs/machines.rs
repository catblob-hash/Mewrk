//! `saveSshMachine`, `deleteSshMachine` and the windows a gear opens beside a machine or a workspace. The windows themselves
//! are the global settings crate's; this is the state `App.tsx` kept for them.

use std::collections::BTreeMap;

use mewrk_client::machine_shells::{with_agent_shell, ShellBackend};
use mewrk_client::model::{RunTarget, SandboxSettings, SshMachineConfig};
use mewrk_client::prelude::*;
use mewrk_client::workspaces::{machine_usage, run_env_key, workspace_env_key};
use mewrk_settings_global::{machine_settings_dialog, workspace_settings_dialog, MachineSettingsProps, WorkspaceSettingsProps};

use crate::state::shell;

/// `saveSshMachine`. A machine is probed as soon as it is added, which is when its first probe records its agent shell, and
/// again when it now names another endpoint, whose shells the old answer says nothing about.
pub fn save_ssh_machine(app: &mut App, machine: SshMachineConfig) {
    let previous = app.doc.assets.execution_environments.ssh_machines.iter().find(|item| item.id == machine.id).cloned();
    let endpoint_changed = previous.as_ref().is_none_or(|p| p.host != machine.host || p.port != machine.port || p.identity_file != machine.identity_file);
    let id = machine.id.clone();
    app.update_document_if(move |doc| {
        let machines = &mut doc.assets.execution_environments.ssh_machines;
        match machines.iter_mut().find(|item| item.id == machine.id) {
            Some(existing) => *existing = machine,
            None => machines.push(machine),
        }
        true
    });
    if endpoint_changed {
        let target = RunTarget::Ssh { machine_id: id };
        app.forget_machine_probe(&target);
        app.flush_latest_document(false, move |app, _| app.probe_machine_shells(Some(target), |_, _| {}));
    }
}

/// `deleteSshMachine`: removes an SSH machine from the catalog, and the variables and sandbox of every workspace on it with it:
/// a machine registered again gets a new id, so those entries could never be reached. Workspaces on it stay where they are and
/// read as a deleted machine until chosen again.
pub fn delete_ssh_machine(app: &mut App, machine_id: &str) {
    let prefix = format!("{}|", run_env_key(Some(&RunTarget::Ssh { machine_id: machine_id.to_owned() })));
    app.forget_machine_probe(&RunTarget::Ssh { machine_id: machine_id.to_owned() });
    let id = machine_id.to_owned();
    app.update_document_if(move |doc| {
        let environments = &mut doc.assets.execution_environments;
        environments.ssh_machines.retain(|machine| machine.id != id);
        environments.env_vars.retain(|key, _| !key.starts_with(&prefix));
        environments.sandboxes.retain(|key, _| !key.starts_with(&prefix));
        true
    });
}

/// `saveWorkspaceEnvVars`: an empty table removes its entry.
pub fn save_workspace_env_vars(app: &mut App, machine: Option<&RunTarget>, path: &str, vars: BTreeMap<String, String>) {
    let key = workspace_env_key(machine, path);
    app.update_document_if(move |doc| {
        let env_vars = &mut doc.assets.execution_environments.env_vars;
        if vars.is_empty() {
            env_vars.remove(&key);
        } else {
            env_vars.insert(key, vars);
        }
        true
    });
}

/// `saveWorkspaceSandbox`: the entry stays when it is switched off, since it records that answer.
pub fn save_workspace_sandbox(app: &mut App, machine: Option<&RunTarget>, path: &str, sandbox: SandboxSettings) {
    let key = workspace_env_key(machine, path);
    app.update_document_if(move |doc| {
        doc.assets.execution_environments.sandboxes.insert(key, sandbox);
        true
    });
}

/// `setMachineAgentShell`.
pub fn set_machine_agent_shell(app: &mut App, machine: RunTarget, backend: ShellBackend) {
    app.update_document_if(move |doc| with_agent_shell(&mut doc.assets.execution_environments, &machine, backend));
}

/// `MachineSettingsDialog` for a machine, with `App.tsx`'s handlers.
pub fn machine_window(app: &mut App, machine: Option<RunTarget>, on_close: Handler<App>, on_deleted: Option<Rc<dyn Fn(&mut App, String)>>) -> E {
    let usage = machine_usage(&app.doc.workspaces, machine.as_ref());
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let wsl_machine = machine.clone();
    machine_settings_dialog(
        app,
        MachineSettingsProps {
            machine,
            ssh_machines,
            projects: usage.projects,
            conversations: usage.conversations,
            on_set_wsl_agent_shell: Rc::new(move |app: &mut App, backend| {
                if let Some(machine) = wsl_machine.clone() {
                    set_machine_agent_shell(app, machine, backend);
                }
            }),
            on_save_ssh_machine: Rc::new(save_ssh_machine),
            on_delete_ssh_machine: Rc::new(move |app: &mut App, id: String| {
                delete_ssh_machine(app, &id);
                if let Some(f) = &on_deleted {
                    f(app, id);
                }
            }),
            on_close,
        },
    )
}

/// `machineSettings`.
pub fn machine_settings_window(app: &mut App) -> Option<E> {
    let machine = shell(app).machine_settings.clone()?;
    Some(machine_window(app, machine, Rc::new(|app: &mut App| shell(app).machine_settings = None), None))
}

/// `workspaceSettings`.
pub fn workspace_settings_window(app: &mut App) -> Option<E> {
    let target = shell(app).workspace_settings.clone()?;
    let key = workspace_env_key(target.machine.as_ref(), &target.path);
    let environments = &app.doc.assets.execution_environments;
    let (ssh_machines, vars, sandbox) = (environments.ssh_machines.clone(), environments.env_vars.get(&key).cloned().unwrap_or_default(), environments.sandboxes.get(&key).cloned());
    let (vars_machine, vars_path, sandbox_machine, sandbox_path) = (target.machine.clone(), target.path.clone(), target.machine.clone(), target.path.clone());
    Some(workspace_settings_dialog(
        app,
        WorkspaceSettingsProps {
            name: target.name,
            machine: target.machine,
            path: target.path,
            ssh_machines,
            vars,
            sandbox,
            on_change_vars: Rc::new(move |app: &mut App, vars| save_workspace_env_vars(app, vars_machine.as_ref(), &vars_path, vars)),
            on_change_sandbox: Rc::new(move |app: &mut App, sandbox| save_workspace_sandbox(app, sandbox_machine.as_ref(), &sandbox_path, sandbox)),
            on_close: Rc::new(|app: &mut App| shell(app).workspace_settings = None),
        },
    ))
}
