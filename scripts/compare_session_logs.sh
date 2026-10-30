#!/usr/bin/env bash
# P2-3: compares every log of `run_session_sim.sh --pe` (crates/pe-session) with the spike core's logs
# at a reference commit (default b01ebdf), with wall-clock timings masked. Prints one line per log
# that differs. A log whose only changed lines carry byte counts (" B", "B/s") is reported as BYTES:
# from S3 on, every signed entry is 64 bytes longer on the wire.
# usage: scripts/compare_session_logs.sh [ref]
set -u
cd "$(dirname "$0")/.."
REF=${1:-b01ebdf}
mask() { sed -E 's/[0-9]+(\.[0-9]+)? ?(ms|us|µs|s)\b/T/g; s/step avg T/T/g'; }
same=0; bytes=0; differ=0
for f in $(git ls-tree -r --name-only "$REF" spikes/out/session-sim/ | grep '\.log$'); do
  rel=${f#spikes/out/session-sim/}
  mine=spikes/out/session-sim-pe/$rel
  if [ ! -f "$mine" ]; then echo "MISSING $rel"; differ=$((differ+1)); continue; fi
  d=$(diff <(git show "$REF:$f" | mask) <(mask < "$mine") | grep '^[<>]')
  if [ -z "$d" ]; then
    same=$((same+1))
  elif ! echo "$d" | grep -vqE ' B( |/|$|\))'; then
    echo "BYTES $rel ($(echo "$d" | grep -c '^>') lines)"; bytes=$((bytes+1))
  else
    echo "DIFFER $rel"; differ=$((differ+1))
  fi
done
echo "compare_session_logs: $same identical, $bytes differ only in byte counts, $differ different (vs $REF, timings masked)"
[ "$differ" = 0 ]
