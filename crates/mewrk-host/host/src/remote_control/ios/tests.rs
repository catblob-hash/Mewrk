use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use serde_json::{json, Value};

use super::*;
use crate::{
    path_guard::ExecutionScope,
    prompt_profile::PromptProfile,
    remote_control::{
        driver::DriverResult,
        fake::{FakeDriver, FakeIos, WdaCall},
    },
    tool_surface::ToolVariant,
};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The tools this module parses, in the catalog's order. `preview_dialog` is parsed by
/// [`IosCall::dialog`] and has its own tests.
const TOOLS: [&str; 3] = ["screen_app", "screen_device", "screen_clipboard"];

fn object(value: Value) -> JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

fn files() -> CallFiles {
    CallFiles::local(&std::env::temp_dir(), ExecutionScope::Unrestricted)
}

fn parse(tool: &str, input: Value) -> Result<IosCall, String> {
    IosCall::parse(tool, &object(input), &files())
}

fn call(tool: &str, input: Value) -> IosCall {
    parse(tool, input.clone()).unwrap_or_else(|error| panic!("{tool} {input}: {error}"))
}

fn refusal(tool: &str, input: Value) -> String {
    match parse(tool, input.clone()) {
        Ok(call) => panic!("{tool} {input} parsed as {call:?}"),
        Err(error) => error,
    }
}

fn dialog(input: Value) -> IosCall {
    IosCall::dialog(&object(input.clone()))
        .unwrap_or_else(|error| panic!("preview_dialog {input}: {error}"))
}

fn dialog_refusal(input: Value) -> String {
    match IosCall::dialog(&object(input.clone())) {
        Ok(call) => panic!("preview_dialog {input} parsed as {call:?}"),
        Err(error) => error,
    }
}

/// The route as a script names it: `/wda/locked` outside any session, `session:/orientation`
/// inside the driver's.
fn route_key(method: &str, route: WdaRoute<'_>) -> String {
    match route {
        WdaRoute::Root(path) => format!("{method} {path}"),
        WdaRoute::Session(suffix) => format!("{method} session:{suffix}"),
    }
}

fn none() -> DriverResult<Value> {
    Ok(Value::Null)
}

/// What an alert-less device answers to `/alert/text`: WebDriverAgent's own words for it.
fn no_such_alert() -> DriverResult<Value> {
    Err(DriverError::Failed(
        "WebDriverAgent could not do /alert/text: HTTP 404: An attempt was made to operate on a modal dialog when one was not open".to_owned(),
    ))
}

/// A WebDriverAgent that answers by route. A route that was given several answers gives them in
/// turn and then keeps the last; one that was given none is a route the build lacks.
#[derive(Default)]
struct Script {
    replies: Mutex<HashMap<String, VecDeque<DriverResult<Value>>>>,
}

impl Script {
    fn new() -> Self {
        Self::default()
    }

    fn on(self, key: &str, reply: DriverResult<Value>) -> Self {
        self.replies
            .lock()
            .unwrap()
            .entry(key.to_owned())
            .or_default()
            .push_back(reply);
        self
    }

    /// [`Self::on`], replacing the answers the route had.
    fn set(self, key: &str, reply: DriverResult<Value>) -> Self {
        self.replies
            .lock()
            .unwrap()
            .insert(key.to_owned(), VecDeque::from([reply]));
        self
    }

    fn answer(&self, method: &str, route: WdaRoute<'_>) -> DriverResult<Value> {
        let key = route_key(method, route);
        let mut replies = self.replies.lock().unwrap();
        match replies.get_mut(&key) {
            Some(queue) if queue.len() > 1 => queue.pop_front().expect("a queued answer"),
            Some(queue) => queue.front().cloned().expect("an answer"),
            None => Err(DriverError::Unsupported(format!(
                "this WebDriverAgent build does not implement {key}."
            ))),
        }
    }

    fn driver(self) -> Arc<FakeDriver> {
        FakeDriver::with_ios(FakeIos::new(move |method, route, _| {
            self.answer(method, route)
        }))
    }
}

const NEVER: &dyn Fn() -> bool = &|| false;

fn run_within(
    driver: &Arc<FakeDriver>,
    call: &IosCall,
    cancelled: &dyn Fn() -> bool,
    within: Duration,
) -> Result<String, OpError> {
    call.run(driver.as_ref(), &Step::for_tests(cancelled, within))
}

fn run(driver: &Arc<FakeDriver>, call: &IosCall) -> Result<String, OpError> {
    run_within(driver, call, NEVER, Duration::from_secs(25))
}

fn said(result: Result<String, OpError>) -> String {
    match result {
        Err(OpError::Said(message)) => message,
        other => panic!("expected a sentence, got {other:?}"),
    }
}

/// The sentence a refused [`clear_field`] says.
fn refused(result: Result<(), OpError>) -> String {
    match result {
        Err(OpError::Said(message)) => message,
        other => panic!("expected a sentence, got {other:?}"),
    }
}

fn calls(driver: &Arc<FakeDriver>) -> Vec<WdaCall> {
    driver.ios_fake().calls()
}

fn lines(driver: &Arc<FakeDriver>) -> Vec<String> {
    driver.ios_fake().lines()
}

fn posts(driver: &Arc<FakeDriver>) -> Vec<WdaCall> {
    calls(driver)
        .into_iter()
        .filter(|call| call.method == "POST")
        .collect()
}

const S: &str = "/session/fake-session";

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// The actions of `screen_device` that only an Android device has.
const ANDROID_ONLY_ACTIONS: [&str; 6] = [
    "wake",
    "animations",
    "notifications",
    "quick_settings",
    "collapse",
    "stay_awake",
];

/// One valid call of each action, with every argument it needs.
fn valid_call(tool: &str, action: &str) -> Value {
    match (tool, action) {
        ("screen_app", "list" | "foreground") => json!({"action": action}),
        ("screen_app", "info" | "launch" | "activate" | "stop") => {
            json!({"action": action, "app": "com.apple.Preferences"})
        }
        ("screen_app", "open_url") => json!({"action": action, "url": "https://example.com"}),
        ("screen_device", "rotate") => json!({"action": action, "value": "landscape_left"}),
        ("screen_device", "appearance") => json!({"action": action, "value": "dark"}),
        ("screen_device", "location") => {
            json!({"action": action, "latitude": 52.52, "longitude": 13.405})
        }
        ("screen_device", _) => json!({"action": action}),
        ("screen_clipboard", "write") => json!({"action": action, "text": "hello"}),
        ("screen_clipboard", _) => json!({"action": action}),
        _ => panic!("{tool} {action}"),
    }
}

fn schema_of(tool: &str) -> Value {
    crate::builtin_schemas::builtin_tool_schema(
        tool,
        ToolVariant::Standard,
        &PromptProfile::builtin(),
    )
    .unwrap_or_else(|| panic!("{tool} has no schema"))
}

/// The arguments that make a call valid apart from `parameter`, the one a bounds test varies.
fn context(tool: &str, parameter: &str) -> Value {
    match (tool, parameter) {
        ("screen_app", "app") => json!({"action": "launch"}),
        ("screen_app", "url") => json!({"action": "open_url"}),
        ("screen_app", "filter" | "limit") => json!({"action": "list"}),
        ("screen_app", "restart" | "arguments" | "environment") => {
            json!({"action": "launch", "app": "com.apple.Preferences"})
        }
        ("screen_device", "value") => json!({"action": "rotate"}),
        ("screen_device", "sections") => json!({"action": "info"}),
        ("screen_device", "latitude") => json!({"action": "location", "longitude": 0}),
        ("screen_device", "longitude") => json!({"action": "location", "latitude": 0}),
        ("screen_clipboard", "text") => json!({"action": "write"}),
        _ => panic!("no context for {tool}.{parameter}"),
    }
}

/// A string of `length` characters that every string parameter takes: a multibyte one where
/// the parameter allows text, so a bound counted in bytes would show.
fn text_of(parameter: &str, length: usize) -> String {
    if parameter == "app" {
        "a".repeat(length)
    } else {
        "\u{e9}".repeat(length)
    }
}

/// Every parameter of every iOS tool is accepted with exactly the bounds its schema states: the
/// edges parse, one past them does not. The schema is the one the model is given, so a bound
/// changed on either side fails here.
#[test]
fn parse_takes_the_schemas_bounds() {
    for tool in TOOLS {
        let schema = schema_of(tool);
        assert_eq!(schema["additionalProperties"], json!(false), "{tool}");
        let properties = schema["properties"].as_object().unwrap();

        // The same parameters, and the same required ones.
        let mut ours: Vec<&str> = params(tool).iter().map(|param| param.name).collect();
        ours.push("serverId");
        ours.push("action");
        ours.sort_unstable();
        let mut theirs: Vec<&str> = properties.keys().map(String::as_str).collect();
        theirs.sort_unstable();
        assert_eq!(ours, theirs, "{tool}");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            required,
            if action_required(tool) {
                vec!["action"]
            } else {
                vec![]
            },
            "{tool}"
        );

        for (name, property) in properties {
            let at = format!("{tool}.{name}");
            match (name.as_str(), property["type"].as_str().unwrap()) {
                // The tool layer's: let through, and its bounds are checked there.
                ("serverId", _) => {}
                ("action", "string") => {
                    let values: Vec<&str> = property["enum"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .collect();
                    assert_eq!(values, actions(tool).unwrap(), "{at}");
                    for action in &values {
                        let input = valid_call(tool, action);
                        let parsed = parse(tool, input.clone());
                        if tool == "screen_device" && ANDROID_ONLY_ACTIONS.contains(action) {
                            assert!(parsed.is_err(), "{at} {input} is Android's");
                        } else {
                            parsed.unwrap_or_else(|error| panic!("{at} {input}: {error}"));
                        }
                    }
                    refusal(tool, json!({"action": "erase"}));
                    // Only the clipboard's action is required; the others have a default, which
                    // is the schema's.
                    assert_eq!(
                        property["default"].as_str(),
                        (!action_required(tool)).then_some(values[0]),
                        "{at}"
                    );
                    if action_required(tool) {
                        refusal(tool, json!({}));
                    } else {
                        call(tool, json!({}));
                    }
                }
                ("value", "string") => {
                    // Its values are the few the description lists; its length is the schema's.
                    assert_eq!(property["minLength"], json!(1), "{at}");
                    assert_eq!(property["maxLength"], json!(VALUE_MAX), "{at}");
                    for value in ORIENTATIONS {
                        call(tool, json!({"action": "rotate", "value": value}));
                    }
                    for value in ["light", "dark"] {
                        call(tool, json!({"action": "appearance", "value": value}));
                    }
                    let long = refusal(tool, json!({"action": "rotate", "value": "a".repeat(33)}));
                    assert_eq!(long, "\"value\" must be 1 to 32 characters.");
                    refusal(tool, json!({"action": "rotate", "value": ""}));
                    refusal(tool, json!({"action": "rotate", "value": "sideways"}));
                    refusal(tool, json!({"action": "appearance", "value": "auto"}));
                }
                // Android's own: whatever its value, an iPhone refuses it by name; a `null` is no
                // argument.
                ("scope" | "activity", "string") => {
                    let (mut input, value) = if name == "scope" {
                        (json!({"action": "list"}), json!("all"))
                    } else {
                        (
                            json!({"action": "launch", "app": "a.b"}),
                            json!(".MainActivity"),
                        )
                    };
                    input[name.as_str()] = value;
                    let refused = refusal(tool, input.clone());
                    assert!(
                        refused.starts_with(&format!(
                            "\"{name}\" is Android-only, and this screen is an iPhone or iPad: "
                        )),
                        "{at}: {refused}"
                    );
                    input[name.as_str()] = json!(null);
                    call(tool, input);
                }
                ("limit", "integer") => {
                    let min = property["minimum"].as_u64().unwrap();
                    let max = property["maximum"].as_u64().unwrap();
                    assert_eq!((min, max), (LIMIT_MIN, LIMIT_MAX), "{at}");
                    assert_eq!(property["default"], json!(LIMIT_DEFAULT), "{at}");
                    let with = |value: Value| {
                        let mut input = context(tool, name);
                        input[name.as_str()] = value;
                        parse(tool, input)
                    };
                    assert!(with(json!(min)).is_ok(), "{at}");
                    assert!(with(json!(max)).is_ok(), "{at}");
                    assert!(with(json!(min - 1)).is_err(), "{at}");
                    assert!(with(json!(max + 1)).is_err(), "{at}");
                    assert!(with(json!(1.5)).is_err(), "{at}");
                    assert!(with(json!("5")).is_err(), "{at}");
                    assert!(with(json!(null)).is_ok(), "{at}");
                }
                ("restart", "boolean") => {
                    let with = |value: Value| {
                        let mut input = context(tool, name);
                        input[name.as_str()] = value;
                        parse(tool, input)
                    };
                    assert!(with(json!(true)).is_ok(), "{at}");
                    assert!(with(json!(false)).is_ok(), "{at}");
                    assert!(with(json!(null)).is_ok(), "{at}");
                    assert!(with(json!("yes")).is_err(), "{at}");
                    assert!(with(json!(1)).is_err(), "{at}");
                }
                ("sections", "array") => {
                    assert_eq!(property["minItems"], json!(1), "{at}");
                    assert_eq!(property["maxItems"], json!(SECTIONS.len()), "{at}");
                    assert_eq!(property["uniqueItems"], json!(true), "{at}");
                    let items: Vec<&str> = property["items"]["enum"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .collect();
                    assert_eq!(items, SECTIONS, "{at}");
                    let with = |value: Value| {
                        let mut input = context(tool, name);
                        input[name.as_str()] = value;
                        parse(tool, input)
                    };
                    assert!(with(json!(["device"])).is_ok(), "{at}");
                    assert!(with(json!(["power", "display", "device"])).is_ok(), "{at}");
                    // The fourth is Android's alone.
                    assert!(with(json!(["notifications"])).is_err(), "{at}");
                    assert!(
                        with(json!(["device", "display", "power", "notifications"])).is_err(),
                        "{at}"
                    );
                    assert!(with(json!([])).is_err(), "{at}");
                    assert!(with(json!(["device", "device"])).is_err(), "{at}");
                    assert!(
                        with(json!(["device", "display", "power", "device", "x"])).is_err(),
                        "{at}"
                    );
                    assert!(with(json!(["sound"])).is_err(), "{at}");
                    assert!(with(json!([1])).is_err(), "{at}");
                    assert!(with(json!("device")).is_err(), "{at}");
                    assert!(with(json!(null)).is_ok(), "{at}");
                }
                (_, "string") => {
                    let min = property
                        .get("minLength")
                        .map_or(0, |v| v.as_u64().unwrap() as usize);
                    let max = property["maxLength"].as_u64().unwrap() as usize;
                    let base = context(tool, name);
                    let with = |value: Value| {
                        let mut input = base.clone();
                        input[name.as_str()] = value;
                        parse(tool, input)
                    };
                    assert!(
                        with(json!(text_of(name, max))).is_ok(),
                        "{at} at its maximum"
                    );
                    assert!(
                        with(json!(text_of(name, max + 1))).is_err(),
                        "{at} past its maximum"
                    );
                    assert!(
                        with(json!(text_of(name, min.max(1)))).is_ok(),
                        "{at} at its minimum"
                    );
                    assert_eq!(with(json!("")).is_ok(), min == 0, "{at} when empty");
                    assert!(with(json!(7)).is_err(), "{at} as a number");
                    // A `null` is no argument at all.
                    assert_eq!(
                        with(json!(null)).is_ok(),
                        parse(tool, base.clone()).is_ok(),
                        "{at} as null"
                    );
                }
                ("arguments", "array") => {
                    let max_items = property["maxItems"].as_u64().unwrap() as usize;
                    let item = &property["items"];
                    assert_eq!(item["type"], json!("string"), "{at}");
                    assert!(item.get("minLength").is_none(), "{at}");
                    let max_length = item["maxLength"].as_u64().unwrap() as usize;
                    let base = context(tool, name);
                    let with = |value: Value| {
                        let mut input = base.clone();
                        input["arguments"] = value;
                        parse(tool, input)
                    };
                    assert!(with(json!(vec!["-x"; max_items])).is_ok(), "{at}");
                    assert!(with(json!(vec!["-x"; max_items + 1])).is_err(), "{at}");
                    assert!(with(json!(["a".repeat(max_length)])).is_ok(), "{at}");
                    assert!(with(json!(["a".repeat(max_length + 1)])).is_err(), "{at}");
                    assert!(with(json!([""])).is_ok(), "{at}");
                    assert!(with(json!([1])).is_err(), "{at}");
                    assert!(with(json!("-x")).is_err(), "{at}");
                }
                ("environment", "object") => {
                    let max_properties = property["maxProperties"].as_u64().unwrap() as usize;
                    let values = &property["additionalProperties"];
                    assert_eq!(values["type"], json!("string"), "{at}");
                    let max_length = values["maxLength"].as_u64().unwrap() as usize;
                    let base = context(tool, name);
                    let with = |value: Value| {
                        let mut input = base.clone();
                        input["environment"] = value;
                        parse(tool, input)
                    };
                    let variables = |count: usize| {
                        Value::Object(
                            (0..count)
                                .map(|number| (format!("V{number}"), json!("1")))
                                .collect(),
                        )
                    };
                    assert!(with(variables(max_properties)).is_ok(), "{at}");
                    assert!(with(variables(max_properties + 1)).is_err(), "{at}");
                    assert!(with(json!({"A": "a".repeat(max_length)})).is_ok(), "{at}");
                    assert!(
                        with(json!({"A": "a".repeat(max_length + 1)})).is_err(),
                        "{at}"
                    );
                    assert!(with(json!({"A": 1})).is_err(), "{at}");
                    assert!(with(json!(["A=1"])).is_err(), "{at}");
                }
                ("latitude" | "longitude", "number") => {
                    let min = property["minimum"].as_f64().unwrap();
                    let max = property["maximum"].as_f64().unwrap();
                    let base = context(tool, name);
                    let with = |value: Value| {
                        let mut input = base.clone();
                        input[name.as_str()] = value;
                        parse(tool, input)
                    };
                    assert!(with(json!(min)).is_ok(), "{at}");
                    assert!(with(json!(max)).is_ok(), "{at}");
                    assert!(with(json!(0.5)).is_ok(), "{at}");
                    assert!(with(json!(max + 0.000001)).is_err(), "{at}");
                    assert!(with(json!(min - 0.000001)).is_err(), "{at}");
                    assert!(with(json!(max + 1.0)).is_err(), "{at}");
                    assert!(with(json!("1")).is_err(), "{at}");
                }
                other => panic!("{at} is a {other:?}, which the bounds test does not know"),
            }
        }
    }
}

/// `preview_dialog`'s parameters, as its schema states them (the page's side takes the same
/// ones): a button, an `accept`, and the text for a prompt.
#[test]
fn a_dialog_takes_the_schemas_bounds() {
    let schema = schema_of("preview_dialog");
    assert_eq!(schema["additionalProperties"], json!(false));
    let properties = schema["properties"].as_object().unwrap();
    let mut ours: Vec<&str> = params("preview_dialog")
        .iter()
        .map(|param| param.name)
        .collect();
    ours.push("serverId");
    ours.sort_unstable();
    let mut theirs: Vec<&str> = properties.keys().map(String::as_str).collect();
    theirs.sort_unstable();
    assert_eq!(ours, theirs);
    assert_eq!(schema["required"], json!([]));

    assert_eq!(properties["button"]["minLength"], json!(1));
    assert_eq!(properties["button"]["maxLength"], json!(BUTTON_MAX));
    // No `minLength`: an empty answer is what a prompt's own default is.
    assert!(properties["prompt_text"].get("minLength").is_none());
    assert_eq!(
        properties["prompt_text"]["maxLength"],
        json!(PROMPT_TEXT_MAX)
    );
    assert_eq!(properties["accept"]["type"], json!("boolean"));

    // The edges parse, one past them does not; characters are counted, not bytes.
    dialog(json!({"button": "\u{e9}".repeat(BUTTON_MAX)}));
    assert_eq!(
        dialog_refusal(json!({"button": "\u{e9}".repeat(BUTTON_MAX + 1)})),
        "\"button\" must be 1 to 256 characters."
    );
    assert_eq!(
        dialog_refusal(json!({"button": ""})),
        "\"button\" must be 1 to 256 characters."
    );
    dialog(json!({"prompt_text": "\u{e9}".repeat(PROMPT_TEXT_TYPABLE)}));
    assert_eq!(
        dialog_refusal(json!({"prompt_text": "\u{e9}".repeat(PROMPT_TEXT_MAX + 1)})),
        "\"prompt_text\" must be at most 4096 characters."
    );
    // An empty prompt text is a text (a field cleared), not an absent one.
    dialog(json!({"prompt_text": ""}));
    dialog(json!({"prompt_text": "", "accept": true}));
    assert_eq!(
        dialog_refusal(json!({"button": 5})),
        "\"button\" must be a string."
    );
    assert_eq!(
        dialog_refusal(json!({"prompt_text": 5})),
        "\"prompt_text\" must be a string."
    );
    assert_eq!(
        dialog_refusal(json!({"accept": "yes"})),
        "\"accept\" must be true or false."
    );
    assert_eq!(
        dialog_refusal(json!({"accept": 1})),
        "\"accept\" must be true or false."
    );
}

#[test]
fn a_call_the_tool_does_not_have_is_refused_naming_what_it_has() {
    // `serverId` is the tool layer's and is let through; so is a `null`.
    for tool in TOOLS {
        let mut input = valid_call(tool, actions(tool).unwrap()[0]);
        input["serverId"] = json!("phone");
        parse(tool, input).unwrap_or_else(|error| panic!("{tool}: {error}"));
    }
    let mut input = json!({"button": "OK"});
    input["serverId"] = json!("phone");
    dialog(input);

    assert_eq!(
        refusal("screen_app", json!({"action": "launch", "app": "a.b", "workspace": 1})),
        "screen_app has no parameter \"workspace\"; it takes serverId, action, app, url, filter, limit, scope, activity, restart, arguments and environment."
    );
    assert_eq!(
        refusal("screen_app", json!({"bundle_id": "a.b"})),
        "screen_app has no parameter \"bundle_id\"; it takes serverId, action, app, url, filter, limit, scope, activity, restart, arguments and environment."
    );
    assert_eq!(
        refusal("screen_device", json!({"action": "lock", "x": 1})),
        "screen_device has no parameter \"x\"; it takes serverId, action, value, sections, latitude and longitude."
    );
    assert_eq!(
        refusal(
            "screen_clipboard",
            json!({"action": "read", "content": "x"})
        ),
        "screen_clipboard has no parameter \"content\"; it takes serverId, action and text."
    );
    assert_eq!(
        dialog_refusal(json!({"button": "OK", "label": "OK"})),
        "preview_dialog has no parameter \"label\"; it takes serverId, accept, prompt_text and button."
    );
    // The key is a stranger's text: it is cleaned before it reaches a sentence.
    let odd = refusal("screen_device", json!({"a\nb\"c": 1}));
    assert!(!odd.contains('\n'), "{odd}");
    assert!(
        odd.starts_with("screen_device has no parameter \"a"),
        "{odd}"
    );
    // The tools this module does not parse are not iOS tools.
    for other in [
        "ios_shell",
        "screen_package",
        "android_shell",
        "preview_dialog",
    ] {
        assert_eq!(
            IosCall::parse(other, &object(json!({})), &files()).unwrap_err(),
            format!("{other} is not an iOS tool.")
        );
    }
}

#[test]
fn what_only_android_has_is_refused_by_name_before_anything_is_sent() {
    // A parameter of Android's: which one, what this screen is, what it does instead.
    assert_eq!(
        refusal("screen_app", json!({"scope": "third_party"})),
        "\"scope\" is Android-only, and this screen is an iPhone or iPad: WebDriverAgent lists only the apps running now, whatever the scope. Leave it out."
    );
    assert_eq!(
        refusal(
            "screen_app",
            json!({"action": "launch", "app": "a.b", "activity": ".Main"})
        ),
        "\"activity\" is Android-only, and this screen is an iPhone or iPad: an app starts from its bundle id alone. Leave it out."
    );
    // ... whichever action it comes with.
    assert!(refusal(
        "screen_app",
        json!({"action": "stop", "app": "a.b", "scope": "all"})
    )
    .starts_with("\"scope\" is Android-only"));

    // An action of Android's.
    assert_eq!(
        refusal("screen_device", json!({"action": "wake"})),
        "An iPhone or iPad has no wake on its own; unlock wakes it and swipes the lock screen away (only without a passcode)."
    );
    for (action, instead) in [
        ("animations", "preview_wait waits for an animation to end."),
        ("stay_awake", "Unlock it again when it sleeps."),
        (
            "notifications",
            "Swipe down from the top edge of the screen with preview_input.",
        ),
        (
            "quick_settings",
            "Swipe down from the top-right corner with preview_input",
        ),
        (
            "collapse",
            "Swipe up from the bottom edge with preview_input",
        ),
    ] {
        let refused = refusal("screen_device", json!({"action": action}));
        assert!(
            refused.starts_with(&format!(
                "screen_device {action} is Android-only, and this screen is an iPhone or iPad: "
            )),
            "{action}: {refused}"
        );
        assert!(refused.contains(instead), "{action}: {refused}");
        // Whatever it comes with, the action is what is refused.
        let with_value = refusal("screen_device", json!({"action": action, "value": "on"}));
        assert_eq!(with_value, refused, "{action}");
    }

    // A value of Android's.
    assert_eq!(
        refusal("screen_device", json!({"action": "rotate", "value": "auto"})),
        "\"auto\" is Android-only, and this screen is an iPhone or iPad: it has no auto-rotate for WebDriverAgent to switch on. Give portrait, landscape_left, landscape_right or portrait_upside_down."
    );
    assert_eq!(
        refusal("screen_device", json!({"action": "appearance", "value": "auto"})),
        "\"auto\" is Android-only, and this screen is an iPhone or iPad: its appearance is light or dark, and WebDriverAgent cannot set it to follow the time of day. Give light or dark."
    );
    // The old word for a landscape that does not say which way.
    let landscape = refusal(
        "screen_device",
        json!({"action": "rotate", "value": "landscape"}),
    );
    assert!(
        landscape.starts_with("\"landscape\" does not say which way the device is turned."),
        "{landscape}"
    );
    assert!(landscape.contains("landscape_left") && landscape.contains("landscape_right"));

    // A section of Android's.
    assert_eq!(
        refusal(
            "screen_device",
            json!({"action": "info", "sections": ["power", "notifications"]})
        ),
        "\"notifications\" is Android-only, and this screen is an iPhone or iPad: WebDriverAgent cannot read its notifications. Ask for device, display or power."
    );
}

#[test]
fn an_argument_for_another_action_is_refused_naming_the_action() {
    for (tool, input, expected) in [
        (
            "screen_app",
            json!({"action": "launch", "app": "a.b", "url": "x://y"}),
            "\"url\" does not apply to screen_app launch; it is for open_url.",
        ),
        (
            "screen_app",
            json!({"action": "stop", "app": "a.b", "arguments": ["-x"]}),
            "\"arguments\" does not apply to screen_app stop; it is for launch.",
        ),
        (
            "screen_app",
            json!({"action": "open_url", "url": "x://y", "environment": {"A": "1"}}),
            "\"environment\" does not apply to screen_app open_url; it is for launch.",
        ),
        (
            "screen_app",
            json!({"action": "activate", "app": "a.b", "restart": true}),
            "\"restart\" does not apply to screen_app activate; it is for launch.",
        ),
        (
            "screen_app",
            json!({"action": "launch", "app": "a.b", "filter": "x"}),
            "\"filter\" does not apply to screen_app launch; it is for list.",
        ),
        (
            "screen_app",
            json!({"action": "foreground", "limit": 5}),
            "\"limit\" does not apply to screen_app foreground; it is for list.",
        ),
        (
            "screen_app",
            json!({"action": "list", "app": "a.b"}),
            "\"app\" does not apply to screen_app list; it is for info, launch, activate, stop and open_url.",
        ),
        (
            "screen_app",
            json!({"app": "a.b"}),
            "\"app\" does not apply to screen_app list; it is for info, launch, activate, stop and open_url.",
        ),
        (
            "screen_device",
            json!({"action": "lock", "value": "dark"}),
            "\"value\" does not apply to screen_device lock; it is for rotate and appearance.",
        ),
        (
            "screen_device",
            json!({"action": "rotate", "value": "portrait", "latitude": 1}),
            "\"latitude\" does not apply to screen_device rotate; it is for location.",
        ),
        (
            "screen_device",
            json!({"action": "clear_location", "longitude": 1}),
            "\"longitude\" does not apply to screen_device clear_location; it is for location.",
        ),
        (
            "screen_device",
            json!({"action": "lock", "sections": ["power"]}),
            "\"sections\" does not apply to screen_device lock; it is for info.",
        ),
        (
            "screen_clipboard",
            json!({"action": "read", "text": "x"}),
            "\"text\" does not apply to screen_clipboard read; it is for write.",
        ),
    ] {
        assert_eq!(refusal(tool, input), expected);
    }
    // A `null` is not an argument.
    call("screen_clipboard", json!({"action": "read", "text": null}));
    call(
        "screen_app",
        json!({"action": "stop", "app": "a.b", "url": null}),
    );
    call("screen_app", json!({"action": "list", "app": null}));
}

#[test]
fn a_missing_argument_is_one_clear_sentence() {
    for (tool, input, expected) in [
        (
            "screen_app",
            json!({"action": "launch"}),
            "screen_app launch needs \"app\", the app's bundle id such as com.apple.Preferences.",
        ),
        (
            "screen_app",
            json!({"action": "activate"}),
            "screen_app activate needs \"app\", the app's bundle id such as com.apple.Preferences.",
        ),
        (
            "screen_app",
            json!({"action": "stop"}),
            "screen_app stop needs \"app\", the app's bundle id such as com.apple.Preferences.",
        ),
        (
            "screen_app",
            json!({"action": "info"}),
            "screen_app info needs \"app\", the app's bundle id such as com.apple.Preferences.",
        ),
        (
            "screen_app",
            json!({"action": "open_url"}),
            "screen_app open_url needs \"url\", the URL or deep link to open.",
        ),
        (
            "screen_app",
            json!({"action": "terminate"}),
            "\"action\" must be list, foreground, info, launch, activate, stop or open_url, not \"terminate\".",
        ),
        (
            "screen_device",
            json!({"action": "rotate"}),
            "screen_device rotate needs \"value\": portrait, landscape_left, landscape_right or portrait_upside_down.",
        ),
        (
            "screen_device",
            json!({"action": "appearance"}),
            "screen_device appearance needs \"value\": light or dark.",
        ),
        (
            "screen_device",
            json!({"action": "location"}),
            "screen_device location needs both \"latitude\" and \"longitude\", in degrees.",
        ),
        (
            "screen_device",
            json!({"action": "location", "latitude": 1}),
            "screen_device location needs both \"latitude\" and \"longitude\", in degrees.",
        ),
        (
            "screen_device",
            json!({"action": "reboot"}),
            "\"action\" must be info, lock, unlock, wake, rotate, appearance, animations, location, clear_location, notifications, quick_settings, collapse, stay_awake or dismiss_keyboard, not \"reboot\".",
        ),
        (
            "screen_clipboard",
            json!({"action": "write"}),
            "screen_clipboard write needs \"text\", the text to put on the clipboard (empty to clear it).",
        ),
        (
            "screen_clipboard",
            json!({}),
            "\"action\" is required: read or write.",
        ),
        (
            "screen_clipboard",
            json!({"action": "tap"}),
            "\"action\" must be read or write, not \"tap\".",
        ),
    ] {
        assert_eq!(refusal(tool, input), expected);
    }
    // The action the schema defaults: `screen_app` lists, `screen_device` reports.
    assert_eq!(
        call("screen_app", json!({})),
        call("screen_app", json!({"action": "list"}))
    );
    assert_eq!(
        call("screen_device", json!({})),
        call("screen_device", json!({"action": "info"}))
    );
    assert_eq!(
        call("screen_app", json!({"action": null})),
        call("screen_app", json!({"action": "list"}))
    );
}

#[test]
fn a_dialog_needs_an_answer_or_a_text_and_not_two_answers() {
    assert_eq!(
        dialog_refusal(json!({})),
        "preview_dialog needs button (the exact label of the button to press), accept (true or false) or prompt_text (to type into the alert's field)."
    );
    // A `null` is no argument.
    assert_eq!(
        dialog_refusal(json!({"button": null, "accept": null, "prompt_text": null})),
        dialog_refusal(json!({}))
    );
    assert_eq!(
        dialog_refusal(json!({"button": "OK", "accept": true})),
        "preview_dialog takes button or accept, not both: button presses the button with exactly that label, accept guesses one by position."
    );
    assert_eq!(
        dialog_refusal(json!({"button": "OK", "accept": false})),
        "preview_dialog takes button or accept, not both: button presses the button with exactly that label, accept guesses one by position."
    );
    // Dismissing does not use text.
    assert!(dialog_refusal(json!({"prompt_text": "x", "accept": false}))
        .starts_with("\"prompt_text\" does not apply to preview_dialog with accept false"));
    // The rest are one call each.
    for input in [
        json!({"button": "OK"}),
        json!({"accept": true}),
        json!({"accept": false}),
        json!({"prompt_text": "1234"}),
        json!({"prompt_text": "1234", "accept": true}),
        json!({"prompt_text": "1234", "button": "OK"}),
    ] {
        dialog(input);
    }
    assert_eq!(
        dialog(json!({"prompt_text": "1234", "button": "OK"})),
        IosCall {
            op: Op::Dialog(Dialog {
                prompt_text: Some("1234".to_owned()),
                answer: Some(Answer::Button("OK".to_owned())),
            })
        }
    );
}

#[test]
fn app_ids_and_variable_names_are_checked_before_they_travel() {
    for good in [
        "com.apple.Preferences",
        "a",
        "A-b.c-1",
        "com.example.my-app",
    ] {
        call("screen_app", json!({"action": "launch", "app": good}));
        call("screen_app", json!({"action": "info", "app": good}));
    }
    for bad in [
        "com apple",
        "com.apple/../x",
        "com.apple\n",
        "com.apple;rm",
        "com_apple",
        "\u{e9}",
        "a b",
        "\"quoted\"",
        "a?b",
    ] {
        let error = refusal("screen_app", json!({"action": "stop", "app": bad}));
        assert_eq!(
            error,
            "\"app\" must be a bundle id such as com.apple.Preferences: letters, digits, dots and hyphens.",
            "{bad:?}"
        );
        refusal("screen_app", json!({"action": "info", "app": bad}));
        // The app an open_url opens the URL in is held to the same rule.
        refusal(
            "screen_app",
            json!({"action": "open_url", "url": "x://y", "app": bad}),
        );
    }
    for good in ["A", "_x", "HOME", "a1_B2", "_"] {
        call(
            "screen_app",
            json!({"action": "launch", "app": "a.b", "environment": {good: "v"}}),
        );
    }
    for bad in ["", "1A", "A-B", "A B", "A=B", "\u{e9}", "A.B", "A\n"] {
        let error = refusal(
            "screen_app",
            json!({"action": "launch", "app": "a.b", "environment": {bad: "v"}}),
        );
        assert!(
            error.starts_with("The environment variable name \""),
            "{bad:?}: {error}"
        );
        assert!(!error.contains('\n'), "{error}");
    }
}

#[test]
fn bad_types_and_sizes_say_which_argument_and_what_it_takes() {
    assert_eq!(
        refusal("screen_app", json!({"action": "open_url", "url": 5})),
        "\"url\" must be a string."
    );
    assert_eq!(
        refusal("screen_app", json!({"action": "open_url", "url": ""})),
        "\"url\" must be 1 to 2048 characters."
    );
    assert_eq!(
        refusal("screen_app", json!({"filter": ""})),
        "\"filter\" must be 1 to 200 characters."
    );
    assert_eq!(
        refusal("screen_app", json!({"limit": 0})),
        "\"limit\" must be a whole number from 1 to 1000."
    );
    assert_eq!(
        refusal("screen_app", json!({"limit": 1001})),
        "\"limit\" must be a whole number from 1 to 1000."
    );
    assert_eq!(
        refusal("screen_app", json!({"limit": "ten"})),
        "\"limit\" must be a whole number from 1 to 1000."
    );
    assert_eq!(
        refusal(
            "screen_app",
            json!({"action": "launch", "app": "a.b", "restart": "yes"})
        ),
        "\"restart\" must be true or false."
    );
    assert_eq!(
        refusal(
            "screen_clipboard",
            json!({"action": "write", "text": "a".repeat(100_001)})
        ),
        "\"text\" must be at most 100000 characters."
    );
    assert_eq!(
        refusal(
            "screen_device",
            json!({"action": "location", "latitude": 91, "longitude": 0})
        ),
        "\"latitude\" must be a number from -90 to 90."
    );
    assert_eq!(
        refusal(
            "screen_device",
            json!({"action": "location", "latitude": 0, "longitude": -180.5})
        ),
        "\"longitude\" must be a number from -180 to 180."
    );
    assert_eq!(
        refusal(
            "screen_device",
            json!({"action": "rotate", "value": "sideways"})
        ),
        "\"value\" for screen_device rotate must be portrait, landscape_left, landscape_right or portrait_upside_down, not \"sideways\"."
    );
    assert_eq!(
        refusal(
            "screen_device",
            json!({"action": "appearance", "value": "sepia"})
        ),
        "\"value\" for screen_device appearance must be light or dark, not \"sepia\"."
    );
    assert_eq!(
        refusal("screen_device", json!({"sections": []})),
        "\"sections\" must be a list of 1 to 4 of device, display, power and notifications, each at most once."
    );
    assert_eq!(
        refusal("screen_device", json!({"sections": ["power", "power"]})),
        "\"sections\" must be a list of 1 to 4 of device, display, power and notifications, each at most once."
    );
    assert!(refusal(
        "screen_app",
        json!({"action": "launch", "app": "a.b", "arguments": "-x"})
    )
    .starts_with("\"arguments\" must be an array of at most 32 strings"));
    assert!(refusal(
        "screen_app",
        json!({"action": "launch", "app": "a.b", "environment": [1]})
    )
    .starts_with("\"environment\" must be an object of at most 32 variables"));
    // What is allowed is allowed in any case.
    assert_eq!(
        call(
            "screen_device",
            json!({"action": "rotate", "value": "Landscape_Left"})
        ),
        call(
            "screen_device",
            json!({"action": "rotate", "value": "landscape_left"})
        )
    );
    call(
        "screen_device",
        json!({"action": "appearance", "value": " dark "}),
    );
    // A whole number is a whole number whichever way JSON wrote it.
    call("screen_app", json!({"limit": 200.0}));
}

#[test]
fn each_call_declares_its_effect_its_time_and_what_it_does() {
    for (tool, input, effect, what) in [
        (
            "screen_app",
            json!({}),
            Effect::Look,
            "listing the running apps",
        ),
        (
            "screen_app",
            json!({"action": "foreground"}),
            Effect::Look,
            "reading the app in front",
        ),
        (
            "screen_app",
            json!({"action": "info", "app": "a.b"}),
            Effect::Look,
            "reading the state of a.b",
        ),
        (
            "screen_app",
            json!({"action": "launch", "app": "com.apple.Preferences"}),
            Effect::Act,
            "launching com.apple.Preferences",
        ),
        (
            "screen_app",
            json!({"action": "launch", "app": "a.b", "restart": true}),
            Effect::Act,
            "restarting a.b",
        ),
        (
            "screen_app",
            json!({"action": "activate", "app": "a.b"}),
            Effect::Act,
            "bringing a.b to the front",
        ),
        (
            "screen_app",
            json!({"action": "stop", "app": "a.b"}),
            Effect::Act,
            "quitting a.b",
        ),
        (
            "screen_app",
            json!({"action": "open_url", "url": "myapp://secret-token"}),
            Effect::Act,
            "opening a URL",
        ),
        (
            "screen_device",
            json!({}),
            Effect::Look,
            "reading the device's state",
        ),
        (
            "screen_device",
            json!({"action": "info", "sections": ["power"]}),
            Effect::Look,
            "reading the device's state",
        ),
        (
            "screen_device",
            json!({"action": "lock"}),
            Effect::Act,
            "locking the device",
        ),
        (
            "screen_device",
            json!({"action": "unlock"}),
            Effect::Act,
            "unlocking the device",
        ),
        (
            "screen_device",
            json!({"action": "rotate", "value": "portrait"}),
            Effect::Act,
            "rotating the device",
        ),
        (
            "screen_device",
            json!({"action": "appearance", "value": "light"}),
            Effect::Act,
            "changing the appearance",
        ),
        (
            "screen_device",
            json!({"action": "dismiss_keyboard"}),
            Effect::Act,
            "dismissing the keyboard",
        ),
        (
            "screen_device",
            json!({"action": "location", "latitude": 1, "longitude": 2}),
            Effect::Act,
            "setting the simulated location",
        ),
        (
            "screen_device",
            json!({"action": "clear_location"}),
            Effect::Act,
            "clearing the simulated location",
        ),
        (
            "screen_clipboard",
            json!({"action": "read"}),
            Effect::Look,
            "reading the clipboard",
        ),
        (
            "screen_clipboard",
            json!({"action": "write", "text": "secret"}),
            // The input lock is held, so a write never lands between another caller's write and
            // their paste; the read stays a look.
            Effect::Act,
            "writing the clipboard",
        ),
    ] {
        let parsed = call(tool, input.clone());
        assert_eq!(parsed.effect(), effect, "{tool} {input}");
        assert_eq!(parsed.what(), what, "{tool} {input}");
        assert_eq!(parsed.time(), SCREEN_CALL_TIME, "{tool} {input}");
    }
    for (input, what) in [
        (json!({"button": "OK"}), "answering the alert"),
        (json!({"accept": true}), "answering the alert"),
        (json!({"accept": false}), "answering the alert"),
        (
            json!({"prompt_text": "1", "accept": true}),
            "answering the alert",
        ),
        // Only typing answers nothing.
        (json!({"prompt_text": "1234"}), "typing into the alert"),
    ] {
        // Answering a dialog changes what the screen shows: an act, never a look.
        let parsed = dialog(input.clone());
        assert_eq!(parsed.effect(), Effect::Act, "{input}");
        assert_eq!(parsed.what(), what, "{input}");
        assert_eq!(parsed.time(), SCREEN_CALL_TIME, "{input}");
    }
}

// ---------------------------------------------------------------------------------------------
// Requests and replies
// ---------------------------------------------------------------------------------------------

#[test]
fn requests_are_built_as_webdriveragent_reads_them() {
    assert_eq!(
        launch_body("com.apple.Preferences", &[], &[]),
        json!({"bundleId": "com.apple.Preferences"})
    );
    assert_eq!(
        launch_body(
            "a.b",
            &["-AppleLanguages".to_owned(), "(de)".to_owned()],
            &[("MODE".to_owned(), "test".to_owned())]
        ),
        json!({
            "bundleId": "a.b",
            "arguments": ["-AppleLanguages", "(de)"],
            "environment": {"MODE": "test"}
        })
    );
    assert_eq!(bundle_body("a.b"), json!({"bundleId": "a.b"}));
    assert_eq!(
        url_body("https://x.y/z?q=1", None),
        json!({"url": "https://x.y/z?q=1"})
    );
    assert_eq!(
        url_body("myapp://open", Some("com.example.app")),
        json!({"url": "myapp://open", "bundleId": "com.example.app"})
    );
    assert_eq!(
        orientation_body(Orientation::Portrait),
        json!({"orientation": "PORTRAIT"})
    );
    // WebDriverAgent's `LANDSCAPE` is `UIDeviceOrientationLandscapeLeft`; see [`Orientation`].
    assert_eq!(
        orientation_body(Orientation::LandscapeLeft),
        json!({"orientation": "LANDSCAPE"})
    );
    assert_eq!(
        orientation_body(Orientation::LandscapeRight),
        json!({"orientation": "UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT"})
    );
    assert_eq!(
        orientation_body(Orientation::PortraitUpsideDown),
        json!({"orientation": "UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN"})
    );
    assert_eq!(appearance_body(Appearance::Dark), json!({"name": "dark"}));
    assert_eq!(appearance_body(Appearance::Light), json!({"name": "light"}));
    assert_eq!(
        keyboard_body(),
        json!({"keyNames": ["Done", "done", "Return", "return", "Search", "Go", "Hide keyboard"]})
    );
    assert_eq!(
        location_body(52.52, -13.405),
        json!({"latitude": 52.52, "longitude": -13.405})
    );
    assert_eq!(alert_name_body("Allow"), json!({"name": "Allow"}));
    assert_eq!(alert_text_body("1234"), json!({"value": "1234"}));
    assert_eq!(
        pasteboard_write_body("h\u{e9}llo \u{4e2d}"),
        json!({"content": "aMOpbGxvIOS4rQ==", "contentType": "plaintext"})
    );
    assert_eq!(pasteboard_read_body(), json!({"contentType": "plaintext"}));
    assert_eq!(
        find_body("type == 'XCUIElementTypeTextField'"),
        json!({"using": "predicate string", "value": "type == 'XCUIElementTypeTextField'"})
    );
    // The words the model uses are the ones that map.
    for (word, orientation) in [
        ("portrait", Orientation::Portrait),
        ("landscape_left", Orientation::LandscapeLeft),
        ("landscape_right", Orientation::LandscapeRight),
        ("portrait_upside_down", Orientation::PortraitUpsideDown),
    ] {
        assert_eq!(Orientation::parse(word), Some(orientation));
        assert_eq!(orientation.word(), word);
    }
    assert_eq!(ORIENTATIONS.len(), 4);
    // The old word for a landscape that does not say which way is not one of them.
    assert_eq!(Orientation::parse("landscape"), None);
}

#[test]
fn replies_are_read_as_webdriveragent_writes_them() {
    // Alerts: a string, its lines joined; buttons a list of labels.
    assert_eq!(
        alert_text(&json!("Line one\nLine two")).as_deref(),
        Some("Line one\nLine two")
    );
    assert_eq!(alert_text(&json!("")).as_deref(), Some(""));
    assert_eq!(alert_text(&json!(["a", "b"])).as_deref(), Some("a\nb"));
    // `null` is what a command that returns nothing answers: there is no alert.
    assert_eq!(alert_text(&Value::Null), None);
    assert_eq!(alert_text(&json!(5)), None);
    assert_eq!(
        alert_buttons(&json!(["Cancel", "", 5, "Allow"])),
        Some(vec!["Cancel".to_owned(), "Allow".to_owned()])
    );
    assert_eq!(alert_buttons(&json!([])), None);
    assert_eq!(alert_buttons(&json!("OK")), None);
    assert_eq!(
        alert_buttons(&Value::Array(
            (0..40).map(|n| json!(format!("b{n}"))).collect()
        ))
        .unwrap()
        .len(),
        BUTTONS_SHOWN
    );

    // Battery: a fraction and a state number; a simulator has neither.
    assert_eq!(
        battery_text(&json!({"level": 0.8, "state": 2})).as_deref(),
        Some("80%, charging")
    );
    assert_eq!(
        battery_text(&json!({"level": 1.0, "state": 3})).as_deref(),
        Some("100%, full")
    );
    assert_eq!(
        battery_text(&json!({"level": 0.2, "state": 1})).as_deref(),
        Some("20%, unplugged")
    );
    assert_eq!(
        battery_text(&json!({"level": 0.34, "state": 0})).as_deref(),
        Some("34%")
    );
    assert_eq!(
        battery_text(&json!({"level": -1, "state": 2})).as_deref(),
        Some("level unknown, charging")
    );
    assert_eq!(battery_text(&json!({"level": -1, "state": 0})), None);
    assert_eq!(battery_text(&json!("full")), None);

    // The app in front.
    assert_eq!(
        front_text(&json!({"pid": 5, "bundleId": "com.apple.Preferences", "name": "Settings"}))
            .as_deref(),
        Some("Settings (com.apple.Preferences)")
    );
    assert_eq!(
        front_text(&json!({"bundleId": "com.apple.springboard", "name": " "})).as_deref(),
        Some("home screen or system UI (com.apple.springboard)")
    );
    assert_eq!(
        front_text(&json!({"bundleId": "a.b"})).as_deref(),
        Some("a.b")
    );
    assert_eq!(front_text(&json!({"pid": 1})), None);

    // App states (XCUIApplicationState).
    for (state, name) in [
        (1, "not running"),
        (2, "background (suspended)"),
        (3, "background"),
        (4, "foreground"),
    ] {
        assert_eq!(app_state_name(&json!(state)), Some(name));
    }
    assert_eq!(app_state_name(&json!(0)), None);
    assert_eq!(app_state_name(&json!(9)), None);
    assert_eq!(app_state_name(&json!("4")), None);

    // The lock state.
    assert_eq!(lock_text(Some(&json!(true))).as_deref(), Some("locked"));
    assert_eq!(lock_text(Some(&json!(false))).as_deref(), Some("unlocked"));
    assert_eq!(lock_text(Some(&json!("yes"))), None);
    assert_eq!(lock_text(None), None);

    // The system line.
    assert_eq!(
        system_text(&json!({"os": {"name": "iOS", "version": "17.5"}})).as_deref(),
        Some("iOS 17.5")
    );
    assert_eq!(
        system_text(&json!({"os": {"version": "18.0"}})).as_deref(),
        Some("18.0")
    );
    assert_eq!(system_text(&json!({"os": {"name": "iOS"}})), None);

    // Degrees read short.
    assert_eq!(degrees_text(52.52), "52.52");
    assert_eq!(degrees_text(-13.405), "-13.405");
    assert_eq!(degrees_text(0.0), "0");
    assert_eq!(degrees_text(-0.0), "0");
    assert_eq!(degrees_text(90.0), "90");
    assert_eq!(degrees_text(1.0 / 3.0), "0.333333");
}

#[test]
fn what_a_device_sent_is_cleaned_before_it_is_shown() {
    assert_eq!(plain("a\u{7}b\u{1b}[31mc\r\nd\te\u{0}"), "ab[31mc\nd\te");
    // Format characters show nothing of their own and are dropped as well: bidirectional
    // overrides and isolates, zero-width and joiner characters, the BOM, tag characters.
    assert_eq!(
        plain("Al\u{202e}low\u{2066}!\u{2069} \u{200b}Pa\u{200d}ste\u{feff}\u{e0041}?"),
        "Allow! Paste?"
    );
    assert_eq!(
        one_line("  Allow\u{202e} \u{200b}\n  Paste?\t ", 50),
        "Allow Paste?"
    );
    // Text and the marks that belong to it stay.
    assert_eq!(
        plain("caf\u{e9} \u{4e2d}\u{6587} \u{1f600}"),
        "caf\u{e9} \u{4e2d}\u{6587} \u{1f600}"
    );
    assert_eq!(one_line("  Allow \u{7}\n  Paste?\t ", 50), "Allow Paste?");
    assert_eq!(one_line("abcdef", 3), "abc\u{2026}");
    assert_eq!(one_line("abc", 3), "abc");
    assert_eq!(
        cut("\u{4e2d}\u{4e2d}\u{4e2d}", 2),
        "\u{4e2d}\u{4e2d}\u{2026}"
    );
    assert_eq!(
        field(Some(&json!("  Sam\u{7}'s\niPhone ")), 80).as_deref(),
        Some("Sam's iPhone")
    );
    assert_eq!(field(Some(&json!("  ")), 80), None);
    assert_eq!(field(Some(&json!(5)), 80), None);
    assert_eq!(field(None, 80), None);
    // Labels with a comma are quoted, so the list stays one.
    assert_eq!(
        label_list(&["Yes, delete".to_owned(), "No".to_owned()]),
        "\"Yes, delete\", No"
    );
    assert_eq!(
        alert_sentence(
            "An alert is showing",
            &Alert {
                text: "Open in \u{201c}App\u{201d}?\n".to_owned(),
                buttons: Some(vec!["Cancel".to_owned(), "Open".to_owned()]),
            }
        ),
        "An alert is showing: \"Open in \u{201c}App\u{201d}?\". Buttons: Cancel, Open."
    );
    assert_eq!(
        alert_sentence(
            "An alert is showing",
            &Alert {
                text: String::new(),
                buttons: None,
            }
        ),
        "An alert is showing: no text. Its buttons could not be read."
    );
    // No alert is how WebDriverAgent says it, whichever words reach us.
    for message in [
        "WebDriverAgent could not do /alert/text: HTTP 404: An attempt was made to operate on a modal dialog when one was not open",
        "WebDriverAgent could not do /alert/text: HTTP 404 (no such alert)",
        "no such alert",
        "HTTP 404",
    ] {
        assert!(is_no_alert(&DriverError::Failed(message.to_owned())), "{message}");
    }
    for error in [
        DriverError::Failed("WebDriverAgent could not do /alert/text: HTTP 500: boom".to_owned()),
        DriverError::Disconnected("HTTP 404".to_owned()),
        DriverError::Unsupported("no such alert".to_owned()),
    ] {
        assert!(!is_no_alert(&error), "{error:?}");
    }
}

#[test]
fn a_pasteboard_reply_is_decoded_cleaned_and_cut_on_a_character() {
    let encoded = |text: &str| json!(STANDARD.encode(text.as_bytes()));
    assert_eq!(
        pasteboard_text(&encoded("h\u{e9}llo \u{4e2d}\r\nline\u{7}")).unwrap(),
        "h\u{e9}llo \u{4e2d}\nline"
    );
    // Missing padding and wrapped lines are tolerated, as in the driver's screenshots.
    assert_eq!(pasteboard_text(&json!("aGk")).unwrap(), "hi");
    assert_eq!(pasteboard_text(&json!("aG\nk=")).unwrap(), "hi");
    assert_eq!(pasteboard_text(&json!("")).unwrap(), "");
    assert_eq!(pasteboard_text(&Value::Null).unwrap(), "");
    // Bytes that are not UTF-8 are replaced, not refused.
    assert_eq!(
        pasteboard_text(&json!(STANDARD.encode([0x68, 0xff, 0x69]))).unwrap(),
        "h\u{fffd}i"
    );
    assert_eq!(
        pasteboard_text(&json!("not base64 !!")).unwrap_err(),
        "WebDriverAgent's clipboard reply is not valid base64."
    );
    assert_eq!(
        pasteboard_text(&json!(5)).unwrap_err(),
        "WebDriverAgent's clipboard reply is not text."
    );

    assert_eq!(clipped("abcdef", 10), ("abcdef", false));
    assert_eq!(clipped("abcdef", 3), ("abc", true));
    // Three bytes a character: cut at 4 is cut at 3.
    assert_eq!(clipped("\u{4e2d}\u{4e2d}", 4), ("\u{4e2d}", true));
    assert_eq!(clipped("\u{4e2d}", 2), ("", true));
}

// ---------------------------------------------------------------------------------------------
// screen_app
// ---------------------------------------------------------------------------------------------

#[test]
fn an_app_is_launched_activated_and_stopped_by_its_bundle_id() {
    let driver = Script::new()
        .on("POST session:/wda/apps/launch", none())
        .on("POST session:/wda/apps/activate", none())
        .on("POST session:/wda/apps/terminate", Ok(json!(true)))
        .on("POST session:/wda/apps/terminate", Ok(json!(false)))
        .on("POST session:/wda/apps/terminate", none())
        .driver();

    let launch = call(
        "screen_app",
        json!({
            "action": "launch",
            "app": "com.apple.Preferences",
            "arguments": ["-a", "b"],
            "environment": {"MODE": "test"}
        }),
    );
    assert_eq!(
        run(&driver, &launch).unwrap(),
        "Launched com.apple.Preferences."
    );
    let activate = call("screen_app", json!({"action": "activate", "app": "a.b"}));
    assert_eq!(
        run(&driver, &activate).unwrap(),
        "Brought a.b to the front."
    );
    // `stop` is WebDriverAgent's terminate.
    let stop = call("screen_app", json!({"action": "stop", "app": "a.b"}));
    assert_eq!(run(&driver, &stop).unwrap(), "Quit a.b.");
    assert_eq!(run(&driver, &stop).unwrap(), "a.b was not running.");
    assert_eq!(run(&driver, &stop).unwrap(), "Quit a.b.");

    let made = calls(&driver);
    assert_eq!(
        made.iter()
            .map(|call| call.path.clone())
            .collect::<Vec<_>>(),
        [
            format!("{S}/wda/apps/launch"),
            format!("{S}/wda/apps/activate"),
            format!("{S}/wda/apps/terminate"),
            format!("{S}/wda/apps/terminate"),
            format!("{S}/wda/apps/terminate"),
        ]
    );
    assert!(made.iter().all(|call| call.method == "POST"));
    assert_eq!(
        made[0].body,
        Some(json!({
            "bundleId": "com.apple.Preferences",
            "arguments": ["-a", "b"],
            "environment": {"MODE": "test"}
        }))
    );
    assert_eq!(made[1].body, Some(json!({"bundleId": "a.b"})));
    assert_eq!(made[2].body, Some(json!({"bundleId": "a.b"})));
}

#[test]
fn a_restart_quits_the_app_and_then_launches_it_fresh() {
    let restart = call(
        "screen_app",
        json!({
            "action": "launch",
            "app": "a.b",
            "restart": true,
            "arguments": ["-x"],
            "environment": {"MODE": "test"}
        }),
    );
    // It was running: quit, then a fresh start, which is where the arguments apply.
    let driver = Script::new()
        .on("POST session:/wda/apps/terminate", Ok(json!(true)))
        .on("POST session:/wda/apps/launch", none())
        .driver();
    assert_eq!(
        run(&driver, &restart).unwrap(),
        "Restarted a.b: quit it and launched it again."
    );
    assert_eq!(
        lines(&driver),
        [
            format!("POST {S}/wda/apps/terminate"),
            format!("POST {S}/wda/apps/launch"),
        ]
    );
    let made = calls(&driver);
    assert_eq!(made[0].body, Some(json!({"bundleId": "a.b"})));
    assert_eq!(
        made[1].body,
        Some(json!({"bundleId": "a.b", "arguments": ["-x"], "environment": {"MODE": "test"}}))
    );

    // It was not running: said, and launched all the same.
    let driver = Script::new()
        .on("POST session:/wda/apps/terminate", Ok(json!(false)))
        .on("POST session:/wda/apps/launch", none())
        .driver();
    assert_eq!(
        run(&driver, &restart).unwrap(),
        "a.b was not running; launched it."
    );

    // Without `restart` (or with it false) nothing is quit.
    for input in [
        json!({"action": "launch", "app": "a.b"}),
        json!({"action": "launch", "app": "a.b", "restart": false}),
    ] {
        let driver = Script::new()
            .on("POST session:/wda/apps/launch", none())
            .driver();
        assert_eq!(
            run(&driver, &call("screen_app", input)).unwrap(),
            "Launched a.b."
        );
        assert_eq!(lines(&driver), [format!("POST {S}/wda/apps/launch")]);
    }

    // The launch fails after the quit: the answer says what was done.
    let driver = Script::new()
        .on("POST session:/wda/apps/terminate", Ok(json!(true)))
        .on(
            "POST session:/wda/apps/launch",
            Err(DriverError::Failed("HTTP 500: no such app".to_owned())),
        )
        .driver();
    assert_eq!(
        said(run(&driver, &restart)),
        "Quit a.b. Then launching it failed: HTTP 500: no such app."
    );
    // ... and a lost connection stays the driver's.
    let driver = Script::new()
        .on("POST session:/wda/apps/terminate", Ok(json!(true)))
        .on(
            "POST session:/wda/apps/launch",
            Err(DriverError::Disconnected("gone".to_owned())),
        )
        .driver();
    assert!(matches!(
        run(&driver, &restart),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
    // The quit failing launches nothing.
    let driver = Script::new()
        .on(
            "POST session:/wda/apps/terminate",
            Err(DriverError::Failed("HTTP 500: busy".to_owned())),
        )
        .on("POST session:/wda/apps/launch", none())
        .driver();
    assert_eq!(
        run(&driver, &restart),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: busy".to_owned()
        )))
    );
    assert_eq!(lines(&driver), [format!("POST {S}/wda/apps/terminate")]);
    // No time for the launch after the quit: the answer says what was done.
    let driver = FakeDriver::with_ios(FakeIos::new(|_, route, _| {
        if route_key("POST", route) == "POST session:/wda/apps/terminate" {
            std::thread::sleep(Duration::from_millis(700));
            return Ok(json!(true));
        }
        none()
    }));
    assert_eq!(
        said(run_within(
            &driver,
            &restart,
            NEVER,
            Duration::from_millis(900)
        )),
        "Quit a.b. Ran out of this call's time before asking the device."
    );
    assert_eq!(lines(&driver), [format!("POST {S}/wda/apps/terminate")]);
}

#[test]
fn the_running_apps_are_listed_and_the_list_says_what_it_leaves_out() {
    let apps = json!([
        {"pid": 1, "bundleId": "com.apple.springboard"},
        {"pid": 2, "bundleId": "com.apple.Preferences"},
        {"pid": 3, "bundleId": "com.example.Notes"},
        {"pid": 4, "bundleId": "com.apple.Preferences"},
        {"pid": 5},
        {"pid": 6, "bundleId": "  "},
        {"pid": 7, "bundleId": "com.ex\u{7}ample.Evil\u{202e}"}
    ]);
    let driver = Script::new()
        .on("GET session:/wda/apps/list", Ok(apps))
        .driver();
    let list = |input: Value| run(&driver, &call("screen_app", input)).unwrap();

    // Each app once, as the device named it, with the note that the list is only what runs now.
    let listed = list(json!({}));
    assert_eq!(
        listed,
        "Running apps: 4 (showing 4). Only apps running now are listed; an iPhone or iPad does not tell WebDriverAgent what is installed.\ncom.apple.springboard\ncom.apple.Preferences\ncom.example.Notes\ncom.example.Evil"
    );
    assert_eq!(RUNNING_ONLY, "Only apps running now are listed; an iPhone or iPad does not tell WebDriverAgent what is installed.");
    // A filter matches inside the id, whichever the case; a limit cuts the list and says so.
    assert_eq!(
        list(json!({"action": "list", "filter": "APPLE"})),
        format!("Running apps matching \"APPLE\": 2 (showing 2). {RUNNING_ONLY}\ncom.apple.springboard\ncom.apple.Preferences")
    );
    assert_eq!(
        list(json!({"filter": "apple", "limit": 1})),
        format!(
            "Running apps matching \"apple\": 2 (showing 1). {RUNNING_ONLY}\ncom.apple.springboard"
        )
    );
    assert_eq!(
        list(json!({"limit": 2})),
        format!("Running apps: 4 (showing 2). {RUNNING_ONLY}\ncom.apple.springboard\ncom.apple.Preferences")
    );
    // Nothing matches: the note is still there.
    assert_eq!(
        list(json!({"filter": "nothing"})),
        format!("Running apps matching \"nothing\": 0 (showing 0). {RUNNING_ONLY}")
    );
    // The list is read, not changed.
    assert!(calls(&driver)
        .iter()
        .all(|call| call.method == "GET" && call.path == format!("{S}/wda/apps/list")));

    // `null` is an empty list; anything but a list is not one.
    let driver = Script::new()
        .on("GET session:/wda/apps/list", none())
        .driver();
    assert_eq!(
        run(&driver, &call("screen_app", json!({}))).unwrap(),
        format!("Running apps: 0 (showing 0). {RUNNING_ONLY}")
    );
    let driver = Script::new()
        .on(
            "GET session:/wda/apps/list",
            Ok(json!("com.apple.Preferences")),
        )
        .driver();
    assert_eq!(
        said(run(&driver, &call("screen_app", json!({})))),
        "WebDriverAgent's list of running apps is not a list."
    );
    let driver = Script::new().driver();
    assert_eq!(
        said(run(&driver, &call("screen_app", json!({})))),
        "This WebDriverAgent build has no /wda/apps/list; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
}

#[test]
fn the_app_in_front_and_the_lock_screen_are_told_together() {
    let foreground = call("screen_app", json!({"action": "foreground"}));
    let driver = Script::new()
        .on(
            "GET /wda/activeAppInfo",
            Ok(json!({"pid": 123, "bundleId": "com.apple.Preferences", "name": "Settings"})),
        )
        .on("GET /wda/locked", Ok(json!(false)))
        .driver();
    assert_eq!(
        run(&driver, &foreground).unwrap(),
        "app in front: Settings (com.apple.Preferences)\nlock state: unlocked"
    );
    assert_eq!(
        lines(&driver),
        ["GET /wda/activeAppInfo", "GET /wda/locked"]
    );

    // Behind the lock screen the home screen is what is in front.
    let driver = Script::new()
        .on(
            "GET /wda/activeAppInfo",
            Ok(json!({"bundleId": "com.apple.springboard", "name": "SpringBoard"})),
        )
        .on("GET /wda/locked", Ok(json!(true)))
        .driver();
    assert_eq!(
        run(&driver, &foreground).unwrap(),
        "app in front: home screen or system UI (com.apple.springboard)\nlock state: locked"
    );

    // Either piece may be missing; neither at all is a failure that says why.
    let driver = Script::new()
        .on("GET /wda/locked", Ok(json!(false)))
        .driver();
    assert_eq!(
        run(&driver, &foreground).unwrap(),
        "app in front: unknown\nlock state: unlocked"
    );
    let driver = Script::new().driver();
    assert_eq!(
        said(run(&driver, &foreground)),
        "This WebDriverAgent build has none of the routes screen_app foreground reads; it is too old (appium/WebDriverAgent 6 or newer has them)."
    );
    let driver = Script::new()
        .on(
            "GET /wda/activeAppInfo",
            Err(DriverError::Disconnected("gone".to_owned())),
        )
        .driver();
    assert!(matches!(
        run(&driver, &foreground),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
    assert_eq!(lines(&driver).len(), 1, "the rest would fail the same way");
}

#[test]
fn an_apps_state_is_told_in_words() {
    let info = call("screen_app", json!({"action": "info", "app": "a.b"}));
    for (state, said) in [
        (json!(1), "app a.b: not running (WebDriverAgent cannot tell this from an app that is not installed)"),
        (json!(2), "app a.b: background (suspended)"),
        (json!(3), "app a.b: background"),
        (json!(4), "app a.b: foreground"),
        (json!(0), "app a.b: unknown"),
    ] {
        let driver = Script::new()
            .on("POST session:/wda/apps/state", Ok(state.clone()))
            .driver();
        assert_eq!(run(&driver, &info).unwrap(), said, "{state}");
        assert_eq!(lines(&driver), [format!("POST {S}/wda/apps/state")]);
        assert_eq!(calls(&driver)[0].body, Some(json!({"bundleId": "a.b"})));
    }
    let driver = Script::new().driver();
    assert_eq!(
        said(run(&driver, &info)),
        "This WebDriverAgent build has no /wda/apps/state; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
}

#[test]
fn a_build_without_the_app_routes_is_told_to_update() {
    let driver = Script::new().driver();
    for (input, route) in [
        (
            json!({"action": "launch", "app": "a.b"}),
            "/wda/apps/launch",
        ),
        (
            json!({"action": "activate", "app": "a.b"}),
            "/wda/apps/activate",
        ),
        (
            json!({"action": "stop", "app": "a.b"}),
            "/wda/apps/terminate",
        ),
        (
            json!({"action": "launch", "app": "a.b", "restart": true}),
            "/wda/apps/terminate",
        ),
        (json!({"action": "open_url", "url": "x://y"}), "/url"),
    ] {
        assert_eq!(
            said(run(&driver, &call("screen_app", input))),
            format!("This WebDriverAgent build has no {route}; it is too old (appium/WebDriverAgent 6 or newer has it).")
        );
    }
}

fn open_url_call() -> IosCall {
    call("screen_app", json!({"action": "open_url", "url": "x://y"}))
}

#[test]
fn opening_a_url_reports_the_alert_that_may_follow() {
    let alert = "Open in \u{201c}Example\u{201d}?";
    let driver = Script::new()
        .on("POST session:/url", none())
        .on("GET session:/alert/text", Ok(json!(alert)))
        .on(
            "GET session:/wda/alert/buttons",
            Ok(json!(["Cancel", "Open"])),
        )
        .driver();
    let open = call(
        "screen_app",
        json!({"action": "open_url", "url": "example://deep/link?x=1", "app": "com.example.app"}),
    );
    let started = std::time::Instant::now();
    assert_eq!(
        run(&driver, &open).unwrap(),
        format!("Opened example://deep/link?x=1.\nAn alert is showing: \"{alert}\". Buttons: Cancel, Open.")
    );
    assert!(
        started.elapsed() >= URL_SETTLE,
        "the alert is given a moment to appear"
    );
    assert_eq!(
        lines(&driver),
        [
            format!("POST {S}/url"),
            format!("GET {S}/alert/text"),
            format!("GET {S}/wda/alert/buttons"),
        ]
    );
    // The app the URL opens in travels as WebDriverAgent's `bundleId`.
    assert_eq!(
        calls(&driver)[0].body,
        Some(json!({"url": "example://deep/link?x=1", "bundleId": "com.example.app"}))
    );

    // No alert: just the opening. The URL is shown cut, on one line.
    let driver = Script::new()
        .on("POST session:/url", none())
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    let long = format!("https://example.com/{}", "a".repeat(300));
    let open = call("screen_app", json!({"action": "open_url", "url": long}));
    let said = run(&driver, &open).unwrap();
    assert_eq!(said.lines().count(), 1, "{said}");
    assert!(
        said.starts_with("Opened https://example.com/aaaa"),
        "{said}"
    );
    assert!(said.ends_with("\u{2026}."), "{said}");
    assert_eq!(calls(&driver)[0].body, Some(json!({"url": long})));

    // A build without alerts, or one that fails to answer, leaves the opening as it was.
    let driver = Script::new().on("POST session:/url", none()).driver();
    assert_eq!(run(&driver, &open_url_call()).unwrap(), "Opened x://y.");
    let driver = Script::new()
        .on("POST session:/url", none())
        .on(
            "GET session:/alert/text",
            Err(DriverError::Failed("HTTP 500: busy".to_owned())),
        )
        .driver();
    assert_eq!(run(&driver, &open_url_call()).unwrap(), "Opened x://y.");
}

#[test]
fn a_url_opened_with_no_time_left_to_look_says_so() {
    // The opening takes most of the call's time: the call does not wait for an alert it could
    // not look for.
    let driver = FakeDriver::with_ios(FakeIos::new(|_, _, _| {
        std::thread::sleep(Duration::from_millis(700));
        none()
    }));
    let said = run_within(
        &driver,
        &open_url_call(),
        NEVER,
        Duration::from_millis(1000),
    )
    .unwrap();
    assert_eq!(
        said,
        "Opened x://y.\nNo time was left to check for an alert."
    );
    assert_eq!(lines(&driver), [format!("POST {S}/url")]);

    // Stopped while the opening was under way.
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let driver = FakeDriver::with_ios(FakeIos::new(move |_, _, _| {
        flag.store(true, Ordering::SeqCst);
        none()
    }));
    let cancelled = stopped.clone();
    let said = run_within(
        &driver,
        &open_url_call(),
        &move || cancelled.load(Ordering::SeqCst),
        Duration::from_secs(20),
    )
    .unwrap();
    assert_eq!(
        said,
        "Opened x://y.\nNo time was left to check for an alert."
    );
    assert_eq!(lines(&driver), [format!("POST {S}/url")]);
}

// ---------------------------------------------------------------------------------------------
// Time, stops and the wrong screen
// ---------------------------------------------------------------------------------------------

#[test]
fn a_call_gets_the_shorter_of_its_limit_and_what_is_left() {
    let launch = call("screen_app", json!({"action": "launch", "app": "a.b"}));
    let driver = Script::new()
        .on("POST session:/wda/apps/launch", none())
        .driver();
    run_within(&driver, &launch, NEVER, Duration::from_secs(25)).unwrap();
    assert_eq!(calls(&driver)[0].timeout, APP_CALL);

    let driver = Script::new()
        .on("POST session:/wda/apps/launch", none())
        .driver();
    run_within(&driver, &launch, NEVER, Duration::from_secs(3)).unwrap();
    let timeout = calls(&driver)[0].timeout;
    assert!(
        timeout <= Duration::from_secs(3) && timeout > Duration::from_millis(2500),
        "{timeout:?}"
    );

    // Every question of the device's info is bounded the same way, and none starts without room.
    let info = call("screen_device", json!({}));
    let driver = Script::new()
        .on("GET /status", Ok(json!({"os": {"version": "17"}})))
        .driver();
    run_within(&driver, &info, NEVER, Duration::from_secs(2)).unwrap();
    assert!(!calls(&driver).is_empty());
    assert!(calls(&driver)
        .iter()
        .all(|call| call.timeout <= Duration::from_secs(2)));

    let driver = Script::new()
        .on("POST session:/wda/apps/launch", none())
        .driver();
    let error = said(run_within(
        &driver,
        &launch,
        NEVER,
        Duration::from_millis(100),
    ));
    assert_eq!(
        error,
        "Ran out of this call's time before asking the device."
    );
    assert!(calls(&driver).is_empty());
}

#[test]
fn a_stopped_call_asks_nothing() {
    let driver = Script::new()
        .on("POST session:/wda/apps/launch", none())
        .on("GET /status", none())
        .driver();
    let always = || true;
    let launch = call("screen_app", json!({"action": "launch", "app": "a.b"}));
    assert_eq!(
        said(run_within(
            &driver,
            &launch,
            &always,
            Duration::from_secs(20)
        )),
        "Cancelled before asking the device."
    );
    let info = call("screen_device", json!({}));
    assert_eq!(
        said(run_within(&driver, &info, &always, Duration::from_secs(20))),
        "Cancelled before asking the device."
    );
    let answer = dialog(json!({"button": "OK"}));
    assert_eq!(
        said(run_within(
            &driver,
            &answer,
            &always,
            Duration::from_secs(20)
        )),
        "Cancelled before asking the device."
    );
    assert!(calls(&driver).is_empty());
}

#[test]
fn another_platforms_screen_is_told_so() {
    let driver = FakeDriver::touch();
    let launch = call("screen_app", json!({"action": "launch", "app": "a.b"}));
    assert_eq!(
        said(run_within(&driver, &launch, NEVER, Duration::from_secs(20))),
        "this screen is not an iPhone or iPad"
    );
    let answer = dialog(json!({"accept": true}));
    assert_eq!(
        said(run_within(&driver, &answer, NEVER, Duration::from_secs(20))),
        "this screen is not an iPhone or iPad"
    );
}

#[test]
fn a_lost_connection_is_the_drivers_error_for_the_session_to_act_on() {
    let gone = || DriverError::Disconnected("WebDriverAgent is not answering".to_owned());
    let driver = Script::new()
        .on("POST session:/wda/apps/launch", Err(gone()))
        .on("GET session:/wda/apps/list", Err(gone()))
        .on("GET /status", Err(gone()))
        .on("GET session:/alert/text", Err(gone()))
        .on("POST /wda/setPasteboard", Err(gone()))
        .driver();
    for input in [
        ("screen_app", json!({"action": "launch", "app": "a.b"})),
        ("screen_app", json!({})),
        ("screen_device", json!({})),
        ("screen_clipboard", json!({"action": "write", "text": "x"})),
    ] {
        match run(&driver, &call(input.0, input.1)) {
            Err(OpError::Driver(DriverError::Disconnected(_))) => {}
            other => panic!("{other:?}"),
        }
    }
    match run(&driver, &dialog(json!({"accept": true}))) {
        Err(OpError::Driver(DriverError::Disconnected(_))) => {}
        other => panic!("{other:?}"),
    }
    // The device's info stopped asking at the first: the rest would have failed the same way.
    let info = call("screen_device", json!({}));
    let driver = Script::new().on("GET /status", Err(gone())).driver();
    let _ = run(&driver, &info);
    assert_eq!(lines(&driver), ["GET /status".to_owned()]);

    // A device's own error is the driver's too, for the session to put a prefix on.
    let driver = Script::new()
        .on(
            "POST session:/wda/apps/launch",
            Err(DriverError::Failed("HTTP 500: no such app".to_owned())),
        )
        .driver();
    let launch = call("screen_app", json!({"action": "launch", "app": "a.b"}));
    assert_eq!(
        run(&driver, &launch),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: no such app".to_owned()
        )))
    );
}

// ---------------------------------------------------------------------------------------------
// screen_device info
// ---------------------------------------------------------------------------------------------

fn phone_script() -> Script {
    Script::new()
        .on(
            "GET /status",
            Ok(json!({"ready": true, "os": {"name": "iOS", "version": "17.5"}})),
        )
        .on(
            "GET /wda/device/info",
            Ok(json!({
                "name": "Sam's iPhone",
                "model": "iPhone",
                "currentLocale": "en_US",
                "timeZone": "Europe/Berlin",
                "userInterfaceStyle": "dark",
                "isSimulator": false
            })),
        )
        .on(
            "GET /wda/activeAppInfo",
            Ok(json!({"pid": 123, "bundleId": "com.apple.Preferences", "name": "Settings"})),
        )
        .on("GET /wda/locked", Ok(json!(false)))
        .on("GET session:/orientation", Ok(json!("PORTRAIT")))
        .on(
            "GET session:/wda/batteryInfo",
            Ok(json!({"level": 0.8, "state": 2})),
        )
}

/// [`phone_script`] with no alert showing and the app asked about in the foreground.
fn quiet_phone() -> Script {
    phone_script()
        .on("GET session:/alert/text", no_such_alert())
        .on("POST session:/wda/apps/state", Ok(json!(4)))
}

/// The `orientation:` line the device's info prints when the device answers `reply` to
/// `GET /orientation` and `rotation` (if any) to `GET /rotation`.
fn orientation_line(reply: Value, rotation: Option<Value>) -> String {
    let script = quiet_phone().set("GET session:/orientation", Ok(reply));
    let script = match rotation {
        Some(rotation) => script.on("GET session:/rotation", Ok(rotation)),
        None => script,
    };
    let driver = script.driver();
    run(
        &driver,
        &call("screen_device", json!({"sections": ["display"]})),
    )
    .unwrap()
    .lines()
    .find(|line| line.trim_start().starts_with("orientation:"))
    .expect("an orientation line")
    .trim_start()
    .to_owned()
}

#[test]
fn the_orientation_is_told_in_the_words_screen_device_rotate_takes() {
    // WebDriverAgent names the four orientations its own way (FBOrientationCommands); a device
    // that answers with them is read the way they are set, and what is told is what rotate takes,
    // back to the same orientation.
    for (wda, said) in [
        ("PORTRAIT", "portrait"),
        ("LANDSCAPE", "landscape_left"),
        ("UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT", "landscape_right"),
        (
            "UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN",
            "portrait_upside_down",
        ),
    ] {
        assert_eq!(
            orientation_line(json!(wda), None),
            format!("orientation: {said}"),
            "{wda}"
        );
        let rotate = call("screen_device", json!({"action": "rotate", "value": said}));
        let driver = Script::new()
            .on("POST session:/orientation", none())
            .driver();
        run(&driver, &rotate).unwrap();
        assert_eq!(calls(&driver)[0].body, Some(json!({"orientation": wda})));
    }
    for orientation in [
        Orientation::Portrait,
        Orientation::LandscapeLeft,
        Orientation::LandscapeRight,
        Orientation::PortraitUpsideDown,
    ] {
        assert_eq!(Orientation::from_wda(orientation.wda()), Some(orientation));
        assert_eq!(Orientation::parse(orientation.word()), Some(orientation));
    }

    // A current build answers `GET /orientation` with `PORTRAIT` or `LANDSCAPE` only, whichever
    // way the device is turned; `GET /rotation` says which (WebDriverAgent's `fb_rotationMapping`:
    // portrait 0, landscape right 90, upside down 180, landscape left 270).
    for (z, said) in [
        (0, "portrait"),
        (90, "landscape_right"),
        (180, "portrait_upside_down"),
        (270, "landscape_left"),
    ] {
        let folded = if z == 0 || z == 180 {
            "PORTRAIT"
        } else {
            "LANDSCAPE"
        };
        assert_eq!(
            orientation_line(json!(folded), Some(json!({"x": 0, "y": 0, "z": z}))),
            format!("orientation: {said}"),
            "z {z}"
        );
    }
    assert_eq!(
        Orientation::from_degrees(270.0),
        Some(Orientation::LandscapeLeft)
    );
    assert_eq!(
        Orientation::from_degrees(90.0),
        Some(Orientation::LandscapeRight)
    );
    // A rotation it cannot place (-1) or does not make sense of leaves the orientation's own name.
    for rotation in [
        json!({"x": -1, "y": -1, "z": -1}),
        json!({"z": 45}),
        json!({"z": "90"}),
        json!("90"),
        Value::Null,
    ] {
        assert_eq!(
            orientation_line(json!("LANDSCAPE"), Some(rotation.clone())),
            "orientation: landscape_left",
            "{rotation}"
        );
    }

    // A name that is none of the four is shown as it came, cleaned; none at all is unknown.
    assert_eq!(
        orientation_line(json!("Unknown orientation"), None),
        "orientation: unknown orientation"
    );
    assert_eq!(
        orientation_line(json!("FACE\u{7}UP\u{202e}"), None),
        "orientation: faceup"
    );
    assert_eq!(orientation_line(Value::Null, None), "orientation: unknown");
    assert_eq!(orientation_line(json!(""), None), "orientation: unknown");
    assert_eq!(Orientation::from_wda("landscape_right"), None);
}

#[test]
fn the_device_reports_what_it_is_and_what_it_shows() {
    let driver = quiet_phone().driver();
    let said = run(&driver, &call("screen_device", json!({}))).unwrap();
    assert_eq!(
        said,
        [
            "device:",
            "  name: Sam's iPhone",
            "  model: iPhone",
            "  system: iOS 17.5",
            "  kind: real device",
            "  locale: en_US",
            "  time zone: Europe/Berlin",
            "display:",
            "  appearance: dark",
            "  orientation: portrait",
            "  app in front: Settings (com.apple.Preferences)",
            "power:",
            "  lock state: unlocked",
            "  battery: 80%, charging",
            "alert: none",
        ]
        .join("\n")
    );
    // Nothing is changed; the app's state is `screen_app info`'s.
    assert!(lines(&driver).iter().all(|line| line.starts_with("GET ")));
    assert_eq!(
        lines(&driver),
        [
            "GET /status".to_owned(),
            "GET /wda/device/info".to_owned(),
            format!("GET {S}/orientation"),
            format!("GET {S}/rotation"),
            "GET /wda/activeAppInfo".to_owned(),
            "GET /wda/locked".to_owned(),
            format!("GET {S}/wda/batteryInfo"),
            format!("GET {S}/alert/text"),
        ]
    );
}

#[test]
fn the_sections_asked_for_are_the_ones_told_and_the_ones_read() {
    // In the order of the sections, whichever the order asked in; only what they need is read.
    let driver = quiet_phone().driver();
    let said = run(
        &driver,
        &call(
            "screen_device",
            json!({"action": "info", "sections": ["power", "device"]}),
        ),
    )
    .unwrap();
    assert_eq!(
        said,
        [
            "device:",
            "  name: Sam's iPhone",
            "  model: iPhone",
            "  system: iOS 17.5",
            "  kind: real device",
            "  locale: en_US",
            "  time zone: Europe/Berlin",
            "power:",
            "  lock state: unlocked",
            "  battery: 80%, charging",
            "alert: none",
        ]
        .join("\n")
    );
    assert_eq!(
        lines(&driver),
        [
            "GET /status".to_owned(),
            "GET /wda/device/info".to_owned(),
            "GET /wda/locked".to_owned(),
            format!("GET {S}/wda/batteryInfo"),
            format!("GET {S}/alert/text"),
        ]
    );

    let driver = quiet_phone().driver();
    let said = run(
        &driver,
        &call("screen_device", json!({"sections": ["display"]})),
    )
    .unwrap();
    assert_eq!(
        said,
        [
            "display:",
            "  appearance: dark",
            "  orientation: portrait",
            "  app in front: Settings (com.apple.Preferences)",
            "alert: none",
        ]
        .join("\n")
    );
    assert_eq!(
        lines(&driver),
        [
            "GET /wda/device/info".to_owned(),
            format!("GET {S}/orientation"),
            format!("GET {S}/rotation"),
            "GET /wda/activeAppInfo".to_owned(),
            format!("GET {S}/alert/text"),
        ]
    );

    let driver = quiet_phone().driver();
    let said = run(
        &driver,
        &call("screen_device", json!({"sections": ["power"]})),
    )
    .unwrap();
    assert_eq!(
        said,
        "power:\n  lock state: unlocked\n  battery: 80%, charging\nalert: none"
    );
    assert_eq!(
        lines(&driver),
        [
            "GET /wda/locked".to_owned(),
            format!("GET {S}/wda/batteryInfo"),
            format!("GET {S}/alert/text"),
        ]
    );
}

#[test]
fn an_alert_in_the_way_is_reported_with_its_buttons() {
    let driver = phone_script()
        .on("GET session:/alert/text", Ok(json!(MAPS)))
        .on(
            "GET session:/wda/alert/buttons",
            Ok(json!(["Don\u{2019}t Allow", "Allow While Using App"])),
        )
        .driver();
    let said = run(&driver, &call("screen_device", json!({}))).unwrap();
    assert!(
        said.ends_with(&format!(
            "\nalert: \"{MAPS}\"\nalert buttons: Don\u{2019}t Allow, Allow While Using App"
        )),
        "{said}"
    );
    // Whichever sections are asked for, the alert is told.
    let said = run(
        &driver,
        &call("screen_device", json!({"sections": ["power"]})),
    )
    .unwrap();
    assert!(said.contains("\nalert: \""), "{said}");

    // An alert with no text, whose buttons cannot be read.
    let driver = phone_script()
        .on("GET session:/alert/text", Ok(json!("")))
        .driver();
    let said = run(&driver, &call("screen_device", json!({}))).unwrap();
    assert!(
        said.ends_with("\nalert: showing, with no text\nalert buttons: unknown"),
        "{said}"
    );
}

#[test]
fn every_piece_is_optional_and_a_missing_one_reads_unknown() {
    // Only the status and the lock answer; the rest is a build that lacks them or fails.
    let driver = Script::new()
        .on(
            "GET /status",
            Ok(json!({"os": {"name": "iOS", "version": "15.8"}})),
        )
        .on("GET /wda/locked", Ok(json!(true)))
        .on(
            "GET /wda/device/info",
            Err(DriverError::Failed("HTTP 500: boom".to_owned())),
        )
        .driver();
    let said = run(&driver, &call("screen_device", json!({}))).unwrap();
    assert_eq!(
        said,
        [
            "device:",
            "  name: unknown",
            "  model: unknown",
            "  system: iOS 15.8",
            "  kind: unknown",
            "  locale: unknown",
            "  time zone: unknown",
            "display:",
            "  appearance: unknown",
            "  orientation: unknown",
            "  app in front: unknown",
            "power:",
            "  lock state: locked",
            "  battery: unknown",
            "alert: unknown",
        ]
        .join("\n")
    );

    // A simulator has no battery to report, and says it is one.
    let driver = phone_script()
        .set(
            "GET /wda/device/info",
            Ok(json!({"name": "iPhone 15", "model": "iPhone", "isSimulator": true, "userInterfaceStyle": "unsupported"})),
        )
        .set("GET session:/wda/batteryInfo", Ok(json!({"level": -1, "state": 0})))
        .driver();
    let said = run(&driver, &call("screen_device", json!({}))).unwrap();
    assert!(said.contains("  kind: simulator"), "{said}");
    assert!(said.contains("  battery: unknown"), "{said}");
    assert!(said.contains("  appearance: unknown"), "{said}");
}

#[test]
fn the_call_fails_only_when_nothing_answers() {
    let info = call("screen_device", json!({}));
    // A build that has none of the routes.
    let driver = Script::new().driver();
    assert_eq!(
        said(run(&driver, &info)),
        "This WebDriverAgent build has none of the routes screen_device info reads; it is too old (appium/WebDriverAgent 6 or newer has them)."
    );
    assert_eq!(calls(&driver).len(), 8, "every piece was tried");
    // Failures that are not a missing route say what failed.
    let driver = Script::new()
        .on("GET /status", Err(DriverError::Unsupported("x".to_owned())))
        .on(
            "GET /wda/device/info",
            Err(DriverError::Failed("HTTP 500: busy".to_owned())),
        )
        .driver();
    assert_eq!(
        run(&driver, &info),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: busy".to_owned()
        )))
    );
    // Out of time before anything.
    let driver = Script::new()
        .on("GET /status", Ok(json!({"os": {"version": "17"}})))
        .driver();
    assert_eq!(
        said(run_within(
            &driver,
            &info,
            NEVER,
            Duration::from_millis(100)
        )),
        "Ran out of this call's time before asking the device."
    );
    // No alert is an answer, even when it is the only one.
    let driver = Script::new()
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    let said = run(&driver, &info).unwrap();
    assert!(said.contains("alert: none"), "{said}");
}

#[test]
fn a_call_that_runs_out_of_time_midway_keeps_what_it_read_and_says_so() {
    let ticks = Arc::new(Mutex::new(0usize));
    let counter = ticks.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let script = phone_script();
    let driver = FakeDriver::with_ios(FakeIos::new(move |method, route, _| {
        let mut count = counter.lock().unwrap();
        *count += 1;
        // After the third answer the call is stopped.
        if *count >= 3 {
            flag.store(true, Ordering::SeqCst);
        }
        script.answer(method, route)
    }));
    let stopped = stop.clone();
    let said = run_within(
        &driver,
        &call("screen_device", json!({})),
        &move || stopped.load(Ordering::SeqCst),
        Duration::from_secs(20),
    )
    .unwrap();
    assert!(said.contains("  name: Sam's iPhone"), "{said}");
    assert!(said.contains("  orientation: portrait"), "{said}");
    assert!(said.contains("  lock state: unknown"), "{said}");
    assert!(said.contains("  app in front: unknown"), "{said}");
    assert_eq!(
        said.lines().last().unwrap(),
        "Cancelled before asking the device."
    );
    assert_eq!(lines(&driver).len(), 3);
}

// ---------------------------------------------------------------------------------------------
// preview_dialog
// ---------------------------------------------------------------------------------------------

fn alert_script(text: &str, buttons: Value) -> Script {
    Script::new()
        .on("GET session:/alert/text", Ok(json!(text)))
        .on("GET session:/wda/alert/buttons", Ok(buttons))
        .on("POST session:/alert/accept", none())
        .on("POST session:/alert/dismiss", none())
        .on("POST session:/alert/text", none())
}

const MAPS: &str = "Allow \u{201c}Maps\u{201d} to use your location?";

fn press(label: &str) -> IosCall {
    dialog(json!({"button": label}))
}

#[test]
fn a_button_is_pressed_by_its_exact_label_after_reading_the_alert() {
    let driver = alert_script(MAPS, json!(["Don\u{2019}t Allow", "Allow While Using App"]))
        // The alert is gone when it is looked at again.
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &press("Allow While Using App")).unwrap(),
        format!("Pressed \"Allow While Using App\" on \"{MAPS}\".")
    );
    assert_eq!(
        lines(&driver),
        [
            format!("GET {S}/alert/text"),
            format!("GET {S}/wda/alert/buttons"),
            format!("POST {S}/alert/accept"),
            format!("GET {S}/alert/text"),
        ]
    );
    assert_eq!(
        calls(&driver)[2].body,
        Some(json!({"name": "Allow While Using App"}))
    );
}

/// [`MAPS`] and the first of its buttons as an error message says them: device text goes through
/// `sanitize_for_message`, which turns quote-likes into plain quotes and marks a change with an
/// ellipsis.
const MAPS_IN_AN_ERROR: &str = "Allow 'Maps' to use your location?\u{2026}";
const DONT_ALLOW_IN_AN_ERROR: &str = "Don't Allow\u{2026}";

#[test]
fn an_unknown_label_is_refused_with_the_buttons_there_are() {
    let driver =
        alert_script(MAPS, json!(["Don\u{2019}t Allow", "Allow While Using App"])).driver();
    assert_eq!(
        said(run(&driver, &press("Allow"))),
        format!("No button \"Allow\" on the alert \"{MAPS_IN_AN_ERROR}\"; its buttons: {DONT_ALLOW_IN_AN_ERROR}, Allow While Using App.")
    );
    assert_eq!(
        said(run(&driver, &press("allow while using app"))),
        format!("No button \"allow while using app\" on the alert \"{MAPS_IN_AN_ERROR}\"; its buttons: {DONT_ALLOW_IN_AN_ERROR}, Allow While Using App.")
    );
    assert!(
        !lines(&driver).iter().any(|line| line.starts_with("POST")),
        "nothing was pressed: {:?}",
        lines(&driver)
    );

    // Labels with commas are quoted in the list.
    let driver = alert_script("Delete?", json!(["Yes, delete", "No"])).driver();
    assert_eq!(
        said(run(&driver, &press("Maybe"))),
        "No button \"Maybe\" on the alert \"Delete?\"; its buttons: \"Yes, delete\", No."
    );
    // The label the model read is the one that works, though the device's has stray characters.
    let driver = alert_script("Sure?", json!(["Yes\u{7}", "No"])).driver();
    run(&driver, &press("Yes")).unwrap();
    assert_eq!(
        calls(&driver)[2].body,
        Some(json!({"name": "Yes\u{7}"})),
        "the device is addressed by its own label"
    );
}

#[test]
fn device_text_in_a_refusal_goes_through_sanitize_for_message() {
    // An app can put anything on an alert: bidirectional overrides and isolates, zero-width
    // characters, quote-likes, a line break to start a second "sentence".
    let driver = alert_script(
        "Allow \u{202e}Maps\u{2066}?\nsecond \"line\"",
        json!([
            "Al\u{202e}low\u{200b}",
            "Say \"no\"",
            "\u{201c}Quoted\u{201d}, then",
            "Other"
        ]),
    )
    .driver();
    let refusal = said(run(&driver, &press("Nope \"x\"\nmore")));
    assert_eq!(
        refusal,
        "No button \"Nope 'x'\u{2026}\" on the alert \"Allow \u{fffd}Maps\u{fffd}?\u{2026}\"; its buttons: Al\u{fffd}low\u{fffd}\u{2026}, Say 'no'\u{2026}, \"'Quoted', then\u{2026}\", Other."
    );
    // The answer is the same line of text, whatever the device put in it.
    for hidden in [
        '\u{202e}', '\u{2066}', '\u{200b}', '\u{201c}', '\u{201d}', '\n',
    ] {
        assert!(!refusal.contains(hidden), "{hidden:?} in {refusal}");
    }
    assert_eq!(refusal.matches('"').count(), 6, "{refusal}");
    assert!(!lines(&driver).iter().any(|line| line.starts_with("POST")));

    // A long alert text is cut like any device text in an error.
    let driver = alert_script(&"w".repeat(500), json!(["OK", "Cancel"])).driver();
    let refusal = said(run(&driver, &press("Nope")));
    assert!(
        refusal.starts_with(&format!(
            "No button \"Nope\" on the alert \"{}\u{2026}\"; its buttons: OK, Cancel.",
            "w".repeat(120)
        )),
        "{refusal}"
    );
}

#[test]
fn a_label_copied_from_an_error_still_presses_its_button() {
    // The error says "Don't Allow\u{2026}" for the device's "Don\u{2019}t Allow": a model that
    // writes what it was told gets the button it means, and the device is addressed by its own label.
    let buttons = json!(["Don\u{2019}t Allow", "Allow While Using App", "Say \"no\""]);
    for (typed, device) in [
        ("Don't Allow", "Don\u{2019}t Allow"),
        ("Don't Allow\u{2026}", "Don\u{2019}t Allow"),
        ("Don\u{2019}t Allow", "Don\u{2019}t Allow"),
        ("Say 'no'", "Say \"no\""),
        ("Say 'no'\u{2026}", "Say \"no\""),
    ] {
        let driver = alert_script(MAPS, buttons.clone())
            .on("GET session:/alert/text", no_such_alert())
            .driver();
        run(&driver, &press(typed)).unwrap_or_else(|error| panic!("{typed}: {error:?}"));
        assert_eq!(
            calls(&driver)[2].body,
            Some(json!({"name": device})),
            "{typed}"
        );
    }

    // Only what reads the same in an error does: a different word, or a different case, does not.
    let driver = alert_script(MAPS, buttons.clone()).driver();
    for typed in ["Dont Allow", "don't allow", "Don't", "\u{2026}"] {
        assert!(
            matches!(run(&driver, &press(typed)), Err(OpError::Said(_))),
            "{typed}"
        );
    }
    assert!(!lines(&driver).iter().any(|line| line.starts_with("POST")));

    // Two buttons that read alike in an error are not guessed between; exactly one is still pressed.
    let alike = json!(["Don\u{2019}t", "Don\u{2018}t"]);
    let driver = alert_script("Sure?", alike.clone()).driver();
    assert_eq!(
        said(run(&driver, &press("Don't"))),
        "No button \"Don't\" on the alert \"Sure?\"; its buttons: Don't\u{2026}, Don't\u{2026}."
    );
    assert!(!lines(&driver).iter().any(|line| line.starts_with("POST")));
    let driver = alert_script("Sure?", alike)
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    run(&driver, &press("Don\u{2018}t")).unwrap();
    assert_eq!(
        calls(&driver)[2].body,
        Some(json!({"name": "Don\u{2018}t"}))
    );
}

/// Of two buttons with the same label, which one the model means is not known: pressing the first
/// could answer a permission or a deletion the other way. None is pressed, and the answer lists
/// the buttons; a label that picks exactly one is pressed as before.
#[test]
fn two_buttons_with_the_same_label_are_not_guessed_between() {
    for (buttons, label) in [
        (json!(["OK", "Cancel", "OK"]), "OK"),
        (json!(["Don\u{2019}t", "Don\u{2019}t"]), "Don\u{2019}t"),
    ] {
        let driver = alert_script("Sure?", buttons).driver();
        let refusal = said(run(&driver, &press(label)));
        assert!(
            refusal.starts_with("Several buttons of the alert \"Sure?\" are labelled")
                && refusal.contains("so none was pressed; its buttons: ")
                && refusal.ends_with(
                    "Take a preview_snapshot and press the one you mean with preview_click."
                ),
            "{label}: {refusal}"
        );
        assert!(
            !lines(&driver).iter().any(|line| line.starts_with("POST")),
            "nothing was pressed: {:?}",
            lines(&driver)
        );
    }
    assert_eq!(
        said(run(
            &alert_script("Sure?", json!(["OK", "Cancel", "OK"])).driver(),
            &press("OK")
        )),
        "Several buttons of the alert \"Sure?\" are labelled \"OK\", so none was pressed; its buttons: OK, Cancel, OK. Take a preview_snapshot and press the one you mean with preview_click."
    );

    // Buttons that differ in case are different labels: the exact one is pressed.
    let driver = alert_script("Sure?", json!(["OK", "Cancel", "Ok"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    run(&driver, &press("Ok")).unwrap();
    assert_eq!(calls(&driver)[2].body, Some(json!({"name": "Ok"})));
}

#[test]
fn hidden_characters_in_an_alert_are_dropped_from_what_is_shown() {
    let hostile = "Allow \u{202e}Maps\u{200b}?";
    let driver = alert_script(hostile, json!(["Cancel", "O\u{2066}K\u{2069}"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    // A successful answer keeps the person's quotes and reads whole, with nothing hidden in it.
    assert_eq!(
        run(&driver, &press("OK")).unwrap(),
        "Pressed \"OK\" on \"Allow Maps?\"."
    );
    let driver = phone_script()
        .on("GET session:/alert/text", Ok(json!(hostile)))
        .on(
            "GET session:/wda/alert/buttons",
            Ok(json!(["Cancel", "O\u{2066}K\u{2069}"])),
        )
        .driver();
    let info = run(&driver, &call("screen_device", json!({}))).unwrap();
    assert!(
        info.ends_with("\nalert: \"Allow Maps?\"\nalert buttons: Cancel, OK"),
        "{info}"
    );
}

#[test]
fn no_alert_is_a_sentence_about_tapping_by_position() {
    let driver = Script::new()
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    for input in [
        json!({"button": "OK"}),
        json!({"accept": true}),
        json!({"accept": false}),
        json!({"prompt_text": "x"}),
        json!({"prompt_text": "x", "accept": true}),
    ] {
        assert_eq!(
            said(run(&driver, &dialog(input))),
            "No alert is showing that WebDriverAgent can see. If one is on the screen, tap its button by its position instead."
        );
    }
    assert_eq!(
        lines(&driver).len(),
        5,
        "only the looks: {:?}",
        lines(&driver)
    );

    // A build without the route cannot see alerts.
    let driver = Script::new().driver();
    assert_eq!(
        said(run(&driver, &press("OK"))),
        "This WebDriverAgent build has no /alert/text; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
    // Another failure is not "no alert".
    let driver = Script::new()
        .on(
            "GET session:/alert/text",
            Err(DriverError::Failed("HTTP 500: broken".to_owned())),
        )
        .driver();
    assert_eq!(
        run(&driver, &press("OK")),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: broken".to_owned()
        )))
    );
}

/// A WebDriverAgent that answers `null` to everything (the reply of a command that returns
/// nothing) has no alert and nothing to report.
#[test]
fn a_device_that_answers_nothing_shows_no_alert_and_nothing_else() {
    let driver = FakeDriver::with_ios(FakeIos::new(|_, _, _| none()));
    assert_eq!(
        said(run(&driver, &dialog(json!({"accept": true})))),
        "No alert is showing that WebDriverAgent can see. If one is on the screen, tap its button by its position instead."
    );
    assert!(!lines(&driver).iter().any(|line| line.starts_with("POST")));
    let info = run(&driver, &call("screen_device", json!({}))).unwrap();
    assert!(
        info.starts_with("device:\n  name: unknown\n  model: unknown\n  system: unknown\n"),
        "{info}"
    );
    assert!(info.ends_with("\nalert: none"), "{info}");
}

#[test]
fn accept_and_dismiss_guess_the_button_and_say_which_one_it_was() {
    // An alert is accepted by its last button and dismissed by its first (WebDriverAgent's
    // `FBAlert`), and the answer says it was a guess by position and which label that makes.
    let driver = alert_script(MAPS, json!(["No", "Yes"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": true}))).unwrap(),
        format!("Accepted the alert \"{MAPS}\". The button was guessed by position: the last one, most likely \"Yes\".")
    );
    assert_eq!(calls(&driver)[2].path, format!("{S}/alert/accept"));
    assert_eq!(calls(&driver)[2].body, Some(json!({})));

    let driver = alert_script(MAPS, json!(["No", "Yes"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": false}))).unwrap(),
        format!("Dismissed the alert \"{MAPS}\". The button was guessed by position: the first one, most likely \"No\".")
    );
    assert_eq!(calls(&driver)[2].path, format!("{S}/alert/dismiss"));
    assert_eq!(calls(&driver)[2].body, Some(json!({})));

    let driver = alert_script("", json!(["OK"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": false}))).unwrap(),
        "Dismissed the alert. The button was guessed by position: the first one, most likely \"OK\"."
    );

    // The label is the one the model is shown, whatever the device put in it.
    let driver = alert_script("Sure?", json!(["Can\u{202e}cel", "O\u{7}K"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": true}))).unwrap(),
        "Accepted the alert \"Sure?\". The button was guessed by position: the last one, most likely \"OK\"."
    );

    // Buttons that cannot be read: it is still guessed, and which one it was is not known.
    let driver = Script::new()
        .on("GET session:/alert/text", Ok(json!("Hm")))
        .on("GET session:/alert/text", no_such_alert())
        .on("POST session:/alert/accept", none())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": true}))).unwrap(),
        "Accepted the alert \"Hm\". The button was guessed by position: its buttons could not be read in full, so which one was pressed is not known."
    );

    // A list cut at the buttons shown has no known last button, but still a first one.
    let many = Value::Array((0..30).map(|n| json!(format!("b{n}"))).collect());
    let driver = alert_script("Pick", many.clone())
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": true}))).unwrap(),
        "Accepted the alert \"Pick\". The button was guessed by position: its buttons could not be read in full, so which one was pressed is not known."
    );
    let driver = alert_script("Pick", many)
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": false}))).unwrap(),
        "Dismissed the alert \"Pick\". The button was guessed by position: the first one, most likely \"b0\"."
    );
}

#[test]
fn text_is_typed_into_the_alerts_field_and_the_alert_stays() {
    // `prompt_text` alone only types: the answer is a button's to give, and is left to the next call.
    let driver = alert_script("Enter the code", json!(["Cancel", "OK"])).driver();
    assert_eq!(
        run(&driver, &dialog(json!({"prompt_text": "hunter2"}))).unwrap(),
        "Typed 7 characters into the alert \"Enter the code\". Buttons: Cancel, OK."
    );
    assert_eq!(
        lines(&driver),
        [
            format!("GET {S}/alert/text"),
            format!("GET {S}/wda/alert/buttons"),
            format!("POST {S}/alert/text"),
        ]
    );
    assert_eq!(calls(&driver)[2].body, Some(json!({"value": "hunter2"})));
    // The typed text is not echoed back.
    let driver = alert_script("Enter the code", json!(["OK"])).driver();
    let said = run(&driver, &dialog(json!({"prompt_text": ""}))).unwrap();
    assert!(said.starts_with("Typed 0 characters"), "{said}");
    assert!(!lines(&driver).iter().any(|line| line.ends_with("/accept")));
    assert_eq!(calls(&driver)[2].body, Some(json!({"value": ""})));
    // Its buttons cannot be read: nothing to list.
    let driver = Script::new()
        .on("GET session:/alert/text", Ok(json!("Enter the code")))
        .on("POST session:/alert/text", none())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"prompt_text": "12"}))).unwrap(),
        "Typed 2 characters into the alert \"Enter the code\"."
    );
    // The most a prompt takes.
    let driver = alert_script("Note", json!(["OK"])).driver();
    let text = "x".repeat(PROMPT_TEXT_TYPABLE);
    run(&driver, &dialog(json!({"prompt_text": text}))).unwrap();
    assert_eq!(calls(&driver)[2].body, Some(json!({"value": text})));
}

/// WebDriverAgent types about 60 characters a second and the alert call has 10 s: a longer text
/// would be cut off part way and leave the field with half of it, so it is refused before
/// anything is sent, with what to do instead. The page's 4096 is still the schema's bound.
#[test]
fn a_prompt_text_longer_than_the_alert_call_can_type_is_refused_before_anything_is_sent() {
    assert_eq!(PROMPT_TEXT_TYPABLE, 500);
    assert!(PROMPT_TEXT_TYPABLE < PROMPT_TEXT_MAX);
    assert!(PROMPT_TEXT_TYPABLE * 1000 / TYPING_CHARS_PER_SECOND < ALERT_CALL.as_millis() as usize);
    for input in [
        json!({"prompt_text": "x".repeat(PROMPT_TEXT_TYPABLE + 1)}),
        json!({"prompt_text": "x".repeat(2000), "accept": true}),
        json!({"prompt_text": "x".repeat(PROMPT_TEXT_MAX), "button": "OK"}),
    ] {
        let length = input["prompt_text"].as_str().unwrap().chars().count();
        assert_eq!(
            dialog_refusal(input),
            format!("\"prompt_text\" is {length} characters, and WebDriverAgent types about 60 characters a second: an iPhone or iPad takes at most 500 here, within this call's 10 s. Nothing was typed or pressed, and the alert is still showing. Tap the alert's field to focus it, then type the longer text with preview_input type instead.")
        );
    }
    // Characters are counted, not bytes.
    dialog(json!({"prompt_text": "\u{4e2d}".repeat(PROMPT_TEXT_TYPABLE), "accept": true}));
}

#[test]
fn text_is_typed_first_and_then_the_alert_is_answered() {
    // With `accept` true: typed, then accepted by position.
    let driver = alert_script("Enter the code", json!(["Cancel", "OK"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"prompt_text": "1234", "accept": true}))).unwrap(),
        "Typed 4 characters into the alert \"Enter the code\". Accepted the alert \"Enter the code\". The button was guessed by position: the last one, most likely \"OK\"."
    );
    assert_eq!(
        lines(&driver),
        [
            format!("GET {S}/alert/text"),
            format!("GET {S}/wda/alert/buttons"),
            format!("POST {S}/alert/text"),
            format!("POST {S}/alert/accept"),
            format!("GET {S}/alert/text"),
        ]
    );
    assert_eq!(calls(&driver)[2].body, Some(json!({"value": "1234"})));
    assert_eq!(calls(&driver)[3].body, Some(json!({})));

    // With a button: typed, then that button pressed.
    let driver = alert_script("Enter the code", json!(["Cancel", "OK"]))
        .on("GET session:/alert/text", no_such_alert())
        .driver();
    assert_eq!(
        run(&driver, &dialog(json!({"prompt_text": "1234", "button": "Cancel"}))).unwrap(),
        "Typed 4 characters into the alert \"Enter the code\". Pressed \"Cancel\" on \"Enter the code\"."
    );
    assert_eq!(calls(&driver)[2].path, format!("{S}/alert/text"));
    assert_eq!(calls(&driver)[3].path, format!("{S}/alert/accept"));
    assert_eq!(calls(&driver)[3].body, Some(json!({"name": "Cancel"})));

    // A button the alert does not have is found out before anything is typed.
    let driver = alert_script("Enter the code", json!(["Cancel", "OK"])).driver();
    assert_eq!(
        said(run(
            &driver,
            &dialog(json!({"prompt_text": "1234", "button": "Done"}))
        )),
        "No button \"Done\" on the alert \"Enter the code\"; its buttons: Cancel, OK."
    );
    assert!(posts(&driver).is_empty(), "{:?}", lines(&driver));

    // The text goes in and the answer fails: the answer says the text is there.
    let driver = alert_script("Enter the code", json!(["Cancel", "OK"]))
        .set(
            "POST session:/alert/accept",
            Err(DriverError::Failed("HTTP 500: boom".to_owned())),
        )
        .driver();
    assert_eq!(
        said(run(
            &driver,
            &dialog(json!({"prompt_text": "1234", "button": "OK"}))
        )),
        "Typed 4 characters into the alert \"Enter the code\". Then answering it failed: HTTP 500: boom."
    );
    // The text cannot go in: nothing is answered.
    let driver = alert_script("Enter the code", json!(["Cancel", "OK"]))
        .set(
            "POST session:/alert/text",
            Err(DriverError::Failed("HTTP 500: no text field".to_owned())),
        )
        .driver();
    assert_eq!(
        run(
            &driver,
            &dialog(json!({"prompt_text": "1234", "accept": true}))
        ),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: no text field".to_owned()
        )))
    );
    assert_eq!(
        posts(&driver)
            .iter()
            .map(|call| call.path.clone())
            .collect::<Vec<_>>(),
        [format!("{S}/alert/text")]
    );
}

#[test]
fn a_following_alert_is_reported_and_so_is_one_that_did_not_go_away() {
    // A permission, then the next permission.
    let driver = Script::new()
        .on("GET session:/alert/text", Ok(json!("First?")))
        .on("GET session:/alert/text", Ok(json!("Second?")))
        .on("GET session:/wda/alert/buttons", Ok(json!(["No", "Yes"])))
        .on(
            "GET session:/wda/alert/buttons",
            Ok(json!(["Later", "Allow"])),
        )
        .on("POST session:/alert/accept", none())
        .driver();
    let started = std::time::Instant::now();
    assert_eq!(
        run(&driver, &press("Yes")).unwrap(),
        "Pressed \"Yes\" on \"First?\".\nAnother alert is showing: \"Second?\". Buttons: Later, Allow."
    );
    assert!(started.elapsed() >= ALERT_SETTLE);

    // The same alert, still there: it was not answered.
    let driver = alert_script("Sure?", json!(["No", "Yes"])).driver();
    assert_eq!(
        run(&driver, &press("Yes")).unwrap(),
        "Pressed \"Yes\" on \"Sure?\".\nThe same alert is still showing: \"Sure?\". Buttons: No, Yes."
    );
    // ... also after a guess by position.
    let driver = alert_script("Sure?", json!(["No", "Yes"])).driver();
    assert_eq!(
        run(&driver, &dialog(json!({"accept": true}))).unwrap(),
        "Accepted the alert \"Sure?\". The button was guessed by position: the last one, most likely \"Yes\".\nThe same alert is still showing: \"Sure?\". Buttons: No, Yes."
    );

    // Buttons that cannot be read are said so, and the label is passed on for the device to judge.
    let driver = Script::new()
        .on("GET session:/alert/text", Ok(json!("Hm")))
        .on("GET session:/alert/text", no_such_alert())
        .on("POST session:/alert/accept", none())
        .driver();
    assert_eq!(
        run(&driver, &press("Whatever")).unwrap(),
        "Pressed \"Whatever\" on \"Hm\"."
    );
    assert_eq!(calls(&driver)[2].body, Some(json!({"name": "Whatever"})));
    let driver = Script::new()
        .on("GET session:/alert/text", Ok(json!("Hm")))
        .on("POST session:/alert/accept", none())
        .driver();
    assert_eq!(
        run(&driver, &press("Whatever")).unwrap(),
        "Pressed \"Whatever\" on \"Hm\".\nThe same alert is still showing: \"Hm\". Its buttons could not be read."
    );
}

#[test]
fn a_pressed_button_is_not_looked_after_when_there_is_no_time() {
    // The three calls fit; pressing takes most of the time, so the settling and the look after
    // it do not.
    let script = alert_script("Sure?", json!(["No", "Yes"]));
    let driver = FakeDriver::with_ios(FakeIos::new(move |method, route, _| {
        if method == "POST" {
            std::thread::sleep(Duration::from_millis(700));
        }
        script.answer(method, route)
    }));
    let said = run_within(&driver, &press("Yes"), NEVER, Duration::from_millis(1000)).unwrap();
    assert_eq!(said, "Pressed \"Yes\" on \"Sure?\".");
    assert_eq!(lines(&driver).len(), 3);
}

// ---------------------------------------------------------------------------------------------
// screen_device
// ---------------------------------------------------------------------------------------------

#[test]
fn the_device_is_locked_rotated_and_restyled() {
    let driver = Script::new()
        .on("POST /wda/lock", none())
        .on("POST session:/orientation", none())
        .on("POST /wda/device/appearance", none())
        .driver();
    assert_eq!(
        run(&driver, &call("screen_device", json!({"action": "lock"}))).unwrap(),
        "Locked the device."
    );
    // Rotating both ways: the device is turned a quarter counter-clockwise for landscape_left
    // (Home button on the right; WebDriverAgent's `LANDSCAPE`) and clockwise for landscape_right.
    for (value, wda) in [
        ("portrait", "PORTRAIT"),
        ("landscape_left", "LANDSCAPE"),
        ("landscape_right", "UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT"),
        (
            "portrait_upside_down",
            "UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN",
        ),
    ] {
        assert_eq!(
            run(
                &driver,
                &call("screen_device", json!({"action": "rotate", "value": value}))
            )
            .unwrap(),
            format!("Rotated to {value}.")
        );
        assert_eq!(
            calls(&driver).last().unwrap().body,
            Some(json!({"orientation": wda}))
        );
    }
    for value in ["light", "dark"] {
        assert_eq!(
            run(
                &driver,
                &call(
                    "screen_device",
                    json!({"action": "appearance", "value": value})
                )
            )
            .unwrap(),
            format!("Set the appearance to {value}.")
        );
        assert_eq!(
            calls(&driver).last().unwrap().body,
            Some(json!({"name": value}))
        );
    }
    let made = lines(&driver);
    assert_eq!(made[0], "POST /wda/lock");
    assert_eq!(made[1], format!("POST {S}/orientation"));
    assert_eq!(made[5], "POST /wda/device/appearance");
}

#[test]
fn unlocking_never_gets_past_a_passcode() {
    let unlock = call("screen_device", json!({"action": "unlock"}));
    // It worked: the device says it is unlocked.
    let driver = Script::new()
        .on("POST /wda/unlock", none())
        .on("GET /wda/locked", Ok(json!(false)))
        .driver();
    assert_eq!(run(&driver, &unlock).unwrap(), "The device is unlocked.");
    assert_eq!(lines(&driver), ["POST /wda/unlock", "GET /wda/locked"]);

    // WebDriverAgent presses Home and the device stays locked: a passcode is set.
    let passcode =
        "The device is locked with a passcode. Ask the user to unlock it; never type a passcode.";
    let driver = Script::new()
        .on("POST /wda/unlock", none())
        .on("GET /wda/locked", Ok(json!(true)))
        .driver();
    assert_eq!(said(run(&driver, &unlock)), passcode);
    // The same when WebDriverAgent itself gave up waiting.
    let driver = Script::new()
        .on(
            "POST /wda/unlock",
            Err(DriverError::Failed(
                "HTTP 500: timed out waiting for unlock".to_owned(),
            )),
        )
        .on("GET /wda/locked", Ok(json!(true)))
        .driver();
    assert_eq!(said(run(&driver, &unlock)), passcode);
    // Nothing more is tried: no keys, no buttons.
    assert_eq!(lines(&driver), ["POST /wda/unlock", "GET /wda/locked"]);

    // The call failed but the device is unlocked all the same.
    let driver = Script::new()
        .on(
            "POST /wda/unlock",
            Err(DriverError::Failed("HTTP 500: slow".to_owned())),
        )
        .on("GET /wda/locked", Ok(json!(false)))
        .driver();
    assert_eq!(run(&driver, &unlock).unwrap(), "The device is unlocked.");
    // The lock state cannot be read: the unlock is reported as sent.
    let driver = Script::new().on("POST /wda/unlock", none()).driver();
    assert_eq!(
        run(&driver, &unlock).unwrap(),
        "Sent the unlock, but the lock state could not be read; look at the screen to see whether it worked."
    );
    // ... and a failed unlock whose state cannot be read is that failure.
    let driver = Script::new()
        .on(
            "POST /wda/unlock",
            Err(DriverError::Failed("HTTP 500: slow".to_owned())),
        )
        .driver();
    assert_eq!(
        run(&driver, &unlock),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: slow".to_owned()
        )))
    );
    // A build without it, or a lost connection, ends it at once.
    let driver = Script::new().driver();
    assert_eq!(
        said(run(&driver, &unlock)),
        "This WebDriverAgent build has no /wda/unlock; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
    assert_eq!(lines(&driver).len(), 1);
    let driver = Script::new()
        .on(
            "POST /wda/unlock",
            Err(DriverError::Disconnected("gone".to_owned())),
        )
        .driver();
    assert!(matches!(
        run(&driver, &unlock),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
    assert_eq!(lines(&driver).len(), 1);
}

#[test]
fn the_keyboard_is_dismissed_by_a_key_or_the_model_is_told_to_tap_away() {
    let dismiss = call("screen_device", json!({"action": "dismiss_keyboard"}));
    let driver = Script::new()
        .on("POST session:/wda/keyboard/dismiss", none())
        .driver();
    assert_eq!(run(&driver, &dismiss).unwrap(), "Dismissed the keyboard.");
    assert_eq!(
        calls(&driver)[0].body,
        Some(
            json!({"keyNames": ["Done", "done", "Return", "return", "Search", "Go", "Hide keyboard"]})
        )
    );

    let driver = Script::new()
        .on(
            "POST session:/wda/keyboard/dismiss",
            Err(DriverError::Failed(
                "WebDriverAgent could not do /wda/keyboard/dismiss: HTTP 500: Did not know how to dismiss the keyboard. Try to dismiss it in the way supported by your application under test.".to_owned(),
            )),
        )
        .driver();
    assert_eq!(
        said(run(&driver, &dismiss)),
        "There is no key to dismiss this keyboard; tap outside the field instead."
    );
    // Another failure is its own.
    let driver = Script::new()
        .on(
            "POST session:/wda/keyboard/dismiss",
            Err(DriverError::Failed("HTTP 500: no keyboard".to_owned())),
        )
        .driver();
    assert_eq!(
        run(&driver, &dismiss),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: no keyboard".to_owned()
        )))
    );
}

#[test]
fn a_location_is_simulated_and_cleared() {
    let driver = Script::new()
        .on("POST session:/wda/simulatedLocation", none())
        .on("DELETE session:/wda/simulatedLocation", none())
        .driver();
    assert_eq!(
        run(
            &driver,
            &call(
                "screen_device",
                json!({"action": "location", "latitude": 52.52, "longitude": 13.405})
            )
        )
        .unwrap(),
        "Set the simulated location to 52.52, 13.405."
    );
    assert_eq!(
        calls(&driver)[0].body,
        Some(json!({"latitude": 52.52, "longitude": 13.405}))
    );
    assert_eq!(
        run(
            &driver,
            &call(
                "screen_device",
                json!({"action": "location", "latitude": -90, "longitude": 180})
            )
        )
        .unwrap(),
        "Set the simulated location to -90, 180."
    );
    assert_eq!(
        run(
            &driver,
            &call("screen_device", json!({"action": "clear_location"}))
        )
        .unwrap(),
        "Cleared the simulated location."
    );
    let last = calls(&driver).pop().unwrap();
    assert_eq!((last.method.as_str(), last.body), ("DELETE", None));

    let driver = Script::new().driver();
    assert_eq!(
        said(run(
            &driver,
            &call("screen_device", json!({"action": "location", "latitude": 1, "longitude": 2}))
        )),
        "This WebDriverAgent build has no /wda/simulatedLocation; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
}

// ---------------------------------------------------------------------------------------------
// screen_clipboard
// ---------------------------------------------------------------------------------------------

#[test]
fn text_is_put_on_the_clipboard_as_base64() {
    let driver = Script::new().on("POST /wda/setPasteboard", none()).driver();
    let write = |text: &str| call("screen_clipboard", json!({"action": "write", "text": text}));
    assert_eq!(
        run(&driver, &write("h\u{e9}llo \u{4e2d}\u{1f600}")).unwrap(),
        "Put 8 characters on the clipboard."
    );
    assert_eq!(
        calls(&driver)[0].body,
        Some(json!({
            "content": STANDARD.encode("h\u{e9}llo \u{4e2d}\u{1f600}".as_bytes()),
            "contentType": "plaintext"
        }))
    );
    assert_eq!(run(&driver, &write("")).unwrap(), "Emptied the clipboard.");
    assert_eq!(
        calls(&driver)[1].body,
        Some(json!({"content": "", "contentType": "plaintext"}))
    );
    let driver = Script::new().driver();
    assert_eq!(
        said(run(&driver, &write("x"))),
        "This WebDriverAgent build has no /wda/setPasteboard; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
    // A big one goes whole.
    let driver = Script::new().on("POST /wda/setPasteboard", none()).driver();
    let text = "x".repeat(CLIPBOARD_MAX);
    assert_eq!(
        run(&driver, &write(&text)).unwrap(),
        format!("Put {CLIPBOARD_MAX} characters on the clipboard.")
    );
}

#[test]
fn the_clipboard_is_read_back_as_text() {
    let read = call("screen_clipboard", json!({"action": "read"}));
    let encoded = |text: &str| Ok(json!(STANDARD.encode(text.as_bytes())));
    let driver = Script::new()
        .on(
            "POST /wda/getPasteboard",
            encoded("h\u{e9}llo\r\nworld\u{7}"),
        )
        .driver();
    assert_eq!(
        run(&driver, &read).unwrap(),
        "Clipboard (11 characters):\nh\u{e9}llo\nworld"
    );
    assert_eq!(
        calls(&driver)[0].body,
        Some(json!({"contentType": "plaintext"}))
    );
    assert_eq!(calls(&driver)[0].method, "POST");
    assert_eq!(calls(&driver)[0].path, "/wda/getPasteboard");

    for empty in [json!(""), Value::Null] {
        let driver = Script::new()
            .on("POST /wda/getPasteboard", Ok(empty))
            .driver();
        assert_eq!(
            run(&driver, &read).unwrap(),
            "The clipboard is empty (on a real device this can also mean WebDriverAgent could not read it)."
        );
    }

    // Cut at 64 KiB on a character boundary, with the full count said.
    let driver = Script::new()
        .on(
            "POST /wda/getPasteboard",
            encoded(&"\u{4e2d}".repeat(30_000)),
        )
        .driver();
    let said = run(&driver, &read).unwrap();
    let (head, body) = said.split_once('\n').unwrap();
    assert_eq!(
        head,
        "Clipboard (30000 characters; the first 65536 bytes are shown):"
    );
    assert_eq!(body.len(), 65535);
    assert!(body.chars().all(|character| character == '\u{4e2d}'));

    let driver = Script::new()
        .on("POST /wda/getPasteboard", Ok(json!("%%%")))
        .driver();
    assert_eq!(
        said_of(run(&driver, &read)),
        "WebDriverAgent's clipboard reply is not valid base64."
    );
    let driver = Script::new().driver();
    assert_eq!(
        said_of(run(&driver, &read)),
        "This WebDriverAgent build has no /wda/getPasteboard; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
}

fn said_of(result: Result<String, OpError>) -> String {
    said(result)
}

// ---------------------------------------------------------------------------------------------
// Clearing a field
// ---------------------------------------------------------------------------------------------

const FIELD: &str = "5F1B0A2C-3D4E-4F50-8A61-7B8C9D0E1F2A";
const OTHER_FIELD: &str = "9A8B7C6D-5E4F-4A3B-9C2D-1E0F9A8B7C6D";

/// A reply to `POST /elements` finding the elements `ids`, each named both ways WebDriverAgent
/// names one.
fn found(ids: &[&str]) -> Value {
    Value::Array(
        ids.iter()
            .map(|id| json!({"element-6066-11e4-a52e-4f735466cecf": id, "ELEMENT": id}))
            .collect(),
    )
}

fn clear_within(
    driver: &Arc<FakeDriver>,
    role: &str,
    value: &str,
    label: Option<&str>,
    within: Duration,
) -> Result<(), OpError> {
    clear_field(
        driver.as_ref(),
        &Step::for_tests(NEVER, within),
        role,
        value,
        label,
    )
}

fn clear(
    driver: &Arc<FakeDriver>,
    role: &str,
    value: &str,
    label: Option<&str>,
) -> Result<(), OpError> {
    clear_within(driver, role, value, label, Duration::from_secs(25))
}

/// A screen with one element found for any search and a clear that works.
fn one_field() -> Arc<FakeDriver> {
    Script::new()
        .on("POST session:/elements", Ok(found(&[FIELD])))
        .on(&format!("POST session:/element/{FIELD}/clear"), none())
        .driver()
}

#[test]
fn a_field_that_is_exactly_one_element_is_found_and_cleared() {
    let driver = one_field();
    clear(&driver, "TextField", "hello", Some("Name")).unwrap();
    assert_eq!(
        lines(&driver),
        [
            format!("POST {S}/elements"),
            format!("POST {S}/element/{FIELD}/clear"),
        ]
    );
    let made = calls(&driver);
    assert_eq!(
        made[0].body,
        Some(json!({
            "using": "predicate string",
            "value": "type == 'XCUIElementTypeTextField' AND value == 'hello' AND label == 'Name'"
        }))
    );
    assert_eq!(made[1].body, Some(json!({})));
    // Each call is bounded by its route's own limit.
    assert!(made[0].timeout <= FIND_CALL && made[1].timeout <= CLEAR_CALL);

    // Without a label the field is found by its type and text alone.
    let driver = one_field();
    clear(&driver, "TextView", "two lines", None).unwrap();
    assert_eq!(
        calls(&driver)[0].body,
        Some(json!({
            "using": "predicate string",
            "value": "type == 'XCUIElementTypeTextView' AND value == 'two lines'"
        }))
    );
    let driver = one_field();
    clear(&driver, "SearchField", "cats", None).unwrap();
    assert_eq!(
        calls(&driver)[0].body.as_ref().unwrap()["value"],
        json!("type == 'XCUIElementTypeSearchField' AND value == 'cats'")
    );

    // A label that is empty is a label: the field has none.
    let driver = one_field();
    clear(&driver, "TextField", "x", Some("")).unwrap();
    assert_eq!(
        calls(&driver)[0].body.as_ref().unwrap()["value"],
        json!("type == 'XCUIElementTypeTextField' AND value == 'x' AND label == ''")
    );

    // The older name of an element is taken as well.
    let driver = Script::new()
        .on("POST session:/elements", Ok(json!([{"ELEMENT": FIELD}])))
        .on(&format!("POST session:/element/{FIELD}/clear"), none())
        .driver();
    clear(&driver, "TextField", "hello", None).unwrap();
    assert_eq!(lines(&driver)[1], format!("POST {S}/element/{FIELD}/clear"));
}

#[test]
fn nothing_is_cleared_unless_exactly_one_field_matches() {
    // None: the screen is not what the snapshot said.
    let driver = Script::new()
        .on("POST session:/elements", Ok(json!([])))
        .on(&format!("POST session:/element/{FIELD}/clear"), none())
        .driver();
    assert_eq!(
        refused(clear(&driver, "TextField", "hello", None)),
        "No field on the screen has exactly that text, so nothing was cleared. The screen may have changed since the snapshot: take a new preview_snapshot."
    );
    assert_eq!(lines(&driver), [format!("POST {S}/elements")]);

    // Several: which one is not certain.
    let driver = Script::new()
        .on("POST session:/elements", Ok(found(&[FIELD, OTHER_FIELD])))
        .on(&format!("POST session:/element/{FIELD}/clear"), none())
        .on(
            &format!("POST session:/element/{OTHER_FIELD}/clear"),
            none(),
        )
        .driver();
    assert_eq!(
        refused(clear(&driver, "TextField", "hello", None)),
        "2 fields on the screen have exactly that text, so which one to clear is not certain and nothing was cleared. Tap the one you mean and press Backspace with preview_input instead."
    );
    assert_eq!(lines(&driver), [format!("POST {S}/elements")]);

    // A reply that is not a list is not one match.
    for reply in [Value::Null, json!({"ELEMENT": FIELD}), json!("x")] {
        let driver = Script::new()
            .on("POST session:/elements", Ok(reply.clone()))
            .driver();
        assert_eq!(
            refused(clear(&driver, "TextField", "hello", None)),
            "WebDriverAgent's answer to the search was not a list of elements; nothing was cleared.",
            "{reply}"
        );
        assert_eq!(lines(&driver).len(), 1);
    }
}

#[test]
fn the_text_it_looks_for_is_escaped_for_a_predicate() {
    // A quote or a backslash in the text cannot end its literal: each is escaped with a backslash.
    assert_eq!(predicate_literal("plain"), "'plain'");
    assert_eq!(predicate_literal("it's"), "'it\\'s'");
    assert_eq!(predicate_literal("a\\b"), "'a\\\\b'");
    assert_eq!(predicate_literal("\\'"), "'\\\\\\''");
    assert_eq!(predicate_literal(""), "''");
    assert_eq!(
        predicate_literal("\u{e9}\u{4e2d}\"x\""),
        "'\u{e9}\u{4e2d}\"x\"'"
    );

    let driver = one_field();
    clear(&driver, "TextField", "it's a \\ test", Some("O'Brien\\")).unwrap();
    assert_eq!(
        calls(&driver)[0].body.as_ref().unwrap()["value"],
        json!("type == 'XCUIElementTypeTextField' AND value == 'it\\'s a \\\\ test' AND label == 'O\\'Brien\\\\'")
    );

    // Text written to turn the predicate into another stays one literal in each place.
    let hostile = "x' OR value != '";
    let driver = one_field();
    clear(&driver, "TextField", hostile, Some(hostile)).unwrap();
    assert_eq!(
        calls(&driver)[0].body.as_ref().unwrap()["value"],
        json!("type == 'XCUIElementTypeTextField' AND value == 'x\\' OR value != \\'' AND label == 'x\\' OR value != \\''")
    );
}

#[test]
fn an_element_id_that_is_not_safe_for_a_route_is_never_spliced_into_one() {
    for id in [
        "../../wda/shutdown",
        "a/b",
        "a b",
        "a.b",
        "a?b",
        "%2e%2e",
        "",
        &"a".repeat(65),
        "id\n",
        "\u{e9}",
    ] {
        let driver = Script::new()
            .on("POST session:/elements", Ok(found(&[id])))
            .driver();
        assert_eq!(
            refused(clear(&driver, "TextField", "hello", None)),
            "WebDriverAgent found the field but named it in a way that cannot be used safely in a route, so nothing was cleared.",
            "{id:?}"
        );
        assert_eq!(lines(&driver).len(), 1, "{id:?}: {:?}", lines(&driver));
    }
    // An element with no id at all is the same.
    for reply in [json!([{}]), json!([{"ELEMENT": 5}]), json!([null])] {
        let driver = Script::new()
            .on("POST session:/elements", Ok(reply.clone()))
            .driver();
        assert!(
            refused(clear(&driver, "TextField", "hello", None)).contains("cannot be used safely"),
            "{reply}"
        );
        assert_eq!(lines(&driver).len(), 1);
    }
    // The strictness is a session id's.
    for good in [FIELD, "abc123", &"a".repeat(64)] {
        let driver = Script::new()
            .on("POST session:/elements", Ok(found(&[good])))
            .on(&format!("POST session:/element/{good}/clear"), none())
            .driver();
        clear(&driver, "TextField", "hello", None)
            .unwrap_or_else(|error| panic!("{good}: {error:?}"));
    }
}

#[test]
fn only_a_text_field_is_cleared_and_never_a_password_field() {
    let driver = one_field();
    // A password is the user's: refused before anything is asked.
    assert_eq!(
        refused(clear(&driver, "SecureTextField", "hunter2", Some("Password"))),
        "That is a password field. Its text is hidden, so it cannot be found to clear, and a password is the user's to enter; ask the user to clear it."
    );
    for role in [
        "Button",
        "StaticText",
        "Other",
        "TextFieldX",
        "textfield",
        "",
        "TextField' OR 1 == 1",
    ] {
        let refusal = refused(clear(&driver, role, "hello", None));
        assert!(
            refusal.starts_with(
                "Only a text field, text view or search field can be cleared this way, not a "
            ),
            "{role:?}: {refusal}"
        );
        assert!(!refusal.contains('\n'), "{refusal}");
    }
    // Nothing to find it by.
    assert_eq!(
        refused(clear(&driver, "TextField", "", None)),
        "The field's text is empty: there is nothing to clear, and nothing to find it by."
    );
    // Text with line breaks or other control characters cannot be told in a predicate.
    for (value, label) in [
        ("two\nlines", None),
        ("tab\there", None),
        ("bell\u{7}", None),
        ("fine", Some("two\nlines")),
        ("fine", Some("bell\u{7}")),
    ] {
        assert_eq!(
            refused(clear(&driver, "TextView", value, label)),
            "The field's text has line breaks or other control characters, which a search by its text cannot match, so it was not cleared.",
            "{value:?} {label:?}"
        );
    }
    assert!(lines(&driver).is_empty(), "{:?}", lines(&driver));
}

#[test]
fn clearing_a_field_keeps_to_the_calls_time_and_the_screens_kind() {
    // No time: nothing is asked.
    let driver = one_field();
    assert_eq!(
        refused(clear_within(
            &driver,
            "TextField",
            "hello",
            None,
            Duration::from_millis(100)
        )),
        "Ran out of this call's time before asking the device."
    );
    assert!(lines(&driver).is_empty());

    // The time runs out between the search and the clear: the field is not cleared.
    let script = Script::new()
        .on("POST session:/elements", Ok(found(&[FIELD])))
        .on(&format!("POST session:/element/{FIELD}/clear"), none());
    let driver = FakeDriver::with_ios(FakeIos::new(move |method, route, _| {
        if route_key(method, route) == "POST session:/elements" {
            std::thread::sleep(Duration::from_millis(700));
        }
        script.answer(method, route)
    }));
    assert_eq!(
        refused(clear_within(
            &driver,
            "TextField",
            "hello",
            None,
            Duration::from_millis(800)
        )),
        "Ran out of this call's time before asking the device."
    );
    assert_eq!(lines(&driver), [format!("POST {S}/elements")]);

    // Another kind of screen.
    let driver = FakeDriver::touch();
    assert_eq!(
        refused(clear(&driver, "TextField", "hello", None)),
        "this screen is not an iPhone or iPad"
    );

    // A build without the routes says so; a failure of the clear is the driver's own.
    let driver = Script::new().driver();
    assert_eq!(
        refused(clear(&driver, "TextField", "hello", None)),
        "This WebDriverAgent build has no /elements; it is too old (appium/WebDriverAgent 6 or newer has it)."
    );
    let driver = Script::new()
        .on("POST session:/elements", Ok(found(&[FIELD])))
        .driver();
    assert_eq!(
        refused(clear(&driver, "TextField", "hello", None)),
        format!("This WebDriverAgent build has no /element/{FIELD}/clear; it is too old (appium/WebDriverAgent 6 or newer has it).")
    );
    let driver = Script::new()
        .on("POST session:/elements", Ok(found(&[FIELD])))
        .on(
            &format!("POST session:/element/{FIELD}/clear"),
            Err(DriverError::Failed(
                "HTTP 500: the element is not a text field".to_owned(),
            )),
        )
        .driver();
    assert_eq!(
        clear(&driver, "TextField", "hello", None),
        Err(OpError::Driver(DriverError::Failed(
            "HTTP 500: the element is not a text field".to_owned()
        )))
    );
    let driver = Script::new()
        .on(
            "POST session:/elements",
            Err(DriverError::Disconnected("gone".to_owned())),
        )
        .driver();
    assert!(matches!(
        clear(&driver, "TextField", "hello", None),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
}

// ---------------------------------------------------------------------------------------------
// Through the tool layer
// ---------------------------------------------------------------------------------------------

mod end_to_end {
    use std::path::Path;

    use super::*;
    use crate::{
        model::{ToolExecutionRequest, ToolExecutionResponse},
        preview::ControlApproval,
        remote_control::{fake, place::ControlEnvironment},
        state::AppState,
        workspace_set::WorkspaceSet,
    };

    const CONVERSATION: &str = "conversation-ios-test";
    const IPHONE: &str = r#"{"name":"iphone","type":"remote-control","protocol":"wda"}"#;
    const READ_ONLY_IPHONE: &str =
        r#"{"name":"iphone","type":"remote-control","protocol":"wda","readOnly":true}"#;
    const DESK: &str =
        r#"{"name":"desk","type":"remote-control","protocol":"vnc","machine":"mac"}"#;

    /// A workspace whose `launch.json` holds `entries`, and the host state its screens are
    /// registered in, as a model's `preview_start` would register one.
    struct Bench {
        workspace: tempfile::TempDir,
        app_data: tempfile::TempDir,
        state: AppState,
    }

    impl Bench {
        fn new(entries: &str) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let mewrk = workspace.path().join(".mewrk");
            std::fs::create_dir_all(&mewrk).unwrap();
            std::fs::write(
                mewrk.join("launch.json"),
                format!(r#"{{"configurations":[{entries}]}}"#),
            )
            .unwrap();
            Self {
                workspace,
                app_data: tempfile::tempdir().unwrap(),
                state: AppState::default(),
            }
        }

        fn path(&self) -> &Path {
            self.workspace.path()
        }

        fn start(&self, name: &str, driver: Arc<FakeDriver>) {
            let environment = ControlEnvironment {
                launch_port_wait: Duration::ZERO,
                ..ControlEnvironment::this_computer()
            }
            .with_connect(fake::ready(driver));
            self.start_in(name, &environment);
        }

        /// [`Self::start`] for a screen on the machine `mac`, as a desktop's is.
        fn start_on_a_machine(&self, name: &str, driver: Arc<FakeDriver>) {
            let environment = ControlEnvironment {
                machines: Ok(vec![fake::machine("m1", "mac", "203.0.113.7")]),
                launch_port_wait: Duration::ZERO,
                ..ControlEnvironment::this_computer()
            }
            .with_route_check(Arc::new(|_| Ok(())))
            .with_connect(fake::ready(driver));
            self.start_in(name, &environment);
        }

        fn start_in(&self, name: &str, environment: &ControlEnvironment) {
            crate::preview::start_with_approval(
                &self.state.preview_servers,
                self.path(),
                Some(name),
                Some(CONVERSATION),
                environment,
                &ControlApproval::Person,
            )
            .expect("the screen starts");
        }

        fn call(&self, tool: &str, input: Value) -> ToolExecutionResponse {
            let request = ToolExecutionRequest {
                conversation_id: CONVERSATION.into(),
                workspace_path: self.path().to_string_lossy().into_owned(),
                tool_name: tool.into(),
                input: input.as_object().cloned().unwrap_or_default(),
            };
            crate::tool_executor::execute_with_scope_and_attachments_verified(
                request,
                &self.state,
                ExecutionScope::workspace_only(self.path()),
                Some(self.app_data.path()),
                &crate::cancel::CancelSignal::default(),
                &WorkspaceSet::local_root(self.path().to_string_lossy().into_owned()),
                None,
                &PromptProfile::builtin(),
            )
            .result
        }
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            self.state.preview_servers.stop_all();
        }
    }

    #[test]
    fn screen_device_info_answers_through_the_tool_layer() {
        let bench = Bench::new(IPHONE);
        let driver = quiet_phone().driver();
        bench.start("iphone", driver.clone());
        let info = bench.call("screen_device", json!({}));
        assert!(info.success, "{}", info.output);
        assert!(
            info.output.starts_with(
                "device:\n  name: Sam's iPhone\n  model: iPhone\n  system: iOS 17.5\n"
            ),
            "{}",
            info.output
        );
        assert!(
            info.output
                .contains("\n  app in front: Settings (com.apple.Preferences)\n"),
            "{}",
            info.output
        );
        assert!(info.output.ends_with("\nalert: none"), "{}", info.output);
        assert!(!info.output.contains("127.0.0.1"), "{}", info.output);

        // The sections asked for, the screen named the way a model names it.
        let info = bench.call(
            "screen_device",
            json!({"serverId": "iphone", "action": "info", "sections": ["power"]}),
        );
        assert!(info.success, "{}", info.output);
        assert_eq!(
            info.output,
            "power:\n  lock state: unlocked\n  battery: 80%, charging\nalert: none"
        );

        // A call that does not parse never reaches the device.
        let before = lines(&driver).len();
        let bad = bench.call("screen_device", json!({"sections": ["notifications"]}));
        assert!(!bad.success);
        assert!(
            bad.output.starts_with(
                "\"notifications\" is Android-only, and this screen is an iPhone or iPad"
            ),
            "{}",
            bad.output
        );
        let unknown = bench.call("screen_device", json!({"app": "a.b"}));
        assert_eq!(
            unknown.output,
            "screen_device has no parameter \"app\"; it takes serverId, action, value, sections, latitude and longitude."
        );
        let wake = bench.call("screen_device", json!({"action": "wake"}));
        assert_eq!(
            wake.output,
            "An iPhone or iPad has no wake on its own; unlock wakes it and swipes the lock screen away (only without a passcode)."
        );
        assert_eq!(lines(&driver).len(), before);
        // No input was sent to the screen: reading is not touching.
        assert!(driver.inputs().is_empty(), "{:?}", driver.inputs());
    }

    #[test]
    fn screen_app_runs_through_the_tool_layer() {
        let bench = Bench::new(IPHONE);
        let driver = quiet_phone()
            .on(
                "GET session:/wda/apps/list",
                Ok(json!([{"pid": 1, "bundleId": "com.apple.Preferences"}])),
            )
            .on("POST session:/wda/apps/terminate", Ok(json!(true)))
            .on("POST session:/wda/apps/launch", none())
            .driver();
        bench.start("iphone", driver.clone());

        let list = bench.call("screen_app", json!({}));
        assert!(list.success, "{}", list.output);
        assert_eq!(
            list.output,
            format!("Running apps: 1 (showing 1). {RUNNING_ONLY}\ncom.apple.Preferences")
        );
        let foreground = bench.call("screen_app", json!({"action": "foreground"}));
        assert!(foreground.success, "{}", foreground.output);
        assert_eq!(
            foreground.output,
            "app in front: Settings (com.apple.Preferences)\nlock state: unlocked"
        );
        let state = bench.call("screen_app", json!({"action": "info", "app": "a.b"}));
        assert_eq!(state.output, "app a.b: foreground");
        let restarted = bench.call(
            "screen_app",
            json!({"serverId": "iphone", "action": "launch", "app": "a.b", "restart": true}),
        );
        assert!(restarted.success, "{}", restarted.output);
        assert_eq!(
            restarted.output,
            "Restarted a.b: quit it and launched it again."
        );

        // Android's parameters are refused by name, and nothing reaches the device.
        let before = lines(&driver).len();
        let scope = bench.call("screen_app", json!({"scope": "all"}));
        assert!(!scope.success);
        assert!(
            scope
                .output
                .starts_with("\"scope\" is Android-only, and this screen is an iPhone or iPad"),
            "{}",
            scope.output
        );
        let activity = bench.call(
            "screen_app",
            json!({"action": "launch", "app": "a.b", "activity": ".Main"}),
        );
        assert!(!activity.success);
        assert_eq!(lines(&driver).len(), before);
        for output in [&list.output, &foreground.output, &restarted.output] {
            assert!(!output.contains("127.0.0.1"), "{output}");
        }
    }

    #[test]
    fn preview_dialog_press_answers_through_the_tool_layer() {
        let bench = Bench::new(IPHONE);
        // The alert stays until a button is pressed.
        let pressed_flag = Arc::new(AtomicBool::new(false));
        let flag = pressed_flag.clone();
        let driver = FakeDriver::with_ios(FakeIos::new(move |method, route, _| {
            match (method, route) {
                ("GET", WdaRoute::Session("/alert/text")) if flag.load(Ordering::SeqCst) => {
                    no_such_alert()
                }
                ("GET", WdaRoute::Session("/alert/text")) => Ok(json!(MAPS)),
                ("GET", WdaRoute::Session("/wda/alert/buttons")) => {
                    Ok(json!(["Don\u{2019}t Allow", "Allow While Using App"]))
                }
                ("POST", WdaRoute::Session("/alert/accept")) => {
                    flag.store(true, Ordering::SeqCst);
                    none()
                }
                _ => Err(DriverError::Unsupported("no such route".to_owned())),
            }
        }));
        bench.start("iphone", driver.clone());

        // A label the alert does not have lists the ones it has, and presses nothing.
        let wrong = bench.call("preview_dialog", json!({"button": "Allow"}));
        assert!(!wrong.success);
        assert_eq!(
            wrong.output,
            format!("No button \"Allow\" on the alert \"{MAPS_IN_AN_ERROR}\"; its buttons: {DONT_ALLOW_IN_AN_ERROR}, Allow While Using App.")
        );
        assert!(!lines(&driver).iter().any(|line| line.starts_with("POST")));

        let pressed = bench.call("preview_dialog", json!({"button": "Allow While Using App"}));
        assert!(pressed.success, "{}", pressed.output);
        assert_eq!(
            pressed.output,
            format!("Pressed \"Allow While Using App\" on \"{MAPS}\".")
        );
        let made = posts(&driver);
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].path, format!("{S}/alert/accept"));
        assert_eq!(made[0].body, Some(json!({"name": "Allow While Using App"})));
        assert!(!pressed.output.contains("127.0.0.1"));

        // Asked for with nothing to answer with, or with two answers, it is refused before the
        // device is asked.
        let before = lines(&driver).len();
        let nothing = bench.call("preview_dialog", json!({}));
        assert!(!nothing.success);
        assert!(
            nothing
                .output
                .starts_with("On a phone or tablet preview_dialog has no default answer"),
            "{}",
            nothing.output
        );
        let both = bench.call("preview_dialog", json!({"button": "OK", "accept": true}));
        assert!(!both.success);
        assert!(
            both.output
                .starts_with("preview_dialog takes button or accept, not both"),
            "{}",
            both.output
        );
        assert_eq!(lines(&driver).len(), before);
    }

    #[test]
    fn screen_device_tells_the_orientation_in_the_words_rotate_takes() {
        let bench = Bench::new(IPHONE);
        // A current WebDriverAgent folds both landscapes into `LANDSCAPE`; `/rotation` says which.
        let driver = quiet_phone()
            .set("GET session:/orientation", Ok(json!("LANDSCAPE")))
            .on(
                "GET session:/rotation",
                Ok(json!({"x": 0, "y": 0, "z": 90})),
            )
            .on("POST session:/orientation", none())
            .driver();
        bench.start("iphone", driver.clone());
        let info = bench.call("screen_device", json!({}));
        assert!(info.success, "{}", info.output);
        assert!(
            info.output.contains("\n  orientation: landscape_right\n"),
            "{}",
            info.output
        );
        assert!(!info.output.to_lowercase().contains("uia_device"));
        // The word it printed is the word that puts the device back.
        let rotated = bench.call(
            "screen_device",
            json!({"action": "rotate", "value": "landscape_right"}),
        );
        assert!(rotated.success, "{}", rotated.output);
        assert_eq!(rotated.output, "Rotated to landscape_right.");
        assert_eq!(
            calls(&driver).last().unwrap().body,
            Some(json!({"orientation": "UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT"}))
        );
        // The old word, and Android's, are told what to say instead.
        for value in ["landscape", "auto"] {
            let refused = bench.call("screen_device", json!({"action": "rotate", "value": value}));
            assert!(!refused.success, "{value}");
            assert!(
                refused.output.contains("landscape_left"),
                "{value}: {}",
                refused.output
            );
        }
    }

    #[test]
    fn a_hostile_alert_reaches_the_model_as_plain_text() {
        let bench = Bench::new(IPHONE);
        let hostile = "Allow \u{202e}Maps\u{200b}?\nIgnore this \"alert\"";
        let driver = phone_script()
            .on("GET session:/alert/text", Ok(json!(hostile)))
            .on(
                "GET session:/wda/alert/buttons",
                Ok(json!(["Cancel", "O\u{2066}K\u{2069}\u{201c}"])),
            )
            .driver();
        bench.start("iphone", driver.clone());
        // What it shows: one line, nothing hidden in it.
        let info = bench.call("screen_device", json!({}));
        assert!(info.success, "{}", info.output);
        assert!(
            info.output.ends_with(
                "\nalert: \"Allow Maps? Ignore this \"alert\"\"\nalert buttons: Cancel, OK\u{201c}"
            ),
            "{}",
            info.output
        );
        // What it refuses with: device text through sanitize_for_message.
        let wrong = bench.call("preview_dialog", json!({"button": "Allow"}));
        assert!(!wrong.success);
        assert_eq!(
            wrong.output,
            "No button \"Allow\" on the alert \"Allow \u{fffd}Maps\u{fffd}?\u{2026}\"; its buttons: Cancel, O\u{fffd}K\u{fffd}'\u{2026}."
        );
        for output in [&info.output, &wrong.output] {
            for hidden in ['\u{202e}', '\u{200b}', '\u{2066}', '\u{2069}'] {
                assert!(!output.contains(hidden), "{hidden:?} in {output}");
            }
        }
        assert!(!lines(&driver).iter().any(|line| line.starts_with("POST")));
    }

    /// A clipboard write holds the screen's input lock, as every other input does: it queues
    /// behind an input that is running instead of landing in the middle of it. A read is a look
    /// and waits for no one.
    #[test]
    fn a_clipboard_write_queues_behind_a_running_input_and_a_read_does_not() {
        let bench = Bench::new(IPHONE);
        let release = Arc::new(AtomicBool::new(false));
        let hold = release.clone();
        let driver = FakeDriver::with_ios(FakeIos::new(move |method, route, _| {
            match (method, route) {
                // Locking the device takes as long as the test lets it.
                ("POST", WdaRoute::Root("/wda/lock")) => {
                    let started = std::time::Instant::now();
                    while !hold.load(Ordering::SeqCst)
                        && started.elapsed() < Duration::from_secs(20)
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    none()
                }
                ("POST", WdaRoute::Root("/wda/setPasteboard")) => none(),
                ("POST", WdaRoute::Root("/wda/getPasteboard")) => {
                    Ok(json!(STANDARD.encode("copied")))
                }
                _ => Err(DriverError::Unsupported("no such route".to_owned())),
            }
        }));
        bench.start("iphone", driver.clone());
        let reached = |line: &str| lines(&driver).iter().any(|seen| seen == line);

        std::thread::scope(|scope| {
            let locking = scope.spawn(|| bench.call("screen_device", json!({"action": "lock"})));
            let started = std::time::Instant::now();
            while !reached("POST /wda/lock") {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "the lock never started"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let writing = scope.spawn(|| {
                bench.call(
                    "screen_clipboard",
                    json!({"action": "write", "text": "to paste"}),
                )
            });
            // A look goes straight through the running input.
            let read = bench.call("screen_clipboard", json!({"action": "read"}));
            assert!(read.success, "{}", read.output);
            assert_eq!(read.output, "Clipboard (6 characters):\ncopied");
            // The write has had every chance to run, and has not.
            std::thread::sleep(Duration::from_millis(500));
            assert!(
                !reached("POST /wda/setPasteboard"),
                "the write landed in the middle of the running input: {:?}",
                lines(&driver)
            );
            release.store(true, Ordering::SeqCst);
            let locked = locking.join().unwrap();
            let written = writing.join().unwrap();
            assert!(locked.success, "{}", locked.output);
            assert!(written.success, "{}", written.output);
            assert_eq!(written.output, "Put 8 characters on the clipboard.");
        });
        let seen = lines(&driver);
        let position = |line: &str| seen.iter().position(|seen| seen == line).unwrap();
        assert!(
            position("POST /wda/lock") < position("POST /wda/setPasteboard"),
            "{seen:?}"
        );
    }

    #[test]
    fn a_read_only_screen_lets_the_ios_tools_look_and_nothing_more() {
        let bench = Bench::new(READ_ONLY_IPHONE);
        let driver = quiet_phone()
            .on(
                "GET session:/wda/apps/list",
                Ok(json!([{"pid": 1, "bundleId": "com.apple.Preferences"}])),
            )
            .on(
                "POST /wda/getPasteboard",
                Ok(json!(STANDARD.encode("copied"))),
            )
            .on("POST /wda/setPasteboard", none())
            .on("POST session:/alert/accept", none())
            .driver();
        bench.start("iphone", driver.clone());
        // The looks: the device's info, the apps, the app in front, an app's state, the clipboard.
        assert!(bench.call("screen_device", json!({})).success);
        assert!(bench.call("screen_app", json!({})).success);
        assert!(
            bench
                .call("screen_app", json!({"action": "foreground"}))
                .success
        );
        assert!(
            bench
                .call("screen_app", json!({"action": "info", "app": "a.b"}))
                .success
        );
        let read = bench.call("screen_clipboard", json!({"action": "read"}));
        assert_eq!(read.output, "Clipboard (6 characters):\ncopied");
        for (tool, input) in [
            ("screen_clipboard", json!({"action": "write", "text": "x"})),
            ("preview_dialog", json!({"accept": true})),
            ("preview_dialog", json!({"button": "OK"})),
            ("preview_dialog", json!({"prompt_text": "1234"})),
            ("screen_app", json!({"action": "launch", "app": "a.b"})),
            ("screen_app", json!({"action": "stop", "app": "a.b"})),
            ("screen_app", json!({"action": "open_url", "url": "x://y"})),
            ("screen_device", json!({"action": "lock"})),
            (
                "screen_device",
                json!({"action": "rotate", "value": "portrait"}),
            ),
        ] {
            let refused = bench.call(tool, input.clone());
            assert!(!refused.success, "{tool} {input}");
            assert!(
                refused.output.contains("is read-only"),
                "{tool}: {}",
                refused.output
            );
        }
        // Only the looks reached the device: the clipboard read and an app's state are its POSTs.
        assert!(
            !lines(&driver).iter().any(|line| line.starts_with("POST")
                && !line.contains("getPasteboard")
                && !line.contains("apps/state")),
            "{:?}",
            lines(&driver)
        );
    }

    #[test]
    fn an_ios_tool_on_a_screen_that_is_not_an_iphone_says_where_it_is() {
        let bench = Bench::new(DESK);
        let screen = FakeDriver::with_desktop(fake::FakeDesktop::new(
            crate::remote_control::driver::ClipboardChannel::Extended,
        ));
        bench.start_on_a_machine("desk", screen.clone());
        let wrong = bench.call("screen_app", json!({"serverId": "desk"}));
        assert!(!wrong.success);
        assert_eq!(
            wrong.output,
            "\"desk\" is a desktop, and screen_app works on phones or tablets only. A desktop's apps are out of reach over VNC; use preview_screenshot and preview_input."
        );
        let wrong = bench.call(
            "screen_device",
            json!({"serverId": "desk", "action": "rotate", "value": "portrait"}),
        );
        assert!(!wrong.success);
        assert!(
            wrong.output.starts_with(
                "\"desk\" is a desktop, and screen_device works on phones or tablets only."
            ),
            "{}",
            wrong.output
        );
        // Without a name there is no phone or tablet to find.
        let none = bench.call("screen_device", json!({"action": "lock"}));
        assert!(
            none.output
                .starts_with("No phone or tablet is connected as a remote-control screen."),
            "{}",
            none.output
        );
        // A dialog is not a desktop's either.
        let dialog = bench.call(
            "preview_dialog",
            json!({"serverId": "desk", "accept": true}),
        );
        assert!(!dialog.success);
        assert!(dialog.output.contains("desktop"), "{}", dialog.output);
        // Nothing reached the desktop.
        assert!(screen.inputs().is_empty(), "{:?}", screen.inputs());
        assert!(screen.desktop_fake().writes().is_empty());
    }
}
