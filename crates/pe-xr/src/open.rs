//! Opening VR in one call (spec 3.3, 4.1): the runtime, the instance, the headset, the GPU context through the
//! runtime, the session and its swapchain images as wgpu textures. Anything short of all of it is a reason to stay
//! on the desktop, which the application shows and retries later; the simulation runs either way.
//!
//! The guard (spec 10.3) is checked first, inside [`loader::load`]: without `PE_XR_REAL=1` or VR turned on in the
//! configuration, no runtime library is ever opened.

use std::fmt;
use std::sync::Arc;

use openxr as xr;
use pe_render::gpu::GpuContext;

use crate::binding::{BindingError, XrTargets, vulkan_context};
use crate::guard::Guard;
use crate::hands::GraspConfig;
use crate::loader::{self, Discovery, LoadError};
use crate::real::{GraphicsPath, Headset, InstanceError, XrInstance};
use crate::real_session::{CreateError, RealBackend};
use crate::ring::HandRing;
use crate::session::XrSession;

/// Why VR did not open: the application stays on the desktop.
#[derive(Debug)]
pub enum NotOpened {
    /// The guard refused, or no runtime is installed, or it would not load.
    Runtime(LoadError),
    Instance(InstanceError),
    /// The runtime is up but the headset is not connected: retry later (spec 3.3).
    HeadsetAbsent,
    /// The runtime has no head-mounted form factor.
    Unsupported,
    Binding(BindingError),
    Session(CreateError),
    /// The swapchain's images could not be listed or wrapped.
    Images(String),
}

impl fmt::Display for NotOpened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotOpened::Runtime(e) => write!(f, "no OpenXR runtime: {e}"),
            NotOpened::Instance(e) => write!(f, "{e}"),
            NotOpened::HeadsetAbsent => write!(f, "the headset is not connected"),
            NotOpened::Unsupported => write!(f, "the runtime has no head-mounted display"),
            NotOpened::Binding(e) => write!(f, "{e}"),
            NotOpened::Session(e) => write!(f, "session: {}", e.0),
            NotOpened::Images(e) => write!(f, "swapchain images: {e}"),
        }
    }
}

impl std::error::Error for NotOpened {}

/// VR on Vulkan, open. Its parts drop in OpenXR's order: the image wrappers (after the GPU is idle on them), then the
/// session and its swapchain, then the GPU context, whose device and instance hold the runtime library to the end.
pub struct VulkanVr {
    pub targets: XrTargets,
    pub session: XrSession<RealBackend<xr::Vulkan>>,
    pub gpu: GpuContext,
}

impl Drop for VulkanVr {
    fn drop(&mut self) {
        // The swapchain goes with the session, right after this: the GPU must be done with its images first (L4).
        self.targets.release(&self.gpu);
    }
}

/// Open VR on Vulkan. `ring` is the hand ring the simulation thread reads (spec 6.2); it outlives any session.
pub fn open_vulkan(
    discovery: &dyn Discovery,
    guard: &Guard,
    grasp: GraspConfig,
    ring: Arc<HandRing>,
) -> Result<VulkanVr, NotOpened> {
    let runtime = loader::load(discovery, guard).map_err(NotOpened::Runtime)?;
    let instance = XrInstance::create(&runtime, GraphicsPath::Vulkan).map_err(NotOpened::Instance)?;
    let system = match instance.headset().map_err(NotOpened::Instance)? {
        Headset::Present(s) => s,
        Headset::Absent => return Err(NotOpened::HeadsetAbsent),
        Headset::Unsupported => return Err(NotOpened::Unsupported),
    };
    // SAFETY: the instance was created on the Vulkan path, which enables XR_KHR_vulkan_enable2, and `system` is its
    // headset.
    let ctx = unsafe { vulkan_context(&instance, system) }.map_err(NotOpened::Binding)?;
    // SAFETY: the handles are the live objects `vulkan_context` just made through this instance, for this system.
    let backend = unsafe { RealBackend::<xr::Vulkan>::new(instance, system, &ctx.session_info, grasp) }
        .map_err(NotOpened::Session)?;
    let images = backend.swapchain_images().map_err(|e| NotOpened::Images(format!("{e:?}")))?;
    let depth = backend.depth_swapchain().map_err(|e| NotOpened::Images(format!("{e:?}")))?;
    let depth = depth.as_ref().map(|(images, f)| (images.as_slice(), *f));
    let (size, format) = match (backend.swapchain_size(), backend.swapchain_format()) {
        (Some(s), Some(f)) => ([s.width, s.height], f),
        _ => return Err(NotOpened::Images("no swapchain".into())),
    };
    // SAFETY: the live swapchain's images, of its format and size, on the device they were created for; `VulkanVr`
    // drops the wrappers before the session.
    let targets = unsafe { XrTargets::vulkan(&ctx.gpu, &images, format, size, depth) }
        .map_err(|e| NotOpened::Images(e.to_string()))?;
    Ok(VulkanVr { targets, session: XrSession::new(backend, ring, grasp), gpu: ctx.gpu })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::NoRuntime;
    use crate::ring::DEFAULT_WINDOW_NS;

    #[test]
    fn without_a_runtime_vr_stays_closed_and_says_why() {
        let ring = Arc::new(HandRing::new(DEFAULT_WINDOW_NS, 8_333_333));
        // Allowed by the guard, but nothing is installed: no library is opened, and the reason is the runtime's.
        let guard = Guard::from_env(true);
        match open_vulkan(&NoRuntime, &guard, GraspConfig::default(), ring) {
            Err(NotOpened::Runtime(_)) => {}
            Err(e) => panic!("wrong reason: {e}"),
            Ok(_) => panic!("opened without a runtime"),
        }
    }
}
