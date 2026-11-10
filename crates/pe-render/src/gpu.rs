//! The GPU context (render.md 4): accepted from outside (pe-xr makes it on the runtime's device in VR), or made
//! here for the desktop and for headless captures.
//!
//! - **Vulkan first,** on Windows too [R coordinator, on review M1]; D3D12 is the fallback.
//! - **The DX12 shader compiler is pinned to FXC** (`d3dcompiler_47.dll`, part of Windows). wgpu's default `Auto`
//!   would load any `dxcompiler.dll` found on the PATH, an unregistered native boundary (render.md 2).
//! - **No GLES:** the crate's wgpu features leave it out, so no khronos-egl and no `opengl32.dll` import.
//! - **Limits:** `Limits::default()`, never an adapter's full limits (wgpu-core asserts `max_buffer_size <=
//!   u32::MAX`, and full limits break it [V M0]).

use std::fmt;
use std::sync::Mutex;

/// Instances, adapters and devices are made one at a time in a process. Made concurrently on D3D12 (WARP, FXC),
/// the process segfaulted in 6 of 15 runs of the GPU tests; serialized, 0 of 50 [V 2026-11-10, render/m1]. Which
/// layer races (wgpu-hal's dx12, FXC's loading, or WARP) is not yet known. Creation is rare, so the lock costs
/// nothing.
static CREATE: Mutex<()> = Mutex::new(());

fn creating() -> std::sync::MutexGuard<'static, ()> {
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

/// The backends pe-render enables: Vulkan everywhere, plus D3D12 on Windows.
pub fn backends() -> wgpu::Backends {
    if cfg!(windows) { wgpu::Backends::VULKAN | wgpu::Backends::DX12 } else { wgpu::Backends::VULKAN }
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
    /// A context for the desktop window: Vulkan first, then D3D12, on the best adapter that can present to
    /// `surface` (when given).
    pub fn desktop(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<GpuContext, ContextError> {
        let all = {
            let _one_at_a_time = creating();
            pollster::block_on(instance.enumerate_adapters(backends()))
        };
        let mut adapters: Vec<wgpu::Adapter> =
            all.into_iter().filter(|a| surface.is_none_or(|s| a.is_surface_supported(s))).collect();
        // Vulkan before D3D12 on any one class of device; hardware before software.
        adapters.sort_by_key(|a| {
            let (kind, backend) = rank(a);
            (backend, kind)
        });
        let hw = adapters.iter().position(|a| rank(a).0 < 4);
        let adapter = match hw {
            Some(i) => adapters.swap_remove(i),
            None => adapters.into_iter().next().ok_or(ContextError::NoAdapter)?,
        };
        Self::on(instance, adapter)
    }

    /// A context with no window, for captures and tests.
    pub fn headless(choice: AdapterChoice) -> Result<GpuContext, ContextError> {
        Self::headless_on(backends(), choice)
    }

    /// [`GpuContext::headless`] on the given backends only.
    pub fn headless_on(backends: wgpu::Backends, choice: AdapterChoice) -> Result<GpuContext, ContextError> {
        let (instance, adapter) = {
            let _one_at_a_time = creating();
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
        let desc = wgpu::DeviceDescriptor {
            label: Some("pe-render"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        };
        let _one_at_a_time = creating();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&desc)).map_err(|e| ContextError::Device(e.to_string()))?;
        Ok(GpuContext { instance, adapter, device, queue })
    }

    /// The adapter's name and backend, for reports.
    pub fn describe(&self) -> String {
        let i = self.adapter.get_info();
        format!("{} ({:?}, {:?})", i.name, i.backend, i.device_type)
    }
}
