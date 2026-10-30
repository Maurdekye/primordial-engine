//! Headless wgpu renderer: instanced lit spheres with a shadow map, translucent pit walls, the
//! hands as spheres on an arm, and a translucent ghost at the engager's controller position.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};

pub const COLOR_FMT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const DEPTH_FMT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SHADOW_SIZE: u32 = 2048;
const MSAA: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Instance {
    pub model: [[f32; 4]; 4],
    pub color: [f32; 4],
}

impl Instance {
    pub fn sphere(c: [f32; 3], r: f32, color: [f32; 4]) -> Self {
        let m = Mat4::from_scale_rotation_translation(Vec3::splat(r), glam::Quat::IDENTITY, Vec3::from(c));
        Instance { model: m.to_cols_array_2d(), color }
    }
    /// Axis-aligned box from its centre and half extents (the box mesh spans -1..1).
    pub fn cuboid(c: [f32; 3], half: [f32; 3], color: [f32; 4]) -> Self {
        let m = Mat4::from_scale_rotation_translation(Vec3::from(half), glam::Quat::IDENTITY, Vec3::from(c));
        Instance { model: m.to_cols_array_2d(), color }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    light_vp: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    light_dir: [f32; 4],
}

/// One frame to draw: opaque spheres and boxes, then translucent spheres and boxes.
#[derive(Default)]
pub struct Frame {
    pub spheres: Vec<Instance>,
    pub boxes: Vec<Instance>,
    pub glass_spheres: Vec<Instance>,
    pub glass_boxes: Vec<Instance>,
}

struct Mesh {
    vb: wgpu::Buffer,
    ib: wgpu::Buffer,
    n: u32,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pub adapter_info: wgpu::AdapterInfo,
    sphere: Mesh,
    cube: Mesh,
    opaque: wgpu::RenderPipeline,
    glass: wgpu::RenderPipeline,
    shadow: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    main_bg: wgpu::BindGroup,
    shadow_bg: wgpu::BindGroup,
    shadow_view: wgpu::TextureView,
    inst: wgpu::Buffer,
    inst_cap: u64,
    targets: Vec<Targets>,
}

struct Targets {
    w: u32,
    h: u32,
    msaa: wgpu::TextureView,
    resolve: wgpu::Texture,
    resolve_view: wgpu::TextureView,
    depth: wgpu::TextureView,
    readback: wgpu::Buffer,
}

const SHADER: &str = r#"
struct Globals { view_proj: mat4x4<f32>, light_vp: mat4x4<f32>, cam_pos: vec4<f32>, light_dir: vec4<f32> };
@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var shadow_tex: texture_depth_2d;
@group(0) @binding(2) var shadow_smp: sampler_comparison;

struct VIn {
    @location(0) pos: vec3<f32>, @location(1) normal: vec3<f32>,
    @location(2) m0: vec4<f32>, @location(3) m1: vec4<f32>, @location(4) m2: vec4<f32>, @location(5) m3: vec4<f32>,
    @location(6) color: vec4<f32>,
};
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>, @location(1) normal: vec3<f32>, @location(2) color: vec4<f32>,
};

@vertex fn vs(v: VIn) -> VOut {
    let m = mat4x4<f32>(v.m0, v.m1, v.m2, v.m3);
    let w = m * vec4<f32>(v.pos, 1.0);
    var o: VOut;
    o.clip = g.view_proj * w;
    o.world = w.xyz;
    // Scale is axis-aligned, so the inverse-scale normal transform is a per-axis divide.
    let s = vec3<f32>(length(v.m0.xyz), length(v.m1.xyz), length(v.m2.xyz));
    o.normal = normalize(v.normal / s);
    o.color = v.color;
    return o;
}

@vertex fn vs_shadow(v: VIn) -> @builtin(position) vec4<f32> {
    let m = mat4x4<f32>(v.m0, v.m1, v.m2, v.m3);
    return g.light_vp * m * vec4<f32>(v.pos, 1.0);
}

fn lit(i: VOut) -> vec3<f32> {
    let n = normalize(i.normal);
    let l = -normalize(g.light_dir.xyz);
    let lc = g.light_vp * vec4<f32>(i.world, 1.0);
    let uv = vec2<f32>(lc.x * 0.5 + 0.5, 0.5 - lc.y * 0.5);
    var sh = 0.0;
    let texel = 1.0 / 2048.0;
    for (var dx = -1; dx <= 1; dx++) {
        for (var dy = -1; dy <= 1; dy++) {
            sh += textureSampleCompareLevel(shadow_tex, shadow_smp, uv + vec2<f32>(f32(dx), f32(dy)) * texel, lc.z - 0.0015);
        }
    }
    sh = sh / 9.0;
    let diff = max(dot(n, l), 0.0) * sh;
    let vdir = normalize(g.cam_pos.xyz - i.world);
    let h = normalize(l + vdir);
    let spec = pow(max(dot(n, h), 0.0), 48.0) * 0.35 * sh;
    let sky = 0.30 + 0.12 * n.y;
    return i.color.rgb * (sky + 0.85 * diff) + vec3<f32>(spec);
}

@fragment fn fs(i: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(lit(i), 1.0);
}

@fragment fn fs_glass(i: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(lit(i), i.color.a);
}
"#;

fn uv_sphere(seg: u32, rings: u32) -> (Vec<Vertex>, Vec<u32>) {
    let mut v = Vec::new();
    let mut idx = Vec::new();
    for r in 0..=rings {
        let th = std::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=seg {
            let ph = std::f32::consts::TAU * s as f32 / seg as f32;
            let p = [th.sin() * ph.cos(), th.cos(), th.sin() * ph.sin()];
            v.push(Vertex { pos: p, normal: p });
        }
    }
    for r in 0..rings {
        for s in 0..seg {
            let a = r * (seg + 1) + s;
            let b = a + seg + 1;
            idx.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    (v, idx)
}

fn cube() -> (Vec<Vertex>, Vec<u32>) {
    let mut v = Vec::new();
    let mut idx = Vec::new();
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    for (n, a, b) in faces {
        let base = v.len() as u32;
        for (sa, sb) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [n[0] + a[0] * sa + b[0] * sb, n[1] + a[1] * sa + b[1] * sb, n[2] + a[2] * sa + b[2] * sb];
            v.push(Vertex { pos: p, normal: n });
        }
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (v, idx)
}

impl Renderer {
    pub fn new(force_software: bool) -> Self {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: force_software,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .expect("no wgpu adapter");
        let adapter_info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device");
        use wgpu::util::DeviceExt;
        let mk_mesh = |(v, i): (Vec<Vertex>, Vec<u32>)| Mesh {
            vb: device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&v), usage: wgpu::BufferUsages::VERTEX }),
            ib: device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&i), usage: wgpu::BufferUsages::INDEX }),
            n: i.len() as u32,
        };
        let sphere = mk_mesh(uv_sphere(32, 16));
        let cube = mk_mesh(cube());
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: None, source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let vlayouts = [
            wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
            },
            wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Instance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4],
            },
        ];
        let vbufs = [Some(vlayouts[0].clone()), Some(vlayouts[1].clone())];
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Depth, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
            ],
        });
        let sbgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&bgl)], immediate_size: 0 });
        let slayout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[Some(&sbgl)], immediate_size: 0 });
        let mk = |fs: &str, blend: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
                layout: Some(&layout),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &vbufs },
                primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FMT,
                    depth_write_enabled: Some(!blend),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState { count: MSAA, ..Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: COLOR_FMT,
                        blend: if blend { Some(wgpu::BlendState::ALPHA_BLENDING) } else { None },
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let opaque = mk("fs", false);
        let glass = mk("fs_glass", true);
        let shadow = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&slayout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_shadow"), compilation_options: Default::default(), buffers: &vbufs },
            primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FMT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: wgpu::DepthBiasState { constant: 2, slope_scale: 2.0, clamp: 0.0 },
            }),
            multisample: Default::default(),
            fragment: None,
            multiview_mask: None,
            cache: None,
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shadow_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: SHADOW_SIZE, height: SHADOW_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FMT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let shadow_view = shadow_tex.create_view(&Default::default());
        let smp = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let main_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&shadow_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&smp) },
            ],
        });
        let shadow_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &sbgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let inst_cap = 4096u64;
        let inst = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: inst_cap * std::mem::size_of::<Instance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Renderer { device, queue, adapter_info, sphere, cube, opaque, glass, shadow, globals, main_bg, shadow_bg, shadow_view, inst, inst_cap, targets: Vec::new() }
    }

    fn targets(&mut self, w: u32, h: u32) -> usize {
        if let Some(i) = self.targets.iter().position(|t| t.w == w && t.h == h) {
            return i;
        }
        let d = &self.device;
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let tex = |samples: u32, format: wgpu::TextureFormat, usage: wgpu::TextureUsages| {
            d.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let msaa = tex(MSAA, COLOR_FMT, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default());
        let resolve = tex(1, COLOR_FMT, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let resolve_view = resolve.create_view(&Default::default());
        let depth = tex(MSAA, DEPTH_FMT, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default());
        assert!((w * 4) % 256 == 0, "row pitch must be a multiple of 256 bytes");
        let readback = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (w * h * 4) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.targets.push(Targets { w, h, msaa, resolve, resolve_view, depth, readback });
        self.targets.len() - 1
    }

    #[allow(deprecated)]
    /// Render one frame from the fixed pit camera and return tightly packed RGBA8 (sRGB) pixels.
    pub fn render(&mut self, f: &Frame, w: u32, h: u32) -> Vec<u8> {
        let ti = self.targets(w, h);
        let eye = Vec3::new(0.0, 4.1, 2.25);
        let view = Mat4::look_at_rh(eye, Vec3::new(0.0, 0.0, 0.12), Vec3::Y);
        let proj = Mat4::perspective_rh(38f32.to_radians(), w as f32 / h as f32, 0.1, 30.0);
        let light_dir = Vec3::new(-0.35, -1.0, -0.45).normalize();
        let lview = Mat4::look_at_rh(-light_dir * 6.0, Vec3::ZERO, Vec3::Y);
        let lproj = Mat4::orthographic_rh(-2.8, 2.8, -2.8, 2.8, 0.5, 14.0);
        let gl = Globals {
            view_proj: (proj * view).to_cols_array_2d(),
            light_vp: (lproj * lview).to_cols_array_2d(),
            cam_pos: [eye.x, eye.y, eye.z, 1.0],
            light_dir: [light_dir.x, light_dir.y, light_dir.z, 0.0],
        };
        self.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&gl));
        // Glass is drawn back to front.
        let mut glass_boxes = f.glass_boxes.clone();
        let dist = |i: &Instance| -> f32 { (Vec3::new(i.model[3][0], i.model[3][1], i.model[3][2]) - eye).length() };
        glass_boxes.sort_by(|a, b| dist(b).total_cmp(&dist(a)));
        let groups: [&[Instance]; 4] = [&f.spheres, &f.boxes, &f.glass_spheres, &glass_boxes];
        let total: usize = groups.iter().map(|g| g.len()).sum();
        assert!((total as u64) <= self.inst_cap, "instance buffer too small");
        let mut all: Vec<Instance> = Vec::with_capacity(total);
        let mut ranges = Vec::new();
        for g in groups {
            let s = all.len() as u32;
            all.extend_from_slice(g);
            ranges.push(s..all.len() as u32);
        }
        self.queue.write_buffer(&self.inst, 0, bytemuck::cast_slice(&all));
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut p = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.shadow_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            p.set_pipeline(&self.shadow);
            p.set_bind_group(0, &self.shadow_bg, &[]);
            p.set_vertex_buffer(1, self.inst.slice(..));
            // Opaque spheres and boxes cast shadows (the floor and walls are in `boxes`/glass).
            p.set_vertex_buffer(0, self.sphere.vb.slice(..));
            p.set_index_buffer(self.sphere.ib.slice(..), wgpu::IndexFormat::Uint32);
            p.draw_indexed(0..self.sphere.n, 0, ranges[0].clone());
        }
        let t = &self.targets[ti];
        {
            let mut p = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &t.msaa,
                    depth_slice: None,
                    resolve_target: Some(&t.resolve_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.035, g: 0.04, b: 0.055, a: 1.0 }),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &t.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            p.set_bind_group(0, &self.main_bg, &[]);
            p.set_vertex_buffer(1, self.inst.slice(..));
            for (gi, pipe) in [(0usize, &self.opaque), (1, &self.opaque), (2, &self.glass), (3, &self.glass)] {
                let mesh = if gi % 2 == 0 { &self.sphere } else { &self.cube };
                p.set_pipeline(pipe);
                p.set_vertex_buffer(0, mesh.vb.slice(..));
                p.set_index_buffer(mesh.ib.slice(..), wgpu::IndexFormat::Uint32);
                p.draw_indexed(0..mesh.n, 0, ranges[gi].clone());
            }
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &t.resolve, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &t.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = t.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
        let out = slice.get_mapped_range().expect("mapped").to_vec();
        t.readback.unmap();
        out
    }
}
