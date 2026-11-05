//! Port of `lib/timelineHistory.test.ts`.

use std::collections::HashSet;

use super::fixtures::user;
use crate::model::ContextItem;
use crate::runs::timeline_history::*;

fn message(id: &str) -> ContextItem {
    user(id, id)
}

fn list(names: &[&str]) -> Vec<ContextItem> {
    names.iter().map(|id| message(id)).collect()
}

fn set(names: &[&str]) -> HashSet<String> {
    names.iter().map(|id| (*id).to_owned()).collect()
}

fn ids(contexts: &[ContextItem]) -> Vec<&str> {
    contexts.iter().map(ContextItem::id).collect()
}

fn removed_patch(contexts: &[ContextItem], names: &[&str]) -> TimelinePatch {
    TimelinePatch { removed: placed_contexts(contexts, &set(names)), ..Default::default() }
}

#[test]
fn takes_contexts_out_and_puts_them_back_where_they_were() {
    let contexts = list(&["a", "b", "c", "d"]);
    let patch = removed_patch(&contexts, &["b", "c"]);
    let deleted = apply_timeline_patch(&contexts, &patch).unwrap();
    assert_eq!(ids(&deleted), ["a", "d"]);
    assert_eq!(ids(&apply_timeline_patch(&deleted, &invert_timeline_patch(&patch)).unwrap()), ["a", "b", "c", "d"]);
}

#[test]
fn puts_a_context_back_after_the_neighbour_it_had_wherever_that_has_moved_since() {
    let contexts = list(&["a", "b", "c"]);
    let patch = removed_patch(&contexts, &["c"]);
    // Two contexts went in above it after it was deleted, so its old index is wrong now.
    let later = list(&["x", "y", "a", "b", "reply"]);
    assert_eq!(ids(&apply_timeline_patch(&later, &invert_timeline_patch(&patch)).unwrap()), ["x", "y", "a", "b", "c", "reply"]);
}

#[test]
fn keeps_what_a_model_run_appended_between_an_edit_and_its_undo() {
    let contexts = list(&["a"]);
    let inserted = insertion_patch(&contexts, message("placed"), 1);
    let mut after_run = apply_timeline_patch(&contexts, &inserted).unwrap();
    after_run.push(message("reply"));
    assert_eq!(ids(&apply_timeline_patch(&after_run, &invert_timeline_patch(&inserted)).unwrap()), ["a", "reply"]);
}

#[test]
fn rewrites_a_context_in_place_in_both_directions() {
    let (before, after) = (user("a", "old"), user("a", "new"));
    let patch = TimelinePatch { replaced: vec![(before.clone(), after.clone())], ..Default::default() };
    assert_eq!(apply_timeline_patch(&[before.clone(), message("b")], &patch).unwrap()[0], after);
    assert_eq!(apply_timeline_patch(&[after, message("b")], &invert_timeline_patch(&patch)).unwrap()[0], before);
}

#[test]
fn lands_nowhere_once_nothing_it_touches_is_there_to_change() {
    let patch = removed_patch(&list(&["gone"]), &["gone"]);
    assert_eq!(apply_timeline_patch(&list(&["other"]), &patch), None);
}

fn entry(label: &str) -> TimelineHistoryEntry {
    TimelineHistoryEntry { patch: TimelinePatch::default(), label: label.into() }
}

fn label(entry: Option<&TimelineHistoryEntry>) -> Option<&str> {
    entry.map(|entry| entry.label.as_str())
}

#[test]
fn keeps_each_timelines_edits_apart() {
    let mut history = TimelineHistory::default();
    history.record("conv_a", entry("a1"));
    history.record("conv_b", entry("b1"));
    assert_eq!(label(history.next_undo("conv_a")), Some("a1"));
    assert_eq!(label(history.next_undo("conv_b")), Some("b1"));
    // A fork or a hand-over is a new id, and a new id has nothing on record.
    assert_eq!(label(history.next_undo("conv_a_fork")), None);
}

#[test]
fn moves_an_edit_between_undo_and_redo_and_a_new_edit_ends_the_redo() {
    let mut history = TimelineHistory::default();
    history.record("conv", entry("first"));
    history.record("conv", entry("second"));
    history.undone("conv");
    assert_eq!(label(history.next_undo("conv")), Some("first"));
    assert_eq!(label(history.next_redo("conv")), Some("second"));
    history.redone("conv");
    assert_eq!(label(history.next_undo("conv")), Some("second"));
    assert_eq!(label(history.next_redo("conv")), None);
    history.undone("conv");
    history.record("conv", entry("third"));
    assert_eq!(label(history.next_redo("conv")), None);
}

#[test]
fn shares_one_budget_across_timelines_and_lets_the_oldest_edit_go_first() {
    let mut history = TimelineHistory::with_limit(3);
    history.record("conv_a", entry("a1"));
    history.record("conv_b", entry("b1"));
    history.record("conv_a", entry("a2"));
    history.record("conv_b", entry("b2"));
    assert_eq!(history.size(), 3);
    history.undone("conv_a");
    history.undone("conv_a");
    // "a1" was the oldest of all, so it went; "a2" is still there to undo and redo.
    assert_eq!(label(history.next_undo("conv_a")), None);
    assert_eq!(label(history.next_redo("conv_a")), Some("a2"));
    assert_eq!(label(history.next_undo("conv_b")), Some("b2"));
}

#[test]
fn counts_what_is_waiting_to_be_redone_against_the_budget_too() {
    let mut history = TimelineHistory::with_limit(2);
    history.record("conv_a", entry("a1"));
    history.undone("conv_a");
    history.record("conv_b", entry("b1"));
    history.record("conv_b", entry("b2"));
    assert_eq!(history.size(), 2);
    assert_eq!(label(history.next_redo("conv_a")), None);
}

#[test]
fn forgets_a_timeline_outright() {
    let mut history = TimelineHistory::default();
    history.record("conv", entry("a"));
    history.forget("conv");
    assert_eq!(label(history.next_undo("conv")), None);
    assert_eq!(history.size(), 0);
}

fn press(key: &str, ctrl: bool, meta: bool, alt: bool, shift: bool) -> TimelineKeyPress<'_> {
    TimelineKeyPress { key, ctrl, meta, alt, shift }
}

#[test]
fn reads_ctrl_z_as_undo_and_ctrl_x_as_redo_and_nothing_else() {
    assert_eq!(timeline_history_key(press("z", true, false, false, false), false), Some(TimelineHistoryAction::Undo));
    assert_eq!(timeline_history_key(press("X", true, false, false, false), false), Some(TimelineHistoryAction::Redo));
    assert_eq!(timeline_history_key(press("z", false, false, false, false), false), None);
    assert_eq!(timeline_history_key(press("z", true, false, false, true), false), None);
    assert_eq!(timeline_history_key(press("c", true, false, false, false), false), None);
    assert_eq!(timeline_history_key(press("z", false, true, false, false), true), Some(TimelineHistoryAction::Undo));
}

#[test]
fn reads_ctrl_c_as_copy_and_ctrl_v_as_paste_in_either_case() {
    for (key, action) in [("c", TimelineClipboardAction::Copy), ("v", TimelineClipboardAction::Paste), ("C", TimelineClipboardAction::Copy), ("V", TimelineClipboardAction::Paste)] {
        assert_eq!(timeline_clipboard_key(press(key, true, false, false, false), false), Some(action));
    }
}

#[test]
fn ignores_a_press_with_alt_or_shift_held_without_the_modifier_or_on_another_letter() {
    assert_eq!(timeline_clipboard_key(press("c", true, false, true, false), false), None);
    assert_eq!(timeline_clipboard_key(press("v", true, false, true, false), false), None);
    assert_eq!(timeline_clipboard_key(press("c", true, false, false, true), false), None);
    assert_eq!(timeline_clipboard_key(press("v", true, false, false, true), false), None);
    assert_eq!(timeline_clipboard_key(press("c", false, false, false, false), false), None);
    assert_eq!(timeline_clipboard_key(press("z", true, false, false, false), false), None);
    assert_eq!(timeline_clipboard_key(press("x", true, false, false, false), false), None);
}

#[test]
fn does_not_take_cmd_for_the_modifier_off_a_mac_nor_ctrl_and_cmd_together() {
    assert_eq!(timeline_clipboard_key(press("c", false, true, false, false), false), None);
    assert_eq!(timeline_clipboard_key(press("v", false, true, false, false), false), None);
    assert_eq!(timeline_clipboard_key(press("c", true, true, false, false), false), None);
}

#[test]
fn writes_the_keycaps_as_this_platform_does() {
    assert_eq!(timeline_clipboard_shortcut(TimelineClipboardAction::Copy, false), "Ctrl+C");
    assert_eq!(timeline_clipboard_shortcut(TimelineClipboardAction::Paste, true), "⌘V");
}

fn abcde() -> Vec<ContextItem> {
    list(&["a", "b", "c", "d", "e"])
}

/// The order a move leaves, or `None` when it is no change at all.
fn moved(contexts: &[ContextItem], picked: &[&str], index: isize) -> Option<Vec<String>> {
    let patch = move_patch(contexts, &set(picked), index)?;
    Some(apply_timeline_patch(contexts, &patch)?.iter().map(|context| context.id().to_owned()).collect())
}

#[test]
fn gathers_scattered_contexts_together_in_list_order_at_the_insertion_index() {
    assert_eq!(moved(&abcde(), &["a", "d"], 3).unwrap(), ["b", "c", "a", "d", "e"]);
    assert_eq!(moved(&abcde(), &["b", "e"], 0).unwrap(), ["b", "e", "a", "c", "d"]);
    assert_eq!(moved(&abcde(), &["a"], 5).unwrap(), ["b", "c", "d", "e", "a"]);
    // Whatever order the ids were picked in.
    assert_eq!(moved(&abcde(), &["d", "a"], 3).unwrap(), ["b", "c", "a", "d", "e"]);
}

#[test]
fn reads_the_index_as_an_insertion_line_in_the_list_as_it_was_before_the_move() {
    // The line before "e": "a" and "b" sit above it and leave, so it ends up past "c" and "d".
    assert_eq!(moved(&abcde(), &["a", "b"], 4).unwrap(), ["c", "d", "a", "b", "e"]);
    assert_eq!(moved(&abcde(), &["a"], 1), None);
    assert_eq!(moved(&abcde(), &["a"], 2).unwrap(), ["b", "a", "c", "d", "e"]);
    assert_eq!(moved(&abcde(), &["e"], 0).unwrap(), ["e", "a", "b", "c", "d"]);
}

#[test]
fn clamps_an_index_outside_the_list_to_its_ends() {
    assert_eq!(moved(&abcde(), &["b"], 99).unwrap(), ["a", "c", "d", "e", "b"]);
    assert_eq!(moved(&abcde(), &["d"], -3).unwrap(), ["d", "a", "b", "c", "e"]);
}

#[test]
fn a_move_is_none_when_nothing_would_change() {
    // "b" and "c" are already together, and these indexes sit at or inside their run.
    for index in [1, 2, 3] {
        assert_eq!(move_patch(&abcde(), &set(&["b", "c"]), index), None);
    }
    assert_eq!(move_patch(&abcde(), &set(&[]), 2), None);
    assert_eq!(move_patch(&abcde(), &set(&["nope", "gone"]), 2), None);
    assert_eq!(move_patch(&[], &set(&["a"]), 0), None);
}

#[test]
fn a_move_is_some_when_scattered_contexts_go_where_one_of_them_already_sits() {
    // Gathering "a" and "c" is a change even though "a" does not move.
    assert_eq!(moved(&abcde(), &["a", "c"], 0).unwrap(), ["a", "c", "b", "d", "e"]);
}

#[test]
fn ignores_ids_that_are_not_in_the_list_and_moves_the_rest() {
    assert_eq!(moved(&abcde(), &["a", "nope"], 5).unwrap(), ["b", "c", "d", "e", "a"]);
}

#[test]
fn a_move_is_undone_by_the_patch_read_the_other_way_and_redone_by_applying_it_again() {
    let cases: [(&[&str], isize, [&str; 5]); 5] = [
        (&["a", "d"], 3, ["b", "c", "a", "d", "e"]),
        (&["b", "e"], 0, ["b", "e", "a", "c", "d"]),
        (&["a", "c"], 0, ["a", "c", "b", "d", "e"]),
        (&["a"], 5, ["b", "c", "d", "e", "a"]),
        (&["a", "b"], 4, ["c", "d", "a", "b", "e"]),
    ];
    for (picked, index, order) in cases {
        let contexts = abcde();
        let patch = move_patch(&contexts, &set(picked), index).unwrap();
        let after = apply_timeline_patch(&contexts, &patch).unwrap();
        assert_eq!(ids(&after), order);
        let restored = apply_timeline_patch(&after, &invert_timeline_patch(&patch)).unwrap();
        assert_eq!(restored, contexts);
        assert_eq!(ids(&apply_timeline_patch(&restored, &patch).unwrap()), order);
    }
}

#[test]
fn keeps_what_a_model_run_appended_between_a_move_and_its_undo() {
    let contexts = abcde();
    let patch = move_patch(&contexts, &set(&["a", "d"]), 3).unwrap();
    let mut after_run = apply_timeline_patch(&contexts, &patch).unwrap();
    after_run.push(message("f"));
    let undone = apply_timeline_patch(&after_run, &invert_timeline_patch(&patch)).unwrap();
    assert_eq!(ids(&undone), ["a", "b", "c", "d", "e", "f"]);
    assert_eq!(ids(&apply_timeline_patch(&undone, &patch).unwrap()), ["b", "c", "a", "d", "e", "f"]);
}

fn pasted() -> Vec<ContextItem> {
    list(&["x", "y"])
}

fn pasted_order(contexts: &[ContextItem], put: &[ContextItem], index: isize, replaced: &[&str]) -> Vec<String> {
    let patch = paste_patch(contexts, put, index, &set(replaced)).unwrap();
    apply_timeline_patch(contexts, &patch).unwrap().iter().map(|context| context.id().to_owned()).collect()
}

#[test]
fn puts_the_pasted_contexts_where_the_index_points() {
    let abc = list(&["a", "b", "c"]);
    assert_eq!(pasted_order(&abc, &pasted(), 2, &[]), ["a", "b", "x", "y", "c"]);
    assert_eq!(pasted_order(&abc, &pasted(), 0, &[]), ["x", "y", "a", "b", "c"]);
    assert_eq!(pasted_order(&abc, &pasted(), 3, &[]), ["a", "b", "c", "x", "y"]);
    assert_eq!(pasted_order(&abc, &pasted(), 99, &[]), ["a", "b", "c", "x", "y"]);
    assert_eq!(pasted_order(&abc, &pasted(), -4, &[]), ["x", "y", "a", "b", "c"]);
    assert_eq!(pasted_order(&[], &pasted(), 5, &[]), ["x", "y"]);
}

#[test]
fn a_paste_is_none_when_nothing_was_pasted() {
    assert_eq!(paste_patch(&list(&["a"]), &[], 1, &set(&[])), None);
    assert_eq!(paste_patch(&list(&["a"]), &[], 1, &set(&["a"])), None);
}

#[test]
fn puts_in_exactly_the_pasted_objects_and_takes_nothing_out() {
    let patch = paste_patch(&list(&["a", "b", "c"]), &pasted(), 1, &set(&[])).unwrap();
    assert!(patch.removed.is_empty());
    assert!(patch.replaced.is_empty());
    assert_eq!(patch.inserted.iter().map(|entry| entry.context.clone()).collect::<Vec<_>>(), pasted());
}

#[test]
fn a_paste_is_undone_by_the_patch_read_the_other_way_and_redone_by_applying_it_again() {
    let contexts = list(&["a", "b", "c"]);
    let patch = paste_patch(&contexts, &pasted(), 1, &set(&[])).unwrap();
    let after = apply_timeline_patch(&contexts, &patch).unwrap();
    assert_eq!(ids(&after), ["a", "x", "y", "b", "c"]);
    let restored = apply_timeline_patch(&after, &invert_timeline_patch(&patch)).unwrap();
    assert_eq!(ids(&restored), ["a", "b", "c"]);
    assert_eq!(ids(&apply_timeline_patch(&restored, &patch).unwrap()), ["a", "x", "y", "b", "c"]);
}

#[test]
fn puts_a_contiguous_runs_replacement_where_the_run_sat_whatever_the_index() {
    let abcd = list(&["a", "b", "c", "d"]);
    for index in [0, 1, 2, 3, 4, 99] {
        assert_eq!(pasted_order(&abcd, &pasted(), index, &["b", "c"]), ["a", "x", "y", "d"]);
    }
}

#[test]
fn puts_scattered_contexts_replacement_where_the_first_of_them_sat_and_just_removes_the_rest() {
    let abcd = list(&["a", "b", "c", "d"]);
    for index in [0, 2, 4] {
        assert_eq!(pasted_order(&abcd, &pasted(), index, &["a", "c"]), ["x", "y", "b", "d"]);
    }
    assert_eq!(pasted_order(&abcd, &pasted(), 0, &["b", "d"]), ["a", "x", "y", "c"]);
}

#[test]
fn uses_the_index_when_none_of_the_ids_to_replace_is_in_the_list() {
    assert_eq!(pasted_order(&list(&["a", "b", "c", "d"]), &pasted(), 2, &["gone"]), ["a", "b", "x", "y", "c", "d"]);
}

#[test]
fn takes_out_the_contexts_it_replaces_as_they_sat() {
    let patch = paste_patch(&list(&["a", "b", "c", "d"]), &pasted(), 0, &set(&["a", "c"])).unwrap();
    assert_eq!(removed_ids(&patch), ["a", "c"]);
}

#[test]
fn a_replacement_is_undone_to_the_exact_original_list_and_redone_by_applying_it_again() {
    for replaced in [&["b", "c"][..], &["a", "c"][..]] {
        let contexts = list(&["a", "b", "c", "d"]);
        let patch = paste_patch(&contexts, &pasted(), 3, &set(replaced)).unwrap();
        let after = apply_timeline_patch(&contexts, &patch).unwrap();
        let restored = apply_timeline_patch(&after, &invert_timeline_patch(&patch)).unwrap();
        assert_eq!(restored, contexts);
        assert_eq!(ids(&apply_timeline_patch(&restored, &patch).unwrap()), ids(&after));
    }
}

#[test]
fn replaces_everything_when_every_context_is_picked() {
    assert_eq!(pasted_order(&list(&["a", "b", "c", "d"]), &pasted(), 2, &["a", "b", "c", "d"]), ["x", "y"]);
}

mod through_the_app {
    use super::*;
    use crate::runs::tests::rig::*;

    fn delete(rig: &mut Rig, id: &str) -> bool {
        let contexts = rig.conversation().contexts.clone();
        let removed = placed_contexts(&contexts, &set(&[id]));
        rig.app.commit_timeline_edit(CONV, TimelinePatch { removed, ..Default::default() }, "delete", Some("deleted".to_owned()))
    }

    #[test]
    fn runs_an_undo_keeps_a_reply_the_model_added_since_the_edit() {
        let mut rig = Rig::new();
        rig.adopted();
        rig.set_contexts(vec![message("a"), message("b")]);
        assert!(delete(&mut rig, "b"));
        assert_eq!(rig.context_ids(), ["a"]);
        assert!(rig.app.timeline_notice().is_some());
        rig.app.doc.workspaces[0].conversations[0].contexts.push(message("reply"));
        rig.app.step_timeline_history(CONV, TimelineHistoryAction::Undo);
        assert_eq!(rig.context_ids(), ["a", "b", "reply"]);
        rig.app.step_timeline_history(CONV, TimelineHistoryAction::Redo);
        assert_eq!(rig.context_ids(), ["a", "reply"]);
    }

    #[test]
    fn runs_nothing_moves_on_the_timeline_while_a_turn_is_in_progress() {
        let mut rig = Rig::new();
        rig.adopted();
        rig.set_contexts(vec![message("a"), message("b")]);
        assert!(delete(&mut rig, "b"));
        rig.app.send_message();
        let run = rig.wait_for_run(1);
        rig.app.step_timeline_history(CONV, TimelineHistoryAction::Undo);
        assert_eq!(rig.context_ids(), ["a"], "the undo waits for the turn");
        assert!(rig.state().timeline_history.next_undo(CONV).is_some(), "and stays on the stack");
        run.finish(response(serde_json::json!([])));
        rig.run_exits();
        rig.app.step_timeline_history(CONV, TimelineHistoryAction::Undo);
        assert!(rig.context_ids().contains(&"b".to_owned()));
    }

    #[test]
    fn runs_an_edit_that_no_longer_lands_says_so_and_leaves_the_stack() {
        let mut rig = Rig::new();
        rig.adopted();
        rig.set_contexts(vec![message("a"), message("b")]);
        assert!(delete(&mut rig, "b"));
        rig.app.doc.workspaces[0].conversations[0].contexts.push(message("b"));
        rig.app.step_timeline_history(CONV, TimelineHistoryAction::Undo);
        assert!(rig.state().timeline_history.next_undo(CONV).is_none());
        assert!(rig.app.timeline_notice().is_some());
    }
}
