#!/usr/bin/env bash
# Thin links, Part A (design 10.9; @item:primordial-engine/localize-congestion-to-the-thin-link-player-othe):
# one member on its own slow link (both directions), the others at 256 kbit/s, the ball pit's sweep by
# member 1. "up" cells: the thin member sweeps (its uplink); "down" cells: it watches member 1's sweep
# (its downlink). Batched + adaptive only. Logs go to spikes/out/r18-thin/. Run it at low priority.
set -eu
cd "$(dirname "$0")/.."
[ -n "${PE_BIN:-}" ] || cargo build --release -q -p pe-spike-session-sim
B=${PE_BIN:-./target/release}/pe-spike-session-sim
O=${PE_OUT:-spikes/out}/r18-thin
mkdir -p "$O"
# Provenance: the commit (and whether the spikes' sources differ from it, tracked or not; outputs under
# spikes/out don't count, since a re-run overwrites them), the sources' and the binary's hashes. Each log also
# prints the linked sim-core's encoding of a fixed pack.
SRC=$(find spikes/session-sim/src spikes/sim-core/src -name '*.rs' | LC_ALL=C sort | xargs cat | sha256sum | cut -c1-16)
DIRTY=$(git status --porcelain -- spikes ':(exclude)spikes/out' | head -1)
PROV="commit $(git rev-parse --short=12 HEAD)${DIRTY:+ + uncommitted changes}, sources $SRC, binary $(sha256sum "$([ -f "$B" ] && echo "$B" || echo "$B.exe")" | cut -c1-16)"
echo "$PROV" > "$O/PROVENANCE"
run() { local name=$1; shift; { echo "PROVENANCE $PROV"; "$B" capped "$@" --adaptive-only; } > "$O/$name.log"; grep "batched+adaptive" "$O/$name.log" | cut -c1-120; }
for m in 2 3 4 8; do run base-m$m --kbps 256 --members $m --end 30; done
for k in 8 12 16 24 32; do
  for m in 2 4 8; do run up-k$k-m$m --kbps 256 --slow 1 --slow-kbps $k --members $m --end 30; done
  for m in 3 4 8; do run down-k$k-m$m --kbps 256 --slow 3 --slow-kbps $k --members $m --end 30; done
done
run shared64-m4 --kbps 64 --members 4 --shared --end 30
for k in 16 32; do
  for m in 2 4; do run gap-up-k$k-m$m --kbps 256 --slow 1 --slow-kbps $k --members $m --gap 40 --end 66; done
  for m in 3 4; do run gap-down-k$k-m$m --kbps 256 --slow 3 --slow-kbps $k --members $m --gap 40 --end 66; done
done
# Review PA-2: three gestures, 40 s apart, where a second congestion in one D56 episode used to leave an idle
# member at levels 6-8 for the next gesture.
for k in 8 12 16 32; do
  for m in 2 4; do run gap3-up-k$k-m$m --kbps 256 --slow 1 --slow-kbps $k --members $m --gap 40 --gestures 3 --end 110; done
  for m in 3 4; do run gap3-down-k$k-m$m --kbps 256 --slow 3 --slow-kbps $k --members $m --gap 40 --gestures 3 --end 110; done
done
# Review PA2-1: the same, 15 s apart, in the four cells whose later gestures started at levels 6-8 before review
# PA-2 (with 5 s per level while idle above level 5, a gesture 15 s after the one before still started at level 6).
# Review PA2-2: one cell 30 s apart, where level 5 itself congests at the third gesture.
run gap15-up-k12-m4 --kbps 256 --slow 1 --slow-kbps 12 --members 4 --gap 15 --gestures 3 --end 60
run gap15-up-k8-m2 --kbps 256 --slow 1 --slow-kbps 8 --members 2 --gap 15 --gestures 3 --end 60
run gap15-up-k16-m2 --kbps 256 --slow 1 --slow-kbps 16 --members 2 --gap 15 --gestures 3 --end 60
run gap15-down-k8-m4 --kbps 256 --slow 3 --slow-kbps 8 --members 4 --gap 15 --gestures 3 --end 60
run gap30-up-k8-m2 --kbps 256 --slow 1 --slow-kbps 8 --members 2 --gap 30 --gestures 3 --end 90
