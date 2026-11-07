//! The window's content: what `App.tsx` renders.

use fluent::Theme;
use mewrk_client::prelude::*;
use mewrk_client::side_panes::{side_pane_layout_for, SidePanesAction};

use crate::chrome::{shell_nav, TILE_RADIUS, TOPBAR_HEIGHT};
use crate::effects;
use crate::pane_tiles::{pane_tiles, PaneTilesProps, ResizeChange};
use crate::panes::{fork_tray_view, keep_terminal_mounted, render_pane, PaneContext};
use crate::sidebar::sidebar;
use crate::state::shell;
use crate::topbar::topbar;

pub fn init(app: &mut App) {
    effects::init(app);
}

/// Dark or light, from the user's theme preference or the system, with the user's accent and code theme on it.
pub fn theme(app: &App, forced: Option<bool>) -> Theme {
    use mewrk_client::model::ThemePreference;
    let dark = forced.unwrap_or(match app.doc.global_settings.theme {
        ThemePreference::Day => false,
        ThemePreference::Night => true,
        ThemePreference::System => app.system_dark,
    });
    mewrk_settings_global::themed(if dark { Theme::dark() } else { Theme::light() }, &app.doc.global_settings.appearance)
}

/// Whether the keyboard is with a terminal, where Option is Meta rather than a way to type accents, and where a plain letter
/// is the shell's text alone. A panel's widget id is `hash_key(("terminal-panel", session key))`.
pub fn is_terminal_focus(app: &mut App, id: u64) -> bool {
    id != 0 && mewrk_pane_terminal::store(app).controller.current().keys().any(|key| hash_key(("terminal-panel", key.as_str())) == id)
}

thread_local! {
    static CLIPBOARD: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// A text the next frame hands the clipboard, since only the frame holds it.
pub fn set_clipboard(_app: &mut App, text: String) {
    CLIPBOARD.with(|slot| *slot.borrow_mut() = Some(text));
    repaint();
}

fn clipboard_leaf() -> E {
    custom::<App>(|_, ui: &mut Ui<'_>, _| {
        if let Some(text) = CLIPBOARD.with(|slot| slot.borrow_mut().take()) {
            ui.clipboard.set(&text);
        }
    })
    .size(0.0, 0.0)
    .absolute()
    .pass_through()
}

/// Where `feedback.css`'s narrow layout begins.
pub const SIDEBAR_FLOATS_AT: f32 = 860.0;

/// `app-shell`.
pub fn root(app: &mut App) -> E {
    effects::frame(app);
    crate::glass::begin_frame(app);
    mewrk_ui_common::markdown::begin_frame(app);
    let (window_w, window_h) = window_size();
    let sidebar_open = !shell(app).sidebar_closed;
    let sidebar_width = app.sidebar_width() as f32;

    // `@media (max-width: 860px)`: the sidebar leaves the flow and floats over the tiles, which run to the window's left edge
    // under it whether it is open or not.
    let floating = window_w <= SIDEBAR_FLOATS_AT;
    let beside = sidebar_open && !floating;
    let main_w = if beside { window_w - sidebar_width } else { window_w };
    let main = main_pane(app, sidebar_open, beside, (main_w, window_h));
    let mut shell_row: E = row().key("app-shell").absolute().inset(0.0);
    let sidebar_box = rect(0.0, 0.0, sidebar_width, window_h);
    let sidebar_corners = [0.0, TILE_RADIUS, TILE_RADIUS, 0.0];
    if beside {
        crate::glass::surface(sidebar_box, sidebar_corners);
        shell_row = shell_row.child(sidebar(app, sidebar_width));
    } else if !sidebar_open {
        crate::sidebar::keep_state_while_closed();
    }
    shell_row = shell_row.child(main);
    if sidebar_open && floating {
        shell_row = shell_row.child(crate::glass::plate_over_tiles(app, sidebar_box, sidebar_corners));
        shell_row = shell_row.child(sidebar(app, sidebar_width).absolute().left(0.0).top(0.0));
    }

    let mut frame: E = div().key("window-frame").w_full().h_full().bg(if crate::glass::layers().layered() { crate::pane_tiles::solid_gutter() } else { crate::pane_tiles::chat_ground() });
    frame = frame.child(mewrk_settings_global::window_backdrop(app));
    frame = frame.child(crate::glass::under_shell(app, rect(window_w - main_w, 0.0, main_w, TOPBAR_HEIGHT), beside));
    frame = frame.child(shell_row);
    frame = frame.child(shell_nav(app, sidebar_open, sidebar_width));
    frame = frame.child(fork_tray_view(app));
    frame = frame.child(crate::dialogs::path_choice::path_choice_menu(app));
    frame = frame.children(crate::dialogs::windows(app));
    frame = frame.child(crate::chrome::window_caption(app));
    // `is-resizing` on the body: a drag keeps its cursor wherever the pointer strays. The handle holds the capture, so this
    // covering the window takes nothing from it.
    let dragging = if shell(app).sidebar_resizing {
        Some(CursorKind::ColResize)
    } else if sidebar_open && crate::sidebar::sort_drag_active() {
        Some(CursorKind::Grabbing)
    } else {
        shell(app).pane_resizing
    };
    if let Some(cursor) = dragging {
        frame = frame.child(div().key("resize-cursor").absolute().inset(0.0).cursor(cursor));
    }
    frame = frame.child(clipboard_leaf());
    // `shouldSuppressForFocus`: a field or a terminal is typed into, so unmodified combinations yield to it. A focused row is not.
    frame.on_key_global(|app: &mut App, press| {
        let typing = text_input_focused() || is_terminal_focus(app, focused_id());
        crate::shortcuts::handle_key(app, press, typing)
    })
}

/// `main-pane`. `beside` is the sidebar taking its own room, as opposed to floating over this or being closed.
fn main_pane(app: &mut App, sidebar_open: bool, beside: bool, size: (f32, f32)) -> E {
    let top = topbar(app, sidebar_open);
    let mut main: E = col().key("main-pane").role(Role::Main).flex1().min_w(0.0).h_full().child(top);
    // A workspace is optional for entering a draft; sending uses the temporary workspace. Panes that need a real workspace
    // remain individually guarded.
    let Some(cx) = PaneContext::of(app) else { return main };
    if !app.with_active_conversation(|_, conversation| conversation.is_some()) {
        return main;
    }
    let mut terminal = Some(keep_terminal_mounted(app, &cx));
    let mut panes: Vec<(String, E)> = Vec::new();
    for pane in cx.layout.panes.clone() {
        if let Some(node) = render_pane(app, &cx, &pane, &mut terminal) {
            panes.push((pane, node));
        }
    }
    let (_, _, agents) = crate::effects::active_subagents(app, &cx.conversation_id);
    let sources = crate::panes::task_sources_of(app, &cx, agents);
    let running_tasks = crate::chat::running_tasks(&sources);
    let chat = crate::chat::chat_view(app, &cx.conversation_id, running_tasks);
    let layout = side_pane_layout_for(app.side_panes(), Some(&cx.conversation_id)).clone();
    let conversation_id = cx.conversation_id.clone();
    let field_left = if beside { 8.0 } else { 0.0 };
    let tiles = pane_tiles(PaneTilesProps {
        conversation_id: conversation_id.clone(),
        field: (size.0, size.1 - TOPBAR_HEIGHT),
        field_at: (window_size().0 - size.0, TOPBAR_HEIGHT),
        field_left,
        chat,
        panes,
        columns: layout.columns,
        side_flex: layout.side_flex,
        column_flex: layout.column_flex,
        pane_flex: layout.pane_flex,
        expanded: layout.expanded,
        on_change: Rc::new(move |app: &mut App, change| {
            let conversation_id = conversation_id.clone();
            let action = match change {
                ResizeChange::SideFlex(side_flex) => SidePanesAction::SetSideFlex { conversation_id, side_flex },
                ResizeChange::ColumnFlex(column_flex) => SidePanesAction::SetColumnFlex { conversation_id, column_flex },
                ResizeChange::PaneFlex(pane_flex) => SidePanesAction::SetPaneFlex { conversation_id, pane_flex },
            };
            app.dispatch_side_panes(action);
            repaint();
        }),
        on_resize_state: Rc::new(|app: &mut App, cursor| {
            shell(app).pane_resizing = cursor;
            repaint();
        }),
    });
    main = main.child(tiles);
    main
}
