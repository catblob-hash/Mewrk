//! The context meter at the composer's lower right: upstream's `ContextUsageMeter.tsx`. The ring opens a breakdown, and the
//! breakdown's foot opens the auto-compact submenu.
//!
//! The ring keeps its fixed footprint without showing unstable numeric text; its tooltip says the current value.

use crate::rolling::rolling_number;
use mewrk_client::auto_compact::{auto_compact_threshold_tokens, clamp_auto_compact_percent, clamp_retained_tokens, AUTO_COMPACT_MAX_PERCENT, AUTO_COMPACT_MIN_PERCENT, NATIVE_RETAINED_MAX_TOKENS};
use mewrk_client::model::{AutoCompactSettings, CompactionMethod, ContextItem, NativeCompaction};
use mewrk_client::prelude::*;
use mewrk_client::runs::{compute_context_breakdown, format_compact_token_count, ContextBreakdown, ContextSegmentId};
use mewrk_ui_common::popover_menu::probe::say;
use mewrk_ui_common::popover_menu::{menu_flyout, solid_panel, use_popover_anchor, AnchorOptions, Hang, PopoverAnchor};

const PANEL_WIDTH: f32 = 296.0;
/// Meter color thresholds: warn at 70% and alert at 90%.
const WARNING_RATIO: f64 = 0.7;
const DANGER_RATIO: f64 = 0.9;

/// The key `use_popover_anchor` knows the panel by, and its trigger and rows by what follows.
pub const METER_KEY: &str = "context-usage-meter";

pub fn meter_trigger_key() -> u64 {
    mewrk_ui_common::popover_menu::trigger_key(METER_KEY)
}

pub fn compact_row_key() -> u64 {
    hash_key("context-usage-compact-row")
}

pub fn compact_switch_key() -> u64 {
    hash_key("context-usage-compact-switch")
}

pub fn method_tab_key(native: bool) -> u64 {
    hash_key(("context-usage-method", native))
}

pub fn slider_key(native: bool) -> u64 {
    hash_key(("context-usage-slider", native))
}

pub fn percent_field_key(native: bool) -> u64 {
    hash_key(("context-usage-percent", native))
}

pub fn retained_field_key() -> u64 {
    hash_key("context-usage-retained")
}

pub fn compact_now_key() -> u64 {
    hash_key("context-usage-compact-now")
}

/// How many of each the conversation has switched on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextUsageCounts {
    pub tools: usize,
    pub mcp_servers: usize,
    pub skills: usize,
    pub agent_roles: usize,
}

/// What the meter reads. `contexts` is only looked at while the panel is open, so a caller hands it over only then
/// ([`context_usage_is_open`]): the ring itself needs nothing but the totals.
#[derive(Clone, Default)]
pub struct ContextUsageProps {
    pub contexts: Vec<ContextItem>,
    pub tokens: u64,
    /// The meter includes locally estimated components, indicated by `~` in its heading.
    pub estimated: bool,
    /// The current model cannot project context usage.
    pub unprojectable: bool,
    pub context_window: Option<u64>,
    pub counts: ContextUsageCounts,
    /// The global auto-compact setting. Without it (and its handler) the panel has no auto-compact row.
    pub auto_compact: Option<AutoCompactSettings>,
    pub on_auto_compact_change: Option<Rc<dyn Fn(&mut App, AutoCompactSettings)>>,
    /// The selected model cannot take a tool mid-conversation, so the handoff tools could never join a run and it never hands
    /// off. The setting is global and stays as it is; the submenu says it does not apply here.
    pub auto_compact_unavailable: bool,
    /// The selected model compacts natively, so the submenu offers native compaction beside the handoff.
    pub native_compaction_available: bool,
    /// The method this conversation auto-compacts by on the selected model, `Some(None)` where it can do neither. Without it the
    /// conversation hands off where it can.
    pub compaction_method: Option<Option<CompactionMethod>>,
    pub on_compaction_method_change: Option<Rc<dyn Fn(&mut App, CompactionMethod)>>,
    /// The native compaction the request on this model starts at: `contexts` is the timeline from its card on, and the breakdown
    /// counts its item as compacted history.
    pub compaction: Option<NativeCompaction>,
    /// Compacts the conversation natively at once, into a new conversation. Without it the native page has no such button.
    pub on_compact_now: Option<Rc<dyn Fn(&mut App)>>,
    /// Why compacting now is out of reach at the moment, or nothing when it is not.
    pub compact_now_blocked: Option<String>,
}

/// Whether the panel is up, so the caller knows to hand over the timeline.
pub fn context_usage_is_open() -> bool {
    meter_anchor().is_open()
}

fn meter_anchor() -> PopoverAnchor {
    use_popover_anchor(METER_KEY, AnchorOptions { align_end: true, ..AnchorOptions::default() })
}

pub fn segment_label(id: ContextSegmentId) -> &'static str {
    match id {
        ContextSegmentId::SystemPrompt => t("系统提示词", "System prompt"),
        ContextSegmentId::Compacted => t("压缩的历史", "Compacted history"),
        ContextSegmentId::User => t("用户消息", "User messages"),
        ContextSegmentId::Assistant => t("助手回复", "Assistant replies"),
        ContextSegmentId::Reasoning => t("思考", "Reasoning"),
        ContextSegmentId::Tool => t("工具调用", "Tool calls"),
        ContextSegmentId::Other => t("其他（工具定义等）", "Other (tool schemas, etc.)"),
    }
}

fn segment_color(id: ContextSegmentId) -> Color {
    let t_ = theme();
    match id {
        ContextSegmentId::SystemPrompt | ContextSegmentId::Compacted => t_.text_secondary,
        ContextSegmentId::User => Color::rgb(0x7C5CD6),
        ContextSegmentId::Assistant => t_.accent,
        ContextSegmentId::Reasoning => t_.caution,
        ContextSegmentId::Tool => t_.success,
        ContextSegmentId::Other => t_.text_tertiary,
    }
}

/// What a segment is filled with, in the bar and in its swatch. The compaction item is hatched, since it holds history the request no
/// longer spells out: `repeating-linear-gradient(135deg, muted 0 2px, faint 2px 4px)`.
fn segment_fill(id: ContextSegmentId) -> E {
    if id != ContextSegmentId::Compacted {
        return div().bg(segment_color(id));
    }
    custom(|_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let t_ = ui.theme;
        ui.fill(r, 0.0, t_.text_tertiary);
        ui.with_clip(r, |ui| {
            let mut x = r.x - r.h;
            while x < r.right() {
                ui.line(x, r.bottom(), x + r.h, r.y, 2.0, t_.text_secondary);
                x += 4.0 * std::f32::consts::SQRT_2;
            }
        });
    })
    .pass_through()
}

/// The auto-compact threshold across the bar: dashed for native compaction, so the mark says which method it belongs to.
fn threshold_mark(percent: u32, native: bool) -> E {
    let color = theme().text_primary.fade(if native { 0.7 } else { 0.5 });
    let mark: E = if native {
        custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
            let mut y = r.y;
            while y < r.bottom() {
                ui.fill(rect(r.x, y, r.w, 2.0f32.min(r.bottom() - y)), 0.0, color);
                y += 4.0;
            }
        })
    } else {
        div().bg(color)
    };
    mark.key("context-usage-threshold").absolute().top(0.0).bottom(0.0).left_pct(percent as f32 / 100.0).w(2.0).ml(-1.0).pass_through()
}

/// Distinguishes zero from a nonzero share too small to display at one decimal.
pub fn format_share(share: f64) -> String {
    if share > 0.0 && share < 0.001 { "<0.1%".to_owned() } else { format!("{:.1}%", share * 100.0) }
}

/// The heading's words.
pub fn usage_headline(breakdown: &ContextBreakdown, estimated: bool, unprojectable: bool) -> String {
    if unprojectable {
        return t("不可投影", "Not projectable").to_owned();
    }
    let prefix = if estimated { "~" } else { "" };
    let used = format!("{prefix}{}", format_compact_token_count(breakdown.used));
    match breakdown.window {
        None => used,
        Some(window) => tf("{used} / {window}（{percent}%）", "{used} / {window} ({percent}%)", &[("used", &used), ("window", &format_compact_token_count(window)), ("percent", &(breakdown.ratio.unwrap_or(0.0) * 100.0).round())]),
    }
}

/// The method in effect: the caller's word, or the handoff where it can.
pub fn method_in_use(props: &ContextUsageProps) -> Option<CompactionMethod> {
    match props.compaction_method {
        Some(method) => method,
        None if props.auto_compact_unavailable && !props.native_compaction_available => None,
        None if props.auto_compact_unavailable => Some(CompactionMethod::Native),
        None => Some(CompactionMethod::Handoff),
    }
}

/// What the auto-compact row says it is set to.
pub fn compact_hint(props: &ContextUsageProps) -> String {
    let Some(settings) = &props.auto_compact else { return String::new() };
    let unavailable = props.auto_compact_unavailable && !props.native_compaction_available;
    let method = method_in_use(props);
    if unavailable || method.is_none() {
        return t("当前模型不支持", "Not for this model").to_owned();
    }
    if !settings.enabled {
        return t("已关闭", "Off").to_owned();
    }
    if method == Some(CompactionMethod::Native) {
        return tf("原生 {percent}%", "Native {percent}%", &[("percent", &clamp_auto_compact_percent(settings.native.threshold_percent))]);
    }
    let percent = format!("{}%", clamp_auto_compact_percent(settings.threshold_percent));
    // Where there is nothing to choose between, the percent says it all.
    if props.native_compaction_available { tf("交接 {percent}", "Handoff {percent}", &[("percent", &percent)]) } else { percent }
}

/// One threshold's working state: a slider with a small number field above it that says the same value exactly. Neither control
/// saves while it is being worked: the slider commits when it is released, the field on Enter or blur, so dragging across the
/// range does not write the document once per step.
#[derive(Clone, Debug, Default)]
struct PercentState {
    draft: u32,
    field: Option<String>,
    committed: u32,
}

impl PercentState {
    fn commit(&mut self, next: f64) -> Option<u32> {
        let clamped = clamp_auto_compact_percent(next);
        self.draft = clamped;
        self.field = None;
        if clamped == self.committed {
            return None;
        }
        self.committed = clamped;
        Some(clamped)
    }

    /// A partial entry ("8" on the way to "85") only moves the slider when it already names a value in range.
    fn typed(&mut self, text: String) {
        if let Ok(parsed) = text.trim().parse::<u32>()
            && (AUTO_COMPACT_MIN_PERCENT..=AUTO_COMPACT_MAX_PERCENT).contains(&parsed)
        {
            self.draft = parsed;
        }
        self.field = Some(text);
    }

    fn commit_field(&mut self) -> Option<u32> {
        let text = self.field.clone()?;
        match text.trim().parse::<f64>() {
            Ok(parsed) if parsed.is_finite() => self.commit(parsed),
            _ => {
                self.field = None;
                self.draft = self.committed;
                None
            }
        }
    }
}

/// What `<input type="number">` lets be typed at all.
pub fn numeric_text(text: &str) -> bool {
    text.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
}

/// What Up and Down make of `<input type="number" min max step>`: the next value on its grid that way, inside its range. Nothing that
/// parses counts as nothing typed.
pub fn number_step(text: &str, up: bool, (min, max, step): (u32, u32, u32)) -> String {
    let current = text.trim().parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0);
    let steps = (current - f64::from(min)) / f64::from(step);
    let next = f64::from(min) + (if up { steps.floor() + 1.0 } else { steps.ceil() - 1.0 }) * f64::from(step);
    let last = min + (max - min) / step * step;
    next.clamp(f64::from(min), f64::from(last)).to_string()
}

/// Where the keyboard takes `<input type="range">` from `value`. A page goes a tenth of the range, on to the nearest step.
pub fn range_key(key: Key, value: u32) -> Option<u32> {
    let big = (AUTO_COMPACT_MAX_PERCENT - AUTO_COMPACT_MIN_PERCENT) as f64 / 10.0;
    let next = match key {
        Key::Left | Key::Down => value as f64 - 1.0,
        Key::Right | Key::Up => value as f64 + 1.0,
        Key::PageUp => (value as f64 + big).round(),
        Key::PageDown => (value as f64 - big).round(),
        Key::Home => AUTO_COMPACT_MIN_PERCENT as f64,
        Key::End => AUTO_COMPACT_MAX_PERCENT as f64,
        _ => return None,
    };
    Some(clamp_auto_compact_percent(next))
}

/// `.context-usage-compact__field`'s `input`: `width` wide and 24 high, with its number at the far end.
fn number_box(key: u64, width: f32, disabled: bool, number: E) -> E {
    let t_ = theme();
    let focused = !disabled && focused_id() == hash_key(("field", key));
    row().w(width).h(24.0).shrink0().px(6.0).items_center().radius(6.0).border(if focused { t_.accent } else { t_.control_strong_stroke }).child(number.flex1())
}

fn percent_control(native: bool, label: &str, field_label: &str, value: u32, disabled: bool, on_commit: Rc<dyn Fn(&mut App, u32)>, hint: impl Fn(u32) -> E) -> E {
    let t_ = theme();
    let state = use_state(("context-usage-percent-state", native), || PercentState { draft: value, field: None, committed: value });
    {
        let mut s = state.borrow_mut();
        if s.committed != value {
            s.committed = value;
            s.draft = value;
        }
    }
    let (draft, field_text) = {
        let s = state.borrow();
        (s.draft, s.field.clone().unwrap_or_else(|| s.draft.to_string()))
    };
    say(label);
    say(&draft.to_string());
    say(&format!("{AUTO_COMPACT_MIN_PERCENT}%"));
    say(&format!("{AUTO_COMPACT_MAX_PERCENT}%"));

    let (typing, entering, blurring, stepping) = (state.clone(), state.clone(), state.clone(), state.clone());
    let (enter_commit, blur_commit) = (on_commit.clone(), on_commit.clone());
    let number = field::<App>(percent_field_key(native), field_text.clone())
        .controlled()
        .bare()
        .ty(CAPTION)
        .align_end()
        .on_change(move |_: &mut App, text| {
            if numeric_text(&text) {
                typing.update(|s| s.typed(text));
            }
        })
        .on_key(move |_: &mut App, press| {
            if matches!(press.key, Key::Up | Key::Down) {
                let next = number_step(&field_text, press.key == Key::Up, (AUTO_COMPACT_MIN_PERCENT, AUTO_COMPACT_MAX_PERCENT, 1));
                stepping.update(|s| s.typed(next));
            }
            false
        })
        .on_submit(move |app: &mut App, _| {
            if let Some(next) = entering.update(PercentState::commit_field) {
                enter_commit(app, next);
            }
        })
        .on_blur(move |app: &mut App, _| {
            if let Some(next) = blurring.update(PercentState::commit_field) {
                blur_commit(app, next);
            }
        })
        .build()
        .h(CAPTION.line)
        .aria_label(field_label.to_owned());
    let number = if disabled { number.disabled(true) } else { number };
    let field_box: E = row().items_center().gap(3.0).child(number_box(percent_field_key(native), 44.0, disabled, number)).child(text("%").ty(FINE).color(t_.text_tertiary).aria_hidden(true));
    let head: E = row().items_center().justify_between().gap(8.0).child(text(label.to_owned()).ty(FINE).color(t_.text_secondary)).child(field_box);

    // `<input type="range">`.
    let share = (draft - AUTO_COMPACT_MIN_PERCENT) as f32 / (AUTO_COMPACT_MAX_PERCENT - AUTO_COMPACT_MIN_PERCENT) as f32;
    let track: E = custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let t_ = ui.theme;
        let y = r.y + r.h * 0.5;
        ui.fill(rect(r.x, y - 2.0, r.w, 4.0), 2.0, t_.control_strong_stroke);
        ui.fill(rect(r.x, y - 2.0, r.w * share, 4.0), 2.0, if disabled { t_.text_disabled } else { t_.accent });
        ui.fill(rect(r.x + (r.w - 12.0) * share, y - 6.0, 12.0, 12.0), 6.0, if disabled { t_.text_disabled } else { t_.accent });
    })
    .w_full()
    .h(16.0)
    .pass_through();
    let slider: E = div().key(slider_key(native)).role(Role::Slider).aria_label(label.to_owned()).aria_value(draft as f64, AUTO_COMPACT_MIN_PERCENT as f64, AUTO_COMPACT_MAX_PERCENT as f64).aria_disabled(disabled).w_full().h(16.0).mt(2.0).child(track);
    let slider = if disabled {
        slider
    } else {
        let (dragging, pressed, key_commit) = (state.clone(), state.clone(), on_commit.clone());
        slider.cursor(CursorKind::Hand).focusable().on_key(move |app: &mut App, press| {
            let Some(next) = range_key(press.key, pressed.with(|s| s.draft)) else { return false };
            if let Some(next) = pressed.update(|s| s.commit(next as f64)) {
                key_commit(app, next);
            }
            true
        }).on_pointer(move |app: &mut App, event: PointerEvent| {
            let share = ((event.x - event.rect.x - 6.0) / (event.rect.w - 12.0).max(1.0)).clamp(0.0, 1.0);
            let next = AUTO_COMPACT_MIN_PERCENT as f32 + share * (AUTO_COMPACT_MAX_PERCENT - AUTO_COMPACT_MIN_PERCENT) as f32;
            match event.phase {
                Phase::Down | Phase::Move => dragging.update(|s| {
                    s.draft = clamp_auto_compact_percent(next.round());
                    s.field = None;
                }),
                Phase::Up => {
                    if let Some(next) = dragging.update(|s| s.commit(next.round() as f64)) {
                        on_commit(app, next);
                    }
                }
            }
        })
    };
    let scale: E = row().aria_hidden(true).justify_between().child(text(format!("{AUTO_COMPACT_MIN_PERCENT}%")).ty(FINE).color(t_.text_tertiary)).child(text(format!("{AUTO_COMPACT_MAX_PERCENT}%")).ty(FINE).color(t_.text_tertiary));
    col().gap(5.0).pt(9.0).border_top(t_.divider).when(disabled, |e| e.opacity(0.5)).child(head).child(slider).child(scale).child(hint(draft))
}

/// How much of the latest user messages a native compaction keeps, in tokens, as a sentence with the number in it. Saved on Enter
/// or blur, as the threshold field is.
fn retained_control(value: u32, on_commit: Rc<dyn Fn(&mut App, u32)>) -> E {
    let t_ = theme();
    let draft = use_state("context-usage-retained-draft", || None::<String>);
    let commit = {
        let draft = draft.clone();
        move |app: &mut App| {
            let Some(text) = draft.borrow().clone() else { return };
            draft.set(None);
            let Ok(parsed) = text.trim().parse::<f64>() else { return };
            if !parsed.is_finite() {
                return;
            }
            let tokens = clamp_retained_tokens(parsed);
            if tokens != value {
                on_commit(app, tokens);
            }
        }
    };
    let (typing, stepping, on_enter, on_blur) = (draft.clone(), draft.clone(), commit.clone(), commit);
    let shown = draft.get().unwrap_or_else(|| value.to_string());
    say(&shown);
    let number = field::<App>(retained_field_key(), shown.clone())
        .controlled()
        .bare()
        .ty(CAPTION)
        .align_end()
        .on_change(move |_: &mut App, text| {
            if numeric_text(&text) {
                typing.set(Some(text));
            }
        })
        .on_key(move |_: &mut App, press| {
            if matches!(press.key, Key::Up | Key::Down) {
                stepping.set(Some(number_step(&shown, press.key == Key::Up, (0, NATIVE_RETAINED_MAX_TOKENS, 1000))));
            }
            false
        })
        .on_submit(move |app: &mut App, _| on_enter(app))
        .on_blur(move |app: &mut App, _| on_blur(app))
        .build()
        .h(CAPTION.line)
        .aria_label(t("保留最近的用户消息（token）", "Latest user messages kept (tokens)"));
    row().wrap().items_center().gap(4.0).child(text(t("保留最近的", "Keep the latest")).ty(FINE).color(t_.text_secondary)).child(number_box(retained_field_key(), 60.0, false, number)).child(text(t("token 用户消息", "tokens of user messages")).ty(FINE).color(t_.text_secondary))
}

fn threshold_hint(context_window: Option<u64>, percent: u32, native: bool, section: &'static str) -> E {
    let t_ = theme();
    let Some(window) = context_window.filter(|window| *window > 0) else {
        let none = t("当前模型没有设置上下文窗口，无法自动压缩", "The current model has no context window set, so it cannot auto-compact");
        say(none);
        return text(none).ty(FINE).color(t_.text_tertiary).mt(1.0);
    };
    let tokens = format_compact_token_count(auto_compact_threshold_tokens(window, percent));
    let line = if native { tf("上下文达到 {tokens} tokens 时压缩", "Compacts at {tokens} tokens", &[("tokens", &tokens)]) } else { tf("上下文达到 {tokens} tokens 时交接", "Hands off at {tokens} tokens", &[("tokens", &tokens)]) };
    say(&line);
    row().wrap().items_center().gap(4.0).mt(1.0).child(text(line).ty(FINE).color(t_.text_tertiary).min_w(0.0)).child(docs_link("context", section))
}

/// `DocsLink inline`.
fn docs_link(page: &'static str, section: &'static str) -> E {
    let t_ = theme();
    let locale = if mewrk_client::i18n::is_english() { "en" } else { "zh-CN" };
    let href = format!("https://mewrk.dev/{locale}/{page}.html#{section}");
    say(&format!("docs:{page}#{section}"));
    row().key(("composer-docs", page, section)).role(Role::Link).items_center().gap(2.0).cursor(CursorKind::Hand).on_click(move |app: &mut App| app.invoke_logged("open_external_url", json!({ "url": href }))).child(text(t("详情", "Learn more")).ty(FINE).color(t_.accent_text).nowrap()).child(icon("ExternalLink", 9.0).color(t_.accent_text))
}

/// The auto-compact submenu. The switch heads it; under it, at the left, the two methods a conversation chooses between as two
/// buttons. The one pressed is this conversation's choice, and the page under them is that method's: its threshold, and for native
/// compaction how much of the latest user messages it keeps and a button that compacts at once. The choice is the conversation's;
/// the switch and the numbers are global.
fn auto_compact_flyout(anchor: &PopoverAnchor, props: &ContextUsageProps, settings: &AutoCompactSettings, method: Option<CompactionMethod>, context_window: Option<u64>, on_change: Rc<dyn Fn(&mut App, AutoCompactSettings)>) -> E {
    let t_ = theme();
    let enable_label = t("启用自动压缩", "Enable auto-compact");
    say(enable_label);
    let toggled = {
        let (settings, on_change) = (settings.clone(), on_change.clone());
        move |app: &mut App| {
            let mut next = settings.clone();
            next.enabled = !next.enabled;
            on_change(app, next);
        }
    };
    let switch_row: E = row().items_center().justify_between().gap(10.0).child(text(enable_label).ty(CAPTION).color(t_.text_primary)).child(div().key(compact_switch_key()).child(switch(settings.enabled, toggled).aria_label(enable_label)));
    let method_tab = |value: CompactionMethod, label: &'static str, available: bool, unavailable: &'static str| -> E {
        say(label);
        let active = method == Some(value);
        let tab: E = row().key(method_tab_key(value == CompactionMethod::Native)).role(Role::Tab).aria_selected(active).min_h(24.0).px(9.0).items_center().radius(6.0).when(active, |e| e.bg(t_.subtle_hover)).child(text(label).ty(CAPTION_STRONG).color(if !available { t_.text_disabled } else if active { t_.text_primary } else { t_.text_secondary }));
        match (&props.on_compaction_method_change, available) {
            (Some(change), true) => {
                let change = change.clone();
                tab.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| {
                    if !active {
                        change(app, value);
                    }
                })
            }
            _ => tab.disabled(true).on_click(|_: &mut App| {}).when(!available, |e| e.disabled_reason(unavailable)),
        }
    };
    let tabs: E = row().role(Role::TabList).aria_label(t("压缩方式", "Compaction method")).self_start().gap(2.0).child(method_tab(CompactionMethod::Handoff, t("交接", "Handoff"), !props.auto_compact_unavailable, t("当前模型不支持中途追加工具，交接用的工具无法在对话中途加入。", "The selected model cannot take a tool mid-conversation, so the handoff tools could never join."))).child(method_tab(CompactionMethod::Native, t("原生压缩", "Native compaction"), props.native_compaction_available, t("当前模型不支持原生压缩。", "The selected model does not compact natively.")));

    let mut panel: E = solid_panel().key("context-usage-flyout").role(Role::Group).aria_label(t("自动压缩", "Auto-compact")).w(236.0).px(11.0).pt(10.0).pb(9.0).gap(9.0).child(switch_row).child(tabs);
    if method == Some(CompactionMethod::Handoff) {
        let commit = {
            let (settings, on_change) = (settings.clone(), on_change.clone());
            Rc::new(move |app: &mut App, percent: u32| {
                let mut next = settings.clone();
                next.threshold_percent = percent;
                on_change(app, next);
            })
        };
        panel = panel.child(col().role(Role::TabPanel).aria_label(t("交接", "Handoff")).gap(9.0).child(percent_control(false, t("交接阈值", "Handoff threshold"), t("交接阈值（百分比）", "Handoff threshold (percent)"), settings.threshold_percent, !settings.enabled, commit, move |percent| threshold_hint(context_window, percent, false, "auto-compact"))));
    }
    if method == Some(CompactionMethod::Native) {
        let commit = {
            let (settings, on_change) = (settings.clone(), on_change.clone());
            Rc::new(move |app: &mut App, percent: u32| {
                let mut next = settings.clone();
                next.native.threshold_percent = percent;
                on_change(app, next);
            })
        };
        let retained = {
            let (settings, on_change) = (settings.clone(), on_change.clone());
            Rc::new(move |app: &mut App, tokens: u32| {
                let mut next = settings.clone();
                next.native.retained_tokens = tokens;
                on_change(app, next);
            })
        };
        let mut page: E = col().role(Role::TabPanel).aria_label(t("原生压缩", "Native compaction")).gap(9.0).child(percent_control(true, t("原生压缩阈值", "Native compaction threshold"), t("原生压缩阈值（百分比）", "Native compaction threshold (percent)"), settings.native.threshold_percent, !settings.enabled, commit, move |percent| threshold_hint(context_window, percent, true, "native-compaction"))).child(retained_control(settings.native.retained_tokens, retained));
        if let Some(compact_now) = props.on_compact_now.clone() {
            let label = t("立即压缩", "Compact now");
            say(label);
            let button: E = button(label).key(compact_now_key()).h(26.0).px(10.0).self_start();
            let closing = anchor.clone();
            page = page.child(match &props.compact_now_blocked {
                Some(blocked) => {
                    say(blocked);
                    button.disabled_reason(blocked.clone()).disabled(true).opacity(0.5).on_click(|_: &mut App| {})
                }
                None => button.on_click(move |app: &mut App| {
                    closing.close();
                    compact_now(app);
                }),
            });
        }
        panel = panel.child(page);
    }
    let note = t("方式按对话记，其余设置全局生效。", "The method is per conversation; the rest is global.");
    say(note);
    let panel = panel.child(text(note).ty(FINE).color(t_.text_tertiary));
    // It hangs upward from its row, which sits at the foot of a panel that usually opens above the composer.
    menu_flyout("context-usage-compact", anchor, compact_row_key(), panel, (11.0, 10.0), Hang::Up)
}

/// The ring and, while open, its panel.
pub fn context_usage_meter(props: ContextUsageProps) -> E {
    let t_ = theme();
    let anchor = meter_anchor();
    let compact_open = use_state("context-usage-compact-open", || false);
    // A reopened panel starts with its submenu closed, as a menu does.
    if !anchor.is_open() && compact_open.get() {
        compact_open.set(false);
    }
    let window = if props.unprojectable { None } else { props.context_window };
    let breakdown = compute_context_breakdown(&props.contexts, props.compaction.as_ref(), props.tokens, window);
    let ratio = if props.unprojectable { None } else { breakdown.ratio };
    let tone = match ratio {
        Some(ratio) if ratio >= DANGER_RATIO => t_.critical,
        Some(ratio) if ratio >= WARNING_RATIO => t_.caution,
        _ => t_.text_secondary,
    };
    let headline = usage_headline(&breakdown, props.estimated, props.unprojectable);
    let trigger_label = if props.unprojectable { t("上下文用量：不可投影", "Context usage: not projectable").to_owned() } else { tf("上下文用量：{headline}", "Context usage: {headline}", &[("headline", &headline)]) };
    say(&trigger_label);
    let ring: E = custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let (cx, cy) = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        ui.arc(cx, cy, 6.0 + 1.25, 2.5, 0.0, std::f32::consts::TAU, ui.theme.control_strong_stroke);
        if let Some(ratio) = ratio.filter(|ratio| *ratio > 0.0) {
            ui.arc(cx, cy, 6.0 + 1.25, 2.5, 0.0, std::f32::consts::TAU * ratio as f32, tone);
        }
    })
    .size(16.0, 16.0)
    .pass_through();
    let toggler = anchor.clone();
    let trigger: E = row().key(anchor.trigger_key()).center().size(25.0, 25.0).radius(7.0).shrink0().focusable().aria_haspopup(HasPopup::Dialog).aria_expanded(anchor.is_open()).when(anchor.is_open(), |e| e.bg(t_.subtle_hover)).hover_bg(t_.subtle_hover).tooltip(trigger_label).on_click(move |_: &mut App| {
        toggler.toggle();
        // The dialog takes the focus, so its actions are in reach of the keyboard.
        if toggler.is_open() {
            focus_request(toggler.panel_key());
        }
    }).child(ring);
    let mut root: E = div().row().min_w(0.0).child(trigger);
    if !anchor.is_open() {
        return root;
    }

    // ---- the panel
    let title = t("上下文窗口", "Context window");
    say(title);
    say(&headline);
    let head: E = row().items_baseline().justify_between().gap(10.0).child(text(title).ty(FINE_STRONG).color(t_.text_tertiary)).child(rolling_number("context-usage-head", &headline, CAPTION_STRONG, t_.text_primary));

    let auto_compact = props.auto_compact.clone().filter(|_| props.on_auto_compact_change.is_some());
    let method = method_in_use(&props);
    let mut bar: E = row().h(6.0).gap(1.0).radius(3.0).clip().bg(t_.divider);
    for segment in &breakdown.segments {
        bar = bar.child(segment_fill(segment.id).basis(0.0).flex_grow(segment.tokens as f32).min_w(1.0));
    }
    if let Some(free) = breakdown.free.filter(|free| *free > 0) {
        bar = bar.child(div().basis(0.0).flex_grow(free as f32).min_w(1.0).bg(t_.divider));
    }
    let threshold = auto_compact.as_ref().filter(|settings| settings.enabled && breakdown.window.is_some()).and_then(|settings| {
        method.map(|method| {
            let native = method == CompactionMethod::Native;
            (clamp_auto_compact_percent(if native { settings.native.threshold_percent } else { settings.threshold_percent }), native)
        })
    });
    let mut bar_host: E = div().key("context-usage-bar").child(bar);
    if let Some((percent, native)) = threshold {
        say(&format!("threshold:{percent}"));
        if native {
            say("threshold:dashed");
        }
        bar_host = bar_host.child(threshold_mark(percent, native));
    }

    let share_row = |key: String, swatch: E, name: &str, tokens: String, share: String| -> E {
        say(name);
        say(&tokens);
        say(&share);
        row().min_h(21.0).items_center().gap(7.0).child(swatch.size(8.0, 8.0).radius(2.0).clip().shrink0()).child(text(name.to_owned()).ty(FINE).color(t_.text_primary).ellipsis().flex1()).child(rolling_number((key.as_str(), "tokens"), &tokens, FINE, t_.text_secondary)).child(row().w(44.0).shrink0().justify_end().child(rolling_number((key.as_str(), "share"), &share, FINE, t_.text_tertiary)))
    };
    let mut rows: E = col();
    for segment in &breakdown.segments {
        rows = rows.child(share_row(format!("{:?}", segment.id), segment_fill(segment.id), segment_label(segment.id), format_compact_token_count(segment.tokens), format_share(segment.share)));
    }
    if let Some(free) = breakdown.free {
        rows = rows.child(share_row("free".to_owned(), div().bg(t_.divider).border(t_.control_strong_stroke), t("剩余空间", "Free space"), format_compact_token_count(free), format_share(breakdown.free_share.unwrap_or(0.0))));
    }
    if breakdown.segments.is_empty() && breakdown.free.is_none() {
        let empty = t("还没有可拆解的上下文", "Nothing to break down yet");
        say(empty);
        rows = rows.child(row().min_h(21.0).items_center().child(text(empty).ty(FINE).color(t_.text_tertiary)));
    }

    let mut counts: E = row().wrap().gap_x(10.0).gap_y(4.0).pt(7.0).border_top(t_.divider);
    for (label, value) in [(t("工具", "Tools"), props.counts.tools), (t("MCP", "MCP"), props.counts.mcp_servers), (t("技能", "Skills"), props.counts.skills), (t("角色", "Roles"), props.counts.agent_roles)] {
        say(&format!("{label} {value}"));
        counts = counts.child(row().items_baseline().gap(4.0).child(text(label).ty(FINE).color(t_.text_tertiary)).child(text(value.to_string()).ty(FINE_STRONG).color(t_.text_secondary)));
    }

    // Escape hands the focus back to the ring, on its way to closing the panel.
    let trigger_key = anchor.trigger_key();
    let mut panel: E = solid_panel().role(Role::Dialog).aria_label(t("上下文窗口用量", "Context window usage")).w(PANEL_WIDTH).px(10.0).pt(9.0).pb(7.0).gap(8.0).on_key(move |_: &mut App, press| {
        if press.key == Key::Escape {
            focus_request(trigger_key);
        }
        false
    }).child(head).child(bar_host).child(rows).child(counts);
    if let (Some(settings), Some(on_change)) = (auto_compact, props.on_auto_compact_change.clone()) {
        // The row is out of reach only when neither method applies to the model.
        let unavailable = props.auto_compact_unavailable && !props.native_compaction_available;
        let hint = compact_hint(&props);
        let label = t("自动压缩", "Auto-compact");
        say(label);
        say(&hint);
        let expanded = compact_open.get() && !unavailable;
        let mut compact_row: E = row().key(compact_row_key()).aria_haspopup(HasPopup::Menu).aria_expanded(expanded).min_h(24.0).px(7.0).py(2.0).mx(-6.0).mb(-3.0).mt(-3.0).gap(7.0).items_center().radius(5.0).when(expanded, |e| e.bg(t_.subtle_hover)).child(text(label).ty(CAPTION).color(if unavailable { t_.text_disabled } else { t_.text_primary }).ellipsis().flex1()).child(text(hint).ty(FINE).color(t_.text_tertiary).nowrap().shrink0()).child(icon("ChevronRight", 13.0).color(t_.text_tertiary));
        if unavailable {
            compact_row = compact_row.disabled(true).on_click(|_: &mut App| {}).disabled_reason(t("当前模型不支持中途追加工具，交接用的工具无法在对话中途加入，所以不会自动压缩。", "The selected model cannot take a tool mid-conversation, so the handoff tools could never join and it does not auto-compact."));
        } else {
            let toggling = compact_open.clone();
            compact_row = compact_row.hover_bg(t_.subtle_hover).on_click(move |_: &mut App| toggling.update(|open| *open = !*open));
        }
        panel = panel.child(compact_row);
        if expanded {
            panel = panel.child(auto_compact_flyout(&anchor, &props, &settings, method, window, on_change));
        }
    }
    root = root.child(anchor.place(panel));
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breakdown(used: u64, window: Option<u64>) -> ContextBreakdown {
        compute_context_breakdown(&[], None, used, window)
    }

    #[test]
    fn marks_an_estimated_total_and_drops_the_percentage_when_no_window_is_declared() {
        let _language = crate::tests::language(true);
        assert_eq!(usage_headline(&breakdown(180_000, Some(1_000_000)), false, false), "180k / 1m (18%)");
        assert_eq!(usage_headline(&breakdown(180_000, Some(1_000_000)), true, false), "~180k / 1m (18%)");
        assert_eq!(usage_headline(&breakdown(180_000, None), true, false), "~180k");
        assert_eq!(usage_headline(&breakdown(180_000, Some(1_000_000)), false, true), "Not projectable");
    }

    #[test]
    fn says_a_share_is_too_small_to_print_instead_of_rounding_it_to_zero() {
        assert_eq!(format_share(0.0), "0.0%");
        assert_eq!(format_share(0.0005), "<0.1%");
        assert_eq!(format_share(0.18), "18.0%");
    }

    fn settings() -> AutoCompactSettings {
        mewrk_client::auto_compact::default_auto_compact_settings()
    }

    #[test]
    fn the_row_says_what_auto_compact_is_set_to() {
        let _language = crate::tests::language(true);
        let mut props = ContextUsageProps { auto_compact: Some(settings()), ..ContextUsageProps::default() };
        assert_eq!(compact_hint(&props), "80%");
        props.native_compaction_available = true;
        assert_eq!(compact_hint(&props), "Handoff 80%");
        props.compaction_method = Some(Some(CompactionMethod::Native));
        assert_eq!(compact_hint(&props), "Native 90%");
        props.auto_compact.as_mut().unwrap().enabled = false;
        assert_eq!(compact_hint(&props), "Off");
        let none = ContextUsageProps { auto_compact: Some(settings()), auto_compact_unavailable: true, ..ContextUsageProps::default() };
        assert_eq!(compact_hint(&none), "Not for this model");
        assert_eq!(method_in_use(&none), None);
        let native_only = ContextUsageProps { auto_compact: Some(settings()), auto_compact_unavailable: true, native_compaction_available: true, ..ContextUsageProps::default() };
        assert_eq!(method_in_use(&native_only), Some(CompactionMethod::Native));
    }

    #[test]
    fn the_arrow_keys_step_a_number_field_along_its_grid_and_inside_its_range() {
        let percent = (AUTO_COMPACT_MIN_PERCENT, AUTO_COMPACT_MAX_PERCENT, 1);
        assert_eq!(number_step("80", true, percent), "81");
        assert_eq!(number_step("80", false, percent), "79");
        assert_eq!(number_step("97", true, percent), "97");
        assert_eq!(number_step("", true, percent), "20", "nothing typed starts from the bottom of the range");
        let tokens = (0, NATIVE_RETAINED_MAX_TOKENS, 1000);
        assert_eq!(number_step("5000", true, tokens), "6000");
        assert_eq!(number_step("5500", true, tokens), "6000", "off the grid goes to the next on it");
        assert_eq!(number_step("5500", false, tokens), "5000");
        assert_eq!(number_step("128000", true, tokens), "128000");
        assert_eq!(number_step("0", false, tokens), "0");
    }

    #[test]
    fn a_range_input_moves_by_step_page_and_to_its_ends() {
        assert_eq!(range_key(Key::Right, 80), Some(81));
        assert_eq!(range_key(Key::Down, 80), Some(79));
        assert_eq!(range_key(Key::PageUp, 80), Some(88));
        assert_eq!(range_key(Key::PageDown, 80), Some(72));
        assert_eq!(range_key(Key::Home, 80), Some(20));
        assert_eq!(range_key(Key::End, 80), Some(97));
        assert_eq!(range_key(Key::Right, 97), Some(97));
        assert_eq!(range_key(Key::Enter, 80), None);
    }

    #[test]
    fn a_number_field_takes_what_a_number_input_would() {
        assert!(numeric_text("85"));
        assert!(numeric_text("1e3"));
        assert!(!numeric_text("8a"));
    }

    #[test]
    fn a_threshold_commits_clamped_and_only_when_it_changes() {
        let mut state = PercentState { draft: 80, field: None, committed: 80 };
        state.typed("8".to_owned());
        assert_eq!(state.draft, 80, "a partial entry moves nothing until it names a value in range");
        state.typed("85".to_owned());
        assert_eq!(state.draft, 85);
        assert_eq!(state.committed, 80, "nothing is saved until Enter or blur");
        assert_eq!(state.commit_field(), Some(85));
        state.typed("200".to_owned());
        assert_eq!(state.commit_field(), Some(97));
        assert_eq!(state.draft, 97);
        state.typed("97".to_owned());
        assert_eq!(state.commit_field(), None);
        state.typed("abc".to_owned());
        assert_eq!(state.commit_field(), None);
        assert_eq!(state.draft, 97, "what will not parse goes back");
        assert_eq!(state.field, None);
    }
}
