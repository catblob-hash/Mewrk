//! A real host in a temp data folder, driven the way a window drives it: frames, a pointer and keys, no GPU.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use fluent::el::{hash_key, rect_of, render, scroll_request, set_theme, set_window_size, El, ScrollRequest};
use fluent::ui::{Clipboard, Input, Key, KeyPress, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::{App, StartOptions};

/// A credential store that lives in memory, so nothing in a test can reach the real keychain.
#[derive(Debug, Default)]
struct MemoryStore {
    secrets: Arc<Mutex<HashMap<(String, String), Vec<u8>>>>,
}

#[derive(Debug)]
struct MemoryCredential {
    key: (String, String),
    secrets: Arc<Mutex<HashMap<(String, String), Vec<u8>>>>,
}

/// Credential writes fail while this holds a message.
pub static REFUSE_WRITES: Mutex<Option<String>> = Mutex::new(None);
/// Credential reads fail while this holds a message.
pub static REFUSE_READS: Mutex<Option<String>> = Mutex::new(None);

fn platform_failure(message: String) -> keyring::Error {
    keyring::Error::PlatformFailure(message.into())
}

impl keyring::credential::CredentialApi for MemoryCredential {
    fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
        if let Some(message) = REFUSE_WRITES.lock().unwrap().clone() {
            return Err(platform_failure(message));
        }
        self.secrets.lock().unwrap().insert(self.key.clone(), secret.to_vec());
        Ok(())
    }

    fn get_secret(&self) -> keyring::Result<Vec<u8>> {
        if let Some(message) = REFUSE_READS.lock().unwrap().clone() {
            return Err(platform_failure(message));
        }
        self.secrets.lock().unwrap().get(&self.key).cloned().ok_or(keyring::Error::NoEntry)
    }

    fn delete_credential(&self) -> keyring::Result<()> {
        if let Some(message) = REFUSE_WRITES.lock().unwrap().clone() {
            return Err(platform_failure(message));
        }
        self.secrets.lock().unwrap().remove(&self.key).map(|_| ()).ok_or(keyring::Error::NoEntry)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl keyring::credential::CredentialBuilderApi for MemoryStore {
    fn build(&self, _target: Option<&str>, service: &str, user: &str) -> keyring::Result<Box<keyring::credential::Credential>> {
        Ok(Box::new(MemoryCredential { key: (service.to_owned(), user.to_owned()), secrets: self.secrets.clone() }))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn persistence(&self) -> keyring::credential::CredentialPersistence {
        keyring::credential::CredentialPersistence::ProcessOnly
    }
}

fn install_memory_store() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| keyring::set_default_credential_builder(Box::new(MemoryStore::default())));
}

#[derive(Default)]
pub struct TestClipboard(pub Option<String>);

impl Clipboard for TestClipboard {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

/// A folder that goes away with the test.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mewrk-settings-{name}-{}-{:x}", std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn start_app(dir: &Path) -> App {
    install_memory_store();
    App::start(StartOptions {
        identifier: "dev.mewrk.settings-test".to_owned(),
        version: "0.0.1".to_owned(),
        data_dir: dir.to_owned(),
        resource_dir: None,
        wake: Arc::new(|| {}),
        exit: Arc::new(|_| {}),
        show_window: Arc::new(|| {}),
    })
    .unwrap_or_else(|e| panic!("the host starts: {e:?}"))
}

pub struct Rig {
    pub scene: Scene,
    pub text: TextCtx,
    pub mem: UiMemory,
    pub input: Input,
    pub clipboard: TestClipboard,
    pub theme: Theme,
    pub size: (f32, f32),
    /// The scroller a click scrolls first, to reach what's below the fold as a person would.
    pub scroller: Option<(&'static str, u64)>,
}

impl Rig {
    pub fn new() -> Self {
        Self { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: TestClipboard::default(), theme: Theme::dark(), size: (1280.0, 900.0), scroller: None }
    }

    pub fn frame(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>) {
        self.scene.clear();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut self.clipboard, 1.0, self.size, 0.016, 0.0);
        set_theme(self.theme);
        set_window_size(self.size.0, self.size.1);
        app.tick();
        let root = view(app);
        render(root, app, &mut ui);
        self.input.end_frame();
    }

    /// Frames until the mailbox has been quiet for a moment.
    pub fn settle(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>) {
        for _ in 0..4 {
            self.frame(app, view);
        }
    }

    /// Runs frames until `done`, giving the host's threads time to answer.
    pub fn until(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, what: &str, done: impl Fn(&mut App) -> bool) {
        let begun = Instant::now();
        loop {
            self.frame(app, view);
            if done(app) {
                return;
            }
            assert!(begun.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn click_at(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(app, view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app, view);
        self.input.down = false;
        self.input.released = true;
        self.frame(app, view);
        self.settle(app, view);
    }

    /// Scrolling is animated, so this runs until what it scrolled to has stopped moving.
    pub fn reveal(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, key: &(impl std::hash::Hash + ?Sized)) {
        let Some(scroller) = self.scroller else { return };
        let (Some(frame), Some(r)) = (rect_of(scroller), rect_of(key)) else { return };
        if r.y < frame.y || r.bottom() > frame.bottom() {
            scroll_request(scroller, ScrollRequest::By(r.y - frame.y - 60.0));
            let mut last = f32::MAX;
            for _ in 0..600 {
                self.frame(app, view);
                let y = rect_of(key).map_or(0.0, |r| r.y);
                if (y - last).abs() < 0.01 {
                    break;
                }
                last = y;
            }
        }
    }

    /// Clicks the middle of whatever has this key on screen.
    pub fn click(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, key: impl std::hash::Hash + std::fmt::Debug) {
        self.reveal(app, view, &key);
        let r = rect_of(&key).unwrap_or_else(|| panic!("nothing on screen has the key {key:?}"));
        assert!(r.w > 0.0 && r.h > 0.0, "{key:?} has no size: {r:?}");
        self.click_at(app, view, r.x + r.w * 0.5, r.y + r.h * 0.5);
    }

    /// A press on the settings window's own title, where nothing answers: focus leaves whatever had it.
    pub fn click_away(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>) {
        self.click_at(app, view, self.size.0 * 0.5 - 500.0, self.size.1 * 0.5 - 370.0);
    }

    /// Focuses a text field by its key and types.
    pub fn type_in(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, key: impl std::hash::Hash + std::fmt::Debug, typed: &str) {
        let id = hash_key(("field", &key));
        self.reveal(app, view, &id);
        let r = rect_of(id).unwrap_or_else(|| panic!("no field has the key {key:?}"));
        self.click_at(app, view, r.x + r.w * 0.5, r.y + r.h * 0.5);
        self.select_all_and_type(app, view, typed);
    }

    pub fn select_all_and_type(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, typed: &str) {
        self.press(app, view, Key::Char('a'), |k| k.cmd = true);
        self.input.text = typed.to_owned();
        self.frame(app, view);
        if typed.is_empty() {
            self.press(app, view, Key::Backspace, |_| {});
        }
        self.settle(app, view);
    }

    pub fn press(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, key: Key, f: impl FnOnce(&mut KeyPress)) {
        let mut press = KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false };
        f(&mut press);
        self.input.keys.push(press);
        self.frame(app, view);
        self.settle(app, view);
    }

    /// Picks an option from a `select` by its key and index.
    pub fn choose(&mut self, app: &mut App, view: &dyn Fn(&mut App) -> El<App>, key: impl std::hash::Hash + Clone + std::fmt::Debug, index: usize) {
        let anchor = hash_key(("select", hash_key(key.clone())));
        self.click(app, view, anchor);
        let panel = rect_of(("select-panel", anchor)).unwrap_or_else(|| panic!("the menu of {key:?} is open"));
        self.click_at(app, view, panel.x + 20.0, panel.y + 4.0 + 30.0 * index as f32 + 15.0);
    }
}

pub fn field_rect(key: impl std::hash::Hash) -> Option<fluent::ui::Rect> {
    rect_of(hash_key(("field", key)))
}

pub fn on_screen(key: impl std::hash::Hash) -> bool {
    rect_of(key).is_some_and(|r| r.w > 0.0 && r.h > 0.0)
}

/// A local `/models` endpoint. Nothing here leaves the machine, and the only key it ever sees is the test's own.
pub struct MockModels {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<String>>>,
    pub status: Arc<Mutex<(u16, String)>>,
    /// How long an answer takes.
    pub delay: Arc<Mutex<Duration>>,
}

impl MockModels {
    pub fn start(body: &str) -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new((200u16, body.to_owned())));
        let delay = Arc::new(Mutex::new(Duration::ZERO));
        let (seen, answer, wait) = (requests.clone(), status.clone(), delay.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = [0u8; 8192];
                let n = stream.read(&mut buffer).unwrap_or(0);
                let head = String::from_utf8_lossy(&buffer[..n]).to_string();
                seen.lock().unwrap().push(head);
                std::thread::sleep(*wait.lock().unwrap());
                let (code, body) = answer.lock().unwrap().clone();
                let response = format!("HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { base_url: format!("http://127.0.0.1:{port}/v1"), requests, status, delay }
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

pub const TWO_MODELS: &str = r#"{"object":"list","data":[{"id":"gpt-5","object":"model"},{"id":"qwen-mini","object":"model"}]}"#;

/// What changes the language for a moment holds this, and so does what compares words it read a moment ago.
pub static LANGUAGE: Mutex<()> = Mutex::new(());

/// The language is one switch for the process and every host start sets it, so wording is checked against both.
pub fn says(actual: &str, zh: &str, en: &str) -> bool {
    actual.contains(zh) || actual.contains(en)
}

/// A provider list that asks nothing of the machine: the page opens on this row, not on Claude Agent's.
pub fn open_without_network(app: &mut App, keep: &str) {
    let state = mewrk_settings_global::providers::state(app);
    state.started = true;
    state.selected_id = Some(keep.to_owned());
}

// ---------------------------------------------------------------- accessibility

pub type AxNodes = Vec<(accesskit::NodeId, accesskit::Node)>;

/// The whole tree as the frame left it, with a client that has just arrived.
pub fn a11y_tree(rig: &mut Rig, app: &mut App, view: &dyn Fn(&mut App) -> El<App>) -> AxNodes {
    fluent::el::set_a11y_active(true);
    rig.settle(app, view);
    fluent::el::take_a11y_update().expect("a client gets the tree").nodes
}

/// What a node is called: a label reads its value, everything else its label.
pub fn a11y_name(node: &accesskit::Node) -> &str {
    let words = if node.role() == accesskit::Role::Label { node.value() } else { node.label() };
    words.unwrap_or("")
}

pub fn a11y_named<'a>(tree: &'a AxNodes, name: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| a11y_name(node) == name).unwrap_or_else(|| panic!("nothing says {name:?} in {:#?}", a11y_names(tree)))
}

/// The node of this role that says this: the words beside a field say what it does too.
pub fn a11y_is<'a>(tree: &'a AxNodes, role: accesskit::Role, name: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.role() == role && a11y_name(node) == name).unwrap_or_else(|| panic!("no {role:?} says {name:?} in {:#?}", a11y_names(tree)))
}

pub fn a11y_names(tree: &AxNodes) -> Vec<String> {
    tree.iter().map(|(_, node)| format!("{:?} {:?}", node.role(), a11y_name(node))).collect()
}

/// Whatever a keyboard or a reader can act on and has no name. A sweep of a surface asserts this is empty.
pub fn a11y_unnamed(tree: &AxNodes) -> Vec<String> {
    use accesskit::{Action, Role};
    tree.iter()
        .map(|(_, node)| node)
        .filter(|node| !matches!(node.role(), Role::Window | Role::TextRun | Role::Label))
        .filter(|node| [Action::Click, Action::Focus, Action::SetValue, Action::Increment].into_iter().any(|a| node.supports_action(a)) || matches!(node.role(), Role::Dialog | Role::AlertDialog))
        .filter(|node| a11y_name(node).trim().is_empty() && node.labelled_by().is_empty())
        .map(|node| format!("{:?} at {:?}", node.role(), node.bounds()))
        .collect()
}

/// Walks the Tab order from the top, all the way round, and says what each stop is called. A stop the tree doesn't know lands the
/// focus on the window or the dialog, and reads as what is around it.
pub fn a11y_tab_stops(rig: &mut Rig, app: &mut App, view: &dyn Fn(&mut App) -> El<App>) -> Vec<String> {
    let mut seen = Vec::new();
    let mut stops = Vec::new();
    for _ in 0..400 {
        rig.press(app, view, Key::Tab, |_| {});
        let focus = fluent::el::focused_id();
        if focus == 0 {
            break;
        }
        if seen.contains(&focus) {
            assert_eq!(focus, seen[0], "Tab goes round the whole page, and this one sends it back early after {stops:#?}");
            break;
        }
        seen.push(focus);
        fluent::el::set_a11y_active(true);
        rig.settle(app, view);
        let update = fluent::el::take_a11y_update().expect("a client gets the tree");
        let node = update.nodes.iter().find(|(id, _)| *id == update.focus).map(|(_, node)| node).expect("the focus is in the tree");
        stops.push(format!("{:?} {:?}", node.role(), a11y_name(node)));
    }
    stops
}
