//! The renderer: instance arrays in, a few draw calls out.

use std::ops::Range;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use crate::theme::{Color, Theme};

pub const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// At 2x, each distinct glyph takes up to four subpixel variants, and CJK text has thousands of distinct glyphs.
pub const MASK_ATLAS_SIZE: u32 = 4096;
pub const COLOR_ATLAS_SIZE: u32 = 2048;

pub const KIND_RECT: f32 = 0.0;
pub const KIND_ARC: f32 = 2.0;
pub const KIND_LINE: f32 = 3.0;
pub const KIND_ACRYLIC: f32 = 4.0;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct RectInst {
    pub rect: [f32; 4],
    pub clip: [f32; 4],
    pub radii: [f32; 4],
    pub fill: Color,
    pub fill2: Color,
    pub border: Color,
    pub border2: Color,
    pub params: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, Default)]
pub struct GlyphInst {
    pub pos: [f32; 2],
    pub size: [u16; 2],
    pub uv: [u16; 2],
    pub color: Color,
    pub clip: [i16; 4],
    pub flags: u32,
}

pub const GLYPH_COLOR: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct ImageInst {
    pub rect: [f32; 4],
    pub clip: [f32; 4],
    pub uv: [f32; 4],
    /// Top-left, top-right, bottom-right, bottom-left.
    pub radii: [f32; 4],
    pub opacity: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
pub struct ImageDraw {
    pub image: ImageId,
    pub inst: ImageInst,
    /// Every pixel of the image is known to be solid, so it hides whatever lies under it, within its corners.
    pub opaque: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageId(pub u32);

/// Images a view asks for while it has no renderer in reach. Views run with `&mut App` and a `Ui`, the renderer belongs
/// to whoever owns the window, so ids are handed out here at once and the pixels reach the GPU at the next `render`.
/// Ids from here sit in a range of their own, so they never meet the slots `add_image` reuses.
const QUEUED_IMAGE_BASE: u32 = 1 << 24;

#[derive(Default)]
struct ImageQueue {
    next: u32,
    uploads: Vec<(u32, Vec<u8>, u32, u32)>,
    /// Textures a live image gets, with nothing in them yet.
    blanks: Vec<(u32, u32, u32)>,
    /// Textures a live pass can draw into, as well as have pixels written to.
    targets: Vec<(u32, u32, u32)>,
    writes: Vec<ImageWrite>,
    #[cfg(target_os = "macos")]
    surfaces: Vec<(u32, SurfaceFrame)>,
    plates: Vec<PlatePass>,
    releases: Vec<u32>,
}

/// One go at working out the glass on the GPU: see [`blur_into`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlatePass {
    pub source: ImageId,
    pub target: ImageId,
    /// In the target's pixels.
    pub sigma: f32,
    pub saturate: f32,
}

/// A decoded picture in memory the GPU can read as it is: no copy on the way, whatever its size.
#[cfg(target_os = "macos")]
pub struct SurfaceFrame {
    /// An `IOSurfaceRef`, BGRA, kept alive by `keep`.
    pub surface: *mut std::ffi::c_void,
    pub width: u32,
    pub height: u32,
    /// What holds the memory. It goes once the GPU is done with the last frame that sampled it, and only then can the decoder reuse it.
    pub keep: Box<dyn Send>,
}

// SAFETY: an IOSurface may be used from any thread, and `keep` is `Send`.
#[cfg(target_os = "macos")]
unsafe impl Send for SurfaceFrame {}

/// Pixels going into part of an image that is already on the GPU.
struct ImageWrite {
    id: u32,
    origin: (u32, u32),
    size: (u32, u32),
    pixels: Box<dyn AsRef<[u8]> + Send>,
}

static IMAGE_QUEUE: std::sync::Mutex<Option<ImageQueue>> = std::sync::Mutex::new(None);

/// Uploads straight-alpha RGBA at the next frame and returns its id now. Callable from any thread.
pub fn queue_image(rgba: Vec<u8>, width: u32, height: u32) -> ImageId {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let queue = queue.get_or_insert_with(ImageQueue::default);
    let id = QUEUED_IMAGE_BASE + queue.next;
    queue.next += 1;
    queue.uploads.push((id, rgba, width, height));
    ImageId(id)
}

/// An image whose pixels keep changing: a texture of this size, filled by [`write_image`]. Callable from any thread.
pub fn queue_live_image(width: u32, height: u32) -> ImageId {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let queue = queue.get_or_insert_with(ImageQueue::default);
    let id = QUEUED_IMAGE_BASE + queue.next;
    queue.next += 1;
    queue.blanks.push((id, width, height));
    ImageId(id)
}

/// The same with a texture that a live pass can draw into too: [`blur_into`]. Callable from any thread.
pub fn queue_target_image(width: u32, height: u32) -> ImageId {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let queue = queue.get_or_insert_with(ImageQueue::default);
    let id = QUEUED_IMAGE_BASE + queue.next;
    queue.next += 1;
    queue.targets.push((id, width, height));
    ImageId(id)
}

/// An image with no texture of its own, showing whichever surface [`show_surface`] was last given. Nothing draws until the first.
#[cfg(target_os = "macos")]
pub fn queue_surface_image() -> ImageId {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let queue = queue.get_or_insert_with(ImageQueue::default);
    let id = QUEUED_IMAGE_BASE + queue.next;
    queue.next += 1;
    ImageId(id)
}

/// Shows a surface in an image from the next frame on.
#[cfg(target_os = "macos")]
pub fn show_surface(id: ImageId, frame: SurfaceFrame) {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let queue = queue.get_or_insert_with(ImageQueue::default);
    // One not drawn yet never will be. Letting go of it gives the decoder its memory back.
    queue.surfaces.retain(|(queued, _)| *queued != id.0);
    queue.surfaces.push((id.0, frame));
}

/// Fills `target`, a [`queue_target_image`], with `source` shrunk, blurred and saturated, on the GPU, before the next frame draws. For a
/// target asked for more than once before that frame, the last request stands.
pub fn blur_into(pass: PlatePass) {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let queue = queue.get_or_insert_with(ImageQueue::default);
    queue.plates.retain(|queued| queued.target != pass.target);
    queue.plates.push(pass);
}

/// Replaces a rectangle of an image with tightly packed RGBA at the next frame, without touching the rest. It costs what
/// the rectangle costs, so a small change to a large picture stays small.
pub fn write_image(id: ImageId, x: u32, y: u32, width: u32, height: u32, rgba: impl AsRef<[u8]> + Send + 'static) {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    queue.get_or_insert_with(ImageQueue::default).writes.push(ImageWrite { id: id.0, origin: (x, y), size: (width, height), pixels: Box::new(rgba) });
}

/// Frees an image from [`queue_image`] at the next frame.
pub fn release_image(id: ImageId) {
    let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    queue.get_or_insert_with(ImageQueue::default).releases.push(id.0);
}

#[derive(Default)]
pub struct Layer {
    pub rects: Vec<RectInst>,
    pub images: Vec<ImageDraw>,
    pub glyphs: Vec<GlyphInst>,
}

impl Layer {
    fn clear(&mut self) {
        self.rects.clear();
        self.images.clear();
        self.glyphs.clear();
    }
}

/// The page, then what floats over it, each in a band of its own that draws wholly over the one before. An element tree spends a layer per
/// overlay and per scroller, and a layer draws its boxes before its text, so two surfaces that shared one would let the lower one's text
/// through the upper one's box. So a band never runs out: it holds as many layers as are asked of it, and only those cost anything.
pub const LAYER_BASE: usize = 0;
const BAND_SHIFT: u32 = 16;
const BAND_SLOTS: usize = (1 << BAND_SHIFT) - 1;
pub const LAYER_MODAL: usize = 1 << BAND_SHIFT;
pub const LAYER_POPUP: usize = 2 << BAND_SHIFT;
/// Where hover text is drawn: a cut line's whole words, and why something is disabled.
pub const LAYER_TOOLTIP: usize = 3 << BAND_SHIFT;
const BANDS: usize = 4;

static NO_LAYER: Layer = Layer { rects: Vec::new(), images: Vec::new(), glyphs: Vec::new() };

/// A frame's layers, addressed as `LAYER_POPUP + n`. Reading one nothing has drawn in finds it empty; writing to one makes it.
#[derive(Default)]
pub struct Layers {
    bands: [Vec<Layer>; BANDS],
}

impl Layers {
    /// In the order they draw.
    pub fn iter(&self) -> std::vec::IntoIter<&Layer> {
        self.drawn().collect::<Vec<_>>().into_iter()
    }

    pub(crate) fn drawn(&self) -> impl Iterator<Item = &Layer> {
        self.bands.iter().flatten()
    }

    pub(crate) fn drawn_mut(&mut self) -> impl Iterator<Item = &mut Layer> {
        self.bands.iter_mut().flatten()
    }

    /// Those above the page, in the order they draw.
    pub fn over_the_page(&self) -> impl Iterator<Item = &Layer> {
        self.bands[1..].iter().flatten()
    }
}

impl std::ops::Index<usize> for Layers {
    type Output = Layer;

    fn index(&self, layer: usize) -> &Layer {
        self.bands[layer >> BAND_SHIFT].get(layer & BAND_SLOTS).unwrap_or(&NO_LAYER)
    }
}

impl std::ops::IndexMut<usize> for Layers {
    fn index_mut(&mut self, layer: usize) -> &mut Layer {
        let (band, slot) = (&mut self.bands[layer >> BAND_SHIFT], layer & BAND_SLOTS);
        if slot >= band.len() {
            band.resize_with(slot + 1, Layer::default);
        }
        &mut band[slot]
    }
}

/// What a layer held when a mark was taken: boxes, glyphs, images.
type Held = (usize, usize, usize);

/// One frame's worth of geometry. Its buffers are kept between frames, so that a steady scene allocates
/// nothing at all.
#[derive(Default)]
pub struct Scene {
    pub layers: Layers,
    /// The counts of every open `Ui::paint_mark`. Here rather than on the stack because the element tree paints by recursion, and it outlives the `Ui` so that it allocates once.
    marks: Vec<Held>,
    /// Something solid covers the whole window, so the ground under everything has nothing to show.
    pub ground_covered: bool,
    /// [`Self::cull_occluded`]'s working space, kept so that a steady frame allocates nothing.
    pub(crate) surfaces: Vec<crate::occlusion::Surface>,
    pub(crate) extra_rects: Vec<(usize, RectInst)>,
}

impl Scene {
    pub fn clear(&mut self) {
        for l in self.layers.drawn_mut() {
            l.clear();
        }
        self.marks.clear();
        self.ground_covered = false;
    }

    /// Notes how much every layer holds, band by band, each band's count first since a band may have grown by the time the mark is closed.
    pub(crate) fn mark(&mut self) -> usize {
        let mark = self.marks.len();
        for band in &self.layers.bands {
            self.marks.push((band.len(), 0, 0));
            self.marks.extend(band.iter().map(|layer| (layer.rects.len(), layer.glyphs.len(), layer.images.len())));
        }
        mark
    }

    /// Closes a mark, handing over every layer with what it held then: nothing, for one made since.
    pub(crate) fn close_mark(&mut self, mark: usize, mut each: impl FnMut(&mut Layer, Held)) {
        let mut at = mark;
        for band in &mut self.layers.bands {
            let known = self.marks[at].0;
            at += 1;
            for (slot, layer) in band.iter_mut().enumerate() {
                each(layer, if slot < known { self.marks[at + slot] } else { (0, 0, 0) });
            }
            at += known;
        }
        self.marks.truncate(mark);
    }

    /// Whether anything of an image is drawn this frame.
    pub fn draws_image(&self, image: ImageId) -> bool {
        self.layers.drawn().any(|layer| layer.images.iter().any(|draw| draw.image == image))
    }

    pub fn counts(&self) -> (usize, usize) {
        let rects = self.layers.drawn().map(|l| l.rects.len()).sum();
        let glyphs = self.layers.drawn().map(|l| l.glyphs.len()).sum();
        (rects, glyphs)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    viewport: [f32; 2],
    text_gamma: f32,
    noise_alpha: f32,
    mica_base: [f32; 4],
    mica_tint_a: [f32; 4],
    mica_tint_b: [f32; 4],
}

struct GrowBuffer {
    buffer: wgpu::Buffer,
    capacity: u64,
    label: &'static str,
}

impl GrowBuffer {
    fn new(device: &wgpu::Device, label: &'static str, capacity: u64) -> Self {
        Self { buffer: Self::make(device, label, capacity), capacity, label }
    }

    fn make(device: &wgpu::Device, label: &'static str, capacity: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: capacity, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
    }

    fn ensure(&mut self, device: &wgpu::Device, bytes: u64) {
        if bytes > self.capacity {
            self.capacity = (bytes * 2).next_power_of_two();
            self.buffer = Self::make(device, self.label, self.capacity);
        }
    }
}

struct GpuImage {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// What a live pass of the glass draws with: its pipelines, and the textures between the source and the plate.
struct PlateRenderer {
    shrink: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    finish: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// How far apart a pass's values lie in `uniforms`.
    stride: u32,
    scratch: Vec<GpuImage>,
    /// When a pass last ran, so that its textures don't outlive the need for them.
    last_used: Option<std::time::Instant>,
}

/// Enough for an 8K source, halved down to a few pixels, then two passes for the blur.
const PLATE_PASSES: u32 = 16;
/// How long the textures between source and plate stay after their last use. Baked glass needs none, and they cost memory.
const PLATE_SCRATCH_KEEPS: std::time::Duration = std::time::Duration::from_secs(2);

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PlateUniform {
    step: [f32; 2],
    sigma: f32,
    saturate: f32,
}

/// The sizes a source goes through on its way to the target: halved for as long as that doesn't go below it, then the target's own.
fn shrink_sizes(source: (u32, u32), target: (u32, u32)) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    let (mut width, mut height) = source;
    while width / 2 >= target.0 && height / 2 >= target.1 {
        width /= 2;
        height /= 2;
        sizes.push((width, height));
    }
    if sizes.last().copied().unwrap_or(source) != target || sizes.is_empty() {
        sizes.push(target);
    }
    sizes
}

pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Picks a device. `surface` steers the choice towards one that can present to it.
    pub fn new(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Self, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: surface,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("no GPU adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("fluent"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::Performance,
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU device: {e}"))?;
        Ok(Self { instance, adapter, device, queue })
    }
}

pub struct Renderer {
    pub gpu: Arc<Gpu>,
    /// Each layer's boxes and glyphs in this frame's buffers. Kept so that a steady frame allocates nothing.
    ranges: Vec<(Range<u32>, Range<u32>)>,
    rect_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
    mica_pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    image_layout: wgpu::BindGroupLayout,
    image_sampler: wgpu::Sampler,
    mask_atlas: wgpu::Texture,
    color_atlas: wgpu::Texture,
    rect_buf: GrowBuffer,
    glyph_buf: GrowBuffer,
    image_buf: GrowBuffer,
    images: Vec<Option<GpuImage>>,
    queued_images: std::collections::HashMap<u32, GpuImage>,
    /// What holds each surface image's memory, for as long as it is the one showing.
    #[cfg(target_os = "macos")]
    surface_keeps: std::collections::HashMap<u32, Box<dyn Send>>,
    plate: PlateRenderer,
}

pub struct AtlasUpload<'a> {
    pub data: &'a [u8],
    pub bytes_per_pixel: u32,
    pub stride: u32,
    pub rect: [u32; 4],
}

fn instance_attrs<const N: usize>(attrs: &'static [wgpu::VertexAttribute; N], stride: u64) -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout { array_stride: stride, step_mode: wgpu::VertexStepMode::Instance, attributes: attrs }
}

const RECT_ATTRS: [wgpu::VertexAttribute; 8] = wgpu::vertex_attr_array![
    0 => Float32x4, 1 => Float32x4, 2 => Float32x4,
    3 => Unorm8x4, 4 => Unorm8x4, 5 => Unorm8x4, 6 => Unorm8x4,
    7 => Float32x4
];
const GLYPH_ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
    0 => Float32x2, 1 => Uint16x4, 2 => Unorm8x4, 3 => Sint16x4, 4 => Uint32
];
const IMAGE_ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
    0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4
];

impl Renderer {
    pub fn new(gpu: Arc<Gpu>) -> Self {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("fluent"), source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()) });

        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                texture_entry(1),
                texture_entry(2),
            ],
        });
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image"),
            entries: &[
                texture_entry(0),
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });

        let base_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("base"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
        let image_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("image"), bind_group_layouts: &[Some(&layout), Some(&image_layout)], immediate_size: 0 });

        let make = |label: &str, vs: &str, fs: &str, pipeline_layout: &wgpu::PipelineLayout, buffers: &[Option<wgpu::VertexBufferLayout<'_>>], strip: bool, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(pipeline_layout),
                vertex: wgpu::VertexState { module: &shader, entry_point: Some(vs), compilation_options: Default::default(), buffers },
                primitive: wgpu::PrimitiveState {
                    topology: if strip { wgpu::PrimitiveTopology::TriangleStrip } else { wgpu::PrimitiveTopology::TriangleList },
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: SURFACE_FORMAT, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let premul = Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let rect_layout = instance_attrs(&RECT_ATTRS, size_of::<RectInst>() as u64);
        let glyph_layout = instance_attrs(&GLYPH_ATTRS, size_of::<GlyphInst>() as u64);
        let image_vb_layout = instance_attrs(&IMAGE_ATTRS, size_of::<ImageInst>() as u64);
        let rect_pipeline = make("rect", "rect_vs", "rect_fs", &base_layout, &[Some(rect_layout)], true, premul);
        let glyph_pipeline = make("glyph", "glyph_vs", "glyph_fs", &base_layout, &[Some(glyph_layout)], true, premul);
        let image_pipeline = make("image", "image_vs", "image_fs", &image_pipeline_layout, &[Some(image_vb_layout)], true, premul);
        let mica_pipeline = make("mica", "mica_vs", "mica_fs", &base_layout, &[], false, None);

        let atlas = |label, size, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let mask_atlas = atlas("mask atlas", MASK_ATLAS_SIZE, wgpu::TextureFormat::R8Unorm);
        let color_atlas = atlas("color atlas", COLOR_ATLAS_SIZE, wgpu::TextureFormat::Rgba8Unorm);
        let globals = device.create_buffer(&wgpu::BufferDescriptor { label: Some("globals"), size: size_of::<Globals>() as u64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&mask_atlas.create_view(&Default::default())) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&color_atlas.create_view(&Default::default())) },
            ],
        });
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor { mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, ..Default::default() });

        let plate = {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("plate"), source: wgpu::ShaderSource::Wgsl(include_str!("plate.wgsl").into()) });
            let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("plate pass"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: true, min_binding_size: wgpu::BufferSize::new(size_of::<PlateUniform>() as u64) },
                    count: None,
                }],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("plate"), bind_group_layouts: &[Some(&uniform_layout), Some(&image_layout)], immediate_size: 0 });
            let make = |label: &str, fragment: &str| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState { module: &shader, entry_point: Some("plate_vs"), compilation_options: Default::default(), buffers: &[] },
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some(fragment), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba8Unorm, blend: None, write_mask: wgpu::ColorWrites::ALL })] }),
                    multiview_mask: None,
                    cache: None,
                })
            };
            let stride = device.limits().min_uniform_buffer_offset_alignment.max(size_of::<PlateUniform>() as u32);
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor { label: Some("plate pass"), size: u64::from(stride * PLATE_PASSES), usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("plate pass"),
                layout: &uniform_layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &uniforms, offset: 0, size: wgpu::BufferSize::new(size_of::<PlateUniform>() as u64) }) }],
            });
            PlateRenderer { shrink: make("plate shrink", "shrink_fs"), blur: make("plate blur", "blur_fs"), finish: make("plate finish", "finish_fs"), uniforms, bind_group, stride, scratch: Vec::new(), last_used: None }
        };

        let rect_buf = GrowBuffer::new(device, "rects", 1 << 20);
        let glyph_buf = GrowBuffer::new(device, "glyphs", 4 << 20);
        let image_buf = GrowBuffer::new(device, "images", 1 << 14);
        Self {
            ranges: Vec::new(),
            rect_pipeline,
            glyph_pipeline,
            image_pipeline,
            mica_pipeline,
            globals,
            bind_group,
            image_layout,
            image_sampler,
            mask_atlas,
            color_atlas,
            rect_buf,
            glyph_buf,
            image_buf,
            images: Vec::new(),
            queued_images: std::collections::HashMap::new(),
            #[cfg(target_os = "macos")]
            surface_keeps: std::collections::HashMap::new(),
            plate,
            gpu,
        }
    }

    /// Uploads one RGBA image, downscaling nothing: the caller decides how large a thumbnail should be.
    pub fn add_image(&mut self, rgba: &[u8], width: u32, height: u32) -> ImageId {
        let slot = self.make_image(rgba, width, height);
        if let Some(free) = self.images.iter().position(Option::is_none) {
            self.images[free] = Some(slot);
            ImageId(free as u32)
        } else {
            self.images.push(Some(slot));
            ImageId(self.images.len() as u32 - 1)
        }
    }

    /// Brings in what views queued since the last frame, and hands back the live passes asked for, to run before it draws.
    fn drain_image_queue(&mut self) -> Vec<PlatePass> {
        let (uploads, blanks, targets, writes, plates, releases) = {
            let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
            let Some(queue) = queue.as_mut() else { return Vec::new() };
            #[cfg(target_os = "macos")]
            for (id, frame) in std::mem::take(&mut queue.surfaces) {
                self.show_surface(id, frame);
            }
            (std::mem::take(&mut queue.uploads), std::mem::take(&mut queue.blanks), std::mem::take(&mut queue.targets), std::mem::take(&mut queue.writes), std::mem::take(&mut queue.plates), std::mem::take(&mut queue.releases))
        };
        for (id, rgba, width, height) in uploads {
            let slot = self.make_image(&rgba, width, height);
            self.queued_images.insert(id, slot);
        }
        for (id, width, height) in blanks {
            let slot = self.blank_image(width, height);
            self.queued_images.insert(id, slot);
        }
        for (id, width, height) in targets {
            let slot = self.target_image(width, height);
            self.queued_images.insert(id, slot);
        }
        for write in writes {
            // Released in the meantime, or written past its edge by a size that just changed.
            let Some(image) = self.queued_images.get(&write.id) else { continue };
            let (x, y) = write.origin;
            let (width, height) = write.size;
            let size = image.texture.size();
            let pixels = (*write.pixels).as_ref();
            if x + width > size.width || y + height > size.height || pixels.len() < (width * height * 4) as usize {
                continue;
            }
            self.gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &image.texture, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                pixels,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width * 4), rows_per_image: None },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
        for id in releases {
            self.queued_images.remove(&id);
            #[cfg(target_os = "macos")]
            if let Some(keep) = self.surface_keeps.remove(&id) {
                self.gpu.queue.on_submitted_work_done(move || drop(keep));
            }
        }
        plates
    }

    /// Wraps the surface in a texture, there being no pixels to move, and shows that. The one before it may still be drawing, so what
    /// holds its memory goes when the work already sent is done.
    #[cfg(target_os = "macos")]
    fn show_surface(&mut self, id: u32, frame: SurfaceFrame) {
        use objc2_metal::{MTLDevice, MTLPixelFormat, MTLResourceOptions, MTLStorageMode, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};
        use wgpu::hal::api::Metal;
        let SurfaceFrame { surface, width, height, keep } = frame;
        let device = &self.gpu.device;
        // SAFETY: the raw device isn't kept past this call. The descriptor is what `newTextureWithDescriptor:iosurface:plane:` asks for
        // of plane 0 of a BGRA surface, which `keep` holds alive for as long as the texture can be drawn.
        let raw = unsafe {
            let Some(hal) = device.as_hal::<Metal>() else { return };
            let descriptor = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::BGRA8Unorm, width as usize, height as usize, false);
            descriptor.setUsage(MTLTextureUsage::ShaderRead);
            descriptor.setStorageMode(MTLStorageMode::Shared);
            descriptor.setResourceOptions(MTLResourceOptions::StorageModeShared);
            hal.raw_device().newTextureWithDescriptor_iosurface_plane(&descriptor, &*surface.cast::<objc2_io_surface::IOSurfaceRef>(), 0)
        };
        let Some(raw) = raw else { return };
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        // SAFETY: `raw` is a 2D texture of this format and size, made on this device just now.
        let texture = unsafe {
            let hal = wgpu::hal::metal::Device::texture_from_raw(raw, wgpu::TextureFormat::Bgra8Unorm, MTLTextureType::Type2D, 1, 1, wgpu::hal::CopyExtent { width, height, depth: 1 }, None);
            device.create_texture_from_hal::<Metal>(
                hal,
                &wgpu::TextureDescriptor { label: Some("surface"), size, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: wgpu::TextureFormat::Bgra8Unorm, usage: wgpu::TextureUsages::TEXTURE_BINDING, view_formats: &[] },
                wgpu::TextureUses::RESOURCE,
            )
        };
        let image = self.image_of(texture);
        self.queued_images.insert(id, image);
        if let Some(old) = self.surface_keeps.insert(id, keep) {
            self.gpu.queue.on_submitted_work_done(move || drop(old));
        }
    }

    fn make_image(&self, rgba: &[u8], width: u32, height: u32) -> GpuImage {
        let image = self.blank_image(width, height);
        self.gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &image.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width * 4), rows_per_image: None },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        image
    }

    fn blank_image(&self, width: u32, height: u32) -> GpuImage {
        self.texture_image(width, height, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST)
    }

    fn target_image(&self, width: u32, height: u32) -> GpuImage {
        self.texture_image(width, height, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT)
    }

    fn texture_image(&self, width: u32, height: u32, usage: wgpu::TextureUsages) -> GpuImage {
        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage,
            view_formats: &[],
        });
        self.image_of(texture)
    }

    fn image_of(&self, texture: wgpu::Texture) -> GpuImage {
        let device = &self.gpu.device;
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            layout: &self.image_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&texture.create_view(&Default::default())) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.image_sampler) },
            ],
        });
        GpuImage { texture, bind_group }
    }

    /// Works the glass out on the GPU: the source down to the plate's size in halves, then a blur each way.
    fn run_plate_passes(&mut self, passes: &[PlatePass], encoder: &mut wgpu::CommandEncoder) {
        if passes.is_empty() {
            if self.plate.last_used.is_some_and(|at| at.elapsed() > PLATE_SCRATCH_KEEPS) {
                self.plate.scratch.clear();
                self.plate.last_used = None;
            }
            return;
        }
        self.plate.last_used = Some(std::time::Instant::now());
        // A step is a texture to draw into, a texture to read and what the shader is told.
        for &PlatePass { source, target, sigma, saturate } in passes {
            let sized = |image: &GpuImage| {
                let size = image.texture.size();
                (size.width, size.height)
            };
            let image_at = |id: ImageId| if id.0 >= QUEUED_IMAGE_BASE { self.queued_images.get(&id.0) } else { self.images.get(id.0 as usize).and_then(Option::as_ref) };
            let (Some(source_image), Some(target_image)) = (image_at(source), image_at(target)) else { continue };
            let target_size = sized(target_image);
            let sizes = shrink_sizes(sized(source_image), target_size);
            let mut needed: Vec<(u32, u32)> = sizes.clone();
            needed.push(target_size);
            self.keep_scratch(&needed);
            let image_at = |id: ImageId| if id.0 >= QUEUED_IMAGE_BASE { self.queued_images.get(&id.0) } else { self.images.get(id.0 as usize).and_then(Option::as_ref) };
            let (Some(source_image), Some(target_image)) = (image_at(source), image_at(target)) else { continue };

            let stride = self.plate.stride;
            let mut uniforms = vec![0u8; (stride * PLATE_PASSES) as usize];
            let mut steps: Vec<(&wgpu::RenderPipeline, &GpuImage, &GpuImage)> = Vec::new();
            let mut used = vec![false; self.plate.scratch.len()];
            let mut take = |size: (u32, u32), scratch: &'_ [GpuImage]| -> Option<usize> {
                let at = (0..scratch.len()).find(|&at| !used[at] && sized(&scratch[at]) == size)?;
                used[at] = true;
                Some(at)
            };
            let mut write = |at: usize, value: PlateUniform| uniforms[at * stride as usize..at * stride as usize + size_of::<PlateUniform>()].copy_from_slice(bytemuck::bytes_of(&value));
            let mut last = source_image;
            let mut complete = true;
            for &size in &sizes {
                let Some(at) = take(size, &self.plate.scratch) else {
                    complete = false;
                    break;
                };
                // One output pixel's width in the source's coordinates, divided among the samples.
                write(steps.len(), PlateUniform { step: [0.5 / size.0.max(1) as f32, 0.5 / size.1.max(1) as f32], sigma: 0.0, saturate: 1.0 });
                steps.push((&self.plate.shrink, last, &self.plate.scratch[at]));
                last = &self.plate.scratch[at];
            }
            let Some(across) = take(target_size, &self.plate.scratch).filter(|_| complete) else { continue };
            let across = &self.plate.scratch[across];
            write(steps.len(), PlateUniform { step: [1.0 / target_size.0.max(1) as f32, 0.0], sigma, saturate: 1.0 });
            steps.push((&self.plate.blur, last, across));
            write(steps.len(), PlateUniform { step: [0.0, 1.0 / target_size.1.max(1) as f32], sigma, saturate });
            steps.push((&self.plate.finish, across, target_image));
            if steps.len() > PLATE_PASSES as usize {
                continue;
            }
            self.gpu.queue.write_buffer(&self.plate.uniforms, 0, &uniforms[..steps.len() * stride as usize]);
            for (index, (pipeline, read, draw)) in steps.into_iter().enumerate() {
                let view = draw.texture.create_view(&Default::default());
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("plate"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store } })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &self.plate.bind_group, &[index as u32 * stride]);
                pass.set_bind_group(1, &read.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
    }

    /// Makes sure there is a texture for every size in `needed`, as many of each as it appears, and that nothing else is kept.
    fn keep_scratch(&mut self, needed: &[(u32, u32)]) {
        let mut wanted = needed.to_vec();
        let mut kept = Vec::with_capacity(needed.len());
        for image in std::mem::take(&mut self.plate.scratch) {
            let size = image.texture.size();
            if let Some(at) = wanted.iter().position(|want| *want == (size.width, size.height)) {
                wanted.swap_remove(at);
                kept.push(image);
            }
        }
        for (width, height) in wanted {
            let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT;
            kept.push(self.texture_image(width, height, usage));
        }
        self.plate.scratch = kept;
    }

    pub fn remove_image(&mut self, id: ImageId) {
        if let Some(slot) = self.images.get_mut(id.0 as usize) {
            *slot = None;
        }
    }

    fn upload_atlas(&self, texture: &wgpu::Texture, up: &AtlasUpload<'_>) {
        let [x, y, w, h] = up.rect;
        if w == 0 || h == 0 {
            return;
        }
        let bpp = up.bytes_per_pixel;
        let start = ((y * up.stride + x) * bpp) as usize;
        self.gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
            &up.data[start..],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(up.stride * bpp), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    pub fn upload_mask_atlas(&self, up: &AtlasUpload<'_>) {
        self.upload_atlas(&self.mask_atlas, up);
    }

    pub fn upload_color_atlas(&self, up: &AtlasUpload<'_>) {
        self.upload_atlas(&self.color_atlas, up);
    }

    /// Records and submits one frame into `target`.
    pub fn render(&mut self, scene: &Scene, theme: &Theme, target: &wgpu::TextureView, size: (u32, u32)) -> wgpu::SubmissionIndex {
        let plate_passes = self.drain_image_queue();
        self.upload_scene(scene, theme, size);
        self.draw_scene(scene, plate_passes, target)
    }

    /// The scene last given to [`Self::render`] once more, over whatever has changed in its images since. Its boxes and text are on the
    /// GPU already, so nothing about them is sent again. `scene` has to be that one, as it was.
    pub fn render_again(&mut self, scene: &Scene, target: &wgpu::TextureView) -> wgpu::SubmissionIndex {
        let plate_passes = self.drain_image_queue();
        self.draw_scene(scene, plate_passes, target)
    }

    fn upload_scene(&mut self, scene: &Scene, theme: &Theme, size: (u32, u32)) {
        let gpu = self.gpu.clone();
        let (device, queue) = (&gpu.device, &gpu.queue);

        let globals = Globals {
            viewport: [size.0 as f32, size.1 as f32],
            text_gamma: theme.text_gamma,
            noise_alpha: theme.noise_alpha,
            mica_base: theme.mica_base.to_linear_f32(),
            mica_tint_a: theme.mica_tint_a.to_linear_f32(),
            mica_tint_b: theme.mica_tint_b.to_linear_f32(),
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let rect_size = size_of::<RectInst>() as u64;
        let glyph_size = size_of::<GlyphInst>() as u64;
        let image_size = size_of::<ImageInst>() as u64;
        let (total_rects, total_glyphs) = scene.counts();
        let total_images: usize = scene.layers.drawn().map(|l| l.images.len()).sum();
        self.rect_buf.ensure(device, total_rects as u64 * rect_size);
        self.glyph_buf.ensure(device, total_glyphs as u64 * glyph_size);
        self.image_buf.ensure(device, total_images as u64 * image_size);

        // Each layer's array is copied straight into its range of one GPU buffer, with no intermediate.
        self.ranges.clear();
        let (mut r, mut g, mut im) = (0u32, 0u32, 0u32);
        for layer in scene.layers.drawn() {
            if !layer.rects.is_empty() {
                queue.write_buffer(&self.rect_buf.buffer, r as u64 * rect_size, bytemuck::cast_slice(&layer.rects));
            }
            if !layer.glyphs.is_empty() {
                queue.write_buffer(&self.glyph_buf.buffer, g as u64 * glyph_size, bytemuck::cast_slice(&layer.glyphs));
            }
            for draw in &layer.images {
                queue.write_buffer(&self.image_buf.buffer, im as u64 * image_size, bytemuck::bytes_of(&draw.inst));
                im += 1;
            }
            self.ranges.push((r..r + layer.rects.len() as u32, g..g + layer.glyphs.len() as u32));
            r += layer.rects.len() as u32;
            g += layer.glyphs.len() as u32;
        }
    }

    fn draw_scene(&mut self, scene: &Scene, plate_passes: Vec<PlatePass>, target: &wgpu::TextureView) -> wgpu::SubmissionIndex {
        let gpu = self.gpu.clone();
        let (device, queue) = (&gpu.device, &gpu.queue);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        self.run_plate_passes(&plate_passes, &mut encoder);
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    // The Mica pass or a surface as large as the window covers every pixel, so the previous contents never need loading.
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            if !scene.ground_covered {
                pass.set_pipeline(&self.mica_pipeline);
                pass.draw(0..3, 0..1);
            }

            let mut image_base = 0u32;
            for (i, layer) in scene.layers.drawn().enumerate() {
                let (rects, glyphs) = self.ranges[i].clone();
                if !rects.is_empty() {
                    pass.set_pipeline(&self.rect_pipeline);
                    pass.set_vertex_buffer(0, self.rect_buf.buffer.slice(..));
                    pass.draw(0..4, rects);
                }
                if !layer.images.is_empty() {
                    pass.set_pipeline(&self.image_pipeline);
                    pass.set_vertex_buffer(0, self.image_buf.buffer.slice(..));
                    for (j, draw) in layer.images.iter().enumerate() {
                        let found = if draw.image.0 >= QUEUED_IMAGE_BASE { self.queued_images.get(&draw.image.0) } else { self.images.get(draw.image.0 as usize).and_then(Option::as_ref) };
                        if let Some(image) = found {
                            pass.set_bind_group(1, &image.bind_group, &[]);
                            let k = image_base + j as u32;
                            pass.draw(0..4, k..k + 1);
                        }
                    }
                    image_base += layer.images.len() as u32;
                }
                if !glyphs.is_empty() {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_vertex_buffer(0, self.glyph_buf.buffer.slice(..));
                    pass.draw(0..4, glyphs);
                }
            }
        }
        queue.submit([encoder.finish()])
    }
}
