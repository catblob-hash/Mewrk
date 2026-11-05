//! The settings page, and the conversation settings pane.

use std::sync::mpsc::{self, Receiver};

use fluent::theme::*;
use fluent::ui::{rect, CursorKind, Icon, MenuItem, Rect, Scroll, Ui};
use fluent::TextEdit;
use mewrk_core::*;
use serde_json::Value;

use crate::app::{App, Page};
use crate::layout::TILE_RADIUS;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Providers,
    General,
    About,
}

struct ModelRow {
    id: TextEdit,
    window: TextEdit,
    output: TextEdit,
}

pub struct ProviderForm {
    id: String,
    name: TextEdit,
    url: TextEdit,
    key: TextEdit,
    models: Vec<ModelRow>,
    key_set: bool,
}

pub struct SettingsPage {
    section: Section,
    pub message: Option<String>,
    selected: Option<String>,
    form: Option<ProviderForm>,
    scroll: Scroll,
    instructions: TextEdit,
    instructions_loaded: bool,
    fetch: Option<Receiver<Result<Vec<String>, String>>>,
    conv_instructions: Option<(String, TextEdit)>,
    conv_scroll: Scroll,
}

impl SettingsPage {
    pub fn new() -> Self {
        Self {
            section: Section::Providers,
            message: None,
            selected: None,
            form: None,
            scroll: Scroll::from_top(),
            instructions: TextEdit::new(),
            instructions_loaded: false,
            fetch: None,
            conv_instructions: None,
            conv_scroll: Scroll::from_top(),
        }
    }
}

struct Preset {
    label: &'static str,
    name: &'static str,
    kind: ProviderKind,
    url: &'static str,
    models: &'static [(&'static str, u64, u32)],
    prefill: bool,
}

const PRESETS: [Preset; 6] = [
    Preset { label: "Anthropic", name: "Anthropic", kind: ProviderKind::Anthropic, url: "https://api.anthropic.com/v1", models: &[("claude-opus-5-5", 200_000, 32_000), ("claude-sonnet-5-5", 200_000, 32_000), ("claude-haiku-5-5", 200_000, 16_000)], prefill: false },
    Preset { label: "OpenAI", name: "OpenAI", kind: ProviderKind::OpenAiResponses, url: "https://api.openai.com/v1", models: &[], prefill: false },
    Preset { label: "DeepSeek", name: "DeepSeek", kind: ProviderKind::OpenAiChat, url: "https://api.deepseek.com/v1", models: &[], prefill: false },
    Preset { label: "OpenRouter", name: "OpenRouter", kind: ProviderKind::OpenAiChat, url: "https://openrouter.ai/api/v1", models: &[], prefill: false },
    Preset { label: "LFM3 (本机网络)", name: "LFM3", kind: ProviderKind::OpenAiChat, url: "http://127.0.0.1:8080/v1", models: &[("LFM3-VL-2B", 128_000, 4_096)], prefill: true },
    Preset { label: "自定义 OpenAI 兼容", name: "自定义", kind: ProviderKind::OpenAiChat, url: "", models: &[], prefill: false },
];

fn form_for(p: &ProviderConfig) -> ProviderForm {
    let mut name = TextEdit::new();
    name.set_text(p.name.clone());
    let mut url = TextEdit::new();
    url.set_text(p.base_url.clone());
    let models = p
        .models
        .iter()
        .map(|m| {
            let mut id = TextEdit::new();
            id.set_text(m.id.clone());
            let mut window = TextEdit::new();
            window.set_text(if m.context_window > 0 { m.context_window.to_string() } else { String::new() });
            let mut output = TextEdit::new();
            output.set_text(if m.max_output > 0 { m.max_output.to_string() } else { String::new() });
            ModelRow { id, window, output }
        })
        .collect();
    ProviderForm { id: p.id.clone(), name, url, key: TextEdit::new(), models, key_set: false }
}

fn card_row(ui: &mut Ui<'_>, r: Rect, title: &str, hint: &str) -> Rect {
    let t = ui.theme;
    ui.label(r.x, r.y, title, &BODY_SMALL_STRONG, t.text_primary);
    if !hint.is_empty() {
        ui.label_ellipsis(r.x, r.y + 18.0, r.w, hint, &FINE, t.text_tertiary);
    }
    rect(r.x, r.y + if hint.is_empty() { 22.0 } else { 36.0 }, r.w, 32.0)
}

pub fn show(app: &mut App, ui: &mut Ui<'_>, field: Rect) {
    let t = ui.theme;
    ui.card_radii(field, [TILE_RADIUS, 0.0, 0.0, TILE_RADIUS], t.layer, t.card_stroke);
    ui.with_clip(field, |ui| {
        // The left rail.
        let rail = rect(field.x, field.y, 200.0, field.h);
        ui.label(rail.x + 20.0, rail.y + 18.0, "设置", &SUBTITLE, t.text_primary);
        let mut y = rail.y + 62.0;
        for (section, label, icon) in [(Section::Providers, "模型供应商", Icon::Pulse), (Section::General, "通用", Icon::Sliders), (Section::About, "关于", Icon::Info)] {
            let row = rect(rail.x + 10.0, y, rail.w - 20.0, 32.0);
            if ui.nav_item(0xD000 + section as u64, row, icon, label, app.settings.section == section).clicked {
                app.settings.section = section;
            }
            y += 34.0;
        }
        let body = rect(field.x + 200.0, field.y, field.w - 200.0, field.h);
        ui.fill(rect(body.x, body.y + 16.0, 1.0, body.h - 32.0), 0.0, t.divider);
        match app.settings.section {
            Section::Providers => providers(app, ui, body),
            Section::General => general(app, ui, body),
            Section::About => about(app, ui, body),
        }
    });
    if ui.input.keys.iter().any(|k| k.key == fluent::ui::Key::Escape) {
        app.page = Page::Chat;
    }
}

fn providers(app: &mut App, ui: &mut Ui<'_>, body: Rect) {
    let t = ui.theme;
    // Providers on the left of the page, the chosen one's form on the right.
    let list = rect(body.x + 16.0, body.y + 16.0, 200.0, body.h - 32.0);
    let add_r = rect(list.x, list.y, list.w, 32.0);
    if ui.button(0xD100, add_r, "添加供应商").clicked {
        ui.toggle_popup(0xD101);
    }
    let items: Vec<MenuItem<'_>> = PRESETS.iter().map(|p| MenuItem::new(p.label)).collect();
    if let Some(i) = ui.menu(0xD101, add_r, false, 220.0, &items) {
        let preset = &PRESETS[i];
        let id = new_id();
        let models = preset.models.iter().map(|(m, w, o)| ModelProfile { id: (*m).to_owned(), name: String::new(), context_window: *w, max_output: *o, vision: false }).collect();
        app.doc.settings.providers.push(ProviderConfig { id: id.clone(), name: preset.name.to_owned(), kind: preset.kind, base_url: preset.url.to_owned(), models, resume_prefill: preset.prefill, stall_secs: 0 });
        app.settings.selected = Some(id);
        app.settings.form = None;
        app.mark_dirty();
    }
    let mut y = list.y + 40.0;
    let ids: Vec<(String, String, usize)> = app.doc.settings.providers.iter().map(|p| (p.id.clone(), p.name.clone(), p.models.len())).collect();
    if app.settings.selected.is_none() {
        app.settings.selected = ids.first().map(|x| x.0.clone());
    }
    for (n, (id, name, count)) in ids.iter().enumerate() {
        let row = rect(list.x, y, list.w, 40.0);
        let resp = ui.interact(0xD110 + n as u64, row);
        let selected = app.settings.selected.as_deref() == Some(id.as_str());
        let fill = if selected { t.subtle_hover } else { t.subtle_hover.fade(resp.hover_t) };
        if fill.a > 0 {
            ui.fill(row, CONTROL_RADIUS, fill);
        }
        if selected {
            ui.fill(rect(row.x, row.y + 10.0, 3.0, 20.0), 1.5, t.accent);
        }
        ui.label_ellipsis(row.x + 12.0, row.y + 4.0, row.w - 20.0, name, &BODY_SMALL, t.text_primary);
        ui.label(row.x + 12.0, row.y + 22.0, &format!("{count} 个模型"), &FINE, t.text_tertiary);
        if resp.clicked {
            app.settings.selected = Some(id.clone());
            app.settings.form = None;
        }
        if resp.hovered {
            ui.cursor = CursorKind::Hand;
        }
        y += 42.0;
    }

    let form_area = rect(list.right() + 16.0, body.y, (body.right() - list.right() - 16.0).max(0.0), body.h);
    if let Some(msg) = app.settings.message.clone() {
        ui.info_bar(rect(form_area.x, form_area.y + 16.0, (form_area.w - 24.0).max(0.0), 36.0), Severity::Info, "", &msg);
    }
    let Some(selected) = app.settings.selected.clone() else {
        ui.label_centered(form_area, "还没有模型供应商。点“添加供应商”开始。", &BODY_SMALL, t.text_secondary);
        return;
    };
    let Some(index) = app.doc.settings.providers.iter().position(|p| p.id == selected) else {
        app.settings.selected = None;
        return;
    };
    if app.settings.form.as_ref().is_none_or(|f| f.id != selected) {
        app.settings.form = Some(form_for(&app.doc.settings.providers[index]));
    }
    if let Some(rx) = &app.settings.fetch
        && let Ok(result) = rx.try_recv()
    {
        app.settings.fetch = None;
        match result {
            Ok(ids) => {
                let p = &mut app.doc.settings.providers[index];
                for id in ids {
                    if !p.models.iter().any(|m| m.id == id) {
                        p.models.push(ModelProfile { id, ..Default::default() });
                    }
                }
                app.settings.form = Some(form_for(p));
                app.settings.message = None;
                app.mark_dirty();
            }
            Err(e) => app.settings.message = Some(format!("获取模型列表失败：{e}")),
        }
    }

    let mut form = app.settings.form.take().unwrap();
    let top = if app.settings.message.is_some() { 60.0 } else { 16.0 };
    let viewport = rect(form_area.x, form_area.y + top, form_area.w, (form_area.h - top).max(0.0));
    let content = 300.0 + form.models.len() as f32 * 40.0 + 80.0;
    app.settings.scroll.update(ui, 0xD120, viewport, content);
    let mut changed = false;
    let mut delete = false;
    let mut fetch = false;
    let w = (viewport.w - 40.0).clamp(200.0, 560.0);
    let x = viewport.x + 8.0;
    let saved = ui.clip;
    ui.clip = saved.intersect(&viewport);
    let mut y = viewport.y - app.settings.scroll.offset;

    let r = card_row(ui, rect(x, y, w, 60.0), "名称", "");
    if form.name.show(ui, 0xD130, r, "", &BODY_SMALL, true).changed {
        app.doc.settings.providers[index].name = form.name.text.clone();
        changed = true;
    }
    y += 64.0;
    let r = card_row(ui, rect(x, y, w, 60.0), "协议", "");
    let kinds: Vec<&str> = ProviderKind::ALL.iter().map(|k| k.label()).collect();
    let current = ProviderKind::ALL.iter().position(|k| *k == app.doc.settings.providers[index].kind).unwrap_or(0);
    if let Some(i) = ui.combo(0xD131, r, &kinds, current) {
        app.doc.settings.providers[index].kind = ProviderKind::ALL[i];
        changed = true;
    }
    y += 64.0;
    let r = card_row(ui, rect(x, y, w, 60.0), "接口地址", "");
    if form.url.show(ui, 0xD132, r, "https://…/v1", &BODY_SMALL, true).changed {
        app.doc.settings.providers[index].base_url = form.url.text.clone();
        changed = true;
    }
    y += 64.0;
    let has_key = app.keys_cache.get(&selected).map_or_else(|| secrets::get(&selected).is_some(), Option::is_some);
    let r = card_row(ui, rect(x, y, w, 76.0), "API Key", if has_key { "已保存在系统钥匙串里。输入新的会覆盖它。" } else { "保存在系统钥匙串里，不写入配置文件。" });
    let field = rect(r.x, r.y, (r.w - 84.0).max(60.0), r.h);
    let resp = form.key.show(ui, 0xD133, field, if has_key { "••••••••" } else { "sk-…" }, &BODY_SMALL, true);
    let save = rect(field.right() + 8.0, r.y, 76.0, 32.0);
    if (ui.button(0xD134, save, "保存").clicked || resp.submitted) && !form.key.text.trim().is_empty() {
        match secrets::set(&selected, form.key.text.trim()) {
            Ok(()) => {
                app.keys_cache.insert(selected.clone(), Some(form.key.text.trim().to_owned()));
                form.key.clear();
                form.key_set = true;
                app.settings.message = None;
            }
            Err(e) => app.settings.message = Some(format!("保存失败：{e}")),
        }
    }
    y += 80.0;

    ui.label(x, y, "模型", &BODY_SMALL_STRONG, t.text_primary);
    let add = rect(x + w - 76.0, y - 4.0, 76.0, 28.0);
    if ui.button(0xD135, add, "添加").clicked {
        app.doc.settings.providers[index].models.push(ModelProfile::default());
        form.models.push(ModelRow { id: TextEdit::new(), window: TextEdit::new(), output: TextEdit::new() });
        changed = true;
    }
    let get = rect(add.x - 118.0, y - 4.0, 110.0, 28.0);
    if ui.button(0xD136, get, if app.settings.fetch.is_some() { "获取中…" } else { "获取模型列表" }).clicked && app.settings.fetch.is_none() {
        fetch = true;
    }
    y += 28.0;
    ui.label(x, y, "模型 ID", &FINE, t.text_tertiary);
    ui.label(x + w - 208.0, y, "上下文窗口", &FINE, t.text_tertiary);
    ui.label(x + w - 112.0, y, "最大输出", &FINE, t.text_tertiary);
    y += 18.0;
    let mut remove = None;
    for (i, row) in form.models.iter_mut().enumerate() {
        let base = 0xD200 + i as u64 * 16;
        let idr = rect(x, y, (w - 248.0).max(80.0), 32.0);
        let mut touched = row.id.show(ui, base, idr, "model-id", &MONO_BODY, true).changed;
        touched |= row.window.show(ui, base + 1, rect(x + w - 208.0, y, 88.0, 32.0), "200000", &MONO_BODY, true).changed;
        touched |= row.output.show(ui, base + 2, rect(x + w - 112.0, y, 80.0, 32.0), "16000", &MONO_BODY, true).changed;
        if touched
            && let Some(m) = app.doc.settings.providers[index].models.get_mut(i)
        {
            m.id = row.id.text.trim().to_owned();
            m.context_window = row.window.text.trim().parse().unwrap_or(0);
            m.max_output = row.output.text.trim().parse().unwrap_or(0);
            changed = true;
        }
        let del = rect(x + w - 26.0, y + 4.0, 24.0, 24.0);
        let dresp = ui.interact(base + 3, del);
        if dresp.hover_t > 0.0 {
            ui.fill(del, CONTROL_RADIUS, t.subtle_hover.fade(dresp.hover_t));
        }
        ui.icon(Icon::Trash, del.x + 12.0, del.y + 12.0, 12.0, t.text_secondary);
        if dresp.clicked {
            remove = Some(i);
        }
        y += 40.0;
    }
    if let Some(i) = remove {
        form.models.remove(i);
        app.doc.settings.providers[index].models.remove(i);
        changed = true;
    }
    y += 12.0;
    let mut prefill = app.doc.settings.providers[index].resume_prefill;
    ui.label(x, y + 4.0, "断线时从已收到的文本续传（需要服务端支持前缀）", &CAPTION, t.text_secondary);
    if ui.toggle(0xD137, rect(x + w - 44.0, y, 44.0, 22.0), &mut prefill).clicked {
        app.doc.settings.providers[index].resume_prefill = prefill;
        changed = true;
    }
    y += 40.0;
    if ui.button(0xD138, rect(x, y, 120.0, 32.0), "删除这个供应商").clicked {
        delete = true;
    }
    ui.clip = saved;

    if fetch {
        let p = app.doc.settings.providers[index].clone();
        let key = secrets::get(&p.id);
        let (tx, rx) = mpsc::channel();
        app.settings.fetch = Some(rx);
        let wake = app.wake.clone();
        app.engine.rt.spawn(async move {
            let result = fetch_models(&p, key.as_deref()).await;
            let _ = tx.send(result);
            wake();
        });
    }
    app.settings.form = Some(form);
    if changed {
        app.mark_dirty();
    }
    if delete {
        let _ = secrets::set(&selected, "");
        app.keys_cache.remove(&selected);
        app.doc.settings.providers.remove(index);
        app.settings.selected = None;
        app.settings.form = None;
        app.mark_dirty();
    }
}

async fn fetch_models(p: &ProviderConfig, key: Option<&str>) -> Result<Vec<String>, String> {
    let transport = provider::build_transport(p, key)?;
    let v = transport.get_json("/models").await.map_err(|e| e.to_string())?;
    let list = v.get("data").and_then(Value::as_array).or_else(|| v.as_array()).ok_or("返回的不是模型列表")?;
    Ok(list.iter().filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_owned)).collect())
}

fn general(app: &mut App, ui: &mut Ui<'_>, body: Rect) {
    let t = ui.theme;
    let x = body.x + 24.0;
    let w = (body.w - 48.0).clamp(200.0, 620.0);
    let mut y = body.y + 22.0;
    ui.label(x, y, "主题", &BODY_SMALL_STRONG, t.text_primary);
    let options = ["跟随系统", "浅色", "深色"];
    let current = match app.doc.settings.theme {
        ThemeChoice::System => 0,
        ThemeChoice::Light => 1,
        ThemeChoice::Dark => 2,
    };
    if let Some(i) = ui.combo(0xD300, rect(x + w - 160.0, y - 6.0, 160.0, 32.0), &options, current) {
        app.doc.settings.theme = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark][i];
        app.mark_dirty();
    }
    y += 44.0;
    let toggle_row = |ui: &mut Ui<'_>, id: u64, y: &mut f32, label: &str, hint: &str, on: &mut bool| -> bool {
        ui.label(x, *y, label, &BODY_SMALL_STRONG, t.text_primary);
        ui.label(x, *y + 18.0, hint, &FINE, t.text_tertiary);
        let clicked = ui.toggle(id, rect(x + w - 44.0, *y + 6.0, 44.0, 22.0), on).clicked;
        *y += 48.0;
        clicked
    };
    let mut enter = app.doc.settings.enter_sends;
    if toggle_row(ui, 0xD301, &mut y, "Enter 发送", "关闭后用 Shift+Enter 发送，Enter 换行", &mut enter) {
        app.doc.settings.enter_sends = enter;
        app.mark_dirty();
    }
    let mut wide = app.doc.settings.wide_messages;
    if toggle_row(ui, 0xD302, &mut y, "加宽消息列", "对话内容的最大宽度从 820 放宽到 1180", &mut wide) {
        app.doc.settings.wide_messages = wide;
        app.mark_dirty();
    }
    ui.label(x, y, "全局指令", &BODY_SMALL_STRONG, t.text_primary);
    ui.label(x, y + 18.0, "加到每个对话的系统提示词里", &FINE, t.text_tertiary);
    y += 38.0;
    if !app.settings.instructions_loaded {
        app.settings.instructions.set_text(app.doc.settings.global_instructions.clone());
        app.settings.instructions_loaded = true;
    }
    if app.settings.instructions.show(ui, 0xD303, rect(x, y, w, 140.0), "", &BODY_SMALL, false).changed {
        app.doc.settings.global_instructions = app.settings.instructions.text.clone();
        app.mark_dirty();
    }
}

fn about(app: &mut App, ui: &mut Ui<'_>, body: Rect) {
    let t = ui.theme;
    let x = body.x + 24.0;
    let mut y = body.y + 22.0;
    ui.label(x, y, "Mewrk 原生版", &SUBTITLE, t.text_primary);
    y += 34.0;
    ui.label(x, y, "纯 Rust 重写：wgpu 渲染，rustls + Graviola 传输，不编译任何 C 代码。", &BODY_SMALL, t.text_secondary);
    y += 28.0;
    ui.label(x, y, &format!("数据目录：{}", app.store.dir().display()), &CAPTION, t.text_tertiary);
    y += 22.0;
    ui.label(x, y, "这个版本的数据和原版 Mewrk 分开存放，两者可以同时运行。", &CAPTION, t.text_tertiary);
}

/// The conversation settings pane: what this conversation may do and be told.
pub fn conversation_settings(app: &mut App, ui: &mut Ui<'_>, id: &str, base: u64, body: Rect) {
    let t = ui.theme;
    let Some(meta) = app.meta(id).cloned() else { return };
    let x = body.x + 14.0;
    let w = (body.w - 28.0).max(60.0);
    let tools = mewrk_core::tools::offered(false, &[]);
    let content = 210.0 + (tools.len() as f32) * 30.0 + 60.0;
    app.settings.conv_scroll.update(ui, base, body, content);
    let saved = ui.clip;
    ui.clip = saved.intersect(&body);
    let mut y = body.y + 10.0 - app.settings.conv_scroll.offset;
    ui.label(x, y, "这个对话的指令", &BODY_SMALL_STRONG, t.text_primary);
    y += 22.0;
    if app.settings.conv_instructions.as_ref().is_none_or(|(c, _)| c != id) {
        let mut e = TextEdit::new();
        e.set_text(meta.settings.instructions.clone());
        app.settings.conv_instructions = Some((id.to_owned(), e));
    }
    let (_, edit) = app.settings.conv_instructions.as_mut().unwrap();
    let mut new_instructions = None;
    if edit.show(ui, base + 1, rect(x, y, w, 110.0), "加到系统提示词的末尾", &BODY_SMALL, false).changed {
        new_instructions = Some(edit.text.clone());
    }
    y += 124.0;
    ui.label(x, y, "工具", &BODY_SMALL_STRONG, t.text_primary);
    ui.label(x + 34.0, y + 2.0, "关掉的工具不会告诉模型，也不能被调用", &FINE, t.text_tertiary);
    y += 26.0;
    let mut toggled = None;
    for (i, name) in tools.iter().enumerate() {
        let mut on = !meta.settings.disabled_tools.contains(name);
        ui.label(x, y + 3.0, name, &MONO_BODY, t.text_secondary);
        if ui.toggle(base + 10 + i as u64, rect(x + w - 44.0, y, 44.0, 22.0), &mut on).clicked {
            toggled = Some((name.clone(), on));
        }
        y += 30.0;
    }
    ui.clip = saved;
    if let Some(m) = app.meta_mut(id) {
        if let Some(text) = new_instructions {
            m.settings.instructions = text;
        }
        if let Some((name, on)) = toggled {
            m.settings.disabled_tools.retain(|d| *d != name);
            if !on {
                m.settings.disabled_tools.push(name);
            }
        }
    }
    app.mark_dirty();
}

use fluent::ui::Severity;
