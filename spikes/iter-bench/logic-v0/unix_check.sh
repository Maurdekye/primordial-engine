#!/usr/bin/env bash
# WS-logic measurement 5 (docs/spec/logic.md section 11): does the patched wasmtime build and run on Linux
# with no C compiler at all? Runs inside WSL Ubuntu, as the user, no sudo.
#   1. rustup at user level if missing (stable, minimal profile).
#   2. Control: the probe against the PRISTINE registry wasmtime with CC=/bin/false must FAIL (helpers.c).
#   3. The probe against the PATCHED vendored wasmtime with CC=/bin/false must build.
#   4. Run its trap tests (fuel and depth determinism, wasmtime's trap handling on Linux).
#   5. (always) With `debug-builtins` enabled, the patched copy must fail with its compile_error.
# Output: one log, path given as $1. With $2 = quick, step 2 (the control) is skipped.
set -u
LOG="$1"
MODE="${2:-full}"
REPO="/mnt/s/primordial-engine/repo/.worktrees/logic"
WORK="$HOME/pe-logic-unix-check"
exec > >(tee "$LOG") 2>&1
echo "# start $(date -u +%FT%TZ) $(uname -srm)"
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  echo "## installing rustup (user level, minimal profile)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
fi
export PATH="$HOME/.cargo/bin:$PATH"
rustc -V; cargo -V
echo "# which cc gcc: $(command -v cc gcc | tr '\n' ' ')"
rm -rf "$WORK"; mkdir -p "$WORK"
# The probe without its guest-dependent parts: only `traps` is needed, so copy the crate and drop pe_api/logic_game.
cp -r "$REPO/spikes/iter-bench/logic-v0/src" "$WORK/src"
python3 - "$WORK" <<'EOF'
import sys, re, os
w = sys.argv[1]
m = open(os.path.join(w, "src/main.rs")).read()
# Linux check needs only traps(); stub the native guest so the guest crates are not needed.
m = m.replace("use pe_api::{Host, World, fnv};", "mod pe_stub;\nuse pe_stub::{Host, World, fnv};\nuse pe_stub as pe_api;\nuse pe_stub as logic_game;")
open(os.path.join(w, "src/main.rs"), "w").write(m)
open(os.path.join(w, "src/pe_stub.rs"), "w").write('''
pub fn fnv(h: u64, v: u64) -> u64 { (h ^ v).wrapping_mul(0x100000001b3) }
pub fn rng_next(s: &mut u64) -> u64 { *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); *s }
pub trait Host { fn terrain_height(&mut self, x: f32, z: f32) -> f32; fn emit(&mut self, k: u32, a: u32, b: f32); fn rand_u32(&mut self) -> u32; }
pub struct World;
impl World { pub fn new(_n: usize, _s: u64) -> World { World } pub fn hash(&self) -> u64 { 0 } }
pub fn tick(_w: &mut World, _f: u64, _m: u32, _h: &mut dyn Host) {}
''')
EOF
manifest() { # $1 = patch line or empty; $2 = extra wasmtime features (default none)
cat > "$WORK/Cargo.toml" <<EOF2
[package]
name = "logic-probe"
version = "0.0.1"
edition = "2024"
publish = false

[workspace]

[dependencies]
wasmparser = { version = "=0.262.0", default-features = false, features = ["std", "validate", "features", "simd"] }
wasm-encoder = { version = "=0.262.0", default-features = false, features = ["std", "wasmparser"] }
wasmtime = { version = "=50.0.1", default-features = false, features = ["std", "runtime", "cranelift", "parallel-compilation", "pooling-allocator"${2:-}] }

[profile.release]
debug = false
codegen-units = 16
$1
EOF2
}
export CC=/bin/false CXX=/bin/false CC_x86_64_unknown_linux_gnu=/bin/false
if [ "$MODE" != quick ]; then
echo "## control: pristine wasmtime, CC=/bin/false (expected to FAIL on helpers.c)"
manifest ""
(cd "$WORK" && CARGO_TARGET_DIR="$WORK/target-pristine" cargo build --release 2>&1 | grep -E "helpers|error|warning: wasmtime|Finished" | head -8; echo "exit=${PIPESTATUS[0]}")
fi
echo "## patched wasmtime, CC=/bin/false (expected to build)"
manifest "[patch.crates-io]
wasmtime = { path = \"$REPO/crates/pe-logic/vendor/wasmtime-50.0.1\" }"
(cd "$WORK" && CARGO_TARGET_DIR="$WORK/target-patched" cargo build --release 2>&1 | grep -E "helpers|error|Finished" | head -8; echo "exit=${PIPESTATUS[0]}")
echo "## objects in the patched build directories (expect none):"
find "$WORK/target-patched/release/build" \( -name '*.o' -o -name '*.a' \) | head
echo "## traps on Linux"
"$WORK/target-patched/release/logic-probe" traps
PATCH="[patch.crates-io]
wasmtime = { path = \"$REPO/crates/pe-logic/vendor/wasmtime-50.0.1\" }"
echo "## patched wasmtime with debug-builtins (expected to FAIL with the PE compile_error)"
manifest "$PATCH" ', "debug-builtins"'
(cd "$WORK" && CARGO_TARGET_DIR="$WORK/target-patched" cargo build --release 2>&1 | grep -E "Primordial|no C helpers|error|Finished" | head -6; echo "exit=${PIPESTATUS[0]}")
echo "## cargo tree: is cc anywhere in the patched graph? (expect nothing)"
manifest "$PATCH"
(cd "$WORK" && cargo tree -e normal,build -i cc 2>&1 | head -5)
echo "# end $(date -u +%FT%TZ)"
