//! The wall: every concurrent stream on screen at once, live.
//!
//! This page is a stress test of the renderer as much as of the network. At 128 streams it is showing dozens
//! of token updates in every frame, and the frame budget still has to hold.

use std::collections::VecDeque;
use std::sync::Arc;

use agent_rt::media::MediaInput;
use agent_rt::{run_agent, AgentConfig, Calc, Cancel, FailKind, Message, Role, ToolRegistry};
use fluent::theme::*;
use fluent::ui::{rect, Rect, Ui};
use fluent::{Color, Doc, TextCtx};

use crate::app::Conn;
use crate::bridge::{Inbox, InboxData, LogHist, Notifier, Op};

pub const TILE_ID_BASE: u32 = 1000;
pub const SCENARIOS: [&str; 5] = ["文本", "图片", "视频", "混合", "工具调用"];
pub const SIZES: [&str; 5] = ["8 路", "32 路", "64 路", "128 路", "128 路（循环）"];
const SIZE_VALUES: [(usize, bool); 5] = [(8, false), (32, false), (64, false), (128, false), (128, true)];

const TINY: TypeStyle = TypeStyle { size: 8.5, line: 11.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
const SMALL: TypeStyle = TypeStyle { size: 10.0, line: 13.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
const TILE_HEAD: TypeStyle = TypeStyle { size: 10.0, line: 13.0, weight: Weight::Semibold, mono: false, italic: false, serif: false };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileState {
    Idle,
    Queued,
    Connecting,
    Prompt,
    Thinking,
    Streaming,
    Tool,
    Retrying,
    Done,
    Failed,
}

pub struct Tile {
    inbox: Arc<Inbox>,
    doc: Doc,
    pub state: TileState,
    tokens: u32,
    attempt: u32,
    retries: u32,
    flash: f32,
    fail: Option<FailKind>,
    fail_detail: String,
}

impl Tile {
    fn new(id: u32, notifier: &Arc<Notifier>) -> Self {
        let mut doc = Doc::new(160.0);
        doc.track_carets = false;
        doc.max_lines = 12;
        Self {
            inbox: Inbox::new(id, notifier.clone(), true),
            doc,
            state: TileState::Idle,
            tokens: 0,
            attempt: 0,
            retries: 0,
            flash: 0.0,
            fail: None,
            fail_detail: String::new(),
        }
    }

    fn reset(&mut self) {
        self.doc.clear();
        self.state = TileState::Queued;
        self.tokens = 0;
        self.attempt = 0;
        self.retries = 0;
        self.flash = 0.0;
        self.fail = None;
        self.fail_detail.clear();
    }
}

#[derive(Default, Clone)]
pub struct SwarmMedia {
    pub image: Option<Arc<MediaInput>>,
    pub video: Option<Arc<MediaInput>>,
}

pub struct SwarmPage {
    pub tiles: Vec<Tile>,
    notifier: Arc<Notifier>,
    cancel: Option<Cancel>,
    pub scenario: usize,
    pub size_choice: usize,
    pub media: SwarmMedia,
    active_n: usize,
    rates: VecDeque<f32>,
    bucket_tokens: u32,
    bucket_start: f32,
    pub total_tokens: u64,
    ttft: LogHist,
    gaps: LogHist,
    pub retries: u64,
    pub resumes: u64,
    pub finished_ok: u64,
    pub finished_failed: u64,
    run_started: f32,
    running: bool,
    scratch: InboxData,
    tile_font_is_small: bool,
    last_tile_w: f32,
    last_error: Option<String>,
}

fn prompt_for(index: usize, round: u64) -> String {
    const TOPICS: [&str; 8] = [
        "how TCP recovers from a lost packet",
        "why zero-copy parsers are fast",
        "what makes GPU text rendering hard",
        "how a tokio runtime schedules tasks",
        "the history of the Fluent design language",
        "how Mica differs from Acrylic",
        "how prompt caching works in llama.cpp",
        "the difference between HTTP/1.1 and HTTP/2 under packet loss",
    ];
    format!("#{index}.{round} In a few short paragraphs, explain {}.", TOPICS[(index + round as usize) % TOPICS.len()])
}

impl SwarmPage {
    pub fn new(notifier: Arc<Notifier>) -> Self {
        Self {
            tiles: Vec::new(),
            notifier,
            cancel: None,
            scenario: 0,
            size_choice: 3,
            media: SwarmMedia::default(),
            active_n: 0,
            rates: VecDeque::with_capacity(128),
            bucket_tokens: 0,
            bucket_start: 0.0,
            total_tokens: 0,
            ttft: LogHist::default(),
            gaps: LogHist::default(),
            retries: 0,
            resumes: 0,
            finished_ok: 0,
            finished_failed: 0,
            run_started: 0.0,
            running: false,
            scratch: InboxData::default(),
            tile_font_is_small: false,
            last_tile_w: 0.0,
            last_error: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn stop(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        self.running = false;
    }

    pub fn start(&mut self, conn: &Conn, time: f32) {
        self.stop();
        let (n, repeat) = SIZE_VALUES[self.size_choice];
        while self.tiles.len() < n {
            let id = TILE_ID_BASE + self.tiles.len() as u32;
            self.tiles.push(Tile::new(id, &self.notifier));
        }
        self.active_n = n;
        self.rates.clear();
        self.bucket_tokens = 0;
        self.bucket_start = time;
        self.total_tokens = 0;
        self.ttft = LogHist::default();
        self.gaps = LogHist::default();
        self.retries = 0;
        self.resumes = 0;
        self.finished_ok = 0;
        self.finished_failed = 0;
        self.run_started = time;
        self.running = true;
        self.last_error = None;

        let cancel = Cancel::new();
        self.cancel = Some(cancel.clone());
        for (i, tile) in self.tiles.iter_mut().take(n).enumerate() {
            tile.reset();
            let inbox = tile.inbox.clone();
            let rt = conn.runtime.clone();
            let mut cfg = conn.agent.clone();
            cfg.max_tokens = cfg.max_tokens.min(160);
            cfg.max_steps = 1;
            let scenario = self.scenario;
            let media = self.media.clone();
            let cancel = cancel.clone();
            conn.handle.spawn(async move {
                let mut round = 0u64;
                loop {
                    round += 1;
                    let (conversation, cfg) = job(scenario, i, round, &cfg, &media);
                    let mut conversation = conversation;
                    let result = run_agent(&rt, &cfg, TILE_ID_BASE + i as u32, &mut conversation, inbox.as_ref(), &cancel).await;
                    let cancelled = cancel.is_cancelled();
                    match result {
                        Ok(s) => inbox.finished(None, s.attempts, s.resumes),
                        Err((e, s)) => inbox.finished(Some((e.kind, e.detail)), s.attempts, s.resumes),
                    }
                    if !repeat || cancelled {
                        break;
                    }
                }
            });
        }
    }

    /// Applies what one tile's task did since the last frame. Only tiles that changed ever get here.
    pub fn drain_tile(&mut self, index: usize, text: &mut TextCtx, theme: &Theme) {
        let mut data = std::mem::take(&mut self.scratch);
        let tile = &mut self.tiles[index];
        tile.inbox.drain(&mut data);
        self.total_tokens += data.tokens as u64;
        self.bucket_tokens += data.tokens;
        self.gaps.merge(&data.gaps);
        let ty = if self.tile_font_is_small { TINY } else { SMALL };
        for op in &data.ops {
            match op {
                Op::Queued => {
                    if tile.state != TileState::Queued && tile.tokens > 0 {
                        // Next round of a repeating run: keep the wall alive with fresh text.
                        tile.doc.clear();
                        tile.tokens = 0;
                    }
                    tile.state = TileState::Queued;
                }
                Op::Attempt { attempt, resumed_bytes } => {
                    tile.attempt = *attempt;
                    tile.state = TileState::Connecting;
                    if *resumed_bytes > 0 {
                        self.resumes += 1;
                    }
                }
                Op::Connected(_) => tile.state = TileState::Prompt,
                Op::Progress { .. } => tile.state = TileState::Prompt,
                Op::FirstToken { us } => self.ttft.record(*us),
                Op::Reasoning(a, b) => {
                    tile.state = TileState::Thinking;
                    tile.doc.push(text, &data.arena[*a as usize..*b as usize], ty, theme.text_tertiary);
                }
                Op::Text(a, b) => {
                    tile.state = TileState::Streaming;
                    tile.doc.push(text, &data.arena[*a as usize..*b as usize], ty, theme.text_secondary);
                }
                Op::ToolStart { name, .. } => {
                    tile.state = TileState::Tool;
                    tile.doc.push(text, &format!("[{name}] "), ty, theme.accent_text);
                }
                Op::ToolArgs { range: (a, b), .. } => tile.doc.push(text, &data.arena[*a as usize..*b as usize], ty, theme.accent_text),
                Op::ToolRewind => tile.doc.push(text, " [rewind] ", ty, theme.caution),
                Op::ToolResult { output, .. } => tile.doc.push(text, &format!(" = {output} "), ty, theme.accent_text),
                Op::Retry { .. } => {
                    tile.retries += 1;
                    tile.state = TileState::Retrying;
                    tile.flash = 1.0;
                    self.retries += 1;
                }
                Op::TurnEnd { .. } => {}
                Op::Finished { error, .. } => match error {
                    None => {
                        tile.state = TileState::Done;
                        self.finished_ok += 1;
                    }
                    Some((FailKind::Cancelled, _)) => tile.state = TileState::Idle,
                    Some((kind, detail)) => {
                        tile.state = TileState::Failed;
                        tile.fail = Some(*kind);
                        tile.fail_detail = detail.clone();
                        self.finished_failed += 1;
                        self.last_error = Some(format!("{}: {detail}", kind.label()));
                    }
                },
            }
        }
        tile.tokens += data.tokens;
        self.scratch = data;
    }

    fn update_rates(&mut self, time: f32) {
        const BIN: f32 = 0.25;
        while time - self.bucket_start >= BIN {
            self.bucket_start += BIN;
            if self.rates.len() >= 120 {
                self.rates.pop_front();
            }
            self.rates.push_back(self.bucket_tokens as f32 / BIN);
            self.bucket_tokens = 0;
        }
        if self.running && self.active_n > 0 {
            let all_over = self.tiles.iter().take(self.active_n).all(|t| matches!(t.state, TileState::Done | TileState::Failed | TileState::Idle));
            if all_over {
                self.running = false;
                self.cancel = None;
            }
        }
    }

    pub fn current_rate(&self) -> f32 {
        // Averaged over a second, because one bin is too jumpy to read.
        let n = self.rates.len().min(4);
        if n == 0 { 0.0 } else { self.rates.iter().rev().take(n).sum::<f32>() / n as f32 }
    }

    pub fn show(&mut self, ui: &mut Ui<'_>, area: Rect, conn: &Conn, frame_ms: f32, fps: f32) {
        self.update_rates(ui.time);
        let t = ui.theme;
        let pad = 16.0;
        let mut y = area.y + 10.0;

        ui.label(area.x + pad, y, "压力墙", &SUBTITLE, t.text_primary);
        let bar_x = area.x + pad + 90.0;
        let bar = rect(bar_x, y, 0.0, 32.0);

        let mut x = bar.x;
        let running = self.running;
        if running {
            if ui.button(0xA001, rect(x, bar.y, 92.0, 32.0), "停止").clicked {
                self.stop();
            }
        } else if ui.accent_button(0xA001, rect(x, bar.y, 92.0, 32.0), "开始", true).clicked {
            self.start(conn, ui.time);
        }
        x += 100.0;
        if let Some(i) = ui.combo(0xA100, rect(x, bar.y, 112.0, 32.0), &SCENARIOS, self.scenario) {
            self.scenario = i;
        }
        x += 120.0;
        if let Some(i) = ui.combo(0xA200, rect(x, bar.y, 148.0, 32.0), &SIZES, self.size_choice) {
            self.size_choice = i;
        }
        x += 160.0;
        let needs = match self.scenario {
            1 => self.media.image.is_none(),
            2 => self.media.video.is_none(),
            3 => self.media.image.is_none() || self.media.video.is_none(),
            _ => false,
        };
        if needs {
            let note = "需要先在对话页附加图片和视频";
            ui.label(x, bar.y + 8.0, note, &CAPTION, t.caution);
        } else if let Some(err) = &self.last_error {
            let err = err.clone();
            ui.label_ellipsis(x, bar.y + 8.0, (area.right() - x - pad).max(0.0), &err, &CAPTION, t.critical);
        }
        y += 44.0;

        // Metric cards.
        let cards = 6;
        let gap = 8.0;
        let card_w = ((area.w - pad * 2.0 - gap * (cards - 1) as f32) / cards as f32).max(60.0);
        let card_h = 74.0;
        let mut cx = area.x + pad;
        let rate = self.current_rate();
        let max_rate = self.rates.iter().copied().fold(1.0f32, f32::max);
        let ms = |us: u64| us as f32 / 1000.0;

        let card = |ui: &mut Ui<'_>, cx: f32, title: &str, value: &str, sub: &str| {
            let r = rect(cx, y, card_w, card_h);
            let t = ui.theme;
            ui.card(r, OVERLAY_RADIUS, t.card, t.card_stroke);
            ui.label(r.x + 12.0, r.y + 8.0, title, &CAPTION, t.text_secondary);
            ui.label(r.x + 12.0, r.y + 24.0, value, &SUBTITLE, t.text_primary);
            ui.label(r.x + 12.0, r.y + 52.0, sub, &CAPTION, t.text_tertiary);
            r
        };

        let r = card(ui, cx, "吞吐", &thousands(rate as u64).to_string(), &format!("tok/s · 共 {}", thousands(self.total_tokens)));
        let spark = rect(r.x + r.w * 0.5, r.y + 12.0, r.w * 0.5 - 10.0, 34.0);
        let values: Vec<f32> = self.rates.iter().copied().collect();
        ui.sparkline(spark, values.into_iter(), max_rate * 1.1, t.accent);
        cx += card_w + gap;

        card(ui, cx, "首 token 延迟", &format_ms(ms(self.ttft.quantile_us(0.5))), &format!("p99 {}", format_ms(ms(self.ttft.quantile_us(0.99)))));
        cx += card_w + gap;
        card(ui, cx, "token 间隔 p99", &format_ms(ms(self.gaps.quantile_us(0.99))), &format!("p50 {} · 最大 {}", format_ms(ms(self.gaps.quantile_us(0.5))), format_ms(ms(self.gaps.max_us))));
        cx += card_w + gap;

        let live = self.tiles.iter().take(self.active_n).filter(|t| matches!(t.state, TileState::Connecting | TileState::Prompt | TileState::Thinking | TileState::Streaming | TileState::Tool | TileState::Retrying)).count();
        card(ui, cx, "流", &format!("{live}"), &format!("完成 {} · 失败 {}", thousands(self.finished_ok), self.finished_failed));
        cx += card_w + gap;
        card(ui, cx, "重连", &thousands(self.retries), &format!("断点续传 {}", thousands(self.resumes)));
        cx += card_w + gap;
        card(ui, cx, "UI 帧", &format!("{frame_ms:.1} ms"), &format!("{fps:.0} fps"));
        y += card_h + 10.0;

        // The grid: tile size is chosen so that all of them fit, with no scrolling and no culling to hide behind.
        let grid = rect(area.x + pad, y, area.w - pad * 2.0, area.bottom() - y - pad);
        let n = self.active_n;
        if n == 0 {
            ui.label_centered(grid, "选择场景和路数后点击“开始”", &BODY, t.text_tertiary);
            return;
        }
        let (cols, tile_w, tile_h) = fit_grid(grid.w, grid.h, n, 4.0);
        let small = tile_h < 96.0 || tile_w < 150.0;
        if small != self.tile_font_is_small || (tile_w - self.last_tile_w).abs() > 0.5 {
            self.tile_font_is_small = small;
            self.last_tile_w = tile_w;
        }
        let ty = if small { TINY } else { SMALL };
        let doc_w = ((tile_w - 10.0) * ui.scale).max(8.0);
        let lines = ((tile_h - 19.0) / ty.line).floor().max(1.0) as usize;

        for i in 0..n.min(self.tiles.len()) {
            let (col, row) = (i % cols, i / cols);
            let r = rect(grid.x + col as f32 * (tile_w + 4.0), grid.y + row as f32 * (tile_h + 4.0), tile_w, tile_h);
            let tile = &mut self.tiles[i];
            tile.doc.max_lines = lines + 2;
            tile.doc.set_width(ui.text, doc_w);
            let color = state_color(&t, tile.state);
            let flash = tile.flash;
            if flash > 0.0 {
                tile.flash = (flash - ui.dt * 1.6).max(0.0);
                ui.animating = true;
            }
            let fill = t.card_secondary.mix(t.caution.with_alpha(0.25), flash);
            let stroke = if tile.state == TileState::Failed { t.critical.with_alpha(0.6) } else { t.card_stroke.mix(t.caution, flash) };
            ui.card(r, CONTROL_RADIUS, fill, stroke);

            ui.fill(rect(r.x + 5.0, r.y + 5.0, 6.0, 6.0), 3.0, color);
            let head = &TILE_HEAD;
            ui.label(r.x + 14.0, r.y + 1.5, &num_label(i + 1), head, t.text_secondary);
            let tail = if tile.retries > 0 { format!("{} ↻{}", tile.tokens, tile.retries) } else { format!("{}", tile.tokens) };
            ui.label_right(r.right() - 5.0, r.y + 1.5, &tail, &CAPTION_TINY, t.text_tertiary);

            let inner = rect(r.x + 5.0, r.y + 16.0, r.w - 10.0, r.h - 19.0);
            let height = tile.doc.height() / ui.scale;
            // Bottom-aligned, so that the newest text is what stays visible.
            let doc_y = inner.y + (inner.h - height).min(0.0);
            ui.with_clip(inner, |ui| ui.doc(&mut tile.doc, inner.x, doc_y));
        }
    }
}

const CAPTION_TINY: TypeStyle = TypeStyle { size: 9.0, line: 13.0, weight: Weight::Regular, mono: false, italic: false, serif: false };

fn job(scenario: usize, index: usize, round: u64, base: &AgentConfig, media: &SwarmMedia) -> (Vec<Message>, AgentConfig) {
    let mut cfg = base.clone();
    cfg.seed = Some(index as u64 * 1009 + round);
    // Only the tool scenario is about tools. This model calls one whenever one is on offer, and the wall measures streams.
    cfg.tools = ToolRegistry::new();
    let scenario = match scenario {
        3 => match index % 10 {
            0 => 2,
            1 | 2 => 1,
            _ => 0,
        },
        s => s,
    };
    match scenario {
        1 => {
            if let Some(image) = &media.image {
                return (vec![Message::user_with_media(format!("#{index}.{round} Describe this image."), vec![image.clone()])], cfg);
            }
        }
        2 => {
            if let Some(video) = &media.video {
                return (vec![Message::user_with_media(format!("#{index}.{round} Describe this video."), vec![video.clone()])], cfg);
            }
        }
        4 => {
            cfg.tools = ToolRegistry::new().with(Calc);
            cfg.max_steps = 4;
            cfg.thinking = Some(false);
            let (a, b) = (1000 + index * 37 + round as usize, 100 + index);
            return (vec![Message::text(Role::User, format!("What is {a} * {b}? Use the calc tool, then state the result."))], cfg);
        }
        _ => {}
    }
    (vec![Message::text(Role::User, prompt_for(index, round))], cfg)
}

/// Chooses the column count that gives the largest readable tile.
pub fn fit_grid(w: f32, h: f32, n: usize, gap: f32) -> (usize, f32, f32) {
    let mut best = (1, w, h);
    let mut best_score = -1.0f32;
    for cols in 1..=n.max(1) {
        let rows = n.div_ceil(cols);
        let tw = (w - gap * (cols - 1) as f32) / cols as f32;
        let th = (h - gap * (rows - 1) as f32) / rows as f32;
        if tw <= 0.0 || th <= 0.0 {
            continue;
        }
        // Text reads along its width, so width counts for more than height in deciding what is best.
        let score = tw.min(th * 1.7);
        if score > best_score {
            best_score = score;
            best = (cols, tw, th);
        }
    }
    best
}

fn state_color(t: &Theme, s: TileState) -> Color {
    match s {
        TileState::Idle => t.text_disabled,
        TileState::Queued => t.text_tertiary,
        TileState::Connecting | TileState::Prompt => t.text_secondary,
        TileState::Thinking => t.accent_text,
        TileState::Streaming | TileState::Tool => t.accent,
        TileState::Retrying => t.caution,
        TileState::Done => t.success,
        TileState::Failed => t.critical,
    }
}

fn state_name(s: TileState) -> &'static str {
    match s {
        TileState::Idle => "空闲",
        TileState::Queued => "排队",
        TileState::Connecting => "连接中",
        TileState::Prompt => "处理提示词",
        TileState::Thinking => "思考",
        TileState::Streaming => "输出",
        TileState::Tool => "工具调用",
        TileState::Retrying => "重连中",
        TileState::Done => "完成",
        TileState::Failed => "失败",
    }
}

fn num_label(n: usize) -> String {
    format!("{n}")
}

pub fn thousands(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn format_ms(ms: f32) -> String {
    if ms >= 10_000.0 {
        format!("{:.0} s", ms / 1000.0)
    } else if ms >= 1000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_uses_all_the_space_for_128() {
        let (cols, w, h) = fit_grid(1560.0, 700.0, 128, 4.0);
        let rows = 128usize.div_ceil(cols);
        assert!(rows * cols >= 128);
        assert!(cols as f32 * (w + 4.0) <= 1564.1);
        assert!(rows as f32 * (h + 4.0) <= 704.1);
        assert!(w > 80.0 && h > 40.0, "{cols} cols, {w}x{h}");
    }

    #[test]
    fn number_formatting() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(format_ms(0.5), "0.5 ms");
        assert_eq!(format_ms(635.0), "635 ms");
        assert_eq!(format_ms(1500.0), "1.5 s");
        assert_eq!(format_ms(23000.0), "23 s");
    }
}
