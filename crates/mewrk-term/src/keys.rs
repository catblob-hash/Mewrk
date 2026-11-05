//! What each key press sends to the PTY. xterm's encodings, which is what `TERM=xterm-256color` promises.

use alacritty_terminal::term::TermMode;
use fluent::{Input, Key, KeyPress};

const ESC: u8 = 0x1b;

fn cursor_key(letter: u8, mods: u8, app_cursor: bool) -> Vec<u8> {
    match (mods, app_cursor) {
        (1, true) => vec![ESC, b'O', letter],
        (1, false) => vec![ESC, b'[', letter],
        _ => format!("\x1b[1;{mods}{}", letter as char).into_bytes(),
    }
}

fn tilde_key(code: u8, mods: u8) -> Vec<u8> {
    if mods == 1 { format!("\x1b[{code}~").into_bytes() } else { format!("\x1b[{code};{mods}~").into_bytes() }
}

fn function_key(n: u8, mods: u8) -> Option<Vec<u8>> {
    const FROM_F5: [u8; 16] = [15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34];
    match n {
        1..=4 if mods == 1 => Some(vec![ESC, b'O', b'P' + n - 1]),
        1..=4 => Some(format!("\x1b[1;{mods}{}", (b'P' + n - 1) as char).into_bytes()),
        5..=20 => Some(tilde_key(FROM_F5[n as usize - 5], mods)),
        _ => None,
    }
}

fn meta(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(ESC);
    }
    out.extend_from_slice(bytes);
    out
}

fn control_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        l @ 'a'..='z' => l as u8 & 0x1f,
        '@' | ' ' | '`' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

fn char_key(c: char, press: &KeyPress) -> Vec<u8> {
    if press.ctrl
        && let Some(byte) = control_byte(c)
    {
        return meta(press.alt, &[byte]);
    }
    let c = if press.shift { c.to_ascii_uppercase() } else { c };
    meta(press.alt, c.encode_utf8(&mut [0; 4]).as_bytes())
}

/// The bytes for a key press, or `None` for what the terminal does not send: Command chords belong to the app.
pub fn encode_key(press: &KeyPress, mode: TermMode) -> Option<Vec<u8>> {
    if press.cmd && !press.ctrl {
        return None;
    }
    let mods = 1 + press.shift as u8 + 2 * press.alt as u8 + 4 * press.ctrl as u8;
    let app = mode.contains(TermMode::APP_CURSOR);
    Some(match press.key {
        Key::Up => cursor_key(b'A', mods, app),
        Key::Down => cursor_key(b'B', mods, app),
        // Option+arrow is word motion on a Mac: zsh binds these out of the box and not the CSI forms, so xterm.js
        // sends them too.
        Key::Right if mods == 3 => vec![ESC, b'f'],
        Key::Left if mods == 3 => vec![ESC, b'b'],
        Key::Right => cursor_key(b'C', mods, app),
        Key::Left => cursor_key(b'D', mods, app),
        Key::Home => cursor_key(b'H', mods, app),
        Key::End => cursor_key(b'F', mods, app),
        Key::PageUp => tilde_key(5, mods),
        Key::PageDown => tilde_key(6, mods),
        Key::Delete => tilde_key(3, mods),
        Key::F(n) => return function_key(n, mods),
        Key::Enter if mode.contains(TermMode::LINE_FEED_NEW_LINE) => meta(press.alt, b"\r\n"),
        Key::Enter => meta(press.alt, b"\r"),
        Key::Tab if press.shift => b"\x1b[Z".to_vec(),
        Key::Tab => meta(press.alt, b"\t"),
        Key::Backspace => meta(press.alt, if press.ctrl { b"\x08" } else { b"\x7f" }),
        Key::Escape => meta(press.alt, b"\x1b"),
        Key::Char(c) => char_key(c, press),
    })
}

/// What pasted text becomes. Bracketing tells the shell that none of it is a command being typed.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if !mode.contains(TermMode::BRACKETED_PASTE) {
        return body.into_bytes();
    }
    // A terminator inside the clipboard would end the bracket early and run the rest as typed.
    let body = body.replace(['\x1b', '\u{9b}'], "");
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// The wheel in a full-screen app, which has no scrollback of its own to move: it becomes arrow keys.
pub fn wheel_arrows(up: bool, count: usize, mode: TermMode) -> Vec<u8> {
    cursor_key(if up { b'A' } else { b'B' }, 1, mode.contains(TermMode::APP_CURSOR)).repeat(count)
}

/// `moveToCellSequence`: the arrow keys that walk a shell's cursor from one cell to another, as `(column, line)`. A full-screen
/// app takes the lines and then the columns. A shell's line editor only moves along what it is editing, so everything else
/// goes along the wrap: left when the target is above, right when it is below.
pub fn cursor_moves(from: (usize, i32), to: (usize, i32), cols: usize, mode: TermMode) -> Vec<u8> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let along = |key: u8, count: usize| cursor_key(key, 1, app_cursor).repeat(count);
    if mode.contains(TermMode::ALT_SCREEN) {
        let lines = if to.1 < from.1 { along(b'A', from.1.abs_diff(to.1) as usize) } else { along(b'B', to.1.abs_diff(from.1) as usize) };
        let columns = if to.0 < from.0 { along(b'D', from.0 - to.0) } else { along(b'C', to.0 - from.0) };
        return [lines, columns].concat();
    }
    if from.1 == to.1 {
        return if to.0 < from.0 { along(b'D', from.0 - to.0) } else { along(b'C', to.0 - from.0) };
    }
    let up = to.1 < from.1;
    let rows = from.1.abs_diff(to.1) as usize;
    let (end_of, start_of) = if up { (to.0, from.0) } else { (from.0, to.0) };
    // To the end of the upper line, across whole lines, around the wrap, and in along the lower one.
    let cells = cols.saturating_sub(end_of + 1) + (rows - 1) * cols + 1 + start_of;
    along(if up { b'D' } else { b'C' }, cells)
}

/// Something the pointer did, for a program that asked to hear about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press(u8),
    Release(u8),
    /// Moved with this button held.
    Drag(u8),
    /// Up when true.
    Wheel(bool),
}

/// The report xterm would send, in the encoding the program chose: SGR when it asked, the legacy bytes otherwise.
pub fn mouse_report(action: MouseAction, col: usize, line: usize, mode: TermMode) -> Vec<u8> {
    let (button, release) = match action {
        MouseAction::Press(b) => (b, false),
        MouseAction::Release(b) => (b, true),
        MouseAction::Drag(b) => (b + 32, false),
        MouseAction::Wheel(up) => (if up { 64 } else { 65 }, false),
    };
    if mode.contains(TermMode::SGR_MOUSE) {
        return format!("\x1b[<{};{};{}{}", button, col + 1, line + 1, if release { 'm' } else { 'M' }).into_bytes();
    }
    // The legacy encoding has no way to say which button came up.
    let code = if release { 3 } else { button };
    let mut out = b"\x1b[M".to_vec();
    out.push(32 + code);
    // One byte per coordinate, which cannot say anything past 223. A UTF-8 terminal widens that, and `write` keeps text.
    for v in [col, line] {
        let v = (v + 33).min(if mode.contains(TermMode::UTF8_MOUSE) { 2047 } else { 127 }) as u32;
        let mut buf = [0u8; 4];
        out.extend_from_slice(char::from_u32(v).unwrap_or(' ').encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// Plain typing, without what `Input` also reports as keys. winit gives Enter and friends a text of their own,
/// and Ctrl+C a text of "c", which must not follow the 0x03 into the shell.
pub fn typed_text(input: &Input) -> String {
    let mut typed: String = input.text.chars().filter(|c| !c.is_control()).collect();
    for press in input.keys.iter().filter(|k| k.ctrl) {
        if let Key::Char(c) = press.key
            && let Some(at) = typed.find(|t: char| t.eq_ignore_ascii_case(&c))
        {
            typed.remove(at);
        }
    }
    typed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key) -> KeyPress {
        KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false }
    }

    fn sends(p: KeyPress) -> Vec<u8> {
        encode_key(&p, TermMode::empty()).unwrap()
    }

    #[test]
    fn control_letters_and_punctuation() {
        let ctrl = |c| KeyPress { ctrl: true, ..press(Key::Char(c)) };
        assert_eq!(sends(ctrl('c')), [0x03]);
        assert_eq!(sends(ctrl('C')), [0x03]);
        assert_eq!(sends(ctrl('a')), [0x01]);
        assert_eq!(sends(ctrl('z')), [0x1a]);
        assert_eq!(sends(ctrl(' ')), [0x00]);
        assert_eq!(sends(ctrl('[')), [0x1b]);
        assert_eq!(sends(ctrl('\\')), [0x1c]);
        assert_eq!(sends(ctrl('/')), [0x1f]);
        assert_eq!(sends(KeyPress { alt: true, ..ctrl('x') }), [0x1b, 0x18]);
    }

    #[test]
    fn option_is_meta() {
        let alt = |key| KeyPress { alt: true, ..press(key) };
        assert_eq!(sends(alt(Key::Char('b'))), b"\x1bb");
        assert_eq!(sends(KeyPress { shift: true, ..alt(Key::Char('b')) }), b"\x1bB");
        assert_eq!(sends(alt(Key::Backspace)), b"\x1b\x7f");
        assert_eq!(sends(alt(Key::Enter)), b"\x1b\r");
        assert_eq!(sends(alt(Key::Left)), b"\x1bb");
        assert_eq!(sends(alt(Key::Right)), b"\x1bf");
        assert_eq!(sends(alt(Key::Up)), b"\x1b[1;3A");
    }

    #[test]
    fn arrows_follow_the_cursor_mode_and_carry_modifiers() {
        assert_eq!(sends(press(Key::Up)), b"\x1b[A");
        assert_eq!(sends(press(Key::Left)), b"\x1b[D");
        assert_eq!(sends(press(Key::Home)), b"\x1b[H");
        assert_eq!(sends(press(Key::End)), b"\x1b[F");
        let app = TermMode::APP_CURSOR;
        assert_eq!(encode_key(&press(Key::Up), app).unwrap(), b"\x1bOA");
        assert_eq!(encode_key(&press(Key::End), app).unwrap(), b"\x1bOF");
        assert_eq!(sends(KeyPress { shift: true, ..press(Key::Up) }), b"\x1b[1;2A");
        assert_eq!(sends(KeyPress { ctrl: true, ..press(Key::Right) }), b"\x1b[1;5C");
        assert_eq!(sends(KeyPress { shift: true, ctrl: true, ..press(Key::Down) }), b"\x1b[1;6B");
        assert_eq!(encode_key(&KeyPress { shift: true, ..press(Key::Up) }, app).unwrap(), b"\x1b[1;2A", "modifiers always use the CSI form");
    }

    #[test]
    fn function_and_editing_keys() {
        assert_eq!(sends(press(Key::F(1))), b"\x1bOP");
        assert_eq!(sends(press(Key::F(4))), b"\x1bOS");
        assert_eq!(sends(press(Key::F(5))), b"\x1b[15~");
        assert_eq!(sends(press(Key::F(10))), b"\x1b[21~");
        assert_eq!(sends(press(Key::F(12))), b"\x1b[24~");
        assert_eq!(sends(KeyPress { shift: true, ..press(Key::F(2)) }), b"\x1b[1;2Q");
        assert_eq!(sends(KeyPress { ctrl: true, ..press(Key::F(5)) }), b"\x1b[15;5~");
        assert!(encode_key(&press(Key::F(30)), TermMode::empty()).is_none());
        assert_eq!(sends(press(Key::Delete)), b"\x1b[3~");
        assert_eq!(sends(press(Key::PageUp)), b"\x1b[5~");
        assert_eq!(sends(press(Key::PageDown)), b"\x1b[6~");
        assert_eq!(sends(press(Key::Backspace)), [0x7f]);
        assert_eq!(sends(KeyPress { ctrl: true, ..press(Key::Backspace) }), [0x08]);
        assert_eq!(sends(press(Key::Tab)), b"\t");
        assert_eq!(sends(KeyPress { shift: true, ..press(Key::Tab) }), b"\x1b[Z");
        assert_eq!(sends(press(Key::Enter)), b"\r");
        assert_eq!(encode_key(&press(Key::Enter), TermMode::LINE_FEED_NEW_LINE).unwrap(), b"\r\n");
        assert_eq!(sends(press(Key::Escape)), [0x1b]);
    }

    #[test]
    fn command_chords_stay_with_the_app() {
        assert!(encode_key(&KeyPress { cmd: true, ..press(Key::Char('c')) }, TermMode::empty()).is_none());
        assert!(encode_key(&KeyPress { cmd: true, ..press(Key::Left) }, TermMode::empty()).is_none());
    }

    #[test]
    fn paste_normalises_lines_and_cannot_escape_its_bracket() {
        assert_eq!(encode_paste("a\nb\r\nc", TermMode::empty()), b"a\rb\rc");
        assert_eq!(encode_paste("a\nb", TermMode::BRACKETED_PASTE), b"\x1b[200~a\rb\x1b[201~");
        assert_eq!(encode_paste("x\x1b[201~rm -rf", TermMode::BRACKETED_PASTE), b"\x1b[200~x[201~rm -rf\x1b[201~");
    }

    #[test]
    fn wheel_arrows_follow_the_cursor_mode() {
        assert_eq!(wheel_arrows(true, 2, TermMode::empty()), b"\x1b[A\x1b[A");
        assert_eq!(wheel_arrows(false, 1, TermMode::APP_CURSOR), b"\x1bOB");
    }

    #[test]
    fn a_shell_s_cursor_walks_along_the_wrap_and_a_full_screen_app_s_goes_straight_there() {
        let shell = TermMode::empty();
        assert_eq!(cursor_moves((5, 2), (2, 2), 80, shell), b"\x1b[D\x1b[D\x1b[D");
        assert_eq!(cursor_moves((2, 2), (4, 2), 80, shell), b"\x1b[C\x1b[C");
        // One line down in a ten-column grid: seven to the end of the line, one around the wrap, one in.
        assert_eq!(cursor_moves((2, 0), (1, 1), 10, shell), b"\x1b[C".repeat(9));
        // And back up, the same walk the other way.
        assert_eq!(cursor_moves((1, 1), (2, 0), 10, shell), b"\x1b[D".repeat(9));
        // Two lines takes a whole line more.
        assert_eq!(cursor_moves((2, 0), (1, 2), 10, shell), b"\x1b[C".repeat(19));
        let app = TermMode::ALT_SCREEN | TermMode::APP_CURSOR;
        assert_eq!(cursor_moves((1, 3), (3, 2), 10, app), b"\x1bOA\x1bOC\x1bOC");
    }

    #[test]
    fn mouse_reports_follow_the_encoding_the_program_asked_for() {
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(mouse_report(MouseAction::Press(0), 2, 4, sgr), b"\x1b[<0;3;5M");
        assert_eq!(mouse_report(MouseAction::Release(0), 2, 4, sgr), b"\x1b[<0;3;5m");
        assert_eq!(mouse_report(MouseAction::Drag(0), 0, 0, sgr), b"\x1b[<32;1;1M");
        assert_eq!(mouse_report(MouseAction::Wheel(true), 0, 0, sgr), b"\x1b[<64;1;1M");
        assert_eq!(mouse_report(MouseAction::Wheel(false), 0, 0, sgr), b"\x1b[<65;1;1M");
        let legacy = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(mouse_report(MouseAction::Press(0), 2, 4, legacy), [0x1b, b'[', b'M', 32, 35, 37]);
        assert_eq!(mouse_report(MouseAction::Release(0), 2, 4, legacy), [0x1b, b'[', b'M', 35, 35, 37]);
    }

    #[test]
    fn typed_text_drops_control_characters_and_chord_letters() {
        let mut input = Input { text: "ls\r".into(), ..Input::default() };
        assert_eq!(typed_text(&input), "ls");
        input.text = "c".into();
        input.keys.push(KeyPress { ctrl: true, ..press(Key::Char('c')) });
        assert_eq!(typed_text(&input), "");
        input.text = "世界".into();
        assert_eq!(typed_text(&input), "世界");
    }
}
