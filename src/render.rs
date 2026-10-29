use std::collections::HashMap;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use crate::library::Library;
use crate::ui::Frame;
use crate::ui_render::UiRenderer;
use crate::loader::Tier;
use crate::settings::GpuMemory;

pub const NO_SLOT: u32 = u32::MAX;
pub const LARGE_BIT: u32 = 0x8000_0000;
pub const XL_BIT: u32 = 0x4000_0000;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Instance {
    pub rect: [f32; 4],
    pub th: u32,
    pub slot: u32,
    pub fade: f32,
    /// `SELECTED_BIT`, plus how far the tile has faded out (`LEAVING_SHIFT`, 0..255).
    pub flags: u32,
}

pub const SELECTED_BIT: u32 = 1;
pub const LEAVING_SHIFT: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view: [f32; 4],
    th: [f32; 4],
    small: [f32; 4],
    large: [f32; 4],
    xl: [f32; 4],
    xl_size: [f32; 4],
    /// Small atlas width, height; large atlas width, height.
    sizes: [f32; 4],
    /// Theme colours in linear light: selection accent, the tick on it, and the background
    /// (leaving tiles fade to it; the header scrim is made of it).
    accent: [f32; 4],
    on_accent: [f32; 4],
    bg: [f32; 4],
}

/// Atlas texture size per tier for a memory budget, clamped to what the GPU supports.
/// All sizes are multiples of the largest slot (1024), so slots tile them exactly.
pub fn atlas_dims(mem: GpuMemory, tier: Tier, max: u32) -> (u32, u32) {
    let (w, h) = match (mem, tier) {
        // ~255 MB: fits an integrated GPU.
        (GpuMemory::Low, Tier::Small) => (4096, 4096),
        (GpuMemory::Low, Tier::Large) => (4096, 5120),
        (GpuMemory::Low, Tier::Xl) => (4096, 3072),
        // ~895 MB.
        (GpuMemory::Normal, Tier::Xl) => (8192, 4096),
        (GpuMemory::Normal, _) => (8192, 8192),
        // ~1.4 GB.
        (GpuMemory::High, Tier::Large) => (8192, 16384),
        (GpuMemory::High, _) => (8192, 8192),
    };
    let max = (max / 1024 * 1024).max(1024);
    (w.min(max), h.min(max))
}

/// Video memory of one atlas including its mips.
pub fn atlas_bytes((w, h): (u32, u32)) -> u64 {
    (0..Tier::MIPS).map(|l| (w as u64 >> l) * (h as u64 >> l) * 4).sum()
}

/// A big texture cut into equal square slots, recycled least-recently-seen first.
///
/// Slots remember which asset (by id) they hold, so switching collections re-attaches every
/// thumbnail still in the atlas instead of loading it again. Slots whose asset isn't in the
/// current collection stay "parked" until space is needed.
pub struct SlotAtlas {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    tier: Tier,
    width: u32,
    height: u32,
    owner: Vec<usize>,
    /// Asset id held by each slot ("" when empty), current collection or not.
    holds: Vec<String>,
    last_used: Vec<u64>,
    loaded: Vec<Instant>,
    slot_of: Vec<u32>,
    free: Vec<u32>,
    evict: Vec<u32>,
    evict_frame: u64,
}

impl SlotAtlas {
    fn new(device: &wgpu::Device, tier: Tier, (width, height): (u32, u32)) -> Self {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("slot atlas"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: Tier::MIPS,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let cap = ((width / tier.slot()) * (height / tier.slot())) as usize;
        Self {
            tex,
            view,
            tier,
            width,
            height,
            owner: vec![usize::MAX; cap],
            holds: vec![String::new(); cap],
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
        self.width / self.tier.slot()
    }

    pub fn bytes(&self) -> u64 {
        atlas_bytes((self.width, self.height))
    }

    /// Attaches to a new collection (or an edited one), given its asset ids in order: slots
    /// holding one of them serve it again; the rest stay parked, first in line for eviction.
    fn adopt(&mut self, ids: &[String]) {
        let at: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        self.slot_of = vec![NO_SLOT; ids.len()];
        self.free.clear();
        for s in 0..self.capacity() {
            self.owner[s] = usize::MAX;
            match at.get(self.holds[s].as_str()) {
                _ if self.holds[s].is_empty() => self.free.push(s as u32),
                Some(&j) => {
                    self.owner[s] = j;
                    self.slot_of[j] = s as u32;
                }
                None => {}
            }
        }
        self.free.reverse();
        self.evict.clear();
    }

    /// How many of the current collection's assets have a thumbnail in this atlas.
    pub fn attached(&self) -> usize {
        self.slot_of.iter().filter(|&&s| s != NO_SLOT).count()
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

    fn alloc(&mut self, asset: usize, id: &str, frame: u64) -> Option<u32> {
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
        self.holds[slot as usize] = id.to_string();
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
        [self.tier.slot() as f32, self.per_row() as f32, self.width as f32, Tier::MIPS as f32]
    }
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
    pub xl: SlotAtlas,
    /// The current collection's asset ids, by index.
    ids: Vec<String>,
    instances: wgpu::Buffer,
    instance_count: u32,
    pub ui: UiRenderer,
    pub gpu_memory: GpuMemory,
    /// The GPU in use, for the About page.
    pub adapter: wgpu::AdapterInfo,
}

impl Renderer {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        adapter: wgpu::AdapterInfo,
        gpu_memory: GpuMemory,
    ) -> Self {
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
                tex_entry(5),
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
                attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Uint32, 2 => Uint32, 3 => Float32, 4 => Uint32],
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
        let ui = UiRenderer::new(&device, &queue, format, &globals);

        let max = device.limits().max_texture_dimension_2d;
        let atlas = |tier| SlotAtlas::new(&device, tier, atlas_dims(gpu_memory, tier, max));
        Self {
            small: atlas(Tier::Small),
            large: atlas(Tier::Large),
            xl: atlas(Tier::Xl),
            ids: Vec::new(),
            gpu_memory,
            adapter,
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
            ui,
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

    /// Shows another collection (or an edited copy of this one, see `Library::edit`). Thumbnails
    /// already on the GPU are kept and found again by asset id.
    pub fn set_library(&mut self, lib: &Library) {
        self.upload_thumbhashes(lib);
        self.ids = lib.assets.iter().map(|a| a.id.clone()).collect();
        for atlas in [&mut self.small, &mut self.large, &mut self.xl] {
            atlas.adopt(&self.ids);
        }
        self.rebind();
    }

    fn upload_thumbhashes(&mut self, lib: &Library) {
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
    }


    /// Recreates the thumbnail atlases for a new memory budget; what they held reloads.
    pub fn set_gpu_memory(&mut self, mem: GpuMemory) {
        if mem == self.gpu_memory {
            return;
        }
        self.gpu_memory = mem;
        let max = self.device.limits().max_texture_dimension_2d;
        for tier in [Tier::Small, Tier::Large, Tier::Xl] {
            let mut atlas = SlotAtlas::new(&self.device, tier, atlas_dims(mem, tier, max));
            atlas.adopt(&self.ids);
            *match tier {
                Tier::Small => &mut self.small,
                Tier::Large => &mut self.large,
                Tier::Xl => &mut self.xl,
            } = atlas;
        }
        log::info!("thumbnail atlases now {} MB ({mem:?})", self.atlas_bytes() >> 20);
        if self.th_view.is_some() {
            self.rebind();
        }
    }

    pub fn atlas_bytes(&self) -> u64 {
        self.small.bytes() + self.large.bytes() + self.xl.bytes()
    }

    fn rebind(&mut self) {
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
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&self.xl.view) },
            ],
        }));
    }

    /// Puts a freshly decoded thumbnail into its atlas. Returns false if there was no room.
    pub fn upload_thumb(&mut self, index: usize, tier: Tier, mips: &[Vec<u8>], frame: u64, now: Instant) -> bool {
        let atlas = match tier {
            Tier::Small => &mut self.small,
            Tier::Large => &mut self.large,
            Tier::Xl => &mut self.xl,
        };
        if index >= atlas.slot_of.len() || atlas.get(index).is_some() {
            return true;
        }
        let Some(id) = self.ids.get(index) else { return true };
        let Some(slot) = atlas.alloc(index, id, frame) else { return false };
        atlas.upload(&self.queue, slot, mips);
        atlas.loaded[slot as usize] = now;
        true
    }

    pub fn render(
        &mut self,
        view: &wgpu::TextureView,
        size: (u32, u32),
        header: f32,
        scale: f32,
        tiles: &[Instance],
        frame: &Frame,
    ) {
        let (w, h) = size;
        let g = Globals {
            view: [w as f32, h as f32, header, scale],
            th: self.th_info,
            small: self.small.globals(),
            large: self.large.globals(),
            xl: self.xl.globals(),
            xl_size: [self.xl.width as f32, self.xl.height as f32, 0.0, 0.0],
            sizes: [self.small.width as f32, self.small.height as f32, self.large.width as f32, self.large.height as f32],
            accent: linear(crate::theme::palette().accent),
            on_accent: linear(crate::theme::palette().on_accent),
            bg: linear(crate::theme::scrim()),
        };
        self.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&g));
        let bytes = std::mem::size_of_val(tiles) as u64;
        if bytes > self.instances.size() {
            self.instances = Self::instance_buffer(&self.device, tiles.len().next_power_of_two());
        }
        self.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(tiles));
        self.instance_count = tiles.len() as u32;

        self.ui.prepare(&self.device, &self.queue, frame, size);

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color()),
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
            self.ui.draw_layer(&mut pass, 0);
            self.ui.draw_layer(&mut pass, 1);
            self.ui.draw_layer(&mut pass, 2);
        }
        self.queue.submit(Some(encoder.finish()));
        self.ui.trim();
    }
}

/// A palette colour in linear light, for shaders writing to the sRGB target.
fn linear(hex: u32) -> [f32; 4] {
    let [r, g, b, _] = crate::theme::rgba(hex, 1.0);
    let lin = |c: f32| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    [lin(r), lin(g), lin(b), 1.0]
}

/// The theme background.
fn clear_color() -> wgpu::Color {
    let [r, g, b, _] = linear(crate::theme::bg());
    wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: 1.0 }
}
