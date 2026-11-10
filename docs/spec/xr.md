# pe-xr: the OpenXR session, stereo, tracked hands and 90 Hz pacing

Status: draft 1, 2026-11-10, by @xr-eng (WS-xr). Nothing in it is built. It specifies M1's VR work in
`docs/refound-plan.md` (branch `docs/refound-plan`, b610464), section 3.1: "the OpenXR session, stereo through
pe-render, tracked hands into hand entries, 90 Hz pacing". Seam S5 is proposed in section 8. @engine-architect (WS-seams)
takes its pe-seams part as proposed, for S5 v0; the pe-xr and pe-render interface (8.2) is to agree with @ballpit-viz
(WS-render).

Marks: **[V]** verified, with what I checked named; **[I]** my inference or design judgement; **[M0]** to be settled
by @ballpit-viz's M0 feasibility check (OpenXR with wgpu), which this spec does not repeat.

## 1. What pe-xr is for, and what it is not

pe-xr turns a headset into three things the rest of the engine already understands:
1. **Views to draw.** Two eyes per display frame, each with a pose, a field of view and a target texture. pe-render
   draws them.
2. **Hand inputs.** A sample of each hand per display frame: a pose, a grip value and a grasp state. The simulation
   thread turns them into hand entries.
3. **A clock.** The headset's predicted display time, as a time the renderer and the simulation share.

It holds no world state, decides nothing about physics, agreement or entries, and never blocks the simulation. The
simulation runs at its own fixed rate whether a headset is on, off, lost or absent (R2).

Rules it follows:
- **The user's requirements:** full VR (R10); a fixed timestep independent of the frame rate (R2); the 90 Hz target
  (D4); 100% Rust, with the OpenXR runtime loaded at run time as an operating-system boundary, by a loader of
  our own in Rust (md35ba688f6b4; section 2); Windows and Linux PC VR (D18a, D20); standalone headsets excluded from the prototype
  (mb09ab8952a38).
- **The governing principle** (mfc7e0ddc4137): the local user is never held up. For pe-xr this means: head motion
  never waits for the simulation or the network, and a player's own hand never waits for agreement.
- **The display layer's rule** (file 14, 4.1): a player's own hands and what they hold are never blended.

Not in M1: avatar tracking beyond head and hands (M5); eye and face tracking; passthrough; standalone headsets;
macOS (flat screen only, later); a VR user interface (the UI lab follows M1, WS-render).

## 2. Platforms, runtimes and the loader

- **The crate:** `openxr` 0.22 with `default-features = false`: no `loaded`, `static` or `linked` feature [V: the
  crate's features in the cargo cache: `static` builds Khronos' C++ loader with CMake, `linked` links it, `loaded`
  opens it with `libloading`]. The native-code audit's allowlist names `openxr-sys` with none of them.
- **There is no system loader to open on Windows.** Khronos' loader is a C library each application ships; this PC
  has no `openxr_loader.dll` in System32 [V: @ballpit-viz's M0 finding, and my listing of System32]. So pe-xr has its
  own small loader in Rust, the one @ballpit-viz is writing for the M0 check:
  1. find the active runtime's manifest: `XR_RUNTIME_JSON` if set, else `ActiveRuntime` under
     `HKLM\SOFTWARE\Khronos\OpenXR\1` on Windows, else `active_runtime.json` under the XDG config paths on Linux;
  2. open the runtime's library from the manifest with `libloading`;
  3. call its `xrNegotiateLoaderRuntimeInterface`;
  4. build the `openxr::Entry` from the returned `xrGetInstanceProcAddr` (`Entry::from_get_instance_proc_addr`).

  The cost: no API layers. Virtual Desktop registers one implicit layer (an Oculus compatibility layer), which we do
  without [M0]. The runtime is then the only boundary, and it goes in the register of boundaries (plan C6).
- **Graphics path in M1,** following the M0 check (section 4.4): D3D12 through `XR_KHR_D3D12_enable` on Windows, and
  Vulkan through `XR_KHR_vulkan_enable2` on Linux and as Windows' alternative.
- **Runtimes on this PC** [V: read from `HKLM\SOFTWARE\Khronos\OpenXR\1`, registry only, no session started]: the
  active runtime is Virtual Desktop's (`virtualdesktop-openxr.json`); Meta Horizon's (`oculus_openxr_64.json`) is
  installed too. No SteamVR runtime is registered. Which runtime the headset tests use is the coordinator's and the
  user's choice (section 11, X5).
- **Linux:** SteamVR and Monado, through the same Vulkan path [I; not tried here].
- **No runtime registered, no runtime library, or no headset** is not an error. The application runs in desktop mode, says why once, and
  offers to retry (section 3.3).

Extensions:

| Extension | Use | If absent |
|---|---|---|
| `XR_KHR_D3D12_enable` (Windows) | The session draws into D3D12 swapchains from wgpu's D3D12 device, on the adapter the runtime names | The Vulkan path, or desktop mode |
| `XR_KHR_vulkan_enable2` | The runtime creates the VkInstance and VkDevice that wgpu then wraps | On Linux, no VR; on Windows, the D3D12 path |
| `XR_KHR_win32_convert_performance_counter_time` (Windows), `XR_KHR_convert_timespec_time` (Linux) | Converting `XrTime` to the engine's host clock (section 6.1) | No VR: without it the time base cannot be shared [I: every PC runtime I know offers it] |
| `XR_EXT_hand_tracking` | Tracked hands: 26 joints a hand | Controllers only |
| `XR_EXT_hand_interaction` | A runtime-computed grip and pinch for tracked hands, through the action system | pe-xr computes grip from the joints (section 5.3) |
| `XR_FB_display_refresh_rate` | Ask for 90 Hz where the headset offers it | Run at the runtime's rate, and report it |
| `XR_EXT_local_floor` (core in OpenXR 1.1) | A floor-level space when STAGE is not offered | LOCAL, with the floor estimated from the head's height at start |
| `XR_KHR_composition_layer_depth` | Submit depth, so the compositor reprojects better | Colour only |
| `XR_MND_headless` | Sessions without graphics, for input tests against Monado in CI | Those tests skip |

[V: every extension above is in `openxr` 0.22's `ExtensionSet`, in the cargo cache.]

## 3. The session

### 3.1 Lifecycle

pe-xr runs the OpenXR session state machine as the specification defines it: IDLE, READY, SYNCHRONIZED, VISIBLE,
FOCUSED, STOPPING, LOSS_PENDING and EXITING. What each state means for the engine:

| State | The frame loop | Hand inputs | The simulation |
|---|---|---|---|
| IDLE, before READY | Not running | None: hands absent | Runs; the desktop window draws |
| READY | `xrBeginSession`, then the frame loop starts | None yet | Runs |
| SYNCHRONIZED | Frames are paced but not shown; submit no layers | None | Runs |
| VISIBLE | Frames are shown; input is not focused | Absent (the runtime gives no input) | Runs |
| FOCUSED | Full frame loop | Sampled every frame | Runs |
| STOPPING | `xrEndSession`; back to desktop mode | Absent | Runs |
| LOSS_PENDING | Destroy the session and the graphics objects; try again later | Absent | Runs |
| EXITING | Destroy the session; desktop mode | Absent | Runs |

- **The simulation never stops for VR.** Every row says "runs".
- **Hands become absent** whenever input is not focused. Section 5.5 says what an absent hand does to a grab.
- **Instance loss** (`XR_ERROR_INSTANCE_LOST`, or a runtime restart) is handled as session loss plus a new instance.
  The GPU objects made on the runtime's device go with it, so pe-render must be able to drop and re-create
  its GPU resources (section 4.3).

### 3.2 Reference space and the rig

- **The tracking space is STAGE** when the runtime offers it, else LOCAL_FLOOR, else LOCAL with an estimated floor.
  In it, OpenXR's convention holds: metres, right-handed, +Y up, -Z forward.
- **pe-xr never sees world coordinates.** Everything it produces is in the **rig frame**: the tracking space, in
  small `f32` metres. The rig's place in the world is the local player's root object, which lives in the world model:
  an exact `WorldPos`, a rotation, and a scale, since the player's scale is unbounded (m3f7ef0e1f879). The simulation
  side applies that transform. So the precision rules for large worlds (file 17, the user's 19:23Z ruling) never
  reach pe-xr, and moving the player (locomotion, teleport, scaling) is an ordinary world change, not an XR concern.
- **Recentring** (`XrEventDataReferenceSpaceChangePending`) moves the tracking space under the player. pe-xr bumps a
  discontinuity counter in every hand sample (section 5.1), so a jump is never taken as a throw.

### 3.3 Desktop mode, VR mode, and the switch

- The application starts in desktop mode. It enters VR when the user asks for it, or at start when configured to,
  and a runtime and headset are present.
- **Probing for a runtime** creates an instance and asks for the HMD's system id. It starts no session. Probing is
  still a call into the runtime, so it is off unless configured (section 10.3).
- In VR mode the desktop window may stay open as a mirror of one eye (pe-render's choice; useful for recording the
  demo).
- **The desktop hand and the tracked hands are alternatives** for one player: the input source in use is named in
  every sample (section 5.1).

## 4. Graphics: one GPU device, created through OpenXR

### 4.1 Who creates the device

The runtime decides which GPU the headset is drawn on, and on the Vulkan path it creates the VkInstance and VkDevice
itself, with the extensions the runtime and wgpu each need. So:
- **In VR mode, pe-xr creates the GPU context** and gives it to pe-render.
- **In desktop mode, pe-render creates it** itself.
- **pe-render accepts a context from outside** in either case. Switching modes re-creates it (section 4.3).

**D3D12, on Windows** [M0: @ballpit-viz's first path]:
1. Ask the runtime for its graphics requirements (`XR_KHR_D3D12_enable`): the adapter's LUID and the minimum
   feature level.
2. Create wgpu's D3D12 instance as usual, and pick the adapter whose LUID matches.
3. Create the device with the features pe-render asks for.
4. Create the session with the D3D12 binding: the raw `ID3D12Device` and command queue, taken from wgpu through
   `as_hal`.

**Vulkan, on Linux and as Windows' alternative** [M0; the names are wgpu 30's, V: in the cargo cache]:
1. Ask wgpu-hal's Vulkan backend for the instance extensions it wants (`Instance::desired_extensions`), and create
   the VkInstance with `xrCreateVulkanInstanceKHR` (`openxr::Instance::create_vulkan_instance`).
2. Wrap it with wgpu-hal's `Instance::from_raw`, and then `wgpu::Instance::from_hal`.
3. Ask the runtime for its physical device (`vulkan_graphics_device`) and expose it as a wgpu-hal adapter
   (`expose_adapter`).
4. Create the VkDevice with `xrCreateVulkanDeviceKHR` (`create_vulkan_device`), enabling the adapter's
   `required_device_extensions` for the features pe-render asks for, multiview among them.
5. Wrap it with `device_from_raw`, then `wgpu::Adapter::create_device_from_hal`.
6. Create the session with the Vulkan binding: the instance, physical device, device, queue family and
   queue index that wgpu uses.

### 4.2 Swapchains

- **Colour:** one swapchain with two array layers, one per eye, for single-pass multiview. If the adapter lacks
  `Features::MULTIVIEW` [V: the feature exists in wgpu-types 30], two swapchains and two passes.
- **Format:** the first sRGB format the runtime offers that pe-render can target (RGBA8 or BGRA8 sRGB).
- **Size:** the runtime's recommended size per eye, times a render scale pe-render may lower under load (section 7).
- **Depth:** a second swapchain in a depth format, submitted with `XR_KHR_composition_layer_depth` when offered.
- **Wrapping:** each swapchain image is wrapped once, at creation, with wgpu-hal's `texture_from_raw` and
  `create_texture_from_hal`, and the textures are kept for the swapchain's life [M0]. pe-xr acquires, waits for and
  releases images; pe-render only draws into the view it is handed.

### 4.3 Losing the device

On session or instance loss, or when leaving VR, the GPU context goes away. pe-render must drop every GPU resource
and rebuild from the frame view on the next context. Its meshes and materials are derived from world data, so
nothing is lost but time [I].

### 4.4 Which path, on which runtime

- Virtual Desktop's runtime, the active one here, draws through Direct3D underneath, so its Vulkan support is interop
  [I]. That is why the M0 check tries D3D12 first on Windows, and Vulkan second.
- **pe-xr keeps both paths behind one interface:** creating the context, the session binding, and wrapping swapchain
  images are the only graphics-specific code, and pe-render sees only wgpu types either way.
- **Linux needs Vulkan,** so the Vulkan path is built in M1 even if D3D12 serves Windows. Whether it can be tested
  before M1's end depends on a Linux machine with a headset, or on the Monado tier (section 10.2).
- If neither path works on Virtual Desktop's runtime, the fallback is the Meta runtime, which offers both [I], and
  the user picks (X5).

## 5. Hands

### 5.1 What a hand sample is

Every display frame, while input is focused, pe-xr samples both hands and publishes one input sample (section 8.2):

- `hand`: left or right.
- `source`: a tracked hand, a controller, or (from pe-render) the desktop hand.
- `sampled_at`: the host time the pose is for.
- `tracked`: whether the pose is valid now. When false, the pose is the last valid one.
- `pose`: the grip pose in the rig frame: position in metres and a unit quaternion, `f32`.
- `linear_velocity`, `angular_velocity`: in the rig frame, when the runtime gives them.
- `grip`: 0 to 1, how closed the hand is (section 5.3).
- `closed`: the grasp state, decided on the author's machine with hysteresis (section 5.3).
- `discontinuity`: a counter, bumped when the pose jumps for a reason other than motion: a recentre, tracking
  reacquired after a loss, or a change of source.

A sample is published every frame, moving or still. A held hand keeps sending even when it is still (design.md 6.10,
review SH-1; D52's coast must never run past a held hand). Dropping unchanged samples is never pe-xr's to do.

### 5.2 From samples to hand entries

- **The hand entry's format belongs to pe-world** (WS-world; the input payload of S1). pe-xr only feeds it.
- **The simulation thread, at frame k,** takes the hand sample for frame k's time (section 6.2), applies the rig
  transform, and quantizes the result into the entry. Plan M1 says hand entries carry "position, rotation and grip".
  My proposal for the quantization, as design.md 18.4 has it, is a 1 mm grid for position and a 0.001 rad grid for
  rotation, plus `grip` as 8 bits and `closed` as one bit. WS-world fixes it.
- **Determinism needs nothing from pe-xr.** The floating-point work before quantization runs only on the author's
  machine. Every peer sees the quantized, signed bits.
- **Own hands only.** An entry moves only its author's own hands (the M1 test of
  @item:primordial-engine/session-validation-a-member-moves-only-its-own-h). pe-xr produces samples for the local
  player and nobody else.
- **When a hand becomes physical** (D18b, "only when engaged", or the alternative of "always physical within reach")
  needs the world's geometry, so it is decided on the simulation side, not in pe-xr. Both modes are wanted at the
  headset test, so the choice is a setting there.

### 5.3 Grip and grasp

- **With `XR_EXT_hand_interaction`,** `grip` is the runtime's grasp value for a tracked hand.
- **Without it,** pe-xr computes `grip` from the joints: the mean curl of the middle, ring and little fingers, from
  the angle at each finger's proximal and intermediate joints, scaled to 0 (open) to 1 (fist) [I].
- **With a controller,** `grip` is the squeeze value.
- **`closed` uses hysteresis:** it closes when `grip` rises past 0.7 and opens when it falls below 0.4. The
  thresholds are settings, tuned at the headset test [I].
- **Pinch** (thumb tip to index tip) is sampled and kept for later tools, but it is not in M1's entries.

### 5.4 Controllers

Controllers go through the action system, with one action set: a grip pose, an aim pose, a squeeze value, a trigger
value and haptic output, with suggested bindings for `khr/simple_controller`, `oculus/touch_controller` and
`valve/index_controller`. Where a runtime offers both a tracked hand and a controller for one hand, the tracked hand
is used while it is tracked [I]. M1 needs controllers because tracking-only runtimes are not the only ones, and a
controller gives a recording of the same demo with less noise.

### 5.5 When tracking is lost

- **While not holding:** the sample goes `tracked = false`. The simulation side applies its world rule for an
  entry-less hand (D52: coast for at most 6 frames, then hold) [V design.md 6.10].
- **While holding:** after a grace period of 250 ms without tracking, pe-xr sets `closed = false`, so the object is
  released deliberately rather than held by a frozen hand [I]. The grace period is a setting.
- **When tracking returns,** `discontinuity` is bumped.
- **When focus is lost** (the headset is taken off, a system menu opens), both hands release at once.

### 5.6 Drawing the local player's own hands

The display layer (file 14, 4.1) never blends a player's own hands or what they hold. In VR there is one more choice:
where to draw them.
- **The simulated hand** lags the real one by up to one simulation frame plus the wait to the display time: 11 to 28
  ms at 60 Hz [I]. At 2 m/s that is 2 to 6 cm.
- **My proposal: draw the own hand at the tracked pose for the predicted display time** (late-latched), and draw a
  held object at that hand pose times the simulation's grip offset (the held object's pose relative to the simulated
  hand). This is design.md 18.3's rule. The simulated hand still does the pushing: other objects react to the
  simulated hand, so they may be pushed a few centimetres behind the drawn hand during fast motion.
- **The alternative:** draw the own hand where it is simulated. Nothing is ever drawn where the simulation does not
  have it, at the cost of a hand that trails the real one.
- **Both are a setting,** and the user judges them at the headset test, alongside D18b.
- **The plan's test** "the error of a player's own hands, and of what they hold, is exactly zero" reads, as agreed
  with @engine-architect: the display layer never applies a correction offset to the own hands or to what they hold;
  they are never blended toward a corrected simulated state. It does not mean "drawn pose equals simulated pose",
  which late-latching breaks on purpose.

## 6. Time

### 6.1 One clock

- **The host clock** is a monotonic nanosecond count from the operating system's performance counter (QPC on
  Windows, `CLOCK_MONOTONIC` on Linux). pe-xr, pe-render and the simulation thread all use it. In pe-seams it is
  `HostTime` (section 8.1).
- **pe-xr converts `XrTime` to and from host time** with the two time-conversion extensions (section 2).
- **The simulation's frame clock** (pe-net steers it toward the group's, file 14, 4.2) maps each frame number to a
  host time. pe-xr does not steer it and does not read the network.

### 6.2 Which sample a simulation frame gets

- pe-xr keeps a short ring of hand samples, about the last 100 ms, each stamped with its host time.
- **The simulation thread asks for frame k's hand at frame k's host time.** pe-xr interpolates between the two
  samples around that time. If the time is past the newest sample, it extrapolates from the newest sample's velocity
  for at most one display period, and otherwise returns the newest sample as it is.
- **Sampling time.** Each display frame pe-xr locates the hands twice: at the predicted display time, for drawing
  (section 5.6), and at the current time, for the ring. The ring's samples are what entries are made from, so an
  entry never carries a pose predicted into the future.
- **Measured:** the age of the sample each frame used, and the size of each extrapolation (section 9).
- **The thread.** The ring is written by the XR thread and read by the simulation thread with no lock either side
  waits on: a fixed-size ring with a sequence counter [I].

## 7. The frame loop and 90 Hz pacing

### 7.1 One frame

On the XR thread, which is also the VR render thread:
1. `xrWaitFrame`: the runtime paces the thread and returns the predicted display time and period.
2. `xrBeginFrame`.
3. Locate the hands at the current time; push the samples to the ring (section 6.2).
4. Take the newest published frame view (section 8): one atomic pointer swap, never a lock the simulation holds.
5. Acquire and wait for a swapchain image.
6. Locate the views (`xrLocateViews`) and the hands at the predicted display time. This is as late as possible, so
   the poses are fresh.
7. pe-render draws the frame view into the image: both eyes, the scene interpolated to the display time, the own
   hands late-latched (section 5.6).
8. Release the image, and `xrEndFrame` with one projection layer (and its depth, when offered).

When `shouldRender` is false, steps 3 to 7 are skipped, and `xrEndFrame` submits no layers. The loop never skips
`xrWaitFrame`, `xrBeginFrame` and `xrEndFrame`.

### 7.2 Rates

- **Display rate:** pe-xr asks for 90 Hz with `XR_FB_display_refresh_rate` where offered, and otherwise takes the
  runtime's rate. It reports the rate it got.
- **Simulation rate:** a world setting (D4), 60 or 90 Hz. pe-xr does not depend on it.
- **Interpolation:** each display frame draws the simulation at the predicted display time. pe-render interpolates
  between the two newest frame views, or extrapolates from the newest for at most one simulation frame (WS-render
  decides which) [I]. So 72, 90 and 120 Hz displays all work against 60 and 90 Hz simulations, as M1's rendering test
  asks.

### 7.3 The budget at 90 Hz

A frame is 11.1 ms. My budget [I], to be measured:

| Part | Budget | Note |
|---|---|---|
| XR thread CPU: steps 2-8, without drawing | 1 ms | Locating, acquiring, submitting |
| pe-render CPU: recording the frame | 3 ms | 500 balls, two eyes, multiview |
| GPU frame time | 8 ms | Leaves room for the runtime's compositor and, with Virtual Desktop, its video encoder |
| Simulation thread | its own core | Never on the XR thread. Rollback replays already cost 2.5 ms a frame at 90 Hz with 500 balls and one remote hand (file 02) |

- **When the GPU misses:** pe-render lowers its render scale step by step (down to 0.7) and raises it again after a
  stretch of frames within budget. The compositor's reprojection covers a single miss.
- **Thread placement:** the XR thread runs at normal priority; the simulation's workers leave at least two cores free
  for the runtime and the compositor [I]. The user's PC froze once while they were in VR under build load (team notes
  of 2026-10-30), so the engine must not take the whole machine either.

## 8. Seam S5: the frame view and the hand inputs (a proposal)

Plan 3.2 defines S5 as "a read-only view published once per frame: objects, simulated and drawn poses, appearance,
the events that trigger one-shot effects. It also carries the hand inputs from pe-xr and the desktop hand". This
section is my proposal for its XR side. WS-seams fixes v0 with WS-render and WS-state; until then, these are names
and shapes only.

### 8.1 In pe-seams (no GPU types)

```rust
/// Monotonic host nanoseconds, shared by pe-xr, pe-render and the simulation thread.
pub struct HostTime(pub i64);

pub enum Hand { Left, Right }
pub enum HandSource { Tracked, Controller, Desktop }

/// A pose in the local player's rig frame (tracking space): metres, right-handed, +Y up.
pub struct RigPose { pub position: [f32; 3], pub orientation: [f32; 4] }

pub struct HandInput {
    pub hand: Hand,
    pub source: HandSource,
    pub sampled_at: HostTime,
    pub tracked: bool,
    pub pose: RigPose,
    pub linear_velocity: Option<[f32; 3]>,
    pub angular_velocity: Option<[f32; 3]>,
    pub grip: f32,            // 0 open .. 1 closed
    pub closed: bool,         // author-side grasp, with hysteresis
    pub discontinuity: u32,   // bumped on a jump that is not motion
}

/// Written by pe-xr (or pe-render's desktop hand), read by the simulation thread.
pub trait HandFeed {
    /// The local player's hand for a simulation frame's host time; None while the hand is absent.
    fn hand_at(&self, hand: Hand, at: HostTime) -> Option<HandInput>;
}
```

And in the frame view, for the display layer and the cameras:

```rust
pub trait FrameView {
    // ... as plan Appendix A: frame(), objects(), effects() ...
    fn sim_time(&self) -> HostTime;           // the host time of this simulation frame
    fn local_rig(&self) -> RigTransform;      // the local player's rig in the world: exact position, rotation, scale
}
// Each ViewObject also carries flags: OWN_HAND (a hand of the local player) and HELD_LOCAL (held by one),
// and for a held object, the hand and the grip offset (its pose relative to that simulated hand).
```

`RigTransform`'s exact position is pe-core's `WorldPos` (or its successor). pe-render draws relative to the camera,
so large coordinates never reach a GPU float [I; pe-render's call].

### 8.2 In pe-render (GPU types stay out of pe-seams)

pe-xr depends on pe-render (plan 3.3: WS-render, then WS-xr), so the interface with GPU types lives in pe-render:

```rust
pub struct GpuContext { pub instance: wgpu::Instance, pub adapter: wgpu::Adapter,
                        pub device: wgpu::Device, pub queue: wgpu::Queue }

pub struct EyeView {
    pub pose: RigPose,                        // the eye, in the rig frame, at the display time
    pub fov: Fov,                             // asymmetric: left, right, up, down angles
    pub color: wgpu::TextureView,             // one array layer, or its own texture
    pub depth: Option<wgpu::TextureView>,
    pub viewport: Rect,
}

pub struct LocalPresentation {
    pub display_time: HostTime,
    pub hands: [Option<DisplayHand>; 2],      // the late-latched pose, plus joints for drawing the hand
}

impl Renderer {
    pub fn new(ctx: GpuContext, settings: RenderSettings) -> Self;
    pub fn render_views(&mut self, view: &dyn FrameView, eyes: &[EyeView], local: &LocalPresentation);
}
```

- pe-render chooses the projection's details (reverse Z, infinite far plane) and multiview or two passes.
- pe-render reports its frame times and render scale to pe-xr's counters.
- The desktop path calls the same `render_views` with one eye.

### 8.3 What I need from the others

| From | What |
|---|---|
| WS-seams (@engine-architect) | `HostTime`, `HandInput` and `HandFeed` in S5 v0; `sim_time` and `local_rig` in the frame view |
| WS-render (@ballpit-viz) | `GpuContext` accepted from outside; `render_views` over eye views; drawing own hands late-latched; dropping and rebuilding GPU resources; the M0 confirmations (section 4) |
| WS-world | The hand entry format and its quantization; the player's root object as the rig |
| WS-state | Sampling `hand_at` once per simulation frame; the own-hand and held-object flags in the frame view |
| WS-app | The XR thread, the mode switch, and the settings of section 10.3 |

## 9. Counters

pe-xr reports, per second and as distributions over a run:
- display frames, frames rendered, frames with `shouldRender` false, and missed frames (a predicted display time more
  than 1.5 periods after the previous one);
- `xrWaitFrame` time, XR-thread CPU time, pe-render's CPU and GPU time, the render scale;
- the age of the frame view drawn at its display time;
- for each simulation frame's hand: the sample's age, and each extrapolation's length;
- hand tracking lost, regained, and releases forced by the grace period;
- the display rate the runtime gave, and session state changes.

## 10. Tests

### 10.1 Without a runtime, on the gate

pe-xr's logic is written against a small backend trait, so almost all of it runs without OpenXR [I]:
- **A fake runtime** scripts display periods, poses, hand joints, tracking loss, focus changes, recentring, session
  loss and instance loss. The real backend wraps `openxr` and is thin.
- **The session:** every state and transition in section 3.1, including loss and re-creation. The simulation keeps
  stepping through all of them.
- **The frame loop:** wait, begin and end are called in order every frame; with `shouldRender` false no layer is
  submitted; nothing on the XR thread takes a lock that the simulation holds.
- **Rates:** displays at 72, 90 and 120 Hz against simulations at 60 and 90 Hz. Each display frame gets the right
  interpolation time, and the simulation's frame count is unchanged by the display rate.
- **Hands:** on scripted and later recorded joint traces, `closed` never chatters; a lost hand that holds is released
  after the grace period; a recentre bumps `discontinuity`; `hand_at` interpolates between samples and never
  extrapolates more than one display period.
- **Fallback:** with no runtime registered, or a manifest that points nowhere, VR is reported unavailable, and
  nothing panics. The Rust loader's manifest parsing is tested on fixture files.
- **The audit:** pe-xr adds no native build code (the gate's audit).

### 10.2 Against a simulated runtime (tier 2, if it works)

Monado on a hosted Linux runner, headless (`XR_MND_headless`) for the input paths, and with its null compositor and a
software Vulkan device (lavapipe) for the frame loop [I; untried. If it does not run on hosted runners, this tier
stays empty and the fake runtime carries the load].

### 10.3 Never against the real headset by accident

The user may be in VR on this PC. So:
- **No test loads an OpenXR runtime** unless `PE_XR_REAL=1` is set. Those tests are `#[ignore]` as well.
- **The application probes for a runtime only when configured to** (section 3.3).
- **I run nothing against the real runtime without @coordinator-opus's go**, each time.

### 10.4 On the headset, by hand (with the coordinator's go; recorded)

From plan M1:
- the frame time at 90 Hz with 500 balls and one remote hand at 60 ms one-way delay;
- dropped frames, and judder under rollback;
- the hand feel: D18b's two modes, and section 5.6's two ways to draw the own hand;
- the grasp thresholds and the tracking-loss grace period.

## 11. Open points

For @coordinator-opus unless marked; my proposal first.

| # | Point | My proposal |
|---|---|---|
| X1 | Where the own hands are drawn in VR | At the late-latched tracked pose, with a held object at the simulation's grip offset (section 5.6), and "draw where simulated" as a setting. The user judges both at the headset test. The test's wording ("no correction offset", section 5.6) is agreed with @engine-architect |
| X2 | Controllers in M1 | Yes, alongside tracked hands (section 5.4) |
| X3 | A lost hand that holds something | Release after 250 ms (section 5.5) |
| X4 | Who quantizes and formats hand entries | pe-world, from pe-xr's rig-frame samples (section 5.2) |
| X5 (user) | Which runtime, and which headset, the headset tests use | The active one, Virtual Desktop's, if the Vulkan path holds there; otherwise Meta's. The headset model decides the refresh rates offered |
| X6 | The graphics paths | D3D12 on Windows and Vulkan on Linux, both behind one interface, following the M0 check (section 4.4) |
| X7 | API layers | None in M1: the Rust loader loads no layers (section 2). Revisit if a runtime needs its implicit layer |

## 12. Size and order

About 5-7 engineer-weeks of plan M1's 8-12 for "the renderer and VR with hands" [I]:

| Step | Size | Needs |
|---|---|---|
| The backend trait, the fake runtime, the session state machine | 1 | S5 v0 |
| The Rust loader; D3D12 and Vulkan through OpenXR into wgpu; swapchains; the frame loop | 2 | The M0 check; `GpuContext` in pe-render |
| Hands: tracked, controllers, grip and grasp, the ring, `hand_at` | 1.5 | S5 v0 |
| Pacing, refresh rate, counters, render scale | 1 | pe-render's frame times |
| Headset tests and tuning | 0.5-1 | The coordinator's go |
| The Monado tier-2 job | 0.5 | WS-ci |
