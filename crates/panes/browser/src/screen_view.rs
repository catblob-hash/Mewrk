//! A remote screen in the preview pane: upstream's viewer page, without the page.
//!
//! The page shows a phone or a desktop by way of a JPEG encoder, an HTTP server, an image decoder
//! and a whole browser engine. This takes the session's pixels straight to a GPU texture and sends
//! what a person does straight to the session, so a screen costs no renderer process and no
//! encoding, the picture updates as fast as the screen does, and a desktop's blinking cursor moves
//! a few hundred bytes. Everything stops when the view goes, since nothing else asks for frames.

use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use fluent::gpu::{queue_live_image, release_image, write_image, ImageId};
use fluent::Color;
use mewrk_client::prelude::*;
use mewrk_client::screen::{LiveScreen, PointerPhase, RemoteControlSession, ScreenPlatform, ScreenUpdate, SessionPhase, ViewerCredentials, ViewerEvent, ViewerState, Zeroizing};

/// How often the state a page polls for once a second is read. Reading it is a lock.
const STATE_READ: Duration = Duration::from_millis(250);
/// A pointer moves the screen's cursor at most this often; what happens between goes in the next move.
const MOVE_INTERVAL: Duration = Duration::from_millis(16);
/// The most a paste types. A phone's `input text` and a server's clipboard are slow past this.
const PASTE_MAX: usize = 20_000;
/// A wheel's notch in the units each kind of device reports, as the page reckoned them.
const PIXELS_PER_NOTCH: f32 = 100.0;
const LINES_PER_NOTCH: f32 = 48.0;
const MAX_NOTCHES: f64 = 10.0;

/// How long the form waits for an answer to do something before it lets the person try again.
const ANSWER_WAIT: Duration = Duration::from_millis(2500);

/// What the sign-in form shows.
struct SignInForm {
    username: String,
    submitted: bool,
    remember: bool,
}

/// What the sign-in form holds. The password is wiped when it goes.
#[derive(Default)]
struct SignIn {
    key: String,
    username: String,
    password: Zeroizing<String>,
    remember: bool,
    submitted: Option<Instant>,
}

pub(crate) struct ScreenView {
    pub url: String,
    session: Arc<RemoteControlSession>,
    live: Option<Arc<LiveScreen>>,
    /// The texture, and the size of the picture in it.
    image: Option<(ImageId, u32, u32)>,
    input: Sender<Vec<ViewerEvent>>,
    pub state: ViewerState,
    state_read: Instant,
    started: Instant,
    /// A press under way on the picture.
    held: bool,
    last_place: (f64, f64),
    last_move: Option<Instant>,
    last_pointer: Option<(f32, f32)>,
    sign_in: SignIn,
    waker: Arc<dyn Fn() + Send + Sync>,
    /// `MEWRK_TRACE`: what the picture costs, once a second.
    trace: bool,
    traced: (usize, usize, Instant),
}

/// The session's input, on a thread of its own: it waits for the screen, and a phone's `input tap`
/// takes hundreds of milliseconds. Batches gather while one is running, so a drag does not fall
/// behind the finger, and what is held comes up when the view goes.
fn start_input(session: Arc<RemoteControlSession>, viewer: String) -> Sender<Vec<ViewerEvent>> {
    let (sender, receiver) = mpsc::channel::<Vec<ViewerEvent>>();
    std::thread::spawn(move || {
        while let Ok(mut events) = receiver.recv() {
            while let Ok(more) = receiver.try_recv() {
                events.extend(more);
            }
            session.viewer_input(&viewer, coalesced(events));
        }
        session.release_viewer(&viewer);
    });
    sender
}

/// Moves with nothing between them are one move, and wheel notches one turn.
fn coalesced(events: Vec<ViewerEvent>) -> Vec<ViewerEvent> {
    let mut out: Vec<ViewerEvent> = Vec::with_capacity(events.len());
    for event in events {
        match (out.last_mut(), &event) {
            (Some(ViewerEvent::Pointer { phase: PointerPhase::Move, buttons: a, .. }), ViewerEvent::Pointer { phase: PointerPhase::Move, buttons: b, .. }) if a == b => {
                *out.last_mut().expect("just matched") = event;
            }
            (Some(ViewerEvent::Wheel { x, y, dx, dy }), ViewerEvent::Wheel { x: next_x, y: next_y, dx: next_dx, dy: next_dy }) => {
                *x = *next_x;
                *y = *next_y;
                *dx = (*dx + next_dx).clamp(-MAX_NOTCHES, MAX_NOTCHES);
                *dy = (*dy + next_dy).clamp(-MAX_NOTCHES, MAX_NOTCHES);
            }
            _ => out.push(event),
        }
    }
    out
}

/// Where a picture of this size sits in `area`, letterboxed.
fn fit(area: Rect, width: u32, height: u32) -> Rect {
    if width == 0 || height == 0 {
        return area;
    }
    let scale = (area.w / width as f32).min(area.h / height as f32);
    let (w, h) = (width as f32 * scale, height as f32 * scale);
    rect(area.x + (area.w - w) * 0.5, area.y + (area.h - h) * 0.5, w, h)
}

/// A key as `KeyboardEvent.key` and `.code` spell it, which is what the session reads.
fn dom_key(key: Key) -> Option<(String, String)> {
    let named = |name: &str| Some((name.to_owned(), name.to_owned()));
    match key {
        Key::Enter => named("Enter"),
        Key::Backspace => named("Backspace"),
        Key::Delete => named("Delete"),
        Key::Tab => named("Tab"),
        Key::Escape => named("Escape"),
        Key::Home => named("Home"),
        Key::End => named("End"),
        Key::PageUp => named("PageUp"),
        Key::PageDown => named("PageDown"),
        Key::Left => named("ArrowLeft"),
        Key::Right => named("ArrowRight"),
        Key::Up => named("ArrowUp"),
        Key::Down => named("ArrowDown"),
        Key::F(number) => named(&format!("F{number}")),
        Key::Char(' ') => Some((" ".to_owned(), "Space".to_owned())),
        Key::Char(character) => {
            let code = if character.is_ascii_alphabetic() { format!("Key{}", character.to_ascii_uppercase()) } else if character.is_ascii_digit() { format!("Digit{character}") } else { String::new() };
            Some((character.to_string(), code))
        }
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

fn key_event(key: &str, code: &str, down: bool) -> ViewerEvent {
    ViewerEvent::Key { key: key.to_owned(), code: code.to_owned(), down, alt_graph: false }
}

/// One whole press, modifiers around it. There are no key-up events to forward here, so a press goes
/// down and comes up at once.
fn press_events(press: &KeyPress, key: &str, code: &str) -> Vec<ViewerEvent> {
    let modifiers: Vec<(&str, &str)> = [(press.ctrl, "Control", "ControlLeft"), (press.cmd, "Meta", "MetaLeft"), (press.alt, "Alt", "AltLeft"), (press.shift, "Shift", "ShiftLeft")].into_iter().filter(|(on, ..)| *on).map(|(_, name, code)| (name, code)).collect();
    let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
    events.extend(modifiers.iter().map(|(name, code)| key_event(name, code, true)));
    events.push(key_event(key, code, true));
    events.push(key_event(key, code, false));
    events.extend(modifiers.iter().rev().map(|(name, code)| key_event(name, code, false)));
    events
}

impl ScreenView {
    pub fn new(app: &mut App, url: String, session: Arc<RemoteControlSession>) -> Self {
        let state = session.viewer_state();
        let viewer = format!("native-{:x}", fastrand_id());
        let input = start_input(session.clone(), viewer);
        let waker = app.waker();
        let live = session.watch({
            let waker = waker.clone();
            Box::new(move || waker())
        });
        Self { url, session, live: Some(live), image: None, input, state, state_read: Instant::now(), started: Instant::now(), held: false, last_place: (0.0, 0.0), last_move: None, last_pointer: None, sign_in: SignIn { remember: true, ..SignIn::default() }, waker, trace: std::env::var_os("MEWRK_TRACE").is_some(), traced: (0, 0, Instant::now()) }
    }

    /// The window is not being drawn, or the pane is not showing: nothing should be working for a
    /// picture nobody can see. Coming back asks for the whole picture again.
    pub fn set_watching(&mut self, watching: bool) {
        match (watching, &self.live) {
            (true, None) => {
                let waker = self.waker.clone();
                self.live = Some(self.session.watch(Box::new(move || waker())));
            }
            (false, Some(live)) => {
                self.session.unwatch(live);
                self.live = None;
            }
            _ => {}
        }
    }

    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    fn can_input(&self) -> bool {
        self.state.phase == SessionPhase::Ready && !self.state.read_only && !self.state.model_active
    }

    fn send(&self, events: Vec<ViewerEvent>) {
        if !events.is_empty() {
            let _ = self.input.send(events);
        }
    }

    /// Pixels that came in since the last frame, onto the texture.
    fn take_updates(&mut self) {
        let Some(live) = &self.live else { return };
        let updates = live.take();
        if self.trace {
            self.traced.0 += updates.len();
            self.traced.1 += updates.iter().map(ScreenUpdate::bytes).sum::<usize>();
            if self.traced.2.elapsed() >= Duration::from_secs(1) {
                eprintln!("screen: {} updates/s, {:.2} MB/s", self.traced.0, self.traced.1 as f64 / 1e6);
                self.traced = (0, 0, Instant::now());
            }
        }
        for update in updates {
            match update {
                ScreenUpdate::Full { width, height, rgba } => {
                    let id = match self.image {
                        Some((id, w, h)) if (w, h) == (width, height) => id,
                        other => {
                            if let Some((id, ..)) = other {
                                release_image(id);
                            }
                            queue_live_image(width, height)
                        }
                    };
                    write_image(id, 0, 0, width, height, rgba);
                    self.image = Some((id, width, height));
                }
                ScreenUpdate::Patch { x, y, width, height, rgba } => {
                    if let Some((id, w, h)) = self.image
                        && x + width <= w
                        && y + height <= h
                    {
                        write_image(id, x, y, width, height, rgba);
                    }
                }
            }
        }
    }

    fn read_state(&mut self) {
        if self.state_read.elapsed() >= STATE_READ {
            self.state_read = Instant::now();
            self.state = self.session.viewer_state();
        }
    }

    fn at_ms(&self) -> Option<f64> {
        Some(self.started.elapsed().as_secs_f64() * 1000.0)
    }

    fn paint(&mut self, ui: &mut Ui<'_>, key: u64, area: Rect) {
        self.read_state();
        self.take_updates();
        ui.wake_after(STATE_READ.as_secs_f32());
        ui.fill(area, 0.0, Color::rgb(0x121315));
        let picture = match self.image {
            Some((id, width, height)) => {
                let picture = fit(area, width, height);
                ui.image(id, picture, 0.0, 1.0);
                picture
            }
            None => area,
        };
        let inside = ui.pointer_in(&picture);
        if ui.input.pressed || ui.input.right_pressed {
            if ui.pointer_in(&area) {
                ui.mem.focus = key;
            } else if ui.mem.focus == key && !ui.mem.press_keeps_focus {
                ui.mem.focus = 0;
            }
        }
        let focused = ui.mem.focus == key;
        ui.mem.text_input_focused |= focused;
        if focused {
            ui.ime_area = Some(rect(picture.x + picture.w * 0.5, picture.y + picture.h * 0.5, 2.0, 16.0));
        }
        let can = self.can_input();
        self.pointer(ui, picture, inside, can);
        if focused && can {
            self.keyboard(ui);
        }
    }

    fn pointer(&mut self, ui: &mut Ui<'_>, picture: Rect, inside: bool, can: bool) {
        let Some((px, py)) = ui.input.pointer else { return };
        let fraction = |clamp: bool| {
            let x = f64::from((px - picture.x) / picture.w.max(1.0));
            let y = f64::from((py - picture.y) / picture.h.max(1.0));
            if clamp { (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)) } else { (x, y) }
        };
        if !can {
            // Input stopped under a press: the model took over, or the connection went.
            if std::mem::take(&mut self.held) {
                let (x, y) = self.last_place;
                self.send(vec![ViewerEvent::Pointer { phase: PointerPhase::Cancel, x, y, buttons: 0, at: self.at_ms() }]);
            }
            return;
        }
        let touch = self.state.platform != ScreenPlatform::Desktop;
        if ui.input.pressed && inside && !self.held {
            let (x, y) = fraction(false);
            self.held = true;
            self.last_place = (x, y);
            self.last_move = None;
            self.send(vec![ViewerEvent::Pointer { phase: PointerPhase::Down, x, y, buttons: 1, at: self.at_ms() }]);
        } else if ui.input.right_pressed && inside && !touch {
            let (x, y) = fraction(false);
            self.send(vec![ViewerEvent::Pointer { phase: PointerPhase::Down, x, y, buttons: 2, at: self.at_ms() }, ViewerEvent::Pointer { phase: PointerPhase::Up, x, y, buttons: 0, at: self.at_ms() }]);
        }
        let moved = self.last_pointer != Some((px, py));
        self.last_pointer = Some((px, py));
        if self.held {
            let (x, y) = fraction(true);
            self.last_place = (x, y);
            if ui.input.released || !ui.input.down {
                self.held = false;
                self.send(vec![ViewerEvent::Pointer { phase: PointerPhase::Up, x, y, buttons: 0, at: self.at_ms() }]);
            } else if moved && self.last_move.is_none_or(|at| at.elapsed() >= MOVE_INTERVAL) {
                self.last_move = Some(Instant::now());
                self.send(vec![ViewerEvent::Pointer { phase: PointerPhase::Move, x, y, buttons: 1, at: self.at_ms() }]);
            }
        } else if moved && inside && !touch && self.last_move.is_none_or(|at| at.elapsed() >= MOVE_INTERVAL) {
            // A desktop has a pointer that hovers.
            let (x, y) = fraction(false);
            self.last_move = Some(Instant::now());
            self.send(vec![ViewerEvent::Pointer { phase: PointerPhase::Move, x, y, buttons: 0, at: self.at_ms() }]);
        }
        if inside && (ui.input.scroll != 0.0 || ui.input.scroll_x != 0.0) {
            let unit = if ui.input.scroll_is_pixels { PIXELS_PER_NOTCH } else { LINES_PER_NOTCH };
            let (x, y) = fraction(false);
            let notches = |delta: f32| f64::from(-delta / unit).clamp(-MAX_NOTCHES, MAX_NOTCHES);
            self.send(vec![ViewerEvent::Wheel { x, y, dx: notches(ui.input.scroll_x), dy: notches(ui.input.scroll) }]);
        }
    }

    fn keyboard(&mut self, ui: &mut Ui<'_>) {
        let touch = self.state.platform != ScreenPlatform::Desktop;
        let mut events = Vec::new();
        for press in ui.input.keys.clone() {
            let chord = press.cmd || press.ctrl;
            if let Key::Char(character) = press.key {
                if chord && character.eq_ignore_ascii_case(&'v') {
                    if let Some(text) = ui.clipboard.get() {
                        let text: String = text.chars().take(PASTE_MAX).collect();
                        events.push(ViewerEvent::Text { text });
                    }
                    continue;
                }
                // A letter on its own arrives as text too, which also knows about layouts.
                if !chord {
                    continue;
                }
                // Command belongs to this app on a phone, which has no such key.
                if press.cmd && touch {
                    continue;
                }
            }
            if let Some((key, code)) = dom_key(press.key) {
                events.extend(press_events(&press, &key, &code));
            }
        }
        if !ui.input.text.is_empty() {
            let text: String = ui.input.text.chars().filter(|character| !character.is_control()).collect();
            if !text.is_empty() {
                events.push(ViewerEvent::Text { text });
            }
        }
        if let Some(text) = &ui.input.ime_commit {
            events.push(ViewerEvent::Text { text: text.clone() });
        }
        self.send(events);
    }

    fn button(&self, name: &str) {
        self.send(vec![ViewerEvent::Button { name: name.to_owned() }]);
    }
}

impl Drop for ScreenView {
    fn drop(&mut self) {
        if let Some(live) = self.live.take() {
            self.session.unwatch(&live);
        }
        if let Some((id, ..)) = self.image.take() {
            release_image(id);
        }
        // The sender goes with this, which ends the input thread once it has let go of what is held.
    }
}

/// An id for this viewer among a session's. Nothing depends on it but its being different.
fn fastrand_id() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_nanos())
}

/// What the page's top bar said, in the pane's own words.
fn status_text(state: &ViewerState) -> &'static str {
    match state.phase {
        SessionPhase::Connecting => t("连接中", "Connecting"),
        SessionPhase::Credentials => t("需要登录", "Sign-in"),
        SessionPhase::Ready => t("已就绪", "Ready"),
        SessionPhase::Reconnecting => t("重连中", "Reconnecting"),
        SessionPhase::Failed => t("失败", "Failed"),
    }
}

fn device_keys(platform: ScreenPlatform) -> &'static [(&'static str, &'static str, &'static str)] {
    match platform {
        ScreenPlatform::Android => &[("Back", "返回", "Back"), ("HomeScreen", "主屏幕", "Home"), ("AppSwitch", "最近任务", "Recents")],
        ScreenPlatform::Ios => &[("HomeScreen", "主屏幕", "Home")],
        ScreenPlatform::Desktop => &[("ctrl+alt+Delete", "Ctrl+Alt+Del", "Ctrl+Alt+Del")],
    }
}

/// How a pane reaches its view from a handler.
pub(crate) type Reach = Rc<dyn Fn(&mut App) -> Option<&mut ScreenView>>;

/// The view's whole body: its bar, the picture, and whatever covers the picture.
fn screen_body(state: &ViewerState, picture_waiting: bool, form: SignInForm, session_key: &str, reach: Reach) -> E {
    let t_ = theme();
    let key = hash_key(("browser-screen", session_key));
    let paint_reach = reach.clone();
    let canvas: E = custom(move |app: &mut App, ui: &mut Ui<'_>, area: Rect| {
        if let Some(view) = paint_reach(app) {
            view.paint(ui, key, area);
        }
    })
    .key(("browser-screen", session_key))
    .editable()
    .absolute()
    .inset(0.0)
    .role(Role::Application)
    .aria_label(state.name.as_str());

    let mut bar = row().h(28.0).w_full().shrink0().px(8.0).gap(8.0).items_center().bg(t_.layer).border_bottom(t_.divider);
    bar = bar.child(text(state.name.clone()).ty(CAPTION_STRONG).ellipsis().min_w(0.0));
    let tone = match state.phase {
        SessionPhase::Ready => t_.success,
        SessionPhase::Failed => t_.critical,
        SessionPhase::Credentials | SessionPhase::Reconnecting => t_.caution,
        SessionPhase::Connecting => t_.text_secondary,
    };
    bar = bar.child(text(status_text(state)).ty(FINE).color(tone).nowrap().role(Role::Status));
    if let Some(screen) = state.screen {
        bar = bar.child(text(format!("{}×{}", screen.width, screen.height)).ty(FINE).color(t_.text_tertiary).nowrap());
    }
    if state.read_only {
        bar = bar.child(text(t("只读", "Read-only")).ty(FINE).color(t_.text_tertiary).nowrap());
    }
    if state.model_active {
        bar = bar.child(text(t("模型正在操作", "The model is controlling")).ty(FINE).color(t_.accent_text).nowrap());
    }
    bar = bar.child(div().flex1());
    let enabled = state.phase == SessionPhase::Ready && !state.read_only && !state.model_active;
    for (index, (name, zh, en)) in device_keys(state.platform).iter().enumerate() {
        let (name, press) = (*name, reach.clone());
        bar = bar.child(
            row()
                .key(("browser-screen-key", session_key, index))
                .h(20.0)
                .px(8.0)
                .items_center()
                .radius(4.0)
                .border(t_.card_stroke)
                .hover_bg(t_.subtle_hover)
                .keep_focus()
                .disabled(!enabled)
                .on_click(move |app: &mut App| {
                    if let Some(view) = press(app) {
                        view.button(name);
                    }
                })
                .child(text(t(zh, en)).ty(FINE).nowrap()),
        );
    }

    let mut stage = col().key(("browser-screen-stage", session_key)).flex1().min_h(0.0).clip().child(canvas);
    if let Some(cover) = cover(state, form, session_key, reach) {
        stage = stage.child(cover);
    } else if picture_waiting {
        stage = stage.child(col().absolute().inset(0.0).items_center().justify_center().pass_through().child(text(t("等待画面…", "Waiting for the picture…")).ty(CAPTION).color(t_.text_secondary).role(Role::Status)));
    }
    col().key(("browser-screen-view", session_key)).flex1().min_h(0.0).child(bar).child(stage)
}

/// What covers the picture while there is none to use.
fn cover(state: &ViewerState, form: SignInForm, session_key: &str, reach: Reach) -> Option<E> {
    let t_ = theme();
    let card = |child: E| col().absolute().inset(0.0).items_center().justify_center().p(18.0).bg(Color::rgb(0x121315).fade(0.72)).child(col().w_full().max_w(340.0).p(16.0).gap(10.0).radius(8.0).bg(t_.flyout).border(t_.card_stroke).shadow(14.0, 4.0, t_.shadow).child(child));
    match state.phase {
        SessionPhase::Ready => None,
        SessionPhase::Credentials if state.credentials.is_some() => Some(card(sign_in_form(state, form, session_key, reach))),
        SessionPhase::Credentials | SessionPhase::Connecting => Some(card(busy_card(t("正在连接…", "Connecting…"), state.message.as_deref()))),
        SessionPhase::Reconnecting => Some(card(busy_card(t("连接中断，正在重连…", "Connection lost, reconnecting…"), state.message.as_deref()))),
        SessionPhase::Failed => Some(card(col().gap(6.0).role(Role::Alert).child(text(t("无法连接", "Could not connect")).ty(BODY_STRONG).color(t_.critical)).child(text(state.message.clone().unwrap_or_else(|| t("没有更多信息。", "No further details.").to_owned())).ty(CAPTION).color(t_.text_secondary).selectable()))),
    }
}

fn busy_card(title: &str, detail: Option<&str>) -> E {
    let t_ = theme();
    let mut card = col().gap(6.0).items_center().child(text(title.to_owned()).ty(BODY_STRONG));
    if let Some(detail) = detail {
        card = card.child(text(detail.to_owned()).ty(CAPTION).color(t_.text_secondary).selectable());
    }
    card
}

fn sign_in_form(state: &ViewerState, form: SignInForm, session_key: &str, reach: Reach) -> E {
    let t_ = theme();
    let Some(request) = state.credentials.clone() else { return div() };
    let (username, password, remember, submitting) = (reach.clone(), reach.clone(), reach.clone(), reach.clone());
    let SignInForm { username: name_value, submitted, remember: remembered } = form;
    let submit: Rc<dyn Fn(&mut App)> = Rc::new(move |app: &mut App| {
        let Some(view) = submitting(app) else { return };
        if view.sign_in.submitted.is_some() {
            return;
        }
        view.sign_in.submitted = Some(Instant::now());
        let credentials = ViewerCredentials { username: (!view.sign_in.username.is_empty()).then(|| view.sign_in.username.clone()), password: std::mem::take(&mut view.sign_in.password), remember: view.sign_in.remember };
        let session = view.session.clone();
        // The password does not stay here once it has been sent.
        std::thread::spawn(move || session.answer_credentials(credentials));
    });
    let mut form = col().gap(8.0).child(text(t("登录远程屏幕", "Sign in to the remote screen")).ty(BODY_STRONG)).child(text(format!("{} {} · {}", t("密码会发送到", "Your password is sent to"), state.name, state.endpoint)).ty(CAPTION).color(t_.text_secondary));
    if request.retry {
        form = form.child(text(t("密码被拒绝，请重试。", "The password was refused. Try again.")).ty(CAPTION).color(t_.critical).role(Role::Alert));
    }
    if request.needs_username {
        form = form.child(text(t("账户名", "Account name")).ty(CAPTION)).child(
            field(("browser-screen-username", session_key), name_value)
                .max_chars(128)
                .on_change(move |app: &mut App, value| {
                    if let Some(view) = username(app) {
                        view.sign_in.username = value;
                    }
                })
                .build()
                .aria_label(t("账户名", "Account name")),
        );
    }
    let on_password_submit = submit.clone();
    form = form
        .child(text(t("密码", "Password")).ty(CAPTION))
        .child(
            field(("browser-screen-password", session_key), "")
                .obscure(true)
                .max_chars(1024)
                .on_change(move |app: &mut App, value| {
                    if let Some(view) = password(app) {
                        *view.sign_in.password = value;
                    }
                })
                .on_submit(move |app: &mut App, _| on_password_submit(app))
                .build()
                .aria_label(t("密码", "Password")),
        )
        .child(
            row()
                .gap(6.0)
                .items_center()
                .child(checkbox(remembered, move |app: &mut App| {
                    if let Some(view) = remember(app) {
                        view.sign_in.remember = !view.sign_in.remember;
                    }
                }).aria_label(t("在这台电脑上记住", "Remember on this computer")))
                .child(text(t("在这台电脑上记住", "Remember on this computer")).ty(CAPTION)),
        )
        .child(
            button(if submitted { t("正在登录…", "Signing in…") } else { t("登录", "Sign in") })
                .key(("browser-screen-signin", session_key))
                .disabled(submitted)
                .on_click(move |app: &mut App| submit(app)),
        );
    form
}

/// A screen's body, with what its form holds worked out first.
pub(crate) fn body(app: &mut App, session_key: &str, reach: Reach) -> E {
    let Some(view) = reach(app) else { return div() };
    let state = view.state.clone();
    let waiting = view.picture_waiting();
    let form = view.sign_in_form_state();
    screen_body(&state, waiting, form, session_key, reach)
}

impl ScreenView {
    pub fn picture_waiting(&self) -> bool {
        self.image.is_none() && self.state.phase == SessionPhase::Ready
    }

    /// What the sign-in form shows this frame. A request it has not seen starts it clean, with focus where typing goes next.
    fn sign_in_form_state(&mut self) -> SignInForm {
        if let Some(request) = &self.state.credentials {
            let key = format!("{}|{}|{:?}|{}", self.state.name, self.state.endpoint, request.username, request.retry);
            if self.sign_in.key != key {
                self.sign_in = SignIn { key, username: request.username.clone().unwrap_or_default(), remember: self.sign_in.remember, ..SignIn::default() };
                let wants_name = request.needs_username && self.sign_in.username.is_empty();
                focus_request(("field", if wants_name { "browser-screen-username" } else { "browser-screen-password" }));
            }
        }
        // Still asking after a fair wait: the answer did not take.
        if self.sign_in.submitted.is_some_and(|at| at.elapsed() > ANSWER_WAIT) {
            self.sign_in.submitted = None;
            *self.sign_in.password = String::new();
        }
        SignInForm { username: self.sign_in.username.clone(), submitted: self.sign_in.submitted.is_some(), remember: self.sign_in.remember }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_sits_in_the_middle_of_what_it_does_not_fill() {
        let tall = fit(rect(10.0, 20.0, 400.0, 400.0), 1080, 2160);
        assert_eq!((tall.w, tall.h), (200.0, 400.0));
        assert_eq!((tall.x, tall.y), (110.0, 20.0));
        let wide = fit(rect(0.0, 0.0, 400.0, 400.0), 1920, 960);
        assert_eq!((wide.w, wide.h), (400.0, 200.0));
        assert_eq!((wide.x, wide.y), (0.0, 100.0));
    }

    #[test]
    fn moves_gather_and_presses_do_not() {
        let at = |phase, x: f64| ViewerEvent::Pointer { phase, x, y: 0.0, buttons: 1, at: None };
        let events = coalesced(vec![at(PointerPhase::Down, 0.0), at(PointerPhase::Move, 0.1), at(PointerPhase::Move, 0.2), at(PointerPhase::Move, 0.3), at(PointerPhase::Up, 0.3)]);
        assert_eq!(events.len(), 3);
        assert!(matches!(events[1], ViewerEvent::Pointer { phase: PointerPhase::Move, x, .. } if x == 0.3));
        let turned = coalesced(vec![ViewerEvent::Wheel { x: 0.0, y: 0.0, dx: 0.0, dy: 6.0 }, ViewerEvent::Wheel { x: 0.5, y: 0.5, dx: 0.0, dy: 6.0 }]);
        assert_eq!(turned, vec![ViewerEvent::Wheel { x: 0.5, y: 0.5, dx: 0.0, dy: 10.0 }]);
    }

    #[test]
    fn a_press_is_spelled_as_a_page_would_and_its_modifiers_wrap_it() {
        assert_eq!(dom_key(Key::Char('c')), Some(("c".to_owned(), "KeyC".to_owned())));
        assert_eq!(dom_key(Key::Left), Some(("ArrowLeft".to_owned(), "ArrowLeft".to_owned())));
        let press = KeyPress { key: Key::Char('c'), shift: false, cmd: false, alt: false, ctrl: true };
        let events = press_events(&press, "c", "KeyC");
        let names: Vec<(&str, bool)> = events.iter().map(|event| match event {
            ViewerEvent::Key { key, down, .. } => (key.as_str(), *down),
            _ => ("", false),
        }).collect();
        assert_eq!(names, [("Control", true), ("c", true), ("c", false), ("Control", false)]);
    }
}
