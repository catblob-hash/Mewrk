//! `components/InlineToolEditor.tsx`: a call as its arguments and then its result, named on the left and boxed on the right, the
//! same shape a card takes when it is only being read.
//!
//! Values are typed as text throughout, booleans included: a call is a JSON document, and a control that renders `false` as an
//! unlit switch hides the difference between an argument set to false and one never given at all.
//!
//! Both arguments and result are editable, whether the card is being corrected or placed for the first time. Saving them does not
//! execute anything, so the host has to issue the card's attestation over what was typed, which is also what lets a call with
//! side effects be recorded without causing them. Running is offered on a placed call, and on a recorded one only for the tools
//! `can_rerun_tool` accepts. A caller that hands over no `on_run` has nothing to run against, and gets Cancel and Save alone.

use std::hash::Hash;

use mewrk_client::model::{ContextItem, ImageAttachment, JsonObject, ToolDescriptor, ToolParameter, ToolParameterType, ToolResult};
use mewrk_client::prelude::*;
use mewrk_pane_tasks::widgets::spinner;
use taffy::prelude::{fr, length, line, max_content, minmax, span};

use crate::image_strip::{image_strip_with, ImageStripProps};
use crate::kit::{action_button, action_button_disabled};

pub type ToolDone = Box<dyn FnOnce(&mut App, Result<(), String>)>;
pub type RunTool = Rc<dyn Fn(&mut App, String, JsonObject, ToolDone)>;
pub type SaveToolEdit = Rc<dyn Fn(&mut App, JsonObject, String, Vec<ImageAttachment>, ToolDone)>;

/// `.tool-kv__key`.
const KEY: TypeStyle = TypeStyle { size: 10.0, line: 15.5, weight: Weight::Regular, mono: true, italic: false, serif: false };
/// `.tool-kv__value > .plain-field`, whose line is whole: a box rounds to whole pixels, and text a fraction taller than its box scrolls.
const VALUE: TypeStyle = TypeStyle { size: 10.5, line: 16.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
/// `.inline-tool-editor__failure`.
const FAILURE: TypeStyle = TypeStyle { size: 10.0, line: 14.5, weight: Weight::Regular, mono: false, italic: false, serif: false };

/// `.plain-field` grows with its text and never scrolls inside a frame of its own, so a value's box has no ceiling.
const GROWS: f32 = 1_000_000.0;

/// `lib/toolRerun.ts`: which recorded calls the timeline may re-execute with edited arguments. A name qualifies only when
/// re-running it changes nothing outside this app's own record, and the host's `security.rs::classify` accepts it outside the model
/// run loop. Shell is deliberately absent even though the host classifies `bash "git status"` as a read: that verdict comes from
/// analysing the concrete command, which this list cannot do. The preview tools that act, and every screen tool, would replay
/// their effect or read what a device holds.
const RERUNNABLE_TOOLS: [&str; 12] = ["ls", "grep", "find", "read", "lsp", "preview_list", "preview_logs", "preview_snapshot", "preview_inspect", "preview_console_logs", "preview_network", "preview_screenshot"];

pub fn can_rerun_tool(tool_name: &str) -> bool {
    RERUNNABLE_TOOLS.contains(&tool_name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decode {
    String,
    Number,
    Boolean,
    Json,
}

/// One editable argument row, flattened from either a descriptor or a recorded call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditableField {
    pub name: String,
    /// What upstream names the control; required arguments keep the trailing " *".
    pub label: String,
    pub required: bool,
    /// How the typed text becomes JSON again.
    pub decode: Decode,
    /// Shown in the empty control; the descriptor's hint at what belongs there.
    pub placeholder: Option<String>,
}

fn field_from_parameter(parameter: &ToolParameter) -> EditableField {
    let default_text = parameter.default_value.as_ref().filter(|value| !value.is_null()).map(|value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    });
    EditableField {
        name: parameter.name.clone(),
        label: if parameter.required { format!("{} *", parameter.label) } else { parameter.label.clone() },
        required: parameter.required,
        decode: match parameter.parameter_type {
            ToolParameterType::Number => Decode::Number,
            ToolParameterType::Boolean => Decode::Boolean,
            ToolParameterType::Json => Decode::Json,
            ToolParameterType::String | ToolParameterType::Multiline => Decode::String,
        },
        // A declared default is only ever a hint in the empty control: the call carries what was written, and an argument left
        // out stays left out.
        placeholder: parameter.placeholder.clone().or(default_text),
    }
}

/// The descriptor's arguments, plus anything the recorded call carries that the descriptor no longer declares. A card outlives
/// the catalog entry that made it, and an argument that cannot be seen cannot be corrected.
pub fn recorded_fields(input: &JsonObject, descriptor: Option<&ToolDescriptor>) -> Vec<EditableField> {
    let mut fields: Vec<EditableField> = descriptor.map_or_else(Vec::new, |tool| tool.parameters.iter().map(field_from_parameter).collect());
    for (name, recorded) in input {
        if fields.iter().any(|field| field.name == *name) {
            continue;
        }
        fields.push(EditableField {
            name: name.clone(),
            label: name.clone(),
            required: false,
            decode: match recorded {
                Value::Bool(_) => Decode::Boolean,
                Value::String(_) => Decode::String,
                _ => Decode::Json,
            },
            placeholder: None,
        });
    }
    fields
}

/// One text per field, in the fields' order.
pub type Draft = Vec<(String, String)>;

fn draft_value<'a>(draft: &'a Draft, name: &str) -> &'a str {
    draft.iter().find(|(key, _)| key == name).map_or("", |(_, value)| value.as_str())
}

/// The controls' text, from the arguments the call carries and nothing else. Filling a declared default in here would write it
/// into the call on save: an argument the model never gave, replayed to it as if it had.
pub fn draft_from_input(fields: &[EditableField], input: &JsonObject) -> Draft {
    fields
        .iter()
        .map(|field| {
            let text = match input.get(&field.name) {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(value)) if field.decode == Decode::String => value.clone(),
                Some(value) => serde_json::to_string_pretty(value).unwrap_or_default(),
            };
            (field.name.clone(), text)
        })
        .collect()
}

/// JavaScript's `Number(text)` on already trimmed text.
fn js_number(raw: &str) -> Option<f64> {
    let radix = |prefix: &str, radix: u32| raw.get(..2).filter(|head| head.eq_ignore_ascii_case(prefix)).and_then(|_| u64::from_str_radix(&raw[2..], radix).ok()).map(|v| v as f64);
    if let Some(value) = radix("0x", 16).or_else(|| radix("0o", 8)).or_else(|| radix("0b", 2)) {
        return Some(value);
    }
    match raw {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // What `str::parse` accepts beyond JavaScript's grammar: `inf`, `nan` and the like, all of which spell a letter JavaScript has
    // no use for.
    if raw.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    raw.parse().ok()
}

fn number_value(value: f64) -> Value {
    // `JSON.stringify` writes a whole number without a point, and anything it cannot write as null.
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

pub struct DraftMessages {
    pub required: &'static str,
    pub number: &'static str,
    pub boolean: &'static str,
    pub json: &'static str,
}

pub fn draft_messages() -> DraftMessages {
    DraftMessages { required: t("此项为必填", "This field is required"), number: t("请输入有效数字", "Enter a valid number"), boolean: t("请输入 true 或 false", "Enter true or false"), json: t("JSON 格式不正确", "Invalid JSON") }
}

/// The typed call, or what is wrong with it field by field, in the fields' order.
pub fn parse_draft(fields: &[EditableField], draft: &Draft, messages: &DraftMessages, recorded: Option<(&JsonObject, &Draft)>) -> Result<JsonObject, Vec<(String, String)>> {
    let mut input = JsonObject::new();
    let mut errors: Vec<(String, String)> = Vec::new();
    for field in fields {
        let name = field.name.as_str();
        let typed = draft_value(draft, name);
        // A control left as it was keeps the recorded value exactly (a `null`, an empty string, a number in a text field) rather
        // than whatever the typed text would decode to. Only what was edited is read back from the text.
        if let Some((recorded_input, recorded_draft)) = recorded
            && let Some(value) = recorded_input.get(name)
            && typed == draft_value(recorded_draft, name)
        {
            input.insert(name.to_owned(), value.clone());
            continue;
        }
        let raw = typed.trim();
        if raw.is_empty() {
            if field.required {
                errors.push((name.to_owned(), messages.required.to_owned()));
            }
            continue;
        }
        match field.decode {
            Decode::Number => match js_number(raw) {
                Some(value) if !value.is_nan() => {
                    input.insert(name.to_owned(), number_value(value));
                }
                _ => errors.push((name.to_owned(), messages.number.to_owned())),
            },
            // Written as typed, even when it equals the default: an explicit `false` the call carried is the call's, and an empty
            // control already omits it.
            Decode::Boolean => match raw {
                "true" | "false" => {
                    input.insert(name.to_owned(), Value::Bool(raw == "true"));
                }
                _ => errors.push((name.to_owned(), messages.boolean.to_owned())),
            },
            Decode::Json => match serde_json::from_str::<Value>(raw) {
                Ok(value) => {
                    input.insert(name.to_owned(), value);
                }
                Err(_) => errors.push((name.to_owned(), messages.json.to_owned())),
            },
            Decode::String => {
                input.insert(name.to_owned(), Value::String(typed.to_owned()));
            }
        }
    }
    if errors.is_empty() { Ok(input) } else { Err(errors) }
}

/// The card an insertion is drafting: the chosen tool, no arguments, and no result until it runs.
pub fn draft_tool_context(tool_name: &str) -> ContextItem {
    tool_context("draft", tool_name, JsonObject::new(), None, ToolResult { success: true, output: String::new(), images: Vec::new(), diff: None, executed_at: String::new(), duration_ms: 0 }, "")
}

/// A tool card with only the fields an editor reads.
pub(crate) fn tool_context(id: &str, tool_name: &str, input: JsonObject, requested_input: Option<JsonObject>, result: ToolResult, created_at: &str) -> ContextItem {
    ContextItem::Tool {
        id: id.to_owned(),
        tool_name: tool_name.to_owned(),
        round: None,
        model_turn_id: None,
        provider_call_id: None,
        requested_input,
        input,
        result,
        subagent: None,
        notice: None,
        attestation: String::new(),
        created_at: created_at.to_owned(),
    }
}

pub struct ToolEditorProps {
    pub item: ContextItem,
    pub descriptor: Option<ToolDescriptor>,
    /// The call is being placed rather than corrected. It decides only what the run button offers to do; the form is the same
    /// either way, result included, because a placed call may be written out by hand instead of executed.
    pub inserting: bool,
    pub on_cancel: Handler<App>,
    /// Executes the call and writes its arguments and result together. Absent on a surface with nothing to execute against (a
    /// template, which belongs to no conversation and no workspace), where the button is therefore not drawn.
    pub on_run: Option<RunTool>,
    /// Writes the arguments and result without executing anything.
    pub on_save: Option<SaveToolEdit>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Save,
    Rerun,
}

struct EditorState {
    fields: Vec<EditableField>,
    /// What `fields` was read from: the catalog can answer after the editor opened, and its rows then join the form.
    descriptor: Option<ToolDescriptor>,
    call_input: JsonObject,
    initial_draft: Draft,
    draft: Draft,
    output: String,
    errors: Vec<(String, String)>,
    images: Vec<ImageAttachment>,
    busy: Option<Busy>,
    failure: Option<String>,
}

/// What the actions run on: the typed call, or nothing after noting why not.
fn parse(state: &Local<EditorState>) -> Option<JsonObject> {
    let mut s = state.borrow_mut();
    let parsed = parse_draft(&s.fields, &s.draft, &draft_messages(), Some((&s.call_input, &s.initial_draft)));
    repaint();
    match parsed {
        Ok(input) => {
            s.errors.clear();
            s.failure = None;
            Some(input)
        }
        Err(errors) => {
            s.failure = errors.first().map(|(_, message)| message.clone());
            s.errors = errors;
            None
        }
    }
}

/// `run`: the same steps for both buttons.
fn act(state: &Local<EditorState>, app: &mut App, mode: Busy, go: impl FnOnce(&mut App, JsonObject, ToolDone)) {
    if state.borrow().busy.is_some() {
        return;
    }
    let Some(input) = parse(state) else { return };
    state.update(|s| {
        s.busy = Some(mode);
        s.failure = None;
    });
    let state = state.clone();
    go(
        app,
        input,
        Box::new(move |_, result| {
            // A success leaves the editor busy: the card it was editing is about to replace it.
            if let Err(message) = result {
                state.update(|s| {
                    s.failure = Some(message);
                    s.busy = None;
                });
            }
        }),
    );
}

/// The editor's two halves. A timeline row lays `actions` over the end of its header, where its edit and delete buttons were
/// (`.inline-tool-editor__actions`, absolute against `.timeline-row`), and `body` in its details.
pub struct ToolEditorParts {
    pub actions: E,
    pub body: E,
}

pub fn inline_tool_editor_parts(key: impl Hash + Clone, props: ToolEditorProps) -> ToolEditorParts {
    let t_ = theme();
    let k = hash_key(key);
    let ToolEditorProps { item, descriptor, inserting, on_cancel, on_run, on_save } = props;
    let empty = JsonObject::new();
    let (tool_name, input, requested_input, result_output, result_images, success) = match &item {
        ContextItem::Tool { tool_name, input, requested_input, result, .. } => (tool_name.clone(), input, requested_input.as_ref(), result.output.as_str(), result.images.as_slice(), result.success),
        _ => (String::new(), &empty, None, "", &[][..], true),
    };
    let rerunnable = !inserting && can_rerun_tool(&tool_name) && descriptor.is_some();
    let state = use_state(("tool-editor", k), || {
        // The call as the model made it. A hook may have run it with other arguments, but what the model is shown of its own call
        // is what it wrote; editing the executed arguments instead would, on save, hand the model a rewrite as its own words even
        // when nothing was changed.
        let call_input = requested_input.unwrap_or(input).clone();
        let fields = recorded_fields(&call_input, descriptor.as_ref());
        let initial_draft = draft_from_input(&fields, &call_input);
        EditorState { draft: initial_draft.clone(), fields, descriptor: descriptor.clone(), call_input, initial_draft, output: result_output.to_owned(), errors: Vec::new(), images: result_images.to_vec(), busy: None, failure: None }
    });
    if state.with(|s| s.descriptor != descriptor) {
        state.update(|s| {
            s.fields = recorded_fields(&s.call_input, descriptor.as_ref());
            s.descriptor = descriptor.clone();
        });
    }

    let cancel_on_escape = {
        let (state, on_cancel) = (state.clone(), on_cancel.clone());
        Rc::new(move |app: &mut App, press: &KeyPress| {
            if press.key == Key::Escape && state.borrow().busy.is_none() {
                on_cancel(app);
                return true;
            }
            false
        })
    };

    let busy = state.borrow().busy;
    let disabled = busy.is_some();
    let rows: Vec<E> = {
        let s = state.borrow();
        s.fields
            .iter()
            .flat_map(|arg| {
                let invalid = s.errors.iter().any(|(name, _)| *name == arg.name);
                let name = arg.name.clone();
                let on_change = {
                    let state = state.clone();
                    let name = name.clone();
                    move |_: &mut App, value: String| {
                        state.update(|s| {
                            // `disabled` while busy: a controlled field whose change is not taken goes back to what it was given.
                            if s.busy.is_some() {
                                return;
                            }
                            match s.draft.iter_mut().find(|(key, _)| *key == name) {
                                Some(slot) => slot.1 = value,
                                None => s.draft.push((name.clone(), value)),
                            }
                        });
                    }
                };
                let escape = cancel_on_escape.clone();
                let key_cell = text(arg.name.clone()).ty(KEY).color(t_.text_tertiary).ellipsis().pt(4.0).max_w(104.0).tooltip(arg.name.clone());
                let value_field = field(arg_field_key(k, &arg.name), draft_value(&s.draft, &arg.name).to_owned())
                    .placeholder(arg.placeholder.clone().unwrap_or_default())
                    .multiline(VALUE.line, GROWS)
                    .ty(VALUE)
                    .bare()
                    .controlled()
                    .on_change(on_change)
                    .on_key(move |app, press| escape(app, press))
                    .build()
                    .aria_label(arg.label.clone())
                    .aria_invalid(invalid)
                    .aria_disabled(disabled);
                let focused = focused_id() == hash_key(("field", arg_field_key(k, &arg.name)));
                let border = if focused { t_.accent } else if invalid { t_.critical } else { t_.card_stroke };
                let value_cell = div().min_w(0.0).py(3.0).px(7.0).radius(6.0).border(border).bg(t_.control_fill).disabled(disabled).child(value_field);
                [key_cell, value_cell]
            })
            .collect()
    };

    let s = state.borrow();
    // A failed result reads as one even while it is being rewritten.
    let result_field = {
        let state2 = state.clone();
        let escape = cancel_on_escape.clone();
        let written = field(result_field_key(k), s.output.clone()).multiline(VALUE.line, GROWS).ty(VALUE).bare().controlled();
        let written = if success { written } else { written.color(t_.critical) };
        written
            .on_change(move |_: &mut App, value| {
                state2.update(|s| {
                    if s.busy.is_none() {
                        s.output = value;
                    }
                });
            })
            .on_key(move |app, press| escape(app, press))
            .build()
            .aria_label(t("返回值", "Result"))
            .aria_disabled(disabled)
    };
    let result_focused = focused_id() == hash_key(("field", result_field_key(k)));
    let on_remove: Rc<dyn Fn(&mut App, &str)> = {
        let state = state.clone();
        Rc::new(move |_, id: &str| state.update(|s| s.images.retain(|image| image.id != id)))
    };
    let strip = image_strip_with(("tool-editor-images", k), ImageStripProps { images: &s.images, files: &[], compact: true, busy: false, on_remove: Some(on_remove), on_remove_file: None }).map(|strip| div().mb(10.0).child(strip));
    let result_row = div()
        .min_w(0.0)
        .py(3.0)
        .px(7.0)
        .radius(6.0)
        .border(if result_focused { t_.accent } else { t_.card_stroke })
        .bg(t_.control_fill)
        .disabled(disabled)
        .with_style(|style| style.grid_column = taffy::Line { start: line(1), end: span(2) })
        .child(strip)
        .child(result_field);

    let grid = div()
        .key(("tool-kv", k))
        .with_style(|style| {
            style.display = taffy::style::Display::Grid;
            style.grid_template_columns = vec![minmax(length(0.0_f32), max_content()), minmax(length(0.0_f32), fr(1.0_f32))];
            style.gap = taffy::Size { width: length(9.0_f32), height: length(5.0_f32) };
            style.align_items = Some(taffy::style::AlignItems::Start);
        })
        .children(rows)
        .child(result_row);

    let failure = s.failure.clone();
    let body = col()
        .key(("tool-editor-body", k))
        .gap(5.0)
        .min_w(0.0)
        .on_key({
            let escape = cancel_on_escape.clone();
            move |app: &mut App, press: &KeyPress| escape(app, press)
        })
        .child(grid)
        .child(failure.map(|message| text(message).key(("tool-editor-failure", k)).ty(FAILURE).color(t_.critical).selectable().role(Role::Alert)));
    drop(s);

    // Cancel, then Run, then Save last, on the column every control on the timeline keeps.
    let cancel_label = t("取消", "Cancel");
    let cancel = if disabled {
        action_button_disabled(("tool-editor-cancel", k), "X", cancel_label)
    } else {
        let on_cancel = on_cancel.clone();
        action_button(("tool-editor-cancel", k), "X", cancel_label, move |app| on_cancel(app))
    };
    let run = on_run.filter(|_| inserting || rerunnable).map(|on_run| {
        let label = if inserting { t("执行并添加", "Run and add") } else { t("重新执行", "Rerun") };
        let run_key = ("tool-editor-run", k);
        if busy == Some(Busy::Rerun) {
            return busy_button(run_key, label);
        }
        if disabled {
            return action_button_disabled(run_key, "Play", label);
        }
        let (state, tool_name) = (state.clone(), tool_name.clone());
        action_button(run_key, "Play", label, move |app| {
            let (on_run, tool_name) = (on_run.clone(), tool_name.clone());
            act(&state, app, Busy::Rerun, move |app, input, done| on_run(app, tool_name, input, done));
        })
    });
    let save = on_save.map(|on_save| {
        let label = t("保存", "Save");
        let save_key = ("tool-editor-save", k);
        if busy == Some(Busy::Save) {
            return busy_button(save_key, label);
        }
        if disabled {
            return action_button_disabled(save_key, "Check", label);
        }
        let state = state.clone();
        action_button(save_key, "Check", label, move |app| {
            let on_save = on_save.clone();
            let (output, images) = state.with(|s| (s.output.clone(), s.images.clone()));
            act(&state, app, Busy::Save, move |app, input, done| on_save(app, input, output, images, done));
        })
    });
    let actions = row().key(("tool-editor-actions", k)).items_center().gap(1.0).child(cancel).child(run).child(save);

    ToolEditorParts { actions, body }
}

/// The same two halves in one column, for a surface with no row header to lay the actions over.
pub fn inline_tool_editor(key: impl Hash + Clone, props: ToolEditorProps) -> E {
    let ToolEditorParts { actions, body } = inline_tool_editor_parts(key, props);
    col().gap(5.0).min_w(0.0).child(row().justify_end().pr(4.0).child(actions)).child(body)
}

/// A button that is doing what it says: not to be pressed again.
fn busy_button(key: impl Hash, label: &'static str) -> E {
    let t_ = theme();
    row().key(key).center().size(24.0, 24.0).radius(6.0).shrink0().tooltip(label).aria_disabled(true).child(spinner(13.0, t_.text_secondary))
}

/// An argument's field, as `field(..)` and `focus_request(("field", ..))` take it.
pub fn arg_field_key(editor: u64, name: &str) -> (&'static str, u64, String) {
    ("tool-arg", editor, name.to_owned())
}

pub fn result_field_key(editor: u64) -> (&'static str, u64) {
    ("tool-result", editor)
}
