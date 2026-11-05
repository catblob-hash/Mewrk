//! Key names, as the model writes them and as the viewer's `KeyboardEvent.key`
//! reports them, and what each protocol calls them.
//!
//! One vocabulary for both callers keeps `preview_input`'s `"key": "ctrl+c"`
//! and a person pressing Ctrl+C in the pane on the same path. The names follow
//! the DOM's `KeyboardEvent.key` where one exists (`Enter`, `Escape`,
//! `Backspace`), with the arrow keys shortened the way computer-use tools
//! write them (`Up`, not `ArrowUp`; both are accepted), plus the hardware
//! buttons a phone has and a keyboard does not.
//!
//! `Home` is the one word that means two things. On a keyboard it is the
//! line-start key; on a phone it is the button that goes back to the home
//! screen, which is what anyone driving a phone means by it. [`parse_chord`]
//! therefore resolves it by platform, and the viewer — whose Home key really
//! is a keyboard's — goes through [`from_dom_key`], which never does.

use super::driver::Platform;

/// A key that is not a printable character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Space,
    Up,
    Down,
    Left,
    Right,
    /// The keyboard's line-start key.
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    /// `F1`..`F12`.
    F(u8),
    Shift,
    Control,
    Alt,
    /// Command on a Mac, the Windows key elsewhere.
    Meta,
    /// The shortcut key of the screen's system — `cmd` on macOS Screen Sharing, `ctrl` on
    /// Android. Which one is a fact of the live connection, so a name that says it
    /// (`"mod+a"`) is read as this and replaced only once a batch holds the screen
    /// ([`KeyChord::with_mod_as`], [`replace_mod`]); it never reaches a driver, and has no
    /// keysym and no key code.
    Mod,
    CapsLock,
    ContextMenu,
    // Hardware buttons.
    /// Android's back button.
    Back,
    /// The phone's home button: back to the home screen.
    HomeScreen,
    /// Android's recent-apps button.
    AppSwitch,
    VolumeUp,
    VolumeDown,
    Power,
}

/// One key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Named(NamedKey),
    Char(char),
}

impl Key {
    pub fn is_modifier(self) -> bool {
        matches!(
            self,
            Self::Named(
                NamedKey::Shift
                    | NamedKey::Control
                    | NamedKey::Alt
                    | NamedKey::Meta
                    | NamedKey::Mod
            )
        )
    }
}

/// `mod` as a key.
const MOD: Key = Key::Named(NamedKey::Mod);

/// Modifiers held while one key is pressed: `ctrl+shift+t`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChord {
    /// In the order they were written, which is the order they go down (and
    /// the reverse of the order they come up).
    pub modifiers: Vec<Key>,
    pub key: Key,
}

impl KeyChord {
    /// Whether the chord names `mod`, as a modifier or as the key itself.
    pub fn uses_mod(&self) -> bool {
        self.key == MOD || self.modifiers.contains(&MOD)
    }

    /// The chord with `mod` replaced by `primary` (`ctrl` or `cmd`); a modifier that is then
    /// named twice is held once.
    pub fn with_mod_as(&self, primary: NamedKey) -> Self {
        Self {
            modifiers: mod_as(&self.modifiers, primary),
            key: if self.key == MOD {
                Key::Named(primary)
            } else {
                self.key
            },
        }
    }
}

/// `modifiers` with `mod` replaced by `primary`, in the order written, each held once.
pub fn mod_as(modifiers: &[Key], primary: NamedKey) -> Vec<Key> {
    let mut resolved: Vec<Key> = Vec::with_capacity(modifiers.len());
    for modifier in modifiers {
        let modifier = if *modifier == MOD {
            Key::Named(primary)
        } else {
            *modifier
        };
        if !resolved.contains(&modifier) {
            resolved.push(modifier);
        }
    }
    resolved
}

/// The names the model may use, for error messages.
pub const KEY_NAMES_HINT: &str = "Enter, Tab, Escape, Backspace, Delete, Space, Up, Down, Left, Right, Home, End, PageUp, PageDown, Insert, F1-F12, a single character, modifiers ctrl/shift/alt/cmd/mod joined with + (\"ctrl+c\"; mod is cmd on macOS Screen Sharing and ctrl on Android); on a phone also Back, Home (the home screen), AppSwitch, VolumeUp, VolumeDown, Power";

fn named(name: &str, platform: Platform) -> Option<NamedKey> {
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "enter" | "return" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "escape" | "esc" => NamedKey::Escape,
        "backspace" => NamedKey::Backspace,
        "delete" | "del" => NamedKey::Delete,
        "space" | "spacebar" => NamedKey::Space,
        "up" | "arrowup" => NamedKey::Up,
        "down" | "arrowdown" => NamedKey::Down,
        "left" | "arrowleft" => NamedKey::Left,
        "right" | "arrowright" => NamedKey::Right,
        "home" if platform.is_touch() => NamedKey::HomeScreen,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" | "page_up" => NamedKey::PageUp,
        "pagedown" | "page_down" => NamedKey::PageDown,
        "insert" => NamedKey::Insert,
        "shift" => NamedKey::Shift,
        "ctrl" | "control" => NamedKey::Control,
        "alt" | "option" | "opt" => NamedKey::Alt,
        "cmd" | "command" | "meta" | "super" | "win" | "windows" => NamedKey::Meta,
        "mod" => NamedKey::Mod,
        "capslock" => NamedKey::CapsLock,
        "contextmenu" | "menu" => NamedKey::ContextMenu,
        "back" => NamedKey::Back,
        "homescreen" => NamedKey::HomeScreen,
        "appswitch" | "recents" | "recentapps" => NamedKey::AppSwitch,
        "volumeup" => NamedKey::VolumeUp,
        "volumedown" => NamedKey::VolumeDown,
        "power" => NamedKey::Power,
        _ => {
            let number = lower.strip_prefix('f')?.parse::<u8>().ok()?;
            if !(1..=12).contains(&number) {
                return None;
            }
            NamedKey::F(number)
        }
    })
}

fn one_key(text: &str, platform: Platform) -> Option<Key> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(only), None) if only == ' ' => Some(Key::Named(NamedKey::Space)),
        (Some(only), None) if !only.is_control() => Some(Key::Char(only)),
        _ => named(text, platform).map(Key::Named),
    }
}

/// The `+`-separated parts of a chord's text, which has no space around it. A `+` right at a
/// part's start is the plus key itself.
fn chord_parts(trimmed: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = trimmed;
    while !rest.is_empty() {
        let first = rest.chars().next().map_or(1, char::len_utf8);
        let end = rest[first..].find('+').map_or(rest.len(), |at| at + first);
        parts.push(&rest[..end]);
        rest = rest[end..].strip_prefix('+').unwrap_or("");
    }
    parts
}

/// A chord's text with every part that says `mod` written as `primary` instead (`"Mod+a"` as
/// `"ctrl+a"`), the rest as it was.
pub fn replace_mod(text: &str, primary: NamedKey) -> String {
    let trimmed = text.trim();
    let name = modifier_name(Key::Named(primary)).unwrap_or("ctrl");
    chord_parts(trimmed)
        .into_iter()
        .map(|part| {
            if named(part.trim(), Platform::Desktop) == Some(NamedKey::Mod) {
                name
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Reads what the model wrote: `"Enter"`, `"ctrl+c"`, `"cmd+shift+4"`, `"a"`,
/// `"Back"`. A lone `+` is the plus key; `"ctrl++"` is Ctrl with plus. `mod` is the screen
/// system's shortcut key ([`NamedKey::Mod`]); whether that is ctrl or cmd is for the batch to
/// say once it holds the screen.
pub fn parse_chord(text: &str, platform: Platform) -> Result<KeyChord, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!("\"key\" is empty. Key names: {KEY_NAMES_HINT}."));
    }
    let parts = chord_parts(trimmed);
    let unknown = |part: &str| {
        format!(
            "Unknown key \"{}\" in \"{}\". Key names: {KEY_NAMES_HINT}.",
            crate::preview_launch_config::sanitize_for_message(part),
            crate::preview_launch_config::sanitize_for_message(trimmed)
        )
    };
    let (last, leading) = parts.split_last().ok_or_else(|| unknown(trimmed))?;
    let mut modifiers = Vec::with_capacity(leading.len());
    for part in leading {
        let key = one_key(part.trim(), platform).ok_or_else(|| unknown(part))?;
        if !key.is_modifier() {
            return Err(format!(
                "\"{}\" is not a modifier; only ctrl, shift, alt and cmd can be combined with a key (\"ctrl+c\").",
                crate::preview_launch_config::sanitize_for_message(part)
            ));
        }
        if !modifiers.contains(&key) {
            modifiers.push(key);
        }
    }
    let key = one_key(last.trim(), platform).ok_or_else(|| unknown(last))?;
    Ok(KeyChord { modifiers, key })
}

/// The modifier names, for error messages.
pub const MODIFIER_NAMES_HINT: &str =
    "shift, ctrl, alt (or option), cmd (or command, meta, super, win) and mod (cmd on macOS Screen Sharing), joined with + (\"ctrl+shift\")";

/// Reads the modifiers `preview_input` holds down during a click, long press,
/// drag or scroll: modifier names only, joined with `+`, in any case —
/// `"shift"`, `"Ctrl+Shift"`, `"cmd+alt"`. In the order written, which is the
/// order they go down; one written twice is held once.
pub fn parse_modifiers(text: &str) -> Result<Vec<Key>, String> {
    let shown = crate::preview_launch_config::sanitize_for_message(text);
    let mut modifiers = Vec::new();
    for part in text.split('+') {
        let name = part.trim();
        if name.is_empty() {
            return Err(format!(
                "\"{shown}\" has an empty name between its + signs. Modifiers: {MODIFIER_NAMES_HINT}."
            ));
        }
        let modifier = named(name, Platform::Desktop)
            .map(Key::Named)
            .filter(|key| key.is_modifier())
            .ok_or_else(|| {
                format!(
                    "\"{}\" is not a modifier; modifiers are {MODIFIER_NAMES_HINT}.",
                    crate::preview_launch_config::sanitize_for_message(name)
                )
            })?;
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    Ok(modifiers)
}

/// The name a modifier is written with: `shift`, `ctrl`, `alt`, `cmd` or `mod`.
pub fn modifier_name(key: Key) -> Option<&'static str> {
    match key {
        Key::Named(NamedKey::Shift) => Some("shift"),
        Key::Named(NamedKey::Control) => Some("ctrl"),
        Key::Named(NamedKey::Alt) => Some("alt"),
        Key::Named(NamedKey::Meta) => Some("cmd"),
        Key::Named(NamedKey::Mod) => Some("mod"),
        _ => None,
    }
}

/// What a `KeyboardEvent.key` from the viewer is. `None` for keys this
/// vocabulary has no word for (dead keys, `Unidentified`, IME process keys);
/// the viewer sends composed text separately. Never `mod`: no keyboard has that key.
pub fn from_dom_key(key: &str) -> Option<Key> {
    match key {
        "Home" => Some(Key::Named(NamedKey::Home)),
        "OS" => Some(Key::Named(NamedKey::Meta)),
        _ => one_key(key, Platform::Desktop).filter(|key| *key != MOD),
    }
}

/// The X11 keysym RFB sends for `key`, or `None` for a phone button. A
/// character beyond Latin-1 is a Unicode keysym (`0x0100_0000 | code point`),
/// which a server that reads keysyms in sixteen bits misreads
/// ([`misread_in_sixteen_bits`]); the VNC driver keeps those away from it.
pub fn keysym(key: Key) -> Option<u32> {
    Some(match key {
        Key::Char(character) => {
            if is_latin1_keysym(character) {
                u32::from(character)
            } else {
                0x0100_0000 | u32::from(character)
            }
        }
        Key::Named(named) => match named {
            NamedKey::Enter => 0xff0d,
            NamedKey::Tab => 0xff09,
            NamedKey::Escape => 0xff1b,
            NamedKey::Backspace => 0xff08,
            NamedKey::Delete => 0xffff,
            NamedKey::Space => 0x0020,
            NamedKey::Up => 0xff52,
            NamedKey::Down => 0xff54,
            NamedKey::Left => 0xff51,
            NamedKey::Right => 0xff53,
            NamedKey::Home => 0xff50,
            NamedKey::End => 0xff57,
            NamedKey::PageUp => 0xff55,
            NamedKey::PageDown => 0xff56,
            NamedKey::Insert => 0xff63,
            NamedKey::F(number) => 0xffbe + u32::from(number) - 1,
            NamedKey::Shift => 0xffe1,
            NamedKey::Control => 0xffe3,
            // Alt_L; Apple's server takes it for Command, so the VNC driver
            // sends Meta_L, its Option, there instead.
            NamedKey::Alt => 0xffe9,
            // Super_L, which is what noVNC sends for the Meta key and what
            // macOS Screen Sharing maps to Command.
            NamedKey::Meta => 0xffeb,
            NamedKey::CapsLock => 0xffe5,
            NamedKey::ContextMenu => 0xff67,
            NamedKey::Mod
            | NamedKey::Back
            | NamedKey::HomeScreen
            | NamedKey::AppSwitch
            | NamedKey::VolumeUp
            | NamedKey::VolumeDown
            | NamedKey::Power => return None,
        },
    })
}

/// Whether `character` is its own keysym: printable ASCII and printable Latin-1.
pub fn is_latin1_keysym(character: char) -> bool {
    let code = u32::from(character);
    (0x20..=0x7e).contains(&code) || (0xa0..=0xff).contains(&code)
}

/// Whether a server that looks keysyms up in sixteen bits — QEMU's — would
/// take the Unicode keysym of `character` for a key that does something.
///
/// QEMU drops the `0x0100` prefix: what is left of U+FF08 `（` is 0xFF08,
/// BackSpace; of U+FF0D `－`, Return; of U+FFE5 `￥`, Caps_Lock; of
/// U+FF50-FF57, Home, the arrows and End. Everything from 0xFD00 up is the
/// 3270, ISO (dead and modifier) and function-key block — and the full-width
/// forms CJK text is full of land there.
pub fn misread_in_sixteen_bits(character: char) -> bool {
    !is_latin1_keysym(character) && u32::from(character) & 0xffff >= 0xfd00
}

/// Android's `KEYCODE_*` number for `key`, as `input keyevent` takes it.
/// Characters other than letters and digits have no single keycode; they are
/// typed as text.
pub fn android_keycode(key: Key) -> Option<u32> {
    Some(match key {
        Key::Char(character) if character.is_ascii_lowercase() || character.is_ascii_uppercase() => {
            29 + u32::from(character.to_ascii_lowercase() as u8 - b'a')
        }
        Key::Char(character) if character.is_ascii_digit() => 7 + u32::from(character as u8 - b'0'),
        Key::Char(_) => return None,
        Key::Named(named) => match named {
            NamedKey::Enter => 66,
            NamedKey::Tab => 61,
            // Escape leaves the current screen on Android, which is Back.
            NamedKey::Escape | NamedKey::Back => 4,
            NamedKey::Backspace => 67,
            NamedKey::Delete => 112,
            NamedKey::Space => 62,
            NamedKey::Up => 19,
            NamedKey::Down => 20,
            NamedKey::Left => 21,
            NamedKey::Right => 22,
            NamedKey::Home => 122,
            NamedKey::End => 123,
            NamedKey::PageUp => 92,
            NamedKey::PageDown => 93,
            NamedKey::Insert => 124,
            NamedKey::F(number) => 130 + u32::from(number),
            NamedKey::Shift => 59,
            NamedKey::Control => 113,
            NamedKey::Alt => 57,
            NamedKey::Meta => 117,
            // Resolved to ctrl before it can be sent.
            NamedKey::Mod => return None,
            NamedKey::CapsLock => 115,
            NamedKey::ContextMenu => 82,
            NamedKey::HomeScreen => 3,
            NamedKey::AppSwitch => 187,
            NamedKey::VolumeUp => 24,
            NamedKey::VolumeDown => 25,
            NamedKey::Power => 26,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_read_modifiers_in_order_and_the_key_last() {
        let chord = parse_chord("ctrl+shift+t", Platform::Desktop).unwrap();
        assert_eq!(
            chord.modifiers,
            vec![Key::Named(NamedKey::Control), Key::Named(NamedKey::Shift)]
        );
        assert_eq!(chord.key, Key::Char('t'));
        assert_eq!(
            parse_chord("Cmd+Space", Platform::Desktop).unwrap(),
            KeyChord {
                modifiers: vec![Key::Named(NamedKey::Meta)],
                key: Key::Named(NamedKey::Space)
            }
        );
    }

    #[test]
    fn a_plus_sign_is_a_key_of_its_own() {
        assert_eq!(parse_chord("+", Platform::Desktop).unwrap().key, Key::Char('+'));
        let chord = parse_chord("ctrl++", Platform::Desktop).unwrap();
        assert_eq!(chord.modifiers, vec![Key::Named(NamedKey::Control)]);
        assert_eq!(chord.key, Key::Char('+'));
        assert_eq!(parse_chord("中", Platform::Desktop).unwrap().key, Key::Char('中'));
        assert_eq!(
            parse_chord("alt+é", Platform::Desktop).unwrap().key,
            Key::Char('é')
        );
    }

    #[test]
    fn home_is_the_home_screen_on_a_phone_and_line_start_on_a_desktop() {
        assert_eq!(
            parse_chord("Home", Platform::Android).unwrap().key,
            Key::Named(NamedKey::HomeScreen)
        );
        assert_eq!(
            parse_chord("home", Platform::Desktop).unwrap().key,
            Key::Named(NamedKey::Home)
        );
        // The viewer's Home key is a keyboard's, whatever the screen is.
        assert_eq!(from_dom_key("Home"), Some(Key::Named(NamedKey::Home)));
    }

    #[test]
    fn unknown_names_and_non_modifier_prefixes_are_refused_with_the_vocabulary() {
        let error = parse_chord("hyper+x", Platform::Desktop).unwrap_err();
        assert!(error.contains("Unknown key \"hyper\""), "{error}");
        assert!(error.contains("Enter, Tab"), "{error}");
        let error = parse_chord("a+b", Platform::Desktop).unwrap_err();
        assert!(error.contains("not a modifier"), "{error}");
        assert!(parse_chord("  ", Platform::Desktop).is_err());
        assert!(parse_chord("F13", Platform::Desktop).is_err());
    }

    #[test]
    fn modifiers_are_modifier_names_only_in_the_order_written() {
        assert_eq!(
            parse_modifiers("Ctrl+SHIFT").unwrap(),
            vec![Key::Named(NamedKey::Control), Key::Named(NamedKey::Shift)]
        );
        assert_eq!(
            parse_modifiers(" cmd + option + shift + command ").unwrap(),
            vec![
                Key::Named(NamedKey::Meta),
                Key::Named(NamedKey::Alt),
                Key::Named(NamedKey::Shift)
            ]
        );
        for name in ["meta", "super", "win", "Windows", "control", "opt", "alt"] {
            assert!(parse_modifiers(name).is_ok(), "{name}");
        }
        let error = parse_modifiers("shift+a").unwrap_err();
        assert!(error.starts_with("\"a\" is not a modifier"), "{error}");
        assert!(error.contains("shift, ctrl, alt"), "{error}");
        let error = parse_modifiers("ctrl+Enter").unwrap_err();
        assert!(error.starts_with("\"Enter\" is not a modifier"), "{error}");
        assert!(parse_modifiers("hyper").unwrap_err().contains("\"hyper\""));
        for empty in ["", "ctrl+", "+shift", "ctrl++shift", " "] {
            let error = parse_modifiers(empty).unwrap_err();
            assert!(error.contains("empty name"), "{empty:?}: {error}");
        }
        let names: Vec<_> = parse_modifiers("cmd+alt+ctrl+shift")
            .unwrap()
            .into_iter()
            .map(|key| modifier_name(key).unwrap())
            .collect();
        assert_eq!(names, ["cmd", "alt", "ctrl", "shift"]);
        assert_eq!(modifier_name(Key::Char('a')), None);
    }

    /// `mod` parses wherever a modifier does, in any case, and stays itself until a batch that
    /// knows the screen's system replaces it: it has no keysym and no key code of its own.
    #[test]
    fn mod_is_a_modifier_that_waits_to_be_resolved() {
        let mod_key = Key::Named(NamedKey::Mod);
        let chord = parse_chord("Mod+a", Platform::Desktop).unwrap();
        assert_eq!(chord.modifiers, vec![mod_key]);
        assert_eq!(chord.key, Key::Char('a'));
        assert!(chord.uses_mod());
        assert!(!parse_chord("ctrl+a", Platform::Desktop).unwrap().uses_mod());
        assert!(parse_chord("MOD", Platform::Android).unwrap().uses_mod());
        assert_eq!(
            parse_modifiers("shift+MOD").unwrap(),
            vec![Key::Named(NamedKey::Shift), mod_key]
        );
        assert!(mod_key.is_modifier());
        assert_eq!(modifier_name(mod_key), Some("mod"));
        assert_eq!(keysym(mod_key), None);
        assert_eq!(android_keycode(mod_key), None);
        // No keyboard has a key of that name, so the viewer never sends one.
        assert_eq!(from_dom_key("Mod"), None);
        assert_eq!(from_dom_key("m"), Some(Key::Char('m')));

        // Replaced by the shortcut key of the system, a modifier written twice held once.
        let ctrl = parse_chord("mod+a", Platform::Android)
            .unwrap()
            .with_mod_as(NamedKey::Control);
        assert_eq!(ctrl, parse_chord("ctrl+a", Platform::Android).unwrap());
        let cmd = parse_chord("shift+mod+Z", Platform::Desktop)
            .unwrap()
            .with_mod_as(NamedKey::Meta);
        assert_eq!(cmd, parse_chord("shift+cmd+Z", Platform::Desktop).unwrap());
        let once = parse_chord("ctrl+mod+a", Platform::Desktop)
            .unwrap()
            .with_mod_as(NamedKey::Control);
        assert_eq!(once.modifiers, vec![Key::Named(NamedKey::Control)]);
        let alone = parse_chord("mod", Platform::Desktop)
            .unwrap()
            .with_mod_as(NamedKey::Meta);
        assert_eq!(alone.key, Key::Named(NamedKey::Meta));
        assert_eq!(
            mod_as(&[mod_key, Key::Named(NamedKey::Alt)], NamedKey::Meta),
            vec![Key::Named(NamedKey::Meta), Key::Named(NamedKey::Alt)]
        );

        // The text of a chord, with what is not `mod` left as written.
        assert_eq!(replace_mod("Mod+a", NamedKey::Control), "ctrl+a");
        assert_eq!(
            replace_mod(" mod+ shift+Z ", NamedKey::Meta),
            "cmd+ shift+Z"
        );
        assert_eq!(replace_mod("ctrl++", NamedKey::Meta), "ctrl++");
        assert_eq!(replace_mod("Enter", NamedKey::Meta), "Enter");
        assert_eq!(replace_mod("+", NamedKey::Meta), "+");
    }

    #[test]
    fn dom_keys_cover_the_keyboard() {
        assert_eq!(from_dom_key("ArrowLeft"), Some(Key::Named(NamedKey::Left)));
        assert_eq!(from_dom_key("Enter"), Some(Key::Named(NamedKey::Enter)));
        assert_eq!(from_dom_key("é"), Some(Key::Char('é')));
        assert_eq!(from_dom_key(" "), Some(Key::Named(NamedKey::Space)));
        assert_eq!(from_dom_key("Dead"), None);
        assert_eq!(from_dom_key("Unidentified"), None);
    }

    #[test]
    fn keysyms_use_latin1_directly_and_the_unicode_range_beyond_it() {
        assert_eq!(keysym(Key::Char('a')), Some(0x61));
        assert_eq!(keysym(Key::Char('é')), Some(0xe9));
        assert_eq!(keysym(Key::Char('中')), Some(0x0100_4e2d));
        assert_eq!(keysym(Key::Char('😀')), Some(0x0101_f600));
        assert_eq!(keysym(Key::Named(NamedKey::F(1))), Some(0xffbe));
        assert_eq!(keysym(Key::Named(NamedKey::F(12))), Some(0xffc9));
        assert_eq!(keysym(Key::Named(NamedKey::Back)), None);
    }

    /// A server that reads keysyms in sixteen bits would press BackSpace for `（`, Return for
    /// `－`, Caps_Lock for `￥`.
    #[test]
    fn characters_a_sixteen_bit_server_would_read_as_keys_are_told_apart() {
        for character in [
            '（',
            '）',
            '；',
            '－',
            '，',
            '￥',
            '～',
            '\u{FD3E}',
            '\u{FE50}',
            '\u{1FF08}',
        ] {
            assert!(misread_in_sixteen_bits(character), "{character:?}");
        }
        for character in ['a', 'é', '中', '。', '\u{FCFF}', '😀'] {
            assert!(!misread_in_sixteen_bits(character), "{character:?}");
        }
    }

    #[test]
    fn android_keycodes_match_the_platform_constants() {
        assert_eq!(android_keycode(Key::Char('a')), Some(29));
        assert_eq!(android_keycode(Key::Char('Z')), Some(54));
        assert_eq!(android_keycode(Key::Char('0')), Some(7));
        assert_eq!(android_keycode(Key::Char('9')), Some(16));
        assert_eq!(android_keycode(Key::Named(NamedKey::F(1))), Some(131));
        assert_eq!(android_keycode(Key::Named(NamedKey::HomeScreen)), Some(3));
        assert_eq!(android_keycode(Key::Named(NamedKey::Escape)), Some(4));
        assert_eq!(android_keycode(Key::Char('?')), None);
    }
}
