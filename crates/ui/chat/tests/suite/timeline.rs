//! `ContextStream.test.tsx`, `TimelineBlock.test.tsx`, `timelineClipboard.test.ts`: the timeline's logic and what it does under a
//! scripted pointer and keyboard. Assertions read what a card reads, not a DOM.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use fluent::el::*;
use fluent::ui::Key;
use mewrk_client::model::{ContextItem, ToolCategory};
use mewrk_client::prelude::E;
use mewrk_client::runs::{ConversationTurn, ConversationTurnError, ConversationTurnStatus, LiveContext, StreamingToolPhase};
use mewrk_client::App;
use mewrk_ui_chat::clipboard::*;
use mewrk_ui_chat::drag::{Look, MarkState};
use mewrk_ui_chat::frame::{timeline_snapshot, unit_key, MenuView};
use mewrk_ui_chat::inline_text_editor::InsertKind;
use mewrk_ui_chat::item::Item;
use mewrk_ui_chat::nodes::*;
use mewrk_ui_chat::props::ConversationViewProps;
use mewrk_ui_chat::stream::{insertable_tool_groups, step_follow_glide};
use mewrk_ui_chat::*;
use mewrk_ui_common::context_menu::menu_row_key;
use serde_json::json;

use crate::fixture::*;
use crate::rig::Rig;

const ID: &str = "t";

fn shape(contexts: &[ContextItem]) -> Vec<String> {
    let items: Vec<Item<'_>> = contexts.iter().map(Item::settled).collect();
    shape_of(&items, &HashSet::new(), &|_| false)
}

fn shape_of(items: &[Item<'_>], breaks: &HashSet<usize>, views: &dyn Fn(&str) -> bool) -> Vec<String> {
    build_context_render_nodes(items, breaks, |id| views(id))
        .iter()
        .map(|node| match node {
            RenderNode::Context { index } => format!("context:{}", items[*index].id()),
            RenderNode::Block { entries, .. } => format!("block:{}", entries.iter().map(|entry| items[entry.index].id()).collect::<Vec<_>>().join(",")),
            RenderNode::Question { entry, answer, .. } => format!("question:{}{}", items[*entry].id(), answer.map_or(String::new(), |answer| format!("+{}", items[answer].id()))),
        })
        .collect()
}

// ---------------------------------------------------------------- Resolved

#[test]
fn draws_a_background_task_overlay_over_its_card_in_place_where_a_run_projection_is_appended() {
    use mewrk_client::runs::{SubagentLiveState, SubagentLiveStatus};
    use mewrk_ui_chat::frame::Resolved;
    let persisted = vec![user("u", "dispatch a scout"), tool("spawn", "agent_spawn", json!({ "task": "map the repo" }), "started"), assistant("a", "turn over")];
    let mut card = streaming(persisted[1].clone(), Some(StreamingToolPhase::Completed));
    card.live = Some(std::sync::Arc::new(SubagentLiveState { status: Some(SubagentLiveStatus::Running), ..SubagentLiveState::default() }));
    let live = vec![card];
    let ids = |resolved: &Resolved<'_>| (0..resolved.count()).map(|index| resolved.get(index).id().to_owned()).collect::<Vec<_>>();

    let in_place = Resolved { persisted: &persisted, live: &live, map: Rc::new(Resolved::map_in_place(&persisted, &live)) };
    assert_eq!(ids(&in_place), ["u", "spawn", "a"]);
    let spawn = in_place.get(1);
    assert!(spawn.streaming);
    assert_eq!(spawn.live.and_then(|live| live.status), Some(SubagentLiveStatus::Running));
    assert!(in_place.get(0).live.is_none() && !in_place.get(2).streaming);

    // A run's projection is the run's own tail, so it follows what is persisted.
    let appended = Resolved { persisted: &persisted, live: &live, map: Rc::new(Resolved::map_of(&persisted, &live)) };
    assert_eq!(ids(&appended), ["u", "a", "spawn"]);
}

// ---------------------------------------------------------------- buildContextRenderNodes

#[test]
fn keeps_empty_model_turn_anchors_out_of_the_visible_timeline() {
    assert_eq!(shape(&[user("u", "hi"), assistant("anchor", ""), tool("t", "read", json!({}), "x"), assistant("a", "done")]), ["context:u", "block:t", "context:a"]);
}

#[test]
fn keeps_a_summary_less_reasoning_round_visible_when_it_carries_its_time_or_tokens() {
    let mut timed = reasoning("r1", None);
    if let ContextItem::Reasoning { duration_ms, .. } = &mut timed {
        *duration_ms = Some(10);
    }
    let mut counted = reasoning("r2", None);
    if let ContextItem::Reasoning { tokens, .. } = &mut counted {
        *tokens = Some(10);
    }
    assert_eq!(shape(&[timed, counted, reasoning("r3", None)]), ["block:r1,r2"]);
}

#[test]
fn drops_an_empty_encrypted_reasoning_record_while_it_is_still_streaming() {
    let live_card = LiveContext { item: reasoning("r", None), streaming: Some(true), stream_status: None, live: None, started_at: None };
    let after = tool("after", "read", json!({}), "x");
    let items = [Item::live(&live_card), Item::settled(&after)];
    assert_eq!(shape_of(&items, &HashSet::new(), &|_| false), ["block:after"]);
}

#[test]
fn runs_one_block_until_a_message_interrupts_it() {
    assert_eq!(shape(&[tool("a", "read", json!({}), ""), reasoning("r", Some("think")), hook("h", "completed", "", false), assistant("m", "text"), tool("b", "read", json!({}), "")]), ["block:a,r,h", "context:m", "block:b"]);
}

#[test]
fn regroups_rows_after_a_visible_boundary_is_deleted_even_when_a_hidden_anchor_remains() {
    assert_eq!(shape(&[tool("a", "read", json!({}), ""), assistant("anchor", ""), tool("b", "read", json!({}), "")]), ["block:a,b"]);
}

#[test]
fn splits_the_run_at_an_ask_user_instead_of_hoisting_the_question_out_of_the_middle() {
    let question = tool("q", "ask_user", json!({ "questions": [] }), "");
    assert_eq!(shape(&[tool("a", "read", json!({}), ""), question, tool("b", "read", json!({}), "")]), ["block:a", "question:q", "block:b"]);
}

#[test]
fn pairs_the_next_user_boundary_with_a_legacy_ask_user_and_removes_the_standalone_user_node() {
    let question = tool("q", "ask_user", json!({ "questions": [{ "question": "x", "header": "h", "options": [] }] }), "");
    assert_eq!(shape(&[question.clone(), assistant("anchor", ""), user("answer", "yes")]), ["question:q+answer"]);
    // A real turn anchor stays outside the card.
    let contexts = [question, user("next", "something else")];
    let items: Vec<Item<'_>> = contexts.iter().map(Item::settled).collect();
    assert_eq!(shape_of(&items, &HashSet::from([1]), &|_| false), ["question:q", "context:next"]);
}

#[test]
fn drops_an_unfinished_workflow_call_with_no_view_but_keeps_a_settled_or_viewed_one() {
    let running = streaming(tool("w", "workflow", json!({}), ""), Some(StreamingToolPhase::Running));
    let items = [Item::live(&running)];
    assert_eq!(shape_of(&items, &HashSet::new(), &|_| false), Vec::<String>::new());
    assert_eq!(shape_of(&items, &HashSet::new(), &|id| id == "w"), ["block:w"]);
    assert_eq!(shape(&[tool("w", "workflow", json!({}), "ok")]), ["block:w"]);
}

#[test]
fn hoists_an_ordinary_call_that_is_still_in_flight_onto_the_stream_indicator() {
    let running = streaming(tool("t", "read", json!({}), ""), Some(StreamingToolPhase::Running));
    assert_eq!(shape_of(&[Item::live(&running)], &HashSet::new(), &|_| false), Vec::<String>::new());
}

#[test]
fn treats_a_tools_added_record_as_nothing_a_reader_sees() {
    let mut added = system("s", "");
    if let ContextItem::System { tools_added, .. } = &mut added {
        *tools_added = vec!["read".to_owned()];
    }
    assert_eq!(shape(&[added, user("u", "x")]), ["context:u"]);
}

#[test]
fn places_a_failure_notice_after_the_messages_a_turn_still_owns_once_its_anchor_is_deleted() {
    let contexts = [assistant("a", "partial"), assistant("b", "more")];
    let items: Vec<Item<'_>> = contexts.iter().map(Item::settled).collect();
    let nodes = build_context_render_nodes(&items[..], &HashSet::new(), |_| false);
    let turn = |id: &str, ids: &[&str], status, error: bool| ConversationTurn { id: id.to_owned(), request_id: id.to_owned(), anchor_context_id: "gone".to_owned(), model_id: String::new(), started_at: AT.to_owned(), ended_at: None, duration_ms: None, status, context_ids: ids.iter().map(|id| (*id).to_owned()).collect(), usage: Default::default(), usage_offset: Default::default(), usage_baseline: Default::default(), usage_revision_at_start: 0, segment_count: 1, error: error.then(|| ConversationTurnError { message: "boom".to_owned(), provider_name: "P".to_owned(), model_name: "M".to_owned(), at: AT.to_owned() }) };
    let mut opening = turn("t2", &[], ConversationTurnStatus::Running, false);
    opening.anchor_context_id = String::new();
    let turns = [turn("t1", &["a", "b"], ConversationTurnStatus::Completed, true), opening];
    let index_of = |id: &str| items.iter().position(|item| item.id() == id);
    let node_of = |index: usize| nodes.iter().position(|node| node.indexes().contains(&index));
    let projection = project_turns(&turns, index_of, node_of, &items[..]);
    assert_eq!(projection.notices_after_node.get(&1), Some(&vec![0]));
    // Owning nothing and anchored nowhere it leads the stream, rather than drop the only surface a live run has.
    assert_eq!(projection.leading_turns, vec![1]);
    assert!(projection.any_visible_running);
    // A finished turn draws nothing of its own.
    let done = [turn("t3", &["a"], ConversationTurnStatus::Completed, false)];
    assert_eq!(project_turns(&done, index_of, node_of, &items[..]), TurnProjection::default());
}

#[test]
fn maps_insertion_across_undrawn_contexts() {
    assert_eq!(display_insertion_index(&[0, 2, 5], 1, 7), 2);
    assert_eq!(display_insertion_index(&[0, 2, 5], 5, 7), 5);
    assert_eq!(display_insertion_index(&[0, 2, 5], 6, 7), 7);
}

#[test]
fn shows_the_newest_reasoning_line_while_it_streams_and_the_first_once_it_settles() {
    let text = "## Plan\n\n- **read** the file\n3. `then` write";
    assert_eq!(reasoning_line(text, false).as_deref(), Some("Plan"));
    assert_eq!(reasoning_line(text, true).as_deref(), Some("then write"));
    assert_eq!(reasoning_line("##", false).as_deref(), Some("##"));
    assert_eq!(reasoning_line("   \n", false), None);
    let long = reasoning_line(&"x".repeat(400), false).unwrap();
    assert_eq!(long.chars().count(), 220);
    assert!(long.ends_with('…'));
}

#[test]
fn renders_only_http_citations_as_links() {
    assert_eq!(safe_source_href(Some("https://a.example/x")).as_deref(), Some("https://a.example/x"));
    assert_eq!(safe_source_href(Some("javascript:alert(1)")), None);
    assert_eq!(safe_source_href(Some("not a url")), None);
    assert_eq!(source_hostname(Some("https://docs.example.org/p")).as_deref(), Some("docs.example.org"));
}

// ---------------------------------------------------------------- timelineClipboard

fn ids() -> impl FnMut(&str) -> String {
    let mut next = 0;
    move |prefix: &str| {
        next += 1;
        format!("{prefix}-{next}")
    }
}

#[test]
fn copies_plain_contexts_and_refuses_those_whose_meaning_is_their_place_in_a_run() {
    let tools = descriptors();
    for item in [user("u", "x"), assistant("a", "x"), system("s", "x"), reasoning("r", Some("x")), tool("t", "read", json!({}), "x")] {
        assert!(timeline_copyable(&item, false, &tools), "{item:?}");
        assert!(!timeline_copyable(&item, true, &tools), "still streaming: {item:?}");
    }
    assert!(!timeline_copyable(&compaction("c"), false, &tools));
    assert!(!timeline_copyable(&hook("h", "completed", "x", false), false, &tools));
    assert!(!timeline_copyable(&tool("t", "agent_spawn", json!({}), "x"), false, &tools), "orchestration");
    assert!(timeline_copyable(&tool("t", "unknown_tool", json!({}), "x"), false, &tools), "no descriptor at all");
    assert!(!timeline_copyable(&user("ctx_agent-result_1", "x"), false, &tools), "a host card's id is how the host knows it");
    let mut notice = tool("t", "box", json!({}), "x");
    if let ContextItem::Tool { notice: held, .. } = &mut notice {
        *held = Some("handoff".to_owned());
    }
    assert!(!timeline_copyable(&notice, false, &tools));
}

#[test]
fn refuses_to_move_only_a_compaction() {
    assert!(!timeline_movable(&compaction("c")));
    assert!(timeline_movable(&user("u", "x")));
    assert!(timeline_movable(&system("s", "x")));
}

#[test]
fn clipboard_text_reads_like_what_it_was() {
    let contexts = [user("u", "  one  "), tool("t", "read", json!({ "path": "a" }), "body"), assistant("empty", ""), tool("n", "ls", json!({}), "")];
    assert_eq!(timeline_clipboard_text(&contexts), "one\n\nread {\"path\":\"a\"}\nbody\n\nls {}");
}

#[test]
fn every_copy_gets_a_new_id_and_a_round_stays_one_group_under_a_new_turn() {
    let mut a1 = assistant("a1", "x");
    let mut a2 = assistant("a2", "y");
    let mut other = assistant("a3", "z");
    for (item, turn) in [(&mut a1, "turn-1"), (&mut a2, "turn-1"), (&mut other, "turn-2")] {
        if let ContextItem::Assistant { model_turn_id, sources, .. } = item {
            *model_turn_id = Some(turn.to_owned());
            sources.push(mewrk_client::model::ContextSource { id: "s".to_owned(), url: None, title: None });
        }
    }
    let clipboard = TimelineClipboard { conversation_id: "c".to_owned(), contexts: vec![a1, a2, other, user("u", "x")] };
    let mut next = ids();
    let copies = copies_for_paste(&clipboard, "c", &mut HashSet::new(), &mut next);
    let turn_of = |item: &ContextItem| if let ContextItem::Assistant { model_turn_id, sources, .. } = item { assert!(sources.is_empty(), "sources are left behind"); model_turn_id.clone() } else { None };
    assert_eq!(copies.contexts.len(), 4);
    assert!(copies.contexts.iter().all(|context| !["a1", "a2", "a3", "u"].contains(&context.id())));
    assert_eq!(turn_of(&copies.contexts[0]), turn_of(&copies.contexts[1]));
    assert_ne!(turn_of(&copies.contexts[0]), turn_of(&copies.contexts[2]));
}

fn image(id: &str, short_id: u32) -> mewrk_client::model::ImageAttachment {
    mewrk_client::model::ImageAttachment { id: id.to_owned(), name: "x.png".to_owned(), mime: "image/png".to_owned(), width: 1, height: 1, bytes: 1, short_id: Some(short_id) }
}

#[test]
fn numbers_an_image_again_when_the_destination_uses_its_number_and_never_twice_across_a_paste() {
    let message = |id: &str| ContextItem::User { id: id.to_owned(), content: "see [Image #1]".to_owned(), images: vec![image("i", 1)], files: Vec::new(), created_at: AT.to_owned() };
    let clipboard = TimelineClipboard { conversation_id: "c".to_owned(), contexts: vec![message("u1"), message("u2")] };
    let mut taken = HashSet::from([1]);
    let copies = copies_for_paste(&clipboard, "d", &mut taken, &mut ids());
    let numbers: Vec<u32> = copies.contexts.iter().filter_map(|context| if let ContextItem::User { images, .. } = context { images[0].short_id } else { None }).collect();
    assert_eq!(numbers, [2, 3]);
    let text_of = |context: &ContextItem| if let ContextItem::User { content, .. } = context { content.clone() } else { String::new() };
    assert_eq!(text_of(&copies.contexts[0]), "see [Image #2]");
}

#[test]
fn a_copied_tool_card_leaves_behind_what_tied_it_to_its_place_and_is_pasted_only_where_its_numbers_are_free() {
    let mut card = tool("t", "read", json!({}), "x");
    if let ContextItem::Tool { provider_call_id, attestation, result, .. } = &mut card {
        *provider_call_id = Some("call_1".to_owned());
        *attestation = "proof".to_owned();
        result.images = vec![image("i", 4)];
    }
    let clipboard = TimelineClipboard { conversation_id: "c".to_owned(), contexts: vec![card] };
    let copies = copies_for_paste(&clipboard, "c", &mut HashSet::from([4]), &mut ids());
    assert_eq!(copies.skipped, 0, "the same number in its own conversation is the same image");
    let ContextItem::Tool { provider_call_id, attestation, id, .. } = &copies.contexts[0] else { panic!() };
    assert_eq!(provider_call_id, &None);
    assert!(attestation.is_empty());
    assert_eq!(copies.tools[0].0, *id);
    let elsewhere = copies_for_paste(&clipboard, "other", &mut HashSet::from([4]), &mut ids());
    assert_eq!((elsewhere.contexts.len(), elsewhere.skipped), (0, 1));
    let free = copies_for_paste(&clipboard, "other", &mut HashSet::new(), &mut ids());
    assert_eq!(free.contexts.len(), 1);
    let proved = with_copied_tool_proofs(&free, &["signed".to_owned()]);
    assert!(matches!(&proved[0], ContextItem::Tool { attestation, .. } if attestation == "signed"));
}

#[test]
fn glides_down_without_overshooting() {
    let mut glide = mewrk_ui_chat::frame::FollowGlide { position: 0.0, velocity: 0.0, time: None };
    let start = std::time::Instant::now();
    let mut last = 0.0;
    let mut done = false;
    for frame in 1..400 {
        let (next, finished) = step_follow_glide(glide, 400.0, start + std::time::Duration::from_millis(frame * 16));
        assert!(next.position >= last - 0.01 && next.position <= 400.0, "{} after {last}", next.position);
        last = next.position;
        glide = next;
        if finished {
            done = true;
            break;
        }
    }
    assert!(done);
    assert_eq!(last, 400.0);
}

// ---------------------------------------------------------------- the view, under a scripted pointer

#[derive(Default)]
struct Calls {
    inserted: Vec<(usize, InsertKind, Option<String>)>,
    deleted: Vec<Vec<String>>,
    moved: Vec<(Vec<String>, usize)>,
    copied: Vec<Vec<String>>,
    pasted: Vec<(usize, Vec<String>)>,
    undo: usize,
    redo: usize,
    forked: Vec<usize>,
    edited: Vec<String>,
    removed: Vec<String>,
}

type Log = Rc<RefCell<Calls>>;

fn props(contexts: Vec<LiveContext>, log: &Log) -> ConversationViewProps {
    let (a, b, c, d, e, f, g, h, i, j) = (log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone());
    ConversationViewProps {
        contexts: Rc::new(contexts),
        timeline_id: Some(ID.to_owned()),
        tools: Rc::new(descriptors()),
        enabled_tools: Rc::new(vec!["read".to_owned(), "write".to_owned(), "bash".to_owned(), "preview_click".to_owned(), "web_search".to_owned(), "agent_spawn".to_owned()]),
        on_insert: Some(Rc::new(move |_, index, kind, tool| a.borrow_mut().inserted.push((index, kind, tool)))),
        on_delete_contexts: Some(Rc::new(move |_, ids| b.borrow_mut().deleted.push(ids))),
        on_move_contexts: Some(Rc::new(move |_, ids, index| c.borrow_mut().moved.push((ids, index)))),
        on_copy_contexts: Some(Rc::new(move |_, ids| d.borrow_mut().copied.push(ids))),
        on_paste_contexts: Some(Rc::new(move |_, index, ids| e.borrow_mut().pasted.push((index, ids)))),
        on_undo: Some(Rc::new(move |_| f.borrow_mut().undo += 1)),
        on_redo: Some(Rc::new(move |_| g.borrow_mut().redo += 1)),
        on_fork_at: Some(Rc::new(move |_, index| h.borrow_mut().forked.push(index))),
        on_edit: Some(Rc::new(move |_, item| i.borrow_mut().edited.push(item.id().to_owned()))),
        on_delete: Some(Rc::new(move |_, item| j.borrow_mut().removed.push(item.id().to_owned()))),
        ..ConversationViewProps::default()
    }
}

fn messages(names: &[&str]) -> Vec<LiveContext> {
    names.iter().map(|id| live(assistant(id, &format!("Message {id}")))).collect()
}

fn root(app: &mut App, props: &ConversationViewProps) -> E {
    col().w_full().h_full().child(conversation_view(app, props.clone()))
}

fn card_rect(id: &str) -> fluent::ui::Rect {
    rect_of(unit_key(0, id)).unwrap_or_else(|| panic!("{id} is not built"))
}

fn scroller() -> u64 {
    hash_key(("context-scroll", ID))
}

fn menu_row(path: &[&str], id: &str) -> u64 {
    menu_row_key(("tl-menu", scroller()), path, id)
}

#[test]
fn uses_the_clicked_half_of_a_card_to_insert_before_or_after_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["a"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = card_rect("a");
    rig.right_click(&mut view, r.x + 40.0, r.y + r.h * 0.25);
    let snapshot = timeline_snapshot(&mut rig.app, ID);
    assert_eq!((snapshot.menu, snapshot.menu_index), (Some(MenuView::Insert), Some(0)));
    rig.settle(&mut view);
    rig.click_key(&mut view, menu_row(&[], "Model reply"));
    assert_eq!(log.borrow().inserted, vec![(0, InsertKind::Assistant, None)]);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu, None, "choosing closes the menu");
    let r = card_rect("a");
    rig.right_click(&mut view, r.x + 40.0, r.y + r.h * 0.75);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(1));
}

#[test]
fn maps_a_right_click_in_timeline_whitespace_to_the_nearest_gap_and_opens_on_an_empty_timeline() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["a", "b"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let (a, b) = (card_rect("a"), card_rect("b"));
    rig.right_click(&mut view, 4.0, b.bottom() + 200.0);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(2), "below everything");
    rig.input.pressed = true;
    rig.frame(&mut view);
    rig.input.pressed = false;
    rig.frame(&mut view);
    rig.right_click(&mut view, 4.0, a.y + 1.0);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(0));

    let mut rig = Rig::new();
    let p = props(Vec::new(), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    rig.right_click(&mut view, 300.0, 300.0);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(0), "an actually empty conversation surface inserts");
}

#[test]
fn offers_no_preview_or_orchestration_tool_as_a_call_to_place_by_hand() {
    let tools = descriptors();
    let enabled: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    let groups = insertable_tool_groups(&tools, &enabled);
    let names: Vec<(ToolCategory, Vec<&str>)> = groups.iter().map(|(category, tools)| (*category, tools.iter().map(|tool| tool.name.as_str()).collect())).collect();
    assert_eq!(names, vec![(ToolCategory::Filesystem, vec!["read", "write"]), (ToolCategory::Shell, vec!["bash"]), (ToolCategory::Web, vec!["web_search"])]);
    assert!(insertable_tool_groups(&tools, &[]).is_empty());
}

#[test]
fn forks_at_the_insertion_line_and_refuses_with_nothing_above_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["a", "b"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let b = card_rect("b");
    rig.right_click(&mut view, b.x + 10.0, b.y + b.h * 0.75);
    rig.settle(&mut view);
    rig.click_key(&mut view, menu_row(&[], "fork"));
    assert_eq!(log.borrow().forked, vec![2]);
    let a = card_rect("a");
    rig.right_click(&mut view, a.x + 10.0, a.y + a.h * 0.25);
    rig.settle(&mut view);
    rig.click_key(&mut view, menu_row(&[], "fork"));
    assert_eq!(log.borrow().forked, vec![2], "nothing above the first line to fork");
}

#[test]
fn read_only_offers_no_menu_and_no_mutation() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(messages(&["a"]), &log);
    p.editable = false;
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = card_rect("a");
    rig.right_click(&mut view, r.x + 10.0, r.y + 5.0);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu, None);
    assert!(rect_of(("tl-edit", 0u64, "a")).is_none() && rect_of(("tl-delete", 0u64, "a")).is_none());
}

#[test]
fn a_lock_keeps_the_controls_up_but_lets_nothing_through() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(messages(&["a"]), &log);
    p.timeline_mutation_locked = true;
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = rect_of(("tl-delete", 0u64, "a")).expect("drawn, disabled");
    rig.click(&mut view, r.x + 2.0, r.y + 2.0);
    assert!(log.borrow().removed.is_empty());
}

#[test]
fn lines_every_message_action_up_with_delete_last() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["a"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let copy = rect_of(("copy", hash_key(("tl-copy", 0u64, "a")))).expect("copy");
    let edit = rect_of(("tl-edit", 0u64, "a")).expect("edit");
    let delete = rect_of(("tl-delete", 0u64, "a")).expect("delete");
    assert!(copy.x < edit.x && edit.x < delete.x, "{copy:?} {edit:?} {delete:?}");
    let card = card_rect("a");
    rig.point(card.x + 5.0, card.y + 3.0);
    rig.settle(&mut view);
    rig.click(&mut view, delete.x + 2.0, delete.y + 2.0);
    assert_eq!(log.borrow().removed, ["a"]);
    rig.point(card.x + 5.0, card.y + 3.0);
    rig.settle(&mut view);
    rig.click(&mut view, edit.x + 2.0, edit.y + 2.0);
    assert_eq!(log.borrow().edited, ["a"]);
}

#[test]
fn folds_a_long_user_message_and_a_slightly_long_one_shows_whole() {
    let mut rig = Rig::new();
    let log = Log::default();
    let long = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let middling = (0..12).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let p = props(vec![live(user("long", &long)), live(user("middling", &middling))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let scroll_top = hash_key(("context-scroll", ID));
    scroll_request(scroll_top, ScrollRequest::Shift(-100_000.0));
    rig.settle(&mut view);
    let folded = card_rect("long").h;
    assert!(folded < 300.0, "folded to its first lines: {folded}");
    assert!(rect_of(hash_key(("tl-fold", 0u64, "middling"))).is_none(), "folding would hide next to nothing");
    let toggle = rect_of(hash_key(("tl-fold", 0u64, "long"))).expect("Show more");
    rig.click(&mut view, toggle.x + 4.0, toggle.y + 4.0);
    rig.settle(&mut view);
    let whole = card_rect("long").h;
    assert!(whole > folded + 300.0, "{whole}");
    rig.point(300.0, 300.0);
    rig.wheel(&mut view, -400.0);
    rig.settle(&mut view);
    let toggle = rect_of(hash_key(("tl-fold", 0u64, "long"))).expect("Show less");
    rig.click(&mut view, toggle.x + 4.0, toggle.y + 4.0);
    rig.settle(&mut view);
    assert_eq!(card_rect("long").h, folded);
    // Folding a message read to its end leaves the reader below it, so the toggle comes back into view.
    let toggle = rect_of(hash_key(("tl-fold", 0u64, "long"))).expect("Show more");
    assert!(toggle.y >= 0.0 && toggle.bottom() <= rig.size.1, "{toggle:?}");
}

#[test]
fn hides_expansions_the_model_reads_but_the_user_never_typed() {
    let mut rig = Rig::new();
    let log = Log::default();
    let message = ContextItem::User { id: "u".to_owned(), content: "look at [Image #2] please\n\n[Image #1]".to_owned(), images: vec![image("i", 1)], files: Vec::new(), created_at: AT.to_owned() };
    let p = props(vec![live(message)], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    assert!(rect_of(unit_key(0, "u")).is_some());
}

#[test]
fn keeps_reasoning_rows_closed_and_opens_them_on_a_click_with_no_mutation_for_encrypted_ones() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut encrypted = reasoning("enc", None);
    if let ContextItem::Reasoning { tokens, form, .. } = &mut encrypted {
        *tokens = Some(300);
        *form = None;
    }
    let p = props(vec![live(reasoning("plain", Some("## A plan\nthe body of the thought"))), live(encrypted)], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let closed = card_rect("plain").h;
    assert!(rect_of(("tl-edit", 0u64, "plain")).is_some());
    assert!(rect_of(("tl-edit", 0u64, "enc")).is_none(), "encrypted reasoning cannot be edited");
    assert!(rect_of(("tl-delete", 0u64, "enc")).is_some(), "but it can be deleted");
    let r = card_rect("plain");
    rig.click(&mut view, r.x + 60.0, r.y + 10.0);
    rig.settle(&mut view);
    assert!(card_rect("plain").h > closed + 10.0, "opened");
    rig.click(&mut view, r.x + 60.0, r.y + 10.0);
    rig.settle(&mut view);
    assert_eq!(card_rect("plain").h, closed, "closed again");
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu, None);
}

#[test]
fn an_agent_row_goes_to_its_panel_instead_of_opening_a_drawer() {
    let mut rig = Rig::new();
    let log = Log::default();
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    let sink = opened.clone();
    let mut p = props(vec![live(tool("spawn", "agent_spawn", json!({ "name": "scout" }), "spawned"))], &log);
    p.on_open_subagent = Some(Rc::new(move |_, id| sink.borrow_mut().push(id.to_owned())));
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let closed = card_rect("spawn").h;
    let r = card_rect("spawn");
    rig.click(&mut view, r.x + 60.0, r.y + 10.0);
    rig.settle(&mut view);
    assert_eq!(*opened.borrow(), ["scout"]);
    assert_eq!(card_rect("spawn").h, closed, "no drawer");
}

#[test]
fn a_failed_call_is_titled_by_why_and_stays_closed() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(vec![live(failed_tool("f", "bash", json!({ "command": "x" }), "Exit code 1\nerror: nope"))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    assert!(card_rect("f").h < 40.0);
}

#[test]
fn draws_a_failure_notice_with_retry_only_for_the_held_request_and_dismiss() {
    let mut rig = Rig::new();
    let log = Log::default();
    let retried = Rc::new(RefCell::new(0));
    let dismissed = Rc::new(RefCell::new(0));
    let turn = ConversationTurn { id: "turn".to_owned(), request_id: "req".to_owned(), anchor_context_id: "a".to_owned(), model_id: String::new(), started_at: AT.to_owned(), ended_at: None, duration_ms: None, status: ConversationTurnStatus::Completed, context_ids: vec!["a".to_owned()], usage: Default::default(), usage_offset: Default::default(), usage_baseline: Default::default(), usage_revision_at_start: 0, segment_count: 1, error: Some(ConversationTurnError { message: "boom".to_owned(), provider_name: "P".to_owned(), model_name: "M".to_owned(), at: AT.to_owned() }) };
    let (r, d) = (retried.clone(), dismissed.clone());
    let mut p = props(messages(&["a"]), &log);
    p.turns = Rc::new(vec![turn]);
    p.on_retry_turn_error = Some(Rc::new(move |_| *r.borrow_mut() += 1));
    p.on_dismiss_turn_error = Some(Rc::new(move |_| *d.borrow_mut() += 1));
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    assert!(rect_of(("tl-turn-error", "turn")).is_some());
    assert!(rect_of(("tl-turn-retry", "turn")).is_none(), "a reloaded transcript shows the notice without a dead button");
    let mut p2 = p.clone();
    p2.retryable_turn_request_id = Some("req".to_owned());
    p2.timeline_id = Some("other".to_owned());
    let mut view2 = |app: &mut App| root(app, &p2);
    rig.settle(&mut view2);
    let retry = rect_of(("tl-turn-retry", "turn")).expect("offered");
    rig.click(&mut view2, retry.x + 2.0, retry.y + 2.0);
    assert_eq!(*retried.borrow(), 1);
    let dismiss = rect_of(("tl-turn-dismiss", "turn")).expect("dismiss");
    rig.click(&mut view2, dismiss.x + 2.0, dismiss.y + 2.0);
    assert_eq!(*dismissed.borrow(), 1);
}

#[test]
fn draws_a_supplied_empty_state_and_right_click_still_inserts_on_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(Vec::new(), &log);
    p.empty_state = Some(Rc::new(|_| div().key("landing").h(200.0)));
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let landing = rect_of("landing").expect("drawn");
    rig.right_click(&mut view, landing.x + 10.0, landing.y + 10.0);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(0));
}

#[test]
fn the_composer_slot_goes_with_the_ability_to_edit() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(messages(&["a"]), &log);
    p.composer = Some(Rc::new(|_| div().key("composer").h(40.0)));
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    assert!(rect_of("composer").is_some());
    let mut read_only = p.clone();
    read_only.editable = false;
    let mut view = |app: &mut App| root(app, &read_only);
    rig.settle(&mut view);
    assert!(rect_of("composer").is_none());
}

// ---------------------------------------------------------------- follow-output

fn tall(count: usize) -> Vec<LiveContext> {
    (0..count).map(|at| live(assistant(&format!("m{at}"), &"A long reply that takes up some room.\n\n".repeat(6)))).collect()
}

fn bottom_gap() -> f32 {
    let (offset, content, viewport) = scroll_state(scroller()).expect("a scroller");
    content - offset - viewport
}

#[test]
fn opens_at_the_bottom_and_stays_there_as_rows_are_measured() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(tall(200), &log);
    let mut view = |app: &mut App| root(app, &p);
    for _ in 0..8 {
        rig.frame(&mut view);
    }
    assert!(bottom_gap().abs() < 1.0, "gap {}", bottom_gap());
    assert!(rect_of(unit_key(0, "m199")).is_some());
}

#[test]
fn drops_follow_output_after_one_small_upward_scroll_and_re_attaches_at_the_bottom() {
    let mut rig = Rig::new();
    let log = Log::default();
    let contexts = tall(60);
    let p = props(contexts.clone(), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.point(300.0, 300.0);
    rig.settle(&mut view);
    assert!(timeline_snapshot(&mut rig.app, ID).pinned);
    rig.wheel(&mut view, 40.0);
    rig.frame(&mut view);
    assert!(!timeline_snapshot(&mut rig.app, ID).pinned, "one notch up lets go");
    rig.wheel(&mut view, -2000.0);
    rig.settle(&mut view);
    assert!(timeline_snapshot(&mut rig.app, ID).pinned, "resting at the bottom re-attaches");
}

fn with_tail(contexts: &[LiveContext], tail: &str, content: &str) -> Vec<LiveContext> {
    let mut next = contexts.to_vec();
    next.push(live(assistant(tail, content)));
    next
}

#[test]
fn follows_an_appended_tail_and_not_what_is_placed_above_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let base = tall(30);
    let p = props(base.clone(), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let appended = props(with_tail(&base, "new", &"tail\n\n".repeat(30)), &log);
    let mut view = |app: &mut App| root(app, &appended);
    rig.settle(&mut view);
    assert!(bottom_gap().abs() < 1.0, "followed: {}", bottom_gap());
    // Contexts pasted further up make it longer too, and following those would carry the reader off.
    let mut above = appended.contexts.as_ref().clone();
    above.insert(2, live(assistant("placed", "x")));
    let (offset, ..) = scroll_state(scroller()).unwrap();
    let placed = props(above, &log);
    let mut view = |app: &mut App| root(app, &placed);
    rig.settle(&mut view);
    let (after, ..) = scroll_state(scroller()).unwrap();
    assert!((after - offset).abs() < 80.0, "did not jump: {offset} -> {after}");
}

#[test]
fn glides_down_to_a_streaming_tail_over_several_frames_and_lets_go_the_moment_the_wheel_turns_up() {
    let mut rig = Rig::new();
    let log = Log::default();
    let base = tall(30);
    let growing = |text: &str| {
        let mut contexts = base.clone();
        contexts.push(streaming(assistant("live", text), None));
        contexts
    };
    let mut p = props(growing("first"), &log);
    p.streaming = true;
    let mut view = |app: &mut App| root(app, &p);
    rig.point(300.0, 300.0);
    rig.settle(&mut view);
    let mut more = p.clone();
    more.contexts = Rc::new(growing(&"more text in the stream\n\n".repeat(30)));
    let mut view = |app: &mut App| root(app, &more);
    rig.frame(&mut view);
    rig.frame(&mut view);
    let gap = bottom_gap();
    assert!(gap > 10.0, "still gliding, not jumped: {gap}");
    assert!(timeline_snapshot(&mut rig.app, ID).gliding);
    rig.wheel(&mut view, 30.0);
    rig.frame(&mut view);
    rig.frame(&mut view);
    let snapshot = timeline_snapshot(&mut rig.app, ID);
    assert!(!snapshot.pinned && !snapshot.gliding, "{snapshot:?}");
}

#[test]
fn does_not_treat_a_different_timeline_as_appended_output() {
    let mut rig = Rig::new();
    let log = Log::default();
    let first = props(tall(30), &log);
    let mut view = |app: &mut App| root(app, &first);
    rig.point(300.0, 300.0);
    rig.settle(&mut view);
    let mut other = props(tall(40), &log);
    other.timeline_id = Some("other".to_owned());
    let mut view = |app: &mut App| root(app, &other);
    rig.settle(&mut view);
    let other_scroller = hash_key(("context-scroll", "other"));
    assert!(scroll_state(other_scroller).is_some());
}

#[test]
fn a_context_menu_stops_follow_output_and_scrolling_underneath_closes_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(tall(30), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.point(300.0, 300.0);
    rig.settle(&mut view);
    rig.right_click(&mut view, 300.0, 300.0);
    assert!(timeline_snapshot(&mut rig.app, ID).menu.is_some());
    rig.point(900.0, 700.0);
    rig.frame(&mut view);
    rig.wheel(&mut view, 200.0);
    rig.settle(&mut view);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu, None);
}

// ---------------------------------------------------------------- virtualisation

#[test]
fn builds_only_the_rows_near_the_viewport_of_a_long_timeline() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(long_timeline(20_000), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let snapshot = timeline_snapshot(&mut rig.app, ID);
    assert!(snapshot.nodes > 5000, "{}", snapshot.nodes);
    assert!(snapshot.built.len() < 60, "built {} of {}", snapshot.built.len(), snapshot.nodes);
}

#[test]
fn keeps_what_the_reader_sees_still_as_heights_above_are_measured() {
    let mut rig = Rig::new();
    let log = Log::default();
    // Replies far taller than a first guess at them, so scrolling up measures every one of them wrong.
    let contexts: Vec<LiveContext> = (0..300).map(|at| live(assistant(&format!("m{at}"), &format!("{}\n\n{}", "Short line.", "```\ncode\ncode\ncode\ncode\ncode\ncode\ncode\ncode\n```\n\n".repeat(2 + at % 3))))).collect();
    let p = props(contexts, &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.point(300.0, 300.0);
    rig.settle(&mut view);
    for _ in 0..12 {
        rig.wheel(&mut view, 160.0);
        rig.frame(&mut view);
        // Whatever is on screen keeps its place from one frame to the next while nothing scrolls.
        let snapshot = timeline_snapshot(&mut rig.app, ID);
        let probe = snapshot.built.iter().copied().find_map(|node| {
            let id = format!("m{node}");
            rect_of(unit_key(0, &id)).filter(|r| r.y > 100.0 && r.y < 500.0).map(|r| (id, r.y))
        });
        let Some((id, before)) = probe else { continue };
        rig.frame(&mut view);
        rig.frame(&mut view);
        let after = rect_of(unit_key(0, &id)).expect("still built").y;
        assert!((after - before).abs() < 1.0, "{id} moved from {before} to {after}");
    }
}

// ---------------------------------------------------------------- selection box, clipboard keys, undo keys

fn modified_drag(rig: &mut Rig, view: &mut dyn FnMut(&mut App) -> E, from: (f32, f32), to: (f32, f32)) {
    rig.input.zoom_modifier = true;
    rig.press(view, from.0, from.1);
    rig.point(to.0, to.1);
    rig.frame(view);
    rig.frame(view);
    rig.release(view, to.0, to.1);
    rig.input.zoom_modifier = false;
    rig.settle(view);
}

#[test]
fn picks_out_what_a_ctrl_drag_touches_and_deletes_it_all_from_its_menu() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let (a, b, c) = (card_rect("p1"), card_rect("p2"), card_rect("p3"));
    modified_drag(&mut rig, &mut view, (a.x + 10.0, a.y + 3.0), (b.x + 60.0, b.y + 3.0));
    let snapshot = timeline_snapshot(&mut rig.app, ID);
    assert_eq!(snapshot.selection, ["p1", "p2"]);
    assert!(!snapshot.marquee, "the box is gone once the drag is over; what it picked stays picked");
    // With contexts picked out, a right-click anywhere is about them.
    rig.right_click(&mut view, c.x + 10.0, c.y + 3.0);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu, Some(MenuView::Selection));
    rig.settle(&mut view);
    rig.click_key(&mut view, menu_row(&[], "delete"));
    assert_eq!(log.borrow().deleted, vec![vec!["p1".to_owned(), "p2".to_owned()]]);
    assert!(timeline_snapshot(&mut rig.app, ID).selection.is_empty());
}

#[test]
fn lets_a_selection_go_on_a_plain_press_and_draws_no_box_without_the_modifier_or_a_delete() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let (a, b) = (card_rect("p1"), card_rect("p2"));
    // No modifier: selects text, picks nothing.
    rig.press(&mut view, a.x + 10.0, a.y + 3.0);
    rig.point(b.x + 60.0, b.y + 3.0);
    rig.frame(&mut view);
    rig.release(&mut view, b.x + 60.0, b.y + 3.0);
    assert!(timeline_snapshot(&mut rig.app, ID).selection.is_empty());
    modified_drag(&mut rig, &mut view, (a.x + 10.0, a.y + 3.0), (b.x + 60.0, b.y + 3.0));
    assert_eq!(timeline_snapshot(&mut rig.app, ID).selection.len(), 2);
    rig.click(&mut view, 4.0, 700.0);
    assert!(timeline_snapshot(&mut rig.app, ID).selection.is_empty());
    // No way to delete: no box at all.
    let mut no_delete = props(messages(&["p1", "p2"]), &log);
    no_delete.on_delete_contexts = None;
    no_delete.timeline_id = Some("no-delete".to_owned());
    let mut view = |app: &mut App| root(app, &no_delete);
    let mut rig = Rig::new();
    rig.settle(&mut view);
    let (a, b) = (card_rect("p1"), card_rect("p2"));
    modified_drag(&mut rig, &mut view, (a.x + 10.0, a.y + 3.0), (b.x + 60.0, b.y + 3.0));
    assert!(timeline_snapshot(&mut rig.app, "no-delete").selection.is_empty());
}

#[test]
fn picks_one_out_or_lets_it_go_on_a_ctrl_click_and_a_box_adds_to_what_is_picked() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let (a, b, c) = (card_rect("p1"), card_rect("p2"), card_rect("p3"));
    modified_drag(&mut rig, &mut view, (a.x + 10.0, a.y + 3.0), (a.x + 10.0, a.y + 3.0));
    assert_eq!(timeline_snapshot(&mut rig.app, ID).selection, ["p1"]);
    modified_drag(&mut rig, &mut view, (c.x + 10.0, c.y + 3.0), (c.x + 10.0, c.y + 3.0));
    assert_eq!(timeline_snapshot(&mut rig.app, ID).selection, ["p1", "p3"]);
    modified_drag(&mut rig, &mut view, (a.x + 10.0, a.y + 3.0), (a.x + 10.0, a.y + 3.0));
    assert_eq!(timeline_snapshot(&mut rig.app, ID).selection, ["p3"]);
    modified_drag(&mut rig, &mut view, (b.x + 10.0, b.y + 3.0), (b.x + 60.0, b.y + 8.0));
    assert_eq!(timeline_snapshot(&mut rig.app, ID).selection, ["p2", "p3"]);
}

#[test]
fn copies_what_is_picked_and_pastes_over_it_or_at_the_insertion_line() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(messages(&["p1", "p2", "p3"]), &log);
    p.paste_count = 2;
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let (a, b) = (card_rect("p1"), card_rect("p2"));
    modified_drag(&mut rig, &mut view, (a.x + 10.0, a.y + 3.0), (b.x + 60.0, b.y + 3.0));
    rig.key(&mut view, Key::Char('c'), true, false);
    assert_eq!(log.borrow().copied, vec![vec!["p1".to_owned(), "p2".to_owned()]]);
    rig.key(&mut view, Key::Char('v'), true, false);
    assert_eq!(log.borrow().pasted, vec![(0, vec!["p1".to_owned(), "p2".to_owned()])]);
    rig.key(&mut view, Key::Escape, false, false);
    assert!(timeline_snapshot(&mut rig.app, ID).selection.is_empty());
    let c = card_rect("p3");
    rig.right_click(&mut view, c.x + 10.0, c.y + c.h * 0.75);
    rig.key(&mut view, Key::Char('v'), true, false);
    assert_eq!(log.borrow().pasted.last(), Some(&(3, Vec::new())));
}

#[test]
fn answers_the_undo_keys_anywhere_in_the_timeline_but_inside_a_field_or_with_focus_elsewhere() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["a"]), &log);
    let mut view = |app: &mut App| {
        let timeline = conversation_view(app, p.clone());
        col().w_full().h_full().child(div().key("elsewhere").h(40.0).w_full().focusable().on_click(|_: &mut App| {})).child(field::<App>("a-field", "").build()).child(timeline)
    };
    rig.settle(&mut view);
    rig.key(&mut view, Key::Char('z'), true, false);
    assert_eq!(log.borrow().undo, 0, "focus has never been in the timeline");
    let a = card_rect("a");
    rig.click(&mut view, a.x + 10.0, a.y + 5.0);
    rig.key(&mut view, Key::Char('z'), true, false);
    assert_eq!(log.borrow().undo, 1);
    rig.key(&mut view, Key::Char('x'), true, false);
    assert_eq!(log.borrow().redo, 1);
    rig.click(&mut view, 20.0, 10.0);
    rig.key(&mut view, Key::Char('z'), true, false);
    assert_eq!(log.borrow().undo, 1, "pressed outside it");
    let f = rect_of(hash_key(("field", "a-field"))).expect("a field");
    rig.click(&mut view, f.x + 5.0, f.y + 5.0);
    rig.key(&mut view, Key::Char('z'), true, false);
    assert_eq!(log.borrow().undo, 1, "typed into a field");
}

// ---------------------------------------------------------------- dragging what is picked

struct DragRig<'a> {
    rig: Rig,
    view: &'a mut dyn FnMut(&mut App) -> E,
}

fn pick_two(rig: &mut Rig, view: &mut dyn FnMut(&mut App) -> E, first: &str, second: &str) {
    for id in [first, second] {
        let r = card_rect(id);
        modified_drag(rig, view, (r.x + 10.0, r.y + 3.0), (r.x + 10.0, r.y + 3.0));
    }
}

impl DragRig<'_> {
    fn press_on(&mut self, id: &str) -> (f32, f32) {
        let r = card_rect(id);
        let at = (r.x + 40.0, r.y + r.h * 0.5);
        self.rig.press(self.view, at.0, at.1);
        at
    }
    fn move_to(&mut self, x: f32, y: f32) {
        self.rig.point(x, y);
        self.rig.frame(self.view);
        self.rig.frame(self.view);
    }
    fn snapshot(&mut self) -> mewrk_ui_chat::frame::TimelineSnapshot {
        timeline_snapshot(&mut self.rig.app, ID)
    }
    fn let_go(&mut self, x: f32, y: f32) {
        self.rig.release(self.view, x, y);
        self.rig.settle(self.view);
    }
}

#[test]
fn a_press_that_never_travels_is_a_click_and_lets_the_selection_go() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3", "p4"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "p1", "p3");
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p3");
    assert!(!d.snapshot().drag_active);
    d.let_go(at.0, at.1);
    assert!(d.snapshot().selection.is_empty());
    assert!(log.borrow().moved.is_empty());
}

#[test]
fn gathers_scattered_messages_as_the_drag_begins_and_carries_them_to_the_insertion_line() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3", "p4"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "p1", "p3");
    let p2 = card_rect("p2");
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p3");
    assert!(!d.snapshot().drag_active, "nothing happens until the press has travelled");
    d.move_to(at.0, at.1 + 7.0);
    let snapshot = d.snapshot();
    assert_eq!(snapshot.look, Some(Look::Gathered), "gathered at once");
    assert_eq!(snapshot.ghosts, 2);
    assert_eq!(snapshot.mark, Some(MarkState::At), "still where it began: the home line is where a drop would land");
    assert_eq!(snapshot.line, None, "and it stands in for the timeline's own line");
    assert!(rect_of(unit_key(0, "p1")).is_none() && rect_of(unit_key(0, "p3")).is_none(), "they leave the timeline");
    // At the head of the column, closed up over what left it: that line lies under the strip a drag scrolls from, which does not count
    // as in sight, so the end that can be seen is taken instead.
    d.move_to(at.0 + 300.0, 0.0);
    d.move_to(at.0 + 300.0, p2.y - 30.0);
    assert_eq!(d.snapshot().mark, Some(MarkState::At));
    // Past everything, away from the line.
    d.move_to(at.0 + 300.0, 700.0);
    let snapshot = d.snapshot();
    assert_eq!(snapshot.mark, Some(MarkState::Away));
    assert_eq!(snapshot.line, Some(4), "the timeline's line marks the drop: below what stays");
    d.let_go(at.0 + 300.0, 700.0);
    assert_eq!(log.borrow().moved, vec![(vec!["p1".to_owned(), "p3".to_owned()], 4)]);
    let snapshot = d.snapshot();
    assert!(!snapshot.drag_active && snapshot.line.is_none());
    assert_eq!(snapshot.selection, ["p1", "p3"], "the selection is kept");
    // Let go without ever leaving, the scattered messages come together where the grabbed one sat.
    wait_out_the_motion(&mut d.rig, d.view);
    let at = d.press_on("p3");
    d.move_to(at.0, at.1 + 6.0);
    d.let_go(at.0, at.1 + 6.0);
    assert_eq!(log.borrow().moved.last(), Some(&(vec!["p1".to_owned(), "p3".to_owned()], 3)));
}

#[test]
fn docks_scattered_pieces_at_the_home_line_and_counts_them_back_apart_afresh_on_each_return() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3", "p4"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "p1", "p3");
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p3");
    d.move_to(at.0, at.1 + 7.0);
    // Away, so the pin lets go, and back to the line.
    d.move_to(at.0 + 300.0, 700.0);
    assert_eq!(d.snapshot().mark, Some(MarkState::Away));
    let home = d.snapshot().mark_y.unwrap();
    d.move_to(at.0, home + 2.0);
    let snapshot = d.snapshot();
    assert_eq!(snapshot.look, Some(Look::Docked), "{snapshot:?}");
    assert_eq!(snapshot.mark, Some(MarkState::Counting));
    // Let go now, the pieces stay as they are shown: together, at the home line.
    assert_eq!(snapshot.target, Some(3));
    // The timeline has opened out around them: what follows the line has moved down by their room.
    // It slides there, from where it was.
    assert!(snapshot.sliding > 0, "{snapshot:?}");
    std::thread::sleep(mewrk_ui_chat::drag::MOTION + std::time::Duration::from_millis(20));
    d.rig.frame(d.view);
    d.rig.frame(d.view);
    let below = card_rect("p4");
    assert!(below.y > snapshot.mark_y.unwrap() + 40.0, "{below:?} {snapshot:?}");
    std::thread::sleep(std::time::Duration::from_millis(mewrk_ui_chat::drag::TIMELINE_DRAG_DOCK_MS));
    d.move_to(at.0, home + 2.0);
    assert_eq!(d.snapshot().look, Some(Look::Apart), "the count ran out");
    assert_eq!(d.snapshot().target, None);
    d.let_go(at.0, home + 2.0);
    assert!(log.borrow().moved.is_empty(), "apart, letting go changes nothing");
    assert_eq!(d.snapshot().selection, ["p1", "p3"]);
}

#[test]
fn takes_messages_that_sit_together_apart_at_the_home_line_without_a_count() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3", "p4"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "p2", "p3");
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p3");
    d.move_to(at.0, at.1 + 7.0);
    assert_eq!(d.snapshot().look, Some(Look::Gathered));
    d.move_to(at.0 + 300.0, 700.0);
    let home = d.snapshot().mark_y.unwrap();
    d.move_to(at.0, home + 2.0);
    let snapshot = d.snapshot();
    assert_eq!(snapshot.look, Some(Look::Apart), "{snapshot:?}");
    assert_eq!(snapshot.mark, Some(MarkState::At));
    d.let_go(at.0, home + 2.0);
    assert!(log.borrow().moved.is_empty());
}

#[test]
fn escape_calls_a_drag_off_and_a_changed_order_ends_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3", "p4"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "p1", "p3");
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p3");
    d.move_to(at.0, at.1 + 7.0);
    assert!(d.snapshot().drag_active);
    d.rig.key(d.view, Key::Escape, false, false);
    assert!(!d.snapshot().drag_active);
    d.let_go(at.0, at.1);
    assert!(log.borrow().moved.is_empty());
}

#[test]
fn picks_a_row_out_of_a_block_and_the_rest_of_the_block_stays() {
    let mut rig = Rig::new();
    let log = Log::default();
    let contexts = vec![live(assistant("top", "above")), live(tool("r1", "read", json!({ "path": "a" }), "x")), live(tool("r2", "read", json!({ "path": "b" }), "x")), live(tool("r3", "read", json!({ "path": "c" }), "x")), live(assistant("end", "below"))];
    let p = props(contexts, &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = card_rect("r2");
    modified_drag(&mut rig, &mut view, (r.x + 10.0, r.y + 3.0), (r.x + 10.0, r.y + 3.0));
    assert_eq!(timeline_snapshot(&mut rig.app, ID).selection, ["r2"]);
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("r2");
    d.move_to(at.0, at.1 + 7.0);
    assert!(rect_of(unit_key(0, "r2")).is_none(), "out of its block");
    assert!(rect_of(unit_key(0, "r1")).is_some() && rect_of(unit_key(0, "r3")).is_some(), "the rest stays");
    assert_eq!(d.snapshot().mark, Some(MarkState::At));
}


/// An edit and a read, a block that ends the span of work after `u`, and so the row that holds the changed files' list.
fn a_turn_with_changes() -> Vec<LiveContext> {
    let edit = ContextItem::Tool { id: "e1".to_owned(), tool_name: "edit".to_owned(), round: None, model_turn_id: None, provider_call_id: None, requested_input: None, input: input(json!({ "path": "src/main.rs" })), result: mewrk_client::model::ToolResult { diff: Some("--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n".to_owned()), ..result(true, "ok") }, subagent: None, notice: None, attestation: String::new(), created_at: AT.to_owned() };
    vec![live(user("u", "change it")), live(assistant("top", "on it")), live(edit), live(tool("r1", "read", json!({ "path": "a" }), "x")), live(tool("r2", "read", json!({ "path": "b" }), "x")), live(user("next", "thanks")), live(assistant("end", "below"))]
}

fn changes_y() -> f32 {
    rect_of(("turn-changes", "after:u")).unwrap_or_else(|| panic!("the list of changed files is not built")).y
}

#[test]
fn the_files_a_block_changed_slide_when_a_row_is_lifted_out_of_it_and_again_when_it_lands() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(a_turn_with_changes(), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let before = changes_y();
    let r = card_rect("r1");
    modified_drag(&mut rig, &mut view, (r.x + 10.0, r.y + 3.0), (r.x + 10.0, r.y + 3.0));
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("r1");
    d.move_to(at.0, at.1 + 7.0);
    d.move_to(at.0 + 80.0, 700.0);
    assert_eq!(d.snapshot().look, Some(Look::Gathered));
    let block = "block:e1";
    let up = mewrk_ui_chat::frame::extras_slide(&mut d.rig.app, ID, block).expect("the list moves up as the row leaves, by a slide");
    assert!(up.dy > 10.0, "from where it was: {}", up.dy);
    wait_out_the_motion(&mut d.rig, d.view);
    assert!(changes_y() < before - 10.0, "closed up over the row");
    d.rig.release(d.view, at.0 + 80.0, 700.0);
    d.rig.frame(d.view);
    // These props do not move anything, so it goes back down to where it was, and only by being drawn there.
    let down = mewrk_ui_chat::frame::extras_slide(&mut d.rig.app, ID, block).expect("the list slides as what was dragged lands");
    assert!(down.dy < -10.0, "from up where it stood: {}", down.dy);
    assert!(mewrk_ui_chat::frame::node_slide(&mut d.rig.app, ID, block).is_none(), "the rows move by themselves, not the whole slot");
    wait_out_the_motion(&mut d.rig, d.view);
    assert!(mewrk_ui_chat::frame::extras_slide(&mut d.rig.app, ID, block).is_none());
    assert!((changes_y() - before).abs() < 0.5, "and it ends where it began");
}

#[test]
fn the_files_a_block_changed_slide_when_the_whole_block_is_lifted_from_above_them() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(a_turn_with_changes(), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let before = changes_y();
    // A press from the card above down through the block's last row takes all three of them.
    let (from, to) = (card_rect("e1"), card_rect("r2"));
    modified_drag(&mut rig, &mut view, (from.x + 10.0, from.y + 3.0), (to.x + 10.0, to.y + to.h - 3.0));
    assert_eq!(timeline_snapshot(&mut rig.app, ID).selection, ["e1", "r1", "r2"]);
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("r1");
    d.move_to(at.0, at.1 + 7.0);
    d.move_to(at.0 + 80.0, 700.0);
    let slide = mewrk_ui_chat::frame::extras_slide(&mut d.rig.app, ID, "block:e1").expect("it stays on the timeline, and moves there by a slide");
    assert!(slide.dy > 10.0, "{}", slide.dy);
    wait_out_the_motion(&mut d.rig, d.view);
    assert!(changes_y() < before - 10.0);
}

// ---------------------------------------------------------------- what a drag does between its looks

mod drag_model {
    use std::time::{Duration, Instant};

    use mewrk_ui_chat::drag::*;

    /// A column of cards and block rows, 40 px each, as a drag sees it: it remembers what it was told to do.
    struct Column {
        tops: Vec<f32>,
        flown: Vec<(usize, f32)>,
        reflows: usize,
        dock: f32,
    }

    impl DragGeometry for Column {
        fn span(&self, unit: usize) -> (f32, f32) {
            (self.tops[unit], self.tops[unit] + 40.0)
        }
        fn view(&self) -> (f32, f32) {
            (0.0, 1000.0)
        }
        fn column_top(&self) -> f32 {
            0.0
        }
        fn end_index(&self) -> usize {
            self.tops.len()
        }
        fn scroll_by(&mut self, _: f32) -> f32 {
            0.0
        }
        fn apply_look(&mut self, _: &DragSession, _: Look) {}
        fn dock_top(&self, _: &DragSession) -> Option<f32> {
            Some(self.dock)
        }
        fn reflow_snapshot(&self, _: &DragSession) -> Vec<(Mover, f32)> {
            Vec::new()
        }
        fn reflow_from(&mut self, _: &DragSession, _: Vec<(Mover, f32)>, _: Instant) {
            self.reflows += 1;
        }
        fn fly_piece(&mut self, _: &DragSession, piece: usize, _: f32, dy: f32, _: Instant) {
            self.flown.push((piece, dy));
        }
    }

    fn column(count: usize) -> Column {
        Column { tops: (0..count).map(|at| at as f32 * 50.0).collect(), flown: Vec::new(), reflows: 0, dock: 500.0 }
    }

    /// `(node, row)` per unit, and which are picked.
    fn units(shape: &[(usize, Option<usize>, bool)]) -> Vec<Unit> {
        shape.iter().enumerate().map(|(at, (node, row, picked))| Unit { node: *node, row: *row, id: format!("u{at}"), ids: vec![format!("u{at}")], index: at, picked: *picked }).collect()
    }

    fn session(shape: &[(usize, Option<usize>, bool)], geometry: &Column, still: bool) -> DragSession {
        let press_unit = shape.iter().position(|unit| unit.2).unwrap();
        let press = (30.0, geometry.tops[press_unit] + 5.0);
        start_session(units(shape), press, geometry, |unit| unit == press_unit, |_| (10.0, 300.0), still).unwrap()
    }

    #[test]
    fn the_home_line_between_two_rows_of_one_block_is_inside_that_block() {
        let geometry = column(4);
        // A card, then a block of three rows with the middle one picked.
        let inside = session(&[(0, None, false), (1, Some(0), false), (1, Some(1), true), (1, Some(2), false)], &geometry, false);
        assert_eq!(inside.dock_place(), DockAt::Inside { node: 1, before: 3 });
        // The block's first row: nothing of the block above it, so the line is between slots.
        let ahead = session(&[(0, None, false), (1, Some(0), true), (1, Some(1), false), (1, Some(2), false)], &geometry, false);
        assert_eq!(ahead.dock_place(), DockAt::Before(1));
        // Its last row: what follows is the next slot.
        let behind = session(&[(0, None, false), (1, Some(0), false), (1, Some(1), true), (2, None, false)], &geometry, false);
        assert_eq!(behind.dock_place(), DockAt::Before(2));
        // Nothing below it stays: the line is at the foot, and with nothing at all staying, the head.
        let foot = session(&[(0, None, false), (1, None, true), (2, None, true), (3, None, true)], &geometry, false);
        assert_eq!(foot.dock_place(), DockAt::End);
        let all = session(&[(0, None, true), (1, None, true), (2, None, true), (3, None, true)], &geometry, false);
        assert_eq!(all.dock_place(), DockAt::End);
    }

    #[test]
    fn rows_set_down_next_to_each_other_share_a_block_and_a_card_stands_as_a_slot() {
        let geometry = column(6);
        // A row of one block, a row of another, a card, and a whole block of two rows.
        let drag = session(&[(0, Some(0), true), (0, Some(1), false), (1, Some(0), true), (1, Some(1), false), (2, None, true), (3, None, false)], &geometry, false);
        let layout = drag.dock_layout();
        assert_eq!(layout.groups.iter().map(|(pieces, _)| pieces.clone()).collect::<Vec<_>>(), vec![vec![0, 1], vec![2]]);
        // Two rows in one block's padding, a gap, the card.
        assert_eq!(layout.groups[0].1, 40.0 + 40.0 + BLOCK_PAD_TOP + BLOCK_PAD_BOTTOM);
        assert_eq!(layout.tops, vec![BLOCK_PAD_TOP, BLOCK_PAD_TOP + 40.0, 84.0 + SLOT_GAP]);
        assert_eq!(layout.height, 84.0 + SLOT_GAP + 40.0);
        assert!(!layout.first_is_slot && layout.last_is_slot, "only a card stands off from the rows around the dock");

        let whole = session(&[(0, Some(0), true), (0, Some(1), true), (1, None, false), (2, None, false)], &geometry, false);
        let layout = whole.dock_layout();
        assert_eq!(layout.groups.len(), 1);
        assert!(layout.first_is_slot && layout.last_is_slot, "a whole block is a slot of its own");
        assert_eq!(layout.height, 90.0 + BLOCK_PAD_TOP + BLOCK_PAD_BOTTOM, "from the top of its first row to the foot of its last, in its own padding");
    }

    #[test]
    fn the_stack_flies_out_goes_home_faded_and_turns_round_from_where_it_got_to() {
        let mut geometry = column(4);
        let mut drag = session(&[(0, None, true), (1, None, false), (2, None, true), (3, None, false)], &geometry, false);
        let start = Instant::now();
        let pointer = (30.0, 105.0);
        drag.set_look(&mut geometry, Look::Gathered, pointer, start);
        assert_eq!(drag.ghost.len(), 2);
        assert_eq!(geometry.reflows, 1, "closing up slides what it moves");
        // It sets out from where the piece sat, against the pointer, and arrives at its place in the stack.
        let first = &drag.ghost[0];
        let ((x, y, h), fade) = first.frame(start);
        assert_eq!((x, y, h, fade), (10.0 - pointer.0, 0.0 - pointer.1, 40.0, None));
        let (at_rest, _) = first.frame(start + MOTION);
        assert_eq!(at_rest, (first.x, first.y, first.height));
        assert!(drag.ghost_shown(start));

        // Set down at the home line: the stack flies onto the copies there, fading, and is gone when it lands.
        let later = start + MOTION * 2;
        drag.set_look(&mut geometry, Look::Docked, pointer, later);
        let flight = drag.ghost[0].flight.unwrap();
        assert!(flight.fade);
        assert_eq!(flight.to.1, 500.0 - pointer.1, "onto the first copy in the dock");
        assert!(drag.ghost_shown(later));
        assert!(!drag.ghost_shown(later + MOTION));
        assert_eq!(drag.ghost[0].frame(later + MOTION).1, Some(0.0));

        // Picked up again part of the way there, it turns round from where it has got to.
        let turned = later + MOTION / 2;
        let (drawn, _) = drag.ghost[0].frame(turned);
        drag.set_look(&mut geometry, Look::Gathered, pointer, turned);
        let ((x, y, h), fade) = drag.ghost[0].frame(turned);
        assert_eq!((x, y, h), drawn);
        assert_eq!(fade, None, "riding again, it is the stack's own opacity");
    }

    #[test]
    fn docked_pieces_counted_back_apart_fly_to_their_own_places() {
        let mut geometry = column(4);
        let mut drag = session(&[(0, None, true), (1, None, false), (2, None, true), (3, None, false)], &geometry, false);
        let now = Instant::now();
        drag.set_look(&mut geometry, Look::Gathered, (30.0, 105.0), now);
        drag.set_look(&mut geometry, Look::Docked, (30.0, 105.0), now + Duration::from_secs(1));
        assert!(geometry.flown.is_empty());
        drag.set_look(&mut geometry, Look::Apart, (30.0, 105.0), now + Duration::from_secs(2));
        assert_eq!(geometry.flown.len(), 2);
        // From the first copy at the dock's top, down to where it lives; the second from its own place in the dock.
        assert_eq!(geometry.flown[0], (0, 500.0));
        assert_eq!(geometry.flown[1].1, 500.0 + 40.0 + SLOT_GAP - 100.0);
    }

    #[test]
    fn with_motion_reduced_nothing_flies() {
        let mut geometry = column(4);
        let mut drag = session(&[(0, None, true), (1, None, false), (2, None, true), (3, None, false)], &geometry, true);
        let now = Instant::now();
        drag.set_look(&mut geometry, Look::Gathered, (30.0, 105.0), now);
        assert!(drag.ghost.iter().all(|entry| entry.flight.is_none()));
        assert!(drag.ghost_shown(now), "the stack rides as ever");
        drag.set_look(&mut geometry, Look::Docked, (30.0, 105.0), now);
        assert!(!drag.ghost_shown(now), "and is gone at once");
        drag.set_look(&mut geometry, Look::Apart, (30.0, 105.0), now);
        assert!(geometry.flown.is_empty());
    }

    #[test]
    fn the_motion_runs_on_upstreams_curve() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        // `cubic-bezier(.2, .75, .25, 1)` is most of the way there by half time, and never goes back.
        assert!((ease(0.5) - 0.9297).abs() < 0.01, "{}", ease(0.5));
        let steps: Vec<f32> = (0..=20).map(|at| ease(at as f32 / 20.0)).collect();
        assert!(steps.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}

fn turn_with_error(owns: &str) -> ConversationTurn {
    ConversationTurn { id: "turn".to_owned(), request_id: "req".to_owned(), anchor_context_id: owns.to_owned(), model_id: String::new(), started_at: AT.to_owned(), ended_at: None, duration_ms: None, status: ConversationTurnStatus::Completed, context_ids: vec![owns.to_owned()], usage: Default::default(), usage_offset: Default::default(), usage_baseline: Default::default(), usage_revision_at_start: 0, segment_count: 1, error: Some(ConversationTurnError { message: "boom".to_owned(), provider_name: "P".to_owned(), model_name: "M".to_owned(), at: AT.to_owned() }) }
}

fn wait_out_the_motion(rig: &mut Rig, view: &mut dyn FnMut(&mut App) -> E) {
    std::thread::sleep(mewrk_ui_chat::drag::MOTION + std::time::Duration::from_millis(20));
    rig.settle(view);
}

#[test]
fn a_drop_slides_what_stayed_and_lands_what_was_dragged_from_where_its_copy_was() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3", "p4"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "p1", "p3");
    let resting: Vec<f32> = ["p1", "p2", "p3", "p4"].iter().map(|id| card_rect(id).y).collect();
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p3");
    d.move_to(at.0, at.1 + 7.0);
    // Well away from where it began.
    d.move_to(at.0 + 80.0, 700.0);
    assert_eq!(d.snapshot().look, Some(Look::Gathered));
    assert!(d.snapshot().sliding > 0, "closing up over what left slides what it moves");
    wait_out_the_motion(&mut d.rig, d.view);
    d.rig.release(d.view, at.0 + 80.0, 700.0);
    d.rig.frame(d.view);
    assert_eq!(log.borrow().moved.len(), 1, "the drop lands");
    let snapshot = d.snapshot();
    assert!(!snapshot.drag_active);
    // These props do not move anything: each card lands back where it was, from the stack under the pointer.
    let slide = mewrk_ui_chat::frame::unit_slide(&mut d.rig.app, ID, "p3").expect("what was dragged lands");
    assert_eq!(slide.fade_from, Some(0.9));
    assert!(slide.dy > 100.0, "from down at the pointer: {}", slide.dy);
    assert!(slide.dx.abs() > 1.0, "and from where it rode sideways: {}", slide.dx);
    assert!(mewrk_ui_chat::frame::unit_slide(&mut d.rig.app, ID, "p1").is_some());
    assert!(snapshot.sliding > 0, "what stayed slides to where it is now");
    // It is only drawn there: drawn from a moment ago, and in the end where the layout has it.
    wait_out_the_motion(&mut d.rig, d.view);
    assert_eq!((d.snapshot().sliding, d.snapshot().landing), (0, 0));
    let settled: Vec<f32> = ["p1", "p2", "p3", "p4"].iter().map(|id| card_rect(id).y).collect();
    assert_eq!(settled, resting);
}

/// Flips a process-wide switch, so it runs on its own: `cargo test -p mewrk-ui-chat --test suite -- --ignored --exact timeline::with_motion_reduced_a_drop_just_lands`.
#[test]
#[ignore = "flips the process-wide reduced-motion switch"]
fn with_motion_reduced_a_drop_just_lands() {
    fluent::set_reduced_motion(true);
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3", "p4"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "p1", "p3");
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p3");
    d.move_to(at.0, at.1 + 7.0);
    d.move_to(at.0 + 80.0, 700.0);
    assert_eq!(d.snapshot().look, Some(Look::Gathered));
    assert_eq!((d.snapshot().sliding, d.snapshot().landing), (0, 0));
    d.rig.release(d.view, at.0 + 80.0, 700.0);
    d.rig.frame(d.view);
    fluent::set_reduced_motion(false);
    assert_eq!(log.borrow().moved.len(), 1);
    assert_eq!((d.snapshot().sliding, d.snapshot().landing), (0, 0));
}

#[test]
fn a_drag_called_off_settles_as_a_drop_does_and_one_let_go_apart_has_nothing_to_settle() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = card_rect("p2");
    modified_drag(&mut rig, &mut view, (r.x + 10.0, r.y + 3.0), (r.x + 10.0, r.y + 3.0));
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p2");
    d.move_to(at.0, at.1 + 7.0);
    d.move_to(at.0 + 80.0, 700.0);
    d.rig.key(d.view, Key::Escape, false, false);
    d.rig.frame(d.view);
    assert!(!d.snapshot().drag_active);
    assert!(mewrk_ui_chat::frame::unit_slide(&mut d.rig.app, ID, "p2").is_some(), "it lands from the stack");
    assert!(log.borrow().moved.is_empty());
    wait_out_the_motion(&mut d.rig, d.view);

    // Apart at the home line: the pieces are where they were, and letting go moves nothing.
    let at = d.press_on("p2");
    d.move_to(at.0, at.1 + 7.0);
    d.move_to(at.0 + 80.0, 700.0);
    d.move_to(at.0, at.1 - 6.0);
    assert_eq!(d.snapshot().look, Some(Look::Apart));
    wait_out_the_motion(&mut d.rig, d.view);
    d.rig.release(d.view, at.0, at.1 - 6.0);
    d.rig.frame(d.view);
    assert_eq!((d.snapshot().sliding, d.snapshot().landing), (0, 0));
    assert!(log.borrow().moved.is_empty());
}

#[test]
fn what_sits_above_the_drop_stays_put_as_the_rest_falls_into_place() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(tall(40), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let built = timeline_snapshot(&mut rig.app, ID).built;
    let id = |position: usize| format!("m{position}");
    let probe = id(built[built.len() - 6]);
    let r = card_rect(&probe);
    modified_drag(&mut rig, &mut view, (r.x + 10.0, r.y + 3.0), (r.x + 10.0, r.y + 3.0));
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on(&probe);
    d.move_to(at.0, at.1 + 7.0);
    d.move_to(at.0 + 80.0, 300.0);
    d.rig.frame(d.view);
    wait_out_the_motion(&mut d.rig, d.view);
    // The card just above the pointer.
    let steady = (0..40).map(id).filter(|name| *name != probe).filter_map(|name| rect_of(unit_key(0, &name)).map(|r| (name, r))).filter(|(_, r)| (r.y + r.bottom()) * 0.5 <= 300.0).last().expect("something above the pointer");
    d.rig.release(d.view, at.0 + 80.0, 300.0);
    d.rig.frame(d.view);
    d.rig.frame(d.view);
    d.rig.frame(d.view);
    let after = card_rect(&steady.0);
    assert!((after.bottom() - steady.1.bottom()).abs() < 2.0, "{} moved from {} to {}", steady.0, steady.1.bottom(), after.bottom());
}

#[test]
fn docks_among_a_blocks_rows_when_the_line_is_inside_it_and_makes_room_there() {
    let mut rig = Rig::new();
    let log = Log::default();
    let read = |id: &str| live(tool(id, "read", json!({ "path": id }), "x"));
    let contexts = vec![live(assistant("top", "above")), read("r1"), read("r2"), read("r3"), read("r4"), read("r5")];
    let p = props(contexts, &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    pick_two(&mut rig, &mut view, "r2", "r4");
    let (r1, r3) = (card_rect("r1"), card_rect("r3"));
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("r2");
    d.move_to(at.0, at.1 + 7.0);
    d.move_to(at.0 + 80.0, 700.0);
    // Back to the line, which is between r1 and r3.
    d.move_to(at.0, at.1 - 4.0);
    let snapshot = d.snapshot();
    assert_eq!(snapshot.look, Some(Look::Docked));
    let room = snapshot.dock.expect("the pieces take room");
    assert!(matches!(room.at, mewrk_ui_chat::drag::DockAt::Inside { .. }), "{:?}", room.at);
    // Two rows, in a block of their own padding, with nothing standing off.
    assert_eq!(room.lead, 0.0);
    assert!((room.height - (2.0 * r1.h + 4.0)).abs() < 2.0, "{} for rows of {}", room.height, r1.h);
    std::thread::sleep(mewrk_ui_chat::drag::MOTION + std::time::Duration::from_millis(20));
    d.rig.frame(d.view);
    d.rig.frame(d.view);
    // r1 stays and r3 is pushed down, inside the one block; the line is the copies' top.
    let (r1_now, r3_now) = (card_rect("r1"), card_rect("r3"));
    assert!((r3_now.y - r1_now.bottom() - room.height).abs() < 2.0, "{} under {} with {} between", r3_now.y, r1_now.bottom(), room.height);
    assert!(r3_now.y > r3.y - 1.0);
    let line = d.snapshot().mark_y.unwrap();
    assert!((line - r1_now.bottom()).abs() < 2.0, "{line} at {}", r1_now.bottom());
}

#[test]
fn nothing_stands_at_the_head_of_the_column_for_what_left_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["p1", "p2", "p3"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let head = card_rect("p1").y;
    let r = card_rect("p1");
    modified_drag(&mut rig, &mut view, (r.x + 10.0, r.y + 3.0), (r.x + 10.0, r.y + 3.0));
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("p1");
    d.move_to(at.0, at.1 + 7.0);
    wait_out_the_motion(&mut d.rig, d.view);
    assert!((card_rect("p2").y - head).abs() < 1.0, "p2 at {} takes the head at {head}", card_rect("p2").y);
}

#[test]
fn what_a_row_drew_for_a_drag_is_not_what_it_measures_at_rest() {
    let mut rig = Rig::new();
    let log = Log::default();
    let read = |id: &str| live(tool(id, "read", json!({ "path": id }), "x"));
    let p = props(vec![live(assistant("top", "above")), read("r1"), read("r2"), read("r3"), live(assistant("end", "below"))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let total = timeline_snapshot(&mut rig.app, ID).total;
    let r = card_rect("r2");
    modified_drag(&mut rig, &mut view, (r.x + 10.0, r.y + 3.0), (r.x + 10.0, r.y + 3.0));
    let mut d = DragRig { rig, view: &mut view };
    let at = d.press_on("r2");
    d.move_to(at.0, at.1 + 7.0);
    wait_out_the_motion(&mut d.rig, d.view);
    assert!((d.snapshot().total - (total - r.h)).abs() < 1.0, "{} after taking {} out of {total}", d.snapshot().total, r.h);
    d.rig.key(d.view, Key::Escape, false, false);
    wait_out_the_motion(&mut d.rig, d.view);
    assert!((d.snapshot().total - total).abs() < 1.0, "{} back to {total}", d.snapshot().total);
}

#[test]
fn a_wheel_an_output_pane_takes_does_not_let_go_of_the_bottom() {
    let mut rig = Rig::new();
    let log = Log::default();
    let long_output: String = (0..400).map(|at| format!("line {at}\n")).collect();
    let mut contexts = tall(3);
    contexts.push(live(tool("t", "bash", json!({ "command": "seq 400" }), &long_output)));
    let p = props(contexts, &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = card_rect("t");
    rig.click(&mut view, r.x + 200.0, r.y + r.h * 0.5);
    rig.settle(&mut view);
    scroll_request(scroller(), ScrollRequest::ToEnd);
    rig.settle(&mut view);
    let body = rect_of(mewrk_ui_chat::tool_detail::tool_body_key("t")).expect("the row is open");
    rig.point(body.x + 40.0, body.y + body.h * 0.5);
    rig.frame(&mut view);
    rig.wheel(&mut view, -200.0);
    rig.settle(&mut view);
    assert!(timeline_snapshot(&mut rig.app, ID).pinned);
    let before = scroll_state(mewrk_ui_chat::tool_detail::tool_body_key("t")).expect("it scrolls").0;
    assert!(before > 0.0, "the pane took it going down");
    rig.wheel(&mut view, 60.0);
    rig.settle(&mut view);
    assert!(scroll_state(mewrk_ui_chat::tool_detail::tool_body_key("t")).unwrap().0 < before, "and going up");
    assert!(timeline_snapshot(&mut rig.app, ID).pinned, "the timeline did not move, so the reader has not left the bottom");
}

// ---------------------------------------------------------------- what outlives a row scrolled away

#[test]
fn an_editor_scrolled_far_from_the_view_keeps_what_was_typed() {
    use mewrk_ui_chat::props::TimelineEditorState;
    let mut rig = Rig::new();
    let log = Log::default();
    let contexts = tall(60);
    let item = contexts[0].item.clone();
    let mut p = props(contexts, &log);
    p.on_cancel_edit = Some(Rc::new(|_| {}));
    p.on_save_text = Some(Rc::new(|_, _, _, _| {}));
    p.editor = Some(TimelineEditorState::Edit { kind: InsertKind::Assistant, item, index: 0 });
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    scroll_request(scroller(), ScrollRequest::Shift(-1_000_000.0));
    rig.settle(&mut view);
    let k = hash_key(("tl-text-edit", 0u64, "m0"));
    let draft = |_: ()| use_state(("text-editor-edit", k), fluent::edit::TextEdit::new);
    draft(()).borrow_mut().set_text("typed by hand".to_owned());
    scroll_request(scroller(), ScrollRequest::Shift(1_000_000.0));
    rig.settle(&mut view);
    assert!(rect_of(unit_key(0, "m0")).is_none(), "the row is not built");
    rig.settle(&mut view);
    scroll_request(scroller(), ScrollRequest::Shift(-1_000_000.0));
    rig.settle(&mut view);
    assert_eq!(draft(()).borrow().text, "typed by hand");
}

#[test]
fn a_row_opened_by_its_editor_stays_open_when_the_editor_goes() {
    use mewrk_ui_chat::props::TimelineEditorState;
    let mut rig = Rig::new();
    let log = Log::default();
    let contexts = vec![live(reasoning("r", Some("a thought"))), live(tool("t", "read", json!({ "path": "a" }), "x"))];
    let item = contexts[1].item.clone();
    let closed = {
        let p = props(contexts.clone(), &log);
        let mut view = |app: &mut App| root(app, &p);
        rig.settle(&mut view);
        card_rect("t").h
    };
    let mut editing = props(contexts.clone(), &log);
    editing.on_cancel_edit = Some(Rc::new(|_| {}));
    editing.on_save_tool_edit = Some(Rc::new(|_, _, _, _, _| {}));
    editing.editor = Some(TimelineEditorState::Edit { kind: InsertKind::Tool, item, index: 1 });
    let mut view = |app: &mut App| root(app, &editing);
    rig.settle(&mut view);
    assert!(card_rect("t").h > closed + 20.0);
    let p = props(contexts, &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    assert!(card_rect("t").h > closed + 20.0, "still open, onto its detail");
}

#[test]
fn shift_f10_on_a_row_opens_the_menu_with_the_line_below_it() {
    let mut rig = Rig::new();
    let log = Log::default();
    let read = |id: &str| live(tool(id, "read", json!({ "path": id }), "x"));
    let p = props(vec![read("r1"), read("r2"), live(assistant("a", "after"))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    // A click on a row leaves the focus in it, as one on its button did, and it answers for the row itself: the line goes right below it.
    let r = card_rect("r1");
    rig.click(&mut view, r.x + 300.0, r.y + r.h * 0.5);
    rig.key(&mut view, Key::F(10), false, true);
    rig.settle(&mut view);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(1));
    rig.key(&mut view, Key::Escape, false, false);
    rig.settle(&mut view);
    // With the focus on the block around the rows, it goes below the block.
    focus_request_id(hash_key(("tl-block", 0u64, "r1")));
    rig.settle(&mut view);
    rig.key(&mut view, Key::F(10), false, true);
    rig.settle(&mut view);
    assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(2));
}

#[test]
fn a_failure_notice_keeps_no_distance_from_what_is_around_it() {
    // `.turn-error` has no margin of its own, and hides the slot under it from the rule that spaces one slot from the next.
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(vec![live(user("u", "ask")), live(assistant("a", "reply")), live(user("next", "again"))], &log);
    p.turns = Rc::new(vec![turn_with_error("a")]);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let (reply, next) = (card_rect("a"), card_rect("next"));
    let notice = rect_of(("tl-turn-error", "turn")).expect("the notice is drawn");
    assert!((notice.y - reply.bottom()).abs() < 1.0, "{} under {}", notice.y, reply.bottom());
    assert!((next.y - notice.bottom()).abs() < 1.0, "{} under {}", next.y, notice.bottom());
}

#[test]
fn a_message_takes_up_all_the_room_its_markdown_paints() {
    let mut rig = Rig::new();
    let log = Log::default();
    let reply = "Words with `code` and a path `src/main.rs:12`.\n\n```rust\nfn main() {}\n```\n\nInline math $a^2 + b^2 = c^2$ and a [link](https://example.com).";
    let p = props(vec![live(assistant("one", reply)), live(assistant("two", "Next."))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let (one, two) = (card_rect("one"), card_rect("two"));
    assert!((two.y - one.bottom() - 16.0).abs() < 1.0, "{one:?} {two:?}");
    let leaf = rect_of(mewrk_ui_common::markdown::markdown_key(("tl-assistant", "one"))).expect("a leaf");
    assert!(leaf.bottom() <= one.bottom() + 0.5, "{leaf:?} in {one:?}");
    eprintln!("leaf {leaf:?}");
}


/// Layout rounds a box after asking for its height at the unrounded width: a line that only just fits must not wrap when painted.
#[test]
fn a_wrap_decided_by_a_fraction_of_a_pixel_does_not_overflow_the_room_it_took() {
    let mut rig = Rig::new();
    let log = Log::default();
    let reply = "Here is what I found, with **bold**, *emphasis*, `inline code` and a path `src/main.rs:12`.\n\nThe end.";
    for width in 600..640 {
        rig.size = (width as f32 + 0.37, 800.0);
        let mut p = props(vec![live(assistant("one", reply)), live(assistant("two", "Next."))], &log);
        p.timeline_id = Some(format!("wrap-{width}"));
        let mut view = |app: &mut App| root(app, &p);
        rig.settle(&mut view);
        let (one, two) = (card_rect("one"), card_rect("two"));
        assert!((two.y - one.bottom() - 16.0).abs() < 1.0, "at {width}: {one:?} {two:?}");
        let leaf = rect_of(mewrk_ui_common::markdown::markdown_key(("tl-assistant", "one"))).expect("a leaf");
        assert!(leaf.bottom() <= one.bottom() + 0.5, "at {width}: the leaf {leaf:?} overflows its card {one:?}");
        let lines_tall = (leaf.h / 22.0).round() as usize;
        assert!((3..=5).contains(&lines_tall), "at {width}: {leaf:?}");
        let glyphs: usize = rig.scene.layers.iter().map(|layer| layer.glyphs.len()).sum();
        assert!(glyphs > 50, "at {width} the words are on the screen: {glyphs}");
    }
}

// ---------------------------------------------------------------- the handlers, on the real host

mod live_conversation {
    use super::*;
    use mewrk_client::doc::NewConversation;
    use mewrk_client::model::AttachedWorkspace;
    use mewrk_ui_chat::handlers::*;

    fn start(rig: &mut Rig, contexts: Vec<ContextItem>) -> (String, String) {
        rig.app.add_workspace("work", &[] as &[AttachedWorkspace], Some(false));
        let conversation = rig.app.create_conversation(NewConversation::default()).expect("a conversation");
        let workspace = rig.app.workspace_of_conversation(&conversation).expect("its workspace").id.clone();
        rig.app.select_conversation(&workspace, &conversation);
        // The host answers the creation with the document as it saved it, over whatever was written meanwhile.
        for _ in 0..40 {
            rig.app.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        rig.app.update_conversation(&workspace, &conversation, move |held| held.contexts = contexts);
        (workspace, conversation)
    }

    fn ids_of(rig: &mut Rig, conversation: &str) -> Vec<String> {
        rig.app.timeline_contexts_of(conversation).iter().map(|context| context.id().to_owned()).collect()
    }

    fn view(conversation: String, tools: Rc<Vec<mewrk_client::model::ToolDescriptor>>) -> impl FnMut(&mut App) -> E {
        move |app: &mut App| {
            let enabled = Rc::new(tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>());
            let props = timeline_props(app, &conversation, tools.clone(), enabled);
            col().w_full().h_full().child(streamed_conversation_view(app, props)).child(timeline_dialogs(app))
        }
    }

    #[test]
    fn deletes_a_tool_row_and_restores_it_with_one_undo() {
        let mut rig = Rig::new();
        let (_, conversation) = start(&mut rig, vec![user("u", "hi"), tool("t", "read", json!({ "path": "a" }), "x"), assistant("a", "done")]);
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        let contexts = rig.app.timeline_contexts_of(&conversation);
        let target = contexts.iter().find(|context| context.id() == "t").unwrap().clone();
        delete_context(&mut rig.app, &target, true);
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u", "a"]);
        assert!(rect_of(unit_key(0, "t")).is_none(), "gone from the screen too");
        rig.app.step_timeline_history(&conversation, mewrk_client::runs::TimelineHistoryAction::Undo);
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u", "t", "a"]);
        assert!(rect_of(unit_key(0, "t")).is_some());
    }

    #[test]
    fn asks_before_deleting_when_the_preference_is_on_and_deletes_nothing_unless_the_answer_is_yes() {
        let mut rig = Rig::new();
        let (_, conversation) = start(&mut rig, vec![user("u", "hi"), assistant("a", "done")]);
        rig.app.doc.global_settings.appearance.confirm_message_delete = true;
        let mut appearance = rig.app.appearance_snapshot().clone();
        appearance.confirm_message_delete = true;
        rig.app.publish_appearance(appearance);
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        let target = rig.app.timeline_contexts_of(&conversation)[1].clone();
        delete_context(&mut rig.app, &target, false);
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u", "a"]);
        rig.click_key(&mut v, "timeline-confirm-cancel");
        assert_eq!(ids_of(&mut rig, &conversation), ["u", "a"]);
        delete_context(&mut rig.app, &target, false);
        rig.settle(&mut v);
        rig.click_key(&mut v, "timeline-confirm-accept");
        assert_eq!(ids_of(&mut rig, &conversation), ["u"]);
    }

    #[test]
    fn edits_text_in_place_and_clears_an_interrupted_mark() {
        let mut rig = Rig::new();
        let mut cut = assistant("a", "partial");
        if let ContextItem::Assistant { interrupted, .. } = &mut cut {
            *interrupted = true;
        }
        let (_, conversation) = start(&mut rig, vec![user("u", "hi"), cut.clone()]);
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        handle_context_edit(&mut rig.app, &cut);
        rig.settle(&mut v);
        save_text_context(&mut rig.app, "whole".to_owned(), Vec::new(), Vec::new());
        rig.settle(&mut v);
        let contexts = rig.app.timeline_contexts_of(&conversation);
        assert!(matches!(&contexts[1], ContextItem::Assistant { content, interrupted: false, .. } if content == "whole"));
        rig.app.step_timeline_history(&conversation, mewrk_client::runs::TimelineHistoryAction::Undo);
        rig.settle(&mut v);
        assert_eq!(rig.app.timeline_contexts_of(&conversation)[1], cut);
    }

    #[test]
    fn refuses_to_edit_encrypted_reasoning_at_every_entry_point() {
        let mut rig = Rig::new();
        let encrypted = reasoning("r", None);
        let (_, conversation) = start(&mut rig, vec![encrypted.clone()]);
        let mut v = view(conversation, Rc::new(descriptors()));
        rig.settle(&mut v);
        handle_context_edit(&mut rig.app, &encrypted);
        assert!(rig.app.local::<TimelineEdits>().editor.is_none());
    }

    #[test]
    fn inserts_text_at_the_line_and_as_plaintext_reasoning() {
        let mut rig = Rig::new();
        let (_, conversation) = start(&mut rig, vec![user("u", "hi"), assistant("a", "done")]);
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        handle_context_insert(&mut rig.app, 1, InsertKind::Reasoning, None);
        rig.settle(&mut v);
        save_text_context(&mut rig.app, "a thought".to_owned(), Vec::new(), Vec::new());
        rig.settle(&mut v);
        let contexts = rig.app.timeline_contexts_of(&conversation);
        assert_eq!(contexts.len(), 3);
        assert!(matches!(&contexts[1], ContextItem::Reasoning { form: Some(mewrk_client::model::ReasoningForm::Plaintext), content: Some(text), .. } if text == "a thought"));
    }

    #[test]
    fn copies_two_picked_messages_and_pastes_them_over_a_third_which_one_undo_brings_back() {
        let mut rig = Rig::new();
        let (_, conversation) = start(&mut rig, vec![user("u1", "one"), assistant("a1", "two"), user("u2", "three")]);
        let tools = descriptors();
        let mut v = view(conversation.clone(), Rc::new(tools.clone()));
        rig.settle(&mut v);
        copy_timeline_contexts(&mut rig.app, vec!["u1".to_owned(), "a1".to_owned()], &tools);
        rig.settle(&mut v);
        assert_eq!(rig.clipboard.0.as_deref(), Some("one\n\ntwo"), "the text reaches the system clipboard too");
        paste_timeline_contexts(&mut rig.app, 2, vec!["u2".to_owned()]);
        rig.settle(&mut v);
        let after = ids_of(&mut rig, &conversation);
        assert_eq!(after.len(), 4);
        assert_eq!(&after[..2], ["u1", "a1"]);
        assert!(!after.contains(&"u2".to_owned()), "replaced");
        assert!(after[2] != "u1" && after[3] != "a1", "new cards with new ids");
        rig.app.step_timeline_history(&conversation, mewrk_client::runs::TimelineHistoryAction::Undo);
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u1", "a1", "u2"]);
    }

    #[test]
    fn moves_two_picked_messages_below_the_last_in_one_drag_which_one_undo_takes_back() {
        let mut rig = Rig::new();
        let (_, conversation) = start(&mut rig, vec![user("u1", "one"), assistant("a1", "two"), user("u2", "three"), assistant("a2", "four")]);
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        let (first, second) = (card_rect("u1"), card_rect("a1"));
        modified_drag(&mut rig, &mut v, (first.x + first.w * 0.5, first.y + 3.0), (first.x + first.w * 0.5, first.y + 3.0));
        modified_drag(&mut rig, &mut v, (second.x + 10.0, second.y + 3.0), (second.x + 10.0, second.y + 3.0));
        let shared_id = conversation.clone();
        assert_eq!(timeline_snapshot(&mut rig.app, &shared_id).selection, ["u1", "a1"]);
        let at = (second.x + 40.0, second.y + second.h * 0.5);
        rig.press(&mut v, at.0, at.1);
        rig.point(at.0, at.1 + 8.0);
        rig.frame(&mut v);
        rig.frame(&mut v);
        rig.point(at.0 + 200.0, 780.0);
        rig.frame(&mut v);
        rig.frame(&mut v);
        rig.release(&mut v, at.0 + 200.0, 780.0);
        rig.frame(&mut v);
        // What was dragged lands from the stack under the pointer, on the timeline as it now stands.
        let landing = mewrk_ui_chat::frame::unit_slide(&mut rig.app, &shared_id, "a1").expect("it lands");
        assert_eq!(landing.fade_from, Some(0.9));
        assert!(landing.dx > 100.0, "from where it rode: {landing:?}");
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u2", "a2", "u1", "a1"]);
        std::thread::sleep(mewrk_ui_chat::drag::MOTION + std::time::Duration::from_millis(20));
        rig.settle(&mut v);
        assert_eq!(timeline_snapshot(&mut rig.app, &shared_id).landing, 0);
        assert!(card_rect("u2").y < card_rect("a2").y && card_rect("a2").y < card_rect("u1").y && card_rect("u1").y < card_rect("a1").y, "and in the end each is where the layout has it");
        rig.app.step_timeline_history(&conversation, mewrk_client::runs::TimelineHistoryAction::Undo);
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u1", "a1", "u2", "a2"]);
    }

    #[test]
    fn does_not_move_a_compaction_and_says_so() {
        let mut rig = Rig::new();
        let (_, conversation) = start(&mut rig, vec![user("u1", "one"), compaction("c"), assistant("a2", "four")]);
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        move_timeline_contexts(&mut rig.app, vec!["c".to_owned()], 3);
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u1", "c", "a2"]);
        assert!(rig.app.timeline_notice().is_some_and(|notice| notice.message.contains("cannot be moved")));
    }

    #[test]
    fn draws_a_conversation_through_its_real_store_with_branch_navigation_under_the_fork() {
        let mut rig = Rig::new();
        let (workspace, conversation) = start(&mut rig, vec![user("u1", "one"), assistant("a1", "two")]);
        rig.app.update_conversation(&workspace, &conversation, |held| {
            held.branches = serde_json::from_value(json!([{ "forkContextId": "u1", "activeBranchId": "b1", "branches": [{ "id": "b1", "contexts": [], "createdAt": AT }, { "id": "b2", "contexts": [{ "id": "a9", "kind": "assistant", "content": "other", "createdAt": AT }], "createdAt": AT }] }])).unwrap_or_default();
        });
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        assert!(rect_of(unit_key(0, "u1")).is_some());
    }

    #[test]
    fn rewinds_the_conversation_only_without_touching_a_file_and_one_undo_brings_it_back() {
        let mut rig = Rig::new();
        let (_, conversation) = start(&mut rig, vec![user("u1", "one"), assistant("a1", "two"), user("u2", "three"), assistant("a2", "four")]);
        let mut v = view(conversation.clone(), Rc::new(descriptors()));
        rig.settle(&mut v);
        let target = rig.app.timeline_contexts_of(&conversation)[2].clone();
        open_rewind_dialog(&mut rig.app, &target, None);
        for _ in 0..200 {
            rig.frame(&mut v);
            if rig.app.local::<TimelineEdits>().rewind.as_ref().is_some_and(|state| !matches!(state.preview, mewrk_ui_chat::rewind::RewindPreview::Loading)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        rig.settle(&mut v);
        assert!(rig.app.local::<TimelineEdits>().rewind.is_some(), "the window is up");
        assert!(rect_of("rewind-conversation-only").is_some());
        rig.click_key(&mut v, "rewind-conversation-only");
        assert_eq!(ids_of(&mut rig, &conversation), ["u1", "a1"]);
        assert!(rig.app.local::<TimelineEdits>().rewind.is_none());
        assert_eq!(rig.app.composer_draft(&conversation).as_str(), "three", "the message goes back to the composer");
        rig.app.step_timeline_history(&conversation, mewrk_client::runs::TimelineHistoryAction::Undo);
        rig.settle(&mut v);
        assert_eq!(ids_of(&mut rig, &conversation), ["u1", "a1", "u2", "a2"]);
    }

    #[test]
    fn keeps_the_rewind_entry_off_while_a_model_turn_is_running() {
        use mewrk_ui_chat::rewind::{rewind_preview_summary, rewind_result_summary, summarize_rewind_files, RewindFileEntry};
        let entry = |action: &str, skipped: Option<&str>| RewindFileEntry { path: "a".to_owned(), display_path: "a".to_owned(), action: action.to_owned(), skipped: skipped.map(str::to_owned) };
        let summary = summarize_rewind_files(&[entry("restore", None), entry("delete", None), entry("restore", Some("read-only"))]);
        assert_eq!((summary.restored, summary.deleted, summary.skipped), (1, 1, 1));
        assert_eq!(rewind_preview_summary(&summary), "Will restore 1 file, delete 1 file created since then; 1 will be skipped");
        assert_eq!(rewind_result_summary(&summary), "restored 1 file, deleted 1 file created since then; skipped 1: read-only");
        assert_eq!(rewind_result_summary(&summarize_rewind_files(&[])), "no file was restored");
    }
}


// ---------------------------------------------------------------- more of ContextStream.test.tsx

#[test]
fn offers_exactly_five_insertable_context_kinds() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(messages(&["a"]), &log);
    p.on_save_tool_edit = Some(Rc::new(|_, _, _, _, _| {}));
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = card_rect("a");
    rig.right_click(&mut view, r.x + 10.0, r.y + 2.0);
    rig.settle(&mut view);
    for id in ["System prompt", "User input", "Reasoning field", "tool", "Model reply", "paste", "fork"] {
        assert!(rect_of(menu_row(&[], id)).is_some(), "{id} is offered");
    }
    assert!(rect_of(menu_row(&[], "Encrypted reasoning")).is_none());
}

#[test]
fn a_timeline_with_no_way_to_commit_a_placed_call_does_not_offer_to_place_one() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(messages(&["a"]), &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let r = card_rect("a");
    rig.right_click(&mut view, r.x + 10.0, r.y + 2.0);
    rig.settle(&mut view);
    assert!(rect_of(menu_row(&[], "tool")).is_none());
    assert!(rect_of(menu_row(&[], "Model reply")).is_some());
}

#[test]
fn draws_the_insert_editor_at_the_line_and_a_row_for_kinds_that_live_in_one() {
    use mewrk_ui_chat::props::TimelineEditorState;
    let mut rig = Rig::new();
    let log = Log::default();
    let cancelled = Rc::new(RefCell::new(0));
    for kind in [InsertKind::System, InsertKind::Reasoning, InsertKind::User, InsertKind::Assistant] {
        let sink = cancelled.clone();
        let mut p = props(messages(&["a", "b"]), &log);
        p.timeline_id = Some(format!("insert-{kind:?}"));
        p.on_cancel_edit = Some(Rc::new(move |_| *sink.borrow_mut() += 1));
        p.on_save_text = Some(Rc::new(|_, _, _, _| {}));
        p.editor = Some(TimelineEditorState::Insert { kind, index: 1, tool_name: None });
        let mut view = |app: &mut App| root(app, &p);
        rig.settle(&mut view);
        let (a, b) = (card_rect("a"), card_rect("b"));
        assert!(b.y - a.bottom() > 60.0, "{kind:?} sits between them: {a:?} {b:?}");
        assert_eq!(rect_of(unit_key(0, "draft")).is_some(), matches!(kind, InsertKind::System | InsertKind::Reasoning), "{kind:?}");
    }
}

#[test]
fn an_insert_at_the_end_lands_after_everything() {
    use mewrk_ui_chat::props::TimelineEditorState;
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(messages(&["a"]), &log);
    p.on_cancel_edit = Some(Rc::new(|_| {}));
    p.on_save_tool_edit = Some(Rc::new(|_, _, _, _, _| {}));
    p.editor = Some(TimelineEditorState::Insert { kind: InsertKind::Tool, index: 1, tool_name: Some("read".to_owned()) });
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let draft = mewrk_ui_chat::inline_tool_editor::draft_tool_context("read");
    let row = rect_of(unit_key(0, draft.id())).expect("the call being placed is drawn as its row");
    assert!(row.y >= card_rect("a").bottom());
}

#[test]
fn edits_any_card_in_place() {
    use mewrk_ui_chat::props::TimelineEditorState;
    let mut rig = Rig::new();
    let log = Log::default();
    let contexts = vec![live(user("u", "hello")), live(assistant("a", "world")), live(reasoning("r", Some("a thought"))), live(system("s", "the prompt")), live(tool("t", "read", json!({ "path": "a" }), "x"))];
    for (id, kind, field_inside) in [("u", InsertKind::User, true), ("a", InsertKind::Assistant, true), ("r", InsertKind::Reasoning, true), ("s", InsertKind::System, true), ("t", InsertKind::Tool, true)] {
        let item = contexts.iter().find(|context| context.id() == id).unwrap().item.clone();
        let mut p = props(contexts.clone(), &log);
        p.timeline_id = Some(format!("edit-{id}"));
        p.on_cancel_edit = Some(Rc::new(|_| {}));
        p.on_save_text = Some(Rc::new(|_, _, _, _| {}));
        p.on_save_tool_edit = Some(Rc::new(|_, _, _, _, _| {}));
        p.editor = Some(TimelineEditorState::Edit { kind, item, index: 0 });
        let mut view = |app: &mut App| root(app, &p);
        rig.settle(&mut view);
        let scroller_ = hash_key(("context-scroll", format!("edit-{id}").as_str()));
        scroll_request(scroller_, ScrollRequest::Shift(-10_000.0));
        rig.settle(&mut view);
        assert!(field_inside);
        assert!(rect_of(unit_key(0, id)).is_some_and(|r| r.h > 30.0), "{id} shows an editor");
    }
}

#[test]
fn the_system_prompt_is_a_row_that_opens_onto_the_prompt() {
    let mut rig = Rig::new();
    let log = Log::default();
    let p = props(vec![live(system("s", "Be careful.\nAnd kind.")), live(user("u", "hi"))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let scroller_ = scroller();
    scroll_request(scroller_, ScrollRequest::Shift(-10_000.0));
    rig.settle(&mut view);
    let closed = card_rect("s").h;
    let r = card_rect("s");
    rig.click(&mut view, r.x + r.w * 0.5, r.y + 12.0);
    rig.settle(&mut view);
    assert!(card_rect("s").h > closed + 10.0);
}

#[test]
fn the_normal_assistant_card_is_used_while_streaming_with_its_mutations_off() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut p = props(vec![streaming(assistant("a", "partial words"), None)], &log);
    p.streaming = true;
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let delete = rect_of(("tl-delete", 0u64, "a")).expect("drawn, disabled");
    rig.click(&mut view, delete.x + 2.0, delete.y + 2.0);
    assert!(log.borrow().removed.is_empty());
    // One indicator hosted at the end of the stream, since no turn hosts it.
    assert!(card_rect("a").h > 10.0);
}

#[test]
fn branches_and_rewinds_from_real_user_messages_only_and_shows_why_not() {
    let mut rig = Rig::new();
    let log = Log::default();
    let branched: Rc<RefCell<Vec<String>>> = Rc::default();
    let rewound: Rc<RefCell<Vec<String>>> = Rc::default();
    let (b, r) = (branched.clone(), rewound.clone());
    let mut p = props(vec![live(user("u", "hello")), live(assistant("a", "world")), live(user("ctx_agent-result_1", "a note the host wrote"))], &log);
    p.on_branch_from = Some(Rc::new(move |_, item| b.borrow_mut().push(item.id().to_owned())));
    p.on_rewind_to = Some(Rc::new(move |_, item| r.borrow_mut().push(item.id().to_owned())));
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    assert!(rect_of(("tl-branch-from", 0u64, "a")).is_none(), "a reply has no branch");
    assert!(rect_of(("tl-rewind", 0u64, "ctx_agent-result_1")).is_none(), "the host's note was never sent by the user");
    let card = card_rect("u");
    rig.point(card.x + 5.0, card.y + 3.0);
    rig.settle(&mut view);
    let branch = rect_of(("tl-branch-from", 0u64, "u")).expect("branch");
    rig.click(&mut view, branch.x + 2.0, branch.y + 2.0);
    assert_eq!(*branched.borrow(), ["u"]);
    let rewind = rect_of(("tl-rewind", 0u64, "u")).expect("rewind");
    rig.click(&mut view, rewind.x + 2.0, rewind.y + 2.0);
    assert_eq!(*rewound.borrow(), ["u"]);

    let mut held = p.clone();
    held.timeline_id = Some("held".to_owned());
    held.rewind_disabled_reason = Some("busy".to_owned());
    held.branch_from_disabled_reason = Some("deleting".to_owned());
    let mut view = |app: &mut App| root(app, &held);
    rig.settle(&mut view);
    let rewind = rect_of(("tl-rewind", 0u64, "u")).expect("rewind, disabled");
    rig.click(&mut view, rewind.x + 2.0, rewind.y + 2.0);
    let branch = rect_of(("tl-branch-from", 0u64, "u")).expect("branch, disabled");
    rig.click(&mut view, branch.x + 2.0, branch.y + 2.0);
    assert_eq!((branched.borrow().len(), rewound.borrow().len()), (1, 1));
}

#[test]
fn navigates_stable_sibling_positions_below_their_fork_message() {
    use mewrk_client::conversation_branches::ContextBranchNavigation;
    let mut rig = Rig::new();
    let log = Log::default();
    let picked: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
    let sink = picked.clone();
    let mut p = props(vec![live(user("u", "hello")), live(assistant("a", "world"))], &log);
    p.branch_navigations = Rc::new(std::collections::HashMap::from([("u".to_owned(), ContextBranchNavigation { active_index: 1, branch_ids: vec!["b0".to_owned(), "b1".to_owned(), "b2".to_owned()] })]));
    p.on_select_branch = Some(Rc::new(move |_, fork, branch| sink.borrow_mut().push((fork.to_owned(), branch.to_owned()))));
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let nav = rect_of(("tl-branches", 0u64, "u")).expect("under the bubble");
    assert!(nav.y >= card_rect("u").bottom(), "it never covers a word of the message");
    assert!(nav.bottom() <= card_rect("a").y, "and the card keeps the room for it");
    let previous = rect_of(("tl-branch", "previous", 0u64, "u".to_owned())).unwrap();
    rig.click(&mut view, previous.x + 2.0, previous.y + 2.0);
    let next = rect_of(("tl-branch", "next", 0u64, "u".to_owned())).unwrap();
    rig.click(&mut view, next.x + 2.0, next.y + 2.0);
    assert_eq!(*picked.borrow(), [("u".to_owned(), "b0".to_owned()), ("u".to_owned(), "b2".to_owned())]);
    let mut held = p.clone();
    held.timeline_id = Some("held".to_owned());
    held.branch_switch_disabled_reason = Some("busy".to_owned());
    let mut view = |app: &mut App| root(app, &held);
    rig.settle(&mut view);
    let next = rect_of(("tl-branch", "next", 0u64, "u".to_owned())).unwrap();
    rig.click(&mut view, next.x + 2.0, next.y + 2.0);
    assert_eq!(picked.borrow().len(), 2);
}

#[test]
fn copies_a_message_whole_read_only_or_not_and_offers_no_copy_of_nothing() {
    let mut rig = Rig::new();
    let log = Log::default();
    let image_only = ContextItem::User { id: "img".to_owned(), content: String::new(), images: vec![image("i", 1)], files: Vec::new(), created_at: AT.to_owned() };
    let mut p = props(vec![live(user("u", "hello there")), live(image_only), live(assistant("a", "the reply")), streaming(reasoning("r", Some("still thinking")), None)], &log);
    p.editable = false;
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let copy_key = |id: &str| ("copy", hash_key(("tl-copy", 0u64, id)));
    assert!(rect_of(copy_key("img")).is_none(), "no empty text to copy");
    assert!(rect_of(copy_key("r")).is_none(), "a thought still being written has no settled text");
    let card = card_rect("u");
    rig.point(card.x + 5.0, card.y + 3.0);
    rig.settle(&mut view);
    let copy = rect_of(copy_key("u")).expect("copy stays on a read-only timeline");
    rig.click(&mut view, copy.x + 2.0, copy.y + 2.0);
    assert_eq!(rig.clipboard.0.as_deref(), Some("hello there"));
}

#[test]
fn renders_citations_as_chips_and_only_a_trusted_one_is_a_link() {
    let mut rig = Rig::new();
    let log = Log::default();
    let mut cited = assistant("a", "An answer.");
    if let ContextItem::Assistant { sources, .. } = &mut cited {
        *sources = vec![mewrk_client::model::ContextSource { id: "s1".to_owned(), url: Some("https://docs.example.com/x".to_owned()), title: None }, mewrk_client::model::ContextSource { id: "s2".to_owned(), url: Some("javascript:alert(1)".to_owned()), title: Some("Bad".to_owned()) }];
    }
    let p = props(vec![live(cited)], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    assert!(rect_of(("tl-source", 0u64, "a", "s1")).is_some());
    assert!(rect_of(("tl-source", 0u64, "a", "s2")).is_some(), "citations without a trusted URL are listed too");
}

#[test]
fn opens_the_file_a_row_names_in_a_page_of_its_own_without_opening_the_row() {
    use mewrk_pane_files::links::PathOpenRequest;
    let mut rig = Rig::new();
    let log = Log::default();
    let opened: Rc<RefCell<Vec<PathOpenRequest>>> = Rc::default();
    let sink = opened.clone();
    mewrk_pane_files::path_open::set_path_open_handler(Rc::new(move |_: &mut App, request: &PathOpenRequest| {
        sink.borrow_mut().push(request.clone());
        true
    }));
    let mut p = props(vec![live(tool("t", "read", json!({ "path": "src/lib.rs", "start_line": 40 }), "x"))], &log);
    p.path_base_dir = Some("/work".to_owned());
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let closed = card_rect("t").h;
    let file = rect_of(("tl-file", hash_key((0u64, "t")))).expect("the name is a link");
    rig.click(&mut view, file.x + file.w * 0.7, file.y + file.h * 0.5);
    rig.settle(&mut view);
    let opened = opened.borrow();
    assert_eq!(opened.len(), 1);
    assert_eq!((opened[0].path.as_str(), opened[0].base_dir.as_deref(), opened[0].new_page), ("src/lib.rs", Some("/work"), true));
    assert_eq!(card_rect("t").h, closed, "its click stops there rather than also opening the row's body");
    mewrk_pane_files::path_open::clear_path_open_handler();
}

mod workflow_rows {
    use super::*;
    use mewrk_pane_tasks::workflow_runs::WorkflowRunView;

    fn view_of(name: &str) -> Rc<WorkflowRunView> {
        Rc::new(serde_json::from_value::<WorkflowRunViewShim>(json!({ "id": "run-1", "name": name })).unwrap().into())
    }

    #[derive(serde::Deserialize)]
    struct WorkflowRunViewShim {
        id: String,
        name: String,
    }

    impl From<WorkflowRunViewShim> for WorkflowRunView {
        fn from(shim: WorkflowRunViewShim) -> Self {
            WorkflowRunView { id: shim.id, run_id: None, name: shim.name, script_name: None, description: String::new(), state: mewrk_pane_tasks::task_container::TaskItemState::Running, step_count: 3, elapsed_ms: None, tokens: None, phases: Vec::new(), steps: Vec::new(), logs: Vec::new(), running: true }
        }
    }

    #[test]
    fn sends_the_click_to_the_task_panel_and_keeps_the_run_in_the_block_beside_its_neighbours() {
        let mut rig = Rig::new();
        let log = Log::default();
        let opened: Rc<RefCell<Vec<String>>> = Rc::default();
        let sink = opened.clone();
        let mut p = props(vec![live(tool("before", "read", json!({}), "x")), live(tool("w", "workflow", json!({ "name": "plan" }), "ok")), live(tool("after", "read", json!({}), "x"))], &log);
        p.workflow_run_by_call = Rc::new(std::collections::HashMap::from([("w".to_owned(), view_of("My plan"))]));
        p.on_open_workflow_run = Some(Rc::new(move |_, id| sink.borrow_mut().push(id.to_owned())));
        let mut view = |app: &mut App| root(app, &p);
        rig.settle(&mut view);
        assert_eq!(timeline_snapshot(&mut rig.app, ID).nodes, 1, "one block");
        let closed = card_rect("w").h;
        let r = card_rect("w");
        rig.click(&mut view, r.x + 60.0, r.y + 10.0);
        rig.settle(&mut view);
        assert_eq!(*opened.borrow(), ["run-1"]);
        assert_eq!(card_rect("w").h, closed, "no drawer");
        // Insertion goes by the row's own context index.
        let r = card_rect("w");
        rig.right_click(&mut view, r.x + 60.0, r.y + r.h * 0.25);
        assert_eq!(timeline_snapshot(&mut rig.app, ID).menu_index, Some(1));
    }

    #[test]
    fn a_read_only_transcript_has_no_panel_so_the_body_states_the_run() {
        let mut rig = Rig::new();
        let log = Log::default();
        let mut p = props(vec![live(tool("w", "workflow", json!({}), "ok"))], &log);
        p.editable = false;
        p.workflow_run_by_call = Rc::new(std::collections::HashMap::from([("w".to_owned(), view_of("My plan"))]));
        p.on_open_workflow_run = Some(Rc::new(|_, _| panic!("no task container to bring forward")));
        let mut view = |app: &mut App| root(app, &p);
        rig.settle(&mut view);
        let closed = card_rect("w").h;
        let r = card_rect("w");
        rig.click(&mut view, r.x + 60.0, r.y + 10.0);
        rig.settle(&mut view);
        assert!(card_rect("w").h > closed, "opened");
    }

    #[test]
    fn degrades_a_settled_call_with_no_run_view_to_the_plain_tool_body() {
        let mut rig = Rig::new();
        let log = Log::default();
        let p = props(vec![live(tool("w", "workflow", json!({ "name": "plan" }), "receipt"))], &log);
        let mut view = |app: &mut App| root(app, &p);
        rig.settle(&mut view);
        let closed = card_rect("w").h;
        let r = card_rect("w");
        rig.click(&mut view, r.x + 60.0, r.y + 10.0);
        rig.settle(&mut view);
        assert!(card_rect("w").h > closed);
    }
}

#[test]
fn the_transcript_view_is_the_same_surface_with_nothing_to_mutate() {
    use mewrk_pane_tasks::context::Ctx;
    use mewrk_pane_tasks::subagent_panel::TranscriptArgs;
    let mut rig = Rig::new();
    let contexts: Vec<Ctx> = vec![Ctx::from(user("first", "the task")), Ctx::from(assistant("second", "the answer"))];
    let mut view = |app: &mut App| col().w_full().h_full().child(transcript_view(app, TranscriptArgs { contexts: &contexts, streaming: false, path_base_dir: None, aria_label: "Child".to_owned() }, Rc::new(Vec::new()), None));
    rig.settle(&mut view);
    assert!(rect_of(unit_key(0, "second")).is_some());
    assert!(rect_of(("tl-delete", 0u64, "second")).is_none());
    let r = card_rect("second");
    rig.right_click(&mut view, r.x + 10.0, r.y + 3.0);
    assert_eq!(timeline_snapshot(&mut rig.app, "transcript:first").menu, None);
}

#[test]
fn picks_every_part_of_an_answered_question_as_one_card() {
    let mut rig = Rig::new();
    let log = Log::default();
    let question = tool("q", "ask_user", json!({ "questions": [{ "question": "x", "header": "h", "options": [] }] }), "");
    let p = props(vec![live(question), live(user("answer", "yes")), live(assistant("a", "ok"))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    let card = card_rect("q");
    modified_drag(&mut rig, &mut view, (card.x + 10.0, card.y + 3.0), (card.x + 60.0, card.y + 8.0));
    let selection = timeline_snapshot(&mut rig.app, ID).selection;
    assert!(selection.contains(&"q".to_owned()) && selection.contains(&"answer".to_owned()), "{selection:?}");
}

#[test]
fn undo_and_a_copied_tool_card_the_host_will_not_vouch_for() {
    use mewrk_client::doc::NewConversation;
    use mewrk_client::model::AttachedWorkspace;
    use mewrk_ui_chat::handlers::*;
    let mut rig = Rig::new();
    rig.app.add_workspace("work", &[] as &[AttachedWorkspace], Some(false));
    let conversation = rig.app.create_conversation(NewConversation::default()).unwrap();
    let workspace = rig.app.workspace_of_conversation(&conversation).unwrap().id.clone();
    rig.app.select_conversation(&workspace, &conversation);
    for _ in 0..40 {
        rig.app.tick();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let seeded = vec![user("u", "one"), tool("forged", "read", json!({}), "made up")];
    rig.app.update_conversation(&workspace, &conversation, move |held| held.contexts = seeded);
    let tools = Rc::new(descriptors());
    let enabled = Rc::new(tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>());
    let id = conversation.clone();
    let mut view = move |app: &mut App| {
        let props = timeline_props(app, &id, tools.clone(), enabled.clone());
        col().w_full().h_full().child(streamed_conversation_view(app, props))
    };
    rig.settle(&mut view);
    copy_timeline_contexts(&mut rig.app, vec!["forged".to_owned()], &descriptors());
    paste_timeline_contexts(&mut rig.app, 2, Vec::new());
    for _ in 0..300 {
        rig.frame(&mut view);
        if rig.app.timeline_notice().is_some_and(|notice| notice.message.contains("Nothing was pasted")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let notice = rig.app.timeline_notice().map(|notice| notice.message).unwrap_or_default();
    assert!(notice.contains("Nothing was pasted"), "{notice}");
    // A forged card fails the host's save as well, which is not this test's business; what matters is that no copy landed.
    assert!(rig.app.timeline_contexts_of(&conversation).iter().all(|context| ["u", "forged"].contains(&context.id())), "not pasted at all");
}

/// The attest commands' wire shapes, against the real host: a call written out by hand comes back signed, a re-edit is re-signed, and a
/// copy of it is pasted under a proof of its own.
#[test]
fn places_a_call_by_hand_and_the_host_vouches_for_it_and_for_its_copy() {
    use mewrk_client::doc::NewConversation;
    use mewrk_client::model::AttachedWorkspace;
    use mewrk_ui_chat::handlers::*;
    let mut rig = Rig::new();
    rig.app.add_workspace("work", &[] as &[AttachedWorkspace], Some(false));
    let conversation = rig.app.create_conversation(NewConversation::default()).unwrap();
    let workspace = rig.app.workspace_of_conversation(&conversation).unwrap().id.clone();
    rig.app.select_conversation(&workspace, &conversation);
    for _ in 0..40 {
        rig.app.tick();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    rig.app.update_conversation(&workspace, &conversation, |held| held.contexts = vec![user("u", "one")]);
    let tools = Rc::new(descriptors());
    let enabled = Rc::new(tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>());
    let id = conversation.clone();
    let mut view = move |app: &mut App| {
        let props = timeline_props(app, &id, tools.clone(), enabled.clone());
        col().w_full().h_full().child(streamed_conversation_view(app, props))
    };
    rig.settle(&mut view);

    let wait = |rig: &mut Rig, view: &mut dyn FnMut(&mut App) -> E, until: &dyn Fn(&mut Rig) -> bool| {
        for _ in 0..500 {
            rig.frame(view);
            if until(rig) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the host never answered: {:?}", rig.app.timeline_notice());
    };

    handle_context_insert(&mut rig.app, 1, InsertKind::Tool, Some("read".to_owned()));
    rig.settle(&mut view);
    let result: Rc<RefCell<Option<Result<(), String>>>> = Rc::default();
    let sink = result.clone();
    save_tool_context_edit(&mut rig.app, input(json!({ "path": "notes.md" })), "the body".to_owned(), Vec::new(), Box::new(move |_, done| *sink.borrow_mut() = Some(done)));
    let held = result.clone();
    wait(&mut rig, &mut view, &|_| held.borrow().is_some());
    assert_eq!(result.borrow().clone(), Some(Ok(())));
    let conversation_id = conversation.clone();
    let placed = rig.app.timeline_contexts_of(&conversation_id)[1].clone();
    let ContextItem::Tool { id: placed_id, attestation, result: placed_result, .. } = &placed else { panic!("{placed:?}") };
    assert!(!attestation.is_empty(), "signed");
    assert_eq!(placed_result.output, "the body");
    assert!(rect_of(unit_key(0, placed_id)).is_some(), "on the timeline");

    // Rewritten by hand: signed again, over the new text.
    handle_context_edit(&mut rig.app, &placed);
    rig.settle(&mut view);
    let result: Rc<RefCell<Option<Result<(), String>>>> = Rc::default();
    let sink = result.clone();
    save_tool_context_edit(&mut rig.app, input(json!({ "path": "notes.md" })), "the new body".to_owned(), Vec::new(), Box::new(move |_, done| *sink.borrow_mut() = Some(done)));
    let held = result.clone();
    wait(&mut rig, &mut view, &|_| held.borrow().is_some());
    assert_eq!(result.borrow().clone(), Some(Ok(())));
    let edited = rig.app.timeline_contexts_of(&conversation_id)[1].clone();
    assert!(matches!(&edited, ContextItem::Tool { result, attestation: signed, .. } if result.output == "the new body" && !signed.is_empty() && signed != attestation));

    // Copied, with a proof of its own and without the source's provider id.
    copy_timeline_contexts(&mut rig.app, vec![placed_id.clone()], &descriptors());
    paste_timeline_contexts(&mut rig.app, 2, Vec::new());
    let held_id = conversation_id.clone();
    wait(&mut rig, &mut view, &|rig| rig.app.timeline_contexts_of(&held_id).len() == 3);
    let copy = rig.app.timeline_contexts_of(&conversation_id)[2].clone();
    assert!(matches!(&copy, ContextItem::Tool { id, attestation: proof, provider_call_id: None, .. } if id != placed_id && !proof.is_empty()), "{copy:?}");
}

// ---------------------------------------------------------------- what a row's facts are read from

#[test]
fn a_descriptor_that_loads_after_a_row_was_drawn_names_it_in_the_users_words() {
    let mut rig = Rig::new();
    let log = Log::default();
    let wire = "mcp__docs_0123456789__look_up__abcdef";
    let contexts = vec![live(tool("m", wire, json!({ "term": "macro" }), "A macro."))];
    let tools: Rc<RefCell<Vec<mewrk_client::model::ToolDescriptor>>> = Rc::default();
    let base = props(contexts, &log);
    let reading = tools.clone();
    let mut view = move |app: &mut App| {
        let mut p = base.clone();
        p.tools = Rc::new(reading.borrow().clone());
        root(app, &p)
    };
    rig.settle(&mut view);
    assert_eq!(mewrk_ui_chat::frame::row_title(&mut rig.app, ID, "m").as_deref(), Some("Called MCP tool look_up"));
    tools.borrow_mut().push(descriptor(wire, "MCP Docs / Look it up (asks first)", ToolCategory::Mcp));
    rig.settle(&mut view);
    assert_eq!(mewrk_ui_chat::frame::row_title(&mut rig.app, ID, "m").as_deref(), Some("Called MCP tool Look it up"));
}

#[test]
fn a_child_settling_on_a_spawns_record_changes_how_its_row_reads_with_the_output_untouched() {
    let mut rig = Rig::new();
    let log = Log::default();
    let with_status = |status: &str| {
        let mut card = tool("s", "agent_spawn", json!({ "name": "scout", "label": "Scout", "message": "Look" }), "");
        if let ContextItem::Tool { subagent, .. } = &mut card {
            *subagent = Some(serde_json::from_value(json!({ "name": "scout", "task": "Look", "status": status, "contexts": [], "updates": [] })).expect("a run record"));
        }
        live(card)
    };
    let running = props(vec![with_status("completed")], &log);
    let done = props(vec![with_status("failed")], &log);
    let flipped = Rc::new(std::cell::Cell::new(false));
    let reading = flipped.clone();
    let mut view = move |app: &mut App| root(app, if reading.get() { &done } else { &running });
    rig.settle(&mut view);
    assert_eq!(mewrk_ui_chat::frame::row_status_label(&mut rig.app, ID, "s").as_deref(), Some("Completed"));
    flipped.set(true);
    rig.settle(&mut view);
    assert_eq!(mewrk_ui_chat::frame::row_status_label(&mut rig.app, ID, "s").as_deref(), Some("Failed"));
}

#[test]
fn a_file_a_row_names_keeps_its_width_beside_a_message_long_enough_to_wrap() {
    let mut rig = Rig::new();
    rig.size = (700.0, 900.0);
    let log = Log::default();
    // The message's bubble reaches its cap, which has the frame laid out a second time.
    let long = "Please look at src/main.rs and tell me what is wrong with it. Also check the table layout, while you are there.";
    let p = props(vec![live(user("u", long)), live(tool("r", "read", json!({ "path": "src/main.rs" }), "x"))], &log);
    let mut view = |app: &mut App| root(app, &p);
    rig.settle(&mut view);
    rig.settle(&mut view);
    let name = rect_of(("tl-file-name", hash_key((0u64, "r")))).expect("the row names its file");
    assert!(name.w > 20.0, "as wide as the name, not cut to its ellipsis: {name:?}");
}

// ---------------------------------------------------------------- what a reader gets

mod a11y {
    use mewrk_client::conversation_branches::ContextBranchNavigation;

    use super::*;
    use crate::rig::{nameless, said, says, Tree};

    fn every_kind_view(log: &Log) -> impl FnMut(&mut App) -> E + use<> {
        let mut p = props(every_kind(), log);
        p.aria_label = Some("A transcript".to_owned());
        p.on_save_tool = Some(Rc::new(|_, _, _, _| {}));
        move |app: &mut App| root(app, &p)
    }

    fn rows_of(tree: &Tree) -> Vec<&accesskit::Node> {
        tree.iter().map(|(_, n)| n).filter(|n| n.role() == Role::ListItem).collect()
    }

    #[test]
    fn every_kind_of_card_reads_with_a_name_on_every_control_open_or_closed() {
        let log = Log::default();
        let mut view = every_kind_view(&log);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let mut rig = Rig::new();
        rig.size = (1200.0, 9000.0);
        let tree = rig.a11y_tree(view);
        assert_eq!(said(&tree, "A transcript").role(), Role::Region);
        assert_eq!(nameless(&tree), Vec::<String>::new());
        assert!(!rows_of(&tree).is_empty());
        assert!(tree.iter().any(|(_, n)| n.role() == Role::List && n.label().is_some_and(|l| !l.is_empty())), "a block goes by what it holds");
        assert!(tree.iter().any(|(_, n)| n.role() == Role::Article));
        assert_eq!(said(&tree, "Cited sources").role(), Role::Navigation);
        assert_eq!(said(&tree, "The guide").role(), Role::Link);

        // Everything that opens, opened: what is in the bodies has to have its names too.
        let closed: Vec<_> = tree.iter().filter(|(_, n)| n.is_expanded() == Some(false) && n.supports_action(accesskit::Action::Click) && n.has_popup().is_none()).map(|(id, _)| *id).collect();
        assert!(closed.len() > 3, "{closed:?}");
        for id in closed {
            a11y_action(accesskit::ActionRequest { action: accesskit::Action::Click, target_tree: accesskit::TreeId::ROOT, target_node: id, data: None });
        }
        rig.settle(view);
        let tree = rig.a11y_tree(view);
        assert_eq!(nameless(&tree), Vec::<String>::new());
        assert!(tree.iter().any(|(_, n)| n.role() == Role::Region && !n.labelled_by().is_empty()), "a row's body goes by its row");
        assert_eq!(said(&tree, "Search results").role(), Role::List);
        assert_eq!(said(&tree, "File results").role(), Role::List);
        assert_eq!(said(&tree, "src/main.rs contents").role(), Role::Region);
        assert_eq!(said(&tree, "src/main.rs file diff").role(), Role::Region);
        assert_eq!(said(&tree, "Sites searched").role(), Role::Navigation);
        assert!(says(&tree, "›").is_none(), "the prompt is `aria-hidden`");
        assert!(tree.iter().any(|(_, n)| n.label() == Some("Raw data") && n.is_expanded() == Some(false)));
        set_a11y_active(false);
    }

    #[test]
    fn a_row_says_what_happened_and_what_it_is_with_its_state_and_not_its_figures() {
        let log = Log::default();
        let p = props(vec![live(tool("t", "bash", json!({ "command": "cargo test" }), "ok"))], &log);
        let mut view = move |app: &mut App| root(app, &p);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let mut rig = Rig::new();
        let tree = rig.a11y_tree(view);
        let summary = tree.iter().map(|(_, n)| n).find(|n| n.role() == Role::Button && n.is_expanded().is_some()).expect("the row's button");
        let name = summary.label().expect("named");
        assert!(name.contains(" · ") && name.contains("Completed"), "{name}");
        assert!(!name.contains("token"), "{name}");
        assert_eq!(summary.is_expanded(), Some(false));
        assert_eq!(rows_of(&tree).len(), 1);
        set_a11y_active(false);
    }

    #[test]
    fn a_running_row_is_busy_and_a_row_with_nowhere_to_go_says_so() {
        let log = Log::default();
        let p = props(vec![streaming(reasoning("r", Some("Reading the file")), None), live(hook("h", "completed", "", false))], &log);
        let mut view = move |app: &mut App| root(app, &p);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let mut rig = Rig::new();
        let tree = rig.a11y_tree(view);
        let rows = rows_of(&tree);
        assert_eq!(rows.iter().map(|row| row.is_busy()).collect::<Vec<_>>(), [true, false]);
        assert_eq!(said(&tree, "Thinking").role(), Role::Status);
        let spent = tree.iter().map(|(_, n)| n).find(|n| n.role() == Role::Button && n.label().is_some_and(|l| l.contains("hook"))).expect("the hook's button");
        assert!(spent.is_disabled(), "a hook that printed nothing has no body and no panel");
        assert_eq!(spent.is_expanded(), Some(false));
        set_a11y_active(false);
    }

    #[test]
    fn a_failed_turn_is_an_alert_and_a_messages_branches_are_a_named_group() {
        let log = Log::default();
        let mut p = props(vec![live(user("u", "hi"))], &log);
        p.branch_navigations = Rc::new(std::collections::HashMap::from([("u".to_owned(), ContextBranchNavigation { active_index: 1, branch_ids: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()] })]));
        p.on_select_branch = Some(Rc::new(|_, _, _| {}));
        p.turns = Rc::new(vec![ConversationTurn { error: Some(ConversationTurnError { message: "It broke".to_owned(), provider_name: "Acme".to_owned(), model_name: "one".to_owned(), at: AT.to_owned() }), ..turn_with_error("u") }]);
        p.on_dismiss_turn_error = Some(Rc::new(|_| {}));
        let mut view = move |app: &mut App| root(app, &p);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let mut rig = Rig::new();
        let tree = rig.a11y_tree(view);
        assert_eq!(said(&tree, "Branches from this message").role(), Role::Group);
        assert_eq!(said(&tree, "Previous branch").role(), Role::Button);
        assert_eq!(said(&tree, "Next branch").role(), Role::Button);
        assert_eq!(said(&tree, "2 / 3").live(), Some(accesskit::Live::Polite));
        let alert = tree.iter().map(|(_, n)| n).find(|n| n.role() == Role::Alert).expect("the notice");
        assert!(alert.label().is_some_and(|l| l.contains("Acme · one") && l.contains("It broke")), "{:?}", alert.label());
        assert_eq!(said(&tree, "Dismiss model error").role(), Role::Button);
        set_a11y_active(false);
    }

    #[test]
    fn a_messages_actions_go_by_what_they_do_with_the_longer_words_as_their_description() {
        let log = Log::default();
        let mut p = props(vec![live(user("u", "hi"))], &log);
        p.on_branch_from = Some(Rc::new(|_, _| {}));
        p.on_rewind_to = Some(Rc::new(|_, _| {}));
        p.rewind_disabled_reason = Some("A run is going".to_owned());
        let mut view = move |app: &mut App| root(app, &p);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let mut rig = Rig::new();
        let tree = rig.a11y_tree(view);
        let branch = said(&tree, "Branch from this message");
        assert_eq!((branch.role(), branch.description()), (Role::Button, Some("Continue this message in a new conversation")));
        let rewind = said(&tree, "Rewind to before this message");
        assert_eq!((rewind.role(), rewind.description(), rewind.is_disabled()), (Role::Button, Some("A run is going"), true));
        assert_eq!(said(&tree, "Copy user message").role(), Role::Button);
        assert_eq!(nameless(&tree), Vec::<String>::new());
        set_a11y_active(false);
    }

    #[test]
    fn the_rewind_dialog_says_how_the_check_stands_and_goes_by_its_title() {
        use mewrk_ui_chat::rewind::{rewind_dialog, RewindDialogProps, RewindDialogState, RewindPreview};
        let mut rig = Rig::new();
        let mut view = |preview: RewindPreview, blocked: Option<&str>| {
            let blocked = blocked.map(str::to_owned);
            let state = RewindDialogState { conversation_id: "c".to_owned(), context_id: "u".to_owned(), preview, busy: false, action_error: Some("It would not go".to_owned()) };
            let mut view = move |_: &mut App| rewind_dialog(RewindDialogProps { state: state.clone(), blocked_reason: blocked.clone(), conversation_blocked_reason: None, on_run: Rc::new(|_, _| {}), on_close: Rc::new(|_| {}) });
            rig.a11y_tree(&mut view)
        };
        let tree = view(RewindPreview::Loading, None);
        let title = "Rewind to before this message";
        assert!(tree.iter().any(|(_, n)| n.role() == Role::Dialog && n.label() == Some(title)));
        assert_eq!(said(&tree, "Checking file changes…").role(), Role::Status);
        assert_eq!(said(&tree, "It would not go").role(), Role::Alert);
        let tree = view(RewindPreview::Error("no".to_owned()), None);
        assert_eq!(said(&tree, "Could not read the file changes: no").role(), Role::Alert);
        let tree = view(RewindPreview::Loading, Some("A run is going"));
        assert_eq!(said(&tree, "A run is going").role(), Role::Status);
        set_a11y_active(false);
    }

    #[test]
    fn the_timelines_menus_go_by_upstreams_names() {
        let log = Log::default();
        let p = props(messages(&["a", "b"]), &log);
        let mut view = move |app: &mut App| root(app, &p);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let mut rig = Rig::new();
        rig.settle(view);
        let r = card_rect("a");
        rig.right_click(view, r.x + 20.0, r.y + 4.0);
        let tree = rig.a11y_tree(view);
        assert_eq!(said(&tree, "Add context").role(), Role::Menu);
        assert_eq!(nameless(&tree), Vec::<String>::new());
        set_a11y_active(false);
    }
}
