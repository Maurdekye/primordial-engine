# The world model (WS-world)

Status: **r12, 2026-11-11.** r11 is on world/core-v1 (with the first WS-world code), reviewed piecewise. r12 follows
pe-seams S2 v1 (c93fa631): the tree is addressed by nibble prefix, and `pe_seams::Node::hash` is the single definition
of every tree hash (6.2, 6.5); it adds a hierarchy depth limit (3.3, @chunks-reviewer's Q2 via @coordinator-opus
m8fe5d95c6f0d); for review of those parts. Written by @world-model. Nothing in it is built.
Reviewers: @design-reviewer (section 6, the hash tree, and section 7, the edit entries and their conflict rules);
@chunks-reviewer (everything else). Seams S1, S2 and S5 are proposals to @engine-architect until pe-seams v0 is fixed.

Sources: the re-founding plan (`docs/refound-plan.md` on `docs/refound-plan`, b610464), sections 2.1, 3.1, 3.2 and 5;
@fable-reviewer's review files 04, 11, 14 and 17 (`_teams/1847/fable-review/`); the user's decisions of 2026-11-10 in
`_shared/user-design-decisions.md`; the code of `crates/pe-core` and `crates/pe-sim` at main 926bc9d.

Marks: **[V]** verified in the code or record named; **[I]** my design judgement, open to review. **[U]** a user
ruling, cited by message id. Where this file and a user ruling disagree, the ruling wins and this file is wrong.

## 0. What this file fixes, on one page

- **The world is data.** A world is a tree of objects. Each object has a 128-bit id, a parent, ordered children and
  components. Variable-length data (names, meshes, textures, script sources) lives in content-addressed blobs,
  referenced by a 256-bit hash. The ball-pit template is a set of edits, not code.
- **Everything that matters is hashed** in one tree: objects, their components and physics state, and the world
  tables (governance, claims, the component registry, script versions, edit versions, tombstones). The root goes in
  every cut. A mismatch descends to the objects that differ, in a few rounds.
- **One edit entry, three conflict rules,** applied in the agreed order:
  1. *exclusive holds* (grabs): the first wins;
  2. *edits with a precondition* (the version the author read): refused if stale, naming the entry that won;
  3. *plain writes*: the latest in the agreed order wins, and the overwritten author is told.

  A transaction applies all or nothing. The user's rule for a delete against an add-on (m35138bafc46a) is the
  ordinary rule, live and in a merge, not a special case.
- **Ids are derived, never counted.** An object's id is a hash of the entry that created it, so it is the same on
  every machine, before and after a rollback, and in a merge, whatever the arrival order.
- **Positions cannot become numbers by accident** (the user, m06d2643578d7). `Pos` has no readable fields. The only
  way to raw numbers is `pos - Pos::ORIGIN`, which gives an `Offset`. The engine obeys the same wall as scripts. The
  chunk index never saturates: an out-of-range result is a refusal, and its encoding already reaches past 64 bits.
- **Kept, and evolved:** pe-core and pe-sim. Their existing tests stay green except where a test pins a format this
  file changes on purpose (section 10 lists each).

## 1. Scope and boundaries

**WS-world owns** `crates/pe-world`, `crates/pe-sim`, `crates/pe-core` and this file (plan 3.1).

**In M1:**
- the object tree, components and blobs (sections 3-5);
- the hash tree (section 6);
- edit and input payloads, their validity rules and the three conflict rules (sections 7-8);
- 128-bit ids (section 2);
- the coordinate format and the type wall (section 4);
- the world tables that the hash must cover in M1: governance (as storage, with a minimal permission check),
  claims, cooldowns, the registry, script versions (section 5.4);
- the world-model side of seams S1, S2 and S5 (section 9), and the data S6 (WorldApi) reads.

**Not mine,** with the owner who decides:
- the entry envelope, signatures, chains and order: WS-agree (pe-log); I define only the edit, input and governance
  *payloads*;
- what a frame steps, rollback, and binding bodies to physics: WS-state (pe-replica, pe-physics);
- governance *rules* (roles, kicks, guarded mode): WS-gov, M3. M1 stores the tables and checks one permission;
- the logic runtime: WS-logic. I store logic state as components and script sources as blobs, and hash both; compiled modules are each machine's own (3.4);
- `Pos` and `Offset` *for scripts* (typed spaces, `OffsetFrom<Parent>`): WS-logic's pe-sdk, on top of section 4;
- the repair ladder and the merge engine: WS-diverge. I supply the diff and the patch (section 6.5).

**Later in WS-world:** the world record keeping every entry from creation (M3, the user 19:09Z); saves as signed
named roots (M5); domains per contact group (the format is ready from M1, section 6.2).

**The world line tool** (the user, mcd8bb1211150; fable-review file 19;
@item:primordial-engine/world-line-visualization-tool-braided-glowing-li): any admin can take out a view of each
player's recorded actions as braided lines. So that long histories draw cheaply, each stored state of the world
record (M3) carries a **per-player summary** since the previous stored state [I, with @diverge-eng]:
- per member key: the count of entries by class, the count of edits applied and refused, and the count of objects
  touched, with a 256-bit digest of their sorted ids;
- the line id, and any branch or merge made in that interval.

pe-diverge computes it from the entries' footprints; the world record stores it beside the stored state, covered
by that state's signature. It is derived from the log, so it is not in the world root, and anyone can recompute
and check it.

## 2. Identity

### 2.1 Object ids

- `ObjectId(u128)`, world-scoped. `ObjectId::NONE` is 0 and never names an object.
- **Derived, never allocated** [I]. The id of the `k`-th object an edit entry creates is

  `ObjectId = first 16 bytes of BLAKE3("pe.object" ‖ line id ‖ author key ‖ chain id ‖ author's chain seq as u64
  LE ‖ k as u32 LE)`,

  agreed with @api-eng (its O2). The author knows its key, its chain id and the next sequence number of its own
  chain when it composes the entry, so the ids are known before ordering and before signing, and they do not depend
  on the envelope's hash format.
- **(chain id, seq) is unique for the world's whole life** (@chunks-reviewer's N1). A chain id is
  `BLAKE3("pe.chain" ‖ line id ‖ author key ‖ nonce)`, where the nonce is 32 random bytes the author picks when it
  starts a chain and puts in that chain's first entry. Two requirements on S1, for @engine-architect and WS-agree:
  - a member keeps one chain per world for the world's life where it can: a returning member (a rejoin, a new
    session, a new machine with the same key) is handed its chain head and continues it;
  - if a chain must ever start again (a member restored an older local state, or lost its head), it starts with a
    new nonce, so a new chain id. Its sequence numbers then derive fresh ids and never take an earlier object's id,
    even after that object's tombstone has expired; and an honest restart is a new chain, not an equivocation. Two entries with one sequence number (equivocation) would derive the same ids; pe-agree
  drops both as proof against their author, and a create of an id already present is refused anyway. A result of 0
  is replaced by 1 (probability 2^-128).
- **Why derived, not counted.** Today an id is `(creator, counter)` [V `crates/pe-core/src/ids.rs:50-58`], with a
  per-creator counter in the store [V `crates/pe-sim/src/store.rs:28`]. Under reordering (rollback, a merge, a
  world-line merge after a long compile) a counter hands out different ids depending on what applied first, and a
  later entry that names the object (an agent's "double the cube") then names the wrong one. A hash of the creating
  entry is fixed when the author writes the entry. The author knows the id at once, which keeps edits local-first.
- **Objects created by logic** get `BLAKE3("pe.spawn" ‖ line id ‖ spawner object id ‖ frame ‖ n)`, where `n` counts
  the spawner's *applied* spawns in that frame. Systems run in a fixed total order (script id, then system index)
  and each command names the bound object that issued it, so `n` is deterministic. A call that traps or returns a
  bad status has all its commands discarded, and its spawns do not advance `n`. Confirmed by @iter-bench (WS-logic).
  Per-player scripts (M2) never spawn as logic: their effects are their player's inputs, with ids by the edit rule.
- **The line id in every derivation** is that of the line the entry was authored on (2.3), which the signed
  envelope names. So two branches that each create objects never derive the same id, and after a merge every
  object keeps the id it was born with.
- **Objects of a preset** (the ball-pit template) are created by the edits on the preset's own line, so the first
  rule covers them, and two worlds branched from one preset share those objects by id.
- **A collision** (two derivations giving one id) is refused deterministically, like any create of an existing id.
  At 128 bits it is not expected in the life of the project; the refusal makes it safe, not silent.
- **A refused create creates nothing.** A later entry naming the id it would have had finds no object and no
  tombstone, and is refused with `Unknown(id)`. Only a tombstone orphans (7.5).
- **Ground ids, and grinding.** Derived ids are uniform for honest authors. A member could grind entry bytes so its
  ids share a prefix of d nibbles, at about 16^d work per object; that only deepens a few trie paths, and the trie
  and its diff walk are sized for the full depth of 32 (6.2).
- **A transaction names its own new objects** by a local index (`New(k)`) until it is applied; the API resolves
  `New(k)` to the derived id. So an agent can create a cube and parent a light to it in one transaction.
- **Order.** Ids sort as `u128`. Iteration in id order is deterministic and has no meaning beyond that; children
  carry their own order.
- **Carrying between worlds** (copy then remove, Q7 [U md2fa23dfa774]) creates a new object in the destination by an
  ordinary edit, so it gets an id derived on the destination's line. The source's `(world, id)` may be kept in a component for provenance;
  nothing depends on it.

### 2.2 World ids, entry ids and member ids

- **`WorldId` goes; a world is a line** (2.3). Today's `u32` [V `ids.rs:84`] is replaced by `LineId`, a 256-bit hash.
  The plan's "a world's id is the hash of its signed genesis" (plan 2.1) no longer distinguishes worlds once every
  world shares the origin's first entry, so the line id takes its place everywhere (@diverge-eng's R-W1).
- `EntryId` is pe-log's 256-bit entry hash. The world model stores it wherever it must name an edit (the versions of
  section 7.3, tombstones, refusal reasons).
- **Members.** The world model names a member by its 32-byte public key (`MemberKey`) wherever the name is stored in
  the world (hand ownership, claims, roles), because a session-local index changes when a member rejoins (file 04,
  defect 7). pe-log may keep a compact session index for the wire; it never enters world state.
- `pe_core::LOGIC` (member 0, world logic) stays as the author of logic-caused changes. In stored fields it is the
  all-zero key. Governance refuses the all-zero key as a member's key (it is a small-order point, which strict
  signature verification rejects anyway), so no member can act as world logic.

### 2.3 Lines, the origin and branching

The user (md2e515db107c): "i like the idea of there not being any real distinction between different worlds, and all
worlds can be descendant of one singular world "origin". all worlds can be merged with all other worlds, and if they
share history, that history is kept." And (mdad0d5626305): "the origin is an empty world, and there are also other
preset worlds that sprout from it with some preset content". The design, agreed with @diverge-eng [I]:

- **One tree of versions.** Every world is a line in one tree whose root is the origin. Making a world, copying it,
  forking it and starting from a preset are one operation: a **branch**.
- **The origin** is a constant built into pe-core: an empty world. Its record is one entry whose bytes are fixed for
  ever: the format tag `"pe.origin.v1"` and nothing else (no key, no signature, no content, no registry).
  `LineId::ORIGIN = BLAKE3(those bytes)`. Every engine version must produce exactly these bytes, so a test pins
  them, and changing them is forbidden.
- **A branch entry** creates a line. It is a signed entry naming its parent line and the state it branches from,
  plus the new line's settings (5.1). The new line's id is the branch entry's hash. The state is either a final cut
  of the parent (its frame and root), or a stored state signed by the brancher (its frame and root): a removed
  admin's saved copy, "as it stood up to the moment they were removed", is a local stored state and not at a final
  cut, and the merge's search for a common base must find it (@design-reviewer's r9 Low 3).
- **Who signs a branch entry** (@chunks-reviewer's r9 L4): the member who branches, with their own key. The entry
  sits outside every chain, on the parent line and on the new one, since the new line's id is its hash. Who may
  branch is governance (WS-gov): by the user's rulings, any admin, and a former admin for the saved copy they keep.
  A merged line's head names both parents (WS-diverge's merge record).
- **Entries on a line** are signed over that line's id (S1), and object, chain, spawn and hand ids derive from it
  (2.1). A merged line accepts the entries of every line in its ancestry, so both parents' signatures and ids stay
  valid after a merge, but **only up to each parent's merge point** (@diverge-eng, after @design-reviewer's review of
  divergence.md): the merge record names, for every chain of each parent line, its last included (chain id, seq).
  An entry a parent line's chain makes after that point belongs to that line alone, so an admin who keeps playing on
  a parent after the merge does not leak entries into the merged line; bringing them in needs a new merge.
- **A chain belongs to one line.** A member that continues on a branch (a saved copy reopened alone, a rejoin that
  keeps a fork) starts a new chain there, with a new nonce, so (chain id, seq) never repeats across lines.
- **Presets** are ordinary lines one branch from the origin, shipped with the engine as records. A preset can be
  re-versioned by publishing a new line; worlds made from the old one keep it in their history. Only the origin
  never changes.
- **Engine components are registered by the first branch from the origin** (a preset, or an empty world), not by
  the origin, since the origin can never change while engine components may. A newer engine that changes an engine
  component gives it a new id (3.2); a line keeps the descriptions it was made with, and an upgrade is an edit.
- **Old records stay readable for ever** (file 20, 5): every payload format carries a version tag, and a decoder
  keeps every version it ever accepted [I].
- **What a world is called** is a label on a line, which its admins set; labels need not be unique.

**Merging two worlds that share only the origin** (or a preset), the parts that touch the world model; the merge
itself is WS-diverge's (M5, "re-merging later, by choice"):

- **Placement** (the user's answer 1, accepted as recommended): the incoming line's objects are placed where the
  admins choose, by default beside the other world and clear of it. The world model makes that exact by requiring
  the shift to be a whole number of chunks per axis, and applying it to **every `Pos`-typed field** of the incoming
  objects (the schema marks them): root transforms, and also waypoints, spawn points and targets stored on any
  object (@design-reviewer's r9 Medium 2). An integer chunk shift changes no in-chunk offset, and so no bit of any
  relative position, physics frame or script result [I]. `Offset` fields and child-local transforms are relative,
  so they do not shift. `Pos` values held in the incoming side's world tables shift too: the last world transforms
  in its tombstones (7.5), so a later orphan lands where its parent really was (@chunks-reviewer's r9 L2).
- **The shift is checked first** (@chunks-reviewer's r9 L1): every shifted `Pos` is computed with `checked_add`, and
  if any would leave the range or land on the reserved `i64::MIN` (4.3), the placement is refused before anything
  applies, and the preview offers another. Objects the two sides share (a common preset's) are not shifted; they are already in the
  same place.
- **What cannot shift:** a number a script stored after `pos - Pos::ORIGIN` is a plain number, and the engine
  cannot know it was a position. That is the user's deliberate opt-in (4.2); after a placement shift it points at
  the old place. A note for WS-logic's documentation.
- **Settings** (answer 2): the line merged into keeps its settings unless the admins choose otherwise in the preview.
- **World-wide logic** (answer 4): it stays limited to the things it came with unless an admin widens it. For that,
  every object records the line it was born on, as a small index (`Node.born_on: u32`) into a hashed world table,
  `lines`, of the line ids that object births name; and each script row (5.4) holds the set of lines its world-wide
  queries cover, which only an admin's edit widens. The rule itself and its worked example are WS-logic's, before
  it is built (file 20, 7) [I].
- **Merging two `lines` tables** (@design-reviewer's r9 Low 1; @chunks-reviewer's r9 L3): the two sides may give one
  index to different lines. The line merged into keeps its indices unchanged; the incoming side's lines that it
  lacks are appended in `LineId` order; and the incoming objects' `born_on` values are remapped to the merged
  indices. It is deterministic. The cost is one row write per incoming object (shared objects already carry the
  merged-into side's index), which the placement shift touches anyway.
- **History is kept, deleted things included** (file 20, 5): the world record of each side comes along, joined at
  the common ancestor. Dropping history on purpose ("share from now, without its past") is a branch from a cut with
  the history cut off, a later feature.

**For M1:** the origin constant, branch entries for one preset (the ball pit) and one world made from it, line ids
in every derivation, and the reserved `Node.born_on` field and `lines` table. Merging unrelated worlds is M5's.

## 3. Objects and components

### 3.1 An object

| Part | What it is | Stored in |
|---|---|---|
| Id | `ObjectId` | the object table |
| Class | `World` (shared, hashed, saved); `Local` objects live elsewhere, below | the object table |
| Hierarchy | parent, ordered children | the built-in `Node` component, as today [V `crates/pe-sim/src/hierarchy.rs`] |
| Components | typed rows, zero or more instances of each type | one sorted column per type |

- **`Local` objects** are for things like a player's own UI and drawn-only helpers [I]. They are kept apart from
  the world, so shared state cannot depend on them by construction (@chunks-reviewer's F5):
  - they live in a separate per-machine store, outside every root and every snapshot. A rollback or a restore of
    the world never touches them, and nothing replays them;
  - their ids are a distinct type, `LocalId`, from a per-machine counter. An edit names objects only by `ObjectId`,
    so it cannot name a `Local` object at all;
  - a `Local` object may refer to a `World` object (a label that follows a ball); a `World` object can hold no
    `LocalId`, since no field kind stores one.
- The plan's "persistence class" also names `Session` (shared, hashed, not saved), which I add in M3 with the world
  record.
- **Several instances of one type** (plan 2.1). A row's key is `(ObjectId, instance: u16)`; instance 0 is the
  default. Columns sort by that key. M1 builds the key; most engine components allow only instance 0 [I].

### 3.2 Components

- **Kept from pe-sim:** a component is a fixed-size row of plain data with a runtime description (name, kind,
  offset, count) that covers it exactly, with no padding [V `crates/pe-sim/src/registry.rs:1-10`]. The description
  is also the schema WorldApi publishes.
- **Field kinds added** (to the twelve of `registry.rs:36-54` [V]):

  | Kind | Bytes | Meaning |
  |---|---|---|
  | `ObjectRef` | 16 | an `ObjectId`, stored as two `u64` words (`hi`, then `lo`, each little-endian), so rows stay 8-byte aligned (`u128` is 16-byte aligned on x86-64, and columns are `u64` buffers [V `column.rs:6-13`]); replaces the 8-byte `ObjectId` kind |
  | `BlobRef` | 40 | a display blob (3.4): a 32-byte blob hash and a `u64` length; all-zero is "none" |
  | `SimBlobRef` | 40 | a simulation blob (3.4), the same layout |
  | `Pos` | 48 | an exact position (section 4); only `Pos` operations read it |
  | `MemberKey` | 32 | a member's public key |
  | `EntryRef` | 32 | an `EntryId` |
  | `Hash` | 32 | any other 256-bit hash |

- **Bodies, as @vdw-physics proposed** (WS-state) [I]:
  - mass comes from each shape's density (mass over volume), computed by pe-physics in one fixed order everywhere.
    The schema shows a derived, read-only `mass_kg` per body, so people and agents still see a mass; setting it is
    done through density (@chunks-reviewer's r5 Low, a note for @api-eng's schema mapping);
  - a body's shapes are its own `Shape` plus those of its descendants that have no `Body` of their own, in id order,
    up to 64 (a compound body: a table is one body with five boxes);
  - a mesh shape is for static bodies only; a dynamic body uses primitives or a hull (`SimBlobRef`).
- **`BodyState` is derived and hash-only** (@design-reviewer's r5 Low 2). The step never reads it and no edit writes
  it; pe-replica writes it at cut frames and on export, so between cuts it is stale, and a keyframe whose
  `BodyState` is a few frames old restores correctly. No reader treats it as physics truth: WorldApi's `query`, the
  inspector and logic read poses and velocities from the replica's frame view (S5), never from `BodyState`.
- **Edit versions are not a component** (@chunks-reviewer's F6). Every column keeps, beside each row, that row's
  version (7.3): the `EntryRef` of the last edit that wrote it and the op's index, 36 bytes padded to 40, in a
  parallel array of the column's blocks. An object's structural version and its object version (7.3) are fields of
  its `Node` row. All are in the object's leaf hash (6.3).
- **No string kind, no list kind.** Variable-length data is a blob [I, from file 04 A1]. A name is a `BlobRef` to
  UTF-8 bytes. A list of objects is children, or a blob. That keeps rows fixed-size, which is what makes them cheap
  to copy, hash and roll back.
- **Engine components for M1** [I; each is one row, the exact fields are in the code and the schema:
  `crates/pe-world/src/components.rs`, reserved ids 3 to 11 after pe-sim's `Node` (1) and `Transform` (2)]:

  | Component | Holds | Who writes it |
  |---|---|---|
  | `Node` | the hierarchy | the hierarchy API only |
  | `Transform` | position (section 4.4), rotation, scale | edits; physics writes back the simulated pose of a body |
  | `Name` | `BlobRef` to UTF-8 | edits |
  | `Shape` | a primitive (box, sphere, capsule) with its sizes and density, or a mesh `SimBlobRef` (static bodies only); a plane is a large thin box | edits |
  | `Body` | static, dynamic or kinematic; friction, restitution, linear and angular damping, rolling resistance, gravity scale, bullet, may sleep; grabbable | edits |
  | `BodyState` | derived: the simulated state of a bound body (linear and angular velocity, awake flag and sleep timer) and the physics digest of section 6.3 | pe-replica, at cut frames and on export |
  | `Appearance` | base colour, emissive colour, roughness, metalness, an optional material `BlobRef` | edits |
  | `Light` | kind, colour, intensity, range | edits |
  | `Hand` | the owning `MemberKey`, side, grip, engaged | created by world logic at admission; then inputs of its owner only (section 8) |
  | `SimSize` | the simulation size of 3.5 | set at creation; edits |
  | `Protection` | the owning `MemberKey` (7.8) | edits by its owner |

- **The registry is world state** [I]. Today it is configuration shared by every peer [V `registry.rs:216-218`]. With
  a logic edit in M1 [U mc40fc6379bc6, U1], a script that declares components changes the schema at a frame. So:
  - engine components are registered by a line's first branch from the origin, under their reserved ids (2.3);
  - a script's components are registered by the edit that commits the script, at that frame;
  - the registry is a world table (section 5.4), hashed, and rolled back with everything else;
  - a component id never changes its description. A changed schema gets a new id, and a migration is an edit.
- **Component ids are derived too** (@chunks-reviewer's F7). `ComponentId` widens to 128 bits. Engine components
  keep small fixed values below 2^32. A script's component gets `BLAKE3("pe.component" ‖ script asset id ‖ name ‖
  schema hash)[..16]`. The script asset id is stable across the script's versions (@iter-bench), so a code commit
  that leaves a component's description unchanged keeps its id and its rows, and the swap needs no migration; a
  changed description gets a new id. Two commits on two branches can never give one id two meanings: the same id
  means the same description everywhere. `Register` of an id already registered with the same description is a no-op; with a
  different one (a collision) it refuses the transaction.

### 3.3 Hierarchy and transforms

- Kept: parents, ordered children, the frame rule (an object is placed relative to its nearest ancestor with a
  `Transform`), and deterministic composition [V `crates/pe-sim/src/transform.rs:1-37`].
- Kept: plain edits store what they are given; keep-world edits are explicit [V `transform.rs:11-21`]. A reparent
  edit says which it wants.
- **A cycle** (reparenting an object under its own descendant) is refused.
- **A depth limit** (@chunks-reviewer's Q2 on pe-api, via @coordinator-opus m8fe5d95c6f0d): an object may be at
  most 256 levels below its root [I; a world setting, at most 4,096]. A `Create`, `Copy`, `Reparent` or `Undelete`
  that would put any object deeper is refused with `TooDeep`. Every tree walk in pe-world, pe-sim and the API is
  iterative, never recursive, so even a corrupt state cannot overflow a stack (5.3). Lowering the setting neither
  refuses nor moves objects already deeper; later edits that do not deepen them still apply. A merge can join two
  in-limit sides into a deeper chain; its replay refuses the deepening edit deterministically, like any edit
  (@chunks-reviewer's r12 note).

### 3.4 Blobs

- **Content-addressed:** a blob's name is `BLAKE3(bytes)`; it never changes. A world refers to a blob only through a
  `BlobRef` field, so the hash tree covers each blob by its hash, and the bytes are verified on arrival.
- **Where they live:** a per-machine store keyed by hash, shared by every world on the machine. A blob is kept while
  any kept root, world record or open edit refers to it. Nothing is deleted while the world record keeps an entry
  that names it (the user, 19:09Z).
- **How they travel:** an edit that introduces a blob names its hash and length. The bytes travel alongside on the
  bulk lane (S4), or are fetched by hash from any member that has them.
- **Blobs are of two uses, with two rules** [I, after @chunks-reviewer's F1]. A component field declares which, by
  its kind: `BlobRef` for display, `SimBlobRef` for simulation. The enforcement point is the reader: pe-physics and
  pe-sdk read bytes only through `SimBlobRef` fields, and pe-sdk gives logic no way to read a display blob's bytes
  (@chunks-reviewer's N5; a note for WS-logic).
  - **Display blobs** (a mesh to draw, textures, names, materials) may be missing. The entry applies, the `BlobRef`
    is stored as given, and the hash stays exact. Only drawing waits: a mesh is drawn as its bounding box. Shared
    simulation never reads a display blob, so no two machines can compute differently because of one.
  - **Simulation blobs** (a collision hull, a script's source tree, anything shared logic or physics reads) are
    part of the entry for agreement. Small ones (up to 1 MiB, which covers hulls) travel *inside* the edit entry, so
    any machine that has the entry has the bytes. Larger ones follow the rule of a late entry: a machine that lacks
    the bytes at the entry's frame simulates as if that change had not arrived yet, which is a prediction, and
    rewinds and replays from that frame when the bytes arrive. It does not vote for a cut that includes the entry
    until it holds the bytes. Nobody waits; only that machine's view lags.
  - **A compiled module is never a blob** (@iter-bench, under Q3c [U mf7daea6446ca]: no machine runs another's
    build). The script's source is the simulation blob; each machine builds its own module from it, a local
    product that is never stored in the world, sent or fetched. Activation after each machine's own compile is
    WS-logic's, on the same late-entry path.
- So D17's "a missing artifact pauses that peer" narrows to: nothing pauses; a machine lacking a simulation blob
  lags as if behind on entries, and a machine lacking a display blob draws a stand-in.
- **Limits:** a blob is at most 64 MiB in M1; a transaction introduces at most 256 MiB [I, a guard against
  flooding, to tune].

### 3.5 Simulation size

The user (maa60620a6ced, via @coordinator-opus m78c6bd2b2bf5; file 16, 6.4): every object has a nonzero "size" used
purely for simulation distance, with no bearing on collision, and it gets a reasonable default at creation. The user
accepted the details below (m8c42b4b093bd, via @coordinator-opus m04de7b0d9a18; file 16, 6.5):

- **Every `World` object has a `SimSize` row,** in metres: positive and finite. It never affects physics, drawing or
  collision; WS-regions reads it to choose the object's layer (M4).
- **The default at creation** is 1 m times the scale of the member who made it: the world scale of that member's
  avatar root, a `World` object, at the edit's frame. So the default is a function of hashed state, the same on
  every machine (@chunks-reviewer's N4). An agent's edit takes the scale of the member it acts for. Objects made by
  world logic or at genesis take 1 m times the spawner's world scale [I]. In M1, before avatars, a member's
  avatar root is its hands' parent object, created at admission with scale 1.
- **Never below the real extent.** The size WS-regions uses is `max(SimSize, extent)`, where the extent is the
  object's world-space bounding size from its `Shape` and world scale. So an object that grows is never smaller than
  itself, and no rule needs to rewrite the stored value [I].
- **Settable above the extent, up to the world maximum** (a world config value). An edit that sets it to zero, a
  non-finite value or above the maximum is refused.
- **A floor of 2^-50 m** (`pe_core::coords::MIN_EXTENT`, about 1e-15 m; @chunks-reviewer's C2 on world/core-v1):
  a `SimSize` below it is refused, and an extent below it counts as the floor, so every size has a cell level within
  the supported range and no input can reach a level the engine cannot represent (5.3).
- **Saved objects keep their relative size** (m8c42b4b093bd). A saved object (an inventory item, M5) records its
  `SimSize` divided by the saving player's scale; spawning it sets `SimSize` to that ratio times the spawning
  player's scale. So an object saved by a giant and spawned by a mouse is, for simulation distance, the same size
  relative to each.
- It is an ordinary component: hashed, versioned (7.3), and written by plain edits.

## 4. Coordinates: `Pos`, `Offset` and the wall

### 4.1 The user's rulings

- m06d2643578d7, verbatim: "1. yes, and instead, just require them to intentionally subtract the origin position
  (which is a world constant always available) if they really want that 2. no warning or test, enforce this via the
  type system 3. is there a difference? in resonite those are the same things".
- m7e85960c864a: coordinates go past the 64-bit range without saturating (plan 5.2, R16).

### 4.2 The types, in pe-core

```rust
pub struct Pos { chunk: [i64; 3], local: [f64; 3] }   // fields private: no reader, no constructor from numbers
pub struct Offset(pub [f64; 3]);                       // a displacement, world axes, metres
impl Pos {
    pub const ORIGIN: Pos;                             // chunk 0, offset 0: the world constant
}
impl Sub<Pos> for Pos { type Output = Offset; }       // total: the chunk difference is taken in i128 (below)
impl Pos {
    pub fn checked_add(self, d: Offset) -> Result<Pos, OutOfRange>;   // no panicking Pos + Offset exists
    pub fn checked_sub(self, d: Offset) -> Result<Pos, OutOfRange>;
}
impl PartialEq for Pos;                                // exact, on canonical values (no -0.0, below)
```

- **No operator on a `Pos` can panic** (@chunks-reviewer's F3). `Pos - Pos` is total. Moving a position is
  `checked_add` or `checked_sub` and returns `Result`; there is no `Pos + Offset` operator, so no input can reach a
  panic through operator sugar.
- **`Pos - Pos`, pinned** (@chunks-reviewer's F4), per axis, as part of the determinism contract:

  ```
  dc = (a.chunk as i128) - (b.chunk as i128)        // exact; |dc| < 2^65
  dl = a.local - b.local                             // one IEEE f64 subtraction
  d  = (dc as f64) * 1024.0 + dl                     // the cast rounds only if |dc| > 2^53; * 1024 is exact
  ```

  Each step is an IEEE operation in this fixed order, so every machine gets the same bits. The result is exact in
  the chunk part while |dc| ≤ 2^53, which is every pair of positions closer than about 9 × 10^18 m; past that, its
  error is relative to the distance, which is what such code needs. It is not "rounded once" in general: `dl` and
  the final add each round.
- **No negative zero** (@chunks-reviewer's L1): every constructor and operation canonicalizes a `-0.0` offset to
  `+0.0`, so equal positions have equal bytes and the decoder of 4.5 accepts every encoding a `Pos` produces.

- **What is missing on purpose:** `Pos + Pos`, `Pos * f64`, any field access, `From<[f64; 3]>`, `Into<[f64; 3]>`,
  and `from_abs` [V today's `WorldPos::from_abs`, `coords.rs:70`, and its public fields, `coords.rs:23-28`, are
  removed].
- **The only way to numbers** is `pos - Pos::ORIGIN`, and the only way back is `Pos::ORIGIN.checked_add(offset)`.
  Both read as what they are.
- **The engine obeys the same wall** [I]. Physics, rendering and regions all need numbers in some frame, and each gets
  them by subtracting a `Pos` it chose as that frame's origin: physics a region origin (today's `to_region`
  [V `coords.rs:154-162`] becomes `pos - region_origin`), the renderer the camera. There is no `pub(crate)` escape
  hatch: pe-core's own modules use the same operators. So a lossy use is visible as `- Pos::ORIGIN` in review.
- **Kept, because integers are exact** [I; file 17, 3.5, not withdrawn by 7.1]:
  - `pos.cell(level) -> CellKey`: exact integer cells for regions, procedural generation and grids. A `CellKey` is
    not a position and has no `Offset` arithmetic;
  - `pos.encode()` and `Pos::decode(bytes)`: the canonical bytes (4.5), for hashing, the wire and saves. Bytes are
    not numbers; WorldApi shows a `Pos` to people and agents as exact text, never as three floats (section 9.4).
    Bytes and text can be parsed back into numbers, so they are side doors; the ruling is aimed at accidents, so the
    engine may use them, but pe-sdk should not give scripts `encode()` or a text parser (a note for WS-logic,
    after @chunks-reviewer's L6).
- `dist(&Pos, &Pos) -> f64` and `dist2` stay, as sugar for the difference.

### 4.3 Range without saturation

- In memory a chunk index is an `i64` per axis, 1,024 m chunks [V `coords.rs:17`]: about ±9.4 × 10^21 m, roughly
  a million light-years.
- **No silent saturation** [U m7e85960c864a]. Today `normalize` clamps at ±2^62 [V `coords.rs:83-108`]. Instead:
  - moving a `Pos` returns `Result<Pos, OutOfRange>` (4.2); every path that writes a position into the world (an
    edit, an input, a physics write-back, a spawn) turns an error into a refusal of that entry. For physics, as
    @vdw-physics confirmed: when a write-back would leave the range (or pe-physics's own 10^6 m from its physics
    origin), the pose is not written; the body is set back to its last valid pose, the one still in `Transform`,
    with zero velocity and its history reset, and an event is raised, at the same frame on every replica;
  - a non-finite offset is a refusal, not 0 [V today it becomes 0, `coords.rs:85`].
- **A staged limitation of m7e85960c864a** (@chunks-reviewer's L7): from M1 to M3, a position beyond the `i64`
  chunk range is refused, never clamped. The ruling asks for promotion to arbitrary precision past it; that is M4's.
- **The format already goes past 64 bits** (plan 2.1: the format in M1, the rest in M4), and what is hashed does
  not change at M4 (@chunks-reviewer's F2):
  - the canonical encoding of a chunk index is a signed variable-length integer (4.5);
  - an object's leaf hashes a `Pos` field through that canonical encoding, never as its raw row bytes (6.3);
  - in M4 the row keeps its 48-byte inline form, an `i64` per axis, and an index beyond `i64` is held in an overflow
    side table keyed by (object, component, instance, field), with a reserved marker inline: the value `i64::MIN`.
    From M1, `i64::MIN` is never a valid chunk index (a position there is out of range, 4.3), so no M1 world can
    store it legitimately (@chunks-reviewer's N2) [I; WS-regions may choose another storage]. The encoding and so the hash of every position already stored stay the same; only the
    in-memory storage grows.
- **Offsets inside a chunk** keep today's resolution: about 1.1 × 10^-13 m anywhere [V `coords.rs:10-11`].

### 4.4 Positions in the store

- A root object's `Transform` holds a `Pos`. A child's holds an `f64` offset in its frame's space, rotation and
  scale, as today [V `transform.rs:4-10`]. So "an object's own position is relative to its parent", as in Resonite
  (file 17, 7.3).
- The `Transform` row keeps today's layout [I, as built on world/core-v1]: `chunk: [i64; 3]` and `pos: [f64; 3]`,
  `rot: [f32; 4]`, `scale: [f32; 3]`, `_pad`. For a root, the position it denotes is `Pos::at_chunk(chunk)` moved by
  `pos` (`checked_add`), so the row stores a chunk index and an offset, never a lossy absolute; for a child, `chunk`
  is zero and `pos` is the offset in its frame. The limits of `transform.rs:22-26` [V] stay, including the root
  chunk limit of 2^61 [V `transform.rs:51`], which is now a refusal limit on stored roots (an insert past it is
  refused, never clamped), so a stored root's own position is always in range. A composed world position that
  would leave the range is refused (`world_transform` returns `None`), never clamped (4.3). The leaf hash reads a
  root's position through its canonical `Pos` encoding (6.3), so equal places hash equally however the row
  splits them.
- World transforms compose as today and give a `Pos` [V `transform.rs:31-37`].

### 4.5 The canonical encoding of a `Pos`

- Per axis: the chunk index as a zigzag LEB128 varint, then the offset as the 8 little-endian bytes of the `f64`,
  which must be finite and in `[0, 1024)`.
- A decoder refuses a non-minimal varint, an offset out of range and a negative zero, so each position has exactly
  one encoding and therefore one hash [I].

## 5. The state of a world

### 5.1 What one world holds

```
World
├── frame, seed, line id, config (tick rate within the supported range, 60 Hz by default; quorum setting; limits)
├── objects: the object table and one sorted column per component type
└── world tables:
    ├── registry           component descriptions (3.2)
    ├── lines              the line ids that objects' `born_on` indices name (2.3)
    ├── modules            per script asset: its version (source), toolchain, declared components (5.4)
    ├── governance         members, roles, permissions, bans, join rules (storage; rules are WS-gov's)
    ├── claims, cooldowns  exclusive holds (7.2)
    └── tombstones         deleted objects, for the delete rule (7.5)
(edit versions are kept beside each row, 3.2; Local objects are outside the world, 3.1)
```

### 5.2 Snapshots and roots

- Kept: a snapshot shares the store's copy-on-write buffers and costs one reference per column
  [V `crates/pe-sim/src/snapshot.rs:1-12`].
- **Evolved** (plan 2.1; file 04, A6), about 500 lines of internals:
  - **copy-on-write per block,** not per whole column: a column is split into blocks of rows (256 rows, [I]); the
    first write after a snapshot copies one block. Today it copies the whole column [V `column.rs:43-48`];
  - **a dirty bit per block, and a cached hash per object,** so a hash at a cut costs what changed since the last
    cut, not the world (6.4);
  - the world tables are copy-on-write too, so `save` stays a handful of reference counts.
- A **root** is a snapshot plus its root hash (6.2). S2's `save` returns one; `restore` puts it back.

### 5.3 Determinism

- Every world operation is a function of the state and the entry. Every container that is iterated is sorted
  (`BTreeMap`, sorted `Vec`) [V today's rule, `crates/pe-core/src/ids.rs:1-2`].
- **No input aborts a replica** (plan 4.1). Every decode and every operation returns a refusal with a reason; none
  panics on any byte string. pe-world's tests feed arbitrary bytes and entries (section 11).
- Floating-point validity is checked on every write that comes from an entry: no NaN, no infinity, transforms within
  limits [V today's checks, `crates/pe-sim/src/store.rs:205-226`]. Writes from trusted engine code through
  `get_mut` stay unchecked, and `check_values` runs at every cut, as today [V `store.rs:296-313`].

### 5.4 World tables in M1

- **Registry:** as 3.2.
- **Modules** (@iter-bench's layout, WS-logic): one row per script asset id, holding
  - the version: the source tree, a `SimBlobRef`;
  - the toolchain and SDK hashes;
  - the author's module hash, as a plain 32-byte `Hash` field: advisory, for mismatch reports, never a blob
    reference;
  - the declared components;
  - `activate_at`, the agreed frame;
  - the faulted mark.

  No module bytes are in the world (3.4).
- **Governance:** the tables only. M1 stores the member list (keys and roles), the role table (permission bits) and
  the ban list, all hashed. The one permission M1 checks is `EDIT` (section 7.6). Roles follow the user's rulings,
  with no M1 simplification (@coordinator-opus, m78746a465fd2):
  - a private world's template gives the admin role to its creator only;
  - others join under the home-world ruling [U m76967fa60b43]: friends as Guest, a role that has `EDIT` for the
    slice's editing; the confidants of the world's admins as admins;
  - public worlds start strangers with no admin rights [U md2fa23dfa774, Q6]; that template is WS-gov's (M3).
- **No separate world-ownership state** (the user, 19:50Z, via @coordinator-opus mfa5afc01f0ce): anyone with the
  admin role is a co-owner of the world, so "owner" in a world means "an admin", and the world model stores no
  ownership table or certificate chain. (An object's `Protection` owner, 7.8, is a different thing: the member who
  created or imported that object.) How admins reconvene after a split is WS-gov's and WS-diverge's.

  How M1 decides who is a friend or a confidant (the relationship data of m9b1db6041d50) is WS-gov's and WS-app's;
  the M1 demo needs only the creator and one Guest.
- **Claims and cooldowns:** ported from pe-session's tables (plan 2.2: `session/store.rs` → pe-world)
  [V `crates/pe-session/src/session/store.rs:28-55`], keyed by `ObjectId` and `MemberKey` instead of indices.
- **Tombstones:** section 7.5.

## 6. The hash tree

### 6.1 The hash

- **BLAKE3, 256 bits,** for every commitment: object leaves, tree nodes, roots, blobs, ids [plan 3.2, S1: the only
  hash type is 256 bits]. Keyed by domain-separation strings (`"pe.leaf"`, `"pe.node"` and so on): the notation
  `BLAKE3("pe.x" ‖ a ‖ b)` means BLAKE3's key-derivation mode with the context string `"pe.x"`, fed `a` then `b`
  (`pe_core::commit::Hasher::new`). Only content addresses (a blob's name, 3.4) and the origin entry (2.3) use plain
  BLAKE3.
- The crate is used with its pure-Rust implementation (feature `pure`, or the portable path), so the native audit
  passes [I; WS-ci confirms the audit's verdict].
- XXH64 stays in pe-core for non-commitment uses: physics test records and caches [V `crates/pe-core/src/hash.rs`;
  `crates/pe-physics` records]. It never appears in a cut, a vote, a root or an id.

### 6.2 The shape

**The single definition is `pe_seams::Node::hash`** (S2 v1, c93fa631, approved by @design-reviewer at 7bd3a3cd):
pe-world calls it, and so does every walker that checks a node against its parent, so the two can never disagree.
`H(ctx ‖ …)` is BLAKE3 in key-derivation mode with the context `ctx` (6.1); every integer is little-endian; counts come
before variable lists.

```
world root   = H("pe.root"   ‖ frame u64 ‖ config hash ‖ tables root ‖ count of domains u64 ‖ each domain root,
                               in DomainId order)
  config hash  = the hash of the world's canonical config bytes
  tables root  = H("pe.tables" ‖ count u64 ‖ each table's hash, in this order: registry, lines, modules, governance,
                               claims, cooldowns, tombstones)          each table hash is a sorted-map hash
  domain root  = H("pe.domain" ‖ domain id u64 ‖ objects root ‖ physics_global)
                 physics_global is pe-physics's hash of that domain's physics image (6.3); per domain, so M4's
                 domains per contact group change no format (@design-reviewer's r5 Low 1)
  objects root = the root of a path-compressed 16-way trie over the ObjectIds of that domain's World objects
    inner node = H("pe.node"  ‖ depth u8 ‖ the node's prefix, ceil(depth / 2) bytes, two nibbles per byte, high
                               nibble first, the last low nibble zero when depth is odd ‖ child mask u16
                               ‖ the hashes of the present children, in nibble order)
    leaf       = H("pe.leaf"  ‖ object id u128 ‖ object leaf hash)
    empty trie = H("pe.empty") of no bytes
```

- **Nibbles run from the most significant** nibble of the id's `u128` down (`pe_seams::Nibbles::of_id`), so prefix
  order is id order.
- **A node binds its own prefix**, so a source cannot relabel a subtree without breaking its parent's hash
  (@diverge-eng's Medium 3). A walker from the agreed root checks each received node against the hash its parent
  named, and the first lie is caught.
- **Malformed nodes have no hash** (`Node::hash` returns `None`): an inner node at depth 32, one with no children,
  one with a child nibble of 16 or more or children out of order, and domains out of order. An inner node with one
  child is legal in the formula, but the canonical trie never makes one, so a peer's one-child node fails its
  parent's hash.

- **A trie keyed by id, with one canonical shape** [I]. The rule:
  - a leaf sits at the shortest prefix of its id (in nibbles) that no other id in the trie shares;
  - an inner node exists only where at least two children do, at the depth where their ids first differ, and its
    hash includes that depth;
  - a delete that leaves an inner node with one child collapses it, so the shape is exactly the one a fresh build of
    the same id set gives.

  So the shape is a function of the set of ids alone, never of the order of inserts and deletes, and two machines
  with equal state have equal tries. A property test checks it: random insert and delete sequences, in different
  orders, give equal roots and equal shapes.
- **Depth.** Derived ids are uniform, so a leaf sits at about log16(n) nibbles: 5 for a million objects. An author
  could grind entry bytes so its ids share a long prefix. That only lengthens one path (at most 32 levels, the id's
  32 nibbles), a linear cost, and it changes no outcome [I; noted, not capped].
- **Why not by place:** balls cross cells many times a second; a spatial tree would rehash paths on every move. The
  plan puts agreement per contact group from M4 (file 16, C4), and a group is a set of ids, so the trie per domain
  fits it.
- **Domains from M1** (plan S1): one domain per world until M4, so M1's world root has exactly one domain root.
- **Empty and absent are the same:** a column with no rows, an object with no components of a type, and an empty
  table each contribute nothing, as today's hash skips empty columns [V `snapshot.rs:11-12`].

### 6.3 What an object's leaf covers

`object leaf hash = H("pe.obj" ‖ id ‖ Node row ‖ for each component row in (type, instance) order: type id ‖
schema hash ‖ instance ‖ row version ‖ canonical row bytes)`

- **Canonical row bytes** are the row's raw little-endian bytes, except that each `Pos` field is replaced by its
  canonical encoding (4.5). So the hash never depends on how a position is stored in memory (4.3).
- **Every component row,** including `Hand`, and every row's edit version (3.2), since versions decide precondition
  outcomes.
- **The physics state of a bound body** enters through `BodyState`, which pe-replica writes at cut frames. Its
  digest is BLAKE3 over the body's pose bits, velocities, awake flag and sleep timer, and the contact-cache entries
  of every pair whose lower id is this body, in canonical order (agreed with @vdw-physics). Then one ball's
  divergence shows in one leaf. This digest **localizes**; it does not by itself make the root complete.
- **`physics_global` makes it complete** (@vdw-physics). A step also reads state that belongs to no body or pair and
  cannot be split out of the fork: the id pools, the constraint graph's colours and order, joint colours, solver-set
  membership and order, islands, the broad-phase trees and move buffer, and sensor state [V as @vdw-physics cites:
  `crates/box3d-fork/src/id_pool.rs:36-44`, `constraint_graph.rs:75-130`]. So `physics_global` is pe-physics's
  full-state check hash: BLAKE3 over the canonical transfer bytes of the whole physics world
  [V as cited: `crates/pe-physics/src/image.rs:343-347, 477-481`]. With it, equal roots give equal futures
  (@diverge-eng's R-H1).
- **What a mismatch means.** A mismatch in a leaf names the object. A mismatch only in `physics_global` means equal
  body values with a different solver history: rare, not localizable, and repaired by the whole-image restore.
- **Cost at a cut** [I, @vdw-physics's estimate]: about 0.2 ms for 500 body digests and 1-2 ms for the image hash;
  M2's incremental images cache both.
- **Not covered at all,** by design: `Local` objects; the drawn state of the display layer; caches; blob bytes
  (covered by their hashes). Nothing that changes a later shared outcome may live outside the tree: that is the rule
  F5 asks for (file 14, 2.3), and section 11 tests it by injecting a divergence into each covered part.

### 6.4 When it is computed

- **At every cut frame** proposed or voted on (pe-agree decides which frames those are), and on demand.
- **Which frames are cut frames** is pe-agree's. My proposal to WS-agree [I]: cuts are proposed only at frames on a
  fixed cadence, so the replica knows in advance which frames are cut frames. The cadence is a duration (a world
  setting in seconds), which pe-agree turns into K frames at the world's tick rate; the tick rate itself is a world
  setting within a supported range, 60 Hz by default (the user, md4b42d16d9df).
- **Hashing is lazy** (@design-reviewer's r5 Medium). At each pending cut frame (one this replica has not yet voted
  on, or not yet compared after finality) the replica keeps a copy-on-write snapshot, a handful of reference counts
  (5.2). A re-simulation across that frame replaces the snapshot; it hashes nothing. The root is computed from the
  snapshot only when it is needed: to vote, or to compare. So a deep rollback never pays the 1-2 ms image hash per
  cut frame it crosses; a vote pays it once. Once a cut is final and compared, its snapshot is dropped (the last 8
  final roots are kept, 9.2). M1 measures the cost of one deep rollback and one vote (acceptance 15).
- **Incrementally:** each object leaf is cached; a write marks the object dirty (by block, 5.2); a hash at a cut
  rehashes the dirty objects and the trie paths above them. A moving ball dirties its leaf once per frame; 500 moving
  balls rehash 500 leaves and about 500 × 3 trie nodes per cut [I; measured in M1].
- **Caches roll back with the state.** The cached leaf hashes and trie node hashes live inside the snapshotted
  copy-on-write blocks, so `restore` brings back the caches of the restored frame, never hashes from the future. A
  property test: after random sequences of steps, snapshots and rollbacks, the incremental root equals the root
  computed from scratch.
- **Verified restore** (@diverge-eng's R-H3): `verify(root)` rehashes a root from its rows and tables alone,
  ignoring every cached hash, and compares. Repair rung 1 uses it on a restored root; tests use it after every
  incremental hash, so a stale cache is a test failure, never a silent wrong root.
- Today a whole-store XXH64 runs on every simulated and re-simulated frame [V file 04, 1]. The
  per-frame hash goes: nothing in the new design needs a hash at a frame that is not a cut [I].

### 6.5 Finding what differs, and patching it (S2)

- **Diff** is a walk of two tries, in rounds: ask for a node's children, descend into those that differ. Over the
  network that is about one round trip per level, 5 at a million objects. The S2 calls (S2 v1; section 9.2):
  - **paths are nibble prefixes** (`TreePath`: 0 to 32 nibbles of an id, with the domain and table paths as named
    prefixes above it), because the trie is path-compressed and two sides with different id sets have tries of
    different shapes (@diverge-eng's Medium 1);
  - `node(prefix)` returns the node covering that prefix, passing through compressed segments:
    `Inner { prefix, children }` (its depth is its prefix's length; the mask is computed from the children's nibbles,
    never sent), `Leaf { id, leaf_hash }`, or `Empty`;
  - when my node's prefix and theirs differ, the difference is structural: everything under theirs is fetched, and
    everything under mine that they lack is removed;
  - `export(root, keys)` returns the records for objects and tables (`LeafKey`) with their paths to the root.
  Tables are named paths of the same tree, so a differing table is found and exported the same way.
- **Import** (repair rung 2): `import(at, records)` replaces the named objects' rows and the named tables with
  fetched ones. Every record's path is checked, before anything is applied, against the quorum-agreed root of a
  final cut, never against the sender's claim. It is a restore of part of a root; WS-diverge drives it and
  pe-replica replays forward from it.
- `object_hashes(ids)` returns the current leaf hash of each object, for the fault bundle's replay
  (@diverge-eng's `replay-bundle`).
- An object present on one side and absent on the other is a differing object like any other.
- **A bound body cannot be patched by rows alone** (@diverge-eng's R-H1; @design-reviewer's High 2). The fork's
  API cannot set contact manifolds, warm starts, sleep timers, islands, solver sets or the broadphase, and writing a
  body's state back through it diverged at the first step: 2 of 256 bodies bit-equal [V design.md 6.9, as the
  reviewer cites]. @vdw-physics confirmed that an exact per-body setter is not feasible: even with every contact
  copied, the ids, colours, islands and broad-phase topology would differ. So:
  - `import` refuses an object that has a bound body, with `NeedsPhysicsImage`;
  - WS-diverge restores the whole physics image at the agreed cut for such a divergence (it folds the whole-transfer
    restore into its rung 2 for M1), and imports only physics-free rows by object.

  The per-body digest still localizes the fault: the fault bundle names the ball, even when the repair restores
  more than the ball. The lasting fix is a history-free physics state, which the plan keeps for later.

## 7. Edit entries and the three conflict rules

### 7.1 The edit payload

An edit entry is pe-log's envelope with class `Edit` and this payload [I]:

```rust
pub struct Transaction {
    pub actor: Actor,                     // who produced it: for history, the UI and one-step revert (C11)
    pub label: Option<String>,            // at most 200 bytes: "make the balls blue", for the UI and history
    pub preconditions: Vec<Precondition>, // all must hold, or nothing applies
    pub ops: Vec<Op>,                     // applied in order, all or nothing
    pub blobs: Vec<BlobRef>,              // blobs this transaction introduces (3.4)
}
pub enum Op {
    Create { new: u32, parent: Target, index: Option<u32>, class: Class, rows: Vec<Row> },
    Delete { id: Target },                                    // the object and its subtree
    SetRow { id: Target, component: ComponentId, instance: u16, row: Bytes },   // add or overwrite
    SetField { id: Target, component: ComponentId, instance: u16, field: u16, value: Bytes },
    RemoveRow { id: Target, component: ComponentId, instance: u16, tag: u128 },   // names the instance (7.6)
    Reparent { id: Target, parent: Target, index: Option<u32>, keep_world: bool },
    Copy { source: Target, new: u32, parent: Target, index: Option<u32> },   // the subtree; templates (7.1)
    Undelete { id: ObjectId },                                // restore a deleted subtree from its tombstones (7.5)
    Register { components: Vec<ComponentDesc> },             // with a script commit (WS-logic)
    CommitScript { script: ScriptId, source: SimBlobRef, toolchain: Hash, sdk: Hash, author_module: Hash,
                   activate_at: Frame, base: Option<Version> },   // WS-logic's commit; no module bytes (5.4)
}
pub enum Target { Existing(ObjectId), New(u32), Root }
pub enum Actor {
    Person,                                                   // the entry's author, by hand
    Agent { harness: String, principal: MemberKey,            // "Alice's agent (Claude Code)"
            requested_by: Option<MemberKey> },                // a guest at an unlocked panel (M5, A1); None in M1
}
pub enum Precondition {
    Version { id: ObjectId, component: ComponentId, instance: u16, is: Version },  // the version the author read
    ObjectVersion { id: ObjectId, is: Version },              // anything about the object: any row, parent, children
    Exists(ObjectId),
    Absent(ObjectId),
    Content { id: ObjectId, component: ComponentId, instance: u16, hash: Hash },  // the row's bytes, as read
}
```

- **The actor** (@api-eng's O12). An agent's edit is authored and signed by its principal's member, and the agent
  holds all of that member's permissions (the user's moderation answer, @coordinator-opus m220b6210535b), so
  `principal` must equal the entry's author key; otherwise the transaction is refused as `Invalid`. `harness` (at
  most 64 bytes) names the agent program for the label. `requested_by` is reserved for M5's guests at an unlocked
  panel and must be `None` until then. The actor decides nothing else: permissions come from the author.
- **`label` and `harness` are untrusted peer text** (@design-reviewer's and @chunks-reviewer's r8 Low). They must be
  valid UTF-8 within their length limits; anything else refuses the transaction as `Invalid`, so decoding is total
  and identical everywhere (5.3). Every display treats them as untrusted text, and wherever an agent reads them
  (history, watch) they are quoted as peer text, never as instructions (api.md's rule on peer text). `Actor` is an
  honest client's statement: a modified client can claim `Person`, and nothing depends on it.
- **WorldApi's per-op versions** (`if_version`) become `ObjectVersion` preconditions in the one list, and its
  `$ref`s become `New(k)` targets (@api-eng, api.md fd8baf6), so the wire carries no per-op versions.
- **A grab moves no version** (inputs never do, 7.3): an object someone is holding refuses pose edits as `held`
  while held (7.2), and once released an edit based on the version read before the grab still applies. Only an
  edit makes a precondition stale.
- **Copy and templates** (@api-eng's O3). `Copy` duplicates an object and its subtree, rows and all, under a new
  parent. The copies take derived ids: `new` is the index `k` of the copy's root, and the rest of the subtree takes
  the next indices in depth-first order, children in their order. So a `Copy` of an n-object subtree reserves the
  contiguous range `new .. new + n` of the transaction's `New` indices. Two ops whose ranges overlap (a `Create` and
  a `Copy`, or two `Copy`s) refuse the transaction at validation with `NewIndexOverlap`, never as "id exists"
  (@design-reviewer's r2 Low 1). Templates are world data: ordinary objects under
  a `Templates` root that the schema lists (the ball-pit's box, ball and lamp), and creating from one is a `Copy`.
  "Double the cube" is a `Copy` too. A copy is copy-on-write in storage (file 04 A1: new ids over the same records)
  [I; an optimisation, invisible in the hash].
- Field writes name a field by its index in the description, and give its bytes. WorldApi turns names into indices
  against the schema at the author's frame.
- **Limits** per transaction [I, to tune]: 4,096 ops, 1 MiB of payload apart from blobs. Over the limit is a
  refusal, not a truncation.

### 7.2 Rule 1: exclusive holds (grabs), the first wins

- A grab is an **input** (section 8), not an edit. It claims its object for its author while held; the claim renews
  with each held-grab input and lapses after a short timeout, with a cooldown before the same author may claim it
  again. These are pe-session's claims and cooldowns, ported unchanged in substance (plan 2.2, D72, review M3)
  [V `session/store.rs:37-55`].
- **The first in the agreed order wins** (R5): a grab of an object claimed by someone else is refused, with the
  holder named. pe-log's order decides "first": frame stamp, then the seeded rank (D37) [plan 5.2]. When both grab
  one ball, the later grab's author sees the ball slide out of their grip (the display layer's cue, file 14, 4.1).
- **Out of reach** (@vdw-physics's proposal) [I]: a grab is refused with `OutOfReach` if the hand's centre is
  further from the object than the hand's reach plus the object's bounding radius, both from `Transform` and
  `Shape` at the grab's frame, plus a slack (a world setting, a few centimetres by default). The slack absorbs
  prediction error: the check runs in the agreed order, and the author's prediction may have placed a thrown ball
  just in reach, so without it the author could feel a grab and then lose it, a correction of what they hold
  (@design-reviewer's r5 Low 3). It is deterministic, and it stops a grab joint yanking an object across the world.
- **Holds protect what the holder feels** [I, after @design-reviewer's Low 6]. The display rule is that a player's
  hands and what they hold are never corrected (plan M1; file 14, 4.1). So, while an object is held:
  - a `Delete` of it, or of any ancestor of it, by anyone but the holder is refused with `Held { by }`. The
    holder's own delete applies and ends its hold, so a VR player can delete what they hold without letting go
    first (@coordinator-opus m6d5969e46cb1: option (b), since a release is an input, not an edit op, and cannot
    join the delete in one transaction);
  - an edit that writes its `Transform`, `Body` or `Shape`, or reparents it, is refused with `Held { by }`;
  - every other edit passes: an agent may recolour a held ball. WorldApi reports the refusal as `held` (@api-eng's
    O5).

  In a merge the order decides, as everywhere: a delete before the grab destroys the ball and the grab is refused;
  a grab before the delete refuses the delete.

### 7.3 Rule 2: edits with a precondition, refused if stale

- **What a version is.** Every component row has a version: the `EntryRef` of the last *edit* that wrote it, with
  the op's index. Every object also has a structural version: the last edit that created it, reparented it or changed
  its children. Row versions sit beside their rows and the structural version in `Node` (3.2), so both are in the hash.
- **The instance tag** (@coordinator-opus m6d5969e46cb1, after @fable-reviewer). Beside each row's version sits a
  128-bit tag that names one addition of the row: `BLAKE3("pe.row" ‖ line ‖ author ‖ chain ‖ seq ‖ op)[..16]` of the
  edit op that added it where there was none, the coordinates `ObjectId::created` uses (2.1), so an author can name a
  row its own transaction adds. Overwrites keep it; a row no edit added has tag 0. `RemoveRow` names it (7.6).
- **The object version** (@api-eng's O1) is the last edit that changed anything about the object: any of its rows,
  its parent or its children. It is also kept in `Node`, and it is what an `ObjectVersion` precondition checks, so
  an agent that read a whole object can ask "nobody edited it since". Row versions are the finer option.
- **Versions are entry ids, not frames.** A version names the edit (its `EntryRef` and op index), so a late entry
  ordered earlier than the author's read still moves the version and is still a change.
- **Only edits move versions** [I]. Physics write-backs, logic systems and inputs do not. Otherwise a moving ball or
  a ticking score would be stale every frame, and no agent could ever edit them with a precondition. A `Version`
  precondition is about other *edits*: "nobody changed this since I read it".
- **A content precondition** covers every writer [I, after @design-reviewer's Medium 3]. `Content` holds the
  BLAKE3 hash of the row bytes as the author read them, and is stale if the row's bytes now differ, whoever wrote
  them: an edit, logic, an input or physics. It needs no extra state. The plan's QR (b) mitigation (plan 5.1, Q8:
  a per-player script's effect names the versions it read and is refused if they are stale) uses it, so a lamp that
  logic turned off makes the effect stale. Its one blind spot is a row that changed and changed back between the
  read and the check; then the effect's cause is present again, which I take to be acceptable. WS-logic confirms.
  A row that physics or logic writes every frame (a moving ball's `Transform`) is stale at every check, so an effect
  should name only the rows its script declares as causes (the lamp), never everything it touched
  (@design-reviewer's r2 Low 2; a note for WS-logic).
  The refusal names no winner, since the writer may be no entry; it reports the current row instead.
- **The check.** At the transaction's place in the agreed order, every precondition is evaluated against the state
  just before it. If any fails, nothing applies, and the outcome is

  `Refused(Stale { precondition, current: Version, winner: EntryRef })`

  where `winner` is the edit that set the current version. WorldApi reports it over MCP with the winning entry's
  actor and a summary (plan M1: "a stale precondition is refused, with a reason that names the winner").
- **The 10 s seat** (R7) is this refusal's path: the API offers the author the rebase (re-read, re-apply). The world
  model only needs to make the refusal exact.
- **A code commit** is the same rule on the `modules` table: `CommitScript.base` names the script version the author
  built on (R7's "rejected and asked to rebase"). `base: None` means "this script does not exist yet": if a script
  with that id already exists, the commit is refused as `Stale`, naming the commit that made it, so two authors'
  first commits of one script cannot silently overwrite each other (@design-reviewer's r7 Low 1).
- **`activate_at` is bounded** (@design-reviewer's r7 Low 2): it may be no earlier than the entry's own frame
  stamp; an earlier one is refused, so a commit cannot activate code in the past. **In M1 it must equal the stamp**
  (WS-logic's rule; pe-seams v0 ruling 22), and pe-world refuses any other value as `Invalid`.

### 7.4 Rule 3: plain writes, the latest in order wins

- A `SetRow` or `SetField` with no precondition on that row always applies, if it is valid. Of two plain writes to
  one field, the later in the agreed order stands.
- **The overwritten author is told** [I; file 04, defect 9]: the outcome of the later write lists the versions it
  replaced (`overwrote: Vec<Version>`), and WorldApi's watch delivers "your write to X was overwritten by Y" to the
  earlier writer.
- Field granularity: a `SetField` changes one field, so two edits that set different fields of one row both stand.
  The row's version moves with each [I: one version per row, not per field, to keep the version array small;
  per-field versions are a later option if agents trip on it].

### 7.5 Deletion, and the delete against an add-on

The user's rule (m35138bafc46a): "depends on the order of events. if the delete occurred first, then the add-on is
orphaned. if the delete occured second, then everything is destroyed."

- **Delete second:** `Delete` destroys the object and its whole subtree, add-ons included, as today
  [V `store.rs:131-153`].
- **Delete first:** an edit that creates an object under, or reparents an object to, an object already deleted does
  not fail (@diverge-eng's R-O1, `docs/spec/divergence.md` §10). The object becomes a root:
  - a created object is placed where the deleted parent last was: its last world transform, composed with the op's
    local transform;
  - a reparented object keeps its own world pose.

  The outcome is `Applied` with the note `Orphaned { deleted_by }`. The parent's structural version is never an
  implicit precondition of an add-on.
- **Tombstones** make that possible. A delete leaves a tombstone for **every object of the deleted subtree**, each
  with the deleting `EntryRef`, the frame, that object's own last world transform, and its rows, versions and place
  in its parent as they were (shared copy-on-write with the snapshot before the delete, so it costs no copy). The
  tombstone's hash covers that object's last leaf hash. So an add-on to a grandchild
  of the deleted object is orphaned at the grandchild's last place, as the user's rule asks (@design-reviewer's
  High 1). Tombstones are a world table, so they are hashed.
  Each tombstone stores its **expiry frame**, fixed at the delete: the delete's frame plus one minute (the
  unconfirmed-play bound) converted at the tick rate in effect then, so a later change of the rate does not move
  existing expiries (@design-reviewer's r9 Low 2). It is dropped once the current frame passes its expiry, a
  deterministic rule of the state [I]. Every time limit in this file is in seconds, never a fixed count of frames
  (@coordinator-opus m3e12fe81d732). A merge older than that reads them back from the world record (M3,
  M5).
- **Undelete** (@design-reviewer's r2 Medium). `Undelete { id }` restores a deleted subtree from its tombstones
  with the **same ids**, rows and versions, under its old parent at its old place (or as a root, orphaned, if that
  parent is gone too), and drops the tombstones. It is valid only while the tombstone lives and the id is absent;
  otherwise it refuses with `Unknown(id)` or `Exists(id)`. Because the ids are the same, every `ObjectRef`, script
  target and precondition that named the deleted objects resolves again. A one-step revert of a delete uses it.
  Row versions are restored as they were, but each restored object's object version and structural version move
  to the `Undelete` entry, so an `ObjectVersion` precondition read before the delete is stale afterwards: "nobody
  edited it since" stays strictly true (@design-reviewer's r3 Low).
- **Deleting an object already deleted** applies, with the note `AlreadyDeleted { by }` (@design-reviewer's
  Medium 4). So when both sides of a merge deleted one object, the later side's transaction still applies its other
  ops. `Delete { Root }` is refused: the world's root is not an object.
- **After a tombstone expires,** an op naming that id is refused with `Unknown(id)`, as for an id that never
  existed. A re-merge older than the window (M5) replays from the world record, and the replay re-creates the
  tombstones at the frames it passes through, so it gets the ordinary rule.
- **Other ops on a deleted object** (`SetRow`, `SetField`, `RemoveRow`, `Reparent` of it) refuse the whole
  transaction with `Deleted { by }`. That includes attaching a component to it: a row cannot exist without its
  object, so there is nothing to orphan [I]. It is still the ordinary rule, so a merge needs none of its own. A precondition `Exists(parent)` lets an author choose refusal over orphaning.
- **The same rule, live and in a merge.** Nothing here knows about merges: a merge replays both branches' entries in
  one order (file 14, 4.4), and this rule then gives the user's answer for either order. That is the property I want
  the reviewers to check first.

### 7.6 Validity: when a transaction is refused

Checked in order, at the entry's place in the agreed order. The first failure refuses the whole transaction, with
its reason and the index of the op [I]:

1. **Decoding.** The payload decodes canonically; limits hold.
2. **The actor and permission.** An agent's `principal` is the author (7.1), and the author holds `EDIT` (5.4).
   Governance is in the hashed state, so every machine gives the same answer (plan S2's `permits`).
3. **Preconditions** (7.3), against the state just before the transaction.
4. **Each op, in order,** against the state just before it, which includes the earlier ops of the same
   transaction: its targets exist (or orphan, 7.5); no protection is broken (7.8) and no hold (7.2); the components
   are registered; the rows have the right size and valid values; positions are in range (4.3); no cycle; the depth
   limit holds (3.3); `Hand` rows are not written by edits (they are inputs); `Node`, `BodyState` and row versions
   are never written by edits. Protections and holds are checked here rather than in step 2 because an op may name
   an object an earlier op created, or lift a protection a later op relies on; a transaction that does the latter
   passes only if its author may lift that protection (@coordinator-opus m6d5969e46cb1).
5. **All or nothing.** The ops apply to a copy-on-write working state; any failure discards it. Nothing outside the
   transaction sees a partial result, and the reported reason is the first failing op's, in order.

- **One pure function** (@api-eng's O4). `World::apply(state, frame, entry) -> (state', Outcome)` is the only edit
  path. The ordered step, the author's local prediction and a dry run all call it, so they cannot disagree.
- **A dry run** is the same path, run on a scratch copy of the latest state and discarded. It changes nothing and
  returns the outcome the live run would give at that state.
- **As built** (world/edit-v1, `crates/pe-world/src/edit.rs` and `apply.rs`) [I; each to the reviewers, and the
  multi-way ones to @coordinator-opus]:
  - the codec is `edit.rs`'s: little-endian, one-byte tags, flags 0 or 1, `u16`-prefixed text, `u32`-prefixed bytes
    and lists, ids as `u128`. It is strict, so every accepted payload has exactly one encoding;
  - a `Version` precondition on a row that does not exist compares against the all-zero version, so it holds only
    if the author read "no row". Its `Stale` then reports `current` and `winner` as zero, which means "absent": once
    a row is gone, the world keeps no record of who removed it (the remove's outcome listed it in `overwrote`). A
    `Content` precondition cannot express absence and always fails on a missing row; `Version` with the zero
    version does that (@design-reviewer's Lows 1 and 2). A failed `Exists` gives `Deleted` or `Unknown`, and a
    failed `Absent` gives `Exists`;
  - `Content` is `BLAKE3("pe.content" ‖ the row's stored bytes)`;
  - `RemoveRow` names the instance by its tag (7.3). If that instance is gone (no row, or a row added again since,
    with another tag), the op applies, changes nothing and notes `NoEffect { op }`, so a stale remove cannot take
    out a later row, and two removals in a merge need no rule. The no-effect check comes first, so a remove of an
    absent row is never refused as held or protected. `SimSize` cannot be removed (3.5);
  - deleting an object already deleted behaves the same way, noting `AlreadyDeleted` (7.5);
  - a `Delete` is refused with `Held` if any object of its subtree is held by anyone but the author; the author's
    own holds on the subtree end with it (7.2);
  - only instance 0 is accepted in M1; any other instance is `Invalid`;
  - `Create` carries no class: an edit makes only `World` objects (3.1);
  - `Copy` copies every row except `BodyState`, refuses a subtree that holds a `Hand`, and gives the copies the
    copier as creator;
  - `Undelete` restores the subtree of that one delete: the delete's tombstones below the id, by their recorded
    parents and their place among their siblings at the delete, never through sibling links, which may name an
    object undeleted since (@design-reviewer's Medium 1);
  - the depth-limit refusal is `TooDeep { object, limit }`;
  - the inverse is the undo ops in reverse order, with a `Version` precondition on the last version the
    transaction set on each row it wrote and that still exists, and the zero version on each row it left removed,
    so a revert re-adds a row only while it is still absent (@design-reviewer's Medium 2). A row the transaction
    added and removed again leaves nothing in the inverse. A revert that re-adds a removed row is an edit adding a
    row where there was none, so the row comes back as a new instance with a new tag, and a `RemoveRow` naming the
    old tag is then `NoEffect`. Tags do not survive a revert, and a merge (pe-diverge) must not expect them to
    (@design-reviewer's Low on 5b10293a).

### 7.7 Outcomes

Every entry the world model applies returns one `Outcome` (plan S2):

```rust
pub enum Outcome {
    Applied { created: Vec<ObjectId>, overwrote: Vec<Version>, notes: Vec<Note>, inverse: Vec<Op> },
    Refused { op: Option<u32>, reason: Reason },   // Stale{..}, Deleted{by}, NoPermission, Invalid{field}, ...
}
```

- `inverse` is the ops that undo this transaction (@api-eng's O4): the prior rows it replaced, deletes of what it
  created, and an `Undelete` of what it deleted (within the tombstone window; 7.5).
  WorldApi's one-step revert submits it as a new transaction, with `Version` preconditions on the versions this one
  set, so a revert after someone else's edit is refused as stale rather than undoing their work.
- `Note` includes `Orphaned { deleted_by }`, `AlreadyDeleted { by }` and `NoEffect { op }` (7.6).

Outcomes are a function of the state and the entry, so every machine computes the same ones, and a replay gives them
again. They are not themselves hashed: the state they produce is.

### 7.8 Protections

The user (mbe87e750ca35, V1): "simple protections can exist for objects the same way resonite functions". With
@api-eng (its O7) [I]:

- A `Protection` row names an owner (`MemberKey`). It protects its object and the object's subtree.
- **Who may protect an object** (@coordinator-opus's ruling U5, m857de72925dd, filling a gap in V1 that
  @chunks-reviewer found): only the object's creator, meaning the author of its creating entry, or the member who
  imported it, may add a `Protection` to it, and only with themselves as owner. An agent acting for that member
  counts as them. Anyone else's attempt, `EDIT` or not, is refused with `Protected { owner }` (one reason, which
  WorldApi reports as `protected`).
- So each object records who created or imported it: a `creator: MemberKey` field in `Node`, set by the creating
  edit (for a `Copy`, the copier; for an import, the importer; for world logic, `LOGIC`) and never written after.
- **Auto-protection** applies on import and on creation only, never later. Wearing an avatar never claims or
  protects it.
- Adding a `Protection` inside someone else's protected subtree is refused too.
- A member who is not the owner may not: `Copy` a protected object (or a subtree containing one); remove or change
  its `Protection` row. Such an op refuses its transaction with `Protected { owner }`.
- Every other edit stays under the ordinary role permissions, so an admin can still delete a griefing object.
- Later (M5): wearing, saving, exporting and fetching the assets of a protected object, under the same owner rule.

## 8. Inputs: hands, pushes and grabs

pe-world decodes and applies the `Input` class too, because inputs write world state (hands, claims) [I].

- **Hand pose:** a target position (a `Pos`, encoded as 4.5), a rotation, the grip and the engaged flag, for frames
  from its stamp (packing several frames per entry, as D73 (a) does today [V `session/entry.rs:203-209`]). Hand
  entries now carry rotation and grip (plan M1, demo step 4).
- **Push:** an impulse on an object.
- **Grab, release:** 7.2.
- **Own hands only** (@item:primordial-engine/session-validation-a-member-moves-only-its-own-h, plan M1): a hand is an
  object with a `Hand` component naming its owner's `MemberKey`. A hand input whose author is not the owner is
  refused on every machine, with `NotYourHand`, and changes nothing.
- **Hands are created by world logic at admission** (@chunks-reviewer's F8): when a member's join takes effect,
  logic (author `LOGIC`) creates its two hands with that member's key as owner. Their ids are
  `BLAKE3("pe.hand" ‖ line id ‖ member key ‖ side ‖ admission frame)[..16]` (@chunks-reviewer's N3), so a rejoin
  gets fresh hands.
  No edit ever writes a `Hand` row (7.6), so no edit can create a hand owned by someone else's key. A leave removes
  them the same way.
- **Validity:** finite values, positions in range, impulses within a world limit [I]; a failure refuses the entry.

## 9. The seams, my side

Proposals to @engine-architect for pe-seams v0. Names follow the plan's appendix A where it has them.

### 9.1 S1, entries

- **Where the id and hash types live** [I]: `ObjectId` (u128) and the 256-bit `Hash` are pe-core types, and pe-seams
  re-exports them. pe-sim and pe-physics use them, and they sit below pe-seams; one type each avoids a conversion at
  every seam. `EntryId` can be a pe-seams newtype over `Hash`.
- **The payloads I own:** `Edit` (7.1) and `Input` (8), and the storage side of `Governance` (5.4). pe-world provides
  `decode_edit(&[u8]) -> Result<Transaction, Reason>` and `decode_input`, both canonical and total, and the
  encoders. pe-agree never decodes them (plan 3.2).
- **The domain id** in the envelope: M1 uses one value per world. The world model asks nothing of it until M4.
- **Object ids derive from the author's key, chain id and chain sequence number** (2.1), so pe-world needs all
  three, as typed envelope fields, when it applies an entry. S1 must keep (chain id, seq) unique for the world's
  life: one chain per member per world, continued on return, and a new nonce whenever a chain starts again (2.1).

### 9.2 S2, the replica

The plan's sketch splits between pe-replica (WS-state) and pe-world. My proposal [I]:

| Operation | Who | Notes |
|---|---|---|
| `step(frame, ordered) -> Vec<Outcome>` | pe-replica | for each entry in order, calls `World::apply(frame, &entry) -> Outcome`; then logic and physics |
| `save() -> Root`, `restore(&Root)` | pe-replica | composes pe-world's snapshot with the physics image |
| `root_hash() -> Hash` | pe-world | requires pe-replica to have written `BodyState` and `physics_global` for the frame |
| `verify(&Root) -> Result<(), Mismatch>` | pe-world | rehash from records, no caches (6.4) |
| `permits(who, what) -> bool` | pe-world | from the governance tables |
| `node(root, path) -> Option<Node>` | pe-world | child (key, hash) pairs, or a leaf: the diff walk, 6.5. **Replaces the sketch's `differing(&Hash)`,** which cannot work from a hash alone |
| `export(root, ids) -> Records` | pe-world | records with their paths to the root |
| `import(at, records) -> Result<Root, ImportError>` | pe-world, driven by pe-diverge | checks every path against the quorum-agreed root first; refuses bound bodies (`NeedsPhysicsImage`) |
| `object_hashes(ids)` | pe-world | leaf hashes (which cover each body's digest), for the fault bundle |
| `physics_image(root)` | pe-replica (WS-state) | the domain's physics transfer as one blob tree by hash, for the whole-image restore (@vdw-physics's route c) |

- `Root` is a `Send` handle. Which roots are kept is pe-seams v0's ruling 7 (`crates/pe-seams/RULINGS.md`): the
  floor, `Cv`, and the final cuts within `T_keep` (a duration, 1.5 s by default, at least the last 2 cuts); the
  earlier "last 8 final cuts" is withdrawn, since every limit is a duration (md4b42d16d9df).
- **How S2 reaches the tree** (ruling 1): `Replica::tree(&root)` gives pe-world's `Tree` on one held root, with
  `node`, `export`, `object_hashes` and `verify`; the receiver of an import is pe-replica's `import_at_cut`.
- **pe-world's side of a step** (ruling 9; replica.md 3): `World::apply` returns, with each applied entry's
  outcome, its physics effects (the objects whose physics-relevant rows changed, pushes, a carried check), which
  pe-replica acts on before the next entry; `World::end_frame(f)` then expires claims and cooldowns. Stepping,
  keyframes, save and restore are pe-replica's (ruling 3).
- **From WS-state, agreed with @vdw-physics:** the per-body digest and `physics_global` of 6.3, written at cut
  frames, and the whole-image restore for bound bodies.
- **The frame's stage order** (@engine-architect, after @iter-bench's N1): entries (inputs and edits, in the agreed
  order), logic before physics, physics, logic after physics, events. pe-world's `apply` runs in the first stage.
- **Body binding:** pe-replica creates and updates physics bodies from `Shape`, `Body` and `Transform`, and writes
  back `Transform` and `BodyState`. pe-world gives it the list of objects whose physics-relevant rows an edit changed
  at a frame, so it never scans.

### 9.3 S5, the frame view

pe-replica publishes it; the world model defines what an object carries [I]:

- id; class; the simulated world pose as a `Pos`, rotation and scale; shape; appearance; light;
- `hand_of: Option<MemberKey>` and `held_by: Option<MemberKey>`, so the display layer can apply "never correct a
  player's own hands or what they hold" (file 14, 4.1) without asking anyone;
- the edit version of the object, so the UI can show "changed by the agent".

The renderer turns a `Pos` into numbers by subtracting the camera's `Pos` (4.2). It never sees a raw absolute.

### 9.4 What S6, WorldApi, reads

WS-api owns S6. From the world model it needs: the schema (3.2), queries over snapshots, transactions (7.1),
outcomes (7.7), and the edit history by version. Two points for WS-api [I]:
- **Positions over MCP:** an agent gives a position either relative to an object ("0.5 m above the table": an anchor
  and an `Offset` in its space) or as an exact `Pos` text. The API never takes or returns a `Pos` as three plain
  floats, which would breach the wall at the agent's boundary.
- **One-step revert** (C11): an applied transaction's outcome, with the prior rows it replaced, is enough to build
  its inverse as a new transaction with preconditions on the versions it set. The world model keeps the prior rows
  in the outcome for that.
- **The read side is built** (world/read-v1, agreed with @api-eng m87250005adbd): `World::snapshot` gives an
  `Arc<WorldSnapshot>` that shares the world's copy-on-write buffers, with `roots` and `all` in id order, `children` in
  their order, `info` (name, parent, object and structural versions, creator, `held_by`, `protected_by`, component
  types), `fields` decoded by name (padding hidden; a `Transform`'s place is one `pos` field, a `Pos` for a root and an
  `Offset` in its frame's space otherwise), `world_transform`, `offset_in(id, frame)` and `distance`. pe-app adapts
  it to pe-api's port.

## 10. Changes to pe-core and pe-sim, and their tests

| Change | Crate | Tests that change on purpose |
|---|---|---|
| `ObjectId` becomes `u128`; `(creator, counter)` and the store's per-creator counters go; `create(creator)` becomes `create_with_id` only | pe-core, pe-sim | `ids::tests::object_id_order_matches_u64` (sizes 8 and 12); store tests that use `create(creator)` |
| `WorldId` becomes `LineId`, a 256-bit hash, with the `LineId::ORIGIN` constant (2.3); `GlobalId` goes | pe-core | the size check of `GlobalId` |
| `WorldPos` becomes `Pos` with private fields; `from_abs` and public fields go; saturation becomes `OutOfRange`; varint encoding | pe-core | `coords::tests::normalize_never_panics` (it asserts saturation, which the user ruled out); tests that build positions from numbers move to `ORIGIN + Offset` |
| `FieldKind::ObjectId` (8 bytes) becomes `ObjectRef` (16); new kinds of 3.2; `ComponentId` widens to 128 bits; row versions beside rows | pe-sim | the `Node` size; schema hashes; reserved-id tests |
| block copy-on-write, dirty bits, cached object hashes; instances; classes | pe-sim | none should: these are internals behind the same API |
| the state hash becomes the BLAKE3 tree of section 6 | pe-sim, pe-world | every test that pins an XXH64 state hash value |
| `Transform` holds a `Pos` for roots; the 2^61 root limit goes | pe-sim | transform limit tests at the root chunk limit |

- **Everything else in pe-core stays as it is:** the clock, `dmath`, the RNG streams, the float-environment check
  and XXH64 [V `crates/pe-core/src/lib.rs:20-30`].
- **pe-physics depends on pe-core's `ObjectId`** and serializes it as two `u32`s [V `crates/pe-physics/src/image.rs:242`].
  Widening it touches WS-state's crate, so I send @vdw-physics the patch, after the archive, and it lands it:
  16-byte ids in `image.rs:197-200, 242` and `world.rs:693-694`, `IMAGE_VERSION` 3 and `TRANSFER_VERSION` 2. The
  committed physics records compare the fork's own world bytes, which carry no object ids, so they should not move;
  any printed transfer hash that does is re-blessed in the same landing [V as @vdw-physics cites:
  `tests/spike_records.rs:159-160`].
- **pe-session and the spikes** also use these types [V their manifests], but M0 archives them outside the
  workspace (plan 2.4), so nothing there is rebuilt. I make the breaking changes only after the archive lands.
- **The determinism property behind the changed tests stays tested:** each changed test is replaced by one that
  checks the same property in the new form (for example: an out-of-range position is a refusal on every machine,
  identically).

## 11. Acceptance for WS-world's M1 deliverable

Each is a test in pe-world, pe-sim or pe-core unless marked [I on all thresholds]:

1. **Edits:** create, delete, set a row, set a field, remove a row and reparent each work, alone and combined in one
   transaction.
2. **All or nothing:** a transaction whose last op fails leaves the state and its root hash unchanged; a dry run
   changes nothing and returns the live outcome.
3. **Rule 1:** of two grabs of one ball, the earlier in the agreed order holds it; the later is refused, naming the
   holder.
4. **Rule 2:** an edit whose precondition was stale is refused with `Stale`, naming the edit that won; with no
   intervening edit it applies; physics motion and logic writes never make it stale.
5. **Rule 3:** of two plain writes to one field the later stands, and its outcome names the overwritten version.
6. **Delete and add-on, both orders,** on one machine and through a merge-order replay: delete first gives an
   orphaned root at the parent's last place; delete second destroys both. The same for an add-on under a depth-2
   descendant of the deleted object; and both sides deleting one object leaves the later transaction's other ops
   applied. Revert of a delete: delete a referenced object, apply the outcome's `inverse`, and the reference
   resolves again to the same id.
7. **Ids:** the same entries applied in two different valid orders give the same ids to the same objects; an id
   collision is a refusal.
7a. **Lines:** the origin's bytes and `LineId::ORIGIN` equal their pinned values. After lines A and B merge into M,
    an entry B's chain makes after its recorded merge point is refused on M (the control: accepting it must fail
    this test). A whole-chunk placement shift moves every `Pos` field of the incoming objects and no `Offset`, and
    leaves every relative position bit-identical.
8. **Own hands:** a hand input from a member who does not own the hand is refused and changes nothing.
9. **What the hash covers:** a one-bit change injected into each covered part (a component row, a row version,
   `BodyState`, each world table, the registry) changes the root, and the diff walk names exactly that object or
   table. A change to a `Local` object changes nothing.
10. **Canonical trie and caches:** random insert and delete orders give equal roots and shapes; after random steps,
    snapshots and rollbacks the incremental root equals the root from scratch (`verify`).
11. **Diff and patch:** two worlds that differ in k of n objects are reconciled by the walk and a patch to equal
    roots, with at most about k × log16(n) nodes exchanged.
12. **The wall:** a compile-fail test (`trybuild`, or doc tests marked `compile_fail`) for `Pos + Pos`, `Pos +
    Offset`, `pos.x`, a `Pos` built from numbers and a `Pos` converted to numbers; `pos - Pos::ORIGIN` compiles.
    `Pos - Pos` at the extremes of the range gives the pinned bits of 4.2 and never panics.
13. **No saturation:** an edit or spawn that would place an object past the range is refused on every machine with
    the same reason; a varint chunk index beyond `i64` decodes as a valid encoding that the M1 build reports as out of
    range, never as a different position.
14. **No input aborts:** property tests feed arbitrary bytes as edit and input payloads, and arbitrary valid-looking
    transactions; nothing panics, and each is applied or refused.
15. **Cost, measured, not gated:** the hash at a cut with 500 moving balls (leaves, body digests and the image
    hash), one deep rollback across several pending cut frames (which must hash nothing), and an edit's apply time,
    reported in the checkpoint record.

## 12. Open points

For the reviewers and the coordinator. None needs the user, as far as I can see [I]; I flag where one might.

1. **Holds and edits** (7.2): deletes and pose, shape and parent edits of a held object are refused; others pass.
2. **Version granularity** (7.4): one version per row, not per field.
3. **Only edits move versions** (7.3): physics and logic writes never make a `Version` precondition stale; `Content`
   preconditions cover every writer. A consequence: an
   agent that read a ball's position and then sets it with a precondition overwrites wherever physics has moved it.
   That is a plain write in effect, which I think is right for poses.
4. **Tombstone lifetime** (7.5): one minute, tied to the unconfirmed-play bound.
5. **`pos.cell(level)` and the wall** (4.2): kept. @coordinator-opus ruled (m707c519c0bcf): the user's concern was
   precision loss, and exact integer cells cannot lose it [I, the coordinator's reading]; the liaison tells the user,
   who may correct it.
10. **Simulation size** (3.5): accepted by the user (m8c42b4b093bd).
6. **The engine under the wall** (4.2): no internal escape hatch. It costs a few explicit `- origin` subtractions in
   physics and rendering, and buys a wall nobody has to trust.
7. **Missing blobs never stop the world** (3.4): display blobs draw a stand-in; simulation blobs make only the
   lacking machine lag, as for a late entry. This narrows D17's "a missing artifact pauses that peer".
8. **The per-body physics digest** (6.3, 9.2): agreed with @vdw-physics, with `physics_global` as the full image
   hash and no per-body setter.
9. **Ids of logic spawns** (2.1) are WS-logic's to confirm.
11. **For the hash-tree slice** (@chunks-reviewer, on world/edit-v1): apply outcomes depend on World tables that
    pe-sim's `state_hash` and `Snapshot` do not cover (`members`, `tombstones`, `rate`, `claims`, `depth_limit`;
    tombstones decide `Orphaned`, `AlreadyDeleted` and `Undelete`). The BLAKE3 root (6.2) and the world transfer
    must cover all of them, or two replicas with equal store hashes could still give different outcomes.
