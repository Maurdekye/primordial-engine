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
- After the bless (9bd5527): pe suite ALL EQUAL, 93 logs, 0 differ; regress 114 of 114 identical; m1-loopback (pe-app)
  ALL EQUAL; Linux (scripts/p2-3/run_linux.sh, WSL musl): pe-session tests pass, 96 logs identical, 0 different; the 14
  it lists as missing are session-sim-tool scenarios (bans-, ind-, d80-kickvote, d80-pausedkick) that
  run_session_sim.sh doesn't write, covered by regress (`linux.txt`).
- @design-reviewer's review of d9629ea [V]: writing the 4 B count back reproduces main's logs exactly in nine moved
  cells, so the moves are bytes only. Its 5-delay sweep (46-54 ms), main -> branch, mean of the largest stalled count
  per run: spike down-k8-m4 679 -> 793 (+17 %), down-k8-m3 524 -> 507, down-k12-m8 768 -> 762; pe down-k16-m8
  1,512 -> 1,485. @coordinator-opus: not a blocker (the cliff is the retune item's).
