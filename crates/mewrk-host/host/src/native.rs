//! The host as the native client sees it: start it, then call any of its commands by name with JSON arguments, the
//! same names and argument shapes the upstream renderer uses over Tauri's IPC. A channel argument is written
//! `{"__mewrkChannel": "<id>"}` and its messages reach the client's sink under that id.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    AppConfig, AppHandle, Manager,
};

use super::{
    app_exit::AppExitCoordinator, browser_renderer_mount::BrowserRendererMountLease, document_store, helper_model,
    instance_activation, model::ModelStreamEvent, state::AppState, terminal::TerminalEvent, ui_text,
};

pub use super::remote_control::driver::{Platform as ScreenPlatform, ScreenUpdate};
pub use super::remote_control::frame::LiveScreen;
pub use super::remote_control::session::RemoteControlSession;
pub use super::remote_control::view::{PointerPhase, SessionPhase, ViewerCredentials, ViewerEvent, ViewerState};
pub use zeroize::Zeroizing;

/// Where messages sent on a channel go: the channel's id and the message.
pub type ChannelSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

pub struct HostConfig {
    pub identifier: String,
    pub version: String,
    pub data_dir: PathBuf,
    pub local_data_dir: PathBuf,
    pub resource_dir: Option<PathBuf>,
    /// Runs a closure on the UI thread.
    pub run_on_main: Option<Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>>,
    /// The process is done: the barrier has passed.
    pub exit: Option<Arc<dyn Fn(i32) + Send + Sync>>,
    /// Bring the window back: a second launch asked, or a dialog has to be seen.
    pub show_window: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// What can stop the host from starting. Another Mewrk holding the data folder is the usual one.
#[derive(Debug)]
pub enum StartError {
    AnotherInstanceAnswered,
    Unavailable(String),
}

#[derive(Clone)]
pub struct Host {
    app: AppHandle,
    exit_coordinator: AppExitCoordinator,
    renderer_mount: BrowserRendererMountLease,
    sink: ChannelSink,
}

/// Before any thread exists: the checks that want a single-threaded process.
pub fn prepare_process() {
    super::host_platform::resolve_at_startup();
    #[cfg(unix)]
    super::process_groups::start_watchdog();
    super::child_environment::restore_dev_application_path();
    super::child_environment::adopt_login_shell_path();
    #[cfg(target_os = "macos")]
    super::credential_vault::install();
    #[cfg(unix)]
    super::restrict_user_data_directory();
}

impl Host {
    /// What `run`'s setup does upstream, in its order, with no window to create at the end of it.
    pub fn start(config: HostConfig, sink: ChannelSink) -> Result<Self, StartError> {
        let HostConfig { identifier, version, data_dir, local_data_dir, resource_dir, run_on_main, exit, show_window } = config;
        std::fs::create_dir_all(&data_dir).map_err(|e| StartError::Unavailable(e.to_string()))?;
        std::fs::create_dir_all(&local_data_dir).map_err(|e| StartError::Unavailable(e.to_string()))?;
        let app = AppHandle::new(AppConfig { identifier, version, data_dir, local_data_dir, resource_dir });
        if let Some(run) = run_on_main {
            app.set_main_thread_runner(move |f| run(f));
        }
        if let Some(exit) = exit {
            app.set_exit_hook(move |code| exit(code));
        }
        super::app_tray::install_native(show_window.clone());
        app.manage(AppState::default());
        let exit_coordinator = AppExitCoordinator::default();

        let unavailable = |e: String| StartError::Unavailable(e);
        let path = super::document_path(&app).map_err(unavailable)?;
        let store = app.state::<AppState>().document_store.clone();
        match instance_activation::negotiate_launch(&path, super::PROCESS_LEASE_HANDOVER_WAIT, || store.acquire_process_authority(&path)) {
            instance_activation::LaunchHandover::Acquired => {}
            instance_activation::LaunchHandover::ResidentSignaled => return Err(StartError::AnotherInstanceAnswered),
            instance_activation::LaunchHandover::Unresolved(error) => {
                let mut message = error.to_string();
                if error == document_store::ProcessAuthorityError::Contended {
                    message.push_str("

");
                    message.push_str(&super::still_running_hint(ui_text::current()));
                }
                return Err(StartError::Unavailable(message));
            }
        }
        super::migrate_legacy_app_data(&app).map_err(unavailable)?;
        if let Ok(local_data) = app.path().app_local_data_dir() {
            super::components::initialize(&local_data);
            super::components::aisdk::spawn_startup_check();
        }
        if let Some(app_data) = path.parent() {
            app.state::<AppState>().install_attestation_key(app_data).map_err(unavailable)?;
            match super::conversations::store(&path) {
                Ok(store) => {
                    let state = app.state::<AppState>();
                    state.tool_prompts().attach_allowance_store(store.clone());
                    state.file_read_state.attach_store(store);
                }
                Err(error) => eprintln!("Could not open the conversation store for saved answers and reads: {error}"),
            }
            super::browser_file_preview::sweep_orphans(app_data);
            super::install_machine_links(&app, app_data);
        }
        super::reconcile_image_attachments_on_startup(&app).map_err(unavailable)?;
        if let Ok(local_data) = app.path().app_local_data_dir() {
            let state = app.state::<AppState>();
            state.helper_model.initialize(helper_model::root_dir(&local_data), state.push_events.clone());
        }
        super::install_background_write_failure_reporting(app.state::<AppState>().inner());
        app.state::<AppState>().browser.attach_app(app.clone()).map_err(unavailable)?;
        {
            let state = app.state::<AppState>();
            super::start_browser_renderer_mount_watchdog(state.inner()).map_err(unavailable)?;
        }
        super::start_claude_agent_login_keepalive(&app);
        {
            let state = app.state::<AppState>();
            state.document_store.mark_ipc_ready().map_err(unavailable)?;
            if let Ok(document) = state.document_store.current_snapshot(&path) {
                super::apply_ui_language(&document);
            }
        }
        // A second launch asks this one for its window instead of failing on the data lease.
        if let Some(show) = show_window {
            match instance_activation::listen(&path, move || show()) {
                Ok(listener) => {
                    app.manage(listener);
                }
                Err(error) => eprintln!("实例激活监听不可用，重复启动将只提示已在运行：{error}"),
            }
        }
        let renderer_mount = app.state::<AppState>().browser_renderer_mounts.register_new_mount().map_err(super::browser_renderer_mount_error).map_err(unavailable)?;
        Ok(Self { app, exit_coordinator, renderer_mount, sink })
    }

    /// Calls a command by name with its renderer-side arguments, on the host's runtime.
    pub async fn invoke(&self, command: &str, args: Value) -> Result<Value, String> {
        dispatch(self.app.clone(), self.renderer_mount.clone(), command, &args, self.sink.clone()).await
    }

    /// The same, for a caller that cannot wait.
    pub fn invoke_detached(&self, command: String, args: Value, done: impl FnOnce(Result<Value, String>) + Send + 'static) {
        let host = self.clone();
        tauri::async_runtime::spawn(async move {
            let result = host.invoke(&command, args).await;
            done(result);
        });
    }

    /// The same, blocking the caller.
    pub fn invoke_blocking(&self, command: &str, args: Value) -> Result<Value, String> {
        tauri::async_runtime::block_on(self.invoke(command, args))
    }

    pub fn app(&self) -> &AppHandle {
        &self.app
    }

    /// One of a page's `preview_*` tools, run directly with no model behind it, as the text or the
    /// base64 image it would answer with. What a real-window check drives the tools through.
    pub fn run_preview_tool(&self, session_id: &str, tool: &str, input: Value) -> Result<String, String> {
        let tool = super::browser::PreviewTool::from_tool_name(tool).ok_or_else(|| format!("{tool} is not a page tool"))?;
        let input = input.as_object().cloned().unwrap_or_default();
        let grants = super::browser::BrowserToolGrants::default();
        let output = self.app.state::<AppState>().browser.execute_tool_blocking(session_id, tool, &input, &grants)?;
        Ok(match output {
            super::browser::PreviewToolOutput::Text(text) => text,
            super::browser::PreviewToolOutput::Image(shot) => format!("image {}x{} {}", shot.width, shot.height, shot.data),
        })
    }

    /// The remote-control session whose viewer page is at `url`, for a viewer drawn in this process
    /// instead: the same state and input as that page, and pixels in place of its MJPEG.
    pub fn remote_screen(&self, url: &str) -> Option<Arc<RemoteControlSession>> {
        self.app.state::<AppState>().preview_servers.controls().into_iter().find(|control| control.session.shows(url)).map(|control| control.session)
    }

    /// The window was covered or minimized, or came back. Waits for the main thread, so not on it.
    pub fn set_window_hidden(&self, hidden: bool) {
        self.app.state::<AppState>().browser.set_window_hidden(hidden);
    }

    /// Asks this tab's pane to show the remote screen whose viewer is at `url`.
    pub fn request_screen(&self, session_id: &str, url: String) {
        self.app.state::<AppState>().request_screen(session_id, url);
    }

    /// The screen the model just connected for this tab, once. Asked by the pane as it polls.
    pub fn take_screen_request(&self, session_id: &str) -> Option<String> {
        self.app.state::<AppState>().take_screen_request(session_id)
    }

    /// Quit: every save the app makes before leaving, then the exit hook. Failing to save keeps the app open.
    pub fn request_exit(&self, code: i32) {
        super::request_deferred_exit_with_barrier(self.app.clone(), self.exit_coordinator.clone(), code, |_, code| code);
    }

    /// Whatever is left once the exit barrier passed, or when the process is ending regardless.
    pub fn finalize(&self) {
        super::finalize_app_shutdown(&self.app, &self.exit_coordinator);
    }

    /// Paths the OS dragged over the window, which the drop commands are scoped to.
    pub fn drag_entered(&self, paths: &[PathBuf]) {
        let state = self.app.state::<AppState>();
        state.drag_drop.lock().unwrap_or_else(|p| p.into_inner()).enter(paths);
    }

    pub fn drag_dropped(&self, paths: &[PathBuf]) {
        let state = self.app.state::<AppState>();
        state.drag_drop.lock().unwrap_or_else(|p| p.into_inner()).drop_paths(paths);
    }
}

type Outgoing = ChannelSink;

async fn dispatch(
    app: AppHandle,
    renderer_mount: BrowserRendererMountLease,
    command: &str,
    args: &Value,
    outgoing: Outgoing,
) -> Result<Value, String> {
    match command {
        "load_document" => result_value(super::load_document(app.clone(), app.state::<AppState>())),
        "save_document" => result_value(
            super::save_document(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "document")?,
                optional_arg(args, "durable")?,
            )
            .await,
        ),
        "flush_document_saves" => {
            result_value(super::flush_document_saves(app.state::<AppState>()).await)
        }
        "subscribe_app_events" => {
            let channel = bridge_channel::<super::push_events::AppPushEvent>(
                channel_id(args, "onEvent")?,
                outgoing,
            );
            result_value(super::subscribe_app_events(
                app.state::<AppState>(),
                channel,
            ))
        }
        "create_conversation" => result_value(super::create_conversation(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "workspaceId")?,
            arg(args, "conversation")?,
        )),
        "delete_conversation" => result_value(super::delete_conversation(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "workspaceId")?,
            arg(args, "conversationId")?,
        )),
        "update_conversation" => result_value(super::update_conversation(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "workspaceId")?,
            arg(args, "conversation")?,
            arg(args, "expectedContextIds")?,
        )),
        "attest_edited_tool_context" => result_value(super::attest_edited_tool_context(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "request")?,
        )),
        "attest_inserted_tool_context" => result_value(super::attest_inserted_tool_context(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "request")?,
        )),
        "attest_copied_tool_contexts" => result_value(super::attest_copied_tool_contexts(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "request")?,
        )),
        "rewind_files" => result_value(
            super::rewind_files(app.clone(), app.state::<AppState>(), arg(args, "request")?).await,
        ),
        "reorder_conversations" => result_value(super::reorder_conversations(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "workspaceId")?,
            arg(args, "conversationIds")?,
        )),
        "load_conversation" => result_value(super::load_conversation(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "conversationId")?,
        )),
        "load_conversation_plan" => result_value(super::load_conversation_plan(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "conversationId")?,
        )),
        "list_history_entries" => result_value(super::list_history_entries(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            optional_arg(args, "owners")?,
        )),
        "load_history_entry" => result_value(super::load_history_entry(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "seq")?,
        )),
        "token_usage_statistics" => result_value(super::token_usage_statistics(app.clone()).await),
        "backfill_token_usage" => {
            result_value(super::backfill_token_usage(app.clone(), arg(args, "events")?).await)
        }
        "fork_conversation_contexts" => result_value(super::fork_conversation_contexts(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "workspaceId")?,
            arg(args, "sourceConversationId")?,
            arg(args, "targetConversationId")?,
            arg(args, "throughContextId")?,
        )),
        "list_conversation_templates" => {
            result_value(super::list_conversation_templates(app.clone()))
        }
        "apply_conversation_template" => result_value(super::apply_conversation_template(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "workspaceId")?,
            arg(args, "templateId")?,
            arg(args, "targetConversationId")?,
        )),
        "preview_conversation_template" => result_value(super::preview_conversation_template(
            app.clone(),
            arg(args, "templateId")?,
        )),
        "update_conversation_template" => result_value(super::update_conversation_template(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "templateId")?,
            arg(args, "contexts")?,
        )),
        "capture_conversation_template" => result_value(super::capture_conversation_template(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "workspaceId")?,
            arg(args, "conversationId")?,
            arg(args, "templateId")?,
        )),
        "delete_conversation_template" => result_value(super::delete_conversation_template(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "templateId")?,
        )),
        "reset_document" => {
            result_value(super::reset_document(app.clone(), app.state::<AppState>()))
        }
        "discover_capabilities" => result_value(
            super::discover_capabilities(app.clone(), app.state::<AppState>()).await,
        ),
        "capability_fingerprint" => result_value(
            super::capability_fingerprint(app.clone(), app.state::<AppState>()).await,
        ),
        "delete_hook" => result_value(
            super::delete_hook(app.clone(), app.state::<AppState>(), arg(args, "hookId")?).await,
        ),
        "delete_skill" => result_value(
            super::delete_skill(app.clone(), app.state::<AppState>(), arg(args, "skillId")?).await,
        ),
        "delete_mcp_server" => result_value(
            super::delete_mcp_server(app.clone(), app.state::<AppState>(), arg(args, "serverId")?).await,
        ),
        "read_skill_form" => result_value(
            super::read_skill_form(app.clone(), app.state::<AppState>(), arg(args, "skillId")?).await,
        ),
        "save_skill" => result_value(
            super::save_skill(app.clone(), app.state::<AppState>(), arg(args, "target")?, arg(args, "form")?, optional_arg(args, "original")?).await,
        ),
        "read_mcp_server_form" => result_value(
            super::read_mcp_server_form(app.clone(), app.state::<AppState>(), arg(args, "serverId")?).await,
        ),
        "save_mcp_server" => result_value(
            super::save_mcp_server(app.clone(), app.state::<AppState>(), arg(args, "target")?, arg(args, "form")?, optional_arg(args, "original")?).await,
        ),
        "read_hook_form" => result_value(
            super::read_hook_form(app.clone(), app.state::<AppState>(), arg(args, "hookId")?).await,
        ),
        "save_hook" => result_value(
            super::save_hook(app.clone(), app.state::<AppState>(), arg(args, "target")?, arg(args, "form")?, optional_arg(args, "original")?).await,
        ),
        "save_agent_role" => result_value(
            super::save_agent_role(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
                arg(args, "role")?,
            )
            .await,
        ),
        "delete_agent_role" => result_value(
            super::delete_agent_role(app.clone(), app.state::<AppState>(), arg(args, "roleId")?).await,
        ),
        "reveal_capability_location" => result_value(
            super::reveal_capability_location(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "kind")?,
                optional_arg(args, "workspaceKey")?,
            )
            .await,
        ),
        "mcp_probe_server" => result_value(
            super::mcp_probe_server(app.clone(), app.state::<AppState>(), arg(args, "serverId")?)
                .await,
        ),
        "environment_tool_snapshots" => result_value(
            super::environment_tool_snapshots(app.clone(), app.state::<AppState>()).await,
        ),
        "reveal_environment_tool" => result_value(
            super::reveal_environment_tool(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "executable")?,
            )
            .await,
        ),
        "list_wsl_distros" => result_value(super::list_wsl_distros().await),
        "machine_sandbox_support" => result_value(
            super::machine_sandbox_support(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
            )
            .await,
        ),
        "workspace_sandbox_ignores_case" => result_value(
            super::workspace_sandbox_ignores_case(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "path")?,
            )
            .await,
        ),
        "setup_local_sandbox" => result_value(super::setup_local_sandbox().await),
        "reveal_path_in_file_manager" => result_value(
            super::reveal_path_in_file_manager(arg(args, "path")?, optional_arg(args, "baseDir")?)
                .await,
        ),
        "open_external_url" => result_value(super::open_external_url(arg(args, "url")?).await),
        "permissions_status" => result_value(super::system_permissions::permissions_status(app.clone()).await),
        "request_keychain_access" => result_value(super::system_permissions::request_keychain_access(app.clone()).await),
        "mark_permissions_prompted" => result_value(super::system_permissions::mark_permissions_prompted(app.clone()).await),
        "open_privacy_settings" => result_value(super::system_permissions::open_privacy_settings(arg(args, "pane")?).await),
        "app_version_info" => result_value(super::app_version_info(app.clone())),
        "check_app_update" => result_value(super::check_app_update(app.clone()).await),
        "download_app_update" => {
            let channel = bridge_channel::<super::app_update::DownloadEvent>(
                channel_id(args, "onProgress")?,
                outgoing,
            );
            result_value(
                super::download_app_update(
                    app.clone(),
                    app.state::<AppState>(),
                    arg(args, "asset")?,
                    optional_arg(args, "checksumsAsset")?,
                    channel,
                )
                .await,
            )
        }
        "cancel_app_update_download" => {
            result_value(super::cancel_app_update_download(app.state::<AppState>()))
        }
        "local_model_status" => result_value(super::local_model_status(app.state::<AppState>()).await),
        "local_model_install" => result_value(
            super::local_model_install(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "variant")?,
                arg(args, "chinaMirror")?,
            )
            .await,
        ),
        "local_model_activate" => result_value(
            super::local_model_activate(app.clone(), app.state::<AppState>(), arg(args, "variant")?).await,
        ),
        "local_model_cancel_install" => {
            result_value(super::local_model_cancel_install(app.state::<AppState>()).await)
        }
        "local_model_remove" => {
            result_value(super::local_model_remove(app.state::<AppState>(), arg(args, "variant")?).await)
        }
        "local_model_prompt_info" => result_value(
            super::local_model_prompt_info(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "task")?,
                optional_arg(args, "prompt")?,
                optional_arg(args, "build")?,
            )
            .await,
        ),
        "local_model_default_prompts" => {
            result_value(super::local_model_default_prompts(app.clone(), app.state::<AppState>()).await)
        }
        "settle_conversation_title" => result_value(super::settle_conversation_title(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "conversationId")?,
        )),
        "get_tool_explanations" => {
            result_value(super::get_tool_explanations(app.clone(), arg(args, "conversationId")?))
        }
        "install_app_update" => result_value(
            super::install_app_update(app.clone(), app.state::<AppState>(), arg(args, "path")?)
                .await,
        ),
        "execute_tool" => result_value(
            super::execute_tool(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "request")?,
                optional_arg(args, "approvalNonce")?,
            )
            .await,
        ),
        "request_tool_approval" => result_value(
            super::request_tool_approval(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "request")?,
            )
            .await,
        ),
        "resolve_tool_prompt" => result_value(super::resolve_tool_prompt(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "promptId")?,
            arg(args, "decision")?,
            optional_arg(args, "feedback")?,
            optional_arg(args, "question")?,
        )),
        "pick_workspace_directory" => {
            result_value(super::pick_workspace_directory(app.clone(), app.state::<AppState>()).await)
        }
        "list_machine_shells" => result_value(
            super::list_machine_shells(app.clone(), app.state::<AppState>()).await,
        ),
        "probe_machine_shells" => result_value(
            super::probe_machine_shells(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
            )
            .await,
        ),
        "list_remote_directory" => result_value(
            super::list_remote_directory(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "machine")?,
                arg(args, "path")?,
            )
            .await,
        ),
        "authorize_remote_workspace" => result_value(
            super::authorize_remote_workspace(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "machine")?,
                arg(args, "path")?,
            )
            .await,
        ),
        "save_api_key" => result_value(
            super::save_api_key(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "provider")?,
                arg(args, "apiKey")?,
            )
            .await,
        ),
        "reveal_api_key" => result_value(
            super::reveal_api_key(app.clone(), app.state::<AppState>(), arg(args, "provider")?)
                .await,
        ),
        "delete_api_key" => result_value(
            super::delete_api_key(app.clone(), app.state::<AppState>(), arg(args, "provider")?)
                .await,
        ),
        "codex_oauth_sign_in" => result_value(
            super::codex_oauth_sign_in(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "provider")?,
            )
            .await,
        ),
        "codex_oauth_cancel_sign_in" => result_value(
            super::codex_oauth_cancel_sign_in(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "provider")?,
            )
            .await,
        ),
        "codex_oauth_status" => result_value(
            super::codex_oauth_status(app.clone(), app.state::<AppState>(), arg(args, "provider")?)
                .await,
        ),
        "codex_oauth_sign_out" => result_value(
            super::codex_oauth_sign_out(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "provider")?,
            )
            .await,
        ),
        "claude_agent_login_status" => result_value(
            super::claude_agent_login_status(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "provider")?,
            )
            .await,
        ),
        "claude_agent_open_login" => result_value(
            super::claude_agent_open_login(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "provider")?,
            )
            .await,
        ),
        "claude_agent_cli_status" => result_value(super::claude_agent_cli_status().await),
        "app_components_status" => result_value(super::app_components_status().await),
        "save_search_api_key" => result_value(
            super::save_search_api_key(
                app.state::<AppState>(),
                arg(args, "providerKind")?,
                arg(args, "slot")?,
                arg(args, "apiKey")?,
            )
            .await,
        ),
        "get_search_key_status" => result_value(
            super::get_search_key_status(
                app.state::<AppState>(),
                arg(args, "providerKind")?,
                arg(args, "slot")?,
            )
            .await,
        ),
        "reveal_search_api_key" => result_value(
            super::reveal_search_api_key(
                app.state::<AppState>(),
                arg(args, "providerKind")?,
                arg(args, "slot")?,
            )
            .await,
        ),
        "delete_search_api_key" => result_value(
            super::delete_search_api_key(
                app.state::<AppState>(),
                arg(args, "providerKind")?,
                arg(args, "slot")?,
            )
            .await,
        ),
        "fetch_models" => result_value(
            super::fetch_models(app.clone(), app.state::<AppState>(), arg(args, "provider")?).await,
        ),
        "run_model" => {
            let channel =
                bridge_channel::<ModelStreamEvent>(channel_id(args, "onEvent")?, outgoing);
            result_value(
                super::run_model(
                    app.clone(),
                    app.state::<AppState>(),
                    arg(args, "request")?,
                    arg(args, "requestId")?,
                    channel,
                    optional_arg(args, "forkPromptContextId")?,
                    optional_arg(args, "compactNow")?,
                )
                .await,
            )
        }
        "cancel_model_run" => result_value(super::cancel_model_run(
            app.state::<AppState>(),
            arg(args, "requestId")?,
        )),
        "cancel_conversation_run" => result_value(super::cancel_conversation_run(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
        )),
        "attach_model_run" => {
            let channel =
                bridge_channel::<ModelStreamEvent>(channel_id(args, "onEvent")?, outgoing);
            result_value(super::attach_model_run(
                app.state::<AppState>(),
                arg(args, "conversationId")?,
                channel,
            ))
        }
        "take_run_settlement" => result_value(super::take_run_settlement(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
        )),
        "list_resumable_runs" => result_value(super::list_resumable_runs(app.state::<AppState>())),
        "steer_model_run" => result_value(super::steer_model_run(
            app.state::<AppState>(),
            arg(args, "requestId")?,
            arg(args, "messageId")?,
            arg(args, "content")?,
            optional_arg(args, "images")?,
            optional_arg(args, "files")?,
            arg(args, "createdAt")?,
        )),
        "workflow_step_control" => result_value(super::workflow_step_control(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "runId")?,
            arg(args, "stepIndex")?,
            arg(args, "action")?,
        )),
        "workflow_step_record" => result_value(super::workflow_step_record(
            app.clone(),
            arg(args, "conversationId")?,
            arg(args, "runId")?,
            arg(args, "stepIndex")?,
        )),
        "image_attachment_upload" => result_value(
            super::image_attachment_upload(app.clone(), arg(args, "name")?, arg(args, "data")?)
                .await,
        ),
        "image_attachment_data" => result_value(
            super::image_attachment_data(app.clone(), arg(args, "imageId")?).await,
        ),
        "image_attachment_thumbnail" => result_value(
            super::image_attachment_thumbnail(app.clone(), arg(args, "imageId")?).await,
        ),
        "background_image_put" => result_value(
            super::background_image_put(
                app.clone(),
                optional_arg(args, "uploadId")?,
                arg(args, "data")?,
            )
            .await,
        ),
        "background_image_attach_motion" => result_value(
            super::background_image_attach_motion(
                app.clone(),
                arg(args, "uploadId")?,
                arg(args, "path")?,
                arg(args, "kind")?,
            )
            .await,
        ),
        "background_image_motion" => result_value(
            super::background_image_motion(app.clone(), arg(args, "imageId")?).await,
        ),
        "background_image_commit" => result_value(
            super::background_image_commit(app.clone(), arg(args, "uploadId")?).await,
        ),
        "background_image_data" => result_value(
            super::background_image_data(
                app.clone(),
                arg(args, "imageId")?,
                arg(args, "width")?,
                arg(args, "height")?,
            )
            .await,
        ),
        "background_image_list" => result_value(super::background_image_list(app.clone()).await),
        "background_image_delete" => result_value(
            super::background_image_delete(app.clone(), arg(args, "imageId")?).await,
        ),
        "file_attachment_upload" => result_value(
            super::file_attachment_upload(
                app.clone(),
                arg(args, "name")?,
                arg(args, "data")?,
                arg(args, "format")?,
                optional_arg(args, "text")?,
                optional_arg(args, "pages")?,
            )
            .await,
        ),
        "file_attachment_data" => {
            result_value(super::file_attachment_data(app.clone(), arg(args, "fileId")?).await)
        }
        // No native window receives drops here, so the session stays empty and
        // both commands refuse every path, as the desktop app would for a path
        // that was never dragged.
        "dropped_paths_probe" => result_value(super::dropped_paths_probe(
            app.state::<AppState>(),
            arg(args, "paths")?,
        )),
        "dropped_file_read" => {
            result_value(super::dropped_file_read(app.clone(), arg(args, "path")?).await)
        }
        "web_source_icon" => {
            result_value(super::web_source_icon(app.clone(), arg(args, "url")?).await)
        }
        "browser_register_renderer_mount" => serde_json::to_value(&renderer_mount)
            .map_err(|error| format!("无法序列化 browser-dev renderer mount: {error}")),
        "browser_renderer_mount_heartbeat" => result_value(
            app.state::<AppState>()
                .browser_renderer_mounts
                .heartbeat(&renderer_mount.mount_id, renderer_mount.generation)
                .map_err(super::browser_renderer_mount_error),
        ),
        "browser_open" => result_value(
            super::browser_open(
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                optional_arg(args, "url")?,
                arg(args, "lifecycleEpoch")?,
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "browser_close" => result_value(
            super::browser_close(
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                arg(args, "lifecycleEpoch")?,
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "browser_status" => result_value(
            super::browser_status(
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "browser_set_panel_bounds" => result_value(
            super::browser_set_panel_bounds(
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                arg(args, "bounds")?,
                arg(args, "lifecycleEpoch")?,
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "browser_navigate" => result_value(
            super::browser_navigate(
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                arg(args, "url")?,
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "browser_capture_page" => result_value(
            super::browser_capture_page(
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "browser_take_selected_element" => result_value(
            super::browser_take_selected_element(
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "browser_open_local_file" => {
            let session_id =
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned());
            // A headless bridge cannot drive a native picker, so an E2E supplies the file the
            // dialog would otherwise have returned.
            match optional_arg::<String>(args, "path")? {
                Some(path) => result_value(
                    super::preview_local_file_in_browser(
                        app.clone(),
                        session_id,
                        std::path::PathBuf::from(path),
                        renderer_mount.mount_id.clone(),
                        renderer_mount.generation,
                    )
                    .await,
                ),
                None => result_value(
                    super::browser_open_local_file(
                        app.clone(),
                        app.state::<AppState>(),
                        session_id,
                        optional_arg(args, "target")?,
                        renderer_mount.mount_id.clone(),
                        renderer_mount.generation,
                    )
                    .await,
                ),
            }
        }
        "browser_action" => result_value(
            super::browser_action(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "sessionId")?.unwrap_or_else(|| "browser-dev".to_owned()),
                arg(args, "action")?,
                optional_arg(args, "value")?,
                optional_arg(args, "lifecycleEpoch")?,
                renderer_mount.mount_id.clone(),
                renderer_mount.generation,
            )
            .await,
        ),
        "get_git_workspace_summary" => result_value(
            super::get_git_workspace_summary(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
                optional_arg(args, "knownRevision")?,
            )
            .await,
        ),
        "get_git_change_page" => result_value(
            super::get_git_change_page(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
                arg(args, "request")?,
            )
            .await,
        ),
        "get_git_diff" => result_value(
            super::get_git_diff(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
                arg(args, "request")?,
            )
            .await,
        ),
        "get_git_branches" => result_value(
            super::get_git_branches(app.clone(), app.state::<AppState>(), arg(args, "target")?)
                .await,
        ),
        "browse_list_directory" => result_value(
            super::browse_list_directory(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "path")?,
            )
            .await,
        ),
        "browse_read_file" => result_value(
            super::browse_read_file(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "path")?,
            )
            .await,
        ),
        "browse_read_file_bytes" => result_value(
            super::browse_read_file_bytes(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "path")?,
            )
            .await,
        ),
        "browse_search_files" => result_value(
            super::browse_search_files(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "root")?,
                arg(args, "query")?,
                arg(args, "limit")?,
            )
            .await,
        ),
        "browse_probe_paths" => result_value(
            super::browse_probe_paths(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "targets")?,
                optional_arg(args, "patient")?,
            )
            .await,
        ),
        "browse_rename_path" => result_value(
            super::browse_rename_path(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "path")?,
                arg(args, "name")?,
            )
            .await,
        ),
        "browse_delete_path" => result_value(
            super::browse_delete_path(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "path")?,
            )
            .await,
        ),
        "browse_create_directory" => result_value(
            super::browse_create_directory(
                app.clone(),
                app.state::<AppState>(),
                optional_arg(args, "machine")?,
                arg(args, "parent")?,
                arg(args, "name")?,
            )
            .await,
        ),
        "browse_open_in_file_manager" => {
            result_value(super::browse_open_in_file_manager(arg(args, "path")?).await)
        }
        "browse_open_with_choices" => {
            result_value(super::browse_open_with_choices(arg(args, "path")?).await)
        }
        "browse_open_with" => result_value(
            super::browse_open_with(arg(args, "path")?, optional_arg(args, "app")?).await,
        ),
        "browse_open_with_chooser" => {
            result_value(super::browse_open_with_chooser(arg(args, "path")?).await)
        }
        "preview_list_configurations" => result_value(
            super::preview_list_configurations(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
            )
            .await,
        ),
        "preview_list_servers" => result_value(
            super::preview_list_servers(app.clone(), app.state::<AppState>(), arg(args, "target")?)
                .await,
        ),
        "preview_start_server" => result_value(
            super::preview_start_server(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
                optional_arg(args, "name")?,
            )
            .await,
        ),
        "browser_set_page_network" => result_value(
            super::browser_set_page_network(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "sessionId")?,
                optional_arg(args, "target")?,
            )
            .await,
        ),
        "preview_stop_server" => result_value(
            super::preview_stop_server(app.state::<AppState>(), arg(args, "handle")?).await,
        ),
        "preview_server_logs" => value(super::preview_server_logs(
            app.state::<AppState>(),
            arg(args, "handle")?,
            optional_arg(args, "errorsOnly")?,
            optional_arg(args, "search")?,
            optional_arg(args, "lines")?,
        )),
        "create_conversation_worktree" => result_value(
            super::create_conversation_worktree(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "conversationId")?,
                optional_arg(args, "member")?,
                optional_arg(args, "fromBranch")?,
            )
            .await,
        ),
        "release_conversation_worktree" => result_value(
            super::release_conversation_worktree(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "conversationId")?,
                optional_arg(args, "member")?,
            )
            .await,
        ),
        "list_ssh_prompts" => result_value(Ok(super::list_ssh_prompts())),
        "answer_ssh_prompt" => result_value(super::answer_ssh_prompt(
            arg(args, "id")?,
            optional_arg(args, "answer")?,
        )),
        "release_worktrees_of_deleted_conversations" => result_value(
            super::release_worktrees_of_deleted_conversations(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "conversationIds")?,
            )
            .await,
        ),
        "prepare_git_discard" => result_value(
            super::prepare_git_discard(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
                arg(args, "paths")?,
                arg(args, "includeUntracked")?,
            )
            .await,
        ),
        "execute_git_action" => result_value(
            super::execute_git_action(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "target")?,
                arg(args, "action")?,
            )
            .await,
        ),

        "open_terminal" => {
            let channel = bridge_channel::<TerminalEvent>(channel_id(args, "onEvent")?, outgoing);
            result_value(super::open_terminal(
                app.clone(),
                app.state::<AppState>(),
                arg(args, "conversationId")?,
                arg(args, "terminalId")?,
                arg(args, "cols")?,
                arg(args, "rows")?,
                optional_arg(args, "workspace")?,
                optional_arg(args, "shell")?,
                optional_arg(args, "draftWorkspaceId")?,
                channel,
            ))
        }
        "write_terminal" => result_value(super::write_terminal(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "terminalId")?,
            arg(args, "sessionId")?,
            arg(args, "data")?,
        )),
        "resize_terminal" => result_value(super::resize_terminal(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "terminalId")?,
            arg(args, "sessionId")?,
            arg(args, "cols")?,
            arg(args, "rows")?,
        )),
        "detach_terminal" => value(super::detach_terminal(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "terminalId")?,
            arg(args, "sessionId")?,
        )),
        "close_terminal" => value(super::close_terminal(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "terminalId")?,
        )),
        "live_terminal_count" => value(super::live_terminal_count(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
        )),
        "stop_shell_task" => value(super::stop_shell_task(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "shellTaskId")?,
        )),
        "stop_conversation_task" => result_value(super::stop_conversation_task(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "task")?,
        )),
        "list_wake_pending_conversations" => result_value(super::list_wake_pending_conversations(
            app.state::<AppState>(),
        )),
        "list_pending_tool_prompts" => {
            result_value(super::list_pending_tool_prompts(app.state::<AppState>()))
        }
        "list_pending_fork_starts" => result_value(super::list_pending_fork_starts(app.clone())),
        "list_fork_decisions" => result_value(super::list_fork_decisions(
            app.clone(),
            arg(args, "conversationId")?,
        )),
        "list_pending_fork_requests" => {
            result_value(super::list_pending_fork_requests(app.state::<AppState>()))
        }
        "resolve_fork_request" => result_value(super::resolve_fork_request(
            app.clone(),
            app.state::<AppState>(),
            arg(args, "forkId")?,
            arg(args, "approved")?,
        )),
        "list_shell_tasks" => value(super::list_shell_tasks(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
        )),
        "open_shell_task_output" => {
            let channel = bridge_channel::<super::shell_tasks::ShellOutputEvent>(
                channel_id(args, "onEvent")?,
                outgoing,
            );
            result_value(super::open_shell_task_output(
                app.state::<AppState>(),
                arg(args, "conversationId")?,
                arg(args, "shellTaskId")?,
                channel,
            ))
        }
        "detach_shell_task_output" => value(super::detach_shell_task_output(
            app.state::<AppState>(),
            arg(args, "conversationId")?,
            arg(args, "shellTaskId")?,
            optional_arg(args, "subscriptionId")?,
        )),
        command => Err(format!("宿主不支持命令: {command}")),
    }
}


fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    let value = args.get(key).cloned().ok_or_else(|| format!("调用参数缺少 {key}"))?;
    serde_json::from_value(value).map_err(|error| format!("调用参数 {key} 无效: {error}"))
}

fn optional_arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<Option<T>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone()).map(Some).map_err(|error| format!("调用参数 {key} 无效: {error}")),
    }
}

fn channel_id(args: &Value, key: &str) -> Result<String, String> {
    let id = args
        .get(key)
        .and_then(|value| value.get("__mewrkChannel"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("调用参数 {key} 缺少通道标识"))?;
    Ok(id.to_owned())
}

fn bridge_channel<T>(channel_id: String, outgoing: Outgoing) -> Channel<T> {
    Channel::new(move |body| {
        let payload = match body {
            InvokeResponseBody::Json(json) => serde_json::from_str::<Value>(&json)?,
            InvokeResponseBody::Raw(bytes) => serde_json::to_value(bytes)?,
        };
        outgoing(&channel_id, payload);
        Ok(())
    })
}

fn result_value<T: Serialize>(result: Result<T, String>) -> Result<Value, String> {
    result.and_then(value)
}

fn value<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| format!("无法序列化命令结果: {error}"))
}

#[allow(unused)]
fn _keep(_: Value) -> Value {
    json!(null)
}
