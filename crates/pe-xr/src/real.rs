//! The real runtime, at the instance level: the extensions pe-xr enables, the headset lookup, and the
//! conversion between `XrTime` and host time (spec 2, 3.3, 6.1).
//!
//! Nothing here runs in tests except [`choose_extensions`], which is a pure function. Creating an
//! instance needs a [`Runtime`] from the loader, which needs the headset guard's permit.

use std::sync::Arc;

use openxr::{ExtensionSet, sys};

use crate::loader::{Runtime, RuntimeLibrary};
use crate::seam::HostTime;

/// The graphics API the session draws with (spec 4): Vulkan on Windows and Linux, D3D12 as Windows'
/// fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsPath {
    Vulkan,
    D3D12,
}

/// What [`choose_extensions`] could not find.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingExtension(pub &'static str);

impl std::fmt::Display for MissingExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the OpenXR runtime does not offer {}", self.0)
    }
}

/// The extensions to enable, from those the runtime offers (spec 2's table). The graphics extension for
/// `path` and this platform's time conversion are required; the rest are used when offered.
pub fn choose_extensions(
    available: &ExtensionSet,
    path: GraphicsPath,
) -> Result<ExtensionSet, MissingExtension> {
    let mut want = ExtensionSet::default();
    match path {
        GraphicsPath::Vulkan => {
            if !available.khr_vulkan_enable2 {
                return Err(MissingExtension("XR_KHR_vulkan_enable2"));
            }
            want.khr_vulkan_enable2 = true;
        }
        GraphicsPath::D3D12 => {
            #[cfg(windows)]
            {
                if !available.khr_d3d12_enable {
                    return Err(MissingExtension("XR_KHR_D3D12_enable"));
                }
                want.khr_d3d12_enable = true;
            }
            #[cfg(not(windows))]
            return Err(MissingExtension("XR_KHR_D3D12_enable (Windows only)"));
        }
    }
    #[cfg(windows)]
    {
        if !available.khr_win32_convert_performance_counter_time {
            return Err(MissingExtension(
                "XR_KHR_win32_convert_performance_counter_time",
            ));
        }
        want.khr_win32_convert_performance_counter_time = true;
    }
    #[cfg(unix)]
    {
        if !available.khr_convert_timespec_time {
            return Err(MissingExtension("XR_KHR_convert_timespec_time"));
        }
        want.khr_convert_timespec_time = true;
    }
    want.ext_hand_tracking = available.ext_hand_tracking;
    want.ext_hand_interaction = available.ext_hand_interaction;
    want.fb_display_refresh_rate = available.fb_display_refresh_rate;
    want.ext_local_floor = available.ext_local_floor;
    want.khr_composition_layer_depth = available.khr_composition_layer_depth;
    Ok(want)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstanceError {
    Missing(MissingExtension),
    Xr(String),
}

impl std::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceError::Missing(m) => write!(f, "{m}"),
            InstanceError::Xr(e) => write!(f, "OpenXR: {e}"),
        }
    }
}

/// Whether a headset is there to start a session on (spec 3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Headset {
    Present(openxr::SystemId),
    /// The runtime is up but its headset is not connected (`XR_ERROR_FORM_FACTOR_UNAVAILABLE`, as on this
    /// PC with the headset off [V: M0 report]): desktop mode, and retry later.
    Absent,
    /// The runtime has no head-mounted form factor at all.
    Unsupported,
}

/// An OpenXR instance, holding the runtime's library loaded for as long as it lives.
pub struct XrInstance {
    pub instance: openxr::Instance,
    pub path: GraphicsPath,
    pub enabled: ExtensionSet,
    /// Keeps the runtime's library loaded. A GPU context made through this instance clones it too
    /// (spec 4.1), so the library outlives the instance when it must.
    pub library: Arc<RuntimeLibrary>,
}

impl XrInstance {
    pub fn create(runtime: &Runtime, path: GraphicsPath) -> Result<XrInstance, InstanceError> {
        let available = runtime
            .entry
            .enumerate_extensions()
            .map_err(|e| InstanceError::Xr(e.to_string()))?;
        let enabled = choose_extensions(&available, path).map_err(InstanceError::Missing)?;
        let app = openxr::ApplicationInfo {
            application_name: "Primordial Engine",
            application_version: 0,
            engine_name: "Primordial Engine",
            engine_version: 0,
            api_version: runtime.instance_api_version(),
        };
        let instance = runtime
            .entry
            .create_instance(&app, &enabled, &[], &())
            .map_err(|e| InstanceError::Xr(e.to_string()))?;
        Ok(XrInstance {
            instance,
            path,
            enabled,
            library: runtime.library.clone(),
        })
    }

    /// The head-mounted system, or why there is none now.
    pub fn headset(&self) -> Result<Headset, InstanceError> {
        match self
            .instance
            .system(openxr::FormFactor::HEAD_MOUNTED_DISPLAY)
        {
            Ok(id) => Ok(Headset::Present(id)),
            Err(sys::Result::ERROR_FORM_FACTOR_UNAVAILABLE) => Ok(Headset::Absent),
            Err(sys::Result::ERROR_FORM_FACTOR_UNSUPPORTED) => Ok(Headset::Unsupported),
            Err(e) => Err(InstanceError::Xr(e.to_string())),
        }
    }

    /// `XrTime` to host time.
    #[cfg(windows)]
    pub fn to_host(&self, t: sys::Time) -> Result<HostTime, InstanceError> {
        let ext = self
            .instance
            .exts()
            .khr_win32_convert_performance_counter_time
            .as_ref()
            .ok_or(InstanceError::Missing(MissingExtension(
                "XR_KHR_win32_convert_performance_counter_time",
            )))?;
        let mut counter: sys::platform::LARGE_INTEGER = 0;
        // SAFETY: a live instance, the extension enabled at creation, and a valid out pointer.
        let r = unsafe {
            (ext.convert_time_to_win32_performance_counter)(self.instance.as_raw(), t, &mut counter)
        };
        if r != sys::Result::SUCCESS {
            return Err(InstanceError::Xr(format!(
                "xrConvertTimeToWin32PerformanceCounterKHR: {r:?}"
            )));
        }
        Ok(crate::clock::counter_to_host(counter))
    }

    /// Host time to `XrTime`.
    #[cfg(windows)]
    pub fn to_xr(&self, t: HostTime) -> Result<sys::Time, InstanceError> {
        let ext = self
            .instance
            .exts()
            .khr_win32_convert_performance_counter_time
            .as_ref()
            .ok_or(InstanceError::Missing(MissingExtension(
                "XR_KHR_win32_convert_performance_counter_time",
            )))?;
        let counter: sys::platform::LARGE_INTEGER = crate::clock::host_to_counter(t);
        let mut out = sys::Time::from_nanos(0);
        // SAFETY: as above.
        let r = unsafe {
            (ext.convert_win32_performance_counter_to_time)(
                self.instance.as_raw(),
                &counter,
                &mut out,
            )
        };
        if r != sys::Result::SUCCESS {
            return Err(InstanceError::Xr(format!(
                "xrConvertWin32PerformanceCounterToTimeKHR: {r:?}"
            )));
        }
        Ok(out)
    }

    #[cfg(unix)]
    pub fn to_host(&self, t: sys::Time) -> Result<HostTime, InstanceError> {
        let ext = self
            .instance
            .exts()
            .khr_convert_timespec_time
            .as_ref()
            .ok_or(InstanceError::Missing(MissingExtension(
                "XR_KHR_convert_timespec_time",
            )))?;
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a live instance, the extension enabled at creation, and a valid out pointer.
        let r = unsafe { (ext.convert_time_to_timespec_time)(self.instance.as_raw(), t, &mut ts) };
        if r != sys::Result::SUCCESS {
            return Err(InstanceError::Xr(format!(
                "xrConvertTimeToTimespecTimeKHR: {r:?}"
            )));
        }
        Ok(crate::clock::timespec_to_host(&ts))
    }

    #[cfg(unix)]
    pub fn to_xr(&self, t: HostTime) -> Result<sys::Time, InstanceError> {
        let ext = self
            .instance
            .exts()
            .khr_convert_timespec_time
            .as_ref()
            .ok_or(InstanceError::Missing(MissingExtension(
                "XR_KHR_convert_timespec_time",
            )))?;
        let ts = crate::clock::host_to_timespec(t);
        let mut out = sys::Time::from_nanos(0);
        // SAFETY: as above.
        let r =
            unsafe { (ext.convert_timespec_time_to_time)(self.instance.as_raw(), &ts, &mut out) };
        if r != sys::Result::SUCCESS {
            return Err(InstanceError::Xr(format!(
                "xrConvertTimespecTimeToTimeKHR: {r:?}"
            )));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered() -> ExtensionSet {
        // What Virtual Desktop's runtime offered here [V: M0 report], plus the optional ones.
        let mut e = ExtensionSet::default();
        e.khr_vulkan_enable2 = true;
        #[cfg(windows)]
        {
            e.khr_d3d12_enable = true;
            e.khr_win32_convert_performance_counter_time = true;
        }
        #[cfg(unix)]
        {
            e.khr_convert_timespec_time = true;
        }
        e.ext_hand_tracking = true;
        e
    }

    #[test]
    fn vulkan_first_takes_the_required_and_the_offered_optionals() {
        let e = choose_extensions(&offered(), GraphicsPath::Vulkan).unwrap();
        assert!(e.khr_vulkan_enable2 && e.ext_hand_tracking);
        assert!(
            !e.fb_display_refresh_rate && !e.ext_hand_interaction,
            "not offered, not enabled"
        );
        #[cfg(windows)]
        assert!(!e.khr_d3d12_enable, "only one graphics path");
    }

    #[test]
    fn a_missing_graphics_extension_is_named() {
        let mut a = offered();
        a.khr_vulkan_enable2 = false;
        assert_eq!(
            choose_extensions(&a, GraphicsPath::Vulkan),
            Err(MissingExtension("XR_KHR_vulkan_enable2"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_d3d12_fallback_and_the_time_conversion() {
        let e = choose_extensions(&offered(), GraphicsPath::D3D12).unwrap();
        assert!(e.khr_d3d12_enable && !e.khr_vulkan_enable2);
        let mut a = offered();
        a.khr_win32_convert_performance_counter_time = false;
        assert!(
            choose_extensions(&a, GraphicsPath::Vulkan).is_err(),
            "no shared time base, no VR"
        );
    }
}
