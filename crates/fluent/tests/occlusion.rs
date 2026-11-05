//! `Scene::cull_occluded`, on scenes drawn by hand.

use fluent::gpu::{ImageId, LAYER_MODAL, LAYER_POPUP};
use fluent::ui::{rect, Input, NoClipboard, UiMemory};
use fluent::{Color, Scene, TextCtx, Theme, Ui};

const WINDOW: (u32, u32) = (800, 600);

fn paint(draw: impl FnOnce(&mut Ui<'_>)) -> Scene {
    let mut scene = Scene::default();
    let (mut text, mut mem, input, mut clipboard) = (TextCtx::new(1.0), UiMemory::default(), Input::default(), NoClipboard);
    let mut ui = Ui::new(&mut scene, &mut text, Theme::dark(), &input, &mut mem, &mut clipboard, 1.0, (WINDOW.0 as f32, WINDOW.1 as f32), 0.016, 0.0);
    draw(&mut ui);
    drop(ui);
    scene.cull_occluded(WINDOW);
    scene
}

fn solid(ui: &mut Ui<'_>, x: f32, y: f32, w: f32, h: f32) {
    ui.fill(rect(x, y, w, h), 0.0, Color::rgb(0x202020));
}

fn on_layer(ui: &mut Ui<'_>, layer: usize, draw: impl FnOnce(&mut Ui<'_>)) {
    let saved = ui.layer;
    ui.layer = layer;
    draw(ui);
    ui.layer = saved;
}

/// What is drawn of each box of a colour: its clips.
fn clips_of(scene: &Scene, color: Color) -> Vec<[f32; 4]> {
    scene.layers.iter().flat_map(|layer| &layer.rects).filter(|inst| inst.fill == color).map(|inst| inst.clip).collect()
}

const PAGE: Color = Color::rgb(0x112233);

#[test]
fn what_lies_wholly_under_a_solid_surface_is_not_drawn() {
    let scene = paint(|ui| {
        ui.fill(rect(150.0, 150.0, 100.0, 100.0), 0.0, PAGE);
        ui.label(160.0, 160.0, "covered", &fluent::theme::BODY, Color::WHITE);
        ui.image(ImageId(1), rect(160.0, 180.0, 40.0, 40.0), 0.0, 1.0);
        on_layer(ui, LAYER_MODAL, |ui| solid(ui, 100.0, 100.0, 400.0, 300.0));
    });
    assert!(clips_of(&scene, PAGE).is_empty());
    let page = &scene.layers[0];
    assert!(page.glyphs.is_empty() && page.images.is_empty(), "{} {}", page.glyphs.len(), page.images.len());
    assert_eq!(scene.layers[LAYER_MODAL].rects.len(), 1);
}

#[test]
fn what_lies_over_a_surface_or_is_drawn_after_it_in_its_layer_stays() {
    let scene = paint(|ui| {
        on_layer(ui, LAYER_MODAL, |ui| {
            ui.label(160.0, 160.0, "before its box, drawn after", &fluent::theme::BODY, Color::WHITE);
            ui.fill(rect(110.0, 110.0, 50.0, 50.0), 0.0, PAGE);
            solid(ui, 100.0, 100.0, 400.0, 300.0);
            ui.fill(rect(120.0, 120.0, 50.0, 50.0), 0.0, Color::WHITE);
        });
        on_layer(ui, LAYER_POPUP, |ui| ui.fill(rect(130.0, 130.0, 50.0, 50.0), 0.0, Color::BLACK));
    });
    let modal = &scene.layers[LAYER_MODAL];
    assert!(!modal.glyphs.is_empty(), "a layer draws its text after its boxes");
    assert!(clips_of(&scene, PAGE).is_empty(), "this box was drawn before the surface");
    assert_eq!(clips_of(&scene, Color::WHITE).len(), 1);
    assert_eq!(clips_of(&scene, Color::BLACK).len(), 1);
}

#[test]
fn a_surface_that_can_be_seen_through_hides_nothing() {
    for lid in [Color::rgb(0x202020).with_alpha(0.96), Color::TRANSPARENT] {
        let scene = paint(|ui| {
            ui.fill(rect(150.0, 150.0, 100.0, 100.0), 0.0, PAGE);
            ui.label(160.0, 160.0, "seen", &fluent::theme::BODY, Color::WHITE);
            on_layer(ui, LAYER_MODAL, |ui| ui.fill(rect(100.0, 100.0, 400.0, 300.0), 0.0, lid));
        });
        assert_eq!(clips_of(&scene, PAGE).len(), 1);
        assert!(!scene.layers[0].glyphs.is_empty());
    }
    // A gradient is only as solid as its faintest end.
    let scene = paint(|ui| {
        ui.fill(rect(150.0, 150.0, 100.0, 100.0), 0.0, PAGE);
        on_layer(ui, LAYER_MODAL, |ui| ui.fill_gradient(rect(100.0, 100.0, 400.0, 300.0), [0.0; 4], Color::WHITE, Color::WHITE.fade(0.0)));
    });
    assert_eq!(clips_of(&scene, PAGE).len(), 1);
}

#[test]
fn a_surfaces_corner_lets_through_what_shows_through_it() {
    let scene = paint(|ui| {
        // In the corner of a round box, where its fill does not reach.
        ui.fill(rect(101.0, 101.0, 4.0, 4.0), 0.0, PAGE);
        // Well inside it.
        ui.fill(rect(140.0, 140.0, 4.0, 4.0), 0.0, Color::WHITE);
        on_layer(ui, LAYER_MODAL, |ui| ui.fill(rect(100.0, 100.0, 400.0, 300.0), 20.0, Color::rgb(0x202020)));
    });
    assert_eq!(clips_of(&scene, PAGE).len(), 1);
    assert!(clips_of(&scene, Color::WHITE).is_empty());
}

#[test]
fn a_box_only_partly_covered_is_cut_to_the_parts_that_show_in_its_place() {
    let scene = paint(|ui| {
        ui.fill(rect(0.0, 0.0, 800.0, 600.0), 0.0, PAGE);
        ui.fill(rect(10.0, 10.0, 50.0, 50.0), 0.0, Color::WHITE);
        on_layer(ui, LAYER_MODAL, |ui| solid(ui, 100.0, 100.0, 400.0, 300.0));
    });
    let parts = clips_of(&scene, PAGE);
    assert_eq!(parts.len(), 4, "{parts:?}");
    let area: f32 = parts.iter().map(|[x0, y0, x1, y1]| (x1 - x0) * (y1 - y0)).sum();
    assert_eq!(area, 800.0 * 600.0 - 400.0 * 300.0, "every pixel but the surface's, once: {parts:?}");
    for [x0, y0, x1, y1] in &parts {
        assert!(*x1 <= 100.0 || *x0 >= 500.0 || *y1 <= 100.0 || *y0 >= 400.0, "clear of the surface: {parts:?}");
    }
    let order: Vec<Color> = scene.layers[0].rects.iter().map(|inst| inst.fill).collect();
    assert_eq!(order.last(), Some(&Color::WHITE), "what was drawn after it still is: {order:?}");
    // A sliver of overlap is not worth four instances.
    let scene = paint(|ui| {
        ui.fill(rect(0.0, 0.0, 300.0, 110.0), 0.0, PAGE);
        on_layer(ui, LAYER_MODAL, |ui| solid(ui, 200.0, 100.0, 400.0, 300.0));
    });
    assert_eq!(clips_of(&scene, PAGE).len(), 1);
}

#[test]
fn a_picture_is_cut_as_a_box_is_and_goes_when_wholly_covered() {
    let scene = paint(|ui| {
        ui.image(ImageId(1), rect(0.0, 0.0, 800.0, 600.0), 0.0, 1.0);
        ui.image(ImageId(2), rect(150.0, 150.0, 50.0, 50.0), 0.0, 1.0);
        on_layer(ui, LAYER_MODAL, |ui| solid(ui, 100.0, 100.0, 400.0, 300.0));
    });
    let images = &scene.layers[0].images;
    assert_eq!(images.len(), 4, "{images:?}");
    assert!(images.iter().all(|draw| draw.image == ImageId(1) && draw.inst.uv == [0.0, 0.0, 1.0, 1.0]), "each keeps the whole picture's mapping: {images:?}");
    // A picture may be see-through, so it covers nothing.
    let scene = paint(|ui| {
        ui.fill(rect(150.0, 150.0, 100.0, 100.0), 0.0, PAGE);
        on_layer(ui, LAYER_MODAL, |ui| ui.image(ImageId(1), rect(100.0, 100.0, 400.0, 300.0), 0.0, 1.0));
    });
    assert_eq!(clips_of(&scene, PAGE).len(), 1);
}

#[test]
fn the_ground_is_only_left_out_when_something_solid_covers_the_whole_window() {
    assert!(paint(|ui| solid(ui, 0.0, 0.0, 800.0, 600.0)).ground_covered);
    assert!(!paint(|ui| solid(ui, 0.0, 0.0, 800.0, 599.0)).ground_covered);
    assert!(!paint(|ui| ui.fill(rect(0.0, 0.0, 800.0, 600.0), 10.0, Color::rgb(0x202020))).ground_covered, "its corners don't");
    assert!(!paint(|ui| ui.fill(rect(0.0, 0.0, 800.0, 600.0), 0.0, Color::rgb(0x202020).with_alpha(0.9))).ground_covered);
    let mut scene = paint(|ui| solid(ui, 0.0, 0.0, 800.0, 600.0));
    scene.clear();
    assert!(!scene.ground_covered, "a new frame starts over");
}

#[test]
fn a_surface_partly_under_another_still_covers_what_it_covers() {
    let scene = paint(|ui| {
        ui.fill(rect(320.0, 320.0, 60.0, 60.0), 0.0, PAGE);
        on_layer(ui, LAYER_MODAL, |ui| solid(ui, 100.0, 100.0, 300.0, 300.0));
        on_layer(ui, LAYER_MODAL + 1, |ui| solid(ui, 300.0, 300.0, 300.0, 200.0));
    });
    assert!(clips_of(&scene, PAGE).is_empty());
}

#[test]
fn a_solid_image_hides_what_lies_under_it_and_any_other_image_hides_nothing() {
    let page = rect(150.0, 150.0, 100.0, 100.0);
    let painted = |solid: bool| {
        paint(|ui| {
            ui.fill(page, 0.0, PAGE);
            ui.image(ImageId(1), rect(160.0, 160.0, 40.0, 40.0), 0.0, 1.0);
            on_layer(ui, 1, |ui| {
                ui.fill(rect(170.0, 170.0, 10.0, 10.0), 0.0, Color::rgb(0x445566));
                ui.image(ImageId(2), rect(100.0, 100.0, 400.0, 300.0), 0.0, 1.0);
                if solid {
                    ui.image_is_opaque();
                }
                ui.label(160.0, 160.0, "over the picture", &fluent::theme::BODY, Color::WHITE);
            });
        })
    };
    let see_through = painted(false);
    assert_eq!(clips_of(&see_through, PAGE).len(), 1);
    assert_eq!(see_through.layers[0].images.len(), 1);

    let solid = painted(true);
    assert!(clips_of(&solid, PAGE).is_empty(), "the box below it goes");
    assert!(solid.layers[0].images.is_empty(), "the image below it goes");
    assert!(clips_of(&solid, Color::rgb(0x445566)).is_empty(), "and a box in its own layer, which draws before it");
    assert_eq!(solid.layers[1].images.len(), 1, "it stays");
    assert!(!solid.layers[1].glyphs.is_empty(), "and so does the text in its own layer, which draws after it");
}

#[test]
fn a_solid_image_hides_nothing_of_its_corners_or_while_it_fades() {
    let scene = paint(|ui| {
        ui.fill(rect(100.0, 100.0, 300.0, 200.0), 0.0, PAGE);
        on_layer(ui, 1, |ui| {
            ui.image(ImageId(2), rect(100.0, 100.0, 300.0, 200.0), 20.0, 1.0);
            ui.image_is_opaque();
        });
    });
    assert!(!clips_of(&scene, PAGE).is_empty(), "the corners still show what is under them");

    let fading = paint(|ui| {
        ui.fill(rect(100.0, 100.0, 300.0, 200.0), 0.0, PAGE);
        on_layer(ui, 1, |ui| {
            ui.image(ImageId(2), rect(100.0, 100.0, 300.0, 200.0), 0.0, 0.5);
            ui.image_is_opaque();
        });
    });
    assert_eq!(clips_of(&fading, PAGE).len(), 1);
}

#[test]
fn a_solid_picture_as_large_as_the_window_means_no_ground() {
    let scene = paint(|ui| {
        ui.image(ImageId(1), rect(0.0, 0.0, WINDOW.0 as f32, WINDOW.1 as f32), 0.0, 1.0);
        ui.image_is_opaque();
    });
    assert!(scene.ground_covered);
}
