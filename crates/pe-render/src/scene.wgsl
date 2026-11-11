// pe-render's scene shader (render.md 6): forward-lit instanced shapes, a key light with a 3x3 PCF shadow, up to 16
// point, spot and extra directional lights, a filmic tone map into an sRGB target, and the outline and ghost passes
// of the correction cue (D3, R6).
//
// Every position here is relative to the frame's anchor (the first view's camera), along world axes: the CPU took
// `object Pos - anchor Pos` in f64 before casting (render.md 5). No absolute coordinate reaches this shader.
//
// Two variants are compiled from this one source (scene.rs, `shader_source`): single-view, where `VIEW_INDEX` is 0
// and each view has its own uniform slot, and multiview, where `/*VIEW*/` becomes the `view_index` builtin and one
// slot holds both eyes.

struct GpuLight {
    pos_range: vec4<f32>,   // anchor-relative position, range (0: unbounded)
    color_kind: vec4<f32>,  // rgb x intensity, kind (0 point, 1 spot, 2 directional)
    dir_outer: vec4<f32>,   // the direction light travels, cos(outer angle)
    inner: vec4<f32>,       // cos(inner angle)
};

struct Globals {
    view_proj: array<mat4x4<f32>, 2>,   // per view (multiview: both eyes; single-view: [0])
    light_vp: mat4x4<f32>,
    eye: array<vec4<f32>, 2>,           // per view: eye position, outline width per metre of distance
    key_dir: vec4<f32>,     // the direction the key light travels, shadow on (1) or off (0)
    key_color: vec4<f32>,
    ambient: vec4<f32>,
    counts: vec4<u32>,      // number of lights, encode sRGB in the shader (1: the target is not an sRGB format)
    lights: array<GpuLight, 16>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var shadow_map: texture_depth_2d;
@group(1) @binding(1) var shadow_cmp: sampler_comparison;

struct VIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) color: vec4<f32>,
    @location(7) emissive_rough: vec4<f32>,
    @location(8) params: vec4<f32>, // metalness, -, -, -
};

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
    @location(3) emissive_rough: vec4<f32>,
    @location(4) metal: f32,
    @location(5) eye: vec3<f32>,
};

fn model_of(v: VIn) -> mat4x4<f32> {
    return mat4x4<f32>(v.m0, v.m1, v.m2, v.m3);
}

// M = R S, so the normal transform R S^-1 is M S^-2.
fn normal_of(v: VIn) -> vec3<f32> {
    let s = vec3<f32>(length(v.m0.xyz), length(v.m1.xyz), length(v.m2.xyz));
    return normalize((model_of(v) * vec4<f32>(v.normal / (s * s), 0.0)).xyz);
}

@vertex
fn vs_main(v: VIn/*VIEW*/) -> VOut {
    let vi = VIEW_INDEX;
    let w = model_of(v) * vec4<f32>(v.pos, 1.0);
    var o: VOut;
    o.clip = g.view_proj[vi] * w;
    o.eye = g.eye[vi].xyz;
    o.world = w.xyz;
    o.normal = normal_of(v);
    o.color = v.color;
    o.emissive_rough = v.emissive_rough;
    o.metal = v.params.x;
    return o;
}

@vertex
fn vs_shadow(v: VIn) -> @builtin(position) vec4<f32> {
    return g.light_vp * (model_of(v) * vec4<f32>(v.pos, 1.0));
}

// The outline: back faces of a shell pushed out along the normal, by a width that grows with distance so it stays
// about the same on screen.
@vertex
fn vs_outline(v: VIn/*VIEW*/) -> VOut {
    let vi = VIEW_INDEX;
    let w0 = model_of(v) * vec4<f32>(v.pos, 1.0);
    let n = normal_of(v);
    let width = g.eye[vi].w * distance(w0.xyz, g.eye[vi].xyz);
    let w = vec4<f32>(w0.xyz + n * width, 1.0);
    var o: VOut;
    o.clip = g.view_proj[vi] * w;
    o.eye = g.eye[vi].xyz;
    o.world = w.xyz;
    o.normal = n;
    o.color = v.color;
    o.emissive_rough = v.emissive_rough;
    o.metal = 0.0;
    return o;
}

const PI: f32 = 3.14159265;

fn shadow_at(world: vec3<f32>, n: vec3<f32>) -> f32 {
    if g.key_dir.w < 0.5 {
        return 1.0;
    }
    // A small normal offset against acne.
    let p = g.light_vp * vec4<f32>(world + n * 0.01, 1.0);
    let c = p.xyz / p.w;
    let uv = vec2<f32>(c.x * 0.5 + 0.5, 0.5 - c.y * 0.5);
    if uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0 || c.z > 1.0 {
        return 1.0;
    }
    let texel = 1.0 / vec2<f32>(textureDimensions(shadow_map));
    var lit = 0.0;
    for (var y = -1; y <= 1; y++) {
        for (var x = -1; x <= 1; x++) {
            lit += textureSampleCompareLevel(shadow_map, shadow_cmp, uv + vec2<f32>(f32(x), f32(y)) * texel, c.z - 0.0015);
        }
    }
    return lit / 9.0;
}

// Lambert plus GGX specular, Schlick Fresnel, Smith-Schlick visibility.
fn brdf(n: vec3<f32>, v: vec3<f32>, l: vec3<f32>, base: vec3<f32>, rough: f32, metal: f32) -> vec3<f32> {
    let h = normalize(v + l);
    let nl = max(dot(n, l), 0.0);
    let nv = max(dot(n, v), 1e-4);
    let nh = max(dot(n, h), 0.0);
    let vh = max(dot(v, h), 0.0);
    let a = max(rough * rough, 0.002);
    let a2 = a * a;
    let dd = nh * nh * (a2 - 1.0) + 1.0;
    let d = a2 / (PI * dd * dd);
    let k = (rough + 1.0) * (rough + 1.0) / 8.0;
    let vis = 1.0 / ((nl * (1.0 - k) + k) * (nv * (1.0 - k) + k) * 4.0);
    let f0 = mix(vec3<f32>(0.04), base, metal);
    let f = f0 + (vec3<f32>(1.0) - f0) * pow(1.0 - vh, 5.0);
    let diffuse = (vec3<f32>(1.0) - f) * (1.0 - metal) * base / PI;
    return (diffuse + f * d * vis) * nl;
}

fn window(d: f32, range: f32) -> f32 {
    if range <= 0.0 {
        return 1.0;
    }
    let r = clamp(1.0 - pow(d / range, 4.0), 0.0, 1.0);
    return r * r;
}

// Narkowicz's ACES fit.
fn tone_map(x: vec3<f32>) -> vec3<f32> {
    return clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn shade(i: VOut) -> vec3<f32> {
    let n = normalize(i.normal);
    let v = normalize(i.eye - i.world);
    let base = i.color.rgb;
    let rough = clamp(i.emissive_rough.w, 0.04, 1.0);
    var c = g.ambient.rgb * base * (1.0 - 0.5 * i.metal);
    c += brdf(n, v, -normalize(g.key_dir.xyz), base, rough, i.metal) * g.key_color.rgb * shadow_at(i.world, n);
    for (var k = 0u; k < g.counts.x; k++) {
        let L = g.lights[k];
        var l: vec3<f32>;
        var att = 1.0;
        if L.color_kind.w > 1.5 {
            l = -normalize(L.dir_outer.xyz);
        } else {
            let d = L.pos_range.xyz - i.world;
            let dist = length(d);
            l = d / max(dist, 1e-4);
            att = window(dist, L.pos_range.w) / max(dist * dist, 1e-4);
            if L.color_kind.w > 0.5 {
                let cd = dot(-l, normalize(L.dir_outer.xyz));
                att *= smoothstep(L.dir_outer.w, L.inner.x, cd);
            }
        }
        c += brdf(n, v, l, base, rough, i.metal) * L.color_kind.rgb * att;
    }
    return c + i.emissive_rough.rgb;
}

// Linear to sRGB, for a target whose format doesn't encode it (an sRGB format's hardware does).
fn encode(c: vec3<f32>) -> vec3<f32> {
    if g.counts.y == 0u {
        return c;
    }
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main(i: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(encode(tone_map(shade(i))), 1.0);
}

// The ghost of the correction cue: the same shading, faint.
@fragment
fn fs_ghost(i: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(encode(tone_map(shade(i))), i.color.a);
}

@fragment
fn fs_outline(i: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(encode(i.color.rgb), i.color.a);
}
