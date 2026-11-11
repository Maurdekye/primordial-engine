// The headset's depth layer (render.md 5, review L2). The depth swapchain is single-sampled, and wgpu resolves no
// depth, so a fullscreen triangle writes each pixel's depth from ours: with MSAA the nearest of the samples (reverse
// Z: the greatest), so a foreground edge never takes the background's depth in the compositor's reprojection.

@group(0) @binding(0) var src_ms: texture_depth_multisampled_2d;
@group(0) @binding(1) var src_1: texture_depth_2d;

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs_copy_ms(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    let c = vec2<i32>(p.xy);
    var d = 0.0;
    for (var s = 0; s < i32(textureNumSamples(src_ms)); s++) {
        d = max(d, textureLoad(src_ms, c, s));
    }
    return d;
}

@fragment
fn fs_copy_1(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 {
    return textureLoad(src_1, vec2<i32>(p.xy), 0);
}
