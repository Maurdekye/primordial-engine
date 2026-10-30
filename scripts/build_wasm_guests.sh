#!/usr/bin/env bash
# Builds the wasm guests with local paths remapped (design 13.2, review M8):
# * the logic guests in their own workspace (spikes/wasm-guests -> its target/.../release);
# * physics_in_wasm in the root workspace (-> target/wasm32-unknown-unknown/wasm-guest).
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
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$root=/pe"$'\x1f'"--remap-path-prefix=$home=/cargo"
# PE_HEAVY=<script>: run cargo through the org's heavy-job lock (as scripts/run_determinism.sh does).
run_cargo() { if [ -n "${PE_HEAVY:-}" ]; then bash "$PE_HEAVY" cargo "$@"; else cargo "$@"; fi; }
run_cargo build -q -p physics_in_wasm --target wasm32-unknown-unknown --profile wasm-guest
cd spikes/wasm-guests
run_cargo build --release -q --target wasm32-unknown-unknown
