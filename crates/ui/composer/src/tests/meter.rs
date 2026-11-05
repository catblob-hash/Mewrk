//! `ContextUsageMeter.test.tsx`.

use super::rig::Rig;
use crate::meter::*;
use mewrk_client::auto_compact::default_auto_compact_settings;
use mewrk_client::model::{AutoCompactSettings, CompactionMethod, ContextItem};
use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::panel_key;
use std::cell::{Cell, RefCell};

fn contexts() -> Vec<ContextItem> {
    let at = "2026-08-27T00:00:00Z";
    vec![
        serde_json::from_value(json!({ "id": "s1", "kind": "system", "content": "s".repeat(200), "createdAt": at })).unwrap(),
        serde_json::from_value(json!({ "id": "u1", "kind": "user", "content": "u".repeat(400), "createdAt": at })).unwrap(),
        serde_json::from_value(json!({ "id": "a1", "kind": "assistant", "content": "a".repeat(800), "createdAt": at })).unwrap(),
        serde_json::from_value(json!({ "id": "r1", "kind": "reasoning", "content": "r".repeat(200), "createdAt": at })).unwrap(),
        serde_json::from_value(json!({ "id": "t1", "kind": "tool", "toolName": "read_file", "input": {}, "result": { "success": true, "output": "t".repeat(1200), "executedAt": at, "durationMs": 1 }, "createdAt": at })).unwrap(),
    ]
}

fn base() -> ContextUsageProps {
    ContextUsageProps { contexts: contexts(), tokens: 180_000, estimated: false, context_window: Some(1_000_000), counts: ContextUsageCounts { tools: 38, mcp_servers: 2, skills: 4, agent_roles: 5 }, ..ContextUsageProps::default() }
}

fn meter_rig(props: impl Fn() -> ContextUsageProps + 'static) -> Rig {
    Rig::new().with_view(move |_| row().p(200.0).justify_end().child(context_usage_meter(props())))
}

#[test]
fn puts_the_numbers_on_the_rings_name_instead_of_into_the_composer_row() {
    let mut rig = meter_rig(base);
    let words = rig.words();
    assert!(words.iter().any(|said| said == "Context usage: 180k / 1m (18%)"));
    assert!(!words.iter().any(|said| said == "180k / 1m (18%)"), "the row shows only the ring");
}

#[test]
fn lists_the_composition_free_space_and_the_per_conversation_counts() {
    let mut rig = meter_rig(base);
    rig.click(meter_trigger_key());
    rig.settle(30);
    let words = rig.words();
    for word in ["Context window", "180k / 1m (18%)", "System prompt", "User messages", "Assistant replies", "Reasoning", "Tool calls", "Other (tool schemas, etc.)", "Free space", "820k", "82.0%", "Tools 38", "MCP 2", "Skills 4", "Roles 5"] {
        assert!(words.iter().any(|said| said == word), "{word} in {words:?}");
    }
    // It ends at the counts row, with no settings action.
    assert!(!words.iter().any(|said| said == "Auto-compact"));
}

#[test]
fn says_so_when_the_model_cannot_project_usage_at_all() {
    let mut rig = meter_rig(|| ContextUsageProps { unprojectable: true, ..base() });
    assert!(rig.says("Context usage: not projectable"));
    rig.click(meter_trigger_key());
    rig.settle(30);
    let words = rig.words();
    assert!(words.iter().any(|said| said == "Not projectable"));
    assert!(!words.iter().any(|said| said == "Free space"));
}

#[test]
fn says_there_is_nothing_to_break_down_without_a_window_or_contexts() {
    let mut rig = meter_rig(|| ContextUsageProps { contexts: Vec::new(), tokens: 0, context_window: None, ..ContextUsageProps::default() });
    rig.click(meter_trigger_key());
    rig.settle(30);
    assert!(rig.says("Nothing to break down yet"));
}

struct Compact {
    settings: Rc<RefCell<AutoCompactSettings>>,
    method: Rc<RefCell<Option<CompactionMethod>>>,
    now: Rc<Cell<u32>>,
}

fn compact_rig(native: bool, handoff: bool, blocked: Option<&'static str>) -> (Rig, Compact) {
    let compact = Compact { settings: Rc::new(RefCell::new(default_auto_compact_settings())), method: Rc::new(RefCell::new(None)), now: Rc::default() };
    let (settings, method, now) = (compact.settings.clone(), compact.method.clone(), compact.now.clone());
    let rig = meter_rig(move || {
        let (change, method_change, now) = (settings.clone(), method.clone(), now.clone());
        ContextUsageProps {
            auto_compact: Some(settings.borrow().clone()),
            on_auto_compact_change: Some(Rc::new(move |_, next| *change.borrow_mut() = next)),
            auto_compact_unavailable: !handoff,
            native_compaction_available: native,
            compaction_method: method.borrow().map(Some),
            on_compaction_method_change: Some(Rc::new(move |_, next| *method_change.borrow_mut() = Some(next))),
            on_compact_now: Some(Rc::new(move |_| now.set(now.get() + 1))),
            compact_now_blocked: blocked.map(str::to_owned),
            ..base()
        }
    });
    (rig, compact)
}

fn open_submenu(rig: &mut Rig) {
    rig.click(meter_trigger_key());
    rig.settle(30);
    rig.click(compact_row_key());
    rig.settle(30);
}

fn commit_field(rig: &mut Rig, key: impl std::hash::Hash, text: &str) {
    let area = rig.rect(key);
    rig.click_at(area.x + area.w * 0.5, area.y + area.h * 0.5);
    rig.input.keys.push(fluent::ui::KeyPress { key: Key::Char('a'), shift: false, cmd: true, alt: false, ctrl: false });
    rig.frame();
    rig.type_text(text);
    rig.press(Key::Enter);
}

#[test]
fn opens_its_submenu_from_a_row_at_the_foot_of_the_panel_marks_the_threshold_and_starts_closed_on_reopening() {
    let (mut rig, _) = compact_rig(false, true, None);
    rig.click(meter_trigger_key());
    rig.settle(30);
    let words = rig.words();
    assert!(words.iter().any(|said| said == "Auto-compact"));
    assert!(words.iter().any(|said| said == "80%"), "the row says the percent, with nothing to choose between");
    assert!(words.iter().any(|said| said == "threshold:80"));
    rig.click(compact_row_key());
    rig.settle(30);
    assert!(rig.says("Enable auto-compact"));
    // Beside the panel, and hanging up from its row.
    let (panel, flyout, row) = (rig.rect(panel_key(METER_KEY)), rig.rect(hash_key(("menu-flyout", hash_key("context-usage-compact")))), rig.rect(compact_row_key()));
    assert!(flyout.x >= panel.right() - 1.0 || flyout.right() <= panel.x + 1.0, "{flyout:?} {panel:?}");
    assert!(flyout.bottom() >= row.bottom() - 1.0 && flyout.y < row.y, "{flyout:?} {row:?}");
    rig.press(Key::Escape);
    rig.settle(20);
    assert!(!rig.has(panel_key(METER_KEY)), "Escape closes the whole menu");
    rig.click(meter_trigger_key());
    rig.settle(30);
    assert!(!rig.says("Enable auto-compact"), "a reopened panel starts with its submenu closed");
}

#[test]
fn the_panel_takes_the_focus_and_escape_gives_it_back_to_the_ring() {
    let (mut rig, _) = compact_rig(false, true, None);
    rig.click(meter_trigger_key());
    rig.settle(10);
    assert_eq!(fluent::el::focused_id(), hash_key(panel_key(METER_KEY)));
    rig.press(Key::Escape);
    rig.settle(10);
    assert!(!rig.has(panel_key(METER_KEY)));
    assert_eq!(fluent::el::focused_id(), hash_key(meter_trigger_key()));
}

#[test]
fn switches_auto_compact_off_without_touching_the_threshold() {
    let (mut rig, compact) = compact_rig(false, true, None);
    open_submenu(&mut rig);
    rig.click(compact_switch_key());
    assert!(!compact.settings.borrow().enabled);
    assert_eq!(compact.settings.borrow().threshold_percent, 80);
    rig.settle(10);
    assert!(rig.says("Off"));
}

#[test]
fn commits_a_typed_threshold_on_enter_clamped() {
    let (mut rig, compact) = compact_rig(false, true, None);
    open_submenu(&mut rig);
    commit_field(&mut rig, ("field", percent_field_key(false)).hash_id(), "85");
    assert_eq!(compact.settings.borrow().threshold_percent, 85);
    assert!(rig.says("Hands off at 850k tokens"));
    commit_field(&mut rig, ("field", percent_field_key(false)).hash_id(), "200");
    assert_eq!(compact.settings.borrow().threshold_percent, 97);
    commit_field(&mut rig, ("field", percent_field_key(false)).hash_id(), "1");
    assert_eq!(compact.settings.borrow().threshold_percent, 20);
}

#[test]
fn commits_the_slider_when_it_is_released_not_at_every_step() {
    let (mut rig, compact) = compact_rig(false, true, None);
    open_submenu(&mut rig);
    let track = rig.rect(slider_key(false));
    rig.input.pointer = Some((track.x + 10.0, track.y + 8.0));
    rig.frame();
    rig.input.pressed = true;
    rig.input.down = true;
    rig.frame();
    rig.input.pointer = Some((track.x + track.w * 0.5, track.y + 8.0));
    rig.frame();
    rig.frame();
    assert_eq!(compact.settings.borrow().threshold_percent, 80, "dragging saves nothing");
    rig.input.down = false;
    rig.input.released = true;
    rig.frame();
    rig.settle(10);
    let saved = compact.settings.borrow().threshold_percent;
    assert!((55..=62).contains(&saved), "{saved}");
}

#[test]
fn the_keyboard_works_the_slider_and_steps_the_number_fields() {
    let (mut rig, compact) = compact_rig(true, true, None);
    *compact.method.borrow_mut() = Some(CompactionMethod::Native);
    open_submenu(&mut rig);
    // A press on the slider gives it the keyboard, and each key is its native `change`, which saves.
    let track = rig.rect(slider_key(true));
    rig.click_at(track.x + track.w * 0.5, track.y + 8.0);
    rig.settle(5);
    let pressed = compact.settings.borrow().native.threshold_percent;
    rig.press(Key::Right);
    assert_eq!(compact.settings.borrow().native.threshold_percent, pressed + 1);
    rig.press(Key::End);
    assert_eq!(compact.settings.borrow().native.threshold_percent, 97);
    // Up and Down in a field move what it says, and nothing is saved until Enter.
    let area = rig.rect(("field", percent_field_key(true)).hash_id());
    rig.click_at(area.x + area.w * 0.5, area.y + area.h * 0.5);
    rig.press(Key::Down);
    rig.settle(3);
    assert_eq!(compact.settings.borrow().native.threshold_percent, 97);
    assert!(rig.says("96"));
    rig.press(Key::Enter);
    assert_eq!(compact.settings.borrow().native.threshold_percent, 96);
    let area = rig.rect(("field", retained_field_key()).hash_id());
    rig.click_at(area.x + area.w * 0.5, area.y + area.h * 0.5);
    let before = compact.settings.borrow().native.retained_tokens;
    rig.press(Key::Up);
    rig.settle(3);
    rig.press(Key::Enter);
    assert_eq!(compact.settings.borrow().native.retained_tokens, before + 1000);
    // What a number input would not take is not typed.
    let area = rig.rect(("field", percent_field_key(true)).hash_id());
    rig.click_at(area.x + area.w * 0.5, area.y + area.h * 0.5);
    rig.type_text("x");
    rig.settle(3);
    assert!(rig.says("96"));
}

#[test]
fn the_threshold_mark_says_which_method_it_belongs_to() {
    let (mut rig, compact) = compact_rig(true, true, None);
    rig.click(meter_trigger_key());
    rig.settle(30);
    assert!(rig.says("threshold:80"));
    assert!(!rig.says("threshold:dashed"));
    rig.click(meter_trigger_key());
    rig.settle(10);
    *compact.method.borrow_mut() = Some(CompactionMethod::Native);
    rig.click(meter_trigger_key());
    rig.settle(30);
    assert!(rig.says("threshold:90"));
    assert!(rig.says("threshold:dashed"));
}

#[test]
fn a_method_only_the_handler_holds_back_gives_no_reason_it_would_be_wrong_about() {
    // Available, but nobody to tell: the tab can't be pressed and says nothing of the model.
    let mut rig = meter_rig(|| ContextUsageProps { auto_compact: Some(default_auto_compact_settings()), on_auto_compact_change: Some(Rc::new(|_, _| {})), native_compaction_available: true, on_compaction_method_change: None, ..base() });
    open_submenu(&mut rig);
    let tab = rig.rect(method_tab_key(true));
    rig.click_at(tab.x + tab.w * 0.5, tab.y + tab.h * 0.5);
    rig.settle(3);
    assert!(rig.says("Hands off at 800k tokens"), "still on the handoff's page");
}

#[test]
fn disables_the_threshold_while_it_is_off_and_says_when_there_is_no_window_to_measure() {
    let mut rig = meter_rig(|| {
        let mut settings = default_auto_compact_settings();
        settings.enabled = false;
        ContextUsageProps { auto_compact: Some(settings), on_auto_compact_change: Some(Rc::new(|_, _| {})), context_window: None, ..base() }
    });
    open_submenu(&mut rig);
    assert!(rig.says("The current model has no context window set, so it cannot auto-compact"));
    let track = rig.rect(slider_key(false));
    rig.click_at(track.x + 5.0, track.y + 8.0);
    rig.settle(10);
    assert!(rig.says("80"), "held");
}

#[test]
fn native_compaction_is_one_of_the_two_methods_with_a_threshold_and_a_budget_of_its_own() {
    let (mut rig, compact) = compact_rig(true, true, None);
    *compact.method.borrow_mut() = Some(CompactionMethod::Native);
    open_submenu(&mut rig);
    let words = rig.words();
    assert!(words.iter().any(|said| said == "Native compaction"));
    assert!(words.iter().any(|said| said == "Compacts at 900k tokens"));
    assert!(words.iter().any(|said| said == "Native 90%"));
    commit_field(&mut rig, ("field", percent_field_key(true)).hash_id(), "70");
    assert_eq!(compact.settings.borrow().native.threshold_percent, 70);
    assert_eq!(compact.settings.borrow().threshold_percent, 80, "never the handoff's");
    commit_field(&mut rig, ("field", retained_field_key()).hash_id(), "32000");
    assert_eq!(compact.settings.borrow().native.retained_tokens, 32000);
    rig.click(method_tab_key(false));
    assert_eq!(*compact.method.borrow(), Some(CompactionMethod::Handoff), "the choice is the conversation's");
}

#[test]
fn compacts_at_once_from_its_page_and_holds_that_back_while_it_is_out_of_reach() {
    let (mut rig, compact) = compact_rig(true, true, None);
    *compact.method.borrow_mut() = Some(CompactionMethod::Native);
    open_submenu(&mut rig);
    rig.click(compact_now_key());
    assert_eq!(compact.now.get(), 1);
    assert!(!rig.has(panel_key(METER_KEY)), "the panel closes");
}

#[test]
fn holds_back_compacting_at_once_while_it_is_out_of_reach() {
    let (mut held, compact) = compact_rig(true, true, Some("The conversation is running; stop it to compact"));
    *compact.method.borrow_mut() = Some(CompactionMethod::Native);
    open_submenu(&mut held);
    assert!(held.says("The conversation is running; stop it to compact"));
    held.click(compact_now_key());
    assert_eq!(compact.now.get(), 0);
}

#[test]
fn stays_reachable_on_a_model_that_cannot_hand_off() {
    let (mut rig, _) = compact_rig(true, false, None);
    open_submenu(&mut rig);
    assert!(rig.says("Enable auto-compact"));
}

#[test]
fn goes_out_of_reach_only_when_neither_method_applies() {
    let (mut none, _) = compact_rig(false, false, None);
    none.click(meter_trigger_key());
    none.settle(30);
    assert!(none.says("Not for this model"));
    none.click(compact_row_key());
    none.settle(20);
    assert!(!none.says("Enable auto-compact"));
}

#[test]
fn counts_a_compactions_item_as_compacted_history() {
    let mut rig = meter_rig(|| {
        let compaction = serde_json::from_value(json!({ "providerId": "p", "model": "m", "parts": [], "tokensBefore": 500000, "tokensAfter": 100000, "retained": [{ "role": "user", "sourceId": "u1", "content": "kept" }] })).unwrap();
        let card: ContextItem = serde_json::from_value(json!({ "id": "c1", "kind": "system", "content": "", "createdAt": "2026-08-27T00:00:00Z" })).unwrap();
        ContextUsageProps { contexts: vec![card], compaction: Some(compaction), tokens: 150_000, ..base() }
    });
    rig.click(meter_trigger_key());
    rig.settle(30);
    assert!(rig.says("Compacted history"));
}

/// Keys as `rect_of` wants them, for ids that are hashes of hashes.
trait HashId {
    fn hash_id(self) -> u64;
}

impl<T: std::hash::Hash> HashId for T {
    fn hash_id(self) -> u64 {
        hash_key(self)
    }
}
