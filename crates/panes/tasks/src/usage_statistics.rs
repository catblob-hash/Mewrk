//! `lib/usageStatistics.ts`: two sets of UTC hour buckets folded into the local days and hours the card shows. The
//! conversion lives here rather than in SQL because the time zone is the user's: changing it must not rewrite history.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

use crate::util::{now_ms, parse_time};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActivityBucket {
    pub hour_start_ms: i64,
    pub user_messages: i64,
    pub assistant_messages: i64,
    pub sessions: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageBucket {
    pub hour_start_ms: i64,
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub origin: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageStatistics {
    pub generated_at_ms: i64,
    pub ledger_started_at_ms: i64,
    pub activity: Vec<ActivityBucket>,
    pub usage: Vec<UsageBucket>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageBackfillEvent {
    pub id: String,
    pub occurred_at_ms: i64,
    pub origin: String,
    pub conversation_id: String,
    pub workspace_id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageRange {
    All,
    Days30,
    Days7,
}

pub const USAGE_RANGES: [UsageRange; 3] = [UsageRange::All, UsageRange::Days30, UsageRange::Days7];

/// The heatmap always covers the last 26 weeks, whatever the range switch says.
const HEATMAP_WEEKS: i64 = 26;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelUsageRow {
    pub key: String,
    pub provider_name: String,
    pub model_id: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub share: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageBucketSize {
    Day,
    Week,
    Month,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSeriesPoint {
    pub start_ms: i64,
    pub key: String,
    /// One figure per entry of `UsageSummary::models`, which is why bars and legend need no colour map.
    pub by_model: Vec<i64>,
    pub total_tokens: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSeries {
    pub bucket: UsageBucketSize,
    pub points: Vec<UsageSeriesPoint>,
    pub max: i64,
    pub ticks: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeatmapCell {
    pub day: String,
    pub start_ms: i64,
    pub messages: i64,
    pub tokens: i64,
    pub level: u8,
    pub future: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSummary {
    pub sessions: i64,
    pub messages: i64,
    pub requests: i64,
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub active_days: i64,
    pub current_streak: i64,
    pub longest_streak: i64,
    pub peak_hour: Option<u32>,
    pub favorite_model: Option<ModelUsageRow>,
    pub models: Vec<ModelUsageRow>,
    pub series: UsageSeries,
    pub heatmap_weeks: Vec<Vec<HeatmapCell>>,
    pub has_activity: bool,
    pub has_tokens: bool,
}

/// `new Date(y, m, d, h)`: a wall-clock time that a clock change skipped lands after the gap, and one it repeated
/// takes the earlier instant, as JavaScript resolves them.
pub fn resolve_local(naive: NaiveDateTime) -> DateTime<Local> {
    let mut at = naive;
    for _ in 0..=48 {
        match Local.from_local_datetime(&at) {
            chrono::LocalResult::Single(d) => return d,
            chrono::LocalResult::Ambiguous(first, _) => return first,
            chrono::LocalResult::None => at += Duration::minutes(30),
        }
    }
    Local.from_utc_datetime(&naive)
}

fn local_of(ms: i64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms).single().unwrap_or_else(|| Local.timestamp_millis_opt(0).single().expect("epoch"))
}

fn local_day_key(d: &DateTime<Local>) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn start_of_local_day(ms: i64) -> DateTime<Local> {
    resolve_local(local_of(ms).date_naive().and_hms_opt(0, 0, 0).expect("midnight"))
}

fn add_days(d: &DateTime<Local>, days: i64) -> DateTime<Local> {
    resolve_local(d.naive_local() + Duration::days(days))
}

fn monday_index(d: &DateTime<Local>) -> i64 {
    d.weekday().num_days_from_monday() as i64
}

const MAX_DAY_BUCKETS: i64 = 62;
/// A hard cap on bars. A backfill has been seen carrying timestamps from 1970; the cap cuts from the old end.
pub const MAX_SERIES_POINTS: usize = 240;
const DAY_MS: f64 = 86_400_000.0;

fn start_of_bucket(ms: i64, bucket: UsageBucketSize) -> DateTime<Local> {
    let day = start_of_local_day(ms);
    match bucket {
        UsageBucketSize::Day => day,
        UsageBucketSize::Week => add_days(&day, -monday_index(&day)),
        UsageBucketSize::Month => resolve_local(day.naive_local().with_day(1).expect("first of a month")),
    }
}

fn step_back(d: &DateTime<Local>, bucket: UsageBucketSize) -> DateTime<Local> {
    match bucket {
        UsageBucketSize::Day => add_days(d, -1),
        UsageBucketSize::Week => add_days(d, -7),
        UsageBucketSize::Month => {
            let n = d.naive_local();
            let (y, m) = if n.month() == 1 { (n.year() - 1, 12) } else { (n.year(), n.month() - 1) };
            resolve_local(NaiveDate::from_ymd_opt(y, m, 1).expect("a month").and_time(n.time()))
        }
    }
}

/// Rounded, so the hour a daylight-saving day differs by is absorbed.
fn local_days_between(from: i64, to: i64) -> i64 {
    ((start_of_local_day(to).timestamp_millis() - start_of_local_day(from).timestamp_millis()) as f64 / DAY_MS).round() as i64 + 1
}

fn bucket_span(from: i64, now: i64, bucket: UsageBucketSize) -> i64 {
    let (start, end) = (start_of_bucket(from, bucket), start_of_bucket(now, bucket));
    if bucket == UsageBucketSize::Month {
        return (end.year() as i64 - start.year() as i64) * 12 + (end.month() as i64 - start.month() as i64) + 1;
    }
    let days = local_days_between(start.timestamp_millis(), end.timestamp_millis());
    if bucket == UsageBucketSize::Week { ((days - 1) as f64 / 7.0).round() as i64 + 1 } else { days }
}

/// Chosen by real bars rather than days over seven: a span from a Sunday to a Saturday aligns to one more week.
fn pick_bucket_size(from: i64, now: i64) -> UsageBucketSize {
    if bucket_span(from, now, UsageBucketSize::Day) <= MAX_DAY_BUCKETS {
        UsageBucketSize::Day
    } else if bucket_span(from, now, UsageBucketSize::Week) <= MAX_DAY_BUCKETS {
        UsageBucketSize::Week
    } else {
        UsageBucketSize::Month
    }
}

/// The axis top is a readable number on a 1/2/2.5/5 step, taking whichever candidate leaves the least headroom.
pub fn nice_ticks(max: f64, max_intervals: i64) -> (i64, Vec<i64>) {
    if !max.is_finite() || max <= 0.0 {
        return (0, Vec::new());
    }
    let exponent = max.log10().floor() as i32;
    let mut candidates: Vec<(f64, f64, i64)> = Vec::new();
    for power in exponent - 2..=exponent + 1 {
        for multiplier in [1.0, 2.0, 2.5, 5.0] {
            let step = multiplier * 10f64.powi(power);
            // A fractional step would print `7.5`, and tokens are whole.
            if step < 1.0 || step.fract() != 0.0 {
                continue;
            }
            let count = (max / step).ceil() as i64;
            if count >= 1 && count <= max_intervals {
                candidates.push((step, count as f64 * step, count));
            }
        }
    }
    let dense: Vec<(f64, f64, i64)> = candidates.iter().filter(|c| c.2 >= 3).copied().collect();
    let mut pool = if dense.is_empty() { candidates } else { dense };
    pool.sort_by(|l, r| l.1.total_cmp(&r.1).then((l.2 - 4).abs().cmp(&(r.2 - 4).abs())).then(l.2.cmp(&r.2)));
    let Some(best) = pool.first() else {
        return (max.round() as i64, vec![0, max.round() as i64]);
    };
    ((best.1).round() as i64, (0..=best.2).map(|i| (i as f64 * best.0).round() as i64).collect())
}

fn build_series(totals: &HashMap<i64, HashMap<String, i64>>, models: &[ModelUsageRow], from: i64, now: i64, bucket: UsageBucketSize) -> UsageSeries {
    let model_index: HashMap<&str, usize> = models.iter().enumerate().map(|(i, r)| (r.key.as_str(), i)).collect();
    let floor = start_of_bucket(from, bucket).timestamp_millis();
    let mut starts: Vec<DateTime<Local>> = Vec::new();
    // Back from now, so a cut falls on the oldest end. Renormalising each step keeps a skipped midnight's hour from
    // being carried along, which would stop every key matching.
    let mut cursor = start_of_bucket(now, bucket);
    while cursor.timestamp_millis() >= floor && starts.len() < MAX_SERIES_POINTS {
        starts.push(cursor);
        cursor = start_of_bucket(step_back(&cursor, bucket).timestamp_millis(), bucket);
    }
    starts.reverse();
    let index_by_start: HashMap<i64, usize> = starts.iter().enumerate().map(|(i, s)| (s.timestamp_millis(), i)).collect();
    let oldest = starts.first().map_or(0, DateTime::timestamp_millis);
    let mut columns: Vec<HashMap<&str, i64>> = vec![HashMap::new(); starts.len()];
    for (start, by_model) in totals {
        // A bucket outside the window folds into the nearest bar: bars must add up to the legend.
        let index = index_by_start.get(start).copied().unwrap_or(if *start < oldest { 0 } else { starts.len().saturating_sub(1) });
        let Some(column) = columns.get_mut(index) else { continue };
        for (key, tokens) in by_model {
            *column.entry(key.as_str()).or_default() += tokens;
        }
    }
    let mut peak = 0;
    let points = starts
        .iter()
        .enumerate()
        .map(|(i, start)| {
            let mut by_model = vec![0; models.len()];
            let mut total_tokens = 0;
            for (key, tokens) in &columns[i] {
                if let Some(&slot) = model_index.get(key) {
                    by_model[slot] += tokens;
                    total_tokens += tokens;
                }
            }
            peak = peak.max(total_tokens);
            UsageSeriesPoint { start_ms: start.timestamp_millis(), key: local_day_key(start), by_model, total_tokens }
        })
        .collect();
    let (max, ticks) = nice_ticks(peak as f64, 5);
    UsageSeries { bucket, points, max, ticks }
}

fn range_start_ms(range: UsageRange, now: i64) -> i64 {
    // "The last 30 days" includes today.
    let days = match range {
        UsageRange::All => return i64::MIN,
        UsageRange::Days30 => 30,
        UsageRange::Days7 => 7,
    };
    add_days(&start_of_local_day(now), -(days - 1)).timestamp_millis()
}

#[derive(Default, Clone, Copy)]
struct DayTotals {
    messages: i64,
    tokens: i64,
    requests: i64,
}

fn day_start(day: &str) -> Option<DateTime<Local>> {
    NaiveDate::parse_from_str(day, "%Y-%m-%d").ok().map(|d| resolve_local(d.and_hms_opt(0, 0, 0).expect("midnight")))
}

pub fn summarize_usage(statistics: &UsageStatistics, range: UsageRange, now: Option<i64>) -> UsageSummary {
    let now = now.unwrap_or(if statistics.generated_at_ms != 0 { statistics.generated_at_ms } else { now_ms() });
    let from = range_start_ms(range, now);
    let mut day_totals: HashMap<String, DayTotals> = HashMap::new();
    let mut hour_messages = [0i64; 24];
    let (mut sessions, mut messages) = (0, 0);
    for b in &statistics.activity {
        let count = b.user_messages + b.assistant_messages;
        let at = local_of(b.hour_start_ms);
        // The heatmap looks at all of history; only the totals look at the range.
        day_totals.entry(local_day_key(&at)).or_default().messages += count;
        if b.hour_start_ms < from {
            continue;
        }
        sessions += b.sessions;
        messages += count;
        hour_messages[at.hour() as usize] += count;
    }
    let (mut requests, mut total_tokens, mut input_tokens, mut cached_input_tokens, mut output_tokens) = (0, 0, 0, 0, 0);
    let mut model_rows: Vec<ModelUsageRow> = Vec::new();
    let mut row_at: HashMap<String, usize> = HashMap::new();
    // "All" opens at the first token rather than the first message: there is nothing to draw before it.
    let earliest = statistics.usage.iter().map(|b| b.hour_start_ms).min();
    let series_from = (if range == UsageRange::All { earliest.unwrap_or(now) } else { from }).min(now);
    let series_bucket = pick_bucket_size(series_from, now);
    let mut series_totals: HashMap<i64, HashMap<String, i64>> = HashMap::new();
    for b in &statistics.usage {
        let totals = day_totals.entry(local_day_key(&local_of(b.hour_start_ms))).or_default();
        totals.tokens += b.total_tokens;
        totals.requests += b.requests;
        if b.hour_start_ms < from {
            continue;
        }
        requests += b.requests;
        total_tokens += b.total_tokens;
        input_tokens += b.input_tokens;
        cached_input_tokens += b.cached_input_tokens;
        output_tokens += b.output_tokens;
        let key = format!("{}\u{0}{}", b.provider_id, b.model_id);
        let at = *row_at.entry(key.clone()).or_insert_with(|| {
            model_rows.push(ModelUsageRow { key: key.clone(), provider_name: b.provider_name.clone(), model_id: b.model_id.clone(), ..ModelUsageRow::default() });
            model_rows.len() - 1
        });
        let row = &mut model_rows[at];
        // A backfilled row may have no provider name; whichever side has one first supplies it.
        if row.provider_name.is_empty() && !b.provider_name.is_empty() {
            row.provider_name = b.provider_name.clone();
        }
        row.requests += b.requests;
        row.input_tokens += b.input_tokens;
        row.cached_input_tokens += b.cached_input_tokens;
        row.output_tokens += b.output_tokens;
        row.total_tokens += b.total_tokens;
        if b.total_tokens > 0 {
            let start = start_of_bucket(b.hour_start_ms, series_bucket).timestamp_millis();
            *series_totals.entry(start).or_default().entry(key).or_default() += b.total_tokens;
        }
    }
    for row in &mut model_rows {
        row.share = if total_tokens != 0 { row.total_tokens as f64 / total_tokens as f64 } else { 0.0 };
    }
    let mut models = model_rows;
    models.sort_by(|l, r| r.total_tokens.cmp(&l.total_tokens).then(r.requests.cmp(&l.requests)));

    let active: BTreeSet<String> = day_totals.iter().filter(|(_, t)| t.messages > 0 || t.requests > 0).map(|(d, _)| d.clone()).collect();
    let active_days = active.iter().filter(|d| from == i64::MIN || day_start(d).is_some_and(|s| s.timestamp_millis() >= from)).count() as i64;
    let (current_streak, longest_streak) = streaks(&active, now);
    let peak_hour = (messages > 0).then(|| (0..24).fold(0, |best, h| if hour_messages[h] > hour_messages[best] { h } else { best }) as u32);
    UsageSummary {
        sessions,
        messages,
        requests,
        total_tokens,
        input_tokens,
        cached_input_tokens,
        output_tokens,
        active_days,
        current_streak,
        longest_streak,
        peak_hour,
        favorite_model: models.first().cloned(),
        series: build_series(&series_totals, &models, series_from, now, series_bucket),
        models,
        heatmap_weeks: heatmap(&day_totals, now),
        has_activity: !active.is_empty(),
        has_tokens: !statistics.usage.is_empty(),
    }
}

/// Counted back from today, or from yesterday while today is still quiet: a streak does not break at midnight.
fn streaks(active: &BTreeSet<String>, now: i64) -> (i64, i64) {
    if active.is_empty() {
        return (0, 0);
    }
    let today = start_of_local_day(now);
    let mut cursor = if active.contains(&local_day_key(&today)) { today } else { add_days(&today, -1) };
    let mut current = 0;
    while active.contains(&local_day_key(&cursor)) {
        current += 1;
        cursor = add_days(&cursor, -1);
    }
    let (mut longest, mut run) = (0, 0);
    let mut previous: Option<DateTime<Local>> = None;
    for day in active {
        let date = day_start(day);
        run = if previous.is_some() && previous.map(|p| add_days(&p, 1).timestamp_millis()) == date.map(|d| d.timestamp_millis()) { run + 1 } else { 1 };
        longest = i64::max(longest, run);
        previous = date;
    }
    (current, longest)
}

fn heatmap(day_totals: &HashMap<String, DayTotals>, now: i64) -> Vec<Vec<HeatmapCell>> {
    let today = start_of_local_day(now);
    let this_monday = add_days(&today, -monday_index(&today));
    let first_monday = add_days(&this_monday, -(HEATMAP_WEEKS - 1) * 7);
    let weight = |t: &DayTotals| if t.tokens != 0 { t.tokens } else { t.messages };
    let mut non_zero: Vec<i64> = day_totals.values().map(weight).filter(|v| *v > 0).collect();
    non_zero.sort_unstable();
    let busiest = non_zero.last().copied().unwrap_or(0);
    // Quantiles over the ranks below the maximum: with few days `n * 0.75` lands on the maximum itself, and then no
    // cell ever reaches the darkest level.
    let quartile = |f: f64| if non_zero.is_empty() { 0 } else { non_zero[((non_zero.len() - 1) as f64 * f).floor() as usize] };
    let thresholds = [quartile(0.25), quartile(0.5), quartile(0.75)];
    (0..HEATMAP_WEEKS)
        .map(|week| {
            (0..7)
                .map(|weekday| {
                    let date = add_days(&first_monday, week * 7 + weekday);
                    let day = local_day_key(&date);
                    let totals = day_totals.get(&day);
                    let w = totals.map_or(0, weight);
                    let level = if w <= 0 {
                        0
                    } else if w >= busiest {
                        4
                    } else if w <= thresholds[0] {
                        1
                    } else if w <= thresholds[1] {
                        2
                    } else if w <= thresholds[2] {
                        3
                    } else {
                        4
                    };
                    HeatmapCell { day, start_ms: date.timestamp_millis(), messages: totals.map_or(0, |t| t.messages), tokens: totals.map_or(0, |t| t.tokens), level, future: date.timestamp_millis() > today.timestamp_millis() }
                })
                .collect()
        })
        .collect()
}

/// 115_400_000 → `115.4M`. One decimal kept: this tile is "how much did I spend", and rounding it off hides millions.
pub fn format_compact_count(value: i64) -> String {
    if value <= 0 {
        return "0".to_owned();
    }
    if value < 1000 {
        return value.to_string();
    }
    for (scale, suffix) in [(1_000_000_000i64, "B"), (1_000_000, "M"), (1_000, "K")] {
        if value >= scale {
            let text = format!("{:.1}", value as f64 / scale as f64);
            return format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text));
        }
    }
    value.to_string()
}

pub fn format_exact_count(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if value < 0 { format!("-{out}") } else { out }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurrUnit {
    Minute,
    Hour,
    Day,
    Month,
}

/// 100k tokens is a cat purring for six minutes, carried up to the largest unit that still reads as a number.
pub fn purr_duration(total_tokens: i64) -> Option<(f64, PurrUnit)> {
    if total_tokens <= 0 {
        return None;
    }
    let minutes = total_tokens as f64 * (6.0 / 100_000.0);
    let units = [(PurrUnit::Minute, 1.0), (PurrUnit::Hour, 60.0), (PurrUnit::Day, 1440.0), (PurrUnit::Month, 43_200.0)];
    let chosen = units.iter().rev().find(|u| minutes >= u.1).unwrap_or(&units[0]);
    Some((minutes / chosen.1, chosen.0))
}

/// One round as `lib/conversationTurns.ts` stored it, reduced to what a backfill reads.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StoredTurn {
    pub id: String,
    pub model_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub usage: mewrk_client::model::ModelUsage,
}

/// Rounds before the ledger exist only in the renderer's own table, so only this side can push them. `turn.usage`
/// already absorbed its subagents' usage, so their records must not be pushed as well.
pub fn usage_backfill_events(turns: &HashMap<String, Vec<StoredTurn>>, workspace_by_conversation: &HashMap<String, String>, provider_by_model: &HashMap<String, (String, String)>) -> Vec<UsageBackfillEvent> {
    let mut events = Vec::new();
    for (conversation_id, rows) in turns {
        for turn in rows {
            let (input, output, cached) = (turn.usage.input_tokens.unwrap_or(0) as i64, turn.usage.output_tokens.unwrap_or(0) as i64, turn.usage.cached_input_tokens.unwrap_or(0) as i64);
            let total = turn.usage.total_tokens.map_or(input + output, |t| t as i64);
            if input == 0 && output == 0 && cached == 0 && total == 0 {
                continue;
            }
            let Some(occurred_at_ms) = parse_time(turn.ended_at.as_deref().unwrap_or(&turn.started_at)) else { continue };
            let provider = provider_by_model.get(&turn.model_id);
            events.push(UsageBackfillEvent {
                id: format!("turn:{conversation_id}:{}", turn.id),
                occurred_at_ms,
                origin: "conversation".to_owned(),
                conversation_id: conversation_id.clone(),
                workspace_id: workspace_by_conversation.get(conversation_id).cloned().unwrap_or_default(),
                provider_id: provider.map_or(String::new(), |p| p.0.clone()),
                provider_name: provider.map_or(String::new(), |p| p.1.clone()),
                model_id: turn.model_id.clone(),
                requests: 1,
                input_tokens: input,
                cached_input_tokens: cached,
                output_tokens: output,
                total_tokens: total,
            });
        }
    }
    events
}

/// How one push is bounded: the turn table is as long as the user's history.
pub const BACKFILL_CHUNK: usize = 500;
pub const USAGE_BACKFILL_STORAGE_KEY: &str = "mewrk.usage-backfilled.v1";
