//! The scene renderer (render.md 6): one renderer for the window, the headset's eyes and the capture.
//!
//! - **What it draws:** each object's `Shape` as a built-in unit mesh (sphere, box, capsule), scaled and instanced,
//!   lit by its `Appearance`; the world's `Light`s (up to 16, plus one shadow-casting directional key light).
//!   A mesh `BlobRef` is drawn as its bounding stand-in (a box inscribed in its bounding sphere) until the blob store
//!   is reachable from pe-render; that is not in M1's S5.
//! - **Camera-relative:** each frame picks an anchor, the first view's camera `Pos`. Instances are written as
//!   `object Pos − anchor Pos` (an f64 difference, then f32) along world axes; each view's matrix carries only its
//!   eye's rotation and small offset from the anchor. So both eyes share one instance upload, and no absolute
//!   coordinate reaches the GPU.
//! - **The cue** (D3, R6): an outline pass (back faces of an inflated shell, alpha-blended) and a ghost pass (the
//!   object at the pose it left, faint), after the opaque draws, in the same render pass.
//! - **Stereo:** one pass per view here. Multiview (Vulkan, both eyes in one pass) comes with pe-xr's
//!   `render_views` (render.md 16, step 3).

use std::collections::BTreeMap;

use bytemuck::{Pod, Zeroable};
use glam::{DMat3, DQuat, DVec3, Mat4, Vec3, Vec4};
use pe_seams::{Appearance, FrameView, LightKind, ObjectId, Pos, Shape};

use crate::camera::Camera;
use crate::display::DisplayFrame;
use crate::gpu::GpuContext;
use crate::pose::{WorldPose, offset_vec};

pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SHADOW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const MAX_LIGHTS: usize = 16;
/// One view's uniform slot, a multiple of every adapter's uniform offset alignment (256).
const SLOT: u64 = 1536;

/// The appearance of an object with none (world-model.md 3.2 makes `Appearance` optional).
pub const DEFAULT_APPEARANCE: Appearance =
    Appearance { base_color: [0.7, 0.7, 0.7, 1.0], emissive: [0.0; 3], roughness: 0.6, metalness: 0.0, material: None };

/// The key light and ambient term used when the world defines no directional light.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Environment {
    /// The direction the key light travels, world axes.
    pub key_dir: [f32; 3],
    /// Linear RGB × intensity.
    pub key_color: [f32; 3],
    pub ambient: [f32; 3],
    /// The clear colour, linear RGB.
    pub sky: [f32; 3],
    /// Half the side of the square the key light's shadow covers, around the anchor, in metres.
    pub shadow_extent: f32,
}

impl Default for Environment {
    fn default() -> Self {
        Environment {
            key_dir: [-0.4, -1.0, -0.3],
            key_color: [2.6, 2.5, 2.35],
            ambient: [0.16, 0.17, 0.2],
            sky: [0.05, 0.06, 0.08],
            shadow_extent: 12.0,
        }
    }
}

/// One drawable, posed.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawItem {
    pub id: ObjectId,
    pub pose: WorldPose,
    pub scale: [f32; 3],
    pub shape: Shape,
    pub appearance: Appearance,
    pub radius: f32,
    /// An outline (the cue, or a capture's highlight): linear RGBA.
    pub outline: Option<[f32; 4]>,
    /// The cue's ghost: the pose the object left, and its opacity.
    pub ghost: Option<(WorldPose, f32)>,
}

/// A light, posed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightItem {
    pub pose: WorldPose,
    pub light: pe_seams::Light,
}

/// What one frame draws: built from a frame view (simulated poses: the capture) or from the display layer's frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub items: Vec<DrawItem>,
    pub lights: Vec<LightItem>,
    pub environment: Environment,
}

/// The cue's outline colour: a soft white, so it reads on any base colour without flashing.
const CUE_COLOR: [f32; 3] = [0.95, 0.95, 1.0];

impl Scene {
    /// The view at its simulated poses, the display layer bypassed: what the world holds (render.md 11).
    pub fn simulated(view: &FrameView) -> Scene {
        let mut s = Scene::default();
        for o in &view.objects {
            let pose = WorldPose::of(o);
            if let Some(l) = o.light {
                s.lights.push(LightItem { pose, light: l });
            }
            if let Some(shape) = o.shape {
                s.items.push(DrawItem {
                    id: o.id,
                    pose,
                    scale: o.scale,
                    shape,
                    appearance: o.appearance.unwrap_or(DEFAULT_APPEARANCE),
                    radius: o.radius,
                    outline: None,
                    ghost: None,
                });
            }
        }
        s
    }

    /// The view at the display layer's drawn poses, with its cues.
    pub fn displayed(view: &FrameView, frame: &DisplayFrame) -> Scene {
        let mut s = Scene::default();
        for d in &frame.objects {
            let Some(o) = view.object(d.id) else { continue };
            if let Some(l) = o.light {
                s.lights.push(LightItem { pose: d.pose, light: l });
            }
            let Some(shape) = o.shape else { continue };
            s.items.push(DrawItem {
                id: o.id,
                pose: d.pose,
                scale: o.scale,
                shape,
                appearance: o.appearance.unwrap_or(DEFAULT_APPEARANCE),
                radius: o.radius,
                outline: d.cue.map(|c| [CUE_COLOR[0], CUE_COLOR[1], CUE_COLOR[2], c.outline]),
                ghost: d.cue.filter(|c| c.ghost_alpha > 0.0).map(|c| (c.ghost, c.ghost_alpha)),
            });
        }
        s
    }
}

/// One view to draw: a camera and the colour target it draws into.
pub struct EyeView<'a> {
    pub camera: Camera,
    pub color: &'a wgpu::TextureView,
    pub format: wgpu::TextureFormat,
    pub size: [u32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderSettings {
    /// 4 on the desktop; in VR the render scale controller lowers it (render.md 10).
    pub msaa: u32,
    /// The key light's shadow map side (2048 [I]); 0 turns shadows off.
    pub shadow_size: u32,
    /// The outline's width per metre of distance from the eye.
    pub outline_width: f32,
    /// Draw both eyes in one multiview pass where the device can (render.md 5); false forces one pass per eye.
    pub multiview: bool,
}

impl Default for RenderSettings {
    fn default() -> Self {
        RenderSettings { msaa: 4, shadow_size: 2048, outline_width: 0.004, multiview: true }
    }
}

/// One frame's numbers (render.md 13).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStats {
    pub views: u32,
    pub instances: u32,
    pub draws: u32,
    pub lights: u32,
    pub cpu_record_ms: f64,
    /// A GPU time that arrived during this call: an earlier frame's, usually one or two frames back. `None` when no
    /// new reading arrived (each reading is reported once, review L4), and always on an adapter without timestamp
    /// queries. Captures are not timed.
    pub gpu_ms: Option<f64>,
}

/// Frames whose timestamps can be in flight at once. A frame finding its slot still mapping goes untimed.
const TIMER_RING: usize = 3;

/// GPU timestamps around a frame's passes, read back without ever blocking (render.md 10).
struct GpuTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    slots: Vec<TimerSlot>,
    next: usize,
    frame: u64,
    /// Nanoseconds per timestamp tick.
    period_ns: f64,
    /// The newest reading: (frame number, milliseconds).
    latest: Option<(u64, f64)>,
}

struct TimerSlot {
    buf: wgpu::Buffer,
    /// 0 free, 1 mapping, 2 mapped.
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    frame: u64,
}

impl GpuTimer {
    fn new(ctx: &GpuContext) -> Option<GpuTimer> {
        if !ctx.device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let d = &ctx.device;
        let set = d.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("frame timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: 2 * TIMER_RING as u32,
        });
        let size = wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT * TIMER_RING as u64;
        let resolve = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("timestamp resolve"),
            size,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let slots = (0..TIMER_RING)
            .map(|_| TimerSlot {
                buf: d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("timestamp readback"),
                    size: 16,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                state: Default::default(),
                frame: 0,
            })
            .collect();
        Some(GpuTimer {
            set,
            resolve,
            slots,
            next: 0,
            frame: 0,
            period_ns: ctx.queue.get_timestamp_period() as f64,
            latest: None,
        })
    }

    /// Collect every reading that has arrived; never waits. Returns the newest of them.
    fn harvest(&mut self, device: &wgpu::Device) -> Option<f64> {
        let before = self.latest;
        use std::sync::atomic::Ordering;
        let _ = device.poll(wgpu::PollType::Poll);
        for s in &mut self.slots {
            if s.state.load(Ordering::Acquire) != 2 {
                continue;
            }
            let ticks: Option<[u64; 2]> =
                s.buf.slice(..).get_mapped_range().ok().map(|d| bytemuck::pod_read_unaligned(&d[..16]));
            s.buf.unmap();
            s.state.store(0, Ordering::Release);
            // An end before the start (a reset counter, or a pass the driver reordered) is no reading.
            let Some(ticks) = ticks else { continue };
            if ticks[1] >= ticks[0] && self.latest.is_none_or(|(f, _)| s.frame > f) {
                self.latest = Some((s.frame, (ticks[1] - ticks[0]) as f64 * self.period_ns / 1e6));
            }
        }
        (self.latest != before).then_some(self.latest).flatten().map(|(_, ms)| ms)
    }

    /// The slot this frame writes, if one is free.
    fn begin(&mut self) -> Option<usize> {
        let i = self.next;
        (self.slots[i].state.load(std::sync::atomic::Ordering::Acquire) == 0).then_some(i)
    }

    fn writes(&self, slot: usize, first: bool, last: bool) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        (first || last).then(|| wgpu::RenderPassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: first.then_some(2 * slot as u32),
            end_of_pass_write_index: last.then_some(2 * slot as u32 + 1),
        })
    }

    fn resolve(&self, enc: &mut wgpu::CommandEncoder, slot: usize) {
        let q = 2 * slot as u32;
        let at = wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT * slot as u64;
        enc.resolve_query_set(&self.set, q..q + 2, &self.resolve, at);
        enc.copy_buffer_to_buffer(&self.resolve, at, &self.slots[slot].buf, 0, 16);
    }

    /// After the frame's submit: map the slot's readback.
    fn submitted(&mut self, slot: usize) {
        use std::sync::atomic::Ordering;
        self.frame += 1;
        let s = &mut self.slots[slot];
        s.frame = self.frame;
        s.state.store(1, Ordering::Release);
        let state = s.state.clone();
        s.buf
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| state.store(if r.is_ok() { 2 } else { 0 }, Ordering::Release));
        self.next = (slot + 1) % TIMER_RING;
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
struct Instance {
    model: [[f32; 4]; 4],
    color: [f32; 4],
    emissive_rough: [f32; 4],
    params: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuLight {
    pos_range: [f32; 4],
    color_kind: [f32; 4],
    dir_outer: [f32; 4],
    inner: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    /// Per view: multiview's two eyes, or `[0]` alone in single-view (scene.wgsl).
    view_proj: [[[f32; 4]; 4]; 2],
    light_vp: [[f32; 4]; 4],
    eye: [[f32; 4]; 2],
    key_dir: [f32; 4],
    key_color: [f32; 4],
    ambient: [f32; 4],
    counts: [u32; 4],
    lights: [GpuLight; MAX_LIGHTS],
}

const _: () = assert!(std::mem::size_of::<Globals>() as u64 <= SLOT);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MeshKey {
    Sphere,
    Box,
    /// A unit-radius capsule whose half-height is this many 64ths of its radius.
    Capsule(u32),
}

struct Mesh {
    vb: wgpu::Buffer,
    ib: wgpu::Buffer,
    n: u32,
}

struct Pipelines {
    main: wgpu::RenderPipeline,
    ghost: wgpu::RenderPipeline,
    outline: wgpu::RenderPipeline,
}

struct Targets {
    size: [u32; 2],
    samples: u32,
    format: wgpu::TextureFormat,
    layers: u32,
    sampled_depth: bool,
    msaa_color: Option<wgpu::TextureView>,
    depth: wgpu::TextureView,
    /// Each layer of the depth, as a 2D view (the headset's depth resolve reads them).
    depth_layers: Vec<wgpu::TextureView>,
}

/// The renderer. Owns its GPU resources; a lost context is handled by building a new `Renderer` on the new
/// context (the display layer's state lives outside it, so nothing jumps; render.md 4).
pub struct Renderer {
    ctx: GpuContext,
    settings: RenderSettings,
    module: wgpu::ShaderModule,
    /// The multiview variant, made when a stereo frame first needs it.
    module_mv: Option<wgpu::ShaderModule>,
    depth_copy_module: Option<wgpu::ShaderModule>,
    depth_copy: BTreeMap<(u32, bool), wgpu::RenderPipeline>,
    globals_bgl: wgpu::BindGroupLayout,
    main_layout: wgpu::PipelineLayout,
    shadow_pipeline: wgpu::RenderPipeline,
    pipelines: BTreeMap<(u32, u32, bool), Pipelines>,
    meshes: BTreeMap<MeshKey, Mesh>,
    globals: wgpu::Buffer,
    globals_slots: u64,
    globals_bg: wgpu::BindGroup,
    shadow_view: wgpu::TextureView,
    shadow_bg: wgpu::BindGroup,
    instances: wgpu::Buffer,
    instance_cap: u64,
    targets: Vec<Targets>,
    timer: Option<GpuTimer>,
}

/// The scene shader, single-view or multiview (scene.wgsl's header).
fn shader_source(multiview: bool) -> String {
    let (param, index) = if multiview { (", @builtin(view_index) view: u32", "view") } else { ("", "0u") };
    include_str!("scene.wgsl").replace("/*VIEW*/", param).replace("VIEW_INDEX", index)
}

fn format_key(f: wgpu::TextureFormat) -> u32 {
    // A stable key for the pipeline cache: wgpu's formats have no Ord.
    let s = format!("{f:?}");
    s.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619))
}

impl Renderer {
    pub fn new(ctx: GpuContext, settings: RenderSettings) -> Renderer {
        let device = &ctx.device;
        // Under the creation lock: if the race in gpu.rs is FXC's, a compile is where it would bite.
        let compiling = crate::gpu::creation_lock();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pe-render scene"),
            source: wgpu::ShaderSource::Wgsl(shader_source(false).into()),
        });
        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<Globals>() as u64),
                },
                count: None,
            }],
        });
        let shadow_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
            ],
        });
        let main_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene"),
            bind_group_layouts: &[Some(&globals_bgl), Some(&shadow_bgl)],
            immediate_size: 0,
        });
        let shadow_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadow"),
            bind_group_layouts: &[Some(&globals_bgl)],
            immediate_size: 0,
        });
        let shadow_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shadow"),
            layout: Some(&shadow_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_shadow"),
                compilation_options: Default::default(),
                buffers: &vertex_layouts(),
            },
            primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: SHADOW_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: wgpu::DepthBiasState { constant: 2, slope_scale: 2.0, clamp: 0.0 },
            }),
            multisample: Default::default(),
            fragment: None,
            multiview_mask: None,
            cache: None,
        });
        drop(compiling);
        let globals_slots = 4;
        let globals = Self::globals_buffer(device, globals_slots);
        let globals_bg = Self::globals_bind_group(device, &globals_bgl, &globals);
        let (shadow_view, shadow_bg) = Self::shadow_target(device, &shadow_bgl, settings.shadow_size.max(1));
        let instance_cap = 1024;
        let instances = Self::instance_buffer(device, instance_cap);
        let mut meshes = BTreeMap::new();
        meshes.insert(MeshKey::Sphere, upload(device, sphere_mesh(32, 16)));
        meshes.insert(MeshKey::Box, upload(device, box_mesh()));
        Renderer {
            ctx,
            settings,
            module,
            module_mv: None,
            depth_copy_module: None,
            depth_copy: BTreeMap::new(),
            globals_bgl,
            main_layout,
            shadow_pipeline,
            pipelines: BTreeMap::new(),
            meshes,
            globals,
            globals_slots,
            globals_bg,
            shadow_view,
            shadow_bg,
            instances,
            instance_cap,
            targets: Vec::new(),
            timer: None,
        }
        .with_timer()
    }

    fn with_timer(mut self) -> Renderer {
        self.timer = GpuTimer::new(&self.ctx);
        self
    }

    pub fn context(&self) -> &GpuContext {
        &self.ctx
    }

    pub fn settings(&self) -> &RenderSettings {
        &self.settings
    }

    /// The MSAA sample count from the next frame (the quality ladder, render.md 10). Pipelines and targets per count
    /// are made when first needed.
    pub fn set_msaa(&mut self, samples: u32) {
        self.settings.msaa = samples.max(1);
    }

    fn globals_buffer(device: &wgpu::Device, slots: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: slots * SLOT,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn globals_bind_group(device: &wgpu::Device, bgl: &wgpu::BindGroupLayout, buf: &wgpu::Buffer) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: buf,
                    offset: 0,
                    size: wgpu::BufferSize::new(std::mem::size_of::<Globals>() as u64),
                }),
            }],
        })
    }

    fn shadow_target(
        device: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        size: u32,
    ) -> (wgpu::TextureView, wgpu::BindGroup) {
        let view = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("shadow map"),
                size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: SHADOW_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&Default::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadow"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadow"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        (view, bg)
    }

    fn instance_buffer(device: &wgpu::Device, cap: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: cap * std::mem::size_of::<Instance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn pipelines(&mut self, format: wgpu::TextureFormat, samples: u32, multiview: bool) -> &Pipelines {
        let key = (format_key(format), samples, multiview);
        if !self.pipelines.contains_key(&key) {
            // D3D12 compiles shaders here, at pipeline creation: under the creation lock (gpu.rs).
            let _compiling = crate::gpu::creation_lock();
            let device = &self.ctx.device;
            if multiview && self.module_mv.is_none() {
                self.module_mv = Some(device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("pe-render scene, multiview"),
                    source: wgpu::ShaderSource::Wgsl(shader_source(true).into()),
                }));
            }
            let module = if multiview { self.module_mv.as_ref().expect("made above") } else { &self.module };
            let mask = multiview.then(|| std::num::NonZeroU32::new(0b11).expect("non-zero"));
            let make = |label, vs, fs, cull, depth_write, blend: Option<wgpu::BlendState>| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&self.main_layout),
                    vertex: wgpu::VertexState {
                        module,
                        entry_point: Some(vs),
                        compilation_options: Default::default(),
                        buffers: &vertex_layouts(),
                    },
                    primitive: wgpu::PrimitiveState { cull_mode: cull, ..Default::default() },
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: DEPTH_FORMAT,
                        depth_write_enabled: Some(depth_write),
                        // Reverse Z: nearer is greater.
                        depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: wgpu::MultisampleState { count: samples, ..Default::default() },
                    fragment: Some(wgpu::FragmentState {
                        module,
                        entry_point: Some(fs),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                    }),
                    multiview_mask: mask,
                    cache: None,
                })
            };
            let p = Pipelines {
                main: make("scene", "vs_main", "fs_main", Some(wgpu::Face::Back), true, None),
                ghost: make(
                    "ghost",
                    "vs_main",
                    "fs_ghost",
                    Some(wgpu::Face::Back),
                    false,
                    Some(wgpu::BlendState::ALPHA_BLENDING),
                ),
                outline: make(
                    "outline",
                    "vs_outline",
                    "fs_outline",
                    Some(wgpu::Face::Front),
                    false,
                    Some(wgpu::BlendState::ALPHA_BLENDING),
                ),
            };
            self.pipelines.insert(key, p);
        }
        &self.pipelines[&key]
    }

    fn mesh_for(&mut self, shape: &Shape, radius: f32) -> (MeshKey, [f32; 3]) {
        match *shape {
            Shape::Sphere { radius } => (MeshKey::Sphere, [radius; 3]),
            Shape::Box { half_extents } => (MeshKey::Box, half_extents),
            Shape::Capsule { radius, half_height } => {
                let r = radius.max(1e-6);
                let q = ((half_height / r) * 64.0).round().clamp(0.0, 64.0 * 64.0) as u32;
                let key = MeshKey::Capsule(q);
                if !self.meshes.contains_key(&key) {
                    let m = upload(&self.ctx.device, capsule_mesh(q as f32 / 64.0, 32, 8));
                    self.meshes.insert(key, m);
                }
                (key, [r; 3])
            }
            // The blob's bounding stand-in: a box inscribed in its bounding sphere (world scale already applied).
            Shape::Mesh(_) => (MeshKey::Box, [radius / 3f32.sqrt(); 3]),
        }
    }

    fn target_for(&mut self, i: usize, size: [u32; 2], format: wgpu::TextureFormat, samples: u32, want: TargetUse) {
        let fits = self.targets.get(i).is_some_and(|t| {
            t.size == size
                && t.samples == samples
                && t.format == format
                && t.layers == want.layers
                && t.sampled_depth >= want.sampled_depth
        });
        if fits {
            return;
        }
        let device = &self.ctx.device;
        let layers = want.layers;
        let tex = |label, format, samples, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: layers },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let all = |t: &wgpu::Texture| {
            t.create_view(&wgpu::TextureViewDescriptor {
                dimension: Some(if layers > 1 {
                    wgpu::TextureViewDimension::D2Array
                } else {
                    wgpu::TextureViewDimension::D2
                }),
                ..Default::default()
            })
        };
        let depth_usage = if want.sampled_depth {
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        };
        let depth_tex = tex("depth", DEPTH_FORMAT, samples, depth_usage);
        let t = Targets {
            size,
            samples,
            format,
            layers,
            sampled_depth: want.sampled_depth,
            msaa_color: (samples > 1)
                .then(|| all(&tex("msaa colour", format, samples, wgpu::TextureUsages::RENDER_ATTACHMENT))),
            depth: all(&depth_tex),
            depth_layers: (0..layers).map(|l| layer_view(&depth_tex, l)).collect(),
        };
        while self.targets.len() <= i {
            self.targets.push(Targets {
                size: [0, 0],
                samples: 0,
                format,
                layers: 0,
                sampled_depth: false,
                msaa_color: None,
                depth: t.depth.clone(),
                depth_layers: Vec::new(),
            });
        }
        self.targets[i] = t;
    }

    /// Whether this device can draw both eyes in one pass at `samples` (render.md 5): Vulkan's multiview, and with
    /// MSAA also multisampled array textures. Otherwise one pass per eye (the D3D12 fallback under FXC).
    pub fn multiview_available(&self, samples: u32) -> bool {
        let f = self.ctx.device.features();
        self.settings.multiview
            && f.contains(wgpu::Features::MULTIVIEW)
            && self.ctx.device.limits().max_multiview_view_count >= 2
            && (samples <= 1 || f.contains(wgpu::Features::MULTISAMPLE_ARRAY))
    }

    /// Draw `scene` into every view. One instance upload serves all views. The frame is timed on the GPU where the
    /// adapter allows (`FrameStats::gpu_ms`).
    pub fn render_views(&mut self, scene: &Scene, views: &[EyeView<'_>]) -> FrameStats {
        self.draw(scene, views, true)
    }

    /// [`Renderer::render_views`], untimed when `timed` is false (captures, which run between frames).
    pub(crate) fn draw(&mut self, scene: &Scene, views: &[EyeView<'_>], timed: bool) -> FrameStats {
        let t0 = std::time::Instant::now();
        let fresh = self.timer.as_mut().and_then(|t| t.harvest(&self.ctx.device));
        let timing = if timed { self.timer.as_mut().and_then(GpuTimer::begin) } else { None };
        let mut stats = FrameStats { views: views.len() as u32, ..Default::default() };
        let Some(first) = views.first() else { return stats };
        let samples = self.settings.msaa.max(1);
        let p = self.prepare(scene, first.camera.pose.pos, &mut stats);

        // One uniform slot per view, plus one for the shadow pass.
        let shadow_slot = views.len() as u64;
        self.ensure_slots(shadow_slot + 1);
        self.write_slot(shadow_slot, &self.globals_for(&p, [p.light_vp; 2], [Vec4::ZERO; 2], false));
        for (i, v) in views.iter().enumerate() {
            let (vp, eye) = self.view_of(&p, &v.camera);
            self.write_slot(i as u64, &self.globals_for(&p, [vp; 2], [eye; 2], !v.format.is_srgb()));
        }
        for (i, v) in views.iter().enumerate() {
            self.target_for(i, v.size, v.format, samples, TargetUse { layers: 1, sampled_depth: false });
        }
        for v in views {
            self.pipelines(v.format, samples, false);
        }

        let passes = p.shadows as usize + views.len();
        let mut enc =
            self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pe-render") });
        let ts = |k: usize| timing.and_then(|slot| self.timer.as_ref()?.writes(slot, k == 0, k + 1 == passes));
        if p.shadows {
            stats.draws += self.shadow_pass(&mut enc, &p, shadow_slot, ts(0));
        }
        for (i, v) in views.iter().enumerate() {
            let t = &self.targets[i];
            let pass = PassTarget {
                color: t.msaa_color.as_ref().unwrap_or(v.color),
                resolve: t.msaa_color.as_ref().map(|_| v.color),
                depth: &t.depth,
                store_depth: false,
                extent: v.size,
                multiview: false,
            };
            let pl = &self.pipelines[&(format_key(v.format), samples, false)];
            stats.draws += self.scene_pass(&mut enc, &p, pl, i as u64, &pass, ts(p.shadows as usize + i));
        }
        self.finish(enc, timing, fresh, t0, stats)
    }

    /// Draw `scene` for both eyes of a headset into `target`'s two array layers (render.md 5, xr.md 4.2), and submit.
    ///
    /// - One multiview pass where [`Renderer::multiview_available`], else one pass per eye.
    /// - Only `target.extent` (from the origin) is drawn: the render scale is a viewport, so the swapchain never
    ///   changes size, and the projection layer's `imageRect` must match it.
    /// - With a depth target (`XR_KHR_composition_layer_depth`), reverse-Z depth is written into it, MSAA depth
    ///   resolved to the nearest sample; [`crate::camera::DepthRange::xr_depth_info`] gives the layer's range
    ///   (review L2), from the cameras' unscaled depth range.
    /// - The frame's work ends with the colour image back in COLOR_TARGET and the depth image in
    ///   DEPTH_STENCIL_WRITE, the states OpenXR handed them over in (xr.md 4.2). The runtime's image can be
    ///   released once this returns.
    pub fn render_stereo(&mut self, scene: &Scene, eyes: [Camera; 2], target: &StereoTarget<'_>) -> FrameStats {
        let t0 = std::time::Instant::now();
        let fresh = self.timer.as_mut().and_then(|t| t.harvest(&self.ctx.device));
        let timing = self.timer.as_mut().and_then(GpuTimer::begin);
        let mut stats = FrameStats { views: 2, ..Default::default() };
        let samples = self.settings.msaa.max(1);
        let format = target.color.format();
        let size = [target.color.width(), target.color.height()];
        let extent = [target.extent[0].clamp(1, size[0]), target.extent[1].clamp(1, size[1])];
        let multiview = self.multiview_available(samples);
        let p = self.prepare(scene, eyes[0].pose.pos, &mut stats);

        // The headset's depth: drawn into directly when it is single-sampled Depth32Float like ours; otherwise
        // drawn into our own and resolved into it.
        let xr_depth = target.depth;
        let direct_depth = xr_depth.is_some_and(|d| samples == 1 && d.format() == DEPTH_FORMAT);
        let copy_depth = xr_depth.is_some() && !direct_depth;
        let want = TargetUse { layers: if multiview { 2 } else { 1 }, sampled_depth: copy_depth };

        let views: Vec<(Mat4, Vec4)> = eyes.iter().map(|c| self.view_of(&p, c)).collect();
        let encode = !format.is_srgb();
        let scene_slots: u64 = if multiview { 1 } else { 2 };
        let shadow_slot = scene_slots;
        self.ensure_slots(scene_slots + 1);
        self.write_slot(shadow_slot, &self.globals_for(&p, [p.light_vp; 2], [Vec4::ZERO; 2], false));
        if multiview {
            self.write_slot(0, &self.globals_for(&p, [views[0].0, views[1].0], [views[0].1, views[1].1], encode));
        } else {
            for (i, (vp, eye)) in views.iter().enumerate() {
                self.write_slot(i as u64, &self.globals_for(&p, [*vp; 2], [*eye; 2], encode));
            }
        }
        let base = STEREO_TARGETS;
        for i in 0..scene_slots as usize {
            self.target_for(base + i, size, format, samples, want);
        }
        self.pipelines(format, samples, multiview);
        let depth_format = xr_depth.map(|d| d.format());
        if let (true, Some(f)) = (copy_depth, depth_format) {
            self.depth_copy_pipeline(f, samples > 1);
        }

        let color_layers: Vec<wgpu::TextureView> = (0..2).map(|l| layer_view(target.color, l)).collect();
        let color_array = array_view(target.color);
        let xr_layers: Vec<wgpu::TextureView> =
            xr_depth.map(|d| (0..2).map(|l| layer_view(d, l)).collect()).unwrap_or_default();
        let xr_array = xr_depth.map(array_view);

        // Every pass is inside the timestamp pair, the depth resolves too (review L2, part 2b).
        let passes = p.shadows as usize + scene_slots as usize + if copy_depth { 2 } else { 0 };
        let mut k = 0;
        let mut enc =
            self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pe-render stereo") });
        let ts = |k: usize| timing.and_then(|slot| self.timer.as_ref()?.writes(slot, k == 0, k + 1 == passes));
        if p.shadows {
            stats.draws += self.shadow_pass(&mut enc, &p, shadow_slot, ts(k));
            k += 1;
        }
        let pl = &self.pipelines[&(format_key(format), samples, multiview)];
        // Multiview: one pass over the array; otherwise one pass per eye's layer.
        for (i, layer) in color_layers.iter().enumerate().take(scene_slots as usize) {
            let t = &self.targets[base + i];
            let (color, depth_out) =
                if multiview { (&color_array, xr_array.as_ref()) } else { (layer, xr_layers.get(i)) };
            let pass = PassTarget {
                color: t.msaa_color.as_ref().unwrap_or(color),
                resolve: t.msaa_color.as_ref().map(|_| color),
                depth: if direct_depth { depth_out.expect("direct depth has a target") } else { &t.depth },
                store_depth: direct_depth || copy_depth,
                extent,
                multiview,
            };
            stats.draws += self.scene_pass(&mut enc, &p, pl, i as u64, &pass, ts(k));
            k += 1;
            if let (true, Some(f)) = (copy_depth, depth_format) {
                // Per layer: a multiview pass's depth is resolved layer by layer, as single-layer views.
                let eyes_here: &[usize] = if multiview { &[0, 1] } else { std::slice::from_ref(&i) };
                for &e in eyes_here {
                    let src = &t.depth_layers[if multiview { e } else { 0 }];
                    let how = DepthResolve { format: f, multisampled: samples > 1, extent };
                    self.depth_copy(&mut enc, src, &xr_layers[e], how, ts(k));
                    k += 1;
                    stats.draws += 1;
                }
            }
        }
        // Hand the images back in the states OpenXR gave them (xr.md 4.2).
        let mut back = vec![wgpu::TextureTransition {
            texture: target.color,
            selector: None,
            state: wgpu::TextureUses::COLOR_TARGET,
        }];
        if let Some(d) = xr_depth {
            back.push(wgpu::TextureTransition {
                texture: d,
                selector: None,
                state: wgpu::TextureUses::DEPTH_STENCIL_WRITE,
            });
        }
        enc.transition_resources(std::iter::empty(), back.into_iter());
        self.finish(enc, timing, fresh, t0, stats)
    }

    /// Instances, lights and the key light's shadow box for one frame, with the instances uploaded.
    fn prepare(&mut self, scene: &Scene, anchor: Pos, stats: &mut FrameStats) -> Prepared {
        // Instances, grouped by mesh: opaque, then ghosts, then outlines.
        let mut opaque: BTreeMap<MeshKey, Vec<Instance>> = BTreeMap::new();
        let mut ghosts: BTreeMap<MeshKey, Vec<Instance>> = BTreeMap::new();
        let mut outlines: BTreeMap<MeshKey, Vec<Instance>> = BTreeMap::new();
        for it in &scene.items {
            let (key, shape_scale) = self.mesh_for(&it.shape, it.radius);
            let s = [it.scale[0] * shape_scale[0], it.scale[1] * shape_scale[1], it.scale[2] * shape_scale[2]];
            let a = &it.appearance;
            let inst = |pose: &WorldPose, alpha: f32| Instance {
                model: anchored_model(anchor, pose, s).to_cols_array_2d(),
                color: [a.base_color[0], a.base_color[1], a.base_color[2], alpha],
                emissive_rough: [a.emissive[0], a.emissive[1], a.emissive[2], a.roughness],
                params: [a.metalness, 0.0, 0.0, 0.0],
            };
            opaque.entry(key).or_default().push(inst(&it.pose, 1.0));
            if let Some((g, alpha)) = &it.ghost {
                ghosts.entry(key).or_default().push(inst(g, *alpha));
            }
            if let Some(c) = it.outline {
                let mut o = inst(&it.pose, c[3]);
                o.color = c;
                outlines.entry(key).or_default().push(o);
            }
        }
        let mut all: Vec<Instance> = Vec::new();
        let mut ranges: Vec<(u8, MeshKey, std::ops::Range<u32>)> = Vec::new();
        for (pass, group) in [(0u8, &opaque), (1, &ghosts), (2, &outlines)] {
            for (k, v) in group {
                let start = all.len() as u32;
                all.extend_from_slice(v);
                ranges.push((pass, *k, start..all.len() as u32));
            }
        }
        stats.instances = all.len() as u32;
        if all.len() as u64 > self.instance_cap {
            self.instance_cap = (all.len() as u64).next_power_of_two();
            self.instances = Self::instance_buffer(&self.ctx.device, self.instance_cap);
        }
        if !all.is_empty() {
            self.ctx.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&all));
        }

        // Lights: the first directional light is the key; the rest, nearest and brightest first, up to 16.
        let env = scene.environment;
        let mut key_dir = Vec3::from(env.key_dir).normalize_or(Vec3::NEG_Y);
        let mut key_color = Vec3::from(env.key_color);
        let mut rest: Vec<(f64, GpuLight)> = Vec::new();
        let mut key_taken = false;
        for l in &scene.lights {
            let dir = (l.pose.rot * DVec3::NEG_Z).normalize();
            let color = Vec3::from(l.light.color) * l.light.intensity;
            match l.light.kind {
                LightKind::Directional if !key_taken => {
                    key_taken = true;
                    key_dir = dir.as_vec3();
                    key_color = color;
                }
                kind => {
                    let rel = offset_vec(l.pose.pos - anchor);
                    let (k, outer, inner) = match kind {
                        LightKind::Point => (0.0, -1.0, -1.0),
                        LightKind::Spot { inner_angle, outer_angle } => (1.0, outer_angle.cos(), inner_angle.cos()),
                        LightKind::Directional => (2.0, -1.0, -1.0),
                    };
                    let score =
                        if k == 2.0 { f64::MAX } else { l.light.intensity as f64 / rel.length_squared().max(1.0) };
                    rest.push((
                        score,
                        GpuLight {
                            pos_range: [rel.x as f32, rel.y as f32, rel.z as f32, l.light.range],
                            color_kind: [color.x, color.y, color.z, k],
                            dir_outer: [dir.x as f32, dir.y as f32, dir.z as f32, outer],
                            inner: [inner, 0.0, 0.0, 0.0],
                        },
                    ));
                }
            }
        }
        rest.sort_by(|a, b| b.0.total_cmp(&a.0));
        rest.truncate(MAX_LIGHTS);
        stats.lights = rest.len() as u32 + 1;
        let mut lights = [GpuLight::zeroed(); MAX_LIGHTS];
        for (i, (_, l)) in rest.iter().enumerate() {
            lights[i] = *l;
        }

        // The key light's shadow: an orthographic box around the anchor.
        let e = env.shadow_extent.max(0.1);
        let light_view = glam::camera::rh::view::look_at_mat4(
            -key_dir * (2.0 * e),
            Vec3::ZERO,
            if key_dir.y.abs() > 0.99 { Vec3::X } else { Vec3::Y },
        );
        // wgpu's clip space is glam's DirectX convention: depth 0..1, +Y up.
        let light_vp = glam::camera::rh::proj::directx::orthographic(-e, e, -e, e, 0.0, 4.0 * e) * light_view;
        Prepared {
            ranges,
            anchor,
            light_vp,
            key_dir,
            key_color,
            ambient: Vec3::from(env.ambient),
            sky: env.sky,
            lights,
            light_count: rest.len() as u32,
            shadows: self.settings.shadow_size > 0,
        }
    }

    /// A camera's view-projection relative to the frame's anchor, and its eye (with the outline width in `w`).
    fn view_of(&self, p: &Prepared, cam: &Camera) -> (Mat4, Vec4) {
        let (view_m, eye) = anchored_view(p.anchor, cam);
        (cam.projection() * view_m, eye.extend(self.settings.outline_width))
    }

    fn globals_for(&self, p: &Prepared, view_proj: [Mat4; 2], eye: [Vec4; 2], encode_srgb: bool) -> Globals {
        let (k, c, a) = (p.key_dir, p.key_color, p.ambient);
        Globals {
            view_proj: view_proj.map(|m| m.to_cols_array_2d()),
            light_vp: p.light_vp.to_cols_array_2d(),
            eye: eye.map(|e| e.to_array()),
            key_dir: [k.x, k.y, k.z, if p.shadows { 1.0 } else { 0.0 }],
            key_color: [c.x, c.y, c.z, 0.0],
            ambient: [a.x, a.y, a.z, 0.0],
            // A target without an sRGB format gets the transfer applied in the shader (review L1).
            counts: [p.light_count, encode_srgb as u32, 0, 0],
            lights: p.lights,
        }
    }

    fn ensure_slots(&mut self, needed: u64) {
        if needed > self.globals_slots {
            self.globals_slots = needed.next_power_of_two();
            self.globals = Self::globals_buffer(&self.ctx.device, self.globals_slots);
            self.globals_bg = Self::globals_bind_group(&self.ctx.device, &self.globals_bgl, &self.globals);
        }
    }

    fn write_slot(&self, slot: u64, g: &Globals) {
        self.ctx.queue.write_buffer(&self.globals, slot * SLOT, bytemuck::bytes_of(g));
    }

    fn shadow_pass(
        &self,
        enc: &mut wgpu::CommandEncoder,
        p: &Prepared,
        slot: u64,
        ts: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> u32 {
        let mut draws = 0;
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("shadow"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &self.shadow_view,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                stencil_ops: None,
            }),
            timestamp_writes: ts,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(&self.shadow_pipeline);
        rp.set_bind_group(0, &self.globals_bg, &[(slot * SLOT) as u32]);
        rp.set_vertex_buffer(1, self.instances.slice(..));
        for (pass, k, r) in &p.ranges {
            if *pass == 0 {
                let m = &self.meshes[k];
                rp.set_vertex_buffer(0, m.vb.slice(..));
                rp.set_index_buffer(m.ib.slice(..), wgpu::IndexFormat::Uint32);
                rp.draw_indexed(0..m.n, 0, r.clone());
                draws += 1;
            }
        }
        draws
    }

    fn scene_pass(
        &self,
        enc: &mut wgpu::CommandEncoder,
        p: &Prepared,
        pl: &Pipelines,
        slot: u64,
        t: &PassTarget<'_>,
        ts: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) -> u32 {
        let mut draws = 0;
        let sky = p.sky;
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: t.color,
                depth_slice: None,
                resolve_target: t.resolve,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: sky[0] as f64,
                        g: sky[1] as f64,
                        b: sky[2] as f64,
                        a: 1.0,
                    }),
                    store: if t.resolve.is_some() { wgpu::StoreOp::Discard } else { wgpu::StoreOp::Store },
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: t.depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.0),
                    store: if t.store_depth { wgpu::StoreOp::Store } else { wgpu::StoreOp::Discard },
                }),
                stencil_ops: None,
            }),
            timestamp_writes: ts,
            occlusion_query_set: None,
            multiview_mask: t.multiview.then(|| std::num::NonZeroU32::new(0b11).expect("non-zero")),
        });
        rp.set_viewport(0.0, 0.0, t.extent[0] as f32, t.extent[1] as f32, 0.0, 1.0);
        rp.set_scissor_rect(0, 0, t.extent[0], t.extent[1]);
        rp.set_bind_group(0, &self.globals_bg, &[(slot * SLOT) as u32]);
        rp.set_bind_group(1, &self.shadow_bg, &[]);
        rp.set_vertex_buffer(1, self.instances.slice(..));
        for (pass, k, r) in &p.ranges {
            rp.set_pipeline(match pass {
                0 => &pl.main,
                1 => &pl.ghost,
                _ => &pl.outline,
            });
            let m = &self.meshes[k];
            rp.set_vertex_buffer(0, m.vb.slice(..));
            rp.set_index_buffer(m.ib.slice(..), wgpu::IndexFormat::Uint32);
            rp.draw_indexed(0..m.n, 0, r.clone());
            draws += 1;
        }
        draws
    }

    fn depth_copy_pipeline(&mut self, format: wgpu::TextureFormat, multisampled: bool) {
        let key = (format_key(format), multisampled);
        if self.depth_copy.contains_key(&key) {
            return;
        }
        let _compiling = crate::gpu::creation_lock();
        let device = &self.ctx.device;
        let module = self.depth_copy_module.get_or_insert_with(|| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("pe-render depth resolve"),
                source: wgpu::ShaderSource::Wgsl(include_str!("depth_copy.wgsl").into()),
            })
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("depth resolve"),
            layout: None,
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs_full"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some(if multisampled { "fs_copy_ms" } else { "fs_copy_1" }),
                compilation_options: Default::default(),
                targets: &[],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.depth_copy.insert(key, pipeline);
    }

    /// Resolve one layer of our depth into one layer of the headset's.
    fn depth_copy(
        &self,
        enc: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        dst: &wgpu::TextureView,
        how: DepthResolve,
        ts: Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        let DepthResolve { format, multisampled, extent } = how;
        let pipeline = &self.depth_copy[&(format_key(format), multisampled)];
        let bg = self.ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("depth resolve"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: if multisampled { 0 } else { 1 },
                resource: wgpu::BindingResource::TextureView(src),
            }],
        });
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("depth resolve"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: dst,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0.0), store: wgpu::StoreOp::Store }),
                stencil_ops: None,
            }),
            timestamp_writes: ts,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_viewport(0.0, 0.0, extent[0] as f32, extent[1] as f32, 0.0, 1.0);
        rp.set_scissor_rect(0, 0, extent[0], extent[1]);
        rp.set_pipeline(pipeline);
        rp.set_bind_group(0, &bg, &[]);
        rp.draw(0..3, 0..1);
    }

    fn finish(
        &mut self,
        mut enc: wgpu::CommandEncoder,
        timing: Option<usize>,
        fresh: Option<f64>,
        t0: std::time::Instant,
        mut stats: FrameStats,
    ) -> FrameStats {
        if let (Some(slot), Some(t)) = (timing, self.timer.as_ref()) {
            t.resolve(&mut enc, slot);
        }
        self.ctx.queue.submit([enc.finish()]);
        if let (Some(slot), Some(t)) = (timing, self.timer.as_mut()) {
            t.submitted(slot);
        }
        stats.gpu_ms = fresh;
        stats.cpu_record_ms = t0.elapsed().as_secs_f64() * 1e3;
        stats
    }
}

/// The targets [`Renderer::render_stereo`] keeps start at this index (below it: the desktop's and captures').
const STEREO_TARGETS: usize = 4;

/// What a set of targets must allow.
#[derive(Clone, Copy)]
struct TargetUse {
    layers: u32,
    /// Our depth is read by the headset's depth resolve.
    sampled_depth: bool,
}

/// How the headset's depth is resolved: its format, whether ours is multisampled, and the drawn extent.
#[derive(Clone, Copy)]
struct DepthResolve {
    format: wgpu::TextureFormat,
    multisampled: bool,
    extent: [u32; 2],
}

/// One scene pass's attachments.
struct PassTarget<'a> {
    color: &'a wgpu::TextureView,
    resolve: Option<&'a wgpu::TextureView>,
    depth: &'a wgpu::TextureView,
    store_depth: bool,
    extent: [u32; 2],
    multiview: bool,
}

/// One frame's shared inputs: instance ranges, lights and the shadow box.
struct Prepared {
    ranges: Vec<(u8, MeshKey, std::ops::Range<u32>)>,
    anchor: Pos,
    light_vp: Mat4,
    key_dir: Vec3,
    key_color: Vec3,
    ambient: Vec3,
    sky: [f32; 3],
    lights: [GpuLight; MAX_LIGHTS],
    light_count: u32,
    shadows: bool,
}

/// A headset's swapchain images for one frame (xr.md 4.2): two array layers each, left eye first.
pub struct StereoTarget<'a> {
    /// sRGB colour, wrapped in COLOR_TARGET.
    pub color: &'a wgpu::Texture,
    /// The depth swapchain's image, when the runtime takes depth; wrapped in DEPTH_STENCIL_WRITE.
    pub depth: Option<&'a wgpu::Texture>,
    /// The drawn rectangle, from the origin: the render scale (render.md 10).
    pub extent: [u32; 2],
}

/// One array layer of a texture, as a 2D view.
fn layer_view(t: &wgpu::Texture, layer: u32) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2),
        base_array_layer: layer,
        array_layer_count: Some(1),
        ..Default::default()
    })
}

/// A two-layer texture as one array view (multiview).
fn array_view(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        base_array_layer: 0,
        array_layer_count: Some(2),
        ..Default::default()
    })
}

/// An object's model matrix relative to the frame's anchor, along world axes: the f64 difference, then f32.
fn anchored_model(anchor: Pos, pose: &WorldPose, s: [f32; 3]) -> Mat4 {
    let r = DMat3::from_quat(pose.rot.normalize());
    let t = offset_vec(pose.pos - anchor);
    let c =
        |v: DVec3, k: f32| Vec4::new((v.x * k as f64) as f32, (v.y * k as f64) as f32, (v.z * k as f64) as f32, 0.0);
    Mat4::from_cols(
        c(r.x_axis, s[0]),
        c(r.y_axis, s[1]),
        c(r.z_axis, s[2]),
        Vec4::new(t.x as f32, t.y as f32, t.z as f32, 1.0),
    )
}

/// A view's matrix from anchor space, and its eye in anchor space: the eye's rotation and its small offset from
/// the anchor, in f64, then f32.
fn anchored_view(anchor: Pos, cam: &Camera) -> (Mat4, Vec3) {
    let eye = offset_vec(cam.pose.pos - anchor);
    let inv: DQuat = cam.pose.rot.inverse();
    let r = DMat3::from_quat(inv);
    let t = -(inv * eye);
    let c = |v: DVec3| Vec4::new(v.x as f32, v.y as f32, v.z as f32, 0.0);
    (
        Mat4::from_cols(c(r.x_axis), c(r.y_axis), c(r.z_axis), Vec4::new(t.x as f32, t.y as f32, t.z as f32, 1.0)),
        eye.as_vec3(),
    )
}

fn vertex_layouts() -> [Option<wgpu::VertexBufferLayout<'static>>; 2] {
    const V: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
    const I: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
        2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4, 7 => Float32x4, 8 => Float32x4
    ];
    [
        Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &V,
        }),
        Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &I,
        }),
    ]
}

fn upload(device: &wgpu::Device, (v, i): (Vec<Vertex>, Vec<u32>)) -> Mesh {
    use wgpu::util::DeviceExt;
    Mesh {
        vb: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mesh vertices"),
            contents: bytemuck::cast_slice(&v),
            usage: wgpu::BufferUsages::VERTEX,
        }),
        ib: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mesh indices"),
            contents: bytemuck::cast_slice(&i),
            usage: wgpu::BufferUsages::INDEX,
        }),
        n: i.len() as u32,
    }
}

/// A unit sphere, counter-clockwise outward.
fn sphere_mesh(seg: u32, rings: u32) -> (Vec<Vertex>, Vec<u32>) {
    capsule_mesh(0.0, seg, rings / 2)
}

/// A unit-radius capsule along Y with half-height `h` (0 is a sphere): two hemispheres of `half_rings` rings each,
/// joined by a cylinder.
fn capsule_mesh(h: f32, seg: u32, half_rings: u32) -> (Vec<Vertex>, Vec<u32>) {
    let (mut v, mut idx) = (Vec::new(), Vec::new());
    let half_rings = half_rings.max(1);
    // Rings from the top pole down: the top hemisphere offset by +h, then the bottom by −h.
    let mut rows: Vec<(f32, f32)> = Vec::new(); // (polar angle, y offset)
    for r in 0..=half_rings {
        rows.push((std::f32::consts::FRAC_PI_2 * r as f32 / half_rings as f32, h));
    }
    for r in 0..=half_rings {
        rows.push((std::f32::consts::FRAC_PI_2 * (1.0 + r as f32 / half_rings as f32), -h));
    }
    for (th, dy) in &rows {
        for s in 0..=seg {
            let ph = std::f32::consts::TAU * s as f32 / seg as f32;
            let n = [th.sin() * ph.cos(), th.cos(), th.sin() * ph.sin()];
            v.push(Vertex { pos: [n[0], n[1] + dy, n[2]], normal: n });
        }
    }
    for r in 0..rows.len() as u32 - 1 {
        for s in 0..seg {
            let a = r * (seg + 1) + s;
            let b = a + seg + 1;
            idx.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    (v, idx)
}

/// A cube of half-extent 1, counter-clockwise outward.
fn box_mesh() -> (Vec<Vertex>, Vec<u32>) {
    let (mut v, mut idx) = (Vec::new(), Vec::new());
    for n in [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z] {
        let a = if n.x.abs() > 0.5 { Vec3::Y } else { Vec3::X };
        let b = n.cross(a);
        let a = b.cross(n);
        let base = v.len() as u32;
        for (sa, sb) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            v.push(Vertex { pos: (n + a * sa + b * sb).into(), normal: n.into() });
        }
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (v, idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn winding_outward((v, idx): (Vec<Vertex>, Vec<u32>)) {
        let mut checked = 0;
        for t in idx.chunks(3) {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| Vec3::from(v[i as usize].pos));
            let n = (b - a).cross(c - a);
            if n.length() < 1e-6 {
                continue; // a pole's degenerate triangle
            }
            let centre = (a + b + c) / 3.0;
            let out = Vec3::from(v[t[0] as usize].normal) + Vec3::from(v[t[1] as usize].normal);
            assert!(n.dot(out) > 0.0, "inward triangle at {centre:?}: face {n:?}, vertex normals {out:?}");
            checked += 1;
        }
        assert!(checked > 10);
    }

    /// Both shader variants parse, validate and compile to SPIR-V on every run, multiview included, though no
    /// software adapter here draws it (review L1, part 2b). The multiview variant really uses `view_index`: it fails
    /// validation without the capability.
    #[test]
    fn both_shader_variants_validate_and_compile_to_spirv() {
        use wgpu::naga;
        let compile = |src: &str, caps: naga::valid::Capabilities| -> Result<Vec<u32>, String> {
            let m = naga::front::wgsl::parse_str(src).map_err(|e| e.emit_to_string(src))?;
            let info = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), caps)
                .validate(&m)
                .map_err(|e| format!("{e:?}"))?;
            naga::back::spv::write_vec(&m, &info, &naga::back::spv::Options::default(), None).map_err(|e| e.to_string())
        };
        let base = naga::valid::Capabilities::default();
        assert!(!compile(&shader_source(false), base).expect("single-view").is_empty());
        let mv = shader_source(true);
        assert!(!compile(&mv, base | naga::valid::Capabilities::MULTIVIEW).expect("multiview").is_empty());
        assert!(compile(&mv, base).is_err(), "the multiview variant does not use view_index");
        assert!(!compile(include_str!("depth_copy.wgsl"), base).expect("depth resolve").is_empty());
    }

    #[test]
    fn meshes_wind_outward() {
        winding_outward(box_mesh());
        winding_outward(sphere_mesh(16, 8));
        winding_outward(capsule_mesh(1.5, 16, 4));
    }

    #[test]
    fn anchored_matrices_are_small_differences() {
        let far = Pos::ORIGIN.checked_add(pe_seams::Offset([1e15, 2e14, -5e14])).unwrap();
        let at = |b: Pos, d: [f64; 3]| b.checked_add(pe_seams::Offset(d)).unwrap();
        let pose = |b| WorldPose { pos: at(b, [0.5, 1.0, -3.0]), rot: DQuat::from_rotation_x(0.3) };
        assert_eq!(
            anchored_model(at(Pos::ORIGIN, [0.0, 1.6, 0.0]), &pose(Pos::ORIGIN), [1.0, 2.0, 3.0]).to_cols_array(),
            anchored_model(at(far, [0.0, 1.6, 0.0]), &pose(far), [1.0, 2.0, 3.0]).to_cols_array()
        );
    }
}
