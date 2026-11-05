//! `components/UsageStatsCard.tsx` and `components/UsageSettings.tsx`: the usage statistics page.

use std::collections::HashMap;

use chrono::{Datelike, Local as ChronoLocal, TimeZone};
use mewrk_client::model::AppDocument;
use mewrk_client::prelude::*;

use fluent::{TypeStyle, Weight};

use mewrk_client::runs::conversation_turns::{load_conversation_turns, CONVERSATION_TURNS_STORAGE_KEY};

use crate::usage_statistics::*;
use crate::util::now_ms;
use crate::widgets::ts;

const LEGEND_VISIBLE: usize = 6;
const SERIES_SHADES: usize = 8;
const AXIS_LABELS: usize = 7;
const TOOLTIP_ROWS: usize = 8;

fn label_width(bucket: UsageBucketSize) -> f32 {
    if bucket == UsageBucketSize::Month { 82.0 } else { 68.0 }
}

const GRID_GAP: f32 = 6.0;

/// The even ramp `color-mix` makes of the accent; shade index is model rank, so a colour needs no map.
fn series_color(index: usize) -> Color {
    let c = theme();
    let steps = [1.0, 0.89, 0.78, 0.67, 0.56, 0.45, 0.34, 0.23];
    c.card_secondary.mix(c.accent, steps[index.min(SERIES_SHADES - 1)])
}

fn format_hour12(hour: u32) -> String {
    let suffix = if hour < 12 { "AM" } else { "PM" };
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!("{twelve} {suffix}")
}

/// One decimal: rounding to whole percents lines up a column of "0%" that hides the order.
pub fn format_share(share: f64) -> String {
    let percent = if share.is_finite() { share.max(0.0) * 100.0 } else { 0.0 };
    format!("{percent:.1}%")
}

fn month_name(month: u32) -> &'static str {
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(month as usize - 1).min(11)]
}

/// `Intl.DateTimeFormat` with a short month, in the app's language.
pub fn bucket_label(start_ms: i64, bucket: UsageBucketSize) -> String {
    bucket_label_in(start_ms, bucket, mewrk_client::i18n::is_english())
}

pub fn bucket_label_in(start_ms: i64, bucket: UsageBucketSize, english: bool) -> String {
    let Some(at) = ChronoLocal.timestamp_millis_opt(start_ms).single() else { return String::new() };
    match (bucket, english) {
        (UsageBucketSize::Month, true) => format!("{} {}", month_name(at.month()), at.year()),
        (UsageBucketSize::Month, false) => format!("{}年{}月", at.year(), at.month()),
        (_, true) => format!("{} {}", month_name(at.month()), at.day()),
        (_, false) => format!("{}月{}日", at.month(), at.day()),
    }
}

/// A weekly bar's title spans both ends: the Monday alone never reads as seven days.
pub fn bucket_title(start_ms: i64, bucket: UsageBucketSize) -> String {
    bucket_title_in(start_ms, bucket, mewrk_client::i18n::is_english())
}

pub fn bucket_title_in(start_ms: i64, bucket: UsageBucketSize, english: bool) -> String {
    if bucket != UsageBucketSize::Week {
        return bucket_label_in(start_ms, bucket, english);
    }
    format!("{} – {}", bucket_label_in(start_ms, UsageBucketSize::Day, english), bucket_label_in(start_ms + 6 * 86_400_000, UsageBucketSize::Day, english))
}

fn purr_unit_label(unit: PurrUnit) -> &'static str {
    match unit {
        PurrUnit::Month => t("个月", "months"),
        PurrUnit::Day => t("天", "days"),
        PurrUnit::Hour => t("小时", "hours"),
        PurrUnit::Minute => t("分钟", "minutes"),
    }
}

fn legend_title(row: &ModelUsageRow) -> String {
    [
        if row.provider_name.is_empty() { t("未知提供商", "Unknown provider").to_owned() } else { row.provider_name.clone() },
        tf("{count} 次请求", "{count} requests", &[("count", &format_exact_count(row.requests))]),
        tf("输入 {value}", "Input {value}", &[("value", &format_exact_count(row.input_tokens))]),
        tf("输出 {value}", "Output {value}", &[("value", &format_exact_count(row.output_tokens))]),
        tf("缓存 {value}", "Cached {value}", &[("value", &format_exact_count(row.cached_input_tokens))]),
        tf("总计 {value}", "Total {value}", &[("value", &format_exact_count(row.total_tokens))]),
    ]
    .join(" · ")
}

fn segmented_button(key: (&'static str, &'static str), label: &str, active: bool, compact: bool, on_click: impl Fn(&mut App) + 'static) -> E {
    let c = theme();
    row()
        .key(key)
        .items_center()
        .py(6.0)
        .px(if compact { 11.0 } else { 14.0 })
        .radius(8.0)
        .when(active, |e| e.bg(c.subtle_hover))
        .hover_bg(c.subtle_hover)
        .on_click(on_click)
        .child(text(label.to_owned()).ty(ts(14.0, 19.0, Weight::Semibold)).color(if active { c.text_primary } else { c.text_secondary }).nowrap())
}

#[derive(Clone, Copy, PartialEq)]
enum CardTab {
    Overview,
    Models,
}

/// The usage statistics page body. Conversation and activity data come from the conversation store, token usage from
/// the built-in gateway ledger.
pub fn usage_stats_card(statistics: &UsageStatistics, loading: bool) -> E {
    let c = theme();
    let tab = use_state("usage-card-tab", || CardTab::Overview);
    let range = use_state("usage-card-range", || UsageRange::All);
    let summary = summarize_usage(statistics, range.get(), None);
    let purr = purr_duration(summary.total_tokens);
    let range_label = |v: UsageRange| match v {
        UsageRange::All => t("全部", "All"),
        UsageRange::Days30 => t("30 天", "30d"),
        UsageRange::Days7 => t("7 天", "7d"),
    };
    let toolbar = row()
        .items_center()
        .justify_between()
        .gap(GRID_GAP)
        .shrink0()
        .child(row().gap(GRID_GAP).children([(CardTab::Overview, "usage-tab-overview", t("总览", "Overview")), (CardTab::Models, "usage-tab-models", t("模型", "Models"))].into_iter().map(|(value, key, label)| {
            let tab = tab.clone();
            segmented_button(("usage-card", key), label, tab.get() == value, false, move |_| tab.set(value))
        })))
        .child(row().gap(GRID_GAP).children(USAGE_RANGES.iter().map(|value| {
            let (range, value) = (range.clone(), *value);
            let key = match value {
                UsageRange::All => "usage-range-all",
                UsageRange::Days30 => "usage-range-30d",
                UsageRange::Days7 => "usage-range-7d",
            };
            segmented_button(("usage-card", key), range_label(value), range.get() == value, true, move |_| range.set(value))
        })));
    let mut body = col().flex1().min_h(0.0).gap(14.0).child(toolbar);
    if tab.get() == CardTab::Overview {
        body = body.child(tiles(&summary)).child(heatmap_grid(&summary));
        let note = match purr {
            Some((value, unit)) => tf("相当于一只猫连续呼噜了 {duration}。", "That's a cat purring nonstop for {duration}.", &[("duration", &format!("{value:.2} {}", purr_unit_label(unit)))]),
            None if loading => t("正在读取统计…", "Loading statistics…").to_owned(),
            None => t("还没有记录到 token 用量。", "No token usage recorded yet.").to_owned(),
        };
        body = body.child(text(note).key("usage-note").ty(ts(13.0, 19.5, Weight::Regular)).color(c.text_tertiary).shrink0());
    } else {
        body = body.child(model_usage_panel(&summary, loading));
    }
    col().key("usage-stats").flex1().min_h(0.0).scroll_y().pt(22.0).px(24.0).pb(26.0).child(body)
}

fn tiles(summary: &UsageSummary) -> E {
    let c = theme();
    let peak = match summary.peak_hour {
        None => "—".to_owned(),
        Some(hour) if !mewrk_client::i18n::is_english() => format!("{hour} 时"),
        Some(hour) => format_hour12(hour),
    };
    let favorite_title = summary.favorite_model.as_ref().map(|m| format!("{} · {}", if m.provider_name.is_empty() { t("未知提供商", "Unknown provider") } else { &m.provider_name }, m.model_id));
    let items: Vec<(&str, String, String, Option<String>)> = vec![
        ("sessions", t("会话数", "Sessions").into(), format_exact_count(summary.sessions), None),
        ("messages", t("消息数", "Messages").into(), format_exact_count(summary.messages), None),
        ("tokens", t("总 token", "Total tokens").into(), format_compact_count(summary.total_tokens), Some(format_exact_count(summary.total_tokens))),
        ("activeDays", t("活跃天数", "Active days").into(), format_exact_count(summary.active_days), None),
        ("currentStreak", t("当前连续", "Current streak").into(), tf("{days} 天", "{days}d", &[("days", &summary.current_streak)]), None),
        ("longestStreak", t("最长连续", "Longest streak").into(), tf("{days} 天", "{days}d", &[("days", &summary.longest_streak)]), None),
        ("peakHour", t("高峰时段", "Peak hour").into(), peak, None),
        ("favoriteModel", t("最常用模型", "Favorite model").into(), summary.favorite_model.as_ref().map_or("—".to_owned(), |m| m.model_id.clone()), favorite_title),
    ];
    // Two columns until there is room for four: a quarter of a squeezed pane ellipsises a model id to nothing.
    let width = rect_of("usage-tiles").map_or(0.0, |r| r.w);
    let columns = if width >= 620.0 { 4 } else { 2 };
    let value_size = (width * 0.03).clamp(18.0, 28.0);
    let value_ty: TypeStyle = ts(value_size, value_size + 6.0, Weight::Semibold);
    let mut grid = col().key("usage-tiles").gap(GRID_GAP).flex_grow(1.0).shrink0();
    for chunk in items.chunks(columns) {
        let mut r = row().gap(GRID_GAP).flex_grow(1.0).min_h(64.0);
        for (key, label, value, title) in chunk {
            r = r.child(
                col()
                    .key(("usage-tile", *key))
                    .flex1()
                    .justify_center()
                    .gap(6.0)
                    .py(14.0)
                    .px(18.0)
                    .radius(12.0)
                    .bg(c.card_secondary)
                    .tooltip(title.clone().unwrap_or_default())
                    .child(text(label.clone()).ty(ts(13.0, 18.0, Weight::Regular)).color(c.text_tertiary).ellipsis())
                    .child(text(value.clone()).key(("usage-tile-value", *key)).ty(value_ty).color(c.text_primary).ellipsis()),
            );
        }
        grid = grid.child(r);
    }
    grid
}

fn heatmap_grid(summary: &UsageSummary) -> E {
    let c = theme();
    // Twenty-six stretched columns: the square is derived from the width, and capped so a wide pane does not turn the
    // heatmap into something that pushes everything else off the page.
    let width = rect_of("usage-heatmap").map_or(0.0, |r| r.w);
    let cell = if width > 0.0 { ((width - GRID_GAP * 25.0) / 26.0).clamp(1.0, 34.0) } else { 12.0 };
    let shade = |level: u8| match level {
        1 => c.card_secondary.mix(c.accent, 0.22),
        2 => c.card_secondary.mix(c.accent, 0.45),
        3 => c.card_secondary.mix(c.accent, 0.70),
        4 => c.accent,
        _ => c.card_secondary,
    };
    row().key("usage-heatmap").gap(GRID_GAP).shrink0().clip().children(summary.heatmap_weeks.iter().map(|week| {
        col().flex1().items_center().gap(GRID_GAP).children(week.iter().map(|day| {
            let tip = if day.future { String::new() } else { tf("{day}：{messages} 条消息 · {tokens} token", "{day}: {messages} messages · {tokens} tokens", &[("day", &day.day), ("messages", &format_exact_count(day.messages)), ("tokens", &format_compact_count(day.tokens))]) };
            div().key(("usage-heat", day.day.as_str())).size(cell, cell).radius(5.0).when(!day.future, |e| e.bg(shade(day.level))).tooltip(tip)
        }))
    }))
}

fn model_usage_panel(summary: &UsageSummary, loading: bool) -> E {
    let c = theme();
    let expanded = use_state("usage-legend-expanded", || false);
    let models = &summary.models;
    if models.is_empty() {
        let note = if loading { t("正在读取统计…", "Loading statistics…") } else { t("这个区间还没有任何模型用量。", "No model usage in this range yet.") };
        return col().flex1().child(text(note).key("usage-models-empty").ty(ts(13.0, 19.5, Weight::Regular)).color(c.text_tertiary));
    }
    let is_expanded = expanded.get();
    let visible = if is_expanded { models.as_slice() } else { &models[..models.len().min(LEGEND_VISIBLE)] };
    let hidden = models.len() - visible.len();
    let mut legend = col().shrink0();
    for (index, row_data) in visible.iter().enumerate() {
        legend = legend.child(
            row()
                .key(("usage-legend-row", row_data.key.as_str()))
                .items_center()
                .gap(10.0)
                .py(9.0)
                .when(index > 0, |e| e.border_top(c.card_secondary))
                .tooltip(legend_title(row_data))
                .child(div().size(11.0, 11.0).radius(3.0).bg(series_color(index)).shrink0())
                .child(text(row_data.model_id.clone()).ty(ts(14.0, 19.0, Weight::Semibold)).color(c.text_primary).ellipsis().flex1())
                .child(text(tf("{input} 输入 · {output} 输出 · {cache} 缓存", "{input} in · {output} out · {cache} cache", &[("input", &format_compact_count(row_data.input_tokens)), ("output", &format_compact_count(row_data.output_tokens)), ("cache", &format_compact_count(row_data.cached_input_tokens))])).ty(ts(12.5, 17.0, Weight::Regular)).color(c.text_tertiary).nowrap().shrink0())
                .child(text(format_share(row_data.share)).ty(ts(14.0, 19.0, Weight::Semibold)).color(c.text_secondary).nowrap().min_w(56.0).text_end().shrink0()),
        );
    }
    let mut panel = col().flex1().min_h(0.0).gap(14.0).child(usage_series_chart(&summary.series, models)).child(legend);
    if models.len() > LEGEND_VISIBLE {
        let toggle = expanded.clone();
        let label = if is_expanded { t("收起", "Show less").to_owned() } else { tf("显示其余 {count} 个", "Show {count} more", &[("count", &hidden)]) };
        panel = panel.child(div().key("usage-legend-more").self_start().shrink0().on_click(move |_| toggle.update(|v| *v = !*v)).child(text(label).ty(ts(13.0, 18.0, Weight::Semibold)).color(c.text_secondary).nowrap()));
    }
    panel
}

/// Bars stacked per model over time, with its axis, its dated labels and a hover card.
fn usage_series_chart(series: &UsageSeries, models: &[ModelUsageRow]) -> E {
    let c = theme();
    // Keyed by bucketing, so the hovered index can never point at a column that is no longer under the cursor.
    let hovered = use_state(("usage-chart-hovered", series.bucket as u8, series.points.len()), || None::<usize>);
    let plot_key = "usage-chart-plot";
    let plot_rect = rect_of(plot_key);
    let plot_w = plot_rect.map_or(0.0, |r| r.w);
    // Dividing by zero would make every bar NaN high.
    let scale = if series.max > 0 { series.max as f32 } else { 1.0 };
    let count = series.points.len();
    let last_index = count.saturating_sub(1);
    let point = hovered.get().and_then(|i| series.points.get(i));
    // Labels are counted back from the last bar, so the most recent one always carries its date.
    let budget = if plot_w > 0.0 { ((plot_w / label_width(series.bucket)).floor() as usize).clamp(2, AXIS_LABELS) } else { AXIS_LABELS };
    let stride = count.div_ceil(budget).max(1);
    let plot_h = plot_rect.map_or(140.0, |r| r.h).max(1.0);

    let mut axis = div().w(46.0).shrink0().h_full();
    for tick in &series.ticks {
        let bottom = *tick as f32 / scale * plot_h;
        axis = axis.child(text(format_compact_count(*tick)).ty(ts(12.0, 12.0, Weight::Regular)).color(c.text_tertiary).nowrap().absolute().right(0.0).bottom(bottom - 6.0));
    }

    let mut plot = div().key(plot_key).flex1().h_full().min_w(0.0);
    for tick in &series.ticks {
        plot = plot.child(div().h(1.0).bg(c.divider).absolute().left(0.0).right(0.0).bottom(*tick as f32 / scale * plot_h).pass_through());
    }
    let hovered_now = hovered.get();
    let mut columns = row().absolute().inset(0.0).items_end();
    for (index, entry) in series.points.iter().enumerate() {
        let bar_h = entry.total_tokens as f32 / scale * plot_h;
        let mut stack = col().w_pct(78.0).max_w(44.0).min_w(1.0).h(bar_h).radii([4.0, 4.0, 0.0, 0.0]).clip();
        // The biggest model sits at the bottom of the stack: what `column-reverse` did, written top first.
        for (model_index, tokens) in entry.by_model.iter().enumerate().rev() {
            if *tokens > 0 && models.get(model_index).is_some() {
                stack = stack.child(div().w_full().basis(0.0).flex_grow(*tokens as f32).bg(series_color(model_index)));
            }
        }
        columns = columns.child(
            row()
                .key(("usage-chart-column", entry.key.as_str()))
                .flex1()
                .h_full()
                .items_end()
                .justify_center()
                .when(hovered_now == Some(index), |e| e.bg(c.card_secondary))
                .child(stack),
        );
    }
    plot = plot.child(columns);
    // The whole plot rather than a listener per bar: there can be 240 of them. It follows the pointer only when the pointer
    // moved to another column, so a choice made from the keyboard stays until then.
    let (tracked, last_column) = (hovered.clone(), use_state(("usage-chart-pointer", series.bucket as u8, count), || None::<usize>));
    plot = plot.child(
        custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
            let column = ui.input.pointer.filter(|p| r.contains(p.0, p.1)).map(|p| (((p.0 - r.x) / r.w * count as f32) as usize).min(count.saturating_sub(1)));
            if column != last_column.get() {
                last_column.set(column);
                tracked.set(column);
            }
        })
        .absolute()
        .inset(0.0)
        .pass_through(),
    );
    // One tab stop plus arrow keys, rather than 240 focusable bars.
    let keyed = hovered.clone();
    plot = plot.on_key(move |_, k| {
        let current = keyed.get();
        match k.key {
            Key::Left => keyed.set(Some(current.map_or(last_index, |i| i.saturating_sub(1)))),
            Key::Right => keyed.set(Some(current.map_or(0, |i| (i + 1).min(last_index)))),
            Key::Home => keyed.set(Some(0)),
            Key::End => keyed.set(Some(last_index)),
            Key::Escape => keyed.set(None),
            _ => return false,
        }
        true
    });
    if let Some(point) = point {
        let mut rows: Vec<(i64, usize)> = point.by_model.iter().enumerate().filter(|(i, t)| **t > 0 && models.get(*i).is_some()).map(|(i, t)| (*t, i)).collect();
        rows.sort_by(|l, r| r.0.cmp(&l.0));
        if !rows.is_empty() {
            let at_end = hovered_now.is_some_and(|h| h * 2 >= last_index);
            let mut tip = col().key("usage-chart-tip").absolute().top(0.0).min_w(170f32.min(plot_w.max(1.0))).max_w(plot_w.max(170.0)).gap(4.0).py(9.0).px(12.0).radius(10.0).bg(c.flyout).border(c.flyout_stroke).shadow(9.0, 6.0, c.shadow).pass_through();
            tip = if at_end { tip.right(0.0) } else { tip.left(0.0) };
            tip = tip.child(text(bucket_title(point.start_ms, series.bucket)).ty(ts(13.0, 18.0, Weight::Semibold)).color(c.text_primary).nowrap());
            for (tokens, index) in rows.iter().take(TOOLTIP_ROWS) {
                tip = tip.child(
                    row()
                        .items_center()
                        .gap(8.0)
                        .child(div().size(11.0, 11.0).radius(6.0).bg(series_color(*index)).shrink0())
                        .child(text(models[*index].model_id.clone()).ty(ts(13.0, 18.0, Weight::Regular)).color(c.text_secondary).ellipsis().flex1())
                        .child(text(format_compact_count(*tokens)).ty(ts(13.0, 18.0, Weight::Semibold)).color(c.text_primary).nowrap().shrink0()),
                );
            }
            if rows.len() > TOOLTIP_ROWS {
                tip = tip.child(text(tf("还有 {count} 个模型", "{count} more models", &[("count", &(rows.len() - TOOLTIP_ROWS))])).ty(ts(12.0, 16.0, Weight::Regular)).color(c.text_tertiary).nowrap());
            }
            plot = plot.child(tip);
        }
    }

    let mut days = div().h(16.0).mt(8.0).ml(56.0).clip().shrink0();
    for (index, entry) in series.points.iter().enumerate() {
        if (last_index - index) % stride != 0 {
            continue;
        }
        let label = bucket_label(entry.start_ms, series.bucket);
        let mut day = text(label).key(("usage-chart-day", entry.key.as_str())).ty(ts(12.0, 16.0, Weight::Regular)).color(c.text_tertiary).nowrap().absolute().top(0.0);
        if index == last_index {
            day = day.right(0.0);
        } else if index == 0 {
            day = day.left(0.0);
        } else {
            // `translateX(-50%)`, which needs the label's width from last frame.
            let w = rect_of(("usage-chart-day", entry.key.as_str())).map_or(0.0, |r| r.w);
            day = day.left((index as f32 + 0.5) / count.max(1) as f32 * plot_w - w * 0.5);
        }
        days = days.child(day);
    }
    col().flex_grow(1.0).min_h(0.0).pt(6.0).child(row().min_h(140.0).flex_grow(1.0).gap(10.0).child(axis).child(plot)).child(days)
}

/// What the page reads and when: statistics, then a backfill if this ledger has not had one, then statistics again.
#[derive(Default)]
pub struct UsagePage {
    pub statistics: UsageStatistics,
    pub loading: bool,
}

/// Provider and workspace lookups a backfill needs. A model id is almost always unique in one document; one it cannot
/// place stays empty and the card calls it an unknown provider, rather than inventing one.
pub fn backfill_index(doc: &AppDocument) -> (HashMap<String, String>, HashMap<String, (String, String)>) {
    let mut workspace_by_conversation = HashMap::new();
    for workspace in &doc.workspaces {
        for conversation in &workspace.conversations {
            workspace_by_conversation.insert(conversation.id.clone(), workspace.id.clone());
        }
    }
    let mut provider_by_model: HashMap<String, (String, String)> = HashMap::new();
    for provider in &doc.assets.api_providers {
        for model in &provider.models {
            provider_by_model.entry(model.id.clone()).or_insert_with(|| (provider.id.clone(), provider.name.clone()));
        }
    }
    (workspace_by_conversation, provider_by_model)
}

/// `ensureUsageBackfill`. The marker is the ledger's start rather than a flag, so a rebuilt ledger backfills again.
/// Returns whether anything was pushed, in which case the statistics are worth reading again.
fn ensure_usage_backfill(task: &Task, statistics: &UsageStatistics) -> bool {
    if statistics.ledger_started_at_ms == 0 {
        return false;
    }
    let marker = statistics.ledger_started_at_ms.to_string();
    let started = statistics.ledger_started_at_ms;
    let marker_read = marker.clone();
    let events = task.update(move |app| {
        if app.ui_store().get(USAGE_BACKFILL_STORAGE_KEY) == Some(marker_read.as_str()) {
            return None;
        }
        // Read as the turn table reads itself, which sweeps the rounds that left no record.
        let raw = app.ui_store().get(CONVERSATION_TURNS_STORAGE_KEY).map(str::to_owned);
        let turns: HashMap<String, Vec<StoredTurn>> = load_conversation_turns(raw.as_deref(), now_ms())
            .into_iter()
            .map(|(id, rows)| (id, rows.into_iter().map(|t| StoredTurn { id: t.id, model_id: t.model_id, started_at: t.started_at, ended_at: t.ended_at, usage: t.usage }).collect()))
            .collect();
        let (workspaces, providers) = backfill_index(&app.doc);
        Some(usage_backfill_events(&turns, &workspaces, &providers).into_iter().filter(|e| e.occurred_at_ms < started).collect::<Vec<_>>())
    });
    let Some(events) = events else { return false };
    for chunk in events.chunks(BACKFILL_CHUNK) {
        if let Err(e) = task.invoke::<i64>("backfill_token_usage", json!({ "events": chunk })) {
            eprintln!("{}: {e}", t("读取使用统计失败", "Failed to read usage statistics"));
        }
    }
    // A marker that does not get written only means the next start pushes again, which the idempotent key makes a no-op.
    task.post(move |app| app.ui_store().set(USAGE_BACKFILL_STORAGE_KEY, marker));
    !events.is_empty()
}

fn start_usage_read(app: &mut App) {
    app.local::<UsagePage>().loading = true;
    app.spawn(|task| {
        let read: Result<UsageStatistics, String> = task.invoke("token_usage_statistics", json!({}));
        let statistics = match read {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {e}", t("读取使用统计失败", "Failed to read usage statistics"));
                task.post(|app| {
                    app.local::<UsagePage>().loading = false;
                    repaint();
                });
                return;
            }
        };
        let first = statistics.clone();
        task.post(move |app| {
            app.local::<UsagePage>().statistics = first;
            repaint();
        });
        let refreshed = if ensure_usage_backfill(&task, &statistics) { task.invoke::<UsageStatistics>("token_usage_statistics", json!({})).ok() } else { None };
        task.post(move |app| {
            let page = app.local::<UsagePage>();
            if let Some(next) = refreshed {
                page.statistics = next;
            }
            page.loading = false;
            repaint();
        });
    });
}

/// `UsageSettings`: reads once per mount, and the document is only read for the backfill.
pub fn usage_settings(app: &mut App) -> E {
    // Lives as long as the page is drawn, so coming back to the page reads again.
    let fresh = use_state("usage-settings-fresh", || true);
    if fresh.get() {
        *fresh.borrow_mut() = false;
        start_usage_read(app);
    }
    let page = app.local::<UsagePage>();
    let (statistics, loading) = (page.statistics.clone(), page.loading);
    usage_stats_card(&statistics, loading)
}
