# Equivocation on real peers, a split each group simulates (M1 evidence gap 1, review H1 on da77373)

Measured on p2/m1-gaps 80fc608, Windows loopback, release (`run-*/provenance.txt`), twice:
`bash crates/pe-app/m1-equivocate.sh <pe-app> <out> <port> split-held`.

**The setup:**
- Four pe-app processes over signed UDP. Every one logs member 3's touches (`--watch-member 3`) and traces
  the state hashes of frames 474-500 (`--trace-frames 474:500`).
- From 7.9 s to 8.7 s, the links between the groups are delayed 120 ms: members 1 and 2 to member 4, and
  member 4 to members 1 and 2 (`run-*/faults-*.json`).
- At 7987 ms (run 1) / 7989 ms (run 2), frame 479, member 3 authors a fresh touch on object 0.
  - Version A (impulse +x) goes to members 1 and 2 as usual, and pe-app keeps it from member 4.
  - Member 4 gets version B instead (impulse -x, same seq), undelayed.
- At 7996 ms member 3's own link to member 4 slows too. Its next entry, frame 480, names A and so is the
  proof against B; it reaches member 4 120 ms late.

So for about 8 frames each group simulates its own version. `run-*/key-lines.txt` has every line quoted
below.

**How to read the trace columns:**
- "Depth" is the frame at the rollback minus the frame rolled back to; pe-session's `max_depth` tracks
  the largest. The `TRACE` line's "resimulated" count runs from the keyframe at or before that frame, so
  it is a little larger.
- Hashes are the top 32 bits of pe-session's per-frame state hash.

## Run 1 (seq 238; A e08d21d3, B 39e67417)

| Member | Holds, first seen | At its conf | Proof at | Decides | Rollbacks (time: depth) |
|---|---|---|---|---|---|
| 1 | A at 7.988 s (frame 479); B at 8.128 s | 472 | 8.129 s | 8.137 s | 8.129: 8, 8.140: 9, then 8.406 and 8.740 (resim 8 each) |
| 2 | A at 7.988 s; B at 8.128 s | 472 | 8.129 s | 8.140 s | 8.129: 8, 8.143: 9, then 8.405 and 8.740 |
| 4 | B at 7.989 s; A's successor at 8.126 s, A at 8.253 s | 474 | 8.127 s | 8.258 s | 8.127: 8, 8.258 (resim 7), 8.261: 16, then 8.310, 8.594, 8.641 |

Frame 479's hash, and object 0's velocity just after it:

| | Members 1, 2 | Member 4 |
|---|---|---|
| Before the split (frame 478 → 479) | velocity [0.495, 0.397, -0.745] | the same |
| Simulated holding its version | A: 22d79413, velocity [4.401, 6.090, -0.745] | B: cd540f0f, velocity [-3.411, 6.090, -0.745] |
| After the proof's rollback (both versions withdrawn) | 122e38f2 | 122e38f2 |
| Final (`TRACE FINAL`) | 22d79413 | 22d79413 |

Frame 486 is the same story: f14d6019 (A) and b7756057 (B), then 9899d8b4 at the proof, and f14d6019 at
the end. All 27 traced frames' final hashes are identical at members 1, 2 and 4.

## Run 2 (seq 231; A 3dd451de, B 88edbca2)

| Member | Holds, first seen | At its conf | Proof at | Decides | Rollbacks (time: depth) |
|---|---|---|---|---|---|
| 1 | A at 7.990 s; B at 8.125 s | 472 | 8.126 s | 8.134 s | 8.126: 8, 8.137: 9, then 8.405 and 8.742 |
| 2 | A at 7.990 s; B at 8.125 s | 472 | 8.126 s | 8.136 s | 8.126: 8, 8.140: 9, then 8.405 and 8.742 |
| 4 | B at 7.990 s; A's successor at 8.122 s, A at 8.248 s | 472 | 8.123 s | 8.254 s | 8.123: 8, 8.256: 16, 8.260 (resim 7), then 8.309, 8.593, 8.641 |

Frame 479:
- A, at members 1 and 2: 1b04327b, velocity x +4.253.
- B, at member 4: e212cb99, velocity x -3.559.
- After the proof: 404274c6 at all three.
- Final: 1b04327b at all three.

Frame 486: a2439cc9 / 4c17fa13, then 2bf4f3d7, and a2439cc9 at the end.

## What the runs show

* **Truly split.**
  - Both versions are stamped at frame 479, above every receiver's conf (472-474) when they arrive.
  - Member 4 first sees A at 8.253 s (run 1) / 8.248 s (run 2). That is after its proof, which is member
    3's next entry naming A as its predecessor, against B.
  - Members 1 and 2 first see B about 140 ms after A, through member 4's delayed link.
* **The disagreement, measured.**
  - From frame 479 to 486 the groups simulate different states. Frame 479's hash is 22d79413 (A) at
    members 1-2 against cd540f0f (B) at member 4. Object 0's x velocity is +4.40 against -3.41 (run 1).
  - The proof reaches each member at frame 487, about 140 ms after the split. Each member then rolls back
    8 frames, and all three re-simulate frames 479-486 with neither version: identical hashes,
    122e38f2 ... 9899d8b4. pe-session's `reconcile` withdraws a split seq until the view change decides.
  - Each member's decision restores A. Members 1-2 roll back 9 frames, 8 ms after their proof.
    Member 4 rolls back 16 frames, at 8.254-8.261 s, about 120 ms later, because its votes and theirs
    cross the delayed links.
  - So the groups rejoin twice: at the proof, on the state with neither version, and at member 4's
    decision, on A's state. Between member 1's decision and member 4's, they hold different states for
    about 120 ms (A against neither).
  - The later rollbacks (8.3-8.75 s, resimulating 7-10 frames) are late entries from
    after the removal, still crossing the 120 ms links until 8.7 s. They don't involve the split (inferred
    from their timing and depth).
* **Which version won: A, in both runs, by pe-session's cut rule.**
  - The view change keeps exactly the cut: the deepest point of member 3's chain that at least k of the
    voters' frozen heads extend. Here k = 3 voters - t (1) = 2 (`robust_cut`, design 10.8).
  - A voter's frozen head is the stretch of member 3's chain it had validated when it froze.
  - Members 1 and 2 had received member 3's entries after A without delay. So both froze on A's branch,
    and the cut lands there: seq 243 in run 1, seq 236 in run 2, both from frame 486 (`FINAL` lines).
  - Member 4 holds B and withdraws it. Its `FINAL` line reads "version 1 withdrawn, version 2 counts":
    member 4 saw B first, so B is its version 1 and A its version 2.
  - The rule is pe-session's, read from the code; the cut seqs and the version that counts are measured.
    In the within-one-frame runs (`../equivocation-split-loopback-windows`) neither version survived.
* **End state, both runs.**
  - Members 1, 2 and 4 are ALL EQUAL (digest 0251bd249d397813 in run 1, 713d3092da4ae52c in run 2), each
    equal to its offline replay (`compare.txt`).
  - 148 checks ok and 0 bad at each member, `equivocation_proofs` 1 at each, and one decision each
    (epoch 1, removed [3]).

Member 3 (the equivocator) also traces and logs. Its `out-equivocator` folder is not kept: once removed,
it is ended by the timeout before it writes.
