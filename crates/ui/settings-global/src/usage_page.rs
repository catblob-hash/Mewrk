//! `UsageSettings.tsx` and `UsageStatsCard.tsx`: the usage statistics page. Conversation, message and activity data come
//! from the conversation store; token usage comes from the built-in gateway ledger.

use std::collections::HashMap;

use mewrk_client::prelude::*;
use mewrk_client::runs::conversation_turns::{load_conversation_turns, CONVERSATION_TURNS_STORAGE_KEY};
use serde::Serialize;

use crate::logic::usage::*;
use crate::widgets::*;

/// Records against which ledger the backfill has run. The value is the ledger's start, so a rebuilt ledger backfills again.
const USAGE_BACKFILL_STORAGE_KEY: &str = "mewrk.usage-backfilled.v1";
const BACKFILL_CHUNK: usize = 500;
const LEGEND_VISIBLE: usize = 6;
const SERIES_SHADES: usize = 8;
const AXIS_LABELS: usize = 7;
const TOOLTIP_ROWS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Overview,
    Models,
}

pub struct UsagePage {
    generation: u64,
    started: bool,
    statistics: UsageStatistics,
    loading: bool,
    tab: Tab,
    range: UsageRange,
    expanded: bool,
    hovered: Option<usize>,
    /// The column last read out, since the spoken copy follows `hovered` and only a change is worth saying.
    spoken: Option<usize>,
    series_key: String,
}

impl Default for UsagePage {
    fn default() -> Self {
        Self { generation: 0, started: false, statistics: UsageStatistics::default(), loading: true, tab: Tab::default(), range: UsageRange::All, expanded: false, hovered: None, spoken: None, series_key: String::new() }
    }
}

fn page_state(app: &mut App) -> &mut UsagePage {
    app.local::<UsagePage>()
}

pub fn unmount(app: &mut App) {
    let generation = page_state(app).generation + 1;
    *page_state(app) = UsagePage { generation, ..Default::default() };
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageBackfillEvent {
    pub id: String,
    pub occurred_at_ms: i64,
    pub origin: &'static str,
    pub conversation_id: String,
    pub workspace_id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Pre-gateway usage survives only in the local turn table. `turn.usage` has already absorbed subagent usage, so subagent
/// records aren't added again; `id` is derivable, and the host's insert ignores what it already has.
pub fn usage_backfill_events(app: &mut App, now: i64) -> Vec<UsageBackfillEvent> {
    let raw = app.ui_store().get(CONVERSATION_TURNS_STORAGE_KEY).map(str::to_owned);
    let turns = load_conversation_turns(raw.as_deref(), now);
    let mut workspace_of: HashMap<&str, &str> = HashMap::new();
    for workspace in &app.doc.workspaces {
        for conversation in &workspace.conversations {
            workspace_of.insert(&conversation.id, &workspace.id);
        }
    }
    // Old turns recorded only a model id, so the current catalog is the best restoration: an unknown one stays blank, and
    // the card shows Unknown provider rather than making one up.
    let mut provider_of: HashMap<&str, (&str, &str)> = HashMap::new();
    for provider in &app.doc.assets.api_providers {
        for model in &provider.models {
            provider_of.entry(&model.id).or_insert((&provider.id, &provider.name));
        }
    }
    let mut events = Vec::new();
    for (conversation_id, list) in &turns {
        for turn in list {
            let (input, output, cached) = (turn.usage.input_tokens.unwrap_or(0), turn.usage.output_tokens.unwrap_or(0), turn.usage.cached_input_tokens.unwrap_or(0));
            let total = turn.usage.total_tokens.unwrap_or(input + output);
            if input == 0 && output == 0 && cached == 0 && total == 0 {
                continue;
            }
            let at = turn.ended_at.as_deref().unwrap_or(&turn.started_at);
            let Ok(occurred) = chrono::DateTime::parse_from_rfc3339(at) else { continue };
            let (provider_id, provider_name) = provider_of.get(turn.model_id.as_str()).copied().unwrap_or(("", ""));
            events.push(UsageBackfillEvent { id: format!("turn:{conversation_id}:{}", turn.id), occurred_at_ms: occurred.timestamp_millis(), origin: "conversation", conversation_id: conversation_id.clone(), workspace_id: workspace_of.get(conversation_id.as_str()).map_or_else(String::new, |w| (*w).to_owned()), provider_id: provider_id.to_owned(), provider_name: provider_name.to_owned(), model_id: turn.model_id.clone(), requests: 1, input_tokens: input, cached_input_tokens: cached, output_tokens: output, total_tokens: total });
        }
    }
    events
}

fn read_statistics(app: &mut App, then_backfill: bool) {
    let generation = page_state(app).generation;
    app.invoke::<UsageStatistics>("token_usage_statistics", json!({}), move |app, result| {
        if page_state(app).generation != generation {
            return;
        }
        match result {
            Ok(read) => {
                page_state(app).statistics = read.clone();
                if then_backfill && ensure_backfill(app, &read) {
                    return;
                }
            }
            Err(e) => eprintln!("{}: {e}", t("读取使用统计失败", "Failed to read usage statistics")),
        }
        page_state(app).loading = false;
    });
}

/// Backfills once per ledger, by its accounting start rather than a resettable flag. Whether a push went out, and so
/// whether the statistics will be read again.
fn ensure_backfill(app: &mut App, statistics: &UsageStatistics) -> bool {
    if statistics.ledger_started_at_ms == 0 {
        return false;
    }
    let marker = statistics.ledger_started_at_ms.to_string();
    if app.ui_store().get(USAGE_BACKFILL_STORAGE_KEY) == Some(marker.as_str()) {
        return false;
    }
    let started = statistics.ledger_started_at_ms;
    let events: Vec<UsageBackfillEvent> = usage_backfill_events(app, now_ms()).into_iter().filter(|e| e.occurred_at_ms < started).collect();
    app.ui_store_update(|store| store.set(USAGE_BACKFILL_STORAGE_KEY, marker));
    if events.is_empty() {
        return false;
    }
    let generation = page_state(app).generation;
    let chunks: Vec<Value> = events.chunks(BACKFILL_CHUNK).map(|c| json!({ "events": c })).collect();
    app.spawn(move |task| {
        for chunk in chunks {
            if let Err(e) = task.invoke::<Value>("backfill_token_usage", chunk) {
                eprintln!("backfill_token_usage: {e}");
            }
        }
        task.post(move |app| {
            if page_state(app).generation == generation {
                read_statistics(app, false);
            }
        });
    });
    true
}

/// No track behind the buttons: each one is bare text until it is hovered or selected, and then it carries its own rounded patch.
fn segmented<T: Copy + PartialEq + 'static>(options: Vec<(T, String, u64)>, selected: T, compact: bool, on_select: fn(&mut App, T)) -> E {
    let t_ = theme();
    // The compact one is a choice of range, the other switches what is shown.
    let mut group = row().gap(6.0).items_center().role(if compact { Role::Group } else { Role::TabList });
    for (value, label, key) in options {
        let active = value == selected;
        group = group.child(row().key(("usage-seg", key)).py(6.0).px(if compact { 11.0 } else { 14.0 }).items_center().radius(8.0).when(active, |e| e.bg(t_.subtle_pressed)).hover_bg(t_.subtle_hover).when(compact, |e| e.aria_pressed(active)).when(!compact, |e| e.role(Role::Tab).aria_selected(active)).on_click(move |app: &mut App| on_select(app, value)).child(text(label).ty(BODY_STRONG).color(if active { t_.text_primary } else { t_.text_secondary }).nowrap()));
    }
    group
}

/// The palette is one accent ramp; models past the last step share its palest shade.
fn shade(index: usize) -> Color {
    let t_ = theme();
    let i = index.min(SERIES_SHADES - 1) as f32;
    t_.accent.with_alpha(1.0 - i * 0.1)
}

fn heat_colour(level: u8, future: bool) -> Color {
    let t_ = theme();
    if future {
        return Color::TRANSPARENT;
    }
    match level {
        0 => t_.subtle_pressed,
        1 => t_.accent.with_alpha(0.25),
        2 => t_.accent.with_alpha(0.45),
        3 => t_.accent.with_alpha(0.7),
        _ => t_.accent,
    }
}

fn purr_unit_label(unit: PurrUnit) -> &'static str {
    match unit {
        PurrUnit::Month => t("个月", "months"),
        PurrUnit::Day => t("天", "days"),
        PurrUnit::Hour => t("小时", "hours"),
        PurrUnit::Minute => t("分钟", "minutes"),
    }
}

/// The tiles and the note take the slack, so the rows grow with the pane. Two columns until there is room for four: a model id is a long string,
/// and a quarter of a squeezed pane ellipsises it down to nothing.
fn overview(summary: &UsageSummary, loading: bool, pane_width: f32) -> E {
    let t_ = theme();
    let chinese = !mewrk_client::i18n::is_english();
    let peak = match summary.peak_hour {
        None => "—".to_owned(),
        Some(h) if chinese => format!("{h} 时"),
        Some(h) => format_hour12(h),
    };
    let favorite_tip = summary.favorite_model.as_ref().map(|m| format!("{} · {}", if m.provider_name.is_empty() { t("未知提供商", "Unknown provider") } else { m.provider_name.as_str() }, m.model_id));
    let days = |n: u64| tf("{days} 天", "{days}d", &[("days", &n)]);
    let tiles: Vec<(&str, String, Option<String>)> = vec![
        (t("会话数", "Sessions"), format_exact_count(summary.sessions), None),
        (t("消息数", "Messages"), format_exact_count(summary.messages), None),
        (t("总 token", "Total tokens"), format_compact_count(summary.total_tokens), Some(format_exact_count(summary.total_tokens))),
        (t("活跃天数", "Active days"), format_exact_count(summary.active_days), None),
        (t("当前连续", "Current streak"), days(summary.current_streak), None),
        (t("最长连续", "Longest streak"), days(summary.longest_streak), None),
        (t("高峰时段", "Peak hour"), peak, None),
        (t("最常用模型", "Favorite model"), summary.favorite_model.as_ref().map_or_else(|| "—".to_owned(), |m| m.model_id.clone()), favorite_tip),
    ];
    let columns = if pane_width > 0.0 && pane_width < 620.0 { 2 } else { 4 };
    // `clamp(18px, 3cqw, 28px)`: the number is the point of the tile, bounded so a wide pane doesn't turn it into a poster.
    let value_size = (pane_width * 0.03).clamp(18.0, 28.0);
    let value_type = fluent::theme::TypeStyle { size: value_size, line: (value_size * 1.25).round(), ..BODY_STRONG };
    let mut grid = col().flex1().min_h(0.0).gap(6.0);
    let mut iter = tiles.into_iter().peekable();
    while iter.peek().is_some() {
        let mut line = row().flex1().min_h(64.0).gap(6.0);
        for _ in 0..columns {
            if let Some((label, value, tip)) = iter.next() {
                line = line.child(col().key(("usage-tile", label)).flex1().min_w(0.0).justify_center().py(14.0).px(18.0).gap(6.0).radius(12.0).bg(t_.subtle_hover).child(text(label).ty(BODY_SMALL).color(t_.text_tertiary).ellipsis()).child(text(value).ty(value_type).ellipsis().when(tip.is_some(), |e| e.tooltip(tip.clone().unwrap_or_default()))));
            }
        }
        grid = grid.child(line);
    }
    // Stretched columns rather than fixed squares: the width is whatever the pane gives, and the square is derived from it up to a cap, past which the columns keep their tracks.
    let mut heatmap = row().key("usage-heatmap").gap(6.0).shrink0().role(Role::Image).aria_label(tf("最近 26 周的活跃度，共 {days} 个活跃日", "Activity over the last 26 weeks, {days} active days", &[("days", &summary.active_days)]));
    for week in &summary.heatmap_weeks {
        let mut column = col().flex1().min_w(0.0).items_center().gap(6.0).aria_hidden(true);
        for cell in week {
            let tip = (!cell.future).then(|| tf("{day}：{messages} 条消息 · {tokens} token", "{day}: {messages} messages · {tokens} tokens", &[("day", &cell.day), ("messages", &format_exact_count(cell.messages)), ("tokens", &format_compact_count(cell.tokens))]));
            column = column.child(div().w_full().max_w(34.0).aspect(1.0).radius(5.0).bg(heat_colour(cell.level, cell.future)).when(tip.is_some(), |e| e.tooltip(tip.clone().unwrap_or_default())));
        }
        heatmap = heatmap.child(column);
    }
    let note = match purr_duration(summary.total_tokens) {
        Some((value, unit)) => tf("相当于一只猫连续呼噜了 {duration}。", "That's a cat purring nonstop for {duration}.", &[("duration", &format!("{value:.2} {}", purr_unit_label(unit)))]),
        None if loading => t("正在读取统计…", "Loading statistics…").to_owned(),
        None => t("还没有记录到 token 用量。", "No token usage recorded yet.").to_owned(),
    };
    col().flex1().min_h(0.0).gap(14.0).child(grid).child(heatmap).child(text(note).ty(BODY_SMALL).color(t_.text_tertiary).shrink0())
}

fn legend_title(row_data: &ModelUsageRow) -> String {
    [
        if row_data.provider_name.is_empty() { t("未知提供商", "Unknown provider").to_owned() } else { row_data.provider_name.clone() },
        tf("{count} 次请求", "{count} requests", &[("count", &format_exact_count(row_data.requests))]),
        tf("输入 {value}", "Input {value}", &[("value", &format_exact_count(row_data.input_tokens))]),
        tf("输出 {value}", "Output {value}", &[("value", &format_exact_count(row_data.output_tokens))]),
        tf("缓存 {value}", "Cached {value}", &[("value", &format_exact_count(row_data.cached_input_tokens))]),
        tf("总计 {value}", "Total {value}", &[("value", &format_exact_count(row_data.total_tokens))]),
    ]
    .join(" · ")
}

/// The spoken row. It names the share too: a bare `%` beside a number is not a sentence.
fn legend_detail(row_data: &ModelUsageRow) -> String {
    [row_data.model_id.clone(), legend_title(row_data), tf("占 {value}", "{value} of all tokens", &[("value", &format_share(row_data.share))])].join(" · ")
}

fn bucket_noun(bucket: BucketSize) -> &'static str {
    match bucket {
        BucketSize::Month => t("月", "month"),
        BucketSize::Week => t("周", "week"),
        BucketSize::Day => t("天", "day"),
    }
}

fn chart(app: &mut App, series: &UsageSeries, models: &[ModelUsageRow]) -> E {
    let t_ = theme();
    let chinese = !mewrk_client::i18n::is_english();
    // Remounted when the bucketing changes, so the hovered index can't point at a column that is no longer the one there.
    let series_key = format!("{:?}:{}", series.bucket, series.points.len());
    if page_state(app).series_key != series_key {
        let s = page_state(app);
        s.series_key = series_key;
        s.hovered = None;
    }
    let hovered = page_state(app).hovered;
    let scale = if series.max > 0 { series.max as f32 } else { 1.0 };
    let last = series.points.len().saturating_sub(1);
    let point = hovered.and_then(|i| series.points.get(i));
    let plot_width = rect_of("usage-plot").map_or(0.0, |r| r.w);
    let label_width = if series.bucket == BucketSize::Month { 82.0 } else { 68.0 };
    let budget = if plot_width > 0.0 { ((plot_width / label_width).floor() as usize).clamp(2, AXIS_LABELS) } else { AXIS_LABELS };
    let stride = (series.points.len().div_ceil(budget)).max(1);

    let speak = a11y_active() && page_state(app).spoken != hovered;
    if speak {
        page_state(app).spoken = hovered;
    }

    let mut axis = div().w(46.0).shrink0().aria_hidden(true);
    for tick in &series.ticks {
        axis = axis.child(text(format_compact_count(*tick)).ty(FINE).color(t_.text_tertiary).nowrap().absolute().right(0.0).bottom_pct(*tick as f32 / scale).mb(-7.0));
    }
    let mut columns = row().absolute().inset(0.0).items_end();
    for (index, entry) in series.points.iter().enumerate() {
        // Stacked from the floor up, first model at the bottom, and clipped so the bar's own top edge is what rounds.
        let mut stack = col().w_pct(78.0).max_w(44.0).min_w(1.0).h_pct((entry.total_tokens as f32 / scale) * 100.0).radii([4.0, 4.0, 0.0, 0.0]).clip();
        for (model_index, tokens) in entry.by_model.iter().enumerate().rev() {
            if *tokens > 0 && model_index < models.len() {
                stack = stack.child(div().w_full().basis(0.0).flex_grow(*tokens as f32).bg(shade(model_index)));
            }
        }
        columns = columns.child(
            row()
                .key(("usage-column", index))
                .flex1()
                .min_w(0.0)
                .h_full()
                .items_end()
                .justify_center()
                .when(hovered == Some(index), |e| e.bg(t_.subtle_hover))
                .on_hover_change(move |app: &mut App, on| {
                    let s = page_state(app);
                    if on {
                        s.hovered = Some(index);
                    } else if s.hovered == Some(index) {
                        s.hovered = None;
                    }
                    repaint();
                })
                .child(stack),
        );
    }
    let mut plot = div().key("usage-plot").flex1().min_w(0.0).role(Role::Group).when(a11y_active(), |e| e.aria_label(tf("按{bucket}分列的 token 用量，峰值 {peak}；用左右方向键逐列查看", "Token usage per {bucket}, peaking at {peak}. Use the arrow keys to step through it.", &[("bucket", &bucket_noun(series.bucket)), ("peak", &format_compact_count(series.max))]))).on_key(move |app: &mut App, k: &KeyPress| {
        let s = page_state(app);
        match k.key {
            Key::Left => s.hovered = Some(s.hovered.map_or(last, |c| c.saturating_sub(1))),
            Key::Right => s.hovered = Some(s.hovered.map_or(0, |c| (c + 1).min(last))),
            Key::Home => s.hovered = Some(0),
            Key::End => s.hovered = Some(last),
            Key::Escape => s.hovered = None,
            _ => return false,
        }
        repaint();
        true
    });
    for tick in &series.ticks {
        plot = plot.child(div().absolute().left(0.0).right(0.0).h(1.0).bg(t_.divider).bottom_pct(*tick as f32 / scale).pass_through());
    }
    plot = plot.child(columns);
    if let Some(p) = point {
        let mut rows: Vec<(usize, u64)> = p.by_model.iter().copied().enumerate().filter(|(i, n)| *n > 0 && *i < models.len()).collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        // The card below stays decorative: this is its spoken copy.
        if speak {
            announce(std::iter::once(bucket_title(p.start_ms, series.bucket, chinese)).chain(rows.iter().map(|(i, n)| format!("{} {}", models[*i].model_id, format_exact_count(*n)))).collect::<Vec<_>>().join(" · "));
        }
        if !rows.is_empty() {
            let rest = rows.len().saturating_sub(TOOLTIP_ROWS);
            // The card sits over the columns it describes, so it takes no pointer: that would drop the hover that opened it.
            let mut tip = flyout_panel::<App>().py(9.0).px(12.0).gap(4.0).min_w(170.0).pass_through().aria_hidden(true).child(text(bucket_title(p.start_ms, series.bucket, chinese)).ty(BODY_SMALL_STRONG).nowrap());
            for (i, n) in rows.iter().take(TOOLTIP_ROWS) {
                tip = tip.child(row().gap(8.0).items_center().child(div().size(11.0, 11.0).radius(6.0).shrink0().bg(shade(*i))).child(text(models[*i].model_id.clone()).ty(BODY_SMALL).color(t_.text_secondary).ellipsis().flex1().min_w(0.0)).child(text(format_compact_count(*n)).ty(BODY_SMALL_STRONG).nowrap()));
            }
            if rest > 0 {
                tip = tip.child(text(tf("还有 {count} 个模型", "{count} more models", &[("count", &rest)])).ty(CAPTION).color(t_.text_tertiary));
            }
            let end_side = hovered.is_some_and(|i| i * 2 >= last);
            plot = plot.child(if end_side { tip.absolute().top(0.0).right(0.0) } else { tip.absolute().top(0.0).left(0.0) });
        }
    }
    // Labels are counted back from the last bar, so the most recent one always carries its date. The outer ones hold the plot's edges; the rest centre under their bars.
    let mut days = div().ml(56.0).h(16.0).mt(8.0).clip().aria_hidden(true);
    let total = series.points.len().max(1) as f32;
    for (index, entry) in series.points.iter().enumerate() {
        if (last - index).is_multiple_of(stride) {
            let label = text(bucket_label(entry.start_ms, series.bucket, chinese)).ty(FINE).color(t_.text_tertiary).nowrap().absolute().top(0.0);
            days = days.child(if index == last {
                label.right(0.0)
            } else if index == 0 {
                label.left(0.0)
            } else {
                label.w(label_width).ml(-label_width * 0.5).text_center().left_pct((index as f32 + 0.5) / total)
            });
        }
    }
    col().flex1().min_h(0.0).pt(6.0).child(row().flex1().min_h(140.0).gap(10.0).child(axis).child(plot)).child(days)
}

fn models_panel(app: &mut App, summary: &UsageSummary, loading: bool) -> E {
    let t_ = theme();
    if summary.models.is_empty() {
        return text(if loading { t("正在读取统计…", "Loading statistics…") } else { t("这个区间还没有任何模型用量。", "No model usage in this range yet.") }).ty(BODY_SMALL).color(t_.text_tertiary);
    }
    let expanded = page_state(app).expanded;
    let visible = if expanded { summary.models.len() } else { summary.models.len().min(LEGEND_VISIBLE) };
    let hidden = summary.models.len() - visible;
    let mut legend = col().shrink0().role(Role::List);
    for (index, row_data) in summary.models.iter().take(visible).enumerate() {
        legend = legend.child(
            row()
                .key(("usage-legend", index))
                .gap(10.0)
                .py(9.0)
                .items_center()
                .when(index > 0, |e| e.border_top(t_.divider))
                .tooltip(legend_title(row_data))
                .role(Role::ListItem)
                .when(a11y_active(), |e| e.aria_description(legend_detail(row_data)))
                .child(div().size(11.0, 11.0).radius(3.0).shrink0().bg(shade(index)))
                .child(text(row_data.model_id.clone()).ty(BODY_STRONG).ellipsis().flex1().min_w(0.0))
                .child(text(tf("{input} 输入 · {output} 输出 · {cache} 缓存", "{input} in · {output} out · {cache} cache", &[("input", &format_compact_count(row_data.input_tokens)), ("output", &format_compact_count(row_data.output_tokens)), ("cache", &format_compact_count(row_data.cached_input_tokens))])).ty(CAPTION).color(t_.text_tertiary).nowrap())
                .child(text(format_share(row_data.share)).ty(BODY_STRONG).color(t_.text_secondary).nowrap().min_w(56.0).text_end()),
        );
    }
    let more = (summary.models.len() > LEGEND_VISIBLE).then(|| text(if expanded { t("收起", "Show less").to_owned() } else { tf("显示其余 {count} 个", "Show {count} more", &[("count", &hidden)]) }).key("usage-legend-more").ty(BODY_SMALL_STRONG).color(t_.text_secondary).nowrap().self_start().cursor(CursorKind::Hand).aria_expanded(expanded).on_click(|app: &mut App| {
        let s = page_state(app);
        s.expanded = !s.expanded;
    }));
    let chart_el = chart(app, &summary.series, &summary.models);
    col().flex1().min_h(0.0).gap(14.0).child(chart_el).child(legend).child(more)
}

/// The page.
pub fn usage_settings(app: &mut App) -> E {
    if !page_state(app).started {
        page_state(app).started = true;
        read_statistics(app, true);
    }
    let s = page_state(app);
    let (tab, range, loading) = (s.tab, s.range, s.loading);
    let statistics = s.statistics.clone();
    let now = if statistics.generated_at_ms != 0 { statistics.generated_at_ms } else { now_ms() };
    let summary = summarize_usage(&statistics, range, now);
    let tabs = segmented(vec![(Tab::Overview, t("总览", "Overview").to_owned(), 1), (Tab::Models, t("模型", "Models").to_owned(), 2)], tab, false, |app, v| page_state(app).tab = v).aria_label(t("统计视图", "Statistics view"));
    let range_label = |v: UsageRange| match v {
        UsageRange::All => t("全部", "All").to_owned(),
        UsageRange::D30 => t("30 天", "30d").to_owned(),
        UsageRange::D7 => t("7 天", "7d").to_owned(),
    };
    let ranges = segmented(USAGE_RANGES.iter().enumerate().map(|(i, v)| (*v, range_label(*v), 10 + i as u64)).collect(), range, true, |app, v| page_state(app).range = v).aria_label(t("统计区间", "Statistics range"));
    // The tiles answer to the pane's width rather than the window's.
    let pane_width = rect_of("usage-body").map_or_else(
        || {
            // Known from the first frame on, and nothing else would ask for the frame that uses it.
            repaint();
            0.0
        },
        |r| r.w,
    );
    let body = match tab {
        Tab::Overview => overview(&summary, loading, pane_width),
        Tab::Models => models_panel(app, &summary, loading),
    };
    let strip = settings_strip();
    // The page has no card around it: the statistics are the page, stretched to whatever the pane gives.
    col().min_h(settings_body_height()).pt(if strip { 22.0 } else { 8.0 }).px(24.0).pb(26.0).child(col().key("usage-body").flex1().min_h(0.0).gap(14.0).role(Role::Region).aria_label(t("使用统计", "Usage statistics")).aria_busy(loading).child(row().items_center().justify_between().gap(6.0).pr(if strip { 0.0 } else { 28.0 }).shrink0().child(tabs).child(ranges)).child(body))
}
