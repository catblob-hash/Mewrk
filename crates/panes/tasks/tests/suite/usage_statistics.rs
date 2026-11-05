//! Upstream's `lib/usageStatistics.test.ts`.

use std::collections::HashMap;

use chrono::{Datelike, Local, NaiveDate, TimeZone, Weekday};
use mewrk_client::model::ModelUsage;
use mewrk_pane_tasks::usage_statistics::*;

fn hour_at(y: i32, m: u32, d: u32, h: u32) -> i64 {
    resolve_local(NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap()).timestamp_millis()
}

fn hour(y: i32, m: u32, d: u32) -> i64 {
    hour_at(y, m, d, 10)
}

fn activity(at: i64, user: i64, assistant: i64, sessions: i64) -> ActivityBucket {
    ActivityBucket { hour_start_ms: at, user_messages: user, assistant_messages: assistant, sessions }
}

fn usage(at: i64, f: impl FnOnce(&mut UsageBucket)) -> UsageBucket {
    let mut b = UsageBucket { hour_start_ms: at, provider_id: "p1".into(), provider_name: "Provider".into(), model_id: "model-a".into(), origin: "conversation".into(), requests: 1, ..Default::default() };
    f(&mut b);
    b
}

fn statistics(activity: Vec<ActivityBucket>, usage: Vec<UsageBucket>) -> UsageStatistics {
    UsageStatistics { generated_at_ms: now(), ledger_started_at_ms: 0, activity, usage }
}

fn now() -> i64 {
    hour_at(2026, 8, 20, 12)
}

fn tokens(n: i64) -> impl FnOnce(&mut UsageBucket) {
    move |b| b.total_tokens = n
}

#[test]
fn adds_up_sessions_messages_and_tokens_inside_the_selected_range_only() {
    let activities = vec![activity(hour_at(2026, 6, 1, 9), 5, 5, 2), activity(hour_at(2026, 8, 19, 9), 3, 4, 1)];
    let s = summarize_usage(
        &statistics(activities.clone(), vec![usage(hour_at(2026, 6, 1, 9), |b| { b.total_tokens = 1000; b.input_tokens = 800; b.output_tokens = 200 }), usage(hour_at(2026, 8, 19, 9), |b| { b.total_tokens = 50; b.input_tokens = 30; b.output_tokens = 20 })]),
        UsageRange::Days7,
        Some(now()),
    );
    assert_eq!((s.sessions, s.messages, s.total_tokens, s.input_tokens), (1, 7, 50, 30));
    let all = summarize_usage(&statistics(activities, vec![usage(hour_at(2026, 6, 1, 9), tokens(1000)), usage(hour_at(2026, 8, 19, 9), tokens(50))]), UsageRange::All, Some(now()));
    assert_eq!((all.sessions, all.messages, all.total_tokens), (3, 17, 1050));
}

#[test]
fn counts_today_into_the_current_streak_and_keeps_yesterdays_alive_before_the_first_message() {
    let a = |d| activity(hour_at(2026, 8, d, 9), 1, 0, 0);
    assert_eq!(summarize_usage(&statistics(vec![a(18), a(19), a(20)], vec![]), UsageRange::All, Some(now())).current_streak, 3);
    assert_eq!(summarize_usage(&statistics(vec![a(18), a(19)], vec![]), UsageRange::All, Some(now())).current_streak, 2);
}

#[test]
fn reports_the_longest_streak_across_a_gap_not_the_current_one() {
    let a = |d| activity(hour_at(2026, 8, d, 9), 1, 0, 0);
    let s = summarize_usage(&statistics(vec![a(1), a(2), a(3), a(4), a(20)], vec![]), UsageRange::All, Some(now()));
    assert_eq!((s.longest_streak, s.current_streak, s.active_days), (4, 1, 5));
}

#[test]
fn picks_the_busiest_local_hour_and_the_model_with_the_most_tokens() {
    let s = summarize_usage(
        &statistics(
            vec![activity(hour_at(2026, 8, 19, 9), 2, 0, 0), activity(hour_at(2026, 8, 19, 20), 9, 0, 0)],
            vec![usage(hour_at(2026, 8, 19, 9), |b| { b.model_id = "small".into(); b.total_tokens = 10; b.requests = 9 }), usage(hour_at(2026, 8, 19, 20), |b| { b.model_id = "big".into(); b.total_tokens = 900; b.requests = 1 })],
        ),
        UsageRange::All,
        Some(now()),
    );
    assert_eq!(s.peak_hour, Some(20));
    assert_eq!(s.favorite_model.as_ref().unwrap().model_id, "big");
    assert_eq!(s.models.iter().map(|m| m.model_id.as_str()).collect::<Vec<_>>(), ["big", "small"]);
    assert!((s.models[0].share - 900.0 / 910.0).abs() < 1e-9);
}

#[test]
fn leaves_the_peak_hour_empty_when_the_range_holds_no_messages() {
    let s = summarize_usage(&statistics(vec![], vec![]), UsageRange::All, Some(now()));
    assert_eq!((s.peak_hour, s.favorite_model.is_none(), s.has_activity), (None, true, false));
}

#[test]
fn lays_the_heatmap_out_as_26_week_columns_ending_in_the_current_week() {
    let s = summarize_usage(&statistics(vec![activity(hour_at(2026, 8, 20, 9), 4, 0, 0)], vec![]), UsageRange::All, Some(now()));
    assert_eq!(s.heatmap_weeks.len(), 26);
    assert!(s.heatmap_weeks.iter().all(|w| w.len() == 7));
    let last = s.heatmap_weeks.last().unwrap();
    let today = last.iter().find(|c| c.messages == 4).unwrap();
    assert!(today.level > 0 && !today.future);
    assert!(last.iter().any(|c| c.future));
}

#[test]
fn always_paints_the_busiest_day_at_the_darkest_level() {
    let a = |d, n| activity(hour_at(2026, 8, d, 9), n, 0, 0);
    let s = summarize_usage(&statistics(vec![a(17, 1), a(18, 2), a(19, 3), a(20, 40)], vec![]), UsageRange::All, Some(now()));
    let levels: HashMap<i64, u8> = s.heatmap_weeks.iter().flatten().filter(|c| c.messages > 0).map(|c| (c.messages, c.level)).collect();
    assert_eq!((levels[&40], levels[&1]), (4, 1));
    assert!(levels.values().collect::<std::collections::HashSet<_>>().len() > 1);
}

#[test]
fn keeps_the_heatmap_on_the_full_history_even_when_the_tiles_are_scoped_to_7_days() {
    let s = summarize_usage(&statistics(vec![activity(hour_at(2026, 7, 1, 9), 3, 0, 0)], vec![]), UsageRange::Days7, Some(now()));
    assert_eq!(s.messages, 0);
    assert!(s.heatmap_weeks.iter().flatten().any(|c| c.messages == 3));
}

#[test]
fn lays_one_bar_per_day_across_the_range_and_stacks_them_in_model_order() {
    let s = summarize_usage(
        &statistics(vec![], vec![usage(hour_at(2026, 8, 19, 9), |b| { b.model_id = "small".into(); b.total_tokens = 100 }), usage(hour_at(2026, 8, 19, 20), |b| { b.model_id = "big".into(); b.total_tokens = 700 }), usage(hour_at(2026, 8, 20, 9), |b| { b.model_id = "small".into(); b.total_tokens = 50 })]),
        UsageRange::Days7,
        Some(now()),
    );
    assert_eq!(s.series.bucket, UsageBucketSize::Day);
    assert_eq!(s.series.points.len(), 7);
    assert_eq!(s.series.points.last().unwrap().key, "2026-08-20");
    assert_eq!(s.series.points[0].key, "2026-08-14");
    assert_eq!(s.models.iter().map(|m| m.model_id.as_str()).collect::<Vec<_>>(), ["big", "small"]);
    assert_eq!(s.series.points[5].by_model, [700, 100]);
    assert_eq!(s.series.points[5].total_tokens, 800);
    assert_eq!(s.series.points[6].by_model, [0, 50]);
    assert!(s.series.points[..5].iter().all(|p| p.total_tokens == 0));
}

#[test]
fn opens_the_all_time_window_at_the_first_token_not_the_first_message() {
    let s = summarize_usage(&statistics(vec![activity(hour_at(2026, 3, 1, 9), 40, 0, 0)], vec![usage(hour_at(2026, 8, 19, 9), tokens(10))]), UsageRange::All, Some(now()));
    assert_eq!(s.series.bucket, UsageBucketSize::Day);
    assert_eq!(s.series.points.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(), ["2026-08-19", "2026-08-20"]);
}

#[test]
fn widens_the_bucket_once_the_window_outgrows_daily_bars() {
    let weekly = summarize_usage(&statistics(vec![], vec![usage(hour_at(2026, 5, 1, 9), tokens(10))]), UsageRange::All, Some(now()));
    assert_eq!(weekly.series.bucket, UsageBucketSize::Week);
    assert!(weekly.series.points.len() < MAX_SERIES_POINTS);
    assert!(weekly.series.points.iter().all(|p| Local.timestamp_millis_opt(p.start_ms).unwrap().weekday() == Weekday::Mon));
    assert_eq!(weekly.series.points.last().unwrap().start_ms, hour_at(2026, 8, 17, 0));
    assert_eq!(weekly.series.points.iter().map(|p| p.total_tokens).sum::<i64>(), 10);
    let monthly = summarize_usage(&statistics(vec![], vec![usage(hour_at(2022, 1, 9, 9), tokens(10))]), UsageRange::All, Some(now()));
    assert_eq!(monthly.series.bucket, UsageBucketSize::Month);
    assert!(monthly.series.points.iter().all(|p| Local.timestamp_millis_opt(p.start_ms).unwrap().day() == 1));
    assert_eq!(monthly.series.points.last().unwrap().key, "2026-08-01");
}

#[test]
fn caps_the_bar_count_from_the_recent_end_when_a_timestamp_predates_everything() {
    let s = summarize_usage(&statistics(vec![], vec![usage(0, tokens(10))]), UsageRange::All, Some(now()));
    assert_eq!(s.series.bucket, UsageBucketSize::Month);
    assert_eq!(s.series.points.len(), MAX_SERIES_POINTS);
    assert_eq!(s.series.points.last().unwrap().key, "2026-08-01");
    assert_eq!(s.series.points[0].total_tokens, 10);
}

#[test]
fn never_lets_the_bars_add_up_to_less_than_the_legend() {
    let cases: Vec<(&str, Vec<UsageBucket>)> = vec![
        ("ordinary", vec![usage(hour_at(2026, 8, 19, 9), |b| { b.model_id = "a".into(); b.total_tokens = 700 }), usage(hour_at(2026, 8, 20, 9), |b| { b.model_id = "b".into(); b.total_tokens = 50 })]),
        ("future", vec![usage(hour_at(2026, 8, 21, 9), tokens(900))]),
        ("prehistoric", vec![usage(0, tokens(90))]),
        ("weekly", vec![usage(hour_at(2026, 5, 1, 9), tokens(40))]),
        ("monthly", vec![usage(hour_at(2021, 1, 1, 9), tokens(40))]),
    ];
    for (name, buckets) in cases {
        let s = summarize_usage(&statistics(vec![], buckets), UsageRange::All, Some(now()));
        assert_eq!(s.series.points.iter().map(|p| p.total_tokens).sum::<i64>(), s.total_tokens, "{name}");
        for p in &s.series.points {
            assert_eq!(p.by_model.iter().sum::<i64>(), p.total_tokens, "{name}");
        }
        assert!(s.series.points.len() <= MAX_SERIES_POINTS, "{name}");
    }
}

#[test]
fn counts_real_bars_not_days_when_choosing_the_bucket_width() {
    let s = summarize_usage(&statistics(vec![], vec![usage(hour_at(2025, 6, 15, 9), tokens(10))]), UsageRange::All, Some(hour_at(2026, 8, 22, 12)));
    assert_eq!(s.series.bucket, UsageBucketSize::Month);
    assert!(s.series.points.len() <= 62);
}

#[test]
fn rounds_the_axis_up_to_readable_steps() {
    assert_eq!(nice_ticks(0.0, 5), (0, vec![]));
    assert_eq!(nice_ticks(19_400_000.0, 5), (20_000_000, vec![0, 5_000_000, 10_000_000, 15_000_000, 20_000_000]));
    assert_eq!(nice_ticks(1_000_000.0, 5), (1_000_000, vec![0, 250_000, 500_000, 750_000, 1_000_000]));
    assert_eq!(nice_ticks(7.0, 5), (8, vec![0, 2, 4, 6, 8]));
}

#[test]
fn spends_the_headroom_rather_than_jumping_a_whole_decade() {
    assert_eq!(nice_ticks(20_400_000.0, 5), (25_000_000, vec![0, 5_000_000, 10_000_000, 15_000_000, 20_000_000, 25_000_000]));
    assert_eq!(nice_ticks(1.0, 5), (1, vec![0, 1]));
    assert_eq!(nice_ticks(2.0, 5), (2, vec![0, 1, 2]));
    for peak in [1i64, 3, 7, 999, 1234, 20_400_000, 115_400_000, 2_500_000_000] {
        let (max, ticks) = nice_ticks(peak as f64, 5);
        assert!(max >= peak);
        assert_eq!((ticks[0], *ticks.last().unwrap()), (0, max));
        assert_eq!(ticks.iter().collect::<std::collections::HashSet<_>>().len(), ticks.len());
    }
}

#[test]
fn leaves_the_axis_empty_when_the_range_recorded_requests_but_no_tokens() {
    let s = summarize_usage(&statistics(vec![], vec![usage(hour_at(2026, 8, 20, 9), |b| b.requests = 2)]), UsageRange::Days7, Some(now()));
    assert_eq!(s.models.len(), 1);
    assert_eq!((s.series.max, s.series.ticks.len()), (0, 0));
    assert!(s.series.points.iter().all(|p| p.total_tokens == 0));
}

fn turn(id: &str, at: &str, usage: ModelUsage, ended: bool) -> StoredTurn {
    StoredTurn { id: id.into(), model_id: "model-a".into(), started_at: at.into(), ended_at: ended.then(|| at.to_owned()), usage }
}

#[test]
fn derives_one_idempotent_event_per_turn_and_resolves_the_provider_from_the_model_id() {
    let at = Local.timestamp_millis_opt(hour(2026, 8, 19)).unwrap().to_rfc3339();
    let turns = HashMap::from([("conv".to_owned(), vec![turn("turn-1", &at, ModelUsage { input_tokens: Some(10), cached_input_tokens: Some(3), output_tokens: Some(4), total_tokens: Some(14), reasoning_tokens: None }, true)])]);
    let events = usage_backfill_events(&turns, &HashMap::from([("conv".to_owned(), "ws".to_owned())]), &HashMap::from([("model-a".to_owned(), ("prov".to_owned(), "Prov".to_owned()))]));
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!((e.id.as_str(), e.provider_name.as_str(), e.workspace_id.as_str(), e.total_tokens, e.occurred_at_ms), ("turn:conv:turn-1", "Prov", "ws", 14, hour(2026, 8, 19)));
}

#[test]
fn drops_turns_that_never_reported_any_usage() {
    let turns = HashMap::from([("conv".to_owned(), vec![turn("turn-empty", "2026-08-19T10:00:00Z", ModelUsage::default(), false)])]);
    assert!(usage_backfill_events(&turns, &HashMap::new(), &HashMap::new()).is_empty());
}

#[test]
fn compacts_counts_the_way_the_tiles_read_them() {
    assert_eq!(format_compact_count(0), "0");
    assert_eq!(format_compact_count(999), "999");
    assert_eq!(format_compact_count(1234), "1.2K");
    assert_eq!(format_compact_count(115_400_000), "115.4M");
    assert_eq!(format_compact_count(2_500_000_000), "2.5B");
    assert_eq!(format_compact_count(2_000_000), "2M");
    assert_eq!(format_exact_count(1_234_567), "1,234,567");
}

#[test]
fn carries_the_purr_duration_up_to_the_largest_unit_that_still_reads_as_a_number() {
    let close = |got: Option<(f64, PurrUnit)>, v: f64, u: PurrUnit| {
        let (value, unit) = got.unwrap();
        assert!((value - v).abs() < 0.01 && unit == u, "{value} {unit:?}");
    };
    close(purr_duration(40_000), 2.4, PurrUnit::Minute);
    close(purr_duration(1_000_000), 1.0, PurrUnit::Hour);
    close(purr_duration(115_400_000), 4.81, PurrUnit::Day);
    close(purr_duration(720_000_000), 1.0, PurrUnit::Month);
    assert_eq!(purr_duration(0), None);
}
