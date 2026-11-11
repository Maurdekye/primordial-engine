//! The real runtime at the session level: [`RealBackend`] implements [`Backend`] over the `openxr` crate, generic
//! over its graphics type (`openxr::Vulkan` or `openxr::D3D12`). Only creating the session needs the GPU's raw
//! handles; everything here is the same for both paths (spec 4.4).
//!
//! It is never run by tests: creating it needs an [`XrInstance`], which needs the loader, which needs the headset
//! guard's permit. Its logic is the thin mapping from OpenXR calls to the trait that [`crate::session`] drives, and
//! that logic is tested against [`crate::fake`]. Ported in part from @ballpit-viz's M0 check (`frame_loop`, at
//! f8e9c776).

use openxr as xr;

use crate::backend::{Backend, EyePose, Fov, FrameTiming, ImageIndex, Layers, Located, SessionState, XrError, XrEvent};
use crate::hands::{GraspConfig, Joint, Reading, grip_from_joints, joint};
use crate::real::{GraphicsPath, XrInstance};
use crate::seam::{Hand, HandSource, HostTime, RigPose};

const VIEW: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;

/// The suggested bindings (spec 5.4): (interaction profile, grip pose path, squeeze value path) per hand.
const PROFILES: &[(&str, &str)] = &[
    ("/interaction_profiles/oculus/touch_controller", "squeeze/value"),
    ("/interaction_profiles/valve/index_controller", "squeeze/value"),
    // The simple controller has no squeeze; its select button stands in for the grasp (0 or 1).
    ("/interaction_profiles/khr/simple_controller", "select/click"),
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
                "tracked-hand pose: the palm joint (no XR_EXT_hand_interaction; a known gap from the grip pose,                  accepted for M1 until the headset test)"
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

/// Everything that refers to the session. Fields drop in declaration order, which is OpenXR's teardown order: the
/// hand trackers and spaces, then the swapchain, then the frame stream and waiter, then the session, then the
/// actions (spec 3.1). The GPU context is not here: it outlives the session (spec 4.1).
struct Live<G: xr::Graphics> {
    trackers: Option<[xr::HandTracker; 2]>,
    grip_spaces: [xr::Space; 2],
    space: xr::Space,
    swapchain: xr::Swapchain<G>,
    stream: xr::FrameStream<G>,
    waiter: xr::FrameWaiter,
    session: xr::Session<G>,
    set: xr::ActionSet,
    squeeze: xr::Action<f32>,
    _grip: xr::Action<xr::Posef>,
    hand_paths: [xr::Path; 2],
    hand_interaction: bool,
    size: SwapchainSize,
    /// The swapchain's format, as its raw graphics-API value (VkFormat or DXGI_FORMAT).
    format: i64,
    /// The last `xrWaitFrame`'s display time, exactly as the runtime gave it: `xrEndFrame` must get it back
    /// unchanged, never a value converted through host time.
    display_time: Option<(HostTime, xr::Time)>,
    views: Option<Vec<xr::View>>,
}

pub struct RealBackend<G: xr::Graphics> {
    live: Option<Live<G>>,
    events: xr::EventDataBuffer,
    grasp: GraspConfig,
    /// Interaction profiles the runtime refused to bind, with its reason (for the log).
    pub binding_warnings: Vec<String>,
    /// Where a tracked hand's pose comes from; the application logs it at start-up (spec 5.1).
    pub pose_path: PosePath,
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
        let path = |s: String| instance.string_to_path(&s).map_err(ce("path"));
        let mut binding_warnings = Vec::new();
        for (profile, squeeze_input) in PROFILES {
            let mut b = Vec::new();
            for side in ["left", "right"] {
                b.push(xr::Binding::new(&grip, path(format!("/user/hand/{side}/input/grip/pose"))?));
                b.push(xr::Binding::new(&squeeze, path(format!("/user/hand/{side}/input/{squeeze_input}"))?));
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

        // STAGE, else LOCAL_FLOOR, else LOCAL (spec 3.2).
        let offered = session.enumerate_reference_spaces().map_err(ce("reference spaces"))?;
        let kind = [xr::ReferenceSpaceType::STAGE, xr::ReferenceSpaceType::LOCAL_FLOOR]
            .into_iter()
            .find(|k| offered.contains(k))
            .unwrap_or(xr::ReferenceSpaceType::LOCAL);
        let space = session.create_reference_space(kind, xr::Posef::IDENTITY).map_err(ce("reference space"))?;

        let trackers = if xr.enabled.ext_hand_tracking && instance.supports_hand_tracking(system).unwrap_or(false) {
            Some([
                session.create_hand_tracker(xr::Hand::LEFT).map_err(ce("hand tracker"))?,
                session.create_hand_tracker(xr::Hand::RIGHT).map_err(ce("hand tracker"))?,
            ])
        } else {
            None
        };

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

        Ok(RealBackend {
            live: Some(Live {
                trackers,
                grip_spaces,
                space,
                swapchain,
                stream,
                waiter,
                session,
                set,
                squeeze,
                _grip: grip,
                hand_paths: [left, right],
                hand_interaction,
                size,
                format: G::lower_format(format),
                display_time: None,
                views: None,
            }),
            events: xr::EventDataBuffer::new(),
            grasp,
            binding_warnings,
            pose_path: if hand_interaction { PosePath::GripSpace } else { PosePath::PalmJoint },
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

    fn read_tracked(&self, hand: Hand, t: xr::Time, at: HostTime) -> Option<Reading> {
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
        // The pose is the grip pose (spec 5.1): from the grip action space when XR_EXT_hand_interaction binds it to
        // the tracked hand, with its velocities; otherwise the palm joint, which is the nearest joint to the grip
        // point (spec 5.1 says so).
        let (pose, linear_velocity, angular_velocity) = match live.hand_interaction.then(|| self.grip_space(hand, t)) {
            Some(Some(g)) => g,
            _ => (rig_pose(&palm.pose), None, None),
        };
        // The runtime's grasp value when it offers one (XR_EXT_hand_interaction), else the joints' curl (spec 5.3).
        let grip = if live.hand_interaction {
            self.squeeze_value(hand).or_else(|| grip_from_joints(&joints, &self.grasp))
        } else {
            grip_from_joints(&joints, &self.grasp)
        };
        Some(Reading { source: HandSource::Tracked, at, pose: Some(pose), linear_velocity, angular_velocity, grip })
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
        loop {
            let ev = self.xr.instance.poll_event(&mut self.events).map_err(xr_err)?;
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
        };
        let Ok(t) = self.xr_time(at) else { return absent };
        // The tracked hand while it is tracked; the controller otherwise (spec 5.4).
        if let Some(r) = self.read_tracked(hand, t, at) {
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
        }
    }

    fn acquire_image(&mut self) -> Result<ImageIndex, XrError> {
        let live = self.live()?;
        let i = live.swapchain.acquire_image().map_err(xr_err)?;
        live.swapchain.wait_image(xr::Duration::INFINITE).map_err(xr_err)?;
        Ok(ImageIndex(i))
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
        self.live()?.swapchain.release_image().map_err(xr_err)
    }

    fn end_frame(&mut self, display_time: HostTime, layers: Layers) -> Result<(), XrError> {
        let t = self.xr_time(display_time)?;
        let live = self.live()?;
        match (layers, &live.views) {
            (Layers::Projection { .. }, Some(vs)) => {
                // Depth (XR_KHR_composition_layer_depth) is not submitted yet: colour only.
                let rect = xr::Rect2Di {
                    offset: xr::Offset2Di { x: 0, y: 0 },
                    extent: xr::Extent2Di { width: live.size.width as i32, height: live.size.height as i32 },
                };
                let pv = |i: usize| {
                    xr::CompositionLayerProjectionView::new().pose(vs[i].pose).fov(vs[i].fov).sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&live.swapchain)
                            .image_array_index(i as u32)
                            .image_rect(rect),
                    )
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
