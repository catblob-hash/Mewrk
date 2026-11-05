//! The sidecar's main loop (`main.ts`), in-process: frames in, frames out, one task per step.
//!
//! The vocabulary stays NDJSON text across this boundary so the host's reader is the code it was, and a frame
//! means what it meant on stdio. A dedicated runtime keeps the host's threads out of the HTTP path.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::async_tools::apply_async_tools;
use crate::claude_agent::ClaudeAgentRuntime;
use crate::error::StepError;
use crate::http::Cancel;
use crate::jobj;
use crate::json::{Json, Obj};
use crate::protocol::{HEARTBEAT_INTERVAL_MS, HostFrame, MAX_LINE_BYTES, PROTOCOL_VERSION, decode_host_frame};
use crate::step::run_step;
use crate::system_append::apply_system_append;

/// Receives each outbound frame as one line of JSON, without the newline, in `seq` order.
pub type LineSink = Box<dyn Fn(String) + Send + Sync>;
/// Called once when the sidecar stops by itself: a protocol violation, which ended the process on stdio.
pub type ExitSink = Box<dyn Fn(String) + Send + Sync>;

struct Out {
    seq: u64,
    sink: LineSink,
}

/// What every step shares: the frame writer and the in-flight books.
pub struct Shared {
    out: Mutex<Out>,
    inflight: Mutex<HashMap<String, Cancel>>,
    streaming: Mutex<HashSet<String>>,
    heartbeat: Mutex<Option<JoinHandle<()>>>,
    handle: tokio::runtime::Handle,
}

impl Shared {
    fn write(&self, frame: Json) {
        let mut body = Obj::new();
        body.insert("v", PROTOCOL_VERSION);
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        body.insert("seq", out.seq);
        out.seq += 1;
        if let Json::Obj(frame) = frame {
            body.extend_from(&frame);
        }
        (out.sink)(Json::Obj(body).to_text());
    }

    pub fn emit(&self, id: &str, event: Json) {
        self.write(jobj! { "type": "event", "id": id, "event": event });
    }

    pub fn done(&self, id: &str, result: Json) {
        self.write(jobj! { "type": "done", "id": id, "result": result });
    }

    pub fn fail(&self, id: &str, error: StepError) {
        self.write(jobj! { "type": "error", "id": id, "error": error.to_json() });
    }

    /// A cancellable request starts; heartbeats target only streaming ones.
    pub fn begin(self: &Arc<Self>, id: &str, cancel: Cancel, is_streaming: bool) {
        self.inflight.lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_owned(), cancel);
        if is_streaming {
            self.streaming.lock().unwrap_or_else(|p| p.into_inner()).insert(id.to_owned());
            self.start_heartbeat();
        }
    }

    pub fn end(&self, id: &str) {
        self.inflight.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
        let idle = {
            let mut streaming = self.streaming.lock().unwrap_or_else(|p| p.into_inner());
            streaming.remove(id);
            streaming.is_empty()
        };
        if idle {
            if let Some(task) = self.heartbeat.lock().unwrap_or_else(|p| p.into_inner()).take() {
                task.abort();
            }
        }
    }

    fn start_heartbeat(self: &Arc<Self>) {
        let mut slot = self.heartbeat.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return;
        }
        let shared = Arc::downgrade(self);
        *slot = Some(self.handle.spawn(async move {
            let period = Duration::from_millis(HEARTBEAT_INTERVAL_MS);
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            loop {
                ticks.tick().await;
                let Some(shared) = shared.upgrade() else { return };
                let ids: Vec<String> = shared.streaming.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned().collect();
                for id in ids {
                    shared.emit(&id, jobj! { "k": "heartbeat" });
                }
            }
        }));
    }

    fn abort_all(&self) {
        for cancel in self.inflight.lock().unwrap_or_else(|p| p.into_inner()).values() {
            cancel.cancel();
        }
    }
}

/// The sidecar, running.
pub struct Sidecar {
    shared: Arc<Shared>,
    agent: Arc<ClaudeAgentRuntime>,
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    on_exit: ExitSink,
    alive: AtomicBool,
}

impl Sidecar {
    /// Starts one. Frames go to `on_line` from whichever thread produced them, in order.
    pub fn start(on_line: LineSink, on_exit: ExitSink) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("aisdk")
            .enable_all()
            .build()
            .map_err(|error| format!("无法启动 AI SDK 运行时: {error}"))?;
        let shared = Arc::new(Shared {
            out: Mutex::new(Out { seq: 0, sink: on_line }),
            inflight: Mutex::new(HashMap::new()),
            streaming: Mutex::new(HashSet::new()),
            heartbeat: Mutex::new(None),
            handle: runtime.handle().clone(),
        });
        let agent = Arc::new(ClaudeAgentRuntime::new(shared.clone()));
        Ok(Self { shared, agent, runtime: Mutex::new(Some(runtime)), on_exit, alive: AtomicBool::new(true) })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// One host frame, as the one line of JSON it was on stdin.
    pub fn send_line(&self, line: &str) {
        if !self.is_alive() {
            return;
        }
        if line.len() > MAX_LINE_BYTES {
            self.violation(format!("宿主帧超过行长上限（{} 字节），已断开", line.len()));
            return;
        }
        match decode_host_frame(line) {
            Ok(frame) => self.handle(frame),
            // A protocol violation means the two sides use incompatible generations, not that one request
            // failed. The sidecar stops so the host can start a new one.
            Err(message) => self.violation(format!("协议违例：{message}")),
        }
    }

    fn violation(&self, message: String) {
        eprintln!("[aisdk] {message}");
        self.stop();
        (self.on_exit)(message);
    }

    fn handle(&self, frame: HostFrame) {
        match frame {
            HostFrame::Hello => self.shared.write(jobj! { "type": "ready", "protocol": PROTOCOL_VERSION }),
            HostFrame::Step { id, mut payload } => {
                // Appended system prompts first, for every family, then the host's deliveries.
                apply_system_append(&mut payload);
                apply_async_tools(&mut payload);
                let guard = self.runtime.lock().unwrap_or_else(|p| p.into_inner());
                let Some(runtime) = guard.as_ref() else { return };
                if payload.str_at("family") == Some("claude-agent") {
                    let agent = self.agent.clone();
                    runtime.spawn(async move { agent.step(id, payload).await });
                } else {
                    let cancel = Cancel::new();
                    self.shared.begin(&id, cancel.clone(), true);
                    let shared = self.shared.clone();
                    runtime.spawn(async move {
                        // A step that panics still owes the host a terminal frame, or the host waits for its
                        // absolute limit.
                        let task = tokio::spawn(run_step(shared.clone(), id.clone(), payload, cancel));
                        if let Err(error) = task.await {
                            shared.fail(
                                &id,
                                StepError { kind: "permanent", message: format!("侧车内部错误: {error}"), status: None, retry_after_ms: None },
                            );
                        }
                        shared.end(&id);
                    });
                }
            }
            HostFrame::Release { session } => {
                let guard = self.runtime.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(runtime) = guard.as_ref() {
                    let agent = self.agent.clone();
                    runtime.spawn(async move { agent.release(session).await });
                }
            }
            HostFrame::Cancel { id } => {
                if let Some(cancel) = self.shared.inflight.lock().unwrap_or_else(|p| p.into_inner()).get(&id) {
                    cancel.cancel();
                }
            }
            HostFrame::Shutdown => self.stop(),
        }
    }

    /// Aborts in-flight requests, tears down Claude Code sessions, flushes frames, stops. Idempotent.
    pub fn stop(&self) {
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        self.shared.abort_all();
        let Some(runtime) = self.runtime.lock().unwrap_or_else(|p| p.into_inner()).take() else { return };
        let (tx, rx) = std::sync::mpsc::channel();
        let agent = self.agent.clone();
        runtime.spawn(async move {
            agent.shutdown().await;
            // Gives aborted steps a moment to write their terminal frames.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(Duration::from_secs(3));
        runtime.shutdown_background();
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.stop();
    }
}
