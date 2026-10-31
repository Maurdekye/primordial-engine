#!/usr/bin/env bash
# P2-3: compares every log of `run_session_sim.sh --pe` (crates/pe-session) with the spike core's logs
# at a reference commit (default b01ebdf), with wall-clock timings masked. Prints one line per log
# that differs. A log whose only changed lines carry byte counts (" B", "B/s") is reported as BYTES:
# from S3 on, every signed entry is 64 bytes longer on the wire. With --ids (S3b, SHA-256 entry
# ids), the rest are compared again with entry ids, transfer ids and digests masked and lines sorted:
# a log equal that way is reported as IDS (the same events, labelled and ordered by other ids).
# usage: scripts/compare_session_logs.sh [--ids] [ref]
set -u
cd "$(dirname "$0")/.."
IDS=0; if [ "${1:-}" = --ids ]; then IDS=1; shift; fi
REF=${1:-b01ebdf}
mask() { sed -E 's/[0-9]+(\.[0-9]+)? ?(ms|us|µs|s)\b/T/g; s/step avg T/T/g'; }
# --ids normalization: scripts/session_log_norm.py.
idmask() { python -I "$(dirname "$0")/session_log_norm.py"; }
same=0; bytes=0; ids=0; differ=0
for f in $(git ls-tree -r --name-only "$REF" spikes/out/session-sim/ | grep '\.log$'); do
  rel=${f#spikes/out/session-sim/}
  mine=spikes/out/session-sim-pe/$rel
  if [ ! -f "$mine" ]; then echo "MISSING $rel"; differ=$((differ+1)); continue; fi
  d=$(diff <(git show "$REF:$f" | mask) <(mask < "$mine") | grep '^[<>]')
  if [ -z "$d" ]; then
    same=$((same+1))
  elif ! echo "$d" | grep -vqE ' B( |/|$|\))'; then
    echo "BYTES $rel ($(echo "$d" | grep -c '^>') lines)"; bytes=$((bytes+1))
  elif [ $IDS = 1 ] && diff -q <(git show "$REF:$f" | idmask) <(idmask < "$mine") > /dev/null; then
    echo "IDS $rel"; ids=$((ids+1))
  else
    echo "DIFFER $rel"; differ=$((differ+1))
  fi
done
echo "compare_session_logs: $same identical, $bytes differ only in byte counts,$([ $IDS = 1 ] && echo " $ids equal with ids masked,") $differ different (vs $REF, timings masked)"
[ "$differ" = 0 ]
