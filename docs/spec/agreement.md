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
  - **It follows the world's frame rate,** so an honest player's per-frame inputs pass at any rate. World settings,
    the same on every member: `entries_per_frame` (3: two hands plus one for poses and other per-frame inputs),
    `extra_per_s` (30, for edits and governance) and `budget_slack` (1.25). The rate is `entries_per_frame × rate ×
    budget_slack + extra_per_s` entries a second, the burst half a second of it and the pending cap four seconds,
    each at least today's floors (150/s, 60, 600). That is 255/s, 127 and 1,020 at 60 Hz, and 480/s, 240 and 1,920
    at 120 Hz. pe-app sends two hand entries per frame and paces its edit queue to at most one entry per frame on
    the same chain (@engine-architect). The world's rate is frozen for a session, so the budget never
    changes under a member.
  - **Today these are node defaults, not yet world settings.** They are `NodeConfig` fields, like
    `frame_tolerance_s` and the rest; nothing reads them from the world's config record (world-model 6.6) yet. Until
    pe-app builds its `Node` from the world config, every peer runs the same compiled defaults, and the connection
    guard refuses a different build. The wiring must land before members can run different builds or worlds can
    change the settings (@design-reviewer's Low 2; @engine-architect).
  - **The budget delays a valid entry but never loses it** (coordinator, on @fable-reviewer's; @design-reviewer's Low 1). Before, a refused entry left a hole in its author's chain
    that nothing fetched back: at 120 Hz a 3 s stall's flush (720 entries into a burst of 240) cut the member off for
    good. Now:
    - **Defer, don't refuse.** An entry that passes every check but is over its author's budget waits in the author's
      queue, in chain order, capped at the pending cap in entries and `defer_bytes` in bytes. It is admitted as the
      budget refills, and relayed only then. While an author has entries waiting, a new one waits behind them. A full
      queue makes room by dropping its latest entry, with no fault (the refetch brings it back); only an entry later
      than all of them is refused, `RateExceeded`, the one fault the budget reports. Waiting entries are never named
      in a vote's heads, since only held entries are.
    - **Checks are capped too:** per sender and author, twice the rate, with a burst of twice (pending cap + burst).
      Past it an entry is dropped unchecked, with no fault, so a flood can't burn a peer's CPU on signatures. An entry
      dropped unchecked raises nothing, so its gap shows only once a later checked entry or a vote's heads name a seq
      past it; that works while the author votes (@engine-architect's nit).
    - **Range refetch.** For each chain with a hole (a seq beyond its linked head seen, in a checked entry or a vote's
      heads, and the next one neither held nor waiting) that has lasted 150 ms, the member asks for just the gap,
      `FetchRange { author, chain, from_seq, count }`, of the author first and of everyone on a retry, with the retry
      doubling. The wait keeps an entry merely reordered in flight from being fetched twice; a gap that moves (the
      chain still growing) restarts it. The answer is charged to the budget like any entry. Refusals, unchecked drops
      and network loss heal the same way.
      - **Serving is budgeted** per requester, at one author's rate and burst across all the chains it asks for:
        an answer is cut short to what the requester's bucket holds, and the requester asks again. A member asking
        over and over, for any chains, gets back about one author's stream in all (measured [V]: 1,199 entries in
        2 s across three authors' chains against a bound of 1,200, where 512,000 were asked for; @design-reviewer's
        Medium 2 and later Low 1). An honest requester recovering from its own stall
        asks each author for its own chain first, and each server keeps a separate bucket for it. Answers go on the
        Action lane: they are signed entries, as urgent for their author as live ones; if contention shows in a
        measurement, the fix is pe-net's order within the lane, fresh before refetched (@engine-architect).
      - **A vote's heads are a claim, not evidence.** Only a checked entry, which only its author can sign, raises
        what a member knows of a chain for good. A seq named only by a vote's heads is asked for, and forgotten once
        its asks run out unanswered (five, about 15 s); that voter's claims on that chain are then ignored until the
        chain's checked seq reaches the forgotten claim. An honest voter answers its own claim, so only an
        unreachable or lying voter is ignored, and the entries still come by relay and cuts. At most four chains per
        author are tracked from votes alone, given-up ones included, so fabricated chain ids can't grow the maps
        (@design-reviewer's Low 1). Measured [V]: a claim of seq 2^60 on an honest chain is asked for 13 times
        (one ask, then four retries to three peers) and never again.
    - **Filling a hole is exempt from the pending cap**, though not from the tokens: an entry that extends the linked
      head while entries beyond it are held. Otherwise a cap full of entries behind a hole could never drain. A hole
      fills once, so an author's held entries stay within about twice the cap.
    - **Pacing at the author.** `Node::backlog()` is the author's unacknowledged backlog: its own entries beyond the
      head a quorum holds, counting itself and, for each other member, the head of the author's chain that member's
      latest verified vote names (at least the final cut's). Entries younger than the vote cadence plus the timeout
      floor (0.4 s at the defaults) aren't counted, since no vote can name them yet, and nothing is counted until a
      quorum's worth of peers have named the chain once (the session's first cut lag). While the backlog is over the
      burst (`Node::backlogged()`), pe-app makes no new hand entries; they would be refused or void as late. Edits and
      other inputs are never held back this way. pe-agree itself never re-sends an entry; a transport that buffers
      across an outage flushes at once, and the queue and the refetch absorb that.
      - **Not the entries beyond the final cut** (`Node::unfinal_own()`, kept as a diagnostic): that grows with the
        cut lag, about 400 entries at three a frame and 120 Hz with the lag at its 1 s cap, over the burst of 244, so
        it paced healthy members (@design-reviewer's Medium 1). Measured [V, 2 seeds, 8 s]: the old rule would pace
        880 of 960 frames per member, the backlog none. Votes name heads beyond the cut they vote on, so the backlog
        doesn't grow with the lag. It does grow during a view change with no votes: past the grace plus a burst's
        worth of entries (about 1.4 s at two hands and 120 Hz), hand entries pause until votes resume.
    - Measured [V, pe-agree sim, 120 Hz, two hands a frame, seeds 31-33]: with pacing, stalls of 1, 3 and 10 s give no
      refusal and no fault, and the member's entries are final again from the stall's end. In every stall run, paced
      or not, every peer's copy of the member's chain links through every entry it made up to 2 s before the end
      (asserted: nothing is lost; @design-reviewer's Low 3). Unpaced (seed 31): a 3 s
      stall gives no refusal and no fault; a 10 s stall gives 31-34 refusals per peer, and the entries are final
      again 7 s after it. A flooder at twice the budget gets about 1,500 of its 9,600 entries final in 10 s.
- **Retention.** Final entries are kept from the later of the driver's Cv frame and a cap: the one-minute divergence
  bound plus a margin, as a duration converted with the world's SimRate, and limited in bytes. `kept_from()` says
  where the log is complete; below it `entries_at` answers "not kept", never an empty set (coordinator).

## 3. Cuts (pe-agree)

- **Quorum.** `t = floor((n-1)/3)`, `q = floor((n+t)/2) + 1` (`pe_seams::agree::quorum`). Safety against one
  Byzantine member needs n >= 4 (model: bound-n3-byz); at n = 3 the rules tolerate crashes and author faults.
  - At n <= 3, t = 0, so a single member's timeout for a later view moves the others on (the catch-up rule,
    section 4). That's right for M1: n = 3 tolerates crashes and author faults only, and a crashed member sends
    nothing. A stricter threshold would wedge n = 3 with one member crashed. Under a faulty member it could livelock:
    the member could push the others ahead up to 16 views at a time, each forcing an immediate timeout
    (@design-reviewer).
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
- **The cut grid** (@engine-architect). Every cut's frame is a multiple of the world's cut interval
  (`cut_interval_s`, 0.1 s) in frames at the world's rate (`NodeConfig::cut_every`: 6 frames at 60 Hz, 9 at 90, 12 at
  120; at least 1), counted from frame 0. The leader names the latest grid frame at or below its present less the lag,
  and proposes only when that is above the parent's; a voter refuses a cut off the grid (`Reject::OffGrid`), and its
  view times out like any refused proposal. The replica takes live roots and keeps keyframes on the same grid, so
  every vote reuses the live root instead of re-stepping from a keyframe: pe-app gives the replica
  `NodeConfig::cut_every` as its `cut_every`, so one function decides both grids (@engine-architect). Rounding down adds up to one interval less
  a frame to the lag. A cut may still skip grid frames (one cut catches up after a stall); the grid fixes where cuts
  can fall, not that each grid frame gets one. The cadence (`Timing::cadence_ns`) stays the floor between a block
  and its child's proposal; with the interval at least the cadence, the grid is what spaces cuts.
  Measured [V, pe-agree sim, 4 members, two hands a frame, 5 s]: every final cut on the grid at 60, 90 and 120 Hz
  (40-41 final cuts each); a member running a 0.15 s interval against the others' 0.1 s has its off-grid proposal
  refused, and the line stays live (41 final cuts in 6 s, all on the honest grid).
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
  cadence, the cut interval, the lag's floor, cap and margin, the frame tolerance, the timeouts' floor and ceiling,
  the rate budget's fields, the retention caps); `NodeConfig::consensus_hash` covers the ones every member must
  share (v3: the cadence and timeouts, the frame tolerance, the rate budget's fields, `defer_bytes` and the cut
  interval);
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
- **Catching up after an outage** (M1 requirement, coordinator; @design-reviewer).
  pe-net retransmits Protocol messages through an outage shorter than its idle limit (5 s). Past that limit, or on a
  new connection, whatever was queued is lost (transport.md 3.2; @shader-lang). So:
  - A member re-sends its timeouts for its current and previous view every timeout period. The previous view's
    timeout matters: its certificate moved this member on, and the others may lack it.
  - t + 1 timeouts for a later view show an honest member is there. A member joins that view and times out in it,
    which only gives up voting in it.
  - A fetched block whose parent is missing too is kept, so a gap fills in one round trip per block.
  - Members serve the proposals of their last 256 committed blocks to fetches. A member further behind than that
    needs an import (join, section 8).
  - Tests (pe-agree): a 2 s partition at n = 4, once with one member crashed and once all honest, at core and node
    level. Messages sent during the partition are never delivered. The line resumes, and the final logs match.
- **Hint shares.** Each voter gets its own share of the hint slots (256 / n), so one voter flooding heads can't
  crowd out another's.
- Never treat "the entries stamped in [floor, cut frame)" as "everything the cut includes" (coordinator).

## 5. Holds, the final log, events

- `hold_votes_from(frame)` and its release: no vote for a cut at or past a held frame until released (logic.md 6.2).
- `final_entries(from, to)`: the final log between two final cuts, in agreed order (stamp, then the seeded rank),
  for rung 1's replay, the fault bundle and the merge.
- Events are `pe_seams::AgreeEvent`; below-quorum events (`QuorumLost`, `Unconfirmed`, `LineDiverged`) are v1's.
- **A cut's root is over its own final log** (pe-seams v1.6 `EntrySource::entries_for_cut`; @harness-eng's
  pipelined-void finding, coordinator). For a cut handed out for roots, or the final cut,
  the entries returned at each frame below the cut's frame are exactly what the final log yields there if the cut
  becomes final: the same entries, verdicts (late, back-stamped, forks) and order. The answer is computed from the
  cut's own ancestry, never from local in-flight state: each block from just above the final cut up to the cut's is
  judged against its parent, as at finalization. Below the core's committed block, which the core prunes, the walk
  continues through the final blocks still waiting for their entries. So two candidates at one height each get their
  own answer, and an abandoned cut changes nothing.
  - Why: with pipelining, cut N+1 goes out for roots while cut N is in flight. Filtering `entries_at` by the cut
    applied entries that N+1 voids as late against N's frame (S3's pending rule only starts at the *final* frame),
    and it ordered the cut's newly effective entries after the final ones rather than in the final log's order.
    Either way, the quorum would certify a root the final log doesn't yield.
  - The d1 rule counts from the previous final cut: a candidate goes out for roots only when this member holds
    every entry its whole uncommitted ancestry includes, not only the newest segment; the rest is fetched first.
  - A cut can be final on the line before this member holds its entries (`AgreeEvent::CutFinal` comes first);
    `EntrySource::final_cut` reports it only once they are held. A driver that reads `final_entries` must do it
    from `final_cut`, before `Replica::finalize` lets the trim pass.
  - Tests: the root = final log property over every rooted cut that becomes final, in sims with late authors,
    partitions, slow and jittery links; pipelines three deep, abandoned cuts (one whose replacement includes an
    entry below the abandoned cut's frame) and blocks that reach a member before their parent each occur in them.
    Two reds: the old filter, and the old filter in the final order without verdicts; each fails.
  - **A cut doesn't name its block.** Two blocks can carry a byte-identical cut under different parents (a replacement
    parent after a view change, in a quiet world), and their answers can differ. The lookup takes the votable
    proposal first, the block a vote goes to; for any other cut held under two parents it answers `None` rather than
    guess, and counts it (`Core::shared_cuts`, asserted 0 in pe-agree's sims; the harness should assert it too). If it
    ever fires, or after M1, `Cut` should name its parent or block (a seam change for @engine-architect;
    @design-reviewer, @harness-eng).
  - The log and its indexes (`ChainLog`, `by_ref`, `arrived`) grow for the whole session; pruning them is open work.
    The root cache is versioned by an insert counter, so a prune can't bring back a stale answer. The range index
    (`by_seq`) is pruned with the kept frames: each chain's rows for entries stamped below `kept_from` go, since no
    honest member asks that far back by range (@design-reviewer's Low 2, @engine-architect's Low 1).
  - Open (@design-reviewer, not blocking): a Byzantine voter can fill an author's four vote-only
    claim slots with fabricated chain ids until checked entries reach them; a genuinely new chain of that author is
    then not claimable from votes until one of its entries arrives directly, which is fine while entries are
    broadcast.

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

- **Catching up from far behind needs an import.** Peers serve only their last 256 committed blocks. They serve
  entries from a ChainLog that is never pruned today; once pruning comes, a member behind by more than the kept
  window needs the import path for the entries too (@design-reviewer).
