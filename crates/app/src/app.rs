//! Application state and the shell: navigation pane, settings, and per-frame coordination.

use std::sync::Arc;
use std::time::Duration;

use agent_rt::media::MediaKind;
use agent_rt::{AgentConfig, Calc, Clock, RetryPolicy, Runtime, RuntimeConfig, ToolRegistry, Transport, TransportConfig};
use fluent::theme::*;
use fluent::ui::{rect, Icon, Rect, Severity, Ui};
use fluent::{Renderer, TextCtx, TextEdit};

use crate::bridge::Notifier;
use crate::chat::{ChatPage, CHAT_INBOX_ID};
use crate::swarm::{SwarmPage, TILE_ID_BASE};

/// Everything a page needs to start a run.
pub struct Conn {
    pub runtime: Arc<Runtime>,
    pub agent: AgentConfig,
    pub handle: tokio::runtime::Handle,
    pub model_label: String,
    pub endpoint_label: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Chat,
    Swarm,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug)]
pub struct ConnSettings {
    pub url: String,
    pub ca_path: String,
    pub model: String,
    pub thinking: bool,
    pub http2: bool,
    pub stall_secs: u64,
    pub media_inflight: usize,
}

pub fn build_conn(settings: &ConnSettings, handle: tokio::runtime::Handle) -> Result<Conn, String> {
    let ca_pem = if settings.ca_path.trim().is_empty() {
        None
    } else {
        Some(std::fs::read(settings.ca_path.trim()).map_err(|e| format!("{}: {e}", settings.ca_path))?)
    };
    let transport = Transport::new(TransportConfig { base_url: settings.url.trim().to_owned(), ca_pem, http2: settings.http2, h2_conns: 4, ..Default::default() })?;
    let runtime = Runtime::new(
        transport,
        RuntimeConfig { max_concurrency: 128, max_media_inflight: settings.media_inflight.max(1), stall_timeout: Duration::from_secs(settings.stall_secs.max(1)), retry: RetryPolicy::default() },
    );
    let mut agent = AgentConfig::new(settings.model.trim());
    agent.thinking = if settings.thinking { None } else { Some(false) };
    agent.tools = ToolRegistry::new().with(Calc).with(Clock);
    agent.max_tokens = 2048;
    let endpoint_label = settings.url.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches("/v1").to_owned();
    let secure = settings.url.trim().starts_with("https://");
    Ok(Conn {
        runtime,
        agent,
        handle,
        model_label: settings.model.trim().to_owned(),
        endpoint_label: format!("{} {endpoint_label}", if secure { "HTTPS" } else { "HTTP" }),
    })
}

/// Rolling frame timings, for the wall's readout and the benchmark.
#[derive(Default)]
pub struct Perf {
    pub last_ms: f32,
    pub smooth_ms: f32,
    pub fps: f32,
    frames: u32,
    window_start: f32,
}

impl Perf {
    pub fn record(&mut self, cpu_ms: f32, time: f32) {
        self.last_ms = cpu_ms;
        self.smooth_ms = if self.smooth_ms == 0.0 { cpu_ms } else { self.smooth_ms * 0.95 + cpu_ms * 0.05 };
        self.frames += 1;
        if time - self.window_start >= 0.5 {
            self.fps = self.frames as f32 / (time - self.window_start);
            self.frames = 0;
            self.window_start = time;
        }
    }
}

pub struct SettingsPage {
    url: TextEdit,
    ca: TextEdit,
    model: TextEdit,
    thinking: bool,
    http2: bool,
    message: Option<(Severity, String)>,
}

pub struct App {
    pub page: Page,
    pub chat: ChatPage,
    pub swarm: SwarmPage,
    pub settings_page: SettingsPage,
    pub conn: Conn,
    pub settings: ConnSettings,
    pub theme_mode: ThemeMode,
    pub system_dark: bool,
    pub perf: Perf,
    pub top_inset: f32,
    notifier: Arc<Notifier>,
    dirty: Vec<u32>,
}

impl App {
    pub fn new(notifier: Arc<Notifier>, settings: ConnSettings, handle: tokio::runtime::Handle) -> Result<Self, String> {
        let conn = build_conn(&settings, handle)?;
        let mut url = TextEdit::new();
        url.set_text(settings.url.clone());
        let mut ca = TextEdit::new();
        ca.set_text(settings.ca_path.clone());
        let mut model = TextEdit::new();
        model.set_text(settings.model.clone());
        Ok(Self {
            page: Page::Chat,
            chat: ChatPage::new(notifier.clone()),
            swarm: SwarmPage::new(notifier.clone()),
            settings_page: SettingsPage { url, ca, model, thinking: settings.thinking, http2: settings.http2, message: None },
            conn,
            settings,
            theme_mode: ThemeMode::System,
            system_dark: true,
            perf: Perf::default(),
            top_inset: if cfg!(target_os = "macos") { 28.0 } else { 0.0 },
            notifier,
            dirty: Vec::with_capacity(256),
        })
    }

    pub fn theme(&self) -> Theme {
        let dark = match self.theme_mode {
            ThemeMode::System => self.system_dark,
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
        };
        if dark { Theme::dark() } else { Theme::light() }
    }

    /// Applies new tokens. Only sessions that actually changed are looked at.
    pub fn drain(&mut self, text: &mut TextCtx, theme: &Theme, time: f32, area_w: f32) {
        self.notifier.take_dirty(&mut self.dirty);
        let dirty = std::mem::take(&mut self.dirty);
        let col_w = (area_w - 64.0).clamp(280.0, 820.0);
        // Attachments finish outside of the inbox, so the chat gets checked on every wake.
        self.chat.drain(text, theme, time, col_w);
        for &id in &dirty {
            if id != CHAT_INBOX_ID && id >= TILE_ID_BASE {
                let index = (id - TILE_ID_BASE) as usize;
                if index < self.swarm.tiles.len() {
                    self.swarm.drain_tile(index, text, theme);
                }
            }
        }
        let mut dirty = dirty;
        dirty.clear();
        self.dirty = dirty;
    }

    pub fn prepare_gpu(&mut self, renderer: &mut Renderer) {
        self.chat.upload_thumbs(renderer);
    }

    pub fn show(&mut self, ui: &mut Ui<'_>) {
        let t = ui.theme;
        let (w, h) = ui.size;
        let compact = w < 760.0;
        let nav_w = if compact { 48.0 } else { 188.0 };
        let nav = rect(0.0, 0.0, nav_w, h);
        self.show_nav(ui, nav, compact);

        // Fluent's content layer: a rounded surface that lifts off the Mica backdrop.
        let content = rect(nav_w, self.top_inset.max(8.0), w - nav_w, h - self.top_inset.max(8.0));
        ui.fill_radii(content, [OVERLAY_RADIUS, 0.0, 0.0, 0.0], t.layer);
        ui.fill(rect(content.x, content.y + OVERLAY_RADIUS, 1.0, content.h), 0.0, t.card_stroke);
        ui.fill(rect(content.x + OVERLAY_RADIUS, content.y, content.w, 1.0), 0.0, t.card_stroke);

        let saved = ui.clip;
        ui.clip = saved.intersect(&content);
        match self.page {
            Page::Chat => self.chat.show(ui, content, &self.conn),
            Page::Swarm => {
                self.swarm.media.image = self.chat.attached(MediaKind::Image).or(self.swarm.media.image.take());
                self.swarm.media.video = self.chat.attached(MediaKind::Video).or(self.swarm.media.video.take());
                let (ms, fps) = (self.perf.smooth_ms, self.perf.fps);
                self.swarm.show(ui, content, &self.conn, ms, fps);
            }
            Page::Settings => self.show_settings(ui, content),
        }
        ui.clip = saved;
    }

    fn show_nav(&mut self, ui: &mut Ui<'_>, nav: Rect, compact: bool) {
        let t = ui.theme;
        let mut y = self.top_inset + 8.0;
        if !compact {
            ui.label(nav.x + 16.0, y, "Fluent Agent", &BODY_STRONG, t.text_primary);
            y += 32.0;
        } else {
            y += 8.0;
        }
        let item_h = 36.0;
        let pad = 4.0;
        let items = [(Page::Chat, Icon::Chat, "对话"), (Page::Swarm, Icon::Grid, "压力墙")];
        for (i, (page, icon, label)) in items.iter().enumerate() {
            let r = rect(nav.x + pad, y, nav.w - pad * 2.0, item_h);
            let selected = self.page == *page;
            let label = if compact { "" } else { label };
            if ui.nav_item(0xD000 + i as u64, r, *icon, label, selected).clicked {
                self.page = *page;
            }
            y += item_h + 2.0;
        }

        let bottom = nav.bottom() - 8.0;
        let settings_rect = rect(nav.x + pad, bottom - item_h, nav.w - pad * 2.0, item_h);
        if ui.nav_item(0xD010, settings_rect, Icon::Settings, if compact { "" } else { "设置" }, self.page == Page::Settings).clicked {
            self.page = Page::Settings;
        }
        if !compact {
            let label_y = settings_rect.y - 44.0;
            ui.fill(rect(nav.x + 16.0, label_y + 6.0, 8.0, 8.0), 4.0, if self.swarm.is_running() || self.chat.is_running() { t.accent } else { t.success });
            ui.label_ellipsis(nav.x + 30.0, label_y, nav.w - 42.0, &self.conn.model_label, &CAPTION_STRONG, t.text_secondary);
            ui.label_ellipsis(nav.x + 30.0, label_y + 18.0, nav.w - 42.0, &self.conn.endpoint_label, &CAPTION, t.text_tertiary);
        }
    }

    fn show_settings(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let t = ui.theme;
        let col_w = (area.w - 64.0).clamp(280.0, 720.0);
        let x = area.x + 32.0;
        let mut y = area.y + 24.0;
        ui.label(x, y, "设置", &TITLE, t.text_primary);
        y += 56.0;

        let group = |ui: &mut Ui<'_>, y: f32, h: f32| {
            let r = rect(x, y, col_w, h);
            let t = ui.theme;
            ui.card(r, OVERLAY_RADIUS, t.card, t.card_stroke);
            r
        };

        ui.label(x, y, "连接", &BODY_STRONG, t.text_primary);
        y += 28.0;
        let r = group(ui, y, 188.0);
        let field_x = r.x + 16.0;
        let field_w = r.w - 32.0;
        let mut fy = r.y + 12.0;
        ui.label(field_x, fy, "服务地址（OpenAI 兼容，到 /v1 为止）", &CAPTION, t.text_secondary);
        self.settings_page.url.show(ui, 0xE001, rect(field_x, fy + 18.0, field_w, 32.0), "https://host:8443/v1", &BODY, false);
        fy += 58.0;
        ui.label(field_x, fy, "信任的 CA 证书（PEM 文件路径，公共证书留空）", &CAPTION, t.text_secondary);
        self.settings_page.ca.show(ui, 0xE002, rect(field_x, fy + 18.0, field_w, 32.0), "ca.pem", &BODY, false);
        fy += 58.0;
        ui.label(field_x, fy, "模型", &CAPTION, t.text_secondary);
        self.settings_page.model.show(ui, 0xE003, rect(field_x, fy + 18.0, field_w, 32.0), "LFM3-VL-2B", &BODY, false);
        y += 188.0 + 16.0;

        ui.label(x, y, "行为", &BODY_STRONG, t.text_primary);
        y += 28.0;
        let row_h = 56.0;
        let rows: [(&str, &str, u64); 2] = [
            ("思考过程", "让模型先输出 reasoning_content", 0xE010),
            ("HTTP/2 多路复用", "共用少量连接；丢一个包会阻塞其上所有流", 0xE011),
        ];
        for (i, (title, desc, id)) in rows.iter().enumerate() {
            let r = group(ui, y, row_h);
            ui.label(r.x + 16.0, r.y + 9.0, title, &BODY, t.text_primary);
            ui.label(r.x + 16.0, r.y + 30.0, desc, &CAPTION, t.text_secondary);
            let value = if i == 0 { &mut self.settings_page.thinking } else { &mut self.settings_page.http2 };
            ui.toggle(*id, rect(r.right() - 56.0, r.y, 40.0, row_h), value);
            y += row_h + 4.0;
        }
        let r = group(ui, y, row_h);
        ui.label(r.x + 16.0, r.y + 9.0, "外观", &BODY, t.text_primary);
        ui.label(r.x + 16.0, r.y + 30.0, "跟随系统或固定为浅色、深色", &CAPTION, t.text_secondary);
        let current = match self.theme_mode {
            ThemeMode::System => 0,
            ThemeMode::Light => 1,
            ThemeMode::Dark => 2,
        };
        if let Some(i) = ui.combo(0xE100, rect(r.right() - 136.0, r.y + 12.0, 120.0, 32.0), &["跟随系统", "浅色", "深色"], current) {
            self.theme_mode = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark][i];
        }
        y += row_h + 16.0;

        if ui.accent_button(0xE200, rect(x, y, 112.0, 32.0), "应用连接设置", true).clicked {
            self.apply_settings();
        }
        if let Some((severity, message)) = self.settings_page.message.clone() {
            ui.info_bar(rect(x + 124.0, y - 2.0, col_w - 124.0, 36.0), severity, if severity == Severity::Error { "无法应用" } else { "已应用" }, &message);
        }
    }

    fn apply_settings(&mut self) {
        let mut next = self.settings.clone();
        next.url = self.settings_page.url.text.clone();
        next.ca_path = self.settings_page.ca.text.clone();
        next.model = self.settings_page.model.text.clone();
        next.thinking = self.settings_page.thinking;
        next.http2 = self.settings_page.http2;
        match build_conn(&next, self.conn.handle.clone()) {
            Ok(conn) => {
                self.conn = conn;
                self.settings = next;
                self.settings_page.message = Some((Severity::Success, "新的请求将使用这些设置".into()));
            }
            Err(e) => self.settings_page.message = Some((Severity::Error, e)),
        }
    }
}
