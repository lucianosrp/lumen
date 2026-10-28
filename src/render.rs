use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};

use crate::library::Library;
use crate::loader::Tier;

pub const NO_SLOT: u32 = u32::MAX;
pub const LARGE_BIT: u32 = 0x8000_0000;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Instance {
    pub rect: [f32; 4],
    pub th: u32,
    pub slot: u32,
    pub fade: f32,
    pub _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view: [f32; 4],
    th: [f32; 4],
    small: [f32; 4],
    large: [f32; 4],
}

const ATLAS: u32 = 8192;

/// A big texture cut into equal square slots, recycled least-recently-seen first.
pub struct SlotAtlas {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    tier: Tier,
    owner: Vec<usize>,
    last_used: Vec<u64>,
    loaded: Vec<Instant>,
    slot_of: Vec<u32>,
    free: Vec<u32>,
    evict: Vec<u32>,
    evict_frame: u64,
}

impl SlotAtlas {
    fn new(device: &wgpu::Device, tier: Tier) -> Self {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("slot atlas"),
            size: wgpu::Extent3d { width: ATLAS, height: ATLAS, depth_or_array_layers: 1 },
            mip_level_count: Tier::MIPS,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let cap = (ATLAS / tier.slot()).pow(2) as usize;
        Self {
            tex,
            view,
            tier,
            owner: vec![usize::MAX; cap],
            last_used: vec![0; cap],
            loaded: vec![Instant::now(); cap],
            slot_of: Vec::new(),
            free: (0..cap as u32).rev().collect(),
            evict: Vec::new(),
            evict_frame: u64::MAX,
        }
    }

    pub fn capacity(&self) -> usize {
        self.owner.len()
    }

    fn per_row(&self) -> u32 {
        ATLAS / self.tier.slot()
    }

    fn reset(&mut self, n: usize) {
        self.owner.fill(usize::MAX);
        self.free = (0..self.capacity() as u32).rev().collect();
        self.slot_of = vec![NO_SLOT; n];
        self.evict.clear();
    }

    pub fn get(&self, asset: usize) -> Option<u32> {
        self.slot_of.get(asset).copied().filter(|&s| s != NO_SLOT)
    }

    pub fn touch(&mut self, slot: u32, frame: u64) {
        self.last_used[slot as usize] = frame;
    }

    pub fn loaded_at(&self, slot: u32) -> Instant {
        self.loaded[slot as usize]
    }

    fn alloc(&mut self, asset: usize, frame: u64) -> Option<u32> {
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                if self.evict_frame != frame || self.evict.is_empty() {
                    let mut c: Vec<u32> = (0..self.capacity() as u32)
                        .filter(|&s| self.last_used[s as usize] < frame)
                        .collect();
                    c.sort_by_key(|&s| std::cmp::Reverse(self.last_used[s as usize]));
                    self.evict = c;
                    self.evict_frame = frame;
                }
                let s = self.evict.pop()?;
                let old = self.owner[s as usize];
                if old != usize::MAX {
                    self.slot_of[old] = NO_SLOT;
                }
                s
            }
        };
        self.owner[slot as usize] = asset;
        self.slot_of[asset] = slot;
        self.last_used[slot as usize] = frame;
        Some(slot)
    }

    fn upload(&self, queue: &wgpu::Queue, slot: u32, mips: &[Vec<u8>]) {
        let s = self.tier.slot();
        let (x, y) = ((slot % self.per_row()) * s, (slot / self.per_row()) * s);
        for (l, data) in mips.iter().enumerate() {
            let size = s >> l;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.tex,
                    mip_level: l as u32,
                    origin: wgpu::Origin3d { x: x >> l, y: y >> l, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size * 4), rows_per_image: None },
                wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            );
        }
    }

    fn globals(&self) -> [f32; 4] {
        [self.tier.slot() as f32, self.per_row() as f32, ATLAS as f32, Tier::MIPS as f32]
    }
}

pub struct Text {
    pub title: String,
    pub subtitle: String,
    pub status: String,
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    tile_pipeline: wgpu::RenderPipeline,
    header_pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    globals: wgpu::Buffer,
    bind_group: Option<wgpu::BindGroup>,
    th_view: Option<wgpu::TextureView>,
    th_info: [f32; 4],
    pub small: SlotAtlas,
    pub large: SlotAtlas,
    instances: wgpu::Buffer,
    instance_count: u32,

    font_system: FontSystem,
    swash: SwashCache,
    text_viewport: Viewport,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    buffers: [Buffer; 3],
}

impl Renderer {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("grid.wgsl"));
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
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
                tex_entry(2),
                tex_entry(3),
                tex_entry(4),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |vs: &str, fs: &str, buffers: &[Option<wgpu::VertexBufferLayout>], blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(vs),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let tile_pipeline = pipeline(
            "vs_tile",
            "fs_tile",
            &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Uint32, 2 => Uint32, 3 => Float32],
            })],
            None,
        );
        let header_pipeline = pipeline("vs_header", "fs_header", &[], Some(wgpu::BlendState::ALPHA_BLENDING));

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let instances = Self::instance_buffer(&device, 4096);

        let mut font_system = FontSystem::new();
        let cache = Cache::new(&device);
        let text_viewport = Viewport::new(&device, &cache);
        let mut text_atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer = TextRenderer::new(&mut text_atlas, &device, Default::default(), None);
        let buffers = std::array::from_fn(|_| Buffer::new(&mut font_system, Metrics::new(16.0, 20.0)));

        Self {
            small: SlotAtlas::new(&device, Tier::Small),
            large: SlotAtlas::new(&device, Tier::Large),
            device,
            queue,
            tile_pipeline,
            header_pipeline,
            layout,
            sampler,
            globals,
            bind_group: None,
            th_view: None,
            th_info: [16.0, 512.0, 8192.0, 16.0],
            instances,
            instance_count: 0,
            font_system,
            swash: SwashCache::new(),
            text_viewport,
            text_atlas,
            text_renderer,
            buffers,
        }
    }

    fn instance_buffer(device: &wgpu::Device, n: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (n * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn set_library(&mut self, lib: &Library) {
        let th = &lib.thumbhashes;
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("thumbhash atlas"),
            size: wgpu::Extent3d { width: th.width, height: th.height, depth_or_array_layers: 1 },
            mip_level_count: th.levels.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (l, data) in th.levels.iter().enumerate() {
            let (w, h) = ((th.width >> l).max(1), (th.height >> l).max(1));
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: l as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: None },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        self.th_info = [th.cell as f32, th.cols as f32, th.width as f32, th.height as f32];
        self.th_view = Some(tex.create_view(&Default::default()));
        self.small.reset(lib.len());
        self.large.reset(lib.len());
        self.bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(self.th_view.as_ref().unwrap()),
                },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&self.small.view) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&self.large.view) },
            ],
        }));
    }

    /// Puts a freshly decoded thumbnail into its atlas. Returns false if there was no room.
    pub fn upload_thumb(&mut self, index: usize, tier: Tier, mips: &[Vec<u8>], frame: u64, now: Instant) -> bool {
        let atlas = match tier {
            Tier::Small => &mut self.small,
            Tier::Large => &mut self.large,
        };
        if index >= atlas.slot_of.len() || atlas.get(index).is_some() {
            return true;
        }
        let Some(slot) = atlas.alloc(index, frame) else { return false };
        atlas.upload(&self.queue, slot, mips);
        atlas.loaded[slot as usize] = now;
        true
    }

    pub fn set_text(&mut self, text: &Text, scale: f32, width: f32) {
        let fs = &mut self.font_system;
        let specs = [
            (&text.title, 26.0, Weight::BOLD),
            (&text.subtitle, 13.0, Weight::MEDIUM),
            (&text.status, 14.0, Weight::MEDIUM),
        ];
        for (buf, (s, size, weight)) in self.buffers.iter_mut().zip(specs) {
            buf.set_metrics(Metrics::new(size * scale, size * scale * 1.25));
            buf.set_size(Some(width), None);
            buf.set_text(s, &Attrs::new().family(Family::SansSerif).weight(weight), Shaping::Advanced, None);
            buf.shape_until_scroll(fs, false);
        }
    }

    pub fn render(
        &mut self,
        view: &wgpu::TextureView,
        size: (u32, u32),
        header: f32,
        scale: f32,
        tiles: &[Instance],
    ) {
        let (w, h) = size;
        let g = Globals {
            view: [w as f32, h as f32, header, scale],
            th: self.th_info,
            small: self.small.globals(),
            large: self.large.globals(),
        };
        self.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&g));
        let bytes = std::mem::size_of_val(tiles) as u64;
        if bytes > self.instances.size() {
            self.instances = Self::instance_buffer(&self.device, tiles.len().next_power_of_two());
        }
        self.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(tiles));
        self.instance_count = tiles.len() as u32;

        self.text_viewport.update(&self.queue, Resolution { width: w, height: h });
        let pad = 20.0 * scale;
        let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        let line_w = |b: &Buffer| b.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
        let status_w = line_w(&self.buffers[2]);
        let areas = [
            TextArea {
                buffer: &self.buffers[0],
                left: pad,
                top: 14.0 * scale,
                scale: 1.0,
                bounds,
                default_color: Color::rgb(255, 255, 255),
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &self.buffers[1],
                left: pad,
                top: 48.0 * scale,
                scale: 1.0,
                bounds,
                default_color: Color::rgba(255, 255, 255, 165),
                custom_glyphs: &[],
            },
            TextArea {
                buffer: &self.buffers[2],
                left: (w as f32 - status_w - pad).max(pad),
                top: 22.0 * scale,
                scale: 1.0,
                bounds,
                default_color: Color::rgba(255, 255, 255, 190),
                custom_glyphs: &[],
            },
        ];
        if let Err(e) = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.text_atlas,
            &self.text_viewport,
            areas,
            &mut self.swash,
        ) {
            log::warn!("text: {e}");
        }

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.004, g: 0.004, b: 0.005, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(bg) = &self.bind_group {
                pass.set_bind_group(0, bg, &[]);
                if self.instance_count > 0 {
                    pass.set_pipeline(&self.tile_pipeline);
                    pass.set_vertex_buffer(0, self.instances.slice(..));
                    pass.draw(0..4, 0..self.instance_count);
                }
                pass.set_pipeline(&self.header_pipeline);
                pass.draw(0..4, 0..1);
            }
            let _ = self.text_renderer.render(&self.text_atlas, &self.text_viewport, &mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
        self.text_atlas.trim();
    }
}
