# 25. Massive worlds: hierarchical chunks, f64 coordinates and regions (R16)

Status: ready to merge as section 25, numbered after the architect's 24. Revised twice:
* after @design-reviewer's review of 45d4940 (R16-1 to R16-7);
* after @chunks-reviewer's re-review of 2a228c6 (N1 to N8): the growth clock and region creation rule, split
  hysteresis, single-member eviction and durability, cross-region equivocation, multi-band regions and
  per-band constants, swept closure with static proxies, and labels.

The region and group model (25.5) follows the round-2 protocol (design 10.8). It is designed, not built.
Every measured number is tagged.
Author: @world-chunks. Branch `chunks/r16`. Measurements: [r16-chunks-results.md](r16-chunks-results.md).
Tags as in the rest of the design: **[V]** measured by the spike in `spikes/chunks`, **[S]** read in
source, **[I]** inferred or proposed.

R16 (user, 14:08Z, plus 14:09Z, 14:11Z and 14:12Z):
* raw coordinates and transforms are doubles;
* the world is a hierarchically sized chunk system, top-level chunks about 1 km wide, like the Open World
  Orchestrator (OWO) but built into the engine;
* players can scale arbitrarily up and down;
* objects far away are not simulated, depending on their size, as OWO culls by distance, so that dozens
  or hundreds of players can share a session without slowing each other down as long as they are spread
  out;
* maybe an octree on the chunk system decides locality;
* the performance impact must be measured.

Ruling 14:20Z: Box3D's slop and speculative tolerances stay absolute. Small or large worlds use Box3D
length units, per world or per region.

## 25.1 Summary

* **Canonical coordinates** are a level-0 chunk index (`i64` per axis, 1,024 m chunks) plus an `f64`
  offset inside the chunk. That covers ±2^73 m (about a million light-years) at 1.1e-13 m (2^-43 m)
  resolution everywhere. Transforms are `f64` position plus `f32` rotation and scale, the same split as box3d's
  `double-precision` mode (which makes only positions `f64`; AABBs, velocities, rotations and contact
  anchors stay `f32` [S]).
* **No per-user floating origin in the simulation.** OWO's "controlled desynchronization" gives every user
  a different frame. That cannot coexist with one bit-exact shared world line, so the sim stays canonical.
  The floating origin survives in two places that do not need to agree across peers: rendering (camera-
  relative) and each region's physics origin (deterministic, so every peer computes the same one).
* **Hierarchical levels.** A level-`L` cell is `1024 * 2^L` m wide and corner-aligned, so cells nest
  exactly. Every object is filed at the level whose cell matches its relevance distance (OWO files objects
  in chunks about 8 times their size; we use the relevance ratio).
* **Relevance decides what is simulated.** An object is simulated when:
  * some player is within `rho * r` of it (`rho` = 32 by default, a slider), it is not tiny relative to that
    player (`r >= h / 64`, `h` = player height), and it is inside the reach cap;
  * or it is awake, woken, or supporting something that is simulated.

  Everything else is dormant: frozen exactly, costing memory only.
* **Regions.** The simulated part of the world splits into regions, each its own box3d world with its own
  origin, length unit, keyframes, log and group of peers. A late entry rolls back one region, not the
  world. Peers simulate only the regions they are in.
* **Two-tier log.** A world log every member holds (membership, ACL, SCM), off every region's per-frame
  path, and one log per region held by that region's group. That log carries the members' anchors.
  * Regions depend on each other only through a speed-bounded **neighbour bound**, so a stalled member far
    away never stalls a region [C: modelled].
  * Region state is a function of dormant records plus the region log.
  * No decision depends on local arrival time (the NEW-1 lesson).
  * Membership-dependent values are evaluated at the entry's frame and frozen at confirmation (the NEW-2
    lesson).
* **Arbitrary scale** uses per-region length units (ruling 14:20Z): a region simulates in units of its
  players' scale (`2^round(log2 s)`), so box3d always sees sizes near 1 unit. [V: a mixed-size stack (s/20 to
  2s) is stable from 0.1 mm to 10 km in region units; unscaled it fails below 1 cm.]
* **Headline measurements** (details in 25.8-25.11 and the results doc):
  * box3d `double-precision` costs 2-10% of step time and 1-2% of image size [V];
  * with a region-local origin, physics 100,000 km out behaves as at the origin [V];
  * a late entry rolls back one region: 0.7 ms instead of 10.5 ms for the 16-player world [V];
  * spread-out players cost each other nothing in CPU or memory: 0.12 ms per tick and 1.5 MB per player at
    2 or 256 players
    [V], against about 300 ms (282-635 ms across runs) and 368 MB unchunked at 256 [C, from measured
    region costs];
  * the hashed multi-level grid beats both octrees on query time, build time and memory, and matches them on
    update time [V];
  * box3d-rust ran validation loops in release builds, and creating N bodies cost O(N^2); fixed in
    `box3d-fork` (e1164ff) [V].

## 25.2 Coordinates and precision

| Representation | Step at 1 km | at 100 km | at 10,000 km | at 1 ly | 1 h of walking at 10,000 km |
|---|---|---|---|---|---|
| `f32` absolute | 61 um | 7.8 mm | 1 m | 1.07e9 m | stuck (step rounds to 0 beyond 524 km) |
| `f64` absolute | 1.1e-13 m | 1.5e-11 m | 1.9e-9 m | 2 m | 38 um error |
| chunk + `f32` offset | 61 um | 61 um | 61 um | 61 um | 17 cm error |
| **chunk + `f64` offset (proposed)** | 1.1e-13 m | 1.1e-13 m | 1.1e-13 m | 1.1e-13 m | **3 nm error** |

[V `precision`; exact IEEE arithmetic, so identical on every machine.]

* **Rules (extend 4.3):** coordinate code uses only `+ - * /` by powers of two, `floor`, integer shifts and
  `f64 <-> i64` conversions. Cell keys and levels come from bit patterns (`ceil_log2` reads the exponent),
  never `log2`. Rebasing to a nearer origin is exact.
* **Where `f64` lives:** object transforms (position relative to parent; roots relative to their chunk),
  dormant records, carried state, the renderer's camera maths. **Where `f32` is enough:** inside a region's
  box3d world (positions within a few km of the region origin), rotations, velocities, render vertex data
  after camera-relative conversion.
* **Below 1e-10 m or beyond 2^73 m** a parent object acts as a nested frame: child transforms are `f64`
  relative to it, so precision never runs out; regions for such players take their origin from that
  frame.

## 25.3 Hierarchy and the spatial index

* **Cells:** level `L` cell = `1024 * 2^L` m, key `(L, i128 x3)`, `cell(L + 1) = cell(L) >> 1` exactly.
  Levels -43..+63 span from cells of about 1e-10 m to the whole coordinate range. Finer levels are
  meaningful only inside a nested frame (25.2).
* **Filing:** an object of radius `r` lives at the smallest level whose cell is at least `rho_file * r`
  wide, keyed by the cell of its centre.
* **Index (user suggestion 14:12Z, measured in 25.9):** the recommendation is a **hashed multi-level grid**
  (an implicit loose octree, where the level plays the role of tree depth). The relevance query reads the
  3x3x3 neighbourhood of each occupied level, so its cost does not depend on scale.
* **The index is a cache, not sim state.** Query results are sorted by object id, so the index's internal
  layout may differ between peers and is never snapshotted or rolled back [V: identical results after
  building in shuffled order].

## 25.4 What is simulated: relevance, support, wake and freeze

An object `o` is **active** at a relevance pass when any of these rules holds; otherwise it is **dormant**.
Passes run every `P` = 8 frames, a deterministic cadence. (Rules revised after review R16-2.)

1. **Relevant to an anchor.** For some member anchor `a` (position `p`, height `h`, predicted over the
   next pass), `r_o >= h / 64` and `|p - o| <= min(rho * r_o, reach_cap(h))`. By default
   `reach_cap` = 2 km x `h / 1.7 m`.
2. **Awake stays active.** An active body that is awake stays active until it falls asleep, or until it
   leaves its region's footprint (the hard reach; review N6). A coin knocked off a table lands; it does not
   freeze in mid-air.
3. **Woken.** An object inside the swept bounds of an awake active body no more than 16 times larger
   (`r_o >= r_body / 16`) becomes active. Things you push, throw or knock over wake what they hit.
4. **Support closure** (revised after review N7).
   * **First hop, simulated.** A dormant object whose bounds touch a body active by rule 1, 2 or 3 is
     included, however small it is. The bounds are swept by that body's velocity over the coming pass.
   * **Second hop, static proxy.** Objects touching that first hop are added as static proxies: they
     collide but are not simulated. A proxy becomes simulated when it is itself in a first hop.
   * The stones under a relevant crate are therefore simulated, and the stones under them hold them up,
     without waking the whole heap.

Measured [V], `closure` scene: a 0.9 m crate on a bed of 3 cm stones and a coin knocked off a table, with
the player walking in from 20 m.

| Rule set | Crate | Stones left inside the crate | Coin | Bodies active (max) |
|---|---|---|---|---|
| Rule 1 + wake only (as of 45d4940) | falls 6 cm through the dormant stones to the ground | 81 | frozen in mid-air at 0.94 m | 49 |
| Rules 1-4 (one hop) | rests on the stones (settles 2 cm as they shift) | 0 | lands on the floor | 134 |

Measured [V], `closure2` scenes, each with an all-dynamic control (what physics alone does):

| Scene | All-dynamic control | One hop, static bounds (2a228c6) | Swept first hop + static second hop (revised) |
|---|---|---|---|
| 20 cm crate set on a 3-layer heap of 5 cm gravel: crate height (rests at 0.251) | 0.242, no gravel displaced | **0.197**: falls a layer; 68 gravel cubes sink | **0.243**, none sink; 124 static proxies |
| The same crate thrown at 2 m/s onto 1.2 cm gravel (below r/16): final height (rests at 0.112) | 0.106, 16 gravel inside the crate | 0.105, 2 inside | 0.106, 21 inside |

* **The heap shows the second hop matters.** Without it, the first hop's gravel falls into the space where
  the dormant layer below should be.
* **The thrown case is decided by the solver, not by the rule.** Gravel under 1/16 of the crate's size has
  under 1/4,096 of its mass. Box3D lets the crate sink into it even when everything is simulated, and all
  three variants end within 1 mm of each other.
* A first attempt with a 0.9 m crate on 2.5 cm gravel (a mass ratio of 47,000) sank to the ground even in the
  control, so it was replaced.

* **Dormant means frozen exactly.** Position, rotation and velocities are kept bit for bit in a dormant
  record: about 100 bytes, against about 1.9 KB per body in a box3d image [V]. Time does not pass for
  dormant objects. When something becomes active again it continues from that exact state. That state is
  canonical, because every peer froze it at the same frame by the same rules.
* **Logic catch-up.** Logic modules get `on_wake(chunk, frames_elapsed)`, a deterministic closed-form hook
  (crops grow, timers expire). Physics is never fast-forwarded.
* **Unmodified content is not stored at all.** World content comes from the SCM (committed chunk files) or
  from deterministic procedural generation per `(generator hash, cell)`. Only objects that changed carry
  a dormant record.
* **Why size-based.** With OWO-style whole-chunk activation, everything within 1 km is simulated: about
  2.5e7 objects in the streaming test's content [C]. With relevance it is 330-640 bodies, of which 1-80 are
  awake [V].

## 25.5 Regions and groups (who simulates what) [I; the prototype measures costs and a model of the dependency rule, not the protocol]

For large worlds, this replaces "every peer simulates the whole world" (2, 6). With one region it reduces
to today's design. Revised after review R16-1 and R16-3: the world log is no longer on any region's
per-frame path.

### 25.5.1 Two tiers of log

| | World log | Region log (one per region) |
|---|---|---|
| Held and acked by | every member | the region's group |
| Carries | session membership, ACL, SCM commits, frozen-chunk record hashes, a region directory (informational), cross-region messages | perturbations of objects in the region (touches, hand and body streams, component writes); the members' **anchors**; horizons; check hashes; `Synced` declarations; `NeighbourRef` entries |
| On a region's per-frame path? | **No.** Its facts enter a region as relayed confirmed facts at the relay entry's frame, as in R14. A world-log stall delays ACL and SCM effects, never simulation. | yes, for that region only |
| Confirmation | every member, compact acks, slow cadence | the region's synced group members |

Both logs follow the round-2 protocol (design 10.8, @engine-architect): acks, votes, horizons and check
hashes are entries in each member's hash chain. Each log has its own chains, stability, confirmed frame,
epoch and ack cadence.
* A log depends on another log only through a confirmed fact of that log, referenced explicitly. No log
  reads another log's unconfirmed state.
* There is one chain per member per log. An eviction from a region propagates to the world log as a
  world-log view change.

### 25.5.2 Anchors, footprints and the neighbour bound

Revised after review N1, N2 and N8. Footprints change in steps, so outsiders must never see one grow
faster than `v_max`.

* **Anchor and home region.** Each member has an anchor: the position and scale that drive activation (its
  avatar root). Each member belongs to exactly one region's group, its **home region**. Its anchor entries
  live in that region's log:
  * a coarse `Anchor{chunk, scale_exp}` when it nears a chunk boundary (with hysteresis);
  * the fine quantized position, at 4-10 Hz while it moves.
* **`Lambda` = 30 frames.** That is above the region log's measured confirmation lag of 8.5 frames (at most
  9-10 frames at 256 kbit/s; ed347ac 6.4).
  * If an anchor is still unconfirmed when it takes effect (for example during a stall), its own region
    simulates speculatively and rolls back if needed, as for any entry.
  * Other regions only ever use confirmed data (below).
* **Footprint.** The level-0 chunks within `ceil(reach_cap / 1024) + 1` of the anchor chunk, at the member's
  scale band, so a radius of at least 3 chunks at human scale. A region's footprint is the union over its
  members.
* **Speed bound and the growth clock** (review N1). A footprint may *grow* toward any point by at most
  `v_max` (default 100 m/s at human scale, 25.6) over time.
  * Every change that grows a region's footprint takes effect at
    `max(stamp + Lambda, previous growth's effect frame) + ceil(60 J / v_max)`. Here `J` is how far the
    change extends the union of the current footprint and all pending growth.
  * The growth clock is per region and runs in log order, so growths queue rather than add up.
  * It covers every kind of step:
    * a chunk crossing (`J` = 1,024 m: about 10 s, which the footprint's one-chunk margin covers);
    * a scale-up (+14 km for x8: a 143 s ramp);
    * a joiner, warp or respawn admitted at the edge;
    * a travel declaration.
  * Shrinking is immediate: it only widens gaps.
* **Envelope.** A region `q`'s envelope, as another region `R` sees it, is computed from every anchor
  *stamped* up to the confirmed frame `R` holds for `q`, including growth not yet in effect.
  * By the growth clock, `q`'s true footprint at frame `f` lies within that envelope grown by
    `v_max (f - known_conf_q)`.
* **Neighbour bound.** `R` may simulate frame `f` only while, for every other region `q`,
  `known_conf_q + (envelope gap - D_merge) / v_max >= f`.
  * `R`'s members pull `q`'s confirmed prefix, with its proof (25.5.8), from `q`'s members. They pull every
    60 s, and on demand whenever the bound comes within 2 s of `R`'s present. They record it in `R`'s log as
    `NeighbourRef{q, frame, head, envelope}`.
  * **A region gated by the bound keeps publishing horizons that advance with its present** (review N2).
    Its own confirmation is unaffected, so a stall does not propagate along a chain of close regions.
* **Region creation** (a first member, or a warp into empty space) is a world-log fact.
  * `RegionCreated{region, footprint}` takes effect at `stamp + Lambda`.
  * A region may be created only if its footprint is at least `D_merge + v_max * S_create` from every
    envelope in the world log's confirmed region directory, grown by `v_max` x the directory entry's age.
    `S_create` defaults to 10 s.
  * Nearer than that, the newcomer **joins** the nearest region instead, as a per-region view change
    (25.5.3).
  * Every region simulates `f` only while it holds the world log's region directory confirmed through
    `f - S_create`.
  * A world-log stall shorter than `S_create` minus the world-log lag (about 9 s) therefore never stalls a
    region. This is the world log's only per-frame role, and it is that weak.
* **`D_merge`** (default 3 km at human scale) is the smallest allowed gap between two regions' footprints.
  It is not "two footprint radii": a footprint radius is at least 3 chunks.

**Safety, modelled [C]** (`pe-spike-chunks safety`, `spikes/out/chunks/safety.log`). The model uses two
regions on a line. It counts **late merges**: merge frames that `R` had already simulated past when it first
learned the event causing them. That needs a rollback, possibly below `R`'s confirmed frame, which 6.4
forbids.

| Case | 2a228c6 rules | Revised rules |
|---|---|---|
| Chunk step toward R during a stall in q (4 km gap) | **late** (merge 360, R at 434) | safe (merge at 975) |
| Chunk step, no stall | safe | safe |
| Scale x8, R pulls every 60 s only (10 km gap) | **late** (merge 1,030, R at 3,599) | safe (merge at 9,632) |
| Scale x8, pull every 60 s or on demand | safe | safe |
| Joiner at q's near edge, pull every 60 s only (5 km gap) | **late** | safe |
| Joiner, pull every 60 s or on demand | safe | safe |
| Walker at `v_max` toward R over 12 chunks, q stalls on the way | safe | safe (merge 615 frames later) |
| Region created 4.5 km from R, then walks toward it | **late** (R never learns of it) | safe |
| Region created 3.5 km from R | **late** | refused: joins R |
| Created 4.5 km away, world log stalled 5 s / 12 s | **late** / **late** | safe / safe (R loses 185 frames in the 12 s case) |

* **0 late merges in 11 cases under the revised rules, against 7 under 2a228c6.**
* The price:
  * footprint growth lags by up to `J / v_max`, so a walker's merge with a neighbour happens about 10 s
    later, and a large scale-up ramps over minutes;
  * a world-log stall longer than about 9 s stalls regions.

**Liveness, modelled [C]** (`liveness`, 8.5-frame region lag). Setup: 64 or 256 members, alone or in groups
of 8, with one member's acks stalled 0.5-10 s. A one-member region evicts its silent member by a
**world-log** view change, taking 3.8 s (review N3); larger regions do it in-region in 2.2 s.

| Rule | Unrelated regions that lost frames | Worst loss per region |
|---|---|---|
| World log on the path (45d4940) | **all** (63 of 63, 255 of 255, 31 of 31) for stalls of 1 s or more | 5 / 65 / 77 / 77 frames for 1 / 2 / 3 / 10 s stalls in groups of 8; 5 / 65 / 125 / 173 for members alone |
| Neighbour bound, footprints 3.5-4 km apart | **none**, even when the member is gone for good | 0 |
| Neighbour bound, footprints 3.2 km apart (2 s of slack) | the 2 nearest | up to 23 frames (groups of 8), up to 119 frames (one-member regions, world-log eviction) |

A stalled member slows only its own region, and neighbours whose slack is shorter than the time to evict
it. The gaps in this table are between *footprints*; the anchors are about 6 km further apart.

### 25.5.3 Joining, leaving and confirmation

* **Joining is a per-region view change.**
  * A member counts for `R`'s stability and confirmation only from the admission frame after its
    `Synced{R, frame}` entry.
  * Before that it is **pending**: it receives `R`'s log, and its entries into `R` stamped before admission
    are void. This rule is chain-local, so every holder applies it identically.
  * A late joiner, a warp or a respawn joins `R` directly; nothing waits on the world log.
* **To sync, a joiner needs `R`'s state at a confirmed frame.** It gets it in one of two ways:
  * a **region state transfer** from any synced member: box3d image + tables + logic sync state,
    compressed, about 0.9 MB per 1,000 bodies with lz4 [V], verified against the check hash the group
    published in `R`'s log;
  * if nobody has simulated `R`'s chunks since they froze, it builds `R` itself from dormant records:
    procedural content, SCM content, and frozen-chunk records fetched by their hash.
* **Confirmation of region frame `f`.** Every synced member of `Group(R, f)` has published a horizon of at
  least `f` in `R`'s log, and holds every `R` entry up to `f`. Members outside the group never affect it.
* **Leaving mirrors joining.** The cut is the prefix all remaining group members hold, effective after the
  leaver's last valid horizon. It never touches confirmed frames.
* **A region whose whole group goes silent** (review N3). This is the common case for a one-member region:
  a spread-out player whose connection drops.
  * Nobody inside can vote it out, so eviction is a **world-log view change**, by a strict majority of the
    session (10.8). The modelled latency is about 3.8 s.
  * Neighbours keep simulating as long as their slack lasts. With footprints at least 3.5 km apart they
    lost no frames in the model, even when the member never came back (25.5.2).
  * **Durability.** Every region publishes a checkpoint every 30 s (configurable): a `Frozen{region,
    frame, chunks, record_hash}` entry in the world log for its modified chunks.
  * The record bytes go to at least two custodians: members of the nearest other regions, chosen
    deterministically from the world log. When a region's whole group is evicted, its chunks revert to the
    last checkpoint confirmed in the world log; changes since then are lost.
  * A one-member region therefore risks up to 30 s of changes to its own chunks if its only player vanishes.
    A group of two or more keeps every confirmed frame.
* **Membership-dependent computations are evaluated at the entry's frame and frozen at confirmation**
  (the NEW-2 lesson, review R16-3). Every quantity that depends on who is in the group uses
  `Group(R, f_entry)`, the group at the frame of the entry or decision it concerns:
  * the stability set;
  * vote and eviction thresholds;
  * any median or late-report rule that survives round 2.

  A membership change takes effect only at a frame after `R`'s confirmed frame, and never re-evaluates a
  confirmed entry. **Required protocol test:** a member leaves `R` while one of its entries is still
  unconfirmed, and a member joins while a vote is open. Every member must reach the same verdicts, and
  no confirmed frame may change.

### 25.5.4 Merge and split

* **Merge.**
  * `R` and `q` merge at the first frame `f_m` at which their footprints, computed from both regions'
    confirmed anchors, come within `D_merge`.
  * As they approach, `slack(R, q)` shrinks, and the neighbour bound makes each region wait for the
    other's confirmed state. By `f_m`, both hold both histories through `f_m - Lambda`, enough to know both
    footprints at `f_m`, so both compute the same `f_m`.
  * Members prefetch the other region's state and log during the approach, simulate it as an extra region
    up to `f_m`, and post `Synced` for it.
  * At `f_m` the box3d worlds merge deterministically. Bodies of the higher-id region are recreated in the
    other world in ObjectId order with their exact state: 0.7-0.9 us per body with the validation fix,
    57-61 us without [V].
  * The merge is **deterministic but not physics-neutral** (review R16-4): recreating a body drops its
    contact manifolds, warm-start impulses and island state. The footprint margin keeps the seam in
    dormant chunks, so the bodies recreated at a merge are mostly asleep, far from any player.
  * A member not yet synced to both parts is pending in the merged region. Only that member's liveness
    suffers, never correctness.
* **Split** (revised after review N2).
  * A region splits only when its footprint has fallen into parts at least
    `D_split = D_merge + v_max x 10 s` apart (4 km at human scale), and has stayed so for `T_split`
    (default 10 s). It then splits at a deterministic frame.
  * The 1 km of hysteresis between `D_merge` and `D_split` means a split region is never immediately
    re-merged. Each new region starts with at least 10 s of slack toward the other.
  * Bodies are partitioned by chunk. Every member already holds the whole region, so nothing is
    transferred.
* **Rollback across a merge or split.**
  * It restores the pre-event keyframes of every part, re-simulates them, and redoes the event at the same
    frame.
  * Under D50 this needs no new rule. Each part keeps its own base until the event frame is confirmed, so
    a keyframe at or before the event always exists.
  * During the transition a peer holds both bases plus the merged window: about base(A) + base(B) +
    4 merged images.
* **The D50 memory/CPU knob applies per box3d world**, so per region. A peer's budget is the sum.

### 25.5.5 Perturbations across region boundaries

* **Regions never touch.** Active bodies never leave their region's footprint (25.4 rule 2), and two
  footprints stay at least `D_merge` apart, so physics never crosses a region boundary.
* **Routing.** An entry targets objects by ObjectId and goes to the region holding the object at the
  entry's frame. A member may perturb only regions it is synced in; anything else is refused identically
  on every peer.
* **Bodies leaving a footprint.** A body thrown out of its region's footprint freezes at the edge, and
  continues when its chunk becomes active again. The footprint's one-chunk margin beyond `reach_cap` makes
  this rare in view of a player.
* **Carried state** (7.3) stays per object, now with `f64` positions: 64 bytes instead of 52.
* **Check hashes and divergence repair** (9) are per region. Repair is a region state transfer.

### 25.5.6 What this costs and saves (measured in 25.10)

* **CPU per peer** scales with the bodies in the peer's own regions, not with the session size.
* **Memory per peer:** keyframes for its regions only (the D50 slider applies to the sum).
* **Bandwidth per peer:** region traffic scales with the group size. World-log traffic grows with the
  member count (membership, ACL, SCM and acks; anchors moved to region logs), and needs compact or gossiped
  acks past about 64 members.
* **Liveness:** a stalled member slows only its own region and neighbours whose slack is shorter than the
  time to evict it [C, 25.5.2]. The world log matters only through the region directory, and only for stalls
  longer than about 9 s.
* **Clustered players gain nothing:** when everyone stands in one place, it is one region and the cost is
  today's.

## 25.5.7 Persistence, source control and procedural content [I]

* **Committed content is stored per chunk:** `world/chunks/L<level>/<x>_<y>_<z>.chunk` in the world
  repository (15), content-addressed like every asset (16.6). An edit is a commit touching only the
  chunks it changed, so two builders working in different chunks never conflict. Streaming a chunk in is
  a content-addressed fetch (16.7), done before its activation frame.
* **Procedural content** is a pure function of `(generator module hash, cell)`, run by the
  `on_populate(cell)` hook at activation. Only the seed and the generator travel. Changing the generator
  is a commit; unmodified cells regenerate at its activation frame.
* **Runtime changes** (moved, created or destroyed objects) are session state, not commits: dormant
  records keyed by chunk, persisted with the session and verified by the record hashes in the world log.
  A builder can commit them into the chunk files (OWO's "compile").
* **The world log carries `Frozen{chunk, frame, record_hash}`** when a region freezes a modified chunk, so a
  later visitor can fetch the record from anyone and verify it.

## 25.5.8 Cross-region proofs and equivocation (review N4) [I]

* **What a `NeighbourRef` proves.** `NeighbourRef{q, frame, head, envelope}` carries `q`'s chain heads at
  `frame` and the stable chain entries that confirm them, signed by every member of `Group(q, frame)`.
  * That group is itself read from the referenced prefix: joins and leaves are chain entries (25.5.3).
  * The receiver checks the signatures against identity keys from the world log's membership.
  * A one-member `q`'s proof is that one member's signature. Nothing stronger exists without a second
    witness.
* **Detecting a region that shows different neighbours different histories.** Every region posts
  `RegionHead{region, frame, head digest, envelope}` to the world log every 5 s.
  * A `NeighbourRef` whose head conflicts with a `RegionHead` for the same `(region, frame)` is a
    transferable equivocation proof (two signed heads). It evicts the signers, as in 9.3.
  * So equivocation is detected within about 5 s plus the world-log lag. The world log stays off the
    per-frame path: regions do not wait for `RegionHead`; it only audits.
* **An object two regions both claim.** This happens only after an undetected equivocation.
  * The world log orders the conflicting claims, and the region whose `RegionHead` covering the chunk comes
    first in world-log order owns it.
  * The other region repairs forward, never below its confirmed frame. At its next frame after it holds the
    proof, it replaces its objects in those chunks with the owner's state, by a chunk-scoped state transfer,
    as in divergence repair (9.3).
  * Every peer reads the same world-log order, so the repair is deterministic.
* **MVP:** sessions are trusted (D21), so this machinery only bounds the damage of a malicious member. It
  is a phase-6 item, like the rest of public-session hardening.

## 25.6 Arbitrary scale: per-region length units (measured in 25.8)

* A region simulates in units of `u = 2^k` m, chosen from its members' scales (the smallest, quantized,
  with hysteresis). Box3D sees every length divided by `u`, so its fixed tolerances (5 mm slop, 2 cm
  speculative distance, 0.05 m/s sleep velocity, AABB margins) become relative to the region's scale. That
  is the per-world length unit of ruling 14:20Z, without calling the process-global
  `set_length_units_per_meter` (4.3 rule 10), so regions at different scales run side by side in one
  process.
* **Gravity:** "scale-invariant" (10 units/s^2) is the default. It makes a scaled player's world behave
  like a normal one, and keeps 60 Hz valid. Real gravity (10 m/s^2) at scale `s` is the same system run
  `1/sqrt(s)` times faster. It works, but costs sub-steps in proportion: 10x at 1 cm, 32x at 1 mm [V
  stability, C cost]. Simply adding sub-steps in scaled units does not work [V].
* **Mixed scales in one place** (revised after review N5).
  * Players whose scales differ by less than about 2^6 share one region at the smaller unit.
  * Players further apart in scale, whose footprints overlap, form **one multi-band region**: one log, one
    group, and one box3d world per band. Each frame the bands step from the largest to the smallest.
  * The larger band's bodies appear in the smaller band's world as kinematic proxies **of the same frame**.
    The smaller band's bodies do not exist in the larger.
  * So the coupling is inside one region and one log, and there is no per-frame dependency between regions.
    The cost: members of either band simulate both worlds, and depend on each other's acks, as any group
    does.
  * The mass ratio across bands is at least 2^18, so one-way coupling loses less than 4e-6 of the momentum
    [I].
  * The alternative, separate regions with the small band reading the large band's confirmed state about 10
    frames late, keeps the logs apart but shows a giant's foot about 0.17 s late to small objects.
* **Per-band constants** (review N5). `D_merge`, `D_split`, `v_max` and `S_create` scale with the band's
  unit, as footprints and `reach_cap` already do. At human scale they are 3 km, 4 km, 100 m/s and 10 s.
  * A x100 giant's band (`u` = 128 m) gets about 380 km, 512 km and 12.8 km/s. The times stay the same.
  * Between regions of different bands, the neighbour bound uses the larger band's constants.
* **Levels and ranges follow scale:** a member's footprint, `reach_cap` and `min_r` scale with its height.
  A giant does not simulate pebbles, and an ant does not simulate mountains 50 km away.
* **Consistency with the van der Waals work:** its contact-recycling radius scales per pair, which is also
  unit-free. In region units it gets the same relative sizes at every scale.

## 25.7 OWO concepts: what carries over

| OWO | Primordial | Why |
|---|---|---|
| Floating origin, per user | Camera-relative rendering + deterministic region origins | A shared bit-exact sim cannot let each user see a different frame |
| Compound coordinate: `long3` chunk + scale factor + `float3` | `i64` chunk + `f64` offset; scale through levels and region units | Same idea, more precision, no scale field in positions |
| Chunk layers `32 * 2^n` m, objects in chunks about 8 times their size | Levels `1024 * 2^L` m, objects filed by relevance distance | Same hierarchy; the filing ratio is now the culling ratio |
| Render bubbles per layer | Render and simulation relevance per level | OWO culls rendering; we also cull simulation |
| Transitions: every chunk repositioned, O(total chunks), keep worlds under about 1,000 chunks | None. Region rebase is O(region bodies) and rare; camera-relative rendering is O(visible) | Removes OWO's chunk-count limit |
| Scale transitions | Region length units and per-level footprints | No rescaling of the world |
| Spaces: Users / Userspace / World / Procedural | Avatars (presentation) / runtime sim objects / SCM-committed chunk files / procedural cells | — |
| Compile into world chunks | Commit chunk files to the world repository | Edits to different chunks never conflict (15) |
| Populate Chunk impulse | `on_populate(cell)` deterministic logic hook, cached by (generator hash, cell) | Content is never sent, only the seed and generator |
| Anchoring vehicles to a user's space | Nothing special: vehicles are ordinary sim objects | No per-user frames, so the Appendix B carry bugs cannot exist |
| GC of empty chunks inside transitions | Sparse maps: an unmodified chunk has no record | — |
| Int64 overflow guard | The same guard at ±2^63 chunks | — |
| Settle delays (1-30 updates) | Exact frames | Determinism |
| Host-only sweeps | Every rule runs identically on every peer | P2P |

## 25.8 Measured: f64 cost, distance and scale

Full tables, method and the load caveat are in [r16-chunks-results.md](r16-chunks-results.md) (sections 1-5).

| Question | Result |
|---|---|
| f64 vs f32 box3d step | +2 to +10% (minimum envelope of 8 interleaved rounds; 1,024 and 2,048 piles, 500-ball pit) [V] |
| Snapshot size | +1 to +2% (for example 1.98 → 2.02 MB at 1,024 bodies) [V] |
| f32 absolute coordinates | a 1.3 m/s walker drifts 383 m per hour at 100 km and cannot move beyond 524 km; a ball stops rolling at 1,000 km; a tower never sleeps at 1,000 km and leans 0.39 m at 10,000 km [V] |
| f64 absolute coordinates | precise (tower within 1e-6 m at 100,000 km), but box3d's f32 AABBs inflate the broadphase: 542 → 2,562 contacts and 2.4x the step at 10,000 km; 32,142 contacts and 23x at 100,000 km [V] |
| Region-local origin (chunk centre) | behaves and costs as at the origin at every distance, in both builds [V] |
| Translation invariance | f64 with a region-local origin reproduced a chaotic pile to 1e-12 m in all 7 placements; f32 diverged by metres for any shift, including 512 m. So f64 makes region **rebases** physics-neutral [V]. **Merges** recreate bodies and are deterministic but not neutral (25.5.4) |
| Arbitrary scale | **The design's rule:** units `2^round(log2 s)` (box3d sees 0.78-1.28 units), with a mixed-size scene (cubes 2s down to s/4, balls s/10 and s/20). Stable at every scale from 0.1 mm to 10 km: small-ball penetration 0.06-0.09% of the radius, sleeps at frame 32-34. Unscaled: unstable at 0.1-1 mm; 7% penetration at 1 cm [V, `scale-mixed`]. The earlier "same hash" result used unit = s exactly, so box3d saw the identical scene; that only checks determinism (review R16-7) |
| Coordinate drift, chunk + f64 | 3 nm per hour of walking at any distance up to 1e6 light-years [V] |

**Choice:** box3d `double-precision` **on**, with region-local origins, which are needed anyway. It costs
2-10% of step time, and buys translation-invariant rebases plus precision margin inside large regions. f32 with region origins is the cheaper alternative (D53).

## 25.9 Measured: spatial index

| Index (human-scale workload, 200,000 objects, 64 players) | Query | Memory | Update per moved object | Build |
|---|---|---|---|---|
| Octree rooted at each chunk | 59 us | 183 MB | 557 ns | 149 ms |
| Loose octree rooted at each chunk | 104 us | 321 MB | 312 ns | 121 ms |
| **Hashed multi-level grid** (BTreeMap / HashMap) | **34 / 25 us** | **77 / 79 MB** | **250 / 155 ns** | **69 / 77 ms** |

On the extreme-scale workload (radii 1 um to 1,000 km, 10,000 km apart), the grid also wins: 179 us per
query vs 236 / 312 us, 69 MB vs 131 / 201 MB. Update cost is a near tie with the loose octree. In the
reviewer's rerun, the loose octree was marginally faster on human-scale updates (453 vs 487 ns), within
noise. The tree reaches 28 levels deep; the grid has 40 occupied levels,
each reached by one hash lookup. All four return identical, brute-force-checked results, including when built
in shuffled insertion order [V]. Recommendation: the hashed grid, with `BTreeMap` storage for free ordered
iteration (D58).

## 25.10 Measured: regions and player count

* **Region-scoped rollback** (16 players, 2,016 bodies): a late entry for one player costs 0.55-0.68 ms
  instead of 8.6-13 ms for the flat world (15-18 times less). The 16 regions step in parallel in about 0.2 ms of
  wall time, against about 1 ms in series. Regions gave identical hashes stepped forward, in reverse and in
  parallel, and through rollback [V].
* **Per player, f64 with the validation fix:**

  | Players | Spread out | Groups of 8 | All in one place (= today's unchunked design, any layout) |
  |---|---|---|---|
  | 2 | 0.12 ms/tick, 1.5 MB | 1.9 ms, 3.0 MB | 1.9 ms, 3.0 MB |
  | 16 | 0.12 ms, 1.5 MB | 6.8 ms, 11.6 MB | 12.6 ms, 23 MB |
  | 64 | 0.12 ms, 1.5 MB | 6.8 ms, 11.6 MB | 67 ms, 92 MB |
  | 256 | 0.12 ms, 1.5 MB | 6.8 ms, 11.6 MB | about 300 ms [C] (282-635 ms across runs), 368 MB |

  CPU per 16.7 ms tick and memory per player [V region costs, C combination]. Every player keeps a ball pit
  busy with a hand.
* **Bandwidth per player** [C, from the measured encodings in 7.6]:
  * region traffic: 0 when spread out, 94 kB/s in a group of 8;
  * world-log traffic grows with the session: with today's ack format (one line per member), 15 kB/s at 16,
    216 kB/s at 64 and **3.3 MB/s at 256**; with compact 100 B acks, 6 / 26 / 106 kB/s.
* **Conclusion:** spread-out players are independent. The remaining O(N) cost is the world log, which needs
  compact (or gossiped) acks beyond about 64 members. Clustered players are not helped by regions: 256 engaged
  players in one place would need about 300 ms per tick [C] and 51 MB/s each [C], so a region needs a cap on engaged
  members (D57).

## 25.11 Measured: streaming

A player crossing generated content for 20 s at 1.5, 15, 150 and 1,500 m/s (f64 with the validation fix):
* 330-640 active bodies, out of about 25 million objects within 1 km;
* frame total (step plus streaming) 0.10-0.18 ms mean in my run. The reviewer's rerun on a loaded machine
  gave 0.15-0.54 ms. The p99 and worst-frame tails did **not** reproduce under load (p99 1.3-6.5 ms, worst
  4-65 ms), so they are not quoted until a quiet-machine rerun;
* up to 816 bodies created and 816 destroyed per second;
* generation 0.2-3 ms per second; relevance queries under 1 ms per second.

The f32 build had to rebase the region every 4 km (0.85
ms each); the f64 build never did. A dormant record is 101 B per body, against 0.7-2 KB in a box3d image [V].

## 25.12 Decisions for the user

Numbered after the architect's D52; to be renumbered at merge.

* **D53. Coordinates and box3d precision (25.2, 25.8).**
  * Recommend: canonical `i64` 1,024 m chunk + `f64` offset; box3d `double-precision` on; region-local
    origins. Costs 2-10% of step time [V]; makes rebases physics-neutral (merges stay deterministic) and keeps precision
    everywhere.
  * Alternative: box3d f32 with region-local origins and rebasing every few km. 2-10% cheaper, but every
    rebase or merge perturbs chaotic scenes, deterministically but visibly. This supersedes D3's "keep
    double-precision off".
* **D54. What is simulated (25.4).**
  * Recommend: size-based relevance, `|p - o| <= 32 r`, `r >= h/64`, reach cap 2 km x scale, on the memory/CPU
    slider. Awake bodies stay active until asleep; one-hop support closure. Dormant objects freeze exactly;
    logic catches up through `on_wake`; physics is never fast-forwarded.
  * Alternative: OWO-style whole-chunk activation, about 25 million objects within 1 km in the test
    content.
* **D55. Regions and groups (25.5).**
  * Recommend: the two-tier log, with anchors in region logs and the speed-bounded neighbour bound (growth
    clock, `v_max` 100 m/s, `D_merge` 3 km, `D_split` 4 km, region creation via the world log with `S_create`
    10 s). Modelled [C]: a stalled member slows only nearby regions, and there are 0 late merges across 11
    step, stall and creation cases. Build it in phase 3 together
    with region-scoped rollback (6.6), after the round-2 protocol is reviewed. Phase 2 stays one region per
    world.
  * The cost of the speed bound: travel faster than `v_max` needs a declared travel footprint or a
    portal-style transfer.
  * Alternative: keep "every peer simulates everything" (about 16 players at most in one place, measured
    above).
* **D56. Scale (25.6).**
  * Recommend: per-region length units with scale-invariant gravity by default; real gravity as a per-world
    opt-in, time-scaled at `1/sqrt(s)` CPU cost for small players.
  * Mixed scales: players more than 2^6 apart in scale get separate regions with one-way proxies [I].
* **D57. Crowding.**
  * Recommend: cap engaged members per region (default 16) by stamping the inputs of members beyond the cap a
    few frames ahead (input delay, D41), and lean on avatar LOD (R17). Clustered players cannot be made cheap
    by regions [V].
* **D58. Spatial index (25.3, 25.9).**
  * Recommend: hashed multi-level grid (`BTreeMap`), not a pointer octree. It was faster and 2.4-4 times
    smaller on both workloads [V].
* **D59. World-log acks at scale.**
  * Recommend: compact digest acks on the world log from the start, and gossip relaying past about 64 members.
    Today's per-member ack lines would cost 3.3 MB/s per player at 256 [C].

## 25.13 What this adds elsewhere

* **Crates (23.1):**
  * `pe-core`: `WorldPos`, `CellKey`, `pow2`, `ceil_log2`;
  * `pe-sim`: dormant records, transforms with `f64` position;
  * `pe-worlds` (or a new `pe-regions`): the region set, groups, merge and split, relevance passes, the
    hashed grid;
  * `pe-physics`: per-region length units, region origins, box3d `double-precision`.
* **Plan (23.2):**
  * phase 2: coordinates, `f64` transforms, box3d double precision;
  * phase 3: regions, groups, region-scoped rollback, streaming, length units;
  * phase 6: compact or gossiped world acks for hundreds of members.
* **Determinism rules (4.3):** coordinate maths uses powers of two and bit-pattern logarithms only; the
  spatial index is a cache whose results are sorted by id.
* **MCP (17):** `world.chunk.get`, `world.relevance.explain` (why an object is or is not simulated) and
  `regions.list`.
