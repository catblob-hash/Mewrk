//! Draws every page to a PNG with a real renderer and a real host in a temp folder, to look at the layout:
//! `cargo run -p mewrk-settings-global --example shots -- OUT_DIR`. It stays off the network and off the keychain.

use std::sync::Arc;
use std::time::Duration;

use fluent::gpu::SURFACE_FORMAT;
use fluent::ui::{Input, NoClipboard, UiMemory};
use fluent::{Gpu, Renderer, Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::StartOptions;
use mewrk_settings_global::providers::state;
use mewrk_settings_global::{global_settings_window, open, themed, window_backdrop, SettingsView};

fn save_png(gpu: &Gpu, texture: &wgpu::Texture, size: (u32, u32), path: &str) {
    let row = size.0 * 4;
    let aligned = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: Some("readback"), size: (aligned * size.1) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(aligned), rows_per_image: None } },
        wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = slice.get_mapped_range().unwrap();
    let mut rgba = Vec::with_capacity((row * size.1) as usize);
    for y in 0..size.1 as usize {
        for px in data[y * aligned as usize..y * aligned as usize + row as usize].chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    image::save_buffer(path, &rgba, size.0, size.1, image::ColorType::Rgba8).unwrap();
}

fn main() {
    let out = std::env::args().nth(1).expect("an output folder");
    std::fs::create_dir_all(&out).unwrap();
    // Status reads go to a store that holds nothing, never the real keychain.
    keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
    let dir = std::env::temp_dir().join(format!("mewrk-settings-shots-{}", std::process::id()));
    let mut app = App::start(StartOptions { identifier: "dev.mewrk.settings-shots".into(), version: "0.0.1".into(), data_dir: dir.clone(), resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let gpu = Arc::new(Gpu::new(instance, None).unwrap());
    let mut renderer = Renderer::new(gpu.clone());
    let scale = 2.0;
    // An optional logical size, to look at the narrow layouts: `shots OUT 700 520`.
    let logical = |i: usize, fallback: u32| std::env::args().nth(i).and_then(|v| v.parse::<u32>().ok()).unwrap_or(fallback);
    let size_px = (logical(2, 1280) * 2, logical(3, 900) * 2);
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shots"), size: wgpu::Extent3d { width: size_px.0, height: size_px.1, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: SURFACE_FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
    let view = target.create_view(&Default::default());
    let (mut scene, mut text, mut mem, input, mut clipboard) = (Scene::default(), TextCtx::new(scale), UiMemory::default(), Input::default(), NoClipboard);
    let size = (size_px.0 as f32 / scale, size_px.1 as f32 / scale);
    // The seed selects Claude Agent, whose panel starts the CLI. A custom row is what a provider's page is about anyway.
    let relay = mewrk_settings_global::logic::provider_ops::create_provider("Relay", mewrk_client::model::ProviderFamily::OpenaiCompatible);
    let relay_id = relay.id.clone();
    mewrk_settings_global::logic::store::change(&mut app, move |doc| {
        let mut relay = relay;
        relay.models = ["gpt-5", "gpt-4o-mini", "qwen-mini"].iter().map(|id| serde_json::from_value(json!({ "id": id, "capabilities": ["image_recognition"] })).unwrap()).collect();
        relay.active_model_id = Some("gpt-5".into());
        doc.assets.api_providers.insert(0, relay);
    });
    let s = state(&mut app);
    s.started = true;
    s.selected_id = Some(relay_id);
    let extra: [(&str, fn(&mut App)); 3] = [
        ("add-provider", |app| state(app).adding_provider = true),
        ("model-editor", |app| {
            state(app).adding_provider = false;
            let provider = app.doc.assets.api_providers[0].clone();
            let draft = provider.models[0].clone();
            state(app).model_editor = Some(mewrk_settings_global::providers::ModelEditor { create: false, provider_id: provider.id.clone(), original_id: Some(draft.id.clone()), draft, append_touched: true });
        }),
        ("options", |app| {
            state(app).model_editor = None;
            state(app).options_open = true;
        }),
    ];
    let pages = [(SettingsView::Appearance, "appearance"), (SettingsView::Dependencies, "dependencies"), (SettingsView::SearchProviders, "search"), (SettingsView::Shortcuts, "shortcuts"), (SettingsView::Usage, "usage"), (SettingsView::Updates, "updates"), (SettingsView::Permissions, "permissions"), (SettingsView::Providers, "providers")];
    for dark in [true] {
        for (page, name) in pages {
            open(&mut app, page);
            for frame in 0..40 {
                let base = if dark { Theme::dark() } else { Theme::light() };
                let theme = themed(base, &app.doc.global_settings.appearance);
                text.set_scale(scale);
                scene.clear();
                let mut ui = Ui::new(&mut scene, &mut text, theme, &input, &mut mem, &mut clipboard, scale, size, 0.016, frame as f32 * 0.016);
                fluent::el::set_theme(theme);
                fluent::el::set_window_size(size.0, size.1);
                app.tick();
                let root: E = div().w_full().h_full().child(window_backdrop(&mut app)).child(global_settings_window(&mut app));
                fluent::el::render(root, &mut app, &mut ui);
                drop(ui);
                text.flush_atlases(&renderer);
                let submission = renderer.render(&scene, &theme, &view, size_px);
                let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
                std::thread::sleep(Duration::from_millis(15));
            }
            let path = format!("{out}/{name}-{}.png", if dark { "dark" } else { "light" });
            save_png(&gpu, &target, size_px, &path);
            println!("wrote {path}");
        }
    }
    open(&mut app, SettingsView::Providers);
    for (name, prepare) in extra {
        prepare(&mut app);
        for frame in 0..20 {
            let theme = themed(Theme::dark(), &app.doc.global_settings.appearance);
            scene.clear();
            let mut ui = Ui::new(&mut scene, &mut text, theme, &input, &mut mem, &mut clipboard, scale, size, 0.016, frame as f32 * 0.016);
            fluent::el::set_theme(theme);
            fluent::el::set_window_size(size.0, size.1);
            app.tick();
            let root: E = div().w_full().h_full().child(global_settings_window(&mut app));
            fluent::el::render(root, &mut app, &mut ui);
            drop(ui);
            text.flush_atlases(&renderer);
            let submission = renderer.render(&scene, &theme, &view, size_px);
            let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
        }
        let path = format!("{out}/{name}.png");
        save_png(&gpu, &target, size_px, &path);
        println!("wrote {path}");
    }
    // The backdrop: a built-in picture, decoded off the UI thread and uploaded through fluent's image queue.
    state(&mut app).options_open = false;
    mewrk_settings_global::logic::store::change(&mut app, |doc| doc.global_settings.appearance.background = "builtin:desk".into());
    mewrk_settings_global::close(&mut app);
    for frame in 0..4000 {
        let theme = themed(Theme::dark(), &app.doc.global_settings.appearance);
        scene.clear();
        let mut ui = Ui::new(&mut scene, &mut text, theme, &input, &mut mem, &mut clipboard, scale, size, 0.016, frame as f32 * 0.016);
        fluent::el::set_theme(theme);
        fluent::el::set_window_size(size.0, size.1);
        app.tick();
        let root: E = div().w_full().h_full().child(window_backdrop(&mut app));
        fluent::el::render(root, &mut app, &mut ui);
        drop(ui);
        text.flush_atlases(&renderer);
        let submission = renderer.render(&scene, &theme, &view, size_px);
        let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
        if mewrk_settings_global::effects::backdrop_size(&mut app).is_some() && frame > 2 {
            // One more frame, now that the upload has been drained.
            scene.clear();
            let mut ui = Ui::new(&mut scene, &mut text, theme, &input, &mut mem, &mut clipboard, scale, size, 0.016, 0.0);
            let root: E = div().w_full().h_full().child(window_backdrop(&mut app));
            fluent::el::render(root, &mut app, &mut ui);
            drop(ui);
            let submission = renderer.render(&scene, &theme, &view, size_px);
            let _ = gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let path = format!("{out}/backdrop.png");
    save_png(&gpu, &target, size_px, &path);
    println!("wrote {path}");
    let _ = std::fs::remove_dir_all(dir);
    std::process::exit(0);
}
