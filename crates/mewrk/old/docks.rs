//! Cards above the composer that wait for the user: an approval, or a question.

use fluent::theme::*;
use fluent::ui::{rect, Icon, Rect, Ui};
use fluent::TextEdit;
use mewrk_core::*;
use serde_json::{json, Value};

const DOCK_W: f32 = crate::composer::COMPOSER_MAX_W;

fn dock_rect(area: Rect, y: f32, h: f32) -> Rect {
    let w = (area.w - 48.0).clamp(120.0, DOCK_W);
    rect(area.x + (area.w - w) * 0.5, y, w, h)
}

pub const APPROVAL_H: f32 = 92.0;

pub fn tool_verb(name: &str) -> String {
    match name {
        "write" => "写入文件".to_owned(),
        "edit" => "编辑文件".to_owned(),
        "zsh" | "bash" | "sh" => "运行命令".to_owned(),
        "web_fetch" => "抓取网页".to_owned(),
        "agent_spawn" => "启动子代理".to_owned(),
        "exit_plan_mode" => "批准这个计划".to_owned(),
        n => format!("使用 {n}"),
    }
}

/// Draws an approval card. Returns the decision if one was made this frame.
pub fn approval(ui: &mut Ui<'_>, id: u64, area: Rect, y: f32, req: &ApprovalRequest) -> Option<ApprovalDecision> {
    let t = ui.theme;
    let r = dock_rect(area, y, APPROVAL_H);
    ui.shadow(r, OVERLAY_RADIUS, 8.0, 2.0, t.shadow.fade(0.35));
    ui.acrylic(r, OVERLAY_RADIUS, t.flyout, t.flyout_stroke);
    ui.icon(Icon::Shield, r.x + 20.0, r.y + 22.0, 14.0, t.caution);
    ui.label(r.x + 36.0, r.y + 12.0, &format!("要{}吗？", tool_verb(&req.tool)), &BODY_SMALL_STRONG, t.text_primary);
    ui.label_ellipsis(r.x + 36.0, r.y + 33.0, r.w - 52.0, &req.summary, &MONO_SMALL, t.text_secondary);
    let by = r.bottom() - 40.0;
    let mut x = r.right() - 12.0;
    let mut out = None;
    let w = 72.0;
    x -= w;
    if ui.accent_button(id + 1, rect(x, by, w, 30.0), "允许", true).clicked {
        out = Some(ApprovalDecision::Allow);
    }
    let w2 = 112.0;
    x -= w2 + 8.0;
    if ui.button(id + 2, rect(x, by, w2, 30.0), "本对话都允许").clicked {
        out = Some(ApprovalDecision::AllowAlways);
    }
    let w3 = 72.0;
    x -= w3 + 8.0;
    if ui.button(id + 3, rect(x, by, w3, 30.0), "拒绝").clicked {
        out = Some(ApprovalDecision::Deny);
    }
    out
}

/// The state of an open question card.
#[derive(Default)]
pub struct QuestionState {
    pub request: u64,
    pub page: usize,
    /// For each question, which options are chosen and any free text.
    pub chosen: Vec<Vec<usize>>,
    pub other: Vec<TextEdit>,
}

impl QuestionState {
    fn sync(&mut self, req: &QuestionRequest, n: usize) {
        if self.request != req.request {
            *self = Self { request: req.request, page: 0, chosen: vec![Vec::new(); n], other: (0..n).map(|_| TextEdit::new()).collect() };
        }
    }
}

pub fn question_height(req: &QuestionRequest, state: &QuestionState) -> f32 {
    let qs = req.questions.as_array().map_or(&[][..], Vec::as_slice);
    let q = qs.get(state.page);
    let options = q.and_then(|q| q.get("options")).and_then(Value::as_array).map_or(0, Vec::len);
    46.0 + options as f32 * 44.0 + 44.0 + 48.0
}

/// Draws a question card. Returns the answers once the last page is submitted.
pub fn question(ui: &mut Ui<'_>, id: u64, area: Rect, y: f32, req: &QuestionRequest, state: &mut QuestionState) -> Option<Value> {
    let t = ui.theme;
    let qs: Vec<Value> = req.questions.as_array().cloned().unwrap_or_default();
    state.sync(req, qs.len());
    let Some(q) = qs.get(state.page) else {
        return Some(json!([]));
    };
    let h = question_height(req, state);
    let r = dock_rect(area, y, h);
    ui.shadow(r, OVERLAY_RADIUS, 8.0, 2.0, t.shadow.fade(0.35));
    ui.acrylic(r, OVERLAY_RADIUS, t.flyout, t.flyout_stroke);
    let header = q.get("header").and_then(Value::as_str).unwrap_or("");
    let text = q.get("question").and_then(Value::as_str).unwrap_or("");
    let multi = q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false);
    let hw = ui.measure(header, &FINE_STRONG) + 14.0;
    if !header.is_empty() {
        let chip = rect(r.x + 14.0, r.y + 14.0, hw, 20.0);
        ui.fill(chip, CONTROL_RADIUS, t.accent.with_alpha(0.15));
        ui.label_vcentered(chip.x + 7.0, chip, header, &FINE_STRONG, t.accent_text);
    }
    ui.label_ellipsis(r.x + 14.0 + hw + 8.0, r.y + 15.0, r.w - hw - 40.0, text, &BODY_SMALL_STRONG, t.text_primary);
    let mut oy = r.y + 44.0;
    let options: Vec<Value> = q.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
    for (n, opt) in options.iter().enumerate() {
        let row = rect(r.x + 8.0, oy, r.w - 16.0, 42.0);
        let resp = ui.interact(id + 100 + n as u64, row);
        let on = state.chosen[state.page].contains(&n);
        let fill = if on { t.accent.with_alpha(0.14) } else { t.subtle_hover.fade(resp.hover_t) };
        if fill.a > 0 {
            ui.fill(row, CONTROL_RADIUS, fill);
        }
        let (cx, cy) = (row.x + 15.0, row.y + 21.0);
        ui.arc(cx, cy, 7.0, 1.25, 0.0, std::f32::consts::TAU, if on { t.accent } else { t.text_tertiary });
        if on {
            ui.fill(rect(cx - 3.5, cy - 3.5, 7.0, 7.0), 3.5, t.accent);
        }
        let label = opt.get("label").and_then(Value::as_str).unwrap_or("");
        let description = opt.get("description").and_then(Value::as_str).unwrap_or("");
        ui.label_ellipsis(row.x + 30.0, row.y + 4.0, row.w - 40.0, label, &BODY_SMALL, t.text_primary);
        ui.label_ellipsis(row.x + 30.0, row.y + 23.0, row.w - 40.0, description, &FINE, t.text_tertiary);
        if resp.clicked {
            let chosen = &mut state.chosen[state.page];
            if multi {
                if on {
                    chosen.retain(|c| *c != n);
                } else {
                    chosen.push(n);
                }
            } else {
                chosen.clear();
                chosen.push(n);
            }
        }
        oy += 44.0;
    }
    state.other[state.page].show(ui, id + 90, rect(r.x + 14.0, oy + 4.0, r.w - 28.0, 32.0), "其他回答…", &BODY_SMALL, false);
    let by = r.bottom() - 42.0;
    let last = state.page + 1 >= qs.len();
    let ready = !state.chosen[state.page].is_empty() || !state.other[state.page].text.trim().is_empty();
    if ui.accent_button(id + 2, rect(r.right() - 96.0, by, 84.0, 30.0), if last { "提交" } else { "下一个" }, ready).clicked && ready {
        if last {
            let mut answers = Vec::new();
            for (i, q) in qs.iter().enumerate() {
                let opts = q.get("options").and_then(Value::as_array);
                let mut picked: Vec<String> = state.chosen[i].iter().filter_map(|c| opts.and_then(|o| o.get(*c)).and_then(|o| o.get("label")).and_then(Value::as_str).map(str::to_owned)).collect();
                let other = state.other[i].text.trim();
                if !other.is_empty() {
                    picked.push(other.to_owned());
                }
                answers.push(json!({"question": q.get("question"), "answers": picked}));
            }
            return Some(Value::Array(answers));
        }
        state.page += 1;
    }
    None
}
