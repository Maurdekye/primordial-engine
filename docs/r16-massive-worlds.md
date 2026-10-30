# 25. Massive worlds: hierarchical chunks, f64 coordinates and regions (R16)

Status: ready to merge as section 25, numbered after the architect's 24. The region and group model (25.5)
was reviewed by @engine-architect against the round-2 protocol. The protocol is designed, not built; every
measured number is tagged.
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
* **Relevance decides what is simulated.** An object is simulated when some player is within `rho * r` of
  it (`rho` = 32 by default, a slider), it is not tiny relative to that player (`r >= h / 64`, `h` = player
  height), and it is inside the reach cap. Everything else is dormant: frozen exactly, costing memory only.
* **Regions.** The simulated part of the world splits into regions, each its own box3d world with its own
  origin, length unit, keyframes, log and group of peers. A late entry rolls back one region, not the
  world. Peers simulate only the regions they are in.
* **Two-tier log.** A world log every member holds (membership, ACL, SCM, chunk-level anchors), and one
  log per region held by that region's group. Region topology is a function of the confirmed world log;
  region state is a function of dormant records plus the region log. No decision depends on local arrival
  time (the NEW-1 lesson).
* **Arbitrary scale** uses per-region length units (ruling 14:20Z): a region simulates in units of its
  players' scale, so box3d sees the same numbers at every scale [V: an 8-cube tower gives the same world hash
  from 0.1 mm to 10 km].
* **Headline measurements** (details in 25.8-25.11 and the results doc):
  * box3d `double-precision` costs 2-10% of step time and 1-2% of image size [V];
  * with a region-local origin, physics 100,000 km out behaves as at the origin [V];
  * a late entry rolls back one region: 0.7 ms instead of 10.5 ms for the 16-player world [V];
  * spread-out players cost each other nothing: 0.12 ms per tick and 1.5 MB per player at 2 or 256 players,
    against 310 ms and 368 MB unchunked at 256 [V];
  * the hashed multi-level grid beats the octrees on every measured axis [V];
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

## 25.4 What is simulated: relevance, reach cap, wake and freeze

An object `o` is **active** at a relevance pass (every `P` = 8 frames, a deterministic cadence) when any of
these holds; otherwise it is **dormant**:

1. **Relevant to an anchor:** for some member anchor `a` (position `p`, height `h`, predicted over the next
   pass), `r_o >= h / 64` and `|p - o| <= min(rho * r_o, reach_cap(h))`. `reach_cap` = 2 km x `h / 1.7 m`
   by default.
2. **Woken:** within the swept bounds of an awake active body that is no more than 16 times larger
   (`r_o >= r_body / 16`). Things you push, throw or knock over wake what they hit.
3. **Coasting** (optional, off for the MVP): it was active and is still awake, for at most `T_keep` after
   it stopped being relevant.

* **Dormant means frozen exactly:** position, rotation and velocities are kept bit for bit in a dormant
  record (about 100 bytes, against about 1.9 KB per body in a box3d image [V]). Time does not pass for
  dormant objects. When something becomes active again it continues from that exact state, which is
  canonical because every peer froze it at the same frame.
* **Logic catch-up:** logic modules get `on_wake(chunk, frames_elapsed)`, a deterministic closed-form
  hook (crops grow, timers expire). Physics is never fast-forwarded.
* **Unmodified content is not stored at all.** World content comes from the SCM (committed chunk files)
  or from deterministic procedural generation per `(generator hash, cell)`. Only objects that changed
  carry a dormant record.
* **Why size-based:** with OWO-style whole-chunk activation, everything within 1 km is simulated: in the
  streaming test's content that is about 2.5e7 objects [C]. With relevance it is 330-640 bodies, of which
  1-80 are awake [V].

## 25.5 Regions and groups (who simulates what) [I; the prototype measures costs, not the protocol]

This replaces "every peer simulates the whole world" (2, 6) for large worlds. With one region it reduces
to today's design.

### 25.5.1 Two tiers of log

| | World log | Region log (one per region) |
|---|---|---|
| Held and acked by | every member | the region's group |
| Carries | membership and view changes, ACL, SCM commits, **anchor-chunk declarations**, frozen-chunk record hashes, cross-region messages | perturbations of objects in the region (touches, hand and body streams, component writes), fine anchor positions, horizons, check hashes, `Synced` declarations |
| Rate | about 1 entry per player per 10-60 s plus commits | as today's session log, per region |
| Confirmation | every member (compact acks, slow cadence: every 0.5 s) | the region's synced group members |

Both follow the round-2 protocol (@engine-architect, branch `round2-fixes`): acks, votes, horizons and
check hashes are entries in each member's hash chain, and each log has its own chains, stability,
confirmed frame, epoch and ack cadence. Two rules from it shape this section:
* **A decision in one log may depend on another log only through a confirmed fact of that log,**
  referenced explicitly (like R14's relays). There is no reading of another log's unconfirmed state.
* **One chain per member per log,** so an equivocation proof is per chain. An eviction from a region
  propagates to the world log as a world-log view change.

### 25.5.2 Anchors and the region set

* Each member has an **anchor**: the position and scale that drive activation (its avatar root).
* **Coarse anchor (world log):** `Anchor{member, chunk, scale_exp}`, authored when the anchor nears a
  chunk boundary (with hysteresis), about once per 10-60 s while walking. It **takes effect at
  `stamp + Lambda`**. `Lambda` is derived from the measured world-log confirmation lag plus a margin: with
  0.5 s compact acks that lag is about 2 ack rounds plus latency, 1-1.2 s [I, @engine-architect], so the
  default is 2 s (120 frames). Region topology at frame `f` is therefore a function of the world log's
  confirmed prefix.
* **Footprint:** the level-0 chunks within `ceil(reach_cap / 1024) + 1` of the anchor chunk, at the
  level of the member's scale band.
* **Regions** are the connected components (26-connectivity) of the union of footprints, per scale band
  (25.6). `Group(R, f)` = members whose footprint intersects region `R` at `f`.
* **Fine anchor (region log):** the anchor's exact quantized position, streamed at 4-10 Hz while moving
  (or every 0.5 m). Relevance inside the region (25.4) uses it, so it is in-sim, deterministic data.
* **Cross-log rule.** A region member simulates frame `f` only when it holds the world log confirmed
  through `f - Lambda`. During a world-log outage the region therefore stalls, like at the speculation
  limit, rather than speculating on unconfirmed world-log state. Each region log carries a periodic
  `WorldRef{world_confirmed_frame, prefix_hash}` entry, so a member that read a different world-log
  prefix is detected like any other divergence.
* The footprint margin (one chunk beyond `reach_cap`) and the fine anchor's look-ahead cover travel
  during `Lambda`. A member faster than about 500 m/s (one chunk per 2 s) declares anchors further
  ahead along its velocity.

### 25.5.3 Joining a group, and confirmation

* Joining is a **per-region view change**. A member counts for `R`'s stability and confirmation only from
  the admission frame after its `Synced{R, frame}` entry. Before that it is **pending**: it receives `R`'s
  log, and its entries into `R` stamped before admission are void, a chain-local rule every holder
  applies identically.
* To sync it needs `R`'s state at a confirmed frame:
  * a **region state transfer** from any synced member (box3d image + tables + logic sync state,
    compressed; about 0.9 MB per 1,000 bodies with lz4 [V]), verified against the check hash the group
    published in `R`'s log;
  * or, if nobody simulated `R`'s chunks since they froze, it builds `R` itself from dormant records:
    procedural content, SCM content, and frozen-chunk records fetched by the hash the world log holds.
* **Confirmation of region frame `f`:** every synced member of `Group(R, f)` has published a horizon of at
  least `f` in `R`'s log and holds every `R` entry up to `f`. Members outside the group never affect it.
* **Leaving** mirrors joining: when a member's footprint stops intersecting `R` (an anchor entry,
  `Lambda`-led), the cut is the prefix all remaining group members hold, effective after the leaver's
  last valid horizon. It never touches confirmed frames.

### 25.5.4 Merge and split

* **Merge.** Two regions whose footprints connect at frame `f_m` merge at `f_m`. `f_m` is known `Lambda`
  frames in advance, from confirmed world-log anchors. Each member prefetches the other region's state
  and log, simulates it as an extra region up to `f_m`, and posts `Synced` for it. At `f_m` the box3d
  worlds merge deterministically: bodies of the higher-id region are recreated in the other world in
  ObjectId order with their exact state (0.7-0.9 us per body with the validation fix, 57-61 us without [V]). From `f_m` the merged log
  continues under the usual total order. A member not yet synced to both parts is pending in the merged
  region: liveness suffers for that member only, never correctness.
* **Split.** When the footprint union disconnects, the region splits at a deterministic frame, but only
  after the gap has persisted for `T_split` (default 10 s), so walking along a boundary does not thrash.
  Bodies are partitioned by chunk. Each member already holds the whole region, so nothing is transferred.
* **Rollback across a merge or split** restores the pre-event keyframes of every part, re-simulates them,
  and redoes the event at the same frame. This needs no new rule under D50: each part keeps its own base
  (the newest image at or before its confirmed frame) until the event frame is confirmed, so a keyframe at
  or before the event always exists. During the transition a peer holds both parts' bases plus the
  merged world's window, about base(A) + base(B) + 4 merged images at the default setting.
* **The D50 memory/CPU knob applies per box3d world**, so per region; a peer's budget is the sum.

### 25.5.5 Perturbations across region boundaries

* Regions never touch: by construction there is at least one dormant chunk between two regions of the same
  band. Physics therefore never crosses a region boundary.
* An entry targets objects by ObjectId and is routed to the region holding the object at the entry's
  effective frame (topology is canonical, 25.5.2). A member may perturb only regions it is synced in;
  anything else is refused identically on every peer.
* A body leaving its region's footprint (thrown far) freezes where it crosses into dormant chunks
  (25.4 rule 3 can delay that). It continues when its chunk becomes active again.
* Carried state (7.3) stays per object, now with `f64` positions: 64 bytes instead of 52.
* Check hashes and divergence repair (9) are per region; repair is a region state transfer.

### 25.5.6 What this costs and saves (measured in 25.10)

* **CPU per peer** scales with the bodies in the peer's own regions, not with the session size.
* **Memory per peer:** keyframes for its regions only (the D50 slider applies to the sum).
* **Bandwidth per peer:** region traffic scales with the group size; world-log traffic grows with the
  member count (anchors plus compact acks), which needs compact or gossiped world acks past about 64
  members.
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
* **Mixed scales in one place:** players whose scales differ by less than about 2^6 share one region at the
  smaller unit. Further apart, they get separate regions in different bands. The larger band's bodies
  appear in the smaller band's world as kinematic proxies, and the smaller band's bodies do not exist in
  the larger. The mass ratio across bands is at least 2^18, so one-way coupling loses less than 4e-6 of
  the momentum [I].
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
| Translation invariance | f64 with a region-local origin reproduced a chaotic pile to 1e-12 m in all 7 placements; f32 diverged by metres for any shift, including 512 m. So f64 makes region rebases and merges physics-neutral [V] |
| Arbitrary scale | per-region length units: the same world hash from 0.1 mm to 10 km. Unscaled box3d: the tower hovers at 0.1-1 mm and collapses at 1 cm [V] |
| Coordinate drift, chunk + f64 | 3 nm per hour of walking at any distance up to 1e6 light-years [V] |

**Choice:** box3d `double-precision` **on**, with region-local origins, which are needed anyway. It costs
2-10% of step time and buys translation-invariant rebases and merges plus precision margin inside large
regions. f32 with region origins is the cheaper alternative (D53).

## 25.9 Measured: spatial index

| Index (human-scale workload, 200,000 objects, 64 players) | Query | Memory | Update per moved object | Build |
|---|---|---|---|---|
| Octree rooted at each chunk | 59 us | 183 MB | 557 ns | 149 ms |
| Loose octree rooted at each chunk | 104 us | 321 MB | 312 ns | 121 ms |
| **Hashed multi-level grid** (BTreeMap / HashMap) | **34 / 25 us** | **77 / 79 MB** | **250 / 155 ns** | **69 / 77 ms** |

On the extreme-scale workload (radii 1 um to 1,000 km, 10,000 km apart), the grid also wins: 179 us per
query vs 236 / 312 us, 69 MB vs 131 / 201 MB. The tree reaches 28 levels deep; the grid has 40 occupied levels,
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
  | 256 | 0.12 ms, 1.5 MB | 6.8 ms, 11.6 MB | 310 ms, 368 MB |

  CPU per 16.7 ms tick and memory per player [V region costs, C combination]. Every player keeps a ball pit
  busy with a hand.
* **Bandwidth per player** [C, from the measured encodings in 7.6]:
  * region traffic: 0 when spread out, 94 kB/s in a group of 8;
  * world-log traffic grows with the session: with today's ack format (one line per member), 15 kB/s at 16,
    216 kB/s at 64 and **3.3 MB/s at 256**; with compact 100 B acks, 6 / 26 / 106 kB/s.
* **Conclusion:** spread-out players are independent. The remaining O(N) cost is the world log, which needs
  compact (or gossiped) acks beyond about 64 members. Clustered players are not helped by regions: 256 engaged
  players in one place would need 310 ms per tick and 51 MB/s each, so a region needs a cap on engaged
  members (D57).

## 25.11 Measured: streaming

A player crossing generated content for 20 s at 1.5, 15, 150 and 1,500 m/s (f64 with the validation fix):
* 330-640 active bodies, out of about 25 million objects within 1 km;
* frame total (step plus streaming) 0.10-0.18 ms mean, p99 under 1 ms, worst frame 5.5 ms at 1,500 m/s;
* up to 816 bodies created and 816 destroyed per second;
* generation 0.2-3 ms per second; relevance queries under 1 ms per second.

Without the validation fix, the worst frame is 8.3 ms. The f32 build had to rebase the region every 4 km (0.85
ms each); the f64 build never did. A dormant record is 101 B per body, against 0.7-2 KB in a box3d image [V].

## 25.12 Decisions for the user

Numbered after the architect's D52; to be renumbered at merge.

* **D53. Coordinates and box3d precision (25.2, 25.8).**
  * Recommend: canonical `i64` 1,024 m chunk + `f64` offset; box3d `double-precision` on; region-local
    origins. Costs 2-10% of step time [V]; makes rebases and merges physics-neutral and keeps precision
    everywhere.
  * Alternative: box3d f32 with region-local origins and rebasing every few km. 2-10% cheaper, but every
    rebase or merge perturbs chaotic scenes, deterministically but visibly. This supersedes D3's "keep
    double-precision off".
* **D54. What is simulated (25.4).**
  * Recommend: size-based relevance, `|p - o| <= 32 r`, `r >= h/64`, reach cap 2 km x scale, on the memory/CPU
    slider. Dormant objects freeze exactly; logic catches up through `on_wake`; physics is never
    fast-forwarded.
  * Alternatives: OWO-style whole-chunk activation (about 25 million objects within 1 km in the test content);
    or coasting awake bodies for `T_keep` after interest leaves (smoother at the edge, more CPU, computed lazily
    by the next visitor).
* **D55. Regions and groups (25.5).**
  * Recommend: the two-tier log, with `Lambda` derived from measured world-log confirmation (about 2 s). Build it
    in phase 3 together with region-scoped rollback (6.6). Phase 2 stays one region per world.
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
