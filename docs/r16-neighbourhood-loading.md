# R16: loading the region you are in and nearby regions

Author: @world-chunks. Design note, not built and not yet reviewed. It answers the user's ruling of
2026-10-30 18:42Z on D60: "players should load the region theyre in and nearby regions". It builds on
design.md section 25 (merged at 7a7c26e) and uses the measurements in
[r16-chunks-results.md](r16-chunks-results.md).

Tags: **[V]** measured by the R16 spike, **[C]** computed from measured numbers or from the rules' constants,
**[I]** inferred or proposed.

## Summary

* **The 10 s is not loading time.** Loading what a player needs takes milliseconds:
  * building the region from dormant records: 330-640 bodies at 0.7-0.9 us each [V];
  * procedural generation: 0.2-3 ms per second while moving at up to 1,500 m/s [V].

  The 10 s (`S_create`) is a protocol delay. It lets every other region learn of a new region before it
  exists.
* **"The region you're in and nearby" is already what a player simulates**, in the user's sense of areas
  on the chunk grid. A footprint is the player's chunk plus at least 3 chunks in every direction, at the
  player's scale band (25.5.2), so at least 7 x 7 chunks at human scale. In the design, a "region" is a
  group of players simulating together, not a fixed area, and that is what made the ruling look unmet.
* **Recommendation: three changes, no fixed tiling of simulation regions.**
  1. **Arrival is instant everywhere.** A region created by an arrival takes effect at its stamp, not
     10 s later. It simulates from that frame, and its frames confirm once the world log confirms the
     creation is valid, about 1.1 s later. The `S_create` slack for every other region is unchanged.
  2. **Players follow nearby regions.** A player holds and simulates every other region within about
     8 km (at human scale) from that region's confirmed entries. A follower does not ack or vote, so
     nobody ever waits for it.
  3. **Content preloading.** Chunk files and dormant records within the same radius are fetched ahead
     of time.
* **Cost of following** [C]: about 0.07 ms per tick, 0.3 MB and 7 kB/s for each nearby spread-out
  player; 0.5 ms, 2.3 MB and 70-95 kB/s for a nearby group of 8. It needs a budget on the D50 slider.
* **Making neighbours full members** (co-confirming) would re-couple spread-out players. It would bring
  back "one stall freezes the neighbours": adjacent fixed tiles have zero slack. Not recommended.

## 1. How the ruling maps onto section 25

### What "region" means in each reading

| | User's wording (OWO-style) | Section 25 |
|---|---|---|
| Unit of space | A fixed tile: a chunk or a cell of the chunk hierarchy | The chunk hierarchy (25.3): level-`L` cells `1024 x 2^L` m, fixed and nested |
| What a player loads and simulates | Its tile and the adjacent tiles | Its **footprint**: the chunks within `ceil(reach_cap / 1024) + 1` of its anchor chunk, at its band |
| Unit of simulation and confirmation | (Not specified) | A **region**: the union of nearby members' footprints, one box3d world and one log, merged and split dynamically |

So at the level of space, the ruling is already the design. A player always simulates the area it is in
and the area around it, and the active set inside that area is chosen by size-based relevance (25.4).
What the ruling changes is when that area starts (section 2) and whether players also hold the regions of
other players nearby (sections 3 and 4).

### Why not a fixed tiling of simulation regions

Suppose each tile is its own region and every player holds its tile plus the adjacent ones.

* **Physics would stop at tile seams.** Bodies in two adjacent tiles must be in one box3d world to
  collide. If two players' neighbourhoods overlap, their tiles must be simulated as one world. That is a
  dynamic merge again, the section 25 rule with `D_merge` = 0 and footprints rounded to tiles.
* **It removes no protocol work.**
  * An empty tile has no members to order who arrives first. Two players warping into the same empty tile
    at once is the same race as two creations (review M1), and still needs the world log to order it.
  * A walker approaching a newly occupied tile still has to learn of it before it can merge. That is the
    creation buffer again.
* **It re-couples players** (section 4).

Section 25 already has a fixed tiling where it is useful: chunk files, dormant records, procedural cells
and the spatial index are all keyed by fixed cells. Regions stay dynamic.

### What "nearby" means at different scales

Every distance below scales with the player's band unit `u` (25.6). At human scale `u` = 1 m.

| | Human (`u` = 1 m) | x128 giant (`u` = 128 m) | 1/128 player (`u` = 1/128 m) |
|---|---|---|---|
| Band chunk (1,024 `u`) | 1 km | 131 km | 8 m |
| Footprint radius (own area, already simulated) | 3 km or more | about 400 km | about 25 m or more |
| Follow radius (other regions held, proposed) | about 8 km | about 1,000 km | about 60 m |

* The follow radius is the creation buffer at age 0:
  `D_merge + v_max x S_create + G_inst` = 3 + 1 + 4 km at human scale. That is the distance within which
  another region could reach a merge with yours within about `S_create`.
* Across bands, the reader rule of 25.6 already makes a small player hold a giant region over it. Following
  is the same-band counterpart, without the confirmation dependency.

## 2. The 10 s wait and the creation-race rules

### Proposed: provisional arrival (removes the wait)

* An arrival away from existing regions posts `RegionCreated{region, footprint}` to the world log as today.
  The new region takes effect **at its stamp `s0`**, and the arriving player simulates from `s0`.
* **Its frames confirm only once the world log has confirmed the creation as valid**, in world-log order
  against the directory and the pending creations, exactly as now (M1, with the Q1 buffer).
  * World-log lag is about 1.1 s [C: `S_create` minus the 8.9 s stall budget, 25.5.2], under the 2 s
    speculation limit, so the player never pauses unless the world log itself stalls.
  * No confirmed frame is ever undone.
* **If the creation turns out invalid**, it becomes a join of the nearest region as today. The arriving
  player's speculative first second is dropped, and the player continues as a joiner of that region.
  * That happens when another arrival, or another region's unseen growth, lands within about 8 km of the
    same spot within the world-log lag. The arriving peer checks the confirmed directory first, so an
    arrival it can already see is too close becomes a join at once, without a provisional region.
  * Experience: objects the player moved in the first second snap back, and the surroundings follow the
    growth clock of the joined region (below). Spawn regions remain useful at busy arrival points (portals,
    spawn points), because there two arrivals are ordinary joins and never conflict.
* **For `S_create` after `s0`, the new region's footprint does not grow.** Its growth clock starts at
  `s0 + S_create`, the frame from which every other region holds it in the directory.
  * Why: other regions do not count it before then. The creation buffer covers their growth toward its
    initial footprint, not toward a footprint that is also growing.
  * Experience: the player moves freely within at least 3 km in the first 10 s. A scale-up in that window
    keeps its avatar change, but the footprint growth waits until 10 s have passed: up to 10 s added to the
    D61 ramp, only in that window.
* **Nothing leaves the region until its creation is confirmed valid:** no `Frozen` checkpoints, no
  `RegionHead`, no cross-region messages and no world-log facts that depend on its state. Nobody else
  follows a region that is not yet in the directory.

### What stays and what changes

| Rule | With provisional arrival |
|---|---|
| `S_create` (10 s) and the directory gate: every region simulates `f` only with the directory confirmed through `f - S_create` | **Stays.** It is the world-log stall budget for every region (about 8.9 s), and it no longer costs any player a wait. D60(b), a shorter `S_create`, loses its purpose |
| M1: creation validated in world-log order against pending creations; too close becomes a join | **Stays.** It now decides whether a provisional region becomes real |
| Q1: `G_inst` in the creation buffer and in possible footprints | **Stays, unchanged** |
| Start condition (hold every reachable region's prefix through `f0 - 1`) | **Stays**, with `f0 = s0`. The pull takes about 0.2 s [C], during which the region simulates speculatively |
| Effect frame of a created region | `s0 + S_create` becomes **`s0`**, with confirmation gated on validity |
| Growth of a created region | **New:** frozen until `s0 + S_create` |

### Arrival close to an existing region (an existing cost, not changed here)

Arriving within the creation buffer (about 8 km) of a region's footprint is a join of that region. Its
footprint grows to the arrival through the growth clock: the first 4 km at once, the rest at 100 m/s
[C, 25.5.2]. A landing 6 km out can leave part of the new surroundings frozen for up to about 50 s.

A follow-up could shrink this [I, not modelled]: the arrival announces itself in the logs of the regions
within the follow radius, which confirm it in about 0.15 s instead of 10 s. It could then become a
separate region at any gap above about `D_merge`. That changes the creation buffer's safety argument, so
it needs the safety model extended and a review before it is offered.

### Safety status

Provisional arrival is **[I], not modelled.** The argument:
* other regions' view of the new region is unchanged: it enters their known set at `s0 + S_create`, with
  a footprint fixed until then;
* the new region's view of them is the existing one, through the start condition;
* validity is decided by the same world-log rule.

Before D60 adopts it, `pe-spike-chunks safety` should gain two cases, run under the machine-load rule:
* an arrival at the closest valid gap that walks and scales up inside its first `S_create`;
* two arrivals 5 km apart, 10 frames apart, with the world log stalled 8 s.

Then a pass by @chunks-reviewer.

## 3. Cost of holding neighbours

Three levels of "holding", from cheapest to most expensive.

### (a) Content preloading (recommended within the follow radius)

* Procedural content: nothing to fetch. It is a function of `(generator hash, cell)`, and generation runs
  at 0.2-3 ms per second while moving [V, 25.11].
* Dormant records of modified objects: 101 B each [V].
* Committed chunk files: fetched by hash before they are needed. Their size depends on the world's
  content and has **not been measured**.

### (b) Following a region (recommended within the follow radius, under a budget)

The follower receives the region's entries and acks and simulates only **confirmed** frames. So it needs
no rollback and no keyframe history: one live image, the region's state about 0.15 s late. Physics never
crosses regions (25.5.5), so nothing the follower simulates for a neighbour feeds into its own region. It
serves presentation and merge readiness.

| Per followed region | CPU per 16.7 ms tick | Memory | Bandwidth received | Initial transfer |
|---|---|---|---|---|
| One spread-out player (126 bodies, 71 awake) | 0.065 ms [V step] | 0.3 MB [V image] | about 7 kB/s [C] | 97 kB [V lz4] |
| A group of 8 (1,008 bodies, 366 awake) | 0.47 ms [V step] | 2.3 MB [V image] | about 95 kB/s; 70 kB/s with compact acks [C] | 672 kB [V lz4] |
| For comparison: being a full member of that group of 8 | 6.8 ms [C] | 11.6 MB [C] | 94 kB/s [C] | 672 kB [V] |

* Bandwidth is computed from the encodings in the results doc (section 8): per member of the followed
  region, a 44 B hand target and 52 B of datagram overhead at 60 Hz, plus a `4 + 25n` B ack every 2
  frames. That is about 7 kB/s per member alone and about 12 kB/s per member in a group of 8.
  * The follower needs the acks to judge confirmation, so following a group costs about what membership
    costs in bandwidth. Taking a confirmation summary from one member instead would cut it to about the
    entries alone, about 45 kB/s for a group of 8 [I].
  * At 90 Hz (the D4 ruling) entries and overhead are 1.5x.
* **Upload** for the followed region's members: each follower subscribes to every member's stream, so
  each follower adds about 7-12 kB/s per member [C]. Past a few followers, streams are relayed through a
  fan-out tree as for pose streams (26.3).
* **Typical neighbourhoods** [C]:
  * Regions stay at least 3 km apart and a human footprint is at least 7 km wide, so at most about 6-8
    other regions fit within 8 km.
  * 8 spread-out neighbours cost about 0.5 ms per tick, 2.4 MB and 55 kB/s.
  * 8 groups of 8 would cost 3.8 ms, 18 MB and 0.6-0.8 MB/s. That is too much by default, hence the
    budget.
* **Budget** [I]: follow nearest-first within a CPU budget on the D50 slider, for example 1 ms per tick at
  Balanced. Regions beyond the budget fall back to today's handling:
  * avatars by pose stream (26.3), objects shown as last confirmed or frozen;
  * prefetch by state transfer during the approach (25.5.4), for example 672 kB, about 0.5 s at
    10 Mbit/s [C].
* **What following buys:**
  * merges with no state-transfer hitch, because the follower already holds the region;
  * live motion of large things (vehicles, collapsing buildings, giants) beyond your own footprint;
  * a neighbour bound that stays fresh: `known_conf_q` is about 0.15 s old instead of up to 60 s, so the
    horizon gate and on-demand pulls stop binding for followed regions.

  Small objects in a neighbour's region are below your relevance threshold anyway: at 5 km only objects
  with a radius of about 160 m or more pass `rho` = 32.

### (c) Full membership of neighbours (not recommended)

Holding the neighbours as a co-confirming member costs the full member price in the table above for every
neighbour, rollbacks included. It also couples confirmation (section 4).

## 4. Coupling: does holding neighbours re-couple spread-out players?

* **Followers: no.** A follower publishes no acks, horizons or votes in the followed region's log.
  * The followed region's confirmation is computed only from its own group (25.5.3), so a stalled or slow
    follower changes nothing there.
  * A stall in the followed region freezes only the follower's *view* of it. The follower's own region was
    already bound by the neighbour bound, which is unchanged.
  * The stall model's result stands: a stalled member costs regions whose footprints are 3.5 km or more
    away 0 frames [C, 25.5.2 liveness].
* **Full members: yes, in two ways.**
  * **Co-confirmation spreads.** If every player co-confirms its 3 x 3 neighbourhood, a tile's
    confirmation waits on every player within one tile of it. A chain of players two tiles apart forms one
    chain of coupled confirmations along its whole length, although no two of them are in the same place.
  * **Adjacent tiles have zero slack.** The neighbour bound gives a region slack in proportion to its gap,
    and adjacent tiles have none. A stalled player would hold every adjacent occupied tile until it is
    evicted: 2.2 s in-region or 3.8 s through the world log [C, 25.5.3], about 130-230 frames at 60 Hz.
    Under section 25 as it stands, footprints 3.5 km apart lose nothing.

  That is the "one stall freezes its neighbours" problem the neighbour bound removed (review R16-1),
  smaller in reach but back.
* Players whose footprints come within `D_merge` are coupled in either design, because they share one
  region. That is needed for exact physics between them, and D62 caps it.

## 5. What changes in D60 and D61

### D60, recommended text

* **(a) recommended:** regions as now, plus **provisional arrival**, **following** and **content
  preloading**.
  * Physics starts at once wherever you arrive.
  * For the first 10 s, your simulated area (at least 3 km around you) does not grow.
  * Rarely, another arrival or a region within about 8 km in the same second turns your arrival into a
    join of that region. Your first second of changes is then undone, and distant surroundings catch up
    at 100 m/s.
  * Nearby regions are followed, which costs CPU, memory and bandwidth within a slider budget (section 3).
  * Spawn regions stay at busy arrival points.
* **(b)** as before: a 10 s wait outside spawn regions, with no follow cost.
* **(c)** fixed tiles, each player a full member of its 3 x 3 neighbourhood: no creation wait, but one
  stalled player freezes adjacent occupied tiles for 2-4 s, and coupling spreads along chains of players.
  Not recommended.
* The old (b), a shorter `S_create`, and (c), arrivals tied to their previous region, are dropped: (a)
  removes the wait without their costs.
* Evidence: the wait and the loading costs are [V] or [C] (sections 1-3). The safety of provisional arrival
  is [I] until the two cases in section 2 are modelled and reviewed.

### D61

* Unchanged, except that a scale-up within 10 s of an arrival starts its footprint growth only after
  those 10 s (section 2).
* Following is the same-band counterpart of D61(a)'s reader rule. A giant region over a human region is
  still read with the confirmation dependency; a same-band neighbour is followed without it.

### Elsewhere, if adopted

* design.md 25.5.2: the creation and start-condition text, the effect frame `s0`, frozen growth until
  `s0 + S_create`, and confirmation gated on validity.
* 25.5.4: replace "prefetch during the approach" with following within the follow radius.
* 25.12: replace the D60 options with the text above.
* 26.3: a followed region's avatars are drawn from its entries (their sim parts at full rate). Only
  presentation-only pose detail still needs a pose stream.
