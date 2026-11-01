# Equivocation on real peers, split before confirmation, settled within one frame (M1 evidence gap 1)

The within-one-frame LAN case. On loopback the proof reaches every member inside the frame of the split,
before pe-session's next simulation, so no member ever simulates its version: these runs do not measure
a disagreement. The held split in `../equivocation-split-held-loopback-windows` does (review H1 on
da77373).

Measured on p2/m1-gaps bb5f5f6, Windows loopback, release (`run-*/provenance.txt`), twice:
`bash crates/pe-app/m1-equivocate.sh <pe-app> <out> 7391 split`. Four pe-app processes over signed UDP,
every one logging member 3's touches (`--watch-member 3`). At 8 s member 3 authors a fresh touch on
object 0. Version A (impulse +x) goes to members 1 and 2 as usual, and pe-app keeps it from member 4.
Member 4 gets version B instead (impulse -x, same seq). `run-*/key-lines.txt` has every line quoted.

Run 1 (seq 235; run 2, seq 236, is the same to the millisecond's spread):

| Member | First sees | At its conf | Proof at | Decides | Rollbacks, max depth |
|---|---|---|---|---|---|
| 1 | A (fdd3ca11) at 8.005 s, B at 8.007 s | 477 | 8.008 s | 8.011 s | 2, 1 |
| 2 | A at 8.005 s, B at 8.007 s | 477 | 8.008 s | 8.013 s | 2, 1 |
| 4 | B (e48f2208) at 8.004 s, A at 8.010 s | 477 | 8.005 s | 8.013 s | 1, 1 |

* **Truly split:** both versions are stamped at frame 479, above every receiver's conf (477-478) when they
  arrive. Member 4 does not see A until after its proof: the proof is member 3's next entry, which names A
  as its predecessor, against B. Members 1 and 2 see B only in member 4's proof broadcast.
* **No disagreement was simulated.** Members hold different versions for 2-6 ms. Every line about the
  split, from the first sightings to the decisions, is at frame 480 in run 1. pe-session simulates once
  per new clock frame and folds that frame's late arrivals into one rollback, so by its next simulation
  each member has already withdrawn both versions and re-simulates frame 479 with neither. In run 2
  members 1 and 2 see A at frame 479, but the proof arrives before or with their advance to frame 480.
  The rollbacks (depth 1) are that one coalesced re-simulation, not a resolution of two simulated
  states. This is inferred from the frame stamps and the coalescing rule; these runs have no frame-hash
  trace (`--trace-frames` came later).
* **Which version won: neither.** Every survivor withdraws both versions (`FINAL` lines), in both runs.
  pe-session's rule (review NEW-2, `reconcile`): after a proof, the member's entries from the disputed
  seq on are withdrawn unless held identically by every member, and no version of a split seq is. The
  view change then keeps exactly the cut: the deepest point of member 3's chain that 2 of the 3 voters'
  frozen heads extend (`robust_cut`). Neither version survived, so that cut lies below the split seq.
  That is inferred: bb5f5f6 did not log the cut.
* **End state, both runs:** members 1, 2 and 4 ALL EQUAL, each equal to its offline replay, 148 checks
  ok, 0 bad, `equivocation_proofs` 1 each, one decision (epoch 1, removed [3]) at every member
  (`compare.txt`). In run 2 every member rolled back once.
