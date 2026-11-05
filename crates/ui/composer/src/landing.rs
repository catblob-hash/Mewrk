//! The page of a task nothing has been sent in: upstream's `DraftLanding.tsx`, the timeline's empty state on the main conversation.
//! The stock notice only said the timeline was empty; this one says what the page is for and fills the room above the composer with
//! what a new task can start from: the project's tasks by recency, which the sidebar keeps in the user's own order instead, and the
//! keys that work here, read from the user's bindings so a rebound key shows as bound.

use chrono::{DateTime, Datelike, Duration, Local as LocalTime, TimeZone};
use mewrk_client::model::ShortcutPreference;
use mewrk_client::prelude::*;
use mewrk_client::runs::ConversationStatus;
use mewrk_client::selection::{DraftLandingTask, DRAFT_LANDING_TASK_LIMIT};
use mewrk_client::shortcuts::{resolve_shortcut, shortcut_command, shortcut_key_caps};
use mewrk_ui_common::brand::{mewrk_icon_bare, BRAND_ORANGE, MARK_PATH};
use mewrk_ui_common::popover_menu::probe::say;
use std::collections::BTreeMap;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

fn start_of_day(moment: DateTime<LocalTime>) -> DateTime<LocalTime> {
    let midnight = moment.date_naive().and_hms_opt(0, 0, 0).expect("midnight exists as a time of day");
    // A midnight the clocks skip is the first moment of that day instead.
    LocalTime.from_local_datetime(&midnight).earliest().unwrap_or(moment)
}

/// `new Date(...).toLocaleDateString(language, { month: "short", day: "numeric", year? })`.
fn short_date(then: DateTime<LocalTime>, with_year: bool, english: bool) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (year, month, day) = (then.year(), then.month() as usize, then.day());
    match (english, with_year) {
        (true, true) => format!("{} {day}, {year}", MONTHS[month - 1]),
        (true, false) => format!("{} {day}", MONTHS[month - 1]),
        (false, true) => format!("{year}年{month}月{day}日"),
        (false, false) => format!("{month}月{day}日"),
    }
}

/// How long ago a task was last touched, as coarsely as a glance needs: minutes, hours, days, and past a week the date itself, which
/// is easier to place than "23 days ago".
///
/// Within a day it counts elapsed time; beyond that it counts midnights, the way "yesterday" is meant: 26 hours before one in the
/// morning is the day before yesterday. `now` is in epoch milliseconds.
pub fn task_age_label(updated_at: &str, now: i64) -> String {
    let Ok(then) = DateTime::parse_from_rfc3339(updated_at) else { return String::new() };
    let then = then.with_timezone(&LocalTime);
    let now_at = LocalTime.timestamp_millis_opt(now).single().unwrap_or_else(LocalTime::now);
    // A clock that moved back must not read as a task from the future.
    let elapsed = (now - then.timestamp_millis()).max(0);
    if elapsed < MINUTE {
        return t("刚刚", "Just now").to_owned();
    }
    if elapsed < HOUR {
        let count = elapsed / MINUTE;
        return tf("{count} 分钟前", "{count} min ago", &[("count", &count)]);
    }
    // Rounded, because a day that changes the clocks is not 24 hours long, and on the one that runs to 25, a full day's hours can
    // pass without a midnight, which is still today.
    let between = start_of_day(now_at) - start_of_day(then);
    let days = (between + Duration::hours(12)).num_days();
    if elapsed < DAY || days < 1 {
        let count = elapsed / HOUR;
        return tf("{count} 小时前", if count == 1 { "{count} hour ago" } else { "{count} hours ago" }, &[("count", &count)]);
    }
    if days == 1 {
        return t("昨天", "Yesterday").to_owned();
    }
    if days < 7 {
        return tf("{count} 天前", "{count} days ago", &[("count", &days)]);
    }
    short_date(then, then.year() != now_at.year(), mewrk_client::i18n::is_english())
}

/// The commands worth knowing on a task nothing has been sent in yet, in the order listed.
const LANDING_COMMANDS: [&str; 3] = ["conversation.next", "app.conversation_settings.open", "panel.browser.toggle"];

/// One line of the shortcuts list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutRow {
    pub id: String,
    pub label: String,
    pub caps: Vec<String>,
}

/// The keys the page lists. A command the user switched off or left unbound has no key to show.
pub fn landing_shortcut_rows(shortcuts: &BTreeMap<String, ShortcutPreference>, send: &[String], newline: &[String], can_insert: bool) -> Vec<ShortcutRow> {
    let mut rows = vec![ShortcutRow { id: "send".to_owned(), label: t("发送", "Send").to_owned(), caps: shortcut_key_caps(send, None) }, ShortcutRow { id: "newline".to_owned(), label: t("换行", "New line").to_owned(), caps: shortcut_key_caps(newline, None) }];
    // Not a key, but it is the one gesture of this page nothing on screen gives away.
    if can_insert {
        rows.push(ShortcutRow { id: "insert".to_owned(), label: t("插入上下文", "Insert context").to_owned(), caps: vec![t("右键", "Right-click").to_owned()] });
    }
    for id in LANDING_COMMANDS {
        let Some(command) = shortcut_command(id) else { continue };
        let preference = resolve_shortcut(shortcuts, command);
        if !preference.enabled || preference.binding.is_empty() {
            continue;
        }
        let label = match id {
            "conversation.next" => t("下一个对话", "Next conversation"),
            "app.conversation_settings.open" => t("本对话设置", "Conversation settings"),
            _ => t("内置浏览器", "Built-in browser"),
        };
        rows.push(ShortcutRow { id: id.to_owned(), label: label.to_owned(), caps: shortcut_key_caps(&preference.binding, None) });
    }
    rows.retain(|row| !row.caps.is_empty());
    rows
}

pub fn task_key(conversation_id: &str) -> u64 {
    hash_key(("draft-landing-task", conversation_id))
}

/// A vector drawing as a coloured SVG, sized by the box it is drawn into. `key` names it for the atlas.
fn svg_mark(key: String, svg: String, size: f32) -> E {
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        ui.svg_icon(&key, &svg, r.x + r.w * 0.5, r.y + r.h * 0.5, r.w.max(r.h));
    })
    .size(size, size)
    .pass_through()
}

/// A colour as an SVG paint and its opacity: a theme's quieter text is its plain colour with less alpha.
pub(crate) fn svg_paint(color: Color) -> (String, f32) {
    (format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b), color.a as f32 / 255.0)
}

/// `ConversationStatusMark`: the brand mark, filled in a status's colour; idle is its outline.
/// TODO(sidebar): use the sidebar's own once `crates/mewrk` publishes it.
pub fn conversation_status_mark(status: ConversationStatus) -> E {
    let t_ = theme();
    let (color, label) = match status {
        ConversationStatus::Running => (svg_paint(t_.text_primary), Some(t("正在进行", "In progress"))),
        ConversationStatus::Blocked => (svg_paint(BRAND_ORANGE), Some(t("等待你处理", "Waiting for you"))),
        ConversationStatus::Completed => (("#3769bb".to_owned(), 1.0), Some(t("已完成", "Finished"))),
        ConversationStatus::Idle => (svg_paint(t_.text_tertiary), None),
    };
    let (color, opacity) = color;
    if let Some(label) = label {
        say(label);
    }
    let paint = if status == ConversationStatus::Idle { format!("fill=\"none\" stroke=\"{color}\" stroke-opacity=\"{opacity}\" stroke-width=\"10\"") } else { format!("fill=\"{color}\" fill-opacity=\"{opacity}\"") };
    let svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"14\" height=\"14\" viewBox=\"-28 -14 160 160\"><path d=\"{MARK_PATH}\" {paint}/></svg>");
    let mark = svg_mark(format!("status-mark:{color}:{opacity}:{}", status == ConversationStatus::Idle), svg, 14.0);
    match label {
        // On the mark, so a row it stands in counts it among its words.
        Some(label) => div().child(mark.aria_label(label)).tooltip(label),
        None => mark,
    }
}

/// What the page shows, separate from where it reads it.
pub struct DraftLandingProps {
    pub recent_tasks: Vec<DraftLandingTask>,
    pub shortcuts: BTreeMap<String, ShortcutPreference>,
    pub send_shortcut: Vec<String>,
    pub newline_shortcut: Vec<String>,
    /// Whether a right-click here inserts a context. A locked timeline does not advertise one.
    pub can_insert: bool,
    /// The moment the tasks' ages are measured from. Only tests pass it.
    pub now: Option<i64>,
    pub on_open_task: Rc<dyn Fn(&mut App, &str)>,
}

/// The page. It keeps the empty state's box: the timeline centres it, and a right-click anywhere on it is still a right-click on
/// the timeline, which is how a context is written by hand before anything is sent.
pub fn draft_landing_view(props: DraftLandingProps) -> E {
    let t_ = theme();
    let (width, height) = window_size();
    let title = t("喵。开工。", "Mew. Work.");
    let lede = if props.can_insert { t("在下方说说要做什么，或右键手写上下文。", "Say what to do below, or right-click to hand-write context.") } else { t("在下方说说要做的事。", "Say what to do below.") };
    say(title);
    say(lede);
    let hero: E = col().items_center().child(mewrk_icon_bare(46.0)).child(text(title).role(Role::Heading).aria_level(2).ty(SUBTITLE).color(t_.text_primary).mt(16.0)).child(text(lede).ty(CAPTION).color(t_.text_secondary).max_w(440.0).mt(6.0).text_center());

    let tasks: Vec<&DraftLandingTask> = props.recent_tasks.iter().take(DRAFT_LANDING_TASK_LIMIT).collect();
    let now = props.now.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let heading = |label: &str| -> E {
        say(label);
        row().role(Role::Heading).aria_level(3).aria_label(label.to_owned()).h(26.0).px(8.0).items_center().child(text(label.to_uppercase()).ty(FINE_STRONG).color(t_.text_tertiary))
    };
    let mut columns: E = row().key("draft-landing-columns").w_full().max_w(660.0).justify_center().gap(40.0);
    if !tasks.is_empty() {
        let mut list: E = col();
        for task in &tasks {
            let age = task_age_label(&task.updated_at, now);
            say(&task.title);
            say(&age);
            let id = task.id.clone();
            let open = props.on_open_task.clone();
            list = list.child(
                row()
                    .key(task_key(&task.id))
                    .h(30.0)
                    .pl(7.0)
                    .pr(8.0)
                    .items_center()
                    .gap(7.0)
                    .radius(7.0)
                    .hover_bg(t_.subtle_hover)
                    .on_click(move |app: &mut App| open(app, &id))
                    .child(conversation_status_mark(task.status))
                    .child(text(task.title.clone()).ty(BODY_SMALL).color(t_.text_primary).ellipsis().flex1().ml(5.0))
                    .child(text(age).ty(FINE).color(t_.text_tertiary).nowrap().shrink0()),
            );
        }
        columns = columns.child(col().key("draft-landing-tasks").role(Role::Region).aria_label(t("最近任务", "Recent tasks")).basis(310.0).min_w(0.0).max_w(310.0).child(heading(t("最近任务", "Recent tasks"))).child(list));
    }
    let rows = landing_shortcut_rows(&props.shortcuts, &props.send_shortcut, &props.newline_shortcut, props.can_insert);
    let mut shortcuts: E = col().role(Role::List);
    for shortcut in rows {
        say(&shortcut.label);
        let mut keys: E = row().gap(4.0).shrink0();
        for cap in shortcut.caps {
            say(&cap);
            // The return glyph is an emoji to the fonts that have it, which is not what a keycap wants.
            let face: E = if cap == "↩" { icon("CornerDownLeft", 12.0).color(t_.text_secondary).aria_label(cap) } else { text(cap).ty(FINE_STRONG).color(t_.text_secondary).nowrap() };
            keys = keys.child(row().min_w(20.0).h(20.0).px(5.0).center().radius(5.0).border(t_.card_stroke).bg(t_.card_secondary).child(face));
        }
        shortcuts = shortcuts.child(row().role(Role::ListItem).h(30.0).px(8.0).items_center().justify_between().gap(12.0).child(text(shortcut.label).ty(CAPTION).color(t_.text_secondary).ellipsis().min_w(0.0)).child(keys));
    }
    // The page never scrolls: its whole point is the composer under it, so what does not fit goes. A tile too narrow for two columns
    // keeps the tasks, which are the half that leads somewhere, and a short window keeps the brand alone.
    let narrow = rect_of("draft-landing").map_or(width, |box_| box_.w) <= 520.0;
    let short = height <= 660.0;
    if !(narrow && !tasks.is_empty()) {
        columns = columns.child(col().key("draft-landing-shortcuts").role(Role::Region).aria_label(t("快捷键", "Keyboard shortcuts")).basis(310.0).min_w(0.0).max_w(310.0).child(heading(t("快捷键", "Keyboard shortcuts"))).child(shortcuts));
    }
    let mut page: E = col().key("draft-landing").flex1().min_h(0.0).items_center().justify_center().gap(30.0).pt(20.0).pb(46.0).px(16.0).child(hero);
    // The page can sit open for hours and nothing else redraws it, so the ages keep their own time, by the minute, which is the
    // finest they are told in.
    if props.now.is_none() && !tasks.is_empty() {
        page = page.child(custom(|_: &mut App, ui: &mut Ui<'_>, _: Rect| ui.wake_after(60.0)).w(0.0).h(0.0).pass_through());
    }
    if !short {
        page = page.child(columns);
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<LocalTime> {
        LocalTime.from_local_datetime(&NaiveDate::from_ymd_opt(year, month, day).unwrap().and_hms_opt(hour, minute, 0).unwrap()).earliest().unwrap()
    }

    fn age(then: DateTime<LocalTime>, now: DateTime<LocalTime>) -> String {
        task_age_label(&then.to_rfc3339(), now.timestamp_millis())
    }

    #[test]
    fn counts_elapsed_time_within_a_day_and_calendar_days_past_it() {
        let _language = crate::tests::language(false);
        let now = local(2026, 11, 12, 12, 0);
        for (minutes, expected) in [(0, "刚刚"), (1, "1 分钟前"), (12, "12 分钟前"), (60, "1 小时前"), (300, "5 小时前"), (23 * 60, "23 小时前")] {
            assert_eq!(age(now - Duration::minutes(minutes), now), expected);
        }
        assert_eq!(age(local(2026, 11, 11, 9, 0), now), "昨天");
        assert_eq!(age(local(2026, 11, 10, 12, 0), now), "2 天前");
        assert_eq!(age(local(2026, 11, 6, 12, 0), now), "6 天前");
    }

    #[test]
    fn reads_a_task_under_48_hours_old_as_two_days_back_when_it_was_the_night_before_last() {
        let _language = crate::tests::language(false);
        // 26 hours before 01:00 is 23:00 two days back: a day before yesterday, though under 48 hours old.
        assert_eq!(age(local(2026, 11, 10, 23, 0), local(2026, 11, 12, 1, 0)), "2 天前");
        // Within a day the count stays elapsed time, so a task from 23:50 reads in minutes after midnight.
        assert_eq!(age(local(2026, 11, 11, 23, 50), local(2026, 11, 12, 0, 30)), "40 分钟前");
    }

    #[test]
    fn switches_from_a_count_of_days_to_the_date_at_one_calendar_week_with_the_year_only_from_an_earlier_year() {
        let _language = crate::tests::language(false);
        let now = local(2026, 11, 12, 12, 0);
        assert_eq!(age(local(2026, 11, 5, 12, 0), now), "11月5日");
        assert_eq!(age(local(2025, 10, 8, 12, 0), now), "2025年10月8日");
        drop(_language);
        let _language = crate::tests::language(true);
        assert_eq!(age(local(2026, 11, 5, 12, 0), now), "Nov 5");
        assert_eq!(age(local(2025, 10, 8, 12, 0), now), "Oct 8, 2025");
    }

    #[test]
    fn reads_ages_in_english_and_a_stamp_after_now_as_just_now_and_an_unreadable_one_as_nothing() {
        let _language = crate::tests::language(true);
        let now = local(2026, 11, 12, 12, 0);
        assert_eq!(age(now, now), "Just now");
        assert_eq!(age(now - Duration::minutes(1), now), "1 min ago");
        assert_eq!(age(now - Duration::hours(1), now), "1 hour ago");
        assert_eq!(age(now - Duration::hours(5), now), "5 hours ago");
        assert_eq!(age(local(2026, 11, 11, 9, 0), now), "Yesterday");
        assert_eq!(age(local(2026, 11, 10, 9, 0), now), "2 days ago");
        assert_eq!(age(now + Duration::hours(3), now), "Just now");
        assert_eq!(task_age_label("not a date", now.timestamp_millis()), "");
    }

    #[test]
    fn lists_the_keys_the_user_has_bound() {
        let _language = crate::tests::language(true);
        let enter = vec!["Enter".to_owned()];
        let newline = vec!["Shift".to_owned(), "Enter".to_owned()];
        let rows = landing_shortcut_rows(&BTreeMap::new(), &enter, &newline, true);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["send", "newline", "insert", "conversation.next", "app.conversation_settings.open", "panel.browser.toggle"]);
        assert_eq!(rows[2].caps, ["Right-click"]);
        assert!(!landing_shortcut_rows(&BTreeMap::new(), &enter, &newline, false).iter().any(|row| row.id == "insert"));

        // A rebound command shows its own keys; one switched off or left unbound is left out.
        let mut shortcuts = BTreeMap::new();
        shortcuts.insert("conversation.next".to_owned(), ShortcutPreference { binding: vec!["Alt".to_owned(), "KeyN".to_owned()], enabled: true });
        shortcuts.insert("panel.browser.toggle".to_owned(), ShortcutPreference { binding: vec!["KeyB".to_owned()], enabled: false });
        shortcuts.insert("app.conversation_settings.open".to_owned(), ShortcutPreference { binding: vec![], enabled: true });
        let rows = landing_shortcut_rows(&shortcuts, &enter, &newline, false);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["send", "newline", "conversation.next"]);
        assert_eq!(rows[2].caps.len(), 2);
    }
}
