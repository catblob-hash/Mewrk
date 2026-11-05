//! The pane: upstream's `BackendBrowserPanel` (`BrowserPanel.tsx`) and the cards and menus of
//! `PreviewPane.tsx`. The toolbar goes in the pane's header slot; the body is everything under it.
//!
//! Upstream's other two renderings have nothing to render here: `PreviewBrowserPanel` was the iframe for a
//! browser tab with no backend, and `previewFrame` mirrored a session into one for the dev build.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use fluent::{queue_image, release_image, ImageId};
use mewrk_client::prelude::*;
use mewrk_client::tf;
use serde_json::Value;

use crate::address::{normalize_browser_address, split_browser_address};
use crate::browser::*;
use crate::controller::BrowserController;
use crate::flows::{history_status, PageFlow, PageOps};
use crate::pages;
use crate::preview::*;
use crate::projection::{any_rect_overlaps, Command, Edges, Projection, ProjectionInput};
use crate::screen_view::ScreenView;
use crate::servers::*;
use mewrk_pane_files::sketch::{close_sketch, sketch_overlay, SketchOverlayProps};

/// How long the cleared-profile confirmation holds the menu open before it closes itself.
const CLEARED_FLASH: Duration = Duration::from_millis(1_500);
/// How long the copy confirmation replaces the copy affordance.
const COPY_FLASH: Duration = Duration::from_millis(1_500);
const STATUS_POLL: Duration = Duration::from_millis(700);
/// How often the pages behind this one are asked after. Their strip is all that reads the answer.
const BACKGROUND_POLL: Duration = Duration::from_millis(1_500);
/// The start page lists this many servers before "See all".
const START_PAGE_LIMIT: usize = 5;
const DRAWER_MIN_HEIGHT: f32 = 80.0;
const DRAWER_HEIGHT: f32 = 200.0;

/// `onOpenPage`: opens this tab's page the way selecting the tab does, at `url` when given.
pub type OpenPage = Rc<dyn Fn(&mut App, Option<String>, Box<dyn FnOnce(&mut App, Result<(), String>) + Send>)>;

/// What upstream's props carried.
#[derive(Clone, Default)]
pub struct BrowserPaneProps {
    /// The native page this tab drives. A conversation's first tab uses the conversation id.
    pub session_id: Option<String>,
    /// Other pages of this strip that change with nobody looking at them: the one the model's tools drive, and any not
    /// heard from yet. Each needs its status read for its tab to appear.
    pub background_sessions: Vec<String>,
    pub active: bool,
    /// The workspace whose `.mewrk/launch.json` the picker reads. Without one the pane is a plain browser.
    pub target: Option<PreviewTarget>,
    /// The checkout on this computer the "Open file" picker starts in (`GitTarget`, as the host reads it).
    pub file_target: Option<Value>,
    /// The page strip. Given one, it takes the header and the toolbar moves to `subheader`.
    pub tabs: Option<Rc<dyn Fn(&mut App) -> E>>,
    /// Set while the machine this page's workspace is on can't be reached.
    pub link_notice: Option<String>,
    /// A trusted overlay another component draws over this panel, in window coordinates.
    pub trusted_overlay_rect: Option<Rect>,
    /// What the app last heard about this page, to start from instead of nothing.
    pub initial_status: Option<BrowserStatus>,
    pub on_attach_image: Option<Rc<dyn Fn(&mut App, String, Vec<u8>)>>,
    pub on_element_picked: Option<Rc<dyn Fn(&mut App, SelectedElement)>>,
    pub on_open_page: Option<OpenPage>,
    /// The pane's own bottom rounding, which a native page has to do itself.
    pub bottom_corner_radius: f32,
}

/// Goes in `SidePane`'s slots.
pub struct BrowserPaneView {
    pub header: E,
    pub subheader: Option<E>,
    pub body: E,
}

#[derive(Clone, Copy, PartialEq)]
enum Clearing {
    Idle,
    Running,
    Cleared(Instant),
}

struct Image {
    id: ImageId,
    size: (u32, u32),
    data: Arc<str>,
}

struct PanelState {
    status: BrowserStatus,
    address: String,
    editing: bool,
    pending: bool,
    menu_open: bool,
    /// The menu just opened, and its first row takes the focus once the rows are known.
    menu_focus_first: bool,
    clear_data_open: bool,
    clearing: Clearing,
    annotating: bool,
    annotate_generation: u64,
    backdrop: Option<Arc<str>>,
    still_image: Option<Image>,
    logs_open: bool,
    log_lines: Vec<String>,
    drawer_height: f32,
    drawer_drag_start: f32,
    error: Option<String>,
    copied_until: Option<Instant>,
    start_page_expanded: bool,
    servers: ServersCore,
    projection: Projection,
    alive: Arc<AtomicBool>,
    seen_frame: u64,
    observed: Option<(String, bool, bool, bool)>,
    sent_preferences: Option<(bool, bool)>,
    dialog_text: Option<(u64, String)>,
    props: BrowserPaneProps,
    /// A remote screen, drawn here rather than by a page. While it is here any page this tab has stays parked.
    screen: Option<ScreenView>,
    /// Counts the pages this pane has been through. An answer asked for under an earlier one is about a page that is gone.
    page_epoch: u64,
}

#[derive(Default)]
struct Panels(HashMap<String, PanelState>);

/// Whether the window can be seen. A screen nobody can see is not worth a phone's encoder.
struct WindowVisible(bool);

impl Default for WindowVisible {
    fn default() -> Self {
        Self(true)
    }
}

/// The window was covered, minimized or came back. No frames run while it is hidden, so this reaches the
/// screens directly.
pub fn set_window_visible(app: &mut App, visible: bool) {
    if std::mem::replace(&mut app.local::<WindowVisible>().0, visible) == visible {
        return;
    }
    let host = app.host.clone();
    std::thread::spawn(move || host.set_window_hidden(!visible));
    for state in app.local::<Panels>().0.values_mut() {
        let active = state.props.active;
        if let Some(view) = state.screen.as_mut() {
            view.set_watching(active && visible);
        }
    }
}

fn panel<'a>(app: &'a mut App, session: &str) -> Option<&'a mut PanelState> {
    app.local::<Panels>().0.get_mut(session)
}

fn spinner(size: f32) -> E {
    // Turning a frame at a time: nothing renders while idle, so it has to ask for the next one.
    repaint();
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let turned = START.get_or_init(Instant::now).elapsed().as_secs_f32() * 360.0;
    icon("LoaderCircle", size).rotate(turned % 360.0)
}

fn error_text(reason: impl std::fmt::Display) -> String {
    reason.to_string()
}

fn card_action(label: impl Into<Arc<str>>, disabled: bool) -> E {
    button(label).h(27.0).px(11.0).disabled(disabled)
}

// ----- the host, on a task thread ---------------------------------------------------------------------

struct TaskOps {
    task: Task,
    session: String,
}

impl PageOps for TaskOps {
    fn status(&self) -> Result<BrowserStatus, String> {
        self.task.invoke("browser_status", session_args(&self.session))
    }

    fn navigate(&self, url: &str) -> Result<BrowserStatus, String> {
        self.task.invoke("browser_navigate", navigate_args(&self.session, url))
    }

    fn set_network(&self, target: Option<&PreviewTarget>) -> Result<(), String> {
        self.task.invoke::<Value>("browser_set_page_network", page_network_args(&self.session, target)).map(|_| ())
    }

    fn open_page(&self, url: &str) -> Result<(), String> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let session = self.session.clone();
        let url = url.to_owned();
        self.task.post(move |app| {
            let done: Box<dyn FnOnce(&mut App, Result<(), String>) + Send> = Box::new(move |_, result| {
                let _ = sender.try_send(result);
            });
            match panel(app, &session).and_then(|state| state.props.on_open_page.clone()) {
                Some(open) => open(app, Some(url), done),
                None => pages::present_page(app, &session, Some(url), move |app, result| done(app, result.map(|_| ()))),
            }
        });
        receiver.recv_timeout(Duration::from_secs(60)).map_err(|_| "timed out opening this tab's page".to_owned())?
    }

    fn action(&self, name: &str, value: Value) -> Result<BrowserStatus, String> {
        self.task.invoke("browser_action", action_args(&self.session, name, value))
    }
}

/// What a flow needs from the pane, read once: the rows it decides a viewer by.
#[derive(Clone)]
struct FlowContext {
    session: String,
    target: Option<PreviewTarget>,
    has_page: bool,
    suspended: bool,
    viewer_rows: Vec<String>,
    status: BrowserStatus,
}

fn flow_context(state: &PanelState, session: &str) -> FlowContext {
    FlowContext {
        session: session.to_owned(),
        target: state.props.target.clone(),
        has_page: state.status.has_page,
        suspended: state.status.suspended,
        viewer_rows: state.servers.rows().into_iter().filter(|row| row.remote).filter_map(|row| row.url).collect(),
        status: state.status.clone(),
    }
}

/// `run`: one user action at a time, with its failure in the error strip.
fn run(app: &mut App, session: &str, task_fn: impl FnOnce(&Task, &FlowContext) -> Result<BrowserStatus, String> + Send + 'static) {
    let Some(state) = panel(app, session) else {
        return;
    };
    if state.pending {
        return;
    }
    state.pending = true;
    state.error = None;
    let epoch = state.page_epoch;
    let context = flow_context(state, session);
    let session = session.to_owned();
    app.spawn(move |task| {
        let result = task_fn(&task, &context);
        task.post(move |app| {
            let Some(state) = panel(app, &session).filter(|state| state.page_epoch == epoch) else {
                return;
            };
            state.pending = false;
            match result {
                Ok(next) => {
                    state.address = if next.url == "about:blank" { String::new() } else { next.url.clone() };
                    state.status = next;
                }
                Err(reason) => state.error = Some(error_text(reason)),
            }
        });
    });
}

fn with_flow<R>(task: &Task, context: &FlowContext, f: impl FnOnce(&PageFlow<'_, TaskOps>) -> R) -> R {
    let ops = TaskOps { task: task.clone(), session: context.session.clone() };
    let rows = context.viewer_rows.clone();
    let is_viewer = move |url: &str| rows.iter().any(|address| preview_url_is_served_at_path(Some(url), address));
    let flow = PageFlow { ops: &ops, session_id: &context.session, target: context.target.as_ref(), can_open_page: true, known_has_page: context.has_page, known_suspended: context.suspended, is_viewer: &is_viewer };
    f(&flow)
}

fn action(app: &mut App, session: &str, name: &'static str, value: Value) {
    let args = action_args(session, name, value);
    run(app, session, move |task, _| task.invoke("browser_action", args));
}

fn navigate(app: &mut App, session: &str, raw: String, viewer: bool) {
    // A remote screen is drawn here, with no page and no engine under it. The page is still what any
    // other address means, and what a screen falls back to when this process cannot reach it directly.
    let address = normalize_browser_address(&raw).ok();
    if let Some(url) = address.as_deref().filter(|url| viewer || remote_address(app, session, url))
        && show_screen(app, session, url.to_owned())
    {
        return;
    }
    if let Some(state) = panel(app, session) {
        state.screen = None;
    }
    run(app, session, move |task, context| with_flow(task, context, |flow| flow.navigate(&raw, viewer, &context.status.url)));
}

/// Whether this address is the viewer of one of the remote screens the pane knows.
fn remote_address(app: &mut App, session: &str, url: &str) -> bool {
    panel(app, session).is_some_and(|state| state.servers.rows().into_iter().filter(|row| row.remote).filter_map(|row| row.url).any(|address| preview_url_is_served_at_path(Some(url), &address)))
}

/// Puts the screen whose viewer is at `url` in the pane. `false`: no session here answers to it.
fn show_screen(app: &mut App, session: &str, url: String) -> bool {
    let Some(remote) = app.host.remote_screen(&url) else {
        return false;
    };
    let same = panel(app, session).is_some_and(|state| state.screen.as_ref().is_some_and(|view| view.url == url));
    if same {
        return true;
    }
    let visible = app.local::<WindowVisible>().0;
    let mut view = ScreenView::new(app, url, remote);
    let Some(state) = panel(app, session) else {
        return false;
    };
    view.set_watching(state.props.active && visible);
    state.screen = Some(view);
    state.editing = false;
    state.error = None;
    repaint();
    true
}

fn step_history(app: &mut App, session: &str, back: bool) {
    run(app, session, move |task, context| with_flow(task, context, |flow| flow.step_history(back, &context.status)));
}

fn reload_page(app: &mut App, session: &str) {
    run(app, session, |task, context| with_flow(task, context, |flow| flow.reload(&context.status)));
}

/// A row's page. A remote-control row's page is its viewer, whatever `port` says: it has none.
fn open_server_page(app: &mut App, session: &str, row: &PreviewServerRow) {
    if row.remote && row.url.is_none() {
        return;
    }
    navigate(app, session, preview_server_address(row.port, row.url.as_deref()), row.remote);
}

// ----- servers ----------------------------------------------------------------------------------------

fn run_server(app: &mut App, session: &str, name: String) {
    let Some(state) = panel(app, session) else {
        return;
    };
    let Some(target) = state.props.target.clone() else {
        return;
    };
    state.servers.begin_run(&name);
    let session = session.to_owned();
    let args = start_args(&target, Some(&name));
    app.invoke::<PreviewStartOutcome>("preview_start_server", args, move |app, outcome| {
        let Some(state) = panel(app, &session) else {
            return;
        };
        if let Some(row) = state.servers.run_answered(&name, outcome) {
            open_server_page(app, &session, &row);
        }
    });
}

fn stop_server(app: &mut App, session: &str, row: &PreviewServerRow) {
    let Some(handle) = panel(app, session).and_then(|state| state.servers.stop(row)) else {
        return;
    };
    app.invoke_logged("preview_stop_server", stop_args(&handle));
}

fn stop_all_servers(app: &mut App, session: &str) {
    let handles = panel(app, session).map_or_else(Vec::new, |state| state.servers.stop_all());
    for handle in handles {
        app.invoke_logged("preview_stop_server", stop_args(&handle));
    }
}

fn open_row(app: &mut App, session: &str, row: &PreviewServerRow) {
    if let Some(state) = panel(app, session) {
        state.servers.adopt(row);
    }
    open_server_page(app, session, row);
}

// ----- polling: the only way the pane hears about a page it isn't asking anything of ---------------------

/// Whether the pane is still being drawn. Frames only count up when something renders, so a window with
/// nothing to do doesn't read as a pane that went away.
fn still_mounted(task: &Task, session: &str) -> bool {
    let session = session.to_owned();
    task.update(move |app| {
        let frame = app.frame_no;
        match panel(app, &session) {
            Some(state) if frame.saturating_sub(state.seen_frame) > 2 => {
                unmount(app, &session);
                false
            }
            Some(state) => state.alive.load(Ordering::SeqCst),
            None => false,
        }
    })
}

fn start_pollers(app: &mut App, session: &str) {
    let status_session = session.to_owned();
    app.spawn(move |task| {
        let session = status_session;
        let mut next_background = Instant::now();
        while still_mounted(&task, &session) {
            let read = session.clone();
            let epoch = task.update(move |app| panel(app, &read).map_or(0, |state| state.page_epoch));
            match task.invoke::<BrowserStatus>("browser_status", session_args(&session)) {
                Ok(next) => {
                    let posted = session.clone();
                    task.post(move |app| status_arrived(app, &posted, epoch, next));
                }
                Err(reason) => {
                    let posted = session.clone();
                    task.post(move |app| {
                        if let Some(state) = panel(app, &posted) {
                            state.error = Some(reason);
                        }
                    });
                }
            }
            if Instant::now() >= next_background {
                next_background = Instant::now() + BACKGROUND_POLL;
                let read = session.clone();
                for other in task.update(move |app| panel(app, &read).map_or_else(Vec::new, |state| state.props.background_sessions.clone())) {
                    if let Ok(next) = task.invoke::<BrowserStatus>("browser_status", session_args(&other)) {
                        let posted = session.clone();
                        task.post(move |app| background_status_arrived(app, &posted, &other, next));
                    }
                }
            }
            std::thread::sleep(STATUS_POLL);
        }
    });
    let servers_session = session.to_owned();
    app.spawn(move |task| {
        let session = servers_session;
        let mut next_logs = Instant::now();
        let mut next_servers = Instant::now();
        while still_mounted(&task, &session) {
            let read = session.clone();
            let (target, log_handle) = task.update(move |app| match panel(app, &read) {
                Some(state) => (state.props.target.clone(), if state.logs_open { state.servers.log_row().and_then(|row| row.server.map(|server| server.handle)) } else { None }),
                None => (None, None),
            });
            if let (Some(target), true) = (&target, Instant::now() >= next_servers) {
                next_servers = Instant::now() + PREVIEW_SERVER_POLL_INTERVAL;
                let listed = task.invoke::<PreviewConfigurationList>("preview_list_configurations", target_args(target));
                let running = task.invoke::<Vec<PreviewServerSnapshot>>("preview_list_servers", target_args(target));
                let posted = session.clone();
                task.post(move |app| {
                    let Some(state) = panel(app, &posted) else {
                        return;
                    };
                    match (listed, running) {
                        (Ok(listed), Ok(running)) => {
                            if let Some(row) = state.servers.poll_answered(listed, running) {
                                open_server_page(app, &posted, &row);
                            }
                        }
                        (Err(reason), _) | (_, Err(reason)) => state.servers.poll_failed(reason),
                    }
                });
            }
            if Instant::now() >= next_logs {
                next_logs = Instant::now() + PREVIEW_LOG_POLL_INTERVAL;
                let lines = match &log_handle {
                    // A stopped server takes its buffer with it; the drawer falls back to its empty state.
                    Some(handle) => task.invoke::<String>("preview_server_logs", logs_args(handle, PREVIEW_MAX_LOG_LINES)).ok().map(|rendered| preview_log_lines(&rendered)),
                    None => Some(Vec::new()),
                };
                if let Some(lines) = lines {
                    let posted = session.clone();
                    task.post(move |app| {
                        if let Some(state) = panel(app, &posted) {
                            state.log_lines = lines;
                        }
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

/// A page behind this one. Dropped when it is no longer asked about: it may have closed while the answer was on its way.
fn background_status_arrived(app: &mut App, session: &str, other: &str, next: BrowserStatus) {
    if !panel(app, session).is_some_and(|state| state.props.background_sessions.iter().any(|listed| listed == other)) {
        return;
    }
    let controller = app.local::<BrowserController>();
    if !controller.close_in_flight(other) && controller.statuses.get(other) != Some(&next) {
        controller.statuses.insert(other.to_owned(), next);
        repaint();
    }
}

fn status_arrived(app: &mut App, session: &str, epoch: u64, next: BrowserStatus) {
    if panel(app, session).is_none_or(|state| state.page_epoch != epoch) {
        return;
    }
    // The model connected a screen for this tab.
    if let Some(url) = app.host.take_screen_request(session) {
        show_screen(app, session, url);
    }
    let wants_pick = next.element_picker.as_ref().is_some_and(|picker| picker.pending_pick);
    let Some(state) = panel(app, session) else {
        return;
    };
    if !state.editing {
        state.address = if next.url == "about:blank" { String::new() } else { next.url.clone() };
    }
    state.status = next;
    // The pick itself never rides the poll, or its crop would be re-serialized every 700 ms: the poll
    // only learns one is waiting, and drains it through its own command.
    if wants_pick && state.props.on_element_picked.is_some() {
        let owned = session.to_owned();
        app.invoke::<Option<SelectedElement>>("browser_take_selected_element", session_args(session), move |app, picked| {
            let Ok(Some(picked)) = picked else {
                return;
            };
            if let Some(on_picked) = panel(app, &owned).and_then(|state| state.props.on_element_picked.clone()) {
                on_picked(app, picked);
            }
        });
    }
}

/// The tab's page closed while its pane stays up, as it does on the last one: it is a start page again.
pub(crate) fn page_closed(app: &mut App, session: &str) {
    reset_page_history(session);
    let Some(state) = panel(app, session) else {
        return;
    };
    state.page_epoch += 1;
    state.status = BrowserStatus::default();
    state.address.clear();
    state.editing = false;
    state.pending = false;
    state.error = None;
    state.screen = None;
    state.observed = None;
    state.sent_preferences = None;
    state.dialog_text = None;
    state.servers.forget_page();
    state.projection = Projection::default();
    if state.annotating {
        close_annotate(session, state);
    }
    if let Some(image) = state.still_image.take() {
        release_image(image.id);
    }
    repaint();
}

/// The pane went away.
fn unmount(app: &mut App, session: &str) {
    let Some(mut state) = app.local::<Panels>().0.remove(session) else {
        return;
    };
    state.alive.store(false, Ordering::SeqCst);
    // A pane that goes away while armed would leave the page in inspect mode with nothing to turn it off.
    if state.status.element_picker.as_ref().is_some_and(|picker| picker.armed) {
        app.invoke_logged("browser_action", action_args(session, "select_element", json!(false)));
    }
    for command in state.projection.unmount() {
        if let Command::SetParked { value, .. } = command {
            // A refusal is not the user's to see: the host parks the page on every path that hides it.
            app.invoke::<Value>("browser_action", action_args(session, "project", json!(value)), |_, _| {});
        }
    }
    for image in state.still_image.take() {
        release_image(image.id);
    }
}

// ----- the frame --------------------------------------------------------------------------------------

fn edges(rect: Rect) -> Edges {
    (rect.x, rect.y, rect.x + rect.w, rect.y + rect.h)
}

/// Every trusted surface over the page: this pane's own, then whatever the rest of the app has open.
fn measure_covered(session: &str, state: &PanelState) -> bool {
    if !state.props.active {
        return false;
    }
    let Some(page) = rect_of(("browser-page-area", session)) else {
        return false;
    };
    let mut rects: Vec<Edges> = overlay_rects().into_iter().map(edges).collect();
    rects.extend(state.props.trusted_overlay_rect.map(edges));
    // The log drawer is deliberately absent: it doesn't cover the page, it takes height from it.
    for key in ["browser-error", "browser-notice", "browser-handback", "browser-dialog", "browser-sketch", "browser-body-card"] {
        rects.extend(rect_of((key, session)).map(edges));
    }
    any_rect_overlaps(&rects, edges(page))
}

fn send_stacking(app: &mut App, session: &str, name: &'static str, value: bool, chain: u64) {
    let owned = session.to_owned();
    // A refusal is not the user's to see: the pane measures the moment it mounts, while the host may still
    // be creating the page, and the status poll is what reconciles the two afterwards.
    app.invoke::<BrowserStatus>("browser_action", action_args(session, name, json!(value)), move |app, next| {
        let Some(state) = panel(app, &owned) else {
            return;
        };
        state.projection.answered(chain);
        if let Ok(next) = next {
            state.status = next;
        }
        repaint();
    });
}

fn capture_frame(app: &mut App, session: &str, request: u64) {
    let session = session.to_owned();
    app.spawn(move |task| {
        let capture = task.invoke::<Option<BrowserPageCapture>>("browser_capture_page", session_args(&session)).ok().flatten();
        let frame = capture.and_then(|capture| {
            let bytes = base64::engine::general_purpose::STANDARD.decode(&capture.data).ok()?;
            let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
            let (width, height) = decoded.dimensions();
            Some((Arc::<str>::from(capture.data), Arc::new(decoded.into_raw()), width, height))
        });
        task.post(move |app| {
            if let Some(state) = panel(app, &session) {
                let key = session.clone();
                state.projection.captured(request, &key, frame, Instant::now());
            }
            repaint();
        });
    });
}

/// What this frame's measurements and the machine's state say the host should do.
fn step(app: &mut App, session: &str, body_is_page: bool) {
    let Some(state) = panel(app, session) else {
        return;
    };
    let covered = measure_covered(session, state);
    let parked = !body_is_page || covered || !state.props.active;
    let area = rect_of(("browser-page-area", session));
    let input = ProjectionInput {
        content_key: Some(session.to_owned()),
        enabled: true,
        presented: state.status.open,
        has_content: body_is_page && state.status.has_page,
        parked,
        covered,
        host_parked: state.status.projected,
        host_covered: state.status.occluded,
        box_width: area.map_or(0.0, |area| area.w),
        box_height: area.map_or(0.0, |area| area.h),
    };
    let now = Instant::now();
    let commands = state.projection.frame(&input, now);
    if let Some(at) = state.projection.next_wake() {
        if at > now {
            WAKE.with(|wake| wake.set(Some(at - now)));
        }
    }
    sync_still(state, session);
    for command in commands {
        match command {
            Command::SetParked { chain, value } => send_stacking(app, session, "project", value, chain),
            Command::SetCovered { chain, value } => send_stacking(app, session, "occlude", value, chain),
            Command::Capture { request } => capture_frame(app, session, request),
        }
    }
}

thread_local! {
    /// When the capture loop wants the next frame, for the paint that can ask for it.
    static WAKE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// The frame the machine wants on screen, as a texture. Only what is shown is uploaded: warm frames come
/// once a second, and nearly all of them are never seen.
fn sync_still(state: &mut PanelState, session: &str) {
    let wanted = state.projection.still(Some(session)).cloned();
    match (&wanted, &state.still_image) {
        (Some(frame), Some(image)) if image.data == frame.data => {}
        (Some(frame), _) => {
            if let Some(old) = state.still_image.take() {
                release_image(old.id);
            }
            state.still_image = Some(Image { id: queue_image(frame.rgba.as_ref().clone(), frame.width, frame.height), size: (frame.width, frame.height), data: frame.data.clone() });
        }
        (None, Some(_)) => {
            if let Some(old) = state.still_image.take() {
                release_image(old.id);
            }
        }
        (None, None) => {}
    }
}

/// The pane. Call it every frame the pane is on screen; stopping the calls is the pane unmounting.
pub fn browser_pane(app: &mut App, props: BrowserPaneProps) -> BrowserPaneView {
    pages::attach_window(app);
    let tabs = props.tabs.clone();
    let Some(session) = props.session_id.clone() else {
        let header = tabs.as_ref().map_or_else(|| row(), |tabs| tabs(app));
        return BrowserPaneView { header, subheader: None, body: col().flex1().child(idle_page()) };
    };
    let frame_no = app.frame_no;
    let exists = app.local::<Panels>().0.contains_key(&session);
    if !exists {
        let initial = props.initial_status.clone();
        let address = initial.as_ref().filter(|status| status.url != "about:blank").map_or_else(String::new, |status| status.url.clone());
        let state = PanelState {
            status: initial.unwrap_or_default(),
            address,
            editing: false,
            pending: false,
            menu_open: false,
            menu_focus_first: false,
            clear_data_open: false,
            clearing: Clearing::Idle,
            annotating: false,
            annotate_generation: 0,
            backdrop: None,
            still_image: None,
            logs_open: false,
            log_lines: Vec::new(),
            drawer_height: DRAWER_HEIGHT,
            drawer_drag_start: DRAWER_HEIGHT,
            error: None,
            copied_until: None,
            start_page_expanded: false,
            servers: ServersCore::default(),
            projection: Projection::default(),
            alive: Arc::new(AtomicBool::new(true)),
            seen_frame: frame_no,
            observed: None,
            sent_preferences: None,
            dialog_text: None,
            props: props.clone(),
            screen: None,
            page_epoch: 1,
        };
        app.local::<Panels>().0.insert(session.clone(), state);
        start_pollers(app, &session);
        // A screen lives in the pane, so coming back to its tab is what puts it back.
        if let Some(url) = app.local::<BrowserController>().screen_of(&session).map(str::to_owned) {
            show_screen(app, &session, url);
        }
    }
    {
        let state = panel(app, &session).expect("just ensured");
        state.seen_frame = frame_no;
        state.servers.set_target(props.target.as_ref());
        // A trusted overlay another component drew is the newer intent: this pane's own menu closes.
        if (props.trusted_overlay_rect.is_some() && state.props.trusted_overlay_rect.is_none()) || (!props.active && state.props.active) {
            state.menu_open = false;
            state.clear_data_open = false;
        }
        state.props = props.clone();
    }
    publish_page(app, &session);
    follow_history(app, &session);
    send_preferences(app, &session);
    finish_flashes(app, &session);

    let body_state = {
        let state = panel(app, &session).expect("just ensured");
        let rows = state.servers.rows();
        let count = match &state.servers.configurations {
            Some(list) => Some(list.servers.len()),
            // Still unread only while there's a workspace to read it from and nothing said it can't be.
            None if state.props.target.is_some() && state.servers.unreachable.is_none() => None,
            None => Some(0),
        };
        let url = state.screen.as_ref().map_or(state.status.url.as_str(), |view| view.url.as_str());
        preview_body_state(url, count, &rows, state.servers.pending_name.as_deref(), state.servers.start_error.as_ref(), state.servers.stopped.as_ref())
    };
    let visible = app.local::<WindowVisible>().0;
    let showing_screen = {
        let state = panel(app, &session).expect("just ensured");
        let active = state.props.active;
        match state.screen.as_mut() {
            // Frames flow only while the picture can be seen: not behind a card, and not in a tab at the back.
            Some(view) => {
                view.set_watching(active && visible && body_state == PreviewBodyState::Page);
                true
            }
            None => false,
        }
    };
    step(app, &session, body_state == PreviewBodyState::Page && !showing_screen);
    if let Some(area) = rect_of(("browser-page-area", session.as_str())) {
        let logs_open = panel(app, &session).is_some_and(|state| state.logs_open);
        pages::publish_bounds(app, &session, area, 0.0, if logs_open { 0.0 } else { props.bottom_corner_radius });
    }

    let toolbar = toolbar_view(app, &session);
    let body = body_view(app, &session, &body_state);
    match tabs {
        Some(tabs) => {
            let strip = tabs(app);
            BrowserPaneView { header: strip, subheader: Some(toolbar), body }
        }
        None => BrowserPaneView { header: toolbar, subheader: None, body },
    }
}

/// What the rest of the app reads about this tab: its name in the strip, whether it has one, which server it belongs to.
fn publish_page(app: &mut App, session: &str) {
    let Some(state) = panel(app, session) else {
        return;
    };
    let status = state.status.clone();
    let screen = state.screen.as_ref().map(|view| view.url.clone());
    let controller = app.local::<BrowserController>();
    if controller.close_in_flight(session) {
        return;
    }
    if controller.statuses.get(session) != Some(&status) {
        controller.statuses.insert(session.to_owned(), status);
    }
    controller.set_screen(session, screen);
}

/// The pane's mirror of the page's history follows what the host reports.
fn follow_history(app: &mut App, session: &str) {
    let Some(state) = panel(app, session) else {
        return;
    };
    if !state.status.has_page {
        return;
    }
    let seen = (state.status.url.clone(), state.status.loading, state.status.can_go_back, state.status.can_go_forward);
    if state.observed.as_ref() == Some(&seen) {
        return;
    }
    state.observed = Some(seen);
    let rows: Vec<String> = state.servers.rows().into_iter().filter(|row| row.remote).filter_map(|row| row.url).collect();
    let is_viewer = |url: &str| rows.iter().any(|address| preview_url_is_served_at_path(Some(url), address));
    observe_page_history(session, &history_status(&state.status), &is_viewer);
}

/// The app's theme and language, to a page that has appeared or an app that changed them. Shown again
/// after a pane was closed, the page also drops a colour scheme the model forced.
fn send_preferences(app: &mut App, session: &str) {
    let chinese = app.language_is_chinese();
    let night = theme().dark;
    let Some(state) = panel(app, session) else {
        return;
    };
    if !state.status.has_page || state.sent_preferences == Some((night, chinese)) {
        return;
    }
    state.sent_preferences = Some((night, chinese));
    let theme_action = if take_preview_theme_resync(session) { "theme_resync" } else { "theme" };
    let theme_args = action_args(session, theme_action, json!(if night { "night" } else { "day" }));
    let locale_args = action_args(session, "locale", json!(if chinese { "zh-CN" } else { "en-US" }));
    app.spawn(move |task| {
        let _ = task.invoke::<Value>("browser_action", theme_args);
        let _ = task.invoke::<Value>("browser_action", locale_args);
    });
}

fn finish_flashes(app: &mut App, session: &str) {
    let Some(state) = panel(app, session) else {
        return;
    };
    let now = Instant::now();
    if let Clearing::Cleared(at) = state.clearing {
        if now.duration_since(at) >= CLEARED_FLASH {
            state.clearing = Clearing::Idle;
            // Only the menu this confirmation is sitting in may be closed by it.
            state.menu_open = false;
            state.clear_data_open = false;
        } else {
            repaint();
        }
    }
    if let Some(until) = state.copied_until {
        if now >= until {
            state.copied_until = None;
        } else {
            repaint();
        }
    }
}

// ----- the toolbar ---------------------------------------------------------------------------------------

/// `LoaderCircle aria-label`: the turning glyph, for where nothing else says a page is on its way.
fn page_loading() -> E {
    spinner(13.0).role(Role::Image).aria_label(t("网页加载中", "Page loading"))
}

fn tool(name: &'static str, label: &str, disabled: bool, pressed: bool, key: (&'static str, String)) -> E {
    let t_ = theme();
    let label: Arc<str> = Arc::from(label);
    icon_button(name, label).key(key).w(24.0).h(24.0).radius(6.0).disabled(disabled).when(pressed, |el| el.bg(t_.control_fill_pressed))
}

fn toolbar_view(app: &mut App, session: &str) -> E {
    let t_ = theme();
    let Some(state) = panel(app, session) else {
        return row();
    };
    let screen_label = state.screen.as_ref().map(|view| view.state.name.clone());
    let showing_screen = screen_label.is_some();
    let available = state.status.open && !state.pending && !showing_screen;
    let picker_armed = state.status.element_picker.as_ref().is_some_and(|picker| picker.armed);
    let loading = state.status.loading;
    let busy = state.pending || loading;
    // From a screen, Back is the way to the page it was opened over.
    let can_back = (available && state.status.can_go_back) || (showing_screen && state.status.has_page);
    let can_forward = available && state.status.can_go_forward;
    let annotating = state.annotating;
    let url = state.status.url.clone();
    let address = state.address.clone();
    let editing = state.editing;
    let menu_open = state.menu_open;
    let id = session.to_owned();

    let address_slot: E = if editing {
        let (submit, change, blur, cancel) = (id.clone(), id.clone(), id.clone(), id.clone());
        let revert_to = url.clone();
        row()
            .flex1()
            .min_w(0.0)
            .h(24.0)
            .mr(24.0)
            .px(6.0)
            .gap(6.0)
            .items_center()
            .radius(5.0)
            .bg(t_.control_fill)
            .role(Role::Search)
            .child(
                field(("browser-address", session), address)
                    .bare()
                    .on_key(move |app: &mut App, key: &KeyPress| {
                        if key.key != Key::Escape {
                            return false;
                        }
                        if let Some(state) = panel(app, &cancel) {
                            state.editing = false;
                            state.address = if revert_to == "about:blank" { String::new() } else { revert_to.clone() };
                        }
                        true
                    })
                    .placeholder(t("输入网址", "Type a URL"))
                    .select_on_focus(true)
                    .on_change(move |app: &mut App, text: String| {
                        if let Some(state) = panel(app, &change) {
                            state.address = text;
                        }
                    })
                    .on_blur(move |app: &mut App, _| {
                        if let Some(state) = panel(app, &blur) {
                            state.editing = false;
                        }
                    })
                    .on_submit(move |app: &mut App, text: String| {
                        if let Some(state) = panel(app, &submit) {
                            state.editing = false;
                        }
                        navigate(app, &submit, text, false);
                    })
                    .build()
                    .aria_label(t("页面网址", "Page URL")),
            )
            .child(busy.then(page_loading))
    } else {
        let parts = if showing_screen { None } else { split_browser_address(&url) };
        let label: E = match &parts {
            None if showing_screen => text(screen_label.clone().unwrap_or_default()).ty(BODY_SMALL).ellipsis().flex1().min_w(0.0),
            Some((host, path)) => spans(vec![span(host.clone(), BODY_SMALL), span(path.clone(), BODY_SMALL)]).ellipsis().flex1().min_w(0.0),
            None => text(t("输入网址", "Enter a URL")).ty(BODY_SMALL).color(t_.text_tertiary).ellipsis().flex1().min_w(0.0),
        };
        let open = id.clone();
        let tip: Arc<str> = match &parts {
            Some(_) => Arc::from(tf("页面网址：{url}", "Page URL: {url}", &[("url", &url)])),
            None => Arc::from(t("输入网址", "Enter a URL")),
        };
        row()
            .key(("browser-address-chip", session))
            .flex1()
            .min_w(0.0)
            .h(24.0)
            .mr(24.0)
            .px(6.0)
            .gap(6.0)
            .items_center()
            .radius(5.0)
            .hover_bg(t_.subtle_hover)
            // A screen's name is what it shows, and there is no address to read out.
            .when(!showing_screen, |e| e.aria_label(tip.clone()))
            .tooltip(tip)
            .on_click(move |app: &mut App| {
                if let Some(state) = panel(app, &open) {
                    state.address = if state.status.url == "about:blank" || state.screen.is_some() { String::new() } else { state.status.url.clone() };
                    state.editing = true;
                }
                // A field is focused by its own name, which is its key under "field".
                focus_request(("field", ("browser-address", open.as_str())));
            })
            .child(label)
            .child(busy.then(page_loading))
    };

    let menu_trigger = {
        let toggle = id.clone();
        tool("Globe2", t("服务器与设置", "Servers & settings"), false, menu_open, ("browser-menu-trigger", id.clone())).aria_haspopup(HasPopup::Menu).aria_expanded(menu_open).aria_controls(("browser-menu", session)).on_click(move |app: &mut App| {
            if let Some(state) = panel(app, &toggle) {
                state.menu_open = !state.menu_open;
                state.menu_focus_first = state.menu_open;
                state.error = None;
                if !state.menu_open {
                    state.clear_data_open = false;
                }
            }
        })
    };
    let back = {
        let id = id.clone();
        tool("ChevronLeft", t("后退", "Go back"), !can_back, false, ("browser-back", id.clone())).on_click(move |app: &mut App| {
            let left_screen = panel(app, &id).is_some_and(|state| state.screen.take().is_some());
            if !left_screen {
                step_history(app, &id, true);
            }
        })
    };
    let forward = {
        let id = id.clone();
        tool("ChevronRight", t("前进", "Go forward"), !can_forward, false, ("browser-forward", id.clone())).on_click(move |app: &mut App| step_history(app, &id, false))
    };
    let annotate = {
        let id = id.clone();
        tool("Pencil", t("标注", "Annotate"), !available && !annotating, annotating, ("browser-annotate", id.clone())).aria_pressed(annotating).on_click(move |app: &mut App| toggle_annotate(app, &id))
    };
    let select = {
        let id = id.clone();
        let label = if picker_armed { t("退出选择模式", "Exit selection mode") } else { t("选择元素", "Select element") };
        tool("MousePointerClick", label, !available && !picker_armed, picker_armed, ("browser-select", id.clone())).aria_pressed(picker_armed).on_click(move |app: &mut App| action(app, &id, "select_element", json!(!picker_armed)))
    };
    // The source's reload has no stop state; a hung load still needs a way out of it.
    let reload = {
        let id = id.clone();
        let (name, label) = if loading { ("X", t("停止加载", "Stop loading")) } else { ("RotateCw", t("刷新页面", "Reload")) };
        tool(name, label, !available, false, ("browser-reload", id.clone())).on_click(move |app: &mut App| {
            if loading {
                action(app, &id, "stop", Value::Null);
            } else {
                reload_page(app, &id);
            }
        })
    };
    let menu = menu_open.then(|| menu_view(app, session));
    row()
        .flex1()
        .min_w(0.0)
        .items_center()
        .gap(8.0)
        .child(menu_trigger)
        .child(address_slot)
        .child(row().shrink0().items_center().gap(4.0).child(back).child(forward).child(annotate).child(select).child(reload))
        .child(menu)
}

/// Enters or leaves the drawing layer. The capture is dispatched *before* the layer opens, because opening
/// it covers the pane body and the host withdraws the native page, after which nothing is left to capture.
fn toggle_annotate(app: &mut App, session: &str) {
    let Some(state) = panel(app, session) else {
        return;
    };
    state.annotate_generation += 1;
    if state.annotating {
        close_annotate(session, state);
        return;
    }
    let generation = state.annotate_generation;
    let armed = state.status.element_picker.as_ref().is_some_and(|picker| picker.armed);
    state.annotating = true;
    state.backdrop = None;
    // The drawing layer covers the page, so an armed picker would be waiting on clicks that can't reach it.
    if armed {
        action(app, session, "select_element", json!(false));
    }
    let owned = session.to_owned();
    app.spawn(move |task| {
        let capture = task.invoke::<Option<BrowserPageCapture>>("browser_capture_page", session_args(&owned)).ok().flatten();
        task.post(move |app| {
            let Some(state) = panel(app, &owned) else {
                return;
            };
            if state.annotate_generation != generation {
                return;
            }
            // The drawing layer takes the page down, so this may come back empty. The still is a frame of
            // the same page a moment earlier, a better backdrop to draw on than none.
            let payload = capture.map(|page| Arc::from(page.data)).or_else(|| state.projection.warm().map(|frame| frame.data.clone()));
            state.backdrop = payload.map(|data: Arc<str>| Arc::from(format!("data:image/png;base64,{data}")));
        });
    });
}

fn sketch_key(session: &str) -> String {
    format!("browser:{session}")
}

fn close_annotate(session: &str, state: &mut PanelState) {
    state.annotating = false;
    state.backdrop = None;
    close_sketch(&sketch_key(session));
}

fn attach_annotation(app: &mut App, session: &str, data_url: String) {
    let Some(state) = panel(app, session) else {
        return;
    };
    state.annotate_generation += 1;
    close_annotate(session, state);
    let Some(on_attach) = state.props.on_attach_image.clone() else {
        return;
    };
    let png = data_url.split_once(',').and_then(|(_, data)| base64::engine::general_purpose::STANDARD.decode(data).ok());
    match png {
        Some(png) => on_attach(app, "page-annotation.png".to_owned(), png),
        None => state.error = Some(t("无法生成标注图片", "Could not make the annotated image").to_owned()),
    }
}

// ----- the menu ---------------------------------------------------------------------------------------------

fn menu_group(label: &'static str) -> E {
    text(label).ty(CAPTION_STRONG).color(theme().text_secondary).mx(7.0).mt(4.0).mb(3.0)
}

fn menu_item(key: (&'static str, String), label: impl Into<Arc<str>>) -> E {
    row().key(key).w_full().min_h(24.0).px(7.0).py(2.0).gap(14.0).items_center().justify_between().radius(5.0).hover_bg(theme().subtle_hover).focusable().role(Role::MenuItem).child(text(label).ty(BODY_SMALL))
}

/// `DocsLink inline`.
fn docs_link(page: &'static str, section: &'static str) -> E {
    let t_ = theme();
    let locale = if mewrk_client::i18n::is_english() { "en" } else { "zh-CN" };
    let href = format!("https://mewrk.dev/{locale}/{page}.html#{section}");
    row().key(("browser-docs", page, section)).items_center().gap(2.0).cursor(CursorKind::Hand).role(Role::Link).on_click(move |app: &mut App| app.invoke_logged("open_external_url", json!({ "url": href }))).child(text(t("详情", "Learn more")).ty(FINE).color(t_.accent_text).nowrap()).child(icon("ExternalLink", 9.0).color(t_.accent_text))
}

fn server_tag(content: E) -> E {
    row().shrink0().min_w(34.0).min_h(21.0).px(7.0).items_center().justify_center().radius(6.0).border(theme().divider).child(content)
}

/// Closes the menu and gives the focus back to the control that opened it: Escape, or one of its rows chosen.
fn close_menu(app: &mut App, session: &str) {
    dismiss_menu(app, session);
    focus_request(("browser-menu-trigger", session.to_owned()));
}

/// Closes the menu where a press elsewhere has already put the focus.
fn dismiss_menu(app: &mut App, session: &str) {
    if let Some(state) = panel(app, session) {
        state.menu_open = false;
        state.clear_data_open = false;
    }
}

/// The arrows, Home and End among a menu's rows, wrapping; Enter or Space chooses the one with the focus. A row knows a click
/// but no key, and only the menu knows which rows are its.
fn menu_key(app: &mut App, session: &str, key: &KeyPress, rows: &[(u64, Rc<dyn Fn(&mut App)>)]) -> bool {
    let at = rows.iter().position(|(id, _)| *id == focused_id());
    let last = rows.len().saturating_sub(1);
    let next = match key.key {
        Key::Escape => {
            close_menu(app, session);
            return true;
        }
        Key::Enter | Key::Char(' ') => {
            let Some(at) = at else { return false };
            let choose = rows[at].1.clone();
            choose(app);
            return true;
        }
        Key::Home => 0,
        Key::End => last,
        Key::Down => at.map_or(0, |at| if at == last { 0 } else { at + 1 }),
        Key::Up => at.map_or(last, |at| if at == 0 { last } else { at - 1 }),
        _ => return false,
    };
    if let Some((id, _)) = rows.get(next) {
        focus_request_id(*id);
    }
    true
}

fn clear_browsing_data(app: &mut App, session: &str) {
    let Some(state) = panel(app, session) else {
        return;
    };
    state.clearing = Clearing::Running;
    let owned = session.to_owned();
    app.invoke::<BrowserStatus>("browser_action", action_args(session, "clear_data", Value::Null), move |app, result| {
        let Some(state) = panel(app, &owned) else {
            return;
        };
        match result {
            Ok(next) => {
                state.status = next;
                state.clearing = Clearing::Cleared(Instant::now());
            }
            Err(reason) => {
                state.error = Some(reason);
                state.clearing = Clearing::Idle;
            }
        }
    });
}

fn open_local_file(app: &mut App, session: &str) {
    let target = panel(app, session).and_then(|state| state.props.file_target.clone());
    let owned = session.to_owned();
    app.invoke::<Option<BrowserStatus>>("browser_open_local_file", open_local_file_args(session, target.as_ref()), move |app, result| {
        let Some(state) = panel(app, &owned) else {
            return;
        };
        match result {
            Ok(Some(next)) => {
                state.address = if next.url == "about:blank" { String::new() } else { next.url.clone() };
                state.status = next;
            }
            Ok(None) => {}
            Err(reason) => state.error = Some(reason),
        }
    });
}

fn platform_icon(row: &PreviewServerRow, size: f32) -> E {
    match preview_row_platform(row) {
        Some(RemoteControlPlatform::Desktop) => icon("Monitor", size),
        Some(_) => icon("Smartphone", size),
        None => icon("Server", size),
    }
}

fn menu_view(app: &mut App, session: &str) -> E {
    let t_ = theme();
    let Some(state) = panel(app, session) else {
        return div();
    };
    let rows = state.servers.rows();
    let pending_name = state.servers.pending_name.clone();
    let current_url = state.status.url.clone();
    let logs_open = state.logs_open;
    let clear_open = state.clear_data_open;
    let clearing = state.clearing;
    let id = session.to_owned();
    let any_running = rows.iter().any(|row| row.running || row.starting);
    let focus_first = std::mem::take(&mut state.menu_focus_first);
    // The rows the keyboard walks, in order.
    let mut walked: Vec<(u64, Rc<dyn Fn(&mut App)>)> = Vec::new();
    let mut menu = col()
        .key(("browser-menu", session))
        .w(270.0)
        .max_h(352.0)
        .p(6.0)
        .scroll_y()
        .bg(t_.flyout)
        .border(t_.flyout_stroke)
        .radius(12.0)
        .shadow(16.0, 8.0, t_.shadow)
        .role(Role::Menu)
        .aria_label(t("浏览器菜单", "Browser menu"))
        .popover(("browser-menu-trigger", id.clone()), Side::Below, Align::Start, 4.0, {
            let id = id.clone();
            move |app: &mut App| dismiss_menu(app, &id)
        });
    // Rendered as items rather than a submenu: a row is one hit target, and pressing anywhere runs it.
    // The group is dropped rather than explained: a browser with no servers says so in the pane's body.
    if !rows.is_empty() {
        menu = menu.child(menu_group(t("服务器", "Servers")));
        for row_data in &rows {
            let stoppable = row_data.running || row_data.starting;
            let busy = row_data.starting || pending_name.as_deref() == Some(&row_data.name);
            let openable = preview_row_openable(row_data);
            let others_pending = pending_name.as_deref().is_some_and(|name| name != row_data.name);
            let disabled = (row_data.starting && !row_data.remote) || others_pending;
            let checked = preview_row_is_open(row_data, Some(&current_url));
            let key = format!("{}:{}", row_data.name, row_data.server.as_ref().map_or("config", |server| server.handle.as_str()));
            let name_color = if row_data.failed { t_.critical } else { t_.text_primary };
            let title: Option<String> = row_data.remote.then(|| [Some(preview_row_detail(row_data)), preview_row_failure(row_data)].into_iter().flatten().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(": "));
            let main = {
                let id = id.clone();
                let clicked = row_data.clone();
                let main_key = ("browser-menu-server", id.clone(), key.clone());
                let choose: Rc<dyn Fn(&mut App)> = Rc::new(move |app: &mut App| {
                    close_menu(app, &id);
                    if openable {
                        open_row(app, &id, &clicked);
                    } else {
                        run_server(app, &id, clicked.name.clone());
                    }
                });
                if !disabled {
                    walked.push((hash_key(main_key.clone()), choose.clone()));
                }
                row()
                    .key(main_key)
                    .flex1()
                    .min_w(0.0)
                    .min_h(24.0)
                    .px(4.0)
                    .gap(10.0)
                    .items_center()
                    .radius(5.0)
                    .focusable()
                    .disabled(disabled)
                    .role(Role::MenuItemRadio)
                    .aria_checked(checked)
                    .aria_label(if openable { tf("打开 {name}", "Open {name}", &[("name", &row_data.name)]) } else { tf("运行 {name}", "Run {name}", &[("name", &row_data.name)]) })
                    .on_click(move |app: &mut App| choose(app))
                    .when(checked, |el| el.child(icon("Check", 12.0)))
                    .when(row_data.remote, |el| el.child(platform_icon(row_data, 12.0)))
                    .child(text(row_data.name.clone()).ty(BODY_SMALL_STRONG).color(if disabled { t_.text_disabled } else { name_color }).ellipsis().flex1().min_w(0.0))
                    .when(!stoppable, |el| {
                        let content: E = if busy { spinner(11.0) } else { text(if row_data.attach { t("打开", "Open") } else { t("运行", "Run") }).ty(CAPTION).color(t_.text_secondary) };
                        el.child(server_tag(content))
                    })
                    .map(|el| match title { Some(title) if !title.is_empty() => el.tooltip(title), _ => el })
            };
            let stop = stoppable.then(|| {
                let id = id.clone();
                let clicked = row_data.clone();
                let content: E = if busy { spinner(11.0) } else { text(t("停止", "Stop")).ty(CAPTION).color(t_.critical) };
                let tip: Arc<str> = Arc::from(tf("停止 {name}", "Stop {name}", &[("name", &row_data.name)]));
                server_tag(content)
                    .key(("browser-menu-stop", id.clone(), key.clone()))
                    .mr(3.0)
                    .disabled(others_pending)
                    .tooltip(tip.clone())
                    .aria_label(tip)
                    .on_click(move |app: &mut App| {
                        close_menu(app, &id);
                        stop_server(app, &id, &clicked);
                    })
            });
            menu = menu.child(row().px(3.0).gap(4.0).items_center().radius(7.0).hover_bg(t_.subtle_hover).child(main).child(stop));
        }
        if any_running {
            let id = id.clone();
            let stop_all_key = ("browser-menu-stop-all", id.clone());
            let choose: Rc<dyn Fn(&mut App)> = Rc::new(move |app: &mut App| {
                close_menu(app, &id);
                stop_all_servers(app, &id);
            });
            walked.push((hash_key(stop_all_key.clone()), choose.clone()));
            menu = menu.child(
                row()
                    .key(stop_all_key)
                    .w_full()
                    .min_h(24.0)
                    .px(7.0)
                    .gap(6.0)
                    .items_center()
                    .radius(5.0)
                    .hover_bg(t_.subtle_hover)
                    .focusable()
                    .role(Role::MenuItem)
                    .on_click(move |app: &mut App| choose(app))
                    .child(icon("Ban", 13.0).color(t_.critical))
                    .child(text(t("停止所有服务器", "Stop all servers")).ty(BODY_SMALL).color(t_.critical)),
            );
        }
        menu = menu.child(divider().my(4.0));
    }
    menu = menu.child(menu_group(t("文件", "Files"))).child({
        let id = id.clone();
        let file_key = ("browser-menu-file", id.clone());
        let choose: Rc<dyn Fn(&mut App)> = Rc::new(move |app: &mut App| {
            close_menu(app, &id);
            open_local_file(app, &id);
        });
        walked.push((hash_key(file_key.clone()), choose.clone()));
        menu_item(file_key, t("打开文件", "Open file")).on_click(move |app: &mut App| choose(app))
    });
    menu = menu.child(divider().my(4.0)).child(menu_group(t("设置", "Settings"))).child({
        let id = id.clone();
        let label = if logs_open { t("隐藏开发服务器日志", "Hide dev server logs") } else { t("显示开发服务器日志", "Show dev server logs") };
        let logs_key = ("browser-menu-logs", id.clone());
        let choose: Rc<dyn Fn(&mut App)> = Rc::new(move |app: &mut App| {
            close_menu(app, &id);
            if let Some(state) = panel(app, &id) {
                state.logs_open = !state.logs_open;
            }
        });
        walked.push((hash_key(logs_key.clone()), choose.clone()));
        menu_item(logs_key, label).on_click(move |app: &mut App| choose(app))
    });
    // Clearing is the one action that stays in the menu while it runs: it leaves nothing on screen to look
    // at, so the row holds its own spinner, says what happened, and the menu closes itself afterwards.
    menu = menu.child({
        let id = id.clone();
        let clear_key = ("browser-menu-clear", id.clone());
        let choose: Rc<dyn Fn(&mut App)> = Rc::new(move |app: &mut App| {
            if let Some(state) = panel(app, &id) {
                state.clear_data_open = !state.clear_data_open;
            }
        });
        walked.push((hash_key(clear_key.clone()), choose.clone()));
        menu_item(clear_key, t("清除浏览数据", "Clear browsing data"))
            .aria_expanded(clear_open)
            .on_click(move |app: &mut App| choose(app))
            .child(icon("ChevronRight", 14.0).rotate(if clear_open { 90.0 } else { 0.0 }))
    });
    if clear_open {
        let id = id.clone();
        let confirm_key = ("browser-menu-clear-confirm", id.clone());
        let choose: Rc<dyn Fn(&mut App)> = Rc::new(move |app: &mut App| clear_browsing_data(app, &id));
        if clearing == Clearing::Idle {
            walked.push((hash_key(confirm_key.clone()), choose.clone()));
        }
        let (glyph, label): (E, &str) = match clearing {
            Clearing::Running => (spinner(13.0), t("清除此标签页的全部浏览数据", "Clear all data for this tab")),
            Clearing::Cleared(_) => (icon("Check", 13.0), t("已清除此标签页的浏览数据", "This tab's data was cleared")),
            Clearing::Idle => (icon("Trash2", 13.0), t("清除此标签页的全部浏览数据", "Clear all data for this tab")),
        };
        menu = menu.child(
            col()
                .mx(3.0)
                .mt(2.0)
                .mb(5.0)
                .p(8.0)
                .gap(7.0)
                .radius(8.0)
                .border(t_.divider)
                .role(Role::Group)
                .aria_label(t("清除浏览数据", "Clear browsing data"))
                .child(row().wrap().items_center().gap(4.0).child(text(t("清除此标签页的登录状态和浏览数据。", "Clears this tab's sign-ins and browsing data.")).ty(CAPTION).color(t_.text_secondary).min_w(0.0)).child(docs_link("preview", "pages")))
                .child(
                    row()
                        .key(confirm_key)
                        .w_full()
                        .min_h(30.0)
                        .px(7.0)
                        .gap(6.0)
                        .items_center()
                        .radius(6.0)
                        .hover_bg(t_.subtle_hover)
                        .focusable()
                        .disabled(clearing != Clearing::Idle)
                        .role(Role::MenuItem)
                        .on_click(move |app: &mut App| choose(app))
                        .child(glyph)
                        .child(text(label).ty(BODY_SMALL)),
                ),
        );
    }
    if focus_first
        && let Some((first, _)) = walked.first()
    {
        focus_request_id(*first);
    }
    let owner = id.clone();
    menu.on_key(move |app: &mut App, key: &KeyPress| menu_key(app, &owner, key, &walked))
}

// ----- the body ----------------------------------------------------------------------------------------------

fn body_view(app: &mut App, session: &str, body_state: &PreviewBodyState) -> E {
    let t_ = theme();
    let id = session.to_owned();
    let Some(state) = panel(app, session) else {
        return div();
    };
    let dialog = state.status.dialog.clone();
    let still = state.still_image.as_ref().map(|image| (image.id, image.size));
    let error = state.error.clone();
    let notice = state.props.link_notice.clone().or_else(|| if state.servers.configurations.is_none() { state.servers.unreachable.clone() } else { None });
    let control = state.status.control.clone().unwrap_or_default();
    let handback = control.owner == "user" && control.handoff_requested;
    let handback_tool = control.requested_tool.clone();
    let pending = state.pending;
    let annotating = state.annotating;
    let logs_open = state.logs_open;
    if let Some(wake) = WAKE.with(|wake| wake.take()) {
        repaint_later(app, wake);
    }

    let card = body_card(app, session, body_state);
    let mut page_area = col().key(("browser-page-area", session)).flex1().min_h(0.0).clip();
    if *body_state == PreviewBodyState::Page && panel(app, session).is_some_and(|state| state.screen.is_some()) {
        let owned = session.to_owned();
        let reach: crate::screen_view::Reach = Rc::new(move |app: &mut App| panel(app, &owned).and_then(|state| state.screen.as_mut()));
        page_area = page_area.child(crate::screen_view::body(app, session, reach));
    }
    if let Some((image_id, size)) = still {
        // Sized by the box rather than the capture: a pane resized while the page is covered would leave
        // the pane's own background beside a still of the old size, and a gap reads as a rendering fault.
        page_area = page_area.child(custom(move |_, ui: &mut Ui<'_>, area: Rect| ui.image_cover(image_id, area, size, 0.0, 1.0)).absolute().inset(0.0));
    }
    if let Some(card) = card {
        page_area = page_area.child(col().key(("browser-body-card", session)).flex1().min_h(0.0).child(card));
    }

    let mut content = col().key(("browser-panel", session)).flex1().min_h(0.0).clip().on_key_global({
        let id = id.clone();
        move |app: &mut App, key: &KeyPress| {
            if key.key != Key::Escape {
                return false;
            }
            let Some(state) = panel(app, &id) else {
                return false;
            };
            let armed = state.status.element_picker.as_ref().is_some_and(|picker| picker.armed);
            if !state.menu_open && !armed {
                return false;
            }
            state.menu_open = false;
            state.clear_data_open = false;
            if armed {
                action(app, &id, "select_element", json!(false));
            }
            true
        }
    });
    content = content.child(page_area);
    if logs_open {
        content = content.child(log_drawer(app, session));
    }
    // What floats over the page. A native page paints over all of this, so every one of them is counted
    // by `measure_covered` and takes the page down while it is up.
    if let Some(error) = error {
        content = content.child(strip(("browser-error", session.to_owned()), error, t_.critical, t_.critical_bg, 4.0).role(Role::Alert));
    } else if let Some(notice) = notice {
        content = content.child(strip(("browser-notice", session.to_owned()), notice, t_.text_primary, t_.flyout, 4.0).role(Role::Status));
    }
    if handback {
        let label: String = match &handback_tool {
            Some(tool) => tf("模型请求使用这个页面，要用 {tool}", "The model is asking for this page to use {tool}", &[("tool", tool)]),
            None => t("模型请求使用这个页面", "The model is asking for this page").to_owned(),
        };
        let (keep, give) = (id.clone(), id.clone());
        content = content.child(
            row()
                .key(("browser-handback", session))
                .role(Role::Status)
                .absolute()
                .left(4.0)
                .right(4.0)
                .bottom(4.0)
                .min_h(34.0)
                .wrap()
                .items_center()
                .gap(6.0)
                .pl(10.0)
                .pr(6.0)
                .py(5.0)
                .radius(9.0)
                .bg(t_.flyout)
                .border(t_.flyout_stroke)
                .shadow(14.0, 4.0, t_.shadow)
                .child(text(label).ty(BODY_SMALL).flex1().min_w(0.0))
                .child(card_action(t("继续使用", "Keep using"), pending).on_click(move |app: &mut App| action(app, &keep, "take_control", Value::Null)))
                .child(accent_button(t("交还给模型", "Give it back")).h(27.0).px(11.0).disabled(pending).on_click(move |app: &mut App| action(app, &give, "handoff_agent", Value::Null))),
        );
    }
    if let Some(dialog) = dialog {
        content = content.child(dialog_card(app, session, dialog, pending));
    }
    if annotating {
        content = content.child(annotate_layer(app, session));
    }
    content
}

fn strip(key: (&'static str, String), message: String, foreground: Color, background: Color, top: f32) -> E {
    row().key(key).absolute().top(top).left(4.0).right(4.0).min_h(34.0).items_center().px(9.0).py(6.0).radius(8.0).bg(background).border(theme().card_stroke).shadow(14.0, 4.0, theme().shadow).clip().child(text(message).ty(BODY_SMALL).color(foreground).selectable().flex1().min_w(0.0))
}

/// An alert, confirm or prompt on a page the user is using, answered here rather than left for the model.
fn dialog_card(app: &mut App, session: &str, dialog: BrowserPageDialog, pending: bool) -> E {
    let t_ = theme();
    let Some(state) = panel(app, session) else {
        return div();
    };
    if state.dialog_text.as_ref().map(|(id, _)| *id) != Some(dialog.id) {
        state.dialog_text = Some((dialog.id, dialog.default_value.clone().unwrap_or_default()));
        focus_request(("field", ("browser-dialog-input", session)));
    }
    let answer_text = state.dialog_text.as_ref().map_or_else(String::new, |(_, text)| text.clone());
    let leaving = dialog.kind == "beforeunload";
    let is_prompt = dialog.kind == "prompt";
    let message = if leaving && dialog.message.trim().is_empty() { t("离开这个页面？你所做的更改可能不会保存。", "Leave this page? Changes you made may not be saved.").to_owned() } else { dialog.message.clone() };
    let id = session.to_owned();
    let dialog_id = dialog.id;
    let answer = move |app: &mut App, session: &str, accept: bool, text: Option<String>| {
        action(app, session, "dialog", json!({ "id": dialog_id, "accept": accept, "text": text }));
    };
    let mut card = col()
        .key(("browser-dialog", session))
        .role(Role::AlertDialog)
        .aria_label(t("页面对话框", "Page dialog"))
        .absolute()
        .top(10.0)
        .self_center()
        .w(360.0)
        .max_w(360.0)
        .gap(8.0)
        .p(12.0)
        .radius(12.0)
        .bg(t_.flyout)
        .border(t_.flyout_stroke)
        .shadow(20.0, 8.0, t_.shadow)
        .child(text(t("这个页面显示", "This page says")).ty(CAPTION).color(t_.text_secondary));
    if !message.is_empty() {
        card = card.child(col().max_h(240.0).scroll_y().key(("browser-dialog-message", session)).child(text(message).ty(BODY_SMALL).selectable()));
    }
    if is_prompt {
        let (change, submit) = (id.clone(), id.clone());
        card = card.child(
            field(("browser-dialog-input", session), answer_text.clone())
                .on_change(move |app: &mut App, text: String| {
                    if let Some(state) = panel(app, &change) {
                        if let Some((_, current)) = state.dialog_text.as_mut() {
                            *current = text;
                        }
                    }
                })
                .on_submit(move |app: &mut App, text: String| answer(app, &submit, true, Some(text)))
                .build()
                .aria_label(t("回答", "Answer")),
        );
    }
    let mut actions = row().justify_end().gap(6.0);
    if dialog.kind != "alert" {
        let id = id.clone();
        let label = if leaving { t("留下", "Stay") } else { t("取消", "Cancel") };
        actions = actions.child(card_action(label, pending).on_click(move |app: &mut App| answer(app, &id, false, None)));
    }
    let label = if leaving { t("离开", "Leave") } else { t("确定", "OK") };
    actions = actions.child(accent_button(label).h(27.0).px(11.0).disabled(pending).on_click(move |app: &mut App| {
        let text = is_prompt.then(|| panel(app, &id).and_then(|state| state.dialog_text.as_ref().map(|(_, text)| text.clone())).unwrap_or_default());
        answer(app, &id, true, text);
    }));
    card.child(actions)
}

fn annotate_layer(app: &mut App, session: &str) -> E {
    let Some(state) = panel(app, session) else {
        return div();
    };
    let backdrop = state.backdrop.clone();
    let (cancel, attach) = (session.to_owned(), session.to_owned());
    // `.browser-panel__sketch`: the box `measure_covered` counts as lying over the page.
    div().key(("browser-sketch", session)).absolute().inset(0.0).child(sketch_overlay(
        app,
        SketchOverlayProps {
            key: sketch_key(session),
            backdrop,
            on_cancel: Rc::new(move |app: &mut App| toggle_annotate(app, &cancel)),
            on_attach: Rc::new(move |app: &mut App, data_url| attach_annotation(app, &attach, data_url)),
            attach_label: None,
        },
    ))
}

fn repaint_later(app: &mut App, wake: Duration) {
    // A timer on a task, because a view may not sleep and an idle window draws nothing by itself.
    let task = app.task();
    std::thread::spawn(move || {
        std::thread::sleep(wake);
        task.post(|_| repaint());
    });
}

// ----- the cards ------------------------------------------------------------------------------------------------

fn welcome(child: Option<E>) -> E {
    col().flex1().min_h(0.0).items_center().justify_center().bg(theme().layer).role(Role::Status).aria_busy(child.is_none()).child(child)
}

/// Standby: nothing to preview and nothing configured to preview it with. One centred line.
pub fn idle_page() -> E {
    welcome(Some(text(t("没有开发服务器", "No dev server")).ty(SUBTITLE).mb(36.0)))
}

fn body_card_frame(child: E) -> E {
    col().flex1().min_h(0.0).items_center().justify_center().p(18.0).bg(theme().layer).role(Role::Status).child(col().w_full().max_w(400.0).items_center().gap(10.0).child(child))
}

fn body_card(app: &mut App, session: &str, body_state: &PreviewBodyState) -> Option<E> {
    let t_ = theme();
    let id = session.to_owned();
    Some(match body_state {
        PreviewBodyState::Page => return None,
        PreviewBodyState::Reading => welcome(None),
        PreviewBodyState::NoConfig => idle_page(),
        PreviewBodyState::StartPage => start_page(app, session),
        PreviewBodyState::Starting => body_card_frame(col().items_center().gap(10.0).child(spinner(22.0)).child(text(t("正在启动服务器", "Starting server")).ty(BODY_STRONG))),
        PreviewBodyState::StartFailed { name, message, remote } => {
            let copied = panel(app, session).is_some_and(|state| state.copied_until.is_some());
            let retrying = panel(app, session).is_some_and(|state| state.servers.pending_name.as_deref() == Some(name));
            // The host queues the failure for the conversation's model, which hears it at its next round.
            let meta = t("对话的模型会得知这次失败", "The conversation's model is told about this failure");
            let title = if *remote { t("远程屏幕连接失败", "Remote screen failed to connect") } else { t("开发服务器启动失败", "Dev server failed to start") };
            let mut card = col().items_center().gap(10.0).child(icon("AlertTriangle", 30.0)).child(text(title).ty(BODY_STRONG));
            if message.is_empty() {
                card = card.child(text(meta).ty(CAPTION).color(t_.text_secondary).text_center());
            } else {
                let (copy_id, copy_text) = (id.clone(), message.clone());
                card = card.child(
                    row()
                        .w_full()
                        .items_start()
                        .gap(6.0)
                        .p(8.0)
                        .radius(7.0)
                        .bg(t_.card)
                        .border(t_.card_stroke)
                        .child(spans(vec![span(format!("{meta} · "), CAPTION), span(message.clone(), CAPTION)]).selectable().flex1().min_w(0.0))
                        .child(icon_button(if copied { "Check" } else { "Copy" }, if copied { t("已复制", "Copied") } else { t("复制错误日志", "Copy error log") }).on_click(move |app: &mut App| {
                            let copied = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(copy_text.clone())).is_ok();
                            if let Some(state) = panel(app, &copy_id) {
                                state.copied_until = copied.then(|| Instant::now() + COPY_FLASH);
                            }
                        })),
                );
            }
            let (retry_id, retry_name) = (id.clone(), name.clone());
            card = card.child(card_action(t("重试", "Try again"), retrying).when(retrying, |el| el.child(spinner(12.0))).on_click(move |app: &mut App| run_server(app, &retry_id, retry_name.clone())));
            body_card_frame(card)
        }
        PreviewBodyState::Stopped { label, remote, reason } => {
            let restart_pending = panel(app, session).is_some_and(|state| state.servers.pending_name.is_some());
            let restart = t("重启", "Restart");
            let footnote = if *remote { t("远程屏幕已断开。重新连接，或者关闭预览。", "The remote screen disconnected. Reconnect it, or close the preview.") } else { t("开发服务器已停止。重启它，或者关闭预览。", "The dev server stopped. Restart it, or close the preview.") };
            let restart_id = id.clone();
            let restart_button: E = if restart_pending { server_tag(spinner(12.0)) } else { server_tag(text(restart).ty(CAPTION)) };
            let mut card = col()
                .w_full()
                .gap(8.0)
                .child(
                    row()
                        .w_full()
                        .items_center()
                        .gap(7.0)
                        .p(9.0)
                        .radius(7.0)
                        .bg(t_.card)
                        .border(t_.card_stroke)
                        .child(icon("Server", 14.0))
                        .child(text(label.clone()).ty(BODY_SMALL_STRONG).ellipsis().flex1().min_w(0.0))
                        .child(text(t("已停止", "Stopped")).ty(CAPTION).color(t_.text_tertiary))
                        .child(restart_button.key(("browser-restart", id.clone())).disabled(restart_pending).tooltip(format!("{restart} {label}")).aria_label(format!("{restart} {label}")).on_click(move |app: &mut App| {
                            let name = panel(app, &restart_id).and_then(|state| {
                                let name = state.servers.stopped.as_ref().map(|stopped| stopped.name.clone());
                                state.servers.dismiss_stopped();
                                name
                            });
                            if let Some(name) = name {
                                run_server(app, &restart_id, name);
                            }
                        })),
                )
                .child(text(footnote).ty(CAPTION).color(t_.text_secondary));
            if let Some(reason) = reason {
                card = card.child(text(reason.clone()).ty(CAPTION).color(t_.text_secondary).selectable());
            }
            body_card_frame(card)
        }
    })
}

/// Configurations exist, none of them is answering yet.
fn start_page(app: &mut App, session: &str) -> E {
    let t_ = theme();
    let id = session.to_owned();
    let Some(state) = panel(app, session) else {
        return div();
    };
    let rows = state.servers.rows();
    let pending_name = state.servers.pending_name.clone();
    let expanded = state.start_page_expanded;
    let shown = if expanded { rows.len() } else { rows.len().min(START_PAGE_LIMIT) };
    let mut list = col().w_full().min_w(0.0).max_w(400.0).radius(8.0).border(t_.card_stroke).bg(t_.card).clip().role(Role::List);
    for (index, row_data) in rows.iter().take(shown).enumerate() {
        let stoppable = row_data.running || row_data.starting;
        let busy = row_data.starting || pending_name.as_deref() == Some(&row_data.name);
        let key = format!("{}:{}", row_data.name, row_data.server.as_ref().map_or("config", |server| server.handle.as_str()));
        let open = {
            let id = id.clone();
            let clicked = row_data.clone();
            let openable = preview_row_openable(row_data);
            row()
                .key(("browser-start-open", id.clone(), key.clone()))
                .flex1()
                .min_w(0.0)
                .gap(7.0)
                .items_center()
                .disabled(!openable)
                .tooltip(tf("打开 {name}", "Open {name}", &[("name", &row_data.name)]))
                .aria_label(tf("打开 {name}", "Open {name}", &[("name", &row_data.name)]))
                .on_click(move |app: &mut App| open_row(app, &id, &clicked))
                .child(platform_icon(row_data, 14.0))
                .child(text(row_data.name.clone()).ty(BODY_SMALL_STRONG).ellipsis().flex1().min_w(0.0))
                .child(text(preview_row_detail(row_data)).ty(CAPTION).color(if row_data.failed { t_.critical } else { t_.text_tertiary }).shrink0())
        };
        // No action for an attach row: there is nothing to run, and nothing to stop.
        let action_button = (!row_data.attach).then(|| {
            let id = id.clone();
            let clicked = row_data.clone();
            let others_pending = pending_name.as_deref().is_some_and(|name| name != row_data.name);
            let content: E = if busy { spinner(12.0) } else if stoppable { text(t("停止", "Stop")).ty(CAPTION).color(t_.critical) } else { text(t("运行", "Run")).ty(CAPTION) };
            let tip: Arc<str> = Arc::from(if stoppable { tf("停止 {name}", "Stop {name}", &[("name", &row_data.name)]) } else { tf("运行 {name}", "Run {name}", &[("name", &row_data.name)]) });
            server_tag(content).key(("browser-start-action", id.clone(), key.clone())).disabled(others_pending).tooltip(tip.clone()).aria_label(tip).on_click(move |app: &mut App| {
                if stoppable {
                    stop_server(app, &id, &clicked);
                } else {
                    run_server(app, &id, clicked.name.clone());
                }
            })
        });
        let mut item = col().px(9.0).py(7.0).gap(4.0).hover_bg(t_.subtle_hover).role(Role::ListItem).aria_busy(busy).when(index + 1 < shown, |el| el.border_bottom(t_.card_stroke)).child(row().items_center().gap(7.0).child(open).child(action_button));
        // The row says that it failed; this says why. A device that wouldn't connect is otherwise a red
        // word with nothing to act on.
        if let Some(failure) = preview_row_failure(row_data) {
            item = item.child(text(failure.clone()).ty(CAPTION).color(t_.critical).ellipsis().tooltip(failure));
        }
        list = list.child(item);
    }
    let mut card = col().flex1().min_h(0.0).items_center().justify_center().p(18.0).gap(8.0).bg(t_.layer).role(Role::Status).child(list);
    if shown < rows.len() {
        let id = id.clone();
        card = card.child(subtle_button(t("查看全部", "See all")).h(22.0).px(8.0).key(("browser-see-all", id.clone())).on_click(move |app: &mut App| {
            if let Some(state) = panel(app, &id) {
                state.start_page_expanded = true;
            }
        }));
    }
    card
}

/// A terminal's output, in a terminal's face.
const LOG_TY: TypeStyle = TypeStyle { mono: true, ..CAPTION };

/// The dev-server output, docked below the page rather than over it. The grip drags with capture, so a
/// pointer that leaves the strip mid-drag keeps resizing instead of dropping the gesture on the page.
fn log_drawer(app: &mut App, session: &str) -> E {
    let t_ = theme();
    let id = session.to_owned();
    let Some(state) = panel(app, session) else {
        return div();
    };
    let height = state.drawer_height;
    let lines = state.log_lines.clone();
    let server_name = state.servers.log_row().map(|row| row.name);
    let mut body = col().key(("browser-log-body", session)).flex1().min_h(0.0).scroll_y().scroll_stick().p(12.0);
    if lines.is_empty() {
        body = body.child(text(log_empty_text(server_name.as_deref())).ty(LOG_TY).color(t_.text_tertiary).text_center().w_full());
    } else {
        for line in lines {
            let color = match preview_log_severity(&line) {
                Some(PreviewLogSeverity::Error) => t_.critical,
                Some(PreviewLogSeverity::Warn) => t_.caution,
                None => t_.text_secondary,
            };
            body = body.child(text(line).ty(LOG_TY).color(color).selectable());
        }
    }
    let (grip, close) = (id.clone(), id.clone());
    // `max-height: 60%`: a drawer dragged past that would leave the page no room.
    let height = rect_of(("browser-panel", session)).map_or(height, |pane| height.min(pane.h * 0.6));
    col()
        .key(("browser-log-drawer", session))
        .shrink0()
        .h(height)
        .border_top(t_.divider)
        .bg(t_.flyout)
        .child(body)
        .child(
            div()
                .key(("browser-log-grip", session))
                .absolute()
                .top(0.0)
                .left(0.0)
                .right(0.0)
                .h(8.0)
                .cursor(CursorKind::RowResize)
                .role(Role::Splitter)
                .aria_orientation(Orientation::Horizontal)
                .on_pointer(move |app: &mut App, event: PointerEvent| {
                    let Some(state) = panel(app, &grip) else {
                        return;
                    };
                    match event.phase {
                        Phase::Down => state.drawer_drag_start = state.drawer_height,
                        Phase::Move => state.drawer_height = drawer_height_after(state.drawer_drag_start, event.dy),
                        Phase::Up => {}
                    }
                }),
        )
        .child(
            icon_button("X", t("关闭开发服务器日志", "Close dev server logs"))
                .key(("browser-log-close", session))
                .size(22.0, 22.0)
                .absolute()
                .top(4.0)
                .right(4.0)
                .on_click(move |app: &mut App| {
                    if let Some(state) = panel(app, &close) {
                        state.logs_open = false;
                    }
                }),
        )
}

/// The drawer's height after dragging its grip by `dy`: down shrinks it, and it never goes away.
pub fn drawer_height_after(start: f32, dy: f32) -> f32 {
    (start - dy).max(DRAWER_MIN_HEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragging_the_grip_up_grows_the_drawer_down_to_its_floor() {
        assert_eq!(drawer_height_after(200.0, -50.0), 250.0);
        assert_eq!(drawer_height_after(200.0, 50.0), 150.0);
        assert_eq!(drawer_height_after(200.0, 500.0), DRAWER_MIN_HEIGHT);
    }
}
