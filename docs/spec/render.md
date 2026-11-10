# pe-render: the window, the scene renderer, the display layer, the desktop hand and captures

WS-render, @ballpit-viz. Plan: docs/refound-plan.md, 3.1 (WS-render) and decision C4 ("write a small wgpu renderer for
M1, with a short design document first; evaluate adopting one before M5"). Docket:
@item:primordial-engine/ws-render-m1-pe-render-window-scene-renderer-fro.

Marks: **[V]** measured or read in the named source; **[I]** inferred or proposed; **[R]** a ruling, with its source.
Numbers marked [I] are starting values, tuned at the M1 headset test.

## 0. The page in short

- **pe-render draws what the machine holds, never more.**
  - It reads the frame view that pe-replica publishes (S5), and never simulation state.
  - Between frame views, its display layer decides where each object is drawn: two poses per object, blended
    corrections, and the local player's own hands and held objects never corrected (divergence.md 8, D1-D8).
- **One renderer, three outputs.** The desktop window, the headset's eyes (through pe-xr) and the offscreen capture
  (WorldApi's `view_capture`) all use one call over a list of views. Only the capture skips the display layer: it draws
  simulated poses.
- **wgpu 30**, with the M0 check's findings applied [V m0/feasibility-xr f8e9c776]:
  - default features off, so no GLES;
  - the DX12 shader compiler pinned;
  - a GPU context accepted from outside;
  - Vulkan first, on Windows too, with multiview; D3D12 with FXC and two passes is the fallback [R coordinator, on review
    M1].
- **Pacing follows the display.** pe-render paces to whatever period the monitor or the runtime reports, and budgets
  per period [R m981c546957f2: "work with whatever the runtime gives actually"].
- **Camera-relative everything.** A position reaches the GPU only as `object Pos − camera Pos`, an f64 difference cast
  to f32 near the camera (world-model.md 4.2).
- **The drawn time (X8) is a setting.** Extrapolate to the display time (default), or interpolate a little late. The
  headset test judges them.

## 1. Scope

**In M1** (plan M1, demo steps 1-5 and 7; acceptance tests "Rendering", "The display layer", "The agent"):
- the desktop window, and B's desktop-only view;
- the scene: the world's primitives and meshes with their `Appearance` and `Light`, at 500 balls in VR within budget;
- the display layer, D1-D8;
- the desktop hand (a mouse ray), producing the same `HandInput` as pe-xr's tracked hands;
- the offscreen capture for `view_capture`;
- the GPU context and view interface pe-xr uses (xr.md 8.2), and drawing the own hands late-latched (xr.md 5.6);
- the counters of divergence.md 7 that pe-render owns, and frame-time counters for pe-xr.

**Not in M1:**
- the UI lab and the Gnosarium skin: right after M1 (decision C12);
- user shaders and their time budget (file 09, A6);
- avatars (A7);
- a material `BlobRef` beyond base colour, emissive, roughness and metalness;
- the evaluation of adopting an existing renderer before M5 (C4).

## 2. Dependencies and native boundaries

- **Crates:** `wgpu` and `wgpu-hal` 30, with `default-features = false` and only the `std`, `parking_lot`, `wgsl`,
  `dx12` (Windows) and `vulkan` features; `winit` 0.30; `glam`; `bytemuck`; `png` (encode, pure Rust); `ab_glyph`
  for capture labels (pure Rust). pe-render **re-exports `wgpu` and `wgpu-hal`**, and pe-xr uses only those (xr.md 8.2).
- **Why no GLES:** wgpu's default features pull GLES. That adds khronos-egl ("links a system library") and a static
  import of `opengl32.dll`; without it, both are gone [V M0, `imports-windows.txt`].
- **The DX12 shader compiler is pinned to FXC** (`Dx12Compiler::Fxc`, `d3dcompiler_47.dll`, part of Windows), for
  the D3D12 fallback. wgpu's default `Auto` loads any `dxcompiler.dll` found on the PATH [V wgpu-types 30 source],
  an unregistered boundary.
  - **The cost:** with FXC, wgpu-hal runs D3D12 at shader model 5.1, and D3D12 multiview needs view instancing and
    SM 6.1. So the D3D12 path draws two passes per frame [V @shader-reviewer, wgpu-hal 30 dx12/adapter.rs:436, 722].
  - **Shipping DXC is out:** it would distribute a native Microsoft binary, beyond the boundaries the user's D1
    ruling allows, unless the user asks for it [R coordinator, on review M1].
- **The register of boundaries** (plan A8) gets pe-render's rows, from the M0 report section 4:
  - the Vulkan loader and driver;
  - `d3d12.dll`, `dxgi.dll` and the D3D12 driver;
  - FXC;
  - on Linux, the system's Vulkan loader and the display server's libraries, which winit loads at run time [I].
- **Shaders are WGSL,** compiled by naga, which is Rust. User shaders are out of M1.

## 3. Threads and data flow

```
 simulation thread ──publishes──► FrameView (Arc, atomic swap) ◄──reads── render thread
                                                                        │
       pe-xr (VR mode): xrWaitFrame paces it; it hands eye views        ├─► window (desktop mode)
       desktop: present (Fifo) paces it                                 ├─► headset eyes (via pe-xr)
                                                                        └─► captures (slotted between frames)
 render thread ──HandFeed (desktop hand)──► simulation thread
 render thread ──effects to play──► audio sink (pe-app), haptics (pe-xr)
```

- **The render thread** owns the GPU context, the display layer's state and the desktop hand. In VR mode it is pe-xr's
  XR thread (xr.md 7.1). It never takes a lock the simulation holds: it reads the newest frame view by one atomic
  pointer swap, and keeps the previous one for interpolation.
- **winit's event loop** must run on the main thread on some platforms [V winit 0.30 docs]. WS-app decides where:
  either the main thread is the render thread, or the event loop forwards events to it.
- **The simulation never waits on rendering,** and keeps stepping while the window is minimized or the headset is off.

## 4. The GPU context

```rust
pub struct GpuContext { pub instance: wgpu::Instance, pub adapter: wgpu::Adapter,
                        pub device: wgpu::Device, pub queue: wgpu::Queue }
```

- **Accepted from outside** (xr.md 4.1). In VR mode pe-xr creates it on the runtime's adapter. In desktop mode
  pe-render creates it.
- **Vulkan everywhere first** [R coordinator, on review M1]. The desktop context is Vulkan on Windows and Linux,
  with `Features::MULTIVIEW` when offered.
  - **When VR is configured at start,** pe-xr creates the context through the runtime from the start
    (`xrCreateVulkanInstanceKHR` and `xrCreateVulkanDeviceKHR`), and the window draws on it. Entering and leaving VR
    then re-create nothing: those Vulkan objects are the application's to destroy, so they outlive the session
    (xr.md 4.1, draft 6 ab625b58) [I until checked in code].
  - **The context is re-created only** when VR is entered from a context pe-render made itself (the runtime must create
    the VkInstance and VkDevice), or when the runtime names a different device after instance loss. pe-render handles
    a new context at any time (section 4, losing the context). The display layer's CPU state survives, so nothing
    jumps.
  - **The D3D12 fallback,** if the runtime's Vulkan path fails: a DX12 context on the runtime's LUID, which leaving VR
    can keep, drawing two passes (FXC, section 2).
  - M0 measured both on the user's headset: Vulkan 944 of 944 frames, D3D12 941 of 941, at 120 Hz [V].
- **Limits:** pe-render states its own limit set, `Limits::default()` raised only where it needs more. It never asks
  for an adapter's full limits: wgpu-core asserts `max_buffer_size <= u32::MAX`, and the full limits break that
  [V M0 headset run].
- **Losing the context** (session loss, leaving VR on Vulkan): pe-render drops every GPU resource and rebuilds from the
  next frame view. Meshes and materials are derived from world data, so nothing but time is lost. The display layer's
  CPU state survives, so drawn poses don't jump on a rebuild.

## 5. Views, cameras and coordinates

```rust
pub struct EyeView {
    pub pose: RigPose,                 // in the rig frame, at the display time
    pub fov: Fov,                      // asymmetric angles; a desktop view gives a symmetric one
    pub color: wgpu::TextureView,      // one array layer, or its own texture
    pub depth: Option<wgpu::TextureView>,
    pub viewport: Rect,
}
pub struct LocalPresentation {
    pub display_time: HostTime,
    pub hands: [Option<DisplayHand>; 2],   // late-latched pose and joints, from pe-xr; None on the desktop
}
impl Renderer {
    pub fn new(ctx: GpuContext, settings: RenderSettings) -> Renderer;
    pub fn render_views(&mut self, view: &dyn FrameView, eyes: &[EyeView], local: &LocalPresentation) -> FrameStats;
}
```

- **The camera's world position** is the rig's `Pos` (from the frame view's `local_rig`) plus the eye's rig-frame
  pose, times the rig's rotation and scale.
- **Every object's model matrix is camera-relative:** translation `(object Pos − camera Pos)` per axis, as world-model
  4.2's pinned f64 subtraction, then cast to f32 in the camera's frame. Far objects lose precision only where it
  doesn't show. A raw absolute never reaches the GPU.
- **Projection:** reverse Z, with an infinite far plane on the desktop and in captures. In VR the far plane is finite
  and stated, because the depth layer (`XR_KHR_composition_layer_depth`) carries near and far. Whether Virtual
  Desktop's runtime takes an infinite far is open (R2).
- **The player's scale** is unbounded (xr.md). Near and far scale with the rig's scale, so a giant's world and an
  ant's world both fit the depth range [I].
- **Stereo:** with `Features::MULTIVIEW` (Vulkan), both eyes in one pass. Without it, which includes the D3D12
  fallback under FXC, one pass per eye, as the M0 check does; that cost is measured at the headset test.

## 6. The scene renderer (M1)

**What it draws, from each `ViewObject`** (world-model.md 9.3):

| From | Drawn as |
|---|---|
| `Shape`: box, sphere, capsule (a plane is a thin box) | Built-in unit meshes, scaled and instanced. Capsules are a cylinder and two half-spheres, generated per radius-to-length ratio and cached |
| `Shape`: mesh `SimBlobRef` (static bodies) | The mesh, uploaded once per blob hash |
| A display mesh `BlobRef` | The mesh when its bytes are here; its bounding box until then (world-model 3.4) |
| `Appearance` | Base colour, emissive, roughness, metalness: a small PBR-ish forward shader (Lambert plus GGX specular), one material uniform per instance |
| `Light` | Forward-lit: up to 16 point and spot lights per frame, picked by distance and intensity, plus one directional key light from the world's environment. Only the key light casts a shadow in M1 (R3) |
| Hands (`hand_of`) and the own hands | Section 8 |

- **Batching:** one instanced draw per (mesh, pipeline). Instances are rewritten each frame from the display layer's
  drawn poses: 500 balls is 500 × 80 bytes, trivial to upload.
- **Shadows:** one shadow map for the key light (2048², 3×3 PCF), as in the video renderer. Contact matters in VR
  (balls on a floor), and it costs one depth pass [I: under 0.5 ms for 500 instanced spheres, to be measured].
- **Anti-aliasing:** 4× MSAA on the desktop. In VR, 4× MSAA if the per-period budget allows, else 2× or none, chosen
  by the render scale controller (section 10).
- **Colour:** linear lighting into an sRGB target, with a simple filmic tone map. No HDR display output in M1.
- **Markers for the user:** an outline pass (for `view_capture`'s highlight, and for the correction cue, D3), drawn by
  inflating back faces into a stencil-tested pass.

## 7. Time: interpolation, the drawn time (X8) and rates

- **Frame views arrive at the simulation rate,** a fixed tick set per world within a supported range (60 Hz by
  default) and never tied to the display [R m8ac62c1872ac, md4b42d16d9df]. Each carries its `sim_time` (a
  `HostTime`). The display runs at its own rate. **D8:** the same code serves any display rate against any simulation
  rate, and interpolation is never counted as a correction.
- **The drawn time `t_draw`** for a display frame is a setting [X8, with @xr-eng]:
  - **(a) Extrapolate (default):** `t_draw = display_time`. Each object is moved from the newest frame view by its
    velocity (S5 carries linear and angular velocity, section 12). The extrapolation span is capped at `L_max`, the
    measured display lead plus one simulation frame. This is design.md 18.3's rule and xr.md 7.2's proposal.
  - **(b) Interpolate late:** `t_draw = display_time − D_render`, between the two frame views around it. Nothing is
    drawn from a guess, but the world (not the head or the own hands) is that much late.
- **What each costs, at the lead measured on Virtual Desktop** (38-63 ms p50-p95 [V M0]):
  - (a) draws a ball at 5 m/s up to 30 cm from where the next frame view puts it, when it hits something meanwhile.
    The display layer blends that miss exactly as a correction (D2), and **in-view misses above `E_cue` carry the
    cue and count** under the user's target [R coordinator, on review M2]: the player sees a jump the same way
    whatever caused it. `extrapolation_error` reports them apart as well.
  - (b) shows every object 40-60 ms or more after its simulated time.
  - Neither makes a pushed ball react sooner: the simulation hasn't seen the push yet.
- **Counted either way** (for pe-xr's counters too): `t_draw − display_time`, the extrapolation span, the age of the
  frame view drawn, and `extrapolation_error` under (a).
- **Desktop:** the display time is the next present, estimated as now plus one refresh period; the same rule applies.
- **The test of D8:** displays at 72, 90, 120 and 165 Hz against simulations at 60 Hz, 90 Hz and one odd rate in
  the range, in a fake clock. Each
  display frame's `t_draw` must follow from its display time and the frame views by the chosen rule, and the drawn
  pose of an object in steady motion must be continuous, its step bounded by velocity × display period.

## 8. The display layer (divergence.md 8, D1-D8)

State: per object id, on the render thread, a **drawn pose** and a **correction offset** (position and rotation)
that decays to zero. The simulated pose comes from S5; time-resolving it (section 7) gives the **target pose**.

- **D1. Two poses.** drawn = target ∘ offset. The simulation never reads the drawn pose.
- **D2. Blending a correction.** On **every new frame view**, for every object not held (D4), the offset becomes
  `drawn_before − target_now`, both taken at **this display frame's `t_draw`** (review M3):
  - `drawn_before` is the old state resolved to `t_draw`: the previous frame views time-resolved to `t_draw` (section
    7), composed with the current offset. It is not the pose drawn at the previous display frame, which lags a moving
    object by v × one display period and would put a hitch on every moving object at every simulation tick;
  - `target_now` is the new frame views resolved to the same `t_draw`.

  So the offset is exactly the change of prediction, and the drawn pose never moves at the instant a frame view
  arrives. A non-zero offset has two causes, and both blend under D3:
  - **a correction:** S5 marks the object `corrected` (its simulated pose was rewritten by a replay, repair or merge
    since the last publication). It counts in `corrections_per_min`;
  - **an extrapolation miss** under X8 (a): the object did something between frame views that its velocity didn't
    predict. It counts in `extrapolation_error`, and in `snaps_in_view` and `cueless_large_corrections` exactly like
    a correction [R coordinator, on review M2].
  - Under X8 (b) and for objects in steady motion, the offset is zero up to float error, so nothing blends.
  The blend itself:
  - the offset then decays with a monotone smoothstep over the blend time. Position decays linearly in the gap;
    rotation decays by slerp to identity. There is no overshoot;
  - a further correction mid-blend restarts from the current drawn pose, never jumping back.
- **D3. By size and by view.** With `e` the offset's size (position, or angle as a distance at the object's bounding
  radius):
  - `e < E_small` (5 cm or 10° [I]): blend over `T_blend` (100 ms);
  - `E_small` ≤ `e` ≤ `E_snap`, out of every view: blend over `T_blend`, with no cue (review L1);
  - `e > E_snap` (1 m or 90°) and out of every view: jump, with offset 0;
  - in view: blend over `T_blend`, or over `T_fast` (40 ms) above `E_snap`;
  - **every in-view correction above `E_cue`** (a hand's width, 10 cm [R meef4f248923b]) **carries the cue.** My
    proposal for the cue (R6): a 250 ms fading outline on the object, plus a faint ghost at the pose it left, fading
    over the same time. It is visible, but not a flash. `cueless_large_corrections` counts any miss; the code path
    makes a miss impossible, and the test checks it;
  - "in view" means inside any current view frustum (either eye, the window), tested with the object's bounding
    sphere;
  - the "jump if the blend's path would pass through a solid" rule is after M1 [I, divergence.md 8].
- **D4. Never correct what the player holds.** The local player's own hands, and objects `held_by` the local member,
  get no offset, ever.
  - In VR the own hands are drawn at pe-xr's late-latched pose (`LocalPresentation.hands`), and a held object at
    that pose times the S5 grip offset (xr.md 5.6, X1). "Draw where simulated" is the other setting.
  - On the desktop the own hand is drawn at its current input pose (the mouse ray now, section 9): the desktop's
    late-latch, never extrapolated or corrected. A held object follows it at the grip offset.
  - **When a hold ends** (released, thrown, or undone because a race was lost), the object enters the display layer
    with its last drawn pose and velocity. Its offset is set from there and blends; a lost grab also plays its short
    cue (D7).
  - **When a hold starts,** the object snaps to the hand (xr.md 5.6).
  - `own_error` measures the offset applied to own hands and held objects, which must be exactly zero. It doesn't
    measure "drawn equals simulated", which late-latching breaks on purpose (xr.md 5.6, as agreed with
    @engine-architect).
- **D5. Remote hands blend** like any object.
- **D6. One-shot effects fire once.**
  - pe-render keeps the set of effect keys already played (frame, causing entry or "physics", object, kind,
    ordinal), covering `W_fg` (2 s) (divergence.md 069e147d);
  - a key seen again is not replayed;
  - **a new one-shot effect plays only if its causing entry arrived at this machine within `T_effect_lag` (1 s) of
    the effect's frame.** An effect that arises only from a merge, or from a replay with no newly arrived cause, is
    never played; its sustained state shows as state. So a remote push at 0.6 s one-way still sounds its collision
    once, and a 60 s merge plays nothing (divergence.md 8, D6);
  - a sustained effect whose key lands in S5's retracted list stops, fading over `T_blend`;
  - own effects play at once; others' wait `D_effect`, a duration (default 0 s).
  - pe-render plays visual effects (particles) itself. It hands sound and haptic plays and stops, already
    de-duplicated, to their sinks: audio to pe-app, haptics to pe-xr.
- **D7. Lost races and refusals are told.** A lost grab: the slide out of the grip plus its cue (D4). A refused edit
  is shown by the UI layer from WorldApi's outcome (after M1; in M1, through the agent).
- **D8.** Section 7.
- **Controls that bite** (divergence.md 8): with blending switched off, `snaps_in_view` must rise; with the own hand
  blended, `own_error` must go non-zero. The tests catch both.

## 9. The desktop hand

B has no headset (plan M1, demo step 4: "B's desktop hand, a mouse ray, does the same").

- **The ray:** from the camera through the cursor. Hovering, the hand sits where the ray first hits a drawn solid,
  3 cm short of the surface, so pointing at something never disturbs it. With nothing hit, it sits at the last depth
  used (default 1.5 m).
- **Grab:** the left button closes the hand (`closed = true`, `grip = 1`). While closed, the ray's depth is locked at
  the grab distance, so the held object moves in the plane facing the camera. The wheel changes the depth. Releasing
  the button opens the hand; the hand's velocity at release throws.
- **Push (deliberate, review L3):** holding Shift drives the hand along the ray, 10 cm past the surface it hovers
  over, at about 1 m/s, and moving the cursor then sweeps it through what it touches. The hand is a physical body in
  the simulation, as tracked hands are; only Shift or a grab puts it into contact.
- **Orientation:** the hand faces along the ray. Q and E roll it (for the "rotation" a hand entry carries).
- **Camera:** right-drag orbits and WASD moves the rig, as fly controls in M1. The UI lab revisits them.
- **Output:** `HandInput { source: Desktop, ... }` in the rig frame, pushed to a ring that implements `HandFeed`
  (xr.md 8.1), so the simulation reads desktop and tracked hands the same way. `discontinuity` is bumped when the hit
  depth jumps by more than 20 cm (ray moved onto a far object), so a jump is never read as a throw [I].
- **Hit testing** uses the drawn scene's bounding shapes (CPU, against the last drawn poses), not physics queries,
  so the render thread never asks the simulation [I].

## 10. The window and pacing

- **winit 0.30,** one window. Present mode `Fifo` (vsync) by default, `Mailbox` as a setting.
  `desired_maximum_frame_latency` 2.
- **Pace to what the display gives:** pe-render reads the monitor's refresh rate (winit `refresh_rate_millihertz`) and
  measures present intervals. In VR it reads the runtime's predicted display period every frame. It never assumes 90
  or 120 Hz [R m981c546957f2].
- **Budget per period:** CPU recording and GPU time are measured each frame (GPU time with timestamp queries when the
  adapter has them). Over budget in more than a quarter of the frames of the last 0.25 s: the render scale drops a
  step (0.1, down to 0.7), and MSAA in VR drops before it. Within 70% of budget for 2 s: back up a step. Every limit
  here is a duration, never a frame count [R md4b42d16d9df]. A single miss is the compositor's (VR) or
  vsync's (desktop) to absorb.
- **Passes per frame in VR:** the Vulkan path, one multiview pass plus the key light's shadow pass. The D3D12 fallback,
  two eye passes plus the shadow pass, so its CPU recording and vertex cost about double. The 120 Hz budget
  (xr.md 7.3: about 6 ms of GPU) is checked on both paths at the headset test, with 500 balls [I until measured].
- **Measured baseline** [V M0, a feasibility renderer with 40 bodies]:
  - window at 165 Hz: p50 6.06 ms, p99 7.28 ms present interval;
  - headset at 120 Hz (Quest 3 via Virtual Desktop), D3D12 and Vulkan: 941 and 944 of 941 and 944 frames submitted
    in 8 s.
- **Minimized or occluded:** stop drawing, keep the simulation running.
- **A desktop mirror of the headset** (VR mode, optional): copy the left eye into the window inside the frame's work,
  before the swapchain image's state is restored (xr.md 4.2).

## 11. The offscreen capture (`view_capture`)

As agreed with @api-eng (api.md 8, mcd68d4325ea7):
- **What it draws:** the snapshot the request names, at its **simulated** poses. The display layer is bypassed, so
  the image shows what the world holds.
- **Cameras:**
  - a player's head (default: mine);
  - an orbit around an object, framed by its bounds when no distance is given;
  - a placed camera.
  Size up to 1920×1080, default 768×432.
- **Where it runs:** on the render thread's device, into its own offscreen target, slotted between frames. Only the
  readback is deferred: the buffer map completes on a later frame, so a capture never stalls a present, and if the
  frame budget is tight it waits a frame. The caller's deadline bounds the wait. PNG encoding runs on a worker
  thread.
  - Measured [V M0]: 1280×720, about 40 bodies, 4.5-5.2 ms with readback on an RTX 3080 Ti; two draws give
    identical pixels.
- **Highlight and labels:** the outline pass (section 6), and names drawn beside the object with `ab_glyph`.
- **`visible_list`:** projected bounding boxes without occlusion in M1, marked `visible_approx: true`. An id pass (an
  R32Uint target in the same draw) can follow.
- **Headless:** a software adapter (WARP on Windows, lavapipe on Linux; plan 4.6). With no adapter,
  `capture_unavailable`.
  - Running in Session 0 is still to be measured; it's the first test of the capture path.
- **One capture in flight per principal;** another gets `rate_limited` (api.md).

## 12. What pe-render needs from S5 (to @engine-architect, @world-model, @vdw-physics)

Beyond world-model.md 9.3's object fields (id, class, simulated `Pos`, rotation and scale, shape, appearance, light,
`hand_of`, `held_by`, edit version) and xr.md 8.1's `sim_time` and `local_rig`:
- **linear and angular velocity** per moving object, for extrapolation (X8 (a)) and for a released object's starting
  velocity;
- **`corrected`** per object: the simulated pose was rewritten by a replay since the last publication (divergence.md
  11.3). Interpolation can't tell a correction from fast motion;
- **the grip offset** of a held object (xr.md 8.1);
- **effect events** with their stable keys, plus the retracted list, and per effect the arrival time of its causing
  entry, or a flag that it arose with no newly arrived cause (D6);
- **a bounding radius** per object, or the shape that gives it, for view tests, the desktop hand and captures;
- **a frame view that can be held:** the render thread keeps two (newest and previous), and a capture keeps its
  snapshot until drawn. So frame views are `Arc`s that the simulation never mutates after publishing.

## 13. Counters

pe-render reports, per second and as distributions over a run, to pe-diverge's table (divergence.md 7) and to pe-xr's
(xr.md 9):
- `corrections_per_min`, `visible_error` (mean and max, drawn against target, objects in view), `snaps_in_view`,
  `cueless_large_corrections` (must be 0), `own_error` (must be 0);
- `extrapolation_error`: under X8 (a), the gap between where an object was drawn and where the next frame view put
  it, kept apart from corrections;
- per display frame: CPU recording time, GPU time, render scale and MSAA, `t_draw − display_time`, the age of the
  frame view drawn;
- captures: queued, drawn, deferred, timed out.

## 14. Tests

On the gate, in pe-render, with a software adapter where a GPU is needed:
1. **Display layer, pure CPU** (no GPU):
   - the drawn pose reaches the target within `T_blend` of the last correction;
   - a mid-blend correction never jumps;
   - no overshoot;
   - every in-view correction above `E_cue` has a cue;
   - `own_error == 0`, including a hold that ends by a lost race;
   - the two controls of section 8.
2. **Effects:** a replayed key fires once; a retracted sustained effect stops; own effects play at once.
3. **Rates (D8):** section 7's fake-clock test at 72/90/120/165 Hz displays against 60 Hz, 90 Hz and one odd
   simulation rate in the range (e.g. 75 Hz), under both X8 rules. **Steady motion keeps a zero offset** (to float
   tolerance) across every frame-view arrival, at every rate pair (review M3).
4. **Coordinates:** a scene placed at 10^15 m from the origin draws the same image as at the origin (pixels equal on
   one adapter), because only differences reach the GPU.
5. **Capture:** an image of a known scene has the expected coverage and identical pixels twice; `visible_list` names
   the highlighted object; `capture_unavailable` without an adapter.
6. **Against pe-harness scenarios** (with WS-harness): the counters of section 13 for every network scenario, and the
   user's convergence target "no correction larger than a hand's width in view without a cue" over 20 seeds and 5
   delays (plan M1 acceptance).
7. **By hand, on the headset** (each run with a go through @coordinator-opus): the frame time with 500 balls and one
   remote hand at 60 ms; X8 (a) against (b); the cue's look; hand tracking enabled in Virtual Desktop first (the user
   will, m981c546957f2).

Golden images are compared within a tolerance, per adapter, because software and hardware rasterizers differ in their
last bits [I].

## 15. Open points

For @coordinator-opus unless marked; my proposal first.

| # | Point | Proposal |
|---|---|---|
| R1 (with @xr-eng) | X8, the drawn time | (a) extrapolate, capped at `L_max`, as the default; (b) as a setting; the headset test judges. Misses blend, cue and count either way (ruled, review M2) |
| R2 (with @xr-eng) | Infinite far plane in VR's depth layer | Finite in VR (100 × rig scale [I]) until Virtual Desktop's runtime is shown to accept infinite |
| R3 | Shadows in M1 | One shadow-casting key light; lamps unshadowed |
| R4 | The graphics path and shader compiler | **Ruled** (on review M1): Vulkan first, with multiview; D3D12 with FXC, two passes, as the fallback; no shipped DXC |
| R5 (WS-app) | The window's thread | The main thread is the render thread in desktop mode; WS-app decides |
| R6 | The correction cue's form | A 250 ms fading outline, plus a ghost at the old pose |
| R7 | The desktop hand's controls | As section 9; revisited in the UI lab |

## 16. Size and order

About 6-8 engineer-weeks of M1's "renderer and VR with hands" (8-12, shared with WS-xr) [I]:

| Step | Size (weeks) | Needs |
|---|---|---|
| Context, window, pacing, the scene renderer over a mock frame view; camera-relative transforms | 2 | S5 v0 (mockable) |
| The display layer and its tests, effects | 1.5 | S5's `corrected`, velocities and effect keys |
| `render_views` for pe-xr: multiview, late-latched own hands | 1 | pe-xr's skeleton |
| The offscreen capture, outline and labels, for `view_capture` | 1 | pe-api's port |
| The desktop hand | 0.5 | `HandFeed` in S5 |
| Counters, harness scenarios, headset tuning | 1-1.5 | pe-harness; the coordinator's go |
