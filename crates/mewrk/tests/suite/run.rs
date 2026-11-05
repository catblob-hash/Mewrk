//! A whole run through everything: the composer on screen, the host, the Rust model layer, a socket and back to the timeline.

use mewrk_client::model::{ContextItem, Conversation};
use mewrk_client::prelude::*;
use serde_json::Value;

use crate::mock::{serve, Mock};
use crate::rig::*;

fn with_provider(rig: &mut Rig, app: &mut App) -> Mock {
    let _ = rig;
    let mock = serve(0);
    // A loopback address needs no key, which keeps this out of the machine's credential store.
    let provider = serde_json::from_value(json!({
        "id": "mock", "name": "Mock", "enabled": true, "family": "openai_chat", "baseUrl": mock.address,
        "models": [{ "id": "mock-model", "name": "", "group": "", "capabilities": [], "reasoningContent": "plaintext" }], "activeModelId": "mock-model"
    }))
    .unwrap();
    app.update_document(move |doc| {
        doc.assets.api_providers.push(provider);
        doc.global_settings.active_provider_id = Some("mock".to_owned());
    });
    mock
}

fn stored(app: &mut App, id: &str) -> Conversation {
    let value = app.host.invoke_blocking("load_conversation", json!({ "conversationId": id })).expect("load_conversation");
    serde_json::from_value::<Option<Conversation>>(value).unwrap().expect("the host has it")
}

fn send(rig: &mut Rig, app: &mut App, message: &str) {
    rig.click(app, mewrk_ui_composer::editor::textarea_key());
    rig.type_text(app, message);
    rig.click(app, mewrk_ui_composer::send_button_key());
}

fn tool_names(mock: &Mock) -> Vec<String> {
    let requests = mock.requests.lock().unwrap();
    let Some(first) = requests.iter().find(|body| body["tools"].is_array()) else { return Vec::new() };
    first["tools"].as_array().unwrap().iter().filter_map(|tool: &Value| tool["function"]["name"].as_str().map(str::to_owned)).collect()
}

/// A new task with these tools, on the mock, with the composer on screen.
fn new_task(rig: &mut Rig, app: &mut App, tools: &[&str]) -> Mock {
    let mock = with_provider(rig, app);
    rig.frames(app, 4);
    rig.click(app, ("workspace-new-task", "ws1"));
    rig.frames(app, 4);
    let names: Vec<String> = tools.iter().map(|name| (*name).to_owned()).collect();
    if !names.is_empty() {
        app.update_active_conversation_settings(move |settings| settings.enabled_tools.extend(names));
    }
    rig.frames(app, 2);
    mock
}

fn asked(rig: &mut Rig, app: &mut App, mock: &Mock, times: usize) -> String {
    rig.until(app, "the model to be asked", |_| mock.requests.lock().unwrap().len() >= times);
    app.active_conversation_id().expect("the send made a conversation")
}

fn replies_of(conversation: &Conversation) -> Vec<String> {
    conversation.contexts.iter().filter_map(|c| if let ContextItem::Assistant { content, .. } = c { Some(content.clone()) } else { None }).collect()
}

fn users_of(conversation: &Conversation) -> Vec<String> {
    conversation.contexts.iter().filter_map(|c| if let ContextItem::User { content, .. } = c { Some(content.clone()) } else { None }).collect()
}

fn finished(rig: &mut Rig, app: &mut App, id: &str, what: &str) {
    let id = id.to_owned();
    rig.until(app, what, move |app| !app.model_run_is_active(&id) && app.active_tool_prompt(&id).is_none());
}

#[test]
fn a_message_typed_in_the_composer_comes_back_on_the_timeline_and_on_disk() {
    let (mut rig, mut app) = Rig::new("run-send");
    let mock = new_task(&mut rig, &mut app, &[]);
    send(&mut rig, &mut app, "hello there");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the reply", move |app| !app.model_run_is_active(&check) && app.conversation(&check).is_some_and(|c| !replies_of(&c).is_empty()));
    eprintln!("tools in the catalogue: {:?}", app.doc.tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>());
    eprintln!("tools offered: {:?}", tool_names(&mock));
    let saved = stored(&mut app, &id);
    assert_eq!(users_of(&saved), ["hello there"]);
    assert!(replies_of(&saved).iter().any(|reply| reply.contains("# Result")), "{:?}", saved.contexts);
    // What the timeline drew from it, and it came through the host's own conversion: the model saw the words typed here.
    let last = mock.requests.lock().unwrap().iter().find(|body| body["tools"].is_array()).cloned().unwrap();
    assert!(last["messages"].to_string().contains("hello there"));
}

#[test]
fn a_tool_call_runs_and_its_result_goes_back_to_the_model() {
    let (mut rig, mut app) = Rig::new("run-read");
    std::fs::write(rig.project.join("README.md"), "the words in the file\n").unwrap();
    let mock = new_task(&mut rig, &mut app, &[]);
    send(&mut rig, &mut app, "[read] please");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the answer after the tool", move |app| app.conversation(&check).is_some_and(|c| replies_of(&c).iter().any(|reply| reply.contains("The tool said"))));
    finished(&mut rig, &mut app, &id, "the run to end");
    let saved = stored(&mut app, &id);
    let tool = saved.contexts.iter().find(|c| matches!(c, ContextItem::Tool { .. })).expect("the call is on the timeline");
    assert!(serde_json::to_string(tool).unwrap().contains("the words in the file"), "{tool:?}");
    assert!(mock.requests.lock().unwrap().iter().any(|body| body["messages"].to_string().contains("the words in the file")));
}

#[test]
fn a_dangerous_tool_waits_for_approval_and_a_click_lets_it_run() {
    let (mut rig, mut app) = Rig::new("run-approve");
    let mock = new_task(&mut rig, &mut app, &["write"]);
    send(&mut rig, &mut app, "[write] please");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the approval card", move |app| app.active_tool_prompt(&check).is_some());
    let prompt = app.active_tool_prompt(&id).unwrap().0;
    assert!(!rig.project.join("written.txt").exists(), "nothing runs before the click");
    rig.click(&mut app, mewrk_ui_composer::approval_dock::allow_key(&prompt.prompt_id));
    finished(&mut rig, &mut app, &id, "the run to end");
    assert_eq!(std::fs::read_to_string(rig.project.join("written.txt")).unwrap(), "from the model\n");
}

#[test]
fn a_denied_tool_does_not_run_and_the_model_hears_so() {
    let (mut rig, mut app) = Rig::new("run-deny");
    let mock = new_task(&mut rig, &mut app, &["write"]);
    send(&mut rig, &mut app, "[write] please");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the approval card", move |app| app.active_tool_prompt(&check).is_some());
    let prompt = app.active_tool_prompt(&id).unwrap().0;
    rig.click(&mut app, mewrk_ui_composer::approval_dock::deny_key(&prompt.prompt_id));
    finished(&mut rig, &mut app, &id, "the run to end");
    assert!(!rig.project.join("written.txt").exists());
}

#[test]
fn a_question_is_answered_on_its_card_and_the_answer_reaches_the_model() {
    let (mut rig, mut app) = Rig::new("run-ask");
    let mock = new_task(&mut rig, &mut app, &["ask_user"]);
    send(&mut rig, &mut app, "[ask] please");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the question", move |app| app.active_tool_prompt(&check).is_some());
    let prompt = app.active_tool_prompt(&id).unwrap().0;
    use mewrk_ui_composer::question_dock::{row_key, submit_key};
    rig.click(&mut app, row_key(&prompt.prompt_id, 0, 0));
    rig.frames(&mut app, 4);
    if fluent::el::rect_of(submit_key(&prompt.prompt_id)).is_some() {
        rig.click(&mut app, submit_key(&prompt.prompt_id));
    }
    finished(&mut rig, &mut app, &id, "the run to end");
    assert!(mock.requests.lock().unwrap().iter().any(|body| body["messages"].to_string().contains("Left")), "the answer went back");
}

/// Enter on a card that has only just taken the keyboard answers nothing, however close the pointer mark is; the arrows and Enter do.
#[test]
fn a_question_is_answered_from_the_keyboard_and_not_by_an_enter_already_on_its_way() {
    let (mut rig, mut app) = Rig::new("run-ask-keys");
    let mock = new_task(&mut rig, &mut app, &["ask_user"]);
    send(&mut rig, &mut app, "[ask] please");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the question", move |app| app.active_tool_prompt(&check).is_some());
    let prompt = app.active_tool_prompt(&id).unwrap().0;
    rig.frames(&mut app, 4);
    let enter = KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false };
    rig.key(&mut app, enter);
    rig.frames(&mut app, 10);
    assert!(app.active_tool_prompt(&id).is_some(), "the card still waits");
    assert!(!mewrk_ui_composer::question_dock::dock_state(&mut app, &prompt.prompt_id).unwrap().responded);
    rig.key(&mut app, KeyPress { key: Key::Down, ..enter });
    rig.frames(&mut app, 2);
    rig.key(&mut app, enter);
    finished(&mut rig, &mut app, &id, "the run to end");
    let saved = stored(&mut app, &id);
    let Some(ContextItem::Tool { input, .. }) = saved.contexts.iter().find(|c| matches!(c, ContextItem::Tool { .. })) else { panic!("the call is on the timeline") };
    assert_eq!(input["answers"]["Which one?"], "Right", "the row the arrows moved to, not the first");
}

/// A call the host refused reads as failed on its row and opens to the arguments and the error, and its raw data is the same call.
#[test]
fn a_denied_call_reads_as_failed_on_its_row_and_opens_to_what_it_was_asked_and_answered() {
    let (mut rig, mut app) = Rig::new("run-deny-card");
    let mock = new_task(&mut rig, &mut app, &["write"]);
    send(&mut rig, &mut app, "[write] please");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the approval card", move |app| app.active_tool_prompt(&check).is_some());
    let prompt = app.active_tool_prompt(&id).unwrap().0;
    rig.click(&mut app, mewrk_ui_composer::approval_dock::deny_key(&prompt.prompt_id));
    finished(&mut rig, &mut app, &id, "the run to end");
    let saved = stored(&mut app, &id);
    let card = saved.contexts.iter().find(|c| matches!(c, ContextItem::Tool { .. })).expect("the call is on the timeline");
    let item = mewrk_ui_chat::item::Item::settled(card);
    let tool = item.tool().expect("a tool card");
    assert!(!tool.result.success);
    let presentation = mewrk_ui_chat::tool_view::get_tool_presentation_in(&tool, None, mewrk_ui_chat::tool_view::Lang::En, None, None);
    assert!(presentation.title.starts_with("Failed to write file: "), "the phrase, then why: {}", presentation.title);
    assert_eq!(presentation.target.as_deref(), Some("written.txt"));
    assert!(presentation.subject.is_none(), "a file it never wrote is not a link");
    let detail = mewrk_ui_chat::tool_view::tool_detail_model_in(&tool, None, mewrk_ui_chat::tool_view::Lang::En);
    let card = detail.kv().expect("the shared card, not a diff of nothing");
    assert_eq!(card.get("path"), Some("written.txt"));
    assert_eq!(card.result.as_deref(), Some(tool.result.output.as_str()));
    let raw = mewrk_ui_chat::tool_view::raw_data_text(&tool);
    assert!(raw.starts_with("{\n  \"tool\": \"write\""), "{raw}");
}

#[test]
fn stop_ends_a_live_run_and_the_conversation_stays_usable() {
    let (mut rig, mut app) = Rig::new("run-stop");
    let mock = new_task(&mut rig, &mut app, &[]);
    mock.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    send(&mut rig, &mut app, "[slow] take your time");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the run to be live", move |app| app.model_run_is_active(&check));
    rig.frames(&mut app, 10);
    rig.click(&mut app, mewrk_ui_composer::send_button_key());
    let check = id.clone();
    rig.until(&mut app, "the stop to settle", move |app| !app.model_run_is_active(&check));
    mock.hold.store(false, std::sync::atomic::Ordering::SeqCst);
    // The same box sends again.
    send(&mut rig, &mut app, "again");
    let check = id.clone();
    rig.until(&mut app, "the second reply", move |app| !app.model_run_is_active(&check) && app.conversation(&check).is_some_and(|c| replies_of(&c).iter().any(|reply| reply.contains("# Result"))));
    assert!(users_of(&stored(&mut app, &id)).iter().any(|said| said == "again"));
}

#[test]
fn a_message_sent_during_a_run_waits_its_turn() {
    let (mut rig, mut app) = Rig::new("run-queue");
    let mock = new_task(&mut rig, &mut app, &[]);
    mock.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    send(&mut rig, &mut app, "[slow] first");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the run to be live", move |app| app.model_run_is_active(&check));
    rig.frames(&mut app, 6);
    send(&mut rig, &mut app, "second");
    let check = id.clone();
    rig.until(&mut app, "the queue", move |app| app.conversation(&check).is_some_and(|c| c.queued_messages.len() == 1));
    mock.hold.store(false, std::sync::atomic::Ordering::SeqCst);
    let check = id.clone();
    rig.until(&mut app, "both turns", move |app| !app.model_run_is_active(&check) && app.conversation(&check).is_some_and(|c| users_of(&c).len() == 2 && c.queued_messages.is_empty() && replies_of(&c).len() == 2));
    assert_eq!(users_of(&stored(&mut app, &id)), ["[slow] first", "second"]);
}

#[test]
fn a_provider_error_is_shown_and_the_run_ends() {
    let (mut rig, mut app) = Rig::new("run-fail");
    let mock = new_task(&mut rig, &mut app, &[]);
    send(&mut rig, &mut app, "[fail] now");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the error", move |app| app.model_run_error(&check).is_some());
    let error = app.model_run_error(&id).unwrap();
    assert!(format!("{error:?}").contains("the mock was told to fail"), "{error:?}");
    assert!(!app.model_run_is_active(&id));
}

#[test]
fn a_conversation_survives_quitting_and_opens_again_with_its_reply() {
    let (mut rig, mut app) = Rig::new("run-restart");
    let mock = new_task(&mut rig, &mut app, &[]);
    send(&mut rig, &mut app, "keep this");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the reply", move |app| !app.model_run_is_active(&check) && app.conversation(&check).is_some_and(|c| !replies_of(&c).is_empty()));
    let mut app = rig.restart(app);
    rig.frames(&mut app, 8);
    rig.click(&mut app, ("conversation-main", id.as_str()));
    let check = id.clone();
    rig.until(&mut app, "the body to load", move |app| app.conversation(&check).is_some_and(|c| !c.contexts.is_empty()));
    let conversation = app.conversation(&id).unwrap();
    assert_eq!(users_of(&conversation), ["keep this"]);
    assert!(replies_of(&conversation).iter().any(|reply| reply.contains("# Result")));
    app.host.finalize();
}

/// Two exchanges in a task: a message to come back to, with one before it.
fn two_exchanges(rig: &mut Rig, app: &mut App, mock: &Mock) -> String {
    send(rig, app, "first");
    let id = asked(rig, app, mock, 1);
    let check = id.clone();
    rig.until(app, "the first reply", move |app| !app.model_run_is_active(&check) && app.conversation(&check).is_some_and(|c| !replies_of(&c).is_empty()));
    send(rig, app, "second");
    let check = id.clone();
    rig.until(app, "the second reply", move |app| !app.model_run_is_active(&check) && app.conversation(&check).is_some_and(|c| replies_of(&c).len() == 2));
    id
}

#[test]
fn branching_from_a_message_carries_what_came_before_and_puts_it_back_in_the_box() {
    let (mut rig, mut app) = Rig::new("run-branch");
    let mock = new_task(&mut rig, &mut app, &[]);
    let source = two_exchanges(&mut rig, &mut app, &mock);
    let second = app.conversation(&source).unwrap().contexts.iter().find(|c| matches!(c, ContextItem::User { content, .. } if content == "second")).cloned().unwrap();
    app.branch_from_user_context(&second);
    let check = source.clone();
    rig.until(&mut app, "the new conversation", move |app| app.active_conversation_id().is_some_and(|id| id != check) && app.active_conversation_id().and_then(|id| app.conversation(&id)).is_some_and(|c| users_of(&c) == ["first"]));
    let branch = app.active_conversation_id().unwrap();
    assert_eq!(app.composer_draft(&branch), "second", "the branched message waits to be edited");
    assert_eq!(users_of(&app.conversation(&source).unwrap()), ["first", "second"], "the source keeps its own");
    // The box has the caret, and sending goes on from what was carried over.
    rig.frames(&mut app, 2);
    rig.click(&mut app, mewrk_ui_composer::send_button_key());
    let check = branch.clone();
    rig.until(&mut app, "the branch's reply", move |app| !app.model_run_is_active(&check) && app.conversation(&check).is_some_and(|c| replies_of(&c).len() == 2));
    assert_eq!(users_of(&stored(&mut app, &branch)), ["first", "second"]);
    let last = mock.requests.lock().unwrap().last().cloned().unwrap();
    assert!(last["messages"].to_string().contains("first"), "the model saw the history");
}

#[test]
fn forking_takes_the_timeline_through_that_point_into_a_conversation_of_its_own() {
    let (mut rig, mut app) = Rig::new("run-fork");
    let mock = new_task(&mut rig, &mut app, &[]);
    let source = two_exchanges(&mut rig, &mut app, &mock);
    let through = app.conversation(&source).unwrap().contexts.iter().position(|c| matches!(c, ContextItem::User { content, .. } if content == "second")).unwrap();
    app.fork_conversation_at(through);
    let check = source.clone();
    rig.until(&mut app, "the fork", move |app| app.active_conversation_id().is_some_and(|id| id != check) && app.active_conversation_id().and_then(|id| app.conversation(&id)).is_some_and(|c| users_of(&c) == ["first"]));
    assert_eq!(users_of(&app.conversation(&source).unwrap()), ["first", "second"]);
}

#[test]
fn rewinding_takes_back_the_files_and_the_messages_and_returns_the_message_to_the_box() {
    let (mut rig, mut app) = Rig::new("run-rewind");
    let mock = new_task(&mut rig, &mut app, &["write"]);
    send(&mut rig, &mut app, "[write] please");
    let id = asked(&mut rig, &mut app, &mock, 1);
    let check = id.clone();
    rig.until(&mut app, "the approval card", move |app| app.active_tool_prompt(&check).is_some());
    let prompt = app.active_tool_prompt(&id).unwrap().0;
    rig.click(&mut app, mewrk_ui_composer::approval_dock::allow_key(&prompt.prompt_id));
    let check = id.clone();
    rig.until(&mut app, "the run to end", move |app| !app.model_run_is_active(&check) && app.active_tool_prompt(&check).is_none() && app.conversation(&check).is_some_and(|c| !replies_of(&c).is_empty()));
    let written = rig.project.join("written.txt");
    assert!(written.exists());

    let user = app.conversation(&id).unwrap().contexts.iter().find(|c| matches!(c, ContextItem::User { .. })).cloned().unwrap();
    mewrk_ui_chat::handlers::open_rewind_dialog(&mut app, &user, None);
    // Both stays disabled until the preview has found something to restore, and a click on it till then does nothing.
    let start = std::time::Instant::now();
    while app.conversation(&id).is_some_and(|c| !users_of(&c).is_empty()) {
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "the rewind never went through");
        rig.frames(&mut app, 2);
        if fluent::el::rect_of("rewind-both").is_some() {
            rig.click(&mut app, "rewind-both");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!written.exists(), "the file the tool made is gone");
    assert_eq!(app.composer_draft(&id), "[write] please");
}

/// Sends `message`, waits for the run it starts to end, and gives back the conversation and the last request of that run.
fn exchange(rig: &mut Rig, app: &mut App, mock: &Mock, message: &str) -> (String, Value) {
    let turns = |mock: &Mock| mock.requests.lock().unwrap().iter().filter(|body| body["tools"].is_array()).count();
    let before = turns(mock);
    send(rig, app, message);
    rig.until(app, "the model to be asked", |_| turns(mock) > before);
    let id = app.active_conversation_id().expect("the send made a conversation");
    finished(rig, app, &id, "the run to end");
    let last = mock.requests.lock().unwrap().iter().rev().find(|body| body["tools"].is_array()).cloned().unwrap();
    (id, last)
}

fn offers(body: &Value, tool: &str) -> bool {
    body["tools"].as_array().unwrap().iter().any(|entry| entry["function"]["name"] == tool)
}

/// Flips a tool's switch and waits until the host has it. A run's end refreshes the read model from the store behind a
/// switch moved in that moment, so it is moved again until it holds.
fn set_tool(rig: &mut Rig, app: &mut App, id: &str, tool: &'static str, on: bool) {
    let id = id.to_owned();
    rig.until(app, "the switch to reach the host", move |app| {
        let holds = |settings: &mewrk_client::model::ConversationSettings| settings.enabled_tools.iter().any(|name| name == tool) == on;
        if !app.conversation(&id).is_some_and(|c| holds(&c.settings)) {
            app.update_active_conversation_settings(move |settings| {
                settings.enabled_tools.retain(|name| name != tool);
                if on {
                    settings.enabled_tools.push(tool.to_owned());
                }
            });
        }
        app.conversation(&id).is_some_and(|c| holds(&c.settings)) && holds(&stored(app, &id).settings)
    });
}

/// The cards registering `tool`.
fn registrations(conversation: &Conversation, tool: &str) -> Vec<String> {
    conversation.contexts.iter().filter(|c| matches!(c, ContextItem::System { tools_added, .. } if tools_added.iter().any(|name| name == tool))).map(|c| c.id().to_owned()).collect()
}

#[test]
fn a_tool_turned_off_leaves_what_the_model_is_shown_and_comes_back_without_being_appended() {
    let (mut rig, mut app) = Rig::new("run-tool-closed");
    std::fs::write(rig.project.join("README.md"), "the words in the file\n").unwrap();
    let mock = new_task(&mut rig, &mut app, &[]);
    let (id, body) = exchange(&mut rig, &mut app, &mock, "[read] please");
    assert!(body["messages"].to_string().contains("call_read"), "{body}");

    set_tool(&mut rig, &mut app, &id, "read", false);
    let (_, body) = exchange(&mut rig, &mut app, &mock, "and now");
    assert!(!offers(&body, "read"));
    assert!(!body["messages"].to_string().contains("call_read"), "{}", body["messages"]);
    let card = |conversation: &Conversation| conversation.contexts.iter().any(|c| matches!(c, ContextItem::Tool { tool_name, .. } if tool_name == "read"));
    assert!(card(&stored(&mut app, &id)), "the transcript keeps the call");

    set_tool(&mut rig, &mut app, &id, "read", true);
    let (_, body) = exchange(&mut rig, &mut app, &mock, "and again");
    assert!(offers(&body, "read"));
    assert!(body["messages"].to_string().contains("call_read"), "{}", body["messages"]);
    let saved = stored(&mut app, &id);
    assert!(card(&saved));
    assert!(registrations(&saved, "read").is_empty(), "its calls came back; it was not appended: {:?}", saved.contexts);
}

#[test]
fn a_tool_appended_again_keeps_one_registration_on_disk_and_on_screen() {
    let (mut rig, mut app) = Rig::new("run-tool-again");
    let mock = new_task(&mut rig, &mut app, &[]);
    app.update_active_conversation_settings(|settings| settings.enabled_tools.retain(|name| name != "grep"));
    rig.frames(&mut app, 2);
    let (id, body) = exchange(&mut rig, &mut app, &mock, "one");
    assert!(!offers(&body, "grep"));

    set_tool(&mut rig, &mut app, &id, "grep", true);
    let (_, body) = exchange(&mut rig, &mut app, &mock, "two");
    assert!(offers(&body, "grep"));
    let first = registrations(&stored(&mut app, &id), "grep");
    assert_eq!(first.len(), 1, "with no call of it, it was appended");

    set_tool(&mut rig, &mut app, &id, "grep", false);
    let (_, body) = exchange(&mut rig, &mut app, &mock, "three");
    assert!(!offers(&body, "grep"));
    assert_eq!(registrations(&stored(&mut app, &id), "grep"), first, "turning it off leaves the transcript alone");

    set_tool(&mut rig, &mut app, &id, "grep", true);
    let (_, body) = exchange(&mut rig, &mut app, &mock, "four");
    assert!(offers(&body, "grep"));
    let last = registrations(&stored(&mut app, &id), "grep");
    assert_eq!(last.len(), 1, "the old registration is gone from the store");
    assert_ne!(last, first);
    let check = id.clone();
    let expected = last.clone();
    rig.until(&mut app, "the read model to follow the store", move |app| app.conversation(&check).is_some_and(|c| registrations(&c, "grep") == expected));
}
