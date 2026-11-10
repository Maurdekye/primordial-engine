# pe-xr: the OpenXR session, stereo, tracked hands and 90 Hz pacing

Status: draft 3, 2026-11-10, by @xr-eng (WS-xr). Draft 2 answered @shader-reviewer's review of draft 1 (M1-M5,
L1-L4); draft 3 folds in @ballpit-viz's M0 report (cf7a3bb, branch `m0/feasibility-xr`). Nothing in it is built. It specifies M1's VR work in
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
  opens it with `libloading`]. The native-code audit flags `openxr-sys`, which declares `links = "openxr_loader"` and
  carries the SDK's C and C++ sources; with no feature its build script compiles nothing and the binary imports no
  loader [V: @ballpit-viz's M0 report, cf7a3bb, `imports-windows.txt`]. Its register entry is conditional: "openxr
  without `linked` or `static`".
- **There is no system loader to open on Windows.** Khronos' loader is a C library each application ships; this PC
  has no `openxr_loader.dll` in System32 [V: @ballpit-viz's M0 finding, and my listing of System32]. So pe-xr has its
  own small loader in Rust, the one @ballpit-viz is writing for the M0 check:
  1. find the active runtime's manifest: `XR_RUNTIME_JSON` if set, else `ActiveRuntime` under
     `HKLM\SOFTWARE\Khronos\OpenXR\1` on Windows, else `active_runtime.json` under the XDG config paths on Linux.
     The manifest file must exist; a registry entry is never trusted on its own (this PC lists a runtime whose
     manifest is gone, below);
  2. resolve `runtime.library_path` against the manifest's own folder when it is relative (Virtual Desktop's is
     `.\virtualdesktop-openxr.dll` [V]), and open it with `libloading`;
  3. look up the negotiation entry point, under the name the manifest's optional `functions` map gives it, else
     `xrNegotiateLoaderRuntimeInterface`, and call it with the loader-info and runtime-request structs filled in,
     their struct versions included. The range offered is loader-runtime interface 1, and any API version 1.x
     (maximum 1.0x3ff.0xfff, as Khronos' loader offers): Virtual Desktop's runtime refuses a maximum of openxr-sys's
     own 1.1.58, because it is built against 1.1.62 [V: M0 report, section 2]. A runtime whose negotiated versions
     are out of that range is refused, with the reason;
  4. build the `openxr::Entry` from the returned `xrGetInstanceProcAddr` (`Entry::from_get_instance_proc_addr`).

  Manifest discovery is injected, so tests run the loader on fixture manifests and never read the registry or
  `XR_RUNTIME_JSON` (section 10.3). Opening a runtime's library already runs its code, so the loader is also where
  the headset guard sits (section 10.3).

  The cost: no API layers. Virtual Desktop registers one implicit layer (an Oculus compatibility layer), which we do
  without [M0]. The runtime is then the only boundary, and it goes in the register of boundaries (plan C6).
- **Graphics path in M1,** following the M0 check (section 4.4): D3D12 through `XR_KHR_D3D12_enable` on Windows, and
  Vulkan through `XR_KHR_vulkan_enable2` on Linux and as Windows' alternative.
- **The runtime: Virtual Desktop's.** The user chose it for the headset tests (X5, m2aaafe98cbbe). It is the active
  runtime here, and the only usable one [V: `HKLM\SOFTWARE\Khronos\OpenXR\1` and the file system, read only, no
  session started]. `AvailableRuntimes` also lists Meta's `oculus_openxr_64.json`, but neither that file nor its
  folder exists: the entry is stale (@shader-reviewer's finding, which I re-checked). No SteamVR is installed. No other
  runtime is installed unless the user asks.
- **Linux:** SteamVR and Monado, through the same Vulkan path [I; not tried here].
- **No runtime registered, no runtime library, or no headset** is not an error. The application runs in desktop
  mode, says why once, and offers to retry (section 3.3). "No headset" includes a runtime that is up while its
  headset is not connected: `xrGetSystem` then fails with `XR_ERROR_FORM_FACTOR_UNAVAILABLE`, as it did here [V: M0
  report]. That is desktop mode and a later retry, never an error.

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
| STOPPING | `xrEndSession`, then IDLE. The session and the GPU context are kept; the desktop window draws on the same device. The runtime may send READY again on the same session (some do when the headset sleeps and wakes) | Absent | Runs |
| LOSS_PENDING | Destroy the swapchains, then the session, then the GPU context; try again later | Absent | Runs |
| EXITING | The same teardown; desktop mode on a context of pe-render's | Absent | Runs |

- **The simulation never stops for VR.** Every row says "runs".
- **Hands become absent** whenever input is not focused. Section 5.5 says what an absent hand does to a grab.
- **Instance loss** (`XR_ERROR_INSTANCE_LOST`, or a runtime restart) is handled as session loss plus a new instance.
  The GPU objects made on the runtime's device go with it, so pe-render must be able to drop and re-create
  its GPU resources (section 4.3).
- **Leaving VR by choice** is `xrRequestExitSession`, then STOPPING, IDLE and EXITING, with the teardown above.
- **Events** drive every row. pe-xr drains `xrPollEvent` once per frame while the frame loop runs (section 7.1), and
  about every 100 ms while it does not (IDLE before READY, and after STOPPING), on the XR thread [I: OpenXR has no
  blocking wait for events]. The events it handles: session state changes, instance loss pending, reference-space
  change pending, and interaction-profile changes.

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
  still a call into the runtime, and on some runtimes `xrCreateInstance` starts the runtime's companion application,
  so it is off unless configured (section 10.3).
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
- **pe-render accepts a context from outside** in either case.
- **The context outlives the session.** It is re-created only after `xrDestroySession` (section 3.1), never while a
  session holds its device and queue. The order of teardown is swapchains, then session, then context.
- **On D3D12, leaving VR re-creates nothing:** wgpu creates the device itself, so the desktop keeps the same context,
  unless it wants a different adapter. On Vulkan the runtime created the device, so leaving VR re-creates it.
- **One backend per process.** A wgpu window alone picks Vulkan on this PC [V: M0 report], but the D3D12 path needs
  DX12 on the runtime's adapter. So on Windows pe-render's desktop context uses DX12 too, and entering VR keeps it
  when its adapter's LUID is the runtime's. Otherwise entering VR re-creates the context, before the session exists.

**D3D12, on Windows** [M0: @ballpit-viz's first path]:
1. Ask the runtime for its graphics requirements (`XR_KHR_D3D12_enable`): the adapter's LUID and the minimum
   feature level.
2. Create wgpu's D3D12 instance as usual, and pick the adapter whose LUID matches. wgpu's `AdapterInfo` carries no
   LUID, so each adapter's is read through `as_hal::<Dx12>()`, `raw_adapter()` and `GetDesc1().AdapterLuid`.
3. Create the device with the features pe-render asks for.
4. Create the session with the D3D12 binding: the raw `ID3D12Device` and command queue, taken from wgpu through
   `as_hal`.

**Vulkan, on Linux and as Windows' alternative** [M0; the names are wgpu 30's, V: in the cargo cache]:
0. Call `xrGetVulkanGraphicsRequirements2KHR` (`graphics_requirements::<Vulkan>`) first: the session fails with
   `XR_ERROR_GRAPHICS_REQUIREMENTS_CALL_MISSING` without it.
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

- **Colour:** one swapchain with two array layers, one per eye. With `Features::MULTIVIEW` [V: the feature exists
  in wgpu-types 30, on Vulkan and on D3D12 with view instancing], both layers are drawn in one pass; without it, one
  pass per layer, as the M0 check does now.
- **Format:** the first sRGB format the runtime offers that pe-render can target (RGBA8 or BGRA8 sRGB).
- **Size:** the runtime's recommended size per eye, times a render scale pe-render may lower under load (section 7).
- **Depth:** a second swapchain in a depth format, submitted with `XR_KHR_composition_layer_depth` when offered.
- **Wrapping:** each swapchain image is wrapped once, at creation, with wgpu-hal's `texture_from_raw` and
  `create_texture_from_hal`, and the textures are kept for the swapchain's life [M0]. pe-xr acquires, waits for and
  releases images; pe-render only draws into the view it is handed.
  - D3D12: the initial state given to wgpu is COLOR_TARGET, because OpenXR hands D3D12 colour images over in
    RENDER_TARGET state and wants them back in it [M0; the headset run checks it].
  - Vulkan: wrapped with `TextureMemory::External` and an empty drop callback, so the runtime keeps ownership [M0].

### 4.3 Losing the device

After `xrDestroySession` (session or instance loss, or leaving VR on the Vulkan path), the GPU context may go away.
pe-render must then drop every GPU resource and rebuild from the frame view on the next context. Its meshes and materials are derived from world data, so
nothing is lost but time [I].

### 4.4 Which path, on which runtime

- Virtual Desktop's runtime, the active one here, draws through Direct3D underneath, so its Vulkan support is interop
  [I]. That is why the M0 check tries D3D12 first on Windows, and Vulkan second.
- **pe-xr keeps both paths behind one interface:** creating the context, the session binding, and wrapping swapchain
  images are the only graphics-specific code, and pe-render sees only wgpu types either way.
- **Linux needs Vulkan,** so the Vulkan path is built in M1 even if D3D12 serves Windows. Whether it can be tested
  before M1's end depends on a Linux machine with a headset, or on the Monado tier (section 10.2).
- If neither path works on Virtual Desktop's runtime, there is no other runtime here. A fallback would mean installing
  Meta's PC software or SteamVR, which is the user's call; nothing is installed unless the user asks.

## 5. Hands

### 5.1 What a hand sample is

Every display frame, while input is focused, pe-xr samples both hands and publishes one input sample (section 8.2):

- `hand`: left or right.
- `source`: a tracked hand, a controller, or (from pe-render) the desktop hand.
- `sampled_at`: the host time the pose is for.
- `tracked`: whether the pose is valid now. When false, the pose is the last valid one.
- `pose`: the grip pose in the rig frame: position in metres and a unit quaternion in OpenXR's order (x, y, z, w),
  `f32`.
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

Controllers go through the action system. The action set is attached once, at session creation
(`xrAttachSessionActionSets`), and synced every frame while FOCUSED (`xrSyncActions`, section 7.1); without the sync no
action state or action-space pose is valid, for controllers or for `XR_EXT_hand_interaction`'s grasp. One action set: a grip pose, an aim pose, a squeeze value, a trigger
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
- **The simulated hand** trails the display time by the display lead (the predicted display time is one to three
  display periods ahead of now, 11-33 ms at 90 Hz, and more on a streaming runtime such as Virtual Desktop's), plus up
  to one simulation frame (17 ms at 60 Hz): about 15-50 ms [I; the lead is measured, section 9]. At 2 m/s that is 3
  to 10 cm.
- **My proposal: draw the own hand at the tracked pose for the predicted display time** (late-latched), and draw a
  held object at that hand pose times the simulation's grip offset (the held object's pose relative to the simulated
  hand). This is design.md 18.3's rule. The simulated hand still does the pushing: other objects react to the
  simulated hand, so they may be pushed a few centimetres behind the drawn hand during fast motion.
- **The alternative:** draw the own hand where it is simulated. Nothing is ever drawn where the simulation does not
  have it, at the cost of a hand that trails the real one.
- **Both are a setting,** and the user judges them at the headset test, alongside D18b.
- **When a hold ends** (a release, a throw, a grasp lost to the grace period, or a correction saying the hold never
  happened), the object is handed to the display layer with its last drawn pose and velocity, as an object no longer
  held. From then on it is an ordinary object, which the display layer may blend toward its simulated pose. The rule
  bars blending only while an object is held. Without this, a thrown object would jump back to its simulated pose,
  10-20 cm or more at 5 m/s, just as the eye follows it. A hold the simulation undid also gets the brief cue of file
  14, 4.1.
- **When a hold starts,** the object snaps to the hand pose times the grip offset. At a grasp the hand is nearly
  still, so the snap is small [I]. If the headset test shows a visible pop, a short settle (about 50 ms) is the
  setting to try.
- **A grip offset changed by a rollback** shows as a snap, which the rule allows.
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
  for at most one display period, and otherwise returns the newest sample as it is. For a time older than the ring
  (after a stall, while the simulation catches up), it returns the oldest sample it holds, and counts it.
- **Sampling time.** Each display frame pe-xr locates the hands twice: at the predicted display time, for drawing
  (section 5.6), and at the current time, for the ring. The ring's samples are what entries are made from, so an
  entry never carries a pose predicted into the future.
- **Measured:** the age of the sample each frame used, and the size of each extrapolation (section 9).
- **The thread.** The ring is written by the XR thread and read by the simulation thread with no lock either side
  waits on: a fixed-size ring with a sequence counter [I].

## 7. The frame loop and 90 Hz pacing

### 7.1 One frame

On the XR thread, which is also the VR render thread:
0. Drain `xrPollEvent`, and act on what it returns (section 3.1).
1. `xrWaitFrame`: the runtime paces the thread and returns the predicted display time and period.
2. `xrBeginFrame`.
3. While FOCUSED, `xrSyncActions`; then locate the hands at the current time, and push the samples to the ring
   (section 6.2).
4. Take the newest published frame view (section 8): one atomic pointer swap, never a lock the simulation holds.
5. Acquire and wait for a swapchain image.
6. Locate the views (`xrLocateViews`) and the hands at the predicted display time. This is as late as possible, so
   the poses are fresh.
7. pe-render draws the frame view into the image: both eyes, the scene at the drawn time (section 7.2), the own
   hands late-latched (section 5.6).
8. Release the image, and `xrEndFrame` with one projection layer (and its depth, when offered).

When `shouldRender` is false, steps 4 to 7 are skipped, and `xrEndFrame` submits no layers. The loop never skips
`xrWaitFrame`, `xrBeginFrame` and `xrEndFrame`.

### 7.2 Rates

- **Display rate:** pe-xr asks for 90 Hz with `XR_FB_display_refresh_rate` where offered, and otherwise takes the
  runtime's rate. It reports the rate it got.
- **Simulation rate:** a world setting (D4), 60 or 90 Hz. pe-xr does not depend on it.
- **The drawn time.** The newest frame view is for a simulation frame at or before now, and the predicted display
  time is one to three display periods past now. So the display time is almost always past the newest frame view,
  and interpolating between frame views cannot reach it. The time the world is drawn at is WS-render's choice, one
  of:
  - **(a) extrapolate** from the newest frame view to the display time, up to the measured lead, with the display
    layer's smoothing. This is design.md 18.3's rule, and my proposal [I];
  - **(b) interpolate** between the two newest frame views at the display time minus a render delay. Nothing is
    drawn from a guess, but everything except the head and the own hands is shown that much later.
- **Counted either way:** the gap between the drawn time and the display time, and the lead from the newest frame
  view's `sim_time` to the display time (section 9).
- **So 72, 90 and 120 Hz displays** all work against 60 and 90 Hz simulations, as M1's rendering test asks. The test
  checks the rule WS-render chose: each display frame's drawn time follows from its display time and the newest
  frame view by that rule.

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

- **pe-render re-exports `wgpu` and `wgpu-hal`,** and pe-xr uses only those, so the two crates can never drift to
  different versions or backend features.
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
- the age of the frame view drawn at its display time; the display lead; the gap between the drawn time and the
  display time (section 7.2);
- for each simulation frame's hand: the sample's age, each extrapolation's length, and requests older than the ring;
- hand tracking lost, regained, and releases forced by the grace period;
- the display rate the runtime gave, and session state changes.

## 10. Tests

### 10.1 Without a runtime, on the gate

pe-xr's logic is written against a small backend trait, so almost all of it runs without OpenXR [I]:
- **A fake runtime** scripts display periods, poses, hand joints, tracking loss, focus changes, recentring, session
  loss and instance loss. The real backend wraps `openxr` and is thin.
- **The session:** every state and transition in section 3.1, scripted as events, including STOPPING back to READY
  on the same session, loss, and re-creation. The GPU context is never dropped while a session exists, and teardown
  runs swapchains, session, context. The simulation keeps stepping through every state.
- **The frame loop:** events are drained every frame; actions are attached once and synced before they are read;
  wait, begin and end are called in order every frame; with `shouldRender` false no layer is
  submitted; nothing on the XR thread takes a lock that the simulation holds.
- **Rates:** displays at 72, 90 and 120 Hz against simulations at 60 and 90 Hz, with display leads of one to three
  periods. Each display frame's drawn time follows WS-render's rule (section 7.2), and the simulation's frame count is
  unchanged by the display rate.
- **Hands:** on scripted and later recorded joint traces, `closed` never chatters; a lost hand that holds is released
  after the grace period; a recentre bumps `discontinuity`; `hand_at` interpolates between samples, never
  extrapolates more than one display period, and returns the oldest sample for a time older than the ring; a released
  object leaves the hand from its last drawn pose and velocity, never from its simulated pose.
- **Fallback and the loader,** on injected discovery and fixture manifests only: no runtime registered, a registered
  manifest that does not exist, a library that does not exist, a relative library path, a renamed entry point, and a
  refused negotiation. Each reports VR unavailable with its reason, and nothing panics.
- **The audit:** pe-xr adds no native build code (the gate's audit).

### 10.2 Against a simulated runtime (tier 2, if it works)

Monado on a hosted Linux runner, headless (`XR_MND_headless`) for the input paths, and with its null compositor and a
software Vulkan device (lavapipe) for the frame loop [I; untried. If it does not run on hosted runners, this tier
stays empty and the fake runtime carries the load].

### 10.3 Never against the real headset by accident

The user may be in VR on this PC. So:
- **The guard is a run-time check in the loader,** the one place a runtime's code can start: it refuses to open any
  runtime library unless `PE_XR_REAL=1` is set, or the application's configuration turns VR on, which is off by
  default. `#[ignore]` alone is not enough, since `cargo test -- --ignored` runs those tests; they are `#[ignore]` as
  well, as a second guard.
- **Tests never discover the real runtime:** discovery is injected (section 2), because a runtime is registered on
  this PC.
- **The application probes for a runtime only when configured to** (section 3.3).
- **I run nothing against the real runtime without @coordinator-opus's go**, each time.

### 10.4 On the headset, by hand (with the coordinator's go; recorded)

From plan M1:
- the frame time at 90 Hz with 500 balls and one remote hand at 60 ms one-way delay;
- dropped frames, and judder under rollback;
- the hand feel: D18b's two modes, and section 5.6's two ways to draw the own hand;
- the hold transitions: a throw leaving the hand, and the snap at a grasp (section 5.6);
- the grasp thresholds and the tracking-loss grace period.

## 11. Open points

For @coordinator-opus unless marked; my proposal first. @coordinator-opus ruled X1, X2 and X3 as proposed on
2026-11-10 (m92c4584b835e): the 250 ms of X3 is a parameter. X5 is with the user, through the liaison; until it is
answered, pe-xr is coded against the fake runtime. The user answered X5 (m2aaafe98cbbe): Virtual Desktop.

| # | Point | My proposal |
|---|---|---|
| X1 | Where the own hands are drawn in VR | At the late-latched tracked pose, with a held object at the simulation's grip offset (section 5.6), and "draw where simulated" as a setting. The user judges both at the headset test. The test's wording ("no correction offset", section 5.6) is agreed with @engine-architect |
| X2 | Controllers in M1 | Yes, alongside tracked hands (section 5.4) |
| X3 | A lost hand that holds something | Release after 250 ms (section 5.5) |
| X4 | Who quantizes and formats hand entries | pe-world, from pe-xr's rig-frame samples (section 5.2) |
| X5 (user) | Which runtime the headset tests use | Answered by the user: Virtual Desktop's. It is the only usable runtime here; any other would have to be installed, which is the user's call (section 4.4) |
| X6 | The graphics paths | D3D12 on Windows and Vulkan on Linux, both behind one interface, following the M0 check (section 4.4) |
| X7 | API layers | None in M1: the Rust loader loads no layers (section 2). Revisit if a runtime needs its implicit layer |
| X8 (WS-render) | The drawn time: extrapolate to the display time, or interpolate at a delay | (a), extrapolate (section 7.2) |

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
