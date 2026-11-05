//! Sidecar lifetime and request multiplexing.
//!
//! **One sidecar, multiplexed.** The parent turn and all subagents share one sidecar and demultiplex
//! by request ID. The sidecar is in-process ([`mewrk_aisdk::Sidecar`]): the same frames as the stdio
//! protocol it replaces, one line of JSON each way, with no Node and nothing to download.
//!
//! Threading model:
//!
//! - Each turn worker blocks on its own channel, preserving the synchronous `api.rs` turn loop.
//! - The sidecar calls back with each outbound frame in order, and that callback frames and dispatches
//!   it by ID.
//!
//! When a sidecar stops, all in-flight requests receive a transient error and the next [`run_step`]
//! starts a replacement.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

use super::{ModelRequestError, RequestFailure, StreamPartial};
use crate::api::ModelEventSink;
use crate::model::{ModelStreamEvent, ModelUsage};

use super::protocol::{
    ErrorKind, SidecarFrame, SidecarUsage, StepEvent, StepRequest, StepResult, MAX_LINE_BYTES,
    PROTOCOL_VERSION,
};

/// Hard limit for one exchange. It allows long reasoning while preventing a trickling upstream from
/// holding a turn indefinitely.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Cancellation probe interval while no frame is readable.
const PROBE_INTERVAL: Duration = Duration::from_millis(500);

struct Registry {
    pending: Mutex<HashMap<String, Sender<SidecarFrame>>>,
    alive: AtomicBool,
    /// Protocol version reported by this sidecar; zero means no handshake yet.
    ///
    /// This is per sidecar instance, not process-global. A restarted sidecar must not inherit a
    /// previous instance's ready version, which would bypass protocol-version validation.
    ready_protocol: AtomicU32,
}

impl Registry {
    fn deliver(&self, frame: SidecarFrame) {
        let Some(id) = frame.request_id() else {
            return;
        };
        let sender = {
            let pending = match self.pending.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            pending.get(id).cloned()
        };
        // An absent recipient is valid: cancellation or timeout may have already ended the request.
        if let Some(sender) = sender {
            let _ = sender.send(frame);
        }
    }

    /// Fails every in-flight request when the sidecar exits so they do not wait for
    /// `TOTAL_TIMEOUT`.
    fn fail_all(&self, message: &str) {
        self.alive.store(false, Ordering::SeqCst);
        let drained = {
            let mut pending = match self.pending.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *pending)
        };
        for (id, sender) in drained {
            let _ = sender.send(SidecarFrame::Error {
                id,
                error: super::protocol::StepError {
                    // A crashed sidecar is retryable; the next `run_step` starts a replacement.
                    kind: ErrorKind::Transient,
                    message: message.to_owned(),
                    status: None,
                    retry_after_ms: None,
                },
            });
        }
    }
}

pub(crate) struct Sidecar {
    service: mewrk_aisdk::Sidecar,
    registry: Arc<Registry>,
}

impl Sidecar {
    fn is_alive(&self) -> bool {
        self.registry.alive.load(Ordering::SeqCst)
    }

    fn write_frame(&self, frame: &serde_json::Value) -> Result<(), String> {
        let line =
            serde_json::to_string(frame).map_err(|error| format!("侧车帧序列化失败: {error}"))?;
        if line.len() > MAX_LINE_BYTES {
            return Err(format!("发往侧车的帧超过 {MAX_LINE_BYTES} 字节上限"));
        }
        if !self.service.is_alive() {
            return Err("写入侧车失败: 侧车已停止".to_owned());
        }
        self.service.send_line(&line);
        Ok(())
    }

    fn register(&self, id: &str) -> Receiver<SidecarFrame> {
        let (sender, receiver) = channel();
        let mut pending = match self.registry.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.insert(id.to_owned(), sender);
        receiver
    }

    fn unregister(&self, id: &str) {
        let mut pending = match self.registry.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.remove(id);
    }

    /// Shuts down the sidecar during application exit.
    pub(crate) fn shutdown(&self) {
        let _ = self.write_frame(&json!({ "v": PROTOCOL_VERSION, "type": "shutdown" }));
        self.service.stop();
        // Nothing more arrives on this sidecar, so no request may keep waiting on it.
        self.registry.fail_all("AI SDK 侧车已退出");
    }
}

// ------------------------------------------------------------------ Startup

/// Returns the environment the sidecar may read (`mewrk_aisdk::api::allowed_env`) so tests can assert
/// that unrelated keys do not reach it.
#[cfg(test)]
pub(crate) fn inherited_env() -> Vec<(String, String)> {
    mewrk_aisdk::api::inherited_env()
}

fn spawn() -> Result<Arc<Sidecar>, String> {
    let registry = Arc::new(Registry {
        pending: Mutex::new(HashMap::new()),
        alive: AtomicBool::new(true),
        ready_protocol: AtomicU32::new(0),
    });

    // The sole reader of the sidecar's frames.
    let on_line: mewrk_aisdk::service::LineSink = {
        let registry = Arc::clone(&registry);
        Box::new(move |line: String| {
            if line.len() > MAX_LINE_BYTES {
                registry.fail_all("侧车帧超过行长上限");
                return;
            }
            match serde_json::from_str::<SidecarFrame>(&line) {
                // `ready` has no request ID and cannot be delivered to a channel, so the
                // reader records the handshake state here.
                Ok(SidecarFrame::Ready { protocol, .. }) => {
                    registry.ready_protocol.store(protocol, Ordering::SeqCst);
                }
                Ok(frame) => registry.deliver(frame),
                Err(error) => {
                    // An undecodable frame indicates a protocol mismatch. Continuing would
                    // only obscure the failure.
                    registry.fail_all(&format!("侧车帧无法解析: {error}"));
                }
            }
        })
    };
    let on_exit: mewrk_aisdk::service::ExitSink = {
        let registry = Arc::clone(&registry);
        Box::new(move |message: String| registry.fail_all(&format!("AI SDK 侧车已退出: {message}")))
    };
    let service = mewrk_aisdk::Sidecar::start(on_line, on_exit)?;

    let sidecar = Arc::new(Sidecar { service, registry });

    // Fail the handshake on protocol mismatch: an incompatible sidecar is worse than no sidecar.
    let hello_id = "hello";
    let receiver = sidecar.register(hello_id);
    // `ready` has no ID, so it cannot reach this receiver. Registration only lets `fail_all`
    // report a sidecar death during the handshake.
    sidecar.write_frame(&json!({ "v": PROTOCOL_VERSION, "type": "hello" }))?;
    let handshake = wait_for_ready(&sidecar, &receiver);
    sidecar.unregister(hello_id);
    handshake?;

    Ok(sidecar)
}

/// Waits for `ready`. Because it has no request ID, the reader cannot deliver it to a channel;
/// handshake success comes from the registry liveness flag and a bounded wait.
fn wait_for_ready(sidecar: &Arc<Sidecar>, receiver: &Receiver<SidecarFrame>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if !sidecar.is_alive() {
            // `fail_all` placed the termination reason on this receiver.
            if let Ok(SidecarFrame::Error { error, .. }) = receiver.try_recv() {
                return Err(format!("AI SDK 侧车握手失败: {}", error.message));
            }
            return Err("AI SDK 侧车在握手期间退出".into());
        }
        let protocol = sidecar.registry.ready_protocol.load(Ordering::SeqCst);
        if protocol != 0 {
            return if protocol == PROTOCOL_VERSION {
                Ok(())
            } else {
                Err(format!(
                    "AI SDK 侧车协议世代不匹配：宿主 {PROTOCOL_VERSION}，侧车 {protocol}"
                ))
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("AI SDK 侧车握手超时".into())
}

// ------------------------------------------------------------------ Global handle

static SIDECAR: OnceLock<Mutex<Option<Arc<Sidecar>>>> = OnceLock::new();

/// The running sidecar, started first when there is none.
fn handle() -> Result<Arc<Sidecar>, RequestFailure> {
    let cell = SIDECAR.get_or_init(|| Mutex::new(None));
    let lock_slot = || match cell.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    let live = |slot: &Option<Arc<Sidecar>>| slot.as_ref().filter(|existing| existing.is_alive()).cloned();
    if let Some(existing) = live(&lock_slot()) {
        return Ok(existing);
    }
    let mut slot = lock_slot();
    // Another request may have started one meanwhile.
    if let Some(existing) = live(&slot) {
        return Ok(existing);
    }
    // Replace a dead sidecar so the next request does not reuse a failed handle.
    *slot = None;
    let fresh = spawn().map_err(|error| RequestFailure::bare(ModelRequestError::Fatal(error)))?;
    *slot = Some(Arc::clone(&fresh));
    Ok(fresh)
}

/// Shuts down the sidecar when the application exits.
pub(crate) fn shutdown_sidecar() {
    let Some(cell) = SIDECAR.get() else { return };
    let taken = {
        let mut slot = match cell.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        slot.take()
    };
    if let Some(sidecar) = taken {
        sidecar.shutdown();
    }
}

/// The live sidecar, if one is running. Unlike [`handle`], this never spawns:
/// callers that only want to tell an existing sidecar something have nothing to
/// say to a sidecar that does not exist.
fn existing_handle() -> Option<Arc<Sidecar>> {
    let cell = SIDECAR.get()?;
    let slot = match cell.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    slot.as_ref()
        .filter(|sidecar| sidecar.is_alive())
        .map(Arc::clone)
}

// ------------------------------------------------------------------ Agent sessions

/// Tears down a `claude-agent` CLI session. Fire-and-forget: the frame has no
/// reply, the sidecar treats an unknown session as already released, and a dead
/// sidecar has no session to release. Called from a `Drop` impl, so it must
/// never block or fail.
pub(crate) fn release_session(session: &str) {
    let Some(sidecar) = existing_handle() else {
        return;
    };
    let _ = sidecar.write_frame(&json!({
        "v": PROTOCOL_VERSION,
        "type": "release",
        "session": session,
    }));
}

// ------------------------------------------------------------------ One step

/// Flushes all reasoning segments into retained partial content in item order, independent of
/// whether and in which order their `reasoning-done` events arrived.
fn finish_partial(
    mut partial: StreamPartial,
    reasoning_buffers: BTreeMap<usize, String>,
) -> StreamPartial {
    for (_, buffer) in reasoning_buffers {
        partial.reasoning.push(buffer);
    }
    partial
}

fn usage_from(usage: &SidecarUsage) -> ModelUsage {
    ModelUsage {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cache_read_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        // A subset of `output_tokens`, displayed only and never included in totals.
        reasoning_tokens: usage.reasoning_tokens,
    }
}

/// A request submitted to the sidecar but not yet driven to completion.
///
/// Submission and driving must remain separate. Writing the frame to stdin is the authorization
/// fence's linearization point, allowing callers to release the named-Agent read lock before a
/// long stream completes.
pub(crate) struct Submission<'a> {
    sidecar: Arc<Sidecar>,
    id: String,
    receiver: Receiver<SidecarFrame>,
    _lifetime: std::marker::PhantomData<&'a ()>,
}

impl Drop for Submission<'_> {
    fn drop(&mut self) {
        self.sidecar.unregister(&self.id);
    }
}

/// The sidecar a step is about to be submitted to ([`submit_to`]), running.
pub(crate) struct Prepared(Arc<Sidecar>);

/// Gets the sidecar running for a step: started when it is not. There is nothing to wait for, so
/// `stop` is never asked; it stays in the signature for callers that also wait on other things.
///
/// Called before the caller takes the locks it holds while submitting (the named-agent
/// definition fence, which every document save takes the write side of): the start may not happen
/// while other steps and saves queue behind it.
pub(crate) fn prepare(_stop: &dyn Fn() -> Result<(), String>) -> Result<Prepared, RequestFailure> {
    handle().map(Prepared)
}

/// Submits a model step to the sidecar [`prepare`] got running. Returning means its frame has
/// been written. A sidecar that exited since is a retryable failure: the retry prepares a new
/// one, outside the caller's locks again.
pub(crate) fn submit_to(
    prepared: Prepared,
    request: &StepRequest,
) -> Result<Submission<'static>, RequestFailure> {
    let Prepared(sidecar) = prepared;
    let exited = || RequestFailure::bare(ModelRequestError::Api("AI SDK 侧车已退出".into()));
    if !sidecar.is_alive() {
        return Err(exited());
    }
    let id = Uuid::new_v4().simple().to_string();
    // Register before writing so an extremely fast reply cannot arrive before the reader has a
    // recipient and be discarded.
    let receiver = sidecar.register(&id);
    // An exit marks the sidecar dead before it fails every registered request, so a request
    // registered too late for that is one that sees it dead here, and none waits for a reply
    // that cannot come.
    if !sidecar.is_alive() {
        sidecar.unregister(&id);
        return Err(exited());
    }
    let submission = Submission {
        sidecar: Arc::clone(&sidecar),
        id,
        receiver,
        _lifetime: std::marker::PhantomData,
    };
    let frame = json!({
        "v": PROTOCOL_VERSION,
        "type": "step",
        "id": submission.id,
        "payload": request,
    });
    sidecar
        .write_frame(&frame)
        .map_err(|error| RequestFailure::bare(ModelRequestError::Api(error)))?;
    Ok(submission)
}

/// Drives a submitted step, translating sidecar events to host events.
///
/// It blocks, retains already-streamed partial content on failure, and expresses cancellation through
/// the `event_sink` result.
pub(crate) fn drive_submission(
    submission: &Submission<'_>,
    round: usize,
    event_sink: &ModelEventSink<'_>,
) -> Result<StepResult, RequestFailure> {
    drive(
        &submission.sidecar,
        &submission.id,
        round,
        event_sink,
        &submission.receiver,
    )
}

/// Submits and drives a step in one call.
///
/// Test-only: production must use [`submit`] and [`drive_submission`] separately to release
/// authorization read locks between frame submission and stream completion. `cfg(test)` makes
/// accidental production use a compile error.
#[cfg(test)]
pub(crate) fn run_step(
    request: &StepRequest,
    round: usize,
    event_sink: &ModelEventSink<'_>,
) -> Result<StepResult, RequestFailure> {
    let submission = submit_to(prepare(&|| Ok(()))?, request)?;
    drive_submission(&submission, round, event_sink)
}

fn drive(
    sidecar: &Arc<Sidecar>,
    id: &str,
    round: usize,
    event_sink: &ModelEventSink<'_>,
    receiver: &Receiver<SidecarFrame>,
) -> Result<StepResult, RequestFailure> {
    let mut partial = StreamPartial::default();
    let mut reasoning_buffers: BTreeMap<usize, String> = BTreeMap::new();
    let mut announced: Vec<String> = Vec::new();
    let mut cancelling: Option<String> = None;
    let started = Instant::now();
    // Do not use an inactivity timeout. Encrypted reasoning protocols can remain silent after
    // `reasoning-start` throughout a healthy long reasoning phase, making silence indistinguishable
    // from a stalled upstream. Heartbeats, disconnects, error frames, and HTTP failures provide
    // explicit liveness signals; `TOTAL_TIMEOUT` is the sole absolute exchange limit.

    loop {
        if started.elapsed() > TOTAL_TIMEOUT {
            // Cancel on timeout rather than only stop receiving: otherwise the sidecar continues an
            // unbounded upstream request, billing and retaining in-flight and heartbeat state.
            let _ = sidecar.write_frame(&json!({
                "v": PROTOCOL_VERSION,
                "type": "cancel",
                "id": id,
            }));
            return Err(RequestFailure {
                error: ModelRequestError::Api("单次模型交换超过 30 分钟上限".into()),
                retry_after_ms: None,
                partial: finish_partial(partial, reasoning_buffers),
            });
        }

        let frame = match receiver.recv_timeout(PROBE_INTERVAL) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => {
                // Probe cancellation even when no frame is readable.
                if cancelling.is_none() {
                    if let Err(reason) = event_sink(ModelStreamEvent::Ping) {
                        cancel(sidecar, id, &mut cancelling, reason);
                    }
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(RequestFailure {
                    error: ModelRequestError::Api("侧车通道已断开".into()),
                    retry_after_ms: None,
                    partial: finish_partial(partial, reasoning_buffers),
                });
            }
        };
        match frame {
            SidecarFrame::Ready { .. } => {
                // The reader consumes `ready` because it has no request ID. This branch preserves
                // exhaustive matching if a future protocol adds an ID.
            }
            SidecarFrame::Event { event, .. } => {
                // A cancelling request no longer forwards content to the renderer, but continues
                // reading until the sidecar sends a terminal frame.
                if cancelling.is_some() {
                    continue;
                }
                if let Err(reason) = forward(
                    event,
                    round,
                    event_sink,
                    &mut partial,
                    &mut reasoning_buffers,
                    &mut announced,
                ) {
                    cancel(sidecar, id, &mut cancelling, reason);
                }
            }
            SidecarFrame::Done { result, .. } => {
                if let Some(reason) = cancelling {
                    // Cancellation wins even if the sidecar later reports success.
                    return Err(RequestFailure {
                        error: ModelRequestError::EventSink(reason),
                        retry_after_ms: None,
                        partial: finish_partial(partial, reasoning_buffers),
                    });
                }
                return Ok(result);
            }
            SidecarFrame::Error { error, .. } => {
                if let Some(reason) = cancelling {
                    return Err(RequestFailure {
                        error: ModelRequestError::EventSink(reason),
                        retry_after_ms: None,
                        partial: finish_partial(partial, reasoning_buffers),
                    });
                }
                let message = describe(&error);
                let mapped = match error.kind {
                    ErrorKind::Transient => ModelRequestError::Api(message),
                    ErrorKind::Permanent => ModelRequestError::Fatal(message),
                    // Sidecar cancellation only follows a host `cancel` frame. Without a matching
                    // sink failure, treat it as non-retryable.
                    ErrorKind::Cancelled => ModelRequestError::Fatal(message),
                };
                return Err(RequestFailure {
                    error: mapped,
                    retry_after_ms: error.retry_after_ms,
                    partial: finish_partial(partial, reasoning_buffers),
                });
            }
        }
    }
}

/// Converts a sidecar error into host-visible text.
///
/// Preserve the upstream status code so server failures and invalid model names remain distinguishable
/// in the UI, retry notices, and diagnostics.
fn describe(error: &super::protocol::StepError) -> String {
    match error.status {
        Some(status) => format!("HTTP {status}: {}", error.message),
        None => error.message.clone(),
    }
}

fn cancel(sidecar: &Arc<Sidecar>, id: &str, cancelling: &mut Option<String>, reason: String) {
    if cancelling.is_some() {
        return;
    }
    let _ = sidecar.write_frame(&json!({
        "v": PROTOCOL_VERSION,
        "type": "cancel",
        "id": id,
    }));
    *cancelling = Some(reason);
}

fn forward(
    event: StepEvent,
    round: usize,
    event_sink: &ModelEventSink<'_>,
    partial: &mut StreamPartial,
    reasoning_buffers: &mut BTreeMap<usize, String>,
    announced: &mut Vec<String>,
) -> Result<(), String> {
    match event {
        StepEvent::TextDelta { delta } => {
            partial.text.push_str(&delta);
            event_sink(ModelStreamEvent::TextDelta { round, delta })
        }
        StepEvent::ReasoningStart { item, form } => {
            reasoning_buffers.entry(item).or_default();
            event_sink(ModelStreamEvent::ReasoningStart { round, item, form })
        }
        StepEvent::ReasoningDelta { item, delta } => {
            reasoning_buffers.entry(item).or_default().push_str(&delta);
            event_sink(ModelStreamEvent::ReasoningDelta { round, item, delta })
        }
        StepEvent::ReasoningDone { item, duration_ms } => {
            // Keep completed and open items in the same ordinal-indexed collection until
            // failure settlement; completion order is not item identity.
            event_sink(ModelStreamEvent::ReasoningDone {
                round,
                item,
                duration_ms,
            })
        }
        StepEvent::ReasoningProgress {
            item,
            estimated_tokens,
        } => event_sink(ModelStreamEvent::ReasoningProgress {
            round,
            item,
            estimated_tokens,
        }),
        StepEvent::ToolCallAnnounced { call_id, tool_name } => {
            if announced.iter().any(|seen| seen == &call_id) {
                return Ok(());
            }
            announced.push(call_id.clone());
            event_sink(ModelStreamEvent::ToolCallAnnounced {
                round,
                call_id,
                tool_name,
                // The running event sink supplies this because only it knows the conversation.
                context_id: String::new(),
            })
        }
        StepEvent::ToolCall { call_id, input } => {
            let input = super::call_arguments(input);
            event_sink(ModelStreamEvent::ToolCallArgumentsReady {
                round,
                call_id,
                input,
            })
        }
        StepEvent::Usage { usage } => {
            let usage = usage_from(&usage);
            if usage == ModelUsage::default() {
                return Ok(());
            }
            event_sink(ModelStreamEvent::UsageUpdated { round, usage })
        }
        StepEvent::Heartbeat => event_sink(ModelStreamEvent::Ping),
    }
}

#[cfg(test)]
mod reasoning_partial_tests {
    use super::*;

    #[test]
    fn failed_interleaved_reasoning_preserves_item_order() {
        for finish_zero in [false, true] {
            let mut partial = StreamPartial::default();
            let mut buffers = BTreeMap::new();
            let mut announced = Vec::new();
            let forwarded = std::sync::Mutex::new(Vec::new());
            let sink = |event| {
                forwarded.lock().unwrap().push(event);
                Ok(())
            };
            let mut events = vec![
                StepEvent::ReasoningStart {
                    item: 0,
                    form: None,
                },
                StepEvent::ReasoningDelta {
                    item: 0,
                    delta: "A".into(),
                },
                StepEvent::ReasoningStart {
                    item: 1,
                    form: None,
                },
                StepEvent::ReasoningDelta {
                    item: 1,
                    delta: "B".into(),
                },
                StepEvent::ReasoningDelta {
                    item: 0,
                    delta: "a".into(),
                },
                StepEvent::ReasoningDone {
                    item: 1,
                    duration_ms: Some(1),
                },
            ];
            if finish_zero {
                events.push(StepEvent::ReasoningDone {
                    item: 0,
                    duration_ms: Some(2),
                });
            }
            for event in events {
                forward(event, 3, &sink, &mut partial, &mut buffers, &mut announced).unwrap();
            }
            assert_eq!(finish_partial(partial, buffers).reasoning, ["Aa", "B"]);
            let forwarded = forwarded.lock().unwrap();
            assert!(
                matches!(&forwarded[3], ModelStreamEvent::ReasoningDelta { round: 3, item: 1, delta } if delta == "B")
            );
            assert!(matches!(
                &forwarded[5],
                ModelStreamEvent::ReasoningDone {
                    round: 3,
                    item: 1,
                    ..
                }
            ));
        }
    }
}

#[cfg(test)]
mod inherited_env_tests {
    use super::*;

    /// macOS gives each user a private `$TMPDIR`; the sidecar, and the CLI that inherits its
    /// environment, must keep using it instead of the shared /tmp.
    #[test]
    fn the_posix_temp_directory_reaches_the_sidecar() {
        let Ok(tmpdir) = std::env::var("TMPDIR") else {
            return;
        };
        assert!(
            inherited_env().contains(&("TMPDIR".to_owned(), tmpdir)),
            "TMPDIR 必须继承给侧车"
        );
    }
}
