# Link elision: no checkpoint link on the full-member wire (spike)

Item: @item:primordial-engine/r18-controller-cliff-4-more-bytes-flips-a-thin-l (@world-chunks, 2026-11-07). Shape:
@design-reviewer (mdebf5986505e). Rulings: @coordinator-opus m856815c5499e (elision first), m472b0f4a9dc0 (land it;
the Part B port's gate is judged on 5-delay means; the controller retune is split off to
@item:primordial-engine/r18-controller-retune-for-the-3-player-thin-cell). Branch spike/link-elision.

## Problem

Thin links Part B gave every member's first entry in a 32-frame block a checkpoint link, the id of that member's
previous checkpoint (TLP-19, TLP-31; 8 B in the spike, 32 B in production). At 2 players and 8 kbit/s that grew
entries by about 4% (hand entries 47 -> 49 B, acks 70 -> 71 B). The other member then stalled 762 frames instead
of 209, and returned to real time after 12.1 s instead of 3.3 s (design 10.9).

## What the wire does now

- **`Msg::Live` (tag 14):** an author's own checkpoint, from its broadcast, to a member that isn't fed. It carries
  the canonical bytes without the link's value. The flags byte keeps bit 1 ("has a link"), so H_rest, and with it
  the entry's id, is unchanged.
- **Live-wire flag bit 2: "the link is my prev".** It's set when the previous entry is the checkpoint the link
  names. The wire carries prev, so the receiver has the link without any state. That covers every entry of a chain
  sparse enough for each entry to start its block (R18 levels 7-8, the thin links), and it holds behind holes too.
  Bit 2 never enters the canonical bytes. It costs no bytes, since the flags byte is already there.
- **Otherwise the receiver rebuilds the link** from its checkpoint state after the entry's prev (`ckpt_link_for`,
  the rule `validate` already checks). Then the entry goes in as any explicit one: production checks the id and the
  signature over the rebuilt bytes, and `validate` checks the link against the chain state, as before.
- **Behind a hole** (seq > contiguous + 1):
  - **If the hole can't hold a checkpoint, the link is known.** The entry is stamped in the block right after the
    newest checkpoint of the receiver's contiguous prefix. It is the first entry in its block, so every entry in the
    hole lies in that checkpoint's block or earlier and is no checkpoint (an honest author's would sit in a block
    in between). The entry goes in past the hole at once, as an explicit one does.
  - **If the hole spans a block, the entry waits** (`live_wait`, at most 256) and goes in once the chain reaches
    seq - 1. While waiting, it counts as hearing from its author and keeps the receiver busy, as insertion would, and
    it shows the hole (the same fetch as `insert`).
- **Asked explicit from the sender:**
  - when the chain is held to seq - 1 but there's no state for the entry's prev. That's a chain held from midway, as
    a joiner or a member just off the feed holds it: one fetch per author, until its first checkpoint validates on
    the existing trust path. Or it's another version of prev;
  - when the state says the entry isn't a checkpoint. Production's signature check over the rebuilt bytes would
    fail too, so the receiver never counts it as a bad signature.
- **What stays explicit:**
  - fetch replies, relays and proofs, whose receiver may lack the state after prev;
  - view-change and admin entries, which a receiver takes by value at arrival;
  - entries with a carried state, since the transfer accept reads the id at arrival;
  - anything to a fed member, which gets no entries but those anyway and holds no log.
- **Certificates and `Msg::Ckpts` are unchanged.**
- **Only the author sends live** (review L2). `insert_live` drops a live entry from any other sender and counts it
  (`live_forged`), whatever its caller checks.
- **Waits are capped** (review L1): at most 32 per author, where the farthest goes, and 256 in all, where the author
  holding the most gives up its farthest. An author sending far-ahead live entries evicts only its own. A dropped
  entry is fetched explicit by the hole rule once its chain gets there.
- **Switches** (spike only):
  - `PE_NO_LIVE=1` sends every link, as main did (A/B);
  - `PE_WHATIF_LIVE=1` with it charges the shaper as if elided (measurement);
  - `PE_R18_DELAY_US` replaces R18's 50 ms one-way delay (measurement).
- **r18 logs** print a "live checkpoints per member" line (received / held behind a hole / asked explicit), only
  when live entries went out.

## Evidence [V]

**A/B.** With `PE_NO_LIVE=1` the branch reproduces main's committed logs byte for byte: r18-thin 64, r18-thin-partb
35, r18 14.

**The 2-player rows** (r18 capped-8kbps = r18-thin up-k8-m2): main 761 / 762 stalled frames and back after 12.1 s;
with elision 224 / 209 and 3.3 s. Those are the figures from before the link.

**Every cell, single runs at 50 ms** (`spikes/out/link-elision/cell-compare.txt`; "worse" means more than 10 stalled
frames and 10% more, a return more than 500 ms later, or an end state that's no longer EQUAL):

| Set | Better | Worse | Same |
|---|---|---|---|
| r18-thin-partb (what ships at 3+ players) | 2 (up-k8-m2; up-k24-m4: 758 -> 613, back after 22.5 s instead of never) | 1 (down-k32-m3: 1,872 -> 2,066, never recovers in either) | 32 |
| r18 | 1 (capped-8kbps) | 1 (capped-slow-member: 1,438 -> 2,085, 7.5 s -> 11.1 s) | 12 |
| r18-thin (Part A only) | 8 | 12 | 44 |

No live checkpoint was asked explicit in any probed cell. The thin member holds about a fifth of its live
checkpoints behind holes in down-k8-m3 (23 of 108).

**The single runs aren't the figure: a 4 ms change of delay moves the 3+ player Part A cells 2-7x.**
- The jitter seed moves nothing: runs are byte-identical across seeds. So the spread is measured over the one-way
  delay, 46 / 48 / 50 / 52 / 54 ms (`spikes/out/link-elision/delay-sweep.txt`).
- The table gives the others' stalled frames, mean per member, at each delay, then the mean:

| Row | Main's wire | Mean | Elision | Mean |
|---|---|---|---|---|
| up-k8-m2 | 769 / 769 / 762 / 762 / 762 | 765 | 342 / 209 / 209 / 210 / 210 | 236 |
| up-k8-m4 | 157 / 158 / 159 / 159 / 160 (never back) | 159 | 158 / 159 / 160 / 160 / 161 | 160 |
| up-k12-m4 | 83 / 82 / 80 / 102 / 114 | 92 | 116 / 73 / 116 / 180 / 161 | 129 |
| down-k8-m3 | 478 / 491 / 481 / 499 / 362 | 462 | 96 / 482 / 696 / 771 / 569 | 523 |
| down-k8-m4 | 596 / 742 / 827 / 453 / 821 | 688 | 502 / 861 / 458 / 1,106 / 471 | 680 |
| down-k12-m4 | 2 / 92 / 66 / 68 / 68 | 59 | 59 / 35 / 82 / 51 / 51 | 56 |
| down-k12-m8 | 652 / 491 / 1,115 / 156 / 701 | 623 | 155 / 604 / 485 / 485 / 725 | 491 |
| down-k16-m8 | 114 / 202 / 139 / 195 / 203 | 171 | 113 / 113 / 187 / 53 / 57 | 105 |

- **The 2-player fix holds at every delay.** The other 3+ rows' changes are inside each row's own spread. Over the
  five delays, elision is neutral or better in 6 of the 7 (up-k12-m4 is worse, 92 -> 129).
- **Review [V], @design-reviewer:** with bytes only, up-k12-m4 is 115 / 73 / 116 / 180 / 161, against elision's
  116 / 73 / 116 / 180 / 161. down-k8-m3 is 96 / 490 / 512 / 785 / 569 (mean 490). So up-k12-m4's +40% is the bytes
  moving the controller. Of down-k8-m3's +13%, 6% is bytes, and the rest is one delay (50 ms: 512 against 696),
  from the holds [V]; why is [I]. Both were accepted (@coordinator-opus, 2026-11-07).
- **A bytes-only what-if flips cells both ways too.** That's main's wire, with the shaper and controllers charged
  as if elided and nothing held. Against main it's 13 better and 7 worse (the up cells at 12-16 kbit/s with 4 and 8
  members). The cliff the item names is this sensitivity, not a cost of elision's receive path.
- **So no byte change can meet the item's acceptance at 3+ players** (no more than ~10% movement for +4% bytes);
  only a controller retune aimed at the spread could. That retune is split off and backlogged
  (@item:primordial-engine/r18-controller-retune-for-the-3-player-thin-cell). Under Part B, which ships at 3 or more
  players, the down cells' thin member is fed, and the others don't stall.
- **The Part B port's gate** (pe-session's Part A thin figures no worse than ~10%) is judged on 5-delay means
  (46-54 ms), not single runs (@coordinator-opus m472b0f4a9dc0).

## Production notes [I]

- **Bytes.** The link is 32 B in production, so elision saves four times the spike's 8 B on every checkpoint
  entry.
- **Bit 2 as the common case.** At thin-link levels nearly every entry is a checkpoint whose link is its prev, so
  the link always comes with bit 2. There's no wait, even behind a hole.
- **Signatures and lying authors.** A rebuilt entry is verified exactly like an explicit one: the signature over
  the rebuilt canonical bytes. A lying author's live entry fails verification and is fetched explicit. It's never
  counted as a bad signature, the same rule as pe-session's lean entries (option 1, condition (b)).
- **What a held entry can't do** (review L3): until it's rebuilt, it isn't in the receiver's log, so the receiver
  can't serve it to another member's fetch or see an equivocation at its seq. That only happens behind a hole
  spanning a block. The port keeps the explicit fallback, which also covers a sender whose view of who is fed is
  stale.
- **Porting to pe-session.** pe-session leaves prev out of its lean and run entries, so bit 2's prev is the
  receiver's rebuilt prev. The port goes with Part B's.
