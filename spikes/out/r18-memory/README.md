# D56's memory of the congested level: before and after (design 10.9)

Windows, release, low priority. **before** = the controller on main (lower after 300 ms of calm at
every level); **after** = p2/r18-memory 4737a8b (after congestion begins at level L, lowering into L or
below needs 5 s of calm; forgotten after 30 s with a signal and no congestion; `set_link(None)` holds).

## pe-app, 3 members on loopback, member 1 sweeps its hand, scripted token-bucket cap (`windows/`)

before = p2/app 3f52ad4; after = the same plus p2/r18-memory and pe-app feeding `None` when it has no
report (branch tmp/r18-meas). `r18-capped.sh <exe> <dir> <kbit/s> on 7441 1800`, 30 s.
`cycles.md` is made by `cycles.py windows` from member 1's link series (rows every 250 ms, from 3 s):
a cycle starts where the level rises after it last fell.

| run | cycles | period s (mean) | real queue ms mean / max | per-cycle queue peak ms (median) | level shares % |
|---|---|---|---|---|---|
| cap80-before | 8 | 2.9 | 109 / 389 | 366 | {0: 14, 1: 17, 2: 9, 3: 17, 4: 10, 5: 33} |
| cap80-after | 3 | 7.8 | 25 / 353 | 164 | {1: 19, 2: 56, 3: 7, 4: 5, 5: 13} |
| cap120-before | 9 | 2.9 | 30 / 144 | 141 | {0: 30, 1: 16, 2: 13, 3: 12, 4: 15, 5: 15} |
| cap120-after | 3 | 7.5 | 11 / 140 | 129 | {0: 9, 1: 70, 2: 4, 3: 6, 4: 6, 5: 5} |

Confirmation lag (max frames, member 1): 80 kbit/s 30 before, 31 after; 120 kbit/s 15 before, 55 after
(single runs; the lag maximum varies run to run). Every run ALL EQUAL. `level-trace-1.txt` is member
1's level per row.

The probe that re-enters the congested level still builds a queue: at 80 kbit/s the after run's
largest per-cycle peaks are 352, 338, 291 and 171 ms (before: 379-389 ms every cycle), but it comes
about every 8 s instead of every 3 s.

## pe-session-harness `capped` (`harness/`, batched+adaptive rows; 2 members, 500-ball pit)

| cap | before: m1 kbit/s, max queue, back to real time | after |
|---|---|---|
| 64 kbit/s, 10 s | 68.9, 146 ms, 0 ms | 65.3, 146 ms, 0 ms |
| 48 kbit/s, 10 s | 49.5, 225 ms, 92 ms | 30.4, 225 ms, 0 ms |
| 32 kbit/s, 10 s | 32.2, 206 ms, 0 ms | 24.4, 206 ms, 0 ms |
| 16, 12, 64 then 16, 4 members on 64 shared (30 s) | identical | identical (level 5 throughout) |

## Suite and regress (p2/r18-memory)

* `scripts/run_session_sim.sh --pe`: every scenario ALL EQUAL, 0 false checks; every log identical to
  the committed ones but for wall-clock timings.
* `scripts/regress_session_sim.sh`: 91 of 92 identical; `fault-capped-r18` (R18 on at 46 kbit/s)
  changes in its stats and traffic lines only (acks 1174 -> 956; digests and ALL EQUAL unchanged),
  re-blessed.
