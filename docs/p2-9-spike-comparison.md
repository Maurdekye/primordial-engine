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
(`scripts/p2-3/p29_causes.py <repo>`):

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
testing their point. The mismatches seen are in unconfirmed frames only, so safety holds. The caps
are scaled so that each run tests the same thing again. The scenario descriptions state the new
figures.

| run | what it tests | spike (spike caps) | pe-session, spike caps | pe-session, scaled caps |
|---|---|---|---|---|
| fault-capped | one capped link: its queue fills, the controller-less link drops, member 1 lags, and the rest has headroom | 12 kbit/s 3->1, 64 elsewhere: 156 dropped, m1 lags, ALL EQUAL | conf 46 at m1, MISMATCH in unconfirmed frames | 52 kbit/s 3->1, 186 elsewhere: **56 dropped**; m1 rolls back up to 45 frames (others 2) and confirms behind (552 against 556); ALL EQUAL over 0..530, every checked frame confirmed. Without the 3->1 cap: **0 dropped** (headroom) |
| fault-capped-collapse | a cap that binds everywhere, no R18: queues overflow and confirmation stalls, and safety holds | 16 kbit/s: stall at 114, confirmed prefix equal | stall at 34; the prefix check reaches unconfirmed frames | 46 kbit/s: **534 dropped**, stall at 104, confirmed prefix 0..95 **equal on every replica and the replay** |
| fault-capped-r18 | the same links with R18: it thins entries and acks, keeps confirming, drops nothing | 16 kbit/s: 0 dropped, conf 568 | 98 dropped, conf 34: FAIL | 46 kbit/s: **0 dropped**, conf 586, ALL EQUAL. R18 engages: 1158 acks against collapse's 1752 (spike: 980 against 1752) |

For fault-capped's 3->1 cap I tried:

| 3->1 cap | dropped | lowest confirmed frame | result |
|---|---|---|---|
| 45 kbit/s | 86 | 495 | frames 495..530 unconfirmed |
| 48 kbit/s | 73 | 528 | still short of 530 |
| **52 kbit/s** | **56** | **552** | **passes** |
| 56 kbit/s | 39 | 552 | passes |

52 kbit/s is the lowest cap tried that passes, so it drops the most while still confirming every
checked frame.

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
| p2-4-deepfork | ids and hashes; D72 touch acceptance |
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

