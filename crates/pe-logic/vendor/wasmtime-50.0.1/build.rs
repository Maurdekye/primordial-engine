use std::str;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // NB: duplicating a workaround in the wasmtime-fiber build script.
    custom_cfg("asan", cfg_is("sanitize", "address"));

    let unix = cfg("unix");
    let windows = cfg("windows");
    let miri = cfg("miri");

    // A boolean indicating whether there's a `sys` module for this platform.
    // This is true for `unix` or `windows`, but both of those require the `std`
    // feature to also be active so check that too.
    let supported_os = (unix || windows) && cfg!(feature = "std");

    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    // Determine if the current host architecture is supported by Cranelift
    // meaning that we might be executing native code.
    let has_host_compiler_backend = match target_arch.as_str() {
        "x86_64" | "riscv64" | "s390x" | "aarch64" => true,
        _ => false,
    };

    // Determine if builtin stack-switching routines are provided for the
    // current host architecture by `wasmtime-internal-fiber`.
    let has_builtin_stackswitch = match target_arch.as_str() {
        "aarch64" | "x86_64" | "x86" | "arm" | "s390x" | "riscv64" => true,
        "riscv32" => std::env::var("CARGO_CFG_TARGET_FEATURE")
            .unwrap()
            .split(',')
            .find(|&feat| matches!(feat, "f" | "v"))
            .is_none(),
        _ => false,
    };

    let has_native_signals = !miri
        && (supported_os || cfg!(feature = "custom-native-signals"))
        && has_host_compiler_backend
        && !is_buggy_s390x_qemu_emulation();
    let has_virtual_memory = supported_os || cfg!(feature = "custom-virtual-memory");
    let has_custom_sync = !cfg!(feature = "std")
        && cfg!(feature = "custom-sync-primitives")
        && cfg!(feature = "runtime");
    let has_custom_fiber = !has_builtin_stackswitch && cfg!(feature = "custom-fiber");

    custom_cfg("has_native_signals", has_native_signals);
    custom_cfg("has_virtual_memory", has_virtual_memory);
    custom_cfg("has_custom_fiber", has_custom_fiber);
    custom_cfg("has_custom_sync", has_custom_sync);
    custom_cfg("has_host_compiler_backend", has_host_compiler_backend);
    custom_cfg("gc_zeal", cfg("fuzzing"));

    // If this OS isn't supported and no debug-builtins or if Cranelift doesn't support
    // the host or there's no need to build these helpers.
    // Primordial Engine patch (no C in the engine, the user's 100% Rust ruling): the C helpers file is
    // removed from this copy and never built. On Unix its only job without debug builtins was
    // `wasmtime_libunwind`, which `src/runtime/vm/sys/unix/unwind.rs` now does in Rust.
    let _ = (has_host_compiler_backend, supported_os);

    // Figure out what to do about Pulley.
    //
    // If the target platform does not have any Cranelift support then Pulley
    // will be used by default. That means that the pulley feature is "enabled"
    // here and the default target is pulley. Note that by enabling the feature
    // here it doesn't actually enable the Cargo feature, it just passes a cfg
    // to rustc. That means that conditional dependencies enabled in
    // `Cargo.toml` (or other features) by `pulley` aren't activated, which is
    // why the `pulley` feature of this crate depends on nothing else.
    let default_target_pulley = !has_host_compiler_backend || miri;
    custom_cfg("default_target_pulley", default_target_pulley);
    if default_target_pulley {
        println!("cargo:rustc-cfg=feature=\"pulley\"");
    }
}

// Attempts to detect if we're running in Wasmtime's CI, testing s390x, and
// testing s390x under QEMU. In this situation we're experiencing flaky
// failures, more info in #10000, and current LLM-based analysis seems to point
// to the signal handler state being corrupted in emulation. We're seeing
// many spurious failures per week so this is a bit of a last-ditch attempt to
// work around the issue. In this situation we pretend that native signals are
// not available for s390x which forces disabling signals-based-traps which in
// theory means we don't rely on signal handlers...
fn is_buggy_s390x_qemu_emulation() -> bool {
    std::env::var("CI").is_ok()
        && std::env::var("WASMTIME_TEST_NO_HOG_MEMORY").is_ok()
        && std::env::var("QEMU_BUILD_VERSION").is_ok()
        && std::env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "s390x"
}

fn cfg(key: &str) -> bool {
    std::env::var(&format!("CARGO_CFG_{}", key.to_uppercase())).is_ok()
}

fn cfg_is(key: &str, val: &str) -> bool {
    std::env::var(&format!("CARGO_CFG_{}", key.to_uppercase()))
        .ok()
        .as_deref()
        == Some(val)
}

fn custom_cfg(key: &str, enabled: bool) {
    println!("cargo:rustc-check-cfg=cfg({key})");
    if enabled {
        println!("cargo:rustc-cfg={key}");
    }
}

// Primordial Engine patch: helpers.c and `cc` are removed from this vendored copy (PATCHES.md).
#[cfg(feature = "debug-builtins")]
compile_error!(
    "the Primordial Engine copy of wasmtime has no C helpers: the `debug-builtins` feature is not available"
);
