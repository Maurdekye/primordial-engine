# Equivocation on real peers, split before confirmation (M1 evidence gap 1)

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
* **The disagreement:** for frame 479 members 1-2 apply A and member 4 applies B, so object 0 differs
  between the groups. Once the proof lands each member withdraws its version and rolls back one frame
  (max depth 1), and both groups agree again from frame 480.
* **Which version won: neither.** Every survivor withdraws both versions (`FINAL` lines), in both runs.
  pe-session's rule (review NEW-2, `reconcile`): after a proof, the member's entries from the disputed
  seq on are withdrawn unless held identically by every member, and no version of a split seq is. The
  view change's cut then keeps exactly the entries below it.
* **End state, both runs:** members 1, 2 and 4 ALL EQUAL, each equal to its offline replay, 148 checks
  ok, 0 bad, `equivocation_proofs` 1 each, one decision (epoch 1, removed [3]) at every member
  (`compare.txt`). In run 2 every member rolled back once.
