# P2-9 on P2-3: the session tool's goldens and the spike comparison

After P2-3, `tools/session-sim` runs on pe-session, and its goldens are pe-session's own logs
(`spikes/out/session-sim-pe/`). That's @coordinator-opus's ruling of 2026-10-31 06:22Z. This report
gives the conditions that ruling set:

1. For every scenario whose pe-session log differs from the spike's, its cause.
2. For the bandwidth-capped runs, evidence that each still tests what it was written for.

Roles: the port is @world-chunks's work and @ballpit-viz reviews it. @chunks-reviewer reviews this
report.

Tags: [V] verified by run on this tree; [I] inferred.

## State

- **pe-session:** p2/session at 44a1b2f (d3c11e9 adds evidence only). That is P2-4 round 7 (spike
  at main de3dbf9) merged into pe-session, plus main 5a71ca7, which includes P2-9 delta 5 (26ed241).
- **Spike reference:** the committed spike logs in `spikes/out/session-sim/` at main 5a71ca7. That is
  93 logs: the `run_session_sim.sh` suite, the three controls, and the tool's seven `faults/` goldens.
- **The tool on pe-session** [V]:
  - `regress`: 92 of 92 scenarios identical to the pe-session goldens, every one ALL EQUAL, with 0 false
    checks. This holds on Windows and on Linux (static musl in WSL2): `regress-windows.txt`,
    `regress-linux.txt`.
  - `spike-only.txt` lists two pe-session harness scenarios the tool has no behaviour for yet:
    grabrace (D72 grab races) and p2-3-statejoin (state transfer).
  - randsched stays in `not-scenarios.txt`. It reports `ALL PASS` on pe-session as on the spike [V].
- **The pe-session harness suite** (`run_session_sim.sh --pe`) [V]: 88 logs and 31 tests, with Linux
  equal to Windows on every log.

## Method

Each scenario has three logs:

- **the spike's;**
- **pe-session's under the spike's claim rule.** This is `PE_CLAIM_RULE=spike`: a touch takes or
  renews a claim, as in the spike. It covers the harness suite, plus `regress --only` for the
  `faults/` scenarios.
- **pe-session's golden,** under D72.

Every comparison masks the CPU timings. A difference is attributed in two steps
(`scripts/p2-3/p29_causes.py <repo>`). What the script's masks do, exactly (review L1): it masks
every `N s` / `N ms` value, simulated times (`t=`) included, sorts every bracketed list, and compares
a log's lines as a multiset. @chunks-reviewer reran it with stricter masks (CPU timings only, only
id lists sorted, lines compared in order): they hide exactly two order differences, both from
iterating `all_entries()`, a `BTreeMap` keyed by entry id in both cores, so both are ids and hashes:
member 3's vote list in p2-4-aheadlink, and the order of the RESULT lines in the three r14-chain logs.

- **Spike vs pe-session under the spike rule:** what pe-session's types change.
  - **ids and hashes:** equal once entry ids, transfer ids and digests are masked, and id-keyed
    listings sorted. Entry ids are SHA-256 of the canonical encoding (S3b). R14 transfer ids derive
    from them. The state hash covers pe-physics, the pe-sim store and the world tables (S5c). The
    exchanged check hash is BLAKE3 (S5a).
  - **wire size:** equal only once byte counts are masked too. An entry carries a 64-byte Ed25519
    signature (S3a). Ids are 32 bytes. An ack with a check carries 32 bytes of BLAKE3, against the
    spike's 8.
  - **other:** neither. Any unexplained "other" blocks.
- **pe-session under the spike rule vs pe-session's golden:**
  - **D72 touch acceptance** (S4, design 8.4): only a grab takes, renews or loses a claim.
  - Claim rows are part of the hashed state. So D72 moves the digests of every scenario with a touch:
    65 scenarios change only digests and ids.
  - In 17, outcomes or counts change too, because previously refused pushes now apply: backdate, c1,
    c2, claims, diverged, loss, state-frame, ties, p2-4-aheadlink, r14-forge,
    p2-5-controls/r14-doublespend, and the seven `faults/` runs.

## Result [V]

Of the 93 spike logs, 0 have an unexplained cause:

| cause | logs |
|---|---|
| ids and hashes; D72 touch acceptance | 75 |
| ids and hashes; wire size | 5 (the five ballpit runs: hands only, no touch, byte lines) |
| none (identical) | 4 (p2-4-vcscale-64, p2-4-vcscale-256, randsched, p2-4-controls/onesided) |
| ids and hashes; wire size; D72 touch acceptance | 3 (diverged, loss, state-frame) |
| wire size, caps scaled (below); D72 touch acceptance | 3 (fault-capped, fault-capped-collapse, fault-capped-r18) |
| ids and hashes | 2 (p2-4-prevotefork, review2-ackfork) |
| label text; D72 touch acceptance | 1 (claims) |

**claims:** the script reports this one as "other", but the only difference is the RESULT line's
label. S4 restated it as "D72: only grabs claim, so no push is refused", in the harness and in
`claims.toml`. Its counts under the spike rule equal the spike's: 9 and 360 accepted, 12 and 240
refused. Under D72, 21 and 600 are accepted and none refused.

**Equal with the spike's claim rule.** With the spike rule, pe-session reproduces every spike log
once ids and hashes are masked, apart from claims' label: 4 identical and 81 equal [V]. That
includes round 7's capfake, fakeadmin and adminwedge, and the fix-off runs inside the repros.

## The bandwidth-capped runs (condition 2) [V]

pe-session's messages are larger. In fault-capped, an ack averages 247 B against the spike's 88, and
an entry 188 B against 100 (the `bytes:` lines). Under the spike's caps the three capped runs stall early and stop
testing their point. The mismatches seen are in unconfirmed frames only, so safety holds. Every cap
is scaled by about 2.9, close to the acks' growth, so that each run tests the same thing again. The
scenario descriptions state the new figures (measured at the P2-3 review round, 7aa4163 and after).

| run | what it tests | spike (spike caps) | pe-session, spike caps | pe-session, scaled caps |
|---|---|---|---|---|
| fault-capped | one capped link: its queue fills, the controller-less link drops, member 1 lags and repairs through member 2, and the rest has headroom | 12 kbit/s 3->1, 64 elsewhere: 156 dropped, m1 rolls back up to 60 frames, conf 534-538, ALL EQUAL | conf 42 at m1 at 3af709a (46 at a812805), MISMATCH in unconfirmed frames | 36 kbit/s 3->1, 186 elsewhere: **125 dropped**; m1 rolls back up to 32 frames (others 2); conf 558 at every member, 0 paused; ALL EQUAL over 0..530, every checked frame confirmed. Without the 3->1 cap: **0 dropped** (headroom) |
| fault-capped-collapse | a cap that binds everywhere, no R18: queues overflow and confirmation stalls, and safety holds | 16 kbit/s: stall at 114, confirmed prefix equal | stall at 34; the prefix check reaches unconfirmed frames | 46 kbit/s: **534 dropped**, stall at 104, confirmed prefix 0..95 **equal on every replica and the replay** |
| fault-capped-r18 | the same links with R18: it thins entries and acks, keeps confirming, drops nothing | 16 kbit/s: 0 dropped, conf 568 | 98 dropped, conf 34: FAIL | 46 kbit/s: **0 dropped**, conf 586, ALL EQUAL. R18 engages: 1158 acks against collapse's 1752 (spike: 980 against 1752) |

**fault-capped's 3->1 link, and a cause found by this review (R1, @chunks-reviewer).** At a812805 the
size-scaled 36 kbit/s cap made **every member pause 157-163 frames** (confirmation ended at 432-436
by frame 552-556), where the spike paused none; that run used 52 kbit/s instead, the lowest cap tried
that passed. It was not D72 (the spike's claim rule gave the same) and not the 4 kB burst.
Instrumented runs of both cores (chain prefixes, fetches, queues, repair yields and duplicate
deliveries every 100 ms) named the cause:

- the spike decides a repair per fetch request: under 100 ms of queue, the whole reply (up to 16
  entries) goes out;
- pe-session's sans-IO port marked each reply entry a repair and the IO decided per message, so a
  reply was cut short as soon as the queue passed 100 ms;
- m2->m1, member 1's only repair route for member 3's chain, sat at 70-105 ms of queue; replies
  delivered 1-2 entries, 45% of what m2 delivered to m1 were duplicates (326 of 722; the spike: 106 of
  545), and m1 took in member 3's chain at about 22 entries/s while member 3 wrote 30/s, until frame
  minus conf reached the 120-frame window at t = 7.3 s and every member paused (confirmation waits
  for m1's horizon).

The IO now sends or drops a reply whole, deciding at its first message (`Class::Repair`). The sweep
after the fix (7aa4163):

| 3->1 cap | dropped | paused frames (m1, m2, m3) | lowest confirmed frame | result |
|---|---|---|---|---|
| 30 kbit/s | 151 | 0, 0, 0 | 546 | passes |
| 33 kbit/s | 138 | 0, 0, 0 | 546 | passes |
| **36 kbit/s** | **125** | **0, 0, 0** | **558** | **passes (the scenario's cap)** |
| 40 kbit/s | 108 | 0, 0, 0 | 552 | passes |
| 45 kbit/s | 86 | 0, 0, 0 | 560 | passes (at a812805: 495, frames 495..530 unconfirmed) |
| 52 kbit/s | 56 | 0, 0, 0 | 556 | passes |

So the 3->1 link now scales like the others (12 x 3 = 36).

## Since a812805: the P2-3 review round

The per-scenario table below attributes the spike-to-pe-session differences **at a812805**. Two
changes moved pe-session's goldens after it; this section attributes them. Run at 3af709a,
`scripts/p2-3/p29_causes.py` reports 15 logs as OTHER: diverged (the set_state reset, first bullet)
and the 14 review-round logs (the bullets after it).

- **The set_state history reset (main f65851e, merged at e98261f; goldens re-blessed at dc7c378, the
  tool's fault goldens at 4a044bd).** It moved 71 goldens before the review round: digests and ids
  only, plus p2-4-aheadlink's list order. It also changed **diverged** under the spike rule: 25 of 25
  and 35 of 35 frames equal at that instant, against the spike's 23 of 25 and 33 of 35, and the two
  "differs at that instant" lines are gone. A harness build at 4a044bd already gives 25/25 and 35/35,
  so the cause is the reset [verified by run, @chunks-reviewer; mechanism inferred: carried state is
  exact while speculating, design 7.3].

The review round (7aa4163, 649a77f and the scenario fix after) moved them again. Measured with the
harness suite at 7aa4163 against the goldens committed before it (timings masked): ALL EQUAL, 0 false
checks; 56 logs identical, 17 equal with ids masked, 15 different. Compared at 3af709a it is 56, 16
and 16: statejoin changed after 7aa4163 (P23-3 below) [verified, @chunks-reviewer]. The later commits
change only runs that opt into R18 (none in the harness suite), statejoin and fault-capped. The
tool's `regress` is 92 of 92 identical to the new goldens.

- **P23-2: facts are 256-bit BLAKE3, signed with the session; the view-change value and the chain
  hash are 64-bit BLAKE3.** A precommit carries its fact signature, and an Attest its fact, so their
  entry ids change, and the change values too. With them changes the order in which id- and
  value-keyed tables iterate [inferred mechanism; the set is measured]:
  - 17 logs differ only in ids (R14 transfer ids, entry ids), among them p2-4-aheadlink, whose vote
    list prints in a different order;
  - 13 view-change and eviction scenarios move a decision by up to two frames (onesided: 448 to
    446), a member's final conf by up to 2 frames, or a resimulation count by one: backdate, c2, d80-joinrules, d80-kickpartition,
    p2-4-badproposer, p2-4-bridge, p2-4-controls/onesided, p2-4-lagchurn, p2-4-strikeframe,
    review2-ackfork, review2-lateequiv, review3-badvote, review3-liar. Each stays ALL EQUAL, and
    every decision is the same one.
- **D52 by default (`hand_predict` 6) and span-then-coast:** ballpit-60hz-2hands (about 0.2% more
  balls change per frame under coasting) [inferred]. The other ball-pit logs are unchanged.
- **P23-3: a state is restored only as the answer to this member's request, and never rewinds the
  replica.** p2-3-statejoin's joiner used to simulate from genesis and then rewind to the state.
  It now holds its sim until the state arrives, so it imports once and refuses twice: member 2's
  tampered state (Hash), then member 2's honest answer, which arrives after the tampered one used
  up that request (Unrequested).
- **Whole-reply repair (the R1 cause above):** fault-capped, now at its proportional cap (36 kbit/s).

- **D58 (main ca33fcd, merged at 88aa129; 65dc019, goldens 90e0c22).** Box3D's positions are f64,
  and `BodyState` is 16 words (64 B; a touch entry 200 B, 188 before). Against the review round's
  goldens: ALL EQUAL, 0 false checks; 4 identical, 77 equal with ids and hashes masked, 7 different.
  Among the 77, besides digests and ids [verified, @chunks-reviewer]: diverged's, loss's and
  state-frame's entries grow from 188 to 200 B, statejoin's physics state from 571064 to 581042 B,
  and handowner's grab/carry list and aheadlink's vote list print in another entry-id order. The 7:
  the 5 ball-pit logs' naive-streaming estimate (64 B a state, D58-3), and r14-forge and
  p2-5-controls/r14-forge-no-provenance, whose world A counts one more carried-state overwrite at m1
  and m2 (1 to 2). The spike's r14-forge moved the same way in its D58 re-bless (974c441 to ca33fcd)
  [verified by reading the spike's logs]. The capped runs keep their points (fault-capped: 125
  dropped, 0 paused, conf 558 at every member).

- **P2-4 round 8 (main af23ae5), ported on p2/session-r8.** With the spike's claim rule, against the
  spike's logs: voidfork, voiddeep and plainfork are equal with ids masked; pfsilent and slowcapper
  decide one late epoch a frame later (266 against 265, 278 against 277), the class of the review
  round's 13 view-change logs [inferred mechanism]. Every repro's verdicts, frame ranges and check
  counts equal the spike's. Against pe-session's goldens, exactly the 5 logs round 8 changed in the
  spike differ (adminfork, adminwedge, capfake, fakeadmin, prevotefork), and the 5 new ones are added.
  The tool's five round-8 scenarios and adminfork's and prevotefork's round-8 files reproduce them
  (regress 97 of 97); only `ownership` stays in `pe-pending.txt`.

## Fixture changes made by the port

- **A joiner's relayed log goes in chain order (author, seq).** pe-session's entry ids are hashes, so
  entry-id order is arbitrary, and it changed how far a joiner rolls back (latejoin and joinskip:
  181 against 180). @ballpit-viz made the same change in delta 4.
- **A scenario's own fixture join signature is now forged.** It's a 64-bit value in TOML. pe-session
  pads it to 64 bytes, where it never verifies, which is what such a fixture means.
- **pe-session-only scenarios:** grabrace and p2-3-statejoin. They are in `spike-only.txt` until the
  tool has grab-race and state-transfer behaviours.

## Per scenario

| scenario | cause of the difference from the spike |
|---|---|
| backdate | ids and hashes; D72 touch acceptance |
| ballpit-60hz-2hands | ids and hashes; wire size |
| ballpit-60hz-ahead4 | ids and hashes; wire size |
| ballpit-60hz-noloss | ids and hashes; wire size |
| ballpit-60hz | ids and hashes; wire size |
| ballpit-90hz | ids and hashes; wire size |
| c1 | ids and hashes; D72 touch acceptance |
| c2 | ids and hashes; D72 touch acceptance |
| claims | label text (S4 restated the rule; see below); D72 touch acceptance |
| d80-adminclose | ids and hashes; D72 touch acceptance |
| d80-joinrules | ids and hashes; D72 touch acceptance |
| d80-kickpartition | ids and hashes; D72 touch acceptance |
| d80-kickwar | ids and hashes; D72 touch acceptance |
| d80-lastadmin | ids and hashes; D72 touch acceptance |
| diverged | ids and hashes; wire size; D72 touch acceptance |
| fault-asymmetric | ids and hashes; D72 touch acceptance |
| fault-capped-collapse | wire size: caps scaled (fault-capped section); D72 touch acceptance |
| fault-capped-r18 | wire size: caps scaled (fault-capped section); D72 touch acceptance |
| fault-capped | wire size: caps scaled (fault-capped section); D72 touch acceptance |
| fault-crash-recover | ids and hashes; D72 touch acceptance |
| fault-partition-evict | ids and hashes; D72 touch acceptance |
| fault-partition-heal | ids and hashes; D72 touch acceptance |
| handowner | ids and hashes; D72 touch acceptance |
| loss | ids and hashes; wire size; D72 touch acceptance |
| p2-4-ackgap | ids and hashes; D72 touch acceptance |
| p2-4-adminfork | ids and hashes; D72 touch acceptance |
| p2-4-adminvoid | ids and hashes; D72 touch acceptance |
| p2-4-adminwedge | ids and hashes; D72 touch acceptance |
| p2-4-aheadlink | ids and hashes; D72 touch acceptance |
| p2-4-aheadshield | ids and hashes; D72 touch acceptance |
| p2-4-badproposer | ids and hashes; D72 touch acceptance |
| p2-4-bogusprecommit | ids and hashes; D72 touch acceptance |
| p2-4-bridge | ids and hashes; D72 touch acceptance |
| p2-4-capchurn | ids and hashes; D72 touch acceptance |
| p2-4-capfake | ids and hashes; D72 touch acceptance |
| p2-4-capstale | ids and hashes; D72 touch acceptance |
| p2-4-colluders | ids and hashes; D72 touch acceptance |
| p2-4-cutbackdate | ids and hashes; D72 touch acceptance |
| p2-4-deepfork | ids and hashes; D72 touch acceptance; the ack timer (review PDF-1): 1 and 2 ack once a second here (64d4578, ec773e5). Until 7.6's reductions pe-session's per-message timer was exactly 1,000 ms where the spike's rounds down to whole frames (59 × 16,667 µs = 983 ms), so pe-session paused 12-13 frames per member (conf 419) where the spike paused none. Since 7.6 reduction 2 (cfa21f1) pe-session's per-message timer rounds to frames as the spike's does, so that difference is gone (review AK-1): the cut is (77, 198), confs 469 / 416 / 416, paused 2 / 0 / 2; the fix-off control still fails (m3 conf 115 of frame 235) [V] |
| p2-4-fakeadmin | ids and hashes; D72 touch acceptance |
| p2-4-fakemix | ids and hashes; D72 touch acceptance |
| p2-4-fakeref | ids and hashes; D72 touch acceptance |
| p2-4-fork9 | ids and hashes; D72 touch acceptance |
| p2-4-futureack | ids and hashes; D72 touch acceptance |
| p2-4-ghostkick | ids and hashes; D72 touch acceptance |
| p2-4-join | ids and hashes; D72 touch acceptance |
| p2-4-joinattests | ids and hashes; D72 touch acceptance |
| p2-4-joinskip | ids and hashes; D72 touch acceptance |
| p2-4-joinsquat | ids and hashes; D72 touch acceptance |
| p2-4-jointiming | ids and hashes; D72 touch acceptance |
| p2-4-joinwedge | ids and hashes; D72 touch acceptance |
| p2-4-kickbackdate | ids and hashes; D72 touch acceptance |
| p2-4-lagchurn | ids and hashes; D72 touch acceptance |
| p2-4-lagprecommit | ids and hashes; D72 touch acceptance |
| p2-4-latejoin | ids and hashes; D72 touch acceptance |
| p2-4-prevotefork | ids and hashes |
| p2-4-refwithhold | ids and hashes; D72 touch acceptance |
| p2-4-selfkickhead | ids and hashes; D72 touch acceptance |
| p2-4-slowadmin | ids and hashes; D72 touch acceptance |
| p2-4-slowref | ids and hashes; D72 touch acceptance |
| p2-4-strikeframe | ids and hashes; D72 touch acceptance |
| p2-4-twosilent | ids and hashes; D72 touch acceptance |
| p2-4-twosilent5 | ids and hashes; D72 touch acceptance |
| p2-4-vcscale-256 | none (identical) |
| p2-4-vcscale-64 | none (identical) |
| p2-4-votechurn | ids and hashes; D72 touch acceptance |
| p2-4-withheld | ids and hashes; D72 touch acceptance |
| r14-abort-reverse | ids and hashes; D72 touch acceptance |
| r14-abort | ids and hashes; D72 touch acceptance |
| r14-chain-epoch | ids and hashes; D72 touch acceptance |
| r14-chain-reverse | ids and hashes; D72 touch acceptance |
| r14-chain | ids and hashes; D72 touch acceptance |
| r14-crash | ids and hashes; D72 touch acceptance |
| r14-forge | ids and hashes; D72 touch acceptance |
| r14-late | ids and hashes; D72 touch acceptance |
| r14-ok-reverse | ids and hashes; D72 touch acceptance |
| r14-ok | ids and hashes; D72 touch acceptance |
| r14-reject-reverse | ids and hashes; D72 touch acceptance |
| r14-reject | ids and hashes; D72 touch acceptance |
| r14-throttle | ids and hashes; D72 touch acceptance |
| randsched | none (identical) |
| review2-ackfork | ids and hashes |
| review2-evict | ids and hashes; D72 touch acceptance |
| review2-lateequiv | ids and hashes; D72 touch acceptance |
| review2-outage | ids and hashes; D72 touch acceptance |
| review3-badvote | ids and hashes; D72 touch acceptance |
| review3-crashstall | ids and hashes; D72 touch acceptance |
| review3-liar | ids and hashes; D72 touch acceptance |
| state-frame | ids and hashes; wire size; D72 touch acceptance |
| ties | ids and hashes; D72 touch acceptance |
| p2-4-controls/onesided | none (identical) |
| p2-5-controls/r14-doublespend | ids and hashes; D72 touch acceptance |
| p2-5-controls/r14-forge-no-provenance | ids and hashes; D72 touch acceptance |

