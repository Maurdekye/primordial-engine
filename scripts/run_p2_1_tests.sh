#!/usr/bin/env bash
# P2-1 (pe-core, pe-sim): runs the tests and records their golden digests in spikes/out/p2-1/.
#   scripts/run_p2_1_tests.sh            Windows debug and release (Git Bash)
#   scripts/run_p2_1_tests.sh --linux    also Linux: the static musl build, run inside WSL
# Machine-load rule: every cargo job goes through the org-wide lock (heavy.sh: one at a time, idle
# priority, 2 jobs).
# The golden digests (RESULT lines) must be identical in every file; the end of this script checks that.
set -euo pipefail
cd "$(dirname "$0")/.."
out=spikes/out/p2-1
mkdir -p "$out"
heavy="/c/Users/<user>/AppData/Roaming/Orgtree v2/data/scratch/primordial-engine/_teams/1827/heavy.sh"
low() { bash "$heavy" "$@"; }

for prof in debug release; do
  flag=""
  [ "$prof" = release ] && flag="--release"
  low cargo test -p pe-core -p pe-sim $flag --offline -- --nocapture --test-threads=1 > "$out/windows-$prof.txt" 2>&1
  grep -oE "RESULT.*|^test result.*" "$out/windows-$prof.txt"
done

if [ "${1:-}" = "--linux" ]; then
  target=x86_64-unknown-linux-musl
  low cargo test -p pe-core -p pe-sim --release --offline --target $target \
    --config "target.$target.linker=\"rust-lld\"" --no-run --message-format=json > "$out/.linux-build.json"
  bins=$(python -I -c "
import json, sys
sys.stdout.reconfigure(newline='\n')
for l in open(sys.argv[1], encoding='utf-8'):
    try: m = json.loads(l)
    except ValueError: continue
    if m.get('reason') == 'compiler-artifact' and m.get('executable') and m['profile']['test']:
        print(m['executable'].replace(chr(92), '/'))
" "$out/.linux-build.json")
  rm -f "$out/.linux-build.json"
  root=$(pwd -W 2>/dev/null || pwd)
  : > "$out/linux-musl-release.txt"
  # The checkout path has spaces: one binary per line.
  while IFS= read -r b; do
    rel=${b#"$root/"}
    wsl.exe -e sh -c "cd \"\$(wslpath '$root')\" && uname -srm && ./$rel --nocapture --test-threads=1"       >> "$out/linux-musl-release.txt" 2>&1 < /dev/null
  done <<< "$bins"
  grep -oE "RESULT.*|^test result.*|^Linux.*" "$out/linux-musl-release.txt"
fi

# Every recorded run must give the same golden digests.
ref=""
for f in "$out"/*.txt; do
  d=$(grep -oE "RESULT (dmath golden|golden).*" "$f" | sort)
  if [ -z "$ref" ]; then ref=$d; echo "reference digests ($f):"; echo "$d"
  elif [ "$d" != "$ref" ]; then echo "DIGESTS DIFFER in $f:"; echo "$d"; exit 1
  else echo "same digests: $f"; fi
done
