//! The whole app under scripted input: real engine, real tools, real sockets, no window and no GPU.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluent::theme::Theme;
use fluent::ui::{Input, Key, KeyPress, NoClipboard, Ui, UiMemory};
use fluent::{Scene, TextCtx};
use mewrk_core::*;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::app::{App, Page};
use crate::layout::PaneId;

const FRAME: f32 = 1.0 / 120.0;
const SIZE: (f32, f32) = (1400.0, 900.0);

fn text_reply(s: &str) -> Vec<String> {
    vec![
        serde_json::json!({"choices":[{"delta":{"content":s}}]}).to_string(),
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#.to_owned(),
        "[DONE]".to_owned(),
    ]
}

fn tool_reply(name: &str, args: serde_json::Value) -> Vec<String> {
    vec![
        serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":name,"arguments":args.to_string()}}]}}]}).to_string(),
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned(),
        "[DONE]".to_owned(),
    ]
}

/// One reply per request; a bare status stands in for a rejected request. The last one repeats.
#[derive(Clone)]
enum Reply {
    Sse(Vec<String>),
    Status(u16),
}

struct Server {
    base_url: String,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
}

fn serve(rt: &tokio::runtime::Runtime, replies: Vec<Reply>) -> Server {
    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let (b, served) = (bodies.clone(), Arc::new(AtomicUsize::new(0)));
    rt.spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (b, served, replies) = (b.clone(), served.clone(), replies.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let (head_end, len) = loop {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                            break (p + 4, head.lines().find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0));
                        }
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    while buf.len() < head_end + len {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    if let Ok(v) = serde_json::from_slice(&buf[head_end..head_end + len]) {
                        b.lock().push(v);
                    }
                    buf.drain(..head_end + len);
                    let n = served.fetch_add(1, Ordering::SeqCst);
                    match replies[n.min(replies.len() - 1)].clone() {
                        Reply::Status(code) => {
                            let body = r#"{"error":{"message":"nope"}}"#;
                            let _ = sock.write_all(format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await;
                        }
                        Reply::Sse(events) => {
                            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await;
                            for e in events {
                                let frame = format!("data: {e}\n\n");
                                let _ = sock.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes()).await;
                            }
                            let _ = sock.write_all(b"0\r\n\r\n").await;
                        }
                    }
                }
            });
        }
    });
    Server { base_url, bodies }
}

struct Rig {
    app: App,
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    time: f32,
    server: Server,
    dir: std::path::PathBuf,
    project: std::path::PathBuf,
    // Dropped last, so that nothing above outlives its tasks.
    _rt: tokio::runtime::Runtime,
}

impl Rig {
    fn new(replies: Vec<Reply>, permission: PermissionMode) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let server = serve(&rt, replies);
        let dir = std::env::temp_dir().join(format!("mewrk-app-{}", new_id()));
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut doc = Document::default();
        doc.projects.push(Project { id: "p".into(), name: "项目".into(), path: Some(project.clone()), collapsed: false });
        doc.settings.providers.push(ProviderConfig {
            id: "m".into(),
            name: "Mock".into(),
            kind: ProviderKind::OpenAiChat,
            base_url: server.base_url.clone(),
            models: vec![ModelProfile { id: "mock".into(), ..Default::default() }],
            resume_prefill: false,
            stall_secs: 5,
        });
        doc.settings.default_permission = permission;
        let store = Store::open(dir.join("data")).unwrap();
        store.save_document(&doc);
        store.flush();
        let notifier = Notifier::none();
        let engine = Engine::new(rt.handle().clone(), store.clone(), notifier.clone());
        let app = App::new(store, engine, notifier, Arc::new(|| {}));
        Self { app, scene: Scene::default(), text: TextCtx::new(2.0), mem: UiMemory::default(), input: Input::default(), time: 0.0, server, dir, project, _rt: rt }
    }

    fn frame(&mut self) {
        self.scene.clear();
        self.time += FRAME;
        let mut clipboard = NoClipboard;
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut clipboard, 2.0, SIZE, FRAME, self.time);
        self.app.show(&mut ui);
        self.input.end_frame();
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.frame();
        }
    }

    fn click(&mut self, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frames(2);
        self.input.down = true;
        self.input.pressed = true;
        self.frame();
        self.input.down = false;
        self.input.released = true;
        self.frames(2);
    }

    fn key(&mut self, key: Key, cmd: bool) {
        self.input.keys.push(KeyPress { key, shift: false, cmd, alt: false, ctrl: false });
        self.frame();
    }

    fn typed(&mut self, s: &str) {
        self.input.text.push_str(s);
        self.frame();
    }

    /// Where a widget is, by moving the pointer until it reports being hot. Keeps tests free of layout arithmetic.
    fn find(&mut self, id: u64) -> (f32, f32) {
        for y in (300..900).step_by(6) {
            for x in (300..1400).step_by(30) {
                self.input.pointer = Some((x as f32, y as f32));
                self.frame();
                if self.mem.hot == id {
                    return (x as f32, y as f32);
                }
            }
        }
        panic!("widget {id:#x} is not on screen");
    }

    fn run_until(&mut self, what: &str, mut done: impl FnMut(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(&self.app) {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            self.frame();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn active(&self) -> String {
        self.app.active.clone().unwrap()
    }

    fn idle(&mut self) {
        let id = self.active();
        self.run_until("the run to finish", |a| !a.live[&id].is_running());
    }

    fn texts(&self) -> Vec<String> {
        let id = self.active();
        self.app.live[&id].items.iter().filter_map(|i| if let Item::Text { text, .. } = i { Some(text.clone()) } else { None }).collect()
    }

    fn compose_and_send(&mut self, text: &str) {
        let id = self.active();
        self.app.live.get_mut(&id).unwrap().composer.edit.set_text(text);
        self.app.send();
        self.frame();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

#[test]
fn typing_and_enter_turn_a_draft_into_a_conversation() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("你好"))], PermissionMode::FullAccess);
    assert!(r.app.draft.is_some(), "a first launch opens on a new task");
    r.frames(3);
    r.typed("写个函数\n".trim_end());
    r.key(Key::Enter, false);
    r.idle();
    assert!(r.app.draft.is_none());
    let id = r.active();
    assert_eq!(r.app.doc.conversation(&id).unwrap().title, "写个函数");
    assert_eq!(r.texts(), ["你好"]);
    assert_eq!(r.app.doc.conversation(&id).unwrap().usage.input, 10);
    r.frames(3);
    r.app.store.flush();
    let saved = r.app.store.load_items(&id).unwrap();
    assert!(matches!(&saved[0], Item::User { text, .. } if text == "写个函数"));
    assert!(saved.iter().any(|i| matches!(i, Item::Text { text, .. } if text == "你好")));
}

#[test]
fn an_approval_is_answered_with_its_button_and_the_tool_really_runs() {
    let mut r = Rig::new(vec![Reply::Sse(tool_reply("write", serde_json::json!({"path":"made.txt","content":"内容"}))), Reply::Sse(text_reply("写好了"))], PermissionMode::Ask);
    r.compose_and_send("写个文件");
    let id = r.active();
    r.run_until("the approval card", |a| !a.live[&id].approvals.is_empty());
    assert!(!r.project.join("made.txt").exists(), "nothing happens before the user says so");
    r.frames(3);
    let (x, y) = r.find(0xB401);
    r.click(x, y);
    r.idle();
    assert_eq!(std::fs::read_to_string(r.project.join("made.txt")).unwrap(), "内容");
    assert_eq!(r.texts(), ["写好了"]);
    let tool = r.app.live[&id].items.iter().find_map(|i| if let Item::Tool(t) = i { Some(t.clone()) } else { None }).unwrap();
    assert_eq!(tool.status, ToolStatus::Done);
    assert_eq!(tool.meta.file.as_deref(), Some("made.txt"));
}

#[test]
fn denying_tells_the_model_and_writes_nothing() {
    let mut r = Rig::new(vec![Reply::Sse(tool_reply("write", serde_json::json!({"path":"no.txt","content":"x"}))), Reply::Sse(text_reply("好的"))], PermissionMode::Ask);
    r.compose_and_send("写个文件");
    let id = r.active();
    r.run_until("the approval card", |a| !a.live[&id].approvals.is_empty());
    r.frames(3);
    let (x, y) = r.find(0xB403);
    r.click(x, y);
    r.idle();
    assert!(!r.project.join("no.txt").exists());
    let bodies = r.server.bodies.lock();
    let result = bodies[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap().clone();
    assert!(result["content"].as_str().unwrap().contains("denied"), "{result}");
}

#[test]
fn a_message_sent_during_a_run_waits_for_its_turn() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("一")), Reply::Sse(text_reply("二"))], PermissionMode::FullAccess);
    r.compose_and_send("第一条");
    r.compose_and_send("第二条");
    let id = r.active();
    r.run_until("both answers", |a| {
        let n = a.live[&id].items.iter().filter(|i| matches!(i, Item::Text { .. })).count();
        n == 2 && !a.live[&id].is_running()
    });
    assert_eq!(r.texts(), ["一", "二"]);
    assert_eq!(r.server.bodies.lock().len(), 2);
}

#[test]
fn a_failure_shows_a_notice_that_retrying_clears() {
    let mut r = Rig::new(vec![Reply::Status(400), Reply::Sse(text_reply("好了"))], PermissionMode::FullAccess);
    r.compose_and_send("试试");
    r.idle();
    let id = r.active();
    assert!(matches!(r.app.live[&id].items.last(), Some(Item::Notice { level: NoticeLevel::Error, .. })));
    r.app.retry(&id);
    r.idle();
    assert_eq!(r.texts(), ["好了"]);
    assert!(!r.app.live[&id].items.iter().any(|i| matches!(i, Item::Notice { .. })));
}

#[test]
fn what_the_user_edits_or_deletes_is_what_the_model_gets() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("原来的回答")), Reply::Sse(text_reply("新回答"))], PermissionMode::FullAccess);
    r.compose_and_send("问题");
    r.idle();
    let id = r.active();
    let answer = r.app.live[&id].items.iter().find(|i| matches!(i, Item::Text { .. })).unwrap().id();
    r.app.begin_edit(&id, answer);
    r.app.item_editor.as_mut().unwrap().edit.set_text("改过的回答".to_owned());
    r.app.commit_edit();
    r.compose_and_send("继续");
    r.idle();
    {
        let bodies = r.server.bodies.lock();
        let text = bodies[1].to_string();
        assert!(text.contains("改过的回答") && !text.contains("原来的回答"), "{text}");
    }
    r.frames(2);
    r.app.store.flush();
    let saved = r.app.store.load_items(&id).unwrap();
    assert!(saved.iter().any(|i| matches!(i, Item::Text { text, .. } if text == "改过的回答")), "the edit reached the disk");

    r.app.delete_item(&id, answer);
    r.compose_and_send("再继续");
    r.idle();
    assert!(!r.server.bodies.lock()[2].to_string().contains("改过的回答"));
}

#[test]
fn regenerating_drops_what_came_after_and_asks_again() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("第一次")), Reply::Sse(text_reply("第二次"))], PermissionMode::FullAccess);
    r.compose_and_send("问题");
    r.idle();
    let id = r.active();
    let question = r.app.live[&id].items[0].id();
    r.app.regenerate(&id, question);
    r.idle();
    assert_eq!(r.texts(), ["第二次"]);
    assert!(!r.server.bodies.lock()[1].to_string().contains("第一次"));
}

#[test]
fn a_background_result_wakes_an_idle_conversation() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("一")), Reply::Sse(text_reply("收到"))], PermissionMode::FullAccess);
    r.compose_and_send("开始");
    r.idle();
    let id = r.active();
    r.app.live[&id].session.add_note("<task-notification>done</task-notification>".to_owned());
    r.run_until("the conversation to wake", |a| a.live[&id].is_running());
    r.idle();
    assert_eq!(r.texts(), ["一", "收到"]);
    assert!(r.server.bodies.lock()[1].to_string().contains("task-notification"));
}

#[test]
fn the_top_bar_toggles_panes_and_the_keyboard_opens_search_and_settings() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("一"))], PermissionMode::FullAccess);
    r.frames(2);
    let id = r.active();
    // The preview button: third from the right of the top bar.
    r.click(SIZE.0 - 12.0 - 34.0 - 16.0, 22.0);
    assert!(r.app.layouts[&id].is_open(PaneId::Preview));
    r.click(SIZE.0 - 12.0 - 34.0 - 16.0, 22.0);
    assert!(!r.app.layouts[&id].is_open(PaneId::Preview));

    r.key(Key::Char('k'), true);
    assert!(r.app.search.is_some());
    r.key(Key::Escape, false);
    assert!(r.app.search.is_none());

    r.key(Key::Char(','), true);
    assert!(r.app.page == Page::Settings);
    r.key(Key::Escape, false);
    assert!(r.app.page == Page::Chat);
}

#[test]
fn search_finds_a_conversation_and_opens_it() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("一"))], PermissionMode::FullAccess);
    r.compose_and_send("找得到的标题");
    r.idle();
    let first = r.active();
    r.key(Key::Char('n'), true);
    assert_ne!(r.active(), first);
    r.key(Key::Char('k'), true);
    r.typed("找得到");
    r.key(Key::Enter, false);
    assert_eq!(r.active(), first);
    assert!(r.app.search.is_none());
    assert!(r.app.draft.is_none(), "an empty new task leaves no trace");
}

#[test]
fn deleting_a_conversation_removes_its_file_and_moves_on() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("一"))], PermissionMode::FullAccess);
    r.compose_and_send("要删的");
    r.idle();
    let id = r.active();
    r.frames(2);
    r.app.store.flush();
    let path = r.app.store.dir().join("conversations").join(format!("{id}.jsonl"));
    assert!(path.exists());
    r.app.delete_conversation(&id);
    r.app.store.flush();
    assert!(!path.exists());
    assert!(r.app.draft.is_some(), "with nothing left, a new task opens");
}

#[test]
fn a_reopened_app_finds_its_conversations() {
    let mut r = Rig::new(vec![Reply::Sse(text_reply("记得"))], PermissionMode::FullAccess);
    r.compose_and_send("记住这个");
    r.idle();
    let id = r.active();
    r.frames(3);
    r.app.quit_cleanly();
    let store = Store::open(r.dir.join("data")).unwrap();
    let notifier = Notifier::none();
    let engine = Engine::new(r._rt.handle().clone(), store.clone(), notifier.clone());
    r.app = App::new(store, engine, notifier, Arc::new(|| {}));
    assert_eq!(r.active(), id, "it opens where the user left off");
    r.run_until("the body to load", |a| a.live[&id].load == crate::live::LoadState::Loaded);
    assert_eq!(r.texts(), ["记得"]);
    r.frames(3);
}
