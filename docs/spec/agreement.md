# Agreement: pe-log and pe-agree (WS-agree)

Draft v0, 2026-11-11, @world-chunks. Implements seam S3 (`pe_seams::agree`) and the chains of S1
(`pe_seams::entry`). The cut rules follow the checked model in `models/cuts` (model v0, branch model/cuts-v0, in
review); a rule here that the model checks names its property or control. Every time limit is a duration in seconds,
never a count of frames (user ruling md4b42d16d9df).

Decided (coordinator mc42c566ac995, @fable-reviewer concurring): the two-chain rule with a lock (chained HotStuff /
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
  entry never spends an honest author's tokens (@design-reviewer m03081706a05a, Medium 1). A refused entry is never
  stored and raises `Refused`. One that fails shape, line or signature is a fault of the sending peer, not of the
  author it claims. A second, different entry at a held (chain, seq) is stored as a fork proof and raises
  `Equivocation`; the author's chain is frozen at the fork. The proof is relayed once when it arrives from its author
  (the fast path); a fork fetched by name is not relayed again.
- **Validation in chain order.** An entry is linked once every earlier entry of its chain is held and its `prev`
  names the one before. Entries past a hole are held but not linked: they count for no vote and no verdict
  (model: d1 is the mutation that judges them alone).
- **The stamp rule.** An entry is back-stamped if its stamp is strictly below an earlier stamp in its author's
  chain; an equal stamp is valid and ordered by seq (@design-reviewer md88ada36bfaf). The node keeps each chain's
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
    the same chain (@engine-architect m216ee6ef5fc2). The world's rate is frozen for a session, so the budget never
    changes under a member.
  - **Today these are node defaults, not yet world settings.** They are `NodeConfig` fields, like
    `frame_tolerance_s` and the rest; nothing reads them from the world's config record (world-model 6.6) yet. Until
    pe-app builds its `Node` from the world config, every peer runs the same compiled defaults, and the connection
    guard refuses a different build. The wiring must land before members can run different builds or worlds can
    change the settings (@design-reviewer's Low 2, me5616ae97a1b; @engine-architect m8fef170a9680).
  - **The budget delays a valid entry but never loses it** (coordinator m120f73db271d, on @fable-reviewer's
    m27b1a540090a; @design-reviewer's Low 1, me5616ae97a1b). Before, a refused entry left a hole in its author's chain
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
      past it; that works while the author votes (@engine-architect's nit, mf606459fd5df).
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
        Medium 2 and later Low 1, m15c6eb656e23, m0c154bca4188). An honest requester recovering from its own stall
        asks each author for its own chain first, and each server keeps a separate bucket for it. Answers go on the
        Action lane: they are signed entries, as urgent for their author as live ones; if contention shows in a
        measurement, the fix is pe-net's order within the lane, fresh before refetched (@engine-architect
        m7cc58b89e7f5).
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
  where the log is complete; below it `entries_at` answers "not kept", never an empty set (coordinator m01e5762d1fcc).

## 3. Cuts (pe-agree)

- **Quorum.** `t = floor((n-1)/3)`, `q = floor((n+t)/2) + 1` (`pe_seams::agree::quorum`). Safety against one
  Byzantine member needs n >= 4 (model: bound-n3-byz); at n = 3 the rules tolerate crashes and author faults.
  - At n <= 3, t = 0, so a single member's timeout for a later view moves the others on (the catch-up rule,
    section 4). That's right for M1: n = 3 tolerates crashes and author faults only, and a crashed member sends
    nothing. A stricter threshold would wedge n = 3 with one member crashed. Under a faulty member it could livelock:
    the member could push the others ahead up to 16 views at a time, each forcing an immediate timeout
    (@design-reviewer m3a0b0db4241f).
- **Views and proposers.** Views 1, 2, ... are led in a rotation seeded from the line id. A silent or faulty
  proposer is skipped by timeouts. **Proposer choice: leader reputation** (DiemBFT v4 with a window of one; coordinator
  m15ca8d144c33, by measurement in models/cuts/tools/leader_sim.py): rotation, skipping the leader of the latest view
  that timed out. **It is derived from the chain the proposal itself certifies** (option (b), @design-reviewer
  mbd55865e1a49, as DiemBFT v4 does): the blocks final within the parent's certified chain are its ancestors up to
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
- **The cut grid** (@engine-architect m1a122523d255). Every cut's frame is a multiple of the world's cut interval
  (`cut_interval_s`, 0.1 s) in frames at the world's rate (`NodeConfig::cut_every`: 6 frames at 60 Hz, 9 at 90, 12 at
  120; at least 1), counted from frame 0. The leader names the latest grid frame at or below its present less the lag,
  and proposes only when that is above the parent's; a voter refuses a cut off the grid (`Reject::OffGrid`), and its
  view times out like any refused proposal. The replica takes live roots and keeps keyframes on the same grid, so
  every vote reuses the live root instead of re-stepping from a keyframe: pe-app gives the replica
  `NodeConfig::cut_every` as its `cut_every`, so one function decides both grids (@engine-architect mfabd101b839c). Rounding down adds up to one interval less
  a frame to the lag. A cut may still skip grid frames (one cut catches up after a stall); the grid fixes where cuts
  can fall, not that each grid frame gets one. The cadence (`Timing::cadence_ns`) stays the floor between a block
  and its child's proposal; with the interval at least the cadence, the grid is what spaces cuts.
  Measured [V, pe-agree sim, 4 members, two hands a frame, 5 s]: every final cut on the grid at 60, 90 and 120 Hz
  (40-41 final cuts each); a member running a 0.15 s interval against the others' 0.1 s has its off-grid proposal
  refused, and the line stays live (41 final cuts in 6 s, all on the honest grid).
- **The frame a voter accepts:** above its parent's frame, and at most the voter's own present frame plus a small
  tolerance (a world setting). Never a bound relative to the parent (@design-reviewer m72d5174e95f9, coordinator
  m6a9078c600ee): with one, a 10 s stall would need dozens of cuts to catch up. The upper bound stops a proposer naming
  a frame in the voters' future, which would void as late every honest entry stamped below it but not yet sent. The
  model checks that frames rise and that lateness is measured from them (ctl-noframe); it has no clock, so the
  future-frame bound is a harness test: a proposal whose frame is beyond the voters' present plus the tolerance gets no
  honest vote.
- **The frame a leader names: its present minus the cut lag** (coordinator m0901fd194db2, @fable-reviewer
  m1d9255a4fde2). A leader that names its own present voids as late every honest entry still in flight with a stamp
  just below it (measured: 19 honest entries void in 4 s at n = 4 with 5-30 ms links). The lag starts at 0.25 s and
  then adapts: the 95th percentile of how late other members' entries arrive at this member (present frame at
  arrival against the stamp), plus a margin, clamped between a floor and a cap (world settings, in seconds; defaults
  0.05 s, 1 s, margin 0.05 s). Members need not agree on it: the cut carries its frame. Past the target, a slow
  member stamps its own entries ahead (the author's side, pe-app; app.md). The other members report how late its
  entries reach them (`own_lateness`, below), and it stamps ahead by the excess, `lead = delay - target + margin`.
  The target is a setting at most the cap, 0.1 s by default: receivers can't afford to re-step a second of a slow
  member's input every tick (measured by @engine-architect: under a 1 s target a receiver's view was published on
  583 of 1,800 ticks), so the slow member pays its excess as its own input lag (@coordinator-opus me3c27b7c5e18,
  after the user's thin-link rulings). Its input lags by the lead but is never voided, up to the lead's clamp
  (app.md). Past the clamp its hand entries void as late and its edits go to pe-api's re-send backstop. Harness
  property: no honest entry is voided while every link's delay is below the lag minus the margin.
  - **The measured delay comes from the receivers** (@engine-architect's plan mbfcd75bb89eb, option (a) of
    @item:primordial-engine/pe-app-stamps-a-slow-member-s-entries-ahead-by-i). The author can't see a one-way delay:
    its round trips halve an asymmetric one, and the clock offset folds the rest in. Each member keeps the lag's
    samples per chain as well (the last 128 of each other member's chain, with each entry's seq), signed: negative
    when an entry arrives before its stamp, so a lead that is too large shows and can shed (@engine-architect's Medium
    on 0238ca46; the lag counts an early entry as on time). Its votes and timeouts carry, not signed, one report per
    chain with at least 16 samples: (author, chain, the 95th percentile in frames, the seq of the entry at that
    percentile). That seq, not the window's newest, is what makes the correction exact (@design-reviewer's M1 on
    0238ca46): just after a lead rises, the slowest samples are the oldest, stamped with the old lead, and adding the
    new lead to them would ratchet it up. The author keeps each other member's report from its latest vote or timeout
    (by epoch and view, since views restart in each epoch) on its current chain, and S3's `own_lateness()` (S3 v1.8)
    returns them raw. The driver knows the lead it stamped each seq with and corrects each report by it; at n = 2 it
    takes the one report, and at n >= 4 a quorum-rank figure, not the largest, so one faulty member can't push the
    lead up alone. A report is a hint from an authenticated link: a wrong one moves only the author's own lead, which
    the driver clamps (a world setting of a few seconds). The samples leave out what the lag leaves out: fetched and
    hinted arrivals, a backlog a vote or timeout claimed first, and an outage's backlog: after silence from a peer
    longer than 1 s, its entries stamped below this member's present frame at the first arrival after it (the outage
    mark) waited out the outage, however they come, since a transport that queues through a dark path flushes them
    before any fresh claim (@design-reviewer m7a78b8eaa1e1: unmarked, the first report after a 10 s blackout read 22 s
    and pe-app's lead jumped to its 3 s clamp; marked, the reports stay at 16-25 ms and the cut lag at 66-75 ms after
    a held-and-flushed 10 s blackout in pe-agree's sim [V]). That biases the report low, since the entries a vote
    overtakes are the slowest; the exclusion stays, because after a real outage they would inflate the lead (measured
    negligible at n = 2, below). The 16-sample minimum and the 128-sample window assume a member that streams hands,
    as every M1 member does: a chain of edits alone would wait 16 edits for its first report, and its window would
    span minutes. Measured [V, pe-agree sim, release, n = 2, B -> A slow, A -> B fast, 2 seeds each]: A reports B's
    95th percentile at 1,216-1,225 ms on a 1.2 s fixed link and 1,483-1,500 ms on a 0.9-1.5 s one, over a seq 150-300
    behind B's head; B reports A's at 2-3 frames. On the jittered link 3-15 % of B's arrivals at A are left out as a
    claimed backlog (a vote overtook them), and 0.1-0.2 % in all on the fixed one; the percentile still matches the
    link. A lead stepped up by 0.5 s reads, corrected, at most 1,216-1,225 ms on the fixed link and 1,500 ms on the
    jittered one (with the window's newest seq it read 1,716 ms). With a 1.2 s lead on the 1.2 s link, a drop to 0.3 s
    reads 316-325 ms from 1.1 s after the drop, once the entries already in flight on the slow path have arrived.
  - *Residual:* a faulty leader can name its present (or a frame just behind it) and void entries in flight. That is
    visible as voids per leader.
  - *Residual:* a member with no hand stream (an agent) can stamp up to the lag in the past and win same-frame races.
  - *Residual (the outage mark's threshold, @engine-architect m3c1131ffd4c4) [I]:* a dropout shorter than 1 s sets no
    mark, so its flushed backlog is sampled at its age: a 0.5 s dropout puts about 60 hand entries in the 128-sample
    window and the 95th percentile reads about 0.45 s, which the driver's lead follows until it drains. 1 s stays
    above a reliable lane's retransmit gap, so a lost-and-resent entry isn't read as an outage; the threshold is to be
    set from pe-app measurements of 0.3 s and 0.7 s blackouts. And a step up in delay past 1 s (a link handover, say
    0.1 s to 1.5 s) is read as an outage: the mark falls at the first arrival at the new delay, so about the next
    delay's worth of arrivals goes unsampled by both the reports and the cut lag, and neither reacts until about
    twice the new delay after the change. The author's entries void as late meanwhile.
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
- **A root split ends the view at once** (L4', coordinator m2c0ac98798a0). A member that sees `RootSplit` on its
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
    grows past the round trip) it never finalizes (ctl-early). So pe-agree's timeout grows after each timeout, and
    shrinks only as cuts become final, never merely because a view succeeded.
  - **An asymmetric link** (@item:primordial-engine/finality-stall-past-the-lag-cap-with-no-lead-n-2, rulings
    md72c13f27272, mdf35a62dc095, m8722b9f1f17d). With B -> A slow and A -> B fast, B gets A's proposals and votes at
    once, so it forms each certificate first and enters the next view at once, while A, who needs B's vote over the
    slow link, proposes a slow trip later. B's own views look short, so a timeout derived only from views that
    succeeded expires before A can propose; when a certificate also reset the growth, views alternated certified and
    timed out and nothing became final (measured [V], pe-agree sim, n = 2, 60 Hz, 1.2 s one way: 4 of 12 seeds
    stood still 23.9 s). Two rules fix it. The growth halves at each commit instead of resetting at each certificate
    (@fable-reviewer m208fc6590a7d: halving, so a steady slow link doesn't drop back to a too-short timeout after each
    final). And the timeout covers twice the measured wait for a view's proposal (below). With both, the same 12
    seeds stand still at most 8.4 s, mean lag at most 6.14 s.
  - **The known limit past stamp-ahead's clamp** (ruling m16df0b1e1d39). Without a lead, a member whose entries take
    longer than the lag cap abstains in every view whose cut omits them (section 3's outage rule), twice per final by
    its progress bound, and each abstention costs a slow round trip. Measured [V], same sim, with the fix, 2 seeds
    per cell, both directions, 60 and 120 Hz: a 0.3-0.6 s one-way delay stands still at most 0.3-0.7 s; 2 s at most
    18.0 s (mean lag 6.2-11.5 s); 3 s at most 22.1 s (mean lag 10.2-15.1 s). Main before the fix stood still 9-25 s
    at 2 s. Stamp-ahead keeps links up to its clamp (about 4 s) out of this regime.
- **Finality: two-chain.** A cut is final when its child, proposed in the very next view, is certified. On good links
  that is about two cut intervals.
- **Verdicts at the cut** (S3 v1.1, coordinator m33c08e879042). Only an included entry can be void. A final cut's
  newly included entries take effect in chain order, except: `BackStamped` (section 2), and `Late` (stamped below the
  previous final cut's frame). Every honest member names the same deciding cut for each verdict (harness P4). A fork
  of an included entry is the fork fault, not a second pending entry.
- **An outage's entries are delayed, never void as late** (coordinator md904d9b1a9c7, from @design-reviewer
  m59b87af62f12 and @fable-reviewer mc3d4e34cbc76; @shader-lang's blackout test m118d7ea7bf46). After an outage the
  leader names its present less the lag, over a parent from before the outage. It holds little of the others'
  backlog yet, and a vote checks only what a cut includes, so without these rules the backlog stamped below that
  frame is late at the next cut: in pe-agree's sim, a 10 s blackout at n = 2 voided 102-118 edits and 61-314 hand
  entries per run (crates/pe-agree/src/node_tests.rs, `no_entry_made_during_a_ten_second_outage_is_voided_as_late`).
  - **The voter abstains.** A member doesn't vote for a cut that omits one of its own entries stamped at or above
    the parent's frame and below the cut's (one below the parent's frame is late on its own). It times out in that
    view at once, without growing the timeout (as for a root split); the timeout carries its chain heads (section
    4), and it pushes the omitted entries, at most 256. Two bounds keep it from stalling the line:
    - *by progress* (coordinator m60983b248518, @fable-reviewer's concern): at most 2 abstentions between two final
      cuts. With only a per-entry bound, a sustained one-way delay above the lag (B to A, 0.9-1.5 s at n = 2) makes
      every cut A leads omit a fresh B entry, B abstains in each, and nothing became final in 30 s (measured; the
      model's ctl-early profile). With it, and B also making an edit every 10 frames
      (`a_slow_link_above_the_lag_cap_still_finalizes`): on links above the lag cap (1.2 s fixed, 0.9-1.5 s) the
      final frame stands still at most 3.8-6.1 s, and 24-113 of B's entries per 30 s are void as late, 2-5 of its
      397 edits among them; on a link below the cap (0.5-0.9 s) it stands still at most 0.9 s and nothing is void.
      Entries from an author whose link is slower than the lag cap are the slow member's residual above: pe-app's
      stamping ahead (not built yet) keeps them, and until then an edit among them is re-sent by rule 3 below;
    - *per entry* (@design-reviewer's L1): at most 2 views for one own entry, monotone: once an entry's are used up,
      it never abstains for that entry or any below it, however leaders alternate their omissions.
  - **A view that can't be certified ends at once.** A member that holds more than n - q timeouts for its current
    view times out in it too, without growing the timeout: a member never votes after timing out, so no
    certificate is possible. At n = 2 that is the other member's one timeout, so an abstention costs a round trip,
    not the leader's timer.
  - **The leader caps its cut.** A cut's frame is at most the lowest last-held stamp among other members' chains
    known beyond what the leader has linked: claimed by a vote's or timeout's heads, or seen checked beyond a hole.
    The capped frame is rounded down to the grid, and a frame not above the parent's proposes nothing yet; if that
    view then times out, its timeout doesn't grow the leader's next one ("no cut possible" isn't a failed leader).
    The view isn't given up at once, since the backlog may arrive within it (measured: giving it up at once tripled
    the longest stand-still on a 1.2 s one-way link); and if the leader proposes in it after all, its timeout grows
    as usual (@design-reviewer's L1 on b4b8bedb). The leader waits for each chain once: for its backlog when the
    wait begins (at most the author's pending cap) at the author's rate, plus 1 s, and not again until the chain reaches what was claimed, so a member that claims
    entries and never sends them holds cuts back once (measured: 5.5 s at 120 Hz). "Once" is per chain and claimant
    (@design-reviewer's L3): when a claim's asks run out unanswered it is dropped with its wait, and another member's
    claim on the chain starts a fresh one; a claimant whose claim went unanswered is ignored on that chain until the
    chain reaches it, so a chain is waited for at most f + 1 times. Only members' claims count (a
    removed member's don't, once membership changes, section 8). Entries on a chain claimed beyond them are left
    out of the adaptive lag, as fetched ones are.
  - **Which side does the work** (measured, sim): at n = 2 (seeds 61-64) the cap alone voids nothing and the voter
    side alone leaves 0-2 edits void per run; at n = 4 (seeds 81-83), where three votes make a quorum without the
    owner, the voter side alone leaves 1-8 edits void when all four come back from a 10 s outage, and the cap alone
    voids nothing. Both together void nothing in either. The mutants `NoAbstention` and
    `NoBacklogCap` turn each side off. Since the outage mark keeps the cut lag low after the return, the voter side
    alone voids at n = 4 on fewer seeds: 4-8 entries on seeds 86, 94 and 99 of 81-100, and none on the others [V,
    main dd765d0a]; the control runs on those three. The sim is deterministic per seed since pe-agree's maps became BTreeMaps (@design-reviewer
    mdb7e1d5116ad); sim figures measured before that carried run-to-run noise from HashMap order, so their ranges
    hold but don't reproduce exactly by seed.
  - *Residual:* an edit can still be void as late, for example when a member at n >= 4 is cut off while the others
    finalize past its stamps. Edits get a re-send backstop in pe-api (rule 3 of the ruling, placed by @api-eng and
    @engine-architect): the voided edit and the same author's later edits refused because of it are re-sent in
    order, with preconditions checked, backing off up to the one-minute bound, with one notice of the outcome. Hand
    entries are never re-sent.

- **A second vote in a view is a self-test report** (coordinator m0901fd194db2, m2c0ac98798a0; S3 v1.3 shape by
  @engine-architect m327eaf29e9b2). A member votes once per view. The driver's `vote` after a repair returns
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
- **The order within a frame** (D37, coordinator m0901fd194db2, choice (a)): stamp, then the tie rank
  `H(line id, frame, author key)`, then author, chain and seq. *Residual:* keys are chosen before joining and the
  origin line's id is public, so a member can grind a key: about N frames of advantage against one chosen victim
  costs about 2^N keys, and the rank is predictable anyway. Not seeded from admission (choice (b) not built).
- **Settings** (with @engine-architect m97a909a6049e): pe-agree owns its settings record's format and version (the
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
  (model v0.2: bounded progress). The heads are hints (@design-reviewer m03081706a05a, Medium 2):
  - taken only from a vote the core has verified;
  - fetched within the author's free pending slots and at most 256 at a time;
  - dropped after 5 s if no entry came;
  - never solicited, so the entries they name pass the author's rate budget like any other.

  Timeouts carry the same heads, beyond the member's final cut, as unsigned hints taken the same way (the outage
  rule, section 3): an abstaining member sends no vote, so its timeout is how the next leader learns of its backlog.
- **Votes and views** (@design-reviewer's High on 61e08c0d). A vote counts only for a held block and only in that
  block's own view; one naming another view is a fault. A vote that arrives before its block waits, at most one per
  voter. Votes, timeouts and proposals waiting for a parent are kept only up to 16 views above the member's current
  view, so one faulty member can't grow them without limit. Certificates carried in proposals and timeouts still move
  a member that is further behind.
- **Catching up after an outage** (M1 requirement, coordinator m871a9a3a61d9; @design-reviewer m58bfc0fc985a).
  pe-net retransmits Protocol messages through an outage shorter than its idle limit (5 s). Past that limit, or on a
  new connection, whatever was queued is lost (transport.md 3.2; @shader-lang m3d368226d4d7). So:
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
- Never treat "the entries stamped in [floor, cut frame)" as "everything the cut includes" (coordinator
  m1734aefac651).

## 5. Holds, the final log, events

- `hold_votes_from(frame)` and its release: no vote for a cut at or past a held frame until released (logic.md 6.2).
- `final_entries(from, to)`: the final log between two final cuts, in agreed order (stamp, then the seeded rank),
  for rung 1's replay, the fault bundle and the merge.
- Events are `pe_seams::AgreeEvent`; below-quorum events (`QuorumLost`, `Unconfirmed`, `LineDiverged`) are v1's.
- **A cut's root is over its own final log** (pe-seams v1.6 `EntrySource::entries_for_cut`; @harness-eng's
  pipelined-void finding m365a498e6849, coordinator m90c32fc9ceb2). For a cut handed out for roots, or the final cut,
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
    @design-reviewer m414f8ac5fb25, @harness-eng m637edcfce2a1).
  - The log and its indexes (`ChainLog`, `by_ref`, `arrived`) grow for the whole session; pruning them is open work.
    The root cache is versioned by an insert counter, so a prune can't bring back a stale answer. The range index
    (`by_seq`) is pruned with the kept frames: each chain's rows for entries stamped below `kept_from` go, since no
    honest member asks that far back by range (@design-reviewer's Low 2, @engine-architect's Low 1).
  - Open (@design-reviewer m0c154bca4188, not blocking): a Byzantine voter can fill an author's four vote-only
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
- **What the leader check doesn't stop** (@design-reviewer m21de433b06f1): a voter takes the first proposal of the
  view, by proposer key, whose own parent names its proposer. Under the timeout-certificate path a Byzantine member
  can propose on a different eligible parent whose chain names it as leader. Voters that hold it vote for it, the
  others for the honest proposal, and the view times out [I]. That costs at most one view, like a Byzantine leader's
  silence, and never safety.
- **Blocks travel by name once certified.** A proposal whose parent this member lacks waits (one per view) while
  the parent is fetched by name from every member; a final block's missing ancestors are fetched the same way
  before any of them is reported final. The model delivers every proposal to every member.
- **The adaptive timeout** (crates/pe-agree/src/core.rs): the cadence, plus the largest of a floor, three times the
  measured time from entering a view to its certificate, and twice the measured wait from entering a view to its
  proposal (both averages weighted 1/8 to the newest), growing by half after each timeout certificate, up to a
  ceiling; the count of growths halves at each commit (section 3). The proposal wait is sampled once per view, only
  from the view's rightful leader's proposal that passed the core's checks, clamped at this member's current timeout
  (so a leader can raise others' timeouts no faster than by proposing late), and also when the proposal comes after
  this member timed out in the view; a view this member led and saw certified decays it by 7/8, so a member that
  leads every view (the reputation rule at n = 2) doesn't keep a slow link's estimate after the link recovers
  (measured: timeouts back under 1 s within 2 s of a drop from 1.2 s to 50 ms). The first sample sets the estimate
  directly (ruling m16df0b1e1d39): a slow first view is real information, and the per-view clamp bounds it. All
  three limits are durations. A view
  ended at once (a root split, an abstention, more than n - q timeouts held) or a leader's view its backlog cap kept
  empty doesn't grow it, so the timeout grows only at a member whose own timer fired: at n = 2, where the members
  alternate, it grows about every second timed-out view (@design-reviewer's L2).
- **Empty cuts don't pause yet.** v0 proposes on every cadence, empty or not; the pause after the last content cut
  becomes final (section 3, "The last cut needs a child") comes with an idle mode in which nobody times out.
- Blocks are named by hash; the model names them by their path of views and cuts.
- Roots are real state hashes; the model's root is one bit (right, or wrong on one fault-injected replica).
- The model has one or two entries per author and two or three content cuts; its bounds and what is exhaustive are in
  `models/cuts/README.md`.

## 8. Membership change: epochs (model v1)

Ruled by the coordinator (mdb308c063fa5: epochs, as DiemBFT v4 reconfigures), with @design-reviewer's gaps
(m87f0b168298a) and wording (m8d3f04d51b87). Built in pe-agree for `Leave` only (below); `Admit`, `Remove` and
`Rechain` are not built yet.

- **Build order: `Leave` first** (coordinator mda986537a492; @item:primordial-engine/m1-leave-a-member-leaves-gracefully-and-the-rest).
  M1's demo has a member leave and the other carry on alone, and at n = 2 that is impossible without epochs: q = 2,
  so once the leaver is gone nothing becomes final again. pe-agree builds the epoch machinery (the epoch in
  messages, the reconfiguring cut, suffix cuts, the switch at E's finality) with `Leave` only; `Admit`, `Remove`
  and `Rechain` follow on the same machinery. The persisted vote state below comes with `Rechain`, the only change
  a restarted member makes. At n = 1, `quorum(1)` = 1, so the remaining member certifies alone and finality is the
  usual two-chain (model v1's leave-2to1). The world side is pe-world's, from the same final `Leave` entry, as
  `World::admit` applies a join: it removes the leaver's two hands, releases its claims and drops its permissions
  at the leave's stamp (@world-model).
- **In pe-agree** (crates/pe-agree/src/node.rs, `Leave` only):
  - A `Leave` is a class Protocol entry whose payload is `pe_agree::LEAVE`. The `Leave` rules run over each cut's
    newly effective entries in the final log's order, the same in `finalize` and in the roots' `effective_in`: a
    member's first `Leave` takes effect, and a later entry of its key is void as `AfterLeave`.
  - E is the first block above the final cut whose effective entries include a `Leave`. Past it, the leader proposes
    only suffix cuts and a voter refuses anything else (`Reject::AfterReconfig`); a suffix cut is never reported
    final (`CutFinal`), so the epoch's final log ends at E.
  - When E is final, the node starts the next epoch's core: E's cut is its genesis (heights go on from E's), its
    members are the old ones less the leavers, and q follows. Each epoch signs under its own domain (the line in
    epoch 0, so its signatures are unchanged, then a hash of the line and the epoch), and its core messages travel in
    an epoch envelope (`Wire::Epoch`); a message of an epoch the node isn't in is dropped without a fault.
  - The old core is kept 30 s after the switch: it times out, proposes suffix cuts and votes them with E's root, so a
    member that missed E's finality finishes it.
  - Cuts advance only members' chains: a former member's chain keeps its head, and a cut that advances it is refused
    (`Reject::NotMember`). Its stray entries and messages are dropped without a fault.
  - `submit` refuses anything after the member's own `Leave` (`SubmitError::AfterLeave`).
  - **A `Leave` that would empty the epoch is void as `LastMember`** (@design-reviewer's L1 on e3597563): the line
    keeps its last member, which stays a member and may sign again. With several in one cut, the earlier ones in the
    final log's order take effect.
  - **When the leaver may quit** (@design-reviewer's M2, coordinator m5e5018854743). E's finality is local: at n = 2
    whoever forms the certificate on E's suffix child has E final first, and if that is the leaver and it quits then,
    losing what it hadn't sent, the other stays in the old epoch with q = 2, stuck as after a crash (measured: 3 of 9
    runs). So each remaining member, once E is final there, tells every leaver so (`Wire::EpochFinal`, unsigned: a
    forged one can only make a leaver quit early), every second while its old core is kept, and S3's
    `leave_settled()` is true once every remaining member has. pe-app waits for it before quitting, with a visible
    bound ("leaving…"; @engine-architect). Measured: waiting for it strands no one in the same 9 runs, quitting
    355-558 ms after the `Leave`.
  - **Not built: a member that misses E for more than 30 s is stranded** (@design-reviewer's M3; coordinator
    m5e5018854743: a limit for now). "Fetches it like a committed block" above isn't built: a member still in epoch
    e drops every message of e + 1, and once the others' old cores are gone nothing finishes E for it. At 2 -> 1 the
    laggard is the remaining member, no worse than a crash; at 3 -> 2 it strands both remaining members (q = 2 needs
    it). **So leaves at n >= 3 aren't supported until the epoch-certificate fetch is built** (a member that receives a
    later epoch's message asks its sender for E, the suffix and the finalizing certificate; its own docket item).
  - **A cut's identity is its `CutRef`** (S3 v1.7; L2): heights go on across the switch, so the old epoch's suffix
    cuts and the new epoch's first cuts share heights with different frames.
  - The consensus hash is v5: this build's final log differs from v4's from the first `Leave`.
  - v6 adds no setting: votes and timeouts carry the lateness reports of section 3 (stamp-ahead), a wire change, so
    builds before and after refuse each other at start.
  - Tests (crates/pe-agree/src/node_tests.rs): 2 -> 1 (the leaver stays for E, then quits; the other finalizes
    alone), 3 -> 2, an entry signed after the `Leave` void as `AfterLeave` at the same cut everywhere, the leaver
    waiting for `leave_settled` (and the naive quit as its control), the last member's `Leave` void.
- **A crash at n = 2 is not a leave.** A member that vanishes can't be removed at n = 2 (`Remove` needs its own
  vote), so the other stays unable to finalize; only a graceful quit, which lingers about a round trip for E's
  suffix, ends in an epoch of one. **An interrupted leave ends the same way** (@design-reviewer's L2): if the
  leaver's process exits after E's proposal but before E is final, or the remaining member is unreachable when it
  quits, the remaining member is stuck as after a crash, and worse, since inclusion has paused since E's proposal.
  So pe-app's quit waits for E to be final, with a visible bound ("leaving…"), and says what the remaining member
  shows if it never comes (app.md, @engine-architect).

- **Epochs.** A line's members are fixed within an epoch; epoch 0's are the genesis members. A membership change
  ends the epoch at a final cut, and the next epoch starts from that cut with the changed members. Proposals, votes,
  timeouts and their certificates name their epoch, and a message of another epoch counts for nothing in this one
  (control c). The quorum, the rotation and the views are the epoch's own; views restart at 1.
- **Membership entries** are ordinary signed entries (class Protocol), validated in chain order like any other:
  - `Admit{key}`, by a member: the key becomes a member.
  - `Leave`, by the leaver. **It is the last entry of the leaver's chain** (coordinator mda986537a492, from
    @world-chunks's answer mb1f8796dd9f7): pe-agree refuses to sign anything after it on any chain of that key
    (a local `SubmitError`), and an entry of that key in epoch e that comes after its `Leave` in the final log's
    order, on any chain, is void as `AfterLeave` (@design-reviewer's L1: a key may have two chains in an epoch, and
    a returning member's new chain is in a later epoch), so hand entries stamped after the leave can't enter E. A
    void, not a fault (@design-reviewer m6305373b98d4): verdicts stay per cut, and the key stops being a member at E
    anyway. Both names are S3 changes, to confirm
    with @engine-architect when the seam's version is bumped.
  - `Remove{key}`, by an authorized member other than the one removed, so a crashed, departed or Byzantine member
    can be removed. Who is authorized, for `Admit` too, is governance's (admins, the user's ruling); in the model it
    is abstract, and the Byzantine member runs both authorized (it may sign `Remove{honest}` and `Admit{sybil}`) and
    not (@design-reviewer m27d9369eca11, Low 4).
  - **Assumption: governance never authorizes a Byzantine member.** The authorized run breaks agreement, as
    expected (model v1's remove-auth-byz): removing an honest member at n = 4 leaves n = 3 with t = 0 and the
    Byzantine member still in. Agreement holds only while every key that may sign `Admit` or `Remove` is honest
    (@design-reviewer ma6414f4faeeb).
  - `Rechain{epoch, old head}`: seq 1 of a restarted member's new chain under the same key (below).

  A change takes effect only through a final cut, never at inclusion (control a). Several changes in one cut apply
  in the final log's order; one that changes nothing (admitting a member, removing a non-member) is void.
- **The reconfiguring cut E** is the first cut of a chain in epoch e that includes an effective change. Every
  descendant of E in epoch e is a **suffix cut**: E's frame and E's heads, nothing new. Voters refuse any other
  descendant (`Reject::AfterReconfig`; control b). Suffix cuts are exempt from `FrameNotAbove`; E is on the grid, so
  they are too. They exist only so that E can become final under the two-chain rule. Whichever certificate finalizes
  E or a suffix cut above it, epoch e's final log ends exactly at E (control e), so no epoch has two final
  reconfiguring cuts. After a view change during the suffix, the next leader proposes an empty child of the highest
  certified block (E or a suffix cut) under the usual lock.
- **Epoch e+1 starts** at a member when E is final there. Its genesis is E, its members are e's with E's changes
  applied, and t and q are recomputed. A leaver keeps voting in e until E is final; at n = 2 the suffix always needs
  its vote, so a graceful quit lingers about a round trip (@fable-reviewer, via m8a3b3dc21660). The epoch
  certificate is E's proof of finality: E, the suffix up to the block finalized with it, and the quorum certificate
  of that block's child in the next view. A member that missed it fetches it like a committed block (section 4).
- **Entries across the change.** Only inclusion pauses, from E's proposal until E is final (about one round trip).
  Members whose chains continue keep authoring, and their replicas keep predicting; what they make meanwhile goes
  into e+1's first cut. That cut's parent is E, so "late" is measured against E's frame by the ordinary rule
  (section 3, pe-agree's own predicate). Changes that arrive during the suffix wait for e+1 and are judged against
  e+1's members.
  - **Expected voids:** a leaver's or removed member's entries that E doesn't cover (their author isn't a member of
    e+1), and a restarted member's old-chain entries above E's heads.
  - **Property:** no entry of a member that stays a member across the change, on a chain that continues, is void
    because of the change.
- **A joiner** admitted at E imports the world at E through `World::admit` (the M1 interim's path; E's root covers
  the admission). Its invite pins an epoch: the epoch's id and a hash of its members, signed by the inviter. The
  joiner verifies each epoch certificate from there to E, against the members each epoch had. Its chain starts at
  seq 1 with entries stamped at or above E's frame (lower is late). Admits pending together go into one E, so a burst
  of joins (ten at once, say) takes one epoch, not ten.
  - **Open test:** a joiner entering a frame clock that is already running. Epochs don't change it, and nothing
    tests it yet (@fable-reviewer).
- **Return** is a `Leave` (or a `Remove`), then later an `Admit`; the returning member starts a new chain.
- **Restart in place (`Rechain`).** A member whose process restarted has lost its chain's head. Its new chain under
  the same key starts with `Rechain{epoch, old head}` (the head it can fetch from peers, if any). Naming the epoch
  and the old head is its replay protection outside the chain: an old `Rechain` can't restart the key again in a
  later epoch (@fable-reviewer). It is the one extra chain a key may have in an epoch; two chains under one key are
  otherwise as in section 2. The restarted member signs nothing else until its `Rechain` is final: its authoring
  pauses, unlike a continuing member's. At E the old chain closes at E's heads, and its entries above them are void.
  The new chain's later entries are stamped at or above E's frame: the per-key stamp floor across chains (section 2;
  control d). The `Rechain` entry itself is included in E, so it is stamped below E's frame, possibly below old-chain
  entries E includes; it has no world effect and is exempt from the floor (Low 3).
  - **A second restart before the `Rechain` is final** loses the new chain too. The member fetches its `Rechain`
    (seq 1) from peers and continues that chain. If no peer has it, the earlier `Rechain` was never included, so a
    fresh one is still the only extra chain, and allowed (Low 2).
  - **Votes after a restart** (coordinator m8a3b3dc21660; @design-reviewer's Medium, m27d9369eca11). A restarted
    member that forgot which views it voted in could vote twice in one view: an equivocation by an honest key. One
    that kept its views but forgot its `high_qc` could time out with an older one, so a timeout certificate's
    `high_view` falls below a committed block and the lock rule lets a fork through. So the vote state is safety
    data: the epoch, the highest view voted in, the highest view timed out, and `high_qc` (the certificate itself,
    since timeouts carry it), written before each vote or timeout is sent. Those two moments suffice: the lock's
    proof uses only members that voted for the committing child. A restarted member votes and times out only above
    the stored (epoch, view), compared lexicographically since views restart in each epoch; one that missed epoch
    changes fetches their certificates first. (Not voting in epoch e at all would make a restart impossible at
    n = 2, where q = 2.) It is a requirement on the node: pe-app keeps it in the same per-machine data folder as
    `identity.seed`. The model checks two forgetting variants as control f.
- **Out of scope:** at n = 2 an absent member can't be removed, since q = 2 needs its vote. That case is the user's
  branch rule.
- **Model v1** (models/cuts) checks sizes 2→3, 2→1, 3→2, 3→4 and 4→3, and a restart, with one Byzantine member at n = 4
  (the removed one). Properties: agreement across epochs; the same members per epoch; no epoch with two final
  reconfiguring cuts; the property above on continuing chains; a burst of joins takes one epoch. Controls, each of
  which must give a counterexample:
  - (a) a change takes effect at inclusion;
  - (b) voters accept a descendant of E with new entries;
  - (c) a vote counted in another epoch, a removed member's included;
  - (d) the `Rechain` stamp floor broken (old-chain entries accepted above E's heads, or the new chain's below E's
    frame);
  - (e) the epoch's final log ends at the cut that finalized E, not at E;
  - (f1) a restart that forgets the views it voted and timed out in; (f2) one that keeps them but forgets its
    `high_qc` (n = 4 with the Byzantine member).

## 9. Not in v0

One branch merging with the main line (model v2); quorum loss, unconfirmed play, the one-minute bound and split
groups (model v3).

- **Catching up from far behind needs an import.** Peers serve only their last 256 committed blocks. They serve
  entries from a ChainLog that is never pruned today; once pruning comes, a member behind by more than the kept
  window needs the import path for the entries too (@design-reviewer m3a0b0db4241f).
