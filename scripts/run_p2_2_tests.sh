#!/usr/bin/env bash
# P2-2 (pe-physics): runs the crate's tests, debug and release, on Windows and Linux, and checks
# that every run reproduces the committed spike records (spikes/out/vdw/) with the same digests.
#   scripts/run_p2_2_tests.sh              Windows debug and release (Git Bash)
#   scripts/run_p2_2_tests.sh --linux      also Linux: static musl builds, run inside WSL
#   scripts/run_p2_2_tests.sh --check-only compare the existing logs only
# Every cargo job goes through the org-wide lock (heavy.sh). Never runs `wsl --shutdown`.
# Logs: spikes/out/p2-2/{windows,linux-musl}-{debug,release}.txt, each starting with the commit, the
# working-tree state and the compiler. The RESULT lines must be equal across all four logs, except
# the images digest, which differs between debug and release in Box3D's own snapshot header
# (a debug-only validation flag), so it is compared per profile.
set -euo pipefail
cd "$(dirname "$0")/.."
out=spikes/out/p2-2
mkdir -p "$out"
heavy="/c/Users/<user>/AppData/Roaming/Orgtree v2/data/scratch/primordial-engine/_teams/1827/heavy.sh"
low() { bash "$heavy" "$@"; }
header() {
  echo "commit $(git rev-parse HEAD)"
  echo "working tree changes: $(git status --porcelain -- crates scripts | wc -l)"
  rustc -vV | grep -E "^(rustc|host|LLVM)"
}
linux=no; run=yes
[ "${1:-}" = --linux ] && linux=yes
[ "${1:-}" = --check-only ] && run=no
# With --nocapture a test's first line of output follows the harness's "test name ... " on the
# same line, so RESULT lines are matched anywhere in a line.
results() { grep -oE "RESULT [a-z].*" "$1" || true; }

for prof in debug release; do
  [ $run = yes ] || break
  flag=""; [ $prof = release ] && flag="--release"
  { header; low cargo test -p pe-physics $flag --offline -- --nocapture --test-threads=1; } > "$out/windows-$prof.txt" 2>&1 || true
  results "$out/windows-$prof.txt" | wc -l | sed "s/^/windows $prof RESULT lines: /"
  grep -E "^test result" "$out/windows-$prof.txt" || true
done

if [ $linux = yes ]; then
  target=x86_64-unknown-linux-musl
  root=$(pwd -W 2>/dev/null || pwd)
  for prof in debug release; do
    flag=""; [ $prof = release ] && flag="--release"
    low cargo test -p pe-physics $flag --offline --target $target \
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
    header > "$out/linux-musl-$prof.txt"
    while IFS= read -r b; do
      rel=${b#"$root/"}
      wsl.exe -e sh -c "cd \"\$(wslpath '$root')\" && uname -srm && ./$rel --nocapture --test-threads=1" \
        >> "$out/linux-musl-$prof.txt" 2>&1 < /dev/null || true
    done <<< "$bins"
    results "$out/linux-musl-$prof.txt" | wc -l | sed "s/^/linux $prof RESULT lines: /"
    grep -E "^test result|^Linux" "$out/linux-musl-$prof.txt" || true
  done
fi

# Every log passes every test and gives the same RESULT lines (images per profile).
status=0
ref=""
for f in "$out"/windows-*.txt "$out"/linux-musl-*.txt; do
  [ -e "$f" ] || continue
  if grep -qE "FAILED|panicked|^error" "$f" || [ "$(grep -c '^test result: ok' "$f")" -lt 3 ]; then
    echo "TESTS FAILED in $f"; status=1
  fi
  n=$(results "$f" | wc -l)
  [ "$n" = 61 ] || { echo "$f has $n RESULT lines, expected 61"; status=1; }
  d=$(results "$f" | grep -v "^RESULT images" | sort)
  if [ -z "$d" ]; then echo "NO RESULT LINES in $f"; status=1; continue; fi
  if [ -z "$ref" ]; then ref="$d"; elif [ "$d" != "$ref" ]; then echo "RESULT lines differ in $f"; status=1; fi
done
for prof in debug release; do
  a=$(results "$out/windows-$prof.txt" | grep "^RESULT images" | sort)
  if [ -e "$out/linux-musl-$prof.txt" ]; then
    b=$(results "$out/linux-musl-$prof.txt" | grep "^RESULT images" | sort)
    [ "$a" = "$b" ] || { echo "images differ between Windows and Linux ($prof)"; status=1; }
  fi
done
if [ $status = 0 ]; then echo "RESULT: PASS (every run equal and matching spikes/out/vdw)"; else echo "RESULT: FAIL"; fi
exit $status
