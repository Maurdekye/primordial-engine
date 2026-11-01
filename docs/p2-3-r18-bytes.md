# pe-session's R18 below ~20 kbit/s: where the bytes go

Item: pe-session's R18 below ~20 kbit/s against the spike (@world-chunks, 2026-11-01). Tree: main ebec136
(7.6 ack reductions and D56's idle gate in). Logs: `spikes/out/r18-pe/bytes/` (README there).

## Finding: the gap is per-entry size, not entry count [V]

`capped --by-kind` (both harnesses) splits each member's offered bytes during the gesture by message
kind, inside batches too. Batched + adaptive, member 1 (the author):

| Cap | | Hand entries | Acks | Datagram overhead | Total kbit/s |
|---|---|---|---|---|---|
| 256 (no queue) | pe-session | 60.1/s x 133 B | 30.1/s x 170 B | 60.1/s x 52 B | 131.2 |
| | spike | 60.1/s x 45 B | 30.1/s x 72 B | 60.1/s x 52 B | 65.2 |
| 32 | pe-session | 14.2/s x 171 B | 10.9/s x 170 B | 22.1/s | 43.7 |
| | spike | 24.0/s x 62 B | 15.0/s x 72 B | 30.1/s | 33.6 |
| 16 | pe-session | 13.7/s x 173 B | 10.4/s x 169 B | 22.4/s | 42.6 |
| | spike | 18.9/s x 70 B | 13.1/s x 71 B | 26.0/s | 29.2 |

Uncapped, the two send the same messages at the same rates. Only the sizes differ. From the two wire
formats (`pe-session/src/session/entry.rs`, `spikes/sim-core/src/session.rs`):

- A pe-session entry carries an Ed25519 signature (64 B) that the spike's does not.
- Its prev link is a 32-byte EntryId; the spike's is 8 B (+24).
- Each ack head names a 32-byte id; the spike's is 8 B (+24 per head).
- Its check hash is 32 B against the spike's 8, but reduction R1 sends it only when the check frame
  changes, so on average it costs less than the spike's.

So a hand is +88 B (64 + 24) and an ack +98 B (64 + 24 + 24, less R1's saving). The controller sees
congestion and thins the same way in both. Every entry it still sends is about 2.4x the spike's.

## What each saving would buy [V in the simulation; the codecs are not built]

`capped --what-if <bits>` (pe-session harness, measurement only): the bandwidth shaper and the counters
charge each message fewer bytes, as if the wire left those fields out. Everything else is unchanged:
same protocol, same entries, same timing. Adaptive transport, single gesture:

| What the wire omits | 32 kbit/s: queue, stalled, back | 16 kbit/s: stalled, back | 12 kbit/s: stalled, back |
|---|---|---|---|
| nothing (pe-session today) | 493 ms, 168, 1.9 s | 1,535, never | 1,538, never |
| prev id and ack head ids (3) | 399 ms, 0, at once | 936, 14.9 s | 1,530, never |
| every signature (4) | 313 ms, 0, at once | 770, 12.1 s | 1,525, never |
| ids, and every signature but each author's last in a datagram (11) | 401 ms, 0, at once | 474, 7.3 s | 1,470, never |
| ids and every signature (7) | 120 ms, 0, at once | 176, 2.4 s | 366, 6.1 s |
| the spike, for reference | 213 ms, 0, at once | 449, 7.3 s | 874, 13.6 s |

- Signing only the last own entry per datagram (8) saves almost nothing by itself: at these levels few
  datagrams carry two of the author's entries. With implicit ids (11) it matches the spike at 16
  kbit/s.
- The end states are EQUAL in every what-if run except those that stay collapsed at 12 kbit/s (3, 4,
  11). There the compared frame is the stalled tip, as in today's 12 kbit/s log.

## Options (for a ruling; none is built)

1. **Implicit ids on the wire** (3). The receiver rebuilds `prev` from the author's entry at `seq - 1`
   and each ack head's id from its own entry at `(q, s)`. The signature still covers the full ids, so a
   mismatch (a fork, or an entry the receiver lacks) fails verification. The receiver then fetches the
   explicit form, so the equivocation evidence is unchanged. Codec-local, plus a hold or fetch path for
   entries whose predecessor is missing [I]. Fixes 32 kbit/s; at 16 kbit/s it recovers, but in 15 s.
2. **Option 1 plus one signature per datagram per author** (11). An author's entries are hash-chained,
   so a signature on the last one covers the earlier ones in the datagram. A relayed or evidence entry
   travels with its signed successor [I]. Matches the spike at 16 kbit/s; still collapses at 12.
3. **Link MACs for live traffic, signatures amortized** (towards 7). Each datagram carries a pairwise
   MAC (16 B, keys from the members' X25519 exchange); every Kth own entry, or each check frame, is
   signed, and the chain covers the rest. Third parties verify through the signed successor [I].
   What-if 7 recovers at 12 kbit/s too, faster than the spike, but it charges neither the MAC nor the
   periodic signatures, so it is a bound, not a measurement of this option. Changes the security design (S3: every entry is
   signed), so it needs @engine-architect and a user ruling.

Not an M1 gate: M1 runs on a LAN (coordinator, 2026-11-01).

## Rulings (@coordinator-opus, 2026-11-01)

Option 1 now, with @engine-architect's conditions: (a) the explicit form wherever the receiver may
lack the predecessor (fetch replies, proofs, certificates, saves); (b) an implicit entry that fails
verification is fetched explicit, never dropped as a bad signature; (c) any hold is bounded well inside
W/2, and a withheld entry must not make an honest member look silent (S6). Option 2 (one signature per
author's run per datagram) is decided, to be built after option 1 lands. Option 3 is off the table for now.

## Option 1 as built: measured with the real codec [V]

`Msg::Lean` (wire tag 12): an author's own live entry without `prev` and without an ack's head ids.
The receiver rebuilds them from its own (member, seq) -> id index, then verifies the signature over the
full entry. The rules as built:

- An entry that names one the receiver lacks waits: the hole is asked for from everyone, a head from
  the acker. A head still missing after 15 frames also gets the explicit form asked for from the sender.
- A failed verification fetches the explicit form from the sender, and everyone's version of seq - 1.
- A waiting entry counts as hearing from its author (S6), and one other than an ack keeps the
  receiver busy, as it would once inserted (7.6's idle predicate reads the wait list; an unverified
  stamp is never noted, and any noted stamp is clamped to a window ahead: review LN-2). At most 1,024 entries wait; past that, the longest wait goes
  (review LN-3).
- An ack naming a chain more than 32 entries past what the receiver has acked holding goes explicit
  (bridge). Fetch replies and relays stay explicit.

A hand is 101 B on the wire instead of 133, and a two-member ack 106 B instead of 170.

**pe-app runs explicit-only** (`core.lean_wire = false`) until @item:primordial-engine/pe-app-lean-wire-accept-msg-lean-key-versions-by
lands: its wire allowlist refuses `Msg::Lean`, and its watch log and scripted split equivocation key
entry versions by id, which a lean entry does not carry (found at landing, 2026-11-01: with lean on,
every m1-loopback member stalls at frame 120; with it off, ALL EQUAL).

Single gesture, batched + adaptive, member 1 (explicit = pe-session before option 1, set by `PE_LEAN=0`;
what-if 3 = the earlier byte charge):

| Cap | Explicit: stalled, back | Option 1: stalled, back | What-if 3: stalled, back | Spike |
|---|---|---|---|---|
| 32 | 168, 1.9 s (queue 493 ms) | 0, at once (queue 399 ms) | 0, at once | 0, at once |
| 24 | 563, 8.5 s | 200, 2.4 s | 184, 2.4 s | |
| 20 | 1,462, 24.0 s | 662, 10.8 s | 473, 6.9 s | |
| 18 | 1,535, never | 1,190, 18.9 s | 590, 8.9 s | |
| 16 | 1,535, never | 1,527, never | 936, 14.9 s | 449, 7.3 s |
| 14, 12 | never | never | never | 12: 874, 13.6 s |

- **The 32 kbit/s single-gesture row is back** at no stall and instant recovery, as asked. So are its
  two-gesture and 310 s cells (the second gesture starts at level 5 or, past the ceiling, level 0; no
  stall either way).
- Other R18 cells [V, `spikes/out/r18-pe`]:
  - Four members sharing 64 kbit/s are back in real time 944 ms after the gesture (before: never).
  - 16 kbit/s recovers in the longer runs: 37.5 s after the gesture in the two-gesture run, against
    never. The 310 s cell's second gesture plays out with no stall.
  - Known cost (@coordinator-opus, 2026-11-01; its cause is looked at with option 2, since 2-3-player
    sessions keep slow-member demotion): the slow member on 8 kbit/s is demoted about 0.5 s later (cut seq 25 -> 42). Its
    smaller entries keep it within the window longer, so the others stall 112 frames instead of 94
    and are back 553 ms after the gesture instead of 57.
- Option 1 beats today's form at every cap from 18 kbit/s up. At 16 and below it still collapses.
- **Why it falls short of what-if 3 below 24 kbit/s:** the what-if also charged fetch replies lean. Under
  option 1 they stay explicit (condition (a)), and recovery from a collapse is mostly repair traffic.
  The real codec, with only its explicit entries charged as if lean, reproduces what-if 3 exactly (936 /
  14.9 s, 590 / 8.9 s, 473 / 6.9 s). So the waits behind holes cost no recovery time; the explicit
  repairs are the whole difference. At 18 kbit/s, 159 lean entries waited a mean of 437 frames for holes
  (what that means for condition (c): next section).
- Decided, to be built with option 2 (@coordinator-opus, 2026-11-01): a hole-fill fetch carries a bit
  "I hold (author, from - 1)", and its reply goes lean. That is condition (a)'s case of a receiver that
  does hold the predecessor, and the same bit as option 2's fetch rule. 16 kbit/s is then measured
  against the what-if.

### Condition (c): how long a lean entry waits (review LN-1) [V]

A lean entry past a hole waits until the hole is filled; an explicit one carries its `prev`, verifies,
and takes effect at once. Review LN-1 asked that waits stay well inside W/2. Ruled (@coordinator-opus,
accepted by @design-reviewer, 2026-11-01): **only a wait on a missing head falls back to the explicit
form after 15 frames; a wait on a hole does not.** The measurements behind the ruling:

- **On a link that keeps up, a wait lasts one refetch.** Ballpit (60 ms delay, 10 ms jitter, W = 120
  frames), member 2 (`ballpit --loss P`, at fbeb304):

  | Loss | Lean waits: mean, max | Lean: rollbacks/s, resimulated frames/s, max depth | Explicit: same |
  |---|---|---|---|
  | 5 % | 4.8, 11 frames | 31.7, 196, 15 | 58.7, 289, 15 |
  | 10 % | 5.8, 19 frames (member 1: 21) | 26.2, 207, 23 | 59.0, 331, 15 |

  The longest wait, 21 frames, is well inside W/2 = 60. Lean rolls back half as often, though its
  deepest rollback at 10 % loss is 8 frames deeper. A fallback after 15 frames (measured on an earlier
  build) fired 3 times at 10 % loss and changed nothing useful.
- **On a saturated uplink a wait has no bound in frames, and neither does an explicit entry.**
  Capped, single gesture, 30 s, member 2 receiving (hand confirmation latency is the mean for m1 / m2;
  the lean rows are the same at fbeb304 as at ac815c6; the fallback rows were a test build):

  | Cap | Mode | Stalled, back | Hand confirm latency, frames | Lean wait mean (max) | Rollbacks, max depth, resim frames |
  |---|---|---|---|---|---|
  | 18 | explicit | 1,535, never | 450 / 330 | - | 35, 114, 837 |
  | 18 | lean | 1,190, 18.9 s | 312 / 329 | 437 (882) | 40, 113, 1,273 |
  | 18 | lean + hole fallback at 15 frames | 1,525, never | 567 / 629 | 812 | 40, 113, 1,095 |
  | 18 | lean + hole fallback at 60 frames | 1,262, 20.3 s | 439 / 474 | 562 | 39, 113, 1,119 |
  | 20 | explicit | 1,462, 24.0 s | 505 / 582 | - | 39, 114, 806 |
  | 20 | lean | 662, 10.8 s | 223 / 236 | 258 (553) | 41, 116, 1,267 |
  | 20 | lean + hole fallback at 15 frames | 894, 14.0 s | 344 / 374 | 488 | 41, 116, 1,161 |
  | 20 | lean + hole fallback at 60 frames | 1,166, 18.4 s | 369 / 396 | 459 | 40, 113, 1,111 |
  | 24 | explicit | 563, 8.5 s | 230 / 246 | - | 41, 115, 904 |
  | 24 | lean | 200, 2.4 s | 116 / 122 | 151 (248) | 35, 112, 500 |
  | 24 | lean + hole fallback at 15 frames | 224, 2.9 s | 126 / 134 | 122 | 36, 112, 572 |

  Explicit entries reach the receiver 330-580 frames late at 18-20 kbit/s, no sooner than the lean
  wait, and the max rollback depth is about 113 frames in every mode. A fallback resends entries the
  receiver holds, in the bigger form, on the full link; that delays everything, the hole included
  (mean wait 437 -> 812 frames at 18 kbit/s), and 18 kbit/s no longer recovers. Lean resimulates more
  frames at 18-20 kbit/s, but confirms sooner and is the only form that recovers at 18.
- The limit, stated plainly: condition (c)'s "well inside W/2" holds on a link that keeps up; under
  saturation no entry, lean or explicit, has a bound in frames. `lean_wait_max` is in the harness's
  `--by-kind` output and in ballpit's with `--loss`.

## Option 2 as built: signed-once runs and the hole rule (p2/opt2) [V]

The design delta (wire, authentication, the hole rule, controls, pe-app) is in
`_teams/1827/opt2-design-delta.md`, for @design-reviewer; the suite triage is in `_teams/1827/opt2-triage.md`.
In short: an author's consecutive entries travel as one `Msg::Run`, every prev after the first left out and
only the top entry signed; a fetch states what the requester holds (`HELD_BELOW`, `HELD_ABOVE`), so a
hole-fill answer goes lean and unsigned; a signed run past a hole asks everyone for the missing entries, the
sender for the whole range from the author's verified prefix, and everyone for that range if it still waits
after 15 frames (`fix::HOLE`).

**The hole rule's cost** (single gesture, 30 s, stalled frames / back in real time after the gesture):

| Rule for a signed run past a hole | 16 kbit/s | 12 kbit/s |
|---|---|---|
| the whole range from everyone, at once (option 1's) | 538 / 8.4 s | never |
| missing from everyone, whole range from the sender, everyone after 15 frames (**as built**) | 442 / 6.4 s | 1,070 / 18.0 s |
| missing from everyone, whole range from the sender, no fallback (unsafe: `holeforker`) | 474 / 6.8 s | 934 / 15.5 s |

**R18 cells, option 2 against main** (batched + adaptive; stalled frames per member / back in real time
after the gesture; main measured at b57ec86 in its own worktree and target dir, 2026-11-01):

| Cell | Main (option 1) | Option 2 | Spike (`spikes/out/r18`) |
|---|---|---|---|
| 32, 48, 64, 256 kbit/s; 32 two gestures and after 5 min | 0 / at once | 0 / at once | 0 / at once |
| 16 kbit/s | 1,527 / 1,541, never | 442 / 456, 6.4 s | at once |
| 16, two gestures (40 s gap) | 2,651 / 2,665, 37.5 s | 442 / 456, 6.4 s | |
| 16, second gesture after 310 s | 2,288 / 2,297, 37.5 s | 898 / 921, 6.4 s | |
| 12 kbit/s | 1,530 / 1,539, never | 1,070 / 1,073, 18.0 s | 21.7 s |
| 12, two gestures | 3,689 / 3,703, never | 1,385 / 1,385, 18.0 s | |
| step 64 -> 16 | 1,497 / 1,512, never | 390 / 404, 6.3 s | 61 ms |
| 8 kbit/s | 1,537 / 1,539, never (DIFFER) | 1,530 / 1,533, never (DIFFER) | never |
| four members sharing 64 | 1,430 / 161 / 161 / 163, 1,197 ms | 1,423 / 160 / 160 / 160, 1,248 ms | at once |
| slow member on 8 kbit/s (others at 256) | 163 / 166 / 940, 1,402 ms | 170 / 173 / 932, 1,654 ms | |

- **Stale goldens corrected:** main's committed `capped-slow-member` and `capped-4members-shared64` logs date
  from option 1's landing (1ec446d). Main as it stands measures 1,402 ms and 1,197 ms (the text above says 553
  and 944 ms). The thin-link, anti-stall and indicators landings moved them; the r18-pe set was not rerun.
  Every other main cell above equals its committed log.
- **Costs:** the slow-member cell is back 252 ms later than main (with runs switched off, 1,453 ms: runs
  account for about 200 ms of it [V, `PE_RUNS=0` A/B]), the shared uplink 51 ms later.
- **8 kbit/s** still never recovers, and its end state DIFFERs between members as on main: the unconfirmed
  tail of a run that never catches up. Unchanged by option 2.
- Runs form about 4.6 times a second at 16 kbit/s; most of the gain is the lean, unsigned hole-fill answers.

**Suite** (`run_session_sim.sh --pe`, strict-compared with cut points masked): every scenario ALL EQUAL,
randsched ALL PASS. Six logs change a fact, all with no safety effect (triage class (c)): colluders (the two
removals decided in two epochs instead of one), strikeframe (which fix-off timings end with {2, 3} or
{2, 3, 9}; honest 1 is struck in all of them, so the control still shows the bug), r14-chain, -epoch and
-reverse (the arrival line printed before the departures; every transfer Accepted), and aheadlink-cut (3
catches up epoch 1 directly at 32.6 s instead of through a reset; never removed, worst lag 1,849 frames
against 1,838). The tool's regress reproduces every log (the same `Net`); holeforker is pe only. On Linux (WSL, static musl, at
0a84449) the pe-session tests pass and the 97 suite logs are identical to these (the 19 others are the tool's own).

**Evidence fixes found on the way** (c3d837c, c6e564f):
- **Scripted faults saw no runs.** The harness's fault filters are written for `Msg::Entry`; a run of two or
  more entries reached them as a batch, matched nothing and was delivered, so every relayed or hole-fill run
  slipped past the fault. aheadlink-long and -cut lost their premise (3 never fell behind: worst lag 42
  frames instead of 1,838), and badproposer, jointiming, prevotefork and slowcapper changed facts that the
  triage had first put down to runs. `Net::send` now shows such a run to the filter entry by entry, as each
  was its own message before option 2: one datagram, dropped if any entry is, else delayed by the longest
  delay. Those four then keep their goldens' facts.
- **The tool's controls:** world option `no_runs` mirrors the harness's `set_no_runs` on fork9, pfsilent and
  slowadmin's fix-off parts.
- **fault-capped-collapse** moves from 12 to 8 kbit/s: at 12 the run still pauses, but confirms to frame 304,
  past the scenario's check of 250 (10 kbit/s: 260).
- **Ballpit's check** moves from 8.5 s to 8.0 s of a 9 s run: with option 2, seed 17 ended with member 2
  confirmed to frame 502 of the 510 checked (a 2 %-loss repair in flight), which the tool's UNCONFIRMED guard
  flags. Measured end lag over seeds 17-19 and ends 9.0-9.5 s: 8-31 frames.
