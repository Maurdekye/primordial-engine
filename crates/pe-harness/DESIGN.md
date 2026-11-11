# pe-harness: design (WS-harness, M0-M1)

Status: design v1, 2026-11-10, by @harness-eng. Approved as a design by @design-reviewer at d6a0c25 (m4cc52d285eeb).
Nothing in it is built yet.
Plan: `docs/refound-plan.md` (b610464), sections 3.1, 3.2 and 4. Review: `_teams/1847/fable-review/` files 01, 07, 09
(A11, A12), 11 (3.0, 3.3) and 14 (4.1-4.6). The coordinator's rulings of 2026-11-10 20:05Z are in
`_shared/user-design-decisions.md`: the quorum formula, the merge reading and the S2 widening.

Marks: **[V]** verified in the repository or a named record; **[I]** my inference or design choice.

## 0. In one page

- pe-harness is a **library and a test suite.** It drives the sans-IO cores (pe-agree, pe-diverge, and pe-net's
  protocol logic) through seams S1-S4. It runs them over a deterministic in-process network, with a deterministic
  clock, and checks **properties** on what they do. Other crates use it as a library for their own tests (plan 3.1).
- **One run is a pure function of (scenario, seed).** A failing run is reproduced from its seed, shrunk to a small
  schedule, and written out as a scenario file.
- **Nothing is blessed.** No log is compared with a committed log. A run passes when every property holds. Reports
  and measurements are output only.
- **Every safety property has two controls that bite:**
  - a mutation of the code under test, which a named test must catch;
  - a planted violation, which the checker itself must flag.

  A property whose controls pass is not tested.
- **M0 delivers:**
  - the clock and the network model;
  - the node driver and the scenario format with its runner;
  - the property checkers with their own controls;
  - `KvReplica`, a small hashed state machine that stands in for S2;
  - the three probes, written as tests that are expected to fail until the core exists;
  - randsched's generator and shrinker.
- **M1 delivers:**
  - the probes passing against pe-agree, apart from the parts ruled for M3;
  - the properties on every scenario;
  - the mutation suite;
  - model conformance of pe-agree's traces;
  - the sorted scenario suite;
  - the demo script as a harness scenario;
  - the delay distributions.

## 1. What the harness is for

The plan's test principles (4.1) become five jobs:

1. **Run sessions without sockets or clocks.** Any number of nodes, each made of a real pe-agree and a replica.
   Messages travel over modelled links. Faults, partitions and adversaries follow a schedule.
2. **Judge every run by properties** (section 5), never by a stored log.
3. **Make each safety rule bite.** Each property has its mutations and its planted violation (section 6).
4. **Generate schedules.** randsched searches for runs that break a property, within a seed budget, and shrinks
   any it finds (section 7).
5. **Measure what the user feels.** Paused frames, time until others see an action, time until it is final,
   rollback depth and the display layer's counters. These are distributions over seeds and delays (section 8).

It is not:
- a second implementation of the protocol (file 09, A12);
- a place for behaviour switches in the code under test;
- the two-process QUIC run. That run is WS-app's demo test. It reuses the harness's checkers on logs from real
  processes (section 5.3).

## 2. Lessons from the old harnesses

The old harnesses are `crates/pe-session-harness` (7,868 lines), `tools/session-sim` (6,509) and the spike's
`spikes/session-sim` [V plan 2.2]. They move to `archive/` in M0.

**Kept, in a new form:**
- The in-process network model: a virtual clock in microseconds, directed links with delay, jitter and loss, and a
  token-bucket bottleneck with a tail-drop queue (`pe-session-harness/src/lib.rs:21-85`) [V]. Bytes are really
  encoded, so byte counts are real.
- The TOML scenario format, with `include`, a plan of timed actions per member, `[[fault]]` and `[[report]]`. The
  kept old scenarios port with few changes, and new ones read the same way. Also kept:
  - strict parsing (`deny_unknown_fields`) and the meaning checks of `tools/session-sim/src/validate.rs`;
  - first-match fault rules;
  - adversaries declared in the scenario.
- randsched and its widening, which is already on main (`pe-session-harness/src/randsched.rs`) [V]. Kept from it:
  - a separate random stream for each kind of draw;
  - the global stabilisation time (GST): faults before it, clean links after it, and progress measured from it;
  - written invariants (`INVARIANTS.md`);
  - property S6.
- The offline replay (`session/replay.rs`, 183 lines) behind "live equals replay".
- The final check every old run printed: "ALL EQUAL, NO FALSE CHECKS" (`report.rs:1364-1411, 1616-1653`). It becomes
  the first two properties.
- Controls that must still fail (`control_marker`): a re-bless could not quietly turn a control into a pass. Section 6
  keeps the idea and drops the logs.

**Not repeated:**

| The old harness did | Why it went wrong | pe-harness instead |
|---|---|---|
| Compared each run's whole log with a committed golden log (`tools/session-sim/src/regress.rs:46-127`, 114 logs), masking only CPU timings | Every timing change moved the logs; 62 commits re-blessed them. "A test that must be re-approved on every change tests the reviewer" (file 09, M-T1 and A11) | Properties only. Reports are never compared |
| Read state no real endpoint has [V]. The controller got the bottleneck backlog (`lib.rs:841-846`). The RTT came from the link configuration, not pings (441-450). Repair throttling read `net.backlog_us` (696). randsched fed a joiner replica 1's whole log out of band (`randsched.rs:493-497`) | Every thin-link figure was measured with an oracle. The production estimator never ran on those cells (file 01, defect 10) | The harness tells a node only what a real endpoint observes: arrivals, losses it can infer, and its own clock. A joiner gets state only through the protocol |
| Drew all randomness from one network stream, in send order (`lib.rs:318-321`) [V] | One extra message shifted every later jitter and loss draw, so a change that kept every property still moved every log | Each draw is a pure function of (seed, link, the message's index on that link, the purpose of the draw). A change on one link moves nothing on another |
| Put adversary fields inside production `WorldSim`: `fake_refs`, `horizon_lag`, `lie_in_votes`, `withhold_vc`, `byz_ally` and others (`pe-session/src/session/mod.rs:556-742`) [V] | Cheating code shipped in the core, and each new attack meant a change to it | The adversary lives in the harness. It builds and rewrites messages with test-only codecs (section 3.4), so the core holds no adversary state |
| Found S6 violations by parsing event strings for "vote to remove it" (`randsched.rs:553-700`) [V] | A wording change silently disables the check | Typed events only (S3) |
| Wrote about 45 one-off `check` cases, and repros in `main.rs` with tuned constants: late_us ≥ 1.2 s, GST at 8-11 s, a 250 ms ping cadence [V] | Each check worked only on the trajectory it was tuned on | Properties with stated bounds. A scenario's numbers are parameters, not expectations |
| Changed behaviour with process-global switches: `PE_LEAN`, `PE_RUNS`, `PE_CLAIM_RULE` and the static atomics `NO_RUNS` and `NO_LEAN` (`lib.rs:159-173, 643-681`) [V] | A run's result depended on state outside its scenario and seed | All configuration is in the scenario |
| Ran one trajectory per cell, so changing the jitter seed changed nothing | A 4 ms change of delay moved the results 2-7 times, and nobody could see the spread (file 01, defect 10) | At least 20 seeds and 5 delays for every measurement, reported as quantiles |
| Ran with one player acting, at 60 Hz | Unrepresentative of VR (file 01, A7) | Every network cell has all players acting, and runs at 90 Hz as well as 60 |
| Kept behaviour switches in shipped code: 78 fix switches and 73 `PE_*` environment switches (file 09, M-T2), with 11 test fixtures in pe-session's production state (file 01, medium) | Code paths that tests turn off exist in what ships. The switches multiply the states nobody reviews | Mutations exist only under the `mutants` cargo feature, which only the mutation test binary enables. Nothing is read from the environment |
| Mirrored scenarios between two implementations (`not-mirrored.txt`) | Every protocol change was designed once and built twice (file 01, A5) | One implementation, behind the seams |
| Checked invariants with process aborts (file 01, defect 5) | One bad input stopped every replica at the same frame | Every call into the code under test runs inside `catch_unwind`. A panic fails the run, whatever caused it (P6) |

## 3. Architecture

```
            scenario (TOML or Rust builder) + seed
                          │
                          ▼
   ┌──────────────────── Sim ─────────────────────┐
   │ clock: one event queue, (time µs, seq)       │
   │ net:   links, lanes, partitions, loss         │
   │ nodes: Node { agree: dyn Agreement (S3),     │
   │               replica: Recorder<dyn Replica>  │   adversary: rewrites a faulty
   │               local clock (offset, drift),    │   node's outgoing traffic, or
   │               input script }                  │   is a scripted node of its own
   │ trace: every event, every vote, every restore │
   └──────────────────────┬───────────────────────┘
                          ▼
     properties (P1-P14) · model conformance · measurements
```

### 3.1 Modules

| Module | What it holds |
|---|---|
| `clock` | Global time in nanoseconds and one priority queue of events ordered by (time, sequence number). Each node has a local monotonic clock, its `HostTime`, with an offset and a drift in parts per million (file 14's P9). Frames come from that clock through pe-net's sans-IO session clock, not from the harness |
| `net` | Directed links: base delay, jitter, loss (Bernoulli, or Gilbert-Elliott for bursts), duplication, reordering through jitter, and a bandwidth with a drop-tail queue of bounded bytes. Partitions are sets of nodes. Links go up and down on a schedule. It implements S4's promises: five lanes in priority order; Pose lossy and newest-only (an unsent pose is superseded, never queued); every other lane reliable, each message once, in any order, by retransmission after a timeout until a connection give-up time. **Randomness:** every draw is derived from (seed, link, the message's index on that link, the purpose of the draw), never from one shared stream |
| `node` | The driver loop for one node (section 3.2), the replica recorder, and local input from the scenario plan or the adversary |
| `adversary` | Byzantine behaviours (section 3.4) |
| `scenario` | The scenario struct, its TOML form, a Rust builder, and the include mechanism |
| `props` | The property checkers, run on the trace (section 5) |
| `conform` | Replays pe-agree's event trace through `models/cuts`' transition function (plan 4.1.2) |
| `randsched` | The schedule generator, the seed budgets and the shrinker (section 7) |
| `measure` | Distributions and the report written for people (section 8) |
| `replay` | The offline replay of a final log, ported from `session/replay.rs` |
| `kv` | `KvReplica`, a small deterministic S2 implementation (section 3.3) |
| `pending` | The register of tests that are expected to fail until a named milestone (section 9.4) |

### 3.2 The node driver

Each step of a node runs in this order. Every call into code under test is wrapped in `catch_unwind` and timed in
virtual time only.
1. Deliver the messages that are due: `agree.receive(from, bytes)`. An `Err(Fault)` is recorded, never fatal.
2. Local input due now, from the plan or the adversary: `agree.submit(entry)`. The signed entry goes to the outbox.
3. `agree.tick(host_time)`, with the node's local monotonic nanoseconds. Its outgoing messages go into `net`, by lane.
   pe-agree reasons in frames, and pe-net's sans-IO clock maps HostTime to the session frame. The harness drives the
   offset and drift through that clock (engine-architect, m03f00fa4bc71). Every time limit is stated in seconds,
   never in frames (coordinator, m45292d6517cf): the tick rate is 60 Hz by default and configurable per world within a
   range (the user, md4b42d16d9df). Whatever every machine must decide alike, such as the one-minute bound, is
   converted at the world's tick rate and decided at cuts. The harness checks each bound in seconds, through the
   frames at that rate, and runs the bound tests at several rates.
4. Simulate: the shared driver calls `Replica::advance(present, source)` with pe-agree as the `EntrySource`. The
   replica rolls back to the earliest changed frame and steps forward itself; nobody outside restores an arbitrary
   state (pe-seams ruling 3). The recorder wraps the replica and logs each `Advance` (`rolled_back_to`,
   `resimulated`, `stepped`), each `finalize` result and each root.
5. Drain `agree.events()` into the trace, stamped with global time, local time and the node.

**The shared driver: settled.** The loop is WS-state's `drive`, in pe-replica (pe-seams ruling 3), and pe-app and
the harness both call it, so the harness tests the loop that ships. Until it exists, the harness keeps a minimal
copy, marked to be deleted when `drive` lands.

### 3.3 KvReplica: an S2 stand-in

- **What it is:**
  - a sorted map from object id to fields;
  - a hash tree over the map (objects, then the root);
  - entry payloads that set, add to, create or delete a field;
  - permission checks from a governance table, which is itself inside the hash.
- **What it is for:**
  - agreement scenarios, the probes and randsched run thousands of seeds a minute without physics;
  - it mocks S2 before pe-replica exists.
- **Its control:** the same scenarios also run against pe-replica's ball pit, in the slower profile. Then no
  property depends on KvReplica's simplicity.

### 3.4 Adversaries

An adversary owns a faulty node's signing key. It has two ways to act:
- **Entries:** it builds them with S1's test-only builder, so it can sign anything. Validity is enforced only by the
  receivers.
- **Protocol messages** (acks, votes, proposals): the faulty node runs a real pe-agree. The harness intercepts its
  outgoing messages, decodes them with pe-agree's message codec (exposed under `test-util`), rewrites them, and
  re-signs them with the faulty key.

So lying in acks and votes needs no adversary state inside pe-agree, unlike the old core's adversary fields.
Behaviours, each one a scenario action:

| Kind | What it does | Used by |
|---|---|---|
| `withhold { seq }` | Sends nobody the entry at a chain position | probe-d1 |
| `backdate { frames }` | Stamps an entry earlier than its predecessor allows, up to its own declared horizon | probe-d1 |
| `equivocate` | Sends two different entries at one chain position (one chain id and seq), to different peers | F7; P13 |
| `restart_chain` | After a restart, the same key opens a second chain (a new chain id). That is **not** equivocation, so P13 must not fire. It is the control against a checker that flags every second chain. With a back-stamped first entry, it is also the back-dating control across chains (probe-d1's restarted-chain row) | P13; P4 |
| `forge_hand { of }` | A hand entry that moves another member's hand | M1's "own hands only" |
| `garbage { bytes }` | Arbitrary bytes, or a valid envelope with a corrupt payload | "no input aborts a replica" |
| `flood { rate }` | Entries above the rate budget | defect 8 |
| `lie_vote` | Votes for a cut whose state hash it did not compute | AcceptProposalUnchecked's control |
| `lie_refs` | Acks that claim more, or less, of others' chains than it holds (the old `fake_refs`, `hide_refs` and `lag_horizon`) | P4; randsched |
| `foreign` | Entries signed for another world's id, or in another member's name | NoSignatureCheck's control |
| `lie_state` | Hands a joiner a false subtree, or a false ban list | file 07 H3; M3 |
| `silent`, `crash`, `return` | Stops sending, stops entirely, comes back | probes h1 and g3; S4 |

## 4. Scenarios

### 4.1 The format

A scenario is TOML, close to the old format so that the kept scenarios port with few changes:

```toml
description = "..."
include = ["../common/base.toml"]
end_ms = 15000
replica = "kv"            # or "ballpit"
members = [1, 2, 3, 4, 5]
template = "public"       # the governance template: "public" or "all-admin"

[plan]                    # per-member actions at frames (or at_ms)
1 = [{ at = 60, touch = { obj = 10, impulse = [0, 6, 0] } },
     { at = 360, kick = { target = 4 } }]

[[fault]]                 # network and node faults
involves = [4, 5]
after_ms = 3000
silent = true

[[expect]]                # the properties to check, beyond the defaults
kind = "paused_frames"
nodes = [1, 2, 3]
max = 0
```

- **Changes from the old format:**
  - `[[report]]` becomes `[[expect]]`. An expectation is a property with parameters, and it can fail. A
    report-only block is `[[measure]]`.
  - **Expectations name outcomes, never exact numbers that timing moves.** "The kick is not final before the two
    return" can be an expectation. "Final at frame 357" cannot. The scenario format cannot express a whole-log
    comparison at all.
- **The Rust builder** produces the same struct, for tests that compute schedules (probes, randsched).
- **Seeds.** A scenario runs at one seed by default, and at a seed range in the measured and deep profiles.

### 4.2 Sorting the old scenarios (plan 4.5)

The plan counts 120 old scenario files [V plan 4.5]. By directory there are 114 scenarios [V], plus 4 include files
in `common/`:

| Directory | Files | Likely verdict [I] |
|---|---|---|
| `ballpit` | 5 | Keep |
| `bans` | 3 | Keep |
| `d80` | 9 | Keep: the admin rules |
| `faults` | 7 | Keep |
| `review` | 7 | Keep, file by file |
| `p2-4` | 11 | Keep the silent-member and removal schedules. Archive the view-change internals |
| `p2-4-review` | 40 | Mostly archive: view-change internals. Keep the schedules that still mean something under cuts |
| `phase1` | 8 | File by file |
| `indicators` | 8 | Archive (the indicators are parked, plan section 7) |
| `ownership` | 1 | Archive: the ownership book |
| `r14` | 15 | Archive: cross-world certificates |

Three more have committed logs but no file (`not-mirrored.txt`: `grabrace`, `p2-3-statejoin`, `p2-4-holeforker`).
`grabrace` and `p2-3-statejoin` are written anew, because the earlier grab wins (R5) and joining from a cut are M1
rules. I sort the files in M1 into two groups:
- **Kept, judged by the properties:** schedules that still mean something. That is faults, the ball pit, bans,
  silent members and the D80 admin rules.
- **Left in the archive:** scenarios of removed mechanisms. That is thin-link levels and Part B, view-change
  internals, the ownership book, provenance and cross-world certificates.

The sorting is a table in `scenarios/SORTING.md`, one line per file with its reason. Each kept file is rewritten
with `[[expect]]` blocks in place of its reports.

### 4.3 The divergence catalogue

File 14, section 2 has rows P1-P9, S1-S4 and F1-F8. Each row gets one scenario, judged by the properties and by
the display layer's counters. A row whose system lands in a later milestone is a pending test (section 9.4).

## 5. Properties

### 5.1 The checkers

Each property is a function of the trace. "Honest" means every node that the scenario did not make faulty.

| # | Property | Checked as | Source |
|---|---|---|---|
| P1 | **Agreement.** No two honest nodes finalize different cuts at one height on one line. All honest replicas have equal roots at every final cut | Compare each node's `Final{cut}` events by (line, height). Compare the replica's root at the cut's frame with the cut's state hash. A node whose replica the scenario corrupts on purpose is left out of the root comparison from the corruption until its repair is due (P12's bound), and is held to it from then on | plan 4.1; model |
| P2 | **Live equals replay.** An offline replay of the final log from genesis gives every final cut's root | `replay` rebuilds a fresh replica from the final entries in agreed order. It uses S2 and a plain step loop only, never pe-agree. The old replay built a whole `WorldSim` (`session/replay.rs:39`), so it shared the code it was checking [V] | plan 4.1 |
| P3 | **No final frame is undone.** A replica never rolls back below its own **floor**: the frame of the newest final cut whose entries it holds in full (`EntrySource::final_cut`). Rolling back to the floor itself is the base of every replay. The floor never moves back, and the final height never decreases. The floor can lag the line's `CutFinal`, because a non-voter can learn that a cut is final before all its entries arrive. Re-simulating between the floor and that frame is correct (@engine-architect, mb5f08692ac31) | The recorder's `Advance.rolled_back_to` against the node's own floor, sampled from its source at each `advance`. **Named case** (@design-reviewer): at n = 4, a non-voter receives one of a final cut's entries late. It must apply it, re-simulate from its floor, and match the cut's root (P1). **Named case** (@design-reviewer, via m3db740ac721a): at n = 2, a root split is resolved by the reference; then a different cut at that height becomes final, including a late entry below the split cut. The replica must re-simulate below the split cut (only `finalize` moves `Cv` and the floor, S2 v1), and P1 and P2 must hold | plan 4.1; probe-d1 |
| P4 | **Validity.** A final cut includes only entries whose chain validates in order. Every honest node has the same void set, and an entry's void verdict is decided by a final cut, never by when it arrived | An independent re-check of each included chain (sequence, previous hash, signature, stamp rules) in the harness. Then compare `Voided` sets per cut, and check that each `Voided` names the cut that decided it | model; probe-d1 |
| P5 | **Progress resumes.** Once faults stop and a quorum is honest and connected, a new cut is final within the scenario's bound (default 2 s [I]) | Time from the last fault to the next `Final` | plan 4.1 |
| P6 | **No input aborts a replica.** No call into code under test panics. Bad input is refused with a reason that names its author | `catch_unwind`. Every **invalid** input from an adversary (garbage, forged, foreign, flooding beyond the rate, equivocating) is matched by a `Refused`, `Fault` or `Equivocation` naming the adversary. Legal behaviour on the wire, such as `withhold`, `silent` and `lie_refs`, needs no refusal; it is judged by the other properties | plan 4.1; file 01 defect 5 |
| P7 | **Memory stays bounded; the record stays whole.** Live entry count and bytes stay under a bound over long runs. Every entry below the last final cut is in the world record | `stats()` sampled each second, and the record's entries compared with the final log | plan 4.1, 4.3 |
| P8 | **Nobody freezes below quorum.** An honest, connected node's paused frames are zero. Its actions are marked unconfirmed. Unconfirmed play lasts at most one minute, a limit in seconds (3,600 frames only at 60 Hz), and then the line diverges | `Paused` events (must be none), `Unconfirmed` and `LineDiverged`, at two or more tick rates | Q5; m35138bafc46a; probe h1 |
| P9 | **The merge is deterministic and keeps work.** The same entry sets give the same merged root, whatever the arrival order. An action survives the merge unless the ordinary rules refuse it in a run without the split, or it is a returning entry that would beat a final entry on an exclusive hold or a precondition (20:40Z, decision 1). A group only one side touched need only be deterministic, not bit-exact to its branch (decision 2) | The merge run under k permutations of arrival. A reference run on one line with the same entries, then a comparison of per-entry outcomes. **Measured, not gated:** the drawn poses of groups only one side touched stay within the stated pose tolerance across the merge (decision 2) | plan 4.4; 18:58Z; 20:05Z; 20:40Z |
| P10 | **Own hands only.** A forged hand entry is refused on every node, and the history is the same as without it | The run, and a twin run without the forged entry, compared at every final cut | M1; session-validation item |
| P11 | **S6.** On clean links, an honest node never votes to remove or demote an honest node that has not crashed. **Thin-link variant:** an honest slow member above the floors keeps its vote and is never removed or demoted (rev 15 rule 7; no demotion for slow links, 20:05Z) | `Voted` events joined to the cut's governance entries | randsched widening; 20:05Z |
| P12 | **Repair converges and imports nothing unverified.** After an injected divergence, the damaged replica's root equals the quorum's at a final cut within a bound (rungs 1-2). Nothing it imports during repair is used unless its hash matches the quorum-held subtree hash. On a clean run, no divergence is reported at all | The recorder's roots after the injection. Every imported subtree checked against the cut's tree | plan M1 step 6; file 14 4.3 |
| P13 | **Equivocation is known everywhere.** Once any honest node holds two signed versions of one chain position, every honest connected node holds the proof within a bound | `Equivocation` events per node, timed from the first | file 14 F7, 4.5 |
| P14 | **Convergence targets** (the user, file 14 8.3, accepted as recommended): "others see your action within 0.25 s on good links and 1 s on poor ones", and no correction larger than a hand's width in view without a visible cue | **End to end:** from the author's local input to the moment the action is drawn on the other node within a hand's width of its final pose. That covers network, any refetch past a hole, rollback and the blend together, measured per link class (good, poor) as quantiles over seeds. Gated per class at 0.25 s and 1 s. The cue check reads S5's correction offsets and cue events. **A loss cell on good links** (1% loss, every author sending hands at 60 entries/s) is part of the good class. It confirms that predicting pending entries, flagged unvalidated, keeps a lost entry from stalling a remote hand | user 19:47Z; file 14 4.1, 8.3 |
| P15 | **The entry source's contract** (S3 v1.1, coordinator m7940e24cabf1), per member, never as an ordering across members: `wants_roots` and `final_cut` never hand over a cut unless this member holds every entry it names, void ones included; it reports no verdict on an entry it does not hold; a final cut's height never goes back; below the final frame, `entries_at` holds only entries the final cut covers and has not voided | The recorder counts what each member holds (its own entries, and every valid entry delivered to it, read with the agreement's test-util codec) and records a `Breach`. `tests/conform_source.rs` runs one suite against every agreement; the mock's `vote_without_holding` is the control | probe-d1; S3 v1.1 |

### 5.2 Defaults

- **Every scenario checks P1-P4 and P6.** P5 is checked when the scenario names an end to its faults. P7 is
  checked in the long profile. P8-P14 are checked where the scenario asks for them; P14 in every network cell once the display layer exists; P12 wherever a replica is
  corrupted, and P13 wherever an adversary equivocates.
- **"Paused frame":** a frame at which a node's local input was not applied at once, or its replica did not step
  while its local clock advanced. In the new design it never happens. The h1 control makes it happen.
- **Quorum.** The harness restates the coordinator's formula independently: t = ⌊(n−1)/3⌋ and q = ⌊(n+t)/2⌋ + 1
  (20:05Z). So n = 3 needs 2, n = 4 needs 3, n = 5 needs 4, n = 6 needs 4 and n = 7 needs 5. A test checks that
  pe-agree's quorum function agrees with this formula for n = 1 to 16.

### 5.3 Checkers outside the harness

The checkers read a trace format. It is serialized, so WS-app's two-process QUIC run can write one per process.
The same P1-P4 and P6 then judge the demo's CI run (plan M1), with no second copy of the logic.

## 6. Controls that bite

### 6.1 Mutations of the code under test

- **Where they live:**
  - A mutation is a named variant of an enum in pe-agree, or in pe-diverge for the merge.
  - It is compiled only under the `mutants` cargo feature, and passed in through the node's configuration.
  - The default build has no such feature.
  - The gate checks that no production build enables it. The check runs on pe-app's normal edges
    (`cargo tree -p pe-app -e normal,features`), because a workspace `cargo test` unifies dev-dependency features and
    compiles `mutants` in there by design. I propose the check to @provenance-eng.
  - So a mutation is also **inert unless selected at run time**: compiled in, it does nothing until the node's
    configuration names it.
- **The suite:** `tests/mutants.rs` runs each pair below with the mutation on and **requires the named test to
  fail**, with the named property. A mutation that no test catches fails the suite.

| Mutation | Turns off | Must be caught by |
|---|---|---|
| `VoteWithoutChainValidation` | Votes for a cut before each author's chain validates in order: the d1 defect | probe-d1, through P3 or P4 |
| `StopPlayBelowQuorum` | Pauses play when no quorum is reachable: the h1 freeze | probe-twosilent5-kick, through P8 |
| `AcceptProposalUnchecked` | Votes for a proposer's cut without recomputing its state hash | the `lie_vote` scenario, through P1 |
| `QuorumOneTooSmall` | The quorum is one smaller than the formula | the split-brain scenario, through P1 (20:05Z: "q−1 must produce split-brain") |
| `NoSignatureCheck` | Accepts an entry with a forged author, or another world's id | the `foreign` scenario, through P4 and P6 |
| `OneMinuteBoundRaised` | Unconfirmed play runs past the one-minute bound | twosilent5 variant B, through P8 |
| `LeaveIgnored` | A leave entry has no effect, so the leaver is waited for | the leave scenario (plan 4.3), through P8 |
| `RateLimitRemoved` (pe-agree) | A member's entries beyond its rate are accepted. pe-agree owns the per-member entry rate and this mutation; it is a resource rule, not in the model (@world-chunks, md922aa4d8491) | the `flood` scenario, through P7 |
| `TxLimitsRemoved` (pe-world) | One transaction beyond 4,096 ops or 1 MiB of payload, or a blob beyond its cap, is accepted | one oversized transaction, through P7 |
| `PreconditionIgnored` (pe-world) | A stale edit is applied, not refused (rule 2) | the stale-edit scenario: its `[[expect]]` that the edit is refused, naming the winner |
| `HoldIgnored` (pe-world) | A second grab of a held object is applied (rule 1) | `grabrace`: its `[[expect]]` that the earlier grab wins |
| `TombstoneOrphanRemoved` (pe-world) | A child created under, or reparented to, an object deleted earlier in the order is refused instead of orphaned (world-model.md 7.5, ab30dc5; the user's m35138bafc46a). Components are a separate rule: a row attached to an object deleted earlier is refused with `Deleted { by }` (mfc2bbd9d28ad) | the delete-first scenario: a delete at frame 100 and a child create at 120, including a depth-2 descendant; its `[[expect]]` that the child exists as a root. A twin attaches a component at 120 and expects `Deleted { by }` |
| `LeafOmitsRowVersion` (pe-world) | A leaf's hash leaves out the row version | the hash-coverage scenario (plan M1): a row version changed on one replica, through P1 and P12 |
| `MergeByArrival` (pe-diverge, merge v1, M2) | Orders a merge by arrival, not by stamp and key | the merge scenario, through P9 |
| `MergeDropsSide` (pe-diverge, merge v1, M2) | Drops one side's actions in a merge | the twosilent5 return variant, through P9 |
| `VoidSplitEntries` (pe-diverge, merge v1, M2) | Voids entries made during a split by the tolerance rule | the split scenario, through P9 |
| `MergeByStampOnly` (pe-diverge, merge v1, M2) | Lets a returning, back-stamped entry beat a final entry on an exclusive hold | twosilent5 variant D, through P3 or P9 |
| `OwnHandsCheckRemoved` (pe-world) | Lets an input move another member's hand | the `forge_hand` scenario, through P10 |
| `VoidByArrival` | Decides an entry void by when it arrived, not at the cut | probe-d1 and randsched's late deliveries, through P4 |
| `VoteWithoutRoot` | Votes without the root the voter computed itself (WS-diverge: votes carry the root) | an injected divergence on one replica, through P1 |
| `ImportUnchecked` (pe-replica: under seams v0 `import_at_cut` checks the proofs) | Repair imports records without checking their paths against the remote root | a `lie_state` repair source (divergence.md 12, test 5), through P12 |
| `PatchNoReplay` (pe-replica: a successful import replays to the present) | Rung 2 patches at the cut but does not replay forward | a persistent fault (divergence.md 12, test 2), through P1 at the next cut |
| `CompareUnconformed` (pe-diverge; not in its v0 list, to confirm with @diverge-eng) | Compares roots before conforming to the cut | a clean run, through "no divergence reported on a clean run" (P12's negative half) |
| `SkipVerify` (pe-diverge) | Diffs against its own tree without rehashing it, so a damaged record behind a stale cache goes unseen | a persistent fault (test 2), through P12: the next cut disproves the cure |
| `StickWithSource` (pe-diverge) | Keeps asking a repair source whose records failed their proofs | a `lie_state` source with an honest second source, through P12: the repair must converge from the second |
| `PruneRecord` | Drops entries below the cut from the world record, not only from live memory | the long run, through P7's record half |
| `HorizonRuleOff` | The monotone-stamp rule (the horizon rule's new form) is not enforced | probe-d1 variant 3 and the `backdate` scenario, through P4 |
| `EquivocationNotGossiped` | An equivocation proof stays with the node that found it | the `equivocate` scenario, through P13 |

- **Names.** They match `models/cuts`' controls where both have one (@world-chunks, m83bcc511a466), so the model's
  counterexample and the harness's failing test can be read side by side. The model also has NoLockRule and
  NoTimeouts. Those become pe-agree mutations once pe-agree's rules are fixed.
- **Owners.** The list grows with each safety rule. Every rule's owner adds its mutation, with the harness's test
  that must catch it. The pe-world ones go through @world-model, and the merge ones through @diverge-eng.
- **Tier 2** also runs `cargo-mutants` over pe-agree each night, as a wider net [I]. It reports. It does not block.

### 6.2 Planted violations of the checkers themselves

A checker that never fires proves nothing. For each property, `tests/props_bite.rs` builds a trace that breaks it,
either by hand or by corrupting one KvReplica mid-run, and requires the checker to report it:
- P1: two nodes' replicas differ in one field at one cut;
- P2: the replay is given one entry fewer;
- P3: a restore to one frame below the final frame (and, as its negative twin, a restore to the final frame itself, which must pass);
- P4: a cut that includes an entry past a gap;
- P6: a replica that panics;
- P8: one paused frame;
- P9: a merge result with one action missing;
- and the same for the others.

## 7. randsched

- **What it generates:**
  - member counts of 2 to 5 in the fast profile, and up to 8 in the deep profile. Pairs are included: at n = 2 the
    quorum is both members, so a pair exercises leave, the survivor that never stops (plan 4.3), and the fixed rule
    for "who is right" with no majority (file 14, 4.3);
  - one faulty member, Byzantine only at n ≥ 4 (20:05Z: at n = 3, only crash and author faults);
  - its behaviour, drawn from the adversary kinds;
  - network faults: delay, jitter, loss, duplication, partitions and silences;
  - all of it on random timings.
- **What it checks:** P1-P6 and P11 on every run, and model conformance on every trace.
- **Seed budgets:** the old suite ran 12 seeds (`scripts/run_session_sim.sh:94`). The 120-seed runs for each
  switched-off fix exist only as evidence written into `INVARIANTS.md`; no script ran them [V]. Here the deep
  profile is a script that the gate's tier 2 runs.
  - The deep profile starts at 120 seeds per mutation and per shape. Then each mutation's catch rate is known.
  - The fast profile in tier 1 is bounded in time, not in seeds, so a landing never waits on it.
- **Old shapes kept:** the widening's four review shapes (prevote fork, crash after prevote, the slow acker with a
  late join, climb-ahead) are restated for cuts, as far as they still mean anything. Each is a seeded shape on its
  own random stream, as before.
- **Across platforms:** the old randsched log's normalised digest was the same on all four matrix platforms
  (`spikes/out/matrix/matrix.txt:112`) [V]. Tier 2 keeps that check: each runner writes the digest of the final
  roots of a fixed seed set, and they must be identical (plan 4.6).
- **GST:** each schedule has a global stabilisation time. Faults happen before it and links are clean after it. P5
  and P11 are judged from it.
- **The shrinker:** a failing schedule is reduced one fault and one action at a time, while it still fails.
  Timings are shortened to the smallest that still fails. It writes a scenario file and the seed. That file goes
  into the suite as a regression scenario, judged by the properties like any other.

## 8. Measurements

- **The delay pair**, for each place where a user can be held up (plan section 1):
  - the time until another node first simulates an action;
  - the time until the action is final.
- **The other measurements:**
  - paused frames;
  - rollbacks per second and the deepest rollback;
  - the display layer's counters, once WS-render's display layer is driven headless: corrections per minute, mean
    visible error, and the error of a player's own hands and held objects, which must be exactly 0;
  - the repair rungs used.
- **Each is a distribution:** at least 20 seeds and 5 delays, all players acting, with loss-only congestion, at the
  default 60 Hz and at the ends of the supported tick range once it is set (md4b42d16d9df).
- **Output:** a JSON summary and a Markdown table of quantiles, written to the target directory. Never committed
  as a reference.
- **Gated measurements:** only those the plan states as a rule. Zero paused frames is one. An own hand's error of
  exactly 0 is another.

## 9. The three probes

Each is a test in `crates/pe-harness/tests/probes.rs`, built with the Rust builder, against pe-agree. They run
with `KvReplica`, and then with the ball pit in the slow profile.

### 9.1 probe-d1: the Critical unvalidated-entry confirm

The old schedule (`appendix/evidence/probe-d1-unlinked_confirm.rs`) restated for cuts:
- Three members: V (1), H (2) and M (3). M is faulty. At n = 3 the faults are author faults only, so this is
  within the model.
- At about frame 100, M sends chain entry k, stamped about 100, to everyone.
- M withholds k+1. It then sends k+2, back-dated to frame 97, to V alone. So k+2 arrives past a hole, with a stamp
  below k's.
- Cuts keep being proposed and voted throughout, in rotation.

**The stamp rules,** as model v0 states them (@world-chunks, mb3c42176b5ab and md922aa4d8491, `model/cuts-v0`;
`docs/spec/agreement.md` says the same):
- **Monotone stamps (the horizon rule's new form):** an entry is void if its stamp is **strictly below** the stamp
  of an earlier entry in its author's chain. An equal stamp is valid, ordered by seq. A void entry still links: the
  chain continues through it, and a cut may cover it, but it is never final-effective.
- **Late:** a covered entry stamped before the previous cut's frame is void.
- **Uncovered:** an entry that a final cut does not cover, stamped before that cut's frame, is void.
- **Pending:** while a member lacks an earlier entry of a chain, the later ones are pending. They are neither
  final-effective nor final-void, and they cannot support a vote. Pending is about finality and votes only:
  pe-replica may still **predict** a pending entry, flagged unvalidated, so a lost entry costs no hand stall.
- **Voids are decided by a final cut plus in-order evaluation of the covered chain, never by arrival.**

So k+2 is void in every variant. What differs is the reason, and when the verdict is reached. **The expected void
set and its reason come from `conform`:** the same schedule run through the model's transition rules. The test
compares verdict and reason, so a verdict reached for the wrong reason fails too.

**What must hold, in every variant:**
- k+2 is never final-effective anywhere. While k+1 is missing, V may predict it, but only flagged unvalidated: no
  vote of V's rests on it, and no final cut counts it as effective. Once the verdict is void, V's prediction of it is
  rolled back, above the newest final cut, as any prediction may be (P3 still holds).
- No honest member votes for a cut that covers k+2 while it lacks k+1. A cut that is final in the meantime covers M
  only up to k.
- Every honest member reaches the same verdict on k+2, for the same reason, and that verdict equals the model's.
  All void sets are equal (P4).
- No final frame rolls back (P3). Nothing panics (P6). Every replica is equal to the others and to the replay (P1,
  P2).

**Variants, each with its expected reason:**

| Variant | Expected verdict on k+2, and its reason |
|---|---|
| The main schedule: k+1 arrives after a final cut past frame 97 | Void. If no final cut has covered it, as "uncovered"; if a later cut covers it, as "stamp not above an earlier stamp". `conform` names which |
| k+1 never arrives | Never covered. Void as "uncovered" once a final cut passes frame 97 |
| k+1 arrives before any cut passes frame 97 | Covered, and final as void: "stamp not above an earlier stamp". Never effective |
| The main schedule at n = 4, with a Byzantine M | As the main schedule |
| **The joiner variant:** the same hole in a pending joiner's chain at admission (old `membership.rs:2294-2297`). It runs once WS-agree's join exists (M1) | As the main schedule |
| **Control twin:** the same schedule with k+2 stamped 101, above k | Valid: covered, final and effective everywhere once k+1 arrives. This keeps the test from passing a core that voids everything after a hole |
| **Same-frame twin:** an honest author sends two entries at one stamp (a grab input and a hand pose) | Both valid and final-effective, ordered by seq. The model has the same twin (md922aa4d8491) |
| **Restarted chain:** M opens a new chain (same key, new nonce) whose first entry is stamped below M's last stamp on its old chain | Void: the floor holds per key, across every chain the receiver holds (md922aa4d8491). A restart is membership-shaped, so it is in model v1 (join, leave, return). This row is a pending test until v1 and pe-agree's return exist |

**Controls:**
- `VoteWithoutChainValidation` must make the test fail through P3 or P4.
- `VoidByArrival` must make it fail through P4.
- `HorizonRuleOff` (the monotone-stamp rule off) must make variant 3 fail through P4: k+2 becomes effective.
- The model's own counterexample for the same mutation is WS-model's (plan M0). @world-chunks adds the model twin
  of `HorizonRuleOff`.

### 9.2 probe-capture-close

The old schedule (`probe-capture-close.toml`):
- Four members. Member 1 touches an object at frame 60.
- At frames 120-123, member 4 defines a role with no permissions and gives it to members 1, 2 and 3.
- Member 4 goes silent at 3 s.
- The old result: no admin left, and the session closed for everyone at frame 410 [V `probe-g3-capture-close.log`].

**The public-template variant (the test):**
- Member 4 joined without admin rights, so its DefineRole and three SetRoles are refused with "no permission". Each
  refusal names member 4 (P6). The roles are unchanged, at every final cut.
- Members 1-3 carry on, with no pause (P8). Member 4's silence costs nobody a frame. Cuts stay final at n = 4,
  where q = 3 and three remain.
- All replicas are equal, and equal to the replay (P1, P2). Governance is inside the hash, so P1 covers the roles.

**The all-admin variant (pending, M3):**
- No admin is reachable, so guarded mode, then the owner's countdown (30 s by default), then the world closes with
  its state saved (mb09ab8952a38).
- The owner can then restore the roles.
- Its expectations are written now against S3's events. It stays in the pending register until WS-gov lands.
- The private-world rule is WS-gov's to state. The test follows that specification once it exists.

**When it can pass:** governance validity is WS-world's until WS-gov exists (20:05Z). So the public-template variant
can pass in M1, if pe-world's governance refuses an action without permission. The plan says it fails until M3. I
register it for M1 and move it to M3 if WS-world's M1 scope excludes roles [I].

### 9.3 probe-twosilent5-kick

The old schedule (`probe-twosilent5-kick.toml`):
- Five members. Member 1 touches an object at frame 60.
- Members 4 and 5 go silent at 3 s.
- Admin 1 kicks member 4 at frame 360 and member 5 at frame 361 (about 6 s).
- The old result: no decision, and 616 paused frames by 15 s [V `probe-h1-twosilent5-kick.log`].

**What must hold, under the strict 2/3:** n = 5, so q = 4, and three are left.
- **Nobody freezes.** Members 1-3 have zero paused frames throughout (P8). Their actions apply at once and are
  marked unconfirmed from about 3 s.
- **The kick is undecided.** The two kicks are ordinary entries. No final cut includes them while only three vote.
  The membership is unchanged at every final cut.
- **Variant A: the two return within the minute,** at 30 s.
  - The lines merge with no one asked. The merge is the three-way merge at main's final frontier (20:05Z). All
    five end equal (P1), and equal to a replay of the merged log (P2).
  - The merge keeps work (P9). Every action of 1-3 made while unconfirmed survives, unless the ordinary rules
    refuse it in the reference run with no split.
  - Then the kicks are decided like any entry, now with a quorum.
- **Variant B: they never return.**
  - Members 1-3 stay unconfirmed for at most 60 s, then emit `LineDiverged`. The test checks the bound from the last
    final cut, about 3 s. Whether the minute counts from that cut or from the moment the quorum was lost is for
    `docs/spec/agreement.md` to fix; the test follows it [I].
  - From there they are their own line, with n = 3 and q = 2. Finality resumes on it within P5's bound.
  - No final frame of the old line is undone (P3). Both lines keep their records, so a later re-merge is possible
    (M5, not tested here).
- **Variant B2: 4 and 5 are alive, but cut off** (a partition, not a crash).
  - They also stay unconfirmed, and then emit `LineDiverged` after the minute. They form their own line, with n = 2 and
    q = 2, where finality resumes.
  - Each node knows which line it is on: the lines have different line ids, and no node finalizes on both (P1 per
    line).
- **Variant D: a returner with back-stamped actions.** One of the two is silent for more than 2 s, then returns
  with grabs stamped back inside its silence, on objects that members 1-3 grabbed in entries already final.
  - **The verdict** (coordinator, 2026-11-10 20:40Z, decision 1): a returning entry never beats a final entry on an
    exclusive hold or a precondition. So the back-stamped grabs lose to the final grabs, on every node, with a
    reason naming the final entry. Elsewhere a returning entry replays at its stamp.
  - **Control:** a `MergeByStampOnly` mutation (pe-diverge), where stamp order alone decides, must fail the test
    through P3 or P9.
  - **Variant D2:** a back-stamped delete of an object that main edited later. It is unwitnessed, so it is refused
    with "lost to a final entry" and main's edits stand. `docs/spec/divergence.md` 9.3 and 12 (v1.5, 4cb691d) [V].
  - **The rule behind both** is the witnessed/unwitnessed back-dating rule, which the user confirmed (mfc2bbd9d28ad,
    19:47Z). An entry no other member acknowledged within `B` of its stamp loses to any final entry whose outcome it
    would change. **Witnessed twin:** the same grabs, acknowledged in time by a member on the returner's side. They
    are ordered by stamp like any other entry. This keeps the test from passing a merge that refuses every returning
    entry.
- **Variant C: one returns at 30 s.**
  - Four of five can vote, and four is the quorum. Cuts become final again, and the kicks are decided.
  - The returning member's line merges as in variant A.
- **Controls:**
  - `StopPlayBelowQuorum` must bring back the paused frames: P8 fails.
  - `MergeDropsSide` must fail P9 in variant A.

**When it can pass:** below-quorum play, the one-minute bound and the merge are M3 work (plan 4.2). The zero paused
frames and the undecided kick can pass as soon as pe-agree never pauses. I register the first two bullets for M1,
if WS-agree's M1 scope says play never pauses, and the variants for M3 [I].

### 9.4 Pending tests

A test written before the code it tests exists is registered in `pending.rs`. Its entry names the milestone it
waits for, and **the property id it is expected to fail with** (P3, P8 and so on).
- **The gate runs pending tests and requires each to fail with its named property.** A failure for any other
  reason (a panic in a stub, a build error, a different property) fails the gate. So a stub cannot make every
  pending test fail as expected.
- If one passes, the gate says so, and the test moves out of the register in the same landing.
- So a pending test can neither rot unnoticed nor pass unnoticed.
- `#[ignore]` is not used for this.

## 10. What I need from the seams, and what was agreed

I sent these asks to @engine-architect (WS-seams) and @world-chunks (WS-model) on 2026-11-10 (m6f540250c74f). Their
answers came the same evening: m03f00fa4bc71 and m83bcc511a466. Seams v0 is written after the archive lands, and
this section is checked against it then.

| # | Ask | Answer |
|---|---|---|
| S3.1 | Sans-IO, no hidden inputs: no wall clock, no unseeded randomness, no iteration order that reaches an output. Keys and the seed come in the configuration | **Agreed** |
| S3.2 | The node's own time as input | **Changed:** `tick(HostTime)`, local monotonic nanoseconds. pe-net's sans-IO clock maps it to frames. Bounds that every machine must decide alike are stated in seconds, converted at the world's tick rate, and decided at cuts (section 3.2) |
| S3.3 | `receive` returns `Err(Fault { author, reason })` and never panics. The fault is also an event | **Agreed** |
| S3.4 | Typed events: Proposed, Voted, Final, Voided, Refused, Equivocation, Joined, Left, Kicked, QuorumLost, Unconfirmed, Diverged, Merged, and Paused (which must never fire) | **Agreed** |
| S3.5 | A readable cut: height, frame, line, each author's last included entry, the state hash, the voters' signatures | **Agreed.** Added: the signed bytes of entries and cuts bind the world id (the genesis hash), and cuts carry a line id. Per author, the cut holds the last (chain id, seq, hash). **Votes are over (cut, the voter's own root),** so a cut is final only with a root a quorum computed itself. A `RootSplit` event, and a `revote` after a repair (me815494bd6dc). P1 and P12 read both |
| S3.9 | (from others) `hold_votes_from(frame)` while a build is pending; `final_entries(from, to)`; the self-test verdict in votes, for the tie rule at n = 2 | **Accepted into v0.** P2's replay reads its log through `final_entries`, never through a node's internals |
| S3.6 | Queries: the final cut, membership, quorum size, unconfirmed since when, live statistics | **Agreed** |
| S3.7 | The `mutants` feature with named mutations | **Agreed,** with five more (section 6.1) |
| S3.8 | One trace vocabulary | **Agreed in two parts.** pe-seams carries the events as one `TraceEvent` vocabulary, the harness's trace: pe-agree's (my list) and pe-diverge's (@diverge-eng, m1b8df0d6dede: Diverged, RungStarted, RungFailed, Repaired, RepeatedDivergence, RootSplitStarted, RootSplitEnded, BundleWritten, and from M2 MergeComputed). pe-agree's below-quorum event is named `LineDiverged`, so that it is not confused with pe-diverge's root `Diverged`. The model keeps its own action vocabulary for now: Deliver entry or proposal, Vote, ObserveQc, Timeout, AdvanceByTc, Propose, Commit. One shared enum would pin pe-agree to the model's abstractions (@world-chunks). `conform` holds the mapping from TraceEvent to model actions. It is written once pe-agree exists, and moves into a small crate if both sides need it |
| S2.1 | Object-safe, for the recorder | **Agreed** |
| S2.2 | A deterministic factory | **Agreed** |
| S2.3 | A test-only `corrupt(object, field)` | **Agreed,** behind `test-util` |
| S2.4 | `differing(&Hash)` | **Changed:** a subtree-hash exchange. @engine-architect's preferred calls are `node(root, path)`, `export(root, ids)`, `import(at, records)` (each path checked against the remote root) and `verify(root)`; the roots of the last 8 final cuts are kept. @world-model and @diverge-eng settle the final set. Plus per-group save, restore and step for the merge (20:05Z); in M1 one group is the domain. A body whose solver state can't be patched exactly is refused at rung 2 with `NeedsPhysicsImage`. The repair tests, P9 and P12 drive these; `import` is what `ImportUnchecked` turns off |
| S1.1 | A test-only builder that signs any envelope, valid or not; deterministic keys from a seed | **Agreed.** The envelope: world id (signed), author key, chain id = BLAKE3("pe.chain" ‖ world ‖ key ‖ nonce), seq, prev hash, domain id, frame stamp, class, payload, signature. Equivocation is two entries at one (chain id, seq) |
| — | pe-agree's message codec under `test-util`, for the adversary (section 3.4) | **Agreed** by @world-chunks (md922aa4d8491), who owns it |

**From the model's side** (@world-chunks):
- The model's cut has no state hash in v0. It is the frame for the height, each author's last included entry, and
  the chain of views.
- `StopPlayBelowQuorum` belongs to model v3, because v0 has no below-quorum play. So the harness's h1 control stays
  ahead of the model until M3.
- **An abstract root in the model** (@design-reviewer, Medium 4). WS-diverge makes a cut final only with a root a
  quorum computed itself. Without even an abstract root in the model (a function of the included entries, plus a
  fault flag), that rule is checked only by the harness's P1 with `VoteWithoutRoot`. I have asked @world-chunks to
  add one in v0 or v1.

**S4 Transport:**
- The harness implements S4 over its link model in M1.
- The network distributions of M3 should run through pe-net's real sans-IO congestion path over the harness's
  datagrams (plan 4.3: "the harness has no oracle input"). That is for later, with @shader-lang (WS-net).

## 11. Profiles and the gate

| Profile | Where | What runs | Bound |
|---|---|---|---|
| fast | tier 1, every landing (scoped to the crates it touches, 20:05Z) | the probes; P1-P6 on the kept scenarios with KvReplica; randsched for a fixed time; the props-bite tests; the mutation suite; the pending register | a few minutes |
| slow | tier 1 when pe-replica changes; tier 2 | the same with the ball pit replica | tens of minutes |
| deep | tier 2, nightly | randsched's deep budget at up to 8 members; long runs for P7 (two hours of virtual time with eight members); the measured distributions; cargo-mutants | hours |

Every cargo job here runs through `heavy.sh`, in my own worktree with its own target directory (team rules).

## 12. Order of work

1. **M0, against mocks:**
   - `clock`, `net` and `kv`, with their own unit tests;
   - the scenario format and runner, over a mock S3 that is a trivial agreement over KvReplica: unanimous, with no
     faults. It exists only to test the harness;
   - `props` with the props-bite tests;
   - the three probes written against S3, registered as pending.
2. **M0, once seams v0 is fixed** (it is: f3c1e6a7). Retarget everything at the real traits; the mocks become
   test-only. The pieces, in order:
   - **The clock:** a `FrameClock` over each node's HostTime at the world's `SimRate` (30-120 Hz, 60 by default).
   - **KvReplica as S2's `Replica`.** Its ops travel as `Class::Edit` payloads in its own codec. It keeps a keyframe
     per frame above its floor. `advance` rolls back to the source's `take_changed_from` and replays. `root_at_cut`
     replays from the floor with exactly the cut's entries. `finalize` compares the root, and `replay_from_last_agreed`
     is rung 1. `Tree` serves one inner level over the object and table leaves, with proofs. `import_at_cut` checks
     every proof before it patches. `Corrupt` is test_util's trait. `physics_image` is none, and the view is empty.
   - **The lockstep mock as S3's `Agreement`** (and `EntrySource`), signing with test_util's `TestKeys`. It keeps the
     unanimous rule on purpose, so P8 still bites.
   - **The node:** a copy of the driver loop, deleted when pe-replica's `drive` lands. It delivers, submits, ticks,
     sends by S4 `Lane`, advances, answers `wants_roots` with `root_at_cut` and `vote`, and passes each new
     `final_cut()` to `finalize`. The recorder records `Floor`, `RolledBack` and roots beside the `TraceEvent`s.
   - **The trace and the checkers,** retyped to `MemberKey`, `CutRef` and `TraceEvent` (P1 reads `CutFinal`, P4 reads
     `Voided`'s `cut`).
   - **probe-d1** with `EntryBuilder`, against the node over pe-agree, registered pending until pe-agree runs.
3. **M1, as pe-agree lands:**
   - the probes leave the register as their parts pass;
   - the mutation suite;
   - `conform`, with WS-model's transition function;
   - randsched against pe-agree;
   - the scenario sorting;
   - the divergence catalogue's M1 rows;
   - the measurements;
   - the trace format for WS-app's two-process run.

**Size [I]:** about 1-2 engineer-weeks in M0 and 3-4 in M1, as the plan has it.

## 13. Open points

- **Who builds pe-agree: settled.** @world-chunks owns WS-agree (pe-log, pe-agree, `docs/spec/agreement.md`) as well
  as WS-model (@coordinator-opus, mb19e0cd1edef). The S3 asks of section 10 are therefore agreed with @world-chunks
  for pe-agree's side, and with @engine-architect for the seam.
- **The mapping from TraceEvent to the model's actions** (10, S3.8): written once pe-agree exists.
- **When the capture-close and twosilent5 parts can pass** (9.2, 9.3): this follows WS-world's and WS-agree's M1
  scopes.
- **P4 and dissemination: settled** (@coordinator-opus me16a12755f58, with @fable-reviewer). P4 stays strict. S3
  promises that an entry held by any honest member of a log is held by every honest member of it before any of them
  treats as final the first cut that covers or voids it: relay as the fast path, fetch-by-name at the cut as the
  guarantee. probe-d1 runs each variant with the relay and with fetch alone, plus a control with neither (it must
  stall). **Only a covered entry can be void** (mccb3d2b98d1c): an uncovered entry is held, never applied, and the
  first cut that covers it decides. So the control twin covers k+2 at the first cut past its stamp (k+1 early); with
  k+1 late, a later cut would cover k+2 below the previous cut's frame, and it would be late.
- **P5's default bound** (2 s) and **P7's memory bound:** first values, to be set from measurements.
