# The unread polka ids off the wire (p2/polka-ids, from main 21d69ae)

@item:primordial-engine/remove-the-unread-polka-ids-from-capped-precommi, done under
@item:primordial-engine/pe-session-per-entry-wire-savings-signatures-pe-a. A capped precommit carried its polka's
prevote ids (review VC4R-1); trust survey #4 retired the only rule that read them. Removed in the spike and in
pe-session, with `MAX_POLKA` and the prevote-id index. Every precommit loses the list's u32 count (4 B), and a capped
one its ids too. capfake, which differed from capstale only in a fake id, folded into it (its repro, scenario and two
logs removed).

Measured on c85fea7, Windows, release (target/pol.sh) [V]:
- spike suite: every scenario ALL EQUAL, 0 fact changes; 1 log moved, r14-chain-epoch (the transfer ids are hashes
  over entries that include precommits; one RESULT line moves one line, unchanged).
- pe suite: the same, the same log. (`--pe` failed on main on the four Part B one-way-cut runs, which only the spike
  has; run_session_sim.sh now runs them for the spike only.)
- regress: 113 identical; r14-chain-epoch differs as above and is re-blessed.
- r18-pe: 15 of 15 identical.
- r18, r18-thin, r18-thin-partb, r18-thin-pe: 1, 10, 1 and 15 cells moved; the scripts' end-state check passes.
  main 21d69ae's own binaries reproduce every committed log, so all of the moves are this change. The cells run view
  changes (a temporary print counted 7 precommits in r18-thin up-k8-m4, 12 in down-k8-m3), and a 4 B change per
  precommit moves them both ways: `moved-cells.txt`.
