//! The device-shell commands that inject input, built as strings.
//!
//! Everything here ends up in `sh -c` on the device, so the one thing that
//! carries arbitrary text — `input text` — is built to be a single
//! single-quoted shell word, whatever the text holds. The numbers are
//! formatted from integers and cannot carry anything else.
//!
//! `input text` has a quirk of its own on top of the shell's: it turns the two
//! characters `%s` into a space (spaces cannot be passed through the shell's
//! word splitting otherwise) and has no way to type a literal `%s`. A real
//! percent sign followed by a real `s` is therefore sent as two `input text`
//! calls, `%` and `s...`, so that the pair never meets.

use crate::remote_control::driver::{DriverError, DriverResult};

/// Longest text one `type` action may carry.
pub(super) const MAX_TEXT_CHARS: usize = 5000;
/// Characters of text in one `input text` call. Keeps a call far below any
/// command-line limit (a quote costs four bytes once escaped).
const SEGMENT_CHARS: usize = 400;
/// Longest shell command (several `input` calls joined by `; `) sent at once.
const MAX_COMMAND_BYTES: usize = 4096;
/// `KEYCODE_ENTER`: what a newline in typed text becomes.
const KEYCODE_ENTER: u32 = 66;

pub(super) fn tap_command(x: u32, y: u32, count: u8) -> String {
    let tap = format!("input tap {x} {y}");
    // Several taps go in one shell command: each `input` call can cost a
    // process start (hundreds of milliseconds before Android 12), which would
    // push two taps outside the double-tap window. Still best-effort.
    vec![tap; usize::from(count.clamp(1, 3))].join("; ")
}

/// A press that stays where it is for `ms` is a swipe from a point to itself.
pub(super) fn long_press_command(x: u32, y: u32, ms: u32) -> String {
    swipe_command((x, y), (x, y), ms)
}

pub(super) fn swipe_command(from: (u32, u32), to: (u32, u32), ms: u32) -> String {
    format!("input swipe {} {} {} {} {ms}", from.0, from.1, to.0, to.1)
}

pub(super) fn keyevent_command(code: u32) -> String {
    format!("input keyevent {code}")
}

pub(super) fn keycombination_command(codes: &[u32]) -> String {
    let mut command = "input keycombination".to_owned();
    for code in codes {
        command.push(' ');
        command.push_str(&code.to_string());
    }
    command
}

/// `value` as one word of a POSIX shell: in single quotes, with each single
/// quote closed, escaped and reopened. Nothing inside single quotes is special
/// to the shell, so this is the one quoting that has no exceptions. The Android
/// tools (`remote_control::android`) quote every value they put in a command
/// with it.
pub(crate) fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn describe(character: char) -> String {
    match character {
        '\t' => "a tab (press it with the key action \"Tab\")".to_owned(),
        '\r' => "a lone carriage return".to_owned(),
        control if control.is_control() => {
            format!("the control character U+{:04X}", u32::from(control))
        }
        other => format!("\"{other}\" (U+{:04X})", u32::from(other)),
    }
}

/// The shell commands that type `text`: one or more, each a `; `-joined run of
/// `input` calls, in order. A newline is the Enter key.
pub(super) fn text_commands(text: &str) -> DriverResult<Vec<String>> {
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(DriverError::Failed(format!(
            "The text is longer than {MAX_TEXT_CHARS} characters; type it in smaller pieces."
        )));
    }
    let text = text.replace("\r\n", "\n");
    if let Some(bad) = text
        .chars()
        .find(|character| *character != '\n' && !(' '..='~').contains(character))
    {
        return Err(DriverError::Unsupported(format!(
            "adb can only type ASCII text (letters, digits and punctuation, with a newline for Enter), and the text contains {}; use a different input method or paste it into the field some other way.",
            describe(bad)
        )));
    }

    let mut calls: Vec<String> = Vec::new();
    // What `input text` receives for the segment being built: a space already
    // written as `%s`.
    let mut segment = String::new();
    let mut segment_chars = 0usize;
    let flush = |segment: &mut String, segment_chars: &mut usize, calls: &mut Vec<String>| {
        if !segment.is_empty() {
            calls.push(format!("input text {}", shell_quote(segment)));
            segment.clear();
        }
        *segment_chars = 0;
    };
    for character in text.chars() {
        if character == '\n' {
            flush(&mut segment, &mut segment_chars, &mut calls);
            calls.push(keyevent_command(KEYCODE_ENTER));
            continue;
        }
        // A real '%' directly before a real 's' would be read as a space.
        if character == 's' && segment.ends_with('%') {
            flush(&mut segment, &mut segment_chars, &mut calls);
        }
        if segment_chars >= SEGMENT_CHARS {
            flush(&mut segment, &mut segment_chars, &mut calls);
        }
        if character == ' ' {
            segment.push_str("%s");
        } else {
            segment.push(character);
        }
        segment_chars += 1;
    }
    flush(&mut segment, &mut segment_chars, &mut calls);

    let mut commands: Vec<String> = Vec::new();
    for call in calls {
        match commands.last_mut() {
            Some(last) if last.len() + 2 + call.len() <= MAX_COMMAND_BYTES => {
                last.push_str("; ");
                last.push_str(&call);
            }
            _ => commands.push(call),
        }
    }
    Ok(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gestures_are_plain_integers() {
        assert_eq!(tap_command(10, 20, 1), "input tap 10 20");
        assert_eq!(tap_command(10, 20, 0), "input tap 10 20");
        assert_eq!(tap_command(10, 20, 2), "input tap 10 20; input tap 10 20");
        assert_eq!(
            tap_command(1, 2, 9),
            "input tap 1 2; input tap 1 2; input tap 1 2"
        );
        assert_eq!(long_press_command(5, 6, 800), "input swipe 5 6 5 6 800");
        assert_eq!(
            swipe_command((1, 2), (3, 4), 300),
            "input swipe 1 2 3 4 300"
        );
        assert_eq!(keyevent_command(66), "input keyevent 66");
        assert_eq!(
            keycombination_command(&[113, 31]),
            "input keycombination 113 31"
        );
    }

    #[test]
    fn a_shell_word_survives_every_metacharacter() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("''"), r"''\'''\'''");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(id)`id`;|&<>\"\\"), "'$(id)`id`;|&<>\"\\'");
    }

    fn one(text: &str) -> String {
        let commands = text_commands(text).unwrap();
        assert_eq!(commands.len(), 1, "{commands:?}");
        commands.into_iter().next().unwrap()
    }

    #[test]
    fn text_is_one_quoted_word_with_spaces_as_percent_s() {
        assert_eq!(one("hello"), "input text 'hello'");
        assert_eq!(one("a b"), "input text 'a%sb'");
        assert_eq!(one("  x"), "input text '%s%sx'");
        assert!(text_commands("").unwrap().is_empty());
    }

    #[test]
    fn shell_metacharacters_stay_inside_the_quotes() {
        assert_eq!(
            one("$(reboot); rm -rf / | cat > x & `id`"),
            "input text '$(reboot);%srm%s-rf%s/%s|%scat%s>%sx%s&%s`id`'"
        );
        assert_eq!(one("it's \"quoted\""), r#"input text 'it'\''s%s"quoted"'"#);
        assert_eq!(
            one(r"back\slash <in> out"),
            r"input text 'back\slash%s<in>%sout'"
        );
        assert_eq!(one("'; reboot; '"), r"input text ''\'';%sreboot;%s'\'''");
        assert_eq!(one("~ * ? # ! { }"), "input text '~%s*%s?%s#%s!%s{%s}'");
    }

    #[test]
    fn a_literal_percent_s_is_split_so_the_pair_never_meets() {
        assert_eq!(
            text_commands("100%s").unwrap(),
            vec!["input text '100%'; input text 's'".to_owned()]
        );
        assert_eq!(one("%%s"), "input text '%%'; input text 's'",);
        // A percent before a space is "%" and "%s" (a space): no split needed.
        assert_eq!(one("50% sure"), "input text '50%%ssure'");
        assert_eq!(one("%"), "input text '%'");
        assert_eq!(one("a%b"), "input text 'a%b'");
        assert_eq!(one("%S"), "input text '%S'");
    }

    #[test]
    fn a_newline_is_the_enter_key_between_segments() {
        assert_eq!(
            one("a\nb"),
            "input text 'a'; input keyevent 66; input text 'b'"
        );
        assert_eq!(one("\nab"), "input keyevent 66; input text 'ab'");
        assert_eq!(one("ab\n"), "input text 'ab'; input keyevent 66");
        assert_eq!(one("\n\n"), "input keyevent 66; input keyevent 66");
        assert_eq!(one("a\r\nb"), one("a\nb"));
    }

    #[test]
    fn anything_but_printable_ascii_is_refused_with_a_way_out() {
        for text in [
            "caf\u{e9}",
            "\u{4e2d}\u{6587}",
            "tab\there",
            "bell\u{7}",
            "lone\rcr",
            "\u{1f600}",
        ] {
            match text_commands(text) {
                Err(DriverError::Unsupported(message)) => {
                    assert!(
                        message.contains("adb can only type ASCII text"),
                        "{message}"
                    );
                    assert!(
                        message.contains("use a different input method or paste"),
                        "{message}"
                    );
                }
                other => panic!("{text:?}: {other:?}"),
            }
        }
        let Err(DriverError::Unsupported(tab)) = text_commands("a\tb") else {
            panic!("tab");
        };
        assert!(tab.contains("\"Tab\""), "{tab}");
    }

    #[test]
    fn long_text_is_cut_into_bounded_commands_and_too_long_text_is_refused() {
        let long = "ab ".repeat(1500);
        let commands = text_commands(&long).unwrap();
        assert!(commands.len() > 1);
        for command in &commands {
            assert!(command.len() <= MAX_COMMAND_BYTES, "{}", command.len());
            assert!(command.starts_with("input text '"));
        }
        let too_long = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(matches!(
            text_commands(&too_long),
            Err(DriverError::Failed(_))
        ));
        assert!(text_commands(&"x".repeat(MAX_TEXT_CHARS)).is_ok());
    }

    /// Reads a command line the way `sh` would, for the subset the driver
    /// writes: simple commands separated by `;`, words made of single-quoted
    /// runs, `\'` and bare characters. Anything a shell would treat specially
    /// outside quotes is an error, which is the property under test.
    fn shell_words(command: &str) -> Vec<Vec<String>> {
        let mut commands = vec![Vec::new()];
        let mut word: Option<String> = None;
        let mut characters = command.chars().peekable();
        while let Some(character) = characters.next() {
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
                bare if bare.is_ascii_alphanumeric() => {
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

    /// What `input text` does with its argument (`InputShellCommand.sendText`):
    /// "%s" becomes a space.
    fn typed_by_input_text(argument: &str) -> String {
        let mut characters: Vec<char> = argument.chars().collect();
        let mut escape = false;
        let mut index = 0;
        while index < characters.len() {
            if escape {
                escape = false;
                if characters[index] == 's' {
                    characters[index] = ' ';
                    index -= 1;
                    characters.remove(index);
                }
            }
            if characters[index] == '%' {
                escape = true;
            }
            index += 1;
        }
        characters.into_iter().collect()
    }

    /// Replays commands as the device would and returns what was typed.
    fn replay(commands: &[String]) -> String {
        let mut typed = String::new();
        for command in commands {
            for simple in shell_words(command) {
                match simple
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice()
                {
                    ["input", "text", argument] => typed.push_str(&typed_by_input_text(argument)),
                    ["input", "keyevent", "66"] => typed.push('\n'),
                    other => panic!("unexpected command {other:?}"),
                }
            }
        }
        typed
    }

    #[test]
    fn typing_round_trips_through_a_shell_and_input_text() {
        let mut corpus: Vec<String> = vec![
            "100%s".into(),
            "%s%s%s".into(),
            "%%s".into(),
            "a%".into(),
            "% s".into(),
            "50% sure %s %%s %".into(),
            "'; reboot; echo '".into(),
            "line1\nline2\n\nline4 with 'quotes' and \"doubles\"".into(),
            ('\u{20}'..='\u{7e}').collect(),
            ('\u{20}'..='\u{7e}').rev().collect(),
            "%s".repeat(500),
            "%".repeat(700) + &"s".repeat(700),
        ];
        // Every pair of printable characters, which is where the "%s" trap is.
        for first in ' '..='~' {
            for second in ' '..='~' {
                corpus.push(format!("x{first}{second}y"));
            }
        }
        for text in corpus {
            let commands = text_commands(&text).unwrap();
            for command in &commands {
                assert!(command.len() <= MAX_COMMAND_BYTES);
            }
            assert_eq!(replay(&commands), text, "{commands:?}");
        }
    }
}
