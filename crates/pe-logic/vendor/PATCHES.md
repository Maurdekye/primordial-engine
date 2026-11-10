# Vendored crates in pe-logic

## wasmtime 50.0.1 (no-C patch)

Why: wasmtime's build script compiles `src/runtime/vm/helpers.c` on every Unix target when `runtime` is on, even
with default features off. The user's 100% Rust ruling forbids C in the engine. Without `debug-builtins` that file
only provides `wasmtime_libunwind()`. Coordinator ruling (2026-11-10, option A): carry the patch in this vendored
copy only. The user declined an upstream PR: no upstream contact.

What: three files and two deletions (helpers.c, Cargo.lock), applied by `patch_wasmtime.py` to a pristine copy (commit 15aa0ea7 holds the
pristine copy):
- `build.rs`: the C helpers are never built, on any target (upstream skips them on Windows only, without
  `debug-builtins`). With `debug-builtins` on, the build script stops at a `compile_error!` that says so, so
  enabling it by mistake fails loudly instead of compiling C.
- `Cargo.toml`: the `cc` build-dependency is removed (`runtime` no longer names `dep:cc`). Per @provenance-eng
  (WS-ci, 2026-11-10): with no `cc` edge and no shipped C, the native audit classes this copy `inline-asm`,
  accepted by rule, so it needs no `native_allow.txt` line. `Cargo.toml.orig` is left as published (cargo
  ignores it, and so does the audit).
- `src/runtime/vm/helpers.c`: deleted.
- `src/runtime/vm/sys/unix/unwind.rs`: `wasmtime_libunwind()` is Rust, resolving `__unw_add_dynamic_fde` and
  (except on macOS, as before) the two `__unw_*_eh_frame_section` symbols with `dlsym(RTLD_DEFAULT, ..)`, null when
  absent, which is what the C file's weak symbols gave. Difference: a statically linked libunwind whose symbols are
  not exported now reads as absent, and wasmtime registers frames the libgcc way.

Status: built and trap-tested on Linux with CC=/bin/false, and on Windows (spec 5.2c;
spikes/out/iter-bench/logic-v0/unix-check.log for the two-file patch, unix-check2.log and vendorcheck-win.log for
this one: `debug-builtins` fails with the compile_error, `cargo tree -i cc` is empty). macOS untested.
Native audit (gates/landing 1f11ecdc, `--check` on an export of 1654678b): PASS; this copy is `inline-asm`
(spikes/out/iter-bench/logic-v0/audit-check.log, audit-check.md). The published `Cargo.lock` is removed too: the
audit reads a package with a committed lock as a workspace root of its own.

Upgrading wasmtime: copy the new published crate over the old one, rerun `patch_wasmtime.py` (each edit asserts
its anchor text appears exactly once, so a moved anchor fails), and rerun `spikes/iter-bench/logic-v0/unix_check.sh`.
