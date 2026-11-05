//! The Android tools against scripted devices: parsing with the schema's bounds, the commands
//! as the device's shell would read them, the parsers on samples of what two or more Android
//! versions print, every tool's run on [`FakeAndroid`], and the tool layer end to end.
//!
//! The samples are written from the formats of AOSP's dump code and of real devices' output
//! (research-android.md §2): each says which Android version it stands for.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
};

use serde_json::json;

use super::*;
use crate::{
    cancel::CancelSignal,
    model::ToolExecutionRequest,
    path_guard::ExecutionScope,
    prompt_profile::PromptProfile,
    remote_control::{
        driver::{DriverInfo, DriverResult, Frame, Rect},
        fake::{self, FakeAndroid, FakeDriver},
        keys::Key,
        place::ControlEnvironment,
        session::ConnectFn,
    },
    run_environment::ShellRunner,
    state::AppState,
    tool_surface::ToolVariant,
    workspace_set::WorkspaceSet,
};

// ------------------------------------------------------------- helpers

fn object(value: Value) -> JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

/// A workspace of this computer, and what a call may read of it.
struct Files {
    dir: tempfile::TempDir,
    files: CallFiles,
}

impl Files {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let files = CallFiles::local(dir.path(), ExecutionScope::workspace_only(dir.path()));
        Self { dir, files }
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.dir.path().join(name), bytes).unwrap();
    }
}

fn parse_in(files: &Files, tool: &str, input: Value) -> Result<AndroidCall, String> {
    AndroidCall::parse(tool, &object(input), &files.files)
}

fn parse(tool: &str, input: Value) -> Result<AndroidCall, String> {
    parse_in(&Files::new(), tool, input)
}

fn refused(tool: &str, input: Value) -> String {
    parse(tool, input).unwrap_err()
}

/// An Android device on API `sdk` whose shell answers by `script`.
fn phone(
    sdk: u32,
    script: impl Fn(&str) -> ShellResult + Send + Sync + 'static,
) -> Arc<FakeDriver> {
    FakeDriver::with_android(FakeAndroid::new(sdk, script))
}

fn run_on(driver: &FakeDriver, tool: &str, input: Value) -> Result<String, OpError> {
    let call = parse(tool, input).unwrap();
    call.run(driver, &Step::for_tests(&|| false, Duration::from_secs(20)))
}

fn ran(driver: &FakeDriver, tool: &str, input: Value) -> String {
    match run_on(driver, tool, input) {
        Ok(answer) => answer,
        Err(error) => panic!("{tool}: {error:?}"),
    }
}

fn said(result: Result<String, OpError>) -> String {
    match result {
        Err(OpError::Said(message)) => message,
        other => panic!("expected the tool's own refusal, got {other:?}"),
    }
}

fn out(text: &str) -> ShellResult {
    FakeAndroid::out(text)
}

fn commands(driver: &FakeDriver) -> Vec<String> {
    driver.android_fake().commands()
}

/// A phone whose adb operations are `phone`'s and whose element tree shows the `trees` it is given,
/// one per read and the last one for good: what the screen shows before a button is pressed and
/// after. The shared fakes have a tree or adb operations, not both.
struct TreePhone {
    phone: Arc<FakeDriver>,
    trees: Mutex<VecDeque<UiNode>>,
    reads: AtomicUsize,
    /// Reads that still succeed, then the error every read after them answers with.
    failure: Mutex<Option<(usize, DriverError)>>,
    /// How long a tap takes, as a slow device's does.
    tap_takes: Mutex<Duration>,
    /// How long a read of the tree takes: a read bounded by less fails.
    read_takes: Mutex<Duration>,
    /// The timeouts the bounded reads were given.
    timeouts: Mutex<Vec<Duration>>,
}

impl TreePhone {
    fn new(phone: Arc<FakeDriver>, trees: Vec<UiNode>) -> Arc<Self> {
        assert!(!trees.is_empty());
        Arc::new(Self {
            phone,
            trees: Mutex::new(trees.into()),
            reads: AtomicUsize::new(0),
            failure: Mutex::new(None),
            tap_takes: Mutex::new(Duration::ZERO),
            read_takes: Mutex::new(Duration::ZERO),
            timeouts: Mutex::new(Vec::new()),
        })
    }

    /// A tap takes `time` from now on.
    fn tap_takes(&self, time: Duration) {
        *self.tap_takes.lock().unwrap() = time;
    }

    /// A read of the tree takes `time` from now on.
    fn read_takes(&self, time: Duration) {
        *self.read_takes.lock().unwrap() = time;
    }

    /// The timeouts the bounded reads of the tree have been given.
    fn timeouts(&self) -> Vec<Duration> {
        self.timeouts.lock().unwrap().clone()
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// The next `good` reads succeed; every one after them fails with `error`.
    fn fail_after(&self, good: usize, error: DriverError) {
        *self.failure.lock().unwrap() = Some((good, error));
    }
}

impl Driver for TreePhone {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            has_ui_tree: true,
            ..self.phone.info()
        }
    }

    fn frame(&self, fresh: bool) -> DriverResult<Frame> {
        self.phone.frame(fresh)
    }

    fn tap(&self, x: u32, y: u32, count: u8) -> DriverResult<()> {
        let takes = *self.tap_takes.lock().unwrap();
        if !takes.is_zero() {
            thread::sleep(takes);
        }
        self.phone.tap(x, y, count)
    }

    fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()> {
        self.phone.key(key, down)
    }

    fn text(&self, text: &str) -> DriverResult<()> {
        self.phone.text(text)
    }

    fn ui_tree(&self) -> DriverResult<Option<UiNode>> {
        if let Some((good, error)) = self.failure.lock().unwrap().as_mut() {
            if *good == 0 {
                return Err(error.clone());
            }
            *good -= 1;
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut trees = self.trees.lock().unwrap();
        Ok(Some(if trees.len() > 1 {
            trees.pop_front().unwrap()
        } else {
            trees[0].clone()
        }))
    }

    fn ui_tree_within(&self, timeout: Duration) -> DriverResult<Option<UiNode>> {
        self.timeouts.lock().unwrap().push(timeout);
        if *self.read_takes.lock().unwrap() > timeout {
            return Err(DriverError::Failed(
                "uiautomator did not finish reading the screen in the time this call had left."
                    .to_owned(),
            ));
        }
        self.ui_tree()
    }

    fn close(&self) {
        self.phone.close()
    }

    fn android(&self) -> Option<&dyn AndroidControl> {
        self.phone.android()
    }
}

/// A node of a uiautomator dump: `bounds` as `[left, top, right, bottom]`.
fn node(
    class: &str,
    package: &str,
    id: &str,
    text: &str,
    clickable: bool,
    bounds: [i32; 4],
    children: Vec<UiNode>,
) -> UiNode {
    UiNode {
        role: class.to_owned(),
        name: text.to_owned(),
        id: (!id.is_empty()).then(|| id.to_owned()),
        bounds: Some(Rect {
            x: bounds[0],
            y: bounds[1],
            width: (bounds[2] - bounds[0]) as u32,
            height: (bounds[3] - bounds[1]) as u32,
        }),
        enabled: true,
        package: Some(package.to_owned()),
        clickable,
        text: (!text.is_empty()).then(|| text.to_owned()),
        children,
        ..UiNode::default()
    }
}

/// The screen of an app with nothing on top of it.
fn app_screen() -> UiNode {
    node(
        "FrameLayout",
        "com.example.app",
        "",
        "",
        false,
        [0, 0, 1080, 2400],
        vec![node(
            "Button",
            "com.example.app",
            "com.example.app:id/ok",
            "OK",
            true,
            [100, 200, 400, 300],
            vec![],
        )],
    )
}

/// An `AlertDialog` over an app: its buttons are the ids `button1`..`button3`.
fn alert_dialog(buttons: &[(&str, &str, [i32; 4])]) -> UiNode {
    let mut children = vec![node(
        "TextView",
        "com.example.app",
        "android:id/message",
        "Discard the draft?",
        false,
        [120, 900, 960, 1000],
        vec![],
    )];
    children.extend(buttons.iter().map(|(id, text, bounds)| {
        node("Button", "com.example.app", id, text, true, *bounds, vec![])
    }));
    node(
        "FrameLayout",
        "com.example.app",
        "",
        "",
        false,
        [0, 0, 1080, 2400],
        vec![node(
            "LinearLayout",
            "com.example.app",
            "android:id/parentPanel",
            "",
            false,
            [100, 800, 980, 1400],
            children,
        )],
    )
}

/// A dialog with a negative and a positive button.
fn discard_dialog() -> UiNode {
    alert_dialog(&[
        ("android:id/button2", "Cancel", [140, 1250, 440, 1370]),
        ("android:id/button1", "Discard", [600, 1250, 940, 1370]),
    ])
}

/// The runtime-permission dialog of the permission controller of Android 13: its buttons carry
/// ids of its own, and its text is not a button.
fn permission_dialog(package: &str) -> UiNode {
    let button = |id: &str, text: &str, top: i32| {
        node(
            "Button",
            package,
            &format!("com.android.permissioncontroller:id/{id}"),
            text,
            true,
            [90, top, 990, top + 120],
            vec![],
        )
    };
    node(
        "FrameLayout",
        package,
        "",
        "",
        false,
        [0, 0, 1080, 2400],
        vec![
            node(
                "TextView",
                package,
                "com.android.permissioncontroller:id/permission_message",
                "Allow Example to take pictures and record video?",
                false,
                [90, 700, 990, 900],
                vec![],
            ),
            button(
                "permission_allow_foreground_only_button",
                "While using the app",
                1000,
            ),
            button("permission_allow_one_time_button", "Only this time", 1130),
            button("permission_deny_button", "Don\u{2019}t allow", 1260),
        ],
    )
}

/// `run_on` for a driver that is not a [`FakeDriver`].
fn run_driver(driver: &dyn Driver, tool: &str, input: Value) -> Result<String, OpError> {
    let call = parse(tool, input).unwrap();
    call.run(driver, &Step::for_tests(&|| false, Duration::from_secs(20)))
}

/// `dialog` parsed, and run on `driver`.
fn press_on(driver: &dyn Driver, input: Value) -> Result<String, OpError> {
    let call = AndroidCall::dialog(&object(input)).unwrap();
    call.run(driver, &Step::for_tests(&|| false, Duration::from_secs(20)))
}

/// Reads a command line the way `sh` would, for what the tools write: simple commands separated
/// by `;`, words of single-quoted runs, `\'` and bare characters none of which the shell treats
/// specially, and the one redirection the tools use, a whole word `2>&1` (kept as that word).
/// Anything else outside quotes is an error, which is the property under test.
fn shell_words(command: &str) -> Vec<Vec<String>> {
    let mut commands = vec![Vec::new()];
    let mut word: Option<String> = None;
    let mut characters = command.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '2' && word.is_none() {
            let rest: String = characters.clone().take(4).collect();
            if rest == ">&1"
                || rest
                    .strip_prefix(">&1")
                    .is_some_and(|after| after == " " || after == ";")
            {
                characters.nth(2);
                commands.last_mut().unwrap().push("2>&1".to_owned());
                continue;
            }
        }
        match character {
            '\'' => {
                let word = word.get_or_insert_with(String::new);
                loop {
                    match characters.next().expect("unterminated single quote") {
                        '\'' => break,
                        inside => word.push(inside),
                    }
                }
            }
            '\\' => word
                .get_or_insert_with(String::new)
                .push(characters.next().expect("dangling backslash")),
            ' ' | ';' => {
                if let Some(finished) = word.take() {
                    commands.last_mut().unwrap().push(finished);
                }
                if character == ';' {
                    commands.push(Vec::new());
                }
            }
            bare if bare.is_ascii_alphanumeric() || "-_./=:@,%+".contains(bare) => {
                word.get_or_insert_with(String::new).push(bare);
            }
            special => panic!("unquoted shell character {special:?} in {command:?}"),
        }
    }
    if let Some(finished) = word.take() {
        commands.last_mut().unwrap().push(finished);
    }
    commands.retain(|simple| !simple.is_empty());
    commands
}

// ------------------------------------------------------------- parsing

/// Every tool and action takes its parameters, with `serverId` beside them, and declares what
/// it does to the screen and how long it may take.
#[test]
fn every_tool_takes_its_parameters_and_says_what_it_does() {
    let cases: &[(&str, Value, &str, Effect)] = &[
        ("screen_app", json!({}), "listing the apps", Effect::Look),
        (
            "screen_app",
            json!({"serverId": "pixel", "action": "list", "scope": "all", "filter": "goo", "limit": 5}),
            "listing the apps",
            Effect::Look,
        ),
        (
            "screen_app",
            json!({"action": "foreground"}),
            "reading the app in front",
            Effect::Look,
        ),
        (
            "screen_app",
            json!({"action": "info", "app": "com.android.settings"}),
            "reading com.android.settings's details",
            Effect::Look,
        ),
        (
            "screen_app",
            json!({"action": "launch", "app": "com.example.app", "activity": ".Main$Inner", "restart": true}),
            "launching com.example.app",
            Effect::Act,
        ),
        (
            "screen_app",
            json!({"action": "activate", "app": "com.example.app"}),
            "activating com.example.app",
            Effect::Act,
        ),
        (
            "screen_app",
            json!({"action": "stop", "app": "com.example.app"}),
            "stopping com.example.app",
            Effect::Act,
        ),
        (
            "screen_app",
            json!({"action": "open_url", "url": "myapp://open?x=1&y=2", "app": "com.example.app"}),
            "opening myapp://open?x=1&y=2",
            Effect::Act,
        ),
        (
            "screen_package",
            json!({"action": "uninstall", "app": "com.example.app", "keep_data": true}),
            "uninstalling com.example.app",
            Effect::Background,
        ),
        (
            "screen_package",
            json!({"action": "clear_data", "app": "com.example.app", "workspace": 2}),
            "erasing com.example.app's data",
            Effect::Background,
        ),
        (
            "screen_package",
            json!({"action": "grant", "app": "com.example.app", "permission": "android.permission.CAMERA"}),
            "granting android.permission.CAMERA to com.example.app",
            Effect::Background,
        ),
        (
            "screen_package",
            json!({"action": "revoke", "app": "com.example.app", "permission": "android.permission.CAMERA"}),
            "revoking android.permission.CAMERA from com.example.app",
            Effect::Background,
        ),
        (
            "android_logcat",
            json!({"lines": 50, "level": "W", "tag": "ActivityManager", "package": "com.example.app", "buffer": "crash", "contains": "boom"}),
            "reading the log",
            Effect::Look,
        ),
        (
            "screen_device",
            json!({}),
            "reading the device's state",
            Effect::Look,
        ),
        (
            "screen_device",
            json!({"action": "info", "sections": ["notifications", "device"]}),
            "reading the device's state",
            Effect::Look,
        ),
        (
            "screen_device",
            json!({"action": "lock"}),
            "putting the device to sleep",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "unlock"}),
            "unlocking the device",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "wake"}),
            "waking the device",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "rotate", "value": "landscape_left"}),
            "rotating the screen",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "appearance", "value": "dark"}),
            "switching appearance",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "animations", "value": "off"}),
            "switching animations",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "stay_awake", "value": "on"}),
            "switching stay-awake",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "notifications"}),
            "opening the notification shade",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "quick_settings"}),
            "opening quick settings",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "collapse"}),
            "collapsing the notification shade",
            Effect::Act,
        ),
        (
            "screen_device",
            json!({"action": "dismiss_keyboard"}),
            "dismissing the keyboard",
            Effect::Act,
        ),
        (
            "android_shell",
            json!({"command": "getprop", "timeout": 1000}),
            "running the shell command",
            Effect::Act,
        ),
    ];
    for (tool, input, what, effect) in cases {
        let call =
            parse(tool, input.clone()).unwrap_or_else(|error| panic!("{tool} {input}: {error}"));
        assert_eq!(call.what(), *what, "{tool} {input}");
        assert_eq!(call.effect(), *effect, "{tool} {input}");
        assert_eq!(call.time(), SCREEN_CALL_TIME, "{tool} {input}");
    }
    // A button of the dialog showing is an act.
    let press =
        AndroidCall::dialog(&object(json!({"serverId": "pixel", "button": "Allow"}))).unwrap();
    assert_eq!(press.what(), "pressing \"Allow\"");
    assert_eq!(press.effect(), Effect::Act);
    assert_eq!(press.time(), SCREEN_CALL_TIME);
    // A null is no value at all.
    assert!(parse(
        "screen_app",
        json!({"scope": null, "limit": null, "arguments": null})
    )
    .is_ok());
    assert_eq!(
        refused("screen_app", json!({"action": "erase"})),
        "\"action\" must be list, foreground, info, launch, activate, stop or open_url, not \"erase\"."
    );
    assert_eq!(
        refused("screen_device", json!({"action": "reboot"})),
        "\"action\" must be info, lock, unlock, wake, rotate, appearance, animations, location, clear_location, notifications, quick_settings, collapse, stay_awake or dismiss_keyboard, not \"reboot\"."
    );
    assert_eq!(
        refused("screen_package", json!({})),
        "\"action\" is required: install, uninstall, clear_data, grant or revoke."
    );
    // The other platforms' tools are not Android's.
    for tool in ["screen_clipboard"] {
        assert_eq!(
            AndroidCall::parse(tool, &JsonObject::new(), &Files::new().files).unwrap_err(),
            format!("{tool} is not an Android tool.")
        );
    }
}

#[test]
fn a_parameter_a_tool_does_not_have_is_refused_by_name() {
    assert_eq!(
        refused("screen_app", json!({"colour": "red"})),
        "screen_app has no parameter \"colour\"; it takes serverId, action, app, url, filter, limit, scope, activity, restart, arguments and environment."
    );
    // `app` is what `package` was.
    assert_eq!(
        refused("screen_app", json!({"action": "stop", "package": "com.a.b"})),
        "screen_app has no parameter \"package\"; it takes serverId, action, app, url, filter, limit, scope, activity, restart, arguments and environment."
    );
    assert_eq!(
        refused("screen_package", json!({"action": "uninstall", "package": "com.a.b"})),
        "screen_package has no parameter \"package\"; it takes serverId, action, path, app, permission, keep_data, replace_existing, downgrade and grant_permissions."
    );
    assert_eq!(
        refused("android_shell", json!({"command": "ls", "cwd": "/"})),
        "android_shell has no parameter \"cwd\"; it takes serverId, command and timeout."
    );
    assert_eq!(
        refused("screen_device", json!({"section": ["device"]})),
        "screen_device has no parameter \"section\"; it takes serverId, action, value, sections, latitude and longitude."
    );
    // This build has no `resize` for a device's display.
    assert_eq!(
        refused("screen_device", json!({"action": "info", "width": 720})),
        "screen_device has no parameter \"width\"; it takes serverId, action, value, sections, latitude and longitude."
    );
    // `workspace` is the host's for screen_package's APK, and nobody else's.
    assert_eq!(
        refused("android_logcat", json!({"workspace": 2})),
        "android_logcat has no parameter \"workspace\"; it takes serverId, lines, level, tag, package, buffer and contains."
    );
    assert!(refused("screen_app", json!({"workspace": 2}))
        .starts_with("screen_app has no parameter \"workspace\""));
    assert!(parse(
        "screen_package",
        json!({"action": "uninstall", "app": "com.a.b", "workspace": 1})
    )
    .is_ok());
    assert_eq!(
        AndroidCall::dialog(&object(json!({"button": "OK", "text": "x"}))).unwrap_err(),
        "preview_dialog has no parameter \"text\"; it takes serverId, accept, prompt_text and button."
    );
    // A key made to look like something else is shown without its tricks.
    let sneaky = refused("screen_device", json!({"action": "wake", "val\nue": "x"}));
    assert!(!sneaky.contains('\n'), "{sneaky}");
}

#[test]
fn a_parameter_of_another_action_is_refused_naming_the_action() {
    let cases: &[(&str, Value, &str)] = &[
        (
            "screen_app",
            json!({"action": "foreground", "scope": "all"}),
            "\"scope\" does not apply to screen_app foreground; it is for list.",
        ),
        (
            "screen_app",
            json!({"app": "com.a.b"}),
            "\"app\" does not apply to screen_app list; it is for info, launch, activate, stop or open_url.",
        ),
        (
            "screen_app",
            json!({"action": "foreground", "app": "com.a.b"}),
            "\"app\" does not apply to screen_app foreground; it is for info, launch, activate, stop or open_url.",
        ),
        (
            "screen_app",
            json!({"action": "stop", "app": "com.a.b", "activity": ".Main"}),
            "\"activity\" does not apply to screen_app stop; it is for launch.",
        ),
        (
            "screen_app",
            json!({"action": "launch", "app": "com.a.b", "url": "https://x"}),
            "\"url\" does not apply to screen_app launch; it is for open_url.",
        ),
        (
            "screen_app",
            json!({"action": "open_url", "url": "https://x", "restart": true}),
            "\"restart\" does not apply to screen_app open_url; it is for launch.",
        ),
        // activate brings an app to the front: it does not start it afresh, nor pick its screen.
        (
            "screen_app",
            json!({"action": "activate", "app": "com.a.b", "restart": true}),
            "\"restart\" does not apply to screen_app activate; it is for launch.",
        ),
        (
            "screen_app",
            json!({"action": "activate", "app": "com.a.b", "activity": ".Main"}),
            "\"activity\" does not apply to screen_app activate; it is for launch.",
        ),
        (
            "screen_package",
            json!({"action": "install", "path": "a.apk", "app": "com.a.b"}),
            "\"app\" does not apply to screen_package install; it is for uninstall, clear_data, grant or revoke.",
        ),
        (
            "screen_package",
            json!({"action": "uninstall", "app": "com.a.b", "permission": "x"}),
            "\"permission\" does not apply to screen_package uninstall; it is for grant or revoke.",
        ),
        (
            "screen_package",
            json!({"action": "grant", "app": "com.a.b", "permission": "p", "keep_data": true}),
            "\"keep_data\" does not apply to screen_package grant; it is for uninstall.",
        ),
        (
            "screen_package",
            json!({"action": "clear_data", "app": "com.a.b", "downgrade": true}),
            "\"downgrade\" does not apply to screen_package clear_data; it is for install.",
        ),
        (
            "screen_device",
            json!({"action": "wake", "value": "on"}),
            "\"value\" does not apply to screen_device wake; it is for rotate, appearance, animations or stay_awake.",
        ),
        (
            "screen_device",
            json!({"value": "dark"}),
            "\"value\" does not apply to screen_device info; it is for rotate, appearance, animations or stay_awake.",
        ),
        (
            "screen_device",
            json!({"action": "lock", "sections": ["device"]}),
            "\"sections\" does not apply to screen_device lock; it is for info.",
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
    ];
    for (tool, input, expected) in cases {
        assert_eq!(refused(tool, input.clone()), *expected, "{tool} {input}");
    }
}

/// An iPhone's parameters are in the schema, and an Android device says so rather than ignore
/// them or say it has no such parameter.
#[test]
fn an_iphones_parameters_are_refused_as_the_iphones() {
    for input in [
        json!({"action": "launch", "app": "com.a.b", "arguments": ["-x"]}),
        json!({"arguments": []}),
        json!({"action": "stop", "app": "com.a.b", "arguments": 5}),
    ] {
        assert_eq!(
            refused("screen_app", input.clone()),
            ARGUMENTS_NOT_HERE,
            "{input}"
        );
    }
    for input in [
        json!({"action": "launch", "app": "com.a.b", "environment": {"A": "b"}}),
        json!({"environment": {}}),
        json!({"action": "open_url", "url": "https://x", "environment": "A=b"}),
    ] {
        assert_eq!(
            refused("screen_app", input.clone()),
            ENVIRONMENT_NOT_HERE,
            "{input}"
        );
    }
    assert!(ARGUMENTS_NOT_HERE.starts_with("arguments is for an iPhone or iPad; "));
    assert!(ENVIRONMENT_NOT_HERE.starts_with("environment is for an iPhone or iPad; "));
    // Both say what to do on an Android device instead.
    assert!(ARGUMENTS_NOT_HERE.contains("activity") && ARGUMENTS_NOT_HERE.contains("open_url"));
    assert!(ENVIRONMENT_NOT_HERE.contains("activity") && ENVIRONMENT_NOT_HERE.contains("open_url"));
}

/// A simulated location needs the emulator console, which this build does not use: refused before
/// anything is sent, with the numbers held to the schema's bounds first.
#[test]
fn a_simulated_location_is_refused_before_anything_is_sent() {
    assert_eq!(
        refused(
            "screen_device",
            json!({"action": "location", "latitude": 48.85, "longitude": 2.35})
        ),
        LOCATION_NOT_HERE
    );
    assert!(LOCATION_NOT_HERE.starts_with(
        "A simulated location on an Android device needs the emulator console, which this build does not use; "
    ));
    // The refusal does not wait for numbers.
    assert_eq!(
        refused("screen_device", json!({"action": "location"})),
        LOCATION_NOT_HERE
    );
    assert_eq!(
        refused("screen_device", json!({"action": "clear_location"})),
        CLEAR_LOCATION_NOT_HERE
    );
    // The bounds are the schema's, edges included.
    for (key, value) in [
        ("latitude", 90.5),
        ("latitude", -91.0),
        ("longitude", 180.5),
        ("longitude", -181.0),
    ] {
        let (min, max) = if key == "latitude" {
            (-90, 90)
        } else {
            (-180, 180)
        };
        assert_eq!(
            refused("screen_device", json!({"action": "location", key: value})),
            number_bound(key, min, max),
            "{key} {value}"
        );
    }
    assert_eq!(
        refused(
            "screen_device",
            json!({"action": "location", "latitude": "north"})
        ),
        number_bound("latitude", -90, 90)
    );
    for (key, value) in [
        ("latitude", 90),
        ("latitude", -90),
        ("longitude", 180),
        ("longitude", -180),
    ] {
        assert_eq!(
            refused("screen_device", json!({"action": "location", key: value})),
            LOCATION_NOT_HERE,
            "{key} {value}"
        );
    }
}

#[test]
fn what_an_action_cannot_do_without_is_asked_for() {
    let cases: &[(&str, Value, &str)] = &[
        (
            "screen_app",
            json!({"action": "info"}),
            "screen_app info needs \"app\": the package name, such as com.android.settings.",
        ),
        (
            "screen_app",
            json!({"action": "launch"}),
            "screen_app launch needs \"app\": the package name, such as com.example.app.",
        ),
        (
            "screen_app",
            json!({"action": "activate"}),
            "screen_app activate needs \"app\": the package name, such as com.example.app.",
        ),
        (
            "screen_app",
            json!({"action": "stop"}),
            "screen_app stop needs \"app\": the package name, such as com.example.app.",
        ),
        (
            "screen_app",
            json!({"action": "open_url", "app": "com.a.b"}),
            "screen_app open_url needs \"url\": the URL or deep link to open.",
        ),
        (
            "screen_package",
            json!({"action": "install"}),
            "screen_package install needs \"path\": the .apk file, relative to the workspace or absolute.",
        ),
        (
            "screen_package",
            json!({"action": "clear_data"}),
            "screen_package clear_data needs \"app\": the package name, such as com.example.app.",
        ),
        (
            "screen_package",
            json!({"action": "revoke", "app": "com.a.b"}),
            "screen_package revoke needs \"permission\": the permission, such as android.permission.POST_NOTIFICATIONS.",
        ),
        (
            "android_shell",
            json!({}),
            "android_shell needs \"command\": the shell command line.",
        ),
    ];
    for (tool, input, expected) in cases {
        assert_eq!(refused(tool, input.clone()), *expected, "{tool} {input}");
    }
}

/// Every value that ends up in a command is held to what it may be, before the command is
/// built — the quoting is the second line of defence, not the first.
#[test]
fn values_that_reach_a_command_are_checked_first() {
    for bad in [
        "com",
        "1com.a",
        "com..a",
        "com.a-b",
        "com.a b",
        "com.a;reboot",
        ".com.a",
        "com.a.",
    ] {
        let error = refused("screen_app", json!({"action": "stop", "app": bad}));
        assert!(
            error.starts_with("\"app\" must be a package name such as com.example.app"),
            "{bad}: {error}"
        );
        // Another tool's package is held to the same, under its own name.
        assert!(
            refused("android_logcat", json!({"package": bad}))
                .starts_with("\"package\" must be a package name such as com.example.app"),
            "{bad}"
        );
        assert!(
            refused(
                "screen_package",
                json!({"action": "clear_data", "app": bad})
            )
            .starts_with("\"app\" must be a package name such as com.example.app"),
            "{bad}"
        );
    }
    for good in ["com.a", "com.example.app_2", "a.B9.c_"] {
        assert!(
            parse("screen_app", json!({"action": "stop", "app": good})).is_ok(),
            "{good}"
        );
        assert!(
            parse("screen_app", json!({"action": "activate", "app": good})).is_ok(),
            "{good}"
        );
    }
    assert_eq!(
        refused("screen_app", json!({"action": "launch", "app": "com.a.b", "activity": "com.a/.Main"})),
        "\"activity\" must be an activity class such as .MainActivity or com.example.app.MainActivity (letters, digits, _, . and $), not \"com.a/.Main\"."
    );
    assert_eq!(
        refused("screen_package", json!({"action": "grant", "app": "com.a.b", "permission": "android.permission.CAMERA; reboot"})),
        "\"permission\" must be a permission name such as android.permission.CAMERA (letters, digits, _ and .), not \"android.permission.CAMERA; reboot\"."
    );
    for tag in [
        "Activity Manager",
        "a:b",
        "*",
        "it's",
        "say\"hi\"",
        "back\\slash",
        "tab\tbed",
        // logcat would read these as its options: clear the log, write it to a file.
        "-ce",
        "-c",
        "-f/data/local/tmp/x",
        "--clear",
    ] {
        assert_eq!(
            refused("android_logcat", json!({"tag": tag})),
            "\"tag\" must be one log tag, without spaces, colons, asterisks, quotes or backslashes, and not starting with -.",
            "{tag:?}"
        );
    }
    assert!(parse("android_logcat", json!({"tag": "chromium-1.x_y$z"})).is_ok());
    assert!(parse("android_logcat", json!({"tag": "a-c"})).is_ok());
    // Text-direction controls and zero-width characters make a link read as another than it is.
    for url in [
        "https://x/a b",
        "https://x/\u{7}",
        "myapp://\nx",
        "https://good.example\u{202e}/moc.evil",
        "https://exam\u{200b}ple.com",
        "https://example.com/\u{2066}x\u{2069}",
        "\u{feff}https://example.com",
        "https://example.com/\u{200f}",
        "https://example.com/\u{2060}",
    ] {
        assert_eq!(
            refused("screen_app", json!({"action": "open_url", "url": url})),
            "\"url\" must have no spaces, control characters, or invisible or text-direction characters; percent-encode them (a space is %20).",
            "{url:?}"
        );
    }
    assert!(parse(
        "screen_app",
        json!({"action": "open_url", "url": "https://example.com/?q=a&b='c'$(id)"})
    )
    .is_ok());
    // The package an open_url opens the link in is held to the same.
    assert!(refused(
        "screen_app",
        json!({"action": "open_url", "url": "https://x", "app": "com.a;reboot"})
    )
    .starts_with("\"app\" must be a package name"));
}

#[test]
fn a_device_action_names_its_values_when_one_is_missing_or_wrong() {
    assert_eq!(
        refused("screen_device", json!({"action": "rotate"})),
        "screen_device rotate needs \"value\": portrait, landscape_left, landscape_right, portrait_upside_down or auto."
    );
    // The values of the tools this one replaces are not its own.
    assert_eq!(
        refused(
            "screen_device",
            json!({"action": "rotate", "value": "landscape"})
        ),
        "\"value\" for rotate must be portrait, landscape_left, landscape_right, portrait_upside_down or auto, not \"landscape\"."
    );
    assert_eq!(
        refused(
            "screen_device",
            json!({"action": "appearance", "value": "dim"})
        ),
        "\"value\" for appearance must be light, dark or auto, not \"dim\"."
    );
    assert_eq!(
        refused(
            "screen_device",
            json!({"action": "appearance", "value": "on"})
        ),
        "\"value\" for appearance must be light, dark or auto, not \"on\"."
    );
    assert_eq!(
        refused("screen_device", json!({"action": "appearance"})),
        "screen_device appearance needs \"value\": light, dark or auto."
    );
    assert_eq!(
        refused(
            "screen_device",
            json!({"action": "animations", "value": "ON"})
        ),
        "\"value\" for animations must be on or off, not \"ON\"."
    );
    assert_eq!(
        refused("screen_device", json!({"action": "stay_awake"})),
        "screen_device stay_awake needs \"value\": on or off."
    );
    let settings = |input: Value| match parse("screen_device", input).unwrap().call {
        Call::Settings(setting) => setting,
        other => panic!("{other:?}"),
    };
    for (value, expected) in [
        ("portrait", Some(Orientation::Portrait)),
        ("landscape_left", Some(Orientation::LandscapeLeft)),
        ("landscape_right", Some(Orientation::LandscapeRight)),
        (
            "portrait_upside_down",
            Some(Orientation::PortraitUpsideDown),
        ),
        ("auto", None),
    ] {
        assert_eq!(
            settings(json!({"action": "rotate", "value": value})),
            Setting::Rotate(expected),
            "{value}"
        );
    }
    // `cmd uimode night`'s words: dark is yes, light is no.
    for (value, word) in [("dark", "yes"), ("light", "no"), ("auto", "auto")] {
        assert_eq!(
            settings(json!({"action": "appearance", "value": value})),
            Setting::Appearance(word),
            "{value}"
        );
    }
    assert_eq!(
        settings(json!({"action": "animations", "value": "off"})),
        Setting::Animations(false)
    );
    assert_eq!(
        settings(json!({"action": "collapse"})),
        Setting::Shade("collapse")
    );
    assert_eq!(
        settings(json!({"action": "stay_awake", "value": "on"})),
        Setting::StayAwake(true)
    );
    assert_eq!(settings(json!({"action": "lock"})), Setting::Lock);
    assert_eq!(settings(json!({"action": "wake"})), Setting::Wake);
    assert_eq!(settings(json!({"action": "unlock"})), Setting::Unlock);
    assert_eq!(
        settings(json!({"action": "dismiss_keyboard"})),
        Setting::DismissKeyboard
    );
}

#[test]
fn sections_are_one_to_four_known_names_each_once() {
    let bound = sections_bound();
    assert_eq!(
        bound,
        "\"sections\" must be a list of 1 to 4 of device, display, power and notifications, each at most once."
    );
    for bad in [
        json!([]),
        json!(["device", "device"]),
        json!(["battery"]),
        json!("device"),
        json!([1]),
        json!(["device", "display", "power", "notifications", "device"]),
    ] {
        assert_eq!(
            refused("screen_device", json!({"sections": bad.clone()})),
            bound,
            "{bad}"
        );
    }
    let sections = |input: Value| match parse("screen_device", input).unwrap().call {
        Call::Info(sections) => sections,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        sections(json!({})),
        vec![Section::Device, Section::Display, Section::Power]
    );
    // Reported in one order, whatever order they were asked in.
    assert_eq!(
        sections(json!({"sections": ["notifications", "device"]})),
        vec![Section::Device, Section::Notifications]
    );
}

#[test]
fn an_install_opens_its_apk_through_the_call_files() {
    let files = Files::new();
    const APK: &[u8] = b"PK\x03\x04 an apk";
    files.write("app-debug.apk", APK);
    files.write("App.APK", b"PK");
    files.write("notes.txt", b"hello");
    files.write("empty.apk", b"");
    let install = |input: Value| match parse_in(&files, "screen_package", input) {
        Ok(AndroidCall {
            call:
                Call::Package(Package::Install {
                    apk,
                    replace_existing,
                    flags,
                }),
            ..
        }) => (apk.name, apk.size, replace_existing, flags),
        other => panic!("{other:?}"),
    };
    // Replacing is the device's Android's word (`-r` or `-R`), chosen when the call runs.
    assert_eq!(
        install(json!({"action": "install", "path": "app-debug.apk"})),
        (
            "app-debug.apk".to_owned(),
            APK.len() as u64,
            true,
            vec!["-t"]
        )
    );
    let unreplaced = install(
        json!({"action": "install", "path": "App.APK", "replace_existing": false, "downgrade": true, "grant_permissions": true}),
    );
    assert!(!unreplaced.2);
    assert_eq!(unreplaced.3, vec!["-t", "-d", "-g"]);
    let absolute = files.dir.path().join("app-debug.apk");
    assert_eq!(
        install(json!({"action": "install", "path": absolute.to_string_lossy()})).0,
        "app-debug.apk"
    );
    let call = parse_in(
        &files,
        "screen_package",
        json!({"action": "install", "path": "app-debug.apk"}),
    )
    .unwrap();
    assert_eq!(call.time(), INSTALL_TIME);
    assert_eq!(call.what(), "installing app-debug.apk");
    assert_eq!(call.effect(), Effect::Background);

    let refusal = |input: Value| parse_in(&files, "screen_package", input).unwrap_err();
    assert_eq!(
        refusal(json!({"action": "install", "path": "notes.txt"})),
        "notes.txt is not an .apk file; screen_package install takes an APK."
    );
    assert_eq!(
        refusal(json!({"action": "install", "path": "empty.apk"})),
        "empty.apk is empty."
    );
    assert!(!refusal(json!({"action": "install", "path": "missing.apk"})).is_empty());
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("other.apk"), b"PK").unwrap();
    assert!(parse_in(
        &files,
        "screen_package",
        json!({"action": "install", "path": outside.path().join("other.apk").to_string_lossy()})
    )
    .is_err());
    assert_eq!(
        human_size(APK_MAX_BYTES + 1),
        "2.1 GB",
        "what a too large APK is said to weigh"
    );
}

/// The bounds a test of the schema reads off the parser are the schema's own: each parameter of
/// each tool's schema is accepted at its edges and refused one past them, the schema lists exactly
/// the parameters the parser takes, and says the actions the parser takes in the order it does.
#[test]
fn the_schemas_bounds_are_the_parsers() {
    let files = Files::new();
    files.write("app-debug.apk", b"PK\x03\x04");
    let profile = PromptProfile::builtin();
    // A call where `key` applies, complete but for `key`.
    let base = |tool: &str, key: &str| -> JsonObject {
        object(match (tool, key) {
            ("screen_app", "app") => json!({"action": "stop"}),
            ("screen_app", "url") => json!({"action": "open_url"}),
            ("screen_app", "activity" | "restart") => {
                json!({"action": "launch", "app": "com.example.app"})
            }
            ("screen_app", _) => json!({"action": "list"}),
            ("screen_package", "path") => json!({"action": "install"}),
            ("screen_package", "app") => json!({"action": "uninstall"}),
            ("screen_package", "permission") => {
                json!({"action": "grant", "app": "com.example.app"})
            }
            ("screen_package", "keep_data") => {
                json!({"action": "uninstall", "app": "com.example.app"})
            }
            ("screen_package", _) => json!({"action": "install", "path": "app-debug.apk"}),
            ("screen_device", "sections") => json!({"action": "info"}),
            ("screen_device", "latitude" | "longitude") => json!({"action": "location"}),
            ("screen_device", _) => json!({"action": "rotate"}),
            ("android_shell", "timeout") => json!({"command": "ls"}),
            _ => json!({}),
        })
    };
    // A value of `chars` characters in the form `key` takes, where one that short exists.
    let sample = |key: &str, chars: usize| -> String {
        match key {
            "package" | "app" if chars >= 3 => format!("a.{}", "b".repeat(chars - 2)),
            _ => "x".repeat(chars),
        }
    };
    for tool in [
        "screen_app",
        "screen_device",
        "screen_package",
        "android_logcat",
        "android_shell",
        "preview_dialog",
    ] {
        let schema =
            crate::builtin_schemas::builtin_tool_schema(tool, ToolVariant::Standard, &profile)
                .unwrap_or_else(|| panic!("{tool} has a schema"));
        assert_eq!(schema["additionalProperties"], json!(false), "{tool}");
        let properties = schema["properties"].as_object().unwrap();
        let mut listed: Vec<&str> = properties
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "serverId")
            .collect();
        let mut parsed: Vec<&str> = params(tool).iter().map(|param| param.key).collect();
        listed.sort_unstable();
        parsed.sort_unstable();
        assert_eq!(listed, parsed, "{tool}");
        // The actions are the parser's, in the schema's order, and the default is the first.
        if let Some((values, required)) = actions(tool) {
            let enumerated: Vec<&str> = properties["action"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(enumerated, values, "{tool}");
            assert_eq!(
                properties["action"].get("default").and_then(Value::as_str),
                (!required).then_some(values[0]),
                "{tool}"
            );
            assert_eq!(
                schema["required"].as_array().unwrap().is_empty(),
                !required,
                "{tool}"
            );
        }
        for (key, property) in properties {
            if key == "serverId" || key == "action" {
                continue;
            }
            let with = |value: Value| {
                let mut input = base(tool, key);
                input.insert(key.clone(), value);
                if tool == "preview_dialog" {
                    AndroidCall::dialog(&input).err()
                } else {
                    AndroidCall::parse(tool, &input, &files.files).err()
                }
            };
            let context = format!("{tool}.{key}");
            let kind = params(tool)
                .iter()
                .find(|param| param.key == key)
                .map(|param| param.kind)
                .unwrap();
            // The schema has it for another kind of screen: whatever it holds, the words are one.
            if let Kind::NotHere(words) = kind {
                for value in [
                    json!(true),
                    json!(false),
                    json!("x"),
                    json!(["x"]),
                    json!({"A": "b"}),
                    json!(0),
                ] {
                    assert_eq!(
                        with(value.clone()),
                        Some(words.to_owned()),
                        "{context} = {value}"
                    );
                }
                continue;
            }
            match (property["type"].as_str(), property.get("enum")) {
                (Some("string"), Some(values)) => {
                    let values: Vec<&str> = values
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    for value in &values {
                        let bound = choice_bound(key, &values, &json!(value));
                        assert_ne!(with(json!(value)), Some(bound), "{context} = {value}");
                    }
                    assert_eq!(
                        with(json!("zz")),
                        Some(choice_bound(key, &values, &json!("zz"))),
                        "{context}"
                    );
                }
                (Some("string"), None) => {
                    let min = property["minLength"].as_u64().unwrap() as usize;
                    let max = property["maxLength"].as_u64().unwrap() as usize;
                    let bound = Some(text_bound(key, max));
                    assert_ne!(with(json!(sample(key, max))), bound, "{context} at {max}");
                    assert_eq!(
                        with(json!(sample(key, max + 1))),
                        bound,
                        "{context} at {}",
                        max + 1
                    );
                    assert_ne!(with(json!(sample(key, min))), bound, "{context} at {min}");
                    assert_eq!(
                        with(json!(sample(key, min - 1))),
                        bound,
                        "{context} at {}",
                        min - 1
                    );
                    assert_eq!(with(json!(7)), bound, "{context} as a number");
                }
                (Some("integer"), _) => {
                    let min = property["minimum"].as_u64().unwrap();
                    let max = property["maximum"].as_u64().unwrap();
                    let bound = Some(integer_bound(key, min, max));
                    assert_eq!(with(json!(max)), None, "{context} at {max}");
                    assert_eq!(with(json!(max as f64)), None, "{context} at {max}.0");
                    assert_eq!(with(json!(max + 1)), bound, "{context} at {}", max + 1);
                    assert_eq!(with(json!(min)), None, "{context} at {min}");
                    assert_eq!(with(json!(min - 1)), bound, "{context} at {}", min - 1);
                    assert_eq!(with(json!(min as f64 + 0.5)), bound, "{context} not whole");
                    assert_eq!(with(json!("10")), bound, "{context} as a string");
                }
                (Some("number"), _) => {
                    let min = property["minimum"].as_i64().unwrap() as i32;
                    let max = property["maximum"].as_i64().unwrap() as i32;
                    let bound = Some(number_bound(key, min, max));
                    // Inside the bounds the number fits; the action is then refused on its own.
                    for fits in [json!(min), json!(max), json!(0.5)] {
                        assert_ne!(with(fits.clone()), bound, "{context} = {fits}");
                    }
                    assert_eq!(
                        with(json!(f64::from(max) + 0.5)),
                        bound,
                        "{context} past {max}"
                    );
                    assert_eq!(
                        with(json!(f64::from(min) - 0.5)),
                        bound,
                        "{context} past {min}"
                    );
                    assert_eq!(with(json!("1")), bound, "{context} as a string");
                }
                (Some("boolean"), _) => {
                    assert_eq!(with(json!(true)), None, "{context}");
                    assert_eq!(with(json!(false)), None, "{context}");
                    assert_eq!(with(json!("yes")), Some(flag_bound(key)), "{context}");
                }
                (Some("array"), _) => {
                    let min = property["minItems"].as_u64().unwrap() as usize;
                    let max = property["maxItems"].as_u64().unwrap() as usize;
                    assert_eq!(property["uniqueItems"], json!(true), "{context}");
                    let items: Vec<&str> = property["items"]["enum"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap())
                        .collect();
                    assert_eq!(items, SECTIONS, "{context}");
                    assert_eq!(
                        with(json!(items[..min].to_vec())),
                        None,
                        "{context} at {min}"
                    );
                    assert_eq!(
                        with(json!(items[..max].to_vec())),
                        None,
                        "{context} at {max}"
                    );
                    let mut over = items[..max].to_vec();
                    over.push(items[0]);
                    assert_eq!(
                        with(json!(over)),
                        Some(sections_bound()),
                        "{context} past {max}"
                    );
                    assert_eq!(with(json!([])), Some(sections_bound()), "{context} empty");
                }
                other => panic!("{context}: a schema type the test does not know: {other:?}"),
            }
        }
    }
}

// ------------------------------------------------------------- commands

/// Every value in every command is one word to the device's `sh`, whatever it holds: a model's
/// value reaches a builder only after it was validated, and the quoting holds even if a check
/// were ever wrong.
#[test]
fn every_value_in_a_command_is_one_quoted_word() {
    let evil = "$(reboot); rm -rf / | `id` && echo 'x' \"y\" * > z \\";
    let check = |command: String, expected: &[&str]| {
        let parsed = shell_words(&command);
        assert_eq!(parsed.len(), 1, "{command}");
        assert_eq!(parsed[0], expected, "{command}");
    };
    check(pm_path_command(evil), &["pm", "path", evil]);
    check(pidof_command(evil), &["pidof", evil]);
    check(dumpsys_package_command(evil), &["dumpsys", "package", evil]);
    check(force_stop_command(evil), &["am", "force-stop", evil]);
    check(clear_command(evil), &["pm", "clear", evil]);
    check(
        monkey_command(evil),
        &[
            "monkey",
            "-p",
            evil,
            "--pct-syskeys",
            "0",
            "-c",
            "android.intent.category.LAUNCHER",
            "1",
        ],
    );
    check(
        resolve_launcher_command(evil),
        &[
            "cmd",
            "package",
            "resolve-activity",
            "--brief",
            "-a",
            "android.intent.action.MAIN",
            "-c",
            "android.intent.category.LAUNCHER",
            evil,
        ],
    );
    check(
        start_component_command(evil),
        &[
            "am",
            "start",
            "-n",
            evil,
            "-a",
            "android.intent.action.MAIN",
            "-c",
            "android.intent.category.LAUNCHER",
        ],
    );
    check(
        open_url_command(evil, Some(evil)),
        &[
            "am",
            "start",
            "-a",
            "android.intent.action.VIEW",
            "-d",
            evil,
            evil,
        ],
    );
    check(
        open_url_command("https://x/?a=1&b=2", None),
        &[
            "am",
            "start",
            "-a",
            "android.intent.action.VIEW",
            "-d",
            "https://x/?a=1&b=2",
        ],
    );
    check(
        uninstall_command(34, evil, true),
        &["cmd", "package", "uninstall", "-k", evil],
    );
    check(
        uninstall_command(23, evil, false),
        &["pm", "uninstall", evil],
    );
    check(
        permission_command(true, evil, evil),
        &["pm", "grant", evil, evil],
    );
    check(
        permission_command(false, evil, evil),
        &["pm", "revoke", evil, evil],
    );
    check(
        uid_command(evil),
        &["cmd", "package", "list", "packages", "-U", evil],
    );
    let tagged = format!("{evil}:W");
    check(
        logcat_command(
            200,
            Some(evil),
            Some(LogSource::Uid(10234)),
            Some(evil),
            Some("W"),
        ),
        &[
            "logcat",
            "-d",
            "-v",
            "threadtime",
            "-t",
            "200",
            "-b",
            evil,
            "--uid=10234",
            "--",
            tagged.as_str(),
            "*:S",
        ],
    );
    check(
        logcat_command(5, None, Some(LogSource::Pid(77)), None, Some("E")),
        &[
            "logcat",
            "-d",
            "-v",
            "threadtime",
            "-t",
            "5",
            "--pid=77",
            "--",
            "*:E",
        ],
    );
    check(
        logcat_command(1, None, None, Some("Tag"), None),
        &[
            "logcat",
            "-d",
            "-v",
            "threadtime",
            "-t",
            "1",
            "--",
            "Tag:V",
            "*:S",
        ],
    );
    check(
        logcat_command(1, None, None, None, None),
        &["logcat", "-d", "-v", "threadtime", "-t", "1"],
    );
    // The fixed commands read the same way.
    for command in [
        display_command(),
        power_command(),
        marked(&ANIMATION_SCALES),
        animations_command(false),
        rotate_command(28, Some(1)),
        LAUNCHERS_COMMAND.to_owned(),
    ] {
        assert!(shell_words(&command).len() >= 1, "{command}");
    }
    assert_eq!(
        shell_words(&marked(&[("a", "wm size"), ("b", "wm density")])),
        vec![
            vec!["echo", "@@mewrk:a"],
            vec!["wm", "size", "2>&1"],
            vec!["echo", "@@mewrk:b"],
            vec!["wm", "density", "2>&1"],
        ]
    );
    // A tag that slipped past its check still cannot be one of logcat's options.
    assert_eq!(
        logcat_command(1, None, None, Some("-c"), None),
        "logcat -d -v threadtime -t 1 -- '-c:V' '*:S'"
    );
}

#[test]
fn the_android_version_chooses_the_command() {
    assert_eq!(window_dump_command(28), "dumpsys window windows");
    assert_eq!(window_dump_command(29), "dumpsys window displays");
    assert_eq!(
        rotate_command(29, Some(1)),
        "cmd window user-rotation lock 1"
    );
    assert_eq!(rotate_command(34, None), "cmd window user-rotation free");
    assert_eq!(
        rotate_command(28, Some(2)),
        "settings put system accelerometer_rotation 0; settings put system user_rotation 2"
    );
    assert_eq!(
        rotate_command(28, None),
        "settings put system accelerometer_rotation 1"
    );
    assert_eq!(
        uninstall_command(24, "com.a.b", false),
        "cmd package uninstall 'com.a.b'"
    );
    assert_eq!(
        uninstall_command(23, "com.a.b", true),
        "pm uninstall -k 'com.a.b'"
    );
    assert_eq!(
        animations_command(true),
        "settings put global window_animation_scale 1; settings put global transition_animation_scale 1; settings put global animator_duration_scale 1"
    );
    assert_eq!(list_packages_command(true), "pm list packages -3");
    assert_eq!(list_packages_command(false), "pm list packages");
}

// ------------------------------------------------------------- what devices print

/// Android 14 (`--brief`, one launcher per app) and Android 8.1 (an app with two launchers: the
/// first is kept).
const LAUNCHERS_34: &str = "3 activities found:
  Activity #0:
    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false
    com.android.settings/.Settings
  Activity #1:
    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false
    com.google.android.apps.maps/com.google.android.maps.MapsActivity
  Activity #2:
    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false
    com.android.chrome/com.google.android.apps.chrome.Main
";
const LAUNCHERS_27: &str = "2 activities found:
  Activity #0:
    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true
    com.example.app/.MainActivity
  Activity #1:
    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false
    com.example.app/.SecondLauncher
";

#[test]
fn launchers_and_packages_are_read_from_their_lists() {
    assert_eq!(
        parse_launchers(LAUNCHERS_34),
        vec![
            (
                "com.android.chrome".to_owned(),
                "com.android.chrome/com.google.android.apps.chrome.Main".to_owned()
            ),
            (
                "com.android.settings".to_owned(),
                "com.android.settings/.Settings".to_owned()
            ),
            (
                "com.google.android.apps.maps".to_owned(),
                "com.google.android.apps.maps/com.google.android.maps.MapsActivity".to_owned()
            ),
        ]
    );
    assert_eq!(
        parse_launchers(LAUNCHERS_27),
        vec![(
            "com.example.app".to_owned(),
            "com.example.app/.MainActivity".to_owned()
        )]
    );
    assert!(parse_launchers("No activities found\n").is_empty());
    assert_eq!(
        parse_package_list(
            "package:com.b\npackage:com.a\r\npackage:com.b\nnot a package\npackage:bad name\n"
        ),
        vec!["com.a", "com.b"]
    );
    // `-U` adds the uid; the filter is a substring, so the exact package is picked.
    let listed = "package:com.example.app.debug uid:10235\npackage:com.example.app uid:10234\n";
    assert_eq!(parse_uid(listed, "com.example.app"), Some(10234));
    assert_eq!(parse_uid(listed, "com.example"), None);
    assert_eq!(
        parse_uid("package:com.a.b uid:10001,1010001\n", "com.a.b"),
        Some(10001)
    );
    assert_eq!(parse_pids("4567 4590\n"), vec![4567, 4590]);
    assert!(parse_pids("").is_empty());
    assert!(parse_pids("/system/bin/sh: pidof: not found\n").is_empty());
    assert_eq!(
        parse_resolved_activity("priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.example.app/.MainActivity\n"),
        Some("com.example.app/.MainActivity".to_owned())
    );
    assert_eq!(
        parse_resolved_activity("priority=0 preferredOrder=0 match=0x0 specificIndex=-1 isDefault=false\nandroid/com.android.internal.app.ResolverActivity\n"),
        None
    );
    assert_eq!(parse_resolved_activity("No activity found\n"), None);
}

/// Android 9, `dumpsys window windows`: the focused app inside an `AppWindowToken`.
const WINDOWS_28: &str = "WINDOW MANAGER WINDOWS (dumpsys window windows)
  Window #12 Window{4ffa8d4 u0 com.android.launcher3/com.android.launcher3.Launcher}:
    mDisplayId=0 stackId=0 mSession=Session{a51d0b6 2156:u0a10021} mClient=android.os.BinderProxy@f2b0fa7
    mOwnerUid=10021 mShowToOwnerOnly=true package=com.android.launcher3 appop=NONE
  mHasPermanentDpad=false
  mCurrentFocus=Window{4ffa8d4 u0 com.android.launcher3/com.android.launcher3.Launcher}
  mFocusedApp=AppWindowToken{7d2bdc4 token=Token{a2ce1d7 ActivityRecord{f2e3bd6 u0 com.android.launcher3/.Launcher t2}}}
  mInputMethodTarget=Window{4ffa8d4 u0 com.android.launcher3/com.android.launcher3.Launcher}
";
/// Android 13, `dumpsys window displays` of a phone with a second display that has no focus.
const DISPLAYS_33: &str = "WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)
  Display: mDisplayId=2 rootTasks=1
    init=1920x1080 160dpi cur=1920x1080 app=1920x1080 rng=1080x1032-1920x1872
    mCurrentFocus=null
    mFocusedApp=null
  Display: mDisplayId=0 (organized) rootTasks=3
    init=1080x2400 420dpi base=1080x2400 420dpi cur=1080x2400 app=1080x2337 rng=1080x1017-2337x2274
    deferred=false mLayoutNeeded=false mTouchExcludeRegion=SkRegion((0,0,1080,2400))
    mCurrentFocus=Window{8f1b2a3 u0 com.android.settings/com.android.settings.Settings}
    mFocusedApp=ActivityRecord{2c5e0b1 u0 com.android.settings/.Settings t17}
";
/// Android 13, locked: the keyguard lives in the notification shade window.
const DISPLAYS_33_LOCKED: &str = "WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)
  Display: mDisplayId=0 (organized) rootTasks=3
    mCurrentFocus=Window{b0b8c0a u0 NotificationShade}
    mFocusedApp=ActivityRecord{7e0f9d2 u0 com.google.android.apps.nexuslauncher/.NexusLauncherActivity t2}
";
/// Android 9, locked: `mShowingLockscreen` and `isStatusBarKeyguard`.
const POLICY_28_LOCKED: &str = "WINDOW MANAGER POLICY STATE (dumpsys window policy)
    mSafeMode=false mSystemReady=true mSystemBooted=true
    mScreenOnEarly=true mScreenOnFully=true
    mStatusBar=Window{2b9ce2a u0 StatusBar} isStatusBarKeyguard=true
    mShowingLockscreen=true mShowingDream=false mDreamingLockscreen=false mDreamingSleepToken=null
";
/// Android 13, unlocked: the keyguard delegate and its state monitor.
const POLICY_33: &str = "WINDOW MANAGER POLICY STATE (dumpsys window policy)
    mSafeMode=false mSystemReady=true mSystemBooted=true
    mKeyguardDrawComplete=true mWindowManagerDrawComplete=true
    KeyguardServiceDelegate
      showing=false
      showingAndNotOccluded=false
      inputRestricted=false
      occluded=false
      secure=false
      dreaming=false
      screenState=SCREEN_STATE_ON
      interactiveState=INTERACTIVE_STATE_AWAKE
      KeyguardStateMonitor
        mIsShowing=false
        mSimSecure=false
        mInputRestricted=false
        mTrusted=false
        mCurrentUserId=0
";

#[test]
fn the_focus_is_read_from_either_dump_and_the_lock_from_the_policy() {
    let focus = parse_focus(WINDOWS_28);
    assert_eq!(
        focus,
        Focus {
            window: Some("com.android.launcher3/com.android.launcher3.Launcher".to_owned()),
            app: Some("com.android.launcher3/.Launcher".to_owned()),
        }
    );
    assert_eq!(
        foreground_report(&focus, None),
        "In front: com.android.launcher3/.Launcher"
    );
    let focus = parse_focus(DISPLAYS_33);
    assert_eq!(
        focus.window.as_deref(),
        Some("com.android.settings/com.android.settings.Settings")
    );
    assert_eq!(focus.app.as_deref(), Some("com.android.settings/.Settings"));
    assert_eq!(
        foreground_report(&focus, Some(false)),
        "In front: com.android.settings/.Settings"
    );
    let locked = parse_focus(DISPLAYS_33_LOCKED);
    assert_eq!(
        foreground_report(&locked, lock_screen(&POLICY_33.replace("showing=false", "showing=true"))),
        "In front: com.google.android.apps.nexuslauncher/.NexusLauncherActivity\nFocused window: NotificationShade\nThe lock screen is showing."
    );
    // The shade pulled down over an unlocked phone is not a lock screen.
    assert_eq!(
        foreground_report(&locked, lock_screen(POLICY_33)),
        "In front: com.google.android.apps.nexuslauncher/.NexusLauncherActivity\nFocused window: NotificationShade"
    );
    let anr = parse_focus("  mCurrentFocus=Window{1fdc2ec u0 Application Not Responding: com.example.app}\n  mFocusedApp=null\n");
    assert_eq!(
        foreground_report(&anr, None),
        "No app is in front: the window manager names no focused app.\nFocused window: Application Not Responding: com.example.app"
    );
    let keyguard = parse_focus("  mCurrentFocus=Window{3a u0 Keyguard}\n");
    assert!(foreground_report(&keyguard, None).ends_with("The lock screen is showing."));
    assert_eq!(
        parse_focus("mCurrentFocus=null\nmFocusedApp=null\n"),
        Focus::default()
    );
    assert_eq!(
        short_component("com.a.b/com.a.b.ui.Main"),
        "com.a.b/.ui.Main"
    );
    assert_eq!(
        short_component("com.a.b/com.other.Main"),
        "com.a.b/com.other.Main"
    );
    assert_eq!(
        short_component("com.a.b/com.a.bc.Main"),
        "com.a.b/com.a.bc.Main"
    );

    assert_eq!(lock_screen(POLICY_28_LOCKED), Some(true));
    assert_eq!(lock_screen(POLICY_33), Some(false));
    assert_eq!(
        lock_screen(
            &POLICY_33
                .replace("mIsShowing=false", "mIsShowing=true")
                .replace("showing=false", "showing=x")
        ),
        Some(true)
    );
    assert_eq!(
        lock_screen(&POLICY_28_LOCKED.replace("=true", "=false")),
        Some(false)
    );
    assert_eq!(
        lock_screen("WINDOW MANAGER POLICY STATE (dumpsys window policy)\n"),
        None
    );
}

/// Android 8.0: `firstInstallTime` with the package, the user's runtime permissions.
const PACKAGE_26: &str = "Activity Resolver Table:
  Non-Data Actions:
      android.intent.action.MAIN:
        5f1d2c3 com.example.app/.MainActivity filter 8e9a0b1
Packages:
  Package [com.example.app] (3f2a1b0):
    userId=10086
    pkg=Package{9c1e2d3 com.example.app}
    codePath=/data/app/com.example.app-1
    versionCode=7 minSdk=21 targetSdk=26
    versionName=1.0.6
    splits=[base]
    timeStamp=2018-03-04 10:11:12
    firstInstallTime=2018-03-04 10:11:13
    lastUpdateTime=2018-03-04 10:11:13
    installerPackageName=com.android.vending
    requested permissions:
      android.permission.INTERNET
      android.permission.CAMERA
      android.permission.ACCESS_FINE_LOCATION
    install permissions:
      android.permission.INTERNET: granted=true
    User 0: ceDataInode=409621 installed=true hidden=false suspended=false stopped=false notLaunched=false enabled=0 instant=false
      gids=[3003]
      runtime permissions:
        android.permission.CAMERA: granted=true, flags=[ USER_SET ]
        android.permission.ACCESS_FINE_LOCATION: granted=false, flags=[ USER_SET ]
";
/// Android 14: `firstInstallTime` moved into the user's part; a second block of the same
/// package (the factory version of an updated app) follows and is not read.
const PACKAGE_34: &str = "Packages:
  Package [com.example.app] (b7c1a2e):
    appId=10234
    pkg=Package{2f4d3c1 com.example.app}
    codePath=/data/app/~~Qx3vV0p8l3S6p_ZCJc2Kxg==/com.example.app-Fb1Ql4xY7r9sA9J0s8zJXw==
    primaryCpuAbi=arm64-v8a
    versionCode=42 minSdk=26 targetSdk=34
    minExtensionVersions=[]
    versionName=2.3.1
    usesNonSdkApi=false
    timeStamp=2026-08-30 14:02:11
    lastUpdateTime=2026-09-12 09:15:40
    installerPackageName=null
    installerPackageUid=-1
    requested permissions:
      android.permission.INTERNET
      android.permission.POST_NOTIFICATIONS
      android.permission.CAMERA
    install permissions:
      android.permission.INTERNET: granted=true
    User 0: ceDataInode=180213 deDataInode=0 installed=true hidden=false suspended=false distractionFlags=0 stopped=false notLaunched=false enabled=0 instant=false virtual=false quarantined=false
      installReason=4
      firstInstallTime=2026-08-30 14:02:12
      uninstallReason=0
      gids=[3003]
      runtime permissions:
        android.permission.POST_NOTIFICATIONS: granted=true, flags=[ USER_SET|USER_SENSITIVE_WHEN_GRANTED|USER_SENSITIVE_WHEN_DENIED]
        android.permission.CAMERA: granted=false, flags=[ USER_SENSITIVE_WHEN_GRANTED|USER_SENSITIVE_WHEN_DENIED]
      disabledComponents:
        com.example.app.DebugActivity
    User 10: ceDataInode=0 installed=true hidden=false
      runtime permissions:
        android.permission.CAMERA: granted=true, flags=[ USER_SET ]

Hidden system packages:
  Package [com.example.app] (0a1b2c3):
    versionCode=1 minSdk=26 targetSdk=33
    versionName=1.0
";

#[test]
fn a_packages_dump_is_read_for_its_version_times_and_permissions() {
    let old = parse_package_dump(PACKAGE_26, "com.example.app").unwrap();
    assert_eq!(
        old,
        PackageFacts {
            version_name: Some("1.0.6".into()),
            version_code: Some("7".into()),
            target_sdk: Some("26".into()),
            installed: Some("2018-03-04 10:11:13".into()),
            updated: Some("2018-03-04 10:11:13".into()),
            installer: Some("com.android.vending".into()),
            granted: vec!["android.permission.CAMERA".into()],
            denied: vec!["android.permission.ACCESS_FINE_LOCATION".into()],
        }
    );
    let new = parse_package_dump(PACKAGE_34, "com.example.app").unwrap();
    assert_eq!(new.version_code.as_deref(), Some("42"));
    assert_eq!(new.target_sdk.as_deref(), Some("34"));
    assert_eq!(new.installed.as_deref(), Some("2026-08-30 14:02:12"));
    assert_eq!(new.installer, None);
    assert_eq!(new.granted, vec!["android.permission.POST_NOTIFICATIONS"]);
    assert_eq!(new.denied, vec!["android.permission.CAMERA"]);
    assert_eq!(
        package_report("com.example.app", &new, "pid 4567"),
        "package: com.example.app
version: 2.3.1 (versionCode 42)
target SDK: 34
installed: 2026-08-30 14:02:12
updated: 2026-09-12 09:15:40
installer: none (adb or the system)
running: pid 4567
runtime permissions: granted android.permission.POST_NOTIFICATIONS; denied android.permission.CAMERA"
    );
    assert!(parse_package_dump(PACKAGE_34, "com.example").is_none());
    assert_eq!(
        package_report("com.a.b", &PackageFacts::default(), "no"),
        "package: com.a.b\nversion: unknown\ntarget SDK: unknown\ninstalled: unknown\nupdated: unknown\ninstaller: none (adb or the system)\nrunning: no\nruntime permissions: none"
    );
    let many = PackageFacts {
        granted: (0..80).map(|n| format!("p.G{n}")).collect(),
        denied: (0..40).map(|n| format!("p.D{n}")).collect(),
        ..PackageFacts::default()
    };
    let report = package_report("com.a.b", &many, "no");
    assert!(report.ends_with("p.D19 (the first 100 of 120)"), "{report}");
    assert!(!report.contains("p.D20"), "{report}");
}

#[test]
fn am_start_is_judged_by_what_it_said() {
    let starting = "Starting: Intent { act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER] cmp=com.example.app/.MainActivity }\n";
    assert_eq!(parse_am_start(&out(starting)), Started::Done);
    assert_eq!(
        parse_am_start(&out(&format!("{starting}Warning: Activity not started, its current task has been brought to the front\n"))),
        Started::AlreadyFront
    );
    assert_eq!(
        parse_am_start(&out(&format!("{starting}Error type 3\nError: Activity class {{com.example.app/com.example.app.Nope}} does not exist.\n"))),
        Started::Failed("Error: Activity class {com.example.app/com.example.app.Nope} does not exist.".into())
    );
    let unresolved = parse_am_start(&out("Starting: Intent { act=android.intent.action.VIEW dat=myapp://x/... }\nError: Activity not started, unable to resolve Intent { act=android.intent.action.VIEW dat=myapp://x/... flg=0x10000000 }\n"));
    assert!(
        matches!(&unresolved, Started::Failed(line) if line.contains("unable to resolve Intent")),
        "{unresolved:?}"
    );
    let denied = ShellResult {
        stdout: starting.as_bytes().to_vec(),
        stderr: b"Exception occurred while executing 'start':\njava.lang.SecurityException: Permission Denial: starting Intent { cmp=com.example.app/.Secret } from null (pid=4321, uid=2000) not exported from uid 10123\n\tat com.android.server.wm.ActivityTaskSupervisor.checkStartAnyActivityPermission(ActivityTaskSupervisor.java:1093)\n".to_vec(),
        exit: Some(255),
        truncated: false,
        timed_out: false,
    };
    // Device text in an answer is one line of at most 120 characters (`sanitize_for_message`).
    let line = "java.lang.SecurityException: Permission Denial: starting Intent { cmp=com.example.app/.Secret } from null (pid=4321, uid=2000) not exported from uid 10123";
    assert_eq!(
        parse_am_start(&denied),
        Started::Failed(sanitize_for_message(line))
    );
    assert!(
        matches!(parse_am_start(&denied), Started::Failed(said) if said.ends_with('…') && said.chars().count() == 121)
    );
    assert_eq!(
        parse_am_start(&FakeAndroid::failing("", 1)),
        Started::Failed("exit status 1".into())
    );
    // The echoed intent names the activity and the link: their words are not am's complaints.
    for echoed in [
        "Starting: Intent { act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER] cmp=com.foo/.ExceptionReportActivity }\n",
        "Starting: Intent { act=android.intent.action.VIEW dat=myapp://crash/NullPointerException }\n",
        "Starting: Intent { act=android.intent.action.VIEW dat=myapp://x?e=Error:1&f=Exception: x }\n",
    ] {
        assert_eq!(parse_am_start(&out(echoed)), Started::Done, "{echoed}");
    }
    let waited = "Starting: Intent { cmp=com.foo/.ErrorActivity }\nStatus: ok\nLaunchState: COLD\nActivity: com.foo/.ErrorActivity\nTotalTime: 412\nWaitTime: 415\nComplete\n";
    assert_eq!(parse_am_start(&out(waited)), Started::Done);
    // Its real complaints still are, after an echo with such words in it.
    assert_eq!(
        parse_am_start(&out("Starting: Intent { cmp=com.foo/.ExceptionActivity }\nError type 3\nError: Activity class {com.foo/com.foo.ExceptionActivity} does not exist.\n")),
        Started::Failed("Error: Activity class {com.foo/com.foo.ExceptionActivity} does not exist.".into())
    );
    assert_eq!(
        parse_am_start(&out(
            "Starting: Intent { cmp=com.foo/.Main }\nError type 3\n"
        )),
        Started::Failed("Error type 3".into())
    );
    assert_eq!(
        parse_am_start(&out("Starting: Intent { cmp=com.foo/.Main }\nSecurity exception: Permission Denial: starting Intent\n")),
        Started::Failed("Security exception: Permission Denial: starting Intent".into())
    );
}

/// Android 14 (a phone) and Android 8.1 (an emulator: `ro.kernel.qemu`, no AVD name).
const GETPROP_34: &str = "[dalvik.vm.heapsize]: [512m]
[persist.sys.locale]: [en-US]
[ro.build.version.release]: [14]
[ro.build.version.sdk]: [34]
[ro.build.version.security_patch]: [2026-09-05]
[ro.product.cpu.abi]: [arm64-v8a]
[ro.product.manufacturer]: [Google]
[ro.product.model]: [Pixel 8]
";
const GETPROP_27: &str = "[ro.build.version.release]: [8.1.0]
[ro.build.version.sdk]: [27]
[ro.kernel.qemu]: [1]
[ro.product.cpu.abi]: [x86]
[ro.product.locale]: [en-US]
[ro.product.manufacturer]: [Google]
[ro.product.model]: [Android SDK built for x86]
[ro.build.version.security_patch]: []
";

#[test]
fn the_device_block_reads_getprop() {
    assert_eq!(
        device_report(&parse_getprop(GETPROP_34)),
        "device:\n  maker: Google\n  model: Pixel 8\n  Android: 14 (SDK 34)\n  security patch: 2026-09-05\n  ABI: arm64-v8a\n  locale: en-US\n  emulator: no"
    );
    assert_eq!(
        device_report(&parse_getprop(GETPROP_27)),
        "device:\n  maker: Google\n  model: Android SDK built for x86\n  Android: 8.1.0 (SDK 27)\n  security patch: unknown\n  ABI: x86\n  locale: en-US\n  emulator: yes"
    );
    let avd = parse_getprop("[ro.boot.qemu.avd_name]: [Pixel_8_API_35]\n[ro.boot.qemu]: [1]\n");
    assert!(device_report(&avd).ends_with("emulator: yes (AVD Pixel_8_API_35)"));
    assert_eq!(
        parse_getprop("[a]: [b]]\n[weird\n"),
        BTreeMap::from([("a".to_owned(), "b]".to_owned())])
    );
}

#[test]
fn the_display_block_reads_its_marked_parts() {
    let overridden = "@@mewrk:size\nPhysical size: 1080x2400\nOverride size: 720x1600\n@@mewrk:density\nPhysical density: 420\nOverride density: 320\n@@mewrk:rotation\n1\n@@mewrk:accelerometer\n0\n@@mewrk:night\nNight mode: yes\n@@mewrk:animation\n0.5\n";
    assert_eq!(
        display_report(&sections(overridden)),
        "display:\n  size: 1080x2400 (override 720x1600)\n  density: 420 dpi (override 320)\n  rotation: landscape_left\n  auto-rotate: off\n  appearance: dark\n  animation scale: 0.5"
    );
    // A tablet whose natural orientation is landscape: its rotation 3 is the way it is held upright.
    let tablet = "@@mewrk:size\nPhysical size: 2560x1600\n@@mewrk:density\nPhysical density: 280\n@@mewrk:rotation\n3\n@@mewrk:accelerometer\n0\n@@mewrk:night\nNight mode: auto\n@@mewrk:animation\n1.0\n";
    assert_eq!(
        display_report(&sections(tablet)),
        "display:\n  size: 2560x1600\n  density: 280 dpi\n  rotation: portrait\n  auto-rotate: off\n  appearance: auto\n  animation scale: 1.0"
    );
    // Android 9: no uimode night yet, settings never written.
    let plain = "@@mewrk:size\nPhysical size: 1440x2960\n@@mewrk:density\nPhysical density: 560\n@@mewrk:rotation\nnull\n@@mewrk:accelerometer\n1\n@@mewrk:night\ncmd: Can't find service: uimode\n@@mewrk:animation\nnull\n";
    assert_eq!(
        display_report(&sections(plain)),
        "display:\n  size: 1440x2960\n  density: 560 dpi\n  rotation: auto (when locked: unknown)\n  auto-rotate: on\n  appearance: unknown\n  animation scale: 1 (default)"
    );
}

/// Android 8.0 charging over USB, awake; Android 14 emulator on battery, asleep.
const BATTERY_26: &str = "Current Battery Service state:
  AC powered: false
  USB powered: true
  Wireless powered: false
  Max charging current: 500000
  Max charging voltage: 5000000
  Charge counter: 2948000
  status: 2
  health: 2
  present: true
  level: 85
  scale: 100
  voltage: 4231
  temperature: 280
  technology: Li-ion
";
const BATTERY_34: &str = "Current Battery Service state:
  AC powered: false
  USB powered: false
  Wireless powered: false
  Dock powered: false
  Max charging current: 0
  Charge counter: 10000
  status: 3
  health: 2
  present: true
  level: 100
  scale: 100
  technology: Li-ion
  Charging state: 0
  Capacity level: -1
";
const POWER_26: &str = "POWER MANAGER (dumpsys power)

Power Manager State:
  mDirty=0x0
  mWakefulness=Awake
  mWakefulnessChanging=false
  mIsPowered=true
  mPlugType=2
Display Power: state=ON
";
const POWER_34: &str = "POWER MANAGER (dumpsys power)

Power Manager State:
  Settings power_manager_constants:
    no_cached_wake_locks=true
  mDirty=0x0
  mWakefulness=Asleep
  mWakefulnessChanging=false
  mIsPowered=false
Display Power: state=OFF
";

#[test]
fn the_power_block_reads_battery_power_and_lock() {
    let charging = format!(
        "@@mewrk:battery\n{BATTERY_26}@@mewrk:power\n{POWER_26}@@mewrk:policy\n{POLICY_28_LOCKED}"
    );
    assert_eq!(
        power_report(&sections(&charging)),
        "power:\n  battery: 85% (charging)\n  plugged in: yes (USB)\n  screen: on\n  wakefulness: Awake\n  lock screen: showing"
    );
    let asleep = format!(
        "@@mewrk:battery\n{BATTERY_34}@@mewrk:power\n{POWER_34}@@mewrk:policy\n{POLICY_33}"
    );
    assert_eq!(
        power_report(&sections(&asleep)),
        "power:\n  battery: 100% (discharging)\n  plugged in: no\n  screen: off\n  wakefulness: Asleep\n  lock screen: not showing"
    );
    assert_eq!(
        power_report(&BTreeMap::new()),
        "power:\n  battery: unknown\n  plugged in: unknown\n  screen: unknown\n  wakefulness: unknown\n  lock screen: unknown"
    );
    assert_eq!(
        parse_power("  Display Power: state=DOZE\n")
            .display
            .as_deref(),
        Some("DOZE")
    );
}

/// Android 9 and Android 13 (`--noredact`); the archive after the active list is not read.
const NOTIFICATIONS_28: &str = "Current Notification Manager state:
  Notification List:
    NotificationRecord(0x0e14d6ea: pkg=com.android.systemui user=UserHandle{0} id=7 tag=null importance=2 key=0|com.android.systemui|7|null|10021: Notification(channel=BAT pri=0 contentView=null vibrate=null sound=null defaults=0x0 flags=0x8 color=0xff607d8b vis=PRIVATE))
      uid=10021 userId=0
      icon=Icon(typ=RESOURCE pkg=com.android.systemui id=0x7f080202)
      pri=0
      key=0|com.android.systemui|7|null|10021
      extras={
        android.title=String (Battery is low)
        android.reduced.images=Boolean (true)
        android.subText=null
        android.text=String (15% remaining)
        android.progress=Integer (0)
      }
  mArchive=Archive (2 notifications)
    NotificationRecord(0x0aaaaaaa: pkg=com.old.gone user=UserHandle{0} id=1 tag=null importance=3)
        android.title=String (old)
";
const NOTIFICATIONS_33: &str = "Current Notification Manager state:
  Notification List:
    NotificationRecord(0x08f7e5a2: pkg=com.google.android.gm user=UserHandle{0} id=-1489348226 tag=null importance=3 key=0|com.google.android.gm|-1489348226|null|10165: Notification(channel=^sq^ shortcut=null contentView=null vibrate=null sound=null defaults=0x0 flags=0x218 color=0xffd93025 category=email vis=PRIVATE))
      uid=10165 userId=0
      opPkg=com.google.android.gm
      flags=0x218
      key=0|com.google.android.gm|-1489348226|null|10165
      notification=
        extras={
          android.title=String (Alice)
          android.text=SpannableString (Lunch tomorrow? (12:30))
          android.bigText=SpannableString (Lunch tomorrow? Let me know)
        }
    NotificationRecord(0x0b2c3d4e: pkg=com.whatsapp user=UserHandle{0} id=1 tag=null importance=4 key=0|com.whatsapp|1|null|10200: Notification(channel=individual_chat))
      uid=10200 userId=0
        extras={
          android.title=String [length=3]
          android.text=String [length=12]
        }
    NotificationRecord(0x0c3d4e5f: pkg=android user=UserHandle{-1} id=17040400 tag=null importance=1 key=-1|android|17040400|null|1000: Notification(channel=DEVELOPER))
      uid=1000 userId=-1
";

#[test]
fn notifications_are_read_from_the_active_list() {
    assert_eq!(
        parse_notifications(NOTIFICATIONS_28),
        vec![Notice {
            package: "com.android.systemui".into(),
            title: Some("Battery is low".into()),
            text: Some("15% remaining".into()),
        }]
    );
    assert_eq!(
        notification_report(&parse_notifications(NOTIFICATIONS_33)),
        "notifications: 3 showing
  com.google.android.gm: title \"Alice\", text \"Lunch tomorrow? (12:30)\"
  com.whatsapp: title \"(hidden by this Android version)\", text \"(hidden by this Android version)\"
  android: no title or text"
    );
    assert_eq!(notification_report(&[]), "notifications: none showing");
    // An extra's value is printed raw: a text of several lines goes on at the start of the next
    // lines, and neither ends the record nor the list.
    assert_eq!(
        notification_report(&parse_notifications(NOTIFICATIONS_MULTILINE)),
        "notifications: 3 showing
  com.google.android.gm: title \"Meeting tomorrow\", text \"Hi Bob, see you at 10. Notes: bring the slides\"
  com.example.chat: title \"Carol\", text \"two\"
  com.android.systemui: title \"USB debugging connected\", text \"Tap to turn off\""
    );
    let long = format!("  Notification List:\n    NotificationRecord(0x1: pkg=com.a.b user=UserHandle{{0}})\n      android.text=String ({})\n", "x".repeat(500));
    let notices = parse_notifications(&long);
    assert_eq!(
        notices[0].text.as_ref().unwrap().chars().count(),
        NOTIFICATION_CHARS + 1
    );
    let many: Vec<Notice> = (0..35)
        .map(|n| Notice {
            package: format!("com.app.n{n}"),
            title: None,
            text: None,
        })
        .collect();
    let report = notification_report(&many);
    assert!(
        report.starts_with("notifications: 35 showing (the first 30 listed)"),
        "{report}"
    );
    assert_eq!(report.lines().count(), 31);
}

/// Android 14, `--noredact`: BigTextStyle's text and a ticker of several lines, printed raw.
const NOTIFICATIONS_MULTILINE: &str = "Current Notification Manager state:
  Notification List:
    NotificationRecord(0x01: pkg=com.google.android.gm user=UserHandle{0} id=1 tag=null importance=3 key=0|com.google.android.gm|1|null|10165: Notification(channel=mail))
      uid=10165 userId=0
      tickerText=Meeting tomorrow
Hi Bob:
      notification=
        extras={
          android.title=String (Meeting tomorrow)
          android.bigText=SpannableString (Hi Bob,
see you at 10.
mArchive=not the archive
Lights List:
Alice)
          android.text=SpannableString (Hi Bob,
see you at 10.
Notes: bring the slides)
          android.showWhen=Boolean (true)
        }
    NotificationRecord(0x02: pkg=com.example.chat user=UserHandle{0} id=2 tag=null importance=4 key=0|com.example.chat|2|null|10201: Notification(channel=chat))
      uid=10201 userId=0
        extras={
          android.title=String (Carol)
          android.textLines=CharSequence[] (one
two)
          android.text=String (two)
        }
    NotificationRecord(0x03: pkg=com.android.systemui user=UserHandle{0} id=3 tag=null importance=2 key=0|com.android.systemui|3|null|10021: Notification(channel=DEV))
      uid=10021 userId=0
        extras={
          android.title=String (USB debugging connected)
          android.text=String (Tap to turn off)
        }

  Lights List:
    0|com.old.app|1|null|10300
  mUseAttentionLight=false
  mArchive=Archive (1 notification)
    NotificationRecord(0x04: pkg=com.old.gone user=UserHandle{0} id=1 tag=null importance=3)
";

const LOG: &str = "--------- beginning of main
10-18 01:02:03.456  1234  1250 I ActivityManager: Start proc 4567:com.example.app/u0a123 for activity {com.example.app/com.example.app.MainActivity}
10-18 01:02:03.789  4567  4567 D ExampleApp: onCreate
--------- beginning of crash
10-18 01:02:04.001  4567  4567 E AndroidRuntime: FATAL EXCEPTION: main
10-18 01:02:04.001  4567  4567 E AndroidRuntime: Process: com.example.app, PID: 4567
10-18 01:02:04.001  4567  4567 E AndroidRuntime: java.lang.IllegalStateException: Boom
";

fn logcat_query(input: Value) -> Logcat {
    match parse("android_logcat", input).unwrap().call {
        Call::Logcat(query) => query,
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_log_report_says_what_was_read() {
    let report = log_report(LOG, &logcat_query(json!({})), None, false, None);
    assert!(
        report.starts_with("logcat default buffers, last 5 lines:\n10-18 01:02:03.456"),
        "{report}"
    );
    assert!(!report.contains("beginning of"), "{report}");
    let query = logcat_query(
        json!({"lines": 2, "contains": "BOOM", "package": "com.example.app", "level": "E", "tag": "AndroidRuntime", "buffer": "crash"}),
    );
    assert_eq!(
        log_report(LOG, &query, Some(LogSource::Uid(10123)), false, None),
        "logcat buffer crash, last 1 line, level E and above, tag AndroidRuntime, app com.example.app (uid 10123), containing \"BOOM\":\n10-18 01:02:04.001  4567  4567 E AndroidRuntime: java.lang.IllegalStateException: Boom"
    );
    let last_two = log_report(
        LOG,
        &logcat_query(json!({"lines": 2, "buffer": "all"})),
        Some(LogSource::Pid(4567)),
        true,
        None,
    );
    assert_eq!(
        last_two,
        "logcat all buffers, last 2 lines:\n10-18 01:02:04.001  4567  4567 E AndroidRuntime: Process: com.example.app, PID: 4567\n10-18 01:02:04.001  4567  4567 E AndroidRuntime: java.lang.IllegalStateException: Boom\n(The log was longer than 8 MiB; its newest lines were not read. Ask for fewer lines, or narrow it with package, tag or level.)"
    );
    assert_eq!(
        log_report("", &logcat_query(json!({})), None, false, None),
        "No log lines matched."
    );
    assert_eq!(
        log_report(
            LOG,
            &logcat_query(json!({"contains": "nothing like it"})),
            None,
            false,
            None
        ),
        "No log lines matched."
    );
    // Filters that picked lines out of the newest ones say so when fewer matched than asked for.
    assert_eq!(
        log_report(
            LOG,
            &logcat_query(json!({"contains": "nothing like it"})),
            None,
            false,
            Some(10_000)
        ),
        "No log lines matched.\n(Only the newest 10000 lines of the log were searched.)"
    );
    let few = log_report(
        LOG,
        &logcat_query(json!({"level": "E"})),
        None,
        false,
        Some(10_000),
    );
    assert!(
        few.starts_with("logcat default buffers, last 5 lines, level E and above:")
            && few.ends_with("\n(Only the newest 10000 lines of the log were searched.)"),
        "{few}"
    );
    let enough = log_report(
        LOG,
        &logcat_query(json!({"level": "E", "lines": 2})),
        None,
        false,
        Some(10_000),
    );
    assert!(!enough.contains("newest"), "{enough}");
    let wide = format!("10-18 01:02:03.456  1  1 I Tag: {}\n", "y".repeat(3000));
    let report = log_report(&wide, &logcat_query(json!({})), None, false, None);
    assert_eq!(
        report.lines().nth(1).unwrap().chars().count(),
        LOG_LINE_CHARS + 1
    );
}

#[test]
fn a_shell_answer_has_its_status_and_its_streams() {
    let result = ShellResult {
        stdout: b"line one\n\x1b[31mred\x1b[0m\tok\r\n".to_vec(),
        stderr: b"warn\n".to_vec(),
        exit: Some(3),
        truncated: false,
        timed_out: false,
    };
    assert_eq!(
        shell_report(&result),
        "exit 3\nstdout:\nline one\n[31mred[0m\tok\nstderr:\nwarn"
    );
    let raw = ShellResult {
        stdout: vec![b'x'; 10],
        exit: None,
        truncated: true,
        ..ShellResult::default()
    };
    assert_eq!(
        shell_report(&raw),
        "exit status unknown (this device has no framed shell; stderr is mixed into stdout)\nstdout:\nxxxxxxxxxx\n(cut at 32 KiB)"
    );
    assert_eq!(shell_report(&out("")), "exit 0\n(no output)");
    let stopped = ShellResult {
        stdout: b"partial\n".to_vec(),
        stderr: b"warn\n".to_vec(),
        timed_out: true,
        truncated: true,
        ..ShellResult::default()
    };
    assert_eq!(
        shell_report(&stopped),
        "exit status unknown (the command had not ended when it was stopped)\nstdout:\npartial\nstderr:\nwarn\n(cut at 32 KiB)"
    );
    assert_eq!(
        stopped_report(&stopped, Duration::from_millis(3000)),
        "Stopped after 3000 ms: the command did not end. Output so far:\nstdout:\npartial\nstderr:\nwarn\n(cut at 32 KiB)"
    );
    assert_eq!(
        stopped_report(
            &ShellResult {
                timed_out: true,
                ..ShellResult::default()
            },
            Duration::from_millis(1000)
        ),
        "Stopped after 1000 ms: the command did not end."
    );
    assert_eq!(
        shell_report(&out("caf\u{e9} \u{fffd}\n")),
        "exit 0\nstdout:\ncafé \u{fffd}"
    );
    let invalid = ShellResult {
        stdout: vec![0x66, 0xff, 0x67],
        exit: Some(0),
        ..ShellResult::default()
    };
    assert_eq!(shell_report(&invalid), "exit 0\nstdout:\nf\u{fffd}g");
}

#[test]
fn small_readers_hold_their_shapes() {
    assert_eq!(night_mode("Night mode: no\n"), "light");
    assert_eq!(night_mode("Night mode: yes\n"), "dark");
    assert_eq!(night_mode("Night mode: auto\n"), "auto");
    assert_eq!(night_mode("Night mode: custom_schedule\n"), "custom");
    assert_eq!(night_mode("cmd: Can't find service: uimode\n"), "unknown");
    assert_eq!(
        rotation_name(Some("1"), Some("2"), Some(false)),
        "auto-rotate"
    );
    assert_eq!(
        rotation_name(Some("0"), Some("3"), Some(false)),
        "landscape_right (locked)"
    );
    // The display's natural orientation is what the number counts from; without it, no name.
    assert_eq!(
        rotation_name(Some("0"), Some("3"), Some(true)),
        "portrait (locked)"
    );
    assert_eq!(rotation_name(Some("0"), Some("3"), None), "unknown");
    assert_eq!(rotation_name(None, None, Some(false)), "unknown");
    assert_eq!(status_line("Success\n"), Some("Success".into()));
    assert_eq!(
        status_line("noise\nFailure [DELETE_FAILED_INTERNAL_ERROR]\n"),
        Some("Failure [DELETE_FAILED_INTERNAL_ERROR]".into())
    );
    assert_eq!(status_line("Failed\n"), Some("Failed".into()));
    assert_eq!(status_line(""), None);
    assert_eq!(human_size(999), "999 bytes");
    assert_eq!(human_size(12_345), "12.3 KB");
    assert_eq!(human_size(12_300_000), "12.3 MB");
    assert_eq!(brief("abcdef", 3), "abc…");
    assert_eq!(and_list(&["a", "b", "c"]), "a, b and c");
    assert_eq!(
        complaint("Exception occurred while executing 'grant':\njava.lang.SecurityException: Package com.a.b has not requested permission android.permission.CAMERA\n\tat x\n"),
        Some("java.lang.SecurityException: Package com.a.b has not requested permission android.permission.CAMERA".into())
    );
    assert_eq!(complaint("Night mode: yes\n"), None);
    assert_eq!(
        sections("noise\n@@mewrk:a\n1\n2\n@@mewrk:b\n"),
        BTreeMap::from([
            ("a".to_owned(), "1\n2\n".to_owned()),
            ("b".to_owned(), String::new())
        ])
    );
    // What a dump prints is indented: a title holding a marker's text does not start a part.
    assert_eq!(
        sections(
            "@@mewrk:window\n    mCurrentFocus=Window{1 u0 x\n  @@mewrk:policy\nshowing=false}\n"
        ),
        BTreeMap::from([(
            "window".to_owned(),
            "    mCurrentFocus=Window{1 u0 x\n  @@mewrk:policy\nshowing=false}\n".to_owned()
        )])
    );
    assert_eq!(
        complaint("cmd: Can't find service: statusbar\n"),
        Some("cmd: Can't find service: statusbar".into())
    );
    assert_eq!(
        install_hint("Failure [INSTALL_FAILED_VERSION_DOWNGRADE: Downgrade detected]"),
        " The installed version is newer: pass downgrade true (debuggable apps only), or uninstall it first."
    );
    assert_eq!(install_hint("Failure [INSTALL_FAILED_SOMETHING_NEW]"), "");
}

// ------------------------------------------------------------- running on a device

#[test]
fn a_screen_without_adb_is_not_an_android_device() {
    let desktop = FakeDriver::new(crate::remote_control::driver::Platform::Desktop, 10, 10);
    assert_eq!(said(run_on(&desktop, "screen_app", json!({}))), NOT_ANDROID);
}

#[test]
fn apps_are_listed_by_launcher_or_by_package() {
    let device = phone(34, |command| match command {
        LAUNCHERS_COMMAND => out(LAUNCHERS_34),
        "pm list packages -3" => out("package:com.example.app\npackage:com.android.chrome\n"),
        "pm list packages" => {
            out("package:android\npackage:com.android.settings\npackage:com.example.app\n")
        }
        other => panic!("{other}"),
    });
    assert_eq!(
        ran(&device, "screen_app", json!({})),
        "Launchable apps on Fake Pixel: 3 (showing 3).
com.android.chrome  com.android.chrome/com.google.android.apps.chrome.Main
com.android.settings  com.android.settings/.Settings
com.google.android.apps.maps  com.google.android.apps.maps/com.google.android.maps.MapsActivity"
    );
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"filter": "GOOGLE", "limit": 1})
        ),
        "Launchable apps on Fake Pixel containing \"GOOGLE\": 2 (showing 1).
com.android.chrome  com.android.chrome/com.google.android.apps.chrome.Main"
    );
    assert_eq!(
        ran(&device, "screen_app", json!({"scope": "third_party"})),
        "Third-party packages on Fake Pixel: 2 (showing 2).\ncom.android.chrome\ncom.example.app"
    );
    assert_eq!(
        ran(&device, "screen_app", json!({"scope": "all", "filter": "settings"})),
        "Installed packages on Fake Pixel containing \"settings\": 1 (showing 1).\ncom.android.settings"
    );
    assert_eq!(
        commands(&device),
        vec![
            LAUNCHERS_COMMAND,
            LAUNCHERS_COMMAND,
            "pm list packages -3",
            "pm list packages"
        ]
    );
    for call in device.android_fake().shell_calls() {
        assert_eq!(call.limit, LIST_READ);
        assert!(call.timeout <= COMMAND_TIME);
    }
}

#[test]
fn an_android_that_cannot_list_launchers_lists_third_party_packages_and_says_so() {
    let note = "This Android version cannot list launcher activities (cmd package query-activities needs Android 7.1), so these are its third-party packages instead.\nThird-party packages on Fake Pixel: 1 (showing 1).\ncom.example.app";
    // Android 7.0 is not asked at all.
    let old = phone(24, |command| match command {
        "pm list packages -3" => out("package:com.example.app\n"),
        other => panic!("{other}"),
    });
    assert_eq!(ran(&old, "screen_app", json!({})), note);
    assert_eq!(commands(&old), vec!["pm list packages -3"]);
    // A build that answers with an error is asked for its packages next.
    let odd = phone(25, |command| match command {
        LAUNCHERS_COMMAND => {
            FakeAndroid::failing("Error: unknown command 'query-activities'\n", 255)
        }
        "pm list packages -3" => out("package:com.example.app\n"),
        other => panic!("{other}"),
    });
    assert_eq!(ran(&odd, "screen_app", json!({})), note);
    // No launchable app at all is an empty list, not a fallback.
    let empty = phone(34, |_| out("No activities found\n"));
    assert_eq!(
        ran(&empty, "screen_app", json!({})),
        "Launchable apps on Fake Pixel: 0 (showing 0)."
    );
    let broken = phone(34, |_| {
        FakeAndroid::failing(
            "cmd: Failure calling service package: Broken pipe (32)\n",
            1,
        )
    });
    assert_eq!(
        said(run_on(&broken, "screen_app", json!({"scope": "all"}))),
        "The device could not list its packages: cmd: Failure calling service package: Broken pipe (32)"
    );
}

#[test]
fn the_app_in_front_is_asked_again_while_the_window_manager_names_none() {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let device = phone(33, move |command| {
        assert_eq!(command, "echo @@mewrk:window; dumpsys window displays 2>&1; echo @@mewrk:policy; dumpsys window policy 2>&1");
        if counted.fetch_add(1, Ordering::SeqCst) < 2 {
            out("@@mewrk:window\n  mCurrentFocus=null\n  mFocusedApp=null\n@@mewrk:policy\n")
        } else {
            out(&format!(
                "@@mewrk:window\n{DISPLAYS_33_LOCKED}@@mewrk:policy\n{}",
                POLICY_33.replace("showing=false", "showing=true")
            ))
        }
    });
    assert_eq!(
        ran(&device, "screen_app", json!({"action": "foreground"})),
        "In front: com.google.android.apps.nexuslauncher/.NexusLauncherActivity\nFocused window: NotificationShade\nThe lock screen is showing."
    );
    assert_eq!(asked.load(Ordering::SeqCst), 3);
    // Android 9 reads `dumpsys window windows`; and a window manager that never names a focus is
    // reported as such once the patience is spent.
    let old = phone(28, |command| {
        assert!(command.contains("dumpsys window windows"), "{command}");
        out("@@mewrk:window\nmCurrentFocus=null\n@@mewrk:policy\n")
    });
    let started = Instant::now();
    assert_eq!(
        ran(&old, "screen_app", json!({"action": "foreground"})),
        "No app is in front: the window manager names no focused app."
    );
    assert!(started.elapsed() >= FOCUS_PATIENCE);
    // A call stopped while it waits says so rather than "nothing in front".
    let stopped = AtomicBool::new(false);
    let device = phone(33, |_| out("@@mewrk:window\nmCurrentFocus=null\n"));
    let call = parse("screen_app", json!({"action": "foreground"})).unwrap();
    let cancel = || stopped.swap(true, Ordering::SeqCst);
    assert_eq!(
        said(call.run(&*device, &Step::for_tests(&cancel, Duration::from_secs(20)))),
        "Cancelled before reading the app in front again."
    );
}

#[test]
fn an_apps_details_come_from_three_commands() {
    let device = phone(34, |command| match command {
        "pm path 'com.example.app'" => {
            out("package:/data/app/~~Qx3==/com.example.app-Fb1==/base.apk\n")
        }
        "pidof 'com.example.app'" => out("4567\n"),
        "dumpsys package 'com.example.app'" => out(PACKAGE_34),
        "pm path 'com.gone.app'" => FakeAndroid::failing("", 1),
        other => panic!("{other}"),
    });
    let answer = ran(
        &device,
        "screen_app",
        json!({"action": "info", "app": "com.example.app"}),
    );
    assert!(
        answer.starts_with("package: com.example.app\nversion: 2.3.1 (versionCode 42)\n"),
        "{answer}"
    );
    assert!(answer.contains("\nrunning: pid 4567\n"), "{answer}");
    let calls = device.android_fake().shell_calls();
    assert_eq!(calls[2].limit, PACKAGE_DUMP_READ);
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "info", "app": "com.gone.app"})
        )),
        "com.gone.app is not installed."
    );
    // Stopped between commands: the next one does not start, and the answer says which.
    let stopping = Arc::new(AtomicBool::new(false));
    let flag = stopping.clone();
    let device = phone(34, move |command| {
        flag.store(true, Ordering::SeqCst);
        match command {
            "pm path 'com.example.app'" => out("package:/data/app/base.apk\n"),
            other => panic!("ran {other} after the cancel"),
        }
    });
    let call = parse(
        "screen_app",
        json!({"action": "info", "app": "com.example.app"}),
    )
    .unwrap();
    let cancelled = || stopping.load(Ordering::SeqCst);
    assert_eq!(
        said(call.run(
            &*device,
            &Step::for_tests(&cancelled, Duration::from_secs(20))
        )),
        "Cancelled before checking whether com.example.app runs."
    );
    // Out of time before anything: nothing runs.
    let device = phone(34, |command| panic!("ran {command}"));
    let call = parse(
        "screen_app",
        json!({"action": "info", "app": "com.example.app"}),
    )
    .unwrap();
    assert_eq!(
        said(call.run(&*device, &Step::for_tests(&|| false, Duration::ZERO))),
        "Ran out of this call's time before checking that com.example.app is installed."
    );
}

const STARTING: &str = "Starting: Intent { act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER] cmp=com.example.app/.MainActivity }\n";
const INSTALLED: &str = "package:/data/app/com.example.app-1/base.apk\n";

#[test]
fn an_app_is_launched_by_its_resolved_launcher() {
    let device = phone(34, |command| {
        match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        c if c == resolve_launcher_command("com.example.app") => out("priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.example.app/.MainActivity\n"),
        "am start -n 'com.example.app/.MainActivity' -a android.intent.action.MAIN -c android.intent.category.LAUNCHER" => out(STARTING),
        "am start -n 'com.example.app/.Settings$Pane' -a android.intent.action.MAIN -c android.intent.category.LAUNCHER" => out(&format!("{STARTING}Warning: Activity not started, its current task has been brought to the front\n")),
        "am force-stop 'com.example.app'" => out(""),
        other => panic!("{other}"),
    }
    });
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app"})
        ),
        "Launched com.example.app/.MainActivity."
    );
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app", "restart": true})
        ),
        "Force-stopped com.example.app and launched com.example.app/.MainActivity."
    );
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app", "activity": ".Settings$Pane"})
        ),
        "com.example.app/.Settings$Pane was already running; brought it to the front."
    );
    assert_eq!(
        commands(&device)[3..6],
        [
            "pm path 'com.example.app'",
            "am force-stop 'com.example.app'",
            resolve_launcher_command("com.example.app").as_str(),
        ]
    );
}

#[test]
fn an_app_without_one_launcher_goes_through_monkey() {
    let monkey = Arc::new(Mutex::new(
        "Events injected: 1\n## Network stats: elapsed time=12ms\n".to_owned(),
    ));
    let answer = monkey.clone();
    let device = phone(34, move |command| {
        match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        c if c.starts_with("cmd package resolve-activity") => out("priority=0 preferredOrder=0 match=0x0 specificIndex=-1 isDefault=false\nandroid/com.android.internal.app.ResolverActivity\n"),
        "monkey -p 'com.example.app' --pct-syskeys 0 -c android.intent.category.LAUNCHER 1" => out(&answer.lock().unwrap()),
        other => panic!("{other}"),
    }
    });
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app"})
        ),
        "Launched com.example.app (its launcher activity)."
    );
    *monkey.lock().unwrap() = "** No activities found to run, monkey aborted.\n".to_owned();
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app"})
        )),
        "com.example.app has no launcher activity; pass activity to start one of its activities."
    );
    // Before Android 7.0 there is no `cmd` to ask.
    let old = phone(23, |command| match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        c if c.starts_with("monkey") => out("Events injected: 1\n"),
        other => panic!("{other}"),
    });
    ran(
        &old,
        "screen_app",
        json!({"action": "launch", "app": "com.example.app"}),
    );
    assert_eq!(commands(&old).len(), 2);
}

#[test]
fn a_launch_that_fails_says_why_and_what_was_done_before() {
    let device = phone(34, |command| {
        match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        "pm path 'com.not.here'" => out(""),
        c if c.starts_with("am start") => out(&format!("{STARTING}Error type 3\nError: Activity class {{com.example.app/com.example.app.Nope}} does not exist.\n")),
        "am force-stop 'com.example.app'" => out(""),
        other => panic!("{other}"),
    }
    });
    assert_eq!(
        said(run_on(&device, "screen_app", json!({"action": "launch", "app": "com.example.app", "activity": ".Nope"}))),
        "Could not launch com.example.app/.Nope: Error: Activity class {com.example.app/com.example.app.Nope} does not exist."
    );
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.not.here"})
        )),
        "com.not.here is not installed."
    );
    // Cancelled after the force-stop: the answer says it happened.
    let stopping = Arc::new(AtomicBool::new(false));
    let flag = stopping.clone();
    let device = phone(34, move |command| match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        "am force-stop 'com.example.app'" => {
            flag.store(true, Ordering::SeqCst);
            out("")
        }
        other => panic!("ran {other} after the cancel"),
    });
    let call = parse(
        "screen_app",
        json!({"action": "launch", "app": "com.example.app", "restart": true}),
    )
    .unwrap();
    let cancelled = || stopping.load(Ordering::SeqCst);
    assert_eq!(
        said(call.run(&*device, &Step::for_tests(&cancelled, Duration::from_secs(20)))),
        "Force-stopped com.example.app. Cancelled before finding com.example.app's launcher activity."
    );
}

#[test]
fn apps_are_stopped_and_urls_opened() {
    let device = phone(34, |command| {
        match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        "pm path 'com.gone.app'" => out(""),
        "am force-stop 'com.example.app'" => out(""),
        "am start -a android.intent.action.VIEW -d 'https://example.com/?a=1&b=2'" => out("Starting: Intent { act=android.intent.action.VIEW dat=https://example.com/... }\n"),
        "am start -a android.intent.action.VIEW -d 'myapp://open' 'com.example.app'" => out("Starting: Intent { act=android.intent.action.VIEW dat=myapp://open pkg=com.example.app }\nError: Activity not started, unable to resolve Intent { act=android.intent.action.VIEW dat=myapp://open flg=0x10000000 pkg=com.example.app }\n"),
        "am start -a android.intent.action.VIEW -d 'nope://x'" => out("Starting: Intent { act=android.intent.action.VIEW dat=nope://x }\nError: Activity not started, unable to resolve Intent { act=android.intent.action.VIEW dat=nope://x flg=0x10000000 }\n"),
        "am start -a android.intent.action.VIEW -d 'secret://x'" => FakeAndroid::failing("java.lang.SecurityException: Permission Denial: starting Intent { dat=secret://x }\n", 255),
        other => panic!("{other}"),
    }
    });
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"action": "stop", "app": "com.example.app"})
        ),
        "Stopped com.example.app."
    );
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "stop", "app": "com.gone.app"})
        )),
        "com.gone.app is not installed."
    );
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"action": "open_url", "url": "https://example.com/?a=1&b=2"})
        ),
        "Opened https://example.com/?a=1&b=2."
    );
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "open_url", "url": "myapp://open", "app": "com.example.app"})
        )),
        "com.example.app does not handle this URL."
    );
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "open_url", "url": "nope://x"})
        )),
        "No app on the device handles this URL."
    );
    assert_eq!(
        said(run_on(&device, "screen_app", json!({"action": "open_url", "url": "secret://x"}))),
        "Could not open the URL: java.lang.SecurityException: Permission Denial: starting Intent { dat=secret://x }"
    );
}

#[test]
fn an_apk_is_streamed_with_its_flags_and_pms_answer_is_the_result() {
    let files = Files::new();
    let apk: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
    files.write("app-debug.apk", &apk);
    let android = FakeAndroid::new(34, |command| panic!("{command}")).installing(|install| {
        Ok(if install.flags.contains(&"-d".to_owned()) {
            "Failure [INSTALL_FAILED_VERSION_DOWNGRADE: Downgrade detected: Update version code 7 is older than current 42]".to_owned()
        } else {
            "Success".to_owned()
        })
    });
    let device = FakeDriver::with_android(android);
    let install = |input: Value| {
        let call = parse_in(&files, "screen_package", input).unwrap();
        call.run(
            &*device,
            &Step::for_tests(&|| false, Duration::from_secs(290)),
        )
    };
    assert_eq!(
        install(json!({"action": "install", "path": "app-debug.apk"})).unwrap(),
        "Installed app-debug.apk (200.0 KB) on Fake Pixel."
    );
    let installs = device.android_fake().installs();
    assert_eq!(installs[0].bytes, apk);
    assert_eq!(installs[0].size, 200_000);
    assert_eq!(installs[0].flags, vec!["-r", "-t"]);
    assert!(
        installs[0].timeout > Duration::from_secs(280),
        "{:?}",
        installs[0].timeout
    );
    assert_eq!(
        said(install(json!({"action": "install", "path": "app-debug.apk", "downgrade": true}))),
        "The device did not install app-debug.apk: Failure [INSTALL_FAILED_VERSION_DOWNGRADE: Downgrade detected: Update version code 7 is older than current 42] The installed version is newer: pass downgrade true (debuggable apps only), or uninstall it first."
    );
    assert_eq!(
        device.android_fake().installs()[1].flags,
        vec!["-r", "-t", "-d"]
    );
    assert!(device.android_fake().commands().is_empty());

    // A device too old for a streamed install says so in its own words; a cancel mid-stream is
    // the driver's failure, which the session reports with the call's name.
    let old = FakeDriver::with_android(FakeAndroid::new(23, |_| out("")).installing(|_| {
        Err(DriverError::Unsupported(
            "This device's Android is too old for streamed installs (it needs Android 7.0)."
                .to_owned(),
        ))
    }));
    let call = parse_in(
        &files,
        "screen_package",
        json!({"action": "install", "path": "app-debug.apk"}),
    )
    .unwrap();
    assert_eq!(
        said(call.run(&*old, &Step::for_tests(&|| false, Duration::from_secs(60)))),
        "This device's Android is too old for streamed installs (it needs Android 7.0)."
    );
    let reads = AtomicUsize::new(0);
    let cancelled = || reads.fetch_add(1, Ordering::SeqCst) >= 2;
    let call = parse_in(
        &files,
        "screen_package",
        json!({"action": "install", "path": "app-debug.apk"}),
    )
    .unwrap();
    let fresh = FakeDriver::with_android(FakeAndroid::new(34, |_| out("")));
    match call.run(
        &*fresh,
        &Step::for_tests(&cancelled, Duration::from_secs(60)),
    ) {
        Err(OpError::Driver(DriverError::Failed(message))) => {
            assert!(
                message.contains("stopped after 65536 of 200000 bytes"),
                "{message}"
            )
        }
        other => panic!("{other:?}"),
    }
    assert!(fresh.android_fake().installs().is_empty());
}

#[test]
fn packages_are_uninstalled_cleared_and_granted() {
    let device = phone(34, |command| {
        match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        "pm path 'com.android.phone'" => out("package:/system/priv-app/TeleService/TeleService.apk\n"),
        "cmd package uninstall 'com.example.app'" => out("Success\n"),
        "cmd package uninstall -k 'com.example.app'" => out("Success\n"),
        "cmd package uninstall 'com.android.phone'" => out("Failure [DELETE_FAILED_INTERNAL_ERROR]\n"),
        "pm clear 'com.example.app'" => out("Success\n"),
        "pm clear 'com.android.phone'" => out("Failed\n"),
        "pm grant 'com.example.app' 'android.permission.CAMERA'" => out(""),
        "pm revoke 'com.example.app' 'android.permission.CAMERA'" => out(""),
        "pm grant 'com.example.app' 'android.permission.READ_SMS'" => FakeAndroid::failing("Exception occurred while executing 'grant':\njava.lang.SecurityException: Package com.example.app has not requested permission android.permission.READ_SMS\n\tat com.android.server.pm.permission.PermissionManagerServiceImpl.grantRuntimePermissionInternal(PermissionManagerServiceImpl.java:1412)\n", 255),
        other => panic!("{other}"),
    }
    });
    let package = |input: Value| run_on(&device, "screen_package", input);
    assert_eq!(
        package(json!({"action": "uninstall", "app": "com.example.app"})).unwrap(),
        "Uninstalled com.example.app."
    );
    assert_eq!(
        package(json!({"action": "uninstall", "app": "com.example.app", "keep_data": true}))
            .unwrap(),
        "Uninstalled com.example.app and kept its data and cache."
    );
    assert_eq!(
        said(package(
            json!({"action": "uninstall", "app": "com.android.phone"})
        )),
        "The device did not uninstall com.android.phone: Failure [DELETE_FAILED_INTERNAL_ERROR]"
    );
    assert_eq!(
        package(json!({"action": "clear_data", "app": "com.example.app"})).unwrap(),
        "Erased all data of com.example.app."
    );
    assert_eq!(
        said(package(
            json!({"action": "clear_data", "app": "com.android.phone"})
        )),
        "The device did not erase com.android.phone's data: Failed"
    );
    assert_eq!(
        package(json!({"action": "grant", "app": "com.example.app", "permission": "android.permission.CAMERA"})).unwrap(),
        "Granted android.permission.CAMERA to com.example.app."
    );
    assert_eq!(
        package(json!({"action": "revoke", "app": "com.example.app", "permission": "android.permission.CAMERA"})).unwrap(),
        "Revoked android.permission.CAMERA from com.example.app."
    );
    assert_eq!(
        said(package(json!({"action": "grant", "app": "com.example.app", "permission": "android.permission.READ_SMS"}))),
        "Could not grant android.permission.READ_SMS to com.example.app: java.lang.SecurityException: Package com.example.app has not requested permission android.permission.READ_SMS"
    );
    // Before Android 7.0 the uninstall goes through pm.
    let old = phone(23, |command| match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        "pm uninstall 'com.example.app'" => out("Success\n"),
        other => panic!("{other}"),
    });
    assert_eq!(
        ran(
            &old,
            "screen_package",
            json!({"action": "uninstall", "app": "com.example.app"})
        ),
        "Uninstalled com.example.app."
    );
}

#[test]
fn an_apps_log_lines_are_found_by_uid_or_by_pid() {
    let device = phone(30, |command| match command {
        "cmd package list packages -U 'com.example.app'" => {
            out("package:com.example.app.debug uid:10124\npackage:com.example.app uid:10123\n")
        }
        "cmd package list packages -U 'com.gone.app'" => out(""),
        "logcat -d -v threadtime -t 10000 --uid=10123 -- '*:E'" => out(LOG),
        "logcat -d -v threadtime -t 10000 --uid=10123" => out(LOG),
        "logcat -d -v threadtime -t 10000 -- 'AndroidRuntime:V' '*:S'" => out(LOG),
        "logcat -d -v threadtime -t 10000 -b 'all'" => out(LOG),
        "logcat -d -v threadtime -t 200 -b 'crash'" => FakeAndroid::failing(
            "logcat: Unable to open log device '/dev/log/crash': No such file or directory\n",
            1,
        ),
        other => panic!("{other}"),
    });
    let answer = ran(
        &device,
        "android_logcat",
        json!({"package": "com.example.app", "level": "E", "lines": 200, "contains": "process"}),
    );
    assert_eq!(
        answer,
        "logcat default buffers, last 1 line, level E and above, app com.example.app (uid 10123), containing \"process\":\n10-18 01:02:04.001  4567  4567 E AndroidRuntime: Process: com.example.app, PID: 4567\n(Only the newest 10000 lines of the log were searched.)"
    );
    // logcat counts -t before it applies a uid, a tag or a level: with any of them (or contains,
    // applied here) the newest 10000 lines are searched, and the last `lines` that match kept.
    ran(
        &device,
        "android_logcat",
        json!({"lines": 2000, "contains": "x", "buffer": "all"}),
    );
    let app = ran(
        &device,
        "android_logcat",
        json!({"package": "com.example.app", "lines": 2}),
    );
    assert_eq!(
        app,
        "logcat default buffers, last 2 lines, app com.example.app (uid 10123):\n10-18 01:02:04.001  4567  4567 E AndroidRuntime: Process: com.example.app, PID: 4567\n10-18 01:02:04.001  4567  4567 E AndroidRuntime: java.lang.IllegalStateException: Boom"
    );
    assert!(ran(
        &device,
        "android_logcat",
        json!({"tag": "AndroidRuntime", "lines": 2})
    )
    .starts_with("logcat default buffers, last 2 lines, tag AndroidRuntime:"));
    assert_eq!(
        said(run_on(
            &device,
            "android_logcat",
            json!({"package": "com.gone.app"})
        )),
        "com.gone.app is not installed."
    );
    assert_eq!(
        said(run_on(&device, "android_logcat", json!({"buffer": "crash"}))),
        "logcat failed: logcat: Unable to open log device '/dev/log/crash': No such file or directory"
    );
    let reads: Vec<usize> = device
        .android_fake()
        .shell_calls()
        .iter()
        .filter(|call| call.command.starts_with("logcat"))
        .map(|call| call.limit)
        .collect();
    assert!(reads.iter().all(|limit| *limit == LOG_READ), "{reads:?}");

    let old = phone(29, |command| match command {
        "pidof 'com.example.app'" => out("4567\n"),
        "pidof 'com.idle.app'" => FakeAndroid::failing("", 1),
        "logcat -d -v threadtime -t 3 --pid=4567" => out(LOG),
        "logcat -d -v threadtime -t 10000 --pid=4567 -- 'ExampleApp:V' '*:S'" => out(LOG),
        other => panic!("{other}"),
    });
    assert!(ran(
        &old,
        "android_logcat",
        json!({"package": "com.example.app", "lines": 3})
    )
    .starts_with("logcat default buffers, last 3 lines, app com.example.app (pid 4567):"));
    // logd counts a pid with the tail itself; a tag still is applied after it.
    assert!(ran(
        &old,
        "android_logcat",
        json!({"package": "com.example.app", "tag": "ExampleApp", "lines": 3})
    )
    .starts_with(
        "logcat default buffers, last 3 lines, tag ExampleApp, app com.example.app (pid 4567):"
    ));
    assert_eq!(
        said(run_on(&old, "android_logcat", json!({"package": "com.idle.app"}))),
        "com.idle.app is not running, so its lines cannot be told apart on this Android version; launch it first, or read buffer crash without package."
    );
}

#[test]
fn the_devices_state_is_read_by_section() {
    let device = TreePhone::new(
        phone(34, |command| {
            if command == "getprop" {
                return out(GETPROP_34);
            }
            if command == display_command() {
                return out("@@mewrk:size\nPhysical size: 1080x2400\n@@mewrk:density\nPhysical density: 420\n@@mewrk:rotation\n0\n@@mewrk:accelerometer\n1\n@@mewrk:night\nNight mode: no\n@@mewrk:animation\n1.0\n");
            }
            if command == power_command() {
                return out(&format!(
                    "@@mewrk:battery\n{BATTERY_26}@@mewrk:power\n{POWER_26}@@mewrk:policy\n{POLICY_33}"
                ));
            }
            if command == "dumpsys notification --noredact" {
                return out(NOTIFICATIONS_28);
            }
            panic!("{command}")
        }),
        vec![app_screen()],
    );
    assert_eq!(
        run_driver(&*device, "screen_device", json!({})).unwrap(),
        "device:\n  maker: Google\n  model: Pixel 8\n  Android: 14 (SDK 34)\n  security patch: 2026-09-05\n  ABI: arm64-v8a\n  locale: en-US\n  emulator: no
display:\n  size: 1080x2400\n  density: 420 dpi\n  rotation: auto (when locked: portrait)\n  auto-rotate: on\n  appearance: light\n  animation scale: 1.0
power:\n  battery: 85% (charging)\n  plugged in: yes (USB)\n  screen: on\n  wakefulness: Awake\n  lock screen: not showing
dialog: none showing"
    );
    assert_eq!(
        run_driver(
            &*device,
            "screen_device",
            json!({"action": "info", "sections": ["notifications"]})
        )
        .unwrap(),
        "notifications: 1 showing\n  com.android.systemui: title \"Battery is low\", text \"15% remaining\"\ndialog: none showing"
    );
    assert_eq!(commands(&device.phone).len(), 4);
    assert_eq!(device.reads(), 2);
    // Out of time after the first section: what was read is the answer, and it says where it
    // stopped; the dialog is not looked for.
    let slow = TreePhone::new(
        phone(34, |command| {
            thread::sleep(Duration::from_millis(400));
            if command == "getprop" {
                out(GETPROP_34)
            } else {
                panic!("{command}")
            }
        }),
        vec![app_screen()],
    );
    let call = parse("screen_device", json!({"sections": ["device", "power"]})).unwrap();
    let answer = call
        .run(
            &*slow,
            &Step::for_tests(&|| false, Duration::from_millis(500)),
        )
        .unwrap();
    assert!(answer.starts_with("device:\n  maker: Google"), "{answer}");
    assert!(
        answer.ends_with("\n(Ran out of this call's time before reading the power state.)"),
        "{answer}"
    );
    assert_eq!(slow.reads(), 0);
}

/// The dialog line of `screen_device info`: the buttons of the dialog showing, by the labels an
/// answer lists; a screen without an element tree says so, and a tree that cannot be read leaves
/// the rest of the answer.
#[test]
fn info_lists_the_dialog_showing_and_its_buttons() {
    let sections = json!({"action": "info", "sections": ["device"]});
    let device = |trees: Vec<UiNode>| TreePhone::new(phone(34, |_| out(GETPROP_34)), trees);
    let dialog_line = |tree: UiNode| {
        let answer = run_driver(&*device(vec![tree]), "screen_device", sections.clone()).unwrap();
        answer
            .lines()
            .skip_while(|line| !line.starts_with("dialog:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(dialog_line(app_screen()), "dialog: none showing");
    // An AlertDialog: the ids button1 to button3, in the order of the tree.
    assert_eq!(
        dialog_line(discard_dialog()),
        "dialog: showing; buttons: \"Cancel\", \"Discard\""
    );
    // The permission controller's: its clickable nodes, whatever their ids. The text of the dialog
    // is not a button, and a typographic apostrophe reads as an ASCII one in a listed label, with
    // the mark an answer puts on a label it changed.
    assert_eq!(
        dialog_line(permission_dialog("com.google.android.permissioncontroller")),
        "dialog: showing; buttons: \"While using the app\", \"Only this time\", \"Don't allow…\""
    );
    assert_eq!(
        dialog_line(permission_dialog("com.android.permissioncontroller")),
        "dialog: showing; buttons: \"While using the app\", \"Only this time\", \"Don't allow…\""
    );
    // No element tree at all (a driver without one).
    let bare = phone(34, |_| out(GETPROP_34));
    assert_eq!(
        run_on(&bare, "screen_device", sections.clone()).unwrap(),
        format!(
            "{}\ndialog: unknown (this screen has no element tree)",
            device_block_34()
        )
    );
    // A tree that cannot be read: the sections read before it are still the answer.
    let failing = device(vec![app_screen()]);
    failing.fail_after(
        0,
        DriverError::Failed("uiautomator could not read the screen (null root node)".to_owned()),
    );
    assert_eq!(
        run_driver(&*failing, "screen_device", sections.clone()).unwrap(),
        format!(
            "{}\n(Reading the dialog showing failed: uiautomator could not read the screen (null root node).)",
            device_block_34()
        )
    );
    // A device that went away is the session's to handle.
    failing.fail_after(
        0,
        DriverError::Disconnected("Android device \"x\" is gone".to_owned()),
    );
    assert!(matches!(
        run_driver(&*failing, "screen_device", sections),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
}

/// `screen_device info`'s device block for [`GETPROP_34`].
fn device_block_34() -> &'static str {
    "device:\n  maker: Google\n  model: Pixel 8\n  Android: 14 (SDK 34)\n  security patch: 2026-09-05\n  ABI: arm64-v8a\n  locale: en-US\n  emulator: no"
}

/// A phone whose settings live in a map, so each change can be read back.
#[derive(Clone)]
struct Setup {
    sdk: u32,
    /// Whether the lock screen shows.
    locked: Arc<AtomicBool>,
    /// Whether the keyguard asks for a PIN, pattern or password.
    credential: bool,
    /// Whether the sleep key brings the lock screen up.
    locks_on_sleep: bool,
    /// What `wm size` prints after `Physical size: `.
    size: &'static str,
    /// Whether the keyboard shows.
    keyboard: Arc<AtomicBool>,
    /// Whether `dumpsys input_method` prints `mInputShown` at all.
    reports_keyboard: bool,
}

impl Setup {
    fn new(sdk: u32) -> Self {
        Self {
            sdk,
            locked: Arc::new(AtomicBool::new(false)),
            credential: false,
            locks_on_sleep: true,
            size: "1080x2400",
            keyboard: Arc::new(AtomicBool::new(false)),
            reports_keyboard: true,
        }
    }

    fn phone(&self) -> Arc<FakeDriver> {
        let setup = self.clone();
        let state: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(BTreeMap::from([
            ("accelerometer_rotation".to_owned(), "1".to_owned()),
            ("user_rotation".to_owned(), "0".to_owned()),
            ("night".to_owned(), "no".to_owned()),
            ("stay_on_while_plugged_in".to_owned(), "0".to_owned()),
            ("wakefulness".to_owned(), "Asleep".to_owned()),
        ])));
        phone(self.sdk, move |command| {
            let mut state = state.lock().unwrap();
            let mut printed = String::new();
            for simple in command.split("; ") {
                // A marked read sends each part's stderr with its stdout.
                let words: Vec<&str> = simple
                    .split_whitespace()
                    .filter(|word| *word != "2>&1")
                    .collect();
                match words.as_slice() {
                    ["echo", marker] => printed.push_str(&format!("{marker}\n")),
                    ["wm", "size"] => printed.push_str(&format!("Physical size: {}\n", setup.size)),
                    ["settings", "get", _, key] => printed.push_str(&format!(
                        "{}\n",
                        state.get(*key).map_or("null", String::as_str)
                    )),
                    ["settings", "put", _, key, value] => {
                        state.insert((*key).to_owned(), (*value).to_owned());
                    }
                    ["dumpsys", "power"] => printed.push_str(&format!(
                        "  mWakefulness={}\n  Display Power: state={}\n",
                        state["wakefulness"],
                        if state["wakefulness"] == "Awake" {
                            "ON"
                        } else {
                            "OFF"
                        }
                    )),
                    ["dumpsys", "window", "policy"] => printed.push_str(&format!(
                        "    KeyguardServiceDelegate\n      showing={}\n",
                        setup.locked.load(Ordering::SeqCst)
                    )),
                    ["dumpsys", "input_method"] => {
                        if setup.reports_keyboard {
                            printed.push_str(&format!(
                                "  mShowRequested=false mShowExplicitlyRequested=false mShowForced=false mInputShown={}\n",
                                setup.keyboard.load(Ordering::SeqCst)
                            ));
                        } else {
                            printed.push_str("  mCurMethodId=com.example.ime/.Service\n");
                        }
                    }
                    ["input", "keyevent", "224"] => {
                        state.insert("wakefulness".into(), "Awake".into());
                    }
                    ["input", "keyevent", "223"] => {
                        state.insert("wakefulness".into(), "Asleep".into());
                        if setup.locks_on_sleep {
                            setup.locked.store(true, Ordering::SeqCst);
                        }
                    }
                    // Back hides a keyboard that shows, and otherwise leaves the app.
                    ["input", "keyevent", "4"] => {
                        assert!(
                            setup.keyboard.swap(false, Ordering::SeqCst),
                            "Back pressed with no keyboard showing"
                        );
                    }
                    ["wm", "dismiss-keyguard"] => {
                        if !setup.credential {
                            setup.locked.store(false, Ordering::SeqCst);
                        }
                    }
                    ["cmd", "uimode", "night"] => {
                        printed.push_str(&format!("Night mode: {}\n", state["night"]))
                    }
                    ["cmd", "uimode", "night", value] => {
                        let value = value.trim_matches('\'');
                        state.insert("night".into(), value.to_owned());
                        printed.push_str(&format!("Night mode: {value}\n"));
                    }
                    ["cmd", "window", "user-rotation", "lock", rotation] => {
                        state.insert("accelerometer_rotation".into(), "0".into());
                        state.insert("user_rotation".into(), (*rotation).to_owned());
                    }
                    ["cmd", "window", "user-rotation", "free"] => {
                        state.insert("accelerometer_rotation".into(), "1".into());
                    }
                    ["cmd", "statusbar", _] => {}
                    ["svc", "power", "stayon", value] => {
                        state.insert(
                            "stay_on_while_plugged_in".into(),
                            if *value == "true" { "7" } else { "0" }.into(),
                        );
                    }
                    other => panic!("unexpected {other:?} in {command}"),
                }
            }
            out(&printed)
        })
    }
}

/// The commands a phone was sent that change something: not the reads.
fn changes(device: &FakeDriver) -> Vec<String> {
    commands(device)
        .into_iter()
        .filter(|command| {
            !command.starts_with("echo")
                && !command.starts_with("dumpsys")
                && !command.starts_with("settings get")
                && command != "cmd uimode night"
        })
        .collect()
}

#[test]
fn settings_change_and_say_what_they_were() {
    let setup = Setup::new(34);
    let device = setup.phone();
    let set = |input: Value| ran(&device, "screen_device", input);
    assert_eq!(
        set(json!({"action": "wake"})),
        "Woke the device; it was asleep."
    );
    assert_eq!(
        set(json!({"action": "wake"})),
        "The device was awake already."
    );
    assert_eq!(
        set(json!({"action": "rotate", "value": "landscape_left"})),
        "The rotation is now landscape_left (locked); it was auto-rotate."
    );
    assert_eq!(
        set(json!({"action": "rotate", "value": "auto"})),
        "The rotation is now auto-rotate; it was landscape_left (locked)."
    );
    assert_eq!(
        set(json!({"action": "appearance", "value": "dark"})),
        "Appearance is now dark; it was light."
    );
    assert_eq!(
        set(json!({"action": "appearance", "value": "auto"})),
        "Appearance is now auto; it was dark."
    );
    assert_eq!(
        set(json!({"action": "animations", "value": "off"})),
        "Animations are now off (window, transition and animator duration scales 0); they were 1 (default), 1 (default) and 1 (default)."
    );
    assert_eq!(
        set(json!({"action": "animations", "value": "on"})),
        "Animations are now on (window, transition and animator duration scales 1); they were 0, 0 and 0."
    );
    assert_eq!(
        set(json!({"action": "notifications"})),
        "Opened the notification shade."
    );
    assert_eq!(
        set(json!({"action": "quick_settings"})),
        "Opened quick settings."
    );
    assert_eq!(
        set(json!({"action": "collapse"})),
        "Collapsed the notification shade."
    );
    assert_eq!(
        set(json!({"action": "stay_awake", "value": "on"})),
        "Staying awake while charging is now on; it was off."
    );
    assert_eq!(
        set(json!({"action": "lock"})),
        "Put the device to sleep; it was awake. The screen is off and locked."
    );
    assert!(setup.locked.load(Ordering::SeqCst));
    assert_eq!(set(json!({"action": "unlock"})), "Unlocked the device.");
    assert_eq!(
        set(json!({"action": "unlock"})),
        "The device is awake and its lock screen is not showing."
    );
    assert_eq!(
        changes(&device),
        vec![
            "input keyevent 224",
            "input keyevent 224",
            "cmd window user-rotation lock 1",
            "cmd window user-rotation free",
            "cmd uimode night 'yes'",
            "cmd uimode night 'auto'",
            "settings put global window_animation_scale 0; settings put global transition_animation_scale 0; settings put global animator_duration_scale 0",
            "settings put global window_animation_scale 1; settings put global transition_animation_scale 1; settings put global animator_duration_scale 1",
            "cmd statusbar 'expand-notifications'",
            "cmd statusbar 'expand-settings'",
            "cmd statusbar 'collapse'",
            "svc power stayon true",
            "input keyevent 223",
            "input keyevent 224",
            "wm dismiss-keyguard",
            "input keyevent 224",
        ]
    );
}

/// The sleep key only turns the screen off; whether that locks the device is the device's setting,
/// so the answer reads it once and says which it was.
#[test]
fn a_lock_says_whether_the_device_locked() {
    // A device that locks when it sleeps.
    let setup = Setup::new(34);
    let device = setup.phone();
    ran(&device, "screen_device", json!({"action": "wake"}));
    assert_eq!(
        ran(&device, "screen_device", json!({"action": "lock"})),
        "Put the device to sleep; it was awake. The screen is off and locked."
    );
    // Asleep and locked already.
    assert_eq!(
        ran(&device, "screen_device", json!({"action": "lock"})),
        "The device was asleep already. The screen is off and locked."
    );
    // A device whose lock screen is not up when it is read says what was seen, and neither claims
    // it locked nor that it never locks.
    let setup = Setup {
        locks_on_sleep: false,
        ..Setup::new(34)
    };
    let device = setup.phone();
    ran(&device, "screen_device", json!({"action": "wake"}));
    assert_eq!(
        ran(&device, "screen_device", json!({"action": "lock"})),
        "Put the device to sleep; it was awake. The screen is off; the lock screen was not showing half a second later."
    );
    assert!(!setup.locked.load(Ordering::SeqCst));
    // Exactly one read of the lock screen follows the key, and that read is one command.
    let sent = commands(&device);
    let key = sent
        .iter()
        .position(|command| command == "input keyevent 223")
        .unwrap();
    assert_eq!(
        sent[key + 1..],
        [marked(&[
            ("power", "dumpsys power"),
            ("policy", POLICY_COMMAND)
        ])]
    );
    // A policy dump that says nothing about the lock screen is said not to be known.
    let silent = phone(34, |command| match command {
        "dumpsys power" => out("  mWakefulness=Asleep\n"),
        "input keyevent 223" => out(""),
        c if c.starts_with("echo @@mewrk:power") => {
            out("@@mewrk:power\n  mWakefulness=Asleep\n@@mewrk:policy\n")
        }
        other => panic!("{other}"),
    });
    assert_eq!(
        ran(&silent, "screen_device", json!({"action": "lock"})),
        "The device was asleep already. The screen is off; whether the lock screen is showing could not be read."
    );
    // Stopped between the key and the read: the answer says the device was put to sleep.
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let device = phone(34, move |command| match command {
        "dumpsys power" => out("  mWakefulness=Awake\n"),
        "input keyevent 223" => {
            flag.store(true, Ordering::SeqCst);
            out("")
        }
        other => panic!("ran {other} after the cancel"),
    });
    let call = parse("screen_device", json!({"action": "lock"})).unwrap();
    let cancelled = || stopped.load(Ordering::SeqCst);
    assert_eq!(
        said(call.run(
            &*device,
            &Step::for_tests(&cancelled, Duration::from_secs(20))
        )),
        "Put the device to sleep; it was awake. Cancelled before reading the lock screen."
    );
    // A screen that stayed on is not reported as locked.
    let awake = phone(34, |command| {
        match command {
        "dumpsys power" => out("  mWakefulness=Awake\n"),
        "input keyevent 223" => out(""),
        c if c.starts_with("echo @@mewrk:power") => out(
            "@@mewrk:power\n  mWakefulness=Awake\n  Display Power: state=ON\n@@mewrk:policy\n    KeyguardServiceDelegate\n      showing=false\n",
        ),
        other => panic!("{other}"),
    }
    });
    assert_eq!(
        ran(&awake, "screen_device", json!({"action": "lock"})),
        "Put the device to sleep; it was awake. The screen is still on after the sleep key; something may be keeping the device awake."
    );
}

/// `rotate` is by how the device is turned from portrait; Android's numbers count from the
/// display's natural orientation, so a phone and a tablet get different ones for the same turn.
#[test]
fn rotate_converts_the_devices_turn_to_the_displays_own_rotation_numbers() {
    // A phone: natural portrait. Rotation 1 is the device turned a quarter counter-clockwise.
    for (value, number) in [
        ("portrait", "0"),
        ("landscape_left", "1"),
        ("portrait_upside_down", "2"),
        ("landscape_right", "3"),
    ] {
        let setup = Setup::new(34);
        let device = setup.phone();
        let said = ran(
            &device,
            "screen_device",
            json!({"action": "rotate", "value": value}),
        );
        assert_eq!(
            said,
            format!("The rotation is now {value} (locked); it was auto-rotate."),
            "{value}"
        );
        assert_eq!(
            changes(&device),
            vec![format!("cmd window user-rotation lock {number}")],
            "{value} on a phone"
        );
    }
    // A tablet: natural landscape. Portrait is rotation 3 there, and landscape_left (portrait
    // turned a quarter counter-clockwise) is the natural orientation, rotation 0.
    for (value, number) in [
        ("portrait", "3"),
        ("landscape_left", "0"),
        ("portrait_upside_down", "1"),
        ("landscape_right", "2"),
    ] {
        let setup = Setup {
            size: "2560x1600",
            ..Setup::new(34)
        };
        let device = setup.phone();
        ran(
            &device,
            "screen_device",
            json!({"action": "rotate", "value": value}),
        );
        assert_eq!(
            changes(&device),
            vec![format!("cmd window user-rotation lock {number}")],
            "{value} on a tablet"
        );
    }
    // The previous value is reported in the same words, whichever display it was read from.
    for size in ["1080x2400", "2560x1600"] {
        for value in [
            "portrait",
            "landscape_left",
            "landscape_right",
            "portrait_upside_down",
        ] {
            let device = Setup {
                size,
                ..Setup::new(34)
            }
            .phone();
            ran(
                &device,
                "screen_device",
                json!({"action": "rotate", "value": value}),
            );
            assert_eq!(
                ran(
                    &device,
                    "screen_device",
                    json!({"action": "rotate", "value": "auto"})
                ),
                format!("The rotation is now auto-rotate; it was {value} (locked)."),
                "{size} {value}"
            );
        }
    }
    // An override of the size is what the window manager rotates by.
    let setup = Setup {
        size: "1080x2400\nOverride size: 2400x1080",
        ..Setup::new(34)
    };
    let device = setup.phone();
    ran(
        &device,
        "screen_device",
        json!({"action": "rotate", "value": "portrait"}),
    );
    assert_eq!(
        changes(&device),
        vec!["cmd window user-rotation lock 3".to_owned()]
    );
    // Android 9 writes the two rotation settings itself, with the same numbers.
    let old = Setup::new(28).phone();
    assert_eq!(
        ran(
            &old,
            "screen_device",
            json!({"action": "rotate", "value": "portrait_upside_down"})
        ),
        "The rotation is now portrait_upside_down (locked); it was auto-rotate."
    );
    assert!(commands(&old).contains(
        &"settings put system accelerometer_rotation 0; settings put system user_rotation 2"
            .to_owned()
    ));
    // Without the display's size there is no natural orientation to count from: a locked
    // rotation is refused before anything is changed, and auto-rotate does not need one.
    let nosize = phone(34, |command| match command {
        c if c.starts_with("echo @@mewrk:size") => {
            out("@@mewrk:size\nwm: not found\n@@mewrk:accelerometer\n1\n@@mewrk:rotation\n0\n")
        }
        "cmd window user-rotation free" => out(""),
        other => panic!("{other}"),
    });
    assert_eq!(
        said(run_on(
            &nosize,
            "screen_device",
            json!({"action": "rotate", "value": "landscape_left"})
        )),
        NATURAL_ORIENTATION_UNKNOWN
    );
    assert_eq!(
        ran(
            &nosize,
            "screen_device",
            json!({"action": "rotate", "value": "auto"})
        ),
        "The rotation is now auto-rotate; it was auto-rotate."
    );
}

#[test]
fn the_conversion_follows_the_window_managers_own_table() {
    // AOSP's DisplayRotation.configure: natural portrait has portrait 0, landscape 90,
    // upside-down 180 and seascape 270; natural landscape has landscape 0, seascape 180,
    // portrait 270 and upside-down 90 (with config_reverseDefaultRotation unset).
    let table = [
        (false, Orientation::Portrait, 0),
        (false, Orientation::LandscapeLeft, 1),
        (false, Orientation::PortraitUpsideDown, 2),
        (false, Orientation::LandscapeRight, 3),
        (true, Orientation::LandscapeLeft, 0),
        (true, Orientation::PortraitUpsideDown, 1),
        (true, Orientation::LandscapeRight, 2),
        (true, Orientation::Portrait, 3),
    ];
    for (landscape, orientation, rotation) in table {
        assert_eq!(surface_rotation(orientation, landscape), rotation);
        assert_eq!(orientation_of(rotation, landscape), orientation);
    }
    assert_eq!(natural_landscape("Physical size: 1080x2400\n"), Some(false));
    assert_eq!(natural_landscape("Physical size: 2560x1600\n"), Some(true));
    // Square counts as portrait; an override replaces the physical size.
    assert_eq!(natural_landscape("Physical size: 1000x1000\n"), Some(false));
    assert_eq!(
        natural_landscape("Physical size: 1080x2400\nOverride size: 2400x1080\n"),
        Some(true)
    );
    assert_eq!(natural_landscape("wm: not found\n"), None);
    assert_eq!(natural_landscape("Physical size: wide\n"), None);
    assert_eq!(natural_landscape(""), None);
}

#[test]
fn appearance_takes_light_dark_and_auto() {
    let device = Setup::new(34).phone();
    let set = |value: &str| {
        ran(
            &device,
            "screen_device",
            json!({"action": "appearance", "value": value}),
        )
    };
    assert_eq!(set("dark"), "Appearance is now dark; it was light.");
    assert_eq!(set("dark"), "Appearance is now dark; it was dark.");
    assert_eq!(set("auto"), "Appearance is now auto; it was dark.");
    assert_eq!(set("light"), "Appearance is now light; it was auto.");
    assert_eq!(
        changes(&device),
        vec![
            "cmd uimode night 'yes'",
            "cmd uimode night 'yes'",
            "cmd uimode night 'auto'",
            "cmd uimode night 'no'",
        ]
    );
}

#[test]
fn the_keyboard_is_dismissed_only_while_it_shows() {
    // Showing: Back is pressed, and the answer says the keyboard is gone.
    let setup = Setup::new(34);
    setup.keyboard.store(true, Ordering::SeqCst);
    let device = setup.phone();
    assert_eq!(
        ran(
            &device,
            "screen_device",
            json!({"action": "dismiss_keyboard"})
        ),
        "The keyboard was showing; pressed Back and it is gone."
    );
    assert_eq!(changes(&device), vec!["input keyevent 4"]);
    // Not showing: nothing is pressed, since Back would leave the app's screen.
    assert_eq!(
        ran(
            &device,
            "screen_device",
            json!({"action": "dismiss_keyboard"})
        ),
        "No keyboard was showing; nothing was pressed."
    );
    assert_eq!(changes(&device), vec!["input keyevent 4"]);
    // A keyboard that is still up after Back says so.
    let stubborn = phone(34, |command| match command {
        "dumpsys input_method" => out("  mInputShown=true\n"),
        "input keyevent 4" => out(""),
        other => panic!("{other}"),
    });
    assert_eq!(
        ran(
            &stubborn,
            "screen_device",
            json!({"action": "dismiss_keyboard"})
        ),
        "The keyboard was showing; pressed Back, but it is still showing."
    );
    // An Android whose dump has no such field: nothing is pressed, and the answer says why.
    let setup = Setup {
        reports_keyboard: false,
        ..Setup::new(34)
    };
    let silent = setup.phone();
    let said = said(run_on(
        &silent,
        "screen_device",
        json!({"action": "dismiss_keyboard"}),
    ));
    assert!(
        said.starts_with("Could not tell whether the keyboard is showing"),
        "{said}"
    );
    assert!(changes(&silent).is_empty());
}

#[test]
fn the_input_method_dump_is_read_across_android_versions() {
    // Android 9 to 13: `mInputShown` ends the line of `mShowRequested` and the others.
    assert_eq!(
        input_shown("  mShowRequested=true mShowExplicitlyRequested=false mShowForced=false mInputShown=true\n"),
        Some(true)
    );
    assert_eq!(
        input_shown("  mShowRequested=false mShowExplicitlyRequested=false mShowForced=false mInputShown=false\n"),
        Some(false)
    );
    // Android 14 and later: the visibility state computer's own line.
    assert_eq!(
        input_shown("  ImeVisibilityStateComputer:\n   mRequestedShowExplicitly=false mShowForced=false\n   mImeHiddenByDisplayPolicy=false\n   mInputShown=true\n"),
        Some(true)
    );
    // Shown anywhere is shown (a dump holding the state of several users).
    assert_eq!(
        input_shown("mInputShown=false\nmInputShown=true\n"),
        Some(true)
    );
    assert_eq!(
        input_shown("mInputShown=false\nmInputShown=false\n"),
        Some(false)
    );
    // A dump without the field, an error, nothing.
    assert_eq!(
        input_shown("Input Method Service:\n  mCurMethodId=x\n"),
        None
    );
    assert_eq!(input_shown("cmd: Can't find service: input_method\n"), None);
    assert_eq!(input_shown(""), None);
    // Only the whole word counts.
    assert_eq!(input_shown("xmInputShown=true\nmInputShown=truest\n"), None);
}

#[test]
fn a_lock_with_a_credential_stays_the_users_to_open() {
    let setup = Setup {
        credential: true,
        ..Setup::new(34)
    };
    setup.locked.store(true, Ordering::SeqCst);
    let device = setup.phone();
    assert_eq!(
        said(run_on(
            &device,
            "screen_device",
            json!({"action": "unlock"})
        )),
        LOCKED_WITH_CREDENTIAL
    );
    assert!(setup.locked.load(Ordering::SeqCst));
    // Nothing that looks like typing a credential was sent.
    assert!(
        commands(&device)
            .iter()
            .all(|command| !command.starts_with("input text")),
        "{:?}",
        commands(&device)
    );
    // A device that cannot dismiss its keyguard with wm is given the menu key.
    let device = phone(23, |command| match command {
        "input keyevent 224" | "input keyevent 82" => out(""),
        "wm dismiss-keyguard" => {
            FakeAndroid::failing("Error: unknown command 'dismiss-keyguard'\n", 1)
        }
        "dumpsys window policy" => out("    mShowingLockscreen=true mShowingDream=false\n"),
        other => panic!("{other}"),
    });
    assert_eq!(
        said(run_on(
            &device,
            "screen_device",
            json!({"action": "unlock"})
        )),
        LOCKED_WITH_CREDENTIAL
    );
    assert!(commands(&device).contains(&"input keyevent 82".to_owned()));
}

/// `activate` brings a running app to the front, which on Android is the launch of its launcher
/// activity: the commands are launch's, and so are the answers.
#[test]
fn activate_brings_an_app_to_the_front_as_a_launch_does() {
    let device = phone(34, |command| {
        match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        c if c == resolve_launcher_command("com.example.app") => out(
            "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.example.app/.MainActivity\n",
        ),
        "am start -n 'com.example.app/.MainActivity' -a android.intent.action.MAIN -c android.intent.category.LAUNCHER" => out(&format!(
            "{STARTING}Warning: Activity not started, its current task has been brought to the front\n"
        )),
        "pm path 'com.gone.app'" => out(""),
        other => panic!("{other}"),
    }
    });
    assert_eq!(
        ran(
            &device,
            "screen_app",
            json!({"action": "activate", "app": "com.example.app"})
        ),
        "com.example.app/.MainActivity was already running; brought it to the front."
    );
    assert_eq!(
        commands(&device),
        vec![
            "pm path 'com.example.app'".to_owned(),
            resolve_launcher_command("com.example.app"),
            "am start -n 'com.example.app/.MainActivity' -a android.intent.action.MAIN -c android.intent.category.LAUNCHER"
                .to_owned(),
        ]
    );
    // It never stops the app first.
    assert!(commands(&device)
        .iter()
        .all(|command| !command.contains("force-stop")));
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "activate", "app": "com.gone.app"})
        )),
        "com.gone.app is not installed."
    );
}

// ------------------------------------------------------------- the dialog showing

#[test]
fn a_dialogs_buttons_are_found_by_id_and_by_the_permission_controller() {
    let labels = |tree: &UiNode| -> Vec<String> {
        dialog_buttons(tree)
            .into_iter()
            .map(|button| button.label)
            .collect()
    };
    assert!(dialog_buttons(&app_screen()).is_empty());
    assert_eq!(labels(&discard_dialog()), vec!["Cancel", "Discard"]);
    assert_eq!(
        labels(&permission_dialog("com.android.permissioncontroller")),
        vec![
            "While using the app",
            "Only this time",
            "Don\u{2019}t allow"
        ]
    );
    // Where a tap on each lands: the centre of its bounds, in screen pixels.
    let centres: Vec<_> = dialog_buttons(&discard_dialog())
        .into_iter()
        .map(|button| button.center)
        .collect();
    assert_eq!(centres, vec![Some((290, 1310)), Some((770, 1310))]);
    // A button's label is its text, else its description; one with neither is left out.
    let mut described = node(
        "ImageButton",
        "com.example.app",
        "android:id/button1",
        "",
        true,
        [0, 0, 100, 100],
        vec![],
    );
    described.name = "Close".to_owned();
    assert_eq!(labels(&described), vec!["Close"]);
    let nameless = node(
        "Button",
        "com.example.app",
        "android:id/button1",
        "",
        true,
        [0, 0, 100, 100],
        vec![],
    );
    assert!(dialog_buttons(&nameless).is_empty());
    // Not clickable, or not the permission controller's, and with another id: not a dialog button.
    for (package, id, clickable) in [
        ("com.android.permissioncontroller", "", false),
        ("com.example.app", "", true),
        ("com.example.app", "com.example.app:id/button1", true),
        ("com.example.permissioncontroller", "", true),
    ] {
        let other = node(
            "Button",
            package,
            id,
            "Allow",
            clickable,
            [0, 0, 100, 100],
            vec![],
        );
        assert!(
            dialog_buttons(&other).is_empty(),
            "{package} {id} {clickable}"
        );
    }
    // A clickable label inside a button of the same label is that button, not a second one; one
    // of another label inside it is a button of its own.
    let wrapped = |inner: &str| {
        node(
            "LinearLayout",
            "com.android.permissioncontroller",
            "",
            "Allow",
            true,
            [90, 1000, 990, 1120],
            vec![node(
                "TextView",
                "com.android.permissioncontroller",
                "",
                inner,
                true,
                [100, 1010, 980, 1110],
                vec![],
            )],
        )
    };
    assert_eq!(labels(&wrapped("Allow")), vec!["Allow"]);
    assert_eq!(
        dialog_buttons(&wrapped("Allow"))[0].center,
        Some((540, 1060))
    );
    assert_eq!(labels(&wrapped("Allow all")), vec!["Allow", "Allow all"]);
    // A node with no area is listed, since its label is a fact, but it cannot be tapped.
    let flat = node(
        "Button",
        "com.example.app",
        "android:id/button1",
        "OK",
        true,
        [0, 0, 0, 0],
        vec![],
    );
    assert_eq!(
        dialog_buttons(&flat),
        vec![DialogButton {
            label: "OK".to_owned(),
            center: None
        }]
    );
    // The listing is capped, and says how many it left out.
    let many: Vec<DialogButton> = (0..30)
        .map(|n| DialogButton {
            label: format!("B{n}"),
            center: None,
        })
        .collect();
    let listed = label_list(&many);
    assert!(listed.starts_with("\"B0\", \"B1\""), "{listed}");
    assert!(listed.ends_with("\"B23\" and 6 more"), "{listed}");
}

#[test]
fn a_button_is_pressed_by_the_only_label_that_is_exactly_its_own() {
    let buttons = dialog_buttons(&permission_dialog(
        "com.google.android.permissioncontroller",
    ));
    // The label as the device wrote it, as an answer lists it, and as an answer lists it without
    // the mark of a change: each names the one button.
    for wanted in ["Don\u{2019}t allow", "Don't allow\u{2026}", "Don't allow"] {
        assert_eq!(
            pick_button(&buttons, wanted).unwrap().label,
            "Don\u{2019}t allow",
            "{wanted}"
        );
    }
    assert_eq!(
        pick_button(&buttons, "Only this time").unwrap().center,
        Some((540, 1190))
    );
    // Case-sensitive, whole label.
    for wanted in ["only this time", "Only this", "Only this time ", "Allow"] {
        assert_eq!(
            pick_button(&buttons, wanted).unwrap_err(),
            format!(
                "No button of the dialog is labelled \"{wanted}\". The buttons showing: \"While using the app\", \"Only this time\", \"Don't allow…\"."
            ),
            "{wanted:?}"
        );
    }
    // Two buttons of one label: none is pressed.
    let twins = dialog_buttons(&alert_dialog(&[
        ("android:id/button1", "OK", [100, 1000, 300, 1100]),
        ("android:id/button2", "Cancel", [400, 1000, 600, 1100]),
        ("android:id/button3", "OK", [700, 1000, 900, 1100]),
    ]));
    assert_eq!(
        pick_button(&twins, "OK").unwrap_err(),
        "Several buttons of the dialog are labelled \"OK\", so none was pressed. The buttons showing: \"OK\", \"Cancel\", \"OK\". Take a preview_snapshot and press the one you mean with preview_click."
    );
    assert_eq!(
        pick_button(&twins, "Cancel").unwrap().center,
        Some((500, 1050))
    );
    // A label that is exactly one button's is taken before a likeness to another's: both
    // "Open settings" and "Open settings…" can be pressed.
    let likes = dialog_buttons(&alert_dialog(&[
        (
            "android:id/button1",
            "Open settings",
            [100, 1000, 300, 1100],
        ),
        (
            "android:id/button2",
            "Open settings\u{2026}",
            [400, 1000, 600, 1100],
        ),
    ]));
    assert_eq!(
        pick_button(&likes, "Open settings").unwrap().center,
        Some((200, 1050))
    );
    assert_eq!(
        pick_button(&likes, "Open settings\u{2026}").unwrap().center,
        Some((500, 1050))
    );
    // What the device wrote cannot reshape the answer: one line, no quotes, no invisible marks.
    let hostile = dialog_buttons(&alert_dialog(&[(
        "android:id/button1",
        "A\nB \"x\" \u{202e}",
        [0, 0, 10, 10],
    )]));
    let error = pick_button(&hostile, "nope").unwrap_err();
    assert_eq!(
        error,
        "No button of the dialog is labelled \"nope\". The buttons showing: \"A…\"."
    );
}

#[test]
fn a_dialogs_button_is_pressed_at_its_centre() {
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog(), app_screen()],
    );
    assert_eq!(
        press_on(&*device, json!({"serverId": "pixel", "button": "Discard"})).unwrap(),
        "Pressed \"Discard\"."
    );
    // The centre of [600,1250][940,1370], once, and the screen was read before and after.
    assert_eq!(device.phone.inputs(), vec!["tap 770,1310 x1"]);
    assert_eq!(device.reads(), 2);
    // No adb command was needed for either.
    assert!(commands(&device.phone).is_empty());

    // The permission controller's dialog, by a label with a typographic apostrophe in it.
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![
            permission_dialog("com.android.permissioncontroller"),
            app_screen(),
        ],
    );
    assert_eq!(
        press_on(&*device, json!({"button": "Don't allow\u{2026}"})).unwrap(),
        "Pressed \"Don't allow…\"."
    );
    assert_eq!(device.phone.inputs(), vec!["tap 540,1320 x1"]);
}

#[test]
fn what_shows_next_is_reported_when_another_dialog_follows() {
    // A permission, then the next permission: other buttons.
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![
            permission_dialog("com.android.permissioncontroller"),
            discard_dialog(),
        ],
    );
    assert_eq!(
        press_on(&*device, json!({"button": "Only this time"})).unwrap(),
        "Pressed \"Only this time\".\nAnother dialog is showing; buttons: \"Cancel\", \"Discard\"."
    );
    // The same buttons again: the answer does not claim whether it is the same dialog or the next
    // one of its kind.
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog(), discard_dialog()],
    );
    assert_eq!(
        press_on(&*device, json!({"button": "Cancel"})).unwrap(),
        "Pressed \"Cancel\".\nA dialog with the same buttons is showing; buttons: \"Cancel\", \"Discard\"."
    );
    // Out of time after the tap: the press stands, and the screen is not read again. (A read is
    // not started with less left than it is expected to take, and this tap takes long enough to
    // leave less.)
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog(), discard_dialog()],
    );
    device.tap_takes(Duration::from_millis(700));
    let call = AndroidCall::dialog(&object(json!({"button": "Discard"}))).unwrap();
    let answer = call
        .run(
            &*device,
            &Step::for_tests(&|| false, UI_TREE_READ + Duration::from_millis(500)),
        )
        .unwrap();
    assert_eq!(answer, "Pressed \"Discard\".");
    assert_eq!(device.reads(), 1);
    // A screen that cannot be read afterwards leaves the answer without it; one that went away
    // is the session's to handle.
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog(), discard_dialog()],
    );
    let call = AndroidCall::dialog(&object(json!({"button": "Discard"}))).unwrap();
    device.fail_after(1, DriverError::Failed("null root node".to_owned()));
    assert_eq!(
        call.run(
            &*device,
            &Step::for_tests(&|| false, Duration::from_secs(20))
        )
        .unwrap(),
        "Pressed \"Discard\"."
    );
    device.fail_after(
        1,
        DriverError::Disconnected("Android device \"x\" is gone".to_owned()),
    );
    assert!(matches!(
        call.run(
            &*device,
            &Step::for_tests(&|| false, Duration::from_secs(20))
        ),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
}

/// The reads of a dialog — the one `screen_device info` ends with, the one a press reads its
/// buttons from, and the one it makes afterwards — are bounded by the time the call has left, and
/// none is started with less left than a dump is expected to take: a device whose dump outlasts
/// the call cannot carry it on past the call.
#[test]
fn the_reads_of_a_dialog_are_bounded_by_the_time_the_call_has_left() {
    let step = |time: Duration| Step::for_tests(&|| false, time);
    let timeouts_within = |time: Duration, timeouts: Vec<Duration>| {
        assert!(!timeouts.is_empty());
        for timeout in timeouts {
            assert!(timeout <= time - crate::remote_control::driver::UI_TREE_MARGIN, "{timeout:?} of {time:?}");
            assert!(timeout > time - crate::remote_control::driver::UI_TREE_MARGIN - Duration::from_secs(2), "{timeout:?}");
        }
    };

    // A press reads the screen twice, each time within what is left.
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog(), app_screen()],
    );
    let call = AndroidCall::dialog(&object(json!({"button": "Discard"}))).unwrap();
    let time = Duration::from_secs(12);
    assert_eq!(
        call.run(&*device, &step(time)).unwrap(),
        "Pressed \"Discard\"."
    );
    assert_eq!(device.timeouts().len(), 2);
    timeouts_within(time, device.timeouts());

    // `screen_device info` reads the dialog within what is left, too.
    let device = TreePhone::new(phone(34, |_| out(GETPROP_34)), vec![app_screen()]);
    let call = parse("screen_device", json!({"sections": ["device"]})).unwrap();
    call.run(&*device, &step(time)).unwrap();
    timeouts_within(time, device.timeouts());

    // A dump that takes longer than the call has left fails there, in the answer's own words:
    // nothing was pressed, and the part of `info` read before it stays.
    let slow = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog()],
    );
    slow.read_takes(Duration::from_secs(60));
    let call = AndroidCall::dialog(&object(json!({"button": "Discard"}))).unwrap();
    match call.run(&*slow, &step(time)) {
        Err(OpError::Driver(DriverError::Failed(message))) => {
            assert!(message.contains("did not finish reading the screen"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(slow.phone.inputs().is_empty());
    let slow = TreePhone::new(phone(34, |_| out(GETPROP_34)), vec![app_screen()]);
    slow.read_takes(Duration::from_secs(60));
    let call = parse("screen_device", json!({"sections": ["device"]})).unwrap();
    assert_eq!(
        call.run(&*slow, &step(time)).unwrap(),
        format!(
            "{}\n(Reading the dialog showing failed: uiautomator did not finish reading the screen in the time this call had left.)",
            device_block_34()
        )
    );

    // With less left than a dump takes, none is started: the press is refused, and `info` ends
    // with what it read.
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog()],
    );
    let call = AndroidCall::dialog(&object(json!({"button": "Discard"}))).unwrap();
    let short = UI_TREE_READ - Duration::from_millis(500);
    assert_eq!(
        said(call.run(&*device, &step(short))),
        "Ran out of this call's time before reading the dialog showing."
    );
    assert_eq!(device.reads(), 0);
    assert!(device.timeouts().is_empty());
    assert!(device.phone.inputs().is_empty());
    let device = TreePhone::new(phone(34, |_| out(GETPROP_34)), vec![app_screen()]);
    let call = parse("screen_device", json!({"sections": ["device"]})).unwrap();
    let answer = call.run(&*device, &step(short)).unwrap();
    assert!(
        answer.ends_with("\n(Ran out of this call's time before reading the dialog showing.)"),
        "{answer}"
    );
    assert_eq!(device.reads(), 0);
}

#[test]
fn a_dialog_that_is_not_there_or_not_named_is_refused_without_a_press() {
    // None showing.
    let device = TreePhone::new(phone(34, |command| panic!("{command}")), vec![app_screen()]);
    assert_eq!(
        said(press_on(&*device, json!({"button": "OK"}))),
        "No dialog is showing on \"Fake Pixel\"."
    );
    // None of the labels.
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog()],
    );
    assert_eq!(
        said(press_on(&*device, json!({"button": "Save"}))),
        "No button of the dialog is labelled \"Save\". The buttons showing: \"Cancel\", \"Discard\"."
    );
    // Case-sensitive.
    assert_eq!(
        said(press_on(&*device, json!({"button": "discard"}))),
        "No button of the dialog is labelled \"discard\". The buttons showing: \"Cancel\", \"Discard\"."
    );
    // Several of one label.
    let twins = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![alert_dialog(&[
            ("android:id/button1", "OK", [100, 1000, 300, 1100]),
            ("android:id/button3", "OK", [700, 1000, 900, 1100]),
        ])],
    );
    assert!(said(press_on(&*twins, json!({"button": "OK"})))
        .starts_with("Several buttons of the dialog are labelled \"OK\", so none was pressed."));
    // A button with no area cannot be tapped.
    let flat = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![alert_dialog(&[(
            "android:id/button1",
            "OK",
            [100, 1000, 100, 1100],
        )])],
    );
    assert_eq!(
        said(press_on(&*flat, json!({"button": "OK"}))),
        "The button \"OK\" has no area on the screen, so it cannot be pressed."
    );
    // A driver without an element tree.
    assert!(said(press_on(
        &*phone(34, |command| panic!("{command}")),
        json!({"button": "OK"})
    ))
    .starts_with("This screen has no element tree"));
    // Nothing was tapped by any of them.
    for driver in [&device, &twins, &flat] {
        assert!(driver.phone.inputs().is_empty());
    }
    // A screen that is not an Android device.
    let desktop = FakeDriver::new(crate::remote_control::driver::Platform::Desktop, 10, 10);
    assert_eq!(
        said(press_on(&*desktop, json!({"button": "OK"}))),
        NOT_ANDROID
    );
}

/// A call stopped after it read the dialog never taps: the press is a step of its own.
#[test]
fn a_press_is_not_started_once_the_call_was_stopped() {
    let device = TreePhone::new(
        phone(34, |command| panic!("{command}")),
        vec![discard_dialog()],
    );
    let call = AndroidCall::dialog(&object(json!({"button": "Discard"}))).unwrap();
    let read = || device.reads() >= 1;
    assert_eq!(
        said(call.run(&*device, &Step::for_tests(&read, Duration::from_secs(20)))),
        "Cancelled before pressing \"Discard\"."
    );
    assert!(device.phone.inputs().is_empty());
    // Out of time before the screen is read: nothing is read.
    let call = AndroidCall::dialog(&object(json!({"button": "Discard"}))).unwrap();
    assert_eq!(
        said(call.run(&*device, &Step::for_tests(&|| false, Duration::ZERO))),
        "Ran out of this call's time before reading the dialog showing."
    );
    assert_eq!(device.reads(), 1);
}

#[test]
fn an_android_dialog_is_answered_by_its_button_and_nothing_else() {
    let refused = |input: Value| AndroidCall::dialog(&object(input)).unwrap_err();
    // `accept` guesses a button by position, which is not how an Android dialog is answered, and
    // `false` is no more welcome than `true`.
    for accept in [true, false] {
        assert_eq!(
            refused(json!({"accept": accept})),
            "An Android dialog is answered by its button's label: pass button; screen_device info lists the buttons."
        );
        assert_eq!(
            refused(json!({"button": "OK", "accept": accept})),
            ACCEPT_NOT_HERE
        );
    }
    // A text field is filled with preview_fill.
    for text in ["", "hello"] {
        let error = refused(json!({"prompt_text": text}));
        assert_eq!(error, PROMPT_TEXT_NOT_HERE);
        assert!(error.contains("preview_fill"), "{error}");
        assert_eq!(
            refused(json!({"button": "OK", "prompt_text": text})),
            PROMPT_TEXT_NOT_HERE
        );
    }
    // A null is no value.
    assert!(AndroidCall::dialog(&object(
        json!({"button": "OK", "accept": null, "prompt_text": null})
    ))
    .is_ok());
    // The button is a string of 1 to 256 characters, and must be there.
    assert_eq!(
        refused(json!({})),
        "preview_dialog needs \"button\": the exact label of the button to press; screen_device info lists the buttons."
    );
    assert_eq!(refused(json!({"button": ""})), text_bound("button", 256));
    assert_eq!(
        refused(json!({"button": "x".repeat(257)})),
        text_bound("button", 256)
    );
    assert_eq!(refused(json!({"button": 5})), text_bound("button", 256));
    assert!(AndroidCall::dialog(&object(json!({"button": "x".repeat(256)}))).is_ok());
}

#[test]
fn a_shell_command_answers_with_its_status_and_output() {
    let device = phone(34, |command| match command {
        "ls /sdcard; echo done" => ShellResult {
            stdout: b"Download\nPictures\ndone\n".to_vec(),
            stderr: b"ls: /sdcard/x: Permission denied\n".to_vec(),
            exit: Some(1),
            truncated: false,
            timed_out: false,
        },
        "yes" => out(&"y\n".repeat(40_000)),
        other => panic!("{other}"),
    });
    assert_eq!(
        ran(
            &device,
            "android_shell",
            json!({"command": "ls /sdcard; echo done"})
        ),
        "exit 1\nstdout:\nDownload\nPictures\ndone\nstderr:\nls: /sdcard/x: Permission denied"
    );
    let cut = ran(
        &device,
        "android_shell",
        json!({"command": "yes", "timeout": 1000}),
    );
    assert!(
        cut.ends_with("\n(cut at 32 KiB)"),
        "{}",
        &cut[cut.len() - 40..]
    );
    let calls = device.android_fake().shell_calls();
    assert_eq!(calls[0].limit, SHELL_KEEP);
    assert_eq!(
        calls[0].timeout,
        Duration::from_millis(SHELL_TIMEOUT_DEFAULT_MS)
    );
    assert_eq!(calls[1].timeout, Duration::from_millis(1000));
    // A command that does not end within its timeout is stopped, and said to be.
    let hung = FakeDriver::with_android(FakeAndroid::scripted(
        34,
        |_| -> DriverResult<ShellResult> {
            thread::sleep(Duration::from_millis(1100));
            Err(DriverError::Failed(
                "the command did not end within 1000 ms".to_owned(),
            ))
        },
    ));
    assert_eq!(
        said(run_on(
            &hung,
            "android_shell",
            json!({"command": "logcat", "timeout": 1000})
        )),
        "Stopped after 1000 ms: the command did not end."
    );
    // A device that went away is the session's to handle, not a stopped command.
    let gone = FakeDriver::with_android(FakeAndroid::scripted(34, |_| {
        Err(DriverError::Disconnected(
            "Android device \"x\" is gone".to_owned(),
        ))
    }));
    assert!(matches!(
        run_on(&gone, "android_shell", json!({"command": "ls"})),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
}

/// Android's binary XML for `<manifest package="…">`: a string pool (UTF-8 or UTF-16) and the
/// element with its one attribute, as aapt2 writes them.
fn binary_manifest(package: &str, utf8: bool) -> Vec<u8> {
    let strings = ["manifest", "package", package];
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    for text in strings {
        offsets.push(data.len() as u32);
        if utf8 {
            data.push(text.chars().count() as u8);
            data.push(text.len() as u8);
            data.extend_from_slice(text.as_bytes());
            data.push(0);
        } else {
            let units: Vec<u16> = text.encode_utf16().collect();
            data.extend((units.len() as u16).to_le_bytes());
            for unit in units {
                data.extend(unit.to_le_bytes());
            }
            data.extend([0, 0]);
        }
    }
    while data.len() % 4 != 0 {
        data.push(0);
    }
    let header = 28u16;
    let strings_start = u32::from(header) + 4 * strings.len() as u32;
    let mut pool = Vec::new();
    pool.extend(0x0001u16.to_le_bytes());
    pool.extend(header.to_le_bytes());
    pool.extend((strings_start + data.len() as u32).to_le_bytes());
    pool.extend((strings.len() as u32).to_le_bytes());
    pool.extend(0u32.to_le_bytes());
    pool.extend((if utf8 { 1u32 << 8 } else { 0 }).to_le_bytes());
    pool.extend(strings_start.to_le_bytes());
    pool.extend(0u32.to_le_bytes());
    for offset in offsets {
        pool.extend(offset.to_le_bytes());
    }
    pool.extend(data);
    // A namespace first, which a reader passes over.
    let mut namespace = Vec::new();
    namespace.extend(0x0100u16.to_le_bytes());
    namespace.extend(16u16.to_le_bytes());
    namespace.extend(24u32.to_le_bytes());
    namespace.extend([0u8; 16]);
    let mut element = Vec::new();
    element.extend(0x0102u16.to_le_bytes());
    element.extend(16u16.to_le_bytes());
    element.extend(56u32.to_le_bytes());
    element.extend(1u32.to_le_bytes());
    element.extend(u32::MAX.to_le_bytes());
    element.extend(u32::MAX.to_le_bytes());
    element.extend(0u32.to_le_bytes());
    element.extend(20u16.to_le_bytes());
    element.extend(20u16.to_le_bytes());
    element.extend(1u16.to_le_bytes());
    element.extend([0u8; 6]);
    element.extend(u32::MAX.to_le_bytes());
    element.extend(1u32.to_le_bytes());
    element.extend(2u32.to_le_bytes());
    element.extend(8u16.to_le_bytes());
    element.extend([0u8, 0x03]);
    element.extend(2u32.to_le_bytes());
    let mut xml = Vec::new();
    xml.extend(0x0003u16.to_le_bytes());
    xml.extend(8u16.to_le_bytes());
    xml.extend(((8 + pool.len() + namespace.len() + element.len()) as u32).to_le_bytes());
    xml.extend(pool);
    xml.extend(namespace);
    xml.extend(element);
    xml
}

/// A ZIP archive of `entries` (name, bytes, deflated or stored).
fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, deflate) in entries {
        let packed = if *deflate {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let offset = out.len() as u32;
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(method.to_le_bytes());
        out.extend([0u8; 8]);
        out.extend((packed.len() as u32).to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(&packed);
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(method.to_le_bytes());
        central.extend([0u8; 8]);
        central.extend((packed.len() as u32).to_le_bytes());
        central.extend((data.len() as u32).to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend([0u8; 12]);
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let central_at = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(central_at.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

/// An APK whose manifest names `package`.
fn apk_of(package: &str) -> Vec<u8> {
    let dex = vec![0x64u8; 70_000];
    zip(&[
        ("classes.dex", &dex, false),
        (
            "AndroidManifest.xml",
            &binary_manifest(package, false),
            true,
        ),
        ("resources.arsc", b"arsc", false),
    ])
}

#[test]
fn an_apks_package_is_read_from_its_manifest() {
    let read = |bytes: &[u8]| {
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        apk::package_name(&mut cursor, bytes.len() as u64)
    };
    assert_eq!(
        read(&apk_of("com.example.app")),
        Some("com.example.app".into())
    );
    for utf8 in [true, false] {
        for deflate in [true, false] {
            let manifest = binary_manifest("org.ünï.côde", utf8);
            let archive = zip(&[("AndroidManifest.xml", &manifest, deflate)]);
            assert_eq!(
                read(&archive),
                Some("org.ünï.côde".into()),
                "{utf8} {deflate}"
            );
        }
    }
    // Not an APK, an archive without a manifest, a manifest that is not binary XML.
    assert_eq!(read(b"PK\x03\x04 an apk"), None);
    assert_eq!(read(&zip(&[("classes.dex", b"dex", false)])), None);
    assert_eq!(
        read(&zip(&[(
            "AndroidManifest.xml",
            b"<manifest package=\"a.b\"/>",
            false
        )])),
        None
    );
    let mut torn = binary_manifest("com.example.app", true);
    torn.truncate(70);
    assert_eq!(read(&zip(&[("AndroidManifest.xml", &torn, false)])), None);
}

/// An install that must not replace an installed app is refused before the APK is sent when the
/// app is there; otherwise it streams, with `-R` on Android 9 and later (where pm replaces by
/// default) and without `-r` before.
#[test]
fn an_install_that_must_not_replace_asks_first() {
    let files = Files::new();
    let apk = apk_of("com.example.app");
    files.write("app-debug.apk", &apk);
    files.write("odd.apk", b"PK\x03\x04 not really");
    let device = |sdk: u32, installed: bool| {
        FakeDriver::with_android(FakeAndroid::new(sdk, move |command| match command {
            "pm path 'com.example.app'" if installed => out(INSTALLED),
            "pm path 'com.example.app'" => FakeAndroid::failing("", 1),
            other => panic!("{other}"),
        }))
    };
    let install = |device: &FakeDriver, path: &str| {
        let call = parse_in(
            &files,
            "screen_package",
            json!({"action": "install", "path": path, "replace_existing": false}),
        )
        .unwrap();
        call.run(device, &Step::for_tests(&|| false, Duration::from_secs(60)))
    };
    for sdk in [27, 28, 34] {
        let there = device(sdk, true);
        assert_eq!(
            said(install(&there, "app-debug.apk")),
            "com.example.app is installed already and replace_existing is false, so app-debug.apk was not sent. Leave replace_existing at true to replace it, or uninstall com.example.app first.",
            "{sdk}"
        );
        assert!(there.android_fake().installs().is_empty(), "{sdk}");
    }
    let fresh = device(28, false);
    assert_eq!(
        install(&fresh, "app-debug.apk").unwrap(),
        format!(
            "Installed app-debug.apk ({}) on Fake Pixel.",
            human_size(apk.len() as u64)
        )
    );
    let installs = fresh.android_fake().installs();
    assert_eq!(installs[0].flags, vec!["-R", "-t"]);
    // The manifest was read first; the stream still starts at the APK's first byte.
    assert_eq!(installs[0].bytes, apk);
    let old = device(27, false);
    install(&old, "app-debug.apk").unwrap();
    assert_eq!(old.android_fake().installs()[0].flags, vec!["-t"]);
    // A package this side cannot read is pm's to refuse.
    let unread = device(34, true);
    install(&unread, "odd.apk").unwrap();
    assert!(unread.android_fake().commands().is_empty());
    assert_eq!(unread.android_fake().installs()[0].flags, vec!["-R", "-t"]);
}

#[test]
fn a_later_step_that_fails_keeps_what_was_done_and_read() {
    // The force-stop happened; the launcher could not be found in time.
    let device = FakeDriver::with_android(FakeAndroid::scripted(34, |command| match command {
        "pm path 'com.example.app'" => Ok(out(INSTALLED)),
        "am force-stop 'com.example.app'" => Ok(out("")),
        c if c.starts_with("cmd package resolve-activity") => Err(DriverError::Failed(
            "the command did not end within 15000 ms".to_owned(),
        )),
        other => panic!("{other}"),
    }));
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app", "restart": true})
        )),
        "Force-stopped com.example.app. Then finding com.example.app's launcher activity failed: the command did not end within 15000 ms."
    );
    // A device that went away is still the session's to handle.
    let gone = FakeDriver::with_android(FakeAndroid::scripted(34, |command| match command {
        "pm path 'com.example.app'" => Ok(out(INSTALLED)),
        "am force-stop 'com.example.app'" => Ok(out("")),
        _ => Err(DriverError::Disconnected(
            "Android device \"x\" is gone".to_owned(),
        )),
    }));
    assert!(matches!(
        run_on(
            &gone,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app", "restart": true})
        ),
        Err(OpError::Driver(DriverError::Disconnected(_)))
    ));
    // The sections read before one that failed are still the answer.
    let device = FakeDriver::with_android(FakeAndroid::scripted(34, |command| match command {
        "getprop" => Ok(out(GETPROP_34)),
        "dumpsys notification --noredact" => Err(DriverError::Failed(
            "the command did not end within 20000 ms".to_owned(),
        )),
        other => panic!("{other}"),
    }));
    assert_eq!(
        ran(
            &device,
            "screen_device",
            json!({"sections": ["device", "notifications"]})
        ),
        "device:\n  maker: Google\n  model: Pixel 8\n  Android: 14 (SDK 34)\n  security patch: 2026-09-05\n  ABI: arm64-v8a\n  locale: en-US\n  emulator: no\n(Reading the notifications failed: the command did not end within 20000 ms.)"
    );
    // A read that never ended, cut nowhere, is no answer.
    let hung = phone(34, |_| ShellResult {
        stdout: b"[ro.product.model]: [Pix".to_vec(),
        timed_out: true,
        ..ShellResult::default()
    });
    match run_on(&hung, "screen_device", json!({"sections": ["device"]})) {
        Err(OpError::Driver(DriverError::Failed(message))) => {
            assert!(
                message.starts_with("the command did not end within "),
                "{message}"
            )
        }
        other => panic!("{other:?}"),
    }
}

/// What a service without a shell command answers (Binder's default): a line, and exit 0.
fn no_shell_command() -> ShellResult {
    ShellResult {
        stderr: b"No shell command implementation.\n".to_vec(),
        exit: Some(0),
        ..ShellResult::default()
    }
}

#[test]
fn a_service_without_a_shell_command_is_a_failure() {
    // Android 7 and 8.0: no statusbar shell command, no uimode one.
    let device = phone(25, |command| match command {
        c if c.starts_with("cmd statusbar") || c.starts_with("cmd uimode") => no_shell_command(),
        other => panic!("{other}"),
    });
    assert_eq!(
        said(run_on(
            &device,
            "screen_device",
            json!({"action": "notifications"})
        )),
        "Opening the notification shade failed: No shell command implementation."
    );
    assert_eq!(
        said(run_on(
            &device,
            "screen_device",
            json!({"action": "appearance", "value": "dark"})
        )),
        "Switching appearance failed: No shell command implementation."
    );
}

#[test]
fn a_launch_error_names_the_devices_component_safely() {
    let device = phone(34, |command| {
        match command {
        "pm path 'com.example.app'" => out(INSTALLED),
        c if c.starts_with("cmd package resolve-activity") => {
            out("com.example.app/.Ma\u{202e}in\"x\n")
        }
        c if c.starts_with("am start") => out(&format!(
            "{STARTING}Error: Activity class {{com.example.app/com.example.app.Main}} does not exist.\n"
        )),
        other => panic!("{other}"),
    }
    });
    assert_eq!(
        said(run_on(
            &device,
            "screen_app",
            json!({"action": "launch", "app": "com.example.app"})
        )),
        "Could not launch com.example.app/.Ma\u{fffd}in'x…: Error: Activity class {com.example.app/com.example.app.Main} does not exist."
    );
}

#[test]
fn a_shell_command_that_does_not_end_answers_with_what_it_printed() {
    let device = FakeDriver::with_android(FakeAndroid::scripted(34, |command| {
        thread::sleep(Duration::from_millis(1000));
        Ok(match command {
            "top" => ShellResult {
                stdout: b"Tasks: 512 total".to_vec(),
                timed_out: true,
                ..ShellResult::default()
            },
            _ => ShellResult {
                timed_out: true,
                ..ShellResult::default()
            },
        })
    }));
    assert_eq!(
        said(run_on(
            &device,
            "android_shell",
            json!({"command": "top", "timeout": 1000})
        )),
        "Stopped after 1000 ms: the command did not end. Output so far:\nstdout:\nTasks: 512 total"
    );
    assert_eq!(
        said(run_on(
            &device,
            "android_shell",
            json!({"command": "sleep 9", "timeout": 1000})
        )),
        "Stopped after 1000 ms: the command did not end."
    );
}

// ------------------------------------------------------------- through the tool layer

const CONVERSATION: &str = "conversation-android";

/// A workspace whose launch.json has one adb screen, connected to `driver` as the model's
/// `preview_start` would.
struct Bench {
    workspace: tempfile::TempDir,
    app_data: tempfile::TempDir,
    state: AppState,
}

impl Bench {
    fn new(driver: Arc<FakeDriver>) -> Self {
        Self::connected(fake::ready(driver))
    }

    /// Every connect of the screen answers with `driver`, which need not be a [`FakeDriver`].
    fn serving(driver: Arc<dyn Driver>) -> Self {
        let connect: ConnectFn =
            Arc::new(move |_target, _connector, _auth, _events| Ok(driver.clone()));
        Self::connected(connect)
    }

    fn connected(connect: ConnectFn) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let mewrk = workspace.path().join(".mewrk");
        std::fs::create_dir_all(&mewrk).unwrap();
        std::fs::write(
            mewrk.join("launch.json"),
            r#"{"configurations":[{"name":"pixel","type":"remote-control","protocol":"adb"}]}"#,
        )
        .unwrap();
        let bench = Self {
            workspace,
            app_data: tempfile::tempdir().unwrap(),
            state: AppState::default(),
        };
        let environment = ControlEnvironment {
            launch_port_wait: Duration::ZERO,
            ..ControlEnvironment::this_computer()
        }
        .with_connect(connect);
        crate::preview::start(
            &bench.state.preview_servers,
            bench.workspace.path(),
            Some("pixel"),
            Some(CONVERSATION),
            &environment,
        )
        .unwrap();
        bench
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn call_in(
        &self,
        workspaces: &WorkspaceSet,
        tool: &str,
        input: Value,
    ) -> crate::model::ToolExecutionResponse {
        crate::tool_executor::execute_with_scope_and_attachments_verified(
            ToolExecutionRequest {
                conversation_id: CONVERSATION.into(),
                workspace_path: self.path().to_string_lossy().into_owned(),
                tool_name: tool.into(),
                input: object(input),
            },
            &self.state,
            ExecutionScope::workspace_only(self.path()),
            Some(self.app_data.path()),
            &CancelSignal::default(),
            workspaces,
            None,
            &PromptProfile::builtin(),
        )
        .result
    }

    fn call(&self, tool: &str, input: Value) -> crate::model::ToolExecutionResponse {
        let local = WorkspaceSet::local_root(self.path().to_string_lossy().into_owned());
        self.call_in(&local, tool, input)
    }

    fn viewer_url(&self) -> String {
        self.state
            .preview_servers
            .controls()
            .into_iter()
            .find(|control| control.server.server_id == "pixel")
            .expect("the screen's row")
            .session
            .viewer_url()
    }
}

/// `screen_app` and `screen_package install` end to end: parsed by the tool layer, resolved to
/// the one phone, run through its session on its driver; the answers carry no viewer address.
#[test]
fn the_android_tools_run_through_the_tool_layer() {
    let driver = phone(34, |command| match command {
        LAUNCHERS_COMMAND => out(LAUNCHERS_27),
        other => panic!("{other}"),
    });
    let bench = Bench::new(driver.clone());
    let viewer = bench.viewer_url();
    let listed = bench.call("screen_app", json!({}));
    assert!(listed.success, "{}", listed.output);
    assert_eq!(
        listed.output,
        "Launchable apps on Fake Pixel: 1 (showing 1).\ncom.example.app  com.example.app/.MainActivity"
    );
    let named = bench.call(
        "screen_app",
        json!({"serverId": "pixel", "action": "list", "limit": 5}),
    );
    assert!(named.success, "{}", named.output);

    let apk: Vec<u8> = (0..70_000u32).map(|n| (n % 13) as u8).collect();
    std::fs::write(bench.path().join("app-release.apk"), &apk).unwrap();
    let installed = bench.call(
        "screen_package",
        json!({"action": "install", "path": "app-release.apk", "grant_permissions": true}),
    );
    assert!(installed.success, "{}", installed.output);
    assert_eq!(
        installed.output,
        "Installed app-release.apk (70.0 KB) on Fake Pixel."
    );
    let installs = driver.android_fake().installs();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0].bytes, apk);
    assert_eq!(installs[0].flags, vec!["-r", "-t", "-g"]);
    // The install got its own five minutes, not a screen call's 27 seconds.
    assert!(
        installs[0].timeout > Duration::from_secs(60),
        "{:?}",
        installs[0].timeout
    );

    let refused = bench.call(
        "screen_package",
        json!({"action": "install", "path": "../elsewhere.apk"}),
    );
    assert!(!refused.success);
    let unknown = bench.call("screen_app", json!({"colour": 1}));
    assert_eq!(
        unknown.output,
        "screen_app has no parameter \"colour\"; it takes serverId, action, app, url, filter, limit, scope, activity, restart, arguments and environment."
    );
    // The platform's own refusals reach the model as they are.
    let iphone = bench.call(
        "screen_app",
        json!({"action": "launch", "app": "com.example.app", "arguments": ["-x"]}),
    );
    assert!(!iphone.success);
    assert_eq!(iphone.output, ARGUMENTS_NOT_HERE);
    let location = bench.call(
        "screen_device",
        json!({"action": "location", "latitude": 1, "longitude": 2}),
    );
    assert!(!location.success);
    assert_eq!(location.output, LOCATION_NOT_HERE);
    for text in [
        &listed.output,
        &installed.output,
        &refused.output,
        &unknown.output,
        &iphone.output,
        &location.output,
    ] {
        assert!(!text.contains(&viewer), "{text}");
    }
    assert!(driver.inputs().is_empty());
}

/// `screen_device info` and `preview_dialog` end to end: the dialog showing is read from the
/// phone's element tree through the session, and a button is pressed with a tap.
#[test]
fn the_dialog_and_the_device_run_through_the_tool_layer() {
    let device = TreePhone::new(
        phone(34, |command| match command {
            "getprop" => out(GETPROP_34),
            other => panic!("{other}"),
        }),
        vec![
            permission_dialog("com.google.android.permissioncontroller"),
            permission_dialog("com.google.android.permissioncontroller"),
            app_screen(),
        ],
    );
    let bench = Bench::serving(device.clone());
    let viewer = bench.viewer_url();
    let info = bench.call("screen_device", json!({"sections": ["device"]}));
    assert!(info.success, "{}", info.output);
    assert_eq!(
        info.output,
        format!(
            "{}\ndialog: showing; buttons: \"While using the app\", \"Only this time\", \"Don't allow…\"",
            device_block_34()
        )
    );
    // The accept of a page's dialog is no way to answer an Android one.
    let guessed = bench.call("preview_dialog", json!({"accept": true}));
    assert!(!guessed.success);
    assert_eq!(guessed.output, ACCEPT_NOT_HERE);
    let missing = bench.call("preview_dialog", json!({}));
    assert!(!missing.success);
    let pressed = bench.call(
        "preview_dialog",
        json!({"serverId": "pixel", "button": "Only this time"}),
    );
    assert!(pressed.success, "{}", pressed.output);
    assert_eq!(pressed.output, "Pressed \"Only this time\".");
    assert_eq!(device.phone.inputs(), vec!["tap 540,1190 x1"]);
    let gone = bench.call("preview_dialog", json!({"button": "Only this time"}));
    assert!(!gone.success);
    assert_eq!(gone.output, "No dialog is showing on \"Fake Pixel\".");
    for text in [&info.output, &guessed.output, &pressed.output, &gone.output] {
        assert!(!text.contains(&viewer), "{text}");
    }
}

/// A workspace on an SSH machine has its APK there; the install is refused before anything is
/// read or sent, and says what to do.
#[test]
fn an_install_from_a_workspace_on_another_machine_is_refused_through_the_tool_layer() {
    let driver = phone(34, |command| panic!("{command}"));
    let bench = Bench::new(driver.clone());
    let remote = WorkspaceSet::single(
        "/home/me/app",
        ShellRunner::Ssh {
            host: "203.0.113.7".to_owned(),
            port: 22,
            identity_file: String::new(),
            env: Default::default(),
            agent_shell: Default::default(),
        },
    );
    let refused = bench.call_in(
        &remote,
        "screen_package",
        json!({"action": "install", "path": "app/build/outputs/apk/debug/app-debug.apk"}),
    );
    assert!(!refused.success);
    assert_eq!(
        refused.output,
        "screen_package install reads the APK from this computer; this workspace is on another machine. Copy the APK to this computer, or run `adb install` in that machine's shell."
    );
    assert!(driver.android_fake().installs().is_empty());
}
