//! A decoded frame on a real GPU, with no copy on the way.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fluent::gpu::{ImageDraw, ImageInst, SURFACE_FORMAT};
use fluent::{Gpu, ImageId, Renderer, Scene, Theme};
use mewrk_motion::video::VideoReader;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// What is queued for the GPU is queued for the whole process, so whichever renderer draws next would take another test's images.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn renderer() -> Renderer {
    Renderer::new(Arc::new(Gpu::new(wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()), None).unwrap()))
}

fn read(renderer: &mut Renderer, image: ImageId, size: (u32, u32)) -> Vec<u8> {
    let gpu = renderer.gpu.clone();
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("read"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SURFACE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut scene = Scene::default();
    scene.ground_covered = true;
    let (width, height) = (size.0 as f32, size.1 as f32);
    scene.layers[0].images.push(ImageDraw { image, inst: ImageInst { rect: [0.0, 0.0, width, height], clip: [0.0, 0.0, width, height], uv: [0.0, 0.0, 1.0, 1.0], radii: [0.0; 4], opacity: [1.0, 0.0, 0.0, 0.0] }, opaque: false });
    let view = target.create_view(&Default::default());
    renderer.render(&scene, &Theme::dark(), &view, size);
    let stride = (size.0 * 4).next_multiple_of(256);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: Some("read"), size: u64::from(stride * size.1), usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(target.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: None } }, wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 });
    let submission = gpu.queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(10)) }).unwrap();
    let mapped = buffer.slice(..).get_mapped_range().unwrap();
    mapped.chunks_exact(stride as usize).flat_map(|row| row[..size.0 as usize * 4].chunks_exact(4).flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])).collect()
}

#[test]
fn a_surface_draws_its_own_pixels_and_each_new_one_replaces_the_last() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = renderer();
    let mut reader = VideoReader::open(&fixture("counting.mp4"), None, 0.0).unwrap();
    let image = fluent::queue_surface_image();
    assert!(read(&mut renderer, image, (64, 36)).chunks_exact(4).all(|pixel| pixel[..3] == [0, 0, 0]), "nothing draws until the first surface");

    let first = reader.next_frame().unwrap();
    let expected = first.to_pixels().unwrap();
    fluent::show_surface(image, first.into_surface().unwrap());
    let drawn = read(&mut renderer, image, (64, 36));
    let off: u64 = drawn.iter().zip(&expected.data).map(|(a, b)| u64::from(a.abs_diff(*b))).sum();
    assert!(off / (drawn.len() as u64) < 2, "the frame as decoded, BGRA or not: {off}");

    let mut last_red = drawn[32 * 18 * 4];
    for _ in 0..15 {
        let frame = reader.next_frame().unwrap();
        fluent::show_surface(image, frame.into_surface().unwrap());
        let red = read(&mut renderer, image, (64, 36))[32 * 18 * 4];
        assert!(red >= last_red, "{red} after {last_red}");
        last_red = red;
    }
    assert!(last_red > 200, "{last_red}");
    fluent::release_image(image);
    read(&mut renderer, image, (64, 36));
}

#[test]
fn a_surface_can_be_what_the_glass_is_worked_out_from() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = renderer();
    let mut reader = VideoReader::open(&fixture("counting.mp4"), None, 0.0).unwrap();
    let frame = reader.next_frame().unwrap();
    let source = fluent::queue_surface_image();
    fluent::show_surface(source, frame.into_surface().unwrap());
    let target = fluent::queue_target_image(16, 9);
    fluent::blur_into(fluent::PlatePass { source, target, sigma: 2.0, saturate: 1.0 });
    let plate = read(&mut renderer, target, (16, 9));
    assert!(plate.chunks_exact(4).all(|pixel| pixel[3] == 255), "every pixel of it");
    assert!(plate[..4].iter().any(|channel| *channel > 0), "with something in it: {:?}", &plate[..4]);
    // The fixture's green runs left to right: the blur leaves that, and only softens it.
    assert!(plate[15 * 4 + 1] > plate[1], "{} {}", plate[15 * 4 + 1], plate[1]);
}

/// Decoding, wrapping and drawing a thousand frames must not pile anything up. Holding on to surfaces would stall the decoder's pool long before.
#[test]
fn frames_go_back_to_the_decoder_once_the_gpu_is_done_with_them() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut renderer = renderer();
    let image = fluent::queue_surface_image();
    let mut shown = 0;
    for _ in 0..30 {
        let mut reader = VideoReader::open(&fixture("counting.mp4"), None, 0.0).unwrap();
        while let Some(frame) = reader.next_frame() {
            fluent::show_surface(image, frame.into_surface().unwrap());
            read(&mut renderer, image, (64, 36));
            shown += 1;
        }
    }
    assert_eq!(shown, 480);
}
