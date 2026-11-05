//! `lib/usageStatistics.ts`: the ledger's two sets of UTC hour buckets folded into local days and hours.
//!
//! The conversion happens here rather than in SQL because the time zone is a property of the user: changing it should
//! not recompute history.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityBucket {
    pub hour_start_ms: i64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub sessions: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub hour_start_ms: i64,
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    #[serde(default)]
    pub origin: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatistics {
    pub generated_at_ms: i64,
    pub ledger_started_at_ms: i64,
    pub activity: Vec<ActivityBucket>,
    pub usage: Vec<UsageBucket>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UsageRange {
    All,
    D30,
    D7,
}

pub const USAGE_RANGES: [UsageRange; 3] = [UsageRange::All, UsageRange::D30, UsageRange::D7];

const HEATMAP_WEEKS: i64 = 26;
const MAX_DAY_BUCKETS: i64 = 62;
pub const MAX_SERIES_POINTS: usize = 240;

#[derive(Clone, Debug, PartialEq)]
pub struct ModelUsageRow {
    pub key: String,
    pub provider_name: String,
    pub model_id: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Share of the range's tokens, 0 to 1.
    pub share: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketSize {
    Day,
    Week,
    Month,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SeriesPoint {
    pub start_ms: i64,
    pub key: String,
    /// Aligned index for index with `UsageSummary::models`.
    pub by_model: Vec<u64>,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSeries {
    pub bucket: BucketSize,
    pub points: Vec<SeriesPoint>,
    pub max: u64,
    pub ticks: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HeatmapCell {
    pub day: String,
    pub start_ms: i64,
    pub messages: u64,
    pub tokens: u64,
    pub level: u8,
    pub future: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSummary {
    pub sessions: u64,
    pub messages: u64,
    pub requests: u64,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub active_days: u64,
    pub current_streak: u64,
    pub longest_streak: u64,
    pub peak_hour: Option<u32>,
    pub favorite_model: Option<ModelUsageRow>,
    pub models: Vec<ModelUsageRow>,
    pub series: UsageSeries,
    pub heatmap_weeks: Vec<Vec<HeatmapCell>>,
    pub has_activity: bool,
    pub has_tokens: bool,
}

fn local(ms: i64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms).single().unwrap_or_else(|| Local.timestamp_millis_opt(0).single().expect("the epoch exists"))
}

fn local_date(ms: i64) -> NaiveDate {
    local(ms).date_naive()
}

/// The first moment of a local day. A zone whose midnight doesn't exist that day has it at the next valid instant.
fn start_of_date(date: NaiveDate) -> i64 {
    let mut at = date.and_hms_opt(0, 0, 0).expect("midnight is a time");
    for _ in 0..4 {
        if let Some(found) = Local.from_local_datetime(&at).earliest() {
            return found.timestamp_millis();
        }
        at += Duration::hours(1);
    }
    at.and_utc().timestamp_millis()
}

fn day_key(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// Monday is 0.
fn monday_index(date: NaiveDate) -> i64 {
    date.weekday().num_days_from_monday() as i64
}

fn start_of_bucket(date: NaiveDate, bucket: BucketSize) -> NaiveDate {
    match bucket {
        BucketSize::Day => date,
        BucketSize::Week => date - Duration::days(monday_index(date)),
        BucketSize::Month => date.with_day(1).expect("every month has a first"),
    }
}

fn step_back(date: NaiveDate, bucket: BucketSize) -> NaiveDate {
    match bucket {
        BucketSize::Day => date - Duration::days(1),
        BucketSize::Week => date - Duration::days(7),
        BucketSize::Month => {
            let (y, m) = if date.month() == 1 { (date.year() - 1, 12) } else { (date.year(), date.month() - 1) };
            NaiveDate::from_ymd_opt(y, m, 1).expect("the first of a month exists")
        }
    }
}

fn bucket_span(from: NaiveDate, now: NaiveDate, bucket: BucketSize) -> i64 {
    let (start, end) = (start_of_bucket(from, bucket), start_of_bucket(now, bucket));
    match bucket {
        BucketSize::Month => (end.year() as i64 - start.year() as i64) * 12 + (end.month() as i64 - start.month() as i64) + 1,
        BucketSize::Week => (end - start).num_days() / 7 + 1,
        BucketSize::Day => (end - start).num_days() + 1,
    }
}

/// By the real number of bars rather than days: a 434-day range from a Sunday to a Saturday aligns to 63 weeks.
fn pick_bucket_size(from: NaiveDate, now: NaiveDate) -> BucketSize {
    if bucket_span(from, now, BucketSize::Day) <= MAX_DAY_BUCKETS {
        BucketSize::Day
    } else if bucket_span(from, now, BucketSize::Week) <= MAX_DAY_BUCKETS {
        BucketSize::Week
    } else {
        BucketSize::Month
    }
}

/// A good-looking axis: ticks on whole multiples of 1, 2, 2.5 and 5, choosing the candidate with the least headroom
/// rather than aiming for four cells, which would leave half the plot empty just past a step.
pub fn nice_ticks(max: f64, max_intervals: u32) -> (u64, Vec<u64>) {
    if !max.is_finite() || max <= 0.0 {
        return (0, Vec::new());
    }
    let exponent = max.log10().floor() as i32;
    let mut candidates: Vec<(u64, u64, u32)> = Vec::new();
    for power in exponent - 2..=exponent + 1 {
        for multiplier in [1.0, 2.0, 2.5, 5.0] {
            let step = multiplier * 10f64.powi(power);
            // A step that is not whole would write ticks like `7.5`, and tokens are whole.
            if step < 1.0 || step.fract() != 0.0 {
                continue;
            }
            let count = (max / step).ceil() as u32;
            if count >= 1 && count <= max_intervals {
                candidates.push((step as u64, (count as f64 * step) as u64, count));
            }
        }
    }
    let dense: Vec<_> = candidates.iter().copied().filter(|c| c.2 >= 3).collect();
    let pool = if dense.is_empty() { candidates } else { dense };
    let distance = |c: u32| (c as i64 - 4).abs();
    let Some(best) = pool.into_iter().min_by_key(|c| (c.1, distance(c.2), c.2)) else {
        let m = max.round() as u64;
        return (m, vec![0, m]);
    };
    (best.1, (0..=best.2 as u64).map(|i| i * best.0).collect())
}

fn range_start(range: UsageRange, now: NaiveDate) -> Option<NaiveDate> {
    match range {
        UsageRange::All => None,
        // "The last 30 days" includes today.
        UsageRange::D30 => Some(now - Duration::days(29)),
        UsageRange::D7 => Some(now - Duration::days(6)),
    }
}

fn build_series(totals: &HashMap<NaiveDate, HashMap<String, u64>>, models: &[ModelUsageRow], from: NaiveDate, now: NaiveDate, bucket: BucketSize) -> UsageSeries {
    let floor = start_of_bucket(from, bucket);
    // From now backwards: truncation happens at the oldest end, and the most recent bars are always there.
    let mut starts: Vec<NaiveDate> = Vec::new();
    let mut cursor = start_of_bucket(now, bucket);
    while cursor >= floor && starts.len() < MAX_SERIES_POINTS {
        starts.push(cursor);
        cursor = step_back(cursor, bucket);
    }
    starts.reverse();
    let index_of: HashMap<NaiveDate, usize> = starts.iter().enumerate().map(|(i, d)| (*d, i)).collect();
    let oldest = starts.first().copied();
    let mut columns: Vec<HashMap<String, u64>> = vec![HashMap::new(); starts.len()];
    for (start, tokens_by_model) in totals {
        // A bucket outside the window folds into the nearest bar rather than disappearing: bars must add up to the legend.
        let index = index_of.get(start).copied().unwrap_or(if oldest.is_some_and(|o| *start < o) { 0 } else { starts.len().saturating_sub(1) });
        let Some(column) = columns.get_mut(index) else { continue };
        for (key, tokens) in tokens_by_model {
            *column.entry(key.clone()).or_default() += tokens;
        }
    }
    let model_index: HashMap<&str, usize> = models.iter().enumerate().map(|(i, r)| (r.key.as_str(), i)).collect();
    let mut peak = 0;
    let points = starts
        .iter()
        .zip(columns.iter())
        .map(|(start, column)| {
            let mut by_model = vec![0; models.len()];
            let mut total = 0;
            for (key, tokens) in column {
                if let Some(&slot) = model_index.get(key.as_str()) {
                    by_model[slot] += tokens;
                    total += tokens;
                }
            }
            peak = peak.max(total);
            SeriesPoint { start_ms: start_of_date(*start), key: day_key(*start), by_model, total_tokens: total }
        })
        .collect();
    let (max, ticks) = nice_ticks(peak as f64, 5);
    UsageSeries { bucket, points, max, ticks }
}

#[derive(Default, Clone, Copy)]
struct DayTotals {
    messages: u64,
    tokens: u64,
    requests: u64,
}

pub fn summarize_usage(statistics: &UsageStatistics, range: UsageRange, now_ms: i64) -> UsageSummary {
    let now = local_date(now_ms);
    let from = range_start(range, now);
    let from_ms = from.map_or(i64::MIN, start_of_date);
    let mut day_totals: HashMap<NaiveDate, DayTotals> = HashMap::new();
    let mut hour_messages = [0u64; 24];
    let (mut sessions, mut messages) = (0, 0);
    for bucket in &statistics.activity {
        let bucket_messages = bucket.user_messages + bucket.assistant_messages;
        // The heatmap always looks at all of history and the metrics at the range, so every cell goes in the day table.
        day_totals.entry(local_date(bucket.hour_start_ms)).or_default().messages += bucket_messages;
        if bucket.hour_start_ms < from_ms {
            continue;
        }
        sessions += bucket.sessions;
        messages += bucket_messages;
        hour_messages[local(bucket.hour_start_ms).hour() as usize] += bucket_messages;
    }

    let (mut requests, mut total_tokens, mut input_tokens, mut cached_input_tokens, mut output_tokens) = (0, 0, 0, 0, 0);
    let mut model_rows: Vec<ModelUsageRow> = Vec::new();
    // "All" starts its chart at the earliest usage, not the earliest message: before the first record there is nothing to draw.
    let earliest_usage = statistics.usage.iter().map(|b| b.hour_start_ms).min();
    let series_from_date = match from {
        Some(d) => d.min(now),
        None => earliest_usage.map_or(now, local_date).min(now),
    };
    let series_bucket = pick_bucket_size(series_from_date, now);
    let mut series_totals: HashMap<NaiveDate, HashMap<String, u64>> = HashMap::new();
    for bucket in &statistics.usage {
        let totals = day_totals.entry(local_date(bucket.hour_start_ms)).or_default();
        totals.tokens += bucket.total_tokens;
        totals.requests += bucket.requests;
        if bucket.hour_start_ms < from_ms {
            continue;
        }
        requests += bucket.requests;
        total_tokens += bucket.total_tokens;
        input_tokens += bucket.input_tokens;
        cached_input_tokens += bucket.cached_input_tokens;
        output_tokens += bucket.output_tokens;
        let key = format!("{}\u{0}{}", bucket.provider_id, bucket.model_id);
        let at = match model_rows.iter().position(|r| r.key == key) {
            Some(i) => i,
            None => {
                model_rows.push(ModelUsageRow { key: key.clone(), provider_name: bucket.provider_name.clone(), model_id: bucket.model_id.clone(), requests: 0, input_tokens: 0, cached_input_tokens: 0, output_tokens: 0, total_tokens: 0, share: 0.0 });
                model_rows.len() - 1
            }
        };
        let row = &mut model_rows[at];
        // A backfilled model id may have no provider name; whichever side has one first names it.
        if row.provider_name.is_empty() && !bucket.provider_name.is_empty() {
            row.provider_name = bucket.provider_name.clone();
        }
        row.requests += bucket.requests;
        row.input_tokens += bucket.input_tokens;
        row.cached_input_tokens += bucket.cached_input_tokens;
        row.output_tokens += bucket.output_tokens;
        row.total_tokens += bucket.total_tokens;
        if bucket.total_tokens > 0 {
            let start = start_of_bucket(local_date(bucket.hour_start_ms), series_bucket);
            *series_totals.entry(start).or_default().entry(key).or_default() += bucket.total_tokens;
        }
    }
    for row in &mut model_rows {
        row.share = if total_tokens > 0 { row.total_tokens as f64 / total_tokens as f64 } else { 0.0 };
    }
    let mut models = model_rows;
    models.sort_by(|l, r| r.total_tokens.cmp(&l.total_tokens).then(r.requests.cmp(&l.requests)));

    let active_days: BTreeSet<NaiveDate> = day_totals.iter().filter(|(_, t)| t.messages > 0 || t.requests > 0).map(|(d, _)| *d).collect();
    let range_active = active_days.iter().filter(|d| from.is_none_or(|f| **d >= f)).count() as u64;
    let (current_streak, longest_streak) = streaks(&active_days, now);
    let peak_hour = (messages > 0).then(|| {
        let mut best = 0;
        for (hour, count) in hour_messages.iter().enumerate() {
            if *count > hour_messages[best] {
                best = hour;
            }
        }
        best as u32
    });
    let series = build_series(&series_totals, &models, series_from_date, now, series_bucket);
    UsageSummary {
        sessions,
        messages,
        requests,
        total_tokens,
        input_tokens,
        cached_input_tokens,
        output_tokens,
        active_days: range_active,
        current_streak,
        longest_streak,
        peak_hour,
        favorite_model: models.first().cloned(),
        models,
        series,
        heatmap_weeks: heatmap(&day_totals, now),
        has_activity: !active_days.is_empty(),
        has_tokens: !statistics.usage.is_empty(),
    }
}

/// The current streak counts back from today; with nothing yet today it starts from yesterday, or midnight would show
/// every run as broken when today simply hasn't begun.
fn streaks(active: &BTreeSet<NaiveDate>, today: NaiveDate) -> (u64, u64) {
    if active.is_empty() {
        return (0, 0);
    }
    let mut cursor = if active.contains(&today) { today } else { today - Duration::days(1) };
    let mut current = 0;
    while active.contains(&cursor) {
        current += 1;
        cursor -= Duration::days(1);
    }
    let (mut longest, mut run, mut previous) = (0, 0, None::<NaiveDate>);
    for day in active {
        run = if previous.is_some_and(|p| p + Duration::days(1) == *day) { run + 1 } else { 1 };
        longest = longest.max(run);
        previous = Some(*day);
    }
    (current, longest)
}

fn heatmap(day_totals: &HashMap<NaiveDate, DayTotals>, today: NaiveDate) -> Vec<Vec<HeatmapCell>> {
    let this_monday = today - Duration::days(monday_index(today));
    let first_monday = this_monday - Duration::days((HEATMAP_WEEKS - 1) * 7);
    let weight = |t: &DayTotals| if t.tokens != 0 { t.tokens } else { t.messages };
    let mut non_zero: Vec<u64> = day_totals.values().map(weight).filter(|v| *v > 0).collect();
    non_zero.sort_unstable();
    let busiest = non_zero.last().copied().unwrap_or(0);
    // The quantiles are taken over the ranks below the maximum, and the maximum goes straight to the top level: with few
    // days `floor(n * 0.75)` lands on the maximum itself, and no cell would ever reach level 4.
    let quartile = |fraction: f64| if non_zero.is_empty() { 0 } else { non_zero[((non_zero.len() - 1) as f64 * fraction).floor() as usize] };
    let thresholds = [quartile(0.25), quartile(0.5), quartile(0.75)];
    (0..HEATMAP_WEEKS)
        .map(|week| {
            (0..7)
                .map(|weekday| {
                    let date = first_monday + Duration::days(week * 7 + weekday);
                    let totals = day_totals.get(&date);
                    let w = totals.map_or(0, weight);
                    let level = if w == 0 {
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
                    HeatmapCell { day: day_key(date), start_ms: start_of_date(date), messages: totals.map_or(0, |t| t.messages), tokens: totals.map_or(0, |t| t.tokens), level, future: date > today }
                })
                .collect()
        })
        .collect()
}

/// `115_400_000` → `115.4M`. One decimal always: this cell is what was spent, and rounding would erase four million tokens.
pub fn format_compact_count(value: u64) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    if value < 1000 {
        return value.to_string();
    }
    for (scale, suffix) in [(1_000_000_000u64, "B"), (1_000_000, "M"), (1_000, "K")] {
        if value >= scale {
            let rounded = (value as f64 / scale as f64 * 10.0).round() / 10.0;
            let text = format!("{rounded:.1}");
            return format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text));
        }
    }
    value.to_string()
}

/// `1234567` → `1,234,567`.
pub fn format_exact_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PurrUnit {
    Minute,
    Hour,
    Day,
    Month,
}

/// 100k tokens is a cat purring for six minutes, in the largest unit whose value is at least 1.
pub fn purr_duration(total_tokens: u64) -> Option<(f64, PurrUnit)> {
    if total_tokens == 0 {
        return None;
    }
    let minutes = total_tokens as f64 * 6.0 / 100_000.0;
    let mut chosen = (1.0, PurrUnit::Minute);
    for (m, unit) in [(1.0, PurrUnit::Minute), (60.0, PurrUnit::Hour), (1440.0, PurrUnit::Day), (43200.0, PurrUnit::Month)] {
        if minutes >= m {
            chosen = (m, unit);
        }
    }
    Some((minutes / chosen.0, chosen.1))
}

/// One decimal: rounding to whole percents lines up a column of "0%" that hides the order.
pub fn format_share(share: f64) -> String {
    let percent = if share.is_finite() { share.max(0.0) * 100.0 } else { 0.0 };
    format!("{percent:.1}%")
}

pub fn format_hour12(hour: u32) -> String {
    let suffix = if hour < 12 { "AM" } else { "PM" };
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!("{twelve} {suffix}")
}

/// `Intl.DateTimeFormat` for an axis label, in the two languages the app has.
pub fn bucket_label(start_ms: i64, bucket: BucketSize, chinese: bool) -> String {
    let date = local_date(start_ms);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS[date.month0() as usize];
    match (bucket, chinese) {
        (BucketSize::Month, true) => format!("{}年{}月", date.year(), date.month()),
        (BucketSize::Month, false) => format!("{month} {}", date.year()),
        (_, true) => format!("{}月{}日", date.month(), date.day()),
        (_, false) => format!("{month} {}", date.day()),
    }
}

/// A weekly bar's title spans both ends: the Monday alone never reads as seven days.
pub fn bucket_title(start_ms: i64, bucket: BucketSize, chinese: bool) -> String {
    if bucket != BucketSize::Week {
        return bucket_label(start_ms, bucket, chinese);
    }
    let end = local_date(start_ms) + Duration::days(6);
    format!("{} – {}", bucket_label(start_ms, BucketSize::Day, chinese), bucket_label(start_of_date(end), BucketSize::Day, chinese))
}

/// `formatDate` of the Updates page: the ISO date in the reader's language.
pub fn format_iso_date(iso: &str, chinese: bool) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(iso) else { return iso.to_owned() };
    let date = parsed.with_timezone(&Local).date_naive();
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    if chinese { format!("{}年{}月{}日", date.year(), date.month(), date.day()) } else { format!("{} {}, {}", MONTHS[date.month0() as usize], date.day(), date.year()) }
}

pub fn format_iso_datetime(iso: &str, chinese: bool) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(iso) else { return iso.to_owned() };
    let at = parsed.with_timezone(&Local);
    format!("{} {:02}:{:02}", format_iso_date(iso, chinese), at.hour(), at.minute())
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32) -> i64 {
        Local.with_ymd_and_hms(y, m, d, h, 0, 0).earliest().unwrap().timestamp_millis()
    }

    fn usage(ms: i64, model: &str, tokens: u64) -> UsageBucket {
        UsageBucket { hour_start_ms: ms, provider_id: "p".into(), provider_name: "P".into(), model_id: model.into(), origin: "conversation".into(), requests: 1, input_tokens: tokens / 2, cached_input_tokens: 0, output_tokens: tokens / 2, total_tokens: tokens }
    }

    fn activity(ms: i64, messages: u64) -> ActivityBucket {
        ActivityBucket { hour_start_ms: ms, user_messages: messages, assistant_messages: 0, sessions: 1 }
    }

    // usageStatistics.test.ts
    #[test]
    fn ranges_only_narrow_the_metrics_and_the_heatmap_keeps_all_of_history() {
        let now = at(2026, 6, 15, 12);
        let stats = UsageStatistics { generated_at_ms: now, ledger_started_at_ms: 0, activity: vec![activity(at(2026, 6, 14, 9), 3), activity(at(2026, 1, 2, 9), 5)], usage: vec![usage(at(2026, 6, 14, 9), "a", 100), usage(at(2026, 1, 2, 9), "b", 900)] };
        let all = summarize_usage(&stats, UsageRange::All, now);
        assert_eq!((all.messages, all.total_tokens, all.models.len()), (8, 1000, 2));
        let week = summarize_usage(&stats, UsageRange::D7, now);
        assert_eq!((week.messages, week.total_tokens, week.models.len()), (3, 100, 1));
        assert_eq!(week.active_days, 1);
        assert_eq!(all.heatmap_weeks.len(), 26);
        assert_eq!(all.heatmap_weeks[0].len(), 7);
        assert_eq!(week.heatmap_weeks, summarize_usage(&stats, UsageRange::D30, now).heatmap_weeks);
    }

    #[test]
    fn the_favourite_model_has_the_most_tokens_and_shares_add_up() {
        let now = at(2026, 6, 15, 12);
        let stats = UsageStatistics { generated_at_ms: now, usage: vec![usage(now, "small", 25), usage(now, "big", 75)], ..Default::default() };
        let s = summarize_usage(&stats, UsageRange::All, now);
        assert_eq!(s.favorite_model.unwrap().model_id, "big");
        assert!((s.models[0].share - 0.75).abs() < 1e-9);
        assert!((s.models[1].share - 0.25).abs() < 1e-9);
    }

    #[test]
    fn streaks_forgive_a_today_that_has_not_started() {
        let now = at(2026, 6, 15, 12);
        let stats = UsageStatistics { generated_at_ms: now, activity: vec![activity(at(2026, 6, 14, 9), 1), activity(at(2026, 6, 13, 9), 1), activity(at(2026, 6, 10, 9), 1)], ..Default::default() };
        let s = summarize_usage(&stats, UsageRange::All, now);
        assert_eq!((s.current_streak, s.longest_streak), (2, 2));
        let later = summarize_usage(&stats, UsageRange::All, at(2026, 6, 17, 12));
        assert_eq!(later.current_streak, 0);
    }

    #[test]
    fn the_peak_hour_is_the_busiest_local_hour_and_none_without_messages() {
        let now = at(2026, 6, 15, 12);
        let stats = UsageStatistics { generated_at_ms: now, activity: vec![activity(at(2026, 6, 14, 9), 1), activity(at(2026, 6, 14, 17), 4)], ..Default::default() };
        assert_eq!(summarize_usage(&stats, UsageRange::All, now).peak_hour, Some(17));
        assert_eq!(summarize_usage(&UsageStatistics::default(), UsageRange::All, now).peak_hour, None);
    }

    #[test]
    fn the_bucket_widens_with_the_span() {
        let now = at(2026, 6, 15, 12);
        let of = |from: i64| {
            let stats = UsageStatistics { generated_at_ms: now, usage: vec![usage(from, "a", 10)], ..Default::default() };
            summarize_usage(&stats, UsageRange::All, now).series
        };
        assert_eq!(of(at(2026, 6, 1, 0)).bucket, BucketSize::Day);
        assert_eq!(of(at(2026, 1, 1, 0)).bucket, BucketSize::Week);
        assert_eq!(of(at(2020, 1, 1, 0)).bucket, BucketSize::Month);
    }

    #[test]
    fn an_absurd_timestamp_folds_into_the_oldest_bar_instead_of_vanishing() {
        let now = at(2026, 6, 15, 12);
        let stats = UsageStatistics { generated_at_ms: now, usage: vec![usage(0, "a", 10), usage(now, "a", 5)], ..Default::default() };
        let s = summarize_usage(&stats, UsageRange::All, now);
        assert!(s.series.points.len() <= MAX_SERIES_POINTS);
        assert_eq!(s.series.points.iter().map(|p| p.total_tokens).sum::<u64>(), 15, "bars add up to the legend");
        assert_eq!(s.series.points.last().unwrap().total_tokens, 5);
    }

    #[test]
    fn ticks_land_on_round_numbers_with_little_headroom() {
        assert_eq!(nice_ticks(0.0, 5), (0, vec![]));
        let (max, ticks) = nice_ticks(20_400_000.0, 5);
        assert_eq!(max, 25_000_000);
        assert_eq!(ticks, vec![0, 5_000_000, 10_000_000, 15_000_000, 20_000_000, 25_000_000]);
        let (max, ticks) = nice_ticks(1.0, 5);
        assert_eq!(*ticks.last().unwrap(), max);
        assert!(max >= 1);
    }

    #[test]
    fn counts_format_for_a_glance_or_exactly() {
        assert_eq!(format_compact_count(0), "0");
        assert_eq!(format_compact_count(999), "999");
        assert_eq!(format_compact_count(115_400_000), "115.4M");
        assert_eq!(format_compact_count(2_000), "2K");
        assert_eq!(format_exact_count(1_234_567), "1,234,567");
        assert_eq!(format_share(0.1234), "12.3%");
        assert_eq!(format_hour12(0), "12 AM");
        assert_eq!(format_hour12(13), "1 PM");
    }

    #[test]
    fn purring_takes_the_largest_unit_at_least_one() {
        assert_eq!(purr_duration(0), None);
        let (v, u) = purr_duration(100_000).unwrap();
        assert_eq!(u, PurrUnit::Minute);
        assert!((v - 6.0).abs() < 1e-9);
        assert_eq!(purr_duration(1_000_000).unwrap().1, PurrUnit::Hour);
        assert_eq!(purr_duration(100_000_000).unwrap().1, PurrUnit::Day);
        assert_eq!(purr_duration(100_000_000_000).unwrap().1, PurrUnit::Month);
        assert_eq!(purr_duration(1).unwrap().1, PurrUnit::Minute);
    }

    #[test]
    fn labels_read_in_each_language() {
        let ms = at(2026, 8, 3, 0);
        assert_eq!(bucket_label(ms, BucketSize::Day, false), "Aug 3");
        assert_eq!(bucket_label(ms, BucketSize::Month, true), "2026年8月");
        assert_eq!(bucket_title(ms, BucketSize::Week, false), "Aug 3 – Aug 9");
    }
}
