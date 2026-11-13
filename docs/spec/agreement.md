# Agreement: pe-log and pe-agree (WS-agree)

Draft v0, 2026-11-11, @world-chunks. Implements seam S3 (`pe_seams::agree`) and the chains of S1
(`pe_seams::entry`). The cut rules follow the checked model in `models/cuts` (model v0, branch model/cuts-v0, in
review); a rule here that the model checks names its property or control. Every time limit is a duration in seconds,
never a count of frames (user ruling).

Decided (coordinator, @fable-reviewer concurring): the two-chain rule with a lock (chained HotStuff /
Jolteon), as modelled. Stay on the published rules: any departure from Jolteon gets its own control in the model, and
@design-reviewer's review of the model finishes before pe-agree builds on it.

## 1. What each crate owns

- **pe-log:** the authors' chains. Storing entries, checking them (shape, line, signature), linking them in chain
  order, fork proofs, cut segments and the verdicts at a cut (`judge`).
- **pe-agree's node** owns the rest of the entry side: the per-author rate budget, pending entries, the final log in
  agreed order and its retention window (`kept_from`), and each chain's highest final stamp for back-stamp verdicts.
- **pe-agree:** cuts. Proposals, votes over the voter's root, certificates, timeouts, finality, verdicts at cuts,
  dissemination (relay and fetch by name), holds on votes, and `EntrySource` for the replica. It decodes only
  `Class::Protocol` payloads.
- Both are sans-IO: time comes in through `tick(now, clock)`, messages through `receive`, keys and the seed through
  the configuration. No wall clock, no unseeded randomness, no iteration order that reaches an output. Neither ever
  panics on peer input: bad bytes are `Err(Fault)` and an event.

## 2. Chains (pe-log)

- **One chain per member per line for its life** (S1, @world-model 858d677), continued on return. A restart makes a
  new chain under the same key with a new nonce. Two chains under one key are not equivocation; two entries at one
  (chain id, seq) are.
- **Acceptance, in this order.** The author is a member (`NotMember`); shape (`check_shape`); line (signed for this
  line, else `WrongLine`); the signature (`BadSignature`); only then the rate budget (`RateExceeded`), so a forged
  entry never spends an honest author's tokens (@design-reviewer, Medium 1). A refused entry is never
  stored and raises `Refused`. One that fails shape, line or signature is a fault of the sending peer, not of the
  author it claims. A second, different entry at a held (chain, seq) is stored as a fork proof and raises
  `Equivocation`; the author's chain is frozen at the fork. The proof is relayed once when it arrives from its author
  (the fast path); a fork fetched by name is not relayed again.
- **Validation in chain order.** An entry is linked once every earlier entry of its chain is held and its `prev`
  names the one before. Entries past a hole are held but not linked: they count for no vote and no verdict
  (model: d1 is the mutation that judges them alone).
- **The stamp rule.** An entry is back-stamped if its stamp is strictly below an earlier stamp in its author's
  chain; an equal stamp is valid and ordered by seq (@design-reviewer). The node keeps each chain's
  highest stamp through its final head, so a verdict walks only the cut's new segment (Medium 3).
  - **Not built yet:** the floor per key across chains (a new chain's entries may not be stamped below the key's last
    stamp on any chain this member holds; model v1 checks the restart case). It comes with restart, section 8.
- **Pending, never applied.** A held entry no final cut includes yet is pending on every member. The replica may
  predict it locally, flagged unvalidated; that prediction is a display hint only, never a trace event and never
  part of a vote or a verdict.
- **The rate budget** counts every entry an author makes this member hold: received, relayed, and held while
  pending, including those that end up void, with a small cap on pending entries per author. Only entries a cut this
  member is checking or finalizing names are solicited and pass without a token; entries named only by a vote's heads
  are not (section 4).
- **Retention.** Final entries are kept from the later of the driver's Cv frame and a cap: the one-minute divergence
  bound plus a margin, as a duration converted with the world's SimRate, and limited in bytes. `kept_from()` says
  where the log is complete; below it `entries_at` answers "not kept", never an empty set (coordinator).

## 3. Cuts (pe-agree)

- **Quorum.** `t = floor((n-1)/3)`, `q = floor((n+t)/2) + 1` (`pe_seams::agree::quorum`). Safety against one
  Byzantine member needs n >= 4 (model: bound-n3-byz); at n = 3 the rules tolerate crashes and author faults.
- **Views and proposers.** Views 1, 2, ... are led in a rotation seeded from the line id. A silent or faulty
  proposer is skipped by timeouts. **Proposer choice: leader reputation** (DiemBFT v4 with a window of one; coordinator, by measurement in models/cuts/tools/leader_sim.py): rotation, skipping the leader of the latest view
  that timed out. **It is derived from the chain the proposal itself certifies** (option (b), @design-reviewer, as DiemBFT v4 does): the blocks final within the parent's certified chain are its ancestors up to
  the highest one whose child in that chain came in the very next view, and a view between two consecutive final
  blocks' views had no certified block, so it timed out. Proposer and voters compute the leader from the same data,
  whatever each has committed itself, and a voter refuses a proposal from anyone else (model: committed_in and the
  voter's leader check). Keep-while-good was rejected: at n = 2 its votes cross the
  slow link twice per view (600 ms against 300 ms with a 150 ms member) and it never finalizes under a timeout below
  that; rotation alone collapses with a crashed member (2.1 cuts/s at n = 3). Model: leader_in, control ctl-skipall.
- **Cadence.** A proposer names any frame above its parent's (not a frame fixed by height), so one cut catches up
  after a stall and a link whose round trip exceeds the cadence doesn't fall behind without limit. Proposals come on a
  cadence that is a duration in the world's settings. A cut includes, for each author, its chain up to the head it
  names, all stamped below its frame; it extends its parent's heads. An entry a cut covers that is stamped below the
  parent's frame is late, and void.
- **The frame a voter accepts:** above its parent's frame, and at most the voter's own present frame plus a small
  tolerance (a world setting). Never a bound relative to the parent (@design-reviewer, coordinator): with one, a 10 s stall would need dozens of cuts to catch up. The upper bound stops a proposer naming
  a frame in the voters' future, which would void as late every honest entry stamped below it but not yet sent. The
  model checks that frames rise and that lateness is measured from them (ctl-noframe); it has no clock, so the
  future-frame bound is a harness test: a proposal whose frame is beyond the voters' present plus the tolerance gets no
  honest vote.
- **The frame a leader names: its present minus the cut lag** (coordinator, @fable-reviewer). A leader that names its own present voids as late every honest entry still in flight with a stamp
  just below it (measured: 19 honest entries void in 4 s at n = 4 with 5-30 ms links). The lag starts at 0.25 s and
  then adapts: the 95th percentile of how late other members' entries arrive at this member (present frame at
  arrival against the stamp), plus a margin, clamped between a floor and a cap (world settings, in seconds; defaults
  0.05 s, 1 s, margin 0.05 s). Members need not agree on it: the cut carries its frame. Past the cap, a slow member
  stamps its own entries ahead by its measured delay (the author's side, pe-app), so its input lags but is never
  voided. Harness property: no honest entry is voided while every link's delay is below the lag minus the margin.
  - *Residual:* a faulty leader can name its present (or a frame just behind it) and void entries in flight. That is
    visible as voids per leader.
  - *Residual:* a member with no hand stream (an agent) can stamp up to the lag in the past and win same-frame races.
  - *Residual:* lateness is measured over all authors, so a member that back-stamps its entries raises every
    leader's lag, up to the cap. It costs finality latency, never safety.
- **The last cut needs a child.** When cuts pause (idle, or the session ends), one empty cut follows, so the last
  content becomes final before it is saved (model v0.1: "every content cut ends final"; ctl-skipempty, a proposer
  with nothing new proposes nothing, fails it).
- **A proposal** carries its parent's quorum certificate, and after a timed-out view a timeout certificate. The
  entries it covers travel by name; a voter fetches what it lacks (section 4).
- **Voting.** A member votes at most once per view, never after timing out in it, and only for a proposal of its
  current view whose cut it has checked: every newly covered entry is held, linked in chain order back to the parent's
  head, signed by its author for this line, and stamped below the frame (model: validity: chains;
  AcceptProposalUnchecked, NoSignatureCheck). The vote is one hash over (cut, the voter's own root), carried in the
  voter's next ack, with the voter's chain heads beyond the cut (section 4) and its self-test verdict.
- **The lock.** A proposal that does not extend the previous view's certificate must carry a timeout certificate, and
  a member votes for it only if its parent is at least as high as the highest certificate in that timeout
  certificate (model: NoLockRule gives a split-brain without it).
- **Certificates.** q votes for the same (cut, root) are a certificate. Votes for one cut with different roots that
  can no longer reach q for any root raise `RootSplit` (model: VoteWithoutRoot).
- **A root split ends the view at once** (L4', coordinator). A member that sees `RootSplit` on its
  current view's block times out at once instead of waiting for the timer (a timeout is always safe). That timeout is
  no fault and doesn't grow the adaptive timeout, so each step of the repair ladder costs about a link delay.
- **Timeouts.** A member gives up on a view after a timeout and sends a timeout carrying its highest certificate; q
  timeouts are a timeout certificate and move everyone to the next view. The timeout adapts: derived from the
  measured round trip, and growing after consecutive timeouts. A timed-out view is never recorded as a fault. At
  n = 2 the slow member leads every other view and finality needs two consecutive good views, so a timeout below its
  round trip would stop finality for ever.
  - **The adaptive timeout is required at n = 2** (model v0.1, measured). At q = 2 one premature timeout spoils its
    view, and P of them alternating with good views hold finality off for 2P + 1 views: with P = 3 the model is live
    in 8 views and not in 7 (bound-n2-early), and with timeouts that may always fire early (a timeout that never
    grows past the round trip) it never finalizes (ctl-early). So pe-agree's timeout grows after each timeout until
    views succeed.
- **Finality: two-chain.** A cut is final when its child, proposed in the very next view, is certified. On good links
  that is about two cut intervals.
- **Verdicts at the cut** (S3 v1.1, coordinator). Only an included entry can be void. A final cut's
  newly included entries take effect in chain order, except: `BackStamped` (section 2), and `Late` (stamped below the
  previous final cut's frame). Every honest member names the same deciding cut for each verdict (harness P4). A fork
  of an included entry is the fork fault, not a second pending entry.

- **A second vote in a view is a self-test report** (coordinator; S3 v1.3 shape by
  @engine-architect). A member votes once per view. The driver's `vote` after a repair returns
  `AlreadyVotedThisView` when this member's latest vote went to that cut and the cut isn't final, even if that vote's
  view has ended (a split ends it at once, and a certificate moves it on). The repaired root counts from the next
  view's proposal of the cut, which the driver votes for normally; the lock and finality are unchanged.
- **Only signed verdicts count** (S3 v1.3, @design-reviewer's Medium on it; pe-agree crates/pe-agree/src/core.rs).
  - Whenever the driver hands `vote` a verdict that names a split, pe-agree sends it as a signed **self-test report**
    (line, the split cut, the verdict) on the Protocol lane, whatever becomes of the vote. A verdict in a vote is
    unsigned, so it is only a hint and is never shown.
  - A member relays each report the first time it records it. So two different verdicts from one member reach every
    honest member, even when the member sent each to a different peer.
  - A member's first verdict for a split is final. A second, different one is equivocation: a `Fault`, and that
    member shows `Failed` from then on. `Failed` therefore wins whatever order the verdicts arrive in, and every
    member shows the same verdicts. pe-agree refuses to send a different second verdict of its own.
  - A report never changes a vote: roots are counted from votes only. A report from a member that did not vote for
    the split cut is never shown, since `RootSplit` lists voters only.
  - Every newly recorded verdict re-emits `RootSplit { cut, roots }` for each split block of that cut, with each
    voter's verdict for the split (`NotRun` if none). The newest `RootSplit` for a cut supersedes the earlier ones.
  - A verdict names the split it was run for (`SelfTest::Passed { split }`), and a report whose verdict names
    another cut is refused, so a verdict from an earlier episode never counts for this one.
  - Reports are kept for split cuts from the committed height to 64 heights above it, with at most 16 split cuts per
    member, which bounds what a faulty member can make others store.
- **The pick waits for finality** (L4'). During a split nothing new becomes final: the split block B gets no
  certificate, and its parent is final only through a certified child in the next view, which is B. The newest final
  block is fixed by the certificates B's proposal carries, the same at every member that processed it. Members
  differ only in whether they hold that cut's entries yet (`EntrySource::final_cut` lags `CutFinal` while fetching),
  so pe-diverge picks its reference only once `final_cut()` names the cut of the newest `CutFinal` it has seen.
- **The order within a frame** (D37, coordinator, choice (a)): stamp, then the tie rank
  `H(line id, frame, author key)`, then author, chain and seq. *Residual:* keys are chosen before joining and the
  origin line's id is public, so a member can grind a key: about N frames of advantage against one chosen victim
  costs about 2^N keys, and the rank is predictable anyway. Not seeded from admission (choice (b) not built).
- **Settings** (with @engine-architect): pe-agree owns its settings record's format and version (the
  cadence, the lag's floor, cap and margin, the frame tolerance, the timeouts' floor and ceiling, the retention caps);
  the bytes live in the world, hashed into the root. A change is an agreed entry and takes effect from the first cut
  that includes it, so every member switches at the same cut.

## 4. Dissemination

- **Fast path.** Each new entry is relayed once to every member.
- **Fetch by name.** A member does not report a cut through `final_cut`, issue a verdict under it, or offer it through
  `wants_roots`, until it holds every entry the cut names, void ones included. It fetches what it lacks from any
  voter; at least one honest voter holds each (model: the per-cut void-set property; d1 skips the fetch).
- **Heads in votes.** Votes carry the voter's chain heads beyond the cut, so the next proposer learns what it lacks
  and fetches it before proposing; an entry an honest member holds is then included within a bounded number of cuts
  (model v0.2: bounded progress). The heads are hints (@design-reviewer, Medium 2):
  - taken only from a vote the core has verified;
  - fetched within the author's free pending slots and at most 256 at a time;
  - dropped after 5 s if no entry came;
  - never solicited, so the entries they name pass the author's rate budget like any other.
- **Votes and views** (@design-reviewer's High on 61e08c0d). A vote counts only for a held block and only in that
  block's own view; one naming another view is a fault. A vote that arrives before its block waits, at most one per
  voter. Votes, timeouts and proposals waiting for a parent are kept only up to 16 views above the member's current
  view, so one faulty member can't grow them without limit. Certificates carried in proposals and timeouts still move
  a member that is further behind.
- Never treat "the entries stamped in [floor, cut frame)" as "everything the cut includes" (coordinator).

## 5. Holds, the final log, events

- `hold_votes_from(frame)` and its release: no vote for a cut at or past a held frame until released (logic.md 6.2).
- `final_entries(from, to)`: the final log between two final cuts, in agreed order (stamp, then the seeded rank),
  for rung 1's replay, the fault bundle and the merge.
- Events are `pe_seams::AgreeEvent`; below-quorum events (`QuorumLost`, `Unconfirmed`, `LineDiverged`) are v1's.

## 6. Mutations (feature `mutants`, never in production state)

Named as the model's controls: `VoteWithoutChainValidation` (d1), `AcceptProposalUnchecked`, `QuorumOneTooSmall`,
`NoLockRule`, `NoSignatureCheck`, `NoTimeouts`, `VoteWithoutRoot`; plus `RateLimitRemoved` and `HorizonRuleOff`,
which the model does not cover. Each is inert unless the node's configuration names it.

## 7. Where pe-agree differs from the model

- Votes go to every member as signed messages on the Protocol lane, so each member forms certificates itself (n is
  small; this is n^2 votes per view). Riding them in acks is a later wire saving.
- **The leader from the proposal's own chain, with a bounded history.** As the model does (committed_in), a
  member derives the leader of a proposal's view from the chain its parent certifies, so members at different
  committed heights agree on it. pe-agree keeps every stored block's reputation state, and the headers of the last
  256 committed blocks after their bodies are pruned. A voter that has committed further than a proposal certifies
  can still derive its leader. Past that history it refuses, which costs that view a timeout, never safety:
  certificates, not leaders, decide finality.
- **What the leader check doesn't stop** (@design-reviewer): a voter takes the first proposal of the
  view, by proposer key, whose own parent names its proposer. Under the timeout-certificate path a Byzantine member
  can propose on a different eligible parent whose chain names it as leader. Voters that hold it vote for it, the
  others for the honest proposal, and the view times out [I]. That costs at most one view, like a Byzantine leader's
  silence, and never safety.
- **Blocks travel by name once certified.** A proposal whose parent this member lacks waits (one per view) while
  the parent is fetched by name from every member; a final block's missing ancestors are fetched the same way
  before any of them is reported final. The model delivers every proposal to every member.
- **The adaptive timeout** (crates/pe-agree/src/core.rs): the cadence, plus the larger of a floor and three times
  the measured time from entering a view to its certificate (an average weighted 1/8 to the newest), growing by
  half after each timeout certificate until a view succeeds, up to a ceiling. All three limits are durations.
- **Empty cuts don't pause yet.** v0 proposes on every cadence, empty or not; the pause after the last content cut
  becomes final (section 3, "The last cut needs a child") comes with an idle mode in which nobody times out.
- Blocks are named by hash; the model names them by their path of views and cuts.
- Roots are real state hashes; the model's root is one bit (right, or wrong on one fault-injected replica).
- The model has one or two entries per author and two or three content cuts; its bounds and what is exhaustive are in
  `models/cuts/README.md`.

## 8. Not in v0

Join, leave, return and a restarted chain (model v1, the main open risk: pe-agree does not depend on membership
change before that model is checked); one branch merging with the main line (model v2); quorum loss,
unconfirmed play, the one-minute bound and split groups (model v3).
