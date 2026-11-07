//! The timeline's model updated piece by piece, held to what a full rebuild of the same inputs builds.
//!
//! Two probes see the same updates: one builds everything every time, the other follows commits incrementally. After every step
//! the two have to agree on everything the timeline reads: nodes, keys, indexes, answers, the turn projection, change lists, what
//! is drawn, the selection and the layout.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mewrk_client::model::{ContextItem, ReasoningForm, ToolResult};
use mewrk_client::runs::{ConversationTurn, ConversationTurnError, ConversationTurnStatus, LiveContext, StreamingToolPhase};
use mewrk_pane_tasks::task_container::TaskItemState;
use mewrk_pane_tasks::workflow_runs::WorkflowRunView;
use mewrk_ui_chat::props::ConversationViewProps;
use mewrk_ui_chat::stream::{ModelProbe, ModelUpdate};
use serde_json::json;

use crate::fixture::*;

const WIDTH: f32 = 772.0;
const MESSAGE_SIZE: f32 = 15.0;

/// splitmix64: the same steps from the same seed, with nothing to add to the workspace.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        (!items.is_empty()).then(|| &items[self.below(items.len())])
    }
}

fn diff(additions: usize, deletions: usize) -> String {
    let mut diff = format!("--- a/x\n+++ b/x\n@@ -1,{deletions} +1,{additions} @@\n");
    for at in 0..deletions {
        diff.push_str(&format!("-old {at}\n"));
    }
    for at in 0..additions {
        diff.push_str(&format!("+new {at}\n"));
    }
    diff
}

fn edit(id: &str, path: &str, additions: usize, deletions: usize) -> ContextItem {
    let mut item = tool(id, "edit", json!({ "path": path }), "ok");
    if let ContextItem::Tool { result, .. } = &mut item {
        *result = ToolResult { diff: Some(diff(additions, deletions)), ..result.clone() };
    }
    item
}

fn question(id: &str, output: &str) -> ContextItem {
    tool(id, "ask_user", json!({ "questions": [{ "question": "Which one?", "header": "Pick", "options": [{ "label": "A", "description": "a" }, { "label": "B", "description": "b" }] }] }), output)
}

fn turn(id: &str, anchor: &str, status: ConversationTurnStatus, failed: bool, owns: Vec<String>) -> ConversationTurn {
    ConversationTurn {
        id: id.to_owned(),
        request_id: format!("req-{id}"),
        anchor_context_id: anchor.to_owned(),
        model_id: String::new(),
        started_at: AT.to_owned(),
        ended_at: None,
        duration_ms: None,
        status,
        context_ids: owns,
        usage: Default::default(),
        usage_offset: Default::default(),
        usage_baseline: Default::default(),
        usage_revision_at_start: 0,
        segment_count: 1,
        error: failed.then(|| ConversationTurnError { message: "It broke".to_owned(), provider_name: "P".to_owned(), model_name: "M".to_owned(), at: AT.to_owned() }),
    }
}

fn workflow_view(id: &str) -> Rc<WorkflowRunView> {
    Rc::new(WorkflowRunView { id: id.to_owned(), name: "Plan".to_owned(), description: String::new(), state: TaskItemState::Running, running: true, script_name: None, run_id: None, step_count: 2, elapsed_ms: None, tokens: None, steps: Vec::new(), phases: Vec::new(), logs: Vec::new() })
}

/// What a conversation is at one moment: its persisted contexts, the live ones over them, its turns.
struct World {
    persisted: Vec<ContextItem>,
    persisted_revision: u64,
    live: Rc<Vec<LiveContext>>,
    in_place: usize,
    live_revision: u64,
    props: ConversationViewProps,
    next: u64,
    /// Whether this conversation may take what the incremental update cannot follow, after which every update is a full one.
    allow_ambiguous: bool,
}

impl World {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    fn ids(&self) -> Vec<String> {
        self.persisted.iter().map(|item| item.id().to_owned()).chain(self.live.iter().map(|context| context.id().to_owned())).collect()
    }

    fn user_ids(&self) -> Vec<String> {
        self.persisted.iter().map(|item| item).chain(self.live.iter().map(|context| &context.item)).filter(|item| matches!(item, ContextItem::User { .. })).map(|item| item.id().to_owned()).collect()
    }

    /// A new list of live contexts, as a commit makes one: the old one stays as it was.
    fn edit_live(&mut self, f: impl FnOnce(&mut Vec<LiveContext>, usize)) {
        let mut live = (*self.live).clone();
        f(&mut live, self.in_place);
        self.live = Rc::new(live);
        self.live_revision += 1;
    }

    fn edit_turns(&mut self, f: impl FnOnce(&mut Vec<ConversationTurn>)) {
        let mut turns = (*self.props.turns).clone();
        f(&mut turns);
        self.props.turns = Rc::new(turns);
    }
}

/// A persisted conversation of `count` contexts, with every kind the timeline groups differently.
fn persisted_conversation(rng: &mut Rng, count: usize, next: &mut u64) -> Vec<ContextItem> {
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        *next += 1;
        let id = format!("p{next}");
        items.push(match rng.below(20) {
            0..=2 => user(&id, "A question about something, long enough to take a line or two of the bubble it sits in."),
            3 => user(&format!("ctx_agent-result_{id}"), "What a child reported back."),
            4 | 5 => assistant(&id, &"A reply with a little **Markdown** in it.\n\n- one\n- two\n".repeat(1 + rng.below(4))),
            6 => assistant(&id, ""),
            7 => reasoning(&id, Some("Thinking about what to do next.")),
            8 => reasoning(&id, None),
            9 | 10 => tool(&id, "read", json!({ "path": format!("src/{id}.rs") }), "fn a() {}\n"),
            11 => tool(&id, "bash", json!({ "command": "cargo test" }), "ok"),
            12 | 13 => edit(&id, &format!("src/file{}.rs", rng.below(5)), 1 + rng.below(9), rng.below(4)),
            14 => question(&id, "Asked the user; the run paused."),
            15 => question(&id, "The user answered: A"),
            16 => hook(&id, "completed", "formatted 1 file", true),
            17 => tool(&id, "workflow", json!({ "name": "plan" }), "done"),
            18 => failed_tool(&id, "edit", json!({ "path": "src/x.rs" }), "no such file"),
            _ => {
                let mut item = system(&id, "");
                if let ContextItem::System { tools_added, .. } = &mut item {
                    tools_added.push("read".to_owned());
                }
                item
            }
        });
    }
    items
}

/// One live context the way a run streams it in.
fn streamed_context(rng: &mut Rng, world: &mut World) -> LiveContext {
    let id = world.id("l");
    match rng.below(16) {
        0 | 1 => live(user(&id, "Steering the run with one more thing.")),
        2 => live(user(&format!("ctx_agent-result_{id}"), "A child finished.")),
        3 | 4 => streaming(assistant(&id, ""), None),
        5 => streaming(assistant(&id, "Starting to write"), None),
        6 => streaming(reasoning(&id, Some("")), None),
        7 => {
            let mut item = reasoning(&id, Some(""));
            if let ContextItem::Reasoning { form, .. } = &mut item {
                *form = Some(ReasoningForm::Encrypted);
            }
            streaming(item, None)
        }
        8 | 9 => streaming(tool(&id, "bash", json!({ "command": "ls" }), ""), Some([StreamingToolPhase::Announced, StreamingToolPhase::Ready, StreamingToolPhase::Running][rng.below(3)])),
        10 => streaming(edit(&id, &format!("src/file{}.rs", rng.below(5)), 0, 0), Some(StreamingToolPhase::Running)),
        11 => streaming(tool(&id, "workflow", json!({ "name": "plan" }), ""), Some(StreamingToolPhase::Running)),
        12 => live(question(&id, if rng.chance(50) { "Asked the user; the run paused." } else { "The user answered: B" })),
        13 => streaming(question(&id, ""), Some([StreamingToolPhase::Announced, StreamingToolPhase::Ready, StreamingToolPhase::Running][rng.below(3)])),
        14 => live(hook(&id, "completed", "formatted 1 file", true)),
        _ => live(edit(&id, &format!("src/file{}.rs", rng.below(5)), 1 + rng.below(5), rng.below(3))),
    }
}

/// One step a streaming conversation can take, and a name for it.
fn step(rng: &mut Rng, world: &mut World) -> &'static str {
    let roll = rng.below(100);
    match roll {
        0..=13 => {
            let context = streamed_context(rng, world);
            world.edit_live(|live, _| live.push(context));
            "append"
        }
        14..=33 => {
            let grow = rng.below(3);
            let extra = ["more words ", "a new line\n", "## a heading\n\n- an item\n"][grow].repeat(1 + rng.below(30));
            world.edit_live(|live, in_place| {
                if let Some(last) = live.get_mut(in_place..).and_then(|projection| projection.last_mut()) {
                    match &mut last.item {
                        ContextItem::Assistant { content, .. } => content.push_str(&extra),
                        ContextItem::Reasoning { content, .. } => content.get_or_insert_with(String::new).push_str(&extra),
                        ContextItem::Tool { result, .. } => result.output.push_str(&extra),
                        _ => {}
                    }
                }
            });
            "grow"
        }
        34..=43 => {
            // A call settles: its receipt lands, with the diff it made.
            let pick = rng.next();
            world.edit_live(|live, in_place| {
                let open: Vec<usize> = (in_place..live.len()).filter(|at| live[*at].stream_status.is_some_and(|status| status != StreamingToolPhase::Completed) || (live[*at].streaming == Some(true) && matches!(live[*at].item, ContextItem::Tool { .. }))).collect();
                if open.is_empty() {
                    return;
                }
                let at = open[(pick % open.len() as u64) as usize];
                let context = &mut live[at];
                context.stream_status = Some(StreamingToolPhase::Completed);
                if pick % 3 == 0 {
                    context.streaming = None;
                }
                if let ContextItem::Tool { tool_name, result, .. } = &mut context.item {
                    result.output = if tool_name == "ask_user" { "The user answered: A".to_owned() } else { "done".to_owned() };
                    if tool_name == "edit" {
                        result.diff = Some(diff(1 + (pick % 7) as usize, (pick % 3) as usize));
                    }
                    result.success = pick % 11 != 0;
                }
            });
            "settle"
        }
        44..=48 => {
            // Reasoning or a reply stops streaming.
            let pick = rng.next();
            world.edit_live(|live, in_place| {
                let open: Vec<usize> = (in_place..live.len()).filter(|at| live[*at].streaming == Some(true) && !matches!(live[*at].item, ContextItem::Tool { .. })).collect();
                if let Some(&at) = open.get((pick % open.len().max(1) as u64) as usize) {
                    live[at].streaming = None;
                    if let ContextItem::Reasoning { duration_ms, .. } = &mut live[at].item {
                        *duration_ms = Some(1200);
                    }
                }
            });
            "stop"
        }
        49..=56 => {
            // What a commit does to the turns most of the time: the running one's usage moves.
            world.edit_turns(|turns| {
                if let Some(last) = turns.last_mut() {
                    last.usage_revision_at_start += 1;
                    last.segment_count += 1;
                }
            });
            "usage"
        }
        57..=62 => {
            let users = world.user_ids();
            let ids = world.ids();
            // Mostly at a message of the user's, as a round starts at one; now and then anywhere, or before everything.
            let anchor = if rng.chance(15) {
                String::new()
            } else if rng.chance(30) {
                rng.pick(&ids).cloned().unwrap_or_default()
            } else {
                rng.pick(&users).cloned().unwrap_or_default()
            };
            let owns: Vec<String> = (0..rng.below(4)).filter_map(|_| rng.pick(&ids).cloned()).collect();
            let status = [ConversationTurnStatus::Running, ConversationTurnStatus::Completed][rng.below(2)];
            let failed = rng.chance(35);
            let choice = rng.below(4);
            let id = world.id("turn");
            world.edit_turns(|turns| match choice {
                0 if !turns.is_empty() => {
                    let at = owns.len() % turns.len();
                    turns[at].status = status;
                    turns[at].error = failed.then(|| ConversationTurnError { message: "x".to_owned(), provider_name: "P".to_owned(), model_name: "M".to_owned(), at: AT.to_owned() });
                }
                1 if !turns.is_empty() => {
                    let at = owns.len() % turns.len();
                    turns[at].context_ids.extend(owns);
                }
                2 if turns.len() > 1 => {
                    turns.remove(0);
                }
                _ => turns.push(turn(&id, &anchor, status, failed, owns)),
            });
            "turns"
        }
        63..=65 => {
            // A task overlay's card in place: its child streams, or it changes phase.
            let pick = rng.next();
            world.edit_live(|live, in_place| {
                if in_place == 0 {
                    return;
                }
                let card = &mut live[(pick % in_place as u64) as usize];
                match pick % 3 {
                    0 => card.started_at = Some(format!("{pick}")),
                    1 => card.stream_status = Some([StreamingToolPhase::Running, StreamingToolPhase::Completed][(pick / 3 % 2) as usize]),
                    _ => card.streaming = if card.streaming == Some(true) { None } else { Some(true) },
                }
            });
            "overlay"
        }
        66..=67 => {
            let context = streamed_context(rng, world);
            let at = rng.next();
            world.edit_live(|live, in_place| {
                let at = in_place + (at as usize % (live.len() - in_place + 1));
                live.insert(at, context);
            });
            "insert"
        }
        68..=69 => {
            world.edit_live(|live, in_place| {
                if live.len() > in_place {
                    live.pop();
                }
            });
            "pop"
        }
        70 => {
            // The run's contexts reach the conversation. The last may stay in the projection, over its persisted copy.
            let keep_last = rng.chance(50);
            let projection: Vec<LiveContext> = world.live[world.in_place..].to_vec();
            // A context the conversation already holds is written over where it is, as the host does.
            for context in &projection {
                match world.persisted.iter_mut().find(|item| item.id() == context.id()) {
                    Some(item) => *item = context.item.clone(),
                    None => world.persisted.push(context.item.clone()),
                }
            }
            world.persisted_revision += 1;
            let in_place = world.in_place;
            world.edit_live(|live, _| {
                live.truncate(in_place);
                if keep_last && let Some(last) = projection.last() {
                    live.push(last.clone());
                }
            });
            "persist"
        }
        71 => {
            world.props.streaming = !world.props.streaming;
            "streaming"
        }
        72..=74 => {
            world.live_revision += 1;
            "revision"
        }
        75 | 76 => "width",
        77 => {
            // A task's card starts or stops being drawn over its persisted copy.
            let overlaid: Vec<String> = world.live[..world.in_place].iter().map(|context| context.id().to_owned()).collect();
            let candidate = world.persisted.iter().filter(|item| matches!(item, ContextItem::Tool { .. }) && !overlaid.iter().any(|id| id == item.id())).nth(rng.below(4)).cloned();
            let in_place = world.in_place;
            match candidate {
                Some(item) if rng.chance(60) || in_place == 0 => {
                    world.edit_live(|live, in_place| live.insert(in_place, streaming(item, Some(StreamingToolPhase::Running))));
                    world.in_place += 1;
                }
                _ if in_place > 0 => {
                    world.edit_live(|live, in_place| {
                        live.remove(in_place - 1);
                    });
                    world.in_place -= 1;
                }
                _ => {}
            }
            "in place"
        }
        78 if world.allow_ambiguous => {
            // An id twice over, which the incremental update cannot follow, or one that looks like a node key.
            let twice = rng.pick(&world.live[world.in_place..]).map(|context| context.id().to_owned());
            let id = match twice {
                Some(id) if rng.chance(50) => id,
                _ => world.id("block:"),
            };
            world.edit_live(|live, _| live.push(live_ctx(assistant(&id, "Said twice"))));
            "ambiguous"
        }
        _ => "measure",
    }
}

/// Steps through one seeded conversation and holds the incremental model to the full one after every step.
fn run_seed(seed: u64, steps: usize, persisted_count: usize) -> HashMap<&'static str, usize> {
    let mut rng = Rng(seed);
    let mut next = 0;
    let persisted = persisted_conversation(&mut rng, persisted_count, &mut next);
    // Some persisted tool cards are overlaid in place, as a task overlay draws the cards of tasks still streaming.
    let overlaid: Vec<LiveContext> = persisted.iter().filter(|item| matches!(item, ContextItem::Tool { .. })).take(rng.below(3)).map(|item| streaming(item.clone(), Some(StreamingToolPhase::Running))).collect();
    let in_place = overlaid.len();
    let mut props = ConversationViewProps { streaming: true, timeline_id: Some(format!("seed-{seed}")), ..ConversationViewProps::default() };
    let workflows: HashMap<String, Rc<WorkflowRunView>> = persisted.iter().filter(|item| matches!(item, ContextItem::Tool { tool_name, .. } if tool_name == "workflow")).take(1).map(|item| (item.id().to_owned(), workflow_view(item.id()))).collect();
    props.workflow_run_by_call = Rc::new(workflows);
    let first_user = persisted.iter().find(|item| matches!(item, ContextItem::User { .. })).map(|item| item.id().to_owned()).unwrap_or_default();
    props.turns = Rc::new(vec![turn("t0", &first_user, ConversationTurnStatus::Completed, true, vec![first_user.clone()]), turn("t1", "", ConversationTurnStatus::Running, false, Vec::new())]);
    let mut world = World { persisted, persisted_revision: 1, live: Rc::new(overlaid), in_place, live_revision: 1, props, next, allow_ambiguous: seed % 8 == 0 };
    let mut full = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut incremental = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut outcomes: HashMap<&'static str, usize> = HashMap::new();
    let mut check = |full: &mut ModelProbe, incremental: &mut ModelProbe, world: &World, step: usize, what: &str| {
        full.update(&world.persisted, world.persisted_revision, &world.live, world.in_place, world.live_revision, &world.props, true);
        let outcome = incremental.update(&world.persisted, world.persisted_revision, &world.live, world.in_place, world.live_revision, &world.props, false);
        *outcomes.entry(match outcome {
            ModelUpdate::Unchanged => "unchanged",
            ModelUpdate::Kept => "kept",
            ModelUpdate::Tail => "tail",
            ModelUpdate::Full => "full",
        })
        .or_default() += 1;
        let differences = full.differences(incremental);
        assert!(differences.is_empty(), "seed {seed}, step {step} ({what}, {outcome:?}): the incremental model differs from the full one:\n{}", differences.join("\n"));
    };
    check(&mut full, &mut incremental, &world, 0, "start");
    for at in 1..=steps {
        let what = step(&mut rng, &mut world);
        match what {
            "width" => {
                // A resize lays nothing out by itself: the next update finds the layout made at another width.
                let width = [772.0, 612.0, 1132.0][rng.below(3)];
                full.set_width(width);
                incremental.set_width(width);
            }
            "measure" => {
                // What the next frame finds a row measures, as the rows it built are measured.
                let nodes = incremental.nodes();
                if nodes > 0 {
                    let position = if rng.chance(70) { nodes - 1 - rng.below(nodes.min(4)) } else { rng.below(nodes) };
                    let height = 20.0 + rng.below(400) as f32;
                    full.measure(&world.persisted, &world.live, position, height, true);
                    incremental.measure(&world.persisted, &world.live, position, height, false);
                }
                if rng.chance(20) {
                    let ids = world.ids();
                    let mut picked: Vec<String> = (0..1 + rng.below(3)).filter_map(|_| rng.pick(&ids).cloned()).collect();
                    // A box adds to what was picked out before it, which may have left the timeline since.
                    if rng.chance(30) {
                        picked.push("gone".to_owned());
                    }
                    full.select(picked.clone());
                    incremental.select(picked);
                }
                let differences = full.differences(&incremental);
                assert!(differences.is_empty(), "seed {seed}, step {at} (measure): the layouts differ:\n{}", differences.join("\n"));
            }
            _ => check(&mut full, &mut incremental, &world, at, what),
        }
    }
    outcomes
}

#[test]
fn an_incremental_model_update_builds_exactly_what_a_full_rebuild_does() {
    let mut totals: HashMap<&'static str, usize> = HashMap::new();
    for seed in 0..48u64 {
        let persisted = [0, 3, 12, 40, 120][seed as usize % 5];
        for (what, count) in run_seed(0x5EED_0000 + seed, 220, persisted) {
            *totals.entry(what).or_default() += count;
        }
    }
    println!("model updates over every seed: {totals:?}");
    // The incremental path has to be what most updates take, or this proves nothing about it.
    let incremental = totals.get("tail").copied().unwrap_or(0) + totals.get("kept").copied().unwrap_or(0);
    let full = totals.get("full").copied().unwrap_or(0);
    assert!(incremental > full * 4, "too few updates went the incremental way: {totals:?}");
    assert!(totals.get("kept").copied().unwrap_or(0) > 100, "a turns update that changes nothing should keep the model: {totals:?}");
}

/// The same, over many more conversations: `cargo test --release -p mewrk-ui-chat --test suite -- --ignored timeline_model_soak`.
#[test]
#[ignore]
fn timeline_model_soak() {
    for seed in 0..1500u64 {
        let persisted = [0, 5, 30, 90, 300][seed as usize % 5];
        run_seed(0xC0FF_EE00_0000 + seed, 400, persisted);
    }
}

/// A legacy question's answer is the next message the user writes, which can arrive with a commit long after the question.
#[test]
fn a_legacy_question_still_waiting_takes_its_answer_from_a_commit() {
    let persisted = vec![user("u1", "Help"), question("q1", "Asked the user; the run paused."), reasoning("r1", Some("Waiting.")), tool("t1", "read", json!({ "path": "a" }), "x"), assistant("a1", "Waiting for you.")];
    let props = ConversationViewProps { streaming: true, ..ConversationViewProps::default() };
    let mut full = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut incremental = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut live: Rc<Vec<LiveContext>> = Rc::default();
    let mut revision = 0;
    let mut commit = |live: &Rc<Vec<LiveContext>>, revision: u64| {
        full.update(&persisted, 1, live, 0, revision, &props, true);
        let outcome = incremental.update(&persisted, 1, live, 0, revision, &props, false);
        let differences = full.differences(&incremental);
        assert!(differences.is_empty(), "{}", differences.join("\n"));
        outcome
    };
    commit(&live, revision);
    for context in [streaming(tool("t2", "bash", json!({ "command": "ls" }), ""), Some(StreamingToolPhase::Running)), live_ctx(user("u2", "B, please.")), streaming(assistant("a2", "On it"), None)] {
        let mut next = (*live).clone();
        next.push(context);
        live = Rc::new(next);
        revision += 1;
        assert_eq!(commit(&live, revision), ModelUpdate::Tail);
    }
}

/// A question answered before the cut keeps its answer, which can lie past where the node pass picks up again.
#[test]
fn a_kept_question_keeps_an_answer_that_lies_past_where_the_pass_picks_up() {
    let persisted = vec![user("u1", "Help"), question("q1", "Asked the user; the run paused."), reasoning("r1", Some("Waiting.")), user("u2", "A")];
    let props = ConversationViewProps { streaming: true, ..ConversationViewProps::default() };
    let mut full = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut incremental = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut live: Vec<LiveContext> = Vec::new();
    for (revision, context) in [streaming(reasoning("r2", Some("Now")), None), streaming(assistant("a1", ""), None), streaming(assistant("a1", "Done"), None), live_ctx(user("u3", "Thanks"))].into_iter().enumerate() {
        match live.iter_mut().find(|known| known.id() == context.id()) {
            Some(known) => *known = context,
            None => live.push(context),
        }
        let live = Rc::new(live.clone());
        full.update(&persisted, 1, &live, 0, revision as u64, &props, true);
        incremental.update(&persisted, 1, &live, 0, revision as u64, &props, false);
        let differences = full.differences(&incremental);
        assert!(differences.is_empty(), "{}", differences.join("
"));
    }
}

/// Heights are kept by hashed node key, and two keys can hash alike: measuring one row then moves the other as well, wherever it is.
#[test]
fn a_measured_row_moves_every_row_whose_key_hashes_alike() {
    assert_eq!(mewrk_ui_chat::frame::hash_id("block:p12"), mewrk_ui_chat::frame::hash_id("block:p99"), "these two block keys are chosen to collide");
    let persisted = vec![user("u1", "One"), tool("p12", "read", json!({ "path": "a" }), "x"), user("u2", "Two"), tool("p99", "read", json!({ "path": "b" }), "x"), user("u3", "Three")];
    let props = ConversationViewProps::default();
    let live: Rc<Vec<LiveContext>> = Rc::default();
    let mut full = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut incremental = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    full.update(&persisted, 1, &live, 0, 1, &props, true);
    incremental.update(&persisted, 1, &live, 0, 1, &props, false);
    full.measure(&persisted, &live, 3, 240.0, true);
    incremental.measure(&persisted, &live, 3, 240.0, false);
    let differences = full.differences(&incremental);
    assert!(differences.is_empty(), "{}", differences.join("
"));
}

fn live_ctx(item: ContextItem) -> LiveContext {
    live(item)
}

/// A turn anchored at a context splits the block running through it, so a turn that lands on a context already drawn reshapes
/// the blocks around it.
#[test]
fn a_turn_anchored_inside_a_block_splits_it_on_the_next_commit() {
    let persisted = vec![user("u1", "Go"), tool("t1", "read", json!({ "path": "a" }), "x"), tool("t2", "read", json!({ "path": "b" }), "x"), tool("t3", "read", json!({ "path": "c" }), "x")];
    let mut props = ConversationViewProps { streaming: true, ..ConversationViewProps::default() };
    let mut full = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let mut incremental = ModelProbe::new(WIDTH, MESSAGE_SIZE);
    let live: Rc<Vec<LiveContext>> = Rc::new(vec![streaming(tool("t4", "read", json!({ "path": "d" }), ""), Some(StreamingToolPhase::Completed))]);
    full.update(&persisted, 1, &live, 0, 1, &props, true);
    incremental.update(&persisted, 1, &live, 0, 1, &props, false);
    let before = incremental.nodes();
    props.turns = Rc::new(vec![turn("t", "t2", ConversationTurnStatus::Running, false, vec!["t3".to_owned()])]);
    full.update(&persisted, 1, &live, 0, 1, &props, true);
    assert_eq!(incremental.update(&persisted, 1, &live, 0, 1, &props, false), ModelUpdate::Tail);
    assert!(full.differences(&incremental).is_empty(), "{}", full.differences(&incremental).join("\n"));
    assert_eq!(incremental.nodes(), before + 1, "the block splits in two at the anchor");
    // The same turns again, in a new list: nothing the model reads changed.
    props.turns = Rc::new((*props.turns).clone());
    assert_eq!(incremental.update(&persisted, 1, &live, 0, 1, &props, false), ModelUpdate::Kept);
}

/// A long conversation with a reply streaming at its end: what a commit costs built in full and built from the change. Run with
/// `cargo test --release -p mewrk-ui-chat --test suite -- --ignored --nocapture timeline_model_bench`.
#[test]
#[ignore]
fn timeline_model_bench() {
    // What a long working conversation holds: rounds of reading and editing between the user's messages, replies of a few
    // kilobytes, diffs of a hundred lines.
    let mut persisted = Vec::new();
    let reply = "A reply with a little **Markdown** in it, a list, and some code.

- one
- two

```rust
fn main() {}
```

".repeat(24);
    for at in 0..3000 {
        let id = format!("c{at}");
        persisted.push(match at % 12 {
            0 => user(&id, "Look at the next file and fix what is wrong with it, then run the tests."),
            1 | 6 => reasoning(&id, Some("Thinking about what to do next, and what it would take.")),
            2 | 3 | 7 => tool(&id, "read", json!({ "path": format!("src/file{at}.rs") }), &"fn a() {}
".repeat(80)),
            4 | 8 => edit(&id, &format!("src/file{}.rs", at % 40), 60, 40),
            5 | 9 => tool(&id, "bash", json!({ "command": "cargo test" }), "test result: ok. 12 passed"),
            _ => assistant(&id, &reply),
        });
    }
    persisted.push(user("prompt", "Do the long thing."));
    let props = ConversationViewProps { streaming: true, turns: Rc::new(vec![turn("t", "prompt", ConversationTurnStatus::Running, false, vec!["prompt".to_owned()])]), ..ConversationViewProps::default() };
    let commits = 120;
    // The same commits for every way of building: a reply growing, a call landing now and then.
    let mut lives: Vec<Rc<Vec<LiveContext>>> = Vec::with_capacity(commits);
    let mut live: Vec<LiveContext> = vec![streaming(assistant("reply-0", "Starting"), None)];
    for at in 0..commits {
        if at % 10 == 9 {
            live.last_mut().unwrap().streaming = None;
            live.push(live_ctx(edit(&format!("edit-{at}"), "src/x.rs", 30, 10)));
            live.push(streaming(assistant(&format!("reply-{at}"), ""), None));
        }
        if let ContextItem::Assistant { content, .. } = &mut live.last_mut().unwrap().item {
            content.push_str("Another sentence of the reply, streamed in with this commit. ");
        }
        lives.push(Rc::new(live.clone()));
    }
    // `before` is what the timeline did until now: everything built on every commit, and every row not yet measured estimated
    // again from its text whenever the layout changes.
    let time = |full: bool, before: bool| -> (Duration, Duration) {
        let mut probe = ModelProbe::new(WIDTH, MESSAGE_SIZE);
        probe.update(&persisted, 1, &lives[0], 0, 0, &props, true);
        let mut update = Duration::ZERO;
        let mut measure = Duration::ZERO;
        for (at, live) in lives.iter().enumerate() {
            let started = Instant::now();
            probe.update(&persisted, 1, live, 0, at as u64 + 1, &props, full);
            update += started.elapsed();
            // The frame after a commit measures the row that grew.
            let started = Instant::now();
            if before {
                probe.forget_estimates();
            }
            probe.measure(&persisted, live, probe.nodes() - 1, 100.0 + at as f32, full);
            measure += started.elapsed();
        }
        (update / commits as u32, measure / commits as u32)
    };
    let (full_update, before_measure) = time(true, true);
    let (_, full_measure) = time(true, false);
    let (tail_update, tail_measure) = time(false, false);
    println!("a timeline of {} contexts and {} nodes, {commits} commits:", persisted.len() + live.len(), {
        let mut probe = ModelProbe::new(WIDTH, MESSAGE_SIZE);
        probe.update(&persisted, 1, lives.last().unwrap(), 0, 0, &props, true);
        probe.nodes()
    });
    println!("  model update per commit: full (as before) {full_update:?}, incremental {tail_update:?}");
    println!("  relayout once the grown row is measured: as before (estimating every unmeasured row) {before_measure:?}, in full with kept estimates {full_measure:?}, from that row {tail_measure:?}");
}
