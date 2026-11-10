//! Finding and loading the OpenXR runtime, in Rust (spec 2).
//!
//! Windows has no system OpenXR loader: Khronos' loader is C++ that each application ships. What the
//! system does provide is the runtime's registration. So this does the loader's one essential job:
//! 1. find the active runtime's manifest ([`Discovery`]; injected, so tests never see the real one);
//! 2. resolve the library path against the manifest's folder, and open it with `libloading`, but only
//!    once the [`Guard`] allows it, because opening the library already runs the runtime's code;
//! 3. negotiate the loader-runtime interface (version 1) for any API version 1.x;
//! 4. build an `openxr::Entry` from the `xrGetInstanceProcAddr` the runtime returns.
//!
//! API layers are not loaded. Ported from @ballpit-viz's M0 check (`feasibility/xr-wgpu/src/loader.rs`
//! at f8e9c776), with Linux's lookup, the manifest's `functions` map, version checks and injection added.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openxr::sys;

use crate::guard::{Guard, Refused};

/// The loader-runtime interface version this loader speaks.
pub const INTERFACE_VERSION: u32 = 1;
/// The lowest OpenXR API version accepted.
pub const MIN_API: sys::Version = sys::Version::new(1, 0, 0);
/// The highest offered: any 1.x, as Khronos' loader offers. Virtual Desktop's runtime refuses a maximum
/// of openxr-sys's own 1.1.58, because it is built against 1.1.62 [V: M0 report].
pub const MAX_API: sys::Version = sys::Version::new(1, 0x3ff, 0xfff);
/// The name of the negotiation entry point, unless the manifest's `functions` map renames it.
pub const NEGOTIATE_SYMBOL: &str = "xrNegotiateLoaderRuntimeInterface";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// No runtime is registered (no `XR_RUNTIME_JSON`, no registry value, no XDG file).
    NoRuntimeRegistered(String),
    /// A registration names a manifest that does not exist.
    ManifestMissing(PathBuf),
    ManifestInvalid {
        path: PathBuf,
        why: String,
    },
    LibraryMissing(PathBuf),
    /// The headset guard refused to open the library (spec 10.3).
    Refused(Refused),
    LibraryLoad {
        path: PathBuf,
        why: String,
    },
    NoNegotiateSymbol {
        library: PathBuf,
        symbol: String,
    },
    NegotiationFailed(String),
    VersionOutOfRange {
        interface: u32,
        api: String,
    },
    Entry(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NoRuntimeRegistered(why) => {
                write!(f, "no OpenXR runtime is registered: {why}")
            }
            LoadError::ManifestMissing(p) => write!(f, "the registered OpenXR manifest {} does not exist", p.display()),
            LoadError::ManifestInvalid { path, why } => {
                write!(f, "the OpenXR manifest {} is invalid: {why}", path.display())
            }
            LoadError::LibraryMissing(p) => write!(f, "the OpenXR runtime library {} does not exist", p.display()),
            LoadError::Refused(r) => write!(f, "{r}"),
            LoadError::LibraryLoad { path, why } => write!(f, "loading {}: {why}", path.display()),
            LoadError::NoNegotiateSymbol { library, symbol } => {
                write!(f, "{} has no {symbol}", library.display())
            }
            LoadError::NegotiationFailed(why) => {
                write!(f, "negotiation with the runtime failed: {why}")
            }
            LoadError::VersionOutOfRange { interface, api } => {
                write!(
                    f,
                    "the runtime negotiated interface {interface} and API {api}, outside what this loader accepts"
                )
            }
            LoadError::Entry(why) => write!(f, "building the OpenXR entry: {why}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Where the active runtime's manifest is. Injected: tests use [`FixedManifest`] or [`NoRuntime`], never
/// [`SystemDiscovery`], because a runtime is registered on the machines we test on.
pub trait Discovery {
    fn active_manifest(&self) -> Result<PathBuf, LoadError>;
}

/// The real lookup: `XR_RUNTIME_JSON`, else the registry (Windows) or the XDG paths (Linux).
pub struct SystemDiscovery;

impl Discovery for SystemDiscovery {
    fn active_manifest(&self) -> Result<PathBuf, LoadError> {
        if let Some(p) = std::env::var_os("XR_RUNTIME_JSON") {
            return Ok(PathBuf::from(p));
        }
        platform_active_manifest()
    }
}

#[cfg(windows)]
fn platform_active_manifest() -> Result<PathBuf, LoadError> {
    let key = windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\Khronos\OpenXR\1")
        .map_err(|e| LoadError::NoRuntimeRegistered(format!("HKLM\\SOFTWARE\\Khronos\\OpenXR\\1: {e}")))?;
    let value =
        key.get_string("ActiveRuntime").map_err(|e| LoadError::NoRuntimeRegistered(format!("ActiveRuntime: {e}")))?;
    Ok(PathBuf::from(value))
}

#[cfg(not(windows))]
fn platform_active_manifest() -> Result<PathBuf, LoadError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_home =
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.map(|h| h.join(".config")));
    let config_dirs = std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".into());
    let mut dirs: Vec<PathBuf> = config_home.into_iter().collect();
    dirs.extend(config_dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    xdg_candidates(&dirs, std::env::consts::ARCH)
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| LoadError::NoRuntimeRegistered("no active_runtime.json under the XDG config paths".into()))
}

/// The XDG candidates in order: for each config dir, the per-architecture file first, then the plain one.
#[cfg_attr(windows, allow(dead_code))]
fn xdg_candidates(dirs: &[PathBuf], arch: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let base = d.join("openxr").join("1");
        out.push(base.join(format!("active_runtime.{arch}.json")));
        out.push(base.join("active_runtime.json"));
    }
    out
}

/// A fixed manifest path: for tests on fixture files, and for an explicit `--xr-manifest` setting.
pub struct FixedManifest(pub PathBuf);

impl Discovery for FixedManifest {
    fn active_manifest(&self) -> Result<PathBuf, LoadError> {
        Ok(self.0.clone())
    }
}

/// No runtime registered.
pub struct NoRuntime;

impl Discovery for NoRuntime {
    fn active_manifest(&self) -> Result<PathBuf, LoadError> {
        Err(LoadError::NoRuntimeRegistered("none registered".into()))
    }
}

/// A runtime manifest, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub path: PathBuf,
    pub name: String,
    /// The runtime library, resolved against the manifest's folder when the manifest gives it relative.
    pub library: PathBuf,
    /// The negotiation entry point's name: the `functions` map's, or [`NEGOTIATE_SYMBOL`].
    pub negotiate_symbol: String,
}

/// Parse a manifest's text. `path` is where it was read from, for resolving a relative library path.
pub fn parse_manifest(path: &Path, text: &str) -> Result<Manifest, LoadError> {
    let invalid = |why: String| LoadError::ManifestInvalid { path: path.to_path_buf(), why };
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
    let rt = v.get("runtime").ok_or_else(|| invalid("no \"runtime\" object".into()))?;
    let lib = rt
        .get("library_path")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("no \"runtime.library_path\"".into()))?;
    let name = rt.get("name").and_then(|s| s.as_str()).unwrap_or("(unnamed)").to_string();
    let negotiate_symbol = match rt.get("functions") {
        None => NEGOTIATE_SYMBOL.to_string(),
        Some(map) => {
            let map = map.as_object().ok_or_else(|| invalid("\"runtime.functions\" is not an object".into()))?;
            match map.get(NEGOTIATE_SYMBOL) {
                None => NEGOTIATE_SYMBOL.to_string(),
                Some(alt) => alt
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid(format!("\"runtime.functions.{NEGOTIATE_SYMBOL}\" is not a name")))?
                    .to_string(),
            }
        }
    };
    let lib = PathBuf::from(lib);
    let library = if lib.is_relative() { path.parent().unwrap_or(Path::new(".")).join(lib) } else { lib };
    Ok(Manifest { path: path.to_path_buf(), name, library, negotiate_symbol })
}

/// Read and parse the manifest at `path`, which must exist: a registration is never trusted on its own.
pub fn read_manifest(path: &Path) -> Result<Manifest, LoadError> {
    if !path.is_file() {
        return Err(LoadError::ManifestMissing(path.to_path_buf()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| LoadError::ManifestInvalid { path: path.to_path_buf(), why: e.to_string() })?;
    parse_manifest(path, &text)
}

/// Whether a negotiated interface and API version are ones this loader accepts.
pub fn versions_acceptable(interface: u32, api: sys::Version) -> bool {
    interface == INTERFACE_VERSION && api.major() == 1 && api >= MIN_API && api <= MAX_API
}

/// The API version to ask `xrCreateInstance` for: the negotiated one, capped at 1.1, because a 1.0-only
/// runtime refuses an instance that asks for 1.1 (spec 2). The patch number is left at 0.
pub fn instance_api_version(negotiated: sys::Version) -> sys::Version {
    if negotiated.minor() >= 1 { sys::Version::new(1, 1, 0) } else { sys::Version::new(1, 0, 0) }
}

/// The runtime's library, kept loaded for as long as anything made through it lives: the entry, every
/// instance, and every GPU context made through the runtime, since the runtime may have hooked the Vulkan
/// dispatch (spec 4.1). Clone the `Arc` into whatever must outlive the instance.
pub struct RuntimeLibrary {
    pub path: PathBuf,
    _lib: libloading::Library,
}

/// A loaded runtime.
pub struct Runtime {
    pub manifest: Manifest,
    pub entry: openxr::Entry,
    pub interface_version: u32,
    pub api_version: sys::Version,
    pub library: Arc<RuntimeLibrary>,
}

impl Runtime {
    pub fn instance_api_version(&self) -> sys::Version {
        instance_api_version(self.api_version)
    }
}

/// Find, check and load the active runtime. Everything before opening the library runs without the
/// guard; opening it needs the guard's permit.
pub fn load(discovery: &dyn Discovery, guard: &Guard) -> Result<Runtime, LoadError> {
    let manifest = read_manifest(&discovery.active_manifest()?)?;
    if !manifest.library.is_file() {
        return Err(LoadError::LibraryMissing(manifest.library.clone()));
    }
    guard.check().map_err(LoadError::Refused)?;

    // SAFETY: opening the runtime's library runs its initialisers. That is the boundary the user's ruling
    // allows (the runtime is an operating-system component), and the guard has permitted it.
    let lib = unsafe { libloading::Library::new(&manifest.library) }
        .map_err(|e| LoadError::LibraryLoad { path: manifest.library.clone(), why: e.to_string() })?;
    let mut symbol = manifest.negotiate_symbol.clone().into_bytes();
    symbol.push(0);
    // SAFETY: the symbol has the signature the loader-runtime interface defines.
    let negotiate: libloading::Symbol<sys::pfn::NegotiateLoaderRuntimeInterface> = unsafe { lib.get(&symbol) }
        .map_err(|_| LoadError::NoNegotiateSymbol {
            library: manifest.library.clone(),
            symbol: manifest.negotiate_symbol.clone(),
        })?;
    let info = sys::NegotiateLoaderInfo {
        struct_type: sys::NegotiateLoaderInfo::TYPE,
        struct_version: sys::NegotiateLoaderInfo::VERSION,
        struct_size: std::mem::size_of::<sys::NegotiateLoaderInfo>(),
        min_interface_version: INTERFACE_VERSION,
        max_interface_version: INTERFACE_VERSION,
        min_api_version: MIN_API,
        max_api_version: MAX_API,
    };
    let mut request = sys::NegotiateRuntimeRequest {
        struct_type: sys::NegotiateRuntimeRequest::TYPE,
        struct_version: sys::NegotiateRuntimeRequest::VERSION,
        struct_size: std::mem::size_of::<sys::NegotiateRuntimeRequest>(),
        runtime_interface_version: 0,
        runtime_api_version: sys::Version::new(0, 0, 0),
        get_instance_proc_addr: None,
    };
    // SAFETY: both structs are fully initialised with their types, versions and sizes.
    let r = unsafe { negotiate(&info, &mut request) };
    if r != sys::Result::SUCCESS {
        return Err(LoadError::NegotiationFailed(format!("{r:?}")));
    }
    let (interface_version, api_version) = (request.runtime_interface_version, request.runtime_api_version);
    if !versions_acceptable(interface_version, api_version) {
        return Err(LoadError::VersionOutOfRange { interface: interface_version, api: format!("{api_version}") });
    }
    let gipa = request
        .get_instance_proc_addr
        .ok_or_else(|| LoadError::NegotiationFailed("the runtime returned no xrGetInstanceProcAddr".into()))?;
    // SAFETY: `gipa` comes from the runtime's own negotiation, and the library stays loaded (below).
    let entry = unsafe { openxr::Entry::from_get_instance_proc_addr(gipa, &()) }
        .map_err(|e| LoadError::Entry(e.to_string()))?;
    let library = Arc::new(RuntimeLibrary { path: manifest.library.clone(), _lib: lib });
    Ok(Runtime { manifest, entry, interface_version, api_version, library })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder for one test's fixture files.
    fn fixture_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pe-xr-loader-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, file: &str, text: &str) -> PathBuf {
        let p = dir.join(file);
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn relative_library_path_resolves_against_the_manifest_folder() {
        let m = parse_manifest(
            Path::new(r"C:\Program Files\VD\OpenXR\vd.json"),
            r#"{"file_format_version":"1.0.0","runtime":{"library_path":".\\vd.dll","name":"VD"}}"#,
        )
        .unwrap();
        assert_eq!(m.library, Path::new(r"C:\Program Files\VD\OpenXR").join(r".\vd.dll"));
        assert_eq!(m.name, "VD");
        assert_eq!(m.negotiate_symbol, NEGOTIATE_SYMBOL);
    }

    #[test]
    fn absolute_library_path_is_kept() {
        let abs = std::env::temp_dir().join("rt.so");
        let text =
            format!(r#"{{"runtime":{{"library_path":{}}}}}"#, serde_json::to_string(abs.to_str().unwrap()).unwrap());
        let m = parse_manifest(Path::new("/etc/xdg/openxr/1/active_runtime.json"), &text).unwrap();
        assert_eq!(m.library, abs);
        assert_eq!(m.name, "(unnamed)");
    }

    #[test]
    fn functions_map_renames_the_entry_point() {
        let m = parse_manifest(
            Path::new("m.json"),
            r#"{"runtime":{"library_path":"rt.dll","functions":{"xrNegotiateLoaderRuntimeInterface":"myNegotiate"}}}"#,
        )
        .unwrap();
        assert_eq!(m.negotiate_symbol, "myNegotiate");
        let m = parse_manifest(Path::new("m.json"), r#"{"runtime":{"library_path":"rt.dll","functions":{}}}"#).unwrap();
        assert_eq!(m.negotiate_symbol, NEGOTIATE_SYMBOL);
    }

    #[test]
    fn invalid_manifests_say_why() {
        for text in [
            "not json",
            r#"{}"#,
            r#"{"runtime":{}}"#,
            r#"{"runtime":{"library_path":""}}"#,
            r#"{"runtime":{"library_path":"a.dll","functions":[]}}"#,
            r#"{"runtime":{"library_path":"a.dll","functions":{"xrNegotiateLoaderRuntimeInterface":7}}}"#,
        ] {
            match parse_manifest(Path::new("m.json"), text) {
                Err(LoadError::ManifestInvalid { why, .. }) => assert!(!why.is_empty()),
                other => panic!("{text}: {other:?}"),
            }
        }
    }

    #[test]
    fn no_runtime_registered_is_reported_not_panicked() {
        let e = load(&NoRuntime, &Guard::CLOSED).err().unwrap();
        assert!(matches!(e, LoadError::NoRuntimeRegistered(_)), "{e}");
    }

    #[test]
    fn a_registered_manifest_that_does_not_exist_is_refused() {
        // The shape of this PC's stale Meta registration.
        let gone = fixture_dir("gone").join("oculus_openxr_64.json");
        let e = load(&FixedManifest(gone.clone()), &Guard::CLOSED).err().unwrap();
        assert_eq!(e, LoadError::ManifestMissing(gone));
    }

    #[test]
    fn a_library_that_does_not_exist_is_refused_before_the_guard() {
        let d = fixture_dir("nolib");
        let m = write(&d, "rt.json", r#"{"runtime":{"library_path":"./missing-runtime.dll"}}"#);
        let e = load(&FixedManifest(m), &Guard::CLOSED).err().unwrap();
        assert!(matches!(e, LoadError::LibraryMissing(_)), "{e}");
    }

    #[test]
    fn the_guard_refuses_before_any_library_is_opened() {
        // The "library" exists (it is the manifest itself), so the only thing between it and being opened is
        // the guard. A closed guard must stop it.
        let d = fixture_dir("guard");
        let m = write(&d, "rt.json", r#"{"runtime":{"library_path":"rt.json"}}"#);
        let e = load(&FixedManifest(m), &Guard::CLOSED).err().unwrap();
        assert!(matches!(e, LoadError::Refused(_)), "{e}");
    }

    #[test]
    fn a_file_that_is_not_a_library_fails_to_load_with_a_reason() {
        // With the guard open, a non-library file fails in libloading: no runtime code exists to run.
        let d = fixture_dir("notlib");
        let m = write(&d, "rt.json", r#"{"runtime":{"library_path":"rt.json"}}"#);
        let open = Guard { real_env: None, configured_on: true };
        let e = load(&FixedManifest(m), &open).err().unwrap();
        assert!(matches!(e, LoadError::LibraryLoad { .. }), "{e}");
    }

    #[test]
    fn version_range() {
        assert!(versions_acceptable(1, sys::Version::new(1, 0, 0)));
        assert!(versions_acceptable(1, sys::Version::new(1, 1, 62)));
        assert!(!versions_acceptable(2, sys::Version::new(1, 1, 0)));
        assert!(!versions_acceptable(1, sys::Version::new(2, 0, 0)));
        assert!(!versions_acceptable(1, sys::Version::new(0, 9, 0)));
    }

    #[test]
    fn instance_version_is_capped_at_1_1() {
        assert_eq!(instance_api_version(sys::Version::new(1, 1, 62)), sys::Version::new(1, 1, 0));
        assert_eq!(instance_api_version(sys::Version::new(1, 2, 0)), sys::Version::new(1, 1, 0));
        assert_eq!(instance_api_version(sys::Version::new(1, 0, 34)), sys::Version::new(1, 0, 0));
    }

    #[test]
    fn xdg_candidates_put_the_arch_file_first() {
        let c = xdg_candidates(&[PathBuf::from("/home/u/.config"), PathBuf::from("/etc/xdg")], "x86_64");
        assert_eq!(c.len(), 4);
        assert!(c[0].ends_with("openxr/1/active_runtime.x86_64.json"));
        assert!(c[1].ends_with("openxr/1/active_runtime.json"));
        assert!(c[2].starts_with("/etc/xdg"));
    }
}
