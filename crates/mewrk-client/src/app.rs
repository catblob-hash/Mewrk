//! The application: the document, what was loaded on demand, and the mechanics of talking to the host.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};

use mewrk_lib::model::AppDocument;
use mewrk_lib::native::{ChannelSink, Host, HostConfig, StartError};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

type Boxed = Box<dyn FnOnce(&mut App) + Send>;
type ChannelFn = Arc<dyn Fn(Value) + Send + Sync>;

/// How other threads reach the UI thread.
struct Mailbox {
    queue: Mutex<Vec<Boxed>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Mailbox {
    fn post(&self, f: Boxed) {
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).push(f);
        (self.wake)();
    }
}

#[derive(Default)]
struct Channels {
    table: RwLock<HashMap<String, ChannelFn>>,
    next: Mutex<u64>,
}

/// A channel the host can send on. Its argument form goes into a command's JSON arguments.
#[derive(Clone)]
pub struct Channel {
    id: String,
    channels: Arc<Channels>,
}

impl Channel {
    pub fn arg(&self) -> Value {
        json!({ "__mewrkChannel": self.id })
    }

    pub fn close(&self) {
        self.channels.table.write().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
    }
}

/// Things that need the window, done between frames by whoever owns it.
#[derive(Default)]
pub struct WindowRequests {
    pub drag: bool,
    pub zoom: bool,
    pub quit: bool,
    /// Bring the window to the front.
    pub show: bool,
    pub hide: bool,
}

pub struct StartOptions {
    pub identifier: String,
    pub version: String,
    pub data_dir: PathBuf,
    pub resource_dir: Option<PathBuf>,
    /// Wakes the UI thread when something is waiting for it.
    pub wake: Arc<dyn Fn() + Send + Sync>,
    pub exit: Arc<dyn Fn(i32) + Send + Sync>,
    pub show_window: Arc<dyn Fn() + Send + Sync>,
}

/// The data the upstream renderer kept in `App.tsx`'s top-level state, and everything views reach through.
pub struct App {
    pub host: Host,
    /// Every conversation without its contexts until it is opened.
    pub doc: AppDocument,
    /// Conversations whose contexts have not been loaded yet.
    pub unloaded: HashSet<String>,
    pub window: WindowRequests,
    pub system_dark: bool,
    /// The window's content view, for native child views. Null when headless.
    pub native_view: *mut std::ffi::c_void,
    pub frame_no: u64,
    mailbox: Arc<Mailbox>,
    channels: Arc<Channels>,
    locals: HashMap<TypeId, Box<dyn Any>>,
    push_listeners: Vec<Rc<dyn Fn(&mut App, &Value)>>,
}

use std::rc::Rc;

/// A background job written the way upstream's `async` functions are: sequential, blocking on the host, reaching
/// back to the UI thread for state. This is how to port something full of `await`.
#[derive(Clone)]
pub struct Task {
    host: Host,
    mailbox: Arc<Mailbox>,
    channels: Arc<Channels>,
}

impl Task {
    /// `await invoke(command, args)`.
    pub fn invoke<T: DeserializeOwned>(&self, command: &str, args: Value) -> Result<T, String> {
        let value = self.host.invoke_blocking(command, args)?;
        serde_json::from_value(value).map_err(|e| format!("{command}: {e}"))
    }

    /// Runs on the UI thread and waits for the answer: reading state, or setting it.
    pub fn update<R: Send + 'static>(&self, f: impl FnOnce(&mut App) -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::sync_channel(1);
        self.mailbox.post(Box::new(move |app| {
            let _ = tx.send(f(app));
        }));
        rx.recv().expect("the UI thread outlives its tasks")
    }

    /// The same without waiting.
    pub fn post(&self, f: impl FnOnce(&mut App) + Send + 'static) {
        self.mailbox.post(Box::new(f));
    }

    /// A channel whose messages run on the UI thread.
    pub fn open_channel(&self, on_message: impl Fn(&mut App, Value) + Send + Sync + 'static) -> Channel {
        open_channel(&self.channels, &self.mailbox, on_message)
    }
}

fn open_channel(channels: &Arc<Channels>, mailbox: &Arc<Mailbox>, on_message: impl Fn(&mut App, Value) + Send + Sync + 'static) -> Channel {
    let id = {
        let mut next = channels.next.lock().unwrap_or_else(|p| p.into_inner());
        *next += 1;
        format!("c{next}")
    };
    let on_message = Arc::new(on_message);
    let mailbox = mailbox.clone();
    let f: ChannelFn = Arc::new(move |value| {
        let on_message = on_message.clone();
        mailbox.post(Box::new(move |app| on_message(app, value)));
    });
    channels.table.write().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), f);
    Channel { id, channels: channels.clone() }
}

impl App {
    /// Wakes the UI thread from anywhere, for something that has its own way of handing over what it has.
    pub fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        self.mailbox.wake.clone()
    }
}

impl App {
    pub fn start(options: StartOptions) -> Result<Self, StartError> {
        let channels: Arc<Channels> = Arc::new(Channels::default());
        let sink: ChannelSink = {
            let channels = channels.clone();
            Arc::new(move |id: &str, value: Value| {
                let f = channels.table.read().unwrap_or_else(|p| p.into_inner()).get(id).cloned();
                if let Some(f) = f {
                    f(value);
                }
            })
        };
        let mailbox = Arc::new(Mailbox { queue: Mutex::new(Vec::new()), wake: options.wake.clone() });
        let main_mailbox = mailbox.clone();
        let run_on_main: Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync> = Arc::new(move |f| {
            main_mailbox.post(Box::new(move |_| f()));
        });
        let host = Host::start(
            HostConfig {
                identifier: options.identifier,
                version: options.version,
                local_data_dir: options.data_dir.clone(),
                data_dir: options.data_dir,
                resource_dir: options.resource_dir,
                run_on_main: Some(run_on_main),
                exit: Some(options.exit),
                show_window: Some(options.show_window),
            },
            sink,
        )?;
        let loaded = host.invoke_blocking("load_document", json!({})).map_err(StartError::Unavailable)?;
        let unloaded = loaded.get("unloadedConversationIds").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
        let doc: AppDocument = serde_json::from_value(loaded).map_err(|e| StartError::Unavailable(format!("load_document: {e}")))?;
        let mut app = Self {
            host,
            doc,
            unloaded,
            window: WindowRequests::default(),
            system_dark: true,
            native_view: std::ptr::null_mut(),
            frame_no: 0,
            mailbox,
            channels,
            locals: HashMap::new(),
            push_listeners: Vec::new(),
        };
        crate::i18n::set_english(!app.language_is_chinese());
        app.subscribe_push_events();
        Ok(app)
    }

    /// What `appLanguage` resolves to.
    pub fn language_is_chinese(&self) -> bool {
        use mewrk_lib::model::AppLanguage;
        match self.doc.global_settings.app_language {
            AppLanguage::ZhCn => true,
            AppLanguage::EnUs => false,
            AppLanguage::Auto => crate::i18n::system_prefers_chinese(),
        }
    }

    /// Handles what other threads left for the UI thread. Returns whether there was anything.
    pub fn tick(&mut self) -> bool {
        let jobs = std::mem::take(&mut *self.mailbox.queue.lock().unwrap_or_else(|p| p.into_inner()));
        let any = !jobs.is_empty();
        for job in jobs {
            job(self);
        }
        any
    }

    /// `invoke(command, args).then(done)`. `done` runs on the UI thread.
    pub fn invoke<T: DeserializeOwned + Send + 'static>(&self, command: &str, args: Value, done: impl FnOnce(&mut App, Result<T, String>) + Send + 'static) {
        let mailbox = self.mailbox.clone();
        let name = command.to_owned();
        self.host.invoke_detached(command.to_owned(), args, move |result| {
            let result = result.and_then(|v| serde_json::from_value::<T>(v).map_err(|e| format!("{name}: {e}")));
            mailbox.post(Box::new(move |app| done(app, result)));
        });
    }

    /// A call whose result only matters if it fails.
    pub fn invoke_logged(&self, command: &str, args: Value) {
        let name = command.to_owned();
        self.invoke::<Value>(command, args, move |_, result| {
            if let Err(e) = result {
                eprintln!("{name}: {e}");
            }
        });
    }

    /// Where the app keeps its files.
    pub fn app_data_dir(&self) -> Option<PathBuf> {
        use tauri::Manager;
        self.host.app().path().app_data_dir().ok()
    }

    /// Runs `f` on a background thread; see [`Task`].
    pub fn spawn(&self, f: impl FnOnce(Task) + Send + 'static) {
        let task = self.task();
        std::thread::Builder::new().name("mewrk-task".to_owned()).spawn(move || f(task)).expect("thread");
    }

    pub fn task(&self) -> Task {
        Task { host: self.host.clone(), mailbox: self.mailbox.clone(), channels: self.channels.clone() }
    }

    /// A channel whose messages run on the UI thread.
    pub fn open_channel(&self, on_message: impl Fn(&mut App, Value) + Send + Sync + 'static) -> Channel {
        open_channel(&self.channels, &self.mailbox, on_message)
    }

    /// Runs a closure on the next frame.
    pub fn post(&self, f: impl FnOnce(&mut App) + Send + 'static) {
        self.mailbox.post(Box::new(f));
    }

    /// One value of a type per application: how a view module keeps state that lives as long as the app, without
    /// `App` having to know about it.
    pub fn local<T: Default + 'static>(&mut self) -> &mut T {
        self.locals.entry(TypeId::of::<T>()).or_insert_with(|| Box::new(T::default())).downcast_mut::<T>().expect("keyed by type")
    }

    /// Hears every `AppPushEvent` from the host as JSON (see `src/lib/appEvents.ts`). Dispatch on `"type"`.
    pub fn on_push_event(&mut self, f: impl Fn(&mut App, &Value) + 'static) {
        self.push_listeners.push(Rc::new(f));
    }

    fn subscribe_push_events(&mut self) {
        let channel = self.open_channel(|app, event| {
            let listeners = app.push_listeners.clone();
            for l in listeners {
                l(app, &event);
            }
        });
        let args = json!({ "onEvent": channel.arg() });
        self.invoke_logged("subscribe_app_events", args);
    }
}
