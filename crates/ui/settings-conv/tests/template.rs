//! `ConversationTemplateEditor.test.tsx`, `ToolSelectionGroups.test.tsx` and the pure rules behind them.

mod support;

use std::cell::RefCell;

use fluent::el::{hash_key, rect_of};
use fluent::ui::Key;
use mewrk_client::model::{ContextItem, ImageAttachment, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_settings_conv::logic::*;
use mewrk_settings_conv::ops::{Done, Handlers};
use mewrk_settings_conv::template_editor::{unenabled_tools, write_question_card, write_tool_card};
use mewrk_settings_conv::tools::{group_label, grouped_tools, tool_row_key};
use mewrk_settings_conv::web::*;
use mewrk_settings_conv::{TemplateEditor, ToolGroups};
use mewrk_ui_chat::frame::{timeline_snapshot, unit_key, MenuView};
use mewrk_ui_chat::inline_question_editor::question_field_key;
use mewrk_ui_chat::inline_text_editor::{text_field_key, InsertKind};
use mewrk_ui_chat::inline_tool_editor::{arg_field_key, result_field_key};
use mewrk_ui_chat::{TimelineEditorState, TimelineQuestionEditorState};
use mewrk_ui_composer::attach::{attach_ports, ClipboardContents};
use support::*;

const TEMPLATE: &str = "template_review";

fn tool_taking(name: &str, label: &str, category: &str, dangerous: bool, parameter: &str, parameter_label: &str) -> ToolDescriptor {
    serde_json::from_value(json!({ "name": name, "label": label, "description": "", "category": category, "dangerous": dangerous, "parameters": [{ "name": parameter, "label": parameter_label, "type": "string", "required": true }] })).unwrap()
}

fn tools() -> Rc<Vec<ToolDescriptor>> {
    Rc::new(vec![tool_taking("ls", "列出目录", "filesystem", false, "path", "路径"), tool_taking("bash", "运行命令", "shell", true, "command", "命令"), tool("ask_user", "提问", "orchestration", false), tool("preview_click", "点击", "web", false), tool("read_global_memory", "读记忆", "memory", false)])
}

/// A captured card: already executed once, and attested for the source conversation.
fn recorded_card() -> ContextItem {
    serde_json::from_value(json!({
        "kind": "tool", "id": "ctx_tool", "toolName": "ls", "input": { "path": "." },
        "result": { "success": true, "output": "src", "executedAt": "2026-09-15T00:00:01.000Z", "durationMs": 4 },
        "attestation": "attested-for-the-source-conversation", "createdAt": "2026-09-15T00:00:01.000Z",
    }))
    .unwrap()
}

fn user(id: &str, content: &str) -> ContextItem {
    serde_json::from_value(json!({ "kind": "user", "id": id, "createdAt": "", "content": content })).unwrap()
}

fn assistant(id: &str, content: &str) -> ContextItem {
    serde_json::from_value(json!({ "kind": "assistant", "id": id, "createdAt": "", "content": content })).unwrap()
}

fn body() -> Vec<ContextItem> {
    vec![user("ctx_user", "请审查这段改动"), recorded_card(), assistant("ctx_reply", "看完了。")]
}

#[derive(Clone, Default)]
struct Fx {
    saved: Rc<RefCell<Vec<Vec<ContextItem>>>>,
    enabled_asked: Rc<RefCell<Vec<Vec<String>>>>,
    refuse: Rc<RefCell<Option<String>>>,
}

impl Fx {
    fn last_saved(&self) -> Vec<ContextItem> {
        self.saved.borrow().last().cloned().expect("something was saved")
    }
}

fn page(fx: &Fx, body: Vec<ContextItem>, enabled: &[&str], autosave: bool, image_input_supported: bool) -> impl Fn(&mut App) -> E + use<> {
    let fx = fx.clone();
    let enabled: Vec<String> = enabled.iter().map(|s| (*s).to_owned()).collect();
    move |app: &mut App| {
        let (saved, refuse, asked) = (fx.saved.clone(), fx.refuse.clone(), fx.enabled_asked.clone());
        TemplateEditor {
            key: "t".into(),
            template_id: TEMPLATE.into(),
            contexts: Some(body.clone()),
            tools: tools(),
            enabled_tools: enabled.clone(),
            editable: true,
            image_input_supported,
            autosave,
            on_save: Some(Rc::new(move |app, contexts, done: Done<()>| {
                match refuse.borrow().clone() {
                    Some(reason) => done(app, Err(reason)),
                    None => {
                        saved.borrow_mut().push(contexts);
                        done(app, Ok(()));
                    }
                }
            })),
            on_enable_tools: Some(Rc::new(move |_, names| asked.borrow_mut().push(names))),
        }
        .view(app)
    }
}

fn editor(fx: &Fx, body: Vec<ContextItem>, enabled: &[&str], autosave: bool) -> impl Fn(&mut App) -> E + use<> {
    page(fx, body, enabled, autosave, false)
}

fn menu(path: &[&str], id: &str) -> u64 {
    timeline_menu_row(TEMPLATE, path, id)
}

/// A right click on the timeline's own ground, below everything: where the stream's trailing hint stands for after the last message.
fn open_menu_at_the_end(rig: &mut Rig, app: &mut App, view: View) {
    rig.right_click_at(app, view, 700.0, 900.0);
}

fn pencil(id: &str) -> (&'static str, u64, &str) {
    ("tl-edit", 0, id)
}

fn bin(id: &str) -> (&'static str, u64, &str) {
    ("tl-delete", 0, id)
}

fn user_text_editor(id: &str) -> u64 {
    hash_key(("tl-text-edit", 0u64, id))
}

fn tool_editor(id: &str) -> u64 {
    hash_key(("tl-tool-edit", 0u64, id))
}

fn placed_tool_editor(name: &str) -> u64 {
    hash_key(("tl-tool-insert", name))
}

fn page_save() -> (&'static str, &'static str) {
    ("template-save", "t")
}

fn place_call(rig: &mut Rig, app: &mut App, view: View, group: &str, name: &str) -> u64 {
    open_menu_at_the_end(rig, app, view);
    rig.click(app, view, menu(&[], "tool"));
    rig.click(app, view, menu(&["tool"], group));
    rig.click(app, view, menu(&["tool", group], name));
    placed_tool_editor(name)
}

#[test]
fn is_a_page_with_nothing_but_its_own_save_and_saves_nothing_until_something_changes() {
    let (mut rig, mut app) = Rig::new("tpl-save");
    let fx = Fx::default();
    let view = editor(&fx, body(), &["ls", "bash"], false);
    rig.frame(&mut app, &view);
    assert!(rig.shows("保存模板"));
    rig.click(&mut app, &view, page_save());
    assert!(fx.saved.borrow().is_empty());
    rig.click(&mut app, &view, bin("ctx_user"));
    assert!(fx.saved.borrow().is_empty(), "held until Save");
    assert!(!rig.exists(unit_key(0, "ctx_user")), "gone from the page already");
    rig.click(&mut app, &view, page_save());
    assert_eq!(fx.last_saved(), body()[1..]);
}

#[test]
fn writes_each_committed_edit_back_with_autosave_and_never_an_emptied_body() {
    let (mut rig, mut app) = Rig::new("tpl-auto");
    let fx = Fx::default();
    let view = editor(&fx, vec![user("c1", "a"), user("c2", "b")], &[], true);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("保存模板"), "the window already saves");
    assert!(fx.saved.borrow().is_empty(), "seeding a freshly read body writes nothing back");
    rig.click(&mut app, &view, bin("c1"));
    assert_eq!(fx.saved.borrow().len(), 1);
    rig.click(&mut app, &view, bin("c2"));
    assert_eq!(fx.saved.borrow().len(), 1, "the host refuses an empty body, and there is no button to draw disabled instead");
}

#[test]
fn offers_a_call_to_place_and_no_way_to_run_one() {
    let (mut rig, mut app) = Rig::new("tpl-place");
    let fx = Fx::default();
    let view = editor(&fx, body(), &["ls", "bash"], false);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rig.exists(pencil("ctx_tool")));
    assert!(rig.exists(bin("ctx_tool")));

    open_menu_at_the_end(&mut rig, &mut app, &view);
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).menu_index, Some(3));
    for id in ["System prompt", "User input", "Reasoning field", "tool", "Model reply"] {
        assert!(rig.exists(menu(&[], id)), "{id} is offered");
    }
    assert!(!rig.exists(menu(&[], "paste")), "upstream gives this surface no clipboard");
    assert!(!rig.exists(menu(&[], "fork")), "a template has nothing to fork");

    // `ls` is rerunnable on a real timeline, which is exactly why its absence here proves the button is withheld by the surface and not by the tool.
    rig.press(&mut app, &view, Key::Escape, false);
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).menu, None);
    rig.click(&mut app, &view, pencil("ctx_tool"));
    let card = tool_editor("ctx_tool");
    assert!(rig.exists(("tool-editor-save", card)));
    assert!(!rig.exists(("tool-editor-run", card)));
    rig.click(&mut app, &view, ("tool-editor-cancel", card));
    assert!(!rig.exists(("tool-editor-save", card)));

    let placed = place_call(&mut rig, &mut app, &view, "Filesystem", "ls");
    assert!(rig.exists(("tool-editor-save", placed)));
    assert!(!rig.exists(("tool-editor-run", placed)));
}

#[test]
fn offers_only_the_tools_its_owner_enables_as_calls_to_place() {
    let (mut rig, mut app) = Rig::new("tpl-offered");
    let fx = Fx::default();
    // The owner hands the model `ls` and nothing else, so a `bash` card written here would be prose the model can read and never act on.
    let view = editor(&fx, body(), &["ls", "preview_click"], false);
    open_menu_at_the_end(&mut rig, &mut app, &view);
    rig.click(&mut app, &view, menu(&[], "tool"));
    assert!(!rig.exists(menu(&["tool"], "Shell")));
    assert!(!rig.exists(menu(&["tool"], "Web")), "a call on a live page is not placed by hand");
    rig.click(&mut app, &view, menu(&["tool"], "Filesystem"));
    assert!(rig.exists(menu(&["tool", "Filesystem"], "ls")));
    assert!(!rig.exists(menu(&["tool", "Filesystem"], "bash")));
}

#[test]
fn places_a_call_written_out_by_hand_with_no_result_the_host_did_not_mint() {
    let (mut rig, mut app) = Rig::new("tpl-hand");
    let fx = Fx::default();
    let view = editor(&fx, body(), &["ls", "bash"], false);
    let card = place_call(&mut rig, &mut app, &view, "Shell", "bash");
    rig.type_into(&mut app, &view, arg_field_key(card, "command"), "npm test");
    rig.type_into(&mut app, &view, result_field_key(card), "全部通过");
    rig.click(&mut app, &view, ("tool-editor-save", card));
    assert!(fx.saved.borrow().is_empty(), "held until Save");
    assert!(!rig.exists(("tool-editor-save", card)), "the card's own Save closes its editor");
    rig.click(&mut app, &view, page_save());
    let saved = fx.last_saved();
    // Placed where the menu was opened: after the last message.
    assert_eq!(saved.len(), 4);
    assert_eq!(saved[..3], body());
    let ContextItem::Tool { tool_name, input, result, attestation, requested_input, .. } = &saved[3] else { panic!("{:?}", saved[3]) };
    assert_eq!(tool_name, "bash");
    assert_eq!(input["command"], json!("npm test"));
    assert_eq!(result.output, "全部通过");
    assert!(result.success);
    assert!(result.images.is_empty());
    assert!(result.diff.is_none());
    assert_eq!((result.executed_at.as_str(), result.duration_ms), ("", 0));
    assert!(attestation.is_empty());
    assert!(requested_input.is_none());
}

#[test]
fn refuses_a_call_whose_required_argument_was_left_empty_and_keeps_the_editor_open() {
    let (mut rig, mut app) = Rig::new("tpl-invalid");
    let fx = Fx::default();
    let view = editor(&fx, body(), &["ls", "bash"], false);
    let card = place_call(&mut rig, &mut app, &view, "Shell", "bash");
    rig.click(&mut app, &view, ("tool-editor-save", card));
    assert!(rig.exists(("tool-editor-failure", card)));
    assert!(rig.exists(("tool-editor-save", card)));
    rig.click(&mut app, &view, page_save());
    assert!(fx.saved.borrow().is_empty(), "nothing changed");
}

#[test]
fn rewrites_a_tool_card_without_claiming_the_source_conversations_credential() {
    let (mut rig, mut app) = Rig::new("tpl-rewrite");
    let fx = Fx::default();
    let mut card_body = body();
    if let ContextItem::Tool { requested_input, .. } = &mut card_body[1] {
        *requested_input = Some(json!({ "path": "what the model asked for" }).as_object().unwrap().clone());
    }
    let view = editor(&fx, card_body, &["ls", "bash"], false);
    rig.click(&mut app, &view, pencil("ctx_tool"));
    let card = tool_editor("ctx_tool");
    rig.replace_in(&mut app, &view, arg_field_key(card, "path"), "src");
    rig.replace_in(&mut app, &view, result_field_key(card), "App.tsx");
    rig.click(&mut app, &view, ("tool-editor-save", card));
    rig.click(&mut app, &view, page_save());
    let saved = fx.last_saved();
    let mut expected = recorded_card();
    if let ContextItem::Tool { input, attestation, result, .. } = &mut expected {
        *input = json!({ "path": "src" }).as_object().unwrap().clone();
        attestation.clear();
        result.output = "App.tsx".to_owned();
    }
    assert_eq!(saved[1], expected, "the host owns everything else about a result, but the draft must not ship a token bound to another conversation");
}

#[test]
fn edits_prose_in_place_and_hands_the_whole_body_over_on_save() {
    let (mut rig, mut app) = Rig::new("tpl-prose");
    let fx = Fx::default();
    let view = editor(&fx, body(), &["ls", "bash"], false);
    rig.click(&mut app, &view, pencil("ctx_user"));
    let card = user_text_editor("ctx_user");
    rig.replace_in(&mut app, &view, text_field_key(card), "改过的请求");
    // The card's own Save closes the inline editor; the page's Save is the one that hands the body to the host.
    rig.click(&mut app, &view, ("text-editor-save", card));
    assert!(fx.saved.borrow().is_empty());
    rig.click(&mut app, &view, page_save());
    let mut expected = body();
    if let ContextItem::User { content, .. } = &mut expected[0] {
        *content = "改过的请求".to_owned();
    }
    assert_eq!(fx.last_saved(), expected);
}

#[test]
fn an_edited_reply_is_no_longer_an_interrupted_one() {
    let (mut rig, mut app) = Rig::new("tpl-interrupted");
    let fx = Fx::default();
    let cut: ContextItem = serde_json::from_value(json!({ "kind": "assistant", "id": "a1", "createdAt": "", "content": "half of", "interrupted": true })).unwrap();
    let view = editor(&fx, vec![user("c1", "x"), cut], &[], false);
    rig.click(&mut app, &view, pencil("a1"));
    let card = user_text_editor("a1");
    rig.replace_in(&mut app, &view, text_field_key(card), "all of it");
    rig.click(&mut app, &view, ("text-editor-save", card));
    rig.click(&mut app, &view, page_save());
    assert!(matches!(&fx.last_saved()[1], ContextItem::Assistant { content, interrupted: false, .. } if content == "all of it"));
}

#[test]
fn inserts_each_kind_of_message_at_the_line_a_right_click_drew() {
    let (mut rig, mut app) = Rig::new("tpl-insert");
    let fx = Fx::default();
    let view = editor(&fx, body(), &["ls", "bash"], false);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    // The upper half of a card is the line above it, and the lower half the line below.
    let reply = rect_of(unit_key(0, "ctx_reply")).unwrap();
    rig.right_click_at(&mut app, &view, reply.x + 40.0, reply.y + reply.h * 0.25);
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).menu_index, Some(2));
    rig.click(&mut app, &view, menu(&[], "Reasoning field"));
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).menu, None, "choosing closes the menu");
    let card = hash_key(("tl-text-insert", InsertKind::Reasoning as u8));
    rig.type_into(&mut app, &view, text_field_key(card), "a thought");
    rig.click(&mut app, &view, ("text-editor-save", card));

    let first = rect_of(unit_key(0, "ctx_user")).unwrap();
    rig.right_click_at(&mut app, &view, first.x + first.w - 40.0, first.y + first.h * 0.25);
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).menu_index, Some(0));
    rig.click(&mut app, &view, menu(&[], "System prompt"));
    let card = hash_key(("tl-text-insert", InsertKind::System as u8));
    rig.type_into(&mut app, &view, text_field_key(card), "be brief");
    rig.click(&mut app, &view, ("text-editor-save", card));

    open_menu_at_the_end(&mut rig, &mut app, &view);
    rig.click(&mut app, &view, menu(&[], "Model reply"));
    let card = hash_key(("tl-text-insert", InsertKind::Assistant as u8));
    rig.type_into(&mut app, &view, text_field_key(card), "done");
    rig.click(&mut app, &view, ("text-editor-save", card));

    rig.click(&mut app, &view, page_save());
    let saved = fx.last_saved();
    assert_eq!(saved.len(), 6, "{saved:?}");
    assert!(matches!(&saved[0], ContextItem::System { content, local_only: false, .. } if content == "be brief"));
    assert_eq!(saved[1].id(), "ctx_user");
    assert_eq!(saved[2].id(), "ctx_tool");
    assert!(matches!(&saved[3], ContextItem::Reasoning { content: Some(words), form: Some(model::ReasoningForm::Plaintext), .. } if words == "a thought"), "hand-written reasoning is plaintext by construction: {:?}", saved[3]);
    assert_eq!(saved[4].id(), "ctx_reply");
    assert!(matches!(&saved[5], ContextItem::Assistant { content, .. } if content == "done"));
}

#[test]
fn right_click_deletes_what_is_picked_out_all_at_once() {
    let (mut rig, mut app) = Rig::new("tpl-pick");
    let fx = Fx::default();
    let view = editor(&fx, vec![user("c1", "a"), assistant("c2", "b"), user("c3", "c")], &[], false);
    rig.modified_click(&mut app, &view, unit_key(0, "c1"));
    rig.modified_click(&mut app, &view, unit_key(0, "c2"));
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).selection, ["c1", "c2"]);
    // With messages picked out, a right-click anywhere is about them.
    rig.right_click(&mut app, &view, unit_key(0, "c3"));
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).menu, Some(MenuView::Selection));
    assert!(!rig.exists(menu(&[], "copy")), "upstream gives this surface no clipboard");
    rig.click(&mut app, &view, menu(&[], "delete"));
    assert!(timeline_snapshot(&mut app, TEMPLATE).selection.is_empty());
    assert!(!rig.exists(unit_key(0, "c1")));
    assert!(!rig.exists(unit_key(0, "c2")));
    rig.click(&mut app, &view, page_save());
    assert_eq!(fx.last_saved(), vec![user("c3", "c")]);
}

#[test]
fn deleting_the_card_being_edited_closes_its_editor() {
    let (mut rig, mut app) = Rig::new("tpl-delete-open");
    let fx = Fx::default();
    let view = editor(&fx, vec![user("c1", "a"), user("c2", "b"), user("c3", "c")], &[], false);
    rig.click(&mut app, &view, pencil("c1"));
    let card = user_text_editor("c1");
    assert!(rig.exists(("text-editor-save", card)));
    // Another card's own delete leaves the editor be.
    rig.click(&mut app, &view, bin("c2"));
    assert!(rig.exists(("text-editor-save", card)));
    // A box drawn over the card under the editor is the one way left to reach it while its pencil and bin are given up to the editor.
    let under = rect_of(unit_key(0, "c1")).unwrap();
    rig.modified_drag_at(&mut app, &view, (under.x - 12.0, under.y - 6.0), (under.x + 20.0, under.y + 6.0));
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).selection, ["c1"]);
    rig.right_click(&mut app, &view, unit_key(0, "c3"));
    rig.click(&mut app, &view, menu(&[], "delete"));
    assert!(!rig.exists(("text-editor-save", card)), "its card is gone, so its editor goes too");
    rig.click(&mut app, &view, page_save());
    assert_eq!(fx.last_saved(), vec![user("c3", "c")]);
}

#[test]
fn asks_about_a_call_the_owner_no_longer_enables_and_repairs_it_on_request() {
    let (mut rig, mut app) = Rig::new("tpl-unenabled");
    let fx = Fx::default();
    // The switch was turned off after the body was written: the menu could not have placed this card today, and only saving has both halves in view.
    let view = editor(&fx, body(), &["bash"], false);
    rig.click(&mut app, &view, bin("ctx_user"));
    rig.click(&mut app, &view, page_save());
    assert!(fx.saved.borrow().is_empty(), "the question comes first: nothing is written until it is answered");
    assert!(rig.shows("模板里有未启用的工具"));
    assert!(rig.shows("这些工具当前是关闭的，套用后模型无法调用。"));
    assert!(rig.shows("列出目录"));
    assert!(rig.shows("ls"));
    rig.click(&mut app, &view, ("template-enable-tools", "t"));
    assert_eq!(*fx.enabled_asked.borrow(), vec![vec!["ls".to_owned()]], "exactly the missing names");
    assert_eq!(fx.last_saved(), body()[1..], "and the save goes through in the same breath");
}

#[test]
fn saves_a_body_with_an_unenabled_call_as_it_is_when_told_to() {
    let (mut rig, mut app) = Rig::new("tpl-anyway");
    let fx = Fx::default();
    let view = editor(&fx, body(), &["bash"], false);
    rig.click(&mut app, &view, bin("ctx_user"));
    rig.click(&mut app, &view, page_save());
    rig.click(&mut app, &view, ("template-save-anyway", "t"));
    assert!(fx.enabled_asked.borrow().is_empty(), "saying save anyway is not a request to turn it back on");
    assert_eq!(fx.last_saved(), body()[1..]);
}

#[test]
fn asks_about_a_name_once_under_autosave_rather_than_on_every_later_edit() {
    let (mut rig, mut app) = Rig::new("tpl-asked");
    let fx = Fx::default();
    let view = editor(&fx, vec![recorded_card(), user("c1", "x"), user("c2", "y")], &[], true);
    rig.click(&mut app, &view, bin("c1"));
    assert!(rig.shows("模板里有未启用的工具"));
    rig.click(&mut app, &view, ("template-save-anyway", "t"));
    rig.click(&mut app, &view, bin("c2"));
    assert!(!rig.shows("模板里有未启用的工具"));
    assert_eq!(fx.saved.borrow().len(), 2);
}

#[test]
fn reports_a_refused_save_beside_the_button_and_keeps_the_draft_to_fix() {
    let (mut rig, mut app) = Rig::new("tpl-refused");
    let fx = Fx::default();
    *fx.refuse.borrow_mut() = Some("带有子代理记录的工具卡不能手动编辑".into());
    let view = editor(&fx, vec![user("c1", "a"), user("c2", "b")], &[], false);
    rig.click(&mut app, &view, bin("c1"));
    rig.click(&mut app, &view, page_save());
    assert!(rig.shows("带有子代理记录的工具卡不能手动编辑"));
    assert!(rig.exists(unit_key(0, "c2")), "the draft is still there");
    *fx.refuse.borrow_mut() = None;
    rig.click(&mut app, &view, page_save());
    assert_eq!(fx.saved.borrow().len(), 1, "a refusal is something to fix, so Save stays pressable");
    assert!(!rig.shows("带有子代理记录的工具卡不能手动编辑"));
}

#[test]
fn withholds_every_mutation_affordance_from_a_body_it_does_not_own() {
    let (mut rig, mut app) = Rig::new("tpl-readonly");
    let view = |app: &mut App| {
        TemplateEditor { key: "t".into(), template_id: TEMPLATE.into(), contexts: Some(body()), tools: tools(), enabled_tools: vec!["ls".to_owned()], editable: false, image_input_supported: false, autosave: false, on_save: None, on_enable_tools: None }.view(app)
    };
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rig.exists(unit_key(0, "ctx_user")), "the same body");
    assert!(rig.exists(unit_key(0, "ctx_tool")));
    for id in ["ctx_user", "ctx_tool", "ctx_reply"] {
        assert!(!rig.exists(pencil(id)));
        assert!(!rig.exists(bin(id)));
    }
    assert!(!rig.shows("保存模板"));
    open_menu_at_the_end(&mut rig, &mut app, &view);
    assert_eq!(timeline_snapshot(&mut app, TEMPLATE).menu, None, "nothing to place");
    rig.modified_click(&mut app, &view, unit_key(0, "ctx_user"));
    assert!(timeline_snapshot(&mut app, TEMPLATE).selection.is_empty(), "nothing to pick");
}

#[test]
fn says_loading_until_the_body_arrives_and_seeds_once() {
    let (mut rig, mut app) = Rig::new("tpl-seed");
    let body: Rc<RefCell<Option<Vec<ContextItem>>>> = Rc::default();
    let b2 = body.clone();
    let view = move |app: &mut App| {
        TemplateEditor { key: "t".into(), template_id: "x".into(), contexts: b2.borrow().clone(), tools: tools(), enabled_tools: Vec::new(), editable: true, image_input_supported: false, autosave: false, on_save: None, on_enable_tools: None }.view(app)
    };
    rig.frame(&mut app, &view);
    assert!(rig.shows("正在读取…"));
    *body.borrow_mut() = Some(vec![user("c1", "first")]);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rig.exists(unit_key(0, "c1")));
    assert!(!rig.shows("正在读取…"));
    // The prop changing later must not discard what was edited since.
    rig.click(&mut app, &view, bin("c1"));
    *body.borrow_mut() = Some(vec![user("c9", "later")]);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(!rig.exists(unit_key(0, "c9")));
}

#[test]
fn a_new_editor_on_the_same_template_draws_its_own_body_and_not_what_the_timeline_remembers() {
    let (mut rig, mut app) = Rig::new("tpl-reopen");
    let held: Rc<RefCell<Vec<ContextItem>>> = Rc::new(RefCell::new(vec![user("c1", "a"), user("c2", "b"), user("c3", "c")]));
    let b = held.clone();
    let open = Rc::new(std::cell::Cell::new(true));
    let is_open = open.clone();
    let view = move |app: &mut App| -> E {
        if !is_open.get() {
            return div();
        }
        TemplateEditor { key: "t".into(), template_id: TEMPLATE.into(), contexts: Some(b.borrow().clone()), tools: tools(), enabled_tools: Vec::new(), editable: true, image_input_supported: false, autosave: false, on_save: None, on_enable_tools: None }.view(app)
    };
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rig.exists(unit_key(0, "c3")));
    // Off the screen and back, on a different and shorter body under the same id. Addresses do come back across reopenings, though not the very next one on this sequence, so this passes with or without the reset in `timeline`.
    for round in 0..8 {
        open.set(false);
        rig.frame(&mut app, &view);
        rig.frame(&mut app, &view);
        *held.borrow_mut() = vec![user(&format!("n{round}"), "new"), recorded_card()];
        open.set(true);
        rig.frame(&mut app, &view);
        rig.frame(&mut app, &view);
        assert!(rig.exists(unit_key(0, &format!("n{round}"))), "round {round}");
        assert!(rig.exists(unit_key(0, "ctx_tool")));
        assert!(!rig.exists(unit_key(0, "c3")));
    }
}

#[test]
fn encrypted_reasoning_can_be_deleted_but_not_edited() {
    let (mut rig, mut app) = Rig::new("tpl-encrypted");
    let fx = Fx::default();
    let card: ContextItem = serde_json::from_value(json!({ "kind": "reasoning", "id": "r1", "createdAt": "", "form": "encrypted", "tokens": 12 })).unwrap();
    let view = editor(&fx, vec![card, user("c1", "x")], &[], false);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rig.exists(unit_key(0, "r1")), "its token count is drawn");
    assert!(!rig.exists(pencil("r1")));
    assert!(rig.exists(bin("r1")));
    rig.click(&mut app, &view, bin("r1"));
    rig.click(&mut app, &view, page_save());
    assert_eq!(fx.last_saved(), vec![user("c1", "x")]);
}

fn question_input(n: usize) -> Value {
    let q = |i: usize| json!({ "question": format!("Q{i}?"), "header": "H", "multiSelect": false, "options": [{ "label": "a", "description": "A" }, { "label": "b", "description": "B" }] });
    json!({ "questions": (0..n).map(q).collect::<Vec<_>>() })
}

fn question_card(n: usize) -> ContextItem {
    serde_json::from_value(json!({
        "kind": "tool", "id": "q1", "toolName": "ask_user", "requestedInput": question_input(n), "input": question_input(n),
        "result": { "success": true, "output": "", "executedAt": "", "durationMs": 0 }, "attestation": "signed", "createdAt": "",
    }))
    .unwrap()
}

#[test]
fn rewrites_a_question_card_and_the_answer_that_settled_it() {
    let (mut rig, mut app) = Rig::new("tpl-question");
    let fx = Fx::default();
    let answer = user("a1", r#"User has answered your questions: "Q0?"="a""#);
    let view = editor(&fx, vec![question_card(1), answer], &["ask_user"], false);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    // The answer rides its question card, as one message.
    assert!(rig.exists(unit_key(0, "q1")));
    assert!(!rig.exists(unit_key(0, "a1")));
    rig.click(&mut app, &view, ("question-edit", "q1"));
    let card = hash_key(("question-editor", "q1"));
    rig.replace_in(&mut app, &view, question_field_key(card, "q-question", 0, 0), "Q changed?");
    rig.replace_in(&mut app, &view, question_field_key(card, "q-answer", 0, 0), "b");
    rig.click(&mut app, &view, ("question-editor-save", card));
    assert!(!rig.exists(("question-editor-save", card)), "in place, and closed by its own Save");
    rig.click(&mut app, &view, page_save());
    let saved = fx.last_saved();
    let ContextItem::Tool { attestation, requested_input, input, .. } = &saved[0] else { panic!() };
    assert!(attestation.is_empty());
    assert!(requested_input.is_none());
    assert_eq!(input["questions"][0]["question"], json!("Q changed?"));
    assert!(matches!(&saved[1], ContextItem::User { content, .. } if content.contains("\"b\"")), "{:?}", saved[1]);
}

#[test]
fn deletes_a_question_card_with_the_answer_that_settled_it() {
    let (mut rig, mut app) = Rig::new("tpl-question-delete");
    let fx = Fx::default();
    let view = editor(&fx, vec![user("c1", "x"), question_card(1), user("a1", r#"User has answered your questions: "Q0?"="a""#)], &["ask_user"], false);
    rig.click(&mut app, &view, ("question-delete", "q1"));
    rig.click(&mut app, &view, page_save());
    assert_eq!(fx.last_saved(), vec![user("c1", "x")]);
}

#[test]
fn a_question_cards_save_keeps_its_shape_and_its_answers() {
    let obj = |v: Value| v.as_object().unwrap().clone();
    let answer = user("a1", r#"User has answered your questions: "Q0?"="a""#);
    let editing = TimelineQuestionEditorState { item: question_card(1), answer: Some(answer.clone()) };
    let answered = || r#"User has answered your questions: "Q0?"="b""#.to_owned();
    let mut draft = vec![question_card(1), answer];
    let before = draft.clone();
    let mut refused = |input: Value, content: Option<String>| write_question_card(&mut draft, Some(&editing), obj(input), content).unwrap_err();
    let wrong_shape = "提问数量必须保持不变，且每一项都必须符合提问格式";
    assert_eq!(refused(question_input(2), Some(answered())), wrong_shape, "the same count of questions");
    assert_eq!(refused(json!({ "questions": [] }), Some(answered())), wrong_shape);
    let mut long = question_input(1);
    long["questions"][0]["header"] = json!("a very long header here");
    assert_eq!(refused(long, Some(answered())), wrong_shape);
    let no_answer = "每一个问题都必须保留非空回答";
    assert_eq!(refused(question_input(1), None), no_answer);
    assert_eq!(refused(question_input(1), Some("nothing".to_owned())), no_answer);
    assert_eq!(refused(question_input(1), Some(r#"User has answered your questions: "Q0?"="  ""#.to_owned())), no_answer);
    assert_eq!(draft, before, "a refusal changes nothing");
    assert_eq!(write_question_card(&mut draft, None, obj(question_input(1)), None).unwrap_err(), "没有活动提问");
    write_question_card(&mut draft, Some(&editing), obj(question_input(1)), Some(answered())).unwrap();
    assert!(matches!(&draft[1], ContextItem::User { content, .. } if *content == answered()));
    // A question nobody answered has no answer to keep.
    let unanswered = TimelineQuestionEditorState { item: question_card(1), answer: None };
    let mut lone = vec![question_card(1)];
    write_question_card(&mut lone, Some(&unanswered), obj(question_input(1)), None).unwrap();
}

#[test]
fn a_tool_save_says_what_is_missing_when_there_is_no_call_to_write() {
    let mut draft = vec![user("c1", "x"), recorded_card()];
    let before = draft.clone();
    let args = || json!({ "path": "." }).as_object().unwrap().clone();
    let write = |draft: &mut Vec<ContextItem>, editor: Option<&TimelineEditorState>| write_tool_card(draft, editor, args(), "out".to_owned(), Vec::new());
    assert_eq!(write(&mut draft, None).unwrap_err(), "没有正在编辑的上下文");
    assert_eq!(write(&mut draft, Some(&TimelineEditorState::Insert { kind: InsertKind::Tool, index: 0, tool_name: None })).unwrap_err(), "没有选择工具");
    assert_eq!(write(&mut draft, Some(&TimelineEditorState::Insert { kind: InsertKind::User, index: 0, tool_name: Some("ls".to_owned()) })).unwrap_err(), "没有选择工具");
    assert_eq!(write(&mut draft, Some(&TimelineEditorState::Edit { kind: InsertKind::User, item: user("c1", "x"), index: 0 })).unwrap_err(), "这条上下文不是工具调用");
    assert_eq!(draft, before);
    // An insertion line left past the end by a delete lands at the end.
    write(&mut draft, Some(&TimelineEditorState::Insert { kind: InsertKind::Tool, index: 9, tool_name: Some("ls".to_owned()) })).unwrap();
    assert_eq!(draft.len(), 3);
    assert!(matches!(&draft[2], ContextItem::Tool { result, .. } if result.output == "out"));
    // A rewrite replaces the images as well as the text, for the host to accept or refuse.
    let image = ImageAttachment { id: "i".to_owned(), name: "x.png".to_owned(), mime: "image/png".to_owned(), width: 1, height: 1, bytes: 1, short_id: Some(1) };
    write_tool_card(&mut draft, Some(&TimelineEditorState::Edit { kind: InsertKind::Tool, item: recorded_card(), index: 1 }), args(), "other".to_owned(), vec![image.clone()]).unwrap();
    let ContextItem::Tool { result, .. } = &draft[1] else { panic!() };
    assert_eq!((result.output.as_str(), result.images.as_slice()), ("other", &[image][..]));
    assert_eq!(result.duration_ms, 4, "the rest of the result is not this surface's");
}

/// A body that already spends a number, so a new picture has to take the next one.
fn numbered_body() -> Vec<ContextItem> {
    let image = ImageAttachment { id: "held".to_owned(), name: "held.png".to_owned(), mime: "image/png".to_owned(), width: 1, height: 1, bytes: 1, short_id: Some(3) };
    let mut first = user("c1", "old [Image #3]");
    if let ContextItem::User { images, .. } = &mut first {
        images.push(image);
    }
    vec![first, user("c2", "compare these")]
}

#[test]
fn numbers_a_pasted_image_against_the_templates_own_body_and_saves_it_with_the_message() {
    let (mut rig, mut app) = Rig::new("tpl-image");
    attach_ports(&mut app).read_clipboard = Rc::new(|| ClipboardContents { image: Some((vec![255; 4], 1, 1)), paths: Vec::new() });
    let fx = Fx::default();
    let view = page(&fx, numbered_body(), &[], false, true);
    rig.click(&mut app, &view, pencil("c2"));
    let card = user_text_editor("c2");
    rig.press_cmd(&mut app, &view, Key::Char('v'));
    let strip = hash_key(("text-editor-images", card));
    rig.until(&mut app, &view, "the picture on the message", |_| rect_of(("image-strip", strip)).is_some() && rect_of(("image-strip-pending", strip)).is_none());
    rig.click(&mut app, &view, ("text-editor-save", card));
    rig.click(&mut app, &view, page_save());
    let saved = fx.last_saved();
    let ContextItem::User { content, images, .. } = &saved[1] else { panic!() };
    assert_eq!(content, "compare these [Image #4]");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].short_id, Some(4));
    assert_eq!(saved[0], numbered_body()[0], "the message that holds the number is untouched");
}

#[test]
fn turns_a_pasted_image_away_when_the_model_reads_none() {
    let (mut rig, mut app) = Rig::new("tpl-no-image");
    attach_ports(&mut app).read_clipboard = Rc::new(|| ClipboardContents { image: Some((vec![255; 4], 1, 1)), paths: Vec::new() });
    let fx = Fx::default();
    let view = page(&fx, numbered_body(), &[], false, false);
    rig.click(&mut app, &view, pencil("c2"));
    let card = user_text_editor("c2");
    rig.press_cmd(&mut app, &view, Key::Char('v'));
    rig.until(&mut app, &view, "the reason it was turned away", |_| rect_of(("text-editor-notice", card)).is_some());
    rig.click(&mut app, &view, ("text-editor-save", card));
    rig.click(&mut app, &view, page_save());
    assert_eq!(fx.last_saved().len(), 2);
    let saved = fx.last_saved();
    assert!(matches!(&saved[1], ContextItem::User { images, .. } if images.is_empty()), "no bytes stored that the run would drop: {:?}", saved[1]);
}

#[test]
fn takes_a_pasted_file_onto_a_message_being_placed() {
    mewrk_ui_composer::pasted_text::forget_last_copy();
    let (mut rig, mut app) = Rig::new("tpl-file");
    let notes = rig.project.join("notes.txt");
    std::fs::write(&notes, "the notes").unwrap();
    attach_ports(&mut app).read_clipboard = Rc::new(move || ClipboardContents { image: None, paths: vec![notes.clone()] });
    let fx = Fx::default();
    let view = editor(&fx, body(), &["ls"], false);
    open_menu_at_the_end(&mut rig, &mut app, &view);
    rig.click(&mut app, &view, menu(&[], "User input"));
    let card = hash_key(("tl-text-insert", InsertKind::User as u8));
    rig.press_cmd(&mut app, &view, Key::Char('v'));
    let strip = hash_key(("text-editor-images", card));
    rig.until(&mut app, &view, "the file on the message", |_| rect_of(("image-strip", strip)).is_some() && rect_of(("image-strip-pending", strip)).is_none());
    rig.click(&mut app, &view, ("text-editor-save", card));
    rig.click(&mut app, &view, page_save());
    let saved = fx.last_saved();
    assert_eq!(saved.len(), 4);
    let ContextItem::User { files, images, .. } = &saved[3] else { panic!("{:?}", saved[3]) };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "notes.txt");
    assert!(images.is_empty());
}

/// The whole road on the real host: every act on the view, then what `update_conversation_template` kept.
#[test]
fn what_the_view_commits_is_what_the_host_keeps() {
    let (mut rig, mut app) = Rig::new("tpl-host");
    init_live(&mut app);
    let minted: Rc<RefCell<String>> = Rc::default();
    let write = Handlers::live().on_write_template.clone();
    let view = {
        let minted = minted.clone();
        move |app: &mut App| {
            let (holder, write) = (minted.clone(), write.clone());
            TemplateEditor {
                key: "host".into(),
                template_id: minted.borrow().clone(),
                contexts: Some(Vec::new()),
                tools: tools(),
                enabled_tools: vec!["ls".to_owned(), "bash".to_owned()],
                editable: true,
                image_input_supported: false,
                autosave: true,
                on_save: Some(Rc::new(move |app, contexts, done: Done<()>| {
                    let holder = holder.clone();
                    let id = holder.borrow().clone();
                    write(
                        app,
                        id,
                        contexts,
                        Box::new(move |app, result| {
                            done(
                                app,
                                result.map(|id| {
                                    *holder.borrow_mut() = id;
                                }),
                            );
                        }),
                    );
                })),
                on_enable_tools: None,
            }
            .view(app)
        }
    };
    let stored = |app: &mut App, id: &str| -> Vec<Value> { serde_json::from_value(app.host.invoke_blocking("preview_conversation_template", json!({ "templateId": id })).unwrap()).unwrap() };

    // A first message mints the id.
    rig.frame(&mut app, &view);
    place_user_message(&mut rig, &mut app, &view, "template:unsaved", "开局");
    let waiting = minted.clone();
    rig.until(&mut app, &view, "the minted id", move |_| !waiting.borrow().is_empty());
    let id = minted.borrow().clone();
    let body = stored(&mut app, &id);
    assert_eq!(body.len(), 1, "{body:?}");
    assert_eq!((body[0]["kind"].as_str(), body[0]["content"].as_str()), (Some("user"), Some("开局")));
    let first = body[0]["id"].as_str().unwrap().to_owned();

    // A placed call comes back with the result the host mints for a call nobody ran, and no credential.
    rig.frame(&mut app, &view);
    rig.right_click_at(&mut app, &view, 700.0, 900.0);
    let row = |path: &[&str], name: &str| timeline_menu_row(&id, path, name);
    rig.click(&mut app, &view, row(&[], "tool"));
    rig.click(&mut app, &view, row(&["tool"], "Shell"));
    rig.click(&mut app, &view, row(&["tool", "Shell"], "bash"));
    let card = placed_tool_editor("bash");
    rig.type_into(&mut app, &view, arg_field_key(card, "command"), "npm test");
    rig.type_into(&mut app, &view, result_field_key(card), "全部通过");
    rig.click(&mut app, &view, ("tool-editor-save", card));
    let held_id = id.clone();
    rig.until(&mut app, &view, "the placed card on disk", move |app| stored(app, &held_id).len() == 2);
    let body = stored(&mut app, &id);
    let placed = &body[1];
    assert_eq!(placed["toolName"], json!("bash"));
    assert_eq!(placed["input"], json!({ "command": "npm test" }));
    assert_eq!(placed["result"], json!({ "success": true, "output": "全部通过", "executedAt": "", "durationMs": 0 }));
    assert!(placed.get("attestation").is_none(), "{placed}");
    assert!(placed.get("requestedInput").is_none(), "{placed}");
    let placed_id = placed["id"].as_str().unwrap().to_owned();

    // A rewrite reaches the host too.
    rig.click(&mut app, &view, pencil(&placed_id));
    let card = tool_editor(&placed_id);
    rig.replace_in(&mut app, &view, result_field_key(card), "一处失败");
    rig.click(&mut app, &view, ("tool-editor-save", card));
    let held_id = id.clone();
    rig.until(&mut app, &view, "the rewrite on disk", move |app| stored(app, &held_id)[1]["result"]["output"] == json!("一处失败"));
    assert_eq!(stored(&mut app, &id)[1]["input"], json!({ "command": "npm test" }));

    // Picked out, right-clicked and deleted.
    rig.modified_click(&mut app, &view, unit_key(0, &first));
    rig.right_click(&mut app, &view, unit_key(0, &first));
    assert_eq!(timeline_snapshot(&mut app, &id).menu, Some(MenuView::Selection));
    rig.click(&mut app, &view, row(&[], "delete"));
    let held_id = id.clone();
    rig.until(&mut app, &view, "the deletion on disk", move |app| stored(app, &held_id).len() == 1);
    assert_eq!(stored(&mut app, &id)[0]["id"], json!(placed_id));

    // And the last message is not written away: the host refuses an empty body, and autosave does not ask it to.
    rig.click(&mut app, &view, bin(&placed_id));
    assert!(!rig.exists(unit_key(0, &placed_id)));
    for _ in 0..10 {
        rig.frame(&mut app, &view);
    }
    assert_eq!(stored(&mut app, &id).len(), 1);
    app.host.finalize();
}

#[test]
fn reports_only_what_can_be_enabled() {
    let body = vec![recorded_card(), recorded_card(), serde_json::from_value(json!({ "kind": "tool", "id": "m", "toolName": "read_global_memory", "input": {}, "result": { "success": true, "output": "", "executedAt": "", "durationMs": 0 }, "createdAt": "" })).unwrap()];
    assert_eq!(unenabled_tools(&body, &[], &tools()), vec!["ls"]);
    assert!(unenabled_tools(&body, &["ls".to_owned()], &tools()).is_empty());
}

// ---------------------------------------------------------------- ToolSelectionGroups

fn pick_tools() -> Vec<ToolDescriptor> {
    vec![
        tool("read", "读取", "filesystem", false),
        tool("write", "写入", "filesystem", true),
        tool("bash", "Bash", "shell", true),
        tool("preview_click", "点击", "web", false),
        tool("preview_start", "启动", "web", false),
        tool("preview_stop", "停止", "web", false),
        tool("preview_list", "列表", "web", false),
        tool("screen_app", "应用", "screen", false),
        tool("android_shell", "ADB", "android", false),
        tool("agent_spawn", "子代理", "orchestration", false),
        tool("task_wait", "等", "orchestration", false),
    ]
}

fn groups(enabled: Rc<RefCell<Vec<String>>>, key: Rc<RefCell<String>>) -> impl Fn(&mut App) -> E + use<> {
    move |_: &mut App| {
        let e = enabled.clone();
        ToolGroups { scope: "s".into(), tools: Rc::new(pick_tools()), enabled_tools: enabled.borrow().clone(), on_change: Rc::new(move |_, next| *e.borrow_mut() = next), expansion_key: key.borrow().clone(), tone_of: None }.view()
    }
}

#[test]
fn a_tools_fill_takes_in_its_documentation_link_and_reaches_to_the_row_edges() {
    let (mut rig, mut app) = Rig::new("tools-fill");
    let enabled = Rc::new(RefCell::new(vec!["read".to_owned()]));
    let view = groups(enabled.clone(), Rc::new(RefCell::new("k".into())));
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    let row = rect_of(tool_row_key("s", "read")).expect("the row");
    let docs = rect_of(("tool-docs", "s", "read")).expect("the link");
    assert!(row.x < docs.x && docs.right() < row.right() && row.y <= docs.y && docs.bottom() <= row.bottom(), "{row:?} {docs:?}");
    // The padding beside the link is part of what is filled in, so it is part of what toggles.
    rig.click_at(&mut app, &view, row.x + 1.0, row.y + row.h / 2.0);
    assert!(enabled.borrow().is_empty(), "{:?}", enabled.borrow());
}

#[test]
fn switches_the_lifecycle_tools_on_with_the_first_preview_tool_and_off_with_the_last() {
    let (mut rig, mut app) = Rig::new("tools-lifecycle");
    let enabled = Rc::new(RefCell::new(vec!["read".to_owned()]));
    let view = groups(enabled.clone(), Rc::new(RefCell::new("k".into())));
    rig.click(&mut app, &view, tool_row_key("s", "preview_click"));
    assert_eq!(*enabled.borrow(), vec!["read", "preview_click", "preview_start", "preview_stop", "preview_list"]);
    rig.click(&mut app, &view, tool_row_key("s", "screen_app"));
    rig.click(&mut app, &view, tool_row_key("s", "preview_click"));
    assert!(enabled.borrow().contains(&"preview_start".to_owned()), "keeps them while only a screen tool is left");
    rig.click(&mut app, &view, tool_row_key("s", "screen_app"));
    assert_eq!(*enabled.borrow(), vec!["read"]);
}

#[test]
fn turns_a_whole_group_on_and_off_from_its_heading_opening_a_collapsed_one_when_selected_into() {
    let (mut rig, mut app) = Rig::new("tools-bulk");
    let enabled = Rc::new(RefCell::new(Vec::new()));
    let view = groups(enabled.clone(), Rc::new(RefCell::new("k".into())));
    rig.click(&mut app, &view, ("tool-group", "s", "filesystem"));
    assert!(!rig.shows("读取已关闭"), "collapsed");
    assert!(enabled.borrow().is_empty(), "disclosure changes expansion without changing enabled tools");
    rig.click(&mut app, &view, ("tool-group-plus", "s", "filesystem"));
    assert_eq!(*enabled.borrow(), vec!["read", "write"]);
    assert!(rig.shows("读取已启用"), "turning a group on is also a way of asking to see it");
    rig.click(&mut app, &view, ("tool-group", "s", "filesystem"));
    rig.click(&mut app, &view, ("tool-group-minus", "s", "filesystem"));
    assert!(enabled.borrow().is_empty());
    assert!(!rig.shows("读取已关闭"), "turning one off leaves the disclosure alone");
}

#[test]
fn drops_manual_collapses_when_the_expansion_key_changes_and_never_because_of_a_toggle() {
    let (mut rig, mut app) = Rig::new("tools-collapse");
    let enabled = Rc::new(RefCell::new(vec!["read".to_owned()]));
    let key = Rc::new(RefCell::new("k".to_owned()));
    let view = groups(enabled.clone(), key.clone());
    rig.click(&mut app, &view, tool_row_key("s", "read"));
    assert!(rig.shows("读取已关闭"), "does not collapse a group when its last enabled tool is turned off");
    rig.click(&mut app, &view, ("tool-group", "s", "shell"));
    assert!(!rig.shows("Bash已关闭"));
    *key.borrow_mut() = "other".into();
    rig.frame(&mut app, &view);
    assert!(rig.shows("Bash已关闭"));
}

#[test]
fn draws_the_screen_groups_after_the_preview_group_and_before_orchestration_in_both_languages() {
    let names: Vec<String> = grouped_tools(&pick_tools()).iter().map(|(c, _)| group_label(*c).to_owned()).collect();
    mewrk_client::i18n::set_english(false);
    let names_zh: Vec<String> = grouped_tools(&pick_tools()).iter().map(|(c, _)| group_label(*c).to_owned()).collect();
    assert_eq!(names_zh, vec!["文件与搜索", "Shell", "预览", "屏幕", "Android", "代理编排"]);
    let _ = names;
}

#[test]
fn marks_a_reviewed_tools_row_and_gives_every_row_its_documentation() {
    let (mut rig, mut app) = Rig::new("tools-marks");
    let view = groups(Rc::new(RefCell::new(Vec::new())), Rc::new(RefCell::new("k".into())));
    rig.frame(&mut app, &view);
    assert!(rig.shows("需审查"));
    assert!(rig.shows("docs:https://mewrk.dev/zh-CN/tools/screen_app.html"));
    assert!(!rig.shows("等已关闭"), "a host-derived runtime tool has no control");
}

// ---------------------------------------------------------------- pure rules

fn web() -> model::ConversationWebSearchSettings {
    model::ConversationWebSearchSettings::default()
}

#[test]
fn restores_the_provider_a_leg_had_when_it_is_switched_off_and_on_again() {
    let mut w = web();
    w.provider = model::SearchProviderSelection::Explicit { provider_kind: model::SearchProviderKind::Tavily };
    let off = switch_web_leg(WebLeg::Search, false, None, &w, true, &[], &RememberedWebLegs::default());
    assert_eq!(off.patch.provider, Some(model::SearchProviderSelection::Disabled));
    off.patch.apply(&mut w);
    let on = switch_web_leg(WebLeg::Search, true, None, &w, true, &[], &off.remembered);
    assert_eq!(on.patch.provider, Some(model::SearchProviderSelection::Explicit { provider_kind: model::SearchProviderKind::Tavily }));
    assert_eq!(on.remembered.search, None);
}

#[test]
fn keeps_access_on_when_search_is_switched_off_beside_a_fetch_that_is_only_off_for_lack_of_a_native_fetch() {
    let w = web();
    let flip = switch_web_leg(WebLeg::Search, false, Some(true), &w, false, &[], &RememberedWebLegs::default());
    assert_eq!(flip.web_access, Some(true));
    assert_eq!(flip.patch.provider, Some(model::SearchProviderSelection::Disabled));
}

#[test]
fn turns_fetch_on_with_native_when_the_model_has_one_and_with_the_first_catalog_provider_when_not() {
    let mut w = web();
    w.fetch_provider = model::FetchProviderSelection::Disabled;
    let on = switch_web_leg(WebLeg::Fetch, true, Some(true), &w, true, &[model::SearchProviderKind::Jina], &RememberedWebLegs::default());
    assert_eq!(on.patch.fetch_provider, Some(model::FetchProviderSelection::Native));
    let on = switch_web_leg(WebLeg::Fetch, true, Some(true), &w, false, &[model::SearchProviderKind::Jina], &RememberedWebLegs::default());
    assert_eq!(on.patch.fetch_provider, Some(model::FetchProviderSelection::Explicit { provider_kind: model::SearchProviderKind::Jina }));
}

#[test]
fn asks_a_family_what_its_native_web_tools_can_do() {
    use model::ProviderFamily as F;
    assert!(family_supports_native_fetch(Some(F::Anthropic)));
    assert!(family_supports_native_fetch(Some(F::ClaudeAgent)));
    assert!(!family_supports_native_fetch(Some(F::OpenaiResponses)));
    assert!(family_selects_native_tool_type(Some(F::Bedrock)));
    assert!(!family_selects_native_tool_type(Some(F::ClaudeAgent)));
    assert!(!family_selects_native_tool_type(None));
}

#[test]
fn selected_provider_problems_leave_a_missing_key_alone() {
    let assets: model::WebSearchAssets = serde_json::from_value(json!({ "providers": [{ "kind": "tavily", "enabled": true }, { "kind": "exa", "enabled": false }] })).unwrap();
    let check = |c, cap| selected_provider_problem(c, &assets, cap);
    assert_eq!(check(WebBackendChoice::Explicit(model::SearchProviderKind::Tavily), SearchCapability::SearchKeywords), None);
    assert_eq!(check(WebBackendChoice::Explicit(model::SearchProviderKind::Exa), SearchCapability::SearchKeywords), Some(ProviderProblem::Disabled));
    assert_eq!(check(WebBackendChoice::Explicit(model::SearchProviderKind::Tavily), SearchCapability::FetchUrls), Some(ProviderProblem::Unavailable));
    assert_eq!(check(WebBackendChoice::Unavailable, SearchCapability::FetchUrls), Some(ProviderProblem::Unavailable));
    assert_eq!(check(WebBackendChoice::Native, SearchCapability::FetchUrls), None);
}

#[test]
fn a_stored_order_puts_new_rows_last_and_a_reorder_moves_one() {
    let items = vec!["a".to_owned(), "b".to_owned(), "c".to_owned(), "d".to_owned()];
    let order = vec!["c".to_owned(), "gone".to_owned(), "a".to_owned()];
    assert_eq!(sort_by_order(&items, &order, |s| s.as_str()), vec!["c", "a", "b", "d"]);
    assert_eq!(reorder_ids(&items, "d", "b", true), vec!["a", "d", "b", "c"]);
    assert_eq!(reorder_ids(&items, "a", "c", false), vec!["b", "c", "a", "d"]);
    assert_eq!(reorder_ids(&items, "x", "c", false), items);
}

#[test]
fn a_capability_level_is_named_by_its_machine_and_directory() {
    assert_eq!(capability_workspace_key(None, "/w/a///"), "local|/w/a");
    let ssh = model::RunTarget::Ssh { machine_id: "m1".into() };
    assert_eq!(capability_workspace_key(Some(&ssh), "/srv"), "ssh:m1|/srv");
    assert_eq!(parse_capability_workspace_key("ssh:m1|/srv"), (Some(ssh.clone()), "/srv".to_owned()));
    assert_eq!(parse_capability_workspace_key("wsl:Ubuntu|/x").0, Some(model::RunTarget::Wsl { distro: "Ubuntu".into() }));
    assert_eq!(parse_capability_workspace_key("/bare"), (None, "/bare".to_owned()));
    assert_eq!(workspace_location_title("/srv", Some(&ssh), &[]), format!("/srv ({})", mewrk_client::t("已删除的机器", "Deleted machine")));
}

#[test]
fn docs_addresses_follow_the_site() {
    mewrk_client::i18n::set_english(false);
    assert_eq!(docs_href("features", None), "https://mewrk.dev/zh-CN/working.html#conversation-settings");
    assert_eq!(docs_href("agents", Some("role-window")), "https://mewrk.dev/zh-CN/agents.html#role-window");
    assert_eq!(docs_href("toolDescriptions", None), "https://mewrk.dev/zh-CN/prompt-profiles.html");
    assert_eq!(tool_docs_href("read"), "https://mewrk.dev/zh-CN/tools/read.html");
}
