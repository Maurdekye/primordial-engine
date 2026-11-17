//! The GPU context (render.md 4): accepted from outside (pe-xr makes it on the runtime's device in VR), or made
//! here for the desktop and for headless captures.
//!
//! - **Backend order:** headless contexts and VR (pe-xr) use Vulkan first [R coordinator, on review M1]. A
//!   desktop-only window on Windows tries D3D12 first, then Vulkan, each in its own instance, so a working D3D12
//!   never loads the Vulkan driver: Intel's (igvk64.dll 31.0.101.2134, UHD 620) crashed when the window opened
//!   [R coordinator mf5033d135994, after @fable-reviewer mae2f5aa0eaae]. [`BACKEND_VAR`] forces either one.
//! - **Logged:** every context made records its backend, adapter and driver ([`record_context`]): the embedding
//!   prints them at start ([`set_context_log`]) and adds them to fault bundles ([`contexts_made`]).
//! - **The DX12 shader compiler is pinned to FXC** (`d3dcompiler_47.dll`, part of Windows). wgpu's default `Auto`
//!   would load any `dxcompiler.dll` found on the PATH, an unregistered native boundary (render.md 2).
//! - **No GLES:** the crate's wgpu features leave it out, so no khronos-egl and no `opengl32.dll` import.
//! - **Limits:** `Limits::default()`, never an adapter's full limits (wgpu-core asserts `max_buffer_size <=
//!   u32::MAX`, and full limits break it [V M0]).

use std::fmt;
use std::sync::{Mutex, OnceLock};

/// Instances, adapters and devices are made one at a time in a process. Made concurrently on D3D12 (WARP, FXC),
/// the process segfaulted in 6 of 15 runs of the GPU tests; serialized, 0 of 50 [V 2026-11-10, render/m1]. Which
/// layer races (wgpu-hal's dx12, FXC's loading, or WARP) is not yet known, so the lock also covers pe-render's
/// shader compiles (`Renderer::new`), in case it is FXC. It does not cover an `Instance` the caller makes itself
/// for [`GpuContext::desktop`] (pe-app's window) or pe-xr's runtime-made context: make those one at a time too.
/// Creation is rare, so the lock costs nothing.
static CREATE: Mutex<()> = Mutex::new(());

/// The process-wide creation lock. pe-xr and pe-app take it around any instance, adapter, device or pipeline
/// they make themselves (app.md: every context is made on the main thread, one at a time, XR's first).
pub fn creation_lock() -> std::sync::MutexGuard<'static, ()> {
    CREATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The device pe-render draws with. In VR mode pe-xr builds it through the runtime and hands it in.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextError {
    /// No adapter of the kind asked for (a capture reports `capture_unavailable`).
    NoAdapter,
    Device(String),
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContextError::NoAdapter => write!(f, "no GPU adapter"),
            ContextError::Device(e) => write!(f, "device: {e}"),
        }
    }
}

impl std::error::Error for ContextError {}

/// Which adapters a headless context may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterChoice {
    /// A software rasterizer only (WARP on Windows, lavapipe on Linux; plan 4.6). The gate's tests use this, so a
    /// test never draws on the user's GPU without a go.
    SoftwareOnly,
    /// The best hardware adapter, else software.
    Any,
}

/// The features pe-render asks for, of those the adapter offers: timestamp queries for the frame's GPU time
/// (render.md 10), and multiview with multisampled array textures for one-pass stereo (render.md 5). pe-xr opens its
/// device through the runtime with exactly these.
pub fn wanted_features(offered: wgpu::Features) -> wgpu::Features {
    offered & (wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::MULTIVIEW | wgpu::Features::MULTISAMPLE_ARRAY)
}

/// The limits pe-render asks for: wgpu's defaults (never an adapter's full limits, see the module header), plus two
/// multiview views where the adapter has them. The default allows none, so without this every two-view pass is
/// refused ("Multiview view count limit violated", seen on the RTX's Vulkan at render/xr 99c0d4f7). pe-xr opens its
/// device through the runtime with these.
pub fn wanted_limits(offered: &wgpu::Limits) -> wgpu::Limits {
    wgpu::Limits {
        max_multiview_view_count: if offered.max_multiview_view_count >= 2 { 2 } else { 0 },
        ..wgpu::Limits::default()
    }
}

/// The backends pe-render enables: Vulkan everywhere, plus D3D12 on Windows.
pub fn backends() -> wgpu::Backends {
    if cfg!(windows) { wgpu::Backends::VULKAN | wgpu::Backends::DX12 } else { wgpu::Backends::VULKAN }
}

/// The environment variable that forces one backend for every context pe-render makes itself (the desktop window
/// and headless contexts): `vulkan`, or `dx12` (also `d3d12`), in any case. Unset or empty: pe-render's own order.
/// pe-xr's context is the runtime's Vulkan device and ignores it.
pub const BACKEND_VAR: &str = "PE_RENDER_BACKEND";

/// A backend forced through [`BACKEND_VAR`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendOverride {
    Vulkan,
    Dx12,
}

impl BackendOverride {
    /// `vulkan`, `dx12` or `d3d12`, in any case and with surrounding spaces; `None` for an empty value.
    pub fn parse(value: &str) -> Result<Option<BackendOverride>, ContextError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" => Ok(None),
            "vulkan" => Ok(Some(BackendOverride::Vulkan)),
            "dx12" | "d3d12" => Ok(Some(BackendOverride::Dx12)),
            other => Err(ContextError::Device(format!("{BACKEND_VAR}={other:?}: use vulkan or dx12"))),
        }
    }

    /// The override [`BACKEND_VAR`] sets, if any. A value that isn't a backend is an error, never ignored.
    pub fn from_env() -> Result<Option<BackendOverride>, ContextError> {
        match std::env::var(BACKEND_VAR) {
            Ok(v) => Self::parse(&v),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(e) => Err(ContextError::Device(format!("{BACKEND_VAR}: {e}"))),
        }
    }

    pub fn backends(self) -> wgpu::Backends {
        match self {
            BackendOverride::Vulkan => wgpu::Backends::VULKAN,
            BackendOverride::Dx12 => wgpu::Backends::DX12,
        }
    }
}

/// The backends a desktop-only window tries, in order, each in an instance of its own: the override's alone, else
/// D3D12 then Vulkan on Windows, Vulkan elsewhere.
pub fn desktop_order(over: Option<BackendOverride>) -> Vec<wgpu::Backends> {
    match over {
        Some(o) => vec![o.backends()],
        None if cfg!(windows) => vec![wgpu::Backends::DX12, wgpu::Backends::VULKAN],
        None => vec![wgpu::Backends::VULKAN],
    }
}

/// The backends a headless context may use: the override's alone, else [`backends`] (Vulkan ranked first).
pub fn headless_backends() -> Result<wgpu::Backends, ContextError> {
    Ok(BackendOverride::from_env()?.map_or_else(backends, BackendOverride::backends))
}

/// How many described contexts [`contexts_made`] keeps (the newest).
const MADE_KEPT: usize = 8;
static MADE: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LOG: OnceLock<fn(&str)> = OnceLock::new();

/// Where each context's description goes as it is made (pe-app: its console at start). Set once; later calls are
/// ignored.
pub fn set_context_log(log: fn(&str)) {
    let _ = LOG.set(log);
}

/// Note a context just made: its description goes to the log ([`set_context_log`]) and to [`contexts_made`].
/// [`GpuContext::on`] calls it; a caller that builds a [`GpuContext`] itself (pe-xr) calls it too.
pub fn record_context(ctx: &GpuContext) {
    let d = ctx.describe();
    {
        let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
        made.push(d.clone());
        let extra = made.len().saturating_sub(MADE_KEPT);
        made.drain(..extra);
    }
    if let Some(log) = LOG.get() {
        log(&d);
    }
}

/// The contexts this process has made, oldest first (the newest [`MADE_KEPT`]), for a fault bundle's build line.
pub fn contexts_made() -> Vec<String> {
    MADE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The instance descriptor every pe-render instance uses: FXC pinned, no environment overrides.
pub fn instance_descriptor(backends: wgpu::Backends) -> wgpu::InstanceDescriptor {
    let mut d = wgpu::InstanceDescriptor::new_without_display_handle();
    d.backends = backends;
    d.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
    d
}

fn rank(a: &wgpu::Adapter) -> (u8, u8) {
    let info = a.get_info();
    let kind = match info.device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    };
    let backend = if info.backend == wgpu::Backend::Vulkan { 0 } else { 1 };
    (kind, backend)
}

impl GpuContext {
    /// A context for a desktop window on the best of `instance`'s adapters that can present to `surface` (when
    /// given): hardware only, or software too when `software`. The window makes one instance per backend, in
    /// [`desktop_order`] (window.rs, `surface_and_context`).
    pub fn desktop(
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        software: bool,
    ) -> Result<GpuContext, ContextError> {
        let all = {
            let _one_at_a_time = creation_lock();
            pollster::block_on(instance.enumerate_adapters(backends()))
        };
        let mut adapters: Vec<wgpu::Adapter> = all
            .into_iter()
            .filter(|a| surface.is_none_or(|s| a.is_surface_supported(s)))
            .filter(|a| software || rank(a).0 < 4)
            .collect();
        // Hardware before software, then Vulkan before D3D12 (each of the window's instances enables one).
        adapters.sort_by_key(rank);
        let adapter = adapters.into_iter().next().ok_or(ContextError::NoAdapter)?;
        Self::on(instance, adapter)
    }

    /// A context with no window, for captures and tests, on [`headless_backends`].
    pub fn headless(choice: AdapterChoice) -> Result<GpuContext, ContextError> {
        Self::headless_on(headless_backends()?, choice)
    }

    /// [`GpuContext::headless`] on the given backends only.
    pub fn headless_on(backends: wgpu::Backends, choice: AdapterChoice) -> Result<GpuContext, ContextError> {
        let (instance, adapter) = {
            let _one_at_a_time = creation_lock();
            let instance = wgpu::Instance::new(instance_descriptor(backends));
            let mut adapters: Vec<wgpu::Adapter> = pollster::block_on(instance.enumerate_adapters(backends));
            adapters.retain(|a| choice == AdapterChoice::Any || a.get_info().device_type == wgpu::DeviceType::Cpu);
            adapters.sort_by_key(rank);
            (instance, adapters.into_iter().next().ok_or(ContextError::NoAdapter)?)
        };
        Self::on(instance, adapter)
    }

    /// A device on `adapter` with pe-render's own limits.
    pub fn on(instance: wgpu::Instance, adapter: wgpu::Adapter) -> Result<GpuContext, ContextError> {
        // Timestamp queries where the adapter has them: the frame's GPU time feeds the quality ladder (render.md 10).
        let desc = wgpu::DeviceDescriptor {
            label: Some("pe-render"),
            required_features: wanted_features(adapter.features()),
            required_limits: wanted_limits(&adapter.limits()),
            ..Default::default()
        };
        let (device, queue) = {
            let _one_at_a_time = creation_lock();
            pollster::block_on(adapter.request_device(&desc)).map_err(|e| ContextError::Device(e.to_string()))?
        };
        let ctx = GpuContext { instance, adapter, device, queue };
        record_context(&ctx);
        Ok(ctx)
    }

    /// The adapter's name, backend and kind, its PCI vendor and device ids, and its driver and driver version as
    /// wgpu reports them (either may be empty), for logs, reports and fault bundles.
    pub fn describe(&self) -> String {
        describe_info(&self.adapter.get_info())
    }
}

fn describe_info(i: &wgpu::AdapterInfo) -> String {
    let mut s = format!("{} ({:?}, {:?}) [{:04x}:{:04x}]", i.name, i.backend, i.device_type, i.vendor, i.device);
    let driver: Vec<&str> = [i.driver.trim(), i.driver_info.trim()].into_iter().filter(|x| !x.is_empty()).collect();
    if !driver.is_empty() {
        s.push_str(", driver ");
        s.push_str(&driver.join(" "));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Multiview needs its view count raised from wgpu's default of 0, but never past what the adapter offers.
    #[test]
    fn the_limits_allow_two_views_only_where_offered() {
        let rich = wgpu::Limits { max_multiview_view_count: 6, ..wgpu::Limits::default() };
        assert_eq!(wanted_limits(&rich).max_multiview_view_count, 2);
        assert_eq!(wanted_limits(&wgpu::Limits::default()).max_multiview_view_count, 0);
        assert_eq!(wanted_limits(&rich).max_buffer_size, wgpu::Limits::default().max_buffer_size);
    }

    /// The override takes either backend in any case, an empty value is no override, and anything else is refused.
    #[test]
    fn the_override_names_one_backend_or_is_refused() {
        assert_eq!(BackendOverride::parse(" Vulkan "), Ok(Some(BackendOverride::Vulkan)));
        assert_eq!(BackendOverride::parse("DX12"), Ok(Some(BackendOverride::Dx12)));
        assert_eq!(BackendOverride::parse("d3d12"), Ok(Some(BackendOverride::Dx12)));
        assert_eq!(BackendOverride::parse(""), Ok(None));
        assert!(BackendOverride::parse("gl").is_err());
    }

    /// A desktop-only window tries D3D12 before Vulkan on Windows, each alone; an override leaves one backend.
    #[test]
    fn the_desktop_tries_d3d12_first_on_windows_unless_overridden() {
        let want = if cfg!(windows) {
            vec![wgpu::Backends::DX12, wgpu::Backends::VULKAN]
        } else {
            vec![wgpu::Backends::VULKAN]
        };
        assert_eq!(desktop_order(None), want);
        assert_eq!(desktop_order(Some(BackendOverride::Vulkan)), [wgpu::Backends::VULKAN]);
        assert_eq!(desktop_order(Some(BackendOverride::Dx12)), [wgpu::Backends::DX12]);
    }

    /// The description names the driver and its version when wgpu has them, and leaves them out when empty.
    #[test]
    fn the_description_carries_the_driver() {
        let mut i = wgpu::AdapterInfo {
            name: "Intel(R) UHD Graphics 620".into(),
            vendor: 0x8086,
            device: 0x5917,
            device_type: wgpu::DeviceType::IntegratedGpu,
            device_pci_bus_id: String::new(),
            driver: "Intel Corporation".into(),
            driver_info: "31.0.101.2134".into(),
            backend: wgpu::Backend::Dx12,
            subgroup_min_size: 0,
            subgroup_max_size: 0,
            transient_saves_memory: None,
            limit_bucket: None,
        };
        assert_eq!(
            describe_info(&i),
            "Intel(R) UHD Graphics 620 (Dx12, IntegratedGpu) [8086:5917], driver Intel Corporation 31.0.101.2134"
        );
        i.driver.clear();
        i.driver_info.clear();
        assert_eq!(describe_info(&i), "Intel(R) UHD Graphics 620 (Dx12, IntegratedGpu) [8086:5917]");
    }
}
