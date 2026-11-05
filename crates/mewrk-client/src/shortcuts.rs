//! Shortcuts (lib/shortcuts.ts): the command catalog and everything about key bindings.
//!
//! Commands and their defaults are code constants; documents store only user overrides in `GlobalSettings.shortcuts`,
//! so a new command needs no schema change and resetting deletes the override.
//!
//! A binding is a list of tokens: the modifiers `Control`, `Alt`, `Shift` and `Meta`, and `KeyboardEvent.code` names for
//! everything else. Functions take any `&[impl AsRef<str>]`, so the catalog's `&[&str]` and a stored `Vec<String>` both fit.

use std::collections::{BTreeMap, HashSet};

use fluent::ui::{Key, KeyPress};

use crate::model::ShortcutPreference;
use crate::t;

/// The settings page's shortcut groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShortcutGroup {
    General,
    Conversation,
    Message,
    Panel,
}

impl ShortcutGroup {
    /// The group's name in the settings page (`groupLabels` of `ShortcutSettings`).
    pub fn label(self) -> &'static str {
        match self {
            Self::General => t("通用", "General"),
            Self::Conversation => t("对话", "Conversation"),
            Self::Message => t("消息", "Messages"),
            Self::Panel => t("面板", "Panels"),
        }
    }
}

/// One command the user can bind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutCommand {
    /// The key in `GlobalSettings.shortcuts`, and how the app dispatches it.
    pub id: &'static str,
    pub group: ShortcutGroup,
    /// Factory binding, which may name [`PRIMARY_MODIFIER`]. Empty is initially unbound and needs user input.
    pub default_binding: &'static [&'static str],
    pub default_enabled: bool,
    /// False when the application owns this binding: zoom uses the platform's own keys and can't be re-recorded.
    pub editable: bool,
    /// What the settings page calls it (`commandLabels` of `ShortcutSettings`).
    pub zh: &'static str,
    pub en: &'static str,
}

impl ShortcutCommand {
    /// The command's name in the current language.
    pub fn label(&self) -> &'static str {
        t(self.zh, self.en)
    }
}

const SHORTCUT_MODIFIERS: [&str; 4] = ["Control", "Alt", "Shift", "Meta"];

/// The platform's command modifier in a default binding: ⌘ (`Meta`) on Apple platforms, `Control` everywhere else.
/// Only code defaults use it; a binding the user records, and everything stored, names real keys ([`concrete_binding`]).
pub const PRIMARY_MODIFIER: &str = "Mod";

/// The catalog, in the order the settings page lists it.
pub const SHORTCUT_COMMANDS: &[ShortcutCommand] = &[
    ShortcutCommand { id: "app.settings.open", group: ShortcutGroup::General, default_binding: &[PRIMARY_MODIFIER, "Comma"], default_enabled: true, editable: true, zh: "打开设置", en: "Open settings" },
    ShortcutCommand { id: "app.conversation_settings.open", group: ShortcutGroup::General, default_binding: &[PRIMARY_MODIFIER, "Shift", "Comma"], default_enabled: true, editable: true, zh: "打开本对话设置", en: "Open conversation settings" },
    ShortcutCommand { id: "app.zoom.in", group: ShortcutGroup::General, default_binding: &[PRIMARY_MODIFIER, "Equal"], default_enabled: true, editable: false, zh: "放大", en: "Zoom in" },
    ShortcutCommand { id: "app.zoom.out", group: ShortcutGroup::General, default_binding: &[PRIMARY_MODIFIER, "Minus"], default_enabled: true, editable: false, zh: "缩小", en: "Zoom out" },
    ShortcutCommand { id: "app.zoom.reset", group: ShortcutGroup::General, default_binding: &[PRIMARY_MODIFIER, "Digit0"], default_enabled: true, editable: false, zh: "重置缩放", en: "Reset zoom" },
    ShortcutCommand { id: "conversation.create", group: ShortcutGroup::Conversation, default_binding: &[PRIMARY_MODIFIER, "KeyN"], default_enabled: true, editable: true, zh: "新建对话", en: "New conversation" },
    // Switching conversations is Control+Tab on every platform, as switching tabs is.
    ShortcutCommand { id: "conversation.next", group: ShortcutGroup::Conversation, default_binding: &["Control", "Tab"], default_enabled: true, editable: true, zh: "下一个对话", en: "Next conversation" },
    ShortcutCommand { id: "conversation.previous", group: ShortcutGroup::Conversation, default_binding: &["Control", "Shift", "Tab"], default_enabled: true, editable: true, zh: "上一个对话", en: "Previous conversation" },
    // Unbound by default: stopping already has a persistent button, and a default key could conflict with typing.
    ShortcutCommand { id: "conversation.stop", group: ShortcutGroup::Conversation, default_binding: &[], default_enabled: false, editable: true, zh: "停止当前运行", en: "Stop current run" },
    ShortcutCommand { id: "message.copy_last", group: ShortcutGroup::Message, default_binding: &[PRIMARY_MODIFIER, "Shift", "KeyC"], default_enabled: false, editable: true, zh: "复制最后一条消息", en: "Copy last message" },
    ShortcutCommand { id: "message.edit_last_user", group: ShortcutGroup::Message, default_binding: &[PRIMARY_MODIFIER, "Shift", "KeyE"], default_enabled: false, editable: true, zh: "编辑最后一条用户消息", en: "Edit last user message" },
    ShortcutCommand { id: "panel.browser.toggle", group: ShortcutGroup::Panel, default_binding: &[PRIMARY_MODIFIER, "KeyB"], default_enabled: true, editable: true, zh: "打开或关闭内置浏览器", en: "Toggle built-in browser" },
    ShortcutCommand { id: "panel.close", group: ShortcutGroup::Panel, default_binding: &[PRIMARY_MODIFIER, "Shift", "KeyW"], default_enabled: false, editable: true, zh: "关闭侧边面板", en: "Close side panel" },
];

/// The order the settings page shows the groups in.
pub const SHORTCUT_GROUP_ORDER: [ShortcutGroup; 4] = [ShortcutGroup::General, ShortcutGroup::Conversation, ShortcutGroup::Message, ShortcutGroup::Panel];

/// The catalog entry for an id, as a stored key or a dispatch names it.
pub fn shortcut_command(id: &str) -> Option<&'static ShortcutCommand> {
    SHORTCUT_COMMANDS.iter().find(|command| command.id == id)
}

/// What upstream reads from a `KeyboardEvent`. A view fills it with [`KeyInput::from_key_press`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyInput {
    /// `KeyboardEvent.code`: `KeyN`, `Enter`, `ControlLeft`. Empty when the key has no name.
    pub code: String,
    /// `KeyboardEvent.key`: what the key means, `n`, `Enter`, `Process` for one the input method took.
    pub key: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub is_composing: bool,
    /// `KeyboardEvent.keyCode`; 229 is one the input method took.
    pub key_code: u32,
}

impl KeyInput {
    /// Adapts Fluent's key press.
    ///
    /// `KeyPress` says what a key means, not where it is, so `code` is the name that character has on a US layout, and
    /// a character a US layout doesn't type has none and matches nothing. Upstream's `code` didn't depend on the layout.
    /// Composition reaches Fluent as `Input::ime_preedit`, never as a key press, so nothing here is an input method's.
    pub fn from_key_press(press: KeyPress) -> Self {
        let (code, key) = match press.key {
            Key::Enter => named("Enter"),
            Key::Backspace => named("Backspace"),
            Key::Delete => named("Delete"),
            Key::Left => named("ArrowLeft"),
            Key::Right => named("ArrowRight"),
            Key::Up => named("ArrowUp"),
            Key::Down => named("ArrowDown"),
            Key::Home => named("Home"),
            Key::End => named("End"),
            Key::Escape => named("Escape"),
            Key::Tab => named("Tab"),
            Key::PageUp => named("PageUp"),
            Key::PageDown => named("PageDown"),
            Key::F(number) => {
                let name = format!("F{number}");
                (name.clone(), name)
            }
            Key::Char(character) => (code_of_character(character), character.to_string()),
        };
        // `cmd` is Command on macOS and Control elsewhere, where the Windows key isn't reported at all.
        let apple = cfg!(target_os = "macos");
        Self {
            code,
            key,
            ctrl: press.ctrl || (press.cmd && !apple),
            alt: press.alt,
            shift: press.shift,
            meta: press.cmd && apple,
            is_composing: false,
            key_code: 0,
        }
    }
}

fn named(name: &str) -> (String, String) {
    (name.to_owned(), name.to_owned())
}

/// The `KeyboardEvent.code` of the key that types `character` on a US layout, shifted or not.
fn code_of_character(character: char) -> String {
    let code = match character {
        'a'..='z' | 'A'..='Z' => return format!("Key{}", character.to_ascii_uppercase()),
        '0'..='9' => return format!("Digit{character}"),
        '!' => "Digit1",
        '@' => "Digit2",
        '#' => "Digit3",
        '$' => "Digit4",
        '%' => "Digit5",
        '^' => "Digit6",
        '&' => "Digit7",
        '*' => "Digit8",
        '(' => "Digit9",
        ')' => "Digit0",
        ',' | '<' => "Comma",
        '.' | '>' => "Period",
        '/' | '?' => "Slash",
        '\\' | '|' => "Backslash",
        ';' | ':' => "Semicolon",
        '\'' | '"' => "Quote",
        '`' | '~' => "Backquote",
        '-' | '_' => "Minus",
        '=' | '+' => "Equal",
        '[' | '{' => "BracketLeft",
        ']' | '}' => "BracketRight",
        ' ' => "Space",
        _ => "",
    };
    code.to_owned()
}

#[cfg(test)]
thread_local! {
    // What `vi.spyOn(navigator, "platform")` was for upstream's tests.
    static TEST_PLATFORM: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

/// What `navigator.platform` reads on this machine.
fn navigator_platform() -> &'static str {
    #[cfg(test)]
    if let Some(platform) = TEST_PLATFORM.with(std::cell::Cell::get) {
        return platform;
    }
    if cfg!(target_os = "macos") {
        "MacIntel"
    } else if cfg!(windows) {
        "Win32"
    } else {
        "Linux x86_64"
    }
}

fn is_modifier_token(token: &str) -> bool {
    SHORTCUT_MODIFIERS.contains(&token)
}

/// Whether this is an Apple platform, where ⌘ takes the place of Ctrl. `None` asks about this machine.
pub fn is_apple_platform(platform: Option<&str>) -> bool {
    let platform = platform.unwrap_or(navigator_platform());
    ["Mac", "iPhone", "iPad"].iter().any(|prefix| platform.starts_with(prefix))
}

/// A binding with [`PRIMARY_MODIFIER`] replaced by this platform's key.
pub fn concrete_binding(binding: &[impl AsRef<str>], platform: Option<&str>) -> Vec<String> {
    let primary = if is_apple_platform(platform) { "Meta" } else { "Control" };
    binding.iter().map(|token| if token.as_ref() == PRIMARY_MODIFIER { primary.to_owned() } else { token.as_ref().to_owned() }).collect()
}

/// A command's default binding on this platform, in real keys.
pub fn default_shortcut_binding(command: &ShortcutCommand) -> Vec<String> {
    order_binding(&concrete_binding(command.default_binding, None))
}

/// Resolves the current binding from a user override or the code default.
pub fn resolve_shortcut(shortcuts: &BTreeMap<String, ShortcutPreference>, command: &ShortcutCommand) -> ShortcutPreference {
    match shortcuts.get(command.id) {
        Some(stored) => stored.clone(),
        None => ShortcutPreference { binding: default_shortcut_binding(command), enabled: command.default_enabled },
    }
}

/// Whether a command differs from its default binding.
pub fn is_shortcut_modified(shortcuts: &BTreeMap<String, ShortcutPreference>, command: &ShortcutCommand) -> bool {
    let Some(stored) = shortcuts.get(command.id) else { return false };
    let fallback = default_shortcut_binding(command);
    stored.enabled != command.default_enabled || !(stored.binding.len() == fallback.len() && bindings_conflict(&stored.binding, &fallback))
}

fn named_key_label(token: &str) -> Option<&'static str> {
    Some(match token {
        "Comma" => ",",
        "Period" => ".",
        "Slash" => "/",
        "Backslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "Backquote" => "`",
        "Minus" => "-",
        "Equal" => "=",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Space" => "Space",
        "Enter" => "Enter",
        "Tab" => "Tab",
        "Escape" => "Esc",
        "Backspace" => "Backspace",
        "Delete" => "Delete",
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "PgUp",
        "PageDown" => "PgDn",
        _ => return None,
    })
}

/// Keycap text for a key on an Apple platform.
fn apple_key_label(token: &str) -> Option<&'static str> {
    Some(match token {
        "Control" => "⌃",
        "Alt" => "⌥",
        "Shift" => "⇧",
        "Meta" => "⌘",
        "Enter" => "↩",
        _ => return None,
    })
}

/// Keycap text for one token: the Mac symbols (⌘ ⌥ ⌃ ⇧ ↩) on an Apple platform, the Windows names elsewhere.
pub fn key_token_label(token: &str, platform: Option<&str>) -> String {
    if is_apple_platform(platform) {
        if let Some(label) = apple_key_label(token) {
            return label.to_owned();
        }
    }
    match token {
        "Control" => return "Ctrl".to_owned(),
        "Meta" => return "Win".to_owned(),
        "Alt" | "Shift" => return token.to_owned(),
        _ => {}
    }
    if let Some(letter) = token.strip_prefix("Key") {
        return letter.to_owned();
    }
    if let Some(digit) = token.strip_prefix("Digit") {
        return digit.to_owned();
    }
    if let Some(key) = token.strip_prefix("Numpad") {
        return format!("Num{key}");
    }
    named_key_label(token).unwrap_or(token).to_owned()
}

/// Keycaps for a binding, with modifiers in canonical order.
pub fn shortcut_key_caps(binding: &[impl AsRef<str>], platform: Option<&str>) -> Vec<String> {
    order_binding(&concrete_binding(binding, platform)).iter().map(|token| key_token_label(token, platform)).collect()
}

/// Canonical order: `Control`, `Alt`, `Shift`, `Meta`, then the terminal key: the order macOS menus print them, ⌃ ⌥ ⇧ ⌘.
pub fn order_binding(binding: &[impl AsRef<str>]) -> Vec<String> {
    let modifiers = SHORTCUT_MODIFIERS.iter().filter(|modifier| binding.iter().any(|token| token.as_ref() == **modifier)).map(|modifier| (*modifier).to_owned());
    let rest = binding.iter().map(AsRef::as_ref).filter(|token| !is_modifier_token(token)).map(str::to_owned);
    modifiers.chain(rest).collect()
}

/// `^F([1-9]|1\d|2[0-4])$`.
fn is_function_key(token: &str) -> bool {
    let Some(number) = token.strip_prefix('F') else { return false };
    (1..=2).contains(&number.len()) && number.bytes().all(|b| b.is_ascii_digit()) && !number.starts_with('0') && number.parse::<u8>().is_ok_and(|n| n <= 24)
}

/// Whether a binding is valid: no repeated tokens, and at least one modifier plus exactly one terminal key, except a
/// lone `Escape` or function key. A lone letter would trigger while typing.
pub fn is_valid_binding(binding: &[impl AsRef<str>]) -> bool {
    if binding.is_empty() {
        return false;
    }
    if binding.iter().map(AsRef::as_ref).collect::<HashSet<&str>>().len() != binding.len() {
        return false;
    }
    let terminals: Vec<&str> = binding.iter().map(AsRef::as_ref).filter(|token| !is_modifier_token(token)).collect();
    let [terminal] = terminals[..] else { return false };
    binding.len() > 1 || terminal == "Escape" || is_function_key(terminal)
}

/// Whether two bindings conflict, ignoring modifier order.
pub fn bindings_conflict(left: &[impl AsRef<str>], right: &[impl AsRef<str>]) -> bool {
    if left.is_empty() || right.is_empty() || left.len() != right.len() {
        return false;
    }
    left.iter().all(|token| right.iter().any(|other| other.as_ref() == token.as_ref()))
}

/// The enabled command that already has `binding`, other than `command_id` (`findConflict` of `ShortcutSettings`).
pub fn find_conflict(shortcuts: &BTreeMap<String, ShortcutPreference>, command_id: &str, binding: &[impl AsRef<str>]) -> Option<&'static ShortcutCommand> {
    SHORTCUT_COMMANDS.iter().find(|other| {
        if other.id == command_id {
            return false;
        }
        let resolved = resolve_shortcut(shortcuts, other);
        resolved.enabled && bindings_conflict(binding, &resolved.binding)
    })
}

/// The binding a key press makes, by `code`, so it doesn't depend on the keyboard layout. `None` until a complete
/// combination is pressed: a lone modifier isn't one.
pub fn binding_from_event(event: &KeyInput) -> Option<Vec<String>> {
    let mut tokens: Vec<String> = Vec::new();
    if event.ctrl {
        tokens.push("Control".to_owned());
    }
    if event.alt {
        tokens.push("Alt".to_owned());
    }
    if event.shift {
        tokens.push("Shift".to_owned());
    }
    if event.meta {
        tokens.push("Meta".to_owned());
    }
    if event.code.is_empty() || is_modifier_code(&event.code) {
        return None;
    }
    tokens.push(event.code.clone());
    Some(order_binding(&tokens))
}

/// `^(Control|Alt|Shift|Meta|OS)(Left|Right)$`.
fn is_modifier_code(code: &str) -> bool {
    ["Control", "Alt", "Shift", "Meta", "OS"]
        .iter()
        .any(|name| code.strip_prefix(name).is_some_and(|side| side == "Left" || side == "Right"))
}

/// Whether a key press belongs to the input method rather than the page.
///
/// WebView2 reports `isComposing` on the key that commits a composition. macOS WKWebView fires `compositionend` before
/// that keydown, so Enter confirming Pinyin letters would read as a plain Enter, and only `keyCode` 229 still marks it.
pub fn is_ime_key_event(event: &KeyInput) -> bool {
    event.is_composing || event.key == "Process" || event.key_code == 229
}

/// Whether a key press matches a binding.
pub fn matches_event(binding: &[impl AsRef<str>], event: &KeyInput) -> bool {
    if binding.is_empty() {
        return false;
    }
    binding_from_event(event).is_some_and(|pressed| bindings_conflict(binding, &pressed))
}

/// Whether a key should be suppressed at the current focus. `in_text_input` is what upstream asked the DOM with
/// `closest('input, textarea, select, [contenteditable="true"]')`.
///
/// Unmodified shortcuts yield to typing in inputs. Ctrl/Alt/Meta combinations still dispatch; a lone Shift is not a
/// modifier because `Shift+A` is uppercase A.
pub fn should_suppress_for_focus(binding: &[impl AsRef<str>], in_text_input: bool) -> bool {
    let has_real_modifier = binding.iter().any(|token| matches!(token.as_ref(), "Control" | "Alt" | "Meta"));
    !has_real_modifier && in_text_input
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_TOKENS: [&str; 0] = [];

    fn keydown(key: &str, code: &str) -> KeyInput {
        KeyInput { key: key.to_owned(), code: code.to_owned(), ..KeyInput::default() }
    }

    fn command(id: &str) -> &'static ShortcutCommand {
        shortcut_command(id).unwrap()
    }

    fn no_overrides() -> BTreeMap<String, ShortcutPreference> {
        BTreeMap::new()
    }

    /// Pretends to be on `platform` until it is dropped, as `onTestFinished(() => spy.mockRestore())` did.
    struct OnPlatform;

    fn on_platform(platform: &'static str) -> OnPlatform {
        TEST_PLATFORM.with(|cell| cell.set(Some(platform)));
        OnPlatform
    }

    impl Drop for OnPlatform {
        fn drop(&mut self) {
            TEST_PLATFORM.with(|cell| cell.set(None));
        }
    }

    #[test]
    fn claims_the_webkit_keydown_that_commits_a_composition_after_compositionend() {
        // macOS WKWebView: Enter confirming Pinyin letters arrives with isComposing false.
        assert!(is_ime_key_event(&KeyInput { key_code: 229, ..keydown("Enter", "Enter") }));
        assert!(is_ime_key_event(&KeyInput { key_code: 229, shift: true, ..keydown("Enter", "Enter") }));
    }

    #[test]
    fn claims_chromium_composition_keystrokes() {
        assert!(is_ime_key_event(&KeyInput { is_composing: true, ..keydown("Enter", "Enter") }));
        assert!(is_ime_key_event(&keydown("Process", "KeyN")));
    }

    #[test]
    fn leaves_ordinary_enter_and_shift_enter_to_the_page() {
        assert!(!is_ime_key_event(&KeyInput { key_code: 13, ..keydown("Enter", "Enter") }));
        assert!(!is_ime_key_event(&KeyInput { key_code: 13, shift: true, ..keydown("Enter", "Enter") }));
    }

    #[test]
    fn keeps_the_default_send_and_newline_bindings_apart() {
        let enter = keydown("Enter", "Enter");
        let shift_enter = KeyInput { shift: true, ..keydown("Enter", "Enter") };
        assert!(matches_event(&["Enter"], &enter));
        assert!(!matches_event(&["Enter"], &shift_enter));
        assert!(matches_event(&["Shift", "Enter"], &shift_enter));
        assert!(!matches_event(&["Shift", "Enter"], &enter));
    }

    #[test]
    fn uses_cmd_where_windows_uses_ctrl_on_a_mac_and_shows_mac_keycaps() {
        let _platform = on_platform("MacIntel");
        let none = no_overrides();
        assert_eq!(resolve_shortcut(&none, command("conversation.create")).binding, ["Meta", "KeyN"]);
        assert_eq!(resolve_shortcut(&none, command("app.settings.open")).binding, ["Meta", "Comma"]);
        assert_eq!(resolve_shortcut(&none, command("app.conversation_settings.open")).binding, ["Shift", "Meta", "Comma"]);
        assert_eq!(resolve_shortcut(&none, command("panel.browser.toggle")).binding, ["Meta", "KeyB"]);
        assert_eq!(resolve_shortcut(&none, command("app.zoom.in")).binding, ["Meta", "Equal"]);
        // Switching conversations stays on Control, as switching tabs does.
        assert_eq!(resolve_shortcut(&none, command("conversation.next")).binding, ["Control", "Tab"]);
        assert_eq!(shortcut_key_caps(&["Shift", "Meta", "Comma"], None), ["⇧", "⌘", ","]);
        assert_eq!(shortcut_key_caps(&["Control", "Shift", "Tab"], None), ["⌃", "⇧", "Tab"]);
        assert_eq!(key_token_label("Alt", None), "⌥");
        assert_eq!(key_token_label("Enter", None), "↩");
        // A recorded ⌘N is the default, not an override.
        let recorded = BTreeMap::from([("conversation.create".to_owned(), ShortcutPreference { binding: vec!["Meta".to_owned(), "KeyN".to_owned()], enabled: true })]);
        assert!(!is_shortcut_modified(&recorded, command("conversation.create")));
        let binding = resolve_shortcut(&none, command("conversation.create")).binding;
        assert!(matches_event(&binding, &KeyInput { meta: true, ..keydown("n", "KeyN") }));
        assert!(!matches_event(&binding, &KeyInput { ctrl: true, ..keydown("n", "KeyN") }));
    }

    #[test]
    fn keeps_ctrl_and_the_windows_key_names_elsewhere() {
        let _platform = on_platform("Win32");
        assert_eq!(resolve_shortcut(&no_overrides(), command("conversation.create")).binding, ["Control", "KeyN"]);
        assert_eq!(shortcut_key_caps(&["Control", "Shift", "Comma"], None), ["Ctrl", "Shift", ","]);
        assert_eq!(key_token_label("Meta", None), "Win");
    }

    // The rest aren't upstream's: its test file leaves these exports to the settings page's tests.

    #[test]
    fn names_every_command_in_both_languages_and_groups_them_all() {
        for command in SHORTCUT_COMMANDS {
            assert!(!command.zh.is_empty() && !command.en.is_empty(), "{}", command.id);
            assert!(SHORTCUT_GROUP_ORDER.contains(&command.group), "{}", command.id);
        }
        let ids: HashSet<&str> = SHORTCUT_COMMANDS.iter().map(|command| command.id).collect();
        assert_eq!(ids.len(), SHORTCUT_COMMANDS.len());
        assert_eq!(SHORTCUT_COMMANDS.len(), 13);
    }

    #[test]
    fn leaves_only_the_stop_command_unbound_and_marks_zoom_as_the_apps_own() {
        let unbound: Vec<&str> = SHORTCUT_COMMANDS.iter().filter(|c| c.default_binding.is_empty()).map(|c| c.id).collect();
        assert_eq!(unbound, ["conversation.stop"]);
        let fixed: Vec<&str> = SHORTCUT_COMMANDS.iter().filter(|c| !c.editable).map(|c| c.id).collect();
        assert_eq!(fixed, ["app.zoom.in", "app.zoom.out", "app.zoom.reset"]);
    }

    #[test]
    fn asks_the_given_platform_before_this_machine() {
        assert!(is_apple_platform(Some("MacIntel")));
        assert!(is_apple_platform(Some("iPhone")));
        assert!(is_apple_platform(Some("iPad Simulator")));
        assert!(!is_apple_platform(Some("Win32")));
        assert!(!is_apple_platform(Some("Linux x86_64")));
        assert_eq!(concrete_binding(&[PRIMARY_MODIFIER, "KeyN"], Some("MacIntel")), ["Meta", "KeyN"]);
        assert_eq!(concrete_binding(&[PRIMARY_MODIFIER, "KeyN"], Some("Win32")), ["Control", "KeyN"]);
    }

    #[test]
    fn labels_every_family_of_token() {
        let win = Some("Win32");
        for (token, label) in [
            ("Control", "Ctrl"), ("Meta", "Win"), ("Alt", "Alt"), ("Shift", "Shift"), ("KeyN", "N"), ("Digit0", "0"), ("Numpad5", "Num5"),
            ("F12", "F12"), ("Comma", ","), ("Escape", "Esc"), ("ArrowUp", "↑"), ("PageDown", "PgDn"), ("Enter", "Enter"), ("AudioVolumeUp", "AudioVolumeUp"),
        ] {
            assert_eq!(key_token_label(token, win), label, "{token}");
        }
        assert_eq!(key_token_label("KeyN", Some("MacIntel")), "N");
    }

    #[test]
    fn orders_modifiers_canonically_and_keeps_the_terminal_last() {
        assert_eq!(order_binding(&["Meta", "KeyN", "Shift", "Alt", "Control"]), ["Control", "Alt", "Shift", "Meta", "KeyN"]);
        assert_eq!(order_binding(&["Enter"]), ["Enter"]);
    }

    #[test]
    fn accepts_a_modifier_with_one_key_and_only_escape_or_a_function_key_alone() {
        assert!(is_valid_binding(&["Control", "KeyN"]));
        assert!(is_valid_binding(&["Control", "Shift", "Enter"]));
        assert!(is_valid_binding(&["Escape"]));
        for key in ["F1", "F9", "F10", "F19", "F20", "F24"] {
            assert!(is_valid_binding(&[key]), "{key}");
        }
        for key in ["F0", "F25", "F30", "F01", "F", "F100", "F+1", "KeyA", "Enter"] {
            assert!(!is_valid_binding(&[key]), "{key}");
        }
        assert!(!is_valid_binding(&NO_TOKENS));
        assert!(!is_valid_binding(&["Control"]));
        assert!(!is_valid_binding(&["Control", "Control", "KeyN"]));
        assert!(!is_valid_binding(&["Control", "KeyN", "KeyM"]));
    }

    #[test]
    fn compares_bindings_ignoring_order_and_never_matches_an_empty_one() {
        assert!(bindings_conflict(&["Shift", "Control", "KeyN"], &["Control", "Shift", "KeyN"]));
        assert!(!bindings_conflict(&["Control", "KeyN"], &["Control", "Shift", "KeyN"]));
        assert!(!bindings_conflict(&NO_TOKENS, &NO_TOKENS));
        assert!(!matches_event(&NO_TOKENS, &keydown("Enter", "Enter")));
    }

    #[test]
    fn finds_the_other_enabled_command_with_a_binding() {
        let _platform = on_platform("Win32");
        let none = no_overrides();
        let taken = ["Control", "KeyN"];
        assert_eq!(find_conflict(&none, "panel.close", &taken).map(|c| c.id), Some("conversation.create"));
        assert_eq!(find_conflict(&none, "conversation.create", &taken), None);
        // A disabled command holds no key.
        assert_eq!(find_conflict(&none, "panel.close", &["Control", "Shift", "KeyC"]), None);
        let disabled = BTreeMap::from([("conversation.create".to_owned(), ShortcutPreference { binding: vec!["Control".to_owned(), "KeyN".to_owned()], enabled: false })]);
        assert_eq!(find_conflict(&disabled, "panel.close", &taken), None);
    }

    #[test]
    fn reads_no_binding_from_a_lone_modifier_or_a_nameless_key() {
        for code in ["ControlLeft", "AltRight", "ShiftLeft", "MetaRight", "OSLeft"] {
            assert_eq!(binding_from_event(&keydown("", code)), None, "{code}");
        }
        assert_eq!(binding_from_event(&keydown("é", "")), None);
        assert_eq!(binding_from_event(&KeyInput { meta: true, shift: true, ctrl: true, alt: true, ..keydown("n", "KeyN") }).unwrap(), ["Control", "Alt", "Shift", "Meta", "KeyN"]);
    }

    #[test]
    fn yields_an_unmodified_binding_to_typing_and_nothing_else() {
        assert!(should_suppress_for_focus(&["Escape"], true));
        assert!(!should_suppress_for_focus(&["Escape"], false));
        // A lone Shift isn't a modifier: Shift+A is uppercase A.
        assert!(should_suppress_for_focus(&["Shift", "KeyA"], true));
        for modifier in ["Control", "Alt", "Meta"] {
            assert!(!should_suppress_for_focus(&[modifier, "KeyA"], true), "{modifier}");
        }
    }

    #[test]
    fn adapts_a_fluent_key_press() {
        let press = |key, shift, cmd, alt, ctrl| KeyInput::from_key_press(KeyPress { key, shift, cmd, alt, ctrl });
        assert_eq!(press(Key::Enter, true, false, false, false).code, "Enter");
        assert!(press(Key::Enter, true, false, false, false).shift);
        assert_eq!(press(Key::Left, false, false, false, false).code, "ArrowLeft");
        assert_eq!(press(Key::Left, false, false, false, false).key, "ArrowLeft");
        assert_eq!(press(Key::F(5), false, false, false, false).code, "F5");
        assert_eq!(press(Key::Char('n'), false, false, false, true).code, "KeyN");
        assert_eq!(press(Key::Char('N'), true, false, false, true).code, "KeyN");
        assert_eq!(press(Key::Char('n'), false, false, false, true).key, "n");
        assert_eq!(press(Key::Char('0'), false, false, false, true).code, "Digit0");
        // Shift changes what the key types, not which key it is.
        assert_eq!(press(Key::Char('<'), true, false, false, true).code, "Comma");
        assert_eq!(press(Key::Char(')'), true, false, false, true).code, "Digit0");
        assert_eq!(press(Key::Char('é'), false, false, false, true).code, "");
        assert!(press(Key::Char('n'), false, false, true, false).alt);
        assert!(press(Key::Char('n'), false, false, false, true).ctrl);
        assert!(!press(Key::Char('n'), false, false, false, true).meta);

        let cmd = press(Key::Char('n'), false, true, false, false);
        assert_eq!(cmd.meta, cfg!(target_os = "macos"));
        assert_eq!(cmd.ctrl, !cfg!(target_os = "macos"));
        assert!(!is_ime_key_event(&cmd));
        // Whichever key `cmd` is here, the primary modifier's default binding answers to it.
        assert!(matches_event(&concrete_binding(&[PRIMARY_MODIFIER, "KeyN"], None), &cmd));
    }
}
