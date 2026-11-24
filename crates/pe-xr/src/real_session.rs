//! The real runtime at the session level: [`RealBackend`] implements [`Backend`] over the `openxr` crate, generic
//! over its graphics type (`openxr::Vulkan` or `openxr::D3D12`). Only creating the session needs the GPU's raw
//! handles; everything here is the same for both paths (spec 4.4).
//!
//! It is never run by tests: creating it needs an [`XrInstance`], which needs the loader, which needs the headset
//! guard's permit. Its logic is the thin mapping from OpenXR calls to the trait that [`crate::session`] drives, and
//! that logic is tested against [`crate::fake`]. Ported in part from @ballpit-viz's M0 check (`frame_loop`, at
//! f8e9c776).

use openxr as xr;
use pe_render::{HandJoint, HandJoints};

use crate::backend::{Backend, EyePose, Fov, FrameTiming, ImageIndex, Layers, Located, SessionState, XrError, XrEvent};
use crate::hands::{GraspConfig, Joint, Reading, grip_from_joints, joint};
use crate::pointer::PointerReading;
use crate::real::{GraphicsPath, XrInstance};
use crate::seam::{Hand, HandSource, HostTime, RigPose};

const VIEW: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;

/// The suggested bindings (spec 5.4, 5.7): (interaction profile, squeeze value path, select value path, menu
/// button paths as (hand, path)) per hand. The grip and aim poses are bound on every profile.
#[allow(clippy::type_complexity)]
const PROFILES: &[(&str, &str, &str, &[(&str, &str)])] = &[
    // The Touch controller's right menu button is the system button, reserved for the runtime.
    ("/interaction_profiles/oculus/touch_controller", "squeeze/value", "trigger/value", &[("left", "menu/click")]),
    // The Index controller's system buttons are the runtime's; its left B button is the menu (@design-reviewer's L1).
    ("/interaction_profiles/valve/index_controller", "squeeze/value", "trigger/value", &[("left", "b/click")]),
    // The simple controller has no squeeze; its select button stands in for the grasp (0 or 1), and selects too.
    (
        "/interaction_profiles/khr/simple_controller",
        "select/click",
        "select/click",
        &[("left", "menu/click"), ("right", "menu/click")],
    ),
];
const HAND_INTERACTION: &str = "/interaction_profiles/ext/hand_interaction_ext";

fn xr_err(e: xr::sys::Result) -> XrError {
    match e {
        xr::sys::Result::ERROR_SESSION_LOST => XrError::SessionLost,
        xr::sys::Result::ERROR_INSTANCE_LOST => XrError::InstanceLost,
        other => XrError::Other(other.to_string()),
    }
}

fn rig_pose(p: &xr::Posef) -> RigPose {
    RigPose {
        position: [p.position.x, p.position.y, p.position.z],
        orientation: [p.orientation.x, p.orientation.y, p.orientation.z, p.orientation.w],
    }
}

fn pose_valid(flags: xr::SpaceLocationFlags) -> bool {
    flags.contains(xr::SpaceLocationFlags::POSITION_VALID) && flags.contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
}

fn state(s: xr::SessionState) -> Option<SessionState> {
    Some(match s {
        xr::SessionState::IDLE => SessionState::Idle,
        xr::SessionState::READY => SessionState::Ready,
        xr::SessionState::SYNCHRONIZED => SessionState::Synchronized,
        xr::SessionState::VISIBLE => SessionState::Visible,
        xr::SessionState::FOCUSED => SessionState::Focused,
        xr::SessionState::STOPPING => SessionState::Stopping,
        xr::SessionState::LOSS_PENDING => SessionState::LossPending,
        xr::SessionState::EXITING => SessionState::Exiting,
        _ => return None,
    })
}

/// Where a tracked hand's pose comes from (spec 5.1). Controllers always use the grip action space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosePath {
    /// XR_EXT_hand_interaction binds the grip pose to the tracked hand: tracked hands and controllers agree.
    GripSpace,
    /// No XR_EXT_hand_interaction: the palm joint. A known gap of centimetres from the grip pose, accepted for M1;
    /// its exit is the headset test (spec 5.1, 10.4).
    PalmJoint,
}

impl std::fmt::Display for PosePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PosePath::GripSpace => write!(f, "tracked-hand pose: the grip action space (XR_EXT_hand_interaction)"),
            PosePath::PalmJoint => write!(
                f,
                "tracked-hand pose: the palm joint (no XR_EXT_hand_interaction; a known gap from the grip pose, accepted for M1 until the headset test)"
            ),
        }
    }
}

/// The rank of a swapchain format the runtime offers, as its raw value on `path`: `Some` only for the sRGB 8-bit
/// colour formats pe-render draws to, lower first (spec 4.2).
pub fn srgb_rank(path: GraphicsPath, raw: i64) -> Option<u8> {
    match (path, raw) {
        // VK_FORMAT_R8G8B8A8_SRGB, VK_FORMAT_B8G8R8A8_SRGB.
        (GraphicsPath::Vulkan, 43) => Some(0),
        (GraphicsPath::Vulkan, 50) => Some(1),
        // DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.
        (GraphicsPath::D3D12, 29) => Some(0),
        (GraphicsPath::D3D12, 91) => Some(1),
        _ => None,
    }
}

/// The swapchain pe-xr creates: one colour swapchain with two array layers, one per eye (spec 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapchainSize {
    pub width: u32,
    pub height: u32,
}

/// The depth swapchain, beside the colour one (spec 4.2).
struct DepthChain<G: xr::Graphics> {
    swapchain: xr::Swapchain<G>,
    /// Its raw graphics-API format.
    format: i64,
    /// The image acquired this frame.
    acquired: Option<u32>,
}

/// Whether a raw format is the 32-bit float depth format: VK_FORMAT_D32_SFLOAT (126), DXGI_FORMAT_D32_FLOAT (40).
pub fn is_depth32(path: GraphicsPath, raw: i64) -> bool {
    matches!((path, raw), (GraphicsPath::Vulkan, 126) | (GraphicsPath::D3D12, 40))
}

/// Everything that refers to the session. Fields drop in declaration order, which is OpenXR's teardown order: the
/// hand trackers and spaces, then the swapchain, then the frame stream and waiter, then the session, then the
/// actions (spec 3.1). The GPU context is not here: it outlives the session (spec 4.1).
struct Live<G: xr::Graphics> {
    trackers: Option<[xr::HandTracker; 2]>,
    grip_spaces: [xr::Space; 2],
    space: xr::Space,
    depth: Option<DepthChain<G>>,
    swapchain: xr::Swapchain<G>,
    stream: xr::FrameStream<G>,
    waiter: xr::FrameWaiter,
    session: xr::Session<G>,
    set: xr::ActionSet,
    squeeze: xr::Action<f32>,
    _grip: xr::Action<xr::Posef>,
    /// The pointer actions (spec 5.7): the aim pose's spaces, the select value and the menu button.
    aim_spaces: [xr::Space; 2],
    _aim: xr::Action<xr::Posef>,
    select: xr::Action<f32>,
    menu: xr::Action<bool>,
    hand_paths: [xr::Path; 2],
    hand_interaction: bool,
    size: SwapchainSize,
    /// The swapchain's format, as its raw graphics-API value (VkFormat or DXGI_FORMAT).
    format: i64,
    /// The last `xrWaitFrame`'s display time, exactly as the runtime gave it: `xrEndFrame` must get it back
    /// unchanged, never a value converted through host time.
    display_time: Option<(HostTime, xr::Time)>,
    views: Option<Vec<xr::View>>,
    /// The system, to ask again whether it tracks hands while no trackers exist.
    system: xr::SystemId,
    /// XR_EXT_hand_tracking was enabled, so trackers can be made once the runtime says it tracks hands.
    hand_tracking_ext: bool,
    /// While no trackers exist: when to ask the runtime again (every [`TRACKER_RETRY_NS`]).
    next_tracker_try: Option<HostTime>,
    /// The last tracker-creation failure logged, so a failure that repeats every retry is logged once.
    tracker_error_logged: Option<String>,
}

/// How often pe-xr asks again whether the runtime tracks hands, while it has no hand trackers: a runtime may turn
/// hand tracking on mid-session (Virtual Desktop's forwarding setting), and the trackers are made then.
pub const TRACKER_RETRY_NS: i64 = 2_000_000_000;

pub struct RealBackend<G: xr::Graphics> {
    live: Option<Live<G>>,
    grasp: GraspConfig,
    /// Interaction profiles the runtime refused to bind, with its reason (for the log).
    pub binding_warnings: Vec<String>,
    /// Where a tracked hand's pose comes from; the application logs it at start-up (spec 5.1).
    pub pose_path: PosePath,
    /// The reference space chosen (spec 3.2): "STAGE", "LOCAL_FLOOR" or "LOCAL"; the application logs it at
    /// start-up. In LOCAL the floor is not at y = 0 (the origin is at the head's starting pose).
    pub reference_space: &'static str,
    /// Each hand's joints from the last `read_hand`, with its time, when it came from hand tracking (`hand_joints`).
    joints: [Option<(HostTime, HandJoints)>; 2],
    /// Declared last, so it drops after the session.
    xr: XrInstance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateError(pub String);

fn ce(what: &str) -> impl Fn(xr::sys::Result) -> CreateError + '_ {
    move |e| CreateError(format!("{what}: {e}"))
}

impl<G: xr::Graphics> RealBackend<G> {
    /// Create the session, its actions, spaces, hand trackers and swapchain.
    ///
    /// # Safety
    /// `info` holds the GPU's raw handles for `G` (spec 4.1). They must be valid, made as the runtime required, and
    /// outlive the returned backend.
    pub unsafe fn new(
        xr: XrInstance,
        system: xr::SystemId,
        info: &G::SessionCreateInfo,
        grasp: GraspConfig,
    ) -> Result<Self, CreateError> {
        let instance = xr.instance.clone();
        // SAFETY: the caller's contract.
        let (session, waiter, stream) =
            unsafe { instance.create_session::<G>(system, info) }.map_err(ce("xrCreateSession"))?;

        let set = instance.create_action_set("pe_hands", "Primordial Engine hands", 0).map_err(ce("action set"))?;
        let left = instance.string_to_path("/user/hand/left").map_err(ce("path"))?;
        let right = instance.string_to_path("/user/hand/right").map_err(ce("path"))?;
        let grip = set.create_action::<xr::Posef>("grip", "Hand grip", &[left, right]).map_err(ce("grip action"))?;
        let squeeze =
            set.create_action::<f32>("squeeze", "Hand squeeze", &[left, right]).map_err(ce("squeeze action"))?;
        let aim = set.create_action::<xr::Posef>("aim", "Pointer aim", &[left, right]).map_err(ce("aim action"))?;
        let select =
            set.create_action::<f32>("select", "Pointer select", &[left, right]).map_err(ce("select action"))?;
        let menu = set.create_action::<bool>("menu", "Menu", &[left, right]).map_err(ce("menu action"))?;
        let path = |s: String| instance.string_to_path(&s).map_err(ce("path"));
        let mut binding_warnings = Vec::new();
        for (profile, squeeze_input, select_input, menus) in PROFILES {
            let mut b = Vec::new();
            for side in ["left", "right"] {
                b.push(xr::Binding::new(&grip, path(format!("/user/hand/{side}/input/grip/pose"))?));
                b.push(xr::Binding::new(&squeeze, path(format!("/user/hand/{side}/input/{squeeze_input}"))?));
                b.push(xr::Binding::new(&aim, path(format!("/user/hand/{side}/input/aim/pose"))?));
                b.push(xr::Binding::new(&select, path(format!("/user/hand/{side}/input/{select_input}"))?));
            }
            for (side, input) in menus.iter() {
                b.push(xr::Binding::new(&menu, path(format!("/user/hand/{side}/input/{input}"))?));
            }
            // A runtime may refuse a profile it does not know; the others still apply, and the refusal is kept.
            if let Err(e) = instance.suggest_interaction_profile_bindings(path(profile.to_string())?, &b) {
                binding_warnings.push(format!("{profile}: {e}"));
            }
        }
        let hand_interaction = xr.enabled.ext_hand_interaction;
        if hand_interaction {
            let mut b = Vec::new();
            for side in ["left", "right"] {
                b.push(xr::Binding::new(&grip, path(format!("/user/hand/{side}/input/grip/pose"))?));
                b.push(xr::Binding::new(&squeeze, path(format!("/user/hand/{side}/input/grasp_ext/value"))?));
                b.push(xr::Binding::new(&aim, path(format!("/user/hand/{side}/input/aim/pose"))?));
                b.push(xr::Binding::new(&select, path(format!("/user/hand/{side}/input/pinch_ext/value"))?));
            }
            if let Err(e) = instance.suggest_interaction_profile_bindings(path(HAND_INTERACTION.to_string())?, &b) {
                binding_warnings.push(format!("{HAND_INTERACTION}: {e}"));
            }
        }
        // Attached once, at creation; synced every frame while FOCUSED (spec 5.4).
        session.attach_action_sets(&[&set]).map_err(ce("xrAttachSessionActionSets"))?;
        let grip_spaces = [
            grip.create_space(&session, left, xr::Posef::IDENTITY).map_err(ce("grip space"))?,
            grip.create_space(&session, right, xr::Posef::IDENTITY).map_err(ce("grip space"))?,
        ];
        let aim_spaces = [
            aim.create_space(&session, left, xr::Posef::IDENTITY).map_err(ce("aim space"))?,
            aim.create_space(&session, right, xr::Posef::IDENTITY).map_err(ce("aim space"))?,
        ];

        // STAGE, else LOCAL_FLOOR, else LOCAL (spec 3.2).
        let offered = session.enumerate_reference_spaces().map_err(ce("reference spaces"))?;
        let kind = [xr::ReferenceSpaceType::STAGE, xr::ReferenceSpaceType::LOCAL_FLOOR]
            .into_iter()
            .find(|k| offered.contains(k))
            .unwrap_or(xr::ReferenceSpaceType::LOCAL);
        let space = session.create_reference_space(kind, xr::Posef::IDENTITY).map_err(ce("reference space"))?;
        let reference_space = match kind {
            xr::ReferenceSpaceType::STAGE => "STAGE",
            xr::ReferenceSpaceType::LOCAL_FLOOR => "LOCAL_FLOOR",
            _ => "LOCAL",
        };
        if crate::diag::enabled() {
            let offered: Vec<String> = offered.iter().map(|k| format!("{k:?}")).collect();
            crate::diag::emit(&format!("reference space {reference_space} (offered: {})", offered.join(", ")));
        }

        let supported = xr.enabled.ext_hand_tracking.then(|| instance.supports_hand_tracking(system));
        let trackers = if matches!(supported, Some(Ok(true))) {
            Some([
                session.create_hand_tracker(xr::Hand::LEFT).map_err(ce("hand tracker"))?,
                session.create_hand_tracker(xr::Hand::RIGHT).map_err(ce("hand tracker"))?,
            ])
        } else {
            None
        };
        if crate::diag::enabled() {
            crate::diag::emit(&format!(
                "hand tracking: XR_EXT_hand_tracking {}; the runtime says {}; trackers {}",
                if xr.enabled.ext_hand_tracking { "enabled" } else { "not offered" },
                match supported {
                    None => "nothing".to_string(),
                    Some(Ok(b)) => format!("supportsHandTracking = {b}"),
                    Some(Err(e)) => format!("an error ({e})"),
                },
                if trackers.is_some() { "made" } else { "not made (asked again every 2 s)" },
            ));
            let e = &xr.enabled;
            let optional = [
                ("XR_EXT_hand_tracking", e.ext_hand_tracking),
                ("XR_EXT_hand_interaction", e.ext_hand_interaction),
                ("XR_FB_display_refresh_rate", e.fb_display_refresh_rate),
                ("XR_EXT_local_floor", e.ext_local_floor),
                ("XR_KHR_composition_layer_depth", e.khr_composition_layer_depth),
            ];
            let on: Vec<&str> = optional.iter().filter(|(_, on)| *on).map(|(n, _)| *n).collect();
            let off = if crate::real::hand_interaction_off() {
                format!(" (XR_EXT_hand_interaction left off by {}=1)", crate::real::NO_HAND_INTERACTION_ENV)
            } else {
                String::new()
            };
            crate::diag::emit(&format!("optional extensions enabled: {}{off}", on.join(", ")));
        }

        let views = instance.enumerate_view_configuration_views(system, VIEW).map_err(ce("view configuration"))?;
        if views.len() < 2 {
            return Err(CreateError(format!("{} views in the stereo configuration, two expected", views.len())));
        }
        // One swapchain serves both eyes, so it takes the larger of the two recommended sizes.
        let size = SwapchainSize {
            width: views.iter().take(2).map(|v| v.recommended_image_rect_width).max().unwrap_or(0),
            height: views.iter().take(2).map(|v| v.recommended_image_rect_height).max().unwrap_or(0),
        };
        let formats = session.enumerate_swapchain_formats().map_err(ce("swapchain formats"))?;
        // Only sRGB formats, best first (spec 4.2): pe-render writes linear values and relies on the target's sRGB
        // encoding, so a UNORM format would show the headset image too dark.
        let path = xr.path;
        let format = formats
            .iter()
            .copied()
            .filter_map(|f| srgb_rank(path, G::lower_format(f)).map(|r| (r, f)))
            .min_by_key(|(r, _)| *r)
            .map(|(_, f)| f)
            .ok_or_else(|| CreateError("the runtime offers no sRGB 8-bit colour format (spec 4.2)".into()))?;
        let swapchain = session
            .create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                // Transfer source too, for the desktop mirror (render.md; @ballpit-viz).
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::TRANSFER_SRC,
                format,
                sample_count: 1,
                width: size.width,
                height: size.height,
                face_count: 1,
                array_size: 2,
                mip_count: 1,
            })
            .map_err(ce("xrCreateSwapchain"))?;
        // Depth when the runtime takes it (XR_KHR_composition_layer_depth) and offers a format pe-render draws to:
        // 32-bit float only for now (@ballpit-viz's preference); otherwise colour alone, which every runtime takes.
        let depth = match formats.iter().copied().find(|&f| is_depth32(path, G::lower_format(f))) {
            Some(f) if xr.enabled.khr_composition_layer_depth => Some(DepthChain {
                swapchain: session
                    .create_swapchain(&xr::SwapchainCreateInfo {
                        create_flags: xr::SwapchainCreateFlags::EMPTY,
                        usage_flags: xr::SwapchainUsageFlags::DEPTH_STENCIL_ATTACHMENT,
                        format: f,
                        sample_count: 1,
                        width: size.width,
                        height: size.height,
                        face_count: 1,
                        array_size: 2,
                        mip_count: 1,
                    })
                    .map_err(ce("xrCreateSwapchain (depth)"))?,
                format: G::lower_format(f),
                acquired: None,
            }),
            _ => None,
        };

        Ok(RealBackend {
            live: Some(Live {
                trackers,
                grip_spaces,
                space,
                depth,
                swapchain,
                stream,
                waiter,
                session,
                set,
                squeeze,
                _grip: grip,
                aim_spaces,
                _aim: aim,
                select,
                menu,
                hand_paths: [left, right],
                hand_interaction,
                size,
                format: G::lower_format(format),
                display_time: None,
                views: None,
                system,
                hand_tracking_ext: xr.enabled.ext_hand_tracking,
                next_tracker_try: None,
                tracker_error_logged: None,
            }),
            joints: [None, None],
            grasp,
            binding_warnings,
            pose_path: if hand_interaction { PosePath::GripSpace } else { PosePath::PalmJoint },
            reference_space,
            xr,
        })
    }

    pub fn instance(&self) -> &XrInstance {
        &self.xr
    }

    /// The swapchain's images, for the graphics binding to wrap once as wgpu textures (spec 4.2).
    pub fn swapchain_images(&self) -> Result<Vec<G::SwapchainImage>, XrError> {
        let live = self.live.as_ref().ok_or(XrError::SessionLost)?;
        live.swapchain.enumerate_images().map_err(xr_err)
    }

    /// The depth swapchain's images and raw format, when the runtime takes depth.
    #[allow(clippy::type_complexity)]
    pub fn depth_swapchain(&self) -> Result<Option<(Vec<G::SwapchainImage>, i64)>, XrError> {
        let live = self.live.as_ref().ok_or(XrError::SessionLost)?;
        match &live.depth {
            Some(d) => Ok(Some((d.swapchain.enumerate_images().map_err(xr_err)?, d.format))),
            None => Ok(None),
        }
    }

    pub fn swapchain_size(&self) -> Option<SwapchainSize> {
        self.live.as_ref().map(|l| l.size)
    }

    /// The swapchain's format, as its raw graphics-API value, for the binding to wrap its images in.
    pub fn swapchain_format(&self) -> Option<i64> {
        self.live.as_ref().map(|l| l.format)
    }

    /// Hand the instance back after the session was destroyed, to try a new session or drop it.
    pub fn into_instance(mut self) -> XrInstance {
        self.live = None;
        self.xr
    }

    fn live(&mut self) -> Result<&mut Live<G>, XrError> {
        self.live.as_mut().ok_or(XrError::SessionLost)
    }

    fn xr_time(&self, at: HostTime) -> Result<xr::Time, XrError> {
        if let Some(l) = &self.live
            && let Some((h, x)) = l.display_time
            && h == at
        {
            return Ok(x);
        }
        self.xr.to_xr(at).map_err(|e| XrError::Other(e.to_string()))
    }

    /// While no hand trackers exist, ask the runtime again every [`TRACKER_RETRY_NS`] whether it tracks hands, and
    /// make them once it does.
    fn retry_trackers(&mut self, at: HostTime) {
        let instance = self.xr.instance.clone();
        let Some(live) = self.live.as_mut() else { return };
        if live.trackers.is_some() || !live.hand_tracking_ext || live.next_tracker_try.is_some_and(|t| at.0 < t.0) {
            return;
        }
        live.next_tracker_try = Some(HostTime(at.0.saturating_add(TRACKER_RETRY_NS)));
        if !instance.supports_hand_tracking(live.system).unwrap_or(false) {
            return;
        }
        let made = live
            .session
            .create_hand_tracker(xr::Hand::LEFT)
            .and_then(|l| live.session.create_hand_tracker(xr::Hand::RIGHT).map(|r| [l, r]));
        match made {
            Ok(t) => {
                live.trackers = Some(t);
                if crate::diag::enabled() {
                    crate::diag::emit(&format!("hand tracking: trackers made mid-session at {} ns", at.0));
                }
            }
            Err(e) => {
                let e = e.to_string();
                if crate::diag::enabled() && live.tracker_error_logged.as_ref() != Some(&e) {
                    crate::diag::emit(&format!(
                        "hand tracking: supported now, but xrCreateHandTrackerEXT failed: {e} (logged again only if \
                         it changes)"
                    ));
                }
                live.tracker_error_logged = Some(e);
            }
        }
    }

    fn read_tracked(&self, hand: Hand, t: xr::Time, at: HostTime) -> Option<(Reading, HandJoints)> {
        let live = self.live.as_ref()?;
        let tracker = &live.trackers.as_ref()?[hand.index()];
        let located = live.space.locate_hand_joints(tracker, t).ok()??;
        let joints: [Joint; joint::COUNT] = std::array::from_fn(|i| Joint {
            position: [located[i].pose.position.x, located[i].pose.position.y, located[i].pose.position.z],
            valid: located[i].location_flags.contains(xr::SpaceLocationFlags::POSITION_VALID),
        });
        let palm = &located[joint::PALM];
        if !pose_valid(palm.location_flags) {
            return None;
        }
        // For drawing the hand (spec 6): every joint's position and radius, in the same space and at the same time.
        let drawn = HandJoints(std::array::from_fn(|i| HandJoint {
            position: joints[i].position,
            radius: located[i].radius,
            valid: joints[i].valid,
        }));
        // The pose is the grip pose (spec 5.1): from the grip action space when XR_EXT_hand_interaction binds it to
        // the tracked hand, with its velocities; otherwise the palm joint, which is the nearest joint to the grip
        // point (spec 5.1 says so).
        let (pose, linear_velocity, angular_velocity) = match live.hand_interaction.then(|| self.grip_space(hand, t)) {
            Some(Some(g)) => g,
            _ => (rig_pose(&palm.pose), None, None),
        };
        // Both grip signals (spec 5.3): the squeeze action while it is active, on whichever profile is current (a
        // held controller's squeeze, or the runtime's grasp value through XR_EXT_hand_interaction), and the curl.
        let grip = self.squeeze_value(hand);
        let curl = grip_from_joints(&joints, &self.grasp);
        Some((
            Reading {
                source: HandSource::Tracked,
                at,
                pose: Some(pose),
                linear_velocity,
                angular_velocity,
                grip,
                curl,
            },
            drawn,
        ))
    }

    /// The grip action space's pose and velocities, if valid.
    #[allow(clippy::type_complexity)]
    fn grip_space(&self, hand: Hand, t: xr::Time) -> Option<(RigPose, Option<[f32; 3]>, Option<[f32; 3]>)> {
        let live = self.live.as_ref()?;
        let (loc, vel) = live.grip_spaces[hand.index()].relate(&live.space, t).ok()?;
        if !pose_valid(loc.location_flags) {
            return None;
        }
        let lin = vel.velocity_flags.contains(xr::SpaceVelocityFlags::LINEAR_VALID).then_some([
            vel.linear_velocity.x,
            vel.linear_velocity.y,
            vel.linear_velocity.z,
        ]);
        let ang = vel.velocity_flags.contains(xr::SpaceVelocityFlags::ANGULAR_VALID).then_some([
            vel.angular_velocity.x,
            vel.angular_velocity.y,
            vel.angular_velocity.z,
        ]);
        Some((rig_pose(&loc.pose), lin, ang))
    }

    fn squeeze_value(&self, hand: Hand) -> Option<f32> {
        let live = self.live.as_ref()?;
        let s = live.squeeze.state(&live.session, live.hand_paths[hand.index()]).ok()?;
        s.is_active.then_some(s.current_state)
    }
}

impl<G: xr::Graphics> Backend for RealBackend<G> {
    fn now(&self) -> HostTime {
        crate::clock::now()
    }

    fn poll_event(&mut self) -> Result<Option<XrEvent>, XrError> {
        // A buffer per call, not a field: openxr's buffer holds a raw `next` pointer, which would make the backend
        // (and pe-app's VulkanVr) not `Send`, and it moves to the XR thread (app.md 4). 4 KB on the stack.
        let mut buf = xr::EventDataBuffer::new();
        loop {
            let ev = self.xr.instance.poll_event(&mut buf).map_err(xr_err)?;
            let Some(ev) = ev else { return Ok(None) };
            let mapped = match ev {
                xr::Event::SessionStateChanged(s) => state(s.state()).map(XrEvent::StateChanged),
                xr::Event::InstanceLossPending(_) => Some(XrEvent::InstanceLossPending),
                xr::Event::ReferenceSpaceChangePending(_) => Some(XrEvent::ReferenceSpaceChangePending),
                xr::Event::InteractionProfileChanged(_) => Some(XrEvent::InteractionProfileChanged),
                xr::Event::EventsLost(l) => Some(XrEvent::EventsLost(l.lost_event_count())),
                // The rest: nothing pe-xr acts on; read the next.
                _ => None,
            };
            if mapped.is_some() {
                return Ok(mapped);
            }
        }
    }

    fn begin_session(&mut self) -> Result<(), XrError> {
        self.live()?.session.begin(VIEW).map(|_| ()).map_err(xr_err)
    }

    fn end_session(&mut self) -> Result<(), XrError> {
        self.live()?.session.end().map(|_| ()).map_err(xr_err)
    }

    fn request_exit(&mut self) -> Result<(), XrError> {
        self.live()?.session.request_exit().map_err(xr_err)
    }

    fn destroy_session(&mut self) {
        // `Live`'s field order is the teardown order.
        self.live = None;
    }

    fn sync_actions(&mut self) -> Result<(), XrError> {
        let live = self.live()?;
        live.session.sync_actions(&[(&live.set).into()]).map_err(xr_err)
    }

    fn wait_frame(&mut self) -> Result<FrameTiming, XrError> {
        let fs = self.live()?.waiter.wait().map_err(xr_err)?;
        let host = self.xr.to_host(fs.predicted_display_time).map_err(|e| XrError::Other(e.to_string()))?;
        let live = self.live()?;
        live.display_time = Some((host, fs.predicted_display_time));
        live.views = None;
        Ok(FrameTiming {
            predicted_display_time: host,
            predicted_display_period_ns: fs.predicted_display_period.as_nanos(),
            should_render: fs.should_render,
        })
    }

    fn begin_frame(&mut self) -> Result<(), XrError> {
        self.live()?.stream.begin().map_err(xr_err)
    }

    fn read_hand(&mut self, hand: Hand, at: HostTime) -> Reading {
        let absent = Reading {
            source: HandSource::Controller,
            at,
            pose: None,
            linear_velocity: None,
            angular_velocity: None,
            grip: None,
            curl: None,
        };
        self.joints[hand.index()] = None;
        if hand == Hand::Left {
            self.retry_trackers(at);
        }
        let Ok(t) = self.xr_time(at) else { return absent };
        // The tracked hand while it is tracked; the controller otherwise (spec 5.4).
        if let Some((r, drawn)) = self.read_tracked(hand, t, at) {
            self.joints[hand.index()] = Some((at, drawn));
            return r;
        }
        let Some(live) = self.live.as_ref() else { return absent };
        let Ok((loc, vel)) = live.grip_spaces[hand.index()].relate(&live.space, t) else { return absent };
        if !pose_valid(loc.location_flags) {
            return absent;
        }
        let lin = vel.velocity_flags.contains(xr::SpaceVelocityFlags::LINEAR_VALID).then_some([
            vel.linear_velocity.x,
            vel.linear_velocity.y,
            vel.linear_velocity.z,
        ]);
        let ang = vel.velocity_flags.contains(xr::SpaceVelocityFlags::ANGULAR_VALID).then_some([
            vel.angular_velocity.x,
            vel.angular_velocity.y,
            vel.angular_velocity.z,
        ]);
        Reading {
            source: HandSource::Controller,
            at,
            pose: Some(rig_pose(&loc.pose)),
            linear_velocity: lin,
            angular_velocity: ang,
            grip: self.squeeze_value(hand),
            curl: None,
        }
    }

    fn read_pointer(&mut self, hand: Hand, at: HostTime) -> PointerReading {
        let Some(live) = self.live.as_ref() else { return PointerReading::default() };
        let path = live.hand_paths[hand.index()];
        let select = live.select.state(&live.session, path).ok().filter(|s| s.is_active).map(|s| s.current_state);
        let menu = live.menu.state(&live.session, path).ok().filter(|s| s.is_active).map(|s| s.current_state);
        let aim = self.xr_time(at).ok().and_then(|t| {
            let loc = live.aim_spaces[hand.index()].locate(&live.space, t).ok()?;
            pose_valid(loc.location_flags).then(|| rig_pose(&loc.pose))
        });
        PointerReading { aim, select, menu }
    }

    fn interaction_profiles(&self) -> Option<[String; 2]> {
        let live = self.live.as_ref()?;
        Some(live.hand_paths.map(|top| match live.session.current_interaction_profile(top) {
            Ok(p) if p == xr::Path::NULL => "none".to_string(),
            Ok(p) => self.xr.instance.path_to_string(p).unwrap_or_else(|e| format!("(unnamed: {e})")),
            Err(e) => format!("(unread: {e})"),
        }))
    }

    fn hand_joints(&self, hand: Hand, at: HostTime) -> Option<HandJoints> {
        self.joints[hand.index()].filter(|(t, _)| *t == at).map(|(_, j)| j)
    }

    fn acquire_image(&mut self) -> Result<ImageIndex, XrError> {
        let live = self.live()?;
        let i = live.swapchain.acquire_image().map_err(xr_err)?;
        live.swapchain.wait_image(xr::Duration::INFINITE).map_err(xr_err)?;
        if let Some(d) = &mut live.depth {
            let j = d.swapchain.acquire_image().map_err(xr_err)?;
            d.acquired = Some(j);
            d.swapchain.wait_image(xr::Duration::INFINITE).map_err(xr_err)?;
        }
        Ok(ImageIndex(i))
    }

    fn depth_image(&self) -> Option<ImageIndex> {
        self.live.as_ref()?.depth.as_ref()?.acquired.map(ImageIndex)
    }

    fn locate_views(&mut self, display_time: HostTime) -> Result<Located, XrError> {
        let t = self.xr_time(display_time)?;
        let live = self.live()?;
        let (flags, views) = live.session.locate_views(VIEW, t, &live.space).map_err(xr_err)?;
        let valid =
            flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) && flags.contains(xr::ViewStateFlags::POSITION_VALID);
        if views.len() < 2 {
            return Err(XrError::Other(format!("{} views located, two expected", views.len())));
        }
        let eye = |v: &xr::View| EyePose {
            pose: rig_pose(&v.pose),
            fov: Fov { left: v.fov.angle_left, right: v.fov.angle_right, up: v.fov.angle_up, down: v.fov.angle_down },
        };
        let eyes = [eye(&views[0]), eye(&views[1])];
        // Invalid views are never submitted (end_frame needs them stored to build a layer).
        live.views = valid.then_some(views);
        Ok(Located { eyes, valid })
    }

    fn release_image(&mut self) -> Result<(), XrError> {
        let live = self.live()?;
        // The depth image first, so neither stays acquired into the next frame if one release fails.
        if let Some(d) = &mut live.depth
            && d.acquired.take().is_some()
        {
            d.swapchain.release_image().map_err(xr_err)?;
        }
        live.swapchain.release_image().map_err(xr_err)
    }

    fn end_frame(&mut self, display_time: HostTime, layers: Layers) -> Result<(), XrError> {
        let t = self.xr_time(display_time)?;
        let live = self.live()?;
        match (layers, &live.views) {
            (Layers::Projection { extent, depth }, Some(vs)) => {
                // The drawn rectangle from the origin, never past the image (spec 4.2; render scale as a viewport).
                let [w, h] = extent.unwrap_or([live.size.width, live.size.height]);
                let rect = xr::Rect2Di {
                    offset: xr::Offset2Di { x: 0, y: 0 },
                    extent: xr::Extent2Di {
                        width: w.clamp(1, live.size.width) as i32,
                        height: h.clamp(1, live.size.height) as i32,
                    },
                };
                // Depth is chained to each view (XR_KHR_composition_layer_depth), from the same rectangle of that
                // eye's layer of the depth image. The infos live until xrEndFrame returns.
                let depth_infos: Option<[xr::sys::CompositionLayerDepthInfoKHR; 2]> = match (depth, live.depth.as_ref())
                {
                    (Some(d), Some(chain)) => Some(std::array::from_fn(|i| xr::sys::CompositionLayerDepthInfoKHR {
                        ty: xr::sys::CompositionLayerDepthInfoKHR::TYPE,
                        next: std::ptr::null(),
                        sub_image: xr::sys::SwapchainSubImage {
                            swapchain: chain.swapchain.as_raw(),
                            image_rect: rect,
                            image_array_index: i as u32,
                        },
                        min_depth: d.min_depth,
                        max_depth: d.max_depth,
                        near_z: d.near_z,
                        far_z: d.far_z,
                    })),
                    _ => None,
                };
                let pv = |i: usize| {
                    let v = xr::CompositionLayerProjectionView::new().pose(vs[i].pose).fov(vs[i].fov).sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&live.swapchain)
                            .image_array_index(i as u32)
                            .image_rect(rect),
                    );
                    match &depth_infos {
                        Some(infos) => {
                            let mut raw = v.into_raw();
                            raw.next = &infos[i] as *const xr::sys::CompositionLayerDepthInfoKHR as *const _;
                            // SAFETY: the same view with a valid depth info chained, which outlives xrEndFrame below.
                            unsafe { xr::CompositionLayerProjectionView::from_raw(raw) }
                        }
                        None => v,
                    }
                };
                let pvs = [pv(0), pv(1)];
                let layer = xr::CompositionLayerProjection::new().space(&live.space).views(&pvs);
                live.stream.end(t, xr::EnvironmentBlendMode::OPAQUE, &[&layer]).map_err(xr_err)
            }
            _ => live.stream.end(t, xr::EnvironmentBlendMode::OPAQUE, &[]).map_err(xr_err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_srgb_formats_are_taken_and_rgba_first() {
        // Virtual Desktop's offered lists, in its order [V: M0 headset run, b35c448].
        let vulkan = [43i64, 50, 37, 44, 97, 126, 130, 129, 124];
        let dxgi = [29i64, 91, 28, 87, 93, 88, 10, 40, 20, 45, 55];
        let best = |path, list: &[i64]| list.iter().copied().filter_map(|f| srgb_rank(path, f).map(|r| (r, f))).min();
        assert_eq!(best(GraphicsPath::Vulkan, &vulkan), Some((0, 43)));
        assert_eq!(best(GraphicsPath::D3D12, &dxgi), Some((0, 29)));
        // A runtime listing UNORM first still gets sRGB.
        assert_eq!(best(GraphicsPath::Vulkan, &[37, 44, 50]), Some((1, 50)));
        assert_eq!(best(GraphicsPath::D3D12, &[28, 87, 91]), Some((1, 91)));
        // No sRGB at all: nothing.
        assert_eq!(best(GraphicsPath::Vulkan, &[37, 44]), None);
    }
}
