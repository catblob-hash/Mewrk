//! `PaneTiles`' arithmetic, and its handles driven by the pointer.

use std::collections::BTreeMap;

use mewrk_client::prelude::*;
use mewrk_shell::pane_tiles::*;

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn a_column_that_lost_its_panes_keeps_its_index_and_a_stray_pane_gets_a_column() {
    let columns = vec![ids(&["gone"]), ids(&["a", "b"])];
    let arranged = arrange_columns(&ids(&["a", "b", "stray"]), &columns, &[3.0, 2.0, 5.0]);
    assert_eq!(arranged.len(), 2);
    assert_eq!((arranged[0].index, arranged[0].weight), (1, 2.0));
    assert_eq!(arranged[0].panes, ids(&["a", "b"]));
    assert_eq!((arranged[1].index, arranged[1].weight), (2, 5.0));
    assert_eq!(arranged[1].panes, ids(&["stray"]));
}

#[test]
fn the_field_splits_by_weight_until_a_minimum_wins() {
    let (chat, side) = split_field(1012.0, 2.0, 1);
    assert_eq!((chat, side), (500.0, 500.0));
    // A side area below its floor takes from the chat.
    let (chat, side) = split_field(800.0, 0.25, 1);
    assert_eq!(side, MIN_SIDE_WIDTH);
    assert_eq!(chat, 800.0 - GAP - MIN_SIDE_WIDTH);
    // Three columns need more room than one before any is too narrow to use.
    assert_eq!(minimum_side_width(3), 3.0 * MIN_COLUMN_WIDTH + 2.0 * GAP);
    let (chat, _) = split_field(500.0, 8.0, 1);
    assert_eq!(chat, MIN_CHAT_WIDTH);
}

#[test]
fn tiles_fill_their_column_less_the_gaps_and_a_solo_pane_takes_the_whole_field() {
    let columns = vec![ids(&["a", "b"]), ids(&["c"])];
    let flex = BTreeMap::new();
    let layout = tile_layout((1000.0, 608.0), 8.0, &ids(&["a", "b", "c"]), &columns, 2.0, &[1.0, 1.0], &flex, None);
    let rect_of_tile = |id: &str| layout.tiles.iter().find(|(pane, _)| pane == id).unwrap().1;
    let (a, b, c) = (rect_of_tile("a"), rect_of_tile("b"), rect_of_tile("c"));
    assert_eq!(a.x, layout.side.x);
    assert_eq!(a.w, b.w);
    assert!((b.y - (a.bottom() + GAP)).abs() < 0.01, "{a:?} {b:?}");
    assert!((b.bottom() - layout.side.bottom()).abs() < 0.01);
    assert!((c.x - (a.right() + GAP)).abs() < 0.01);
    assert!((c.right() - layout.side.right()).abs() < 0.01);
    assert_eq!(c.h, layout.side.h);
    assert_eq!(layout.chat.h, 600.0);

    let solo = tile_layout((1000.0, 608.0), 8.0, &ids(&["a", "b", "c"]), &columns, 2.0, &[1.0, 1.0], &flex, Some("b"));
    assert_eq!(solo.solo.as_deref(), Some("b"));
    assert_eq!(solo.tiles.iter().find(|(pane, _)| pane == "b").unwrap().1, rect(8.0, 8.0, 992.0, 600.0));
    // A pane whose subject vanished is not visible; expanding it would leave an empty workspace.
    let gone = tile_layout((1000.0, 608.0), 8.0, &ids(&["a"]), &columns, 2.0, &[1.0, 1.0], &flex, Some("b"));
    assert_eq!(gone.solo, None);
}

#[test]
fn the_side_handle_keeps_under_the_pointer_and_stops_at_the_minimums() {
    let measure = Measure::Side { chat_w: 600.0, side_w: 400.0, minimum: MIN_CHAT_WIDTH - 600.0, maximum: 400.0 - MIN_SIDE_WIDTH, start_flex: 4.0 / 3.0 };
    let flex = |delta: f32| match measure.apply(delta) {
        ResizeChange::SideFlex(flex) => flex,
        other => panic!("{other:?}"),
    };
    // Moved 100 right: 700 and 300, whichever way the ratio is read back.
    let f = flex(100.0);
    assert!((f - 300.0 / 700.0 * 2.0).abs() < 1e-6);
    // Past the side's floor, it stays there.
    assert_eq!(flex(10_000.0), flex(120.0));
    assert_eq!(flex(-10_000.0), flex(-280.0));
    assert_eq!(measure.restore(), ResizeChange::SideFlex(4.0 / 3.0));
}

#[test]
fn a_column_under_its_floor_can_still_be_dragged_wider() {
    let measure = Measure::Columns { first_w: 150.0, second_w: 500.0, sum: 2.0, first_index: 0, second_index: 1, given: vec![1.0, 1.0], minimum: 0.0, maximum: 300.0 };
    let ResizeChange::ColumnFlex(next) = measure.apply(-50.0) else { panic!() };
    assert_eq!(next, vec![150.0 / 650.0 * 2.0, 2.0 - 150.0 / 650.0 * 2.0]);
    let ResizeChange::ColumnFlex(wider) = measure.apply(100.0) else { panic!() };
    assert!(wider[0] > next[0]);
}

#[test]
fn keys_move_a_handle_and_home_and_end_run_the_other_way_on_the_chats() {
    let press = |key| KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false };
    let measure = Measure::Side { chat_w: 600.0, side_w: 400.0, minimum: -280.0, maximum: 120.0, start_flex: 1.0 };
    assert_eq!(keyboard_delta(&press(Key::Left), true, &measure, true), Some(-24.0));
    assert_eq!(keyboard_delta(&KeyPress { shift: true, ..press(Key::Right) }, true, &measure, true), Some(48.0));
    assert_eq!(keyboard_delta(&press(Key::Up), true, &measure, true), None);
    assert_eq!(keyboard_delta(&press(Key::Home), true, &measure, true), Some(120.0));
    assert_eq!(keyboard_delta(&press(Key::End), true, &measure, false), Some(120.0));
}
