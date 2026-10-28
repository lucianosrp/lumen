//! Draws a `ui::Frame`: SDF rounded rects, textured quads and cached glyphon text.

use std::collections::{HashMap, HashSet};

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap,
};

use crate::ui::{Align, Font, Frame, ImageId};

const ICON_FAMILY: &str = "JetBrainsMono Nerd Font";

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct QuadInstance {
    rect: [f32; 4],
    color: [f32; 4],
    uv: [f32; 4],
    params: [f32; 4],
}

struct Image {
    tex: wgpu::Texture,
    bind: wgpu::BindGroup,
    size: (u32, u32),
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct TextKey {
    text: String,
    size: u32,
    weight: u16,
    font: Font,
    max_width: Option<u32>,
}

pub struct UiRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_bind: wgpu::BindGroup,
    image_layout: wgpu::BindGroupLayout,
    white: wgpu::BindGroup,
    images: HashMap<ImageId, Image>,
    next_image: ImageId,
    instances: wgpu::Buffer,
    batches: [Vec<(u32, u32, Option<ImageId>)>; 3],

    font_system: FontSystem,
    swash: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text: [TextRenderer; 3],
    buffers: HashMap<TextKey, (Buffer, f32)>,
    icons: HashMap<crate::icons::IconKey, ImageId>,
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

impl UiRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat, globals: &wgpu::Buffer) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("ui.wgsl"));
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui image"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &globals_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&globals_layout), Some(&image_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ui"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_ui"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<QuadInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4],
                })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_ui"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let font_system = FontSystem::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let text = std::array::from_fn(|_| TextRenderer::new(&mut atlas, device, Default::default(), None));

        // Solid quads still need something bound at group 1.
        let white = make_image(device, queue, &image_layout, 1, 1, &[255, 255, 255, 255], false).bind;
        Self {
            pipeline,
            globals_bind,
            white,
            image_layout,
            images: HashMap::new(),
            next_image: 1,
            instances: Self::buffer(device, 1024),
            batches: Default::default(),
            font_system,
            swash: SwashCache::new(),
            viewport,
            atlas,
            text,
            buffers: HashMap::new(),
            icons: HashMap::new(),
        }
    }

    fn buffer(device: &wgpu::Device, n: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui quads"),
            size: (n * std::mem::size_of::<QuadInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Uploads an RGBA8 sRGB image and returns a handle for `Quad::image`.
    pub fn create_image(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, rgba: &[u8]) -> ImageId {
        let image = make_image(device, queue, &self.image_layout, w, h, rgba, true);
        let id = self.next_image;
        self.next_image += 1;
        self.images.insert(id, image);
        id
    }

    /// Like `create_image` but without mips, for content rewritten every frame (video).
    pub fn create_frame_image(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, rgba: &[u8]) -> ImageId {
        let image = make_image(device, queue, &self.image_layout, w, h, rgba, false);
        let id = self.next_image;
        self.next_image += 1;
        self.images.insert(id, image);
        id
    }

    /// Overwrites mip 0 of an image in place; `rgba` must match its size.
    pub fn update_image(&self, queue: &wgpu::Queue, id: ImageId, rgba: &[u8]) {
        let Some(img) = self.images.get(&id) else { return };
        let (w, h) = img.size;
        queue.write_texture(
            img.tex.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: None },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    pub fn free_image(&mut self, id: ImageId) {
        self.images.remove(&id);
    }

    pub fn image_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.images.get(&id).map(|i| i.size)
    }

    /// Width in pixels of a single line of text, for layout.
    pub fn measure(&mut self, text: &str, size: f32, weight: u16, font: Font) -> f32 {
        let key = TextKey { text: text.to_string(), size: size.to_bits(), weight, font, max_width: None };
        self.shape(&key).1
    }

    fn shape(&mut self, key: &TextKey) -> &(Buffer, f32) {
        if !self.buffers.contains_key(key) {
            let fs = &mut self.font_system;
            let size = f32::from_bits(key.size);
            let mut buf = Buffer::new(fs, Metrics::new(size, size * 1.25));
            buf.set_wrap(Wrap::None);
            buf.set_size(key.max_width.map(f32::from_bits), None);
            let family = match key.font {
                Font::Sans => Family::SansSerif,
                Font::Icons => Family::Name(ICON_FAMILY),
            };
            let attrs = Attrs::new().family(family).weight(Weight(key.weight));
            buf.set_text(&key.text, &attrs, Shaping::Advanced, None);
            buf.shape_until_scroll(fs, false);
            let w = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
            self.buffers.insert(key.clone(), (buf, w));
        }
        &self.buffers[key]
    }

    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &Frame, size: (u32, u32)) {
        // Rasterise icons seen for the first time (each size/weight once, then cached).
        for q in frame.base.quads.iter().chain(&frame.overlay.quads).chain(&frame.popup.quads) {
            if let Some(key) = q.icon.filter(|k| !self.icons.contains_key(k)) {
                if let Some(px) = crate::icons::rasterize(key) {
                    let id = self.create_image(device, queue, key.px as u32, key.px as u32, &px);
                    self.icons.insert(key, id);
                }
            }
        }
        // Quads for all layers go into one buffer; batches split by bound image.
        let mut all = Vec::new();
        for (li, layer) in [&frame.base, &frame.overlay, &frame.popup].into_iter().enumerate() {
            self.batches[li].clear();
            for q in &layer.quads {
                let start = all.len() as u32;
                let lin = [srgb_to_linear(q.color[0]), srgb_to_linear(q.color[1]), srgb_to_linear(q.color[2]), q.color[3]];
                let q_image = q.icon.and_then(|k| self.icons.get(&k).copied()).or(q.image);
                let has_image = q_image.is_some_and(|id| self.images.contains_key(&id));
                all.push(QuadInstance {
                    rect: q.rect,
                    color: lin,
                    uv: q.uv,
                    params: [q.radius, if has_image { 1.0 } else { 0.0 }, 0.0, 0.0],
                });
                let image = if has_image { q_image } else { None };
                match self.batches[li].last_mut() {
                    Some(b) if b.2 == image && b.1 == start => b.1 += 1,
                    _ => self.batches[li].push((start, start + 1, image)),
                }
            }
        }
        let bytes = (all.len() * std::mem::size_of::<QuadInstance>()) as u64;
        if bytes > self.instances.size() {
            self.instances = Self::buffer(device, all.len().next_power_of_two());
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&all));

        self.viewport.update(queue, Resolution { width: size.0, height: size.1 });
        let mut used = HashSet::new();
        for (li, layer) in [&frame.base, &frame.overlay, &frame.popup].into_iter().enumerate() {
            let keys: Vec<TextKey> = layer.texts.iter().map(|t| text_key(t)).collect();
            for k in &keys {
                self.shape(k);
                used.insert(k.clone());
            }
            let areas: Vec<TextArea> = layer
                .texts
                .iter()
                .zip(&keys)
                .map(|(t, k)| {
                    let (buf, w) = &self.buffers[k];
                    let w = t.max_width.map_or(*w, |m| w.min(m));
                    let left = match t.align {
                        Align::Left => t.x,
                        Align::Center => t.x - w / 2.0,
                        Align::Right => t.x - w,
                    };
                    let mut bounds = TextBounds { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
                    if let Some(m) = t.max_width {
                        bounds.left = left as i32;
                        bounds.right = (left + m) as i32;
                    }
                    if let Some([x, y, cw, ch]) = t.clip {
                        bounds.left = bounds.left.max(x as i32);
                        bounds.top = y as i32;
                        bounds.right = bounds.right.min((x + cw) as i32);
                        bounds.bottom = (y + ch) as i32;
                    }
                    let [r, g, b, a] = t.color;
                    TextArea {
                        buffer: buf,
                        left,
                        top: t.y,
                        scale: 1.0,
                        bounds,
                        default_color: Color::rgba(r, g, b, a),
                        custom_glyphs: &[],
                    }
                })
                .collect();
            if let Err(e) = self.text[li].prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash,
            ) {
                log::warn!("text: {e}");
            }
        }
        self.buffers.retain(|k, _| used.contains(k));
    }

    pub fn draw_layer(&self, pass: &mut wgpu::RenderPass<'_>, layer: usize) {
        if !self.batches[layer].is_empty() {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.globals_bind, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            for &(start, end, image) in &self.batches[layer] {
                let bind = image.and_then(|id| self.images.get(&id)).map_or(&self.white, |i| &i.bind);
                pass.set_bind_group(1, bind, &[]);
                pass.draw(0..4, start..end);
            }
        }
        let _ = self.text[layer].render(&self.atlas, &self.viewport, pass);
    }

    pub fn trim(&mut self) {
        self.atlas.trim();
    }
}

fn text_key(t: &crate::ui::Text) -> TextKey {
    TextKey {
        text: t.text.clone(),
        size: t.size.to_bits(),
        weight: t.weight,
        font: t.font,
        max_width: t.max_width.map(f32::to_bits),
    }
}

/// Uploads an RGBA8 sRGB image, with a full mip chain if `mips`.
fn make_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    w: u32,
    h: u32,
    rgba: &[u8],
    mips: bool,
) -> Image {
        let mips = if mips { 32 - w.max(h).max(1).leading_zeros() } else { 1 };
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui image"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut level = image::RgbaImage::from_raw(w, h, rgba.to_vec()).expect("rgba size");
        for l in 0..mips {
            if l > 0 {
                let (nw, nh) = ((w >> l).max(1), (h >> l).max(1));
                level = image::imageops::resize(&level, nw, nh, image::imageops::FilterType::Triangle);
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: l,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                level.as_raw(),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(level.width() * 4), rows_per_image: None },
                wgpu::Extent3d { width: level.width(), height: level.height(), depth_or_array_layers: 1 },
            );
        }
        let view = tex.create_view(&Default::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
        });
        Image { tex, bind, size: (w, h) }
}
