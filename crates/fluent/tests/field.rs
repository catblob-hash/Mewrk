//! What a text field does with focus: blur, its own keys, masking, and the press that leaves it alone.

use std::cell::RefCell;
use std::rc::Rc;

use fluent::el::*;
use fluent::ui::{Input, Key, KeyPress, NoClipboard, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};

#[derive(Default)]
struct App {
    value: String,
    blurred: Vec<String>,
    keys: Vec<Key>,
    clicks: u32,
}

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
}

impl Rig {
    fn new() -> Self {
        Self { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard }
    }

    fn frame(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>) {
        self.scene.clear();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, (800.0, 600.0), 0.016, 0.0);
        render(view(app), app, &mut ui);
        self.input.end_frame();
    }

    fn click(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
        self.frame(app, view);
        self.input.pressed = true;
        self.input.down = true;
        self.frame(app, view);
        self.input.down = false;
        self.input.released = true;
        self.frame(app, view);
        self.frame(app, view);
    }

    fn typed(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>, s: &str) {
        self.input.text = s.to_owned();
        self.frame(app, view);
    }

    fn key(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>, key: Key) {
        self.input.keys.push(KeyPress { key, shift: false, cmd: false, alt: false, ctrl: false });
        self.frame(app, view);
    }
}

fn plain(app: &App) -> El<App> {
    col()
        .child(field("f", app.value.clone()).on_change(|a: &mut App, v| a.value = v).on_blur(|a: &mut App, v| a.blurred.push(v)).build().h(30.0).on_key(|a: &mut App, k: &KeyPress| {
            a.keys.push(k.key);
            k.key == Key::Escape
        }))
        .child(div().key("other").h(40.0).on_click(|a: &mut App| a.clicks += 1))
        .child(div().key("keeper").h(40.0).keep_focus().on_click(|a: &mut App| a.clicks += 1))
}

#[test]
fn leaving_a_field_reports_what_it_holds_once() {
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &plain);
    rig.click(&mut app, &plain, 10.0, 10.0);
    rig.typed(&mut app, &plain, "abc");
    assert_eq!(app.value, "abc");
    assert!(app.blurred.is_empty());
    rig.click(&mut app, &plain, 10.0, 50.0);
    assert_eq!(app.blurred, ["abc"]);
    assert_eq!(app.clicks, 1);
    rig.frame(&mut app, &plain);
    assert_eq!(app.blurred, ["abc"]);
}

#[test]
fn a_press_on_something_that_keeps_focus_does_not_end_the_edit() {
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &plain);
    rig.click(&mut app, &plain, 10.0, 10.0);
    rig.click(&mut app, &plain, 10.0, 90.0);
    assert_eq!(app.clicks, 1);
    assert!(app.blurred.is_empty());
    rig.typed(&mut app, &plain, "x");
    assert_eq!(app.value, "x", "the field still has the keyboard");
}

#[test]
fn a_fields_own_handler_hears_its_keys_while_it_has_focus() {
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &plain);
    rig.key(&mut app, &plain, Key::Escape);
    assert!(app.keys.is_empty());
    rig.click(&mut app, &plain, 10.0, 10.0);
    rig.key(&mut app, &plain, Key::Escape);
    assert_eq!(app.keys, [Key::Escape]);
}

#[test]
fn what_a_field_sits_inside_does_not_hear_its_keys() {
    let view = |app: &App| {
        col().key("list").on_key(|a: &mut App, k: &KeyPress| {
            a.keys.push(k.key);
            true
        })
        .child(field("g", app.value.clone()).on_change(|a: &mut App, v| a.value = v).build().h(30.0))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    rig.click(&mut app, &view, 10.0, 10.0);
    rig.typed(&mut app, &view, "ab");
    rig.key(&mut app, &view, Key::Backspace);
    assert_eq!(app.value, "a");
    assert!(app.keys.is_empty());
}

#[test]
fn an_obscured_field_edits_the_real_text_and_a_controlled_one_takes_what_it_is_given() {
    let view = |app: &App| col().child(field("s", app.value.clone()).obscure(true).controlled().on_change(|a: &mut App, v| a.value = v).build().h(30.0));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    rig.click(&mut app, &view, 10.0, 10.0);
    rig.typed(&mut app, &view, "密码ab");
    assert_eq!(app.value, "密码ab");
    rig.key(&mut app, &view, Key::Backspace);
    assert_eq!(app.value, "密码a");
    // Swapped under the caret, the new value wins.
    app.value = "reset".into();
    rig.frame(&mut app, &view);
    rig.typed(&mut app, &view, "!");
    assert_eq!(app.value, "reset!");
}

#[test]
fn a_field_can_select_on_focus_and_cap_its_length() {
    let seen: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = seen.clone();
    let view = move |app: &App| {
        let log = log.clone();
        col().child(field("c", app.value.clone()).select_on_focus(true).max_chars(4).on_change(move |a: &mut App, v| {
            log.borrow_mut().push(v.clone());
            a.value = v;
        }).build().h(30.0))
    };
    let mut rig = Rig::new();
    let mut app = App { value: "old".into(), ..App::default() };
    rig.frame(&mut app, &view);
    rig.click(&mut app, &view, 10.0, 10.0);
    rig.typed(&mut app, &view, "abcdefg");
    assert_eq!(app.value, "abcd", "what was there was replaced, and the rest dropped");
    assert_eq!(seen.borrow().as_slice(), ["abcd"]);
}

#[test]
fn a_font_family_nothing_provides_changes_nothing() {
    let mut text = TextCtx::new(1.0);
    let before = text.epoch;
    text.set_font_families("", "");
    assert_eq!(text.epoch, before, "what's already in place costs nothing");
    text.set_font_families("No Such Family, Also Not This", "");
    assert_ne!(text.epoch, before);
}

#[test]
fn a_surface_over_another_covers_the_text_at_its_top() {
    use fluent::theme::Color;
    let lid = Color::rgb(0x123456);
    let view = move |_: &App| {
        div::<App>()
            .child(col().w(300.0).h(300.0).child(text("under").absolute().top(0.0).left(0.0)).modal_layer(None))
            .child(col().w(300.0).h(300.0).bg(lid).modal_layer(None))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let top_glyphs = rig.scene.layers.iter().rposition(|l| !l.glyphs.is_empty()).expect("the text was drawn");
    let lid_layer = rig.scene.layers.iter().position(|l| l.rects.iter().any(|r| r.fill == lid)).expect("the lid was drawn");
    assert!(lid_layer > top_glyphs, "the lid is on layer {lid_layer}, the text on {top_glyphs}");
}
