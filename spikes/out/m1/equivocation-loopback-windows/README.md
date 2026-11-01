# Equivocation on real peers, late version (M1 evidence gap 1)

Measured on p2/m1-gaps bb5f5f6, Windows loopback, release (`provenance.txt`):
`bash crates/pe-app/m1-equivocate.sh <pe-app> <out> 7381 late`. Four pe-app processes over signed
UDP, every one logging member 3's touches (`--watch-member 3`). At 8 s member 3 signs a second version of
its latest touch (seq 228, stamped 460 and sent at 7.67 s, already below every member's conf) with its
own key and sends it to member 1 only. `key-lines.txt` has the lines below.

* **What this covers:** a late second version of an entry that was already sent and confirmed, as in
  session-sim's `badvote`. There is nothing to repair. The split before confirmation is
  `../equivocation-split-loopback-windows/`.
* **Detection:** member 1 sees version 2 at 8.004 s (its conf 476) and holds the proof at once. Members
  2, 3 and 4 hold it by 8.007 s, and every member decides **epoch 1, removed [3]** by 8.013 s.
* **Which version counts:** version 1 everywhere, with version 2 withdrawn (`FINAL` lines).
  pe-session's rule (review NEW-2, `reconcile`): after a proof the member's entries from the disputed
  seq on are withdrawn unless held identically by every member, so version 1, stable at all of them,
  stays; after the view change, exactly the cut.
* **End state:** members 1, 2 and 4 ALL EQUAL, each equal to its offline replay, 0 rollbacks, 148 checks
  ok, 0 bad, `equivocation_proofs` 1 each (`compare.txt`). Member 3, removed, stops confirming and its
  timeout ends it, so it writes no summary.
