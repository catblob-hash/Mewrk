//! What `App.tsx` kept in `useState` that no other crate holds, as one value in `app.local`.

use std::collections::{HashMap, HashSet};

use mewrk_client::model::RunTarget;
use mewrk_client::prelude::*;
use mewrk_pane_browser::preview::PreviewServerSnapshot;
use mewrk_pane_files::browser::BrowseMachine;
use mewrk_pane_files::pane::FilesPaneOpenRequest;
use mewrk_pane_git::GitReviewRevealRequest;
use serde::Deserialize;

/// The top bar's title while it is being renamed in place.
#[derive(Clone, Debug, PartialEq)]
pub struct TitleDraft {
    pub conversation_id: String,
    pub value: String,
}

/// A question the user answers before something irreversible happens (`ConfirmationRequest`). The answer is a callback
/// rather than a promise: whatever runs after "yes" has to read the state as it is then, so the caller re-enters its own
/// action with the answer.
#[derive(Clone)]
pub struct ConfirmationRequest {
    pub question: String,
    pub detail: Option<String>,
    pub confirm_label: String,
    pub destructive: bool,
    pub on_confirm: Rc<dyn Fn(&mut App)>,
}

/// `SshPrompt`: a question an SSH connection is waiting on the user for.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshPrompt {
    pub id: String,
    pub machine: String,
    pub kind: SshPromptKind,
    pub prompt: String,
    pub host_key: Option<SshHostKey>,
    #[serde(default)]
    pub retry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SshPromptKind {
    HostKey,
    Secret,
    Confirm,
    Notice,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshHostKey {
    pub host: String,
    pub key_type: String,
    pub fingerprint: String,
}

/// `withSshPrompt`: adds a newly announced question, once, keeping announcement order.
pub fn with_ssh_prompt(prompts: &mut Vec<SshPrompt>, prompt: SshPrompt) {
    if !prompts.iter().any(|existing| existing.id == prompt.id) {
        prompts.push(prompt);
    }
}

/// `withoutSshPrompt`.
pub fn without_ssh_prompt(prompts: &mut Vec<SshPrompt>, id: &str) {
    prompts.retain(|prompt| prompt.id != id);
}

/// The machine whose remote directory browser is open, with its display name and what the chosen directory becomes.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteWorkspacePicker {
    pub machine: RunTarget,
    pub name: String,
}

/// The workspace whose settings a gear opened: its machine and the directory it is registered at, never a worktree
/// standing in for it, with its display name.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSettingsTarget {
    pub machine: Option<RunTarget>,
    pub path: String,
    pub name: String,
}

/// The menu a path the transcript names opens at the link when it could be in more than one place.
pub struct PathChoice {
    pub anchor: PathChoiceAnchor,
    /// Empty while the lookup is still out: the menu says it is looking.
    pub rows: Vec<PathChoiceRow>,
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathChoiceAnchor {
    Rect([f32; 4]),
    Point(f32, f32),
}

pub struct PathChoiceRow {
    pub id: String,
    pub label: String,
    pub label_is_path: bool,
    pub description: String,
    pub is_directory: bool,
    pub machine: BrowseMachine,
    pub path: String,
    pub line: Option<u32>,
    pub new_page: bool,
}

#[derive(Default)]
pub struct ShellState {
    /// `!sidebarOpen`.
    pub sidebar_closed: bool,
    pub sidebar_resizing: bool,
    /// The cursor a pane drag shows wherever the pointer strays to.
    pub pane_resizing: Option<CursorKind>,
    /// What held the keyboard when the first window opened, to give it back when the last one closes.
    pub modal_focus: Option<u64>,
    /// The first window has only just opened, and may yet take the keyboard for one of its own fields.
    pub modal_focus_settling: bool,
    /// A window just closed: what to give the keyboard back to, unless something took it by now from what the window left behind.
    pub modal_focus_return: Option<(u64, u64)>,
    pub search_open: bool,
    pub title_draft: Option<TitleDraft>,
    pub confirmation: Option<ConfirmationRequest>,
    pub ssh_prompts: Vec<SshPrompt>,
    /// A failed answer, for the dialog's next frame: the answer comes back off the UI thread's stack.
    pub ssh_prompt_error: Option<String>,
    pub remote_picker: Option<RemoteWorkspacePicker>,
    /// `{ machine: null }` is this one.
    pub machine_settings: Option<Option<RunTarget>>,
    pub workspace_settings: Option<WorkspaceSettingsTarget>,
    pub files_request: Option<(String, FilesPaneOpenRequest)>,
    pub review_request: Option<(String, usize, GitReviewRevealRequest)>,
    pub request_nonce: u64,
    pub path_choice: Option<PathChoice>,
    /// `lastPreviewPageRef`: the page the user last had in each conversation's preview pane.
    pub last_preview_page: HashMap<String, String>,
    /// `browserAutomationStoppingIds`.
    pub automation_stopping: HashSet<String>,
    /// `previewServers`, and which conversation's target they were read for.
    pub preview_servers: Vec<PreviewServerSnapshot>,
    pub preview_servers_for: Option<String>,
    /// Tabs whose server went away while a run was driving them: (conversation, page, the address it was left at).
    pub doomed_pages: Vec<(String, String, Option<String>)>,
    /// What each conversation not on screen was last known to have running.
    pub preview_servers_of: std::collections::HashMap<String, Vec<PreviewServerSnapshot>>,
    /// `gitSurfaceFailures`.
    pub git_surface_failures: HashMap<String, String>,
    /// `machineMenuDistros`.
    pub machine_menu_distros: Option<Vec<mewrk_settings_global::machine_dialogs::WslDistro>>,
    /// The conversation whose activation effects (shell tasks, fork decisions, plan) last ran.
    pub activated: Option<String>,
    /// `timeline scroll` asked for by a run signal, for the chat view to pick up.
    pub initialized: bool,
}

pub fn shell(app: &mut App) -> &mut ShellState {
    app.local::<ShellState>()
}
