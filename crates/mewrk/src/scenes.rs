//! States to draw for a picture, from `--scene NAME`: every dialog and pane arrangement, reachable headless.

use mewrk_client::model::RunTarget;
use mewrk_client::prelude::*;

use crate::actions::{open_pane, show_tasks_tab};
use crate::state::{shell, ConfirmationRequest, RemoteWorkspacePicker, SshHostKey, SshPrompt, SshPromptKind};

pub const SCENES: &[&str] = &["landing", "panes", "terminal", "solo", "solo-glass", "screen", "page", "tasks-history", "search", "confirm", "rewind", "new-project", "edit-project", "ssh-hostkey", "ssh-secret", "ssh-confirm", "remote-picker", "path-choice", "machine", "ssh-machine", "settings", "settings-glass", "background-library", "providers", "updates", "glass", "glass-clear", "glass-motion", "permissions", "first-launch"];

/// Conversations to draw, in the document's first project: made through the host, as a stored one would have been.
fn ensure_demo(app: &mut App) {
    let Some(workspace) = app.doc.workspaces.first().map(|workspace| workspace.id.clone()) else { return };
    if !app.doc.workspaces[0].conversations.is_empty() || app.doc.presets.conversation_presets.is_empty() {
        if let Some(id) = app.doc.workspaces[0].conversations.first().map(|conversation| conversation.id.clone()) {
            app.set_active(Some(&workspace), Some(&id));
        }
        return;
    }
    let preset = app.doc.presets.conversation_presets[0].clone();
    let Ok(settings) = serde_json::from_value::<mewrk_client::model::ConversationSettings>(json!({ "enabledTools": [] })) else { return };
    let settings = mewrk_client::presets::apply_conversation_preset_settings(&settings, &preset.settings, None);
    let mut last = None;
    for (id, title, user, reply) in [("scene-3", "Write the release notes", "Draft the notes for this week.", "Here is a first pass."), ("scene-2", "Fix the flaky upload test", "It times out on CI once in ten runs.", "The retry never resets its deadline."), ("scene-1", "Port the sidebar to Rust", "Start with the status marks.", "The mark is a template image with four states.")] {
        let contexts = json!([{ "kind": "user", "id": format!("{id}-u"), "content": user, "createdAt": "2026-11-01T00:00:00.000Z" }, { "kind": "assistant", "id": format!("{id}-a"), "content": reply, "createdAt": "2026-11-01T00:00:01.000Z" }]);
        let conversation = json!({ "id": id, "title": title, "createdAt": "2026-11-01T00:00:00.000Z", "updatedAt": "2026-11-01T00:00:00.000Z", "settings": settings, "contexts": contexts, "presetId": preset.id });
        let Ok(created) = app.host.invoke_blocking("create_conversation", json!({ "workspaceId": workspace, "conversation": conversation })) else { continue };
        let Ok(created) = serde_json::from_value::<mewrk_client::model::Conversation>(created) else { continue };
        let target = workspace.clone();
        last = Some(created.id.clone());
        app.update_document(move |doc| {
            if let Some(entry) = doc.workspaces.iter_mut().find(|entry| entry.id == target) {
                entry.conversations.insert(0, created);
            }
        });
    }
    if let Some(id) = last {
        app.set_active(Some(&workspace), Some(&id));
    }
}

/// A rebuilt copy, the state that has the most to say. Written in rather than asked for, so a picture looks the same anywhere.
fn permission_answers(app: &mut App) {
    use mewrk_settings_global::permissions_page::{state, DiskAccess, KeychainAccess, PermissionsSnapshot};
    let s = state(app);
    s.started = true;
    s.snapshot = Some(PermissionsSnapshot { full_disk_access: DiskAccess::Denied, keychain: KeychainAccess::NeedsPermission, app_bundle: Some("/Applications/Mewrk.app".to_owned()), ad_hoc_signed: true, prompted: false });
}

/// Sets the state up. Called once, after `init`, and answers whether it knew the name.
pub fn apply(app: &mut App, name: &str) -> bool {
    if name != "landing" {
        ensure_demo(app);
    }
    match name {
        "landing" => {}
        "panes" => {
            open_pane(app, "tasks");
            open_pane(app, "settings");
        }
        "terminal" => open_pane(app, "terminal"),
        // One pane shown alone over the conversation and the other pane, which stay mounted under it.
        "solo" | "solo-glass" => {
            if name == "solo-glass" {
                app.doc.global_settings.appearance.liquid_glass = true;
                app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
            }
            open_pane(app, "tasks");
            open_pane(app, "settings");
            if let Some(conversation_id) = app.active_conversation_id() {
                app.dispatch_side_panes(mewrk_client::side_panes::SidePanesAction::ToggleExpand { conversation_id, pane: "tasks".to_owned() });
            }
        }
        // A remote screen in the preview pane, the way the model's `preview_start` puts it there: `MEWRK_SCENE_ENTRY` names the
        // entry of `.mewrk/launch.json` in the first project.
        "screen" => {
            let Some(conversation_id) = app.active_conversation_id() else { return true };
            let owner = app.host_conversation_id(&conversation_id);
            let entry = std::env::var("MEWRK_SCENE_ENTRY").unwrap_or_else(|_| "desk".to_owned());
            let started = app.host.invoke_blocking("preview_start_server", json!({ "target": { "conversationId": owner }, "name": entry }));
            match started.map(|outcome| outcome.pointer("/server/url").and_then(Value::as_str).map(str::to_owned).ok_or(outcome)) {
                Ok(Ok(url)) => app.host.request_screen(&owner, url),
                other => eprintln!("scene screen: the entry did not start: {other:?}"),
            }
            app.dispatch_side_panes(mewrk_client::side_panes::SidePanesAction::RegisterPreview { conversation_id: conversation_id.clone(), session_id: owner.clone(), workspace: None });
            app.dispatch_side_panes(mewrk_client::side_panes::SidePanesAction::Open { conversation_id: conversation_id.clone(), pane: mewrk_client::side_panes::preview_pane_id(&owner) });
        }
        // A real page in the preview pane, at `MEWRK_SCENE_URL`. Pages need a window, so this is one to run without `--headless`.
        "page" => {
            let url = std::env::var("MEWRK_SCENE_URL").ok();
            crate::actions::open_browser_tab(app, None, url, |_, result| {
                if let Err(error) = result {
                    eprintln!("scene page: {error}");
                }
            });
        }
        "tasks-history" => show_tasks_tab(app, mewrk_client::side_panes::TasksPaneTab::History),
        "search" => shell(app).search_open = true,
        "confirm" => {
            shell(app).confirmation = Some(ConfirmationRequest { question: t("删除这条消息？", "Delete this message?").to_owned(), detail: Some(t("这条消息和它之后的内容都会从时间线上移除。", "This message and what follows it leave the timeline.").to_owned()), confirm_label: t("删除", "Delete").to_owned(), destructive: true, on_confirm: Rc::new(|_| {}) });
        }
        "rewind" => {
            if let Some(id) = app.active_conversation_id() {
                let context = app.conversation(&id).and_then(|conversation| conversation.contexts.first().cloned());
                if let Some(context) = context {
                    mewrk_ui_chat::handlers::open_rewind_dialog(app, &context, None);
                }
            }
        }
        "new-project" => app.set_workspace_dialog_open(true, false),
        "edit-project" => {
            let project = app.doc.workspaces.first().map(|workspace| workspace.id.clone());
            app.set_project_editor(project);
        }
        "ssh-hostkey" => push_prompt(app, SshPromptKind::HostKey, "The authenticity of host 'build' can't be established.", Some(SshHostKey { host: "build.example".into(), key_type: "ED25519".into(), fingerprint: "SHA256:u6F0p3bT4qWm3l9oH2n7hXk1bZ8yE5rA".into() })),
        "ssh-secret" => push_prompt(app, SshPromptKind::Secret, "me@build's password:", None),
        "ssh-confirm" => push_prompt(app, SshPromptKind::Confirm, "Allow use of key?", None),
        "remote-picker" => shell(app).remote_picker = Some(RemoteWorkspacePicker { machine: RunTarget::Wsl { distro: "Ubuntu".into() }, name: "Ubuntu".into() }),
        "path-choice" => {
            let row = |id: &str, path: &str, is_directory: bool| crate::state::PathChoiceRow { id: id.to_owned(), label: path.to_owned(), label_is_path: true, description: path.to_owned(), is_directory, machine: None, path: path.to_owned(), line: None, new_page: false };
            let rows = vec![row("a", "/work/one/src/main.rs", false), row("b", "/work/two/package.json", false), row("c", "/work/three/src", true)];
            // After the first frame, whose activating the conversation closes any menu of this kind. Draw it with `--frames 60`.
            app.spawn(move |task| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                task.post(move |app| crate::dialogs::path_choice::show_path_choice(app, crate::state::PathChoiceAnchor::Point(320.0, 160.0), rows, Some(t("在哪个工作区打开", "Open in which workspace").to_owned())));
            });
        }
        "machine" => shell(app).machine_settings = Some(None),
        "ssh-machine" => {
            let machine: mewrk_client::model::SshMachineConfig = serde_json::from_value(serde_json::json!({ "id": "sshm_scene", "name": "build", "host": "me@build.example", "port": 2222, "identityFile": "~/.ssh/id_ed25519", "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z" })).expect("the scene's machine");
            let id = machine.id.clone();
            app.doc.assets.execution_environments.ssh_machines.push(machine);
            shell(app).machine_settings = Some(Some(RunTarget::Ssh { machine_id: id }));
        }
        "settings" => crate::actions::open_global_settings(app, "appearance"),
        "settings-glass" => {
            app.doc.global_settings.appearance.liquid_glass = true;
            app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
            crate::actions::open_global_settings(app, "appearance");
        }
        "background-library" => {
            crate::actions::open_global_settings(app, "appearance");
            app.local::<mewrk_settings_global::appearance::AppearancePage>().library_open = true;
        }
        "permissions" => {
            permission_answers(app);
            crate::actions::open_global_settings(app, "permissions");
        }
        "first-launch" => {
            permission_answers(app);
            mewrk_settings_global::permissions_page::state(app).first_launch_open = true;
        }
        // Opens on Claude Agent, the seed's first row. `MEWRK_CLAUDE_BIN` picks what its panel finds.
        "providers" => crate::actions::open_global_settings(app, "providers"),
        "updates" => crate::actions::open_global_settings(app, "updates"),
        "glass" => {
            app.doc.global_settings.appearance.liquid_glass = true;
            app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
            open_pane(app, "tasks");
            open_pane(app, "files");
        }
        // The far end of both settings: most of the picture through, hardly blurred.
        "glass-clear" => {
            app.doc.global_settings.appearance.liquid_glass = true;
            app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
            app.doc.global_settings.appearance.glass_transparency = 70;
            app.doc.global_settings.appearance.glass_blur = 4;
            open_pane(app, "tasks");
        }
        // A background that moves, from the file `MEWRK_SCENE_BACKGROUND` names. It is imported as if picked, so it shows once that's done.
        "glass-motion" => {
            app.doc.global_settings.appearance.liquid_glass = true;
            app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
            open_pane(app, "tasks");
            if let Some(path) = std::env::var_os("MEWRK_SCENE_BACKGROUND") {
                mewrk_settings_global::appearance::import_background_file(app, path.into());
            }
        }
        "glass-solid" => {
            app.doc.global_settings.appearance.liquid_glass = true;
            app.doc.global_settings.appearance.background = "solid".to_owned();
            open_pane(app, "tasks");
        }
        "spell" => {
            app.doc.global_settings.appearance.liquid_glass = false;
            app.doc.global_settings.appearance.background = "solid".to_owned();
            app.doc.global_settings.appearance.spell_check = true;
            if let Some(id) = app.active_conversation_id() {
                app.set_composer_draft(&id, "This sentense has teh mispelled words, and 拼写 is left alone.\nA secnd line, and one still being writen".to_owned());
            }
        }
        _ => return false,
    }
    repaint();
    true
}

fn push_prompt(app: &mut App, kind: SshPromptKind, prompt: &str, host_key: Option<SshHostKey>) {
    shell(app).ssh_prompts.push(SshPrompt { id: "scene".into(), machine: "build".into(), kind, prompt: prompt.to_owned(), host_key, retry: false });
}
