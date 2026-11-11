//! VR mode's frame sink (app.md 1; xr.md 4.2). pe-xr's XR thread asks it to draw each frame: it draws the newest
//! frame view through pe-render's presenter into pe-xr's swapchain images. pe-xr owns the swapchain's wgpu wrappers
//! ([`XrTargets`]); pe-app owns the drawing (option (a), agreed with @xr-eng).
//!
//! This slice has the sink only. Opening VR (`pe_xr::open::open_vulkan`, on the main thread before the window's
//! context), the XR thread that steps the session with it, and the fall back to the desktop come next.

use std::sync::Arc;

use pe_render::present::{FrameSlot, Presenter};
use pe_render::scene::Renderer;
use pe_render::window::EffectSink;
use pe_render::{DisplayHand, LocalPresentation};
use pe_xr::backend::DepthInfo;
use pe_xr::binding::{self, XrTargets};
use pe_xr::session::{FrameRequest, FrameSink};

/// Draws each VR frame. It lives on the XR thread; the simulation thread reaches it only through the frame slot.
///
/// A sink serves one session: after [`FrameSink::release_swapchain`] its targets are gone and it draws nothing. A
/// restarted session comes from a new `open_vulkan`, with a new sink (@xr-eng's L1).
pub struct VrSink {
    slot: Arc<FrameSlot>,
    presenter: Presenter,
    /// Built on the GPU context pe-xr made through the runtime (`VulkanVr`).
    renderer: Renderer,
    targets: XrTargets,
    /// Sounds go to pe-app's audio, haptics to pe-xr (render.md 5).
    effects: Box<dyn EffectSink + Send>,
    /// The render scale as a viewport: the rectangle drawn from the origin, or `None` for the whole image. The
    /// swapchain keeps its size (render.md 4).
    extent: Option<[u32; 2]>,
}

impl VrSink {
    pub fn new(
        slot: Arc<FrameSlot>,
        presenter: Presenter,
        renderer: Renderer,
        targets: XrTargets,
        effects: Box<dyn EffectSink + Send>,
        extent: Option<[u32; 2]>,
    ) -> VrSink {
        VrSink { slot, presenter, renderer, targets, effects, extent }
    }
}

/// The local player's part of a display frame: shown at the runtime's predicted display time, with the hands pe-xr
/// located for it.
fn local_presentation(frame: &FrameRequest<'_>) -> LocalPresentation {
    LocalPresentation {
        display_time: frame.timing.predicted_display_time,
        hands: frame.hands.map(|h| h.map(|pose| DisplayHand { pose })),
    }
}

impl FrameSink for VrSink {
    /// Nothing is drawn before the first view, or for an image outside the swapchain: the frame ends with no layers.
    fn render(&mut self, frame: &FrameRequest<'_>) -> bool {
        let Some(view) = self.slot.newest() else {
            return false;
        };
        let Some(target) = self.targets.stereo(frame.image, frame.depth_image, self.extent) else {
            return false;
        };
        let cameras = binding::eye_cameras(&view.local_rig, frame.eyes);
        let presented = self.presenter.frame(&view, &local_presentation(frame), &cameras);
        for c in &presented.effects {
            self.effects.command(c);
        }
        self.renderer.render_stereo(&presented.scene, cameras, &target);
        true
    }

    fn extent(&self) -> Option<[u32; 2]> {
        self.extent
    }

    /// pe-render draws reverse-Z depth in unscaled metres whenever the target has a depth image.
    fn depth(&self) -> Option<DepthInfo> {
        self.targets.has_depth().then(binding::vr_depth_info)
    }

    /// Waits until the GPU is idle on the swapchain's images, then drops their wrappers.
    fn release_swapchain(&mut self) {
        self.targets.release(self.renderer.context());
    }
}

/// The sink moves to the XR thread.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<VrSink>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use pe_seams::{HostTime, RigPose};
    use pe_xr::backend::{EyePose, Fov, FrameTiming, ImageIndex};

    #[test]
    fn the_local_presentation_takes_the_display_time_and_the_located_hands() {
        let eye = EyePose { pose: RigPose::IDENTITY, fov: Fov { left: -0.8, right: 0.8, up: 0.8, down: -0.8 } };
        let right = RigPose::IDENTITY;
        let frame = FrameRequest {
            timing: FrameTiming {
                predicted_display_time: HostTime(7_000_000),
                predicted_display_period_ns: 11_111_111,
                should_render: true,
            },
            eyes: &[eye, eye],
            image: ImageIndex(0),
            depth_image: None,
            hands: [None, Some(right)],
        };
        let local = local_presentation(&frame);
        assert_eq!(local.display_time, HostTime(7_000_000));
        assert!(local.hands[0].is_none(), "an untracked hand is not drawn");
        assert_eq!(local.hands[1].map(|h| h.pose), Some(right));
    }
}
