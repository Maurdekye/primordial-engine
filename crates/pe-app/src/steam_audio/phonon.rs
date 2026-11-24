//! The subset of the official Steam Audio 4.8.1 phonon.h used by the trial.
use libloading::Library;
use std::{ffi::c_void, path::Path, ptr::NonNull, sync::Arc};

type Handle = *mut c_void;
type Create<S> = unsafe extern "system" fn(Handle, *mut AudioSettings, *mut S, *mut Handle) -> i32;
type Apply<P> = unsafe extern "system" fn(Handle, *mut P, *mut Buffer, *mut Buffer) -> i32;
type Release = unsafe extern "system" fn(*mut Handle);

#[repr(C)]
struct ContextSettings {
    version: u32,
    log: Option<unsafe extern "system" fn(i32, *const i8)>,
    allocate: Option<unsafe extern "system" fn(usize, usize) -> *mut c_void>,
    free: Option<unsafe extern "system" fn(*mut c_void)>,
    simd: i32,
    flags: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Orientation {
    pub right: Vec3,
    pub up: Vec3,
    pub ahead: Vec3,
    pub origin: Vec3,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioSettings {
    rate: i32,
    frames: i32,
}

#[repr(C)]
struct Buffer {
    channels: i32,
    frames: i32,
    data: *mut *mut f32,
}

#[repr(C)]
struct HrtfSettings {
    kind: i32,
    sofa_path: *const i8,
    sofa_data: *const u8,
    sofa_size: i32,
    volume: f32,
    normalization: i32,
}

#[repr(C)]
struct BinauralSettings {
    hrtf: Handle,
}

#[repr(C)]
struct BinauralParams {
    direction: Vec3,
    interpolation: i32,
    spatial_blend: f32,
    hrtf: Handle,
    peak_delays: *mut f32,
}

#[repr(C)]
struct EncodeSettings {
    order: i32,
}

#[repr(C)]
struct EncodeParams {
    direction: Vec3,
    order: i32,
}

#[repr(C)]
struct SpeakerLayout {
    kind: i32,
    count: i32,
    directions: *mut Vec3,
}

#[repr(C)]
struct DecodeSettings {
    layout: SpeakerLayout,
    hrtf: Handle,
    max_order: i32,
}

#[repr(C)]
struct DecodeParams {
    order: i32,
    hrtf: Handle,
    orientation: Orientation,
    binaural: i32,
}

struct Api {
    _dll: Library,
    release_context: Release,
    release_hrtf: Release,
    release_binaural: Release,
    release_encode: Release,
    release_decode: Release,
    reset_binaural: unsafe extern "system" fn(Handle),
    apply_binaural: Apply<BinauralParams>,
    apply_encode: Apply<EncodeParams>,
    apply_decode: Apply<DecodeParams>,
}

struct Object {
    ptr: NonNull<c_void>,
    release: Release,
    _api: Arc<Api>,
}

// SAFETY: the owner moves these handles to a single callback. Each ID's calls
// are serial, and its library stays loaded until the last release returns.
unsafe impl Send for Object {}

impl Object {
    fn new(code: i32, ptr: Handle, release: Release, api: &Arc<Api>, label: &str) -> Result<Self, String> {
        if code != 0 {
            return Err(format!("{label} failed with IPLerror {code}"));
        }
        Ok(Self {
            ptr: NonNull::new(ptr).ok_or_else(|| format!("{label} returned a null handle"))?,
            release,
            _api: Arc::clone(api),
        })
    }

    fn raw(&self) -> Handle {
        self.ptr.as_ptr()
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        unsafe { (self.release)(&mut self.raw()) }
    }
}

pub struct Spatializer {
    binaural: Vec<Object>,
    encoders: Vec<Object>,
    decoder: Object,
    hrtf: Object,
    _context: Object,
    api: Arc<Api>,
    frames: usize,
    order: i32,
}

impl Spatializer {
    pub fn open(path: &Path, rate: u32, frames: usize, direct: usize, bed: usize, order: i32) -> Result<Self, String> {
        let dll = unsafe { Library::new(path) }.map_err(|error| format!("{}: {error}", path.display()))?;
        // The signatures mirror the v4.8.1 header. The system ABI matches
        // IPLCALL on both supported x86-64 targets.
        macro_rules! load {
            ($name:literal, $ty:ty) => {{
                unsafe {
                    *dll.get::<$ty>(concat!($name, "\0").as_bytes()).map_err(|error| format!("{}: {error}", $name))?
                }
            }};
        }
        let create_context =
            load!("iplContextCreate", unsafe extern "system" fn(*mut ContextSettings, *mut Handle) -> i32);
        let create_hrtf = load!("iplHRTFCreate", Create<HrtfSettings>);
        let create_binaural = load!("iplBinauralEffectCreate", Create<BinauralSettings>);
        let create_encode = load!("iplAmbisonicsEncodeEffectCreate", Create<EncodeSettings>);
        let create_decode = load!("iplAmbisonicsDecodeEffectCreate", Create<DecodeSettings>);
        let api = Arc::new(Api {
            release_context: load!("iplContextRelease", Release),
            release_hrtf: load!("iplHRTFRelease", Release),
            release_binaural: load!("iplBinauralEffectRelease", Release),
            release_encode: load!("iplAmbisonicsEncodeEffectRelease", Release),
            release_decode: load!("iplAmbisonicsDecodeEffectRelease", Release),
            reset_binaural: load!("iplBinauralEffectReset", unsafe extern "system" fn(Handle)),
            apply_binaural: load!("iplBinauralEffectApply", Apply<BinauralParams>),
            apply_encode: load!("iplAmbisonicsEncodeEffectApply", Apply<EncodeParams>),
            apply_decode: load!("iplAmbisonicsDecodeEffectApply", Apply<DecodeParams>),
            _dll: dll,
        });
        let mut raw = std::ptr::null_mut();
        let mut context_settings = ContextSettings {
            version: (4 << 16) | (8 << 8) | 1,
            log: None,
            allocate: None,
            free: None,
            simd: 3,
            flags: 0,
        };
        let code = unsafe { create_context(&mut context_settings, &mut raw) };
        let context = Object::new(code, raw, api.release_context, &api, "context")?;
        let mut audio = AudioSettings { rate: rate as i32, frames: frames as i32 };
        macro_rules! create {
            ($func:ident, $settings:expr, $release:ident, $label:literal) => {{
                let mut handle = std::ptr::null_mut();
                let code = unsafe { $func(context.raw(), &mut audio, &mut $settings, &mut handle) };
                Object::new(code, handle, api.$release, &api, $label)?
            }};
        }
        let hrtf = create!(
            create_hrtf,
            HrtfSettings {
                kind: 0,
                sofa_path: std::ptr::null(),
                sofa_data: std::ptr::null(),
                sofa_size: 0,
                volume: 1.0,
                normalization: 0,
            },
            release_hrtf,
            "HRTF"
        );
        let mut binaural = Vec::with_capacity(direct);
        for _ in 0..direct {
            binaural.push(create!(
                create_binaural,
                BinauralSettings { hrtf: hrtf.raw() },
                release_binaural,
                "binaural"
            ));
        }
        let mut encoders = Vec::with_capacity(bed);
        for _ in 0..bed {
            encoders.push(create!(create_encode, EncodeSettings { order }, release_encode, "Ambisonics encoder"));
        }
        let decoder = create!(
            create_decode,
            DecodeSettings {
                layout: SpeakerLayout { kind: 1, count: 0, directions: std::ptr::null_mut() },
                hrtf: hrtf.raw(),
                max_order: order,
            },
            release_decode,
            "Ambisonics decoder"
        );
        Ok(Self { binaural, encoders, decoder, hrtf, _context: context, api, frames, order })
    }

    // Each plane is a distinct preallocated slice of one dense slab.
    fn buffer<const C: usize>(&self, data: &mut [f32], planes: &mut [*mut f32; C]) -> Buffer {
        debug_assert_eq!(data.len() % self.frames, 0);
        let channels = data.len() / self.frames;
        for (ptr, chunk) in planes.iter_mut().zip(data.chunks_exact_mut(self.frames)) {
            *ptr = chunk.as_mut_ptr();
        }
        assert!(channels <= C);
        Buffer { channels: channels as i32, frames: self.frames as i32, data: planes.as_mut_ptr() }
    }

    pub fn binaural(&self, voice: usize, direction: Vec3, bilinear: bool, input: &mut [f32], output: &mut [f32]) {
        let (mut in_ptrs, mut out_ptrs) = ([std::ptr::null_mut(); 1], [std::ptr::null_mut(); 2]);
        let mut input = self.buffer(input, &mut in_ptrs);
        let mut output = self.buffer(output, &mut out_ptrs);
        let mut params = BinauralParams {
            direction,
            interpolation: i32::from(bilinear),
            spatial_blend: 1.0,
            hrtf: self.hrtf.raw(),
            peak_delays: std::ptr::null_mut(),
        };
        unsafe { (self.api.apply_binaural)(self.binaural[voice].raw(), &mut params, &mut input, &mut output) };
    }

    pub fn reset_binaural(&self, voice: usize) {
        unsafe { (self.api.reset_binaural)(self.binaural[voice].raw()) }
    }

    pub fn encode(&self, voice: usize, direction: Vec3, input: &mut [f32], output: &mut [f32]) {
        let (mut in_ptrs, mut out_ptrs) = ([std::ptr::null_mut(); 1], [std::ptr::null_mut(); 9]);
        let mut input = self.buffer(input, &mut in_ptrs);
        let mut output = self.buffer(output, &mut out_ptrs);
        let mut params = EncodeParams { direction, order: self.order };
        unsafe { (self.api.apply_encode)(self.encoders[voice].raw(), &mut params, &mut input, &mut output) };
    }

    pub fn decode(&self, orientation: Orientation, input: &mut [f32], output: &mut [f32]) {
        let (mut in_ptrs, mut out_ptrs) = ([std::ptr::null_mut(); 9], [std::ptr::null_mut(); 2]);
        let mut input = self.buffer(input, &mut in_ptrs);
        let mut output = self.buffer(output, &mut out_ptrs);
        let mut params = DecodeParams { order: self.order, hrtf: self.hrtf.raw(), orientation, binaural: 1 };
        unsafe { (self.api.apply_decode)(self.decoder.raw(), &mut params, &mut input, &mut output) };
    }
}
