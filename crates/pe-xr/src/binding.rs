//! The graphics binding (spec 4.1, 4.2): the GPU context made through the OpenXR runtime, and the runtime's
//! swapchain images wrapped as wgpu textures for pe-render.
//!
//! **Vulkan first** (`XR_KHR_vulkan_enable2`, the coordinator's ruling): the runtime creates the VkInstance and the
//! VkDevice, adding the extensions it needs, and wgpu-hal adopts both. Ported from @ballpit-viz's M0 check
//! (`feasibility/xr-wgpu/src/xr.rs`, b35c448), which ran 944 of 944 frames on the user's headset this way [V M0].
//! The D3D12 fallback comes next.
//!
//! - **One at a time:** everything here that makes an instance, adapter or device runs under
//!   `pe_render::gpu::creation_lock` (render.md 4: concurrent creation crashed D3D12/WARP in 6 of 15 runs).
//! - **The runtime's library outlives the Vulkan objects** (spec 4.1): the runtime may have hooked the instance's
//!   dispatch, so `vkDestroyDevice` and `vkDestroyInstance` must run while it is loaded. wgpu-hal destroys them in
//!   drop callbacks that hold a clone of the runtime's library, so the library goes only after both.
//! - **Images:** the runtime owns them and their memory. Each wrapper's drop callback is empty, and the memory is
//!   external. The wrappers go before `xrDestroySwapchain` ([`XrTargets::release`], through
//!   `FrameSink::release_swapchain`; review 1, L4).

use std::ffi::{CStr, c_char};
use std::fmt;

use ash::vk::{self, Handle};
use openxr as xr;
use pe_render::camera::{Camera, DepthRange, Fov as CameraFov};
use pe_render::gpu::{GpuContext, creation_lock};
use pe_render::{wgpu, wgpu_hal};
use pe_seams::RigTransform;

use crate::backend::EyePose;
use crate::real::XrInstance;

type Vk = wgpu_hal::api::Vulkan;

/// The lowest Vulkan version pe-xr asks for: wgpu needs 1.1 at least, and every runtime M0 met took 1.2 [V M0].
const VK_API: u32 = vk::make_api_version(0, 1, 2, 0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingError(pub String);

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "graphics binding: {}", self.0)
    }
}

impl std::error::Error for BindingError {}

fn be(what: &'static str) -> impl Fn(String) -> BindingError {
    move |e| BindingError(format!("{what}: {e}"))
}

/// The device features pe-render wants, where the adapter offers them: timestamps for the frame's GPU time, and
/// multiview with multisampled arrays for one-pass stereo (render.md 4, @ballpit-viz). This is
/// `pe_render::gpu::wanted_features` until that lands on main; it then replaces this.
pub fn wanted_features(offered: wgpu::Features) -> wgpu::Features {
    offered & (wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::MULTIVIEW | wgpu::Features::MULTISAMPLE_ARRAY)
}

/// A GPU context made through the runtime, and what `xrCreateSession` needs to bind to it.
pub struct VulkanContext {
    pub gpu: GpuContext,
    pub session_info: xr::vulkan::SessionCreateInfo,
}

/// Make the GPU context through the runtime (spec 4.1, the Vulkan steps 1 to 6).
///
/// # Safety
/// `xr` must have been created with `XR_KHR_vulkan_enable2` enabled, and `system` must be its headset. The returned
/// context must be used only with sessions of this instance.
pub unsafe fn vulkan_context(xr_instance: &XrInstance, system: xr::SystemId) -> Result<VulkanContext, BindingError> {
    let instance = &xr_instance.instance;
    let reqs =
        instance.graphics_requirements::<xr::Vulkan>(system).map_err(|e| be("graphics requirements")(e.to_string()))?;
    if xr::Version::new(1, 2, 0) < reqs.min_api_version_supported {
        return Err(BindingError(format!("the runtime needs Vulkan {} or newer", reqs.min_api_version_supported)));
    }

    let _one_at_a_time = creation_lock();
    // The Vulkan loader (vulkan-1.dll, libvulkan.so.1) is a system component, loaded at run time as wgpu does.
    // SAFETY: loading the system's Vulkan loader runs its initialisation, which is what wgpu does too.
    let entry = unsafe { ash::Entry::load() }.map_err(|e| be("the Vulkan loader")(e.to_string()))?;
    let flags = wgpu::InstanceFlags::empty();
    let inst_exts = wgpu_hal::vulkan::Instance::desired_extensions(&entry, VK_API, flags)
        .map_err(|e| be("instance extensions")(e.to_string()))?;
    let inst_ext_ptrs: Vec<*const c_char> = inst_exts.iter().map(|e| e.as_ptr()).collect();
    let app = vk::ApplicationInfo::default().api_version(VK_API).engine_name(c"Primordial Engine");
    let ici = vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&inst_ext_ptrs);
    // SAFETY: the same function, typed as OpenXR's PFN_vkGetInstanceProcAddr.
    let gipa = unsafe {
        std::mem::transmute::<vk::PFN_vkGetInstanceProcAddr, xr::sys::platform::VkGetInstanceProcAddr>(
            entry.static_fn().get_instance_proc_addr,
        )
    };
    // SAFETY: `ici` and everything it points to live across the call.
    let raw_inst = unsafe { instance.create_vulkan_instance(system, gipa, &ici as *const _ as *const _) }
        .map_err(|e| be("xrCreateVulkanInstanceKHR")(e.to_string()))?
        .map_err(|r| be("vkCreateInstance through the runtime")(format!("{:?}", vk::Result::from_raw(r))))?;
    // SAFETY: a live instance the runtime made with this entry's loader.
    let ash_inst = unsafe { ash::Instance::load(entry.static_fn(), vk::Instance::from_raw(raw_inst as _)) };

    // SAFETY: the instance is live.
    let phd_raw = unsafe { instance.vulkan_graphics_device(system, raw_inst) }
        .map_err(|e| be("xrGetVulkanGraphicsDevice2KHR")(e.to_string()))?;
    let phd = vk::PhysicalDevice::from_raw(phd_raw as _);

    let inst_drop: wgpu_hal::DropCallback = {
        let (inst, library) = (ash_inst.clone(), xr_instance.library.clone());
        Box::new(move || {
            // SAFETY: wgpu-hal calls this once, after its last use of the instance; the device (which holds the
            // instance) went first.
            unsafe { inst.destroy_instance(None) };
            drop(library);
        })
    };
    // SAFETY: a valid instance created with `inst_exts`, at VK_API; wgpu-hal calls `inst_drop` instead of destroying
    // it itself.
    let hal_inst = unsafe {
        wgpu_hal::vulkan::Instance::from_raw(
            entry,
            ash_inst.clone(),
            VK_API,
            0,
            None,
            inst_exts,
            flags,
            Default::default(),
            false,
            Some(inst_drop),
        )
    }
    .map_err(|e| be("wgpu-hal Instance::from_raw")(e.to_string()))?;
    let exposed =
        hal_inst.expose_adapter(phd).ok_or_else(|| BindingError("wgpu-hal cannot use the runtime's GPU".into()))?;

    let features = wanted_features(exposed.features);
    let dev_exts = exposed.adapter.required_device_extensions(features);
    let mut phd_features = exposed.adapter.physical_device_features(&dev_exts, features);
    // SAFETY: `phd` belongs to this instance.
    let family = unsafe { ash_inst.get_physical_device_queue_family_properties(phd) }
        .iter()
        .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .ok_or_else(|| BindingError("the runtime's GPU has no graphics queue".into()))? as u32;
    let dev_ext_ptrs: Vec<*const c_char> = dev_exts.iter().map(|e: &&CStr| e.as_ptr()).collect();
    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
    let dci = phd_features.add_to_device_create(
        vk::DeviceCreateInfo::default().queue_create_infos(&qci).enabled_extension_names(&dev_ext_ptrs),
    );
    // SAFETY: `dci` and everything it points to live across the call.
    let raw_dev =
        unsafe { instance.create_vulkan_device(system, gipa, phd.as_raw() as _, &dci as *const _ as *const _) }
            .map_err(|e| be("xrCreateVulkanDeviceKHR")(e.to_string()))?
            .map_err(|r| be("vkCreateDevice through the runtime")(format!("{:?}", vk::Result::from_raw(r))))?;
    // SAFETY: a live device of this instance.
    let ash_dev = unsafe { ash::Device::load(ash_inst.fp_v1_0(), vk::Device::from_raw(raw_dev as _)) };

    let dev_drop: wgpu_hal::DropCallback = {
        let (dev, library) = (ash_dev.clone(), xr_instance.library.clone());
        Box::new(move || {
            // SAFETY: wgpu-hal calls this once, after its last use of the device.
            unsafe { dev.destroy_device(None) };
            drop(library);
        })
    };
    let limits = wgpu::Limits::default();
    // SAFETY: the device was created with `dev_exts` and `phd_features`, with one queue in `family`.
    let open = unsafe {
        exposed.adapter.device_from_raw(
            ash_dev,
            Some(dev_drop),
            &dev_exts,
            features,
            &limits,
            &wgpu::MemoryHints::default(),
            family,
            0,
        )
    }
    .map_err(|e| be("wgpu-hal device_from_raw")(e.to_string()))?;
    // SAFETY: the hal objects above, handed to wgpu whole.
    let wgpu_instance = unsafe { wgpu::Instance::from_hal::<Vk>(hal_inst) };
    let adapter = unsafe { wgpu_instance.create_adapter_from_hal(exposed) };
    let desc = wgpu::DeviceDescriptor {
        label: Some("pe-render (through OpenXR)"),
        required_features: features,
        required_limits: limits,
        ..Default::default()
    };
    // SAFETY: `open` was made on this adapter with these features and limits.
    let (device, queue) = unsafe { adapter.create_device_from_hal(open, &desc) }
        .map_err(|e| be("wgpu create_device_from_hal")(e.to_string()))?;

    Ok(VulkanContext {
        gpu: GpuContext { instance: wgpu_instance, adapter, device, queue },
        session_info: xr::vulkan::SessionCreateInfo {
            instance: raw_inst,
            physical_device: phd.as_raw() as _,
            device: raw_dev,
            queue_family_index: family,
            queue_index: 0,
        },
    })
}

/// The wgpu format of a Vulkan swapchain format pe-xr picks (spec 4.2; [`crate::real_session::srgb_rank`]).
pub fn vulkan_format(raw: i64) -> Option<wgpu::TextureFormat> {
    match vk::Format::from_raw(raw as i32) {
        vk::Format::R8G8B8A8_SRGB => Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        vk::Format::B8G8R8A8_SRGB => Some(wgpu::TextureFormat::Bgra8UnormSrgb),
        vk::Format::D32_SFLOAT => Some(wgpu::TextureFormat::Depth32Float),
        _ => None,
    }
}

/// The swapchain's images as wgpu textures, one per image, each with both eyes' layers (spec 4.2). The frame's
/// target is the texture at its `ImageIndex`.
pub struct XrTargets {
    color: Vec<wgpu::Texture>,
    pub format: wgpu::TextureFormat,
    /// The swapchain's size; a frame draws into `extent` from the origin, which the projection layer's `imageRect`
    /// matches (render scale as a viewport, @ballpit-viz).
    pub size: [u32; 2],
}

impl XrTargets {
    /// Wrap Vulkan swapchain images (`xrEnumerateSwapchainImages`), as created by `RealBackend`: 2 layers, colour
    /// attachment and transfer source, in the colour-target state the runtime hands them over in.
    ///
    /// # Safety
    /// `images` must be the live swapchain's images, of `raw_format` and `size`, on `gpu`'s device; the textures must
    /// be dropped ([`Self::release`]) before the swapchain is destroyed.
    pub unsafe fn vulkan(
        gpu: &GpuContext,
        images: &[u64],
        raw_format: i64,
        size: [u32; 2],
    ) -> Result<XrTargets, BindingError> {
        let format = vulkan_format(raw_format)
            .filter(|f| f.is_srgb())
            .ok_or_else(|| BindingError(format!("swapchain format {raw_format} is not one pe-render draws to")))?;
        let extent = wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 2 };
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC;
        let mut color = Vec::with_capacity(images.len());
        for &img in images {
            let desc = wgpu::TextureDescriptor {
                label: Some("xr swapchain"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            };
            let hal_desc = wgpu_hal::TextureDescriptor {
                label: desc.label,
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: desc.dimension,
                format,
                usage: wgpu::TextureUses::COLOR_TARGET | wgpu::TextureUses::COPY_SRC,
                memory_flags: wgpu_hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            };
            // SAFETY: the device is Vulkan (made by `vulkan_context`).
            let hd = unsafe { gpu.device.as_hal::<Vk>() }
                .ok_or_else(|| BindingError("the GPU context is not Vulkan".into()))?;
            // SAFETY: the caller's contract. The runtime owns the image and its memory: an empty drop callback and
            // external memory, so wgpu never frees either.
            let hal = unsafe {
                hd.texture_from_raw(
                    vk::Image::from_raw(img),
                    &hal_desc,
                    Some(Box::new(|| {})),
                    wgpu_hal::vulkan::TextureMemory::External,
                )
            };
            drop(hd);
            // SAFETY: `hal` matches `desc`; the runtime hands images over as colour targets (spec 4.2).
            color
                .push(unsafe { gpu.device.create_texture_from_hal::<Vk>(hal, &desc, wgpu::TextureUses::COLOR_TARGET) });
        }
        Ok(XrTargets { color, format, size })
    }

    /// The colour texture of the acquired image, both layers.
    pub fn color(&self, image: usize) -> Option<&wgpu::Texture> {
        self.color.get(image)
    }

    pub fn len(&self) -> usize {
        self.color.len()
    }

    pub fn is_empty(&self) -> bool {
        self.color.is_empty()
    }

    /// Drop every wrapper, once the GPU is idle on them (review 1, L4): `FrameSink::release_swapchain` calls this,
    /// before `xrDestroySwapchain`.
    pub fn release(&mut self, gpu: &GpuContext) {
        if self.color.is_empty() {
            return;
        }
        // Waits until every submission so far has finished on the GPU.
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        self.color.clear();
    }
}

/// Both eyes' cameras, from the frame's eye poses (rig frame) and the rig (spec 3.2; render.md 5).
pub fn eye_cameras(rig: &RigTransform, eyes: &[EyePose; 2]) -> [Camera; 2] {
    eyes.map(|e| {
        let fov = CameraFov { left: e.fov.left, right: e.fov.right, up: e.fov.up, down: e.fov.down };
        Camera::of_eye(rig, &e.pose, fov, DepthRange::VR)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swapchain_formats_map_to_srgb_and_depth() {
        // VK_FORMAT_R8G8B8A8_SRGB 43, B8G8R8A8_SRGB 50, D32_SFLOAT 126, R8G8B8A8_UNORM 37.
        assert_eq!(vulkan_format(43), Some(wgpu::TextureFormat::Rgba8UnormSrgb));
        assert_eq!(vulkan_format(50), Some(wgpu::TextureFormat::Bgra8UnormSrgb));
        assert_eq!(vulkan_format(126), Some(wgpu::TextureFormat::Depth32Float));
        assert_eq!(vulkan_format(37), None, "UNORM would show the headset image too dark");
    }

    #[test]
    fn wanted_features_never_asks_for_more_than_offered() {
        let offered = wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::DEPTH_CLIP_CONTROL;
        assert_eq!(wanted_features(offered), wgpu::Features::TIMESTAMP_QUERY);
        assert_eq!(wanted_features(wgpu::Features::empty()), wgpu::Features::empty());
    }

    #[test]
    fn eye_cameras_scale_with_the_rig() {
        let eye = EyePose {
            pose: pe_seams::RigPose { position: [0.03, 1.6, 0.0], orientation: [0.0, 0.0, 0.0, 1.0] },
            fov: crate::backend::Fov { left: -0.8, right: 0.7, up: 0.75, down: -0.8 },
        };
        let rig = RigTransform { pos: pe_seams::Pos::ORIGIN, rotation: [0.0, 0.0, 0.0, 1.0], scale: 10.0 };
        let [l, _] = eye_cameras(&rig, &[eye, eye]);
        assert_eq!(l.fov.left, -0.8);
        assert!((l.near - 0.2).abs() < 1e-9 && l.far == Some(1000.0), "near and far at rig scale 10");
    }
}
