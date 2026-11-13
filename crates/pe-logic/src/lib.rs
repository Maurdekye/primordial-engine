//! pe-logic (WS-logic, @iter-bench). Logic: scripts compiled by Cranelift with metering injected into the module, run
//! as systems over the store.
//!
//! Specification: docs/spec/logic.md. v0 so far:
//! - [`abi`]: the guest ABI v0, its host functions and required exports (3.1, 3.4);
//! - [`meter`]: the validator and the injected fuel and depth pass (5.2, 5.3), with the optional NaN
//!   canonicalization at observation points (5.2b);
//! - [`runtime`]: the wasmtime engine of 5.1, a script's compiled module, instances with the per-frame reset and
//!   deterministic faults (3.6);
//! - [`manifest`]: the manifest decoded and checked against the store's layout rules (3.2);
//! - [`build`]: the build service, v0 (4.2);
//! - [`run`]: the batch runner: scripts as systems over the store, faults reported for the caller to keep (3.3,
//!   3.5, 3.6).
//!
//! Still to come in v0: faults kept as world state (with WS-world) and
//! activation inside the window (6.2), and the backends of `code.check` and `code.commit`.

pub mod abi;
pub mod build;
pub mod manifest;
pub mod meter;
pub mod run;
pub mod runtime;

/// The engine settings of 5.1; `Settings::thread_stack` is the native stack a thread that runs guests needs.
pub use runtime::Settings;

/// pe-logic's result version: bumped on any change that can alter a result (a state, a fault, a fuel count), since
/// the crate's version does not move with its code. That includes pe-logic's own code, the vendored wasmtime patch
/// (`vendor/wasmtime-50.0.1`, `vendor/PATCHES.md`) and wasmtime's cargo features in the workspace manifest, none of
/// which [`WASMTIME_VERSION`] sees (@shader-reviewer's Low on v4-r1). Part of [`identity`].
pub const RESULT_VERSION: u32 = 1;

/// The wasmtime release pe-logic runs on, as pinned in the workspace manifest (`=50.0.1`). A test checks it against
/// Cargo.lock.
pub const WASMTIME_VERSION: &str = "50.0.1";

/// The bytes that identify how this pe-logic computes results, for S2's runner identity (@vdw-physics's
/// `ScriptRunner::identity`): two machines whose identities differ may compute different results from the same
/// state. It covers [`RESULT_VERSION`], the wasmtime release, the metering pass's and the ABI's versions, every
/// field of [`Settings`] and the result-relevant fields of [`run::RunSettings`] (`fuel_per_call`; `skip_unchanged`
/// changes nothing a script can see, 3.3a (c), so it is left out). Whoever adds a setting that can change a result
/// adds it here in the same change.
pub fn identity(settings: &Settings, run: &run::RunSettings) -> Vec<u8> {
    let mut b = Vec::with_capacity(64);
    b.extend_from_slice(b"pe.logic.identity.v1");
    b.extend_from_slice(&RESULT_VERSION.to_le_bytes());
    b.extend_from_slice(&(WASMTIME_VERSION.len() as u32).to_le_bytes());
    b.extend_from_slice(WASMTIME_VERSION.as_bytes());
    b.extend_from_slice(&meter::PASS_VERSION.to_le_bytes());
    b.extend_from_slice(&abi::ABI_VERSION.to_le_bytes());
    let Settings { meter: m, max_wasm_stack, max_memory } = settings;
    let meter::Config { depth_limit, nan_observe } = m;
    b.extend_from_slice(&depth_limit.to_le_bytes());
    b.push(*nan_observe as u8);
    b.extend_from_slice(&(*max_wasm_stack as u64).to_le_bytes());
    b.extend_from_slice(&(*max_memory as u64).to_le_bytes());
    let run::RunSettings { fuel_per_call, skip_unchanged: _ } = run;
    b.extend_from_slice(&fuel_per_call.to_le_bytes());
    b
}

/// Refuses unless this machine's [`identity`] equals `recorded`, the runner identity a fault bundle or the world
/// carries. A replay calls it before [`runtime::Script::load_metered`], whose bytes do not pin the settings
/// (@coordinator-opus: replay refuses a mismatched runner).
pub fn check_identity(recorded: &[u8], settings: &Settings, run: &run::RunSettings) -> Result<(), runtime::LoadError> {
    if identity(settings, run) == recorded { Ok(()) } else { Err(runtime::LoadError::OtherRunner) }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn the_wasmtime_version_is_the_locked_one() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).unwrap();
        let at = lock.find("name = \"wasmtime\"\nversion = \"").expect("wasmtime in Cargo.lock");
        let v = &lock[at + "name = \"wasmtime\"\nversion = \"".len()..];
        assert_eq!(&v[..v.find('"').unwrap()], WASMTIME_VERSION);
    }

    #[test]
    fn every_result_setting_moves_the_identity_and_the_skip_switch_does_not() {
        let (s, r) = (Settings::default(), run::RunSettings::default());
        let base = identity(&s, &r);
        let mut v = Vec::new();
        let mut m = s;
        m.meter.depth_limit += 1;
        v.push(identity(&m, &r));
        let mut m = s;
        m.meter.nan_observe = !m.meter.nan_observe;
        v.push(identity(&m, &r));
        let mut m = s;
        m.max_wasm_stack += 1;
        v.push(identity(&m, &r));
        let mut m = s;
        m.max_memory += 1;
        v.push(identity(&m, &r));
        v.push(identity(&s, &run::RunSettings { fuel_per_call: r.fuel_per_call + 1, ..r }));
        for (i, x) in v.iter().enumerate() {
            assert_ne!(*x, base, "setting {i} left the identity unchanged");
        }
        assert_eq!(identity(&s, &run::RunSettings { skip_unchanged: !r.skip_unchanged, ..r }), base);
        assert!(check_identity(&base, &s, &r).is_ok());
        assert!(matches!(check_identity(&v[0], &s, &r), Err(runtime::LoadError::OtherRunner)));
    }
}
