# 25. Massive worlds: hierarchical chunks, f64 coordinates and regions (R16)

Status: DRAFT for @engine-architect (region and group model first; measurements are being filled in).
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
  offset inside the chunk. That covers ±2^73 m (about a million light-years) at 2.3e-13 m resolution
  everywhere. Transforms are `f64` position plus `f32` rotation and scale, the same split as box3d's
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
  players' scale, so box3d sees the same numbers at every scale. [V: an 8-cube tower is bit-identical from
  0.1 mm to 10 km in region units; pending numbers in 25.8.]

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
  Levels -60..+63 cover everything from about 1e-15 m cells to the whole coordinate range.
* **Filing:** an object of radius `r` lives at the smallest level whose cell is at least `rho_file * r`
  wide, keyed by the cell of its centre.
* **Index (user suggestion 14:12Z, measured in 25.9):** the recommendation is a **hashed multi-level grid**
  (an implicit loose octree). The relevance query reads the 3x3x3 neighbourhood of each occupied level.
  [pending numbers: chunk-rooted octree, chunk-rooted loose octree, hashed grid with `BTreeMap` and with
  `HashMap`.]
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
  streaming test's content that is about 2e7 objects. With relevance it is about [pending] [V].

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

Both are hash-chained per author, as the NEW-1 fix requires: horizons, late reports and `Synced`
declarations are entries, never per-recipient ack contents.

### 25.5.2 Anchors and the region set

* Each member has an **anchor**: the position and scale that drive activation (its avatar root).
* **Coarse anchor (world log):** `Anchor{member, chunk, scale_exp}`, authored when the anchor nears a
  chunk boundary (with hysteresis), about once per 10-60 s while walking. It **takes effect at
  `stamp + Lambda`**, with `Lambda` (default 60 frames, 1 s) longer than the world log's confirmation lag,
  so region topology at frame `f` is fixed by world-log entries confirmed before `f`.
* **Footprint:** the level-0 chunks within `ceil(reach_cap / 1024) + 1` of the anchor chunk, at the
  level of the member's scale band.
* **Regions** are the connected components (26-connectivity) of the union of footprints, per scale band
  (25.6). `Group(R, f)` = members whose footprint intersects region `R` at `f`.
* **Fine anchor (region log):** the anchor's exact quantized position, streamed at 4-10 Hz while moving
  (or every 0.5 m). Relevance inside the region (25.4) uses it, so it is in-sim, deterministic data.
* If the world log is not confirmed through `f - Lambda` when a region reaches `f` (a world-log outage),
  the region simulates speculatively with the last known topology and rolls back if a late anchor changes
  it, within the speculation limit (6.4).

### 25.5.3 Joining a group, and confirmation

* A member counts for `R`'s confirmation only after its `Synced{R, frame}` entry is in `R`'s log. Before
  that it is **pending**: it receives `R`'s log but its entries into `R` are refused, deterministically.
* To sync it needs `R`'s state at a confirmed frame:
  * a **region state transfer** from any synced member (box3d image + tables + logic sync state,
    compressed; about 0.9 MB per 1,000 bodies with lz4 [V]), verified against the check hash the group
    published in `R`'s log;
  * or, if nobody simulated `R`'s chunks since they froze, it builds `R` itself from dormant records:
    procedural content, SCM content, and frozen-chunk records fetched by the hash the world log holds.
* **Confirmation of region frame `f`:** every synced member of `Group(R, f)` has published a horizon of at
  least `f` in `R`'s log and holds every `R` entry up to `f`. Members outside the group never affect it.
* **Leaving:** when a member's footprint stops intersecting `R` (an anchor entry, `Lambda`-led), it leaves
  `Group(R)` at that frame and stops counting.

### 25.5.4 Merge and split

* **Merge.** Two regions whose footprints connect at frame `f_m` merge at `f_m`. `f_m` is known `Lambda`
  frames in advance, from confirmed world-log anchors. Each member prefetches the other region's state
  and log, simulates it as an extra region up to `f_m`, and posts `Synced` for it. At `f_m` the box3d
  worlds merge deterministically: bodies of the higher-id region are recreated in the other world in
  ObjectId order with their exact state (about [pending] us per body [V]). From `f_m` the merged log
  continues under the usual total order. A member not yet synced to both parts is pending in the merged
  region: liveness suffers for that member only, never correctness.
* **Split.** When the footprint union disconnects, the region splits at a deterministic frame, but only
  after the gap has persisted for `T_split` (default 10 s), so walking along a boundary does not thrash.
  Bodies are partitioned by chunk. Each member already holds the whole region, so nothing is transferred.
* **Rollback across a merge or split** restores the pre-event keyframes of every part, re-simulates them,
  and redoes the event at the same frame. Region keyframe rings therefore keep one keyframe at or before
  the event until it is confirmed.

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

### 25.5.6 What this costs and saves [pending numbers, 25.10]

* **CPU per peer** scales with the bodies in the peer's own regions, not with the session size.
* **Memory per peer:** keyframes for its regions only (the D50 slider applies to the sum).
* **Bandwidth per peer:** region traffic scales with the group size; world-log traffic grows with the
  member count (anchors plus compact acks), which needs compact or gossiped world acks past about 64
  members.
* **Clustered players gain nothing:** when everyone stands in one place, it is one region and the cost is
  today's.

## 25.6 Arbitrary scale: per-region length units [pending numbers, 25.8]

* A region simulates in units of `u = 2^k` m, chosen from its members' scales (the smallest, quantized,
  with hysteresis). Box3D sees every length divided by `u`, so its fixed tolerances (5 mm slop, 2 cm
  speculative distance, 0.05 m/s sleep velocity, AABB margins) become relative to the region's scale. That
  is the per-world length unit of ruling 14:20Z, without calling the process-global
  `set_length_units_per_meter` (4.3 rule 10), so regions at different scales run side by side in one
  process.
* **Gravity:** "scale-invariant" (10 units/s^2) is the default. It makes a scaled player's world behave
  like a normal one, and keeps 60 Hz valid. Real gravity (10 m/s^2) at small scale needs more sub-steps
  [pending numbers].
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

## 25.8 Measured: f64 cost, far from the origin, scale [pending]

## 25.9 Measured: spatial index [pending]

## 25.10 Measured: regions and player count [pending]

## 25.11 Streaming [pending]

## 25.12 Decisions for the user [pending]
