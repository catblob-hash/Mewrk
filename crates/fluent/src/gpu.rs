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
pub const KIND_SHADOW: f32 = 1.0;
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
    writes: Vec<ImageWrite>,
    releases: Vec<u32>,
}

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

pub const LAYER_BASE: usize = 0;
/// Layers from here up are for overlays inside the page; an element tree spends one per overlay and one per scroller. A layer draws its
/// boxes before its text, so once a band runs out, what is drawn next shares its last layer and the text under a surface comes through it.
/// Four panes beside a long conversation went through 39, so the page's band leaves room for far more than that.
pub const LAYER_MODAL: usize = 240;
pub const LAYER_POPUP: usize = 272;
/// The top of the popup band, and where hover text is drawn: a cut line's whole words, and why something is disabled.
pub const LAYER_TOOLTIP: usize = 288;
pub const LAYERS: usize = 296;

/// One frame's worth of geometry. Its buffers are kept between frames, so that a steady scene allocates
/// nothing at all.
pub struct Scene {
    pub layers: [Layer; LAYERS],
    /// The counts of every open `Ui::paint_mark`. Here rather than on the stack because the element tree paints by recursion, and it outlives the `Ui` so that it allocates once.
    pub(crate) marks: Vec<(usize, usize, usize)>,
}

impl Default for Scene {
    fn default() -> Self {
        Self { layers: std::array::from_fn(|_| Layer::default()), marks: Vec::new() }
    }
}

impl Scene {
    pub fn clear(&mut self) {
        for l in &mut self.layers {
            l.clear();
        }
        self.marks.clear();
    }

    pub fn counts(&self) -> (usize, usize) {
        let rects = self.layers.iter().map(|l| l.rects.len()).sum();
        let glyphs = self.layers.iter().map(|l| l.glyphs.len()).sum();
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

        let rect_buf = GrowBuffer::new(device, "rects", 1 << 20);
        let glyph_buf = GrowBuffer::new(device, "glyphs", 4 << 20);
        let image_buf = GrowBuffer::new(device, "images", 1 << 14);
        Self {
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

    /// Brings in what views queued since the last frame.
    fn drain_image_queue(&mut self) {
        let (uploads, blanks, writes, releases) = {
            let mut queue = IMAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
            let Some(queue) = queue.as_mut() else { return };
            (std::mem::take(&mut queue.uploads), std::mem::take(&mut queue.blanks), std::mem::take(&mut queue.writes), std::mem::take(&mut queue.releases))
        };
        for (id, rgba, width, height) in uploads {
            let slot = self.make_image(&rgba, width, height);
            self.queued_images.insert(id, slot);
        }
        for (id, width, height) in blanks {
            let slot = self.blank_image(width, height);
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
        let device = &self.gpu.device;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
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
        self.drain_image_queue();
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
        let total_images: usize = scene.layers.iter().map(|l| l.images.len()).sum();
        self.rect_buf.ensure(device, total_rects as u64 * rect_size);
        self.glyph_buf.ensure(device, total_glyphs as u64 * glyph_size);
        self.image_buf.ensure(device, total_images as u64 * image_size);

        // Each layer's array is copied straight into its range of one GPU buffer, with no intermediate.
        let mut ranges: [(Range<u32>, Range<u32>); LAYERS] = std::array::from_fn(|_| (0..0, 0..0));
        let (mut r, mut g, mut im) = (0u32, 0u32, 0u32);
        for (i, layer) in scene.layers.iter().enumerate() {
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
            ranges[i] = (r..r + layer.rects.len() as u32, g..g + layer.glyphs.len() as u32);
            r += layer.rects.len() as u32;
            g += layer.glyphs.len() as u32;
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    // The Mica pass covers every pixel, so the previous contents never need loading.
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_pipeline(&self.mica_pipeline);
            pass.draw(0..3, 0..1);

            let mut image_base = 0u32;
            for (i, layer) in scene.layers.iter().enumerate() {
                let (rects, glyphs) = ranges[i].clone();
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
