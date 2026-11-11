# pe-xr: the OpenXR session, stereo, tracked hands and frame pacing

Status: draft 7, 2026-11-11, by @xr-eng (WS-xr). Draft 2 answered @shader-reviewer's review of draft 1 (M1-M5,
L1-L4); draft 3 folds in @ballpit-viz's M0 report (cf7a3bb, branch `m0/feasibility-xr`); draft 4 answers the re-review of
draft 3 (image states at release; API version; multiview on the desktop context); draft 5 folds in @ballpit-viz's
headset run (Quest 3 through Virtual Desktop, b35c448); draft 6 applies the user's rulings on rates (pace to
whatever the runtime gives; a simulation rate fixed apart from the display, 60 by default and configurable per world;
limits in seconds) and the coordinator's ruling of Vulkan first for VR on Windows; draft 7 answers @shader-reviewer's
review of the first code (error policy in 3.1, the tracked hand's pose in 5.1, the ring as built in 6.2, sRGB
formats in 4.2), and the coordinator's conditions on its three choices (me761033e3c05). The session, the hands, the ring and the real backend's session level are built (`crates/pe-xr`);
the graphics binding is not. It specifies M1's VR work in
`docs/refound-plan.md` (branch `docs/refound-plan`, b610464), section 3.1: "the OpenXR session, stereo through
pe-render, tracked hands into hand entries, 90 Hz pacing"; the user has since ruled that pacing follows the
runtime's rate (section 7.2). Seam S5 is proposed in section 8. @engine-architect (WS-seams)
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
- **The user's requirements:** full VR (R10); a fixed timestep independent of the frame rate (R2), restated on
  2026-11-10: "the simulation rate should be fixed outside of the rendered framerate" (m8ac62c1872ac), "60 default,
  fully configurable in a range per world" (md4b42d16d9df), and for the display "work with whatever the runtime gives
  actually" (m0e225386a052), which replaces D4's 90 Hz target for VR; 100% Rust, with the OpenXR runtime loaded at run time as an operating-system boundary, by a loader of
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
     are out of that range is refused, with the reason. The instance then asks for the negotiated API version,
     capped at 1.1, because a 1.0-only runtime (an older SteamVR or Monado) refuses an instance that asks for 1.1;
  4. build the `openxr::Entry` from the returned `xrGetInstanceProcAddr` (`Entry::from_get_instance_proc_addr`).
     The loader's `Runtime` owns the opened `Library`, and is kept alive by every instance and every GPU context made
     through it, so the library is never unloaded while the runtime's code may still be called (section 4.1).

  Manifest discovery is injected, so tests run the loader on fixture manifests and never read the registry or
  `XR_RUNTIME_JSON` (section 10.3). Opening a runtime's library already runs its code, so the loader is also where
  the headset guard sits (section 10.3).

  The cost: no API layers. Virtual Desktop registers one implicit layer (an Oculus compatibility layer), which we do
  without [M0]. The runtime is then the only boundary, and it goes in the register of boundaries (plan C6).
- **Graphics path in M1:** Vulkan through `XR_KHR_vulkan_enable2` on Windows and Linux; D3D12 through
  `XR_KHR_D3D12_enable` as Windows' fallback (@coordinator-opus's ruling on render.md, mccc7e65b662f; section 4.4).
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
| `XR_KHR_vulkan_enable2` | The runtime creates the VkInstance and VkDevice that wgpu then wraps | On Linux, no VR; on Windows, the D3D12 fallback |
| `XR_KHR_D3D12_enable` (Windows) | The fallback: the session draws into D3D12 swapchains from wgpu's D3D12 device, on the adapter the runtime names | Desktop mode |
| `XR_KHR_win32_convert_performance_counter_time` (Windows), `XR_KHR_convert_timespec_time` (Linux) | Converting `XrTime` to the engine's host clock (section 6.1) | No VR: without it the time base cannot be shared [I: every PC runtime I know offers it] |
| `XR_EXT_hand_tracking` | Tracked hands: 26 joints a hand | Controllers only |
| `XR_EXT_hand_interaction` | A runtime-computed grip and pinch for tracked hands, through the action system | pe-xr computes grip from the joints (section 5.3) |
| `XR_FB_display_refresh_rate` | Read the current rate, and report changes. pe-xr requests no rate (section 7.2) | The rate is read from the predicted display period |
| `XR_EXT_local_floor` (core in OpenXR 1.1; the extension on a 1.0 runtime) | A floor-level space when STAGE is not offered | LOCAL, with the floor estimated from the head's height at start |
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
  change pending, and interaction-profile changes. Lost events (the runtime's queue overflowed) are counted.
- **How the session ends** is told to its caller as one of four reasons: EXITING (by choice, or the runtime's);
  **session loss** (LOSS_PENDING or `XR_ERROR_SESSION_LOST`: the instance is still good, so a new session may be tried
  on it after `xrGetSystem` finds the headset again); **instance loss** (`InstanceLossPending` or
  `XR_ERROR_INSTANCE_LOST`: drop the instance and load the runtime again); or **frames failing** (below).
- **Errors short of a loss never end VR on their own.** An error from any call inside a frame (`xrWaitFrame`,
  `xrBeginFrame`, `xrSyncActions`, the hand and view locates, the image calls, `xrEndFrame`) closes that frame
  cleanly: the image is released if acquired, and the frame is ended with no layers if it was begun, but `xrEndFrame`
  is never called twice for one frame, even when it was the call that failed. The error is counted and the next
  frame runs. The session ends only when no frame has succeeded for one second (`FRAME_ERROR_LIMIT`), counted from
  the last successful frame in host time, so the limit holds at any display rate [I: one second is long enough to
  ride out a runtime's hiccup, short enough that a broken session does not leave the user in a frozen headset].
  Good and bad frames alternating therefore never end VR; that case is reported as **degraded** (a run of failures
  beginning within a second of an earlier one is counted, and the session says it is degraded until a second passes
  without an error). Session and instance loss still end VR at once. [R coordinator me761033e3c05, after
  @fable-reviewer: the 1 s limit, counted from the last good frame, with the degraded report.]
- **Ending VR never stops the world.** Whatever the reason, the world session keeps running and the desktop window
  draws it (section 3.3); the simulation thread never waits on a failing frame, since it only reads the ring
  (section 6.2).
- **An invalid head pose skips the frame.** When `xrLocateViews` does not report both ORIENTATION_VALID and
  POSITION_VALID (a moment of lost head tracking), the poses are unspecified, and a projection layer built from them
  can be refused (`XR_ERROR_POSE_INVALID`). That frame is not drawn: the image is released and the frame ends with
  no layers, which shows the runtime's own fallback; the frame is counted (section 9).
- **Drawing needs VISIBLE or FOCUSED** as well as `shouldRender`; SYNCHRONIZED frames are paced and ended with no
  layers.

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
- **One backend per process, and on Windows it is Vulkan** (the window alone picks Vulkan here anyway [V: M0
  report]). On the Vulkan path the runtime creates the VkInstance and VkDevice, so a context pe-render made by itself
  cannot be reused for VR:
  - **when VR is configured at start,** pe-xr creates the context through the runtime from the start, and the desktop
    window draws on it. Entering and leaving VR then re-create nothing;
  - **when VR is entered from a context pe-render made,** the context is re-created through the runtime before the
    session exists;
  - **leaving VR keeps the context.** The instance and device that `xrCreateVulkanInstanceKHR` and
    `xrCreateVulkanDeviceKHR` return are the application's to destroy [I: my reading of `XR_KHR_vulkan_enable2`; the
    code checks it], so they outlive the session and are destroyed only with the context;
  - after instance loss, the next session needs `xrGetVulkanGraphicsDevice2KHR`'s device again; if it differs, the
    context is re-created;
  - **a context made through the runtime keeps the runtime's library loaded until the context is destroyed,** even
    after `xrDestroyInstance`, because the runtime may have hooked the Vulkan dispatch. The loader's `Runtime` owns
    the `Library`, and the context holds a reference to that `Runtime` (section 2).
- **On the D3D12 fallback,** wgpu creates the device itself, so leaving VR re-creates nothing. Entering VR keeps a DX12
  context only when the fallback was chosen at start (the desktop is otherwise Vulkan) and its adapter's LUID is the
  runtime's; otherwise it re-creates the context.
- The context is created with `Features::MULTIVIEW` whenever the adapter offers it, because VR keeps it.

**Vulkan, on Windows and Linux** [M0; the names are wgpu 30's, V: in the cargo cache]:
0. Call `xrGetVulkanGraphicsRequirements2KHR` (`graphics_requirements::<Vulkan>`) first: the session fails with
   `XR_ERROR_GRAPHICS_REQUIREMENTS_CALL_MISSING` without it. Virtual Desktop's runtime asks for Vulkan 1.1 to 2.0 [V:
   M0 headset run].
1. Ask wgpu-hal's Vulkan backend for the instance extensions it wants (`Instance::desired_extensions`), and create
   the VkInstance with `xrCreateVulkanInstanceKHR` (`openxr::Instance::create_vulkan_instance`).
2. Wrap it with wgpu-hal's `Instance::from_raw`, and then `wgpu::Instance::from_hal`.
3. Ask the runtime for its physical device (`vulkan_graphics_device`) and expose it as a wgpu-hal adapter
   (`expose_adapter`).
4. Create the VkDevice with `xrCreateVulkanDeviceKHR` (`create_vulkan_device`), enabling the adapter's
   `required_device_extensions` for the features pe-render asks for, multiview among them.
5. Wrap it with `device_from_raw`, then `wgpu::Adapter::create_device_from_hal`. Ask for the limits pe-render
   needs (`Limits::default()` or a stated set), never the adapter's full limits: wgpu-core asserts
   `max_buffer_size <= u32::MAX`, which the full limits break [V: M0 headset run, b35c448].
6. Create the session with the Vulkan binding: the instance, physical device, device, queue family and
   queue index that wgpu uses.

**D3D12, Windows' fallback** [M0: @ballpit-viz's first path]:
1. Ask the runtime for its graphics requirements (`XR_KHR_D3D12_enable`): the adapter's LUID and the minimum
   feature level.
2. Create wgpu's D3D12 instance as usual, and pick the adapter whose LUID matches. wgpu's `AdapterInfo` carries no
   LUID, so each adapter's is read through `as_hal::<Dx12>()`, `raw_adapter()` and `GetDesc1().AdapterLuid`.
3. Create the device with the features pe-render asks for.
4. Create the session with the D3D12 binding: the raw `ID3D12Device` and command queue, taken from wgpu through
   `as_hal`.
5. Shaders compile with FXC, which has no view instancing, so the fallback draws one pass per eye (render.md's (a);
   no DXC is shipped, since a native Microsoft binary is beyond the boundaries D1 allows).

### 4.2 Swapchains

- **Colour:** one swapchain with two array layers, one per eye. With `Features::MULTIVIEW` [V: the feature exists
  in wgpu-types 30, on Vulkan and on D3D12 with view instancing], both layers are drawn in one pass; without it, one
  pass per layer, as the M0 check does now. On Vulkan, multiview is the plan; on the D3D12 fallback, one pass per
  layer (FXC). At 120 Hz the two passes must fit the same per-period budget (section 7.3).
- **Format:** only an sRGB format, whatever order the runtime lists them in: RGBA8 sRGB first, then BGRA8 sRGB
  (Vulkan `R8G8B8A8_SRGB`, `B8G8R8A8_SRGB`; DXGI `R8G8B8A8_UNORM_SRGB`, `B8G8R8A8_UNORM_SRGB`). pe-render writes
  linear values and relies on the target's sRGB encoding, so a UNORM format would show the headset image too dark.
  A runtime offering neither fails session creation with that reason.
- **Size:** the larger of the two eyes' recommended sizes (one swapchain serves both), times a render scale pe-render may lower under load (section 7).
- **Depth:** a second swapchain in a depth format, submitted with `XR_KHR_composition_layer_depth` when offered.
- **Wrapping:** each swapchain image is wrapped once, at creation, with wgpu-hal's `texture_from_raw` and
  `create_texture_from_hal`, and the textures are kept for the swapchain's life [M0]. pe-xr acquires, waits for and
  releases images; pe-render only draws into the view it is handed.
  - D3D12: the initial state given to wgpu is COLOR_TARGET, because OpenXR hands D3D12 colour images over in
    RENDER_TARGET state and wants them back in it [M0; the headset run checks it].
  - Vulkan: wrapped with `TextureMemory::External` and an empty drop callback, so the runtime keeps ownership [M0].
- **The image's state, at wrap and at every release.** OpenXR hands each image over in one layout and wants it back
  in the same one at `xrReleaseSwapchainImage`: colour in RENDER_TARGET (D3D12) or COLOR_ATTACHMENT_OPTIMAL (Vulkan),
  depth in DEPTH_WRITE (D3D12) or DEPTH_STENCIL_ATTACHMENT_OPTIMAL (Vulkan). wgpu leaves a texture in its last use.
  So:
  - every image is wrapped in wgpu's matching state on both backends: COLOR_TARGET for colour, DEPTH_STENCIL_WRITE
    for depth;
  - each frame's work ends with the image back in that state, by `CommandEncoder::transition_resources` (in wgpu 30)
    after any other use. The desktop mirror (section 3.3) copies from the eye image within that frame's work, so the
    transition comes after the copy; it never samples the swapchain image later;
  - the frame's work is submitted to the queue before the image is released (section 7.1, step 8).

### 4.3 Losing the device

After `xrDestroySession` the GPU context may go away (section 4.1 says when it does).
pe-render must then drop every GPU resource and rebuild from the frame view on the next context. Its meshes and materials are derived from world data, so
nothing is lost but time [I].

### 4.4 Which path, on which runtime

- **Vulkan first, D3D12 as the fallback** (@coordinator-opus, mccc7e65b662f, on render.md's review). Vulkan keeps
  the build 100% Rust with only system boundaries (the driver), and gives multiview without a shipped shader
  compiler. It ran on the user's headset [V: M0 headset run]. D3D12 on wgpu needs DXC for multiview, which would
  mean shipping a native Microsoft binary; with FXC it draws two passes.
- **One thing to watch:** in the M0 run's 8 s, Vulkan repeated 22 frames and D3D12 9 [V: b35c448]. Virtual Desktop's
  runtime draws through Direct3D underneath, so its Vulkan path is interop [I]. Eight seconds is too short to say
  more; the first M1 headset run measures repeated frames on both paths over a longer stretch, and a lasting gap goes
  to the coordinator.
- **pe-xr keeps both paths behind one interface:** creating the context, the session binding, and wrapping swapchain
  images are the only graphics-specific code, and pe-render sees only wgpu types either way.
- **Linux needs Vulkan** too, so one path serves both. Whether Linux can be tested
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
  `f32`. For a controller it is the grip action space's pose. For a tracked hand it is the grip action space's pose
  too when `XR_EXT_hand_interaction` binds the grip pose to the tracked hand (the joints then give only the curl);
  without that extension it is the PALM joint's pose, the joint nearest the grip point. The palm and the grip pose
  differ a little in origin and axes [I: by centimetres]; a change of source bumps `discontinuity`, so the switch
  itself never reads as motion, but an object held by a tracked hand on a runtime without the extension sits a
  little differently than one held by a controller. That offset is accepted for M1 [R coordinator me761033e3c05,
  after @fable-reviewer]: it is a known difference, recorded, and its exit is the headset test (section 10.4), which
  measures the gap. pe-xr reports which pose path is in use (`PosePath`), and the application logs it at start-up.
- `linear_velocity`, `angular_velocity`: in the rig frame, when the runtime gives them (the grip action space gives
  them; the joints, as pe-xr reads them, do not).
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

**On the user's setup, controllers are the only hands for now** [V: M0 headset run, b35c448]. Virtual Desktop's
runtime offers `XR_EXT_hand_tracking`, but reported the Quest 3 system as not supporting it, because hand tracking
was off in Virtual Desktop (the user, through @coordinator-opus, m26c107e13707: "It's off"). The user also saw the
test's pile of bodies and controller spheres in the headset. Tracked hands at the headset test need the user to
enable hand tracking first (section 11, X9).

### 5.5 When tracking is lost

- **While not holding:** the sample goes `tracked = false`. The simulation side applies its world rule for an
  entry-less hand (D52: coast for at most 6 frames, then hold) [V design.md 6.10]. Under the user's rule that every
  limit is in seconds (m0dbd561302e0), WS-world restates it as a duration (6 frames at 60 Hz is 100 ms).
- **While holding:** after a grace period of 250 ms without tracking, pe-xr sets `closed = false`, so the object is
  released deliberately rather than held by a frozen hand [I]. The grace period is a setting.
- **When tracking returns,** `discontinuity` is bumped.
- **When focus is lost** (the headset is taken off, a system menu opens), both hands release at once.

### 5.6 Drawing the local player's own hands

The display layer (file 14, 4.1) never blends a player's own hands or what they hold. In VR there is one more choice:
where to draw them.
- **The simulated hand** trails the display time by the display lead plus up to one simulation frame (17 ms at the
  default 60 Hz). The lead measured on Virtual Desktop's runtime is 38-63 ms (p50 to p95, D3D12; max 68 ms), about five display
  periods at 120 Hz [V: M0 headset run, b35c448]. So the simulated hand trails the display time by about 40-80 ms, and
  at 2 m/s that is 8 to 16 cm. Late-latching matters more than I first estimated.
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
  waits on. As built, it is an `arc-swap` of immutable snapshots: each push builds a new snapshot of both hands'
  samples and publishes it with one atomic swap, and a reader loads the current one. arc-swap's readers are lock-free
  (usually wait-free) and its writers lock-free [V: arc-swap 1.9's documentation, checked by @shader-reviewer]. The
  allocator is the one shared part that is not lock-free: two snapshots per display frame are allocated on the XR
  thread, and the last reference to an old one may be freed on the simulation thread [I: small, at about 240
  allocations a second]. A fixed-size ring with a sequence counter would remove it. [R coordinator me761033e3c05,
  after @fable-reviewer:] the ring stays behind its own small type (`HandRing`, `ring.rs`), so a swap touches one
  file. **The switch-over threshold** is any sample older than one display period. It is judged by the tail: the ring
  records every read's sample age (how far the asked time is past the newest sample) in a lock-free histogram with
  its exact maximum, and a run of several minutes with a build running alongside reports the max and p99.9 age with
  the frame time. That run belongs to the headset test (section 10.4).

## 7. The frame loop and pacing

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
8. Submit the frame's work, its last command returning the image to its state (section 4.2); then release the image,
   and `xrEndFrame` with one projection layer (and its depth, when offered).

When `shouldRender` is false, steps 4 to 7 are skipped, and `xrEndFrame` submits no layers. The loop never skips
`xrWaitFrame`, `xrBeginFrame` and `xrEndFrame`.

### 7.2 Rates

- **Display rate: whatever the runtime gives.** The user's ruling, verbatim: "work with whatever the runtime gives
  actually" (m0e225386a052). pe-xr has no target rate and requests none. It paces to the predicted display period
  `xrWaitFrame` returns each frame, takes a change of period as it comes, and reports the rate it got. Every budget is
  per period (section 7.3). Virtual Desktop's runtime ran the Quest 3 at 120 Hz, a constant 8.333 ms period [V: M0
  headset run]; the rate is the user's to set in Virtual Desktop.
- **Simulation rate:** fixed apart from the display. The user: "the simulation rate should be fixed outside of the
  rendered framerate" (m8ac62c1872ac), and "60 default, fully configurable in a range per world" (md4b42d16d9df). The
  range's bounds are being proposed by @engine-architect and @vdw-physics. pe-xr does not depend on the rate, and
  nothing in pe-xr counts simulation frames: its limits are durations.
- **The drawn time.** The newest frame view is for a simulation frame at or before now, and the predicted display
  time is well past now: 38-63 ms on Virtual Desktop's runtime (section 5.6), about five periods at 120 Hz. So the display time is almost always past the newest frame view,
  and interpolating between frame views cannot reach it. The time the world is drawn at is WS-render's choice, one
  of:
  - **(a) extrapolate** from the newest frame view to the display time, up to the measured lead, with the display
    layer's smoothing. This is design.md 18.3's rule, and my proposal [I];
  - **(b) interpolate** between the two newest frame views at the display time minus a render delay. Nothing is
    drawn from a guess, but everything except the head and the own hands is shown that much later.
  - At the measured lead, (a) extrapolates other objects by 40-60 ms, and (b) shows the world 40-60 ms or more late.
    Both are larger than design.md 18.3 assumed [I]; the headset test judges them.
- **Counted either way:** the gap between the drawn time and the display time, and the lead from the newest frame
  view's `sim_time` to the display time (section 9).
- **So any display rate** works against any simulation rate in the world's range, as M1's rendering test asks. The test
  checks the rule WS-render chose: each display frame's drawn time follows from its display time and the newest
  frame view by that rule.

### 7.3 The budget, per display period

The budget is a share of whatever period the runtime reports (P). At 120 Hz, as observed, P is 8.3 ms; at 90 Hz,
11.1 ms; at 72 Hz, 13.9 ms. My budget [I], to be measured:

| Part | Budget | At 120 Hz | Note |
|---|---|---|---|
| XR thread CPU: steps 0-8, without drawing | 0.1 P | 0.8 ms | Events, locating, acquiring, submitting |
| pe-render CPU: recording the frame | 0.3 P | 2.5 ms | 500 balls, two eyes: one multiview pass on Vulkan, two passes on the D3D12 fallback |
| GPU frame time | 0.7 P | 5.8 ms | Leaves room for the runtime's compositor and, with Virtual Desktop, its video encoder. The fallback's second pass comes out of the same share |
| Simulation thread | its own core | — | Never on the XR thread, and never paced by it. Rollback replays already cost 2.5 ms a frame at 90 Hz with 500 balls and one remote hand (file 02) |

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
- the display rate the runtime gave, and session state changes;
- frames skipped for an invalid head pose, frames closed after an error short of a loss, degraded runs, and lost
  events;
- the hand sample's age at each read: its maximum and p99.9 (section 6.2).

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
- **Rates:** displays at 72, 90 and 120 Hz, and a period that changes mid-run, against simulations across the
  world's range (60 by default), with display leads of one to six periods (Virtual Desktop's measured lead is about
  five). Every pe-xr limit is checked as a duration at two simulation rates. Each display frame's drawn time follows WS-render's rule (section 7.2), and the simulation's frame count is
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
- the frame time per period, at the rate Virtual Desktop gives, with 500 balls and one remote hand at 60 ms one-way
  delay, on Vulkan and on the D3D12 fallback;
- repeated frames on both paths over a longer stretch than M0's 8 s (section 4.4);
- dropped frames, and judder under rollback;
- the hand feel: D18b's two modes, and section 5.6's two ways to draw the own hand;
- the hold transitions: a throw leaving the hand, and the snap at a grasp (section 5.6);
- the grasp thresholds and the tracking-loss grace period;
- the palm-joint gap: where it applies (a runtime without `XR_EXT_hand_interaction`), the offset between the palm
  joint and the grip pose, measured; it decides whether a fixed offset is applied (section 5.1);
- the hand ring's tail: several minutes with a build running alongside, recording the max and p99.9 sample age and
  the frame time; any sample older than one display period means the fixed ring replaces arc-swap (section 6.2).

## 11. Open points

For @coordinator-opus unless marked; my proposal first. @coordinator-opus ruled X1, X2 and X3 as proposed on
2026-11-10 (m92c4584b835e): the 250 ms of X3 is a parameter. X5 is with the user, through the liaison; until it is
answered, pe-xr is coded against the fake runtime. The user answered X5 (m2aaafe98cbbe): Virtual Desktop. The user
answered X9 (m981c546957f2, m0e225386a052): hand tracking will be enabled before the M1 headset tests, and the display
rate is whatever the runtime gives. @coordinator-opus ruled X6 (mccc7e65b662f): Vulkan first.

| # | Point | My proposal |
|---|---|---|
| X1 | Where the own hands are drawn in VR | At the late-latched tracked pose, with a held object at the simulation's grip offset (section 5.6), and "draw where simulated" as a setting. The user judges both at the headset test. The test's wording ("no correction offset", section 5.6) is agreed with @engine-architect |
| X2 | Controllers in M1 | Yes, alongside tracked hands (section 5.4) |
| X3 | A lost hand that holds something | Release after 250 ms (section 5.5) |
| X4 | Who quantizes and formats hand entries | pe-world, from pe-xr's rig-frame samples (section 5.2) |
| X5 (user) | Which runtime the headset tests use | Answered by the user: Virtual Desktop's. It is the only usable runtime here; any other would have to be installed, which is the user's call (section 4.4) |
| X6 | The graphics paths | Ruled: Vulkan on Windows and Linux; D3D12 with FXC and two passes as Windows' fallback; both behind one interface (section 4.4) |
| X7 | API layers | None in M1: the Rust loader loads no layers (section 2). Revisit if a runtime needs its implicit layer |
| X8 (WS-render) | The drawn time: extrapolate to the display time, or interpolate at a delay | (a), extrapolate (section 7.2) |
| X9 (user) | Tracked hands and the display rate on Virtual Desktop | Answered: the user enables hand tracking before the M1 headset tests; the rate is whatever the runtime gives (sections 5.4, 7.2) |

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
