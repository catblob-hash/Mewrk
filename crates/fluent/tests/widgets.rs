//! Widget behaviour under scripted input. `Ui` needs no GPU, so all of this runs without a window.

use fluent::edit::EditResponse;
use fluent::theme::{Theme, TypeStyle, Weight, BODY};
use fluent::ui::{rect, Clipboard, Input, Key, KeyPress, NoClipboard, Rect, Response, Ui, UiMemory};
use fluent::{Scene, Scroll, TextCtx, TextEdit};

const FRAME: f32 = 1.0 / 120.0;

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    time: f32,
}

trait MeasureHeight {
    fn measure_height(&mut self, rig: &mut Rig, ty: &TypeStyle) -> f32;
}

impl MeasureHeight for TextEdit {
    fn measure_height(&mut self, rig: &mut Rig, ty: &TypeStyle) -> f32 {
        rig.frame(&mut NoClipboard, |ui| self.measure(ui, ty, 120.0))
    }
}

impl Rig {
    fn new() -> Self {
        Self { scene: Scene::default(), text: TextCtx::new(2.0), mem: UiMemory::default(), input: Input::default(), time: 0.0 }
    }

    /// Runs one frame through `f`, then clears per-frame input, the way the real loop does.
    fn frame<R>(&mut self, clipboard: &mut dyn Clipboard, f: impl FnOnce(&mut Ui<'_>) -> R) -> R {
        self.scene.clear();
        self.time += FRAME;
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, clipboard, 2.0, (1000.0, 800.0), FRAME, self.time);
        let out = f(&mut ui);
        self.input.end_frame();
        out
    }

    fn move_to(&mut self, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
    }

    fn press(&mut self) {
        self.input.down = true;
        self.input.pressed = true;
    }

    fn release(&mut self) {
        self.input.down = false;
        self.input.released = true;
    }

    fn key(&mut self, key: Key) {
        self.input.keys.push(KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false });
    }

    fn cmd(&mut self, c: char) {
        self.input.keys.push(KeyPress { key: Key::Char(c), shift: false, cmd: true, alt: false, ctrl: false });
    }
}

const BUTTON: Rect = rect(100.0, 100.0, 120.0, 32.0);

fn button_frame(rig: &mut Rig) -> Response {
    rig.frame(&mut NoClipboard, |ui| ui.button(1, BUTTON, "OK"))
}

#[test]
fn button_clicks_only_when_released_over_itself() {
    let mut rig = Rig::new();
    rig.move_to(110.0, 110.0);
    assert!(button_frame(&mut rig).hovered);

    rig.press();
    assert!(!button_frame(&mut rig).clicked, "a press alone is not a click");
    rig.release();
    assert!(button_frame(&mut rig).clicked);
    assert!(!button_frame(&mut rig).clicked, "a click lasts one frame");

    // Press inside, drag off, release: that is how people change their mind.
    rig.press();
    button_frame(&mut rig);
    rig.move_to(500.0, 500.0);
    button_frame(&mut rig);
    rig.release();
    assert!(!button_frame(&mut rig).clicked);

    // Press elsewhere and drag on: not a click either, and it must not even highlight.
    rig.press();
    button_frame(&mut rig);
    rig.input.pressed = false;
    rig.move_to(110.0, 110.0);
    rig.release();
    assert!(!button_frame(&mut rig).clicked);
}

#[test]
fn hover_animation_settles_and_stops_asking_for_frames() {
    let mut rig = Rig::new();
    rig.move_to(500.0, 500.0);
    button_frame(&mut rig);
    rig.move_to(110.0, 110.0);
    let animating = rig.frame(&mut NoClipboard, |ui| {
        ui.button(1, BUTTON, "OK");
        ui.animating
    });
    assert!(animating, "hovering starts an animation");
    let mut still_animating = true;
    for _ in 0..240 {
        still_animating = rig.frame(&mut NoClipboard, |ui| {
            ui.button(1, BUTTON, "OK");
            ui.animating
        });
    }
    assert!(!still_animating, "an idle UI must go quiet, or it would burn power for ever");
}

#[test]
fn an_open_combo_takes_input_away_from_what_is_under_it() {
    let mut rig = Rig::new();
    let options = ["one", "two", "three"];
    let combo = rect(100.0, 100.0, 200.0, 32.0);
    // A button sits right where the dropdown's rows will open.
    let under = rect(100.0, 140.0, 200.0, 32.0);
    let frame = |rig: &mut Rig| {
        rig.frame(&mut NoClipboard, |ui| {
            let below = ui.button(10, under, "under");
            let picked = ui.combo(20, combo, &options, 0);
            (below, picked)
        })
    };

    rig.move_to(150.0, 110.0);
    rig.press();
    frame(&mut rig);
    rig.release();
    frame(&mut rig);

    // One more frame, so that the popup's area from the previous frame is known.
    rig.move_to(150.0, 150.0);
    frame(&mut rig);
    rig.press();
    let (below, _) = frame(&mut rig);
    assert!(!below.hovered, "the popup must shadow the button under it");
    rig.release();
    let (below, picked) = frame(&mut rig);
    assert!(!below.clicked, "the click belongs to the popup");
    assert_eq!(picked, Some(1), "the second row sits under the pointer");
    assert_eq!(rig.mem.open_popup, 0, "choosing closes it");
}

#[test]
fn clicking_outside_dismisses_a_combo() {
    let mut rig = Rig::new();
    let combo = rect(100.0, 100.0, 200.0, 32.0);
    let frame = |rig: &mut Rig| rig.frame(&mut NoClipboard, |ui| ui.combo(20, combo, &["a", "b"], 0));
    rig.move_to(150.0, 110.0);
    rig.press();
    frame(&mut rig);
    rig.release();
    frame(&mut rig);
    assert_eq!(rig.mem.open_popup, 20);
    rig.move_to(800.0, 700.0);
    rig.press();
    frame(&mut rig);
    assert_eq!(rig.mem.open_popup, 0);
}

const FIELD: Rect = rect(50.0, 50.0, 400.0, 44.0);

fn edit_frame(rig: &mut Rig, edit: &mut TextEdit, clipboard: &mut dyn Clipboard) -> EditResponse {
    rig.frame(clipboard, |ui| edit.show(ui, 7, FIELD, "placeholder", &BODY, true))
}

fn focused(rig: &mut Rig, edit: &mut TextEdit) {
    rig.move_to(100.0, 60.0);
    rig.press();
    edit_frame(rig, edit, &mut NoClipboard);
    rig.release();
    edit_frame(rig, edit, &mut NoClipboard);
}

#[test]
fn text_field_edits_across_multibyte_characters() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    focused(&mut rig, &mut edit);

    rig.input.text.push_str("中文ab");
    assert!(edit_frame(&mut rig, &mut edit, &mut NoClipboard).changed);
    assert_eq!(edit.text, "中文ab");

    rig.key(Key::Backspace);
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "中文a");

    rig.key(Key::Left);
    rig.key(Key::Left);
    rig.key(Key::Backspace);
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "文a", "the caret moves by characters, never into the middle of one");

    rig.key(Key::Delete);
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "a");
}

#[test]
fn enter_submits_and_shift_enter_breaks_the_line() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    focused(&mut rig, &mut edit);
    rig.input.text.push_str("hi");
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    rig.input.keys.push(KeyPress { key: Key::Enter, shift: true, cmd: false, alt: false, ctrl: false });
    assert!(!edit_frame(&mut rig, &mut edit, &mut NoClipboard).submitted);
    assert_eq!(edit.text, "hi\n");
    rig.key(Key::Enter);
    assert!(edit_frame(&mut rig, &mut edit, &mut NoClipboard).submitted);
}

#[test]
fn ime_composition_shows_preedit_and_commits_once() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    focused(&mut rig, &mut edit);
    rig.input.ime_preedit = Some("ni'hao".into());
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "", "composition is not text until it is committed");

    rig.input.ime_preedit = None;
    rig.input.ime_commit = Some("你好".into());
    assert!(edit_frame(&mut rig, &mut edit, &mut NoClipboard).changed);
    assert_eq!(edit.text, "你好");
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "你好", "a commit applies exactly once");
}

struct Board(Option<String>);
impl Clipboard for Board {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

#[test]
fn clipboard_shortcuts_work_on_the_selection() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    let mut board = Board(None);
    focused(&mut rig, &mut edit);
    rig.input.text.push_str("copy me");
    edit_frame(&mut rig, &mut edit, &mut board);

    rig.cmd('a');
    rig.cmd('c');
    edit_frame(&mut rig, &mut edit, &mut board);
    assert_eq!(board.0.as_deref(), Some("copy me"));

    rig.cmd('a');
    rig.cmd('x');
    edit_frame(&mut rig, &mut edit, &mut board);
    assert_eq!(edit.text, "");

    rig.cmd('v');
    rig.cmd('v');
    edit_frame(&mut rig, &mut edit, &mut board);
    assert_eq!(edit.text, "copy mecopy me");
}

#[test]
fn an_unfocused_field_ignores_typing() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    rig.input.text.push_str("nope");
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "");
}

#[test]
fn scrolling_follows_a_stream_until_the_user_scrolls_away() {
    let mut rig = Rig::new();
    let mut scroll = Scroll::default();
    let area = rect(0.0, 0.0, 600.0, 400.0);
    rig.move_to(300.0, 200.0);
    let mut content = 300.0;
    for _ in 0..50 {
        content += 20.0;
        rig.frame(&mut NoClipboard, |ui| scroll.update(ui, 5, area, content));
    }
    assert_eq!(scroll.offset, content - 400.0, "new text stays in view while streaming");

    // The user scrolls up to read something.
    rig.input.scroll = 300.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut NoClipboard, |ui| scroll.update(ui, 5, area, content));
    let reading_at = scroll.offset;
    assert!(!scroll.stick);
    for _ in 0..20 {
        content += 20.0;
        rig.frame(&mut NoClipboard, |ui| scroll.update(ui, 5, area, content));
    }
    assert_eq!(scroll.offset, reading_at, "text arriving must not yank the page away from a reader");

    // Scrolling back to the end re-attaches.
    rig.input.scroll = -100_000.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut NoClipboard, |ui| scroll.update(ui, 5, area, content));
    assert!(scroll.stick);
    content += 20.0;
    rig.frame(&mut NoClipboard, |ui| scroll.update(ui, 5, area, content));
    assert_eq!(scroll.offset, content - 400.0);
}

#[test]
fn a_glide_comes_down_to_an_end_that_keeps_moving_without_overshooting_and_the_wheel_ends_it() {
    let mut rig = Rig::new();
    let mut scroll = Scroll::from_top();
    let area = rect(0.0, 0.0, 600.0, 400.0);
    let mut content = 1400.0;
    rig.frame(&mut NoClipboard, |ui| scroll.advance(ui, area, content));
    scroll.glide_to_end(18.0);
    let mut last = scroll.offset;
    let mut frames = 0;
    while scroll.gliding() {
        frames += 1;
        assert!(frames < 400, "comes to rest");
        // The end moves on partway, as a stream's next commit moves it.
        if frames == 20 {
            content += 300.0;
        }
        let animating = rig.frame(&mut NoClipboard, |ui| {
            scroll.advance(ui, area, content);
            ui.animating
        });
        assert!(scroll.offset >= last - 0.01 && scroll.offset <= content - 400.0, "{} after {last}", scroll.offset);
        assert!((scroll.moving() && animating) || !scroll.gliding(), "asks for the next frame while it glides");
        last = scroll.offset;
    }
    assert!(frames > 10, "glided over {frames} frames rather than jumping");
    assert_eq!(scroll.offset, content - 400.0);

    // The reader turning the wheel takes over from a glide under way.
    content += 600.0;
    scroll.glide_to_end(18.0);
    rig.frame(&mut NoClipboard, |ui| scroll.advance(ui, area, content));
    assert!(scroll.gliding());
    rig.move_to(300.0, 200.0);
    rig.input.scroll = 40.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut NoClipboard, |ui| scroll.advance(ui, area, content));
    assert!(!scroll.gliding());
    let left = scroll.offset;
    rig.frame(&mut NoClipboard, |ui| scroll.advance(ui, area, content));
    assert_eq!(scroll.offset, left, "and nothing pulls the page on after it");
}

#[test]
fn a_steady_scene_keeps_its_buffers() {
    let mut rig = Rig::new();
    for _ in 0..3 {
        rig.frame(&mut NoClipboard, |ui| {
            for i in 0..100 {
                ui.label(0.0, i as f32 * 20.0, "the same label", &BODY, ui.theme.text_primary);
            }
        });
    }
    let capacity = rig.scene.layers[0].glyphs.capacity();
    let pointer = rig.scene.layers[0].glyphs.as_ptr();
    for _ in 0..100 {
        rig.frame(&mut NoClipboard, |ui| {
            for i in 0..100 {
                ui.label(0.0, i as f32 * 20.0, "the same label", &BODY, ui.theme.text_primary);
            }
        });
    }
    assert_eq!(rig.scene.layers[0].glyphs.capacity(), capacity);
    assert_eq!(rig.scene.layers[0].glyphs.as_ptr(), pointer, "a repeating frame must not reallocate");
}

#[test]
fn the_wheel_moves_overflowing_text_and_the_caret_only_pulls_it_back_when_it_moves() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    focused(&mut rig, &mut edit);
    edit.set_text("line\n".repeat(60));
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    let at_the_caret = edit.scroll_offset();
    assert!(at_the_caret > 0.0, "the caret at the end was brought into view");
    rig.input.scroll = 100.0;
    rig.input.scroll_is_pixels = true;
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.scroll_offset(), at_the_caret - 100.0);
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.scroll_offset(), at_the_caret - 100.0, "it stays where the wheel left it");
    rig.input.text.push('x');
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert!(edit.scroll_offset() > at_the_caret - 100.0, "typing brings the caret back");
}

#[test]
fn undo_takes_a_run_of_typing_back_as_one_step_and_redo_brings_it_back() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    focused(&mut rig, &mut edit);
    for typed in ["a", "b"] {
        rig.input.text.push_str(typed);
        edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    }
    assert_eq!(edit.text, "ab");
    rig.input.keys.push(KeyPress { key: Key::Char('z'), shift: false, cmd: true, alt: false, ctrl: false });
    assert!(edit_frame(&mut rig, &mut edit, &mut NoClipboard).changed);
    assert_eq!(edit.text, "");
    rig.input.keys.push(KeyPress { key: Key::Char('z'), shift: true, cmd: true, alt: false, ctrl: false });
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "ab");
}

#[test]
fn composition_leaves_with_the_focus_and_keys_during_it_are_the_input_methods() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    focused(&mut rig, &mut edit);
    rig.input.text.push_str("ab");
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    rig.input.ime_preedit = Some("ni".into());
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert!(edit.is_composing());
    rig.input.keys.push(KeyPress { key: Key::Backspace, shift: false, cmd: false, alt: false, ctrl: false });
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "ab", "Backspace edits what is being composed, which the input method does");
    rig.input.ime_preedit = Some(String::new());
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert!(!edit.is_composing(), "an empty composition is none");
    assert_eq!(edit.text, "ab");
}

#[test]
fn serif_text_is_set_in_its_own_family_and_falls_back_to_the_interface_font() {
    let mut text = TextCtx::new(1.0);
    let plain = TypeStyle { size: 16.0, line: 24.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
    let serif = TypeStyle { serif: true, ..plain };
    let words = "Hamburgefonstiv quick brown fox";
    let width = |text: &mut TextCtx, ty: &TypeStyle| text.line(words, ty, fluent::Color::WHITE).width;
    // Nothing chosen: the same face either way.
    assert_eq!(width(&mut text, &plain), width(&mut text, &serif));
    text.set_serif_family("Georgia, \"Times New Roman\", serif");
    if cfg!(target_os = "macos") {
        assert_ne!(width(&mut text, &plain), width(&mut text, &serif), "Georgia ships with every Mac");
    }
    // A code span inside a serif message stays in code type.
    let mono = TypeStyle { mono: true, ..plain };
    assert_eq!(width(&mut text, &mono), width(&mut text, &TypeStyle { serif: true, ..mono }));
    // And choosing nothing goes back.
    text.set_serif_family("");
    assert_eq!(width(&mut text, &plain), width(&mut text, &serif));
}

/// Everything a hover can pass through on its way from a solid fill to a faint wash, and back from a faint one, in both themes.
#[test]
fn a_colour_going_to_a_faint_one_never_passes_through_a_ghost_of_its_hue() {
    use fluent::theme::Color;
    for theme in [Theme::dark(), Theme::light()] {
        let (base, wash) = (theme.success_bg, theme.subtle_hover);
        let end = base.over(wash);
        let span = |a: u8, b: u8| (a.min(b), a.max(b));
        for step in 0..=20 {
            let t = step as f32 / 20.0;
            let now = base.over(wash.fade(t));
            assert_eq!(now.a, 255, "a wash over a solid fill stays solid");
            for (at, (from, to)) in [(now.r, span(base.r, end.r)), (now.g, span(base.g, end.g)), (now.b, span(base.b, end.b))] {
                assert!(at + 1 >= from && at <= to.saturating_add(1), "{now:?} left the way from {base:?} to {end:?}");
            }
        }
        // Between two colours of different strengths, what lands on the page is a straight fade.
        let (solid, faint) = (Color::rgb(0x393D1B), Color::argb(0x0FFFFFFF));
        let halfway = solid.mix(faint, 0.5);
        let bright = |c: Color| (c.r as u32 + c.g as u32 + c.b as u32) * c.a as u32 / 255;
        assert!(bright(halfway) <= bright(solid).max(bright(faint)) + 3, "{halfway:?} is brighter than both ends");
    }
    let clear = Color::TRANSPARENT.mix(Color::rgb(0x102030), 0.5);
    assert_eq!((clear.r, clear.g, clear.b), (0x10, 0x20, 0x30), "fading in from nothing takes the colour at once");
}

fn single_line_frame(rig: &mut Rig, edit: &mut TextEdit, r: Rect) -> EditResponse {
    edit.set_single_line(true);
    rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "Type a URL", &BODY, true))
}

/// The bounding box of the solid white-ish pieces a frame painted on top of its text: where the caret went.
fn caret_of(rig: &Rig, t: &Theme) -> Option<[f32; 4]> {
    rig.scene.layers.iter().flat_map(|l| l.rects.iter()).find(|q| q.fill == t.text_primary && q.rect[2] <= 2.5).map(|q| q.rect)
}

#[test]
fn an_empty_field_shows_its_caret_the_frame_it_takes_focus() {
    let theme = Theme::dark();
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    let r = rect(100.0, 100.0, 300.0, 40.0);
    assert!(rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "", &BODY, true)) == EditResponse::default());
    assert!(caret_of(&rig, &theme).is_none(), "nothing has focus yet");
    rig.mem.focus = 7;
    rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "", &BODY, true));
    let caret = caret_of(&rig, &theme).expect("a caret in an empty field");
    assert!(caret[3] > 8.0, "as tall as a line would be, not {caret:?}");
}

#[test]
fn focus_given_long_after_the_field_appeared_lights_the_caret_at_once() {
    let theme = Theme::dark();
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    edit.set_text("text");
    let r = rect(100.0, 100.0, 300.0, 40.0);
    // Every phase of the blink: whenever focus arrives, the caret has to be there.
    for _ in 0..200 {
        rig.mem.focus = 0;
        rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "", &BODY, true));
        rig.mem.focus = 7;
        rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "", &BODY, true));
        assert!(caret_of(&rig, &theme).is_some(), "at {}", rig.time);
        rig.time += 0.0371;
    }
}

#[test]
fn a_single_line_field_stays_one_line_centred_in_its_box() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    edit.set_text("https://example.com/a/very/long/path/that/would/never/fit/inside/a/field/this/narrow/if/it/wrapped/it/would/be/many/lines");
    let r = rect(100.0, 100.0, 120.0, 40.0);
    rig.mem.focus = 7;
    single_line_frame(&mut rig, &mut edit, r);
    // The caret sits at the end of the long line, inside the box and in its middle.
    let theme = Theme::dark();
    let caret = caret_of(&rig, &theme).expect("a caret");
    assert_eq!(edit.measure_height(&mut rig, &BODY), BODY.line, "no line was ever wrapped");
    assert!(caret[0] >= 200.0 && caret[0] <= 440.0, "{caret:?} is scrolled into view");
    let middle = (caret[1] + caret[3] * 0.5) / 2.0;
    assert!((middle - 120.0).abs() <= 1.5, "{middle} is the middle of the box");
}

#[test]
fn a_line_end_cannot_get_into_a_single_line_field() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    let r = rect(100.0, 100.0, 300.0, 24.0);
    rig.mem.focus = 7;
    single_line_frame(&mut rig, &mut edit, r);
    rig.input.keys.push(KeyPress { key: Key::Enter, shift: true, cmd: false, alt: false, ctrl: false });
    let out = single_line_frame(&mut rig, &mut edit, r);
    assert!(out.submitted, "Shift+Enter submits an `<input>` as well");
    assert_eq!(edit.text, "");
    struct Paste;
    impl Clipboard for Paste {
        fn get(&mut self) -> Option<String> {
            Some("one\ntwo\r\nthree".to_owned())
        }
        fn set(&mut self, _: &str) {}
    }
    rig.cmd('v');
    rig.frame(&mut Paste, |ui| edit.show_bare(ui, 7, r, "", &BODY, true));
    assert_eq!(edit.text, "onetwothree");
}

#[test]
fn a_second_press_takes_the_word_and_a_third_the_line() {
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    edit.set_text("hello there world");
    let r = rect(100.0, 100.0, 600.0, 24.0);
    rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "", &BODY, true));
    rig.move_to(150.0, 110.0);
    let click = |rig: &mut Rig, edit: &mut TextEdit, double: bool| {
        rig.press();
        rig.input.double_click = double;
        rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "", &BODY, true));
        rig.release();
        rig.frame(&mut NoClipboard, |ui| edit.show_bare(ui, 7, r, "", &BODY, true));
    };
    click(&mut rig, &mut edit, false);
    assert!(edit.selection_range().is_empty());
    click(&mut rig, &mut edit, true);
    let sel = edit.selection_range();
    assert!(!sel.is_empty() && !edit.text[sel.clone()].contains(' '), "{:?}", &edit.text[sel]);
    click(&mut rig, &mut edit, true);
    assert_eq!(edit.selection_range(), 0..edit.text.len());
}

#[test]
fn a_picture_fixed_to_the_window_shows_what_lies_under_each_box() {
    use fluent::ui::fixed_uv;
    let window = rect(0.0, 0.0, 1000.0, 500.0);
    // The same shape as the window: the whole picture, and a box sees its own share of it.
    assert_eq!(fixed_uv(window, window, (2000, 1000), (0.5, 0.5)), [0.0, 0.0, 1.0, 1.0]);
    assert_eq!(fixed_uv(rect(250.0, 100.0, 500.0, 250.0), window, (2000, 1000), (0.5, 0.5)), [0.25, 0.2, 0.5, 0.5]);
    // Moving the box moves the window onto the picture, not the picture with the box.
    let [left, ..] = fixed_uv(rect(0.0, 0.0, 100.0, 100.0), window, (2000, 1000), (0.5, 0.5));
    let [right, ..] = fixed_uv(rect(500.0, 0.0, 100.0, 100.0), window, (2000, 1000), (0.5, 0.5));
    assert_eq!((left, right), (0.0, 0.5));
    // Too wide for the window: cropped at the sides, around where the focus says.
    let [u, v, w, h] = fixed_uv(window, window, (4000, 1000), (0.25, 0.5));
    assert_eq!((u, v, w, h), (0.125, 0.0, 0.5, 1.0));
    // Too tall: at the top and bottom.
    let [u, v, w, h] = fixed_uv(window, window, (1000, 1000), (0.5, 1.0));
    assert_eq!((u, v, w, h), (0.0, 0.5, 1.0, 0.5));
}

/// Knows one bad word, and leaves the user's dictionary alone.
struct OneBadWord;

impl fluent::spell::Speller for OneBadWord {
    fn misspelt(&mut self, _paragraph: &str, words: &[String]) -> Vec<String> {
        words.iter().filter(|word| *word == "teh").cloned().collect()
    }
    fn guesses(&mut self, _word: &str) -> Vec<String> {
        vec!["the".to_owned()]
    }
    fn learn(&mut self, _word: &str) {}
    fn ignore(&mut self, _word: &str) {}
}

/// Frames until the checker has answered, as a field's wake-ups would bring them. Answers how many line segments the field draws.
fn spelled(rig: &mut Rig, edit: &mut TextEdit) -> usize {
    const KIND_LINE: f32 = 3.0;
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        edit_frame(rig, edit, &mut NoClipboard);
    }
    rig.scene.layers.iter().flat_map(|layer| &layer.rects).filter(|inst| inst.params[2] == KIND_LINE).count()
}

#[test]
fn a_misspelt_word_gets_a_wavy_line_when_it_is_finished_and_a_correction_is_one_undo_step() {
    fluent::spell::install(Box::new(OneBadWord));
    let mut rig = Rig::new();
    let mut edit = TextEdit::new();
    edit.set_spell_check(true);
    focused(&mut rig, &mut edit);

    rig.input.text.push_str("teh");
    assert_eq!(spelled(&mut rig, &mut edit), 0, "still being written");
    assert!(edit.misspelt().is_empty());
    rig.input.text.push_str(" cat");
    assert!(spelled(&mut rig, &mut edit) > 0, "a space finishes it");
    assert_eq!(edit.misspelt(), [0..3]);

    // While obscured nothing is shown, or spelled.
    edit.set_obscured(true);
    assert_eq!(spelled(&mut rig, &mut edit), 0);
    edit.set_obscured(false);

    // A word with composed text before it is still marked in the right place.
    rig.input.ime_preedit = Some("世界".to_owned());
    assert!(spelled(&mut rig, &mut edit) > 0);
    assert_eq!(edit.misspelt(), [0..3]);
    rig.input.ime_preedit = None;
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);

    // A press on the word selects it, and a press on another word doesn't.
    let inner = rect(20.0 + 11.0, 20.0 + 6.0, 0.0, 0.0);
    assert_eq!(edit.press_for_menu(inner.x + 4.0, inner.y + 4.0), Some(0..3));
    assert_eq!(edit.selection_range(), 0..3);
    assert_eq!(edit.press_for_menu(inner.x + 400.0, inner.y + 4.0), None);
    assert_eq!(edit.selection_range(), 7..7);

    edit.replace_range(0..3, "the");
    assert_eq!(edit.text, "the cat");
    assert_eq!(spelled(&mut rig, &mut edit), 0);
    rig.cmd('z');
    edit_frame(&mut rig, &mut edit, &mut NoClipboard);
    assert_eq!(edit.text, "teh cat");

    edit.set_spell_check(false);
    assert_eq!(spelled(&mut rig, &mut edit), 0, "off, nothing is drawn");
    assert!(edit.misspelt().is_empty());
}
