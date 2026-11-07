# Thin links, Part A, in pe-session

Port of the spike's thin links Part A (design 10.9 / 25.x; the spike's p2/thin-link, main 11c20c1 with reviews
PA-2 fb12cb4 and PA2-1 ff4e407) into pe-session's R18 controller, under
@item:primordial-engine/localize-congestion-to-the-thin-link-player-othe. Ruled by @coordinator-opus (m0a01a8aff8e2,
m6db6a0b411eb, 2026-11-07). The port source is main's spike code at a0af773 (@engine-architect: Part A's final
form; nothing for Part A exists only on p2/thin-link-b). Part B's fed level floor is not part of this; it ports with
Part B.

## What is ported

- **Levels 6-8** (`peer::HAND_STRIDE` / `ACK_FRAMES` = [1, 2, 3, 4, 6, 8, 8, 16, 32]). Level 6 drains a backlog at
  stride 8 without packs; levels 7 and 8 send a single pose and an ack every 16 and 32 frames.
- **The climb rule above level 5**: climb only while the queue is over 300 ms (`DEEP_QUEUE_US`) or has grown 10 ms
  (`GROWING_US`) since the last level change. Below level 5, one level per 100 ms as before.
- **D56 and the drain level**: until the drain level's quick return has been used in a D56 episode, D56 remembers at
  most level 5. The drain level returns to level 5 after 300 ms of calm, once per episode (`drained` resets when the
  memory is forgotten).
- **Reviews PA-2 / PA2-1**: above level 5, an idle member steps down on calm at the 300 ms pace, an active one after
  the recall calm (5 s), idle or not. At level 5 and below nothing changes.
- **Single poses above level 5**: one pose on clock frames that are multiples of k, with span k and no extra targets;
  a pending pack is sent first; others are dropped (`AuthorError::Stride`).
- **Delta-coded pack targets**: a count, a width byte (1 i8, 2 i16, 0 whole i32), then each extra target as a
  wrapping delta from the one before, in the narrowest width that holds every axis of every step
  (`pack_delta_mode`). A stride-8 pack is 62 bytes smaller.
  - **The one difference from the spike**: pe-session refuses a pack in any width but the narrowest, and an unknown
    width, so an entry has one encoding. Neither encoder writes a wider width, so traffic is unchanged.

Tests: `crates/pe-session/tests/thin_links.rs` (codec widths and round trips, the non-canonical refusal, the climb
rule, the drain return once per episode, single poses on the stride ticks, the PA-2/PA2-1 step-down).

Harness: `r18 capped --gestures N` and `--adaptive-only`, the mean confirmation lag line, the replay built from a
live member's log, the slow member's controller line (all as the spike's), and `scripts/run_r18_thin.sh --pe`
(logs in `spikes/out/r18-thin-pe`). A run whose end state is DIFFER also prints a confirmed-prefix check: each live
replica's confirmed frames against a replay of its own log, and the frames all live replicas have confirmed, across
replicas.

## R18 cells [V]

Batched + adaptive; stalled frames per member, time back in real time after the gesture. Every end state EQUAL.

| Cell | pe-session before (main b58fe5c) | pe-session with Part A | Spike with Part A |
|---|---|---|---|
| 16 kbit/s | 458 / 462, 6.5 s | 4 / 0, at once | 0 / 0, at once |
| 12 kbit/s | 1,046 / 1,048, 16.3 s | 47 / 28, at once | 0 / 0, at once |
| step 64 -> 16 | 318 / 330, 4.8 s | 1 / 0, at once | 0 / 0, 129 ms |
| 8 kbit/s | 1,530 / 1,533, never (DIFFER) | 196 / 171, 2.4 s | 224 / 209, 3.3 s |
| four members sharing 64 | 1,423 / 160 / 160 / 160, 1,248 ms | 5 / 9 / 9 / 9, at once | 0 x 4, at once |
| slow member on 8 kbit/s | 159 / 162 / 932, 1,400 ms | 158 / 161 / 933, 1,405 ms | 618 / 618 / 621, 10.5 s |
| 32, 48, 64, 256 kbit/s and their variants | 0 / at once | 0 / at once | 0 / at once |

## The thin-link set [V]

The spike's 64 cells (`run_r18_thin.sh`), run on pe-session with Part A (`--pe`). Against the spike with Part A, 35
cells are equal or better and 29 worse (gaps 1, 4 and 5 below). 18 of the group cells were also run on pe main (b58fe5c), so the
port can be told apart from what pe-session did before (`spikes/out/r18-thin-pe/README.txt`).

**Better than pe main**, in 12 of those 18:
- the thin watcher at 12 kbit/s with 3 members (pe main demoted it: others 212 stalled frames, back after 2.4 s; the
  port demotes nobody: 53, 0.5 s), so losing the demotion is not always worse;
- the thin watcher at 16 kbit/s (3 and 4 members: never -> 0.7 s and 2.0 s) and at 24 kbit/s (1.7 s / 3.3 s / never
  -> 0 / 0 / 4.3 s);
- the gap cells (never -> 0.7 s, 2.0 s, at once);
- the thin sweeper at 12 kbit/s with 2 members (16.3 s -> at once) and with 4 (never -> 10.1 s, see gap 4), and at
  16 kbit/s with 4 (never -> at once).

## Measured gaps (accepted for landing, @coordinator-opus m6db6a0b411eb)

**The 3+ player cells below are single samples** (added 2026-11-07, docs/r18-link-elision.md):
- In the spike, a 4 ms change of the one-way delay (46-54 ms) moves the 3+ player Part A thin cells 2-7x [V for 8
  rows, spikes/out/link-elision/delay-sweep.txt]. The watcher at 12 kbit/s with 8 players, for example, stalls the
  others 156-1,115 frames, and the jitter seed moves nothing.
- These pe-session cells were run at 50 ms only. Their spread is assumed to be similar [I].
- The gaps' figures, and the port's comparisons with the spike and pe main, are therefore single draws. Cells are
  compared on 5-delay means (46-54 ms), as the Part B port's gate is (@coordinator-opus, 2026-11-07).
- The 2-player cells don't spread this way.

**1. A thin watcher at 12-24 kbit/s: the others stall more than in the spike.** For example down-k16-m4: others 124
stalled frames, back after 2.0 s, against 0 at once; down-k16-m8 1,447 against 115; down-k24-m8 262 against 0.
- Probed in down-k16-m4 with `--by-kind`: the watcher's acks are 121 B against the spike's 103 B (the 64 B signature,
  against the spike's 8 B prev and 24 B head ids), and it sends 34 fetches/s against 11/s (repairs for its thin
  downlink). Its controller sits at level 5 for 23.5 s; the spike's rests at level 2.
- Follow-up: @item:primordial-engine/pe-session-per-entry-wire-savings-signatures-pe-a, including the watcher's repair
  rate.

**2. Worse than pe main in 3 cells, all a thin watcher**, because pe main demoted the watcher there and the port does
not:

| Cell | pe main | pe with Part A |
|---|---|---|
| down-k12-m4 | others 216, back 2.4 s; 3 demoted at (54, 232) | others 462, back 7.5 s; nobody demoted |
| down-k12-m8 | others 182, back 1.8 s; 3 demoted | others 1,531, never (DIFFER, not converged); nobody demoted |
| down-k16-m8 | others 177, back 1.8 s; 3 demoted | others 1,447, back 23.3 s; nobody demoted |

- Part A thins the watcher's own stream (acks every 16-32 frames at levels 7-8), its acks keep arriving, and nothing
  removes it: the spike's PL-1 effect (design 10.9). Accepted as the cost of the confirmed no-demotion rule for slow
  links until Part B reaches pe-session. **pe main's ad-hoc demotion of thin watchers at 12-16 kbit/s goes with the
  port, and that is intended.**
- The final behaviour for thin watchers comes with Part B's port (the state feed and the rule-7 floors). By the
  confirmed figures [I, to be measured with that port], an n = 8 watcher at 12-16 kbit/s is below the 39 kbit/s
  removed-below floor and goes to spectator, and an n = 4 watcher at 12 kbit/s is above the 11 kbit/s floor and is
  fed.

**3. A thin watcher at 8 kbit/s is still demoted**, with and without the port (down-k8-m3 / m4 / m8: {3: (42, 171)},
(44, 184), (35, 159)); the spike with Part A demotes it only at 8 members. The others are back after 1.4 / 2.1 / 1.3 s
against the spike's 10.5 / 7.5 / 1.0 s. Part B's port settles it too: at n = 4, 8 kbit/s is below the confirmed
11 kbit/s removed-below floor [I].

**4. A thin sweeper at 12 kbit/s with 4 members** (up-k12-m4, gap3-up-k12-m4, gap15-up-k12-m4): the others stall
647-663 frames and are back after 10.1 s, against the spike's 111-116 frames and 1.06 s; the spike demotes nobody
there. On pe main up-k12-m4 never converged (others 160, DIFFER), so the port improves on it. The smaller sweeper residuals belong
with it: 12 kbit/s with 2 members 47 / 28 stalled frames (spike 0 / 0), 16 kbit/s with 2 and 4 members 4 / 0 and
34 / 16 / 18 / 18 (spike 0), four members sharing 64 kbit/s 5 / 9 / 9 / 9 (spike 0), all back at once. Part B's port
decides the final figures.

**5. A thin sweeper the spike demotes is not demoted** (up-k8-m4, gap3-up-k8-m4, up-k12-m8: the spike removes member
1 and the others are back after 3.4-5.0 s; pe never recovers). pe main also never recovers there, so this is not
from the port (in up-k12-m8 the others stall about 30 frames more with the port: 167-173 against 136-137); the
trace is dropped (@coordinator-opus).

**DIFFER is non-convergence, not divergence** [V]: in down-k12-m8, up-k8-m4, gap3-up-k8-m4, up-k12-m8 and up-k8-m8,
every live replica's confirmed frames equal a replay of its own log, and the prefix all replicas have confirmed is
EQUAL across them. The thin member had confirmed only 134-158 frames by the end of the run.

**6. The sweeper's own hand trails more above level 5 than in the spike's runs**: member 1's own hand against its
target, mean / max, at 16 kbit/s 353 / 862 mm against 205 / 837 mm; at 8 kbit/s 316 / 862 against 173 / 476; on the
step 280 / 823 against 152 / 750. Above level 5 the hand is not exact by design (single poses, as before D73);
pe-session spends longer there [I: its larger entries].

**7. A 395 ms recovery where nothing stalls, at 32 kbit/s** (down-k32-m3, gap-down-k32-m3, gap3-down-k32-m3,
up-k32-m4, gap-up-k32-m4, gap3-up-k32-m4): 0 stalled frames, back after 395 ms, against 0 ms in the spike. The pe
controller sits at level 3 there (23.5 s in up-k32-m4) and visits levels 6-7, where the spike stays at level 5. No
player stalls; the "35 equal or better" count above treats these cells as equal.
