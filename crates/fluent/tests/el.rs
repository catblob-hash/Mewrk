//! The element layer, driven with scripted input and no GPU.

use fluent::el::*;
use fluent::ui::{Input, Key, KeyPress, NoClipboard, Rect, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use std::sync::Arc;

#[derive(Default)]
struct App {
    clicks: Vec<&'static str>,
    open: bool,
}

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: NoClipboard,
    time: f32,
}

impl Rig {
    fn new() -> Self {
        Self { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: NoClipboard, time: 0.0 }
    }

    fn frame(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>) {
        self.scene.clear();
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, &mut self.clipboard, 1.0, (800.0, 600.0), 0.016, self.time);
        render(view(app), app, &mut ui);
        self.input.end_frame();
    }

    /// Rests the pointer on what carries this key, for long enough that hover text comes up, and says how many glyphs were drawn for it.
    fn rest_on(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>, key: &'static str) -> usize {
        self.frame(app, view);
        let r = rect_of(key).expect("laid out");
        self.input.pointer = Some((r.x + r.w / 2.0, r.y + r.h / 2.0));
        self.frame(app, view);
        assert_eq!(self.hover_glyphs(), 0, "too soon, for {key}");
        self.time += 1.0;
        self.frame(app, view);
        self.hover_glyphs()
    }

    fn hover_glyphs(&self) -> usize {
        self.scene.layers[fluent::gpu::LAYER_TOOLTIP].glyphs.len()
    }

    /// A key on one frame, and the frame after it, which is when a focus request lands.
    fn key(&mut self, app: &mut App, view: &dyn Fn(&App) -> El<App>, key: Key, shift: bool) {
        self.input.keys.push(KeyPress { key, shift, cmd: false, alt: false, ctrl: false });
        self.frame(app, view);
        self.frame(app, view);
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
    }
}

fn rect_near(r: Rect, x: f32, y: f32, w: f32, h: f32) {
    assert!((r.x - x).abs() < 1.0 && (r.y - y).abs() < 1.0 && (r.w - w).abs() < 1.0 && (r.h - h).abs() < 1.0, "{r:?} is not ({x},{y},{w},{h})");
}

#[test]
fn flex_lays_out_like_flex() {
    let view = |_: &App| {
        row::<App>()
            .p(10.0)
            .gap(5.0)
            .child(div().key("a").w(100.0).h(40.0))
            .child(div().key("b").flex1().h(40.0))
            .child(div().key("c").w(50.0).self_stretch())
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    rect_near(rect_of("a").unwrap(), 10.0, 10.0, 100.0, 40.0);
    rect_near(rect_of("b").unwrap(), 115.0, 10.0, 620.0, 40.0);
    rect_near(rect_of("c").unwrap(), 740.0, 10.0, 50.0, 580.0);
}

#[test]
fn a_click_goes_to_the_deepest_element_only() {
    let view = |_: &App| {
        div::<App>().child(
            row().key("row").h(40.0).on_click(|a: &mut App| a.clicks.push("row")).child(div().key("btn").w(30.0).h(30.0).on_click(|a: &mut App| a.clicks.push("btn"))),
        )
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.click(&mut app, &view, 10.0, 10.0);
    assert_eq!(app.clicks, ["btn"]);
    rig.click(&mut app, &view, 200.0, 10.0);
    assert_eq!(app.clicks, ["btn", "row"]);
}

#[test]
fn a_press_that_leaves_the_element_is_not_a_click() {
    let view = |_: &App| div::<App>().child(div().key("x").h(40.0).on_click(|a: &mut App| a.clicks.push("x")));
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.input.pointer = Some((10.0, 10.0));
    rig.frame(&mut app, &view);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.frame(&mut app, &view);
    rig.input.pointer = Some((10.0, 300.0));
    rig.input.down = false;
    rig.input.released = true;
    rig.frame(&mut app, &view);
    assert!(app.clicks.is_empty());
}

#[test]
fn popovers_sit_on_their_anchor_take_clicks_and_close_on_an_outside_press() {
    let view = |a: &App| {
        div::<App>()
            .child(div().key("anchor").w(100.0).h(30.0).ml(50.0).on_click(|a: &mut App| a.open = !a.open))
            .child(div().key("below").h(200.0).on_click(|a: &mut App| a.clicks.push("below")))
            .when(a.open, |e| {
                e.child(div().key("pop").w(120.0).h(60.0).on_click(|a: &mut App| a.clicks.push("pop")).popover("anchor", Side::Below, Align::Start, 4.0, |a: &mut App| a.open = false))
            })
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.click(&mut app, &view, 60.0, 10.0);
    assert!(app.open);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    rect_near(rect_of("pop").unwrap(), 50.0, 34.0, 120.0, 60.0);
    // It lies over `below`, and takes the click instead of it.
    rig.click(&mut app, &view, 60.0, 50.0);
    assert_eq!(app.clicks, ["pop"]);
    assert!(app.open);
    // Elsewhere, the press closes it and still lands.
    rig.click(&mut app, &view, 400.0, 150.0);
    assert!(!app.open);
    assert_eq!(app.clicks, ["pop", "below"]);
}

#[test]
fn a_tray_nothing_dismisses_leaves_escape_to_the_menu_under_it() {
    let view = |a: &App| {
        div::<App>()
            .child(div().key("anchor").w(100.0).h(30.0))
            .when(a.open, |e| e.child(div().key("pop").w(120.0).h(60.0).popover("anchor", Side::Below, Align::Start, 4.0, |a: &mut App| a.open = false)))
            .child(div().key("tray").w(100.0).h(40.0).overlay_at(600.0, 10.0))
            .on_key_global(|a: &mut App, press| {
                if press.key == fluent::ui::Key::Escape {
                    a.clicks.push("global");
                }
                false
            })
    };
    let escape = fluent::ui::KeyPress { key: fluent::ui::Key::Escape, shift: false, cmd: false, alt: false, ctrl: false };
    let mut rig = Rig::new();
    let mut app = App { open: true, ..App::default() };
    rig.frame(&mut app, &view);
    rect_near(rect_of("tray").unwrap(), 600.0, 10.0, 100.0, 40.0);
    rig.input.keys.push(escape.clone());
    rig.frame(&mut app, &view);
    assert!(!app.open, "the menu under the tray closes");
    assert!(app.clicks.is_empty());
    // With no menu left, the tray does not swallow the key either.
    rig.input.keys.push(escape);
    rig.frame(&mut app, &view);
    assert_eq!(app.clicks, ["global"]);
    // And a press beside it goes where it was going, with the tray still up.
    rig.frame(&mut app, &view);
    assert!(rect_of("tray").is_some());
}

#[test]
fn a_press_on_the_anchor_does_not_close_and_reopen() {
    let view = |a: &App| {
        div::<App>()
            .child(div().key("anchor").w(100.0).h(30.0).on_click(|a: &mut App| a.open = !a.open))
            .when(a.open, |e| e.child(div().key("pop").w(120.0).h(60.0).popover("anchor", Side::Below, Align::Start, 4.0, |a: &mut App| a.open = false)))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.click(&mut app, &view, 10.0, 10.0);
    assert!(app.open);
    rig.click(&mut app, &view, 10.0, 10.0);
    assert!(!app.open);
}

#[test]
fn a_scroller_scrolls_its_children_and_clips_their_clicks() {
    let view = |_: &App| {
        col::<App>().child(
            div()
                .key("list")
                .h(100.0)
                .scroll_y()
                .children((0..10).map(|i| div::<App>().key(("row", i)).h(50.0).shrink0().on_click(move |a: &mut App| a.clicks.push(if i == 0 { "zero" } else if i == 2 { "two" } else { "other" })))),
        )
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.input.pointer = Some((10.0, 10.0));
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    let (_, content, viewport) = scroll_state("list").unwrap();
    assert_eq!((content, viewport), (500.0, 100.0));
    rig.input.scroll = -100.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    rect_near(rect_of(("row", 2)).unwrap(), 0.0, 0.0, 800.0, 50.0);
    rig.click(&mut app, &view, 10.0, 10.0);
    assert_eq!(app.clicks, ["two"]);
    // Rows scrolled out of the box are not there to click, even where they would be.
    rig.click(&mut app, &view, 10.0, 300.0);
    assert_eq!(app.clicks, ["two"]);
}

#[test]
fn local_state_lasts_while_asked_for_and_ends_when_not() {
    let mut rig = Rig::new();
    let mut app = App::default();
    let keep = std::cell::Cell::new(true);
    let view = |_: &App| {
        if keep.get() {
            let v = use_state("counter", || 0);
            v.update(|x| *x += 1);
        }
        div::<App>()
    };
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert_eq!(use_state("counter", || 100).get(), 2);
    keep.set(false);
    for _ in 0..4 {
        rig.frame(&mut app, &view);
    }
    assert_eq!(use_state("counter", || 100).get(), 100);
}

#[test]
fn text_wraps_at_the_box_and_pushes_siblings_down() {
    let long = "word ".repeat(100);
    let view = move |_: &App| {
        col::<App>().child(div().w(200.0).child(text(long.clone()).key("t"))).child(div().key("after").h(10.0))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    let t = rect_of("t").unwrap();
    assert_eq!(t.w, 200.0);
    assert!(t.h > 100.0, "{t:?}");
    assert_eq!(rect_of("after").unwrap().y, t.bottom());
}

#[test]
fn ellipsis_shrinks_in_a_row_and_keeps_siblings_in_view() {
    let long = "word ".repeat(100);
    let view = move |_: &App| {
        div::<App>().child(row().w(300.0).h(30.0).items_center().child(text(long.clone()).key("t").ellipsis().flex1()).child(div().key("end").w(40.0).h(10.0)))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.frame(&mut app, &view);
    assert_eq!(rect_of("t").unwrap().w, 260.0);
    assert_eq!(rect_of("end").unwrap().x, 260.0);
}

#[test]
fn hover_handlers_hear_the_pointer_arrive_and_leave_once_each() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let log: Rc<RefCell<Vec<bool>>> = Rc::default();
    let sink = log.clone();
    let view = move |_: &App| {
        let sink = sink.clone();
        div::<App>().child(div().key("h").h(40.0).on_hover_change(move |_: &mut App, on| sink.borrow_mut().push(on)))
    };
    let mut rig = Rig::new();
    let mut app = App::default();
    rig.input.pointer = Some((10.0, 300.0));
    rig.frame(&mut app, &view);
    rig.input.pointer = Some((10.0, 10.0));
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    rig.input.pointer = Some((10.0, 300.0));
    rig.frame(&mut app, &view);
    assert_eq!(*log.borrow(), [true, false]);
}

#[test]
fn a_popover_limited_by_max_w_is_as_tall_as_its_wrapped_text() {
    let long = "A heading long enough that it has to wrap, because the panel may not be any wider than the limit it was given. ".repeat(3);
    let view = move |_: &App| -> El<App> {
        div()
            .child(div().key(1u32).w(10.0).h(10.0))
            .child(
                col()
                    .key(2u32)
                    .max_w(200.0)
                    .child(text(long.clone()))
                    .child(div().key(3u32).h(10.0))
                    .popover(1u32, Side::Below, Align::Start, 0.0, |_: &mut App| {}),
            )
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    let panel = rect_of(2u32).expect("the panel was laid out");
    let last = rect_of(3u32).expect("the last row was laid out");
    assert!((panel.w - 200.0).abs() < 1.0, "{panel:?}");
    assert!(panel.h > 60.0, "the text wraps to several lines: {panel:?}");
    assert!(last.bottom() <= panel.bottom() + 0.5, "the last row is inside: {last:?} {panel:?}");
}

#[test]
fn the_wheel_goes_on_to_the_outer_scroller_once_the_inner_one_is_at_its_end() {
    let view = |_: &App| -> El<App> {
        div().child(col().key(10u32).w(200.0).h(100.0).scroll_y().child(
            col().w(200.0).shrink0().child(col().key(20u32).w(200.0).h(60.0).shrink0().scroll_y().child(div().h(100.0).shrink0())).child(div().h(300.0).shrink0()),
        ))
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.input.pointer = Some((50.0, 30.0));
    rig.frame(&mut app, &view);
    let wheel = |rig: &mut Rig, app: &mut App, delta: f32| {
        rig.input.scroll = delta;
        rig.input.scroll_is_pixels = true;
        rig.frame(app, &view);
        rig.input.scroll = 0.0;
        rig.frame(app, &view);
    };
    let offsets = || (scroll_state(10u32).map_or(0.0, |s| s.0), scroll_state(20u32).map_or(0.0, |s| s.0));

    // The inner one has 40 of room: it takes this all, and the outer one stays put.
    wheel(&mut rig, &mut app, -30.0);
    assert_eq!(offsets(), (0.0, 30.0));
    // It has 10 left, and the rest of the tick moves the outer one.
    wheel(&mut rig, &mut app, -30.0);
    assert_eq!(offsets(), (20.0, 40.0));
    // Back up: the inner one still under the pointer unwinds first.
    wheel(&mut rig, &mut app, 25.0);
    assert_eq!(offsets(), (20.0, 15.0));
}

#[test]
fn what_is_invisible_keeps_its_box_and_runs_but_is_not_drawn_and_takes_no_press() {
    let ran = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = ran.clone();
    let view = move |app: &App| {
        let counted = counted.clone();
        let tile = div::<App>()
            .key("tile")
            .absolute()
            .left(0.0)
            .top(0.0)
            .size(200.0, 200.0)
            .bg(fluent::Color::WHITE)
            .on_click(|app: &mut App| app.clicks.push("tile"))
            .child(text("under"))
            .child(div().absolute().left(0.0).top(0.0).size(10.0, 10.0).bg(fluent::Color::WHITE).opacity(0.5))
            .child(custom::<App>(move |_, _, _| {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }).size(10.0, 10.0));
        div().size(800.0, 600.0).child(if app.open { tile.invisible() } else { tile })
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    assert!(rig.scene.counts().0 > 0 && rig.scene.counts().1 > 0);

    app.open = true;
    let before = ran.load(std::sync::atomic::Ordering::Relaxed);
    rig.frame(&mut app, &view);
    assert_eq!(rig.scene.counts(), (0, 0), "nothing of it is left to draw");
    assert!(ran.load(std::sync::atomic::Ordering::Relaxed) > before, "what keeps state as it paints goes on keeping it");
    rect_near(rect_of("tile").expect("its box"), 0.0, 0.0, 200.0, 200.0);
    rig.click(&mut app, &view, 50.0, 50.0);
    assert!(app.clicks.is_empty(), "{:?}", app.clicks);
}

#[test]
fn what_is_invisible_spends_no_layers_on_what_comes_after_it() {
    let tile = |key: &'static str| div::<App>().key(key).absolute().left(0.0).top(0.0).size(100.0, 100.0).bg(fluent::Color::WHITE);
    let layer_of_last = |rig: &Rig| rig.scene.layers.iter().rposition(|layer| !layer.rects.is_empty());
    let (mut rig, mut app) = (Rig::new(), App::default());
    let shown = |_: &App| div::<App>().size(800.0, 600.0).child(tile("under").child(div().absolute().size(1.0, 1.0).bg(fluent::Color::WHITE))).child(tile("over"));
    rig.frame(&mut app, &shown);
    let spent = layer_of_last(&rig);
    let hidden = |_: &App| div::<App>().size(800.0, 600.0).child(tile("under").child(div().absolute().size(1.0, 1.0).bg(fluent::Color::WHITE)).invisible()).child(tile("over"));
    rig.frame(&mut app, &hidden);
    assert!(layer_of_last(&rig) < spent, "{:?} against {spent:?}", layer_of_last(&rig));
}

#[test]
fn what_turns_invisible_gives_up_the_focus_and_is_passed_by() {
    let view = |app: &App| {
        let covered = div::<App>().child(button("inside"));
        col::<App>().child(if app.open { covered.invisible() } else { covered }).child(button("outside"))
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("inside"));
    app.open = true;
    rig.frame(&mut app, &view);
    assert_eq!(rig.mem.focus, 0, "keys must not go on landing in something nobody can see");
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("outside"));
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("outside"), "the only stop left");
}

#[test]
fn a_surface_drawn_after_many_others_still_lies_over_their_text() {
    let view = |_: &App| {
        let many = (0..120).map(|_| div::<App>().absolute().left(0.0).top(0.0).size(100.0, 20.0).child(text("under"))).collect::<Vec<_>>();
        div::<App>().size(800.0, 600.0).children(many).child(div().absolute().left(0.0).top(0.0).size(800.0, 600.0).bg(fluent::Color::WHITE))
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    let text_top = rig.scene.layers.iter().rposition(|layer| !layer.glyphs.is_empty()).expect("text");
    let surface = rig.scene.layers.iter().rposition(|layer| !layer.rects.is_empty()).expect("the surface");
    assert!(surface > text_top, "a layer draws its boxes first, so sharing one puts the text over the surface: {surface} {text_top}");
}

#[test]
fn a_dialog_over_a_dialog_that_spent_many_layers_still_lies_over_its_text() {
    let view = |_: &App| {
        let many = (0..120).map(|_| div::<App>().absolute().left(0.0).top(0.0).size(100.0, 20.0).child(text("under"))).collect::<Vec<_>>();
        let lower = dialog(400.0, div().h(300.0).children(many), None);
        let upper = dialog(200.0, div().h(100.0), None);
        div::<App>().size(800.0, 600.0).child(lower).child(upper)
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    let text_top = rig.scene.layers.iter().rposition(|layer| !layer.glyphs.is_empty()).expect("text");
    let surface = rig.scene.layers.iter().rposition(|layer| layer.rects.iter().any(|inst| inst.rect[2] == 200.0)).expect("the upper dialog");
    assert!(surface > text_top, "{surface} {text_top}");
}

#[test]
fn a_flyout_over_many_others_still_lies_over_their_text() {
    let view = |_: &App| {
        let many = (0..120).map(|_| div::<App>().absolute().left(0.0).top(0.0).size(100.0, 20.0).child(text("under"))).collect::<Vec<_>>();
        let lower = flyout_panel::<App>().w(300.0).h(200.0).children(many).popover_at(10.0, 10.0, |_| {});
        let upper = flyout_panel::<App>().w(150.0).h(100.0).popover_at(10.0, 10.0, |_| {});
        div::<App>().size(800.0, 600.0).child(lower).child(upper)
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    let text_top = rig.scene.layers.iter().rposition(|layer| !layer.glyphs.is_empty()).expect("text");
    let surface = rig.scene.layers.iter().rposition(|layer| layer.rects.iter().any(|inst| inst.rect[2] == 150.0)).expect("the upper flyout");
    assert!(surface > text_top, "{surface} {text_top}");
}

#[test]
fn a_fade_and_an_invisible_reach_the_layers_made_after_they_began() {
    let lid = fluent::Color::rgb(0x123456);
    let shown = |fade: f32, hide: bool| {
        move |_: &App| {
            let faded = div::<App>().opacity(fade).child(div().absolute().left(0.0).top(0.0).size(50.0, 50.0).bg(lid));
            let hidden = div::<App>().child(div().absolute().left(100.0).top(0.0).size(60.0, 60.0).bg(lid));
            div::<App>().size(800.0, 600.0).child(faded).child(if hide { hidden.invisible() } else { hidden })
        }
    };
    let drawn = |rig: &Rig, width: f32| rig.scene.layers.iter().flat_map(|layer| &layer.rects).find(|inst| inst.rect[2] == width).map(|inst| inst.fill.a);
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &shown(0.5, true));
    assert_eq!(drawn(&rig, 50.0), Some(128));
    assert_eq!(drawn(&rig, 60.0), None);
    rig.frame(&mut app, &shown(1.0, false));
    assert_eq!(drawn(&rig, 50.0), Some(255));
    assert_eq!(drawn(&rig, 60.0), Some(255));
}

#[test]
fn what_floats_over_the_page_is_solid_in_either_theme() {
    for theme in [fluent::Theme::dark(), fluent::Theme::light()] {
        assert_eq!(theme.flyout.a, 255);
        assert_eq!(theme.flyout_stroke.a, 255);
    }
}

fn button(key: &'static str) -> El<App> {
    div().key(key).w(60.0).h(20.0).on_click(move |app: &mut App| app.clicks.push(key))
}

#[test]
fn tab_walks_the_controls_in_document_order_and_comes_round() {
    let view = |_: &App| col::<App>().child(button("a")).child(row().child(button("b")).child(div().key("not a control").w(60.0).h(20.0))).child(button("c"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    assert_eq!(rig.mem.focus, 0);
    for expected in ["a", "b", "c", "a"] {
        rig.key(&mut app, &view, Key::Tab, false);
        assert_eq!(rig.mem.focus, hash_key(expected), "{expected}");
        assert!(focus_visible());
    }
    rig.key(&mut app, &view, Key::Tab, true);
    assert_eq!(rig.mem.focus, hash_key("c"));
    rig.key(&mut app, &view, Key::Tab, true);
    assert_eq!(rig.mem.focus, hash_key("b"));
}

#[test]
fn what_cannot_be_used_is_not_a_stop() {
    let view = |_: &App| col::<App>().child(button("a")).child(button("off").disabled(true)).child(div().inert().child(button("under"))).child(button("hidden").w(0.0)).child(button("b"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Tab, false);
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("b"));
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("a"));
}

#[test]
fn a_click_that_only_stops_the_click_is_not_a_stop() {
    let view = |_: &App| col::<App>().child(div().key("panel").role(Role::GenericContainer).on_click(|_: &mut App| {}).child(button("a"))).child(button("b"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    for expected in ["a", "b", "a"] {
        rig.key(&mut app, &view, Key::Tab, false);
        assert_eq!(rig.mem.focus, hash_key(expected), "{expected}");
    }
}

#[test]
fn tab_passes_by_what_says_so() {
    let view = |_: &App| col::<App>().child(button("a")).child(button("row").focusable().skip_tab(true)).child(button("b"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Tab, false);
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("b"));
    // It can still hold the focus, and Tab goes on from there.
    rig.click(&mut app, &view, 5.0, 25.0);
    assert_eq!(rig.mem.focus, hash_key("row"));
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("a"), "from a passed-by element the walk starts over");
}

#[test]
fn a_dialog_keeps_the_focus_inside_it() {
    let view = |_: &App| col::<App>().child(button("behind")).child(div().modal_layer(Some(std::rc::Rc::new(|_: &mut App| {}))).on_click(|_: &mut App| {}).child(button("one")).child(button("two")));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    for expected in ["one", "two", "one"] {
        rig.key(&mut app, &view, Key::Tab, false);
        assert_eq!(rig.mem.focus, hash_key(expected), "{expected}");
    }
}

#[test]
fn enter_and_space_press_what_holds_the_focus() {
    let view = |_: &App| col::<App>().child(button("a"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Enter, false);
    assert!(app.clicks.is_empty(), "nothing holds the focus yet");
    rig.key(&mut app, &view, Key::Tab, false);
    rig.key(&mut app, &view, Key::Enter, false);
    rig.key(&mut app, &view, Key::Char(' '), false);
    assert_eq!(app.clicks, ["a", "a"]);
    rig.key(&mut app, &view, Key::Char('x'), false);
    assert_eq!(app.clicks.len(), 2);
    rig.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: true, alt: false, ctrl: false });
    rig.frame(&mut app, &view);
    assert_eq!(app.clicks.len(), 2, "a chord is a shortcut's");
}

#[test]
fn what_hears_a_key_first_can_keep_it_from_its_default() {
    let view = |_: &App| col::<App>().child(button("a").on_key(|app: &mut App, press| {
        app.clicks.push("heard");
        press.key == Key::Enter
    })).child(button("b"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Tab, false);
    rig.key(&mut app, &view, Key::Enter, false);
    assert_eq!(app.clicks, ["heard"]);
    rig.key(&mut app, &view, Key::Char(' '), false);
    assert_eq!(app.clicks, ["heard", "heard", "a"], "not taken, so it goes on to the default");
}

#[test]
fn a_text_field_is_a_stop_and_its_keys_stay_its_own() {
    let view = |_: &App| col::<App>().child(button("a")).child(field::<App>("name", "").build()).child(button("b"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Tab, false);
    rig.key(&mut app, &view, Key::Tab, false);
    let in_field = rig.mem.focus;
    assert_ne!(in_field, hash_key("a"));
    assert_ne!(in_field, hash_key("b"));
    assert!(text_input_focused());
    rig.key(&mut app, &view, Key::Enter, false);
    rig.key(&mut app, &view, Key::Char(' '), false);
    assert!(app.clicks.is_empty());
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("b"));
    rig.key(&mut app, &view, Key::Tab, true);
    assert_eq!(rig.mem.focus, in_field);
}

#[test]
fn a_widget_that_wants_tab_keeps_it() {
    let view = |_: &App| col::<App>().child(button("a")).child(custom::<App>(|_, _, _| {}).key("terminal").w(60.0).h(20.0).focus_under(77).keeps_tab()).child(button("b"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Tab, false);
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, 77, "it can be reached");
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, 77, "and Tab stays there, for the shell");
}

#[test]
fn focus_only_shows_for_the_keyboard() {
    let view = |_: &App| col::<App>().child(button("a")).child(button("b").focusable());
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.key(&mut app, &view, Key::Tab, false);
    assert!(focus_visible());
    rig.click(&mut app, &view, 5.0, 25.0);
    assert_eq!(rig.mem.focus, hash_key("b"));
    assert!(!focus_visible(), "a press takes it away");
}

#[test]
fn tab_starts_from_what_was_last_pressed() {
    let view = |_: &App| col::<App>().child(button("a")).child(button("b")).child(button("c"));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    rig.click(&mut app, &view, 5.0, 25.0);
    assert_eq!(rig.mem.focus, 0, "a plain button doesn't keep focus on a press");
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("c"));
}

#[test]
fn a_stop_out_of_view_is_scrolled_to() {
    let view = |_: &App| col::<App>().h(100.0).child(col().key("list").h(100.0).scroll_y().children((0..20).map(|i| div().key(("row", i)).h(30.0).shrink0().on_click(|_: &mut App| {}))));
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    for _ in 0..8 {
        rig.key(&mut app, &view, Key::Tab, false);
    }
    assert_eq!(rig.mem.focus, hash_key(("row", 7)));
    for _ in 0..60 {
        rig.frame(&mut app, &view);
    }
    let row = rect_of(("row", 7)).expect("laid out");
    assert!(row.y >= 0.0 && row.bottom() <= 100.5, "{row:?}");
}

#[test]
fn what_shows_under_a_pointer_shows_for_the_keyboard() {
    use std::cell::Cell;
    use std::rc::Rc;
    let painted = Rc::new(Cell::new(false));
    let view = {
        let painted = painted.clone();
        move |_: &App| {
            let painted = painted.clone();
            row::<App>().child(button("a")).child(button("action").reveal_on_parent_hover().child(custom::<App>(move |_, _, _| painted.set(true)).w(4.0).h(4.0)))
        }
    };
    let (mut rig, mut app) = (Rig::new(), App::default());
    rig.frame(&mut app, &view);
    assert!(!painted.get());
    rig.key(&mut app, &view, Key::Tab, false);
    assert!(!painted.get());
    rig.key(&mut app, &view, Key::Tab, false);
    assert_eq!(rig.mem.focus, hash_key("action"));
    assert!(painted.get());
}

#[test]
fn a_sideways_scroller_moves_what_does_not_fit_and_stops_at_its_ends() {
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| -> El<App> {
        let content = row().child(div().key("first").w(300.0).h(30.0).shrink0()).child(div().key("last").w(300.0).h(30.0).shrink0());
        div().child(col().w(400.0).child(sideways("strip", content)))
    };
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    let start = rect_of("last").unwrap();
    assert_eq!(start.x, 300.0, "laid out at its natural width, not squeezed or wrapped");
    rig.input.pointer = Some((100.0, 10.0));
    rig.frame(&mut app, &view);
    rig.input.scroll_x = -50.0;
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert_eq!(rect_of("last").unwrap().x, 250.0, "the wheel moves it");
    // As a browser reads it, Shift turns the plain wheel sideways.
    rig.input.shift = true;
    rig.input.scroll = -20.0;
    rig.input.scroll_is_pixels = true;
    rig.frame(&mut app, &view);
    rig.input.shift = false;
    rig.frame(&mut app, &view);
    assert_eq!(rect_of("last").unwrap().x, 230.0);
    rig.input.scroll_x = -100_000.0;
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert_eq!(rect_of("last").unwrap().right(), 400.0, "and it stops at the end");
    rig.input.scroll_x = 100_000.0;
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert_eq!(rect_of("first").unwrap().x, 0.0, "and at the start");
}

#[test]
fn a_select_can_set_each_option_in_its_own_family() {
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| -> El<App> {
        select_in_fonts("fonts", vec![Arc::from("Default"), Arc::from("Menlo")], vec![None, Some(Arc::from("Menlo"))], 0, |_: &mut App, _| {}).w(200.0)
    };
    rig.frame(&mut app, &view);
    let anchor = rect_of(hash_key(("select", hash_key("fonts")))).expect("the trigger");
    rig.click(&mut app, &view, anchor.x + 20.0, anchor.y + 10.0);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rect_of(("select-panel", hash_key(("select", hash_key("fonts"))))).is_some(), "it opened, and paints without a word in the interface font's stack failing");
}

#[test]
fn hovering_something_with_a_tooltip_draws_no_text_for_it() {
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| div::<App>().child(icon_button::<App>("X", "Close the window").key("close")).child(button("stop").tooltip("Stops the run"));
    rig.frame(&mut app, &view);
    for key in ["close", "stop"] {
        assert_eq!(rig.rest_on(&mut app, &view, key), 0, "{key}");
    }
}

// No spaces, so that a glyph is drawn for each character.
const LONG: &str = "/Users/somebody/projects/a-very-long-directory-name/that-goes-on/and-on/and-on/until-it-can-not-fit.rs";

#[test]
fn a_line_too_long_for_its_box_comes_up_whole_on_hover_and_one_that_fits_does_not() {
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| col::<App>().child(text(LONG).key("cut").ellipsis().w(120.0)).child(text("short").key("fits").ellipsis().w(120.0));
    assert_eq!(rig.rest_on(&mut app, &view, "fits"), 0);
    rig.input.pointer = None;
    assert_eq!(rig.rest_on(&mut app, &view, "cut"), LONG.chars().count());
}

#[test]
fn hover_text_waits_for_the_pointer_to_rest() {
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| col::<App>().child(text(LONG).key("cut").ellipsis().w(120.0)).child(text(LONG).key("other").ellipsis().w(120.0));
    rig.frame(&mut app, &view);
    let (first, other) = (rect_of("cut").expect("laid out"), rect_of("other").expect("laid out"));
    rig.input.pointer = Some((first.x + 4.0, first.y + 4.0));
    rig.frame(&mut app, &view);
    rig.time += 0.3;
    rig.frame(&mut app, &view);
    assert_eq!(rig.hover_glyphs(), 0);
    // Moving on to another line starts over.
    rig.input.pointer = Some((other.x + 4.0, other.y + 4.0));
    rig.time += 0.3;
    rig.frame(&mut app, &view);
    assert_eq!(rig.hover_glyphs(), 0);
    rig.time += 0.5;
    rig.frame(&mut app, &view);
    assert_eq!(rig.hover_glyphs(), LONG.chars().count());
}

#[test]
fn what_a_line_is_cut_from_comes_up_in_its_place_and_only_while_it_is_cut() {
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| {
        col::<App>()
            .child(row::<App>().full_text(LONG).w(120.0).child(text("a-name-a-good-deal-wider-than-its-box-here.rs").key("cut").ellipsis()))
            .child(row::<App>().full_text(LONG).w(120.0).child(text("name.rs").key("fits").ellipsis()))
    };
    assert_eq!(rig.rest_on(&mut app, &view, "fits"), 0);
    rig.input.pointer = None;
    assert_eq!(rig.rest_on(&mut app, &view, "cut"), LONG.chars().count());
}

#[test]
fn hover_text_beside_cut_text_gives_way_to_it_only_while_the_line_is_cut() {
    const WHERE: &str = "/somewhere/else";
    const BOTH: &str = "both-of-them-at-once";
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| {
        col::<App>()
            .child(text("name.rs").key("fits").ellipsis().w(120.0).hover_text(WHERE).cut_text(BOTH))
            .child(text(LONG).key("cut").ellipsis().w(120.0).hover_text(WHERE).cut_text(BOTH))
    };
    assert_eq!(rig.rest_on(&mut app, &view, "fits"), WHERE.chars().count());
    rig.input.pointer = None;
    assert_eq!(rig.rest_on(&mut app, &view, "cut"), BOTH.chars().count());
}

#[test]
fn a_disabled_reason_comes_up_with_or_without_a_click_handler_and_a_plain_name_does_not() {
    const WHY: &str = "Project-is-being-deleted";
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| {
        col::<App>()
            .child(div::<App>().key("still").size(40.0, 30.0).disabled(true).disabled_reason(WHY))
            .child(button("pressable").disabled(true).on_click(|_: &mut App| {}).disabled_reason(WHY))
            .child(button("named").disabled(true).tooltip("Delete"))
            .child(div::<App>().key("none").size(40.0, 30.0).maybe_disabled_reason(None::<&str>))
    };
    assert_eq!(rig.rest_on(&mut app, &view, "still"), WHY.chars().count());
    rig.input.pointer = None;
    assert_eq!(rig.rest_on(&mut app, &view, "pressable"), WHY.chars().count());
    rig.input.pointer = None;
    assert_eq!(rig.rest_on(&mut app, &view, "named"), 0);
    rig.input.pointer = None;
    assert_eq!(rig.rest_on(&mut app, &view, "none"), 0);
}

#[test]
fn a_press_ends_hover_text_until_the_pointer_moves_on_but_not_on_something_disabled() {
    const WHY: &str = "Not-now";
    let mut rig = Rig::new();
    let mut app = App::default();
    let view = |_: &App| {
        col::<App>()
            .child(row::<App>().key("live").w(120.0).on_click(|_: &mut App| {}).child(text(LONG).ellipsis()))
            .child(div::<App>().key("away").size(40.0, 30.0))
            .child(div::<App>().key("still").size(40.0, 30.0).disabled(true).disabled_reason(WHY))
    };
    assert_eq!(rig.rest_on(&mut app, &view, "live"), LONG.chars().count());
    let (x, y) = rig.input.pointer.expect("resting");
    rig.click(&mut app, &view, x, y);
    rig.time += 1.0;
    rig.frame(&mut app, &view);
    assert_eq!(rig.hover_glyphs(), 0, "after the press");
    let away = rect_of("away").expect("laid out");
    rig.input.pointer = Some((away.x + 2.0, away.y + 2.0));
    rig.frame(&mut app, &view);
    rig.input.pointer = Some((x, y));
    rig.frame(&mut app, &view);
    rig.time += 1.0;
    rig.frame(&mut app, &view);
    assert_eq!(rig.hover_glyphs(), LONG.chars().count(), "after moving off and back");

    rig.input.pointer = None;
    assert_eq!(rig.rest_on(&mut app, &view, "still"), WHY.chars().count());
    let (x, y) = rig.input.pointer.expect("resting");
    rig.click(&mut app, &view, x, y);
    rig.time += 1.0;
    rig.frame(&mut app, &view);
    assert_eq!(rig.hover_glyphs(), WHY.chars().count(), "a press does nothing on it, so the reason stays");
}

#[test]
fn hover_text_wraps_inside_the_window_and_lies_over_a_portal() {
    let words = format!("{LONG} {LONG} {LONG} {LONG} {LONG} {LONG}");
    let mut rig = Rig::new();
    let mut app = App::default();
    let shown = words.clone();
    let view = move |_: &App| {
        col::<App>().p(0.0).child(text(shown.clone()).key("cut").ellipsis().w(120.0)).child(div::<App>().popover_at(300.0, 40.0, |_: &mut App| {}).size(300.0, 200.0).bg(fluent::Color::WHITE))
    };
    rig.frame(&mut app, &view);
    let r = rect_of("cut").expect("laid out");
    rig.input.pointer = Some((r.x + 4.0, r.y + 4.0));
    rig.frame(&mut app, &view);
    rig.time += 1.0;
    rig.frame(&mut app, &view);
    let glyphs = &rig.scene.layers[fluent::gpu::LAYER_TOOLTIP].glyphs;
    assert!(!glyphs.is_empty());
    let distinct: std::collections::BTreeSet<i32> = glyphs.iter().map(|g| g.pos[1] as i32).collect();
    assert!(distinct.len() > 1, "more than one line: {distinct:?}");
    for g in glyphs {
        assert!(g.pos[0] >= 0.0 && g.pos[0] < 800.0 && g.pos[1] >= 0.0 && g.pos[1] < 600.0, "{:?}", g.pos);
    }
}
