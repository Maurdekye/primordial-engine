#!/usr/bin/env bash
# Warm rebuild time of a script crate with the build service's profile, incremental on vs off (logic.md 4.2).
# Usage: scripts/iter_bench_script_incr.sh <worktree> <out-dir> [runs]
# Copies tests/scripts/tally from the worktree's pe-logic, builds it once per mode, then times `runs` rebuilds
# after a one-line change to src/lib.rs (alternating two variants, so every rebuild compiles the script crate).
set -euo pipefail
wt="$1"; out="$2"; runs="${3:-5}"
mkdir -p "$out"
work="$out/work"; rm -rf "$work"; mkdir -p "$work"
cp -r "$wt/crates/pe-logic/tests/scripts/tally" "$work/tally"
# The lamp's relative pe-sdk path points four levels up; use the worktree's pe-sdk by absolute path.
sdk="$(cd "$wt/crates/pe-sdk" && pwd -W 2>/dev/null || pwd)"
sed -i "s|pe-sdk = { path = \"[^\"]*\" }|pe-sdk = { path = \"${sdk//\\//}\" }|" "$work/tally/Cargo.toml"
cp "$work/tally/src/lib.rs" "$work/lib.rs.orig"
log="$out/incr.log"
{
  echo "# $(date -u +%FT%TZ) $(rustc -V) runs=$runs"
  for inc in true false; do
    tgt="$work/target-$inc"
    build() {
      cargo rustc --release --lib --target wasm32-unknown-unknown --manifest-path "$work/tally/Cargo.toml" \
        --offline --message-format=short \
        --config profile.release.opt-level=3 --config profile.release.lto=false \
        --config profile.release.codegen-units=16 --config "profile.release.incremental=$inc" \
        --config 'profile.release.panic="abort"' --config profile.release.debug=false \
        --config 'profile.release.strip="symbols"' -- -F unsafe_code -C link-arg=-zstack-size=65536 >/dev/null 2>&1
    }
    cp "$work/lib.rs.orig" "$work/tally/src/lib.rs"
    CARGO_TARGET_DIR="$tgt" build
    for i in $(seq 1 "$runs"); do
      cp "$work/lib.rs.orig" "$work/tally/src/lib.rs"
      echo "pub fn __bench_$((i % 2))() -> u32 { $i }" >> "$work/tally/src/lib.rs"
      t0=$(date +%s%N)
      CARGO_TARGET_DIR="$tgt" build
      t1=$(date +%s%N)
      echo "incremental=$inc run=$i warm_rebuild_ms=$(( (t1 - t0) / 1000000 ))"
    done
  done
} | tee "$log"
