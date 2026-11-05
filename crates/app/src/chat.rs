//! The conversation page.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use agent_rt::media::{MediaInput, MediaKind};
use agent_rt::{run_agent, Cancel, FailKind, Message, Role};
use fluent::gpu::ImageId;
use fluent::theme::*;
use fluent::ui::{rect, CursorKind, Icon, Key, Rect, Severity, Ui};
use fluent::{BlockKind, Doc, Renderer, Scroll, TextCtx, TextEdit};
use parking_lot::Mutex;

use crate::app::Conn;
use crate::bridge::{Inbox, InboxData, Notifier, Op};
use fluent::md::{MdStream, Palette};
use crate::swarm::{format_ms, thousands};

pub const CHAT_INBOX_ID: u32 = 1;

/// Measured on LFM3-VL-2B: with this, unneeded tool calls fell from 7 of 24 to 0 of 24, and needed ones stayed at 18 of 18.
const TOOL_GUIDANCE: &str = "Only call a tool when the question genuinely needs its result. Otherwise answer directly without any tool.";

struct Thinking {
    doc: Doc,
    open: bool,
    done: bool,
    started: f32,
    ended: f32,
}

struct ToolCard {
    index: u32,
    name: String,
    args: String,
    output: Option<(String, bool, Duration)>,
}

enum Part {
    Thinking(Thinking),
    Tool(ToolCard),
    Text(MdStream),
}

struct Thumb {
    name: String,
    kind: MediaKind,
    summary: String,
    image: Option<(ImageId, (u32, u32))>,
}

struct ChatMsg {
    role: Role,
    parts: Vec<Part>,
    thumbs: Vec<Thumb>,
    height: f32,
    tokens: u32,
    first_token: Option<f32>,
    ttft_ms: Option<f32>,
    last_token: f32,
    retries: u32,
    resumes: u32,
    finished: bool,
    error: Option<String>,
    usage_prompt: u32,
    cached: u32,
}

impl ChatMsg {
    fn new(role: Role, time: f32) -> Self {
        Self { role, parts: Vec::new(), thumbs: Vec::new(), height: 0.0, tokens: 0, first_token: None, ttft_ms: None, last_token: time, retries: 0, resumes: 0, finished: false, error: None, usage_prompt: 0, cached: 0 }
    }
}

struct Attachment {
    id: u64,
    name: String,
    media: Option<Arc<MediaInput>>,
    error: Option<String>,
    thumb: Option<(ImageId, (u32, u32))>,
    summary: String,
}

/// RGBA pixels, width and height.
type Thumbnail = (Vec<u8>, u32, u32);

struct LoadedAttachment {
    id: u64,
    result: Result<(Arc<MediaInput>, Option<Thumbnail>), String>,
}

#[derive(Clone)]
struct Selection {
    msg: usize,
    part: usize,
    a: usize,
    b: usize,
}

struct RetryBanner {
    attempt: u32,
    kind: FailKind,
    until: f32,
}

pub struct ChatPage {
    inbox: Arc<Inbox>,
    notifier: Arc<Notifier>,
    messages: Vec<ChatMsg>,
    conversation: Vec<Message>,
    returned: Arc<Mutex<Option<Vec<Message>>>>,
    cancel: Option<Cancel>,
    running: bool,
    scroll: Scroll,
    pub input: TextEdit,
    attachments: Vec<Attachment>,
    next_attachment: u64,
    loaded_tx: mpsc::Sender<LoadedAttachment>,
    loaded_rx: mpsc::Receiver<LoadedAttachment>,
    pending_thumbs: Vec<(u64, Vec<u8>, u32, u32)>,
    scratch: InboxData,
    banner: Option<RetryBanner>,
    recovered_until: f32,
    selection: Option<Selection>,
    dragging_selection: bool,
    last_col_w: f32,
    pub pick_files: bool,
    pub auto_submit: Option<String>,
}

fn palette(t: &Theme) -> Palette {
    Palette { text: t.text_primary, muted: t.text_secondary, code: t.accent_text }
}

impl ChatPage {
    pub fn new(notifier: Arc<Notifier>) -> Self {
        let (loaded_tx, loaded_rx) = mpsc::channel();
        Self {
            inbox: Inbox::new(CHAT_INBOX_ID, notifier.clone(), false),
            notifier,
            messages: Vec::new(),
            conversation: Vec::new(),
            returned: Arc::new(Mutex::new(None)),
            cancel: None,
            running: false,
            scroll: Scroll::default(),
            input: TextEdit::new(),
            attachments: Vec::new(),
            next_attachment: 1,
            loaded_tx,
            loaded_rx,
            pending_thumbs: Vec::new(),
            scratch: InboxData::default(),
            banner: None,
            recovered_until: 0.0,
            selection: None,
            dragging_selection: false,
            last_col_w: 0.0,
            pick_files: false,
            auto_submit: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// The attached media, for the wall's image and video scenarios.
    pub fn attached(&self, kind: MediaKind) -> Option<Arc<MediaInput>> {
        self.attachments.iter().filter_map(|a| a.media.as_ref()).find(|m| m.kind == kind).cloned()
    }

    /// Starts loading files. Decoding happens on the blocking pool, so even a huge file never drops a frame.
    pub fn attach_paths(&mut self, paths: Vec<PathBuf>, conn: &Conn) {
        for path in paths {
            let id = self.next_attachment;
            self.next_attachment += 1;
            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            self.attachments.push(Attachment { id, name, media: None, error: None, thumb: None, summary: "正在读取…".into() });
            let tx = self.loaded_tx.clone();
            let notifier = self.notifier.clone();
            conn.handle.spawn_blocking(move || {
                let result = MediaInput::from_path_blocking(&path, agent_rt::media::DEFAULT_MAX_IMAGE_EDGE).map_err(|e| e.0).map(|media| {
                    let thumb = if media.kind == MediaKind::Image { make_thumb(&path) } else { None };
                    (Arc::new(media), thumb)
                });
                let _ = tx.send(LoadedAttachment { id, result });
                notifier.poke();
            });
        }
    }

    /// Texture uploads have to happen where the renderer is.
    pub fn upload_thumbs(&mut self, renderer: &mut Renderer) {
        for (id, rgba, w, h) in self.pending_thumbs.drain(..) {
            let image = renderer.add_image(&rgba, w, h);
            if let Some(a) = self.attachments.iter_mut().find(|a| a.id == id) {
                a.thumb = Some((image, (w, h)));
            }
        }
    }

    fn collect_loaded(&mut self) {
        while let Ok(done) = self.loaded_rx.try_recv() {
            let Some(a) = self.attachments.iter_mut().find(|a| a.id == done.id) else { continue };
            match done.result {
                Ok((media, thumb)) => {
                    a.summary = describe(&media);
                    a.media = Some(media);
                    if let Some((rgba, w, h)) = thumb {
                        self.pending_thumbs.push((done.id, rgba, w, h));
                    }
                }
                Err(e) => {
                    a.summary = e.clone();
                    a.error = Some(e);
                }
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
    }

    pub fn send(&mut self, conn: &Conn, ctx: &mut TextCtx, theme: &Theme, time: f32, width: f32, text: &str) {
        if self.running {
            return;
        }
        let ready: Vec<Arc<MediaInput>> = self.attachments.iter().filter_map(|a| a.media.clone()).collect();
        if text.trim().is_empty() && ready.is_empty() {
            return;
        }
        let mut user = ChatMsg::new(Role::User, time);
        for a in self.attachments.drain(..) {
            if let Some(m) = &a.media {
                user.thumbs.push(Thumb { name: a.name, kind: m.kind, summary: a.summary, image: a.thumb });
            }
        }
        if !text.is_empty() {
            let mut md = MdStream::new(width * 0.78 * ctx.scale, BODY, palette(theme));
            md.push(ctx, text);
            md.finish(ctx);
            user.parts.push(Part::Text(md));
        }
        self.messages.push(user);
        self.messages.push(ChatMsg::new(Role::Assistant, time));
        self.scroll.scroll_to_end();
        if self.conversation.is_empty() {
            self.conversation.push(Message::text(Role::System, TOOL_GUIDANCE));
        }
        self.conversation.push(if ready.is_empty() { Message::text(Role::User, text) } else { Message::user_with_media(text, ready) });

        let cancel = Cancel::new();
        self.cancel = Some(cancel.clone());
        self.running = true;
        self.banner = None;
        let inbox = self.inbox.clone();
        let rt = conn.runtime.clone();
        let cfg = conn.agent.clone();
        let slot = self.returned.clone();
        let mut conversation = std::mem::take(&mut self.conversation);
        conn.handle.spawn(async move {
            let result = run_agent(&rt, &cfg, CHAT_INBOX_ID, &mut conversation, inbox.as_ref(), &cancel).await;
            *slot.lock() = Some(conversation);
            match result {
                Ok(s) => inbox.finished(None, s.attempts, s.resumes),
                Err((e, s)) => inbox.finished(Some((e.kind, e.detail)), s.attempts, s.resumes),
            }
        });
    }

    pub fn drain(&mut self, ctx: &mut TextCtx, theme: &Theme, time: f32, col_w: f32) {
        self.collect_loaded();
        let mut data = std::mem::take(&mut self.scratch);
        self.inbox.drain(&mut data);
        let Some(msg) = self.messages.last_mut() else {
            self.scratch = data;
            return;
        };
        let width = col_w * ctx.scale;
        for op in &data.ops {
            match op {
                Op::FirstToken { us } => msg.ttft_ms = Some(*us as f32 / 1000.0),
                Op::Reasoning(a, b) => {
                    let s = &data.arena[*a as usize..*b as usize];
                    msg.first_token.get_or_insert(time);
                    if !matches!(msg.parts.last(), Some(Part::Thinking(_))) {
                        msg.parts.push(Part::Thinking(Thinking { doc: Doc::new((width - 24.0 * ctx.scale).max(1.0)), open: false, done: false, started: time, ended: time }));
                    }
                    if let Some(Part::Thinking(th)) = msg.parts.last_mut() {
                        th.doc.push(ctx, s, CAPTION, theme.text_secondary);
                        th.ended = time;
                    }
                }
                Op::Text(a, b) => {
                    let s = &data.arena[*a as usize..*b as usize];
                    msg.first_token.get_or_insert(time);
                    close_thinking(msg, time);
                    if !matches!(msg.parts.last(), Some(Part::Text(_))) {
                        msg.parts.push(Part::Text(MdStream::new(width, BODY, palette(theme))));
                    }
                    if let Some(Part::Text(md)) = msg.parts.last_mut() {
                        md.push(ctx, s);
                    }
                }
                Op::ToolStart { index, name } => {
                    msg.first_token.get_or_insert(time);
                    close_thinking(msg, time);
                    msg.parts.push(Part::Tool(ToolCard { index: *index, name: name.clone(), args: String::new(), output: None }));
                }
                Op::ToolArgs { index, range: (a, b) } => {
                    let s = &data.arena[*a as usize..*b as usize];
                    if let Some(card) = msg.parts.iter_mut().rev().find_map(|p| match p {
                        Part::Tool(c) if c.index == *index && c.output.is_none() => Some(c),
                        _ => None,
                    }) {
                        card.args.push_str(s);
                    }
                }
                Op::ToolRewind => msg.parts.retain(|p| !matches!(p, Part::Tool(c) if c.output.is_none())),
                Op::ToolResult { name, output, is_error, took } => {
                    if let Some(card) = msg.parts.iter_mut().find_map(|p| match p {
                        Part::Tool(c) if c.name == *name && c.output.is_none() => Some(c),
                        _ => None,
                    }) {
                        card.output = Some((output.clone(), *is_error, *took));
                    }
                }
                Op::Attempt { resumed_bytes, .. } => {
                    if *resumed_bytes > 0 {
                        msg.resumes += 1;
                    }
                }
                Op::Retry { attempt, kind, delay, .. } => {
                    msg.retries += 1;
                    self.banner = Some(RetryBanner { attempt: *attempt, kind: *kind, until: time + delay.as_secs_f32() });
                }
                Op::TurnEnd { usage, timings, .. } => {
                    if let Some(u) = usage {
                        msg.usage_prompt = u.prompt_tokens;
                    }
                    if let Some(t) = timings {
                        msg.cached = t.cache_n;
                    }
                }
                Op::Finished { error, .. } => {
                    close_thinking(msg, time);
                    msg.finished = true;
                    for part in &mut msg.parts {
                        if let Part::Text(md) = part {
                            md.finish(ctx);
                        }
                    }
                    if let Some((kind, detail)) = error {
                        msg.error = Some(if *kind == FailKind::Cancelled { "已停止".to_owned() } else { format!("{}: {detail}", kind.label()) });
                    }
                    self.running = false;
                    self.cancel = None;
                    if self.banner.take().is_some() {
                        self.recovered_until = time + 2.5;
                    }
                    if let Some(mut conv) = self.returned.lock().take() {
                        // A failed question that got no answer at all should not sit in front of the next one.
                        if error.is_some() && conv.last().is_some_and(|m| m.role == Role::User) {
                            conv.pop();
                        }
                        self.conversation = conv;
                    }
                }
                _ => {}
            }
        }
        if data.tokens > 0 {
            msg.tokens += data.tokens;
            msg.last_token = time;
            if self.banner.take().is_some() {
                self.recovered_until = time + 2.5;
            }
        }
        self.scratch = data;
    }

    pub fn show(&mut self, ui: &mut Ui<'_>, area: Rect, conn: &Conn) {
        if !ui.input.dropped.is_empty() {
            self.attach_paths(ui.input.dropped.clone(), conn);
        }
        let t = ui.theme;
        let col_w = (area.w - 64.0).clamp(280.0, 820.0);
        let col_x = area.x + (area.w - col_w) * 0.5;

        if (col_w - self.last_col_w).abs() > 0.5 {
            self.last_col_w = col_w;
            let scale = ui.scale;
            for msg in &mut self.messages {
                let w = if msg.role == Role::User { col_w * 0.78 } else { col_w };
                for part in &mut msg.parts {
                    match part {
                        Part::Text(md) => md.doc.set_width(ui.text, w * scale),
                        Part::Thinking(th) => th.doc.set_width(ui.text, (w - 24.0) * scale),
                        Part::Tool(_) => {}
                    }
                }
            }
        }

        // Input area first: its height decides how much room the messages get.
        let scale = ui.scale;
        let wanted = (self.input.content_height(scale, &BODY) + 12.0).clamp(44.0, 180.0);
        let attach_h = if self.attachments.is_empty() { 0.0 } else { 72.0 };
        let banner_h = if self.banner.is_some() || ui.time < self.recovered_until { 44.0 } else { 0.0 };
        let input_h = wanted + 44.0;
        let bottom_h = input_h + attach_h + banner_h + 20.0;
        let list = rect(area.x, area.y, area.w, (area.h - bottom_h).max(0.0));

        self.show_messages(ui, list, col_x, col_w);

        let mut y = list.bottom();
        if let Some(b) = &self.banner {
            let remaining = (b.until - ui.time).max(0.0);
            let message = format!("第 {} 次重试 · {} · {} · 已收到的内容会保留", b.attempt, if remaining > 0.0 { format!("{remaining:.1} s 后重连") } else { "正在连接".to_owned() }, b.kind.label());
            let banner_rect = rect(col_x, y, col_w, 36.0);
            ui.info_bar(banner_rect, Severity::Warning, "连接中断", &message);
            if remaining > 0.0 {
                ui.animating = true;
            }
            y += banner_h;
        } else if ui.time < self.recovered_until {
            ui.info_bar(rect(col_x, y, col_w, 36.0), Severity::Success, "已恢复", "已从断点继续，之前收到的内容都保留着");
            ui.wake_after(self.recovered_until - ui.time);
            y += banner_h;
        }

        if !self.attachments.is_empty() {
            self.show_attachments(ui, rect(col_x, y, col_w, 64.0));
            y += attach_h;
        }
        self.show_input(ui, rect(col_x, y, col_w, input_h), conn, col_w);

        if ui.input.hovered_file {
            let overlay = area.inset(12.0);
            ui.card(overlay, OVERLAY_RADIUS, t.smoke, t.accent);
            ui.icon(Icon::Attach, overlay.x + overlay.w * 0.5, overlay.y + overlay.h * 0.5 - 24.0, 28.0, t.text_primary);
            ui.label_centered(rect(overlay.x, overlay.y + overlay.h * 0.5, overlay.w, 28.0), "松开以附加图片或视频", &BODY_LARGE, t.text_primary);
        }

        // Copy from a selection.
        if let Some(sel) = &self.selection {
            for press in &ui.input.keys {
                if press.cmd && press.key == Key::Char('c')
                    && let Some(Part::Text(md)) = self.messages.get(sel.msg).and_then(|m| m.parts.get(sel.part)) {
                        let (a, b) = (sel.a.min(sel.b), sel.a.max(sel.b));
                        if let Some(s) = md.doc.text().get(a..b) {
                            ui.clipboard.set(s);
                        }
                    }
            }
        }
    }

    fn show_input(&mut self, ui: &mut Ui<'_>, r: Rect, conn: &Conn, col_w: f32) {
        let t = ui.theme;
        let field = rect(r.x, r.y, r.w, r.h - 44.0);
        let edit = self.input.show(ui, 0xB000, field, "向 LFM3 提问，可拖入图片或视频…（Enter 发送，Shift+Enter 换行）", &BODY, true);
        let row = rect(r.x, field.bottom() + 6.0, r.w, 32.0);
        if ui.icon_button(0xB001, rect(row.x, row.y, 36.0, 32.0), Icon::Attach, None).clicked {
            self.pick_files = true;
        }
        let note = format!("{} · {}", conn.model_label, conn.endpoint_label);
        ui.label_ellipsis(row.x + 44.0, row.y + 8.0, (r.w - 180.0).max(0.0), &note, &CAPTION, t.text_tertiary);
        let send_rect = rect(row.right() - 84.0, row.y, 84.0, 32.0);
        let mut submit = edit.submitted;
        if self.running {
            if ui.button(0xB002, send_rect, "停止").clicked {
                self.stop();
            }
        } else {
            let can_send = !self.input.text.trim().is_empty() || self.attachments.iter().any(|a| a.media.is_some());
            if ui.accent_button(0xB002, send_rect, "发送", can_send).clicked {
                submit = true;
            }
        }
        if let Some(auto) = self.auto_submit.take() {
            self.input.set_text(auto);
            submit = true;
        }
        if submit && !self.running {
            let text = std::mem::take(&mut self.input.text);
            self.input.clear();
            let theme = ui.theme;
            let time = ui.time;
            self.send(conn, ui.text, &theme, time, col_w, &text);
        }
    }

    fn show_attachments(&mut self, ui: &mut Ui<'_>, r: Rect) {
        let t = ui.theme;
        let mut x = r.x;
        let mut remove = None;
        for (i, a) in self.attachments.iter().enumerate() {
            let w = 240.0f32.min(r.right() - x);
            if w < 80.0 {
                break;
            }
            let chip = rect(x, r.y, w, r.h);
            ui.card(chip, OVERLAY_RADIUS, t.card, t.card_stroke);
            let thumb = rect(chip.x + 6.0, chip.y + 6.0, chip.h - 12.0, chip.h - 12.0);
            if let Some((image, size)) = a.thumb {
                ui.image_cover(image, thumb, size, CONTROL_RADIUS, 1.0);
            } else {
                ui.fill(thumb, CONTROL_RADIUS, t.card_secondary);
                let is_video = a.media.as_ref().is_some_and(|m| m.kind == MediaKind::Video);
                ui.icon(if is_video { Icon::Video } else { Icon::Image }, thumb.x + thumb.w * 0.5, thumb.y + thumb.h * 0.5, 20.0, t.text_secondary);
                if a.media.is_none() && a.error.is_none() {
                    ui.progress_ring(thumb.x + thumb.w * 0.5, thumb.y + thumb.h * 0.5, 12.0, 2.0);
                }
            }
            let tx = thumb.right() + 8.0;
            let tw = chip.right() - tx - 26.0;
            ui.label_ellipsis(tx, chip.y + 12.0, tw, &a.name, &CAPTION_STRONG, t.text_primary);
            ui.label_ellipsis(tx, chip.y + 30.0, tw, &a.summary, &CAPTION, if a.error.is_some() { t.critical } else { t.text_tertiary });
            if ui.icon_button(0xB100 + a.id, rect(chip.right() - 24.0, chip.y + 4.0, 20.0, 20.0), Icon::Close, Some(t.text_secondary)).clicked {
                remove = Some(i);
            }
            x += w + 8.0;
        }
        if let Some(i) = remove {
            self.attachments.remove(i);
        }
    }

    fn show_messages(&mut self, ui: &mut Ui<'_>, list: Rect, col_x: f32, col_w: f32) {
        let t = ui.theme;
        let scale = ui.scale;

        if self.messages.is_empty() {
            let cy = list.y + list.h * 0.42;
            ui.icon(Icon::Chat, list.x + list.w * 0.5, cy - 44.0, 36.0, t.accent);
            ui.label_centered(rect(list.x, cy - 12.0, list.w, 36.0), "今天想了解什么？", &TITLE, t.text_primary);
            ui.label_centered(rect(list.x, cy + 30.0, list.w, 20.0), "流式输出 · 断线自动续传 · 支持图片和视频", &BODY, t.text_secondary);
            let suggestions = ["解释 TCP 如何从丢包中恢复", "用 calc 工具算 12345*6789", "写一段 Rust 的 SSE 解析器"];
            let w = 228.0;
            let total = suggestions.len() as f32 * (w + 8.0) - 8.0;
            let mut x = list.x + (list.w - total) * 0.5;
            for (i, s) in suggestions.iter().enumerate() {
                let r = rect(x, cy + 70.0, w, 40.0);
                if ui.button(0xB200 + i as u64, r, s).clicked {
                    self.auto_submit = Some((*s).to_owned());
                }
                x += w + 8.0;
            }
            return;
        }

        // Measure everything so that the scrollbar is honest, and draw only what intersects the viewport.
        let gap = 18.0;
        let top_pad = 16.0;
        let mut content = top_pad;
        for msg in &mut self.messages {
            msg.height = measure_message(msg, scale, col_w);
            content += msg.height + gap;
        }
        self.scroll.update(ui, 0xB300, list, content + 8.0);
        let offset = self.scroll.offset;

        let saved_clip = ui.clip;
        ui.clip = saved_clip.intersect(&list);
        let mut y = list.y + top_pad - offset;
        let n = self.messages.len();
        let mut new_selection = None;
        let mut clicked_doc = false;
        for i in 0..n {
            let h = self.messages[i].height;
            let r = rect(col_x, y, col_w, h);
            if r.intersects(&list) {
                let is_last = i + 1 == n;
                let sel = self.selection.clone().filter(|s| s.msg == i);
                let msg = &mut self.messages[i];
                draw_message(ui, msg, r, i, is_last, sel, &mut new_selection, &mut clicked_doc, self.dragging_selection);
            }
            y += h + gap;
        }
        ui.clip = saved_clip;

        if let Some(sel) = new_selection {
            self.selection = Some(sel);
            if ui.input.pressed {
                self.dragging_selection = true;
            }
        } else if ui.input.pressed && !clicked_doc {
            self.selection = None;
        }
        if ui.input.released {
            self.dragging_selection = false;
        }
    }
}

fn close_thinking(msg: &mut ChatMsg, time: f32) {
    if let Some(Part::Thinking(th)) = msg.parts.last_mut()
        && !th.done {
            th.done = true;
            th.ended = time;
        }
}

fn thinking_height(th: &Thinking, scale: f32) -> f32 {
    34.0 + if th.open { th.doc.height() / scale + 10.0 } else { 0.0 }
}

const TOOL_CARD_H: f32 = 62.0;
const THUMB_H: f32 = 96.0;

fn measure_message(msg: &ChatMsg, scale: f32, _: f32) -> f32 {
    let mut h = 0.0;
    if msg.role == Role::User {
        if !msg.thumbs.is_empty() {
            h += THUMB_H + 8.0;
        }
        for part in &msg.parts {
            if let Part::Text(md) = part {
                h += md.doc.height() / scale + 20.0;
            }
        }
        return h;
    }
    for part in &msg.parts {
        h += match part {
            Part::Thinking(th) => thinking_height(th, scale) + 8.0,
            Part::Tool(_) => TOOL_CARD_H + 8.0,
            Part::Text(md) => md.doc.height() / scale + 8.0,
        };
    }
    if msg.parts.is_empty() {
        h += 28.0;
    }
    h + 24.0 + if msg.error.is_some() { 40.0 } else { 0.0 }
}

#[allow(clippy::too_many_arguments)]
fn draw_message(ui: &mut Ui<'_>, msg: &mut ChatMsg, r: Rect, index: usize, is_last: bool, sel: Option<Selection>, new_sel: &mut Option<Selection>, clicked: &mut bool, dragging: bool) {
    let t = ui.theme;
    let scale = ui.scale;
    if msg.role == Role::User {
        let mut y = r.y;
        if !msg.thumbs.is_empty() {
            let mut x = r.right();
            for thumb in msg.thumbs.iter().rev() {
                let w = match thumb.image {
                    // Wide images stay wide, up to a limit, and a tall one crops rather than turning into a sliver.
                    Some((_, (iw, ih))) => (THUMB_H * iw as f32 / ih.max(1) as f32).clamp(THUMB_H * 0.75, THUMB_H * 2.4),
                    None => 240.0,
                };
                x -= w;
                let card = rect(x, y, w, THUMB_H);
                ui.card(card, OVERLAY_RADIUS, t.card, t.card_stroke);
                if let Some((image, size)) = thumb.image {
                    ui.image_cover(image, card.inset(1.0), size, OVERLAY_RADIUS, 1.0);
                } else {
                    let icon = if thumb.kind == MediaKind::Video { Icon::Video } else { Icon::Image };
                    ui.icon(icon, card.x + 26.0, card.y + THUMB_H * 0.5, 24.0, t.accent);
                    ui.label_ellipsis(card.x + 50.0, card.y + 28.0, w - 60.0, &thumb.name, &BODY_STRONG, t.text_primary);
                    ui.label_ellipsis(card.x + 50.0, card.y + 50.0, w - 60.0, &thumb.summary, &CAPTION, t.text_secondary);
                }
                x -= 8.0;
            }
            y += THUMB_H + 8.0;
        }
        for part in &mut msg.parts {
            if let Part::Text(md) = part {
                let doc_w = md.doc.lines.iter().map(|l| l.width).fold(0.0f32, f32::max) / scale;
                let doc_h = md.doc.height() / scale;
                let bubble = rect(r.right() - doc_w - 28.0, y, doc_w + 28.0, doc_h + 20.0);
                let fill = if t.dark { t.accent.with_alpha(0.16) } else { t.accent.with_alpha(0.10) };
                ui.card(bubble, OVERLAY_RADIUS, fill, t.card_stroke);
                ui.doc(&mut md.doc, bubble.x + 14.0, bubble.y + 10.0);
                y += doc_h + 20.0;
            }
        }
        return;
    }

    let mut y = r.y;
    let part_count = msg.parts.len();
    for (pi, part) in msg.parts.iter_mut().enumerate() {
        match part {
            Part::Thinking(th) => {
                let header = rect(r.x, y, r.w, 34.0);
                let id = 0xC000 + ((index as u64) << 8) + pi as u64;
                let resp = ui.interact(id, header);
                if resp.clicked {
                    th.open = !th.open;
                }
                if resp.hovered {
                    ui.cursor = CursorKind::Hand;
                }
                let streaming = !th.done && !msg.finished;
                let total_h = thinking_height(th, scale);
                let card = rect(r.x, y, r.w, total_h);
                ui.card(card, OVERLAY_RADIUS, t.card_secondary.mix(t.card, resp.hover_t), t.card_stroke);
                ui.icon(if th.open { Icon::ChevronDown } else { Icon::ChevronRight }, r.x + 16.0, y + 17.0, 10.0, t.text_secondary);
                let elapsed = (if streaming { ui.time } else { th.ended } - th.started).max(0.0);
                let title = if streaming { format!("思考中… {elapsed:.1} s") } else { format!("思考了 {elapsed:.1} s") };
                ui.label(r.x + 30.0, y + 9.0, &title, &CAPTION_STRONG, t.text_secondary);
                if streaming {
                    ui.progress_ring(r.right() - 18.0, y + 17.0, 7.0, 1.5);
                }
                if !th.open {
                    // The newest line, so that progress shows without opening the panel.
                    let text = th.doc.text();
                    let tail: String = text.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
                    let tail = tail.rsplit('\n').find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_owned();
                    let x = r.x + 136.0;
                    let right_pad = if streaming { 36.0 } else { 12.0 };
                    ui.label_ellipsis(x, y + 9.0, (r.right() - x - right_pad).max(0.0), &tail, &CAPTION, t.text_tertiary);
                } else {
                    ui.doc(&mut th.doc, r.x + 12.0, y + 34.0);
                }
                y += total_h + 8.0;
            }
            Part::Tool(card) => {
                let c = rect(r.x, y, r.w, TOOL_CARD_H);
                ui.card(c, OVERLAY_RADIUS, t.card, t.card_stroke);
                ui.icon(Icon::Tool, c.x + 18.0, c.y + 18.0, 14.0, t.accent);
                let w = ui.label(c.x + 34.0, c.y + 8.0, &card.name, &BODY_STRONG, t.text_primary);
                ui.label_ellipsis(c.x + 34.0, c.y + 34.0, c.w - 48.0, &card.args, &MONO_SMALL, t.text_secondary);
                match &card.output {
                    None => ui.progress_ring(c.right() - 18.0, c.y + 18.0, 7.0, 1.5),
                    Some((output, is_error, took)) => {
                        let color = if *is_error { t.critical } else { t.success };
                        ui.icon(if *is_error { Icon::Close } else { Icon::Check }, c.right() - 18.0, c.y + 18.0, 12.0, color);
                        let x = c.x + 34.0 + w + 10.0;
                        ui.label_ellipsis(x, c.y + 10.0, (c.right() - x - 90.0).max(0.0), &format!("→ {output}"), &CAPTION, t.text_secondary);
                        ui.label_right(c.right() - 32.0, c.y + 10.0, &format_ms(took.as_secs_f32() * 1000.0), &CAPTION, t.text_tertiary);
                    }
                }
                y += TOOL_CARD_H + 8.0;
            }
            Part::Text(md) => {
                let doc_h = md.doc.height() / scale;
                let doc_rect = rect(r.x, y, r.w, doc_h.max(1.0));
                // Backgrounds and rules for the blocks in view.
                let view_top = (ui.clip.y - y) * scale;
                let view_bottom = (ui.clip.bottom() - y) * scale;
                let mut spans = Vec::new();
                md.doc.block_spans(BlockKind::Code, view_top..view_bottom, &mut spans);
                for [top, bottom, _] in spans.drain(..) {
                    let s = rect(r.x, y + top / scale - 8.0, r.w, (bottom - top) / scale + 16.0);
                    ui.card(s, CONTROL_RADIUS, t.code_bg, t.card_stroke);
                }
                md.doc.block_spans(BlockKind::Quote, view_top..view_bottom, &mut spans);
                for [top, bottom, _] in spans.drain(..) {
                    ui.fill(rect(r.x + 2.0, y + top / scale, 3.0, (bottom - top) / scale), 1.5, t.accent);
                }
                md.doc.block_spans(BlockKind::Rule, view_top..view_bottom, &mut spans);
                for [top, bottom, _] in spans.drain(..) {
                    ui.fill(rect(r.x, y + (top + bottom) * 0.5 / scale, r.w, 1.0), 0.0, t.divider);
                }

                // Selection by dragging across the text.
                let hit_point = ui.input.pointer.map(|(px, py)| md.doc.hit((px - r.x) * scale, (py - y) * scale));
                if ui.pointer_in(&doc_rect) {
                    ui.cursor = CursorKind::Text;
                    if ui.input.pressed {
                        *clicked = true;
                        if let Some(at) = hit_point {
                            *new_sel = Some(Selection { msg: index, part: pi, a: at, b: at });
                        }
                    }
                }
                if let Some(s) = &sel
                    && s.part == pi {
                        if dragging && ui.input.down
                            && let Some(at) = hit_point {
                                *new_sel = Some(Selection { b: at, ..s.clone() });
                            }
                        let (a, b) = (s.a.min(s.b), s.a.max(s.b));
                        if a < b {
                            let mut rects = Vec::new();
                            md.doc.selection_rects(a..b, view_top..view_bottom, &mut rects);
                            for [x, ry, w, h] in rects {
                                ui.fill(rect(r.x + x / scale, y + ry / scale, w / scale, h / scale), 2.0, t.selection);
                            }
                        }
                    }
                ui.doc(&mut md.doc, r.x, y);

                // A live caret at the end of a stream that is still running.
                if is_last && !msg.finished && pi + 1 == part_count
                    && !md.doc.lines.is_empty() {
                        let (cx, cy, h) = md.doc.caret_pos(usize::MAX);
                        let pulse = 0.55 + 0.45 * (ui.time * 5.0).sin();
                        ui.fill(rect(r.x + cx / scale + 2.0, y + cy / scale + 3.0, 2.0, h / scale - 6.0), 1.0, t.accent.fade(pulse));
                        ui.animating = true;
                    }
                y += doc_h + 8.0;
            }
        }
    }

    if msg.parts.is_empty() && !msg.finished {
        ui.progress_ring(r.x + 12.0, y + 14.0, 8.0, 2.0);
        ui.label(r.x + 30.0, y + 4.0, if msg.retries > 0 { "重新连接中…" } else { "等待响应…" }, &CAPTION, t.text_secondary);
        y += 28.0;
    }

    if let Some(err) = &msg.error {
        let cancelled = err == "已停止";
        let severity = if cancelled { Severity::Info } else { Severity::Error };
        ui.info_bar(rect(r.x, y, r.w, 36.0), severity, if cancelled { "已停止" } else { "失败" }, if cancelled { "" } else { err });
        y += 40.0;
    }

    // One line of honest numbers under each answer.
    if msg.tokens > 0 {
        let active = msg.last_token - msg.first_token.unwrap_or(msg.last_token);
        let mut note = format!("{} tok · 首 token {}", thousands(msg.tokens as u64), format_ms(msg.ttft_ms.unwrap_or(0.0)));
        // A rate needs a span of time to mean anything.
        if msg.tokens >= 8 && active > 0.05 {
            note.push_str(&format!(" · {:.0} tok/s", msg.tokens.saturating_sub(1) as f32 / active));
        }
        if msg.retries > 0 {
            note.push_str(&format!(" · 重连 {} 次 · 续传 {} 次", msg.retries, msg.resumes));
        }
        ui.label(r.x, y + 2.0, &note, &CAPTION, t.text_tertiary);
    }
}

fn describe(m: &MediaInput) -> String {
    let size = if m.raw_len >= 1 << 20 { format!("{:.1} MB", m.raw_len as f64 / 1048576.0) } else { format!("{} KB", m.raw_len >> 10) };
    match m.kind {
        MediaKind::Image => match m.transcoded_from {
            Some((w, h)) => format!("{w}×{h} → {}×{} · {size}", m.info.width, m.info.height),
            None => format!("{}×{} · {size}", m.info.width, m.info.height),
        },
        MediaKind::Video => {
            if m.info.duration_ms > 0 {
                format!("{}×{} · {:.1} s · {} · {size}", m.info.width, m.info.height, m.info.duration_ms as f64 / 1000.0, m.info.codec)
            } else {
                format!("视频 · {size}")
            }
        }
    }
}

/// A small RGBA thumbnail, or nothing if the image cannot be decoded here.
fn make_thumb(path: &std::path::Path) -> Option<Thumbnail> {
    let image = image::open(path).ok()?;
    let thumb = image.thumbnail(288, 192).to_rgba8();
    let (w, h) = thumb.dimensions();
    Some((thumb.into_raw(), w, h))
}
