//! Asking the user, in the app, what an SSH connection needs: a password, a
//! key passphrase, or whether to trust a host key met for the first time.
//!
//! OpenSSH asks through the program `SSH_ASKPASS` names whenever it cannot ask
//! on a terminal — and, with `SSH_ASKPASS_REQUIRE=force`, even when it could,
//! which matters for a development launch from a terminal, where `ssh` would
//! otherwise ask on that terminal and wait there unseen. Every `ssh` this host
//! starts for a connection ([`prompting`]) names Mewrk's own executable as
//! that program, together with a loopback port, a token and the connection's
//! identity in its environment. Run that way, the executable never starts the
//! application: [`askpass_main`] sends the question to the port and prints
//! the answer for `ssh`, which acts on it exactly as on a typed one. The
//! [`Broker`] behind the port puts the question in front of the user
//! (`AppPushEvent::SshPromptRequested`) and hands back what they answer
//! (`answer_ssh_prompt`).
//!
//! - A host key is accepted by answering `yes`, and `ssh` itself writes it to
//!   `~/.ssh/known_hosts`. A key that differs from one already there is never
//!   asked about: OpenSSH refuses it outright under the default
//!   `StrictHostKeyChecking`, and the refusal is reported as such.
//! - Only a connection the user is waiting for asks. Work nobody asked for —
//!   the capability scan when Mewrk opens, Git polling, a link reconnecting
//!   by itself — is never answered with a dialog: its question is withheld,
//!   the login fails quietly, and the machine is left alone until something
//!   the user does needs it ([`unattended`], [`demand`]).
//! - A password or passphrase is remembered for its machine and question, in
//!   memory only, until Mewrk quits, so a reconnect or a second connection
//!   does not ask again. An answer `ssh` asks for a second time in the same
//!   connection was wrong, and so is one a login was refused with; it is
//!   forgotten and the user asked again.
//! - A question the user turns down is not asked again for that machine for a
//!   short while, so the few logins one operation makes do not each put it
//!   back on screen. The next thing the user does on the machine asks again.
//! - Logins that need nothing — a key, `~/.ssh/config`, ssh-agent — never
//!   reach any of this.
//!
//! Without a broker (it could not listen, or the executable cannot be found)
//! `ssh` keeps `BatchMode=yes` and fails on any question, as it always did.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const PORT_ENV: &str = "MEWRK_ASKPASS_PORT";
const TOKEN_ENV: &str = "MEWRK_ASKPASS_TOKEN";
const SESSION_ENV: &str = "MEWRK_ASKPASS_SESSION";
const MACHINE_ENV: &str = "MEWRK_ASKPASS_MACHINE";
/// Who the `ssh` is logging in for ([`Asking`]), which decides whether its
/// question may be put in front of the user.
const ORIGIN_ENV: &str = "MEWRK_ASKPASS_ORIGIN";
/// What OpenSSH sets for a question that is not a secret: `confirm` for a
/// yes/no, `none` for a notice it takes down itself (touch your key).
const OPENSSH_PROMPT_ENV: &str = "SSH_ASKPASS_PROMPT";

/// Largest question or answer carried. A prompt is a line or two; anything
/// near this is not one.
const MAX_MESSAGE_BYTES: u64 = 64 * 1024;
/// How long a question waits for the user before it is withdrawn, unanswered
/// rather than declined, so a connection nobody attends to ends at last.
const PROMPT_PATIENCE: Duration = Duration::from_secs(10 * 60);
/// How often a waiting question checks that its `ssh` is still there.
const WAIT_POLL: Duration = Duration::from_millis(250);
/// How long a connection's record of what it asked is kept after it last asked.
const SESSION_MEMORY: Duration = Duration::from_secs(10 * 60);
/// How long a turned-down question stays turned down. Long enough to cover the
/// logins one operation makes — a probe of the login shell, the proxy, an
/// upload — so the user is not asked again for each; short enough that the
/// next thing they do on the machine asks afresh.
const DECLINE_MEMORY: Duration = Duration::from_secs(30);

/// Who the `ssh` this thread starts is logging in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asking {
    /// Something the user did and is waiting on: a tool call, a terminal, a
    /// pane they opened, a run they started. Its questions are shown.
    User,
    /// Work nobody asked for at this moment ([`unattended`]). Its questions
    /// are withheld unless the answer is already known.
    Nobody,
    /// A machine's link (re)connecting on its own thread ([`for_link`]). It
    /// asks only while someone who asked for that machine's work is waiting on
    /// it ([`demand`]).
    Link,
}

impl Asking {
    fn wire(self) -> &'static str {
        match self {
            Asking::User => "user",
            Asking::Nobody => "nobody",
            Asking::Link => "link",
        }
    }
}

thread_local! {
    static ASKING: std::cell::Cell<Asking> = const { std::cell::Cell::new(Asking::User) };
}

/// While held, the `ssh` this thread starts logs in for someone other than the
/// user ([`Asking`]). Restores what the thread was doing before when dropped,
/// and is tied to the thread it was taken on.
pub struct AskingScope {
    previous: Asking,
    _thread: std::marker::PhantomData<*const ()>,
}

impl Drop for AskingScope {
    fn drop(&mut self) {
        ASKING.with(|asking| asking.set(self.previous));
    }
}

fn enter(next: Asking) -> AskingScope {
    let previous = ASKING.with(|asking| asking.replace(next));
    AskingScope {
        previous,
        _thread: std::marker::PhantomData,
    }
}

/// Marks this thread's SSH work as something nobody is waiting for — a scan,
/// a poll — so a login it starts never puts a question on screen. A worker
/// thread it hands the work to takes the mark along with [`carry`].
pub fn unattended() -> AskingScope {
    enter(Asking::Nobody)
}

/// Marks this thread as a machine's link logging in by itself.
pub fn for_link() -> AskingScope {
    enter(Asking::Link)
}

/// What this thread's SSH work is, for a worker thread to take on with [`carry`].
#[derive(Clone, Copy, Debug)]
pub struct Attendance(Asking);

pub fn attendance() -> Attendance {
    Attendance(ASKING.with(std::cell::Cell::get))
}

/// Puts a worker thread in the state of the thread that started it.
pub fn carry(attendance: Attendance) -> AskingScope {
    enter(attendance.0)
}

/// One question on screen, as the renderer draws it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshPrompt {
    pub id: String,
    /// The machine as its `ssh` destination names it.
    pub machine: String,
    pub kind: SshPromptKind,
    /// What `ssh` asked, verbatim.
    pub prompt: String,
    /// The key a first connection met, read out of the question.
    pub host_key: Option<HostKeyFingerprint>,
    /// The answer last given to this question was not accepted.
    pub retry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SshPromptKind {
    /// A host key met for the first time: accept or reject.
    HostKey,
    /// A password, a passphrase, a PIN: typed, never shown.
    Secret,
    /// Some other yes/no `ssh` asks.
    Confirm,
    /// Something to do elsewhere, such as touching a security key; `ssh`
    /// takes it down by itself.
    Notice,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostKeyFingerprint {
    /// The host as `ssh` names it, with its address when it gives one.
    pub host: String,
    /// `ED25519`, `ECDSA`, `RSA`…
    pub key_type: String,
    /// `SHA256:…`
    pub fingerprint: String,
}

/// What the renderer is told.
pub enum PromptEvent {
    Requested(SshPrompt),
    /// The question is over: answered, withdrawn, or its `ssh` gave up.
    Settled(String),
}

type Publisher = Box<dyn Fn(PromptEvent) + Send + Sync>;

static PUBLISHER: OnceLock<Publisher> = OnceLock::new();

/// Installs where questions are announced. Until then a question waits
/// unseen, and is listed once the renderer asks.
pub fn install(publish: impl Fn(PromptEvent) + Send + Sync + 'static) {
    let _ = PUBLISHER.set(Box::new(publish));
}

fn publish(event: PromptEvent) {
    if let Some(publish) = PUBLISHER.get() {
        publish(event);
    }
}

/// The environment one `ssh` needs to ask through Mewrk, and the session it
/// asks as: what a caller's own timeouts ask [`waiting`] about.
pub struct Prompting {
    pub session: String,
    env: Vec<(&'static str, OsString)>,
}

impl Prompting {
    pub fn apply(&self, command: &mut std::process::Command) {
        for (name, value) in &self.env {
            command.env(name, value);
        }
    }
}

/// Whether `ssh` asks through Mewrk at all in this process. Decided once:
/// the connection arguments (`BatchMode`) and the environment of every `ssh`
/// must agree on it.
pub fn available() -> bool {
    broker().is_some()
}

/// What an `ssh` to `host` needs in its environment to ask through Mewrk,
/// or `None` when it cannot ([`available`]). Whether its questions may be
/// shown is decided by the thread starting it ([`unattended`], [`for_link`]).
pub fn prompting(host: &str, port: u16, identity_file: &str) -> Option<Prompting> {
    let broker = broker()?;
    let executable = HELPER.get()?.clone();
    let session = uuid::Uuid::new_v4().simple().to_string();
    let origin = ASKING.with(std::cell::Cell::get);
    let env = vec![
        ("SSH_ASKPASS", executable.into_os_string()),
        // OpenSSH 8.4 and later ask through the program whether or not a
        // terminal is at hand. `DISPLAY` is left alone: the older rule it
        // stood for is not needed then, and a made-up one would set off X11
        // forwarding for a user whose configuration asks for it.
        ("SSH_ASKPASS_REQUIRE", "force".into()),
        (PORT_ENV, broker.port.to_string().into()),
        (TOKEN_ENV, broker.token.clone().into()),
        (SESSION_ENV, session.clone().into()),
        (MACHINE_ENV, machine_key(host, port, identity_file).into()),
        (ORIGIN_ENV, origin.wire().into()),
    ];
    Some(Prompting { session, env })
}

/// Someone who asked for a machine's work, waiting on its link: while one is,
/// the link's own logins may ask the user. Dropped when they stop waiting.
pub struct Demand {
    machine: String,
    /// A login to the machine was withheld before this demand began — it needed
    /// the user and nobody was waiting. The link that tried it has failed for
    /// want of an answer, not because the machine is down, and is better
    /// replaced than waited out.
    pub withheld: bool,
}

impl Drop for Demand {
    fn drop(&mut self) {
        if let Some(broker) = broker() {
            broker.lock().end_demand(&self.machine);
        }
    }
}

/// Records that this thread is waiting on the link to `host` for something
/// the user asked for. `None` when nothing is recorded: the thread is not the
/// user's ([`unattended`], [`for_link`]), or no broker runs.
pub fn demand(host: &str, port: u16, identity_file: &str) -> Option<Demand> {
    if ASKING.with(std::cell::Cell::get) != Asking::User {
        return None;
    }
    let broker = broker()?;
    let machine = machine_key(host, port, identity_file);
    let withheld = broker.lock().begin_demand(&machine);
    Some(Demand { machine, withheld })
}

/// Why a login to `host` started on this thread should not be tried now, or
/// `None` when it may go ahead. A login once withheld is not retried by work
/// nobody is waiting for: every try would reach the machine only to fail its
/// authentication — which a server may count against the account — and come
/// back with the same answer.
pub fn login_deferred(host: &str, port: u16, identity_file: &str) -> Option<String> {
    let broker = broker()?;
    let machine = machine_key(host, port, identity_file);
    let asking = ASKING.with(std::cell::Cell::get);
    broker.lock().deferred(&machine, asking).then(|| {
        let host = machine_label(&machine);
        crate::ui_text::ui_text!(
            "登录 {host} 需要你提供信息；下次用到这台机器时 Mewrk 会询问",
            "Signing in to {host} needs something from you; Mewrk will ask the next time you use this machine"
        )
    })
}

/// A login to that machine was refused (`said` is what `ssh` wrote): a
/// remembered password or passphrase that let it get that far is not the
/// right one any more, so the next connection asks again instead of failing
/// with it until Mewrk quits.
pub fn login_refused(host: &str, port: u16, identity_file: &str, said: &str) {
    if !said.contains("Permission denied") && !said.contains("Authentication failed") {
        return;
    }
    if let Some(broker) = broker() {
        broker
            .lock()
            .forget_secrets(&machine_key(host, port, identity_file));
    }
}

/// Whether a question the `ssh` of `session` asked is with the user now. A
/// caller bounding that `ssh` by time does not count this time against it.
pub fn waiting(session: &str) -> bool {
    broker().is_some_and(|broker| {
        broker
            .lock()
            .pending
            .iter()
            .any(|pending| pending.answer.is_none() && pending.sessions.contains(session))
    })
}

/// Whether a question about the machine `host` reaches is with the user now.
pub fn asking(host: &str, port: u16, identity_file: &str) -> bool {
    let machine = machine_key(host, port, identity_file);
    broker().is_some_and(|broker| {
        broker
            .lock()
            .pending
            .iter()
            .any(|pending| pending.answer.is_none() && pending.key.0 == machine)
    })
}

/// Whether the user turned down a question about that machine, so its
/// connections fail until they ask for one again.
pub fn declined(host: &str, port: u16, identity_file: &str) -> bool {
    let machine = machine_key(host, port, identity_file);
    broker().is_some_and(|broker| broker.lock().declined_now(&machine))
}

/// Lets the next connection to that machine ask again: the user asked for one
/// from its settings.
pub fn forget_declines(host: &str, port: u16, identity_file: &str) {
    let machine = machine_key(host, port, identity_file);
    if let Some(broker) = broker() {
        let mut state = broker.lock();
        state.declined.remove(&machine);
        state.withheld.remove(&machine);
    }
}

/// The questions waiting for the user, for a renderer that just connected.
pub fn pending() -> Vec<SshPrompt> {
    broker().map(|broker| broker.list()).unwrap_or_default()
}

/// The user's answer to question `id`: `None` turns it down.
pub fn answer(id: &str, answer: Option<String>) -> Result<(), String> {
    let broker = broker().ok_or_else(|| {
        crate::ui_text::pick("这个 SSH 询问已经结束", "This SSH question is already over").to_owned()
    })?;
    broker.answer(id, answer.map(Zeroizing::new))
}

/// The identity a question is remembered under: the endpoint the `ssh` was
/// started for. Not secret — it is the destination on the command line.
fn machine_key(host: &str, port: u16, identity_file: &str) -> String {
    format!("{host}\u{1f}{port}\u{1f}{identity_file}")
}

/// What the user sees a machine called: its destination, with a port that is
/// not the default.
fn machine_label(machine: &str) -> String {
    let mut parts = machine.split('\u{1f}');
    let host = parts.next().unwrap_or_default();
    match parts.next().and_then(|port| port.parse::<u16>().ok()) {
        Some(port) if port != 0 && port != 22 => format!("{host}:{port}"),
        _ => host.to_owned(),
    }
}

/// The executable `ssh` runs to ask: this one.
static HELPER: OnceLock<std::path::PathBuf> = OnceLock::new();
static BROKER: OnceLock<Option<Arc<Broker>>> = OnceLock::new();

fn broker() -> Option<&'static Arc<Broker>> {
    BROKER
        .get_or_init(|| {
            // A test binary is not Mewrk: it cannot answer as `SSH_ASKPASS`,
            // so tests keep `BatchMode` unless they run a broker of their own.
            if cfg!(test) {
                return None;
            }
            let executable = std::env::current_exe().ok()?;
            let broker = Broker::start().ok()?;
            let _ = HELPER.set(executable);
            Some(broker)
        })
        .as_ref()
}

/// The listener questions arrive at, and the state of every one in flight.
struct Broker {
    port: u16,
    token: String,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Passwords and passphrases given this run, by machine and question.
    secrets: HashMap<(String, String), Zeroizing<String>>,
    pending: Vec<Pending>,
    /// The questions each `ssh` asked, with when it last asked.
    sessions: HashMap<String, (HashSet<String>, Instant)>,
    /// Machines whose question the user turned down, and when.
    declined: HashMap<String, Instant>,
    /// How many callers who asked for each machine's work are waiting on its
    /// link now ([`demand`]).
    demand: HashMap<String, usize>,
    /// Machines a login to which was withheld — it needed the user and nobody
    /// was waiting — and not tried for anyone since.
    withheld: HashSet<String>,
    next_id: u64,
}

impl State {
    fn declined_now(&self, machine: &str) -> bool {
        self.declined
            .get(machine)
            .is_some_and(|at| at.elapsed() < DECLINE_MEMORY)
    }

    fn waited_on(&self, machine: &str) -> bool {
        self.demand.get(machine).is_some_and(|count| *count > 0)
    }

    /// Whether a question from an `ssh` started for `origin` may be shown.
    fn attended(&self, machine: &str, origin: Option<&str>) -> bool {
        match origin {
            Some("nobody") => false,
            Some("link") => self.waited_on(machine),
            // The user's, and a helper that does not say.
            _ => true,
        }
    }

    /// Returns whether a login to the machine had been withheld until now.
    fn begin_demand(&mut self, machine: &str) -> bool {
        *self.demand.entry(machine.to_owned()).or_default() += 1;
        self.withheld.remove(machine)
    }

    fn end_demand(&mut self, machine: &str) {
        if let Some(count) = self.demand.get_mut(machine) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.demand.remove(machine);
            }
        }
    }

    fn deferred(&self, machine: &str, asking: Asking) -> bool {
        self.withheld.contains(machine)
            && match asking {
                Asking::User => false,
                Asking::Nobody => true,
                Asking::Link => !self.waited_on(machine),
            }
    }

    fn forget_secrets(&mut self, machine: &str) {
        self.secrets.retain(|(known, _), _| known != machine);
    }
}

struct Pending {
    prompt: SshPrompt,
    /// The machine and the question, which another `ssh` asking the same
    /// shares rather than putting a second copy on screen.
    key: (String, String),
    sessions: HashSet<String>,
    waiters: usize,
    asked_at: Instant,
    /// Set once answered: the answer, or `None` for turned down.
    answer: Option<Option<Zeroizing<String>>>,
}

/// What the helper sends: one line of JSON.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskRequest {
    token: String,
    session: String,
    machine: String,
    prompt: String,
    #[serde(default)]
    mode: Option<String>,
    /// Who the `ssh` logs in for, as [`Asking::wire`] spells it.
    #[serde(default)]
    origin: Option<String>,
}

/// What it gets back: the answer, or none when there is nothing to tell `ssh`.
#[derive(Serialize, Deserialize)]
struct AskReply {
    answer: Option<String>,
}

impl Broker {
    fn start() -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let broker = Arc::new(Self {
            port: listener.local_addr()?.port(),
            token: uuid::Uuid::new_v4().simple().to_string(),
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let serving = Arc::clone(&broker);
        std::thread::Builder::new()
            .name("ssh-askpass".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let broker = Arc::clone(&serving);
                    std::thread::spawn(move || broker.serve(stream));
                }
            })?;
        Ok(broker)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One helper's question, start to answer.
    fn serve(&self, stream: TcpStream) {
        let Ok(reader) = stream.try_clone() else {
            return;
        };
        let mut line = Zeroizing::new(String::new());
        if BufReader::new(reader.take(MAX_MESSAGE_BYTES))
            .read_line(&mut line)
            .is_err()
        {
            return;
        }
        let Ok(request) = serde_json::from_str::<AskRequest>(&line) else {
            return;
        };
        // Any account on this computer can reach a loopback port; only an
        // `ssh` this process started carries the token in its environment.
        if request.token != self.token {
            return;
        }
        let peer = stream.try_clone().ok();
        let still_there = || peer.as_ref().is_some_and(peer_is_there);
        let answer = self.ask(&request, &still_there);
        let reply = AskReply {
            answer: answer.as_ref().map(|answer| answer.as_str().to_owned()),
        };
        let bytes = serde_json::to_vec(&reply).map(Zeroizing::new);
        // The copies a secret passes through are wiped, not just dropped.
        drop(reply.answer.map(Zeroizing::new));
        if let Ok(mut bytes) = bytes {
            bytes.push(b'\n');
            let mut writer = stream;
            let _ = writer.write_all(&bytes);
        }
    }

    /// Puts `request` in front of the user, unless it is already answered —
    /// a remembered secret, a machine they turned down — and waits for them.
    fn ask(&self, request: &AskRequest, still_there: &dyn Fn() -> bool) -> Option<Zeroizing<String>> {
        let kind = classify(&request.prompt, request.mode.as_deref());
        let key = (request.machine.clone(), request.prompt.clone());
        let mut state = self.lock();
        state
            .sessions
            .retain(|_, (_, asked)| asked.elapsed() < SESSION_MEMORY);
        let (asked, last) = state
            .sessions
            .entry(request.session.clone())
            .or_insert_with(|| (HashSet::new(), Instant::now()));
        *last = Instant::now();
        // The same `ssh` asking the same question again means the last answer
        // did not work.
        let repeated = !asked.insert(request.prompt.clone());
        let retry = kind == SshPromptKind::Secret && repeated;
        if retry {
            state.secrets.remove(&key);
        } else if kind == SshPromptKind::Secret {
            if let Some(secret) = state.secrets.get(&key) {
                return Some(secret.clone());
            }
        }
        if kind != SshPromptKind::Notice && state.declined_now(&request.machine) {
            return None;
        }
        let shared = state
            .pending
            .iter()
            .position(|pending| pending.answer.is_none() && pending.key == key);
        // A question already on screen is joined whoever asks it: the user is
        // answering it anyway. Otherwise only someone waiting may ask. A login
        // nobody is waiting for fails quietly, and is remembered so that it is
        // not tried again until someone is.
        if shared.is_none() && !state.attended(&request.machine, request.origin.as_deref()) {
            if kind != SshPromptKind::Notice {
                state.withheld.insert(request.machine.clone());
            }
            return None;
        }
        // Somebody is being asked, so the login is no longer waiting for them.
        state.withheld.remove(&request.machine);
        let id = match shared {
            Some(index) => {
                let pending = &mut state.pending[index];
                pending.waiters += 1;
                pending.sessions.insert(request.session.clone());
                pending.prompt.retry |= retry;
                pending.prompt.id.clone()
            }
            None => {
                state.next_id += 1;
                let prompt = SshPrompt {
                    id: format!("ssh-prompt-{}", state.next_id),
                    machine: machine_label(&request.machine),
                    kind,
                    prompt: request.prompt.trim_end().to_owned(),
                    host_key: (kind == SshPromptKind::HostKey)
                        .then(|| parse_host_key(&request.prompt))
                        .flatten(),
                    retry,
                };
                state.pending.push(Pending {
                    prompt: prompt.clone(),
                    key: key.clone(),
                    sessions: HashSet::from([request.session.clone()]),
                    waiters: 1,
                    asked_at: Instant::now(),
                    answer: None,
                });
                let id = prompt.id.clone();
                drop(state);
                publish(PromptEvent::Requested(prompt));
                state = self.lock();
                id
            }
        };
        loop {
            let index = state.pending.iter().position(|pending| pending.prompt.id == id)?;
            if let Some(answer) = &state.pending[index].answer {
                let answer = answer.clone();
                let pending = &mut state.pending[index];
                pending.waiters -= 1;
                if pending.waiters == 0 {
                    state.pending.remove(index);
                }
                return answer;
            }
            let overdue = state.pending[index].asked_at.elapsed() >= PROMPT_PATIENCE;
            if overdue || !still_there() {
                // This `ssh` gave up, or nobody answered in time. The question
                // stays for whoever else is waiting on it.
                let pending = &mut state.pending[index];
                pending.waiters -= 1;
                pending.sessions.remove(&request.session);
                if pending.waiters == 0 {
                    state.pending.remove(index);
                    drop(state);
                    publish(PromptEvent::Settled(id));
                }
                return None;
            }
            state = self
                .changed
                .wait_timeout(state, WAIT_POLL)
                .map(|(state, _)| state)
                .unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    fn answer(&self, id: &str, answer: Option<Zeroizing<String>>) -> Result<(), String> {
        let mut state = self.lock();
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.prompt.id == id && pending.answer.is_none())
        else {
            return Err(crate::ui_text::pick("这个 SSH 询问已经结束", "This SSH question is already over").to_owned());
        };
        let kind = state.pending[index].prompt.kind;
        let key = state.pending[index].key.clone();
        let answer = match (kind, answer) {
            // A notice asks for nothing; dismissing it decides nothing.
            (SshPromptKind::Notice, _) => None,
            (SshPromptKind::HostKey | SshPromptKind::Confirm, Some(_)) => Some(Zeroizing::new("yes".to_owned())),
            (SshPromptKind::Secret, Some(secret)) => {
                state.secrets.insert(key.clone(), secret.clone());
                Some(secret)
            }
            (_, None) => {
                state.declined.insert(key.0.clone(), Instant::now());
                None
            }
        };
        state.pending[index].answer = Some(answer);
        drop(state);
        self.changed.notify_all();
        publish(PromptEvent::Settled(id.to_owned()));
        Ok(())
    }

    fn list(&self) -> Vec<SshPrompt> {
        self.lock()
            .pending
            .iter()
            .filter(|pending| pending.answer.is_none())
            .map(|pending| pending.prompt.clone())
            .collect()
    }
}

/// Whether the helper on the other end of `stream` is still connected: `ssh`
/// kills it when it stops waiting, which closes the socket.
fn peer_is_there(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let mut byte = [0u8; 1];
    let there = match stream.peek(&mut byte) {
        Ok(0) => false,
        Ok(_) => true,
        Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
    };
    let _ = stream.set_nonblocking(false);
    there
}

/// Which kind of question `ssh` is asking, from its words and the mode
/// OpenSSH gives a question that is not a secret.
fn classify(prompt: &str, mode: Option<&str>) -> SshPromptKind {
    if prompt.contains("continue connecting") && prompt.contains("fingerprint") {
        return SshPromptKind::HostKey;
    }
    match mode {
        Some("confirm") => SshPromptKind::Confirm,
        Some("none") => SshPromptKind::Notice,
        _ => SshPromptKind::Secret,
    }
}

/// The host, key type and fingerprint out of OpenSSH's first-contact
/// question, in the wording of older releases and of current ones (which add
/// a colon):
///
/// ```text
/// The authenticity of host 'devbox (192.0.2.7)' can't be established.
/// ED25519 key fingerprint is SHA256:abc….
/// ED25519 key fingerprint is: SHA256:abc…
/// ```
fn parse_host_key(prompt: &str) -> Option<HostKeyFingerprint> {
    let host = prompt
        .split_once("host '")
        .and_then(|(_, rest)| rest.split_once('\''))
        .map(|(host, _)| host.to_owned())?;
    let line = prompt.lines().find(|line| line.contains(" key fingerprint is"))?;
    let (key_type, fingerprint) = line.split_once(" key fingerprint is")?;
    Some(HostKeyFingerprint {
        host,
        key_type: key_type.trim().to_owned(),
        fingerprint: fingerprint
            .trim_start_matches(':')
            .trim()
            .trim_end_matches('.')
            .to_owned(),
    })
}

/// The `SSH_ASKPASS` side: when this executable was started by `ssh` to ask
/// something ([`prompting`] put the broker's port in its environment), asks
/// the broker and prints the answer, returning the exit code. `None` for an
/// ordinary launch, which goes on to start the application.
///
/// Called first thing in `main`, before anything the application loads.
pub fn askpass_main() -> Option<i32> {
    let port = std::env::var(PORT_ENV).ok()?;
    let request = AskRequest {
        token: std::env::var(TOKEN_ENV).unwrap_or_default(),
        session: std::env::var(SESSION_ENV).unwrap_or_default(),
        machine: std::env::var(MACHINE_ENV).unwrap_or_default(),
        prompt: std::env::args().nth(1).unwrap_or_default(),
        mode: std::env::var(OPENSSH_PROMPT_ENV).ok(),
        origin: std::env::var(ORIGIN_ENV).ok(),
    };
    let Ok(port) = port.parse::<u16>() else {
        return Some(1);
    };
    Some(match ask_broker(port, &request) {
        Ok(Some(answer)) => {
            let mut stdout = std::io::stdout().lock();
            let written = stdout
                .write_all(answer.as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .and_then(|()| stdout.flush());
            if written.is_ok() {
                0
            } else {
                1
            }
        }
        // Nothing to say: `ssh` reads a failed helper as no answer.
        Ok(None) | Err(_) => 1,
    })
}

fn ask_broker(port: u16, request: &AskRequest) -> std::io::Result<Option<Zeroizing<String>>> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
    let mut line = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()?;
    let mut reply = Zeroizing::new(String::new());
    BufReader::new(stream.take(MAX_MESSAGE_BYTES)).read_line(&mut reply)?;
    let reply: AskReply = serde_json::from_str(&reply).map_err(std::io::Error::other)?;
    Ok(reply.answer.map(Zeroizing::new))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_KEY_PROMPT: &str = "The authenticity of host 'devbox (192.0.2.7)' can't be established.\nED25519 key fingerprint is SHA256:8Fz0bQh2Yc0p1X7nqZ9Wl+Kq3sE4tR5uV6wX7yZ8aB0.\nThis key is not known by any other names.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";

    fn request(broker: &Broker, session: &str, prompt: &str) -> AskRequest {
        AskRequest {
            token: broker.token.clone(),
            session: session.into(),
            machine: machine_key("dev@devbox", 0, ""),
            prompt: prompt.into(),
            mode: None,
            origin: None,
        }
    }

    /// Asks on a thread of its own, as a helper connection would, and returns
    /// the question's id once it is on screen.
    fn ask_in_background(
        broker: &Arc<Broker>,
        request: AskRequest,
    ) -> std::thread::JoinHandle<Option<Zeroizing<String>>> {
        let broker = Arc::clone(broker);
        std::thread::spawn(move || broker.ask(&request, &|| true))
    }

    fn wait_for_prompt(broker: &Broker, count: usize) -> Vec<SshPrompt> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let listed = broker.list();
            if listed.len() >= count || Instant::now() > deadline {
                return listed;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn questions_are_told_apart_and_the_host_key_is_read_out_of_its_question() {
        assert_eq!(classify(HOST_KEY_PROMPT, None), SshPromptKind::HostKey);
        assert_eq!(classify("dev@devbox's password: ", None), SshPromptKind::Secret);
        assert_eq!(
            classify("Enter passphrase for key '/Users/me/.ssh/id_ed25519': ", None),
            SshPromptKind::Secret
        );
        assert_eq!(classify("Allow use of key?", Some("confirm")), SshPromptKind::Confirm);
        assert_eq!(classify("Confirm user presence for key", Some("none")), SshPromptKind::Notice);
        assert_eq!(
            parse_host_key(HOST_KEY_PROMPT),
            Some(HostKeyFingerprint {
                host: "devbox (192.0.2.7)".into(),
                key_type: "ED25519".into(),
                fingerprint: "SHA256:8Fz0bQh2Yc0p1X7nqZ9Wl+Kq3sE4tR5uV6wX7yZ8aB0".into(),
            })
        );
        // OpenSSH 10 words it with a colon, as a real first contact showed.
        let current = "The authenticity of host '[127.0.0.1]:22999 ([127.0.0.1]:22999)' can't be established.\nED25519 key fingerprint is: SHA256:sTp7+ayipLpsg2LjE/u2clSwV4EYZt7SiXCVm86Etgo\nThis key is not known by any other names.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";
        assert_eq!(classify(current, None), SshPromptKind::HostKey);
        assert_eq!(
            parse_host_key(current).map(|key| (key.host, key.fingerprint)),
            Some((
                "[127.0.0.1]:22999 ([127.0.0.1]:22999)".into(),
                "SHA256:sTp7+ayipLpsg2LjE/u2clSwV4EYZt7SiXCVm86Etgo".into()
            ))
        );
        assert_eq!(machine_label(&machine_key("devbox", 2222, "")), "devbox:2222");
        assert_eq!(machine_label(&machine_key("dev@devbox", 22, "")), "dev@devbox");
    }

    /// A password is asked once, shared by a second connection asking the
    /// same meanwhile, remembered for the next one, and asked again when the
    /// same connection asks for it twice — the first answer was wrong.
    #[test]
    fn a_password_is_asked_once_remembered_and_asked_again_when_it_was_wrong() {
        let broker = Broker::start().unwrap();
        let prompt = "dev@devbox's password: ";
        let first = ask_in_background(&broker, request(&broker, "s1", prompt));
        let second = ask_in_background(&broker, request(&broker, "s2", prompt));
        let shown = wait_for_prompt(&broker, 1);
        assert_eq!(shown.len(), 1, "two connections asking the same share one question");
        assert_eq!(shown[0].kind, SshPromptKind::Secret);
        assert_eq!(shown[0].machine, "dev@devbox");
        assert!(!shown[0].retry);
        // Both connections are waiting on the user, which their own timeouts
        // must not count.
        let deadline = Instant::now() + Duration::from_secs(5);
        while broker.list().first().map(|_| broker.lock().pending[0].waiters) != Some(2) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(broker.lock().pending[0].sessions.contains("s2"));
        broker.answer(&shown[0].id, Some(Zeroizing::new("hunter2".into()))).unwrap();
        assert_eq!(first.join().unwrap().as_deref().map(String::as_str), Some("hunter2"));
        assert_eq!(second.join().unwrap().as_deref().map(String::as_str), Some("hunter2"));
        assert!(broker.list().is_empty());

        // A new connection is answered from memory, without asking.
        let remembered = broker.ask(&request(&broker, "s3", prompt), &|| true);
        assert_eq!(remembered.as_deref().map(String::as_str), Some("hunter2"));

        // The same connection asking again: the answer was wrong.
        let again = ask_in_background(&broker, request(&broker, "s3", prompt));
        let shown = wait_for_prompt(&broker, 1);
        assert!(shown[0].retry, "the user is told the last answer did not work");
        broker.answer(&shown[0].id, Some(Zeroizing::new("correct horse".into()))).unwrap();
        assert_eq!(again.join().unwrap().as_deref().map(String::as_str), Some("correct horse"));
    }

    /// Accepting a host key answers `yes`, which `ssh` takes as leave to write
    /// it to known_hosts; turning a question down makes the machine's
    /// connections fail without asking again for a while.
    #[test]
    fn a_host_key_is_accepted_with_yes_and_a_declined_machine_is_not_asked_again() {
        let broker = Broker::start().unwrap();
        let accepted = ask_in_background(&broker, request(&broker, "s1", HOST_KEY_PROMPT));
        let shown = wait_for_prompt(&broker, 1);
        assert_eq!(shown[0].kind, SshPromptKind::HostKey);
        assert_eq!(shown[0].host_key.as_ref().unwrap().key_type, "ED25519");
        broker.answer(&shown[0].id, Some(Zeroizing::new(String::new()))).unwrap();
        assert_eq!(accepted.join().unwrap().as_deref().map(String::as_str), Some("yes"));

        let declined = ask_in_background(&broker, request(&broker, "s2", "dev@devbox's password: "));
        let shown = wait_for_prompt(&broker, 1);
        broker.answer(&shown[0].id, None).unwrap();
        assert_eq!(declined.join().unwrap(), None);
        assert!(broker.lock().declined_now(&machine_key("dev@devbox", 0, "")));
        assert_eq!(broker.ask(&request(&broker, "s3", HOST_KEY_PROMPT), &|| true), None);
        assert!(broker.list().is_empty(), "nothing was put on screen");
        assert!(broker.answer("ssh-prompt-99", None).is_err());
    }

    /// Only a login somebody is waiting for asks. Work nobody asked for fails
    /// quietly and is not tried again by such work; a machine's link asks only
    /// while somebody waits on it; a question already on screen is joined by
    /// anyone; and a turned-down question is asked again once a moment has
    /// passed.
    #[test]
    fn only_a_login_somebody_waits_for_asks() {
        let broker = Broker::start().unwrap();
        let machine = machine_key("dev@devbox", 0, "");
        let prompt = "dev@devbox's password: ";
        let from = |session: &str, origin: &str| {
            let mut request = request(&broker, session, prompt);
            request.origin = Some(origin.into());
            request
        };

        assert_eq!(broker.ask(&from("s1", "nobody"), &|| true), None);
        assert!(broker.list().is_empty(), "nothing was put on screen");
        {
            let state = broker.lock();
            assert!(state.deferred(&machine, Asking::Nobody), "not tried again unattended");
            assert!(state.deferred(&machine, Asking::Link), "nor by a link nobody waits on");
            assert!(!state.deferred(&machine, Asking::User));
            assert!(!state.declined_now(&machine), "withholding is not turning down");
        }
        assert_eq!(broker.ask(&from("s2", "link"), &|| true), None);
        assert!(broker.list().is_empty());

        // Somebody starts waiting on the machine, and learns its last login was
        // withheld: the link's login asks now.
        assert!(broker.lock().begin_demand(&machine));
        assert!(!broker.lock().deferred(&machine, Asking::Link));
        let link = ask_in_background(&broker, from("s3", "link"));
        let shown = wait_for_prompt(&broker, 1);
        assert_eq!(shown.len(), 1);
        // A poll asking the same meanwhile joins the question on screen.
        let poll = ask_in_background(&broker, from("s4", "nobody"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while broker.lock().pending[0].waiters != 2 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        broker.answer(&shown[0].id, Some(Zeroizing::new("hunter2".into()))).unwrap();
        assert_eq!(link.join().unwrap().as_deref().map(String::as_str), Some("hunter2"));
        assert_eq!(poll.join().unwrap().as_deref().map(String::as_str), Some("hunter2"));
        broker.lock().end_demand(&machine);
        assert!(broker.lock().demand.is_empty());

        // Remembered for anyone, until a login is refused with it.
        let remembered = broker.ask(&from("s5", "nobody"), &|| true);
        assert_eq!(remembered.as_deref().map(String::as_str), Some("hunter2"));
        broker.lock().forget_secrets(&machine);
        assert_eq!(broker.ask(&from("s6", "nobody"), &|| true), None);

        // Turned down: not asked again at once, asked again a while later.
        let asked = ask_in_background(&broker, from("s7", "user"));
        let shown = wait_for_prompt(&broker, 1);
        broker.answer(&shown[0].id, None).unwrap();
        assert_eq!(asked.join().unwrap(), None);
        assert_eq!(broker.ask(&from("s8", "user"), &|| true), None);
        assert!(broker.list().is_empty());
        let long_ago = Instant::now()
            .checked_sub(DECLINE_MEMORY + Duration::from_secs(1))
            .unwrap();
        broker.lock().declined.insert(machine.clone(), long_ago);
        let again = ask_in_background(&broker, from("s9", "user"));
        let shown = wait_for_prompt(&broker, 1);
        assert_eq!(shown.len(), 1, "the next thing the user does asks again");
        broker.answer(&shown[0].id, Some(Zeroizing::new("hunter2".into()))).unwrap();
        assert_eq!(again.join().unwrap().as_deref().map(String::as_str), Some("hunter2"));
    }

    /// A question whose `ssh` went away is taken down, so a dialog never
    /// outlives the connection that asked it.
    #[test]
    fn a_question_is_withdrawn_when_its_ssh_stops_waiting() {
        let broker = Broker::start().unwrap();
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watched = Arc::clone(&gone);
        let asking = {
            let broker = Arc::clone(&broker);
            let request = request(&broker, "s1", "Enter PIN for key: ");
            std::thread::spawn(move || {
                broker.ask(&request, &|| !watched.load(std::sync::atomic::Ordering::SeqCst))
            })
        };
        assert_eq!(wait_for_prompt(&broker, 1).len(), 1);
        gone.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(asking.join().unwrap(), None);
        assert!(broker.list().is_empty());
        assert!(!broker.lock().declined_now(&machine_key("dev@devbox", 0, "")));
    }

    /// Mewrk's own executable run the way `ssh` runs `SSH_ASKPASS`: the
    /// question as its one argument, the broker in its environment, the answer
    /// on its standard output. Needs a built executable:
    /// `MEWRK_ASKPASS_EXECUTABLE=target/debug/mewrk cargo test -- --ignored askpass`.
    #[test]
    #[ignore = "needs a built Mewrk executable in MEWRK_ASKPASS_EXECUTABLE"]
    fn the_executable_answers_as_askpass() {
        let executable = std::env::var_os("MEWRK_ASKPASS_EXECUTABLE").expect("MEWRK_ASKPASS_EXECUTABLE");
        let broker = Broker::start().unwrap();
        let mut command = std::process::Command::new(executable);
        command
            .arg("dev@devbox's password: ")
            .env(PORT_ENV, broker.port.to_string())
            .env(TOKEN_ENV, &broker.token)
            .env(SESSION_ENV, "s1")
            .env(MACHINE_ENV, machine_key("dev@devbox", 0, ""))
            .stdout(std::process::Stdio::piped());
        let child = command.spawn().unwrap();
        let shown = wait_for_prompt(&broker, 1);
        assert_eq!(shown.len(), 1);
        broker.answer(&shown[0].id, Some(Zeroizing::new("hunter2".into()))).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hunter2\n");
    }

    /// A real first connection, end to end: OpenSSH asks Mewrk's executable
    /// to confirm the unknown host key and for the key's passphrase, the
    /// broker puts both on "screen" and is answered, and `ssh` connects and
    /// records the key; the next connection asks nothing at all — the key is
    /// known and the passphrase remembered. Needs a built executable
    /// (`MEWRK_ASKPASS_EXECUTABLE`) and an sshd on 127.0.0.1 that accepts the
    /// key `MEWRK_ASKPASS_SSH_KEY` (passphrase `MEWRK_ASKPASS_SSH_PASSPHRASE`)
    /// on port `MEWRK_ASKPASS_SSH_PORT`.
    #[test]
    #[ignore = "needs a built Mewrk executable and a local sshd"]
    fn a_first_connection_asks_through_mewrk_and_the_next_one_does_not() {
        let variable = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        let executable = variable("MEWRK_ASKPASS_EXECUTABLE");
        let key = variable("MEWRK_ASKPASS_SSH_KEY");
        let passphrase = variable("MEWRK_ASKPASS_SSH_PASSPHRASE");
        let port: u16 = variable("MEWRK_ASKPASS_SSH_PORT").parse().unwrap();
        let known_hosts = tempfile::NamedTempFile::new().unwrap();
        let broker = Broker::start().unwrap();
        let user = std::env::var("USER").unwrap();
        let connect = |session: &str| {
            let mut command = std::process::Command::new("ssh");
            command
                .args(connection_args_for_test(&key, known_hosts.path(), port))
                .arg(format!("{user}@127.0.0.1"))
                .arg("echo connected")
                .env("SSH_ASKPASS", &executable)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(PORT_ENV, broker.port.to_string())
                .env(TOKEN_ENV, &broker.token)
                .env(SESSION_ENV, session)
                .env(MACHINE_ENV, machine_key("127.0.0.1", port, &key))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            command.spawn().unwrap()
        };
        let first = connect("s1");
        let host_key = wait_for_prompt(&broker, 1);
        assert_eq!(host_key[0].kind, SshPromptKind::HostKey, "{:?}", host_key[0]);
        assert!(host_key[0].host_key.as_ref().unwrap().fingerprint.starts_with("SHA256:"));
        broker.answer(&host_key[0].id, Some(Zeroizing::new(String::new()))).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let secret = loop {
            let listed = broker.list();
            if let Some(prompt) = listed.into_iter().find(|prompt| prompt.kind == SshPromptKind::Secret) {
                break prompt;
            }
            assert!(Instant::now() < deadline, "no passphrase question");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(secret.prompt.contains("passphrase"), "{}", secret.prompt);
        broker.answer(&secret.id, Some(Zeroizing::new(passphrase))).unwrap();
        let output = first.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "connected\n", "{}", String::from_utf8_lossy(&output.stderr));
        assert!(std::fs::read_to_string(known_hosts.path()).unwrap().contains(&format!("[127.0.0.1]:{port}")));

        let second = connect("s2").wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&second.stdout), "connected\n");
        assert!(broker.list().is_empty());
    }

    #[cfg(test)]
    fn connection_args_for_test(key: &str, known_hosts: &std::path::Path, port: u16) -> Vec<String> {
        let mut args = crate::run_environment::tests_connection_args("127.0.0.1", port, key);
        // Everything but the destination, which the caller adds with the user.
        args.truncate(args.len() - 1);
        let mut isolated = vec![
            "-o".to_owned(),
            format!("UserKnownHostsFile={}", known_hosts.display()),
            "-o".to_owned(),
            "IdentitiesOnly=yes".to_owned(),
            "-o".to_owned(),
            "IdentityAgent=none".to_owned(),
        ];
        isolated.extend(args);
        isolated
    }

    /// The helper's own exchange with the broker, over the loopback socket:
    /// a request with the wrong token is never answered.
    #[test]
    fn the_helper_asks_over_the_socket_and_needs_the_token() {
        let broker = Broker::start().unwrap();
        let port = broker.port;
        let mut wrong = request(&broker, "s1", "password: ");
        wrong.token = "not-the-token".into();
        assert!(ask_broker(port, &wrong).is_err());
        assert!(broker.list().is_empty());

        let helper = {
            let right = request(&broker, "s1", "password: ");
            std::thread::spawn(move || ask_broker(port, &right).unwrap())
        };
        let shown = wait_for_prompt(&broker, 1);
        assert_eq!(shown.len(), 1);
        broker.answer(&shown[0].id, Some(Zeroizing::new("secret".into()))).unwrap();
        assert_eq!(helper.join().unwrap().as_deref().map(String::as_str), Some("secret"));
    }
}
