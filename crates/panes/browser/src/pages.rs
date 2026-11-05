//! A page's native lifecycle: presenting, hiding, closing and placing it, from upstream's `App.tsx`.
//!
//! The roster of which pages a conversation has is the shell's (`lib/sidePanes.ts`); this is everything
//! between a decision and the host. Every transition claims an intent epoch before it does anything, and
//! each continuation checks that epoch is still the newest for its session before it commits.

use std::collections::HashMap;
use std::time::Duration;

use mewrk_client::prelude::*;
use serde_json::Value;

use crate::browser::*;
use crate::controller::{BrowserController, BrowserIntentDesired};
use crate::preview::*;

/// How long a close waits for an older open to come back before it fences it anyway.
const PENDING_OPEN_LIMIT: Duration = Duration::from_secs(20);

/// How often the client says it is alive. The host hides every page after 12 seconds of silence.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(4);

#[derive(Default)]
struct PageRuntime {
    attached: bool,
    /// One bounds request in flight per page, and only the newest rectangle waiting behind it. A dragged
    /// divider reports a rectangle every frame; sent as they come, requests that each take longer than a
    /// frame queue up and the page falls further behind the pane the longer the drag goes on.
    bounds: HashMap<String, BoundsQueue>,
}

#[derive(Default)]
struct BoundsQueue {
    busy: bool,
    next: Option<(BrowserPanelBounds, u64)>,
    last: Option<BrowserPanelBounds>,
}

fn controller(app: &mut App) -> &mut BrowserController {
    app.local::<BrowserController>()
}

/// Where pages go, and the renderer's proof of life. Once the window exists; idempotent, and views call
/// it too, so a page the model opens before any pane was drawn still has a parent if the shell calls it
/// at startup.
pub fn attach_window(app: &mut App) {
    tauri::webview::attach_main_view(app.native_view);
    if app.native_view.is_null() || std::mem::replace(&mut app.local::<PageRuntime>().attached, true) {
        return;
    }
    app.spawn(|task| loop {
        std::thread::sleep(HEARTBEAT_INTERVAL);
        if let Err(error) = task.invoke::<Value>("browser_renderer_mount_heartbeat", json!({})) {
            eprintln!("browser_renderer_mount_heartbeat: {error}");
        }
    });
}

/// `openBuiltInBrowser`. `done` hears whether the page is now open.
pub fn open_built_in_browser(app: &mut App, session_id: &str, intent_epoch: u64, url: Option<String>, done: impl FnOnce(&mut App, Result<bool, String>) + Send + 'static) {
    attach_window(app);
    let session = session_id.to_owned();
    {
        let controller = controller(app);
        if controller.close_in_flight(&session) || !controller.intent_is_current(&session, intent_epoch, BrowserIntentDesired::Open) {
            return app.post(move |app| done(app, Ok(false)));
        }
        controller.open_started(&session);
    }
    let args = open_args(&session, url.as_deref(), intent_epoch);
    app.invoke::<BrowserStatus>("browser_open", args, move |app, result| {
        let controller = controller(app);
        controller.open_settled(&session);
        let current = controller.intent_is_current(&session, intent_epoch, BrowserIntentDesired::Open);
        match result {
            Ok(status) => {
                let open = status.open;
                if current && controller.visible_session() == Some(session.as_str()) {
                    controller.runtime_ready = open;
                    controller.statuses.insert(session.clone(), status);
                }
                done(app, Ok(current && open));
            }
            Err(error) => {
                if current {
                    controller.clear_visible_session_if(&session);
                    controller.runtime_ready = false;
                }
                done(app, Err(error));
            }
        }
    });
}

/// Presents `session_id`: claims the Open intent, publishes where the page goes, then opens it.
pub fn present_page(app: &mut App, session_id: &str, url: Option<String>, done: impl FnOnce(&mut App, Result<bool, String>) + Send + 'static) {
    let session = session_id.to_owned();
    let epoch = {
        let controller = controller(app);
        controller.set_visible_session(Some(&session));
        controller.issue_intent(&session, BrowserIntentDesired::Open)
    };
    open_built_in_browser(app, &session, epoch, url, done);
}

/// `hideBuiltInBrowser`: hides whichever page is presented, retrying once with the same epoch.
pub fn hide_built_in_browser(app: &mut App) {
    let (session, epoch) = {
        let controller = controller(app);
        controller.runtime_ready = false;
        let Some(session) = controller.take_visible_session() else {
            return;
        };
        // A structured close may retain its tab while native cleanup is pending. The accepted Closed
        // intent already hid the surface; never reinterpret that generation as Hidden.
        if controller.current_intent(&session).is_some_and(|intent| intent.desired == BrowserIntentDesired::Closed) {
            return;
        }
        let epoch = controller.issue_intent(&session, BrowserIntentDesired::Hidden);
        (session, epoch)
    };
    app.spawn(move |task| {
        let args = lifecycle_action_args(&session, "hide", epoch);
        // Hidden is an idempotent native compensation point: a transient failure retries the same epoch.
        let status = task.invoke::<BrowserStatus>("browser_action", args.clone()).or_else(|_| task.invoke::<BrowserStatus>("browser_action", args));
        // A failed hide is non-fatal; the next intent reconciles the surface with its own epoch.
        if let Ok(status) = status {
            task.post(move |app| {
                let controller = controller(app);
                if controller.intent_is_current(&session, epoch, BrowserIntentDesired::Hidden) {
                    controller.statuses.insert(session, status);
                }
            });
        }
    });
}

/// `requestBrowserSessionClose`. A second caller while one is under way shares its outcome by not
/// starting another. `done` hears only the starter's result.
pub fn request_browser_session_close(app: &mut App, session_id: &str, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
    let session = session_id.to_owned();
    // Declared before any competing open intent can be issued.
    let (previous, first_epoch, had_pending) = {
        let controller = controller(app);
        if !controller.begin_close(&session) {
            return app.post(move |app| done(app, Ok(())));
        }
        let previous = controller.current_intent(&session);
        let epoch = controller.issue_intent(&session, BrowserIntentDesired::Closed);
        (previous, epoch, controller.pending_opens(&session) > 0)
    };
    app.spawn(move |task| {
        let result = close_on_task(&task, &session, first_epoch, previous, had_pending);
        task.post(move |app| {
            let controller = controller(app);
            controller.end_close(&session);
            if result.is_ok() && controller.current_intent(&session).is_some_and(|intent| intent.desired == BrowserIntentDesired::Closed) {
                controller.forget_page(&session);
                crate::panel::page_closed(app, &session);
            }
            done(app, result);
        });
    });
}

fn close_on_task(task: &Task, session: &str, first_epoch: u64, previous: Option<crate::controller::BrowserIntent>, had_pending: bool) -> Result<(), String> {
    let owned = session.to_owned();
    let is_current = |epoch: u64| {
        let owned = owned.clone();
        task.update(move |app| controller(app).intent_is_current(&owned, epoch, BrowserIntentDesired::Closed))
    };
    let mut epoch = first_epoch;
    let mut disposition = task.invoke::<BrowserCloseDisposition>("browser_close", close_args(session, epoch))?;
    let collided = matches!(disposition.error_code.as_deref(), Some("staleIntent" | "intentCollision"));
    if !disposition.intent_accepted && collided && is_current(epoch) {
        // The host has already seen a later generation. A rejected disposition is explicitly free of side
        // effects, so retrying once with a fresh epoch can't duplicate anything.
        let again = owned.clone();
        epoch = task.update(move |app| controller(app).issue_intent(&again, BrowserIntentDesired::Closed));
        disposition = task.invoke::<BrowserCloseDisposition>("browser_close", close_args(session, epoch))?;
    }
    if !disposition.intent_accepted {
        if is_current(epoch) {
            let again = owned.clone();
            task.update(move |app| controller(app).restore_intent(&again, previous));
        }
        return Err(disposition.message.unwrap_or_else(|| t("内置浏览器关闭请求未被接受", "The built-in browser close request was not accepted").to_owned()));
    }
    if disposition.surface_hidden && is_current(epoch) {
        task.update(|app| controller(app).runtime_ready = false);
    }
    let not_done = || t("内置浏览器关闭清理尚未完成，请重试", "The built-in browser cleanup is not complete; please retry").to_owned();
    if !disposition.cleanup_complete {
        return Err(disposition.message.unwrap_or_else(not_done));
    }
    if had_pending {
        let start = std::time::Instant::now();
        loop {
            let again = owned.clone();
            let pending = task.update(move |app| controller(app).pending_opens(&again));
            if pending == 0 || start.elapsed() > PENDING_OPEN_LIMIT {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // An older open may have completed after the first close: this makes the intent terminal.
        disposition = task.invoke::<BrowserCloseDisposition>("browser_close", close_args(session, epoch))?;
        if !disposition.intent_accepted || !disposition.cleanup_complete {
            return Err(disposition.message.unwrap_or_else(not_done));
        }
    }
    if is_current(epoch) {
        let again = owned.clone();
        task.update(move |app| {
            let controller = controller(app);
            controller.clear_visible_session_if(&again);
        });
    }
    Ok(())
}

/// The page rectangle the host is given for a measured pane body. The page only reaches the pane's
/// rounded bottom when the log drawer isn't taking the bottom from it.
pub fn page_bounds(body: Rect, reserved_bottom: f32, bottom_corner_radius: f32) -> BrowserPanelBounds {
    BrowserPanelBounds {
        x: body.x as f64,
        y: body.y as f64,
        width: body.w as f64,
        height: (body.h - reserved_bottom).max(1.0) as f64,
        visible: true,
        occluded_top: None,
        bottom_corner_radius: Some(if reserved_bottom > 0.0 { 0.0 } else { bottom_corner_radius as f64 }),
    }
}

/// `onContentBoundsChange`. Runs from the view; only a change reaches the host. `reserved_bottom` is what
/// the log drawer takes from `body`, when `body` is measured with the drawer in it.
pub fn publish_bounds(app: &mut App, session_id: &str, body: Rect, reserved_bottom: f32, bottom_corner_radius: f32) {
    if body.w < 1.0 || body.h < 1.0 {
        return;
    }
    let Some(intent) = controller(app).current_intent(session_id).filter(|intent| intent.desired == BrowserIntentDesired::Open) else {
        return;
    };
    // The page is the window's child, and the window's units know nothing of the app's zoom.
    let zoom = app.window_zoom;
    let runtime = app.local::<PageRuntime>();
    let mut bounds = page_bounds(body, reserved_bottom, bottom_corner_radius);
    bounds.x *= f64::from(zoom);
    bounds.y *= f64::from(zoom);
    bounds.width *= f64::from(zoom);
    bounds.height *= f64::from(zoom);
    let queue = runtime.bounds.entry(session_id.to_owned()).or_default();
    if queue.last == Some(bounds) {
        return;
    }
    queue.last = Some(bounds);
    if queue.busy {
        queue.next = Some((bounds, intent.epoch));
        return;
    }
    queue.busy = true;
    send_bounds(app, session_id.to_owned(), bounds, intent.epoch);
}

fn send_bounds(app: &mut App, session: String, bounds: BrowserPanelBounds, epoch: u64) {
    let args = bounds_args(&session, &bounds, epoch);
    app.invoke::<Value>("browser_set_panel_bounds", args, move |app, _| {
        let next = app.local::<PageRuntime>().bounds.get_mut(&session).and_then(|queue| {
            let next = queue.next.take();
            queue.busy = next.is_some();
            next
        });
        if let Some((bounds, epoch)) = next {
            send_bounds(app, session, bounds, epoch);
        }
    });
}

/// A preview page as far as pairing it with a server goes: what it shows, and on which machine.
#[derive(Clone, Debug)]
pub struct OpenPageInfo {
    pub session_id: String,
    pub url: Option<String>,
    /// By environment key, as `server_machine` spells it: a page's `localhost` is its own machine's.
    pub machine: String,
}

/// `closePagesServedAt`: the pages a server that just went away was serving.
pub fn pages_served_by(server: &PreviewServerSnapshot, server_machine: &str, pages: &[OpenPageInfo]) -> Vec<String> {
    pages.iter().filter(|page| page_is_served_by_server(page.url.as_deref(), &page.machine, server, server_machine)).map(|page| page.session_id.clone()).collect()
}

/// `stopServersOfClosedPage`: closing the page a server was serving stops that server, so a tab the user
/// closed doesn't leave a process running behind nothing. Three kinds are left alone: one another page of
/// this conversation still shows, one this conversation doesn't own (an unowned server is the project's,
/// not this tab's), and an attached entry, which has no process of ours and isn't in the list.
pub fn servers_to_stop_for_closed_page<'a>(closed: &OpenPageInfo, owner_id: &str, servers: &'a [PreviewServerSnapshot], remaining_pages: &[OpenPageInfo], machine_of: &dyn Fn(&PreviewServerSnapshot) -> String) -> Vec<&'a PreviewServerSnapshot> {
    if closed.url.is_none() {
        return Vec::new();
    }
    servers
        .iter()
        .filter(|server| {
            let machine = machine_of(server);
            server.session_id.as_deref() == Some(owner_id)
                && page_is_served_by_server(closed.url.as_deref(), &closed.machine, server, &machine)
                && !remaining_pages.iter().any(|page| page.session_id != closed.session_id && page_is_served_by_server(page.url.as_deref(), &page.machine, server, &machine))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(handle: &str, owner: Option<&str>, port: u32) -> PreviewServerSnapshot {
        PreviewServerSnapshot { handle: handle.into(), server_id: "dev".into(), name: "dev".into(), port, status: PreviewServerStatus::Running, started_at: String::new(), cwd: String::new(), session_id: owner.map(str::to_owned), machine: None, url: None, workspace: None, remote: None }
    }

    fn viewer(handle: &str, owner: &str, url: &str) -> PreviewServerSnapshot {
        PreviewServerSnapshot {
            url: Some(url.to_owned()),
            port: 0,
            remote: Some(PreviewRemoteControl { protocol: RemoteControlProtocol::Vnc, platform: RemoteControlPlatform::Desktop, target: "mac".into(), target_display: None, label: None, state: RemoteControlState::Ready, read_only: false, launch: None, message: None }),
            ..server(handle, Some(owner), 0)
        }
    }

    fn page(session: &str, url: &str) -> OpenPageInfo {
        OpenPageInfo { session_id: session.into(), url: Some(url.into()), machine: "local".into() }
    }

    fn local(_: &PreviewServerSnapshot) -> String {
        "local".to_owned()
    }

    fn handles(servers: Vec<&PreviewServerSnapshot>) -> Vec<&str> {
        servers.into_iter().map(|server| server.handle.as_str()).collect()
    }

    #[test]
    fn stops_the_conversations_server_the_closed_page_was_showing() {
        let servers = [server("h", Some("conv"), 5173)];
        let closed = page("tab", "http://localhost:5173/app");
        assert_eq!(handles(servers_to_stop_for_closed_page(&closed, "conv", &servers, &[], &local)), ["h"]);
    }

    #[test]
    fn leaves_the_server_running_while_another_page_still_shows_it() {
        let servers = [server("h", Some("conv"), 5173)];
        let closed = page("tab", "http://localhost:5173/a");
        let other = page("other", "http://localhost:5173/b");
        assert!(servers_to_stop_for_closed_page(&closed, "conv", &servers, &[other], &local).is_empty());
    }

    #[test]
    fn leaves_a_server_this_conversation_does_not_own() {
        let servers = [server("mine", None, 5173), server("theirs", Some("other"), 5173)];
        let closed = page("tab", "http://localhost:5173/");
        assert!(servers_to_stop_for_closed_page(&closed, "conv", &servers, &[], &local).is_empty());
    }

    #[test]
    fn stops_the_device_whose_tab_closed_not_the_one_sharing_its_loopback_origin() {
        let servers = [viewer("old", "conv", "http://127.0.0.1:9/old/"), viewer("new", "conv", "http://127.0.0.1:9/new/")];
        let closed = page("tab", "http://127.0.0.1:9/new/");
        assert_eq!(handles(servers_to_stop_for_closed_page(&closed, "conv", &servers, &[], &local)), ["new"]);
    }

    #[test]
    fn the_same_address_on_another_machine_is_another_server() {
        let servers = [server("h", Some("conv"), 5173)];
        let mut closed = page("tab", "http://localhost:5173/");
        closed.machine = "ssh:devbox".into();
        assert!(servers_to_stop_for_closed_page(&closed, "conv", &servers, &[], &local).is_empty());
    }

    #[test]
    fn a_page_showing_nothing_stops_nothing() {
        let servers = [server("h", Some("conv"), 5173)];
        let closed = OpenPageInfo { session_id: "tab".into(), url: None, machine: "local".into() };
        assert!(servers_to_stop_for_closed_page(&closed, "conv", &servers, &[], &local).is_empty());
    }

    #[test]
    fn a_server_that_went_away_takes_its_pages_by_the_same_rule() {
        let dev = server("h", Some("conv"), 5173);
        let pages = [page("a", "http://localhost:5173/"), page("b", "http://localhost:4000/"), page("c", "http://localhost:5173/x")];
        assert_eq!(pages_served_by(&dev, "local", &pages), ["a", "c"]);
        assert!(pages_served_by(&dev, "ssh:devbox", &pages).is_empty());
    }

    #[test]
    fn the_page_gives_up_what_the_log_drawer_takes() {
        let body = rect(10.0, 20.0, 300.0, 400.0);
        let free = page_bounds(body, 0.0, 8.0);
        assert_eq!((free.x, free.y, free.width, free.height), (10.0, 20.0, 300.0, 400.0));
        assert!(free.visible);
        assert_eq!(free.bottom_corner_radius, Some(8.0));
        let docked = page_bounds(body, 150.0, 8.0);
        assert_eq!(docked.height, 250.0);
        assert_eq!(docked.bottom_corner_radius, Some(0.0), "the page no longer reaches the pane's rounded bottom");
        assert_eq!(page_bounds(body, 900.0, 8.0).height, 1.0, "never nothing");
    }
}
