import os

# Applies the Primordial Engine no-C patch to a pristine wasmtime 50.0.1. Run from the repo root.
V = "crates/pe-logic/vendor/wasmtime-50.0.1"


def edit(path, a, b):
    p = os.path.join(V, path)
    s = open(p, encoding="utf-8").read()
    assert s.count(a) == 1, (path, a[:60])
    s = s.replace(a, b)
    open(p, "w", encoding="utf-8", newline="\n").write(s)


# 1. build.rs: compile no C on any target. The C helpers are gone, and so is the `cc` build-dependency
#    (step 3); enabling `debug-builtins`, the only feature that still needs helpers.c, fails the build loudly.
edit("build.rs", """    #[cfg(feature = "runtime")]
    if has_host_compiler_backend && (supported_os || cfg!(feature = "debug-builtins")) {
        build_c_helpers();
    }
""", """    // Primordial Engine patch (no C in the engine, the user's 100% Rust ruling): the C helpers file is
    // removed from this copy and never built. On Unix its only job without debug builtins was
    // `wasmtime_libunwind`, which `src/runtime/vm/sys/unix/unwind.rs` now does in Rust.
    let _ = (has_host_compiler_backend, supported_os);
""")
edit("build.rs", """#[cfg(feature = "runtime")]
fn build_c_helpers() {
    use wasmtime_versioned_export_macros::versioned_suffix;

    let mut build = cc::Build::new();
    build.warnings(true);
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    build.define(&format!("CFG_TARGET_OS_{os}"), None);
    build.define(&format!("CFG_TARGET_ARCH_{arch}"), None);
    build.define("VERSIONED_SUFFIX", Some(versioned_suffix!()));
    if std::env::var("CARGO_FEATURE_DEBUG_BUILTINS").is_ok() {
        build.define("FEATURE_DEBUG_BUILTINS", None);
    } else if cfg("windows") {
        // If debug builtins are disabled and this target is for Windows then
        // there's no need to build the C helpers file.
        //
        // TODO: should skip this on Unix targets as well but needs a solution
        // for `wasmtime_using_libunwind`.
        return;
    }

    println!("cargo:rerun-if-changed=src/runtime/vm/helpers.c");
    build.file("src/runtime/vm/helpers.c");
    build.compile("wasmtime-helpers");
}
""", """// Primordial Engine patch: helpers.c and `cc` are removed from this vendored copy (PATCHES.md).
#[cfg(feature = "debug-builtins")]
compile_error!(
    "the Primordial Engine copy of wasmtime has no C helpers: the `debug-builtins` feature is not available"
);
""")


# 2. unwind.rs: look the libunwind entry points up at run time instead of through C weak symbols.
edit("src/runtime/vm/sys/unix/unwind.rs", """    _ => {
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
            fn __deregister_frame(fde: *const u8);
            #[wasmtime_versioned_export_macros::versioned_link]
            fn wasmtime_libunwind() -> Libunwind;
        }
    }
}""", """    _ => {
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
            fn __deregister_frame(fde: *const u8);
        }

        // Primordial Engine patch: helpers.c returned these three symbols through weak linkage, or null
        // when the process uses libgcc's unwinder. Resolving them with `dlsym` on the default handle gives
        // the same result whenever libunwind is a shared library (macOS's libSystem, a shared LLVM
        // libunwind) and null under libgcc. A statically linked libunwind whose symbols are not exported
        // would now read as absent, and wasmtime then registers frames the libgcc way.
        unsafe fn wasmtime_libunwind() -> Libunwind {
            unsafe fn sym(name: &core::ffi::CStr) -> Option<UnwindFn> {
                // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name; a non-null result is the
                // address of a function with the `UnwindFn` ABI, as declared in libunwind.
                unsafe {
                    let p = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
                    if p.is_null() { None } else { Some(core::mem::transmute::<*mut libc::c_void, UnwindFn>(p)) }
                }
            }
            unsafe {
                Libunwind {
                    add_dynamic_fde: sym(c"__unw_add_dynamic_fde"),
                    add_dynamic_eh_frame_section: if cfg!(target_os = "macos") {
                        None
                    } else {
                        sym(c"__unw_add_dynamic_eh_frame_section")
                    },
                    remove_dynamic_eh_frame_section: if cfg!(target_os = "macos") {
                        None
                    } else {
                        sym(c"__unw_remove_dynamic_eh_frame_section")
                    },
                }
            }
        }
    }
}""")


# 3. Cargo.toml: drop the `cc` build-dependency (only build_c_helpers used it), and delete helpers.c.
edit("Cargo.toml", """runtime = [
    "dep:cc",
""", """runtime = [
""")
edit("Cargo.toml", """[build-dependencies.cc]
version = "1.2.0"
optional = true

""", "")
os.remove(os.path.join(V, "src/runtime/vm/helpers.c"))

# 4. The published crate's Cargo.lock: cargo ignores a dependency's lock file, and the native audit
#    (scripts/audit_native.py --check) treats a package with a committed Cargo.lock as a workspace root of its own.
os.remove(os.path.join(V, "Cargo.lock"))
