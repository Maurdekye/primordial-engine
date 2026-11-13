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

**For M1:** the origin constant, line ids in every derivation, and the reserved `Node.born_on` field and `lines`
table. Merging unrelated worlds is M5's.

### 2.4 M1's ball pit: disposable genesis code

**Narrowed for M1** (@coordinator-opus m949a335461b2, option (a), with @fable-reviewer's conditions mbb99c4a64eb7).
This replaces "branch entries for one preset (the ball pit) and one world made from it" above and the
preset-as-a-line text of 2.1 and 2.3 for M1 only. Branch entries and preset lines come in a later slice (option (b)),
which fixes the origin's role deliberately.

- **Genesis code, not a preset line.** `pe_world::preset::ball_pit(seed, blobs, origin) -> World` builds the pit at
  frame 0. In M1 one machine creates the world and the other joins by importing state at a cut, so only one machine
  runs this code.
- **Built through the edit path.** The content is one ordinary edit transaction (`Create` ops with their rows),
  applied by the normal edit path (`dry_run_draft_state`, so encoded and decoded first). The author is world logic
  (`LOGIC`), which holds `EDIT` only while it applies. The coordinates are a reserved line,
  `m1_line() = BLAKE3("pe.m1.disposable" ‖ "ball-pit")`, a chain derived from the seed, and seq 0. The objects'
  versions name the draft's stand-in entry id (9.2's `Draft::entry`).
- **Nothing in it is the origin.** The M1 line is neither `LineId::origin()` nor a preset line, and nothing here is
  treated as one.
- **M1 worlds are disposable and marked.** A world made this way carries `m1_marker = Some(m1_line())`, which is in
  its root. The save format (6, import in slice C) must carry it, so a later build recognises an M1 world and
  refuses it or imports it as unrelated content. No M1 world is kept across the move to (b).
- **Content only.** The pit has a root named "Ball pit" with a static floor, four static walls, a point lamp and the
  balls (200 by default, at most 500: radius 0.06 m, **18 g each** (density 20 kg/m³, a real ball-pit ball;
  the old prototype's 0.2 kg/m³ made them 0.18 g, lighter than air: @design-reviewer's Medium), damping 0.1
  linear and 0.3 angular, rolling resistance 0.2, the old prototype's layout with a seed-dependent jitter). The world's physics origin is set to
  `origin`. It has no members and no hands.
- **An interim session's line comes from its genesis record** (@engine-architect m3692d7694ccf, @world-chunks
  m3839fe322ba7; @coordinator-opus m9ed765ccc34d gives the call to world-model). Until join entries exist, an M1
  session starts from a genesis record both machines check (pe-app's `pe_app::genesis`). Its line is
  `preset::genesis_line(record) = Hasher::new("pe.m1.genesis.line").write_len_prefixed(record)`, over the record's
  bytes **without its root**, since the frame-0 root covers the line. The pit is built on it with
  `preset::ball_pit_on(line, seed, blobs, origin, balls)`, so the pit's object ids and, through `World::admit`, the
  hands' ids derive from it. Why: it is 2.3's rule in M1's form, a line is the hash of the record that makes the
  world, so two sessions never share a line, ids or signatures, where `m1_line()` would give every session with
  one seed the same ids. **The record carries a fresh 32-byte random nonce from the OS**, so two sessions with the
  same seed, members and settings still get different records and lines, and one session's signed entries never
  replay into another (@design-reviewer's and @chunks-reviewer's Low on 1c22bd90; the creation time alone is
  weaker). It is never the origin (`ball_pit_on` refuses `LineId::origin()`), the context is in M1's
  disposable `pe.m1.` namespace so it cannot collide with the branch-entry derivation of the later slice, and the
  world still carries the M1 marker, `Some(m1_line())`, whatever its line. `ball_pit` and `ball_pit_with` stay
  on `m1_line()` for tests and single-machine use.
- **Hands come from admission.** `World::admit(line, member, perms, frame, spawn)` is world logic applying a join
  (8): it records the member's permissions and creates the member's two hands (`ObjectId::hand`, owner the member,
  sides 0 and 1) as roots beside `spawn`. The ball pit's admin is admitted this way at frame 0, like anyone who
  joins, with the world creator's spawn (pe-app passes the pit's `origin`) and `ADMIN | EDIT`. For anyone after
  that, every machine must admit identically, so `perms`, `spawn` and the frame come from the agreed join record
  (8), never from local state. The hands' versions are `GENESIS`, since no edit made them; naming the join's entry
  instead is open until 9.2 gives the join an id (@design-reviewer's Low). A rejoin at a later frame gets two new hands (the ids
  include the admission frame, as 8 intends); removing a leaver's hands belongs to the leave, which is not built yet,
  so until then an M1 world never readmits a member (@chunks-reviewer's note). The avatar root that should parent the hands (3.5) is not built yet; its id derivation is open.
- **An interim session's world** (@engine-architect's `pe_app::genesis`; @coordinator-opus m9571fcf519b9,
  m4d09145cb902) is `preset::m1_session(&Session, blobs)`: the pit of `ball_pit_on` on the session's line, at the
  session's rate, with its gravity and pe-agree's settings record (config item 4; in M1 that is pe-agree's 32-byte
  `NodeConfig::consensus_hash`), and every member admitted at frame 0 through `World::admit` in key order, each with
  the permission bits its role in the genesis record gives (opener `ADMIN | EDIT | AGENT`, joiner `EDIT | AGENT`),
  standing 0.6 m apart just outside the pit's near wall. A key listed twice is refused (`WorldError::Duplicate`). So
  the root covers each member's bits, and the record and the world cannot disagree on anything the root covers; the
  rest (the cut interval, the epoch, the nonce) is bound through the line (2.4, @design-reviewer's nit). The second
  machine rebuilds the world from the record and compares roots, then the replica's frame-0 root, before it starts. A
  test pins the frame-0 root and runs on Windows and Linux [V engine-architect, `crates/pe-world/tests/preset.rs`].
- **No dependence on the preset's ids.** Tests, the harness and API examples find the pit's objects by name
  (`preset::names`, `preset::find`), never by id.
- **Determinism:** the same arguments give the same state root, and admitting the same member at the same frame
  gives the same root again [V `crates/pe-world/tests/preset.rs`].

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
  | `Light` | kind, colour, intensity, range, a spot light's inner and outer cone angles (radians from the local -z axis, 0 ≤ inner ≤ outer ≤ π/2; exactly 0 for other kinds, so unused angles never hash two ways; an edit outside these is refused `Invalid` naming the angle; a `SetRow` changes the kind and the angles together, while field by field a spot becomes a point light by zeroing inner, then outer, then setting the kind) | edits |
  | `Hand` | the owning `MemberKey`, side, grip, engaged, mode (8.1) | created by world logic at admission; then inputs of its owner only (section 8) |
  | `SimSize` | the simulation size of 3.5 | set at creation; edits |
  | `Protection` | the owning `MemberKey` (7.8) | edits by its owner |
  | a script's **tag** (slice D) | nothing: zero size, one per script version, its id derived like the script's other components (7.9) | edits with the object's edit right (attach = add, detach = remove); a script's create gives the new object its tag; a commit re-keys it |

  **Which engine components scripts may write** (@coordinator-opus m58c1611231fe; slice D), only on objects
  carrying the script's tag (7.9), and each written row checked as an edit's would be:

  | Component | Scripts may write | Why |
  |---|---|---|
  | `Transform` | yes, never a hand's (a hand carries no tag, 7.9) | moving things is what logic is for; a written pose of a body is a physics effect (9.2a) |
  | `Light` | yes | the demo's lamp; checked like an edit (the cone-angle rule) |
  | `Appearance` | yes | display only |
  | `Node` | no | the hierarchy API only; scripts create, destroy and orphan through the runner's commands |
  | `Name` | no | a `BlobRef`, and a script has no way to bring a blob into the world |
  | `Shape`, `Body` | no | pe-replica binds bodies from edits' footprints in M1; a later slice may report them as effects |
  | `BodyState` | no | physics state, written only by the step |
  | `Hand` | no | each member's own, through inputs (8.1) |
  | `SimSize` | no | it chooses the object's region layer (3.5, M4); writes come with that work |
  | `Protection` | no | rewriting `owner` would be an ownership change through logic (@iter-bench's question, mc351d0082333) |
  | another script's components or tag | no | a script writes only its own components and the engine ones marked here |

- **The registry is world state** [I]. Today it is configuration shared by every peer [V `registry.rs:216-218`]. With
  a logic edit in M1 [U mc40fc6379bc6, U1], a script that declares components changes the schema at a frame. So:
  - engine components are registered by a line's first branch from the origin, under their reserved ids (2.3);
  - a script's components are registered by the edit that commits the script, at that frame;
  - the registry is a world table (section 5.4), hashed, and rolled back with everything else;
  - a component id never changes its description. A changed schema gets a new id, and a migration is an edit.
- **Component ids are derived too** (@chunks-reviewer's F7). `ComponentId` widens to 128 bits. Engine components keep
  small fixed values below 2^32. A script's component gets `u128::from_le_bytes(BLAKE3("pe.component" ‖ V ‖ name ‖
  schema hash)[..16])` (the schema hash is 6.6's BLAKE3 hash of the description, which has no id in it, so the
  derivation is not circular: pe-sim's own XXH64 schema hash covers the id and is not used here; @iter-bench
  ma47f902e5901), where **V is the content hash of one script version**, the hash of its source archive (one blob,
  logic.md 4.1a; @coordinator-opus mf9dbcfe481ea and me5689a5c645c, @iter-bench m6402c933f295), not the stable asset
  id: this supersedes r7's asset-keyed ids. Every version has its own ids, and a commit **re-keys** the old version's
  rows to the new ones in its own transaction (7.9); a future per-copy fork is the same operation on one copy's rows.
  An id below 2^32 is engine space and is refused at `Register` (a chance of 2^-96). Two commits on two branches can
  never give one id two meanings: the same id means the same description everywhere. `Register` of an id already
  registered with the same description is a no-op; with a different one (a collision) it refuses the transaction.

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
  - **Simulation blobs** (a collision hull, a script's source archive, anything shared logic or physics reads) are
    part of the entry for agreement. Small ones (up to 1 MiB, which covers hulls) travel *inside* the edit entry, so
    any machine that has the entry has the bytes. A script's source always does: `CommitScript` carries the archive
    itself, at most `MAX_SOURCE_BYTES` (7.9), and applying the commit puts it into the machine's blob store, on every
    machine, so each can build the version; a refused commit stores nothing, and what a rollback leaves behind is
    store garbage, pruned with the store (@coordinator-opus m3b68151991e0). Other blobs travel as references only so
    far: a transaction's `blobs` list names them, and their bytes reach other machines by fetch (display blobs may be
    missing, above) [I, the general in-entry carriage of hulls is not built]. Larger ones follow the rule of a late
    entry: a machine that lacks the bytes at the entry's frame simulates as if that change had not arrived yet, which
    is a prediction, and rewinds and replays from that frame when the bytes arrive. It does not vote for a cut that
    includes the entry until it holds the bytes. Nobody waits; only that machine's view lags.
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

### 3.6 Field units

A field's description may name its unit (@coordinator-opus's ruling m8dd4de07426f, on @fable-reviewer's advice). The
unit is part of the description, so it is in the registry's hashed bytes (6.6) and in pe-sim's schema hash, and
WorldApi's `x-unit` comes from it alone. Labels, ranges and widgets are not part of the description and stay out of
hashed bytes.

- **"No unit" has one form,** the empty string. A dimensionless product (`1`, `m/m`) is refused as a unit: such a
  field leaves it empty. A padding field (`_` name) has no unit.
- **Grammar** (`pe_sim::unit`): a product of the symbols `N Pa J W C V Hz kg m s A K mol cd rad sr`, each with an
  integer power, written `factor (op factor)*`, where `op` is `*` or `/` read left to right (`J/kg/K` is
  J·kg⁻¹·K⁻¹), and `factor` is `1` or `symbol` with an optional `^n` (n from -99 to 99, not 0). No spaces and no SI
  prefixes: a field stores the SI unit's magnitude, and a field in another scale has no unit.
- **One canonical spelling,** and registration refuses any other (the error names the canonical one), so `m` and
  `meter`, or `kg/m^3` and `m^-3*kg`, cannot hash two ways. Canonical: each symbol once, its powers summed and
  cancelled symbols gone; symbols with a positive power first, in the order listed above, joined by `*`; then each
  symbol with a negative power, in that order, as `/symbol`; `1/` when no power is positive; a power of 1 unwritten,
  any other as `^n`. Examples: `m/s`, `kg/m^3`, `1/s`, `N*m`, `J/kg/K`.
- **The grammar fixes spelling, not dimension:** `N` and `kg*m/s^2` are both canonical and differ; a field is
  labelled with the one it means. Quantities outside the grammar (colours, frame counts, light intensity whose meaning
  depends on the light's kind) have no unit. A symbol appended to the list later makes new strings valid without
  changing any existing spelling, but older builds refuse a description that uses it, so a newer world's component
  may not register on an older build (@chunks-reviewer's Low).
- **Spelling is not dimension for consumers either:** string equality of units is not dimensional equality (`Hz` is
  not `1/s`, `J` is not `N*m`). Anything that compares or converts units (WorldApi's `x-unit` readers among them)
  reduces them to base dimensions itself.
- **The engine's components** carry: `Transform.pos` m; `Shape.density` kg/m^3, `Shape.size` m; `Body.linear_damping`
  and `angular_damping` 1/s; `BodyState.linear` m/s, `angular` rad/s, `sleep_time` s; `Light.range` m, `inner_angle` and `outer_angle` rad;
  `SimSize.metres` m. Every other engine field has none.
- **Scripted components** (logic.md) declare units through the same description; pe-logic passes them into
  `FieldDesc`, and the same check applies.

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
    ├── tombstones         deleted objects, for the delete rule (7.5)
    ├── hand packs         pending hand input (8.1)
    └── push windows       each member's recent pushes, for the push budget (8.1)
(the tables' order and bytes: 6.6)
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
  - the version: the source archive (logic.md 4.1a), a `SimBlobRef`;
  - the toolchain and SDK hashes;
  - the author's module hash, as a plain 32-byte `Hash` field: advisory, for mismatch reports, never a blob
    reference;
  - the declared components;
  - `activate_at`, the agreed frame;
  - the faulted mark: per asset, not per object (@coordinator-opus mf9dbcfe481ea, item 4). A fault stops all of
    the script's systems; the mark is set after the stage (9.2a) and cleared by the asset's next commit.

  No module bytes are in the world (3.4). The byte layout is 6.6's.
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
                               claims, cooldowns, tombstones, hand packs, push windows)
                                                   each table hash is a sorted-map hash (6.6)
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
- **Empty and absent are the same** for rows: a column with no rows and an object with no components of a type
  contribute nothing, as today's hash skips empty columns [V `snapshot.rs:11-12`]. A world table is different: the
  tables root is positional, so an empty table is hashed with count 0, never omitted (6.6).

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
  **As built (slice C):** pe-sim keeps each object's cached hash beside its rows, with a stale mark that every row
  write sets (a row put or dropped, a version set, a row handed out mutably, a whole column handed out marks every
  object in it, a destroyed subtree), and a restore from a pe-sim `Snapshot` marks everything. `World::object_hash`
  uses a leaf that is not stale and computes the others; `World::refresh_tree_cache` (pe-replica's call, at cuts)
  stores the stale ones. Marks are per object, not per block. The trie's inner nodes are rebuilt for each root, not
  cached: measured on a 507-object pit, a root takes 4.7 ms from scratch and 0.34 ms with every leaf cached, most
  of it the trie [V `root_cost`, release, rough]. Caching inner nodes is left until a measurement asks for it.
- **Caches roll back with the state.** The cached leaf hashes and trie node hashes live inside the snapshotted
  copy-on-write blocks, so `restore` brings back the caches of the restored frame, never hashes from the future. As
  built, the cache is part of the store, so a `World` clone (pe-replica's saved state) carries its own cache, and a
  rollback to it brings that cache back with its rows. A
  property test: after random sequences of steps, snapshots and rollbacks, the incremental root equals the root
  computed from scratch.
- **Verified restore** (@diverge-eng's R-H3): `verify(root)` rehashes a root from its rows and tables alone,
  ignoring every cached hash, and compares. Repair rung 1 uses it on a restored root; tests use it after every
  incremental hash, so a stale cache is a test failure, never a silent wrong root. As built, `Tree::verify` compares every cached leaf with
  its rows (the tables, the config and the inner nodes are computed fresh for each tree), and a property test runs
  random writes, refreshes and rollbacks against an identical world that never caches [V `tests/tree.rs`].
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
  - `export(root, keys)` returns the records for objects, tables and the config (`LeafKey`) with their paths to the
    root.
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

### 6.6 The byte-level definitions (slice C) [I, for review]

What 6.2 and 6.3 leave to the implementation, fixed here so pe-diverge, a joiner and a later build compute the same
bytes. `H(ctx ‖ …)` and the integer conventions are 6.2's; `lp(b)` is a `u64` length then the bytes.

- **The frame** in the world root is the frame the state is at: `World::frame()`, the next frame to step, which is
  the cut's frame when the root is taken at a cut (9.2's convention).
- **Config hash** = `H("pe.config" ‖ config bytes)`. The config bytes are everything 5.1 lists beside the frame,
  since each can change a later shared outcome (@design-reviewer's Medium on ecd11ef0), in this order:
  1. the seed (`u64`): it breaks ties in pe-log's order and seeds logic's random streams;
  2. the world's line id (32 bytes): it feeds every id derivation. In M1 it is the M1 marker's line (2.4), and all
     zero for a world that has none yet (the `lines` table comes with branch entries);
  3. the M1 marker (2.4): a byte 0, or 1 and the 32-byte line id;
  4. pe-agree's settings record, `lp(bytes)`: the quorum setting and the cut cadence, which decide which frames are
     cuts and what agreement is. pe-agree owns the format and pe-world never parses it; the bytes live in the world
     (app.md 4 step 1: world settings live in the world, and a joiner takes them with its state), set by the creator
     at genesis and changed only by an agreed entry at a frame (no such entry in M1: below) (@engine-architect me1c6615d45ae);
  5. the tick rate (`u32` Hz) and the depth limit (`u32`);
  6. gravity (3 × `f32` bits) and the physics origin (4.5's canonical `Pos` encoding);
  7. the input settings of 8.1: `claim_ms`, `cooldown_ms`, `push_budget`, `reach_mm`, `slack_mm`, each `u32`.

  A new setting is appended, never inserted, and that is a world-format version change. The agent-powers slice
  (8.1) appends `max_claims` (without and with the right), `max_chunkloaders`, `max_push_dv` and `max_push_speed`,
  in that order; its per-member override row extends the governance value, also a format change.
- **The config record** (S2 v1.3's `LeafKey::Config`, @engine-architect m79766bd55500): the config bytes, with the
  proof `[Node::World]`; it checks as `H("pe.config" ‖ bytes) == World.config`. Import decodes it exactly (every
  item, nothing after, each value one the world's setters accept: a rate of 30 - 120 Hz, a depth limit of
  1 - 4,096, finite gravity, input settings within 8.1's bounds) and sets every setting from it, the seed included.
  So a joiner imports into a bare world (engine registry, default settings, built at the world's rate, below), and
  an import without the record leaves the importer's config, which the root check catches.
- **Simulation settings live in the world, and the replica reads them from it** (@coordinator-opus m1eb5e9623174,
  after @design-reviewer's Medium on ce6b13f0 and @fable-reviewer ma4438fa0b8ee). Every setting that changes what a
  simulation computes is in the config record, under the root, and the replica reads it from the world it holds.
  It keeps no copy taken at construction. Today that means the rate, gravity, the physics origin and 8.1's input
  settings (the push and speed limits among them); physics settings that a world may choose (CCD switches, solver
  settings) join the config the same way, appended, while those fixed by the engine build are not world state.
  **In M1 a world chooses none beyond the rate, gravity and the origin** (@vdw-physics m7e1e4e93bfe6). The Box3D
  solver constants, CCD, sleep and contact recycling are fixed by the build (`PhysicsConfig::engine()`); sub-steps
  follow from the rate (ceil(240 / hz)); worker count, capacity and event thresholds don't change results (worker
  count [V]: pe-physics tests/api.rs runs `with_workers(2)`, `(3)` and a worker sweep against one result, and
  tests/spike_records.rs per worker count). Every
  physics image carries Box3D's world settings, which `physics_global` covers, and an importer whose settings differ
  refuses the transfer, so a peer on a build with other constants fails import with `PhysicsCheck` and a running
  mismatch shows as a root split at the next cut: nothing silent. A setting a later world may choose (sleep off,
  softer contacts) becomes an appended config item that the replica's one physics-config builder reads.
  - **Gravity and origin** apply from the world's settings on the physics rebuild an import already does
    (pe-replica's, @vdw-physics and @engine-architect).
  - **The rate: M1 runs a world at one rate, checked twice.** At the welcome, so the person is told "this world runs
    at 30 Hz" before anything is built, and the joiner builds its replica at that rate. Again at import: a config
    record whose rate is not the importer's is refused (`ImportError::RateMismatch`, additive in S2, from
    @engine-architect), a fault to report, never a rate to adopt. An import that leaves the config record out
    into a joiner at another rate ends as `RootMismatch` instead (the root check catches it), which pe-diverge's
    stop-at-once rule does not cover; a repair never does this, since the rate is in the config record, so a
    differing rate always makes the config leaf differ and rung 2 sends it (@design-reviewer's nit on a60dac0d).
  - **No change mid-session in M1.** No M1 entry can change gravity, the origin or the rate: pe-world's setters for
    them are trusted calls for genesis, and no edit, input or governance op writes the config. A later slice that
    adds such an entry applies it at its frame, replays included.
  - **Tests:** a 30 Hz world into a 60 Hz bare joiner (refused at the welcome and at import); non-default gravity
    into a default joiner; non-default gravity, an import, then a rung-1 replay on the joiner, its root compared with
    the host's; and one test that sets every simulation setting to a non-default value, imports into a bare joiner,
    steps both sides and compares roots.
- **The tables**, in this order (the `LeafKey::Table` index is the position): 0 registry, 1 lines, 2 modules,
  3 governance, 4 claims, 5 cooldowns, 6 tombstones, 7 hand packs, 8 push windows. 6.2's list gains the last two,
  which 8.1 already puts in the root. M1 has no lines yet, so that table is an empty map.
- **A table's hash** (the "sorted-map hash") = `H("pe.table" ‖ index u8 ‖ count u64 ‖ for each entry in key order:
  lp(key bytes) ‖ lp(value bytes))`. An empty table hashes the same way with count 0; it is never omitted, because
  the tables root names each table by position. A table's export record is its whole entry list (`count ‖ entries`,
  as hashed): tables are small, and a table is imported whole. That has a size edge (@diverge-eng's Low 2 [I]): a
  tombstone carries a whole `Node` record and every row of its object until it expires, so a mass delete (10,000
  objects) makes a tombstones table of several MB, and a difference anywhere in it refetches all of it, against
  rung 2's fetch budget (divergence.md 4.2). Fine for M1. A table that can grow large (tombstones first) may later
  become a keyed sub-trie like the objects, a world-format change. divergence.md 15 (its open points) lists it among the
  limits to check before worlds grow.
- **Import order** (@diverge-eng's Low 1): canonical row bytes are decoded through the registry (`Pos` fields by
  kind), so an import checks every record's hash against the target root first, which needs no registry, then
  applies the registry table, then the other tables, then the objects, decoding each against the registry it just
  applied. **From slice D the registry and modules tables can differ** (a joiner that hasn't seen a commit). An
  import installs the union of its registry and the target's before it decodes any object record, since a record
  may carry a component only the target has registered (@design-reviewer's Low 1), keeping the target's entry
  where both hold one id with a different asset or description, so objects decode as the target encoded them and
  the root check settles the rest (@chunks-reviewer's r2 L2); then patches the objects and
  tables; then prunes the registry to the target's. If an object still has a row of a pruned component, the patched
  root differs from the target and the import fails with `RootMismatch`: such a row still hashes (its description
  is kept until the comparison), never panics, and the import builds a new `World`, so a refused import leaves the
  importer unchanged (@chunks-reviewer's L5). The modules table is replaced whole; pe-replica then rebinds what
  runs (9.2a).
- **Byte order** (@chunks-reviewer's M3): every integer and float is little-endian (floats as their IEEE bits),
  including ids inside records and values and 6.2's `object id u128` in a leaf (`Node::hash` writes it so), except
  **table keys, where ids are `u128` big-endian**, so a key's byte order is id order. Counts come before lists, and
  `lp(b)` is a `u64` length then the bytes. An optional id is a byte 0, or 1 then the `u128`.
- **Table entries.** Keys are fixed width, so sorting entries by key bytes is the key order:
  - registry: key the component id; value `asset (32) ‖ schema hash (32) ‖ description`, where `asset` is the
    script asset that registered the id (zero for an engine component; slice D, so a script id names its asset, 7.9)
    and **description** = `lp(name) ‖ row
    size u32 ‖ field count u32 ‖ per field, in offset order: lp(name) ‖ kind u8 ‖ offset u32 ‖ count u32 ‖
    lp(unit)`, the unit in 3.6's canonical spelling, empty for none. The kind
    numbering is frozen: U8 0, U16 1, U32 2, U64 3, I8 4, I16 5, I32 6, I64 7, F32 8, F64 9, Bool 10, ObjectRef 11,
    BlobRef 12, SimBlobRef 13, Pos 14, MemberKey 15, EntryRef 16, Hash 17; a new kind takes the next number;
  - lines: no entries in M1;
  - modules (slice D): key the script asset id (32); value `version entry (32) ‖ version op u32 ‖ source (hash 32 ‖
    len u64) ‖ toolchain (32) ‖ sdk (32) ‖ author_module (32) ‖ component count u32 ‖ the version's component ids,
    u128 each, strictly ascending (the tag among them) ‖ activate_at u64 ‖ faulted (a byte 0, or 1 ‖ frame u64 ‖
    kind u8)`. V is `source.hash`, stored once (@chunks-reviewer's L1). Decoding is strict: a repeated or
    out-of-order id, a faulted byte other than 0 or 1, or a kind that is not one of pe-logic's `Fault::code()`
    values 1 to 13 (logic.md 3.6) does not decode; `HostStack` has no code, since it is one machine's own fault and
    never world state;
  - governance: key the member key (32); value the permission bits `u32`;
  - claims: key the object id; value `hand u128 ‖ member (32) ‖ since u64 ‖ until u64`;
  - cooldowns: key `member (32) ‖ object id`; value `until u64`;
  - tombstones: key the object id; value `by (32) ‖ frame u64 ‖ expires u64 ‖ parent (optional id) ‖ place u32 ‖
    world pose (a byte 0, or 1 ‖ canonical Pos ‖ rot 4 × f32 ‖ scale 3 × f32) ‖ Node record ‖ row count u32 ‖ per
    row, in component order: component id u128 ‖ row version ‖ lp(canonical row bytes)`. A tombstone keeps one
    instance per component, as M1 has;
  - hand packs: key the hand id; value `stamp u64 ‖ hold u8 ‖ pose count u32 ‖ per pose: canonical Pos ‖ rot 4 ×
    f32 ‖ grip f32 ‖ engaged u8`;
  - push windows: key the member key (32); value `pair count u32 ‖ per pair, oldest first: frame u64 ‖ |impulse|
    as f64 bits`, in N·s exactly as stored (@chunks-reviewer's M2: no conversion to mN·s).
- **The schema hash** in an object leaf and the registry is `H("pe.schema" ‖ description)`, the same description
  bytes the registry value carries after it. BLAKE3 like every commitment; pe-sim's `u64` schema hash is XXH64 and
  stays out of the tree (6.1).
- **The `Node` record** is 192 bytes, in this order, every integer little-endian: `parent`, `first_child`,
  `last_child`, `prev_sibling`, `next_sibling` (each a `u128` id, 0 for none), `creator` (32 bytes),
  `version_entry` (32), `structural_entry` (32), `version_op`, `structural_op`, `born_on` (each `u32`), then 4 zero
  bytes. **A row version** is 52 bytes: `entry` (32), `op` (`u32`), `tag` (16); pe-sim's
  `_pad` is not part of it. These are the
  format, written field by field: they are not pe-sim's in-memory layouts (an `ObjectId` is stored high half
  first), and a test checks the encoder against the fields.
- **An object's record** is the leaf's preimage after the context: `id u128 ‖ Node record ‖ row count u32 ‖ for
  each row except the `Node` column's (already given), in (component id, instance) order: component id u128 ‖
  schema hash (32) ‖ instance u16 ‖ row version ‖ lp(canonical row bytes)`. The object leaf hash
  is `H("pe.obj" ‖ record)`, so an exported record is checked by hashing it. Canonical row bytes are the stored
  bytes with each field whose registry kind is `Pos` replaced by its canonical encoding (4.5), so they are computed
  from the registry alone (in a tombstone too); a `Transform`'s chunk and offset are plain integer and float fields,
  already one encoding per position.
- **Which objects** an objects trie holds: every object of the world (M1 creates no `Local` or `Session` objects,
  3.1), all in the one domain.
- **The physics leaf** of a domain is the `physics_global` record (9.2), all zero until `write_physics` first runs.

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
    Copy { source: Target, new: u32, parent: Target, index: Option<u32>, size: u32 },   // the subtree; templates
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
- **Limits** per transaction [I, to tune]: 4,096 ops, 1 MiB of payload apart from blobs, and 65,536 created
  objects (`MAX_NEW_OBJECTS`), counting each `Create` and every object of each `Copy`'s subtree; `Undelete`
  restores objects that were in the world before, so it does not count. Over a limit is a
  refusal, not a truncation; the object cap's is `TooLarge { limit }`. pe-world's constants are the one
  authority, which pe-api uses, and they are raised only with a measured reason (@coordinator-opus
  m0b9012f192f5).
- **A copy's size** (@design-reviewer's Low 2 on pe-api, @coordinator-opus mce71b99c0063). `Copy` carries `size`,
  the number of objects in the source subtree as the author read it. If the subtree has another size at the op,
  its depth-first indices would name other objects than the author meant, so the op is refused as
  `Stale { precondition: u32::MAX, .. }`, where `u32::MAX` (`OWN_CHECK`) means the op's own check. No single edit
  is the winner (any object in the subtree may have gained or lost children), so `current` and `winner` are zero,
  as for an absent row (7.6). The cap is checked on the declared size before the subtree is walked.

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
    copier as creator. It checks, in order: the cap on its declared `size`, then the size against the subtree
    (7.1), then index overlap, protections and hands;
  - a `Copy` with `size` 0 is `Invalid` (a subtree always has its root), and the object cap's refusal is
    `TooLarge { limit: 65_536 }`;
  - the edit payload has no format version byte. That is fine while no encoded entry outlives a build; one must
    be added before entries are stored across builds (@chunks-reviewer's note on f1a720b2);
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
- One exception (8.1, agent powers, a later slice): an admin may clear `Protection`'s lock flag, and only that
  flag, so a departed owner's grab-and-push lock does not bind a shared object forever.
- Later (M5): wearing, saving, exporting and fetching the assets of a protected object, under the same owner rule.

### 7.9 Scripts in the world (slice D) [I, for review: r2]

logic.md's N4, N5 and N13, under @coordinator-opus's rulings mf9dbcfe481ea, me5689a5c645c, m028c4bf1d412,
m83562b197235 and m58c1611231fe (with @fable-reviewer's third shape, m1711da962a3b), and @design-reviewer's and
@chunks-reviewer's reviews of r1 (m705a821af13f, m52c574e02ece; m983a52c0cad8). A script attaches to an object by
an implicit **tag** component; there is no binding row and no spawn mark; component ids hash the version (3.2).

- **Ids.** `pe_world::script_component_id(V, name, fields) -> ComponentId` is the one derivation: 3.2's formula,
  with the name length-prefixed (`lp(name)`, 6.6's) and the schema hash of the description built from the name
  and fields (6.6, no id). `Register`'s check and
  pe-replica's binding both call it; pe-logic does not depend on pe-world, so pe-replica derives the ids from V and
  the manifest and passes them to `Loaded::bind`. The modules row's id list is then a cross-check, in id order
  where `bind` wants manifest order.
- **The tag.** Every script version declares one zero-size component of its own, named `"pe.tag"` (an SDK name is
  a Rust identifier and has no `.`, so it never clashes), its id derived like the version's other components.
  `Register` refuses `.` in any other component's name, so no script component can pose as an engine one, which
  the manifest names `pe.X` (@chunks-reviewer's code Low 1).
  Attaching a script is adding its tag (`SetRow` with empty row bytes); detaching is removing it (`RemoveRow`), and
  the script's other rows stay. Every system binds the tag implicitly, so an object may run any number of scripts.
  pe-sim allows zero-field components and empty row writes for this (10).
  - **Its row version** is the attaching entry and op, like any `SetRow`, so a precondition can name it; a tag a
    script's create added (below) has the zero row version of a row no edit added (7.6).
  - **It needs the object's edit right** (item 3): adding or removing it is an ordinary row write under 7.6's
    permission checks and 7.8's protections, so a script cannot attach itself to someone else's object.
    Protecting an object later keeps the tags already on it; its owner removes them to revoke the consent
    (@chunks-reviewer's r2 L3).
  - **Never on a hand** (@design-reviewer's r2 Low 2): adding a tag to an object with a `Hand` row is refused
    `Invalid`, so no script can move a hand outside 8.1's input rules (reach, speed, claims following the hand).
  - **A script's create** gives the new object the script's tag as well as the row it carries, so the script sees
    its own spawns (logic.md 3.3). In v0 a create carries only the script's own components (@iter-bench).
- **The destroy right** is "objects carrying the script's tag" (logic.md 3.3). A script's destroy removes the
  object and every tagged object beneath it that is not under an untagged one; each topmost untagged descendant
  (someone's add-on) becomes a root in place, keeping its world pose and its subtree (the orphan rule). A member's
  `Delete` is different: it carries the member's own rights and takes the whole subtree (7.5). Destroys leave
  tombstones (9.2a), so `Undelete` and the user's delete-order rule hold for script destroys too.
- **Writes.** A script writes its own components and the engine components marked "scripts may write" in 3.2, only
  on objects carrying its tag; the owner's consent to the tag covers the writes, on protected objects they own too
  (m58c1611231fe). Each written engine row passes `World::check_script_row(component, object, bytes)`, which runs
  the edit path's per-row checks with no author: row validity (`Light`'s angles, `Transform`'s range and
  normalisation, field rules) and a held object's pose (`Held`). A refusal faults the script's call (the runner's
  `InvalidRow`, code 11), never clamps. pe-world exports the table of engine components a script may name: the
  manifest name (`pe.Transform`, `pe.Light`, `pe.Appearance`, and the read-only ones), the id and the mark.
- **A superseded version's ids** (@chunks-reviewer's M1). An edit that adds or writes a row, or adds a tag, of a
  script component whose asset's current version doesn't list it (an author who resolved names before a commit
  they hadn't received) is refused as `Stale { precondition: OWN_CHECK, current, winner }` with that asset's
  modules row version as `current` and its entry as `winner`, so the author re-resolves. The registry names each
  script id's asset (6.6). Removing such a row or tag is allowed, for cleanup.
- **`Register { components }`** comes only in a transaction with one `CommitScript`, and registers that version's
  components, the tag included, under the commit's asset. Each id must equal `script_component_id(V, ..)` and none
  may be below 2^32 (`Invalid` otherwise). An id already registered with the same description and asset is a
  no-op; with another description or asset, the transaction is refused (a collision). The registry is a world
  table (5.4), rolled back like the rest; registered ids are never removed in M1.
- **`CommitScript { script, base, source, archive, toolchain, sdk, author_module, activate_at }`** needs `EDIT`.
  - **The source comes first, before any op of the transaction** (logic.md 4.1a; @coordinator-opus m9f5e6b2ce690,
    m043445294e6a). `archive` is the version's source, a canonical archive, carried inside the entry; `source` is
    its reference, derived from it when the entry is decoded (only the archive is encoded), so the two never
    disagree. Over `MAX_SOURCE_BYTES` (256 KiB) the transaction is refused `TooLarge { limit }`, judged from the
    length alone; then `pe_world::script::check_source` must accept the bytes, or it is refused `Invalid`. Either
    way nothing changes and the old version keeps running. `check_source` is consensus code: what it accepts
    decides which commits apply, so it changes only with pe-world's apply rules, and its tests pin accept and
    refuse on exact bytes (a case-collision pair, a reserved name, a non-ASCII segment, `..`, a duplicate,
    unsorted order, an archive over the cap). pe-logic's build service runs the same function before it unpacks.
    The rules are 4.1a's: the magic `pe.src.v0`, files in strictly ascending byte order of path, every path `src/`
    then segments of 1 to 64 bytes of `[a-z0-9_]`, the file segment a 1-61-byte stem plus `.rs`, a whole path at
    most 128 bytes and 8 segments, `src` included (so it unpacks inside Windows's `MAX_PATH`), no Windows
    reserved stem, `src/lib.rs` present, every file UTF-8 (`Cargo.toml` is not in the archive: the build service
    writes it from one template).
  - **The bytes come from the entry on every path that applies a commit:** live, a re-simulation after a late
    entry, rung 1's replay, and pe-diverge's bundle replay all hold the entry and so the archive. Nothing may move
    the source out of the entry without moving this check with it.
  - `activate_at` must equal the entry's frame stamp (logic.md 6.1); anything else is `Invalid`.
  - `base: None` makes a new script and is refused as `Stale` if the asset has a row. `base: Some(v)` must equal
    the row's version, or the op is refused as `Stale { precondition: OWN_CHECK, current, winner }`, with the row's
    version as `current` and its entry as `winner`: the loser rebases (R7).
  - **V is the source archive's hash** (`source.hash`). A commit whose V any other asset has ever had is refused as
    `Invalid` ("same version as asset X"), since the two would share every id, tag included (ruled,
    me5689a5c645c item 2 and m028c4bf1d412: any V it ever had; to revisit with logic on objects, where a copy is a
    reference). The check is exact and costs no history: V's tag id is registered, under another asset.
  - **V equal to the base's V** (a toolchain or SDK change only) re-keys nothing and moves no row version: the op
    only rewrites the modules row (m83562b197235, L4).
  - The op writes the asset's modules row (6.6): version = this entry and op, the hashes, the component ids,
    `activate_at`, and no faulted mark.
- **Re-keying** (ruled, me5689a5c645c item 1; m028c4bf1d412). A commit with a base whose V differs moves every row
  of the old version's components to the new version's same-named component, object by object, **the tag rows
  included** (or every attached object would detach), in the commit's own transaction, so no frame sees half-moved
  rows. Each moved row takes the commit as its row version, so an edit that read the old row is stale.
  - **Field mapping**, deterministic and identical on every peer: a field with the same name, kind, `count` and
    unit copies its value; a new field, or a same-name field whose kind, count or unit changed, starts at zero
    (a unit is meaning, never copied silently: m83562b197235, L3); a dropped field is gone; a component whose name
    disappears loses its rows. Padding fields (names starting `_`) are always zero and are never a loss. A moved
    row the new description's checks refuse refuses the commit.
  - **A loss is a dropped field or component, or a zeroed field, whose value was not zero** (a field that was zero
    loses nothing). A loss on an object the committer could not
    edit (held by another member, or protected under 7.8 by another owner) refuses the commit, naming those
    objects; elsewhere it applies. Lossless moves apply on every object, others' and protected ones included. Why
    (m028c4bf1d412): every M1 script is world-level, the user's "linked script" (mb500e054abf1), so changing it in
    one place changes it everywhere; but an `EDIT` holder on the asset must not wipe a protected owner's data by
    retyping a field. A blocked author can still ship a compatible change (add a field instead of retyping one).
  - **Tombstones are re-keyed too** (@chunks-reviewer's code M1): a deleted object's rows (7.5) move like live ones
    and count in the bound, so an `Undelete` brings back the new version's rows, never ones no system binds. A loss
    there is judged by the protection the object had: its tombstone's own `Protection` row, else its nearest
    ancestor's, live or tombstoned (7.8 covers the whole subtree). Claims end at a delete, so none applies.
  - **Its own bound** (@chunks-reviewer's M2): a re-key moves at most `MAX_REKEY_ROWS` = 32,768 rows [I, to
    measure: the frame-time spike of moving them in one step]; past it the commit is refused as
    `TooLarge { limit: 32768 }`. It is separate from 7.1's per-transaction limits, which count only the author's
    ops. So the largest M1 footprint is a script whose rows (the tag plus its components, per object) total 32,768:
    for example a tag and two components on 10,922 objects.
  - **What it costs is reported.** The commit's footprint lists the objects moved, the row count, and each
    component and field zeroed or dropped; a dry run (pe-api) shows the same before the commit, with the objects
    that would block.
  - **The refusal naming objects** needs a seam addition, `Reason::RekeyBlocked { objects, more }` (the first 16
    blocking objects in id order, and how many more), asked of @engine-architect.

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

### 8.1 The input payload, claims and pushes (r14)

An input entry is pe-log's envelope with class `Input` and this payload, in `edit.rs`'s codec conventions (7.6
As built: little-endian, one-byte tags, flags 0 or 1, `u32`-counted lists, ids as `u128`; strict and total) [I]:

```rust
pub enum Input {
    /// Frames `stamp .. stamp + poses.len() - 1 + hold` of one hand (D73 (a)): pose `j` is the target at frame
    /// `stamp + j`; the last pose is then held `hold` more frames (0..=255).
    Hand { hand: ObjectId, poses: Vec<HandPose>, hold: u8 },   // 1..=16 poses
    /// An impulse in newton seconds, in the world frame, at `at` (a world `Pos`) or the centre of mass.
    Push { object: ObjectId, impulse: [f32; 3], at: Option<Pos> },
    /// Claims `object` for the author's `hand` (7.2), or renews the author's claim on it. `hand` is `None` for an
    /// agent with no hand, which then renews by repeating the `Grab`.
    Grab { object: ObjectId, hand: Option<ObjectId> },
    /// Ends the author's claim on `object`; without one it changes nothing and notes `NoEffect`.
    Release { object: ObjectId },
    /// Sets the author's own hand's mode (below); the mode it already has notes `NoEffect`.
    HandMode { hand: ObjectId, mode: u8 },                    // 0 or 1; anything else does not decode
}
pub struct HandPose { pub pos: Pos, pub rot: [f32; 4], pub grip: f32, pub engaged: bool }
```

**Time limits are integer milliseconds** (@design-reviewer's M1). Each is a world setting, a `u32` of
milliseconds within a bound, refused outside it, and converted to frames as `ceil(ms × hz / 1000)` in integer
arithmetic at the tick rate in effect when the claim or cooldown is set, so every machine gets the same frame:

| Setting | Default | Bound | At 60 Hz |
|---|---|---|---|
| `claim_ms` | 500 | 100 - 5,000 | 30 frames (pe-session's `CLAIM_FRAMES`) |
| `cooldown_ms` | 166 | 0 - 2,000 | 10 frames (pe-session's `CLAIM_COOLDOWN`) |
| `push_budget` | 2,000 mN·s per member per second (a `u32` of mN·s, not ms) | 0 - 1,000,000; 0 turns remote push off | |

**Validity.** Every value is finite and every `Pos` in range (4.3). A pose's rotation must have a norm within
`1 ± 1e-3` and is stored normalized by `quat_normalize` (deterministic); `grip` is in [0, 1]. A failure refuses
the whole entry as `Invalid`.

**What a hand input writes** (a trusted write: no version moves, 7.3). It is refused with `NotYourHand` unless
the author is the `Hand` row's owner. Applied at its stamp frame, pose 0 goes to the hand object's `Transform`
(the target; a hand is a root with no `Body`, so physics never writes it, 8) and `grip`, `engaged`, `stamp`,
`span` and the previous input's `stamp`/`span` to its `Hand` row. The row's `span` is the whole coverage,
`poses.len() - 1 + hold`, and `prev_span` the previous input's coverage, as replica.md 3.3's stream rule reads
them. The whole pack is kept in a world table, `hand_packs` (hand -> stamp, poses, hold; in the root, so a
rollback re-applies the same poses).

**Overlap and coverage** (@design-reviewer's M2.1, M2.2). A later input for the same hand replaces the pack
from its own stamp on: chain order decides "later", and stamps are non-decreasing in a chain by the horizon rule.
`World::end_frame(f)` writes each pending pack's pose for `f + 1` (target, `grip`, `engaged`), after claim and
cooldown expiry, so at the start of frame `f + 1` the `Transform` holds that frame's target. Past its coverage a
pack is dropped and the hand keeps its last pose, with `engaged` as last sent: releasing a grip is the input's to
say, never a timeout's.

**Order within a frame** (M2.3, M2.4). At frame `f`, all hand inputs stamped `f` apply first, in agreed order;
then grabs, releases, pushes and every other entry, in agreed order (the full rule is under "As built" below). So a `Grab`'s reach is measured from the hand's target after
`f`'s hand inputs, and a `Release` and a `Grab` by different members in one frame resolve in agreed order. The
`now` of every input is its stamp frame, also when a late-ordered input is applied during re-simulation.

**Claims** (7.2; ported from pe-session's claims and cooldowns [V archive/v0-prototype `session/sim.rs`];
@coordinator-opus md6eb506d0fce, @fable-reviewer concurring).
- **What may be claimed:** an object with a `Body`. A non-owner may grab a protected object, since 7.8 guards
  taking, not touching. The gap, named and not in this slice: a per-object "others can't grab or push this"
  setting, off by default.
- **A `Grab`** by the author's own hand (or with `hand: None`, meant for an agent with no hand) takes the object's claim, or renews
  the author's own, until `now + claim frames`; the claim records the hand, the member, `since` (its first
  frame) and `until`. It is refused with:
  - `Held { by }` while another member's claim on the object, or on anything in its subtree, is unexpired. The
    reverse is allowed: a lamp on a table someone holds may be grabbed (a cup off a carried tray). A `Grab` that
    renews the author's own unexpired claim checks only ownership of the hand and the cooldown, never `Held` or
    reach, so the tray's holder keeps it when someone takes the cup;
  - `OutOfReach` past reach (7.2), measured from the hand's target, since the hand's simulated body is
    pe-replica's own. A blocked hand can grab within reach of its target. A `Grab` with `hand: None` has no reach
    check. **Any member may send one:** an `Input` carries no actor, and an agent's entries are signed by its
    principal's member (7.1), so pe-world cannot tell an agent from a person (@design-reviewer's slice-B M1);
  - `Cooldown { until }` while the author's own cooldown on that object runs.
- **Implicit renewal.** Every applied hand input whose newest pose is `engaged` renews the claims held by that
  hand to `now + claim frames`. Only an applied input renews: the poses `end_frame` writes from a pack never do,
  so a long `hold` cannot keep a disconnected holder's claim alive. The object drops only when a claim really expires with no valid renewal before
  it: one void renewal followed by a good one within the claim keeps the grip. A client sends an engaged hand's
  input at least every third of `claim_ms`, even when it is still, so a held grip is never left to lapse. There
  is no automatic re-grab after a drop.
- **Release** ends the author's claim at once, with no cooldown (pe-session put the cooldown on expiry only).
- **A hand's mode** (D18b; @coordinator-opus ma93ce7600de1, after @vdw-physics m7a559f785a77). Whether a hand has
  a physics body changes the simulation, so every peer reads it from the world, never from a local toggle; and the
  user switches it within a session, so it is not a frozen config item (6.6). It is a **personal comfort choice,
  per member, on the hand**, like a VR turning mode: `Hand.mode`, 0 = **closed hand**: physical only while the
  hand is closed (its input's `engaged`, the grasp; the default), 1 = **always within reach**: always physical
  within reach (the grab reach of this section's **Reach** bullet: `reach_mm` from the hand's target, plus
  `slack_mm`). The labels are @coordinator-opus's (m33b81fdefd5d), so nobody reads mode 0 as D18b's swept reach
  test; pe-app's binding shows them. Only the hand's owner changes it, with a `HandMode` input (`NotYourHand` otherwise),
  applied at the input's frame like any input, so a replay applies it identically; it is a field of the `Hand`
  row, so it is in the hand's leaf hash, and an import carries it. A world-wide admin cap may come later if a
  world needs one, not in M1. pe-app gives the user a key or controller binding to flip their own mode
  (@engine-architect).
  - **No physics footprint, so no cache** (@design-reviewer's point 1 on 31e565a4): hand bodies are rebuilt from
    the `Hand` rows and `World::hand_mode(hand)` inside the deterministic step at each frame, never cached across a
    rollback or a rung-1 replay, and never read from the view or a local setting. The hand-body work adds a test:
    a mode flip, a rollback across it, and roots equal to a straight run.
  - **A hand body's contact speed is capped like a push** (point 2): a pose input can move a hand any distance in
    one frame, and a kinematic body moved that way would sweep balls at Δpos/dt, past the push envelope above. So
    a hand body's contact speed is bounded by `max_push_speed`; flipping to mode 1 with the hand inside a pile must
    not push the overlapped objects apart violently. How is the hand-body spec's (@vdw-physics); this is the bound
    it starts from.
  - **A hand moving faster than the cap moves at the cap** (@coordinator-opus m3a9a9fa32ff0, option (b)): its body
    trails the target and catches up, so it still pushes and throws at up to `max_push_speed`; there is no cliff
    where hard throws begin. Hard throws top out at the cap, 10 m/s by default and adjustable (8.1's pushes). The
    cap limits the motion of the hand **and everything it holds**: a wrist turn that would carry a held object
    faster than the cap is slowed too (@design-reviewer's Low on 0913f962).
  - **Only a teleport skips collision:** the body is placed at its target without collision, carrying what the hand
    holds, when the target moves farther than the teleport distance in one frame (a tracking glitch or a teleport)
    **or the body trails its target by more than it** (@design-reviewer's Medium on 0913f962). The second bounds the
    lag: a hand kept faster than the cap never trails by more than the teleport distance, so the body never sweeps
    through the scene far from the hand everyone sees. For a held object the separation is **the move the hand would
    carry it this frame**, from where it is, keeping its offset in the hand. **A held object never passes through a
    wall** (@coordinator-opus mc017468679d8): pinned against one, its carried move stays the hand's own small move, so
    it is never placed; the bare kinematic hand may pass through, the held object stays pinned by the hold's spring,
    and springs back when the way clears. **A trailing placement carries a held object only if no static geometry lies
    between** where the object is and where the placement would put it (@coordinator-opus m62b0ae1c56d3, option (b),
    after @vdw-physics measured a pinned ball carried through a 10 cm wall by an overrunning hand's placement): a cast
    of the held object's shape against static shapes only, along its carried move, whose yes/no answer does not depend
    on query order; if it hits, the object stays where it is, held by the spring, and the hand goes on. As built
    (@vdw-physics, ws/hold-reach 9d3a0270), it is Box3D's mover cast: spheres and capsules are swept as themselves, a
    hull as the capsule that encloses it, an over-approximation that errs towards not carrying through, the side the
    wall ruling chose: a hull whose own shape would pass close by a wall's edge may be held back (@design-reviewer's
    Low 1, @coordinator-opus m53c7e29eca9b; the capsule inside it would have let its corners clip the edge; the
    enclosing capsule runs along the longest side of the hull's bounding box, its radius the farthest any hull point
    lies from that axis), and a shape the object already touches is passed unless the move goes into it, so a ball
    resting on the floor can still be carried along it, and so can a box whose enclosing capsule overlaps the floor
    (measured [V @vdw-physics, ws/hold-reach f710c9ac]: a 10×10×30 cm box held on the floor, the hand at 15 m/s along
    it, moves with the hand's body at every trailing placement). If the physics fork has no shape cast, five rays
    stand in, any hit counting: one from the object's centre and four from the points at ±r along two axes
    perpendicular to the move, `r` the bounding radius, the axes chosen deterministically from the move's direction
    and the world's up (world x when the move is vertical) (@design-reviewer's nits), and their limits are stated:
    rays this sparse can miss a post thinner than their spacing, and it can pass beside a wall's end or corner, or
    through an opening narrower than the object, leaving part of the object overlapping the wall for the solver's
    push-out to settle (@design-reviewer's Low, adopted by @coordinator-opus mf154876646fa). A real teleport (the
    target jumping past the teleport distance) still carries it at its current offset, through whatever lies between.
    **Which placement is which:** a placement is a trailing one when the body moved at the cap the frame before (at
    least 99.9 % of it, at the body or at any hold, read from the kinematic velocity in the image); a body that was
    not slowed reached its target, so a jump now is the target's own, a real teleport. **So a real teleport during an
    overrun counts as a trailing placement:** what the hand holds is not carried through a wall, and its spring brings
    it after the hand (@vdw-physics's flagged limit, accepted by @coordinator-opus mc86cae687831 as the safer side).
    **A hand held steadily above the cap cycles:** it trails, is placed, and trails again (at 15 m/s the lag grows by
    5 m/s and reaches 0.5 m in about 0.1 s), so its contacts come in bursts, and each placement carries what it holds
    without collision and gives it no velocity from the jump. That keeps "nothing moves faster than the cap" (plus the
    solver's push-out, below); tests expect bursts, not a continuous push (@design-reviewer, approving r2).
  - **The hold's spring is bounded, speed-aware** (@coordinator-opus m62b0ae1c56d3, decision 2; its floor
    m0ee08c54ed95): the separation the spring that pulls a held object to its place in the hand sees is clamped as a
    whole, and then only its part supporting the object against the world's gravity (6.6, a fixed direction, so the
    clamp does not depend on order) is raised to a floor. Write `e` for the object's error, from the object to its
    place (the way the spring pulls), `ĝ` for gravity's unit direction, and `up(x) = max(0, −x·ĝ)` for the upward,
    supporting part of a vector `x`. With `reach = max(0, (max_push_speed − v_hold) / k)`, where `v_hold` is the
    hand's own speed at the hold (its turn included, ω·r) and `k` Box3D's spring rate at the world's rate (14.7 per
    second at 60 Hz, 15.2 at 120 Hz): first `e1 = clamp(e, reach)`, `e`'s length clamped to `reach`; then, if `up(e1)`
    is below `min(up(e), s_min)`, the difference is added along `−ĝ`, so the spring sees `e' = e1 − max(0, min(up(e),
    s_min) − up(e1)) · ĝ`, recomputed every frame from the current state. It sees at most `reach` plus a support of at
    most `s_min`, in every direction: clamping the parts along and across gravity each on its own would let a diagonal
    error come back at up to √2 times the cap (14 m/s for a ball pinned below and beside its place, then released)
    (@design-reviewer's Medium on ws/hold-reach 9d3a0270, adopted by @coordinator-opus m53c7e29eca9b). Without gravity
    `e' = clamp(e, reach)`. **The floor `s_min`** is a margin times `g / ω²`, the separation at which the spring's
    pull just equals the object's weight, with ω = 2π · 5 Hz the spring's own stiffness, not its bias rate `k`: a ball
    held still in the air sits 9.9 mm below its place at 30, 60 and 120 Hz [V @vdw-physics]. The margin lets the
    spring lift a sagging or knocked object back up rather than leave it hanging at the balance point; it is set from
    @vdw-physics's straight-up and straight-down measurements (the margin is 3, so `s_min` = 3 · g/ω² ≈ 0.030 m at
    9.81 m/s² [V @vdw-physics, ws/hold-reach 9d3a0270; @coordinator-opus mc86cae687831]) (@design-reviewer
    m6d351e8959d5, adopted by @coordinator-opus mb2fbb87e391b). In a world without gravity `s_min` = 0 and the clamp
    is the plain speed-aware one. Without the floor the bound reaches zero whenever the hold moves at the cap, the
    spring stops holding the object up and it falls out of the hand; a floor in every direction instead would let the
    spring pull it down on top of gravity, and a clamp on the error's whole length would shrink its upward part with a
    long trail and let it sag (2.9-5.5 cm measured, @vdw-physics). The floor's only excess over the cap is the support
    it needs, at most about `s_min · k` (0.44 m/s at 0.03 m; measured, at most 0.24 m/s, and only upward, where a
    floor in every direction reached 10.27-10.50 m/s [V @vdw-physics]), inside "the cap plus push-out"
    (@design-reviewer's Medium and its shape m2b9a6c8daa95, adopted by @coordinator-opus mf154876646fa,
    m0ee08c54ed95). Free fall under gravity is not a push. So a held object never exceeds the cap to catch up: it
    trails a hand kept above the cap and catches up once the hand slows, and a pinned object springs back at most at
    the cap (8.2-9.0 m/s measured, against 10-50 m/s unbounded, held back 1-6 m). How is the hand-body spec's
    (@vdw-physics: the hand-side anchor's position, never its frame's rotation, moved towards the object for the step
    only, so images, digests and rollbacks see the original anchor).
  - **The teleport distance** is `max(0.5 m, 2 · max_push_speed / rate)` [I, 0.5 m a starting value to tune]: at
    least twice the cap's distance per frame at the world's rate, so a band of trailing remains at any rate (at
    20 Hz and 10 m/s the cap alone is 0.5 m a frame).
  - **As measured** (@vdw-physics, ws/hands 0eebdfca, 60 Hz, 18 g pit balls [V]): "the cap" holds up to the
    physics engine's push-out:
    - **Swept objects** reach up to 10.48 m/s: a body driven at exactly the cap that penetrates is pushed out at
      Box3D's contact speed (3 m/s). The bound tests assert is `max_push_speed` plus the contact speed.
    - **A held object in a sustained overrun** peaked at 11.8 m/s with the hold's spring unbounded (its overshoot
      after each placement); the speed-aware bound below brings it to 9.9 m/s (9.97 at 120 Hz) [V @vdw-physics,
      ws/hold-reach]. Throws from an 11 or 14 m/s hand then leave at about 9.3 or 9.5 m/s.
    - **Throws** leave at the hand's speed up to the cap (4, 8, 11, 14 m/s give 4.00, 8.23, 10.43, 10.67 m/s). **The
      0.5 m edge:** a throw at hand speed `v` over the cap trails by about `(1 − cap/v) · travel`, and once that
      passes the teleport distance it is placed in its release frame, leaving nearly at rest: 20 m/s over 1.33 m (0.56
      m behind as measured, the hand speeding up; leaves at 0.66 m/s), and by the same estimate 15 m/s over 1.6 m or
      12 m/s over 3 m (@design-reviewer, [I]). M1 keeps 0.5 m (@coordinator-opus m3a9a9fa32ff0): such throws are rare,
      and 1.0 m would let a body run up to 1 m behind the hand everyone sees. If the touch test finds the dead throw,
      the teleport distance is the knob.
  - **Tests (hand-body work):** a target at 1.5× the cap for 2 s, with a held object: the body never trails by more
    than the teleport distance, and nothing it touches moves faster than the cap plus the solver's push-out speed
    (Box3D's contact speed, above); a fast wrist turn with an object held 0.5 m out stays under the cap; a one-frame
    jump past the teleport distance places hand and held object; a ball held pinned against a static wall, the hand 1
    m past it, then the way cleared: never placed through it, and it springs back at most at the cap; a hand
    overrunning the cap past a wall with a ball pinned on the near side, and a ball pinned on a wall's face near its
    end with the hand overrunning diagonally, so that a placement's carried move passes the corner (asserting where
    the ball ends up): the ball is never carried through (measured [V @vdw-physics]: a hand at 12 m/s carried 3 m past
    a 10 cm wall leaves the ball on its side at 30, 60 and 120 Hz, where before the check it was carried through; a
    ball whose path runs 1 cm beside the wall's end slides round the corner under physics before any placement, so
    that case does not reach the check; a ball pinned 3 cm from the wall's end with the hand going diagonally at 25
    m/s at 60 Hz: the shape cast leaves it where it is and it slides round the corner, crossing the face's plane at z
    1.085, where a centre ray would let the placement carry it through the corner region, crossing at 1.020); a held
    ball with the hand at 1.5× the cap for 2 s: **sideways**, its height stays within `s_min` of its place's height
    (the static sag is g/ω² = 9.9 mm) and it never falls below the hand; **straight up**, it rises the whole time at
    no more than the cap plus about `s_min · k` and never falls; **straight down**, the spring adds no downward pull
    beyond the speed-aware reach (with the reach at 0, the ball falls freely until it is below its place, then is held
    up), with gravity's fall reported apart. In every run its speed exceeds the cap by at most about `s_min · k`,
    gravity's fall aside; a ball pinned at 45° below and beside its place, and at 45° above and beside it, released
    with the hand still: its top speed is at most the cap plus about `s_min · k` (measured [V @vdw-physics, f710c9ac]:
    8.39 m/s from a corner below, 8.57 m/s from a shelf above) (@vdw-physics's follow-up; @coordinator-opus
    m0ee08c54ed95; @design-reviewer m558fef25533c).
- **Expiry at `end_frame(f)`.** Every claim whose `until <= f + 1` ends, and its holder gets a cooldown on that
  object until `f + 1 + cooldown frames`; cooldowns past their `until` end. A disconnected holder's claim simply
  expires, freeing the subtree. Claims and cooldowns are world tables, in the root (6, 12.11).
- **Same-frame grabs** of one object by two members: the first in agreed order wins and the second is refused
  `Held`, by the engine's fixed order.

**Pushes** (@coordinator-opus md6eb506d0fce).
- **In M1 a push needs no hand and no reach.** It is bounded by a budget per member per second: the sum of
  `|impulse|` (the `f32` norm computed in `f64`, correctly rounded, so deterministic) over the member's applied
  pushes stamped in `(f − hz, f]` may not exceed `push_budget`; a push that would is refused as `Invalid`, and
  refused pushes do not count. The per-member window is a world table, in the root. `push_budget = 0` turns
  remote push off.
- **`at` is bounded** (@design-reviewer's r14 Medium): it must lie within the object's bounding radius (from
  `Shape`, as reach uses) plus the reach slack, else the push is `Invalid`. Otherwise a tiny impulse far away
  would give an unbounded torque, inside the linear budget.
- A push on an object another member holds is refused `Held { by }`. That covers the held object itself only; a
  push on the table under a held lamp acts through physics.
- Every applied push is recorded with its author in the outcome and the history, so admins can see who did it.
- **Required before any session with strangers:** a push must come from the author's own hand, within reach
  measured against that input stream's hand pose, and a hand's target must be bounded: a maximum distance from
  the member's own rig or head, and a maximum change per frame. Otherwise a modified client puts its hand next to
  anything and neither reach check bounds anything (@design-reviewer's r14 point 4, widening the ruling's (b), accepted by
  @coordinator-opus md0052f879762). This is not optional; it gates opening a world to strangers.
- **Agent powers** (user rulings m14ae4ed5bb9e and m9d1bad95e3f7, relayed by @coordinator-opus mee1bd7557579 and
  m7bc8058b37db, overruling the hand-less gate of me99d54f3776e;
  @item:primordial-engine/agent-actions-within-the-owning-user-s-simulatio). This settles the strangers-gate item on
  hand-less grabs. The design below was decided by @coordinator-opus (m734ae6eaa45a, mba51d7873428, with
  @fable-reviewer's m9471ba7f773f and @design-reviewer's review of 95fc4bfc). It is built in a later slice; slice B
  has none of it (below).
  - **What an agent may do.** Any action at any speed and distance, moving instantly and grabbing objects of any
    size, as long as the object lies within its owning user's simulation range. That is the one bound on where an
    agent acts. Agents may set up "chunkloaders" that force regions loaded, only with their owning user's approval
    (M4 regions). The sim-range check is not in slice B (@engine-architect places it in the plan).
  - **Agent powers are a per-member right, granted by the world's admins.** Admins and the members they trust have
    it; guests don't unless an admin grants it. A person acting as their own agent is the same member, so the right
    is checked on the member, never on a claimed actor (an `Input` names none).
    - A role bit `perm::AGENT` (5.4; `1 << 1`, stored but not checked until agent powers are built) gives the
      default: the admin role has it, Guest doesn't. **M1's interim joiner has it, by ruling** (@coordinator-opus
      m4d09145cb902): the genesis record admits the opener `ADMIN | EDIT | AGENT` and the joiner `EDIT | AGENT`
      (2.4), since M1's slice needs either machine to edit through an agent.
    - A per-member override row in governance, written only by admins, grants or revokes it for one member. Like
      every governance change it applies from its point in the agreed order, so every member applies it from the
      same frame. The member list shows who holds agent powers.
  - **Without the right, hand-less inputs are refused** (`NoPermission`), so reach binds the member: a `Grab` with
    `hand: None`, and a `Push` with `hand: None`. Grabs and pushes both need the member's own hand within reach.
    Hand inputs are unaffected. The push budget applies to everyone, with or without the right.
  - **`Push` names its hand** (@design-reviewer's M1 on 95fc4bfc): it gains `hand: Option<ObjectId>`, as `Grab`
    has. This is a codec change to the `Input` payload above (a world-format version change, like the `Protection`
    layout below). With a hand, the hand must be the author's own (`NotYourHand`) and the push within reach: the
    hand's target within `reach + slack` of `at`, or of the object's bounding spheres (as a grab's reach) when
    `at` is `None`. This is the push rule required before strangers, above. Without a hand, the push needs the
    right.
  - **Per-member caps, set by admins,** in the same per-member override row. An absent field takes the world
    setting:
    - `max_claims`, the number of objects the member may hold at once: world settings 4 without the right and 16
      with it, bounds 1 - 64, starting values to tune. Hand claims count; a renewal does not count again. One past
      the cap is refused with a new additive seam reason, `TooManyClaims { limit }`, from @engine-architect
      (`TooLarge` would read as a size limit, which the user ruled out).
    - `max_chunkloaders` (M4): 0 without the right, a small stated default with it. The owning user's approval is
      still required on top.
    - `push_budget`, as now.
  - **Holds and locks win.** An agent's grab never takes an object another member holds (`Held { by }`, as for any
    grab), nor one under its owner's lock.
    - **The lock** is a flag in `Protection` (`flags: u32`, bit 0 = no grab or push by others), so it follows
      7.8's owner rule and covers the subtree. Unknown flag bits are refused (`Invalid`). Adding the field
      changes `Protection`'s row layout, a world-format version change.
    - Under the lock, any other member's grab or push, by hand or hand-less, **admins included**, is refused
      `Protected { owner }`. The owner's own agents are the owner, so they are not bound.
    - **Admins may clear the lock flag**, and only that flag, not the rest of the `Protection` row. This is a
      deliberate exception to 7.8, so a departed owner's lock does not bind a shared object forever. The clear is
      an edit, recorded with the admin as author. An admin who must move a locked object clears the flag first.
    - The lock refuses grab and push inputs only, not physics: a thrown object can still knock a locked one over.
      To make an object immovable, make its body static.
    - Another member's object parented under a locked subtree can still be taken out by its own owner.
    - **Locking does not leave others' holds alive** (@design-reviewer's nit on fac22bf6) [I]: a claim by another
      member on a locked object is not renewed, neither by a renewing `Grab` (refused `Protected { owner }`, so the
      lock is checked before the renewal path returns) nor implicitly by an engaged hand input. It ends at its
      `until`, at most `claim_ms` after the lock.
  - **Admin force-release:** an admin-only `Release { object, force: true }` ends any member's claim. Admin status
    is checked at the applying frame. It is recorded with the admin as author, and its outcome names the holder,
    who is told who released the object. The holder then gets the normal grab cooldown, so they cannot simply
    re-grab next frame. (An admin-chosen hold-off per member and object is a possible later option, not in this
    design. Revoking the right or removing the member are the stronger remedies.)
  - **Visible holder and visible hand-lessness:** every claim shows who holds it, in refusals and the record. A
    hand-less claim has `hand = ObjectId::NONE`, and `Push` in `Footprint` gains `hand: Option<ObjectId>`, so a
    hand-less action is shown as such ("moved by X, hand-less").
  - **Order of checks** for `Grab` and `Push` (@design-reviewer's Low): the hand (`NotYourHand`), the right
    (`NoPermission`), the lock (`Protected { owner }`), `Held`, the claim cap (`TooManyClaims`, grabs only), then
    reach. The existing decode, object, `Body`, cooldown, renewal, `at` and budget checks keep their places around
    these. A locked and held object therefore gives `Protected` everywhere.
  - **Speed-change caps on pushes** (push tunnelling; @coordinator-opus m55af9087bf7c, @fable-reviewer
    mb724b7534c5e). The N·s budget protects heavy bodies; these protect light ones, which one budget-sized push
    would send at about 110 m/s, through walls (@design-reviewer's arithmetic on the 18 g ball).
    - **Per push:** `|impulse| / m` may not exceed `max_push_dv`. **Speed cap** (@design-reviewer's Medium on
      5da33763, accepted by @coordinator-opus m4ffd5ab9f31f): the push is refused if `|v + Δv|` would exceed
      `max_push_speed`, where `v` is the solver's linear velocity when the push applies and `Δv = impulse / m`.
      That `v` already includes earlier pushes of frame f, because pe-replica's `step` hands each applied entry's
      push to the solver as the entry applies (`bodies::push` right after `apply`), so they are not added again
      and several pushes in one frame cannot each pass (@design-reviewer, pe-replica `step` at ce6b8936). A
      per-second speed-change budget would bound acceleration, not speed: sustained pushing, or several members on
      one ball, would still reach tunnelling speeds, so the speed cap replaces it (no `push_dv_budget`). The N·s
      `push_budget` stays. All apply to everyone, members with agent powers included.
    - Defaults [I, starting values to tune]: `max_push_dv` 5 m/s and `max_push_speed` 10 m/s, as integer mm/s
      settings (`u32`), bounds 0 - 1,000,000, where `max_push_dv = 0` turns pushes off. Admins may raise either per
      member in the per-member override row, with "no limit" allowed (`u32::MAX`).
    - **Mass and velocity come from the solver** (my choice, which @coordinator-opus left open). `m` and `v` are the
      body's mass and its current linear velocity in the solver when the push applies (frame f's physics state
      plus that frame's earlier pushes, as above): mass
      with the body's world scale applied the solver's way (non-uniform scale included) and hull volumes from their
      blobs, which pe-world may not hold at apply. pe-world reads them through a query the replica passes to
      `apply_input` (a seam change, with @vdw-physics). It is deterministic because the physics state at the start
      of frame f and the agreed order of its pushes are the same on every machine, and the replica holds the solver
      there. `BodyState` cannot serve:
      it is written only at cut frames. A push on a body the solver does not hold as dynamic, or whose mass is not
      positive and finite, is refused `Invalid`, never divided by zero.
    - **Spin: in M1 a push acts at the centre of mass** (the option @coordinator-opus left to me). A `Push` whose
      `at` is `Some` is refused `Invalid` until a later slice bounds angular speed change too. So the linear caps
      bound everything a push does. (Slice B accepts `at` within the object's radius; the agent-powers slice
      narrows it.)
    - Over either cap, the push is refused with a new additive seam reason, `TooFast { limit }`, the cap in mm/s
      (from @engine-architect, beside `TooManyClaims`; `TooLarge` would read as a size limit). A push is never
      scaled down. Refused pushes do
      not count against `push_budget`.
    - Physics adds continuous collision detection against static geometry for small dynamic bodies, on the
      deterministic path (@vdw-physics's part of the decision).
    - **The tested envelope, and above it** (@coordinator-opus m6be20e26823b, option (b) for M1, on @fable-reviewer's
      advice m07057e26ab0c). The engine sets no hard speed bound: one would narrow the user's "any speed" ruling, and
      would not close the hole, since throws, collisions and scripts can still take a small body past it.
      - **Tested** [V, @vdw-physics's widened sweep, physics/ccd 1cda443e, 73 aims per case]: spheres of radius down to
        0.5 cm against walls down to 1 cm thick, at 30, 60 and 120 Hz, CCD on: nothing fails at 50 m/s or less, at any
        tested rate, size or wall thickness (deepest press-in 1.3 cm). Above that, two failures, classed apart by
        @vdw-physics's rerun (mbd785e99b99e): small balls can **tunnel** through thin (1 cm) walls (or end embedded in
        them), from 150 m/s at 30 Hz and 75 m/s at 60 and 120 Hz; against the pit's 20 cm walls they never pass through,
        up to Box3D's 400 m/s cap, but small balls (2 cm and under) can end **embedded** in the wall or floor, from 200
        m/s at 30 Hz and 75 m/s at 60 and 120 Hz. No ball is deflected over the 1.2 m walls. The pit's 6 cm ball never
        embeds and tunnels only at 120 Hz, at 400 m/s, through 1 cm walls. Why higher rates fail at lower speeds has a
        mechanism [V, per-step trace]: time of impact leaves the ball at the wall at full speed, the next step's new
        contact is soft, and higher rates have fewer sub-steps per step (8 at 30 Hz, 2 at 120 Hz) to push it back before
        the next collision pass, which with a thin wall picks the far face. The figure is final once the high-speed
        tunnelling run on two machines gives equal state roots.
      - **That two-machine run is an M1 acceptance item** (@design-reviewer's nit): before M1 acceptance,
        @vdw-physics's tunnelling case in the release test binary @engine-architect runs here and on the second PC.
        If the roots differ, a hard bound goes in until (c).
      - **The default does not change:** `max_push_speed` stays 10 m/s. Only an admin's choice goes above the
        envelope.
      - **Above it,** small objects can pass through thin walls or end embedded in thick ones. An admin may still
        raise `max_push_speed` to "no limit"; Box3D's 400 m/s ceiling is the real top (the box3d fork's
        `default_world_def()`, `maximum_linear_speed` 400 × length units, with length units 1, which pe-physics keeps:
        a clamp on the solver's velocity, not a per-step travel limit [V, @vdw-physics]). That is the admin's own choice: the admin interface shows the
        tested limit and warns above it, whenever that interface exists.
      - **(c), after M1** (@vdw-physics, on the docket): confirm the spin cause first (a 0.5 cm ball at 75 m/s
        spins at about 15,000 rad/s), then try an angular-speed clamp or a substep.
    - Tests: the largest allowed push on the lightest ball (`max_push_dv` exactly, then 1 mm/s over); the speed
      cap's edge on a ball already moving, and repeated pushes stopping at `max_push_speed`; two pushes on one ball
      in one frame, each under `max_push_dv`, whose sum crosses `max_push_speed` (the second refused); a scaled body's mass
      (@vdw-physics); a push on a static body and on a body with zero density refused; a push with `at` refused;
      an admin's raised cap applied from its point in the agreed order.
  - **Tests:** a guest's hand-less grab and push are refused, and pass once an admin grants the right; a granted
    member grabs across the room; an agent's grab of a held object and of a locked object are refused, and an
    admin's too until the admin clears the flag; the claim cap's edge (cap, cap + 1, renewal at cap) with and
    without the right; an admin force-release, and the holder's re-grab refused `Cooldown` until it ends; a push
    with a hand, in and out of reach; unknown lock bits refused.
  - **Not in slice B.** In B any member may send hand-less inputs, `Push` has no hand, and there is no right,
    lock, cap or force-release. B stays off the strangers gate's list of done items.

**What the replica gets.** `Footprint` gains the pushes (object, impulse, `at`, author). The replica reads
`hands()` and `claims()` every frame. A refused grab's outcome names the holder (`Held { by }`), which is what
the display layer's "slides out of grip" cue needs (7.2).

New seam reason: `Reason::Cooldown { until: Frame }` (@engine-architect, docked as
@item:primordial-engine/pe-seams-reason-cooldown-until-frame-for-pe-worl).

**As built (slice B)** [V `crates/pe-world/src/input.rs`, `tests/input.rs`]:
- **`span` counts frames** (a correction of the text above): a pack covers `stamp ..= stamp + poses.len() - 1 +
  hold`, so the `Hand` row's `span` is `poses.len() + hold`, as replica.md's `HandRow` reads it ("from its stamp,
  `span` frames"); `poses.len() - 1 + hold` is the last covered frame's offset. `has_prev` is set when the row's
  old `span` is nonzero (a hand made at admission has `span` 0).
- **Reach** (7.2) is two world settings beside the three above [I, for review]: `reach_mm`, the hand's reach from
  its target, default 100 mm, bound 0 - 2,000; and `slack_mm`, default 50 mm, bound 0 - 1,000. A grab is in reach
  when the hand's target is within `reach + slack + r` of the centre of any of the object's bounding spheres; a
  push's `at` must lie within `slack` plus the object's radius, the largest `|c − P| + r` over its spheres, from
  its own world position `P`.
- **Bounding spheres:** one per `Shape` on the body and on its compound children (descendants reached without
  passing another `Body`), centred at the shape object's world position (its frame's, without a `Transform`), with
  radius `size[0]` for a sphere, `size[0] + size[1]` for a capsule, and `|size|` (read as half extents) for a box,
  mesh or hull, times the largest axis of its world scale.
- **Order of checks.** `Grab`: decode, the object (`Unknown`, or `Deleted` with a tombstone), the hand
  (`NotYourHand`), a `Body` (`Invalid`), the author's cooldown, then a renewal of the author's own claim stops
  there; otherwise `Held` (the first claim in id order names the holder), then reach. `Push`: finite values, the
  object, a `Body`, `Held`, `at`, the budget. A hand input: the poses' values, then the hand. A `Pos` that does not
  decode canonically is `Malformed`: a decoded `Pos` is in range by construction.
- A renewing `Grab` that names another of the author's hands moves the claim to it: a new hold for that hand, so
  its reach is checked (`OutOfReach`), though not `Held` (@design-reviewer's Low 1). `until` only ever grows. A
  hand-less claim has `hand = ObjectId::NONE`. A pack whose hand is gone is dropped at the next `end_frame`, and so
  is a pack whose next pose cannot be placed (the hand's frame moved so the pose is out of range): nothing of that
  pose is written, and the hand keeps the last pose and `Hand` row actually written, as past coverage
  (@design-reviewer's ruling on @chunks-reviewer's Low 2). A hand input on a hand with no `Transform`, or whose
  frame has no world pose, is `Invalid`; `OutOfRange` is only a pose its frame cannot hold.
- **A mesh or hull's `size`** is the half extents of the blob's bounding box about the shape's origin
  (@design-reviewer's ruling on its Low 2 and @chunks-reviewer's Low 3). Import writes it, and edit validation will
  check it against the blob once blob validation exists; physics and LOD want the same box. Until then the bound
  trusts `size`, and a zero `size` makes the object grabbable only within reach plus slack of its origin. **A `Body` with no `Shape`** has no spheres: no hand can grab it, and a push's
  `at` must lie within the slack of its origin.
- **A push past the budget is refused `Invalid`**, so a sender cannot tell "wait" from "wrong"; a reason of its
  own is open (an additive `Reason` variant, @engine-architect's).
- **The apply order of a frame** (@coordinator-opus m0d3dfd2c0674, option (a); @fable-reviewer's conditions
  md40c940e2178 are requirements):
  - A frame's entries apply in **phase order first, then agreed order within a phase**. The phase is
    `pe_world::input_phase`: 0 for a hand input, 1 for any other entry, read from the class and the payload's first
    byte. There are **exactly two phases**; another is a spec change.
  - An author's own same-frame entries can therefore apply out of their sequence order: a `Grab` sent before a
    hand input of the same frame applies after it, so its reach is measured after the hand moved.
  - **The phase is part of the determinism contract.** It reads only the entry, never world state; changing it is
    a world-format version change.
  - **Void entries are removed before the sort.** Verdicts are computed in chain order; the sort never decides
    which entries are void.
  - **Hashes stay in agreed order.** Cuts, chains and the change record are unchanged; only the state root reflects
    the apply order.
  - **One helper, one path.** pe-replica has a single "entries of frame f in apply order" function, a stable sort
    by `WorldModel::phase` (default 0; PeWorld returns `input_phase`), and every path that applies a frame's
    entries uses it: `Replica::step`, rung-1 repair replay, rung-2 re-simulation, prediction, merge replay and the
    harness oracle (@vdw-physics owns the helper and the seam). The harness tests a frame where a grab precedes a
    hand input in the agreed order: every member and a replay measure reach after the hand moved.

**Tests the slice must have** (@coordinator-opus): grabbing a held object's ancestor is refused and its child
allowed, in both orders; two members grabbing one object in the same frame; implicit renewal surviving one void
renewal; a disconnected holder's claim expiring and freeing the subtree, with a long-`hold` pack not renewing it;
the tray's holder renewing while someone takes the cup; the push budget and `push_budget = 0`; a push with `at`
outside the bound.

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

### 9.2a The logic stages (slice D) [I, for review: r2]

pe-replica runs pe-logic's runner in the two logic stages, after the frame's entries (9.2's order: entries, logic
before physics, physics, logic after physics, events). pe-world gives it the store and what the runner asks for,
and records what the runner reports. pe-world never depends on pe-logic.

- **`World::logic_stage(frame, f)`** keeps the stage-start state (a copy-on-write clone of the store, which costs a
  reference count per column) and calls `f(&mut SimStore, guard, &faulted, check)`, where
  - `guard` names the objects a script may not remove now: those held by anyone (the claims table), and, once the
    owner's lock exists (8.1's agent powers, a later slice), those carrying it. The runner refuses a destroy if any
    object it would **remove** is guarded (an untagged descendant is orphaned, not removed, so it doesn't count:
    @design-reviewer's correction m52c574e02ece). The set is computed when the stage starts. The logic stages run
    after the frame's inputs, so a grab stamped in this frame is already in it; nothing in a stage can hold or lock an
    object, since scripts write neither claims nor `Protection`;
  - `faulted` is the set of asset ids whose modules row has a faulted mark;
  - `check` is `World::check_script_row` (7.9), for every changed engine row.
- **Destroys are the store's, then tombstoned.** The runner destroys through the store (`SimStore::destroy`, its
  default), because it undoes a refused destroy and every command of a faulting call by restoring the store. It
  reports the objects each surviving destroy removed and orphaned.
- **`World::after_logic(frame, report)`**, with the runner's report in plain types (created with their spawners,
  removed, orphaned, written rows, faults), then
  - **writes a tombstone for every removed object** from the stage-start state, exactly as 7.5's delete does (its
    rows, versions, place and last world transform, copy-on-write), with the frame and the one-minute expiry. The
    deleting `EntryRef` is zero, the world's logic; `Deleted { by }` then shows zero (naming the destroying script
    asset instead is a later nicety, @design-reviewer's r2 nit). Undone destroys were rolled back before this and
    leave none (m028c4bf1d412, after @design-reviewer's Medium 1). An object created and removed in the same stage
    leaves none either: nothing of it existed before the stage, so there is nothing to undelete (r2 Low 1);
  - gives each created object its `SimSize`: 1 m times its spawner's world scale (3.5);
  - sets each faulting asset's faulted mark to `(frame, kind)`. The frame's first fault wins, a mark already set is
    kept, and the modules row's version does not move, so the author's next `base` still matches;
  - returns the physics effects: written `Transform`s of bodies and removed or orphaned bodies, which pe-replica
    acts on as it does for an edit's footprint.
- The runner's row writes and its restores mark leaf caches stale through pe-sim (6.4), as any write does.
- **What runs** is pe-replica's to derive from the modules rows of the frame it simulates (@iter-bench
  m277ac6a09ccf): after a commit applies at frame F, or an import replaces the modules table, it builds and binds
  each listed version (`Loaded::bind` with the ids derived from V and the manifest, the tag's id, and the engine
  table); after a rollback before F it puts back the version the row names then. `World::modules()` lists the
  rows. **A new version runs from frame F's own logic stages:** the commit is an entry, entries run before logic,
  and the re-key has already moved the rows (@chunks-reviewer's r2 L1). A machine still building it binds its old
  module under the new version's ids (logic.md 6.2, @coordinator-opus mdc202526c56f): every component the edit left
  unchanged gets the new version's id, since the id is derived from V and the description, both of which match, so
  the old code keeps running on the re-keyed rows (the user's Q3a); if a component's description changed, the bind
  refuses and the script pauses on that machine until its build is ready. Either way those frames are predictions,
  which the replay from `activate_at` replaces, so nothing reaches world state. This is why `script_component_id`
  takes a description, not a registry lookup.

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
| zero-field components (row size 0) and empty row writes, for a script's tag (7.9); run-time `Register` into the live registry, copy-on-write (slice D) | pe-sim | the "no fields" refusal in `ComponentDesc::new` |

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
16. **Push speed envelope** (8.1; @vdw-physics's test, not pe-world's): a high-speed tunnelling case run on two
    machines from one release binary gives equal state roots. If they differ, a hard speed bound goes in until (c).

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
12. **A child's creation moves its parent's version** (@fable-reviewer, me93a9b15ddd3). So an agent that recolours
    `/Table` with an `ObjectVersion` precondition is refused whenever anyone adds a child to it. If that proves
    common, the parent's own rows and its child list could carry separate versions. Measure before changing it.
    A row `Version` precondition already avoids it.
