#!/usr/bin/env bash
# P2-1 (pe-core, pe-sim): runs the tests and records their golden digests in spikes/out/p2-1/.
#   scripts/run_p2_1_tests.sh                Windows debug and release (Git Bash)
#   scripts/run_p2_1_tests.sh --linux        also Linux: the static musl build, run inside WSL
#   scripts/run_p2_1_tests.sh --linux-only   Linux only (keeps the recorded Windows logs)
#   scripts/run_p2_1_tests.sh --wasm         also checks that both crates build for wasm32
# Machine-load rule: every cargo job goes through the org-wide lock (heavy.sh: one at a time, idle
# priority, 2 jobs). Never runs `wsl --shutdown`.
# Each log starts with the commit, the working-tree state and the compiler. The golden digests (RESULT
# lines) must be identical in every test log; the end of this script checks that and fails otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."
out=spikes/out/p2-1
mkdir -p "$out"
heavy="/c/Users/<user>/AppData/Roaming/Orgtree v2/data/scratch/primordial-engine/_teams/1827/heavy.sh"
low() { bash "$heavy" "$@"; }
header() {
  echo "commit $(git rev-parse HEAD)"
  echo "working tree changes: $(git status --porcelain -- crates scripts | wc -l)"
  rustc -vV | grep -E "^(rustc|host|LLVM)"
}
mode="${1:-}"

if [ "$mode" != "--linux-only" ]; then
  for prof in debug release; do
    flag=""
    if [ "$prof" = release ]; then flag="--release"; fi
    { header; low cargo test -p pe-core -p pe-sim $flag --offline -- --nocapture --test-threads=1; } > "$out/windows-$prof.txt" 2>&1 || true
    grep -oE "RESULT.*|^test result.*" "$out/windows-$prof.txt" || true
  done
fi

if [ "$mode" = "--wasm" ]; then
  { header; low cargo build -p pe-core -p pe-sim --release --offline --target wasm32-unknown-unknown; } > "$out/wasm32-build.txt" 2>&1
  tail -2 "$out/wasm32-build.txt"
fi

if [ "$mode" = "--linux" ] || [ "$mode" = "--linux-only" ]; then
  target=x86_64-unknown-linux-musl
  low cargo test -p pe-core -p pe-sim --release --offline --target $target \
    --config "target.$target.linker=\"rust-lld\"" --no-run --message-format=json > "$out/.linux-build.json"
  bins=$(python -I -c "
import json, sys
sys.stdout.reconfigure(newline='\\n')
for l in open(sys.argv[1], encoding='utf-8'):
    try: m = json.loads(l)
    except ValueError: continue
    if m.get('reason') == 'compiler-artifact' and m.get('executable') and m['profile']['test']:
        print(m['executable'].replace(chr(92), '/'))
" "$out/.linux-build.json")
  rm -f "$out/.linux-build.json"
  root=$(pwd -W 2>/dev/null || pwd)
  header > "$out/linux-musl-release.txt"
  # The checkout path has spaces: one binary per line.
  while IFS= read -r b; do
    rel=${b#"$root/"}
    wsl.exe -e sh -c "cd \"\$(wslpath '$root')\" && uname -srm && ./$rel --nocapture --test-threads=1" \
      >> "$out/linux-musl-release.txt" 2>&1 < /dev/null || true
  done <<< "$bins"
  grep -oE "RESULT.*|^test result.*|^Linux.*" "$out/linux-musl-release.txt" || true
fi

# Every recorded test log must report every test passing and the same golden digests.
status=0
ref=""
for f in "$out"/windows-*.txt "$out"/linux-*.txt; do
  [ -e "$f" ] || continue
  if grep -qE "FAILED|panicked|^error" "$f" || ! grep -q "^test result: ok" "$f"; then
    echo "TESTS FAILED in $f"; status=1
  fi
  d=$(grep -oE "RESULT (dmath golden|golden).*" "$f" | sort || true)
  if [ -z "$d" ]; then echo "NO DIGESTS in $f"; status=1; continue; fi
  if [ -z "$ref" ]; then ref=$d; echo "reference digests ($f):"; echo "$d"
  elif [ "$d" != "$ref" ]; then echo "DIGESTS DIFFER in $f:"; echo "$d"; status=1
  else echo "same digests: $f ($(head -1 "$f"))"; fi
done
echo "run_p2_1_tests: done, status $status"
exit $status
