//! The real thing: `/bin/sh` in a real PTY, driven through `show` the way the app drives it. `Ui` needs no GPU.
//!
//! The terminal itself knows nothing of PTYs any more, so these tests stand where the host stands: they read the PTY
//! and call `feed`, and they carry out what the sink says, which is the whole of the bridge. The host's own side of it
//! (spawning, signals, exit codes) has its tests in `mewrk-host`.

use std::fs::File;
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use alacritty_terminal::tty;
use fluent::ui::{Clipboard, Input, KeyPress, UiMemory};
use fluent::{rect, Scene, TextCtx, Theme};

use super::*;

const FRAME: f32 = 1.0 / 120.0;
const ID: u64 = 7;
const AREA: Rect = rect(0.0, 0.0, 800.0, 400.0);

#[derive(Default)]
struct Recorder(Option<String>);

impl Clipboard for Recorder {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

struct Outcome {
    animating: bool,
    wake_in: Option<f32>,
    ime_area: Option<Rect>,
}

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clip: Recorder,
    theme: Theme,
    area: Rect,
    time: f32,
}

impl Rig {
    fn new() -> Self {
        Self { scene: Scene::default(), text: TextCtx::new(2.0), mem: UiMemory::default(), input: Input::default(), clip: Recorder::default(), theme: Theme::dark(), area: AREA, time: 1.0 }
    }

    fn frame(&mut self, term: &mut Terminal) -> Outcome {
        self.scene.clear();
        self.time += FRAME;
        let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut self.clip, 2.0, (1000.0, 800.0), FRAME, self.time);
        term.show(&mut ui, ID, self.area);
        let out = Outcome { animating: ui.animating, wake_in: ui.wake_in, ime_area: ui.ime_area };
        self.input.end_frame();
        out
    }

    fn cell_width(&mut self) -> f32 {
        self.text.line("0", &FONT, Color::WHITE).width / 2.0
    }

    /// The centre of a cell, for the pointer.
    fn at(&mut self, line: usize, col: usize) -> (f32, f32) {
        (AREA.x + PAD_X + (col as f32 + 0.5) * self.cell_width(), AREA.y + PAD_Y + (line as f32 + 0.5) * FONT.line)
    }

    fn press_at(&mut self, term: &mut Terminal, p: (f32, f32), double: bool) {
        self.input.pointer = Some(p);
        self.input.down = true;
        self.input.pressed = true;
        self.input.double_click = double;
        self.frame(term);
    }

    fn release(&mut self, term: &mut Terminal) {
        self.input.down = false;
        self.input.released = true;
        self.frame(term);
    }

    /// Takes focus by clicking at the bottom-right cell, leaving no selection behind.
    fn focus(&mut self, term: &mut Terminal) {
        let (lines, cols) = (term.view.lines, term.view.cols);
        let p = self.at(lines - 1, cols - 1);
        self.press_at(term, p, false);
        self.release(term);
        assert_eq!(self.mem.focus, ID);
    }

    fn key(&mut self, term: &mut Terminal, press: KeyPress) {
        self.input.keys.push(press);
        self.frame(term);
    }
}

fn plain(key: Key) -> KeyPress {
    KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false }
}

fn cmd(c: char) -> KeyPress {
    KeyPress { cmd: true, ..plain(Key::Char(c)) }
}

/// A terminal with a shell on the far end, the way the host gives it one.
struct Sh {
    inner: Terminal,
    master: File,
    pid: libc::pid_t,
    _pty: tty::Pty,
}

impl Deref for Sh {
    type Target = Terminal;
    fn deref(&self) -> &Terminal {
        &self.inner
    }
}

impl DerefMut for Sh {
    fn deref_mut(&mut self) -> &mut Terminal {
        &mut self.inner
    }
}

impl Sh {
    /// A foreground process other than the shell itself is running.
    fn is_busy(&self) -> bool {
        // SAFETY: reads the terminal's foreground group through a descriptor this test owns.
        let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        group > 0 && group != self.pid
    }
}

impl Drop for Sh {
    fn drop(&mut self) {
        // SAFETY: the shell is this test's child, unreaped until `_pty` drops.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
    }
}

fn set_winsize(master: &File, cols: u16, lines: u16) {
    let ws = libc::winsize { ws_row: lines, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: `ws` outlives the call.
    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
}

fn spawn_sh() -> (Sh, Arc<AtomicUsize>) {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let env = [("TERM", "xterm-256color"), ("PS1", "$ "), ("LANG", "en_US.UTF-8")].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    let options = tty::Options { shell: Some(tty::Shell::new("/bin/sh".to_owned(), Vec::new())), working_directory: Some(std::env::temp_dir()), drain_on_exit: true, env };
    let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 17 };
    let pty = tty::new(&options, size, 0).unwrap();
    let pid = pty.child().id() as libc::pid_t;
    let master = pty.file().try_clone().unwrap();
    let (writer, resizer) = (master.try_clone().unwrap(), master.try_clone().unwrap());
    let sink: Sink = Arc::new(move |output| match output {
        Output::Input(text) | Output::Reply(text) => {
            let mut bytes = text.as_bytes();
            // alacritty leaves the descriptor non-blocking, and `cat -v` can be behind.
            while !bytes.is_empty() {
                match (&writer).write(bytes) {
                    Ok(n) => bytes = &bytes[n..],
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
                    Err(_) => break,
                }
            }
        }
        Output::Resize { cols, lines } => set_winsize(&resizer, cols, lines),
    });
    let term = Terminal::new(Options::default(), sink, wake);
    let feeder = term.tx.clone().unwrap();
    let mut reader = master.try_clone().unwrap();
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    if feeder.send(Work::Bytes(chunk[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
                Err(_) => return,
            }
        }
    });
    (Sh { inner: term, master, pid, _pty: pty }, wakes)
}

/// Types a command. Pasting it would not do: zsh takes a bracketed paste as text to edit, Enter included.
fn run(term: &mut Terminal, line: &str) {
    term.input(format!("{line}\r").into_bytes());
}

/// What the viewport shows, one string per line without trailing blanks.
fn screen(term: &Terminal) -> Vec<String> {
    let term = term.term.lock();
    let grid = term.grid();
    let offset = grid.display_offset() as i32;
    (0..grid.screen_lines() as i32)
        .map(|i| {
            let row = &grid[Line(i - offset)][..];
            let text: String = row.iter().filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER)).map(|c| c.c).collect();
            text.trim_end().to_owned()
        })
        .collect()
}

fn shows(term: &Terminal, line: &str) -> bool {
    screen(term).iter().any(|l| l == line)
}

#[track_caller]
fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[track_caller]
fn wait_for_line(term: &Terminal, line: &str) {
    wait(&format!("{line:?} on screen"), || shows(term, line));
}

/// Runs `line`, whose output includes the marker line `done`, and waits for that. A line wide enough to wrap can
/// echo the marker on a line of its own, so the long ones spell it apart.
#[track_caller]
fn run_to(term: &mut Terminal, line: &str, done: &str) {
    run(term, line);
    wait_for_line(term, done);
}

#[test]
fn echo_reaches_the_grid() {
    let (mut term, wakes) = spawn_sh();
    // Spelled apart so that the echoed command line cannot pass for the output.
    run_to(&mut term, "printf 'he%s\\n' llo", "hello");
    assert!(wakes.load(Ordering::Relaxed) >= 1);
}

#[test]
fn resize_reaches_the_pty() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    let (cols, lines) = (term.view.cols, term.view.lines);
    assert_eq!(lines, ((AREA.h - PAD_Y * 2.0) / FONT.line) as usize);
    assert!(cols > 80, "800 px of 12 px mono is about a hundred columns, got {cols}");
    run_to(&mut term, "stty size", &format!("{lines} {cols}"));

    rig.area = rect(0.0, 0.0, 300.0, 120.0);
    for _ in 0..((RESIZE_EVERY / FRAME) as usize + 2) {
        rig.frame(&mut term);
    }
    let (cols, lines) = (term.view.cols, term.view.lines);
    assert!(cols < 60 && lines < 10);
    run_to(&mut term, "stty size", &format!("{lines} {cols}"));
    assert!(rig.scene.counts().1 > 0);

    // Collapsing the pane must not rewrap anything.
    rig.area = rect(0.0, 0.0, 0.0, 0.0);
    for _ in 0..((RESIZE_EVERY / FRAME) as usize + 2) {
        rig.frame(&mut term);
    }
    assert_eq!((term.view.cols, term.view.lines), (cols, lines));
}

#[test]
fn resizing_is_throttled_during_a_drag() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    let first = term.view.cols;
    rig.time += RESIZE_EVERY;
    rig.area.w -= 40.0;
    let out = rig.frame(&mut term);
    assert!(term.view.cols < first, "the first change applies at once");
    let second = term.view.cols;
    assert!(out.wake_in.is_none());
    rig.area.w -= 40.0;
    let out = rig.frame(&mut term);
    assert_eq!(term.view.cols, second, "the next waits its turn");
    assert!(out.wake_in.is_some_and(|w| w <= RESIZE_EVERY), "and asks for the frame it needs");
}

#[test]
fn is_busy_tracks_the_foreground_job_and_ctrl_c_ends_it() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    run_to(&mut term, "echo idle", "idle");
    wait("the shell to be idle", || !term.is_busy());
    run(&mut term, "sleep 60");
    wait("sleep to take the foreground", || term.is_busy());
    rig.frame(&mut term);
    rig.focus(&mut term);
    rig.key(&mut term, KeyPress { ctrl: true, ..plain(Key::Char('c')) });
    wait("sleep to end", || !term.is_busy());
}

#[test]
fn title_follows_the_shell() {
    let (mut term, _) = spawn_sh();
    assert_eq!(term.title(), None);
    run_to(&mut term, "printf '\\033]0;build log\\007'; echo set", "set");
    wait("the title", || term.title().as_deref() == Some("build log"));
}

#[test]
fn typing_reaches_the_shell_only_when_focused() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "echo ready", "ready");

    rig.input.text = "echo stray".into();
    rig.input.keys.push(plain(Key::Enter));
    rig.frame(&mut term);

    rig.focus(&mut term);
    rig.input.text = "echo ty".into();
    rig.input.ime_commit = Some("世界".into());
    rig.input.keys.push(plain(Key::Enter));
    rig.frame(&mut term);
    wait_for_line(&term, "ty世界");
    assert!(!shows(&term, "stray"), "nothing was typed while unfocused");
}

#[test]
fn pressing_outside_gives_up_focus() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    rig.focus(&mut term);
    rig.press_at(&mut term, (900.0, 700.0), false);
    rig.release(&mut term);
    assert_eq!(rig.mem.focus, 0);
}

#[test]
fn preedit_shows_at_the_cursor_and_holds_typed_text() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "echo ready", "ready");
    rig.focus(&mut term);
    assert!(rig.frame(&mut term).ime_area.is_some(), "the candidate window needs an anchor");
    assert!(rig.scene.layers[LAYER_POPUP].glyphs.is_empty());

    rig.input.ime_preedit = Some("ni".into());
    let out = rig.frame(&mut term);
    assert!(!rig.scene.layers[LAYER_POPUP].glyphs.is_empty());
    let (cx, cy) = (term.view.cursor.point.column.0 as f32, term.view.cursor.point.line.0 as f32);
    let ime = out.ime_area.unwrap();
    assert_eq!((ime.x, ime.y), (AREA.x + PAD_X + cx * rig.cell_width(), AREA.y + PAD_Y + cy * FONT.line));

    // While composing, stray text belongs to the IME.
    rig.input.text = "x".into();
    rig.frame(&mut term);
    rig.input.ime_preedit = None;
    rig.input.keys.push(plain(Key::Enter));
    rig.frame(&mut term);
    thread::sleep(Duration::from_millis(100));
    assert!(!screen(&term).iter().any(|l| l.ends_with('x')));
}

fn prepare_words(term: &mut Terminal, rig: &mut Rig) {
    rig.frame(term);
    run_to(term, "printf '\\033[2J\\033[Hfoo bar baz\\n'; sleep 60", "foo bar baz");
    wait("the row at the top", || screen(term)[0] == "foo bar baz");
    rig.frame(term);
}

#[test]
fn drag_selects_and_cmd_c_copies() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    prepare_words(&mut term, &mut rig);

    let start = rig.at(0, 4);
    rig.press_at(&mut term, (start.0 - 3.0, start.1), false);
    let end = rig.at(0, 6);
    rig.input.pointer = Some((end.0 + 3.0, end.1));
    let before = rig.scene.counts().0;
    rig.frame(&mut term);
    rig.release(&mut term);
    assert!(rig.scene.counts().0 > before, "the selection is drawn");
    rig.key(&mut term, cmd('c'));
    assert_eq!(rig.clip.0.as_deref(), Some("bar"));

    // A plain click drops it.
    let p = rig.at(0, 0);
    rig.press_at(&mut term, p, false);
    rig.release(&mut term);
    assert!(term.view.selection.is_none());
}

#[test]
fn double_click_selects_a_word() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    prepare_words(&mut term, &mut rig);
    let p = rig.at(0, 9);
    rig.press_at(&mut term, p, true);
    rig.release(&mut term);
    rig.key(&mut term, cmd('c'));
    assert_eq!(rig.clip.0.as_deref(), Some("baz"));
}

#[test]
fn a_third_click_takes_the_line_and_shift_moves_the_far_end() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    prepare_words(&mut term, &mut rig);
    let line = screen(&term)[0].clone();
    let p = rig.at(0, 1);
    rig.press_at(&mut term, p, false);
    rig.release(&mut term);
    rig.press_at(&mut term, p, true);
    rig.release(&mut term);
    rig.press_at(&mut term, p, true);
    rig.release(&mut term);
    rig.key(&mut term, cmd('c'));
    assert_eq!(rig.clip.0.as_deref().map(str::trim_end), Some(line.trim_end()), "the whole line");

    let (from, to) = (rig.at(0, 0), rig.at(0, 6));
    rig.press_at(&mut term, from, false);
    rig.release(&mut term);
    rig.input.shift = true;
    rig.press_at(&mut term, to, false);
    rig.release(&mut term);
    rig.input.shift = false;
    rig.key(&mut term, cmd('c'));
    // A press in a cell's middle starts past that cell and ends through the other one, as a drag between them does.
    assert_eq!(rig.clip.0.as_deref(), Some(&line[1..7]), "from the first press to the shifted one");
}

#[test]
fn a_double_click_takes_a_path_whole() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    term.feed("see /usr/local/bin:8080 now\r\n");
    term.flush();
    rig.frame(&mut term);
    let p = rig.at(0, 8);
    rig.press_at(&mut term, p, true);
    rig.release(&mut term);
    rig.key(&mut term, cmd('c'));
    assert_eq!(rig.clip.0.as_deref(), Some("/usr/local/bin:8080"), "what xterm calls a word");
}

#[test]
fn a_right_click_takes_the_word_it_landed_on_and_asks_for_the_menu() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    prepare_words(&mut term, &mut rig);
    let p = rig.at(0, 9);
    rig.input.pointer = Some(p);
    rig.input.right_pressed = true;
    rig.frame(&mut term);
    assert_eq!(rig.mem.focus, ID, "the menu's Paste has somewhere to go");
    assert_eq!(term.take_context_menu(), Some(p));
    assert_eq!(term.take_context_menu(), None, "once per click");
    assert_eq!(term.selection_text().as_deref(), Some("baz"));

    // On a selection it keeps that selection.
    let (from, to) = (rig.at(0, 4), rig.at(0, 10));
    rig.press_at(&mut term, from, false);
    rig.input.pointer = Some(to);
    rig.frame(&mut term);
    rig.release(&mut term);
    let kept = term.selection_text();
    rig.input.pointer = Some(rig.at(0, 9));
    rig.input.right_pressed = true;
    rig.frame(&mut term);
    assert_eq!(term.selection_text(), kept);
}

#[test]
fn an_option_click_walks_the_shells_cursor_there_with_arrow_keys() {
    let sent: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = {
        let sent = sent.clone();
        Arc::new(move |output: Output| {
            if let Output::Input(data) = output {
                sent.lock().push(data);
            }
        })
    };
    let mut term = Terminal::new(Options::default(), sink, Arc::new(|| {}));
    let mut rig = Rig::new();
    term.feed("$ echo hello");
    term.flush();
    rig.frame(&mut term);
    let p = rig.at(0, 4);
    rig.input.alt = true;
    rig.press_at(&mut term, p, false);
    rig.release(&mut term);
    rig.input.alt = false;
    assert_eq!(sent.lock().concat(), "\x1b[D".repeat(8), "from the end of `$ echo hello` back to the fifth column");
    assert!(!term.has_selection());
}

#[test]
fn cmd_v_pastes_with_brackets_when_the_program_asks() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run(&mut term, "printf '\\033[?2004h'; cat -v");
    wait("bracketed paste mode", || term.term.lock().mode().contains(TermMode::BRACKETED_PASTE));
    rig.focus(&mut term);
    rig.clip.0 = Some("one\ntwo".into());
    rig.key(&mut term, cmd('v'));
    wait("the bracket to echo", || screen(&term).iter().any(|l| l.contains("^[[200~one")));
    wait("the closing bracket", || screen(&term).iter().any(|l| l.contains("two^[[201~")));
}

#[test]
fn cmd_k_clears_screen_and_history_but_keeps_the_prompt_line() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "seq 1 200; printf 'keep'; sleep 60", "200");
    wait("the cursor line", || shows(&term, "keep"));
    rig.frame(&mut term);
    assert!(term.view.history > 0);
    rig.focus(&mut term);
    rig.key(&mut term, cmd('k'));
    assert_eq!(term.view.history, 0);
    assert_eq!(term.view.offset, 0);
    let lines = screen(&term);
    assert_eq!(lines[0], "keep");
    assert!(lines[1..].iter().all(String::is_empty));
    assert_eq!(term.view.cursor.point.line, Line(0));
}

/// `seq` output scrolled through a small screen, with the view at the bottom.
fn with_history(term: &mut Sh, rig: &mut Rig) {
    rig.frame(term);
    run_to(term, "seq 1 300", "300");
    wait("the prompt", || !term.is_busy());
    thread::sleep(Duration::from_millis(50));
    rig.frame(term);
    assert!(term.view.history > 100);
    assert_eq!(term.view.offset, 0);
}

#[test]
fn shift_page_scrolls_and_typing_comes_back() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    with_history(&mut term, &mut rig);
    rig.focus(&mut term);
    let page = term.view.lines;
    let up = KeyPress { shift: true, ..plain(Key::PageUp) };
    rig.key(&mut term, up);
    assert_eq!(term.view.offset, page);
    rig.key(&mut term, up);
    assert_eq!(term.view.offset, page * 2);
    rig.key(&mut term, KeyPress { shift: true, ..plain(Key::PageDown) });
    assert_eq!(term.view.offset, page);

    rig.input.text = "#".into();
    rig.frame(&mut term);
    assert_eq!(term.view.offset, 0, "typing returns to the prompt");
}

#[test]
fn wheel_moves_through_history_and_back() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    with_history(&mut term, &mut rig);
    rig.input.pointer = Some((100.0, 100.0));
    rig.input.scroll = FONT.line * 10.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut term);
    assert_eq!(term.view.offset, 10);

    // A notch eases there over a few frames, and those frames stop by themselves.
    rig.input.scroll = 48.0;
    rig.input.scroll_is_pixels = false;
    let mut frames = 0;
    while rig.frame(&mut term).animating {
        frames += 1;
        assert!(frames < 240, "the easing has to settle");
    }
    assert_eq!(term.view.offset, 13);

    rig.input.scroll = -FONT.line * 100.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut term);
    assert_eq!(term.view.offset, 0);
    assert!(term.scroll.stick, "the bottom holds again");
}

#[test]
fn the_view_stays_put_while_output_arrives() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    with_history(&mut term, &mut rig);
    // Typing would bring the view back down, so the output has to come from a command already running.
    run(&mut term, "sleep 1; seq 301 400");
    wait("the job to start", || term.is_busy());
    thread::sleep(Duration::from_millis(100));
    rig.frame(&mut term);
    rig.focus(&mut term);
    rig.key(&mut term, KeyPress { shift: true, ..plain(Key::PageUp) });
    let first = screen(&term);
    let history = term.view.history;
    assert!(term.view.offset > 0);

    wait("more history", || {
        rig.frame(&mut term);
        term.view.history >= history + 100
    });
    wait("the prompt", || !term.is_busy());
    thread::sleep(Duration::from_millis(50));
    rig.frame(&mut term);
    assert_eq!(screen(&term), first, "text on screen does not move under the reader");
    assert!(term.view.offset >= 100);
}

#[test]
fn a_burst_keeps_following_the_bottom() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run(&mut term, "seq 1 5000; printf 'fin%s\\n' ished");
    wait("the end of the burst", || {
        let out = rig.frame(&mut term);
        assert_eq!(term.view.offset, 0, "output must never leave the view behind");
        assert!(!out.animating, "and following it must not take animation frames");
        shows(&term, "finished")
    });
}

#[test]
fn wheel_in_a_full_screen_app_sends_arrows() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run(&mut term, "printf '\\033[?1049h'; cat -v");
    wait("the alternate screen", || term.term.lock().mode().contains(TermMode::ALT_SCREEN));
    rig.frame(&mut term);
    rig.input.pointer = Some((100.0, 100.0));
    rig.input.scroll = FONT.line + 1.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut term);
    assert_eq!(term.view.offset, 0);
    wait("the arrow", || screen(&term).iter().any(|l| l.contains("^[[A")));
    rig.input.scroll = -FONT.line * 2.0;
    rig.frame(&mut term);
    wait("the other arrow", || screen(&term).iter().any(|l| l.contains("^[[B")));
}

#[test]
fn app_cursor_mode_reaches_the_keys() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run(&mut term, "printf '\\033[?1h'; cat -v");
    wait("application cursor mode", || term.term.lock().mode().contains(TermMode::APP_CURSOR));
    rig.frame(&mut term);
    rig.focus(&mut term);
    rig.key(&mut term, plain(Key::Up));
    wait("the SS3 arrow", || screen(&term).iter().any(|l| l.contains("^[OA")));
}

#[test]
fn programs_get_answers_about_colour_and_cursor() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    // Asks for the background and reads the reply back through the line discipline.
    run(&mut term, "stty -echo -icanon; printf '\\033]11;?\\007'; dd bs=1 count=24 2>/dev/null | cat -v; stty sane; echo; printf 'fin%s\\n' ished");
    wait_for_line(&term, "finished");
    assert!(screen(&term).iter().any(|l| l.contains("rgb:2020/2020/2020")), "{:?}", screen(&term));
}

#[test]
fn program_output_cannot_write_the_clipboard() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "printf '\\033]52;c;cHduZWQ=\\007'; echo sent", "sent");
    rig.frame(&mut term);
    assert_eq!(rig.clip.0, None);
}

#[test]
fn an_idle_terminal_costs_nothing() {
    let (mut term, wakes) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "echo settled; sleep 60", "settled");
    wait("the job to start", || term.is_busy());
    thread::sleep(Duration::from_millis(100));
    rig.frame(&mut term);
    let before = wakes.load(Ordering::Relaxed);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(wakes.load(Ordering::Relaxed), before, "no output, no wakes");

    // Frames something else asks for find every run in the caches, and never wait on the emulator's lock.
    let stats = rig.text.stats;
    let (held_term, (held_tx, held_rx)) = (term.term.clone(), std::sync::mpsc::channel());
    let holder = thread::spawn(move || {
        let _guard = held_term.lock();
        held_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(300));
    });
    held_rx.recv().unwrap();
    let start = Instant::now();
    for _ in 0..3 {
        let out = rig.frame(&mut term);
        assert!(!out.animating);
        assert!(out.wake_in.is_none(), "unfocused, nothing is scheduled");
    }
    assert!(start.elapsed() < Duration::from_millis(250), "a steady frame waited on the lock");
    holder.join().unwrap();
    assert_eq!(rig.text.stats.shaped_words, stats.shaped_words);
    assert_eq!(rig.text.stats.rasterized, stats.rasterized);
}

#[test]
fn the_cursor_blinks_on_schedule_and_then_rests() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "echo settled; sleep 60", "settled");
    wait("the job to start", || term.is_busy());
    thread::sleep(Duration::from_millis(100));
    rig.frame(&mut term);
    rig.focus(&mut term);
    let wake = rig.frame(&mut term).wake_in.expect("a focused cursor blinks");
    assert!(wake > 0.0 && wake <= BLINK);
    // The cursor is the eared mark, and nothing else in a terminal is a coloured picture.
    let marks = |rig: &Rig| rig.scene.layers.iter().flat_map(|layer| &layer.glyphs).filter(|glyph| glyph.flags & fluent::gpu::GLYPH_COLOR != 0).count();
    assert_eq!(marks(&rig), 1);
    rig.time += BLINK;
    rig.frame(&mut term);
    assert_eq!(marks(&rig), 0, "half the time the block is gone");
    rig.time += BLINK_FOR;
    assert!(rig.frame(&mut term).wake_in.is_none(), "an unused cursor stops asking for frames");
    assert_eq!(marks(&rig), 1, "and rests lit");
}

#[test]
fn losing_focus_turns_the_block_into_an_outline() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "echo settled; sleep 60", "settled");
    wait("the job to start", || term.is_busy());
    thread::sleep(Duration::from_millis(100));
    rig.frame(&mut term);
    let picture = |rig: &Rig| rig.scene.layers[0].glyphs.iter().find(|glyph| glyph.flags & fluent::gpu::GLYPH_COLOR != 0).unwrap().uv;
    let outline = picture(&rig);
    rig.focus(&mut term);
    rig.frame(&mut term);
    assert_ne!(picture(&rig), outline, "the filled mark is another picture from its outline");
    assert_eq!(rig.scene.layers[0].rects.iter().filter(|rect| rect.fill == fluent::mark::BRAND_ORANGE).count(), 0, "and neither is a box");
}

#[test]
fn the_block_cursor_stands_level_with_the_text_and_leaves_a_descender_showing() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "clear; printf 'Hg\\033[D'; sleep 60", "Hg");
    wait("the job to start", || term.is_busy());
    thread::sleep(Duration::from_millis(100));
    rig.focus(&mut term);
    rig.frame(&mut term);
    let glyphs = &rig.scene.layers[0].glyphs;
    let mark = glyphs.iter().position(|glyph| glyph.flags & fluent::gpu::GLYPH_COLOR != 0).expect("a block cursor");
    let capital = glyphs.iter().find(|glyph| glyph.color == Theme::dark().text_primary).expect("the H");
    assert_eq!(glyphs[mark].pos[1], capital.pos[1], "the ears reach the top of a capital");
    assert_eq!(glyphs[mark].pos[1] + glyphs[mark].size[1] as f32, capital.pos[1] + capital.size[1] as f32, "and the mark stops at its foot");
    // The g in its own colour under the mark, and again over it in the cursor's, cut off at the baseline.
    let under = glyphs[..mark].iter().filter(|glyph| glyph.color == Theme::dark().text_primary).count();
    assert_eq!(under, 2, "the H and the g");
    let over = &glyphs[mark + 1..];
    assert_eq!(over.len(), 1);
    assert_eq!(over[0].color, Color::rgb(0x14110F));
    assert!(over[0].clip[3] < over[0].pos[1] as i16 + over[0].size[1] as i16, "the descender is left to the g underneath");
}

#[test]
fn theme_changes_recolour_the_screen() {
    let (mut term, _) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run_to(&mut term, "echo themed; sleep 60", "themed");
    wait("the job to start", || term.is_busy());
    thread::sleep(Duration::from_millis(100));
    rig.frame(&mut term);
    let text_is = |rig: &Rig, color| rig.scene.layers[0].glyphs.iter().filter(|g| g.flags & fluent::gpu::GLYPH_COLOR == 0).all(|g| g.color == color);
    assert!(text_is(&rig, Theme::dark().text_primary));
    rig.theme = Theme::light();
    rig.frame(&mut term);
    assert!(text_is(&rig, Theme::light().text_primary));
}

fn peak_rss() -> usize {
    // SAFETY: an all-zero rusage is valid for the call to fill in.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage.ru_maxrss as usize
    }
}

/// How many instances a frame may hold at most: one rect per cell and one glyph per cell, and a few for the chrome.
fn assert_frame_bounded(term: &Terminal, rig: &Rig) {
    let cells = term.view.cols * term.view.lines;
    let (rects, glyphs) = rig.scene.counts();
    assert!(rects <= cells + 8, "{rects} rects for {cells} cells");
    assert!(glyphs <= cells + 8, "{glyphs} glyphs for {cells} cells");
}

#[test]
fn megabytes_through_cat_stay_bounded() {
    let dir = std::env::temp_dir().join(format!("mewrk-term-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("burst.txt");
    let line = "the quick brown fox jumps over the lazy dog 0123456789 \x1b[31mred\x1b[0m and on it goes\n";
    let total = 8 << 20;
    std::fs::write(&file, line.repeat(total / line.len())).unwrap();

    let (mut term, wakes) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    let rss_before = peak_rss();
    run(&mut term, &format!("cat {}; printf 'fin%s\\n' ished", file.display()));
    let (mut frames, mut slowest, mut spent) = (1usize, Duration::ZERO, Duration::ZERO);
    let start = Instant::now();
    while !shows(&term, "finished") {
        assert!(start.elapsed() < Duration::from_secs(120), "the burst has to end");
        let t = Instant::now();
        rig.frame(&mut term);
        let took = t.elapsed();
        slowest = slowest.max(took);
        spent += took;
        frames += 1;
        assert_frame_bounded(&term, &rig);
        assert!(wakes.load(Ordering::Relaxed) <= frames + 1, "wakes must not outrun frames");
        assert!(term.view.history <= HISTORY);
        thread::sleep(Duration::from_millis(4));
    }
    rig.frame(&mut term);
    assert_frame_bounded(&term, &rig);
    assert_eq!(term.view.history, HISTORY, "the history filled up and stopped there");
    // Twenty-odd megabytes of history at most, some caches, and whatever the tests next door are doing.
    let grown = peak_rss().saturating_sub(rss_before);
    assert!(grown < 300 << 20, "peak memory grew by {} MB", grown >> 20);
    eprintln!("cat: {} MB in {:?}, {frames} frames, show mean {:?} max {slowest:?}, peak RSS +{} MB", total >> 20, start.elapsed(), spent / frames as u32, grown >> 20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn yes_does_not_stall_the_ui_thread() {
    let (mut term, wakes) = spawn_sh();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    run(&mut term, "yes");
    wait("yes to start", || term.is_busy());
    let (mut slowest, mut spent) = (Duration::ZERO, Duration::ZERO);
    let frames = 120;
    for _ in 0..frames {
        let t = Instant::now();
        rig.frame(&mut term);
        let took = t.elapsed();
        slowest = slowest.max(took);
        spent += took;
        assert_frame_bounded(&term, &rig);
        thread::sleep(Duration::from_millis(4));
    }
    assert!(wakes.load(Ordering::Relaxed) <= frames + 2);
    // Generous: this is an unoptimised build, on a machine running the other tests. Fifty frames' worth of budget
    // is still far from what a queue of unparsed output would cost.
    assert!(slowest < Duration::from_millis(400), "a frame took {slowest:?}");
    eprintln!("yes: show mean {:?} max {slowest:?}", spent / frames as u32);
    rig.focus(&mut term);
    rig.key(&mut term, KeyPress { ctrl: true, ..plain(Key::Char('c')) });
    wait("yes to stop", || !term.is_busy());
}

/// A terminal with nothing on the far end but a list of what it was told.
fn fed() -> (Terminal, Arc<Mutex<Vec<Output>>>) {
    fed_with(Options::default())
}

fn fed_with(options: Options) -> (Terminal, Arc<Mutex<Vec<Output>>>) {
    let said = Arc::new(Mutex::new(Vec::new()));
    let log = said.clone();
    let sink: Sink = Arc::new(move |output| log.lock().push(output));
    (Terminal::new(options, sink, Arc::new(|| {})), said)
}

fn screen_after_flush(term: &Terminal) -> Vec<String> {
    term.flush();
    screen(term)
}

#[test]
fn fed_bytes_reach_the_grid_even_split_inside_a_character() {
    let (term, _) = fed();
    let bytes = "plain 世界\r\n".as_bytes();
    // In the middle of a three-byte character, as an 8 KiB chunk from the host often is.
    let cut = "plain ".len() + 1;
    term.feed(&bytes[..cut]);
    term.feed(&bytes[cut..]);
    assert_eq!(screen_after_flush(&term)[0], "plain 世界");
}

#[test]
fn reset_blanks_screen_and_history_and_what_follows_lands_on_the_blank_one() {
    let (mut term, _) = fed();
    term.feed(b"\x1b[?1049h\x1b[31mold".to_vec());
    for i in 0..200 {
        term.feed(format!("line {i}\r\n").into_bytes());
    }
    term.reset();
    term.feed(b"new".to_vec());
    let lines = screen_after_flush(&term);
    assert_eq!(lines[0], "new");
    assert!(lines[1..].iter().all(String::is_empty));
    assert_eq!(term.term.lock().grid().history_size(), 0);
    assert!(!term.term.lock().mode().contains(TermMode::ALT_SCREEN), "the reset leaves the alternate screen too");
}

#[test]
fn replies_to_programs_come_out_as_replies_and_keys_as_input() {
    let (mut term, said) = fed();
    term.feed(b"\x1b[6n".to_vec());
    term.flush();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    said.lock().clear();
    rig.focus(&mut term);
    rig.input.text = "ls".into();
    rig.input.keys.push(plain(Key::Enter));
    rig.frame(&mut term);
    assert_eq!(*said.lock(), [Output::Input("ls".into()), Output::Input("\r".into())]);
    said.lock().clear();
    term.feed(b"\x1b[6n".to_vec());
    term.flush();
    assert_eq!(*said.lock(), [Output::Reply("\x1b[1;1R".into())]);
}

/// Upstream: "blocks terminal input while a Git mutation owns the workspace", and protocol replies pass regardless.
#[test]
fn with_input_off_keys_and_pastes_go_nowhere_but_replies_and_copies_do() {
    let (mut term, said) = fed();
    let mut rig = Rig::new();
    term.feed(b"foo bar\r\n".to_vec());
    term.flush();
    rig.frame(&mut term);
    rig.focus(&mut term);
    said.lock().clear();
    term.set_input_enabled(false);

    rig.input.text = "git status".into();
    rig.input.ime_commit = Some("世界".into());
    rig.input.keys.push(plain(Key::Enter));
    rig.frame(&mut term);
    rig.clip.0 = Some("pasted".into());
    rig.key(&mut term, cmd('v'));
    rig.key(&mut term, KeyPress { ctrl: true, ..plain(Key::Char('c')) });
    assert!(said.lock().is_empty(), "{:?}", said.lock());

    term.feed(b"\x1b[6n".to_vec());
    term.flush();
    assert!(matches!(said.lock().as_slice(), [Output::Reply(_)]));

    // Reading a transcript is not input.
    let (start, end) = (rig.at(0, 4), rig.at(0, 6));
    rig.press_at(&mut term, (start.0 - 3.0, start.1), false);
    rig.input.pointer = Some((end.0 + 3.0, end.1));
    rig.frame(&mut term);
    rig.release(&mut term);
    rig.clip.0 = None;
    rig.key(&mut term, KeyPress { ctrl: true, ..plain(Key::Char('c')) });
    assert_eq!(rig.clip.0.as_deref(), Some("bar"));
}

/// Upstream: "lets Ctrl+C copy a selection and interrupt otherwise".
#[test]
fn ctrl_c_copies_a_selection_and_interrupts_without_one() {
    let (mut term, said) = fed();
    let mut rig = Rig::new();
    term.feed(b"foo bar\r\n".to_vec());
    term.flush();
    rig.frame(&mut term);
    rig.focus(&mut term);
    said.lock().clear();
    let ctrl_c = KeyPress { ctrl: true, ..plain(Key::Char('c')) };

    rig.key(&mut term, ctrl_c);
    assert_eq!(*said.lock(), [Output::Input("\x03".into())]);
    said.lock().clear();

    let (start, end) = (rig.at(0, 0), rig.at(0, 2));
    rig.press_at(&mut term, (start.0 - 3.0, start.1), false);
    rig.input.pointer = Some((end.0 + 3.0, end.1));
    rig.frame(&mut term);
    rig.release(&mut term);
    rig.key(&mut term, ctrl_c);
    assert_eq!(rig.clip.0.as_deref(), Some("foo"));
    assert!(said.lock().is_empty());
    assert!(!is_copy_chord(&KeyPress { shift: true, ..ctrl_c }), "Ctrl+Shift+C is not the chord");
}

#[test]
fn convert_eol_starts_each_line_at_the_left_edge() {
    let (term, _) = fed_with(Options { convert_eol: true, ..Options::default() });
    term.feed(b"one\ntwo\r\nthree".to_vec());
    let lines = screen_after_flush(&term);
    assert_eq!(&lines[..3], ["one", "two", "three"]);
}

#[test]
fn select_all_takes_the_scrollback_too() {
    let (mut term, _) = fed_with(Options { select_all_chord: true, ..Options::default() });
    let mut rig = Rig::new();
    for i in 0..100 {
        term.feed(format!("row{i}\r\n").into_bytes());
    }
    term.flush();
    rig.frame(&mut term);
    rig.focus(&mut term);
    rig.key(&mut term, cmd('a'));
    let text = term.selection_text().unwrap();
    assert!(text.starts_with("row0\n"), "{text:?}");
    assert!(text.contains("row99"));
}

#[test]
fn resize_is_said_through_the_sink() {
    let (mut term, said) = fed();
    let mut rig = Rig::new();
    rig.frame(&mut term);
    let (cols, lines) = term.size();
    assert_ne!((cols, lines), (80, 24));
    assert_eq!(*said.lock(), [Output::Resize { cols, lines }]);
}

#[test]
fn a_program_that_asked_for_the_mouse_gets_presses_and_the_wheel_instead_of_a_selection() {
    let (mut term, said) = fed();
    let mut rig = Rig::new();
    term.feed(b"\x1b[?1000h\x1b[?1006hfoo".to_vec());
    term.flush();
    rig.frame(&mut term);
    said.lock().clear();
    let p = rig.at(2, 3);
    rig.press_at(&mut term, p, false);
    rig.release(&mut term);
    assert_eq!(*said.lock(), [Output::Reply("\x1b[<0;4;3M".into()), Output::Reply("\x1b[<0;4;3m".into())]);
    assert!(!term.has_selection());
    said.lock().clear();
    rig.input.scroll = FONT.line;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut term);
    assert_eq!(*said.lock(), [Output::Reply("\x1b[<64;4;3M".into())]);
}

#[test]
fn text_reads_the_whole_buffer_and_dropping_is_immediate() {
    let (term, _) = fed();
    for i in 0..100 {
        term.feed(format!("row{i}\r\n").into_bytes());
    }
    term.flush();
    let text = term.text();
    assert_eq!(text[0], "row0");
    assert!(text.iter().any(|l| l == "row99"));
    let start = Instant::now();
    drop(term);
    assert!(start.elapsed() < Duration::from_millis(100));
}
