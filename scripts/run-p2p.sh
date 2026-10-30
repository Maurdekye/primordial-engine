#!/usr/bin/env bash
# Runs two pe-spike-p2p peers on loopback and compares both histories with the offline
# reference trace produced by `pe-spike-physics trace 600 spikes/out/trace-win-release-1.txt`.
# usage: scripts/run-p2p.sh <name> "<peer1 extra args>" "<peer2 extra args>"
set -u
cd "$(dirname "$0")/.."
name=$1; a1=${2:-}; a2=${3:-}
B=target/release/pe-spike-p2p
O=spikes/out
awk '{print $1, $3}' $O/trace-win-release-1.txt > $O/reference-state.txt
$B --id 1 --bind 127.0.0.1:40001 --peer 127.0.0.1:40002 --out $O/p2p-$name-peer1.txt $a1 > $O/p2p-$name-peer1.log 2>&1 &
p1=$!
$B --id 2 --bind 127.0.0.1:40002 --peer 127.0.0.1:40001 --out $O/p2p-$name-peer2.txt $a2 > $O/p2p-$name-peer2.log 2>&1
wait $p1
for p in 1 2; do
  if cmp -s $O/reference-state.txt $O/p2p-$name-peer$p.txt; then
    echo "[$name] peer $p history == offline reference (600 frames)"
  else
    echo "[$name] peer $p history DIFFERS from reference; first diff:"; diff $O/reference-state.txt $O/p2p-$name-peer$p.txt | head -3
  fi
done
cmp -s $O/p2p-$name-peer1.txt $O/p2p-$name-peer2.txt && echo "[$name] peer 1 == peer 2" || echo "[$name] peer 1 != peer 2"
