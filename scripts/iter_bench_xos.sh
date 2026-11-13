#!/usr/bin/env bash
# Cross-OS script-build identity (coordinator mcda7c814aebf, condition 2): the lamp built by pe-logic's build
# service on Windows and on Linux (WSL) must be the same bytes.
# Usage (Git Bash on Windows, repo root, each cargo phase under heavy.sh):
#   scripts/iter_bench_xos.sh <rev> <out-dir> [windows|linux|compare|all]   (default all)
# windows: runs the identity test at <rev> in a temporary worktree with its own target dir.
# linux: streams `git archive <rev>` into WSL on stdin (no /mnt/c needed, so it works from Windows Session 0), unpacks
#   it in WSL's own file system and runs the same test there with WSL's own target dir and ~/.cargo/bin toolchain.
# compare: the two lamp modules' sha256 and both rustc versions, from the phases' logs. Exits 1 if they differ.
set -euo pipefail
rev="$1"; out="$2"; phase="${3:-all}"
mkdir -p "$out"
full=$(git rev-parse "$rev")
test_name=the_module_is_the_same_bytes_from_any_folder_and_from_a_warm_cache
lamp_rel=tmp/repro/target-a/wasm32-unknown-unknown/release/lamp.wasm

windows() {
  local tgt wt
  tgt="$(python scripts/cargo_output.py "$(pwd)" "xos-win-$(date +%s)-$$")"
  wt="$(mktemp -d)"
  git worktree add -q --detach "$wt" "$full"
  { echo "# $(date -u +%FT%TZ) windows rev=$full"
    (cd "$wt" && CARGO_TARGET_DIR="$tgt" cargo test -q -p pe-logic --locked --test lamp "$test_name") 2>&1
    echo "windows $(rustc -V)"
    sha256sum "$tgt/$lamp_rel" | sed 's/ .*//; s/^/windows sha256 /'
    echo "windows bytes $(wc -c < "$tgt/$lamp_rel")"
  } | tee "$out/xos-windows.log"
  git worktree remove --force "$wt"
}

linux() {
  { echo "# $(date -u +%FT%TZ) linux rev=$full"
    # --exec: no outer shell in WSL, so nothing here is expanded before bash runs it (a plain `wsl bash -c` let the
    # login shell expand $(...) early, before the module existed and without cargo on PATH).
    git archive --format=tar "$full" | MSYS_NO_PATHCONV=1 wsl --exec bash -c "set -e; export PATH=\$HOME/.cargo/bin:\$PATH; \
      rm -rf ~/xos/src && mkdir -p ~/xos/src && tar -xf - -C ~/xos/src && cd ~/xos/src && \
      CARGO_BUILD_JOBS=\${CARGO_BUILD_JOBS:-2} CARGO_TARGET_DIR=\$HOME/xos/target \
        cargo test -q -p pe-logic --locked --test lamp $test_name && \
      echo linux \$(rustc -V) && sha256sum ~/xos/target/$lamp_rel | sed 's/ .*//; s/^/linux sha256 /' && \
      echo linux bytes \$(wc -c < ~/xos/target/$lamp_rel)" 2>&1 | tr -d '\0' | grep -v "^wsl: Failed to translate"
  } | tee "$out/xos-linux.log"
}

compare() {
  local w l
  w=$(grep '^windows sha256' "$out/xos-windows.log" | awk '{print $3}')
  l=$(grep '^linux sha256' "$out/xos-linux.log" | awk '{print $3}')
  { grep -h '^windows rustc\|^linux rustc\|bytes' "$out/xos-windows.log" "$out/xos-linux.log"
    if [ -n "$w" ] && [ "$w" = "$l" ]; then echo "SAME $w"; else echo "DIFFERENT windows=$w linux=$l"; fi
  } | tee "$out/xos.log"
  grep -q '^SAME' "$out/xos.log"
}

case "$phase" in
  windows) windows ;;
  linux) linux ;;
  compare) compare ;;
  all) windows; linux; compare ;;
  *) echo "unknown phase $phase" >&2; exit 2 ;;
esac
