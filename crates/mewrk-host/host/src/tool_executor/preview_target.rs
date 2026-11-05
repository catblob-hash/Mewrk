//! What a preview page tool acts on — the conversation's web page or a
//! remote-control screen — and acting on a screen (`docs/remote-control.md` §4).
//!
//! A conversation has one page, so for the web tools a `serverId` only ever
//! decided *whether* a call had something to act on. A remote-control row is
//! different: it is a screen, not the page, and a call aimed at it must reach
//! the session behind it and never the page's DevTools. Which of the two a call
//! means is decided once, by [`resolve_preview_target`], from what the call
//! names and what the preview shows:
//!
//! 1. a `serverId` naming a remote-control row is that screen; one naming a dev
//!    server is the page;
//! 2. no `serverId`, and the page shows a screen's viewer: that screen;
//! 3. no `serverId`, no page, no dev server, exactly one screen: that screen —
//!    where a dev server that is a screen's `launch` entry (the virtual machine,
//!    the emulator) is no dev server, since nothing of it is a page;
//! 4. otherwise the page, as before.
//!
//! A call that lands on the page while the page shows *any* screen's viewer is
//! refused: the viewer holds the sign-in form and its token, and no DevTools
//! call may reach either.
//!
//! The run loop resolves each call before the signed-in-page takeover gate and
//! before classifying it, and records what it found ([`PreviewCallTargets`]):
//! the gate applies to the page only, a look at a screen is a sensitive
//! observation, and the executor resolves again when the call runs and refuses
//! it if the answer changed in between — the call was judged for one target and
//! must not act on another.
//!
//! The model never reads a viewer's address: page output that could carry one
//! (a URL in a page report, an error) goes through [`scrub_viewer_addresses`].

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use serde_json::{json, Value};

use super::{optional_bool, optional_owned_string, Outcome, PREVIEW_TOOL_TIMEOUT};
use crate::{
    browser::PreviewTool,
    cancel::CancelSignal,
    image_attachments::ImageAttachmentStore,
    model::{JsonObject, ToolExecutionRequest},
    preview_servers::PreviewServerStatus,
    remote_control::{
        driver::Platform,
        session::{parse_actions, CallContext, ModelScreenshot, RemoteControlSession},
    },
    security::{PreviewJudgement, PreviewSurface},
    state::AppState,
};

/// What `preview_input` answers while no remote-control screen can take its actions.
pub(crate) const PREVIEW_INPUT_NO_SCREEN: &str = "preview_input drives remote-control screens, and no remote-control screen is connected. Start one with preview_start (an entry of .mewrk/launch.json with \"type\": \"remote-control\").";

/// What `preview_input` answers when the call lands on the web page.
pub(crate) const PREVIEW_INPUT_ON_A_PAGE: &str = "preview_input drives remote-control screens, and this call landed on the web page. On a page use preview_click or preview_fill, or preview_eval for keys, hovering and scrolling; to drive a remote-control screen, pass its serverId (preview_list shows it).";

/// What `preview_click` and `preview_fill` answer for a selector on a screen.
const REMOTE_SELECTOR: &str = "A remote-control screen has no CSS selectors. Pass the uid preview_snapshot printed for the element, or use preview_input with x/y from preview_screenshot.";

/// The sentence a page tool answers with on a remote-control screen.
fn not_a_web_page(tool: &str, server_id: &str) -> String {
    format!(
        "\"{server_id}\" is a remote-control screen, not a web page, so {tool} does not apply to it. Use preview_screenshot or preview_snapshot to look at it, and preview_click or preview_fill (uids from preview_snapshot) or preview_input (x/y from preview_screenshot) to act on it."
    )
}

/// The refusal of a page tool while the page shows a remote-control screen's viewer.
pub(crate) fn preview_shows_remote_screen(server_id: &str) -> String {
    format!(
        "The preview shows the remote-control screen \"{server_id}\"; pass serverId \"{server_id}\" to act on it, or open a web page first."
    )
}

/// No `serverId`, no page and several screens: which one is the model's to say.
fn several_screens(rows: &[&PreviewTargetRow]) -> String {
    let ids = rows
        .iter()
        .map(|row| format!("\"{}\"", row.server_id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Several remote-control screens are connected ({ids}) and the preview shows none of them. Pass serverId to say which one."
    )
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// One row a call may address, as [`resolve_preview_target`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviewTargetRow {
    pub handle: String,
    pub server_id: String,
    pub status: PreviewServerStatus,
    /// A remote-control row, rather than a dev server.
    pub remote: bool,
    /// The page shows this row's viewer now.
    pub shown: bool,
    /// A dev-server row a remote-control row's `launch` started (or found running): the process
    /// behind a screen — a virtual machine, an emulator — and no page of its own.
    pub launches_a_screen: bool,
    /// What kind of screen a remote-control row is; `None` for a dev server.
    pub platform: Option<Platform>,
}

/// The conversation's page, as [`resolve_preview_target`] reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PreviewPageFacts {
    /// The browser runtime is up; a host without one has no page at all.
    pub attached: bool,
    pub has_page: bool,
    pub suspended: bool,
    /// The server id of the remote-control row, of whichever conversation, whose viewer the page
    /// shows.
    pub viewer_of: Option<String>,
}

/// Where [`resolve_preview_target`] landed a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolved<'a> {
    /// The conversation's web page.
    Page,
    /// A remote-control row's screen.
    Screen(&'a PreviewTargetRow),
}

/// The one rule for what a preview call acts on (`docs/remote-control.md` §4). Pure: everything
/// it needs is in its arguments, which [`preview_target_for`] gathers.
///
/// `rows` are the rows the conversation may address, in the order they were started — the same
/// scope every other `serverId` resolves in (`preview_servers_for_session`). The answers for a
/// call that resolves to nothing are the source's, unchanged.
pub(crate) fn resolve_preview_target<'a>(
    server_id: Option<&str>,
    rows: &'a [PreviewTargetRow],
    page: &PreviewPageFacts,
) -> Result<Resolved<'a>, String> {
    match server_id {
        Some(server_id) => {
            let row = rows
                .iter()
                .find(|row| crate::preview::server_ids_match(&row.server_id, server_id))
                .ok_or_else(|| super::preview_stale_server_id(server_id))?;
            if row.remote {
                return Ok(Resolved::Screen(row));
            }
        }
        None => {
            if let Some(row) = rows.iter().find(|row| row.remote && row.shown) {
                return Ok(Resolved::Screen(row));
            }
            // Only a dev server counts here: a screen is never what a page tool falls back to, and
            // neither is the process a screen runs on.
            let dev_server = rows.iter().any(|row| {
                !row.remote
                    && !row.launches_a_screen
                    && matches!(
                        row.status,
                        PreviewServerStatus::Running | PreviewServerStatus::Starting
                    )
            });
            let page_open = page.attached && (page.has_page || page.suspended);
            if !dev_server && !page_open {
                let screens: Vec<&PreviewTargetRow> =
                    rows.iter().filter(|row| row.remote).collect();
                return match screens.as_slice() {
                    [only] => Ok(Resolved::Screen(*only)),
                    [] if page.attached => Err(super::PREVIEW_PANE_NOT_OPEN.into()),
                    [] => Err(super::PREVIEW_NO_SERVERS.into()),
                    several => Err(several_screens(several)),
                };
            }
        }
    }
    if let Some(viewer) = &page.viewer_of {
        return Err(preview_shows_remote_screen(viewer));
    }
    Ok(Resolved::Page)
}

/// How the screen tools' answers name the kinds of screen a tool reaches.
struct ScreenWords {
    /// `an Android device`, `a phone or tablet`.
    one: &'static str,
    /// `Android device`, without its article.
    noun: &'static str,
    /// `Android devices`.
    many: &'static str,
    /// The launch.json `protocol` values that connect one, quoted: `"adb"`, `"adb" or "wda"`.
    protocol: &'static str,
}

/// The words for a screen of any of `platforms`.
fn screen_words(platforms: &[Platform]) -> ScreenWords {
    let has = |platform: Platform| platforms.contains(&platform);
    let (one, noun, many, protocol) = match (
        has(Platform::Android),
        has(Platform::Ios),
        has(Platform::Desktop),
    ) {
        (true, false, false) => (
            "an Android device",
            "Android device",
            "Android devices",
            "\"adb\"",
        ),
        (false, true, false) => (
            "an iPhone or iPad",
            "iPhone or iPad",
            "iPhones or iPads",
            "\"wda\"",
        ),
        (false, false, true) => ("a desktop", "desktop", "desktops", "\"vnc\""),
        (true, true, false) => (
            "a phone or tablet",
            "phone or tablet",
            "phones or tablets",
            "\"adb\" or \"wda\"",
        ),
        (false, true, true) => (
            "an iPhone, iPad or desktop",
            "iPhone, iPad or desktop",
            "iPhones, iPads or desktops",
            "\"wda\" or \"vnc\"",
        ),
        (true, false, true) => (
            "an Android device or desktop",
            "Android device or desktop",
            "Android devices or desktops",
            "\"adb\" or \"vnc\"",
        ),
        _ => (
            "a remote-control screen",
            "remote-control screen",
            "remote-control screens",
            "\"vnc\", \"adb\" or \"wda\"",
        ),
    };
    ScreenWords {
        one,
        noun,
        many,
        protocol,
    }
}

/// Why `tool` does not reach a screen of `platform`, where there is more to say than that it
/// does not, and what to do instead.
fn out_of_reach(tool: &str, platform: Platform) -> &'static str {
    match (tool, platform) {
        ("screen_app", Platform::Desktop) => {
            " A desktop's apps are out of reach over VNC; use preview_screenshot and preview_input."
        }
        ("screen_device", Platform::Desktop) => {
            " A desktop's settings are out of reach over VNC; use preview_screenshot and preview_input."
        }
        ("screen_clipboard", Platform::Android) => {
            " An Android device's clipboard needs a helper on the device that this build does not have."
        }
        ("screen_package", Platform::Ios) => {
            " Installing apps on an iPhone or iPad is not available in this build."
        }
        ("screen_package", Platform::Desktop) => {
            " Install software on a desktop from that machine's own shell."
        }
        _ => "",
    }
}

/// The rule for what a screen tool ([`super::screen_tools::SCREEN_TOOLS`]) acts on: a
/// remote-control screen of one of `platforms` — the kinds the tool reaches — never the page.
/// Pure, like [`resolve_preview_target`], over the same rows:
///
/// 1. a `serverId` is the row it names, when that is a screen of those kinds; another kind of
///    screen, a dev server, or no row at all is an error that says which;
/// 2. no `serverId`: the screen whose viewer the page shows, if it is of those kinds;
/// 3. otherwise the only screen of those kinds (screens of other kinds, dev servers and the page
///    do not matter: these tools never act on a page);
/// 4. none, or several, is an error that says what to do.
pub(crate) fn resolve_screen_target<'a>(
    tool: &str,
    platforms: &[Platform],
    server_id: Option<&str>,
    rows: &'a [PreviewTargetRow],
) -> Result<&'a PreviewTargetRow, String> {
    let words = screen_words(platforms);
    let reached = |row: &&PreviewTargetRow| {
        row.remote
            && row
                .platform
                .is_some_and(|platform| platforms.contains(&platform))
    };
    if let Some(server_id) = server_id {
        let row = rows
            .iter()
            .find(|row| crate::preview::server_ids_match(&row.server_id, server_id))
            .ok_or_else(|| super::preview_stale_server_id(server_id))?;
        if reached(&row) {
            return Ok(row);
        }
        if !row.remote {
            return Err(format!(
                "\"{}\" is a dev server, not a remote-control screen.",
                row.server_id
            ));
        }
        return Err(match row.platform {
            Some(platform) => format!(
                "\"{}\" is {}, and {tool} works on {} only.{}",
                row.server_id,
                screen_words(&[platform]).one,
                words.many,
                out_of_reach(tool, platform)
            ),
            None => format!(
                "\"{}\" is a remote-control screen, but not {}; {tool} works on {} only.",
                row.server_id, words.one, words.many
            ),
        });
    }
    if let Some(row) = rows.iter().filter(reached).find(|row| row.shown) {
        return Ok(row);
    }
    let screens: Vec<&PreviewTargetRow> = rows.iter().filter(reached).collect();
    match screens.as_slice() {
        [only] => Ok(*only),
        [] => Err(format!(
            "No {} is connected{}. Connect one with preview_start (a .mewrk/launch.json entry with \"type\": \"remote-control\" and \"protocol\": {}).",
            words.noun,
            if words.noun == "remote-control screen" {
                ""
            } else {
                " as a remote-control screen"
            },
            words.protocol
        )),
        several => Err(format!(
            "Several {} are connected ({}) and the preview shows none of them. Pass serverId to say which one.",
            words.many,
            several
                .iter()
                .map(|row| format!("\"{}\"", row.server_id))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A remote-control row's screen, with the session behind it.
#[derive(Clone)]
pub(crate) struct RemoteScreen {
    pub handle: String,
    pub server_id: String,
    pub session: Arc<RemoteControlSession>,
}

impl fmt::Debug for RemoteScreen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteScreen")
            .field("handle", &self.handle)
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

/// What a preview call acts on.
#[derive(Clone, Debug)]
pub(crate) enum PreviewTarget {
    /// The conversation's page, by its browser session id.
    Web(String),
    Remote(RemoteScreen),
}

impl PreviewTarget {
    pub(crate) fn key(&self) -> PreviewTargetKey {
        match self {
            Self::Web(_) => PreviewTargetKey::Page,
            Self::Remote(screen) => PreviewTargetKey::Screen {
                handle: screen.handle.clone(),
                server_id: screen.server_id.clone(),
            },
        }
    }
}

/// Resolves `request` against the registry and the page as they are now.
pub(crate) fn preview_target_for(
    request: &ToolExecutionRequest,
    state: &AppState,
) -> Result<PreviewTarget, String> {
    let session_id = crate::browser::preview_page_session_id(&request.conversation_id)?;
    let server_id = optional_owned_string(&request.input, "serverId", 256)?;
    let workspace = Path::new(&request.workspace_path);
    let every_screen = state.preview_servers.controls();
    let attached = state.browser.is_attached();
    // A screen tool acts on a screen of a kind it reaches, never on the page.
    let screen_tool = super::screen_tools::screen_tool_platforms(&request.tool_name);
    // The page is only read when something could depend on it: a fallback, or a viewer to tell
    // apart from it — for a screen tool, only which screen it shows. Reading it touches the
    // native page.
    let page_matters = match screen_tool {
        Some(_) => server_id.is_none() && !every_screen.is_empty(),
        None => server_id.is_none() || !every_screen.is_empty(),
    };
    let status = (attached && page_matters).then(|| state.browser.status(&session_id));
    let url = status.as_ref().map_or("", |status| status.url.as_str());
    let shows = |session: &RemoteControlSession| !url.is_empty() && session.shows(url);
    let launched: Vec<String> = every_screen
        .iter()
        .filter_map(|control| control.launch.as_ref().map(|launch| launch.handle.clone()))
        .collect();
    let mut sessions: HashMap<String, Arc<RemoteControlSession>> = HashMap::new();
    let rows: Vec<PreviewTargetRow> =
        super::preview_servers_for_session(state, workspace, &request.conversation_id)
            .into_iter()
            .filter_map(|server| {
                let session = match server.remote {
                    // A row whose session went away since the listing is gone.
                    Some(_) => Some(state.preview_servers.control_session(&server.handle)?),
                    None => None,
                };
                let row = PreviewTargetRow {
                    shown: session.as_deref().is_some_and(shows),
                    remote: session.is_some(),
                    launches_a_screen: launched.contains(&server.handle),
                    platform: session.as_deref().map(RemoteControlSession::platform),
                    handle: server.handle.clone(),
                    server_id: server.server_id,
                    status: server.status,
                };
                if let Some(session) = session {
                    sessions.insert(server.handle, session);
                }
                Some(row)
            })
            .collect();
    let page = PreviewPageFacts {
        attached,
        has_page: status.as_ref().is_some_and(|status| status.has_page),
        suspended: status.as_ref().is_some_and(|status| status.suspended),
        viewer_of: every_screen
            .iter()
            .find(|control| shows(&control.session))
            .map(|control| control.server.server_id.clone()),
    };
    let resolved = match screen_tool {
        Some(platforms) => Resolved::Screen(resolve_screen_target(
            &request.tool_name,
            platforms,
            server_id.as_deref(),
            &rows,
        )?),
        None => {
            let resolved = resolve_preview_target(server_id.as_deref(), &rows, &page)?;
            refuse_a_guess_at_the_page(&request.tool_name, server_id.as_deref(), resolved, &rows)?;
            resolved
        }
    };
    match resolved {
        Resolved::Page => Ok(PreviewTarget::Web(session_id)),
        Resolved::Screen(row) => {
            let session = sessions
                .remove(&row.handle)
                .ok_or_else(|| super::preview_stale_server_id(&row.server_id))?;
            Ok(PreviewTarget::Remote(RemoteScreen {
                handle: row.handle.clone(),
                server_id: row.server_id.clone(),
                session,
            }))
        }
    }
}

/// Whether `tool_name` acts on the page or a screen, and so has a target to resolve: the page
/// tools, `preview_input`, and the screen tools ([`super::screen_tools::SCREEN_TOOLS`]).
pub(crate) fn resolves_a_target(tool_name: &str) -> bool {
    tool_name == "preview_input"
        || PreviewTool::from_tool_name(tool_name).is_some()
        || super::screen_tools::screen_tool_platforms(tool_name).is_some()
}

/// The page tools that also act on remote-control screens of these kinds
/// (`docs/remote-control-unified-tools.md` §4.1, the second class): `preview_dialog` answers a
/// phone's or tablet's alert as well as the page's.
const PAGE_TOOLS_THAT_REACH_SCREENS: &[(&str, &[Platform])] = &[(
    "preview_dialog",
    super::screen_tools::DIALOG_SCREEN_PLATFORMS,
)];

/// Refuses to guess that a call without `serverId` means the page, for a page tool that may as
/// well mean a connected screen ([`PAGE_TOOLS_THAT_REACH_SCREENS`]). Before such a tool reached
/// screens, a call that [`resolve_preview_target`] landed on the page could only mean the page;
/// now, with a phone connected, a `preview_dialog` meant for its alert would answer the page's.
///
/// It refuses only where the page can be named back: a dev server's `serverId` addresses the page
/// ([`resolve_preview_target`] step 1). A page that no dev server stands for still takes the call,
/// as before — refusing would leave no way to answer its dialog at all.
fn refuse_a_guess_at_the_page(
    tool: &str,
    server_id: Option<&str>,
    resolved: Resolved<'_>,
    rows: &[PreviewTargetRow],
) -> Result<(), String> {
    if server_id.is_some() || !matches!(resolved, Resolved::Page) {
        return Ok(());
    }
    let Some((_, platforms)) = PAGE_TOOLS_THAT_REACH_SCREENS
        .iter()
        .find(|(name, _)| *name == tool)
    else {
        return Ok(());
    };
    let quoted = |rows: &[&PreviewTargetRow]| {
        rows.iter()
            .map(|row| format!("\"{}\"", row.server_id))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let screens: Vec<&PreviewTargetRow> = rows
        .iter()
        .filter(|row| {
            row.remote
                && row
                    .platform
                    .is_some_and(|platform| platforms.contains(&platform))
        })
        .collect();
    let dev_servers: Vec<&PreviewTargetRow> = rows
        .iter()
        .filter(|row| !row.remote && !row.launches_a_screen)
        .collect();
    if screens.is_empty() || dev_servers.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{tool} could mean the web page or {} {} ({}). Pass serverId to say which: {} for the screen, or {} for the page.",
        screen_words(platforms).one,
        if screens.len() == 1 {
            "that is connected"
        } else {
            "of those connected"
        },
        quoted(&screens),
        quoted(&screens[..1]),
        quoted(&dev_servers[..1]),
    ))
}

// ---------------------------------------------------------------------------
// What a call was judged for
// ---------------------------------------------------------------------------

/// What a call resolved to, without the session: what is compared between the moment a call is
/// judged and the moment it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreviewTargetKey {
    Page,
    /// A screen, by its row: the same `serverId` started again is another session.
    Screen {
        handle: String,
        server_id: String,
    },
    /// Nothing; the call answers with why when it runs.
    Unresolved,
}

impl PreviewTargetKey {
    fn of(target: &Result<PreviewTarget, String>) -> Self {
        target.as_ref().map_or(Self::Unresolved, PreviewTarget::key)
    }

    pub(crate) fn surface(&self) -> PreviewSurface {
        match self {
            Self::Screen { .. } => PreviewSurface::RemoteScreen,
            Self::Page | Self::Unresolved => PreviewSurface::Page,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Page => "the web page".to_owned(),
            Self::Screen { server_id, .. } => format!("the remote-control screen \"{server_id}\""),
            Self::Unresolved => "nothing it could act on".to_owned(),
        }
    }
}

/// How long a judgement is kept for a call the run loop never settled. Every call the run loop
/// judges is forgotten when it settles ([`SettleJudgement`]), however it ends; this only bounds a
/// judgement whose call was abandoned some other way, and is far longer than any approval card
/// waits (`tool_prompt`'s 30 minutes).
const JUDGEMENT_LIFETIME: std::time::Duration = std::time::Duration::from_secs(4 * 60 * 60);

/// Calls whose judgement outlived [`JUDGEMENT_LIFETIME`] and was dropped, remembered so that such
/// a call is refused if it ever runs rather than run unjudged; the oldest are forgotten first.
const MAX_EXPIRED_CALLS: usize = 1024;

struct JudgedCall {
    tool_name: String,
    input: JsonObject,
    judged_at: std::time::Instant,
    /// What a page tool or `preview_input` resolved to; `None` for `preview_start`.
    key: Option<PreviewTargetKey>,
    /// What a `preview_start` read ([`crate::preview::read_for_start`]).
    start: Option<Arc<crate::preview::StartReading>>,
}

/// One conversation's call, by its id.
type CallKey = (String, String);

#[derive(Default)]
struct JudgedCalls {
    calls: HashMap<CallKey, JudgedCall>,
    expired: VecDeque<CallKey>,
}

/// What a call was judged for, as dispatch finds it.
enum Judged {
    /// The run loop judged it, and this is what it found.
    Found(JudgedCall),
    /// Its judgement was dropped before it ran.
    Expired,
    /// It was never judged: a caller other than the run loop, which judges nothing.
    Never,
}

/// What each preview call of the run loop resolved to (or, for `preview_start`, read) when it was
/// judged, until it settles.
///
/// Held by the host ([`AppState`]) because the judging and the running are far apart: the
/// takeover gate and the classifier run in the run loop, the approval card in the conversation's
/// approval closure, and the call in the executor, each reached through its own chain of calls.
/// A call id is what joins them — the approval card's too, which the run loop hands the call's id
/// (`api::ApprovalRequester::tool_call_id`). Only a card asked for with no call id finds its call
/// by conversation, tool and arguments.
///
/// Kept per call, not in a bounded queue: a call waiting on its approval card for as long as the
/// card waits must still find its judgement when it runs, however many other calls — of this or
/// any other conversation — were judged meanwhile. The run loop forgets each call's judgement when
/// the call settles, run, denied or blocked ([`settle_on_drop`]).
#[derive(Default)]
pub(crate) struct PreviewCallTargets {
    calls: Mutex<JudgedCalls>,
}

impl PreviewCallTargets {
    fn calls(&self) -> std::sync::MutexGuard<'_, JudgedCalls> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records what call `call_id` was judged for, replacing what an earlier judgement of the same
    /// call recorded (a hook may rewrite its arguments, and then it is judged again).
    fn record(
        &self,
        call_id: &str,
        request: &ToolExecutionRequest,
        key: Option<PreviewTargetKey>,
        start: Option<Arc<crate::preview::StartReading>>,
    ) {
        let now = std::time::Instant::now();
        self.expire(now);
        let mut judged = self.calls();
        let id = (request.conversation_id.clone(), call_id.to_owned());
        judged.expired.retain(|expired| *expired != id);
        judged.calls.insert(
            id,
            JudgedCall {
                tool_name: request.tool_name.clone(),
                input: request.input.clone(),
                judged_at: now,
                key,
                start,
            },
        );
    }

    /// Drops the judgements older than [`JUDGEMENT_LIFETIME`] at `now`, remembering their calls.
    fn expire(&self, now: std::time::Instant) {
        let mut judged = self.calls();
        let stale: Vec<CallKey> = judged
            .calls
            .iter()
            .filter(|(_, call)| now.saturating_duration_since(call.judged_at) > JUDGEMENT_LIFETIME)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            judged.calls.remove(&id);
            if judged.expired.len() >= MAX_EXPIRED_CALLS {
                judged.expired.pop_front();
            }
            judged.expired.push_back(id);
        }
    }

    fn for_call(&self, conversation_id: &str, call_id: &str) -> Option<PreviewJudgementRecord> {
        self.calls()
            .calls
            .get(&(conversation_id.to_owned(), call_id.to_owned()))
            .map(PreviewJudgementRecord::of)
    }

    /// Call `call_id`'s judgement, when it was judged as `request` — the same tool with the same
    /// arguments. A judgement of other arguments is not this request's: a hook rewrote the call
    /// and it has not been judged again yet, or the id names another call.
    fn for_call_as(
        &self,
        call_id: &str,
        request: &ToolExecutionRequest,
    ) -> Option<PreviewJudgementRecord> {
        self.calls()
            .calls
            .get(&(request.conversation_id.clone(), call_id.to_owned()))
            .filter(|call| call.tool_name == request.tool_name && call.input == request.input)
            .map(PreviewJudgementRecord::of)
    }

    fn for_request(&self, request: &ToolExecutionRequest) -> Option<PreviewJudgementRecord> {
        self.calls()
            .calls
            .iter()
            .filter(|((conversation_id, _), call)| {
                *conversation_id == request.conversation_id
                    && call.tool_name == request.tool_name
                    && call.input == request.input
            })
            .max_by_key(|(_, call)| call.judged_at)
            .map(|(_, call)| PreviewJudgementRecord::of(call))
    }

    fn take(&self, conversation_id: &str, call_id: &str) -> Judged {
        let mut judged = self.calls();
        let id = (conversation_id.to_owned(), call_id.to_owned());
        if let Some(call) = judged.calls.remove(&id) {
            return Judged::Found(call);
        }
        if judged.expired.contains(&id) {
            return Judged::Expired;
        }
        Judged::Never
    }

    /// Forgets call `call_id` of `conversation_id`: it has settled.
    pub(crate) fn forget(&self, conversation_id: &str, call_id: &str) {
        let mut judged = self.calls();
        let id = (conversation_id.to_owned(), call_id.to_owned());
        judged.calls.remove(&id);
        judged.expired.retain(|expired| *expired != id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.calls().calls.len()
    }
}

/// A judgement as the classifier reads it, without taking it.
struct PreviewJudgementRecord {
    key: Option<PreviewTargetKey>,
    start: Option<Arc<crate::preview::StartReading>>,
}

impl PreviewJudgementRecord {
    fn of(call: &JudgedCall) -> Self {
        Self {
            key: call.key.clone(),
            start: call.start.clone(),
        }
    }
}

/// Forgets a call's judgement when dropped. The run loop holds one for each call from before it is
/// judged until it has settled, so the judgement lives exactly as long as its call — through an
/// approval card that waits, and not a moment past a denial, a hook's block, an error or a panic.
pub(crate) struct SettleJudgement {
    targets: Arc<PreviewCallTargets>,
    conversation_id: String,
    call_id: String,
}

impl Drop for SettleJudgement {
    fn drop(&mut self) {
        self.targets.forget(&self.conversation_id, &self.call_id);
    }
}

/// What the run loop holds for call `call_id` while it handles it ([`SettleJudgement`]).
pub(crate) fn settle_on_drop(
    state: &AppState,
    conversation_id: &str,
    call_id: &str,
) -> SettleJudgement {
    SettleJudgement {
        targets: state.preview_call_targets.clone(),
        conversation_id: conversation_id.to_owned(),
        call_id: call_id.to_owned(),
    }
}

/// The workspace a call acts in, as the executor selects it (`run_tool`): from the conversation's
/// set, or — for a caller that resolved none — the request's own directory.
fn selected_workspace(
    workspaces: &crate::workspace_set::WorkspaceSet,
    request: &ToolExecutionRequest,
) -> Option<crate::workspace_set::ResolvedWorkspace> {
    let fallback;
    let workspaces = if workspaces.is_empty() {
        fallback = crate::workspace_set::WorkspaceSet::local_root(&request.workspace_path);
        &fallback
    } else {
        workspaces
    };
    let requested = super::workspace_argument(&request.input).ok()?;
    workspaces
        .select_for(&request.tool_name, requested)
        .ok()
        .cloned()
}

/// The run loop's judgement of call `call_id`, made once before the takeover gate and the
/// classifier: recorded for both, and for the executor to hold the call to. Returns what the
/// classifier is told.
///
/// A page tool and `preview_input` resolve their target. A `preview_start` reads its workspace's
/// `.mewrk/launch.json` and picks the entry the start would, with the registry as it is now
/// ([`crate::preview::read_for_start`]): the approval card is built from that one reading, and the
/// start connects a remote-control entry only if it is the one this reading picked. Any other tool
/// is the page's business, as before.
pub(crate) fn judge_preview_call(
    state: &AppState,
    call_id: &str,
    request: &ToolExecutionRequest,
    workspaces: &crate::workspace_set::WorkspaceSet,
) -> PreviewJudgement {
    if request.tool_name == "preview_start" {
        let Some(name) = request.input.get("name").and_then(Value::as_str) else {
            return PreviewJudgement::default();
        };
        let reading = selected_workspace(workspaces, request).and_then(|workspace| {
            crate::preview::read_for_start(
                &state.preview_servers,
                &workspace,
                name,
                &request.conversation_id,
            )
        });
        let Some(reading) = reading.map(Arc::new) else {
            return PreviewJudgement::default();
        };
        state
            .preview_call_targets
            .record(call_id, request, None, Some(reading.clone()));
        return PreviewJudgement {
            surface: PreviewSurface::Page,
            start: Some(reading),
        };
    }
    if !resolves_a_target(&request.tool_name) {
        return PreviewJudgement::default();
    }
    let key = PreviewTargetKey::of(&preview_target_for(request, state));
    let surface = key.surface();
    state
        .preview_call_targets
        .record(call_id, request, Some(key), None);
    surface.into()
}

/// What a judgement tells the classifier.
fn judgement_of(
    record: Option<PreviewJudgementRecord>,
    request: &ToolExecutionRequest,
    state: &AppState,
) -> PreviewJudgement {
    if request.tool_name == "preview_start" {
        // An unjudged start is read by the classifier itself, and connects no screen.
        return PreviewJudgement {
            surface: PreviewSurface::Page,
            start: record.and_then(|record| record.start),
        };
    }
    if !resolves_a_target(&request.tool_name) {
        return PreviewJudgement::default();
    }
    record
        .and_then(|record| record.key)
        .unwrap_or_else(|| PreviewTargetKey::of(&preview_target_for(request, state)))
        .surface()
        .into()
}

/// What call `call_id` was judged for, for a later classification of the same call; a page tool
/// the run loop never judged is resolved now.
pub(crate) fn judged_surface(
    state: &AppState,
    call_id: &str,
    request: &ToolExecutionRequest,
) -> PreviewJudgement {
    let record = state
        .preview_call_targets
        .for_call(&request.conversation_id, call_id);
    judgement_of(record, request, state)
}

/// [`judged_surface`] for the approval card of `request`: by the call's id when the requester
/// names it, so that of two concurrent calls with the same tool and arguments — a subagent's shares
/// its parent's conversation id — each card is built from its own call's judgement. A judgement
/// recorded under that id for other arguments is not used: the call is then judged as one the run
/// loop never judged, which for `preview_start` reads the file and judges every entry the name
/// could start. Only a card with no call id falls back to the latest judgement of the same tool
/// and arguments.
pub(crate) fn judged_surface_for_approval(
    state: &AppState,
    call_id: Option<&str>,
    request: &ToolExecutionRequest,
) -> PreviewJudgement {
    let record = match call_id {
        Some(call_id) => state.preview_call_targets.for_call_as(call_id, request),
        None => state.preview_call_targets.for_request(request),
    };
    judgement_of(record, request, state)
}

/// The screen a call was judged to act on, as its approval card and its standing allowance need
/// it ([`judged_screen_for_approval`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JudgedScreen {
    /// The screen's registry row: what [`crate::tool_prompt::allowance_key`] keys an allowance
    /// to, so it holds for this screen and no other — nor for its entry started again. The handle
    /// comes from a per-process counter, so an allowance keyed to it lives in memory only and
    /// ends with the process ([`crate::tool_prompt::allowance_names_a_screen`]).
    pub handle: String,
    pub server_id: String,
    /// What kind of screen it is, while its session is there to say.
    pub platform: Option<Platform>,
    /// How the card names the screen: its server id and, when the device said, its own name —
    /// `pixel (Pixel 8)`. The device's name is its own text: the card escapes it as it does every
    /// argument.
    pub card_name: String,
}

/// The longest device name a card repeats.
const CARD_DEVICE_NAME_CHARS: usize = 60;

/// The screen call `request` was judged to act on, found as [`judged_surface_for_approval`] finds
/// its judgement: `None` when it was judged for the page or for nothing, and for `preview_start`,
/// whose card describes the entry it would start.
pub(crate) fn judged_screen_for_approval(
    state: &AppState,
    call_id: Option<&str>,
    request: &ToolExecutionRequest,
) -> Option<JudgedScreen> {
    if request.tool_name == "preview_start" || !resolves_a_target(&request.tool_name) {
        return None;
    }
    let record = match call_id {
        Some(call_id) => state.preview_call_targets.for_call_as(call_id, request),
        None => state.preview_call_targets.for_request(request),
    };
    let key = record
        .and_then(|record| record.key)
        .unwrap_or_else(|| PreviewTargetKey::of(&preview_target_for(request, state)));
    let PreviewTargetKey::Screen { handle, server_id } = key else {
        return None;
    };
    let session = state.preview_servers.control_session(&handle);
    let device = session
        .as_deref()
        .and_then(|session| session.summary().label)
        .map(|label| label.trim().chars().take(CARD_DEVICE_NAME_CHARS).collect::<String>())
        .filter(|label| !label.is_empty());
    let card_name = match device {
        Some(device) if device != server_id => format!("{server_id} ({device})"),
        _ => server_id.clone(),
    };
    Some(JudgedScreen {
        platform: session.as_deref().map(RemoteControlSession::platform),
        handle,
        server_id,
        card_name,
    })
}

/// Why call `call_id` cannot run on any screen, said before anything asks for it.
///
/// A screen tool ([`super::screen_tools::SCREEN_TOOLS`]) judged for no screen — none of the kinds
/// it reaches is connected, its `serverId` names a screen of another kind, a dev server or
/// nothing, or several are connected and it names none — would be approved only to be refused at
/// dispatch with the same answer; so would `preview_dialog` judged for a screen whose kind answers
/// no dialogs. The run loop refuses both before the approval card, with the answer dispatch would
/// give (`None`: the call can run, and is judged and approved as usual).
pub(crate) fn unreachable_screen_refusal(
    state: &AppState,
    call_id: &str,
    request: &ToolExecutionRequest,
) -> Option<String> {
    if super::screen_tools::screen_tool_platforms(&request.tool_name).is_some() {
        let judged = state
            .preview_call_targets
            .for_call(&request.conversation_id, call_id)
            .and_then(|record| record.key);
        if matches!(judged, Some(PreviewTargetKey::Screen { .. })) {
            return None;
        }
        return preview_target_for(request, state).err();
    }
    if request.tool_name == "preview_dialog" {
        let screen = judged_screen_for_approval(state, Some(call_id), request)?;
        let reached = screen.platform.is_none_or(|platform| {
            super::screen_tools::DIALOG_SCREEN_PLATFORMS.contains(&platform)
        });
        if !reached {
            return Some(super::screen_tools::no_dialogs_on_a_desktop(&screen.server_id));
        }
    }
    None
}

/// How a hook's input names a judged screen: `{"server_id": …, "platform": "android"}`.
pub(crate) fn judged_screen_for_hooks(screen: &JudgedScreen) -> Value {
    json!({
        "server_id": screen.server_id,
        "platform": match screen.platform {
            Some(Platform::Android) => "android",
            Some(Platform::Ios) => "ios",
            Some(Platform::Desktop) => "desktop",
            None => "unknown",
        },
    })
}

/// Whether call `call_id` was judged to act on the web page: the only target the signed-in-page
/// takeover gate is about. A screen is not the page, and a call that resolved to nothing does
/// not run.
pub(crate) fn judged_for_the_page(
    state: &AppState,
    call_id: &str,
    request: &ToolExecutionRequest,
) -> bool {
    state
        .preview_call_targets
        .for_call(&request.conversation_id, call_id)
        .and_then(|record| record.key)
        .unwrap_or_else(|| PreviewTargetKey::of(&preview_target_for(request, state)))
        == PreviewTargetKey::Page
}

/// Which remote-control entries `preview_start` call `call_id` may connect: the ones on the card
/// the person confirmed for it — the entry its judgement read, if that was a remote-control entry
/// — and none for a call the run loop judged no such entry for, or whose judgement is gone. A start
/// that is no model's call (the pane, a direct execution) is the person's own.
pub(crate) fn start_approval(
    state: &AppState,
    request: &ToolExecutionRequest,
    call_id: Option<&str>,
) -> crate::preview::ControlApproval {
    use crate::preview::ControlApproval;
    let Some(call_id) = call_id else {
        return ControlApproval::Person;
    };
    match state
        .preview_call_targets
        .take(&request.conversation_id, call_id)
    {
        Judged::Found(JudgedCall {
            start: Some(reading),
            ..
        }) => ControlApproval::Confirmed(reading.confirmed_controls()),
        Judged::Found(_) | Judged::Expired | Judged::Never => ControlApproval::Confirmed(Vec::new()),
    }
}

/// The target a call runs on: resolved again now, and held to what it was judged for. A call
/// judged for the page that would now land on a screen (or the other way round, or on another
/// screen) was approved, gated and classified for something it would not act on, so it is
/// refused and the model calls it again — as is one whose judgement was dropped before it ran. A
/// call that resolves to nothing now answers with why.
pub(crate) fn target_at_dispatch(
    request: &ToolExecutionRequest,
    state: &AppState,
    call_id: Option<&str>,
) -> Result<PreviewTarget, String> {
    let judged = call_id.map(|id| {
        state
            .preview_call_targets
            .take(&request.conversation_id, id)
    });
    if let Some(Judged::Expired) = judged {
        return Err(format!(
            "This call waited too long to run, and what it was checked for is no longer known, so it was not run. Call {} again.",
            request.tool_name
        ));
    }
    let live = preview_target_for(request, state)?;
    match judged {
        Some(Judged::Found(JudgedCall {
            key: Some(judged), ..
        })) if judged != live.key() => Err(format!(
            "What the preview shows changed while this call waited: it was checked for {} and would now act on {}, so it was not run. Call {} again.",
            judged.describe(),
            live.key().describe(),
            request.tool_name
        )),
        _ => Ok(live),
    }
}

// ---------------------------------------------------------------------------
// Viewer addresses
// ---------------------------------------------------------------------------

/// The secret path segment of a viewer page's address (`http://127.0.0.1:<port>/<token>/`).
fn viewer_token(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let token = parsed.path_segments()?.next()?;
    // A token is long and random; a short segment would match ordinary words.
    (token.len() >= 16).then(|| token.to_owned())
}

/// The page check every DevTools call runs under ([`crate::browser::PageGuard`]): no page tool
/// reaches a page that shows any remote-control screen's viewer, whichever conversation's it is.
/// [`target_at_dispatch`] refuses such a page when the call is dispatched; this is the same rule,
/// asked again by the browser runtime right before and right after the call itself.
pub(crate) fn viewer_page_guard(state: &AppState) -> crate::browser::PageGuard {
    let armed = state.preview_servers.clone();
    let registry = state.preview_servers.clone();
    crate::browser::PageGuard {
        armed: Arc::new(move || !armed.controls().is_empty()),
        refuse: Arc::new(move |url: &str| {
            if url.is_empty() {
                return None;
            }
            registry
                .controls()
                .into_iter()
                .find(|control| control.session.shows(url))
                .map(|control| preview_shows_remote_screen(&control.server.server_id))
        }),
    }
}

/// `text` with every viewer page's address — any spelling of it that carries its token — replaced
/// by the screen's server id. The page tools refuse to act on a viewer, and the browser runtime
/// checks the page again right before and after each DevTools call, so a page's output names one
/// only when something slipped past both; that, an error naming the page, and anything else page
/// output can echo, is still not the model's to read. The token is the viewer's whole
/// authorization — a page script that knew it could drive the screen — so it is looked for in the
/// encodings a script would turn it into, not only as written ([`token_spellings`]).
pub(crate) fn scrub_viewer_addresses(state: &AppState, text: String) -> String {
    let mut text = text;
    for control in state.preview_servers.controls() {
        let Some(token) = viewer_token(&control.session.viewer_url()) else {
            continue;
        };
        let replacement = format!("[remote-control screen \"{}\"]", control.server.server_id);
        text = scrub_token(&text, &token, &replacement);
    }
    text
}

/// [`scrub_viewer_addresses`] for one token.
fn scrub_token(text: &str, token: &str, replacement: &str) -> String {
    let replace = |text: &str, pattern: &str, literal: &str| match regex::Regex::new(pattern) {
        Ok(found) => found
            .replace_all(text, regex::NoExpand(replacement))
            .into_owned(),
        Err(_) => text.replace(literal, replacement),
    };
    // The address around the token, in any case, with whatever scheme, host and path it has.
    let address = format!(
        r#"(?i)(?:[a-z][a-z0-9+.-]*://[^\s"'<>()\[\]]*?)?{}[^\s"'<>()\[\]]*"#,
        regex::escape(token)
    );
    let mut text = replace(text, &address, token);
    for (spelling, any_case) in token_spellings(token) {
        let pattern = if any_case {
            format!("(?i){}", regex::escape(&spelling))
        } else {
            regex::escape(&spelling)
        };
        text = replace(&text, &pattern, &spelling);
    }
    text
}

/// The token as page output could carry it other than as written: reversed; hex-encoded,
/// percent-encoded, or as `\u`/`\x` escapes, character by character; and base64 (both alphabets),
/// as the part of an encoding that the token alone decides at each of the three alignments it can
/// have inside a longer encoded string — `btoa(location.href)` included. Each comes with whether
/// its case is free (`true` for everything but base64, where case is data).
fn token_spellings(token: &str) -> Vec<(String, bool)> {
    use base64::Engine;
    let bytes = token.as_bytes();
    let each = |format: &dyn Fn(u8) -> String| bytes.iter().map(|byte| format(*byte)).collect();
    let mut spellings: Vec<(String, bool)> = vec![
        (token.chars().rev().collect(), true),
        (each(&|byte| format!("{byte:02x}")), true),
        (each(&|byte| format!("%{byte:02x}")), true),
        (each(&|byte| format!("\\u{byte:04x}")), true),
        (each(&|byte| format!("\\x{byte:02x}")), true),
    ];
    for engine in [
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ] {
        for offset in 0..3 {
            let mut data = vec![0_u8; offset];
            data.extend_from_slice(bytes);
            let encoded = engine.encode(&data);
            // Groups of three bytes that lie wholly inside the token encode to four characters
            // that depend on nothing else.
            let first_group = offset.div_ceil(3);
            let end_group = (offset + bytes.len()) / 3;
            if end_group > first_group {
                spellings.push((encoded[first_group * 4..end_group * 4].to_owned(), false));
            }
        }
    }
    spellings
}

// ---------------------------------------------------------------------------
// Acting on a screen
// ---------------------------------------------------------------------------

/// How long a call on a screen gives the session, counted from the call's start: inside the page
/// tools' 30 s wall clock ([`PREVIEW_TOOL_TIMEOUT`]) with room left for the answer to come back.
/// The session keeps [`crate::remote_control::session::CALL_MARGIN`] of it for its own answer,
/// waits for a reconnect or for the input lock only as long as the rest allows, and starts no
/// action that could not end in time — so a batch that runs long answers with what it did.
pub(super) const SCREEN_CALL_DEADLINE: Duration = Duration::from_secs(27);

/// Runs `work` against a screen's session under the page tools' 30 s wall clock, on a thread of
/// its own: a device can stall, and the turn must not stall with it.
///
/// `work` is handed the call's [`CallContext`]: its deadline, [`SCREEN_CALL_DEADLINE`] from now,
/// and whether it is cancelled — the turn or the task was stopped (`cancel`), or the wall clock
/// ran out after all. The session's `*_within` methods honour both, so the answer is the
/// session's own account of what ran, even of a batch cut short; the timeout below is for a
/// driver call that never returns, and says that some of the call may have reached the screen.
fn on_screen<T: Send + 'static>(
    tool: &str,
    screen: &RemoteScreen,
    cancel: &CancelSignal,
    work: impl FnOnce(&RemoteControlSession, &CallContext<'_>) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    on_screen_within(
        tool,
        screen,
        cancel,
        SCREEN_CALL_DEADLINE,
        PREVIEW_TOOL_TIMEOUT,
        work,
    )
}

/// How often the wait for a screen call's answer looks at the call's cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(100);
/// How long a stopped call's work gets to send its own account — what ran, where it stopped —
/// before the call answers for it: the session's methods notice a cancellation within their
/// next step, but a driver call already on the wire (an install's wait for the package
/// installer, a slow device's reply) ends only when it ends.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// [`on_screen`] with its two clocks given: `deadline` for the session, `envelope` for the wait.
///
/// The wait looks at `cancel` every [`CANCEL_POLL`]: once the call is stopped its work has
/// [`CANCEL_GRACE`] to answer, and then the call answers without it — a stopped turn is never
/// held by a device for the rest of a long envelope — and the work, told the call has ended,
/// starts nothing more.
pub(super) fn on_screen_within<T: Send + 'static>(
    tool: &str,
    screen: &RemoteScreen,
    cancel: &CancelSignal,
    deadline: Duration,
    envelope: Duration,
    work: impl FnOnce(&RemoteControlSession, &CallContext<'_>) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let started = Instant::now();
    let (sender, receiver) = mpsc::channel();
    let session = screen.session.clone();
    let expired = Arc::new(AtomicBool::new(false));
    let flag = expired.clone();
    let worker_cancel = cancel.clone();
    thread::spawn(move || {
        let cancelled = || worker_cancel.cancelled() || flag.load(Ordering::Acquire);
        let call = CallContext {
            deadline: started + deadline,
            cancelled: &cancelled,
        };
        let _ = sender.send(work(&session, &call));
    });
    let mut stopped_at: Option<Instant> = None;
    loop {
        let left = envelope.saturating_sub(started.elapsed());
        let grace = stopped_at.map_or(CANCEL_POLL, |at| CANCEL_GRACE.saturating_sub(at.elapsed()));
        match receiver.recv_timeout(left.min(grace).min(CANCEL_POLL)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!(
                    "{tool} failed inside the remote-control session of \"{}\"",
                    screen.server_id
                ))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if stopped_at.is_none() && cancel.cancelled() {
            stopped_at = Some(Instant::now());
        }
        let out_of_grace = stopped_at.is_some_and(|at| at.elapsed() >= CANCEL_GRACE);
        let out_of_time = started.elapsed() >= envelope;
        if !out_of_grace && !out_of_time {
            continue;
        }
        // Whatever the session is still waiting on, nothing more is started once it returns.
        expired.store(true, Ordering::Release);
        return Err(if stopped_at.is_some() {
            format!(
                "{tool} was stopped while the remote-control screen \"{}\" was still working on it, so some of what this call asked for may still complete there. Take a preview_screenshot before repeating any of it.",
                screen.server_id
            )
        } else {
            format!(
                "{tool} timed out after {}s on the remote-control screen \"{}\": the device stopped answering in the middle of it, so some of what this call asked for may already have happened there. Take a preview_screenshot before repeating any of it. preview_logs with serverId \"{}\" shows its connection log.",
                envelope.as_secs(),
                screen.server_id,
                screen.server_id
            )
        });
    }
}

/// The uid `preview_click` or `preview_fill` names on a screen; a selector means nothing there.
fn screen_element_uid(input: &JsonObject) -> Result<u64, String> {
    match crate::browser::element_target_input(input) {
        Ok(crate::browser::ElementTarget::Uid(uid)) => Ok(uid),
        Ok(crate::browser::ElementTarget::Selector(_)) => Err(REMOTE_SELECTOR.to_owned()),
        Err(error) => Err(format!("{error}. {REMOTE_SELECTOR}")),
    }
}

/// A page tool on a remote-control screen (`docs/remote-control.md` §4): the four that mean
/// something on a screen act on it, the rest say what to use instead.
///
/// `cancel` is the call's: a click or a fill whose turn is stopped while it waits for the screen
/// clicks and types nothing more.
pub(crate) fn run_page_tool_on_screen(
    request: &ToolExecutionRequest,
    tool: PreviewTool,
    screen: &RemoteScreen,
    attachment_store: Option<&ImageAttachmentStore>,
    cancel: &CancelSignal,
) -> Result<Outcome, String> {
    let name = tool.as_str();
    match tool {
        PreviewTool::Screenshot => {
            let scale_ignored = request
                .input
                .get("scale")
                .is_some_and(|scale| !scale.is_null());
            let shot = on_screen(name, screen, cancel, |session, call| {
                session.screenshot_within(call)
            })?;
            screenshot_outcome(&shot, scale_ignored, None, attachment_store)
        }
        PreviewTool::Snapshot => {
            on_screen(name, screen, cancel, |session, call| {
                session.snapshot_within(call)
            })
            .map(Outcome::success)
        }
        PreviewTool::Click => {
            let uid = screen_element_uid(&request.input)?;
            let double = optional_bool(&request.input, "doubleClick", false)?;
            on_screen(name, screen, cancel, move |session, call| {
                session.click_uid_within(uid, double, call)
            })
            .map(Outcome::success)
        }
        PreviewTool::Fill => {
            let uid = screen_element_uid(&request.input)?;
            let value = match request.input.get("value") {
                Some(Value::String(value)) => value.clone(),
                _ => return Err("value must be a string".to_owned()),
            };
            on_screen(name, screen, cancel, move |session, call| {
                session.fill_uid_within(uid, &value, call)
            })
            .map(Outcome::success)
        }
        // A phone's or tablet's alert, answered by its platform (`screen_tools`).
        PreviewTool::Dialog => super::screen_tools::run_dialog_on_screen(request, screen, cancel),
        PreviewTool::ConsoleLogs
        | PreviewTool::Inspect
        | PreviewTool::Eval
        | PreviewTool::Network
        | PreviewTool::Resize
        | PreviewTool::UploadImage => Err(not_a_web_page(name, &screen.server_id)),
    }
}

/// A screen's picture as the model receives it: a conversation image attachment, and beside it
/// the coordinate frame its pixels are in. `lead` is text to put before that frame.
///
/// The picture is the coordinate frame, pixel for pixel — `preview_input` reads its x/y in it. The
/// session encodes it within the attachment pipeline's sides and byte budget, so the pipeline
/// hands it back unscaled; one that came back any other size is not delivered at all, since every
/// coordinate read off it would land somewhere else.
fn screenshot_outcome(
    shot: &ModelScreenshot,
    scale_ignored: bool,
    lead: Option<String>,
    attachment_store: Option<&ImageAttachmentStore>,
) -> Result<Outcome, String> {
    let store = attachment_store.ok_or_else(|| {
        "preview_screenshot needs the conversation's attachment store to hand back pixels"
            .to_owned()
    })?;
    let name = format!("preview-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    let image = store.import_compressed(&name, &shot.jpeg)?;
    if (image.width, image.height) != (shot.width, shot.height) {
        return Err(format!(
            "The screenshot came out {}x{} instead of its {}x{} coordinate frame, so it was not delivered: x/y read off it would land in the wrong place. Take another preview_screenshot.",
            image.width, image.height, shot.width, shot.height
        ));
    }
    let mut frame = json!({
        "width": shot.width,
        "height": shot.height,
        "screen": {"width": shot.screen_width, "height": shot.screen_height},
        "note": "x/y for preview_input are pixels of this image",
    });
    if scale_ignored {
        frame["scale"] =
            json!("ignored: a remote-control screen's screenshot is always its coordinate frame");
    }
    let frame = serde_json::to_string_pretty(&frame)
        .map_err(|error| format!("Failed to encode screenshot result: {error}"))?;
    let output = match lead {
        Some(lead) => format!("{lead}\n\n{frame}"),
        None => frame,
    };
    Ok(Outcome::success_with_images(output, vec![image]))
}

/// `preview_input`: a batch of pixel-level actions on a remote-control screen, and with
/// `screenshot: true` a picture of the screen after it.
///
/// The batch stops between actions once `cancel` is raised — the turn or the task was stopped —
/// and starts no action that could not end inside the call's time ([`on_screen`]); the report
/// says which actions ran and which did not. A model that cannot see
/// images never gets here with `screenshot: true`: the run loop refuses that call first.
pub(crate) fn run_preview_input(
    request: &ToolExecutionRequest,
    state: &AppState,
    attachment_store: Option<&ImageAttachmentStore>,
    cancel: &CancelSignal,
    call_id: Option<&str>,
) -> Result<Outcome, String> {
    let actions = parse_actions(request.input.get("actions").unwrap_or(&Value::Null))?;
    let screenshot = optional_bool(&request.input, "screenshot", false)?;
    let named = optional_owned_string(&request.input, "serverId", 256)?.is_some();
    let no_screen = || {
        !named
            && !super::preview_servers_for_session(
                state,
                Path::new(&request.workspace_path),
                &request.conversation_id,
            )
            .iter()
            .any(|server| server.remote.is_some())
    };
    let screen = match target_at_dispatch(request, state, call_id) {
        Ok(PreviewTarget::Remote(screen)) => screen,
        Ok(PreviewTarget::Web(_)) if no_screen() => return Err(PREVIEW_INPUT_NO_SCREEN.into()),
        Ok(PreviewTarget::Web(_)) => return Err(PREVIEW_INPUT_ON_A_PAGE.into()),
        Err(_) if no_screen() => return Err(PREVIEW_INPUT_NO_SCREEN.into()),
        Err(error) => return Err(error),
    };
    let (report, shot) = on_screen("preview_input", &screen, cancel, move |session, call| {
        let report = session.run_input_within(&actions, call)?;
        let shot = (screenshot && !(call.cancelled)()).then(|| session.screenshot_within(call));
        Ok((report, shot))
    })?;
    let report = report.lines.join("\n");
    match shot {
        Some(Ok(shot)) => screenshot_outcome(&shot, false, Some(report), attachment_store),
        Some(Err(error)) => Ok(Outcome::success(format!(
            "{report}\n\nThe screenshot after the batch failed: {error}"
        ))),
        None if screenshot => Ok(Outcome::success(format!(
            "{report}\n\nNo screenshot was taken: the batch was cancelled."
        ))),
        None => Ok(Outcome::success(report)),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use serde_json::{json, Value};

    use super::*;
    use crate::{
        model::ToolExecutionResponse,
        path_guard::ExecutionScope,
        prompt_profile::PromptProfile,
        remote_control::{
            driver::{ClipboardChannel, Platform, Rect, UiNode, WdaRoute},
            fake::{self, FakeAndroid, FakeDesktop, FakeDriver, FakeIos},
            place::ControlEnvironment,
        },
        workspace_set::WorkspaceSet,
    };

    const CONVERSATION: &str = "conversation-test";

    fn row(server_id: &str, remote: bool, shown: bool) -> PreviewTargetRow {
        PreviewTargetRow {
            handle: format!("h-{server_id}"),
            server_id: server_id.to_owned(),
            status: PreviewServerStatus::Running,
            remote,
            shown,
            launches_a_screen: false,
            platform: remote.then_some(Platform::Android),
        }
    }

    fn page(open: bool) -> PreviewPageFacts {
        PreviewPageFacts {
            attached: true,
            has_page: open,
            suspended: false,
            viewer_of: None,
        }
    }

    fn screen_of<'a>(resolved: Result<Resolved<'a>, String>) -> &'a str {
        match resolved {
            Ok(Resolved::Screen(row)) => &row.server_id,
            other => panic!("expected a screen, got {other:?}"),
        }
    }

    /// The four rules of `docs/remote-control.md` §4, and the source's refusals where nothing
    /// resolves.
    #[test]
    fn a_call_resolves_by_its_server_id_then_by_what_the_preview_shows() {
        let rows = [
            row("web", false, false),
            row("pixel", true, false),
            row("mac", true, false),
        ];
        // 1. An explicit id: a remote-control row is its screen, a dev server is the page.
        assert_eq!(
            screen_of(resolve_preview_target(Some("PIXEL"), &rows, &page(true))),
            "pixel"
        );
        assert_eq!(
            resolve_preview_target(Some("web"), &rows, &page(true)),
            Ok(Resolved::Page)
        );
        assert_eq!(
            resolve_preview_target(Some("gone"), &rows, &page(true)),
            Err(crate::tool_executor::preview_stale_server_id("gone"))
        );

        // 2. No id, and the page shows a screen's viewer: that screen, dev servers or not.
        let shown = [
            row("web", false, false),
            row("pixel", true, false),
            row("mac", true, true),
        ];
        assert_eq!(
            screen_of(resolve_preview_target(None, &shown, &page(true))),
            "mac"
        );

        // 3. No id, no page, no dev server, one screen: that screen. A suspended page is a page.
        let lone = [row("pixel", true, false)];
        assert_eq!(
            screen_of(resolve_preview_target(None, &lone, &page(false))),
            "pixel"
        );
        let unattached = PreviewPageFacts::default();
        assert_eq!(
            screen_of(resolve_preview_target(None, &lone, &unattached)),
            "pixel"
        );
        let suspended = PreviewPageFacts {
            suspended: true,
            ..page(false)
        };
        assert_eq!(
            resolve_preview_target(None, &lone, &suspended),
            Ok(Resolved::Page)
        );
        let several = [row("pixel", true, false), row("mac", true, false)];
        let error = resolve_preview_target(None, &several, &page(false)).unwrap_err();
        assert!(error.contains("(\"pixel\", \"mac\")"), "{error}");
        assert!(error.contains("Pass serverId"), "{error}");

        // 4. Otherwise the page: a web page tool never falls back to a screen.
        assert_eq!(
            resolve_preview_target(None, &lone, &page(true)),
            Ok(Resolved::Page)
        );
        let with_dev = [row("pixel", true, false), row("web", false, false)];
        assert_eq!(
            resolve_preview_target(None, &with_dev, &page(false)),
            Ok(Resolved::Page)
        );
        // The process a screen runs on — a virtual machine its `launch` started — is no dev
        // server: with no page open, the one screen is still what a call means. Starting, as such
        // a process stays while its port has nothing HTTP to say, it counts no differently.
        let with_launch = [
            row("debian-vm", true, false),
            PreviewTargetRow {
                launches_a_screen: true,
                status: PreviewServerStatus::Starting,
                ..row("vm-proc", false, false)
            },
        ];
        assert_eq!(
            screen_of(resolve_preview_target(None, &with_launch, &page(false))),
            "debian-vm"
        );
        assert_eq!(
            resolve_preview_target(None, &with_launch, &page(true)),
            Ok(Resolved::Page)
        );
        let starting = [PreviewTargetRow {
            status: PreviewServerStatus::Starting,
            ..row("web", false, false)
        }];
        assert_eq!(
            resolve_preview_target(None, &starting, &unattached),
            Ok(Resolved::Page)
        );
        assert_eq!(
            resolve_preview_target(None, &[], &page(false)),
            Err(crate::tool_executor::PREVIEW_PANE_NOT_OPEN.to_owned())
        );
        assert_eq!(
            resolve_preview_target(None, &[], &unattached),
            Err(crate::tool_executor::PREVIEW_NO_SERVERS.to_owned())
        );
    }

    /// While the page shows a viewer — this conversation's or any other's — no call reaches it
    /// through DevTools, whatever dev server it names.
    #[test]
    fn a_page_tool_never_reaches_a_page_that_shows_a_viewer() {
        let viewer = PreviewPageFacts {
            viewer_of: Some("pixel".into()),
            ..page(true)
        };
        let expected = Err(preview_shows_remote_screen("pixel"));
        let rows = [row("web", false, false)];
        assert_eq!(
            resolve_preview_target(Some("web"), &rows, &viewer),
            expected
        );
        assert_eq!(resolve_preview_target(None, &rows, &viewer), expected);
        assert_eq!(resolve_preview_target(None, &[], &viewer), expected);
        assert_eq!(
            preview_shows_remote_screen("pixel"),
            "The preview shows the remote-control screen \"pixel\"; pass serverId \"pixel\" to act on it, or open a web page first."
        );
        // Naming the screen is the way through.
        let mine = [row("web", false, false), row("pixel", true, true)];
        assert_eq!(
            screen_of(resolve_preview_target(Some("pixel"), &mine, &viewer)),
            "pixel"
        );
    }

    fn request(workspace: &Path, tool_name: &str, input: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            conversation_id: CONVERSATION.into(),
            workspace_path: workspace.to_string_lossy().into_owned(),
            tool_name: tool_name.into(),
            input: input.as_object().cloned().unwrap_or_default(),
        }
    }

    /// A workspace with `entries` as its launch.json, an app-data folder for attachments, and the
    /// host state its screens are registered in.
    struct Bench {
        workspace: tempfile::TempDir,
        app_data: tempfile::TempDir,
        state: AppState,
    }

    impl Bench {
        fn new(entries: &str) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let mewrk = workspace.path().join(".mewrk");
            std::fs::create_dir_all(&mewrk).unwrap();
            std::fs::write(
                mewrk.join("launch.json"),
                format!(r#"{{"configurations":[{entries}]}}"#),
            )
            .unwrap();
            Self {
                workspace,
                app_data: tempfile::tempdir().unwrap(),
                state: AppState::default(),
            }
        }

        fn path(&self) -> &Path {
            self.workspace.path()
        }

        fn request(&self, tool: &str, input: Value) -> ToolExecutionRequest {
            request(self.path(), tool, input)
        }

        /// Starts entry `name` with `driver` behind it, as the model's `preview_start` does.
        fn start(&self, name: &str, driver: Arc<FakeDriver>) -> String {
            let environment = ControlEnvironment {
                machines: Ok(vec![fake::machine("m1", "mac", "203.0.113.7")]),
                launch_port_wait: Duration::ZERO,
                ..ControlEnvironment::this_computer()
            }
            .with_route_check(Arc::new(|_| Ok(())))
            .with_connect(fake::ready(driver));
            let local = WorkspaceSet::local_root(self.path().to_string_lossy().into_owned());
            crate::tool_executor::run_preview_start(
                &self.request("preview_start", json!({"name": name})),
                &self.state,
                local.select(None).unwrap(),
                self.path(),
                &environment,
                None,
            )
            .unwrap()
            .output
        }

        /// `preview_start` of `name` as the run loop dispatches call `call_id` of it, with `connect`
        /// behind every session.
        fn start_call(
            &self,
            name: &str,
            connect: crate::remote_control::session::ConnectFn,
            call_id: Option<&str>,
        ) -> Result<Outcome, String> {
            let environment = ControlEnvironment {
                launch_port_wait: Duration::ZERO,
                ..ControlEnvironment::this_computer()
            }
            .with_connect(connect);
            let local = WorkspaceSet::local_root(self.path().to_string_lossy().into_owned());
            crate::tool_executor::run_preview_start(
                &self.request("preview_start", json!({"name": name})),
                &self.state,
                local.select(None).unwrap(),
                self.path(),
                &environment,
                call_id,
            )
        }

        fn rewrite(&self, entries: &str) {
            std::fs::write(
                self.path().join(".mewrk").join("launch.json"),
                format!(r#"{{"configurations":[{entries}]}}"#),
            )
            .unwrap();
        }

        /// One tool call through the executor, with the conversation's attachment store.
        fn call_with(
            &self,
            tool: &str,
            input: Value,
            cancel: &CancelSignal,
        ) -> ToolExecutionResponse {
            crate::tool_executor::execute_with_scope_and_attachments_verified(
                self.request(tool, input),
                &self.state,
                ExecutionScope::workspace_only(self.path()),
                Some(self.app_data.path()),
                cancel,
                &WorkspaceSet::local_root(self.path().to_string_lossy().into_owned()),
                None,
                &PromptProfile::builtin(),
            )
            .result
        }

        fn call(&self, tool: &str, input: Value) -> ToolExecutionResponse {
            self.call_with(tool, input, &CancelSignal::default())
        }

        fn session(&self, server_id: &str) -> Arc<RemoteControlSession> {
            self.state
                .preview_servers
                .controls()
                .into_iter()
                .find(|control| control.server.server_id == server_id)
                .expect("the screen's row")
                .session
        }

        /// The secret parts of every viewer's address.
        fn viewer_secrets(&self) -> Vec<String> {
            self.state
                .preview_servers
                .controls()
                .iter()
                .flat_map(|control| {
                    let url = control.session.viewer_url();
                    let token = viewer_token(&url).expect("a viewer token");
                    [url, token]
                })
                .collect()
        }
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            self.state.preview_servers.stop_all();
        }
    }

    fn assert_hidden(secrets: &[String], text: &str) {
        assert!(!secrets.is_empty());
        for secret in secrets {
            assert!(!text.contains(secret.as_str()), "{text}");
        }
    }

    fn uid_of(snapshot: &str, role: &str) -> u64 {
        let line = snapshot
            .lines()
            .find(|line| line.contains(&format!("] {role}")))
            .unwrap_or_else(|| panic!("no {role} in {snapshot}"));
        line.trim()
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn phone_tree() -> UiNode {
        let element = |role: &str, name: &str, (x, y, width, height)| UiNode {
            role: role.into(),
            name: name.into(),
            bounds: Some(Rect {
                x,
                y,
                width,
                height,
            }),
            enabled: true,
            ..UiNode::default()
        };
        UiNode {
            role: "FrameLayout".into(),
            bounds: Some(Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            }),
            enabled: true,
            children: vec![
                element("EditText", "Search", (100, 300, 880, 120)),
                element("Button", "OK", (100, 2000, 400, 200)),
            ],
            ..UiNode::default()
        }
    }

    const PHONE: &str =
        r#"{"name":"pixel","type":"remote-control","protocol":"adb","device":"emulator-5554"}"#;

    /// Every page tool on a phone: the four that mean something there act on the screen through
    /// its session, the rest say what to use instead — and none of it carries the viewer's address.
    #[test]
    fn every_page_tool_on_a_phone_acts_on_the_screen_or_says_what_to_use() {
        let bench = Bench::new(PHONE);
        let driver = FakeDriver::touch_with_tree(phone_tree());
        let started = bench.start("pixel", driver.clone());
        let secrets = bench.viewer_secrets();
        let mut seen = vec![started];

        let shot = bench.call("preview_screenshot", json!({"scale": 0.5}));
        assert!(shot.success, "{}", shot.output);
        assert_eq!(shot.images.len(), 1);
        assert_eq!((shot.images[0].width, shot.images[0].height), (576, 1280));
        let frame: Value = serde_json::from_str(&shot.output).unwrap();
        assert_eq!(frame["width"], 576);
        assert_eq!(frame["height"], 1280);
        assert_eq!(frame["screen"], json!({"width": 1080, "height": 2400}));
        assert_eq!(
            frame["note"],
            "x/y for preview_input are pixels of this image"
        );
        assert!(frame["scale"].as_str().unwrap().starts_with("ignored"));
        seen.push(shot.output);

        let snapshot = bench.call("preview_snapshot", json!({}));
        assert!(snapshot.success, "{}", snapshot.output);
        assert!(
            snapshot.output.starts_with("Screen 576x1280"),
            "{}",
            snapshot.output
        );
        let button = uid_of(&snapshot.output, "Button");
        let field = uid_of(&snapshot.output, "EditText");
        seen.push(snapshot.output);

        let clicked = bench.call("preview_click", json!({"uid": button}));
        assert!(clicked.success, "{}", clicked.output);
        assert!(
            clicked
                .output
                .starts_with(&format!("Clicked [{button}] Button \"OK\"")),
            "{}",
            clicked.output
        );
        let doubled = bench.call(
            "preview_click",
            json!({"uid": format!("[{button}]"), "doubleClick": true}),
        );
        assert!(
            doubled.output.starts_with("Double-clicked"),
            "{}",
            doubled.output
        );
        let filled = bench.call("preview_fill", json!({"uid": field, "value": "cats"}));
        assert!(filled.success, "{}", filled.output);
        assert!(
            filled.output.contains("typed 4 characters"),
            "{}",
            filled.output
        );
        assert_eq!(
            driver.inputs(),
            [
                "tap 300,2100 x1",
                "tap 300,2100 x2",
                "tap 540,360 x1",
                "text cats"
            ]
        );
        seen.extend([clicked.output, doubled.output, filled.output]);

        for (tool, input) in [
            ("preview_click", json!({"selector": "button"})),
            ("preview_fill", json!({"selector": "input", "value": "x"})),
        ] {
            let refused = bench.call(tool, input);
            assert!(!refused.success);
            assert_eq!(refused.output, REMOTE_SELECTOR, "{tool}");
        }
        for (tool, input) in [
            ("preview_eval", json!({"expression": "document.title"})),
            ("preview_inspect", json!({"selector": "button"})),
            ("preview_network", json!({})),
            ("preview_console_logs", json!({})),
            ("preview_resize", json!({"preset": "mobile"})),
        ] {
            let refused = bench.call(tool, input);
            assert!(!refused.success, "{tool}");
            assert_eq!(refused.output, not_a_web_page(tool, "pixel"));
            seen.push(refused.output);
        }
        // preview_dialog answers a phone's alert, by its platform: with no default answer, and
        // only with the parameters its schema states.
        for (input, expected) in [
            (json!({}), super::super::screen_tools::DIALOG_NEEDS_AN_ANSWER.to_owned()),
            (
                json!({"accept": true, "dismiss": true}),
                "preview_dialog has no parameter \"dismiss\"; it takes serverId, accept, prompt_text and button.".to_owned(),
            ),
        ] {
            let refused = bench.call("preview_dialog", input);
            assert!(!refused.success);
            assert_eq!(refused.output, expected);
            seen.push(refused.output);
        }
        // …and reaches the phone's own dialog handling, not the page's: this fake is an Android
        // screen with a tree but no adb operations, which the Android handling says it is not
        // (the page's would have said "not a web page"), and nothing was pressed.
        let inputs_before = driver.inputs();
        let reached = bench.call("preview_dialog", json!({"button": "Allow"}));
        assert!(!reached.success, "{}", reached.output);
        assert_eq!(reached.output, "This screen is not an Android device.");
        assert_eq!(driver.inputs(), inputs_before);
        seen.push(reached.output);
        let upload_request = bench.request("preview_upload_image", json!({}));
        let screen = match preview_target_for(&upload_request, &bench.state) {
            Ok(PreviewTarget::Remote(screen)) => screen,
            other => panic!("{other:?}"),
        };
        let upload = run_page_tool_on_screen(
            &upload_request,
            PreviewTool::UploadImage,
            &screen,
            None,
            &CancelSignal::default(),
        )
        .err()
        .unwrap();
        assert_eq!(upload, not_a_web_page("preview_upload_image", "pixel"));

        for listed in ["preview_list", "preview_logs"] {
            seen.push(bench.call(listed, json!({})).output);
        }
        for text in &seen {
            assert_hidden(&secrets, text);
        }
    }

    /// The picture is the coordinate frame whatever the screen shows: a screen every pixel of
    /// which differs is over the attachment budget at the usual quality, and still arrives at the
    /// frame's size, within the budget.
    #[test]
    fn a_screenshot_is_delivered_at_exactly_the_size_of_its_frame() {
        for (platform, width, height, frame) in [
            (Platform::Android, 1080, 2400, (576_u32, 1280_u32)),
            (Platform::Desktop, 2560, 1600, (1280, 800)),
        ] {
            let bench = Bench::new(
                r#"{"name":"pixel","type":"remote-control","protocol":"adb"},
                   {"name":"desk","type":"remote-control","protocol":"vnc","machine":"mac"}"#,
            );
            let name = if platform == Platform::Desktop {
                "desk"
            } else {
                "pixel"
            };
            bench.start(name, FakeDriver::noisy(platform, width, height));
            let shot = bench.call("preview_screenshot", json!({}));
            assert!(shot.success, "{}", shot.output);
            let image = &shot.images[0];
            assert_eq!((image.width, image.height), frame);
            assert!(
                image.bytes <= crate::image_attachments::MODEL_IMAGE_BYTE_BUDGET as u64,
                "{} bytes",
                image.bytes
            );
            let reported: Value = serde_json::from_str(&shot.output).unwrap();
            assert_eq!(reported["width"], json!(frame.0));
            assert_eq!(reported["height"], json!(frame.1));
            assert_eq!(
                reported["screen"],
                json!({"width": width, "height": height})
            );
            assert!(reported.get("scale").is_none());
        }
    }

    /// `preview_input` end to end on a phone and on a desktop: the actions reach the screen in
    /// screen pixels, the report says what ran, and `screenshot: true` adds the picture after.
    #[test]
    fn preview_input_drives_a_phone_and_a_desktop() {
        let phone = Bench::new(PHONE);
        let touch = FakeDriver::touch();
        phone.start("pixel", touch.clone());
        let ran = phone.call(
            "preview_input",
            json!({"actions": [
                {"action": "click", "x": 288, "y": 640},
                {"action": "type", "text": "hi"},
                {"action": "key", "key": "Back"}
            ], "screenshot": true}),
        );
        assert!(ran.success, "{}", ran.output);
        assert!(
            ran.output
                .starts_with("1. click (288, 640): ok\n2. type \"hi\": ok\n3. key Back: ok\n\n"),
            "{}",
            ran.output
        );
        assert!(ran
            .output
            .contains("x/y for preview_input are pixels of this image"));
        assert_eq!(ran.images.len(), 1);
        assert_eq!((ran.images[0].width, ran.images[0].height), (576, 1280));
        let inputs = touch.inputs();
        assert_eq!(inputs[0], "tap 540,1200 x1");
        assert_eq!(inputs[1], "text hi");
        assert_eq!(inputs[2], "key Named(Back) None");
        assert_hidden(&phone.viewer_secrets(), &ran.output);

        let desktop = Bench::new(
            r#"{"name":"desk","type":"remote-control","protocol":"vnc","machine":"mac"}"#,
        );
        let screen = FakeDriver::new(Platform::Desktop, 1920, 1080);
        let receipt = desktop.start("desk", screen.clone());
        assert!(receipt.contains("coordinate frame 1280x720"), "{receipt}");
        let clicked = desktop.call(
            "preview_input",
            json!({"actions": [{"action": "click", "x": 640, "y": 360}]}),
        );
        assert!(clicked.success, "{}", clicked.output);
        assert_eq!(clicked.output, "1. click (640, 360): ok");
        assert!(clicked.images.is_empty());
        assert!(
            screen
                .inputs()
                .contains(&"pointer 960,540 buttons 1".to_owned()),
            "{:?}",
            screen.inputs()
        );
        // A desktop has no element tree; the session says how to work instead.
        let snapshot = desktop.call("preview_snapshot", json!({}));
        assert!(!snapshot.success);
        assert_eq!(snapshot.output, crate::remote_control::snapshot::NO_TREE);
    }

    /// A batch whose turn was stopped runs nothing more, and says so.
    #[test]
    fn preview_input_stops_between_actions_once_the_call_is_cancelled() {
        let bench = Bench::new(PHONE);
        let touch = FakeDriver::touch();
        bench.start("pixel", touch.clone());
        let flag = Arc::new(AtomicBool::new(true));
        let cancelled = bench.call_with(
            "preview_input",
            json!({"actions": [
                {"action": "click", "x": 10, "y": 10},
                {"action": "click", "x": 20, "y": 20},
                {"action": "click", "x": 30, "y": 30}
            ], "screenshot": true}),
            &CancelSignal::from_flag(flag),
        );
        assert!(cancelled.success, "{}", cancelled.output);
        assert!(
            cancelled.output.starts_with(
                "1. click (10, 10): cancelled before it ran (actions 2-3 did not run)"
            ),
            "{}",
            cancelled.output
        );
        assert!(cancelled
            .output
            .ends_with("No screenshot was taken: the batch was cancelled."));
        assert!(cancelled.images.is_empty());
        assert!(touch.inputs().is_empty(), "{:?}", touch.inputs());
    }

    /// A read-only screen can be looked at and not touched.
    #[test]
    fn a_read_only_screen_refuses_every_input() {
        let bench = Bench::new(
            r#"{"name":"pixel","type":"remote-control","protocol":"adb","readOnly":true}"#,
        );
        let touch = FakeDriver::touch_with_tree(phone_tree());
        bench.start("pixel", touch.clone());
        assert!(bench.call("preview_screenshot", json!({})).success);
        let snapshot = bench.call("preview_snapshot", json!({}));
        let button = uid_of(&snapshot.output, "Button");
        for (tool, input) in [
            (
                "preview_input",
                json!({"actions": [{"action": "click", "x": 1, "y": 1}]}),
            ),
            ("preview_click", json!({"uid": button})),
            ("preview_fill", json!({"uid": button, "value": "x"})),
        ] {
            let refused = bench.call(tool, input);
            assert!(!refused.success, "{tool}");
            assert!(
                refused.output.contains("is read-only"),
                "{tool}: {}",
                refused.output
            );
        }
        assert!(touch.inputs().is_empty());
    }

    /// `preview_input` with no screen says how to get one, and malformed actions are refused
    /// with what to fix — the schema's text bound included.
    #[test]
    fn preview_input_off_a_screen_says_what_to_do() {
        let bench = Bench::new(PHONE);
        let none = bench.call(
            "preview_input",
            json!({"actions": [{"action": "click", "x": 1, "y": 1}]}),
        );
        assert_eq!(none.output, PREVIEW_INPUT_NO_SCREEN);
        let bad = bench.call(
            "preview_input",
            json!({"actions": [{"action": "tap", "x": 1, "y": 1}]}),
        );
        assert!(
            bad.output.contains("unknown action \"tap\""),
            "{}",
            bad.output
        );
        // The default `via` takes 2000 characters, held at parse (no screen needed), and says
        // where longer text goes.
        let long = bench.call(
            "preview_input",
            json!({"actions": [{"action": "type", "text": "x".repeat(2_001)}]}),
        );
        assert!(
            long.output.contains("longer than 2000 characters")
                && long.output.contains("via \"paste\""),
            "{}",
            long.output
        );
        // A paste of that text is not too long: it is only that there is no screen yet. Past the
        // schema's own bound it is refused at parse as well.
        let pasted = bench.call(
            "preview_input",
            json!({"actions": [{"action": "type", "text": "x".repeat(2_001), "via": "paste"}]}),
        );
        assert_eq!(pasted.output, PREVIEW_INPUT_NO_SCREEN);
        let too_long_to_paste = bench.call(
            "preview_input",
            json!({"actions": [{"action": "type", "text": "x".repeat(60_001), "via": "paste"}]}),
        );
        assert!(
            too_long_to_paste
                .output
                .contains("longer than 60000 characters"),
            "{}",
            too_long_to_paste.output
        );
        let schema = crate::builtin_schemas::builtin_tool_schema(
            "preview_input",
            crate::tool_surface::ToolVariant::Standard,
            &PromptProfile::builtin(),
        )
        .unwrap();
        assert_eq!(
            schema["properties"]["actions"]["items"]["properties"]["text"]["maxLength"],
            60_000
        );
    }

    /// A call runs on what it was judged for. Judged while nothing was connected and run once a
    /// screen is, it is refused rather than run on a screen it was never approved for; judged with
    /// the screen there, it is classified as a look at a screen and skips the page's takeover gate.
    #[test]
    fn a_call_runs_only_on_the_target_it_was_judged_for() {
        let bench = Bench::new(PHONE);
        let snapshot = bench.request("preview_snapshot", json!({}));
        assert_eq!(
            judge_preview_call(&bench.state, "call-1", &snapshot, &WorkspaceSet::default()).surface,
            PreviewSurface::Page
        );

        bench.start("pixel", FakeDriver::touch_with_tree(phone_tree()));
        let changed = target_at_dispatch(&snapshot, &bench.state, Some("call-1")).unwrap_err();
        assert!(
            changed.starts_with("What the preview shows changed while this call waited"),
            "{changed}"
        );
        assert!(
            changed.contains("would now act on the remote-control screen \"pixel\""),
            "{changed}"
        );
        // The judgement was used up; a call nobody judged runs on what it resolves to now.
        assert!(matches!(
            target_at_dispatch(&snapshot, &bench.state, Some("call-1")),
            Ok(PreviewTarget::Remote(_))
        ));

        assert_eq!(
            judge_preview_call(&bench.state, "call-2", &snapshot, &WorkspaceSet::default()).surface,
            PreviewSurface::RemoteScreen
        );
        assert_eq!(
            judged_surface(&bench.state, "call-2", &snapshot).surface,
            PreviewSurface::RemoteScreen
        );
        assert_eq!(
            judged_surface_for_approval(&bench.state, Some("call-2"), &snapshot).surface,
            PreviewSurface::RemoteScreen
        );
        assert_eq!(
            judged_surface_for_approval(&bench.state, None, &snapshot).surface,
            PreviewSurface::RemoteScreen
        );
        assert!(!judged_for_the_page(&bench.state, "call-2", &snapshot));
        assert!(matches!(
            target_at_dispatch(&snapshot, &bench.state, Some("call-2")),
            Ok(PreviewTarget::Remote(_))
        ));
        // Tools with no target are the page's business.
        let list = bench.request("preview_list", json!({}));
        assert_eq!(
            judge_preview_call(&bench.state, "call-3", &list, &WorkspaceSet::default()).surface,
            PreviewSurface::Page
        );
    }

    /// A viewer's address in page output — however it is spelled — becomes the screen's id.
    #[test]
    fn a_viewer_address_in_page_output_is_replaced_by_its_screen() {
        let bench = Bench::new(PHONE);
        bench.start("pixel", FakeDriver::touch());
        let url = bench.session("pixel").viewer_url();
        let token = viewer_token(&url).unwrap();
        let localhost = url.replace("127.0.0.1", "localhost");
        let text = format!(
            "{{\"page\": {{\"url\": \"{url}state?x=1\"}}, \"also\": \"{localhost}\", \"bare\": \"/{token}/\"}}"
        );
        let scrubbed = scrub_viewer_addresses(&bench.state, text);
        assert!(!scrubbed.contains(&token), "{scrubbed}");
        assert_eq!(
            scrubbed
                .matches("[remote-control screen \"pixel\"]")
                .count(),
            3,
            "{scrubbed}"
        );
        assert_eq!(
            scrub_viewer_addresses(&bench.state, "http://localhost:5173/".into()),
            "http://localhost:5173/"
        );

        // What a page script would turn the address into is scrubbed too: the token's case,
        // reversed, as hex or escapes, and inside base64 of a longer string at every alignment.
        use base64::Engine;
        let hex: String = token.bytes().map(|byte| format!("{byte:02X}")).collect();
        let escaped: String = token.bytes().map(|byte| format!("\\u{byte:04x}")).collect();
        let reversed: String = token.chars().rev().collect();
        let mut encodings = vec![
            token.to_uppercase(),
            hex,
            escaped,
            reversed,
            url.replace(&token, &token.to_uppercase()),
        ];
        for prefix in ["", "x", "xy"] {
            for engine in [
                base64::engine::general_purpose::STANDARD,
                base64::engine::general_purpose::URL_SAFE,
            ] {
                encodings.push(engine.encode(format!("{prefix}{url}state")));
            }
        }
        for encoded in encodings {
            let scrubbed = scrub_viewer_addresses(&bench.state, format!("value: {encoded} end"));
            assert!(
                scrubbed.contains("[remote-control screen \"pixel\"]"),
                "{encoded} -> {scrubbed}"
            );
            assert!(!scrubbed.contains(&encoded), "{encoded} -> {scrubbed}");
        }
    }

    fn sleeper(name: &str, seconds: u32) -> String {
        #[cfg(windows)]
        let (program, args) = (
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
                + "\\System32\\cmd.exe",
            vec![
                "/C".to_owned(),
                format!("ping -n {} 127.0.0.1", seconds + 1),
            ],
        );
        #[cfg(not(windows))]
        let (program, args) = (
            "/bin/sh".to_owned(),
            vec!["-c".to_owned(), format!("sleep {seconds}")],
        );
        format!(
            r#"{{"name":"{name}","runtimeExecutable":{},"runtimeArgs":{},"port":0,"autoPort":true}}"#,
            serde_json::to_string(&program).unwrap(),
            serde_json::to_string(&args).unwrap()
        )
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The entry a screen's `launch` started is what the screen runs on. Stopped, or exited on
    /// its own, it takes the screen's session with it at once: the row stays, failed, saying why,
    /// and the pane hears of it.
    #[test]
    fn a_screen_whose_launch_entry_ends_fails_at_once() {
        for exits_on_its_own in [false, true] {
            let seconds = if exits_on_its_own { 6 } else { 60 };
            let bench = Bench::new(&format!(
                r#"{},{{"name":"pixel","type":"remote-control","protocol":"adb","device":"emulator-5554","launch":"emu-proc"}}"#,
                sleeper("emu-proc", seconds)
            ));
            let changes = Arc::new(AtomicUsize::new(0));
            let counted = changes.clone();
            bench
                .state
                .preview_servers
                .set_change_listener(Arc::new(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                }));
            bench.start("pixel", FakeDriver::touch());
            let session = bench.session("pixel");
            assert!(session.failure().is_none());
            let before = changes.load(Ordering::SeqCst);
            if !exits_on_its_own {
                let stopped = bench.call("preview_stop", json!({"serverId": "emu-proc"}));
                assert_eq!(stopped.output, "Server emu-proc stopped");
            }
            wait_for("the screen to fail", || session.failure().is_some());
            assert_eq!(
                session.failure().as_deref(),
                Some("the launch entry \"emu-proc\" stopped")
            );
            wait_for("the pane to hear of it", || {
                changes.load(Ordering::SeqCst) > before
            });
            let listed: Value =
                serde_json::from_str(&bench.call("preview_list", json!({})).output).unwrap();
            let pixel = listed
                .as_array()
                .unwrap()
                .iter()
                .find(|server| server["serverId"] == "pixel")
                .cloned()
                .expect("the failed row is kept");
            assert_eq!(pixel["status"], "failed");
            assert_eq!(pixel["remote"]["state"], "failed");
            let shot = bench.call("preview_screenshot", json!({"serverId": "pixel"}));
            assert!(!shot.success);
            assert!(
                shot.output
                    .contains("failed: the launch entry \"emu-proc\" stopped."),
                "{}",
                shot.output
            );
            let logs = bench
                .call("preview_logs", json!({"serverId": "pixel"}))
                .output;
            assert!(
                logs.contains("the launch entry \"emu-proc\" stopped"),
                "{logs}"
            );
        }
    }

    /// A model's `preview_start` connects a remote-control entry only when it is the entry the
    /// call was judged for — the reading its mandatory card was built from — exactly as that card
    /// showed it. A call nobody judged, or one whose file changed after its card was built,
    /// connects nothing and starts nothing.
    #[test]
    fn a_model_start_connects_only_the_entry_its_card_was_built_from() {
        let bench = Bench::new(PHONE);
        let workspaces = WorkspaceSet::local_root(bench.path().to_string_lossy().into_owned());
        let start = bench.request("preview_start", json!({"name": "pixel "}));
        let connect = || fake::ready(FakeDriver::touch());

        let unjudged = bench
            .start_call("pixel", connect(), Some("never-judged"))
            .err()
            .expect("a call nobody judged connects no screen");
        assert!(unjudged.contains("was not confirmed for it"), "{unjudged}");
        assert!(bench.state.preview_servers.servers().is_empty());

        let judged = judge_preview_call(&bench.state, "call-1", &start, &workspaces);
        let reading = judged.start.clone().expect("a start is read when it is judged");
        assert_eq!(reading.confirmed_controls().len(), 1);
        // Every classification of the call — the gate's and the card's — reads the same judgement.
        assert_eq!(judged_surface(&bench.state, "call-1", &start), judged);
        assert_eq!(
            judged_surface_for_approval(&bench.state, Some("call-1"), &start),
            judged
        );
        let decision = crate::security::classify_model_call_in_workspaces_at(
            crate::model::SecurityLevel::FullAccess,
            &workspaces,
            bench.path(),
            bench.app_data.path(),
            &[],
            &start,
            &crate::security::remembered_shell_cwd(&bench.state, CONVERSATION),
            judged,
        )
        .unwrap();
        assert!(decision.mandatory_prompt);
        assert_eq!(decision.rule_id, "preview.remote_control_start");
        let started = bench
            .start_call("pixel", connect(), Some("call-1"))
            .unwrap();
        assert!(started.success, "{}", started.output);
        bench.state.preview_servers.stop_all();

        // The file names another device after the card was built: the card showed the first.
        judge_preview_call(&bench.state, "call-2", &start, &workspaces);
        bench.rewrite(
            r#"{"name":"pixel","type":"remote-control","protocol":"adb","device":"R5CT1234ABC"}"#,
        );
        let changed = bench
            .start_call("pixel", connect(), Some("call-2"))
            .err()
            .expect("a screen other than the card's is not connected");
        assert!(changed.contains("was not confirmed for it"), "{changed}");
        assert!(bench.state.preview_servers.servers().is_empty());
    }

    /// Two `preview_start` calls with the same arguments — a subagent's shares its parent's
    /// conversation id — judged while the registry differed: one read the screen, a later one the
    /// dev server of the same repeated name. The approval card of each is built from its own
    /// call's judgement, found by the call's id, so the screen's card stays mandatory however the
    /// two were ordered; only a card asked for with no id falls back to the latest judgement.
    #[test]
    fn each_approval_card_is_built_from_its_own_calls_judgement() {
        let bench = Bench::new(PHONE);
        let workspaces = WorkspaceSet::local_root(bench.path().to_string_lossy().into_owned());
        let start = bench.request("preview_start", json!({"name": "dev"}));
        let entry = |remote_control| crate::preview::PreviewConfiguredServer {
            name: "dev".to_owned(),
            command: None,
            args: Vec::new(),
            cwd: String::new(),
            port: 0,
            auto_port: None,
            url: None,
            remote_control,
            env: Default::default(),
        };
        let list = crate::preview::PreviewConfigurationList {
            launch_json_path: String::new(),
            servers: vec![
                crate::preview::PreviewConfiguredServer {
                    command: Some("npm".to_owned()),
                    ..entry(None)
                },
                entry(Some(crate::remote_control::target::ControlTarget {
                    protocol: crate::remote_control::target::Protocol::Adb,
                    place: crate::remote_control::target::TargetPlace::Workspace,
                    port: 5037,
                    device: Some("emulator-5554".to_owned()),
                    username: None,
                    launch: None,
                    read_only: false,
                })),
            ],
            malformed: Vec::new(),
            problem: None,
            problem_reason: None,
        };
        let reading = |pick: usize| {
            Some(Arc::new(crate::preview::StartReading::Read {
                list: list.clone(),
                picks: vec![pick],
                running_launches: Vec::new(),
            }))
        };
        let targets = &bench.state.preview_call_targets;
        // The screen's call first, then the dev server's: the later judgement is the latest.
        targets.record("subagent-call", &start, None, reading(1));
        std::thread::sleep(Duration::from_millis(5));
        targets.record("parent-call", &start, None, reading(0));
        let card = |call_id: Option<&str>| {
            crate::security::classify_model_call_in_workspaces_at(
                crate::model::SecurityLevel::FullAccess,
                &workspaces,
                bench.path(),
                bench.app_data.path(),
                &[],
                &start,
                &crate::security::remembered_shell_cwd(&bench.state, CONVERSATION),
                judged_surface_for_approval(&bench.state, call_id, &start),
            )
            .unwrap()
        };

        let screen = card(Some("subagent-call"));
        assert!(screen.mandatory_prompt);
        assert_eq!(screen.rule_id, "preview.remote_control_start");
        assert!(!card(Some("parent-call")).mandatory_prompt);
        assert!(!card(None).mandatory_prompt);

        // A judgement under the id for other arguments is not this request's: the call is judged
        // as an unjudged one, from the file, never from the stale reading.
        let rewritten = bench.request("preview_start", json!({"name": "web"}));
        assert_eq!(
            judged_surface_for_approval(&bench.state, Some("subagent-call"), &rewritten).start,
            None
        );
    }

    /// A call's judgement waits with it for as long as its approval card does, however many other
    /// calls are judged meanwhile, and is forgotten the moment the call settles. One abandoned
    /// past any card's wait is dropped, and its call refused if it ever runs.
    #[test]
    fn a_judgement_lives_exactly_as_long_as_its_call() {
        let bench = Bench::new(PHONE);
        let workspaces = WorkspaceSet::default();
        let snapshot = bench.request("preview_snapshot", json!({}));
        let targets = &bench.state.preview_call_targets;

        let waiting = settle_on_drop(&bench.state, CONVERSATION, "waiting");
        judge_preview_call(&bench.state, "waiting", &snapshot, &workspaces);
        for number in 0..300 {
            let other = ToolExecutionRequest {
                conversation_id: format!("other-{number}"),
                ..snapshot.clone()
            };
            judge_preview_call(&bench.state, &format!("call-{number}"), &other, &workspaces);
        }
        assert!(targets.for_call(CONVERSATION, "waiting").is_some());
        drop(waiting);
        assert!(targets.for_call(CONVERSATION, "waiting").is_none());
        assert_eq!(targets.len(), 300);

        targets.expire(std::time::Instant::now() + JUDGEMENT_LIFETIME + Duration::from_secs(1));
        assert_eq!(targets.len(), 0);
        let abandoned = ToolExecutionRequest {
            conversation_id: "other-0".to_owned(),
            ..snapshot.clone()
        };
        let refused = target_at_dispatch(&abandoned, &bench.state, Some("call-0")).unwrap_err();
        assert!(refused.contains("waited too long"), "{refused}");
        // Judged again, it runs on what it resolves to (here, nothing: no page and no server).
        judge_preview_call(&bench.state, "call-0", &abandoned, &workspaces);
        let again = target_at_dispatch(&abandoned, &bench.state, Some("call-0"));
        assert!(
            again
                .as_ref()
                .err()
                .is_none_or(|error| !error.contains("waited too long")),
            "{again:?}"
        );
    }

    /// A screen the start could not connect is a failed start, as a dev server that could not
    /// start is: the call fails with the reason, and the row stays for `preview_logs`.
    #[test]
    fn a_screen_that_could_not_connect_is_a_failed_start() {
        let bench = Bench::new(PHONE);
        let failed = bench
            .start_call(
                "pixel",
                fake::refused("nothing is listening on 127.0.0.1:5037"),
                None,
            )
            .unwrap();
        assert!(!failed.success, "{}", failed.output);
        assert!(failed.output.contains("nothing is listening"), "{}", failed.output);
        assert_eq!(bench.state.preview_servers.servers().len(), 1);
    }

    /// The screen of row `server_id`, as a call resolves it.
    fn screen_of_row(bench: &Bench, server_id: &str) -> RemoteScreen {
        let request = bench.request("preview_input", json!({"serverId": server_id}));
        match preview_target_for(&request, &bench.state).unwrap() {
            PreviewTarget::Remote(screen) => screen,
            PreviewTarget::Web(_) => panic!("{server_id} is a screen"),
        }
    }

    /// A batch that would run past its call's time answers, in time, with what it did — the
    /// actions that ran and where it stopped — rather than with a bare timeout that leaves the
    /// model to guess what to repeat. No action starts that could not end in time.
    #[test]
    fn a_batch_past_its_time_reports_what_ran() {
        let bench = Bench::new(PHONE);
        let touch = FakeDriver::touch();
        bench.start("pixel", touch.clone());
        let screen = screen_of_row(&bench, "pixel");
        let actions = parse_actions(&json!([
            {"action": "click", "x": 10, "y": 10},
            {"action": "wait", "ms": 1500},
            {"action": "wait", "ms": 1500},
            {"action": "click", "x": 20, "y": 20}
        ]))
        .unwrap();
        let started = Instant::now();
        let report = on_screen_within(
            "preview_input",
            &screen,
            &CancelSignal::default(),
            crate::remote_control::session::CALL_MARGIN + Duration::from_millis(2500),
            Duration::from_secs(20),
            move |session, call| session.run_input_within(&actions, call),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        let lines = report.lines;
        assert_eq!(lines[0], "1. click (10, 10): ok", "{lines:?}");
        assert!(lines[1].starts_with("2. wait 1500 ms: ok"), "{lines:?}");
        assert!(
            lines
                .last()
                .unwrap()
                .starts_with("stopped: out of time after action 2 (actions 3-4 did not run)"),
            "{lines:?}"
        );
        assert_eq!(touch.inputs().len(), 1, "{:?}", touch.inputs());
    }

    /// A click or a fill on a screen whose call was stopped before it reached the screen clicks
    /// and types nothing.
    #[test]
    fn a_stopped_click_or_fill_touches_nothing() {
        let bench = Bench::new(PHONE);
        let driver = FakeDriver::touch_with_tree(phone_tree());
        bench.start("pixel", driver.clone());
        let snapshot = bench.call("preview_snapshot", json!({}));
        assert!(snapshot.success, "{}", snapshot.output);
        let button = uid_of(&snapshot.output, "Button");
        let field = uid_of(&snapshot.output, "EditText");
        let stopped = || CancelSignal::from_flag(Arc::new(AtomicBool::new(true)));

        let clicked = bench.call_with("preview_click", json!({"uid": button}), &stopped());
        assert!(!clicked.success, "{}", clicked.output);
        let filled = bench.call_with(
            "preview_fill",
            json!({"uid": field, "value": "hello"}),
            &stopped(),
        );
        assert!(!filled.success, "{}", filled.output);
        assert!(driver.inputs().is_empty(), "{:?}", driver.inputs());
    }

    /// A stopped call is not held for the rest of a long envelope by work that does not look at
    /// the stop — a driver call already on the wire, like an install's wait for the package
    /// installer: the wait notices the stop, gives the work a moment to answer for itself, then
    /// says that some of the request may still complete on the device, and the work, once it
    /// looks, finds the call ended. Work that does look answers for itself, at once.
    #[test]
    fn a_stopped_call_answers_promptly_even_when_its_work_does_not() {
        let bench = Bench::new(PHONE);
        bench.start("pixel", FakeDriver::touch());
        let screen = screen_of_row(&bench, "pixel");

        let flag = Arc::new(AtomicBool::new(false));
        let cancel = CancelSignal::from_flag(flag.clone());
        let release = Arc::new(AtomicBool::new(false));
        let hold = release.clone();
        let (seen, saw) = mpsc::channel();
        let raiser = {
            let flag = flag.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(200));
                flag.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let answer = on_screen_within(
            "screen_package",
            &screen,
            &cancel,
            Duration::from_secs(298),
            Duration::from_secs(305),
            move |_session, call| {
                while !hold.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(10));
                }
                let _ = seen.send((call.cancelled)());
                Ok::<_, String>("Installed app.apk.".to_owned())
            },
        );
        let waited = started.elapsed();
        raiser.join().unwrap();
        assert_eq!(
            answer.unwrap_err(),
            "screen_package was stopped while the remote-control screen \"pixel\" was still working on it, so some of what this call asked for may still complete there. Take a preview_screenshot before repeating any of it."
        );
        assert!(
            waited >= CANCEL_GRACE && waited < CANCEL_GRACE + Duration::from_secs(2),
            "{waited:?}"
        );
        release.store(true, Ordering::SeqCst);
        assert_eq!(saw.recv_timeout(Duration::from_secs(5)), Ok(true));

        // Work that looks at the stop gives its own account, without waiting out the grace.
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = CancelSignal::from_flag(flag.clone());
        let raiser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let own = on_screen_within(
            "preview_wait",
            &screen,
            &cancel,
            Duration::from_secs(27),
            Duration::from_secs(30),
            |_session, call| {
                while !(call.cancelled)() {
                    thread::sleep(Duration::from_millis(10));
                }
                Err::<String, _>("Cancelled while watching the screen.".to_owned())
            },
        );
        raiser.join().unwrap();
        assert_eq!(own.unwrap_err(), "Cancelled while watching the screen.");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    fn screen_row(server_id: &str, platform: Platform, shown: bool) -> PreviewTargetRow {
        PreviewTargetRow {
            platform: Some(platform),
            ..row(server_id, true, shown)
        }
    }

    fn screen_id(resolved: Result<&PreviewTargetRow, String>) -> &str {
        &resolved.expect("a screen").server_id
    }

    /// The rule of a screen tool: its `serverId`'s row when that is a screen of a kind the tool
    /// reaches; else such a screen the preview shows; else the only one — and an answer that says
    /// what to do in every other case. Dev servers, the page and screens of other kinds never
    /// matter.
    #[test]
    fn a_screen_tool_resolves_to_a_screen_of_a_kind_it_reaches() {
        use Platform::{Android, Desktop, Ios};
        const PHONES: &[Platform] = &[Android, Ios];
        const ANY: &[Platform] = &[Android, Ios, Desktop];
        let rows = [
            row("web", false, false),
            screen_row("pixel", Android, false),
            screen_row("iphone", Ios, false),
            screen_row("mac", Desktop, false),
        ];
        // 1. A serverId: its row, a screen of a kind the tool reaches.
        assert_eq!(
            screen_id(resolve_screen_target(
                "android_shell",
                &[Android],
                Some("PIXEL"),
                &rows
            )),
            "pixel"
        );
        assert_eq!(
            screen_id(resolve_screen_target("screen_app", PHONES, Some("iphone"), &rows)),
            "iphone"
        );
        assert_eq!(
            screen_id(resolve_screen_target("preview_zoom", ANY, Some("mac"), &rows)),
            "mac"
        );
        assert_eq!(
            resolve_screen_target("android_shell", &[Android], Some("mac"), &rows),
            Err("\"mac\" is a desktop, and android_shell works on Android devices only.".to_owned())
        );
        assert_eq!(
            resolve_screen_target("screen_app", PHONES, Some("mac"), &rows),
            Err("\"mac\" is a desktop, and screen_app works on phones or tablets only. A desktop's apps are out of reach over VNC; use preview_screenshot and preview_input.".to_owned())
        );
        assert_eq!(
            resolve_screen_target("screen_clipboard", &[Ios, Desktop], Some("pixel"), &rows),
            Err("\"pixel\" is an Android device, and screen_clipboard works on iPhones, iPads or desktops only. An Android device's clipboard needs a helper on the device that this build does not have.".to_owned())
        );
        assert_eq!(
            resolve_screen_target("screen_package", &[Android], Some("iphone"), &rows),
            Err("\"iphone\" is an iPhone or iPad, and screen_package works on Android devices only. Installing apps on an iPhone or iPad is not available in this build.".to_owned())
        );
        assert_eq!(
            resolve_screen_target("android_logcat", &[Android], Some("web"), &rows),
            Err("\"web\" is a dev server, not a remote-control screen.".to_owned())
        );
        assert_eq!(
            resolve_screen_target("android_logcat", &[Android], Some("gone"), &rows),
            Err(crate::tool_executor::preview_stale_server_id("gone"))
        );

        // 2. No serverId: the screen of those kinds the preview shows, beside others of them.
        let phones = [
            screen_row("pixel", Android, false),
            screen_row("iphone", Ios, true),
            screen_row("mac", Desktop, false),
        ];
        assert_eq!(
            screen_id(resolve_screen_target("screen_device", PHONES, None, &phones)),
            "iphone"
        );
        // A screen of another kind on the preview is not this tool's: the only phone is.
        let shows_mac = [
            row("web", false, false),
            screen_row("pixel", Android, false),
            screen_row("mac", Desktop, true),
        ];
        assert_eq!(
            screen_id(resolve_screen_target("screen_app", PHONES, None, &shows_mac)),
            "pixel"
        );
        assert_eq!(
            screen_id(resolve_screen_target("preview_wait", ANY, None, &shows_mac)),
            "mac"
        );

        // 3. The only screen of those kinds, whatever dev servers run (and its launch entry), and
        //    whatever screens of other kinds are connected.
        let lone = [
            row("web", false, false),
            PreviewTargetRow {
                launches_a_screen: true,
                ..row("vm-proc", false, false)
            },
            screen_row("iphone", Ios, false),
            screen_row("mac", Desktop, false),
        ];
        assert_eq!(
            screen_id(resolve_screen_target("screen_app", PHONES, None, &lone)),
            "iphone"
        );
        assert_eq!(
            screen_id(resolve_screen_target("screen_package", &[Android], None, &phones)),
            "pixel"
        );

        // 4. None of those kinds.
        assert_eq!(
            resolve_screen_target("screen_app", PHONES, None, &[row("web", false, false), screen_row("mac", Desktop, true)]),
            Err("No phone or tablet is connected as a remote-control screen. Connect one with preview_start (a .mewrk/launch.json entry with \"type\": \"remote-control\" and \"protocol\": \"adb\" or \"wda\").".to_owned())
        );
        assert_eq!(
            resolve_screen_target("android_shell", &[Android], None, &[]),
            Err("No Android device is connected as a remote-control screen. Connect one with preview_start (a .mewrk/launch.json entry with \"type\": \"remote-control\" and \"protocol\": \"adb\").".to_owned())
        );
        assert_eq!(
            resolve_screen_target("preview_zoom", ANY, None, &[row("web", false, false)]),
            Err("No remote-control screen is connected. Connect one with preview_start (a .mewrk/launch.json entry with \"type\": \"remote-control\" and \"protocol\": \"vnc\", \"adb\" or \"wda\").".to_owned())
        );

        // 5. Several, none of them shown.
        let several = [
            screen_row("pixel", Android, false),
            screen_row("mac", Desktop, true),
            screen_row("iphone", Ios, false),
        ];
        assert_eq!(
            resolve_screen_target("screen_device", PHONES, None, &several),
            Err("Several phones or tablets are connected (\"pixel\", \"iphone\") and the preview shows none of them. Pass serverId to say which one.".to_owned())
        );
        assert!(resolve_screen_target(
            "android_logcat",
            &[Android],
            None,
            &[screen_row("a", Android, false), screen_row("b", Android, false)]
        )
        .unwrap_err()
        .starts_with("Several Android devices are connected"));
    }

    /// `preview_dialog` reaches phones and tablets as well as the page, so a call without
    /// `serverId` that the page rule lands on the page is refused while one is connected — when the
    /// page can be named back by a dev server's id. Otherwise the call is the page's, as before.
    #[test]
    fn a_dialog_without_a_server_id_is_not_guessed_onto_the_page() {
        use Platform::{Android, Desktop, Ios};
        let page_and_phone = [row("web", false, false), screen_row("pixel", Android, false)];
        assert_eq!(
            refuse_a_guess_at_the_page("preview_dialog", None, Resolved::Page, &page_and_phone),
            Err("preview_dialog could mean the web page or a phone or tablet that is connected (\"pixel\"). Pass serverId to say which: \"pixel\" for the screen, or \"web\" for the page.".to_owned())
        );
        let page_and_two = [
            row("web", false, false),
            screen_row("pixel", Android, false),
            screen_row("iphone", Ios, false),
        ];
        assert!(refuse_a_guess_at_the_page("preview_dialog", None, Resolved::Page, &page_and_two)
            .unwrap_err()
            .contains("of those connected (\"pixel\", \"iphone\")"));
        // A serverId, a screen already resolved, another tool: nothing to guess.
        assert_eq!(
            refuse_a_guess_at_the_page("preview_dialog", Some("web"), Resolved::Page, &page_and_phone),
            Ok(())
        );
        assert_eq!(
            refuse_a_guess_at_the_page(
                "preview_dialog",
                None,
                Resolved::Screen(&page_and_phone[1]),
                &page_and_phone
            ),
            Ok(())
        );
        assert_eq!(
            refuse_a_guess_at_the_page("preview_click", None, Resolved::Page, &page_and_phone),
            Ok(())
        );
        // A desktop answers no dialogs, a screen's launch entry is no page, and a page no dev
        // server stands for cannot be named back: the page takes the call.
        assert_eq!(
            refuse_a_guess_at_the_page(
                "preview_dialog",
                None,
                Resolved::Page,
                &[row("web", false, false), screen_row("mac", Desktop, false)]
            ),
            Ok(())
        );
        assert_eq!(
            refuse_a_guess_at_the_page(
                "preview_dialog",
                None,
                Resolved::Page,
                &[
                    PreviewTargetRow {
                        launches_a_screen: true,
                        ..row("emulator", false, false)
                    },
                    screen_row("pixel", Android, false)
                ]
            ),
            Ok(())
        );
    }

    const IPHONE: &str =
        r#"{"name":"iphone","type":"remote-control","protocol":"wda","machine":"mac"}"#;
    const DESK: &str =
        r#"{"name":"desk","type":"remote-control","protocol":"vnc","machine":"mac"}"#;

    fn android_phone() -> Arc<FakeDriver> {
        FakeDriver::with_android(FakeAndroid::new(34, |_| FakeAndroid::out("")))
    }

    /// An Android phone showing a permission dialog, `Allow` and `Deny`, whose shell finds
    /// `com.foo` installed when `installed` and answers nothing else.
    fn dialog_phone(installed: bool) -> Arc<FakeDriver> {
        let button = |label: &str, id: &str, (x, y, width, height)| UiNode {
            role: "Button".into(),
            name: label.into(),
            id: Some(id.into()),
            clickable: true,
            enabled: true,
            bounds: Some(Rect {
                x,
                y,
                width,
                height,
            }),
            ..UiNode::default()
        };
        let tree = UiNode {
            role: "FrameLayout".into(),
            enabled: true,
            bounds: Some(Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            }),
            children: vec![
                button("Allow", "android:id/button1", (100, 1500, 400, 120)),
                button("Deny", "android:id/button2", (580, 1500, 400, 120)),
            ],
            ..UiNode::default()
        };
        FakeDriver::with_android_tree(
            FakeAndroid::new(34, move |command| {
                if installed && command == "pm path 'com.foo'" {
                    FakeAndroid::out("package:/data/app/com.foo/base.apk\n")
                } else {
                    FakeAndroid::out("")
                }
            }),
            tree,
        )
    }

    /// Each platform's tools, end to end through the executor: the call is parsed, its screen
    /// resolved by its platform, and it runs through the session on that screen's driver — the
    /// unified `screen_app`, `screen_device`, `screen_clipboard` and `screen_package`, the
    /// `android_*` pair, `preview_dialog` on a screen, a paste through `preview_input`, and the
    /// session's own two, the close-up and the wait. A tool on a kind of screen it does not
    /// reach is told what is there instead, and reaches no driver. No answer carries the
    /// viewer's address.
    #[test]
    fn each_platforms_tools_reach_its_own_screen_through_the_tool_layer() {
        let phone = Bench::new(PHONE);
        let android = dialog_phone(false);
        phone.start("pixel", android.clone());
        let mut seen = Vec::new();
        for (tool, input, success, output) in [
            (
                "android_shell",
                json!({"command": "getprop"}),
                true,
                "exit 0\n(no output)",
            ),
            (
                "screen_app",
                json!({"serverId": "pixel"}),
                true,
                "Launchable apps on Fake Pixel: 0 (showing 0).",
            ),
            (
                "screen_app",
                json!({"action": "info", "app": "com.foo"}),
                false,
                "com.foo is not installed.",
            ),
            (
                "screen_package",
                json!({"action": "uninstall", "app": "com.foo"}),
                false,
                "com.foo is not installed.",
            ),
            (
                "screen_device",
                json!({"action": "lock"}),
                true,
                "Put the device to sleep. Neither whether the screen is off nor whether the lock screen is showing could be read.",
            ),
            ("android_logcat", json!({}), true, "No log lines matched."),
        ] {
            let ran = phone.call(tool, input.clone());
            assert_eq!(ran.success, success, "{tool} {input}: {}", ran.output);
            assert_eq!(ran.output, output, "{tool} {input}");
            seen.push(ran.output);
        }
        // The device's report is sectioned, and ends with the dialog showing and its buttons.
        let info = phone.call("screen_device", json!({}));
        assert!(info.success, "{}", info.output);
        assert!(
            info.output.starts_with("device:\n  maker: unknown\n"),
            "{}",
            info.output
        );
        assert!(
            info.output.contains("\ndisplay:\n") && info.output.contains("\npower:\n"),
            "{}",
            info.output
        );
        assert!(
            info.output
                .ends_with("\ndialog: showing; buttons: \"Allow\", \"Deny\""),
            "{}",
            info.output
        );
        // A button the dialog does not have presses nothing; one it has is tapped in its middle.
        let wrong = phone.call("preview_dialog", json!({"button": "Maybe"}));
        assert!(!wrong.success);
        assert_eq!(
            wrong.output,
            "No button of the dialog is labelled \"Maybe\". The buttons showing: \"Allow\", \"Deny\"."
        );
        assert!(android.inputs().is_empty(), "{:?}", android.inputs());
        let pressed = phone.call(
            "preview_dialog",
            json!({"serverId": "pixel", "button": "Allow"}),
        );
        assert!(pressed.success, "{}", pressed.output);
        assert_eq!(
            pressed.output,
            "Pressed \"Allow\".\nA dialog with the same buttons is showing; buttons: \"Allow\", \"Deny\"."
        );
        assert_eq!(android.inputs(), ["tap 300,1560 x1"]);
        // The close-up and the wait work on a phone too, and look at it afresh: a phone's driver
        // keeps its last capture, which nothing refreshes while no one watches.
        let asked_before = android.frame_requests().len();
        let zoom = phone.call(
            "preview_zoom",
            json!({"x": 0, "y": 0, "width": 100, "height": 100}),
        );
        assert!(zoom.success, "{}", zoom.output);
        assert_eq!(zoom.images.len(), 1);
        let asked = android.frame_requests()[asked_before..].to_vec();
        assert!(
            !asked.is_empty() && asked.iter().all(|fresh| *fresh),
            "preview_zoom: {asked:?}"
        );
        let asked_before = android.frame_requests().len();
        let wait = phone.call("preview_wait", json!({"timeout": 100}));
        assert!(wait.success, "{}", wait.output);
        assert!(wait.images.is_empty());
        let asked = android.frame_requests()[asked_before..].to_vec();
        assert!(
            !asked.is_empty() && asked.iter().all(|fresh| *fresh),
            "preview_wait: {asked:?}"
        );
        seen.extend([info.output, pressed.output, zoom.output, wait.output]);

        // What does not parse, or is not for this screen, reaches no driver.
        let missing = phone.call("screen_package", json!({}));
        assert!(!missing.success);
        assert_eq!(
            missing.output,
            "\"action\" is required: install, uninstall, clear_data, grant or revoke."
        );
        let no_clipboard = phone.call("screen_clipboard", json!({"action": "read"}));
        assert!(!no_clipboard.success);
        assert!(
            no_clipboard
                .output
                .starts_with("No iPhone, iPad or desktop is connected as a remote-control screen."),
            "{}",
            no_clipboard.output
        );
        let wrong_kind = phone.call(
            "screen_clipboard",
            json!({"serverId": "pixel", "action": "read"}),
        );
        assert!(!wrong_kind.success);
        assert_eq!(
            wrong_kind.output,
            "\"pixel\" is an Android device, and screen_clipboard works on iPhones, iPads or desktops only. An Android device's clipboard needs a helper on the device that this build does not have."
        );
        seen.extend([missing.output, no_clipboard.output, wrong_kind.output]);
        // Only the calls that ran reached the shell: the refused and the misdirected ones sent
        // nothing, and an app that is not installed is neither stopped nor uninstalled.
        let commands = android.android_fake().commands();
        assert_eq!(
            commands
                .iter()
                .filter(|command| *command == "pm path 'com.foo'")
                .count(),
            2,
            "{commands:?}"
        );
        for ran in ["getprop", "input keyevent 223"] {
            assert!(
                commands.iter().any(|command| command == ran),
                "{commands:?}"
            );
        }
        assert!(
            !commands
                .iter()
                .any(|command| command.contains("uninstall") || command.contains("force-stop")),
            "{commands:?}"
        );
        let secrets = phone.viewer_secrets();
        for text in &seen {
            assert_hidden(&secrets, text);
        }

        let tablet = Bench::new(IPHONE);
        let ios = FakeDriver::with_ios(FakeIos::new(|_, _, _| Ok(Value::Null)));
        tablet.start("iphone", ios.clone());
        let mut seen = Vec::new();
        for (tool, input, output) in [
            (
                "screen_device",
                json!({}),
                "device:\n  name: unknown\n  model: unknown\n  system: unknown\n  kind: unknown\n  locale: unknown\n  time zone: unknown\ndisplay:\n  appearance: unknown\n  orientation: unknown\n  app in front: unknown\npower:\n  lock state: unknown\n  battery: unknown\nalert: none",
            ),
            (
                "screen_device",
                json!({"action": "info", "sections": ["power"]}),
                "power:\n  lock state: unknown\n  battery: unknown\nalert: none",
            ),
            (
                "screen_device",
                json!({"action": "lock"}),
                "Locked the device.",
            ),
            (
                "screen_app",
                json!({}),
                "Running apps: 0 (showing 0). Only apps running now are listed; an iPhone or iPad does not tell WebDriverAgent what is installed.",
            ),
            (
                "screen_app",
                json!({"action": "foreground"}),
                "app in front: unknown\nlock state: unknown",
            ),
            (
                "screen_app",
                json!({"action": "info", "app": "a.b"}),
                "app a.b: unknown",
            ),
            (
                "screen_app",
                json!({"action": "launch", "app": "a.b"}),
                "Launched a.b.",
            ),
            (
                "screen_clipboard",
                json!({"action": "write", "text": "你好"}),
                "Put 2 characters on the clipboard.",
            ),
            (
                "screen_clipboard",
                json!({"action": "read"}),
                "The clipboard is empty (on a real device this can also mean WebDriverAgent could not read it).",
            ),
        ] {
            let ran = tablet.call(tool, input.clone());
            assert!(ran.success, "{tool} {input}: {}", ran.output);
            assert_eq!(ran.output, output, "{tool} {input}");
            seen.push(ran.output);
        }
        // No alert is showing: said as it is, not guessed at.
        let alert = tablet.call(
            "preview_dialog",
            json!({"serverId": "iphone", "accept": true}),
        );
        assert!(!alert.success);
        assert_eq!(
            alert.output,
            "No alert is showing that WebDriverAgent can see. If one is on the screen, tap its button by its position instead."
        );
        seen.push(alert.output);
        // A phone has no default answer for a dialog, and an iPhone does not wake on its own:
        // both are said before the device is asked.
        let asked = ios.ios_fake().calls().len();
        assert!(asked > 0);
        let no_answer = tablet.call("preview_dialog", json!({"serverId": "iphone"}));
        assert!(!no_answer.success);
        assert!(
            no_answer
                .output
                .starts_with("On a phone or tablet preview_dialog has no default answer"),
            "{}",
            no_answer.output
        );
        let wake = tablet.call("screen_device", json!({"action": "wake"}));
        assert!(!wake.success);
        assert_eq!(
            wake.output,
            "An iPhone or iPad has no wake on its own; unlock wakes it and swipes the lock screen away (only without a passcode)."
        );
        // The tools of another kind of screen are told what is there.
        for (tool, input, said) in [
            (
                "screen_package",
                json!({"serverId": "iphone", "action": "uninstall", "app": "a.b"}),
                "\"iphone\" is an iPhone or iPad, and screen_package works on Android devices only. Installing apps on an iPhone or iPad is not available in this build.",
            ),
            (
                "android_shell",
                json!({"serverId": "iphone", "command": "ls"}),
                "\"iphone\" is an iPhone or iPad, and android_shell works on Android devices only.",
            ),
            (
                "android_logcat",
                json!({"serverId": "iphone"}),
                "\"iphone\" is an iPhone or iPad, and android_logcat works on Android devices only.",
            ),
        ] {
            let wrong = tablet.call(tool, input);
            assert!(!wrong.success, "{tool}");
            assert_eq!(wrong.output, said, "{tool}");
            seen.push(wrong.output);
        }
        assert_eq!(
            ios.ios_fake().calls().len(),
            asked,
            "what was refused reached no iPhone"
        );
        seen.extend([no_answer.output, wake.output]);
        let secrets = tablet.viewer_secrets();
        for text in &seen {
            assert_hidden(&secrets, text);
        }

        let desk = Bench::new(DESK);
        let screen = FakeDriver::with_desktop(FakeDesktop::new(ClipboardChannel::Extended));
        let receipt = desk.start("desk", screen.clone());
        assert!(receipt.contains("coordinate frame 1280x720"), "{receipt}");
        let clipboard = desk.call("screen_clipboard", json!({"action": "read"}));
        assert_eq!(clipboard.output, "The clipboard is empty.");
        // A paste is a type via "paste" now: the text on the clipboard and the shortcut, in one
        // batch under the input lock.
        let paste = desk.call(
            "preview_input",
            json!({"actions": [{"action": "type", "text": "你好", "via": "paste"}]}),
        );
        assert!(paste.success, "{}", paste.output);
        assert_eq!(
            paste.output,
            "1. type \"你好\" via paste: pasted 2 characters with ctrl+v (the desktop's clipboard now holds them)"
        );
        assert_eq!(
            screen.inputs(),
            [
                "key Named(Control) Some(true)",
                "key Char('v') None",
                "key Named(Control) Some(false)",
            ]
        );
        assert_eq!(screen.desktop_fake().writes(), ["你好"]);
        let written = desk.call(
            "screen_clipboard",
            json!({"action": "write", "text": "copied"}),
        );
        assert!(written.success, "{}", written.output);
        assert_eq!(
            written.output,
            "Put 6 characters on the desktop's clipboard (UTF-8, Extended Clipboard)."
        );
        assert_eq!(screen.desktop_fake().writes(), ["你好", "copied"]);

        let zoom = desk.call(
            "preview_zoom",
            json!({"x": 640, "y": 360, "width": 320, "height": 180}),
        );
        assert!(zoom.success, "{}", zoom.output);
        assert_eq!(zoom.images.len(), 1);
        assert_eq!((zoom.images[0].width, zoom.images[0].height), (960, 540));
        let close_up: Value = serde_json::from_str(&zoom.output).unwrap();
        assert_eq!(
            close_up["region"],
            json!({"x": 640, "y": 360, "width": 320, "height": 180})
        );
        assert_eq!(close_up["image"], json!({"width": 960, "height": 540}));
        assert_eq!(close_up["zoom"], 3.0);
        assert_eq!(
            close_up["note"],
            "close-up only: x/y for preview_input stay in the full screenshot's frame (frame x = region.x + image x × region.width / image.width)"
        );
        let wait = desk.call("preview_wait", json!({"timeout": 200}));
        assert!(wait.success, "{}", wait.output);
        assert_eq!(wait.output, "The screen did not change within 0.2 s.");
        assert!(wait.images.is_empty());
        let bad_wait = desk.call("preview_wait", json!({"x": 1}));
        assert_eq!(
            bad_wait.output,
            "Give x, y, width and height together, or none of them."
        );
        // A key neither tool takes is refused by name, not ignored.
        let misspelt = desk.call(
            "preview_wait",
            json!({"until": "still", "timeout_ms": 15000}),
        );
        assert!(!misspelt.success);
        assert_eq!(
            misspelt.output,
            "preview_wait has no parameter \"timeout_ms\"; it takes serverId, until, timeout, x, y, width and height."
        );
        let extra = desk.call(
            "preview_zoom",
            json!({"x": 0, "y": 0, "width": 10, "height": 10, "scale": 2}),
        );
        assert!(!extra.success);
        assert_eq!(
            extra.output,
            "preview_zoom has no parameter \"scale\"; it takes serverId, x, y, width and height."
        );
        // Too short a wait for stillness, on a screen that never moved, says so.
        let short_still = desk.call("preview_wait", json!({"until": "still", "timeout": 200}));
        assert!(short_still.success, "{}", short_still.output);
        assert_eq!(
            short_still.output,
            "The screen did not change in 0.2 s; that is too short to call it still, which takes half a second without a change. Give a timeout of 500 or more."
        );
        // The tools of another kind of screen are told what is there, or that there is none.
        for (tool, input, said) in [
            (
                "screen_app",
                json!({"serverId": "desk"}),
                "\"desk\" is a desktop, and screen_app works on phones or tablets only. A desktop's apps are out of reach over VNC; use preview_screenshot and preview_input.",
            ),
            (
                "screen_device",
                json!({"serverId": "desk", "action": "lock"}),
                "\"desk\" is a desktop, and screen_device works on phones or tablets only. A desktop's settings are out of reach over VNC; use preview_screenshot and preview_input.",
            ),
            (
                "screen_package",
                json!({"serverId": "desk", "action": "uninstall", "app": "a.b"}),
                "\"desk\" is a desktop, and screen_package works on Android devices only. Install software on a desktop from that machine's own shell.",
            ),
        ] {
            let wrong = desk.call(tool, input);
            assert!(!wrong.success, "{tool}");
            assert_eq!(wrong.output, said, "{tool}");
        }
        let dialog = desk.call(
            "preview_dialog",
            json!({"serverId": "desk", "accept": true}),
        );
        assert!(!dialog.success);
        assert!(dialog.output.contains("desktop"), "{}", dialog.output);
        let android_on_desk = desk.call("android_logcat", json!({}));
        assert!(
            android_on_desk
                .output
                .starts_with("No Android device is connected as a remote-control screen."),
            "{}",
            android_on_desk.output
        );
        // Only the paste pressed keys, and only the paste and the write reached the clipboard.
        assert_eq!(screen.inputs().len(), 3, "{:?}", screen.inputs());
        assert_eq!(screen.desktop_fake().writes(), ["你好", "copied"]);
        let secrets = desk.viewer_secrets();
        for text in [
            &zoom.output,
            &wait.output,
            &clipboard.output,
            &paste.output,
            &written.output,
        ] {
            assert_hidden(&secrets, text);
        }
    }

    /// On a read-only screen the tools that only look still run — on every kind of screen; what
    /// acts, or changes the device in the background, is refused before it reaches the platform.
    #[test]
    fn a_read_only_screen_lets_the_screen_tools_look_and_nothing_more() {
        let phone = Bench::new(
            r#"{"name":"pixel","type":"remote-control","protocol":"adb","readOnly":true}"#,
        );
        let android = dialog_phone(true);
        phone.start("pixel", android.clone());
        let snapshot = phone.call("preview_snapshot", json!({}));
        let button = uid_of(&snapshot.output, "Button");
        let look = phone.call("android_logcat", json!({}));
        assert!(look.success, "{}", look.output);
        assert_eq!(look.output, "No log lines matched.");
        for (tool, input) in [
            ("screen_app", json!({})),
            ("screen_app", json!({"action": "info", "app": "com.foo"})),
            ("screen_device", json!({})),
            (
                "preview_zoom",
                json!({"x": 0, "y": 0, "width": 100, "height": 100}),
            ),
            ("preview_wait", json!({"timeout": 100})),
        ] {
            let ran = phone.call(tool, input.clone());
            assert!(ran.success, "{tool} {input}: {}", ran.output);
        }
        for (tool, input) in [
            ("screen_app", json!({"action": "launch", "app": "com.foo"})),
            ("screen_device", json!({"action": "lock"})),
            (
                "screen_package",
                json!({"action": "uninstall", "app": "com.foo"}),
            ),
            ("preview_dialog", json!({"button": "Allow"})),
            ("preview_fill", json!({"uid": button, "value": "x"})),
            ("android_shell", json!({"command": "input tap 1 1"})),
        ] {
            let refused = phone.call(tool, input.clone());
            assert!(!refused.success, "{tool} {input}");
            assert!(
                refused.output.contains("is read-only"),
                "{tool}: {}",
                refused.output
            );
        }
        // Nothing that changes the device was sent: no key, no tap, no stop, no uninstall.
        assert!(android.inputs().is_empty(), "{:?}", android.inputs());
        let commands = android.android_fake().commands();
        for changing in [
            "keyevent",
            "input tap",
            "force-stop",
            "uninstall",
            "am start",
            "monkey",
        ] {
            assert!(
                !commands.iter().any(|command| command.contains(changing)),
                "{changing}: {commands:?}"
            );
        }

        let tablet = Bench::new(
            r#"{"name":"iphone","type":"remote-control","protocol":"wda","machine":"mac","readOnly":true}"#,
        );
        let ios = FakeDriver::with_ios(FakeIos::new(|_, route, _| {
            // A clipboard that holds "copied" (base64); every other route answers nothing.
            match route {
                WdaRoute::Root("/wda/getPasteboard") => Ok(json!("Y29waWVk")),
                _ => Ok(Value::Null),
            }
        }));
        tablet.start("iphone", ios.clone());
        for (tool, input) in [
            ("screen_app", json!({})),
            ("screen_app", json!({"action": "info", "app": "a.b"})),
            ("screen_device", json!({})),
            ("preview_wait", json!({"timeout": 100})),
        ] {
            let ran = tablet.call(tool, input.clone());
            assert!(ran.success, "{tool} {input}: {}", ran.output);
        }
        let read = tablet.call("screen_clipboard", json!({"action": "read"}));
        assert_eq!(read.output, "Clipboard (6 characters):\ncopied");
        for (tool, input) in [
            ("screen_app", json!({"action": "launch", "app": "a.b"})),
            ("screen_device", json!({"action": "lock"})),
            ("screen_clipboard", json!({"action": "write", "text": "x"})),
            ("preview_dialog", json!({"button": "OK"})),
            ("preview_dialog", json!({"accept": true})),
        ] {
            let refused = tablet.call(tool, input.clone());
            assert!(!refused.success, "{tool} {input}");
            assert!(
                refused.output.contains("is read-only"),
                "{tool}: {}",
                refused.output
            );
        }
        // Only the looks reached the device: the clipboard read and an app's state are its POSTs.
        let posts: Vec<String> = ios
            .ios_fake()
            .lines()
            .into_iter()
            .filter(|line| line.starts_with("POST"))
            .collect();
        assert!(
            posts
                .iter()
                .all(|line| line.contains("getPasteboard") || line.contains("apps/state")),
            "{posts:?}"
        );

        let desk = Bench::new(
            r#"{"name":"desk","type":"remote-control","protocol":"vnc","machine":"mac","readOnly":true}"#,
        );
        let screen = FakeDriver::with_desktop(FakeDesktop::new(ClipboardChannel::Latin1));
        desk.start("desk", screen.clone());
        let zoom = desk.call(
            "preview_zoom",
            json!({"x": 0, "y": 0, "width": 100, "height": 100}),
        );
        assert!(zoom.success, "{}", zoom.output);
        assert!(desk.call("preview_wait", json!({"timeout": 100})).success);
        let write = desk.call("screen_clipboard", json!({"action": "write", "text": "x"}));
        assert!(write.output.contains("is read-only"), "{}", write.output);
        let paste = desk.call(
            "preview_input",
            json!({"actions": [{"action": "type", "text": "x", "via": "paste"}]}),
        );
        assert!(!paste.success);
        assert!(paste.output.contains("is read-only"), "{}", paste.output);
        let read = desk.call("screen_clipboard", json!({"action": "read"}));
        assert_eq!(
            read.output,
            "This server has not sent its clipboard since the connection was made; it sends it when its clipboard changes, so copy the text there again and read once more."
        );
        assert!(screen.desktop_fake().writes().is_empty());
        assert!(screen.inputs().is_empty());
    }

    /// A screen tool is held to the screen it was judged for, as `preview_input` is: judged with
    /// nothing to act on and run once a phone is there, or judged for the one phone and run once
    /// a second makes it ambiguous, it is refused.
    #[test]
    fn a_screen_tool_runs_only_on_the_screen_it_was_judged_for() {
        let bench = Bench::new(
            r#"{"name":"pixel","type":"remote-control","protocol":"adb","device":"emulator-5554"},{"name":"galaxy","type":"remote-control","protocol":"adb","device":"emulator-5556"}"#,
        );
        let shell = bench.request("android_shell", json!({"command": "getprop"}));
        assert_eq!(
            judge_preview_call(&bench.state, "call-1", &shell, &WorkspaceSet::default()).surface,
            PreviewSurface::Page
        );
        bench.start("pixel", android_phone());
        let changed = target_at_dispatch(&shell, &bench.state, Some("call-1")).unwrap_err();
        assert!(
            changed.starts_with("What the preview shows changed while this call waited: it was checked for nothing it could act on and would now act on the remote-control screen \"pixel\""),
            "{changed}"
        );

        assert_eq!(
            judge_preview_call(&bench.state, "call-2", &shell, &WorkspaceSet::default()).surface,
            PreviewSurface::RemoteScreen
        );
        assert_eq!(
            judged_surface(&bench.state, "call-2", &shell).surface,
            PreviewSurface::RemoteScreen
        );
        assert!(!judged_for_the_page(&bench.state, "call-2", &shell));
        assert!(matches!(
            target_at_dispatch(&shell, &bench.state, Some("call-2")),
            Ok(PreviewTarget::Remote(screen)) if screen.server_id == "pixel"
        ));

        assert_eq!(
            judge_preview_call(&bench.state, "call-3", &shell, &WorkspaceSet::default()).surface,
            PreviewSurface::RemoteScreen
        );
        bench.start("galaxy", android_phone());
        let several = target_at_dispatch(&shell, &bench.state, Some("call-3")).unwrap_err();
        assert!(
            several.starts_with("Several Android devices are connected (\"pixel\", \"galaxy\")"),
            "{several}"
        );
        // Named, it is judged and run on the one it names.
        let named = bench.request(
            "android_shell",
            json!({"serverId": "galaxy", "command": "getprop"}),
        );
        judge_preview_call(&bench.state, "call-4", &named, &WorkspaceSet::default());
        assert!(matches!(
            target_at_dispatch(&named, &bench.state, Some("call-4")),
            Ok(PreviewTarget::Remote(screen)) if screen.server_id == "galaxy"
        ));
    }

    /// A phone that says its own name: the fake itself calls every device "Fake Pixel".
    mod named_phone {
        use std::sync::Arc;

        use crate::remote_control::{
            driver::{AndroidControl, Driver, DriverInfo, DriverResult, Frame},
            fake::FakeDriver,
            keys::Key,
        };

        pub(super) struct NamedPhone {
            pub phone: Arc<FakeDriver>,
            pub name: String,
        }

        impl Driver for NamedPhone {
            fn info(&self) -> DriverInfo {
                DriverInfo {
                    label: self.name.clone(),
                    ..self.phone.info()
                }
            }

            fn frame(&self, fresh: bool) -> DriverResult<Frame> {
                self.phone.frame(fresh)
            }

            fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()> {
                self.phone.key(key, down)
            }

            fn text(&self, text: &str) -> DriverResult<()> {
                self.phone.text(text)
            }

            fn close(&self) {
                self.phone.close()
            }

            fn android(&self) -> Option<&dyn AndroidControl> {
                self.phone.android()
            }
        }
    }

    /// Every connect answers with an Android phone that calls itself `device`.
    fn phone_named(device: &str) -> crate::remote_control::session::ConnectFn {
        let phone = Arc::new(named_phone::NamedPhone {
            phone: android_phone(),
            name: device.to_owned(),
        });
        Arc::new(move |_target, _connector, _auth, _events| {
            Ok(phone.clone() as Arc<dyn crate::remote_control::driver::Driver>)
        })
    }

    /// The screen a call was judged for, as its approval card and its standing allowance need it:
    /// the row's handle, the server id and the kind of screen, and a card name that adds the
    /// device's own name to the server id when the device says something else (trimmed, and cut
    /// to 60 characters); the same entry started again is a new handle.
    #[test]
    fn a_judged_screen_is_named_by_its_server_id_and_the_devices_own_name() {
        for (device, card_name) in [
            ("Pixel 8", "pixel (Pixel 8)"),
            ("  Pixel 8  ", "pixel (Pixel 8)"),
            ("", "pixel"),
            ("   ", "pixel"),
            ("pixel", "pixel"),
        ] {
            let bench = Bench::new(PHONE);
            bench
                .start_call("pixel", phone_named(device), None)
                .unwrap();
            let request = bench.request("screen_app", json!({}));
            let judged = judged_screen_for_approval(&bench.state, None, &request)
                .unwrap_or_else(|| panic!("{device:?}: a judged screen"));
            assert_eq!(judged.card_name, card_name, "{device:?}");
            assert_eq!(judged.server_id, "pixel", "{device:?}");
            assert_eq!(judged.platform, Some(Platform::Android), "{device:?}");
            let row = bench.state.preview_servers.controls().remove(0);
            assert_eq!(judged.handle, row.server.handle, "{device:?}");
        }

        // A long name is cut at 60 characters (the session itself cuts it at 80).
        for (device, shown) in [
            ("x".repeat(100), "x".repeat(60)),
            ("设备".repeat(50), "设备".repeat(30)),
        ] {
            let bench = Bench::new(PHONE);
            bench
                .start_call("pixel", phone_named(&device), None)
                .unwrap();
            let request = bench.request("screen_app", json!({}));
            let judged = judged_screen_for_approval(&bench.state, None, &request).unwrap();
            assert_eq!(judged.card_name, format!("pixel ({shown})"));
        }

        // The same entry started again is a new row: the handle an allowance names is the old one.
        let bench = Bench::new(PHONE);
        let request = bench.request("screen_app", json!({}));
        bench
            .start_call("pixel", phone_named("Pixel 8"), None)
            .unwrap();
        let before = judged_screen_for_approval(&bench.state, None, &request).unwrap();
        bench.state.preview_servers.stop_all();
        assert_eq!(
            judged_screen_for_approval(&bench.state, None, &request),
            None
        );
        bench
            .start_call("pixel", phone_named("Pixel 8"), None)
            .unwrap();
        let after = judged_screen_for_approval(&bench.state, None, &request).unwrap();
        assert_eq!(after.card_name, before.card_name);
        assert_ne!(after.handle, before.handle);
    }

    /// A hook is told the judged screen's server id and platform — in the platform's own word,
    /// or `unknown` while its session is not there to say — and nothing that is the host's own,
    /// the registry handle or the card's name for the screen.
    #[test]
    fn a_hook_is_told_the_judged_screens_server_id_and_platform() {
        let screen = |platform| JudgedScreen {
            handle: "7".to_owned(),
            server_id: "pixel".to_owned(),
            platform,
            card_name: "pixel (Pixel 8)".to_owned(),
        };
        for (platform, word) in [
            (Some(Platform::Android), "android"),
            (Some(Platform::Ios), "ios"),
            (Some(Platform::Desktop), "desktop"),
            (None, "unknown"),
        ] {
            assert_eq!(
                judged_screen_for_hooks(&screen(platform)),
                json!({"server_id": "pixel", "platform": word}),
                "{word}"
            );
        }

        // The same, from a screen judged for a real call.
        let bench = Bench::new(PHONE);
        bench.start("pixel", android_phone());
        let judged = judged_screen_for_approval(
            &bench.state,
            None,
            &bench.request("screen_device", json!({})),
        )
        .unwrap();
        assert_eq!(
            judged_screen_for_hooks(&judged),
            json!({"server_id": "pixel", "platform": "android"})
        );
    }

    /// A call is judged for a screen only when it acts on one: the screen tools, `preview_input`,
    /// and the page tools that reach a screen, named or the only one of their kind. A start, a
    /// tool with no target and a screen tool with no screen to reach are judged for none. What a
    /// call was judged for is kept while it waits, when a second screen would make a guess
    /// ambiguous.
    /// A screen call that can reach no screen is refused before any hook or card is spent on it,
    /// with the answer its dispatch would give; one that reaches its screen is not.
    #[test]
    fn a_call_that_reaches_no_screen_is_refused_before_anyone_is_asked() {
        let bench = Bench::new(&format!(
            r#"{{"name":"pixel","type":"remote-control","protocol":"adb","device":"emulator-5554"}},{}"#,
            DESK
        ));
        bench.start("pixel", android_phone());
        let refusal = |call_id: &str, tool: &str, input: Value| {
            let request = bench.request(tool, input);
            judge_preview_call(&bench.state, call_id, &request, &WorkspaceSet::default());
            unreachable_screen_refusal(&bench.state, call_id, &request)
        };
        // Reached: nothing to refuse, the call is asked for as usual.
        assert_eq!(refusal("c1", "screen_app", json!({})), None);
        assert_eq!(refusal("c2", "android_shell", json!({"command": "id"})), None);
        assert_eq!(
            refusal("c3", "preview_dialog", json!({"serverId": "pixel", "button": "Allow"})),
            None
        );
        assert_eq!(refusal("c4", "bash", json!({"command": "ls"})), None);
        // A kind the tool does not reach, a stale id: refused with dispatch's own answer.
        assert_eq!(
            refusal("c5", "screen_clipboard", json!({"action": "read"})),
            Some("No iPhone, iPad or desktop is connected as a remote-control screen. Connect one with preview_start (a .mewrk/launch.json entry with \"type\": \"remote-control\" and \"protocol\": \"wda\" or \"vnc\").".to_owned())
        );
        assert_eq!(
            refusal("c6", "screen_app", json!({"serverId": "gone"})),
            Some(crate::tool_executor::preview_stale_server_id("gone"))
        );
        // A desktop answers no dialogs.
        bench.start(
            "desk",
            FakeDriver::with_desktop(FakeDesktop::new(ClipboardChannel::Extended)),
        );
        assert_eq!(
            refusal("c7", "preview_dialog", json!({"serverId": "desk", "accept": true})),
            Some(super::super::screen_tools::no_dialogs_on_a_desktop("desk"))
        );
        assert_eq!(
            refusal("c8", "screen_app", json!({"serverId": "desk"})).as_deref(),
            Some("\"desk\" is a desktop, and screen_app works on phones or tablets only. A desktop's apps are out of reach over VNC; use preview_screenshot and preview_input.")
        );
    }

    #[test]
    fn a_call_is_judged_for_a_screen_only_when_it_acts_on_one() {
        let bench = Bench::new(
            r#"{"name":"pixel","type":"remote-control","protocol":"adb","device":"emulator-5554"},{"name":"galaxy","type":"remote-control","protocol":"adb","device":"emulator-5556"}"#,
        );
        bench.start("pixel", android_phone());
        let judged = |tool: &str, input: Value| {
            judged_screen_for_approval(&bench.state, None, &bench.request(tool, input))
                .map(|screen| screen.server_id)
        };
        let pixel = Some("pixel".to_owned());
        for (tool, input) in [
            ("screen_app", json!({})),
            ("screen_device", json!({"serverId": "pixel"})),
            ("android_shell", json!({"command": "getprop"})),
            (
                "preview_zoom",
                json!({"x": 0, "y": 0, "width": 10, "height": 10}),
            ),
            ("preview_wait", json!({})),
            (
                "preview_input",
                json!({"serverId": "pixel", "actions": [{"action": "click", "x": 1, "y": 2}]}),
            ),
            (
                "preview_dialog",
                json!({"serverId": "pixel", "button": "Allow"}),
            ),
            ("preview_screenshot", json!({"serverId": "pixel"})),
        ] {
            assert_eq!(judged(tool, input), pixel, "{tool}");
        }
        for (tool, input) in [
            // A start describes the entry it would start.
            ("preview_start", json!({"name": "pixel"})),
            // No target to resolve.
            ("bash", json!({"command": "ls"})),
            ("write", json!({"path": "a.txt", "content": "x"})),
            // A screen the tool does not reach, or none at all.
            ("screen_clipboard", json!({"action": "read"})),
            ("screen_app", json!({"serverId": "gone"})),
        ] {
            assert_eq!(judged(tool, input), None, "{tool}");
        }

        // Judged for the one phone, a call keeps it when a second one is connected after…
        let shell = bench.request("android_shell", json!({"command": "getprop"}));
        judge_preview_call(&bench.state, "call-1", &shell, &WorkspaceSet::default());
        bench.start("galaxy", android_phone());
        assert_eq!(
            judged_screen_for_approval(&bench.state, Some("call-1"), &shell)
                .map(|screen| screen.server_id),
            pixel
        );
        // …where the same call asked afresh is ambiguous, and so is judged for no screen.
        bench
            .state
            .preview_call_targets
            .forget(CONVERSATION, "call-1");
        assert_eq!(
            judged_screen_for_approval(&bench.state, Some("call-1"), &shell),
            None
        );
    }
}
