#!/usr/bin/env bash
# Builds the wasm guests with local paths remapped (design 13.2, review M8):
# * the logic guests in their own workspace (spikes/wasm-guests -> its target/.../release);
# * physics_in_wasm in the root workspace (-> target/wasm32-unknown-unknown/wasm-guest);
# * P2-7's larger test crates, logic_mono and logic_wasm, in theirs (spikes/iter-bench/guests).
# Two things would otherwise make a module depend on where it was built:
# * panic locations embed source paths, so the checkout root and the cargo home are remapped;
# * cargo hashes a path dependency's crate identity by its absolute path unless the dependency lies
#   inside the building workspace, and those hashes changed the linker's function order. So
#   physics_in_wasm, which depends on sim-core and the box3d fork by path, builds in the root workspace.
# usage: scripts/build_wasm_guests.sh [checkout root]   (default: this script's checkout)
set -eu
cd "${1:-$(dirname "$0")/..}"
# Long names: rustc sees long paths, and Windows temp directories often come as 8.3 short names.
winpath() { cygpath -w -l "$1" 2>/dev/null || echo "$1"; }
root=$(winpath "$(pwd)")
home=$(winpath "${CARGO_HOME:-$HOME/.cargo}")
# CARGO_ENCODED_RUSTFLAGS separates flags with 0x1f, so paths may contain spaces.
base="--remap-path-prefix=$root=/pe"$'\x1f'"--remap-path-prefix=$home=/cargo"
# Host OS separators (P2-7): cargo hands rustc workspace members' sources as paths relative to the
# workspace, which the remaps above do not touch, and rustc joins module paths with the host's
# separator. So a panic location reads `logic_mono\src\api\math.rs` from Windows and
# `logic_mono/src/api/math.rs` from Linux, and the module bytes differ. On Windows, map every source
# directory of the workspace in the current directory to its '/' spelling; rustc applies the last
# matching remap, so deeper directories come later.
sepmap() {
  command -v cygpath > /dev/null || return 0
  find . -path ./target -prune -o -path '*/target' -prune -o -name '*.rs' -path './*/*' -print \
    | sed 's#^\./##; s#/[^/]*$##' | sort -u | awk -F/ '{ print NF " " $0 }' | sort -n -s | cut -d' ' -f2- \
    | while read -r d; do printf '\x1f--remap-path-prefix=%s\\=%s/' "${d//\//\\}" "$d"; done
}
flags() { export CARGO_ENCODED_RUSTFLAGS="$base$(sepmap)"; }
# PE_HEAVY=<script>: run cargo through the org's heavy-job lock (as scripts/run_determinism.sh does).
run_cargo() { if [ -n "${PE_HEAVY:-}" ]; then bash "$PE_HEAVY" cargo "$@"; else cargo "$@"; fi; }
flags
run_cargo build -q -p physics_in_wasm --target wasm32-unknown-unknown --profile wasm-guest
cd spikes/wasm-guests
flags
run_cargo build --release -q --target wasm32-unknown-unknown
# P2-7's larger test crates (design 23.4): the generated 6.4k-line game-logic guest, as one crate
# (logic_mono) and split into nine crates linked by path inside their workspace (logic_wasm).
cd ../iter-bench/guests
flags
run_cargo build --release -q --target wasm32-unknown-unknown -p logic_mono -p logic_wasm
