#!/usr/bin/env bash
# Runs two pe-spike-p2p peers (session protocol over real UDP on loopback) and checks:
#  1. both peers' per-frame state hashes are identical for every frame;
#  2. each peer's live history equals an offline replay of its own final log (printed by the peer);
#  3. acceptance (user ruling 09:31Z): every remote touch the receiver re-simulated, compared at the
#     receiver's present frame with the author's final history of that object.
# usage: scripts/run-p2p.sh <name> "<peer1 extra args>" "<peer2 extra args>"
set -u
cd "$(dirname "$0")/.."
name=$1; a1=${2:-}; a2=${3:-}
B=target/release/pe-spike-p2p
O=spikes/out/p2p-v2
mkdir -p $O
$B --id 1 --bind 127.0.0.1:40001 --peer 127.0.0.1:40002 --out $O/$name-peer1.txt $a1 > $O/$name-peer1.log 2>&1 &
p1=$!
$B --id 2 --bind 127.0.0.1:40002 --peer 127.0.0.1:40001 --out $O/$name-peer2.txt $a2 > $O/$name-peer2.log 2>&1
wait $p1
if cmp -s $O/$name-peer1.txt $O/$name-peer2.txt; then
  echo "[$name] peer 1 == peer 2 for all $(wc -l < $O/$name-peer1.txt) frames"
else
  echo "[$name] peer 1 != peer 2; first diff:"; diff $O/$name-peer1.txt $O/$name-peer2.txt | head -3
fi
for p in 1 2; do grep -o "offline replay of my final log [0-9a-f]* ([A-Z]*)" $O/$name-peer$p.log | sed "s/^/[$name] peer $p: /"; done
for r in 1 2; do
  a=$((3 - r))
  awk -v R=$r -v A=$a -v N="$name" '
    FNR == NR { if ($1 == "OBJ") h[$2 " " $3] = $4; next }
    $1 == "ACCEPT" { n++; k = $2 " " $3; c = ($3 == 50 && $2 >= 118 && $2 <= 140) ? " (contended: same-frame conflict window on object 50)" : "";
                     if (h[k] == $4) eq++; else { printf "[%s] receiver %d frame %s obj %s differs from author %d%s\n", N, R, $2, $3, A, c } }
    END { printf "[%s] acceptance at receiver %d: %d/%d re-simulated remote touches equal the author %d final history\n", N, R, eq, n, A }
  ' $O/$name-peer$a.txt.obj $O/$name-peer$r.txt.obj
done
grep -h "DONE" $O/$name-peer1.log $O/$name-peer2.log | sed 's/.*DONE/DONE/' | cut -c1-400
