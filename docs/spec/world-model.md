# The world model (WS-world)

Status: **r12, 2026-11-11.** r11 is on world/core-v1 (with the first WS-world code), reviewed piecewise. r12 follows
pe-seams S2 v1 (c93fa631): the tree is addressed by nibble prefix, and `pe_seams::Node::hash` is the single definition
of every tree hash (6.2, 6.5); it adds a hierarchy depth limit (3.3, @chunks-reviewer's Q2 via @coordinator-opus); for review of those parts. Written by @world-model. Nothing in it is built.
Reviewers: @design-reviewer (section 6, the hash tree, and section 7, the edit entries and their conflict rules);
@chunks-reviewer (everything else). Seams S1, S2 and S5 are proposals to @engine-architect until pe-seams v0 is fixed.

Sources: the re-founding plan (`docs/refound-plan.md` on `docs/refound-plan`, b610464), sections 2.1, 3.1, 3.2 and 5;
@fable-reviewer's review files 04, 11, 14 and 17 (`<notes>`); the user's decisions of 2026-11-10 in
`<notes>`; the code of `crates/pe-core` and `crates/pe-sim` at main 926bc9d.

Marks: **[V]** verified in the code or record named; **[I]** my design judgement, open to review. **[U]** a user
ruling, cited by message id. Where this file and a user ruling disagree, the ruling wins and this file is wrong.

## 0. What this file fixes, on one page

- **The world is data.** A world is a tree of objects. Each object has a 128-bit id, a parent, ordered children and
  components. Variable-length data (names, meshes, textures, script sources) lives in content-addressed blobs,
  referenced by a 256-bit hash. The ball-pit template is a set of edits, not code.
- **Everything that matters is hashed** in one tree: objects, their components, edit versions and physics state, and
  the world tables (the component registry, script versions and their pending commits, governance, claims,
  cooldowns, tombstones, pending hand packs, push windows, field modes and the feed). The root goes in every cut. A mismatch descends to
  the objects that differ, in a few rounds.
- **One edit entry, three conflict rules,** applied in the agreed order:
  1. *exclusive holds* (grabs): the first wins;
  2. *edits with a precondition* (the version the author read): refused if stale, naming the entry that won;
  3. *plain writes*: the latest in the agreed order wins, and the overwritten author is told.

  A transaction applies all or nothing. The user's rule for a delete against an add-on is the
  ordinary rule, live and in a merge, not a special case.
- **Ids are derived, never counted.** An object's id, and a new script asset's (7.9), is a hash of the entry that
  created it, so it is the same on every machine, before and after a rollback, and in a merge, whatever the arrival
  order.
- **Positions cannot become numbers by accident** (the user). `Pos` has no readable fields. The only
  way to raw numbers is `pos - Pos::ORIGIN`, which gives an `Offset`. The engine obeys the same wall as scripts. The
  chunk index never saturates: an out-of-range result is a refusal, and its encoding already reaches past 64 bits.
- **Field modes** (3.7, design only): a field is synchronized (the same for every player), decoupled (one value
  per player, and any player's value can be known by all) or desynchronized (one per player, private). Most fields
  are synchronized. As little as possible is published, and a value drawn per frame never reaches the shared world.
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

**The world line tool** (the user; fable-review file 19;
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
- **Carrying between worlds** (copy then remove, Q7 [U]) creates a new object in the destination by an
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

The user: "i like the idea of there not being any real distinction between different worlds, and all
worlds can be descendant of one singular world "origin". all worlds can be merged with all other worlds, and if they
share history, that history is kept." And: "the origin is an empty world, and there are also other
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

**Narrowed for M1** (@coordinator-opus, option (a), with @fable-reviewer's conditions).
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
- **An interim session's line comes from its genesis record** (@engine-architect, @world-chunks; @coordinator-opus gives the call to world-model). Until join entries exist, an M1
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
  include the admission frame, as 8 intends), once a leave has removed the old ones. The avatar root that should
  parent the hands (3.5) is not built yet; its id derivation is open.
- **A returning member** (F3's n = 2 branch; @diverge-eng's design v5, L3;
  @item:primordial-engine/f3-l3-pe-world-rules-for-the-n-2-branch-re-issue)
  [V `a_branch_keeps_every_object_and_a_returning_member_keeps_its_hands`]: an admission of a key that is already a
  member keeps its permissions (the admission's are ignored), its hands and what they hold, and creates only a hand
  side it lacks. A member with both hands is admitted with no change at all, and its existing hands are returned. A
  branch removes no member, hand, claim or permission, so B admitted on the new line finds its rows from the old one.
  Within one line S3 voids an `Admit` of a key already in the epoch (`NoChange`), so a replica that steps one before
  the void is known changes nothing. Whether a member votes is pe-agree's epoch, never a world rule. `m1_session`
  refuses a key listed twice itself (`WorldError::Duplicate`).
- **The line switch** (`World::switch_line`, trusted; F3 L3): at a branch's frame the replica continues the world on
  the new line. Only the config record's line id changes, so the root moves through it (6.6, config item 2) and every
  object keeps the id it was made with on the old line.
- **The branch record** (`World::begin_branch(parent, tails, sealed)`, trusted; F3 L3, @diverge-eng)
  [V tests/branch.rs]: right after the line switch, before any entry of the new line, the replica records what the
  agreed `Branch` binds: the parent line, each chain's (member, head seq) at the branch's frame, and the sealed
  originals above those heads, each as (chain, seq, entry id, `payload_hash`). `payload_hash` is
  `BLAKE3("pe.branch.payload" ‖ payload bytes)` over the original's payload exactly as signed. The record is the
  `lines` table (6.6), so the root moves with it; a later branch replaces it. Refused, with the world unchanged, for a
  record no `Branch` can bind: the parent is the world's own line (the switch comes first), a chain listed twice, a
  sealed original not above its chain's head, or one listed twice. The founder gains no right from it: it changes
  no member, permission, hand or claim, and saved copies of either line stay out of world state
  (@coordinator-opus).
- **Re-issued edits keep their original's ids** [V tests/branch.rs, with a control: deriving from the entry's own
  coordinates fails the three id tests]. A member re-issues its suffix on its new chain. The payload is
  `0xF0 ‖ parent line ‖ chain ‖ seq u64 LE ‖ entry id ‖ the original payload bytes, unchanged` (105 bytes before
  the original). No transaction starts with 0xF0, since its first byte is the actor's tag, so the two never read
  alike. pe-world applies it only if the reference names the record's parent line, a chain whose member is the
  entry's author, a seq above that chain's head, and a sealed original with that entry id whose `payload_hash` is
  that of the bytes after the reference; otherwise it is refused as `Invalid`, with nothing changed. The
  transaction then applies as any edit, except that its new object ids, row tags (7.6) and new asset ids (7.9)
  derive from the original's line, chain and seq (the author is the same), so they are the ids the original made,
  and a later re-issued edit that names them by id finds them. Its versions (7.6, and a tombstone's `by`) name the
  original entry too (@diverge-eng's M1), so a later re-issued edit's `Version` and `ObjectVersion` preconditions,
  read on the parent line, hold on the new one
  [V `a_re_issued_edit_s_preconditions_read_on_the_parent_line_hold`; control: naming the applying entry gives
  `Stale`]. A version on the new line may therefore name an entry of the
  parent line's history, which a reader looking an entry up by version finds there. An applied re-issue takes its
  original out of the record, so each is re-issued once; a refused one changes nothing and its original stays
  sealed.
  - **A second branch** nests: a re-issued edit that is itself re-issued keeps its inner reference in the bytes
    the new record seals, and its ids derive from the innermost original. The inner references are bound by the
    sealed hash and were checked when they first applied; they are not checked again (at most
    `MAX_REISSUE_DEPTH` = 8 deep, else `Malformed`). The record is single-generation (@diverge-eng's L2): a later
    branch replaces it, so an original of the first branch still sealed then can never be re-issued. Under design v5
    that can't happen, since B's re-issues apply before its promotion, and only then can a second branch occur; the
    caller (L5) founds a second branch only after the first's re-issues are done.
  - **Room for the prefixes** (@diverge-eng's L1): an edit's encoded bytes are at most `edit::MAX_EDIT_BYTES` =
    `MAX_PAYLOAD - 8 · 105` (longer is `Malformed`), so any edit can be re-issued across 8 branches within the
    entry's cap [V `an_edit_leaves_room_to_be_re_issued_across_every_branch`]. The cap is checked where an edit
    decodes, which pe-api's dry run does before it signs, so an author through pe-api hears of it before submitting
    (@diverge-eng's nit). No recorded edit is that large, so no line replays differently under it.
  - **Other readers** decode an edit with `branch::decode_edit(signed entry)`, which gives the transaction and its
    `Origin`: the coordinates its ids derive from and the entry its versions name, for a plain edit and a re-issued
    one alike. pe-app reconciles a re-issue with its original by `Origin.entry` (S5, @diverge-eng). pe-replica's
    prediction uses it; pe-api's and pe-app's own decoders read a re-issued payload as malformed and skip it, which is
    harmless until they need to show one (an item for @api-eng and @engine-architect when the branch is built end to
    end).
  - Only edits are re-issued with a reference. Input and protocol entries create no ids, so re-sending them needs no
    pe-world rule.
- **Where a joiner stands** (`World::join_spawn`; @engine-architect): a member admitted by an `Admit`
  entry at frame F stands at the feet of the lowest slot along genesis's row (slot i at `origin + (0.6 i, 0, 1.5)`,
  as `m1_session` admits) that no current member's hand takes. A hand takes a slot when it is horizontally (x, z)
  closer than 0.3 m, half the spacing, to the midpoint of where that slot's two hands start. It is a function of
  the world as the replica stands when it applies the `Admit`, after F's earlier entries in the final log's order
  (a hand entry in the same frame may come before or after it, and the two orders give different spots, each the
  same on every replica). The hands' poses are hashed state, so every replica admits at the same spot. It keeps no
  counter and changes no root. A leaver's slot comes free when its hands go, and a slot someone's tracked hands
  still crowd is skipped. A hand takes at most one slot, so of the first `n + 1` slots, with `n` hands, one is
  free [V `a_joiner_stands_at_the_lowest_slot_no_hand_takes`]. It is an M1 rule, for one join at a time and a
  handful of players: the row grows along +x without bound, so a slot is not always on the floor, and only hands
  are checked, not a player's body, so a slot may be where a player walked. A spawn point the world carries (an
  avatar spawn object, a home template's) replaces it when one exists (@design-reviewer's nits).
- **A leave** (`World::leave(member, entry)`; @coordinator-opus, after
  @fable-reviewer): the world side of the member's final `Leave` entry (pe-agree's epoch change,
  agreement.md 8), applied at its stamp on every replica, as a join is by `admit`. An admin's `Remove` of a member
  (S3 v1.13) applies the same for the removed key, at the `Remove`'s stamp (pe-replica). It is all or nothing, and refused
  with `WorldError::NotAMember` for a key that holds no permission bits. That is its only error, since every step
  below is infallible: a hand's child that can't keep its world pose under the hand's parent becomes a root. In
  order:
  1. **The hands are removed outright,** with no tombstone, so no edit can undelete one; their hand packs go with
     them, and so does any tombstone of a hand of the member's that an edit deleted. A hand's children first move to
     the hand's parent (M1's hands are roots, so they become roots), keeping their world pose and the hand's child
     order, versioned by the leave's entry.
  2. **Every claim by the member ends,** as a release does (7.2): the object keeps its velocity and no cooldown
     starts, so another member may grab it on the next frame.
  3. **The member's input rows go:** its cooldowns and its push window. A later readmission starts clean. A leave and
     rejoin can't be used to skip a cooldown or refill a push budget at will, since a rejoin needs an admin's
     `Admit`: two epoch changes and a person.
  4. **Its permission bits are dropped,** so any later entry of its is refused (`NoPermission` for an edit; an input
     names a hand that no longer exists). pe-agree also makes `Leave` the last entry of the leaver's chain.
  5. **Pending script commits are left to logic.md 6.2a:** the voters stay those at `c`, and an attestation given
     before the leave still counts. A commit whose quorum the leave makes unreachable is voided at that frame's end
     by 6.2a's shared void rule, as `QuorumUnreachable` (`VoidKind` 5), rather than at `D`. The leaver's own pending
     commit voids at activation as `AuthorRevokedAtActivation` if it reaches its quorum, at that frame's end as
     `QuorumUnreachable` if the leave leaves it unreachable, or else at `D` as `Unattested`; a readmission with
     `EDIT` before its `F` would let it activate, since `EDIT` is checked at `F`.

  Everything else the member made or set stays: a leave is not a delete. Its objects keep their rows, and the
  protections it set stay. Until agent powers (8.1) lets an admin clear a lock, nobody can lift those protections.
  By the user's rule a session always has an admin: the last admin can't give up the role without passing it on,
  and the session closes when the last admin leaves [U]. **Not built:** pe-agree makes the last
  admin's `Leave` effective whenever another member remains (only the last *member* is kept, `LastMember`), and
  nothing then closes the session. M1 can't reach it, since every player is an admin; it is WS-gov's, needing
  pe-agree to void such a `Leave` or the app to close the session, once a session can admit non-admins
  (@design-reviewer's Low). The root after an admit and its leave equals the root before the admit, apart from what
  the member made and changed.
- **An interim session's world** (@engine-architect's `pe_app::genesis`; @coordinator-opus) is `preset::m1_session(&Session, blobs)`: the preset's world on the session's line (`Session.preset`:
  `Preset::BallPit { balls }`, the pit of `ball_pit_on`, or `Preset::Plane`, below), at the session's rate, with its
  gravity and pe-agree's settings record (config item 4; in M1 that is pe-agree's 32-byte `NodeConfig::consensus_hash`),
  and every member admitted at frame 0 through `World::admit` in key order, each with the permission bits its role in
  the genesis record gives (opener and joiner both `ADMIN | EDIT | AGENT`, by the user's session default [U]; @coordinator-opus, built in @engine-architect's join branch; until it lands, this
  interim genesis gives the joiner `EDIT | AGENT`), standing 0.6 m apart just outside the pit's near wall. A key listed
  twice is refused (`WorldError::Duplicate`). So the root covers each member's bits, and the record and the world cannot
  disagree on anything the root covers; the rest (the cut interval, the epoch, the nonce) is bound through the line
  (2.4, @design-reviewer's nit). The second machine rebuilds the world from the record and compares roots, then the
  replica's frame-0 root, before it starts. A test pins the frame-0 root and runs on Windows and Linux [V
  engine-architect, `crates/pe-world/tests/preset.rs`].
- **The plane** (the friend-test prototype,
  @item:primordial-engine/friend-test-flat-plane-genesis-preset-with-templ; @coordinator-opus,
  interface with @xr-eng). `preset::plane_on(line, seed, blobs,
  origin)` builds, like the pit, one genesis transaction by world logic with the M1 marker, its ids on a chain from
  `pe.m1.plane.chain` and the seed. Its roots (`preset::plane_names`):
  - `Plane`, at `origin`, holding `Floor` (a static box 64 m square and 1 m thick, its top at `origin`), `Lamp`
    (a point light 3 m up) and `Course`.
  - `Course` (`preset::course`; the user's for the 11-27 friend test,
    @item:primordial-engine/friend-test-11-27-the-plane-preset-as-a-movement): a movement and physics test course of
    static pieces, one plain frame per station: `Slopes` (10, 20, 30, 40, 45, 55 and 70° up to a 1.5 m plateau; the
    mover holds a player up to about 50°), `Stairs` (steps of 0.1, 0.2, 0.3 and 0.4 m up to a 1.2 m landing; the
    mover steps 0.3 m), `Ledges` (0.5 to 1.5 m), `Halfpipe` (radius 2.5 m), `Basin` (a round bowl 16 m across),
    `Walls` (a corner, one at 45°, a doorway, a corridor half under a 2 m ceiling), `Beams and gaps` (beams 1, 0.5 and
    0.25 m wide, gaps of 1-3 m, a winding path) and `Rough ground` (half-buried spheres and tilted slabs). Every piece
    is a static box or sphere: meshes and hulls stay off in every preset (config item 8), so curves are arcs of
    overlapping box segments. The layout is a constant built from `+ - * /` and `sqrt` only, never from `sin` or
    `cos`, so every machine builds the same root. It keeps clear the ground at the spawn row (x ±7 m, z -10..4 m;
    `course::SPAWN_CLEAR`) and ahead of it for the spawn grid (x -4.5..7 m, z -30..-3 m; `course::GRID_CLEAR`)
    [V `crates/pe-world/tests/plane.rs`; walked on the mover, `crates/pe-replica/tests/players.rs`].
  - `Spawned`, at `origin` and empty: spawned objects go under it, and "clear" deletes its children.
  - `Templates`, a template folder (7.1) parked 50 m below the floor, holding `Box` (a 0.2 m cube), `Sphere`
    (radius 0.1 m), `Capsule` (radius 0.07 m, segment 0.2 m) and `Box stack` (a plain parent over `Box 1` ..
    `Box 5`, 0.15 m cubes stacked bottom up with 2 mm gaps). Each is a dynamic, grabbable, sleep-capable body at
    300 kg/m³.

  A spawn is pe-api's `create` with `template`, under `/Spawned` (api.md 5.3.1), so a `Copy` (7.1). The stack comes
  over whole, and `components` apply to the copy's root. The templates take 11 of the world's shapes (7.1; the 3 heavy ones, `preset::HEAVY`), the floor 1
  and the course `course::piece_count()` (141).
  Members are admitted at the pit's spots. pe-app's genesis record names the plane `"plane"` v4 (v2 the player
  bodies, v3 the `World settings` root, v4 the course and the 64 m floor), its ball count 0,
  beside `"m1-ball-pit"` v1, and pe-app opens a session on it with `--preset plane`. Its root is deterministic and
  differs from the pit's [V `crates/pe-world/tests/plane.rs`].
- **No dependence on the preset's ids.** Tests, the harness and API examples find the pit's objects by name
  (`preset::names`, `preset::plane_names`, `preset::find`), never by id.
- **Determinism:** the same arguments give the same state root, and admitting the same member at the same frame
  gives the same root again [V `crates/pe-world/tests/preset.rs`].

### 2.5 Domain content and the home domain (M2, design domain-eng d1)

Players call worlds **domains** (the user's ruling); the code keeps "world".

- **Content** (`pe_world::content`; @coordinator-opus, C2 "save content"): objects outside any world,
  each with its rows (no `Node`, no `BodyState`) and the bytes of every blob they reference. No ids, row versions,
  velocities, holds, members or history. A top-level object's `Transform` is stored relative to the content's
  origin. Putting content into a world is ordinary `Create` ops (`Content::transactions`), split into edits that
  each fit one entry, so the objects get ids derived on the destination's line (2.1, "Carrying between worlds") and
  every row passes the edit path's checks. Its bytes are versioned (`pe.content`, v1) and decode checked, never
  panicking. In v1 only engine components are carried: script rows are left out and counted (`skipped_rows`) until
  modules travel with content. `Pos` fields inside rows and object references are copied unchanged, as `Copy` does.
- **The home domain** (`preset::home`, the genesis record's preset `home` v1; the user's rulings, 11-27): the player's local "My Domain", an ordinary world built at frame 0 from its
  scenery (a static floor 32 m square, a lamp and the `World settings` root with its limits: one transaction by world
  logic, as the plane's) and then its content (further transactions on the same chain). The save keeps the content
  only: every root except the scenery (`home::scenery_ids`, a function of the line and seed) and members' hands and
  bodies (`World::content_roots`). Ids re-derive in each session; a save and reopen keeps every object and row, and
  its placement to within rounding [V `crates/pe-world/tests/content.rs`].
- **The starter set** (`home::starter`): a new home's content, the objects that ship with the game, each top-level,
  dynamic and grabbable (physics on by default). Each carries a `Starter { key }` row (id 15,
  provenance only), so "Restore starter set" (`home::missing_starters`, C7) adds back exactly the starter objects a
  home no longer holds anywhere.
- **Swappable.** The home's files (pe-app's `home`) hold encoded content; @fable-reviewer's git layout (M3+) replaces
  them, one object per file then, with the same rows.

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
  - **a mesh shape may be on any body, moving ones included** (the user answer 1; [I, draft for
    @fable-reviewer, built in the bullet work's part 2 after M1, with hulls, which can't be built today either
    (replica.md 2.1)]; @coordinator-opus's decisions, after @fable-reviewer):
    - **only in a world that builds them:** meshes and hulls are built only where the world's blob-shapes setting
      (6.6, config item 8) is on. It is set when the world is founded and is off in every preset; off, a mesh or
      hull shape is `Unbuildable::Shape`, as before blobs (@coordinator-opus, after @vdw-physics and
      @fable-reviewer);
    - **it sits at its body's origin, unturned (v1):** Box3D's mesh shape has no local transform, only a per-axis
      scale in the body's frame, and baking a child's offset or turn into the vertices would change their bits
      (and baking its scale would judge the drop after scale, against (a) below). So a mesh is built only where its
      shape's transform, **composed down the chain** from the body to the shape, is exactly the body's: the
      position as the build uses it (the `f32` that pe-replica's `part()` casts from `f64`) all ±0, and the
      rotation's x, y and z all ±0 with w ±1 (the composed rotation's sign is not canonical, so (0, 0, 0, −1)
      passes). That is the body's own `Shape`, or a child at its origin, unturned. Anywhere else the body is
      `Unbuildable::Shape`, with the reason: "a mesh child must sit at its body's origin, unturned: snap it there,
      or make the mesh the body's own shape". The limit is wide: one misplaced mesh child makes the whole body
      unbuildable, and imported models usually carry their meshes on child nodes. Today's workarounds are the
      body's own shape, or one static body per mesh node. The lasting lift is at import, which bakes a node's
      offset and turn into a new blob in the body's frame, not baking at build;
    - **it never collides with another mesh.** It collides with spheres, capsules, boxes and hulls: Box3D has no
      mesh-against-mesh contact [V `box3d-fork/src/contact/lifecycle.rs`, `can_collide`]. So two moving meshes pass
      through each other, and a moving object made only of meshes falls through mesh scenery, a mesh floor included;
      it needs a primitive or a hull among its shapes. A bullet (below) doesn't change this, since a bullet's own
      shapes are primitives. Lifting the limit would take a new narrow phase in the fork (about 2-4 engineer-weeks
      [I, @fable-reviewer]); the usual answer for a concave object that rests on meshes is splitting it into hulls
      at import;
    - **its mass** is its density times the volume it encloses, with that solid's centre of mass and inertia, found
      by the divergence theorem over the mesh **as the fork builds it**: one signed tetrahedron per built triangle, in
      the fork's triangle order (its BVH order, the same on every machine), from the centre of the built mesh's
      bounding box (of the scaled positions), summed in `f64` and rounded to `f32` once (a sum from the shape's
      origin in `f32` loses precision for a mesh far from it), with the body's world scale applied first. It is
      computed in the fork's `compute_shape_mass`, in Box3D's own composition and shape order (a FORK.md
      deviation), once per mesh, and scaled at build, never in the step; a body without a mesh keeps a bit-identical
      mass (@coordinator-opus, Q4). The built mesh is judged, not the blob, because it is the surface
      the physics simulates (@coordinator-opus, after @vdw-physics and @fable-reviewer):
      - **the drop:** Box3D's `create_mesh` leaves out every triangle with an area under 0.01·slop² (2.5×10⁻⁷ m²,
        a triangle about 0.7 mm on a side, or a sliver 1 m long and 0.5 µm high) [V `box3d-fork/src/mesh/bvh.rs`,
        `collect_primitives`]. A dropped triangle is a hole in the collision surface too, so it is out of both the
        closedness test and the sum; a triangle with two equal vertex positions is one of them. The drop is judged in
        the blob's own units, before any scale. So (a) a `Scale` edit never changes which triangles are dropped or
        whether the mesh is closed (a mirroring scale reads its winding back); only the volume floor is judged after
        scale, so shrinking a small mesh below 10⁻⁹ m³ leaves it without mass (@vdw-physics); (b) a mesh authored
        tiny and scaled up loses triangles that are large in the world, an author-facing limit that the import
        warning (below) names;
      - **the scale floor:** any axis of a mesh's or a hull's world scale (parent scales included) under 1e-6 in
        magnitude is refused and the body isn't built. Through pe-replica, a mesh's and a hull's alike are caught
        first, as `Unbuildable::Shape(ShapeReason::ScaleFloor { axis })`; pe-physics's `InvalidValue`, "mesh scale x
        is under 1e-6 in magnitude, Box3D's floor on scale: rescale the mesh at import", stays as the backstop for
        a direct caller (@vdw-physics, CCD part 2 D).
        The reason names the axis and the floor (@coordinator-opus, after @vdw-physics and
        @fable-reviewer). The fork would otherwise raise such an axis to the floor, keeping its sign
        (−0 to +1e-6) [V `box3d-fork/src/math_functions/vector.rs`, `safe_scale`], so the mesh would collide and
        weigh larger than it is drawn. It is a floor on scale, not on size. Upstream Box3D's floor is 0.01, and it
        was the engine's until E2 lowered it (@coordinator-opus, after
        @physics-2's E2 spike). Box3D applies a mesh's scale exactly: the narrow phase gets each triangle as `scale × vertex`,
        in world units, and the triangle queries and rays use `1 / scale` on world-unit bounds. So 0.01 was no
        numeric limit: on a mesh flattened on one axis, contacts stay right to 1e-38, mirrored or not, and rays hit
        to 1e-38 from 1 m away and to 1e-35 from 1 km (1000 / scale overflows below), and a uniform scale to 1e-8
        neither falls through nor gives a NaN [V, E2 spike f1, f3, f7]. A scale of 0.01 or more steps bit for bit
        as before. 1e-6 keeps a wide margin and still lets a 10 km blob reach 1 cm; a millimetre blob at 0.001 and a
        1/200 miniature build. What limits a small mesh is its size in the world, against Box3D's tolerances, which
        are fixed in metres (next);
      - **small meshes, known limits** (E2 spike, @physics-2): they hold for a mesh authored small at scale 1 as much
        as for a scaled one:
        - everything rests 4.93 mm above a mesh (Box3D's `mesh_rest_offset`, the linear slop, 5 mm), whatever the
          sizes [V, E2 spike f3, f5; pe-physics `a_mesh_scaled_under_the_old_floor_collides_where_it_is_drawn`]: a
          1 cm ball floats half its size, and objects of 3 mm and under also slide. A hull floor holds a 1 cm ball
          to within 1% of its size and a 1 cm box to within 5%. Scaling this one offset per contact pair to the
          smaller shape (the user's ruling) was measured in E2 and does not hold: bodies sliding across a mesh then
          catch on its internal triangle edges, more so the faster they go, so the 5 mm stays for now while those edge
          contacts, and a mesh inset 5 mm instead, are studied;
        - a mesh contact gathers its triangles from the other shape's box widened by 7 cm in world units, so on a
          small dense mesh it gathers many. Upstream Box3D kept at most 512 (`MAX_MESH_CONTACT_TRIANGLES`), and a
          dense mesh dropped what landed on it: on a 4,608-triangle terrain 20 cm across, 7 of 9 balls fell through
          (E2 spike f6). A fall-through at an allowed scale is a bug (@coordinator-opus), so the fork
          keeps every triangle in the bounds, sorted by index as upstream sorts them [V
          `box3d-fork/src/contact/mesh_cache.rs`, `query_mesh_triangles`; pe-physics
          `a_dense_mesh_holds_what_lands_on_it`]. A contact then costs its triangles: on a dense mesh, much more than
          on a coarse one (costs in FORK.md);
        - the drop (above) is judged in the blob's units, so a mesh shrunk by its scale keeps triangles that the same
          surface authored at its world size would lose (E2 spike f2), and pe-import bakes units into the vertices,
          so a model imported in millimetres loses its triangles under 0.7 mm, below what the tolerances resolve;
      - **a `Scale` edit rebuilds the body** (pe-replica's `Rebuilt`: the body is removed and added again), never in
        the step. The rebuild sums the mass twice, once in pe-physics's refusal check and once in Box3D's
        composition. Each sum runs the closedness test's sort again, since the fork's mass sees only the built mesh,
        so a rebuild is O(n log n) in the triangles. With the built mesh kept by blob (part D keeps it), a rebuild
        measured 0.16 ms at 1,000 triangles, 2.4 ms at 10,000 and 8.9 ms at 50,000, near the blob cap. Without it,
        each rebuild also builds the mesh from its blob again: 0.45, 5.2 and 22 ms, and about as much again for its
        back with both sides [V @vdw-physics, rough: release build, medians on a shared machine at idle priority,
        `examples/mesh_cost.rs`]. So a scale animation on a large dynamic mesh pays that once per change. Caching
        the unscaled moments and transforming them would give other bits than the scaled sum; that is a later,
        measured change;
      - **holding:** a held mesh is checked against walls by a capsule around its scaled vertices, as a hull is
        (replica.md 3.3, `static_in_way`). So a held object made only of meshes never passes through a wall, but
        the capsule is coarser than the mesh: a held hoop can't be passed over a static post;
      - **the weld:** Box3D can weld vertices that lie within a tolerance of each other before the drop
        (`MeshDef::weld_vertices`, `weld_tolerance`) [V `box3d-fork/src/mesh/create.rs`]. pe-physics builds meshes
        with welding off, the `MeshDef` default (@vdw-physics, stated at the call), so the test reads the blob's
        positions bit for bit. Were welding turned on, the test would read the welded positions, and its tolerance
        would be stated here;
      - only a **closed** mesh encloses a volume. Closed means every edge of the built mesh is in exactly two
        triangles, once each way, edges compared by their vertices' **positions**, bit for bit with −0 read as +0,
        not by index: exporters split vertices at seams and hard edges, and a closed mesh would otherwise read as
        open. Two closed solids that share an edge or a face read as open, since that edge is in four triangles
        (@fable-reviewer), and the inspector names that reason;
      - a scale with an odd number of negative axes mirrors the mesh and so reverses its winding; the sum reads it
        reversed back, so a mirrored closed mesh keeps its positive volume;
      - the volume must be at least 10⁻⁹ m³ (a 1 mm cube) [I, a floor to tune]. Below it, or negative (wound
        inward), or open, the mesh has no mass, and still collides.

      A dynamic body whose shapes together have no mass can't be built (`Unbuildable::Mass`, a fault like replica.md
      2.1's other unbuildable bodies): Box3D would give it an inverse mass of 0, so it would answer no force [V
      `box3d-fork/src/body/mass.rs`], and a refusal shows the missing mass instead. The inspector and the import path
      say why and what to do: close the mesh, or add a primitive or a hull with a density. When dropped triangles
      are what opened the mesh (the blob's triangles, less those with two equal positions, are closed and the built
      mesh is not), the refusal says so with their count, the blob's triangles less the built mesh's, whatever the
      cause: "N of the mesh's triangles are under the minimum area and were left out; without them the mesh is
      open". Otherwise it is the ordinary open-mesh refusal. At import, pe-world neither welds nor repairs a mesh:
      where a mesh blob is imported, a cheap warning runs the same test as the build (how many triangles are dropped,
      and whether the mesh is still closed without them); mesh repair belongs in the import tool, later; one lossless
      step it could take is merging exactly equal positions into one index, since Box3D marks smooth edges by shared
      vertex index and a split seam collides as a sharp edge, which can catch a body sliding across it (it changes
      neither the verdict nor the mass). A golden test pins one blob with a sliver: its mass and its open or closed
      verdict are bit-equal on every tier-2 platform (@fable-reviewer). A static or kinematic body takes no mass, so
      any mesh suits it, open or closed. The mass is computed when the body is built or rebuilt, once per mesh, in
      O(n log n) of its triangles (the closedness test's sort; costs above).
      Considered and set aside: the convex hull's mass (any mesh has one, but a concave one weighs as if filled, a
      bowl as a solid dome) and the bounding box's (cruder still). This rule can
      be loosened later without changing any built world (an open mesh would go from unbuildable to buildable), where
      starting from the hull's mass and switching later would change masses in existing worlds (@fable-reviewer);
  - a hull is a convex `SimBlobRef`, on any body, its mass Box3D's. The mesh's scale floor (above) holds for it
    too, but there it is pe-replica's: pe-replica passes a hull's points already scaled into the body's frame and
    pe-physics never scales a hull, so the refusal is `Unbuildable::Shape` at pe-replica's build, with the same
    reason (@vdw-physics). A hull needs no floor of its own: Box3D's hull builder judges flatness relative to the
    hull's size [V `box3d-fork/src/hull/builder_init.rs`, `compute_tolerance`], and a scale that flattens a hull
    past it is `ScaledHull` (replica.md's shape reasons); the shared floor keeps one rule for both. A hull has no
    placement limit: a child hull's points are scaled on the shape's own axes, then turned, then offset into the
    body's frame, as an offset box is; the scale floor is judged on those axes of the world scale.
  - **The blob formats** (@coordinator-opus, Q2, after @vdw-physics's proposal and @fable-reviewer's
    points). `Shape.mesh` names a mesh or a hull blob by `Shape.kind`; both travel inside their entry (3.4, carried
    blobs). Common to both: little-endian; coordinates are `f32` metres in the shape's local frame, before the
    body's scale; the blob is exactly as long as its counts say, with nothing after; an unknown magic or version is
    a fault, the same on every machine, never a guess. pe-physics checks every blob when it builds the body: every
    value finite, every index in range, the fork's limits, the 960 KiB cap. A blob that fails is a deterministic
    fault, `Unbuildable::Shape`, never a panic. The fork's own built forms (a mesh's BVH, its version and hash) never
    travel in entries: each machine builds them from these bytes. A joiner's physics transfer (pe-physics's
    `save_transfer`, checked against the confirmed check hash) does carry them, at about twice the blob's size per
    mesh shape and twice that again with both sides (42, 381 and 1,819 KiB for blobs of 17, 175 and 878 KiB) [V
    @vdw-physics]; part D's join limits count it.
    - **mesh v1:** the magic `PEM1` (4 bytes), a `u32` vertex count, the vertices as `f32 x3`, a `u32` triangle
      count, then the triangles as `u32 x3` vertex indices. A triangle's front is the side its vertices run
      counter-clockwise when seen from the front. The triangle order is kept as written: the mass sum and the build
      read it in that order.
    - **hull v1:** the magic `PEH1` (4 bytes), a `u32` point count of 4 to 64, then the points as `f32 x3`. Each
      machine builds the hull with the fork's hull builder. Tier 2 checks that builder's output digest across
      platforms on degenerate inputs too: coplanar points, duplicates, slivers. A hull whose triangulated surface
      is too big for Box3D is simplified to 44 vertices: Box3D keeps at most 128 edges per hull, and a hull of
      triangles with V vertices has 3V − 6 edges. pe-physics builds every hull at 64 vertices first, and again at
      44 only if that build is refused, so every hull that built before builds unchanged, and the blob's 64-point
      limit is unchanged [V @vdw-physics: round clouds of 45 to 1,024 points, all refused before, build at 44; 8 to
      32 points as before] (@coordinator-opus, CCD part 2 E).
- **The bullet flag, `Body.bullet`** (the user's rulings answers 2 and
  4; the method @fable-reviewer's with @astra-deliberator; [I, draft for
  @fable-reviewer, built after M1 as @item:primordial-engine/pe-physics-and-box3d-fork-continuous-collision-a's parts
  1-3]):
  - **What it means.** If either body of a pair is a bullet, and the pair collides at all, it is swept: within each
    solver sub-step, before the sub-step's motion is accepted, the pair's earliest time of impact along both bodies'
    paths (turns included) is found, so its contact is never skipped, however fast either moves. At that time the
    two respond as any contact does, with the pair's ordinary restitution and friction, by an impulse at the contact
    point's velocity (spin included), and the rest of the sub-step is swept again. That holds whatever the partner:
    static, kinematic or dynamic, awake or asleep, a primitive, a hull, a mesh (still or moving; a moving mesh is
    swept in its own frame) or another bullet. A pair that doesn't collide today isn't swept either: one a filter or
    a sensor excludes, two bodies whose joint disables their collision, a kinematic or static body against a static
    or kinematic one, and a mesh against a mesh. A pair with no bullet is as today: Box3D's continuous collision
    against static shapes (8.1). replica.md will hold the solver's side.
  - **Which bodies.** It takes effect on a dynamic or kinematic body whose shapes are all spheres, capsules, boxes
    and hulls (a compound of them is swept shape by shape, with the body's motion). Hulls are beyond the user's list
    of spheres, capsules and boxes: the fork sweeps a hull bullet with no extra code, so the coordinator kept it swept (Q6), to be told to the user in the CCD parts 1+2 report. On any other body (one with a mesh, or
    a static body) the flag is kept, hashed and shown, has no effect, and the inspector says why. It isn't
    refused: whether it takes effect depends on the body's shapes, other rows a later edit may change, and an edit's
    validity never depends on rows it doesn't write. Triangle bullets are out for now (answer 2).
  - **Off unless someone turns it on.** The flag is 0 on every body until an edit sets it: no shape, preset,
    template, script default, avatar format or world logic turns it on. A bullet is expected to cost
    far more than an ordinary body and to be used on a few objects at a time, or on short-lived ones such as fired
    projectiles. Every member's machine pays for each bullet, and avatar hands are player-authored, so a per-member
    or per-world limit beside `max_claims` (8.1) is an open point for avatars (refound plan, piece 10).
  - **Its cost falls on bullets' pairs only.** A body in no pair with a bullet does no extra work, and a world with
    no bullet steps bit-identically to one built without the feature. A non-bullet partner's motion can change (it
    is bounced or held), and a sleeping bullet may still cost a search each step [I]. A bullet's cost is its pairs'
    sweeps, counted per step in the physics counters. Making or removing a bullet costs what any body's does: nothing
    is prepared for it beyond its shapes, so many short-lived bullets stay cheap.
  - **The budget is a count, the same everywhere.** A sub-step's sweeps are bounded by a fixed number of
    time-of-impact iterations, a world physics setting every member has the same, never a time; its value is part
    1's to measure. When it runs out, every body that could still cross something that sub-step is held at its last
    time proven safe: bullets and their partners, kinematic ones included. A held kinematic body lags its target for
    that sub-step and catches up after, as a capped hand does (8). Each hold is counted, and a bullet is never let
    through.
  - **Deterministic.** The flag is a `Body` field, in the object's leaf; the sweeps run inside the deterministic
    step in one fixed order, so rollback and replay give the same roots on every machine.
  - **Since CCD part 1** (main b476a3d2), pe-physics maps the flag to the fork's swept flag and no longer sets Box3D's
    own `is_bullet` [V by @vdw-physics, `pe-physics/src/world.rs`]. replica.md 9.2 holds the solver's side.
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
  | `Shape` | a primitive (box, sphere, capsule) with its sizes and density, or a mesh or hull `SimBlobRef` (a mesh on any body; 3.2's mass rule); a plane is a large thin box | edits |
  | `Body` | static, dynamic or kinematic; friction, restitution, linear and angular damping, rolling resistance, gravity scale, bullet, may sleep; grabbable | edits |
  | `BodyState` | derived: the simulated state of a bound body (linear and angular velocity, awake flag and sleep timer) and the physics digest of section 6.3 | pe-replica, at cut frames and on export |
  | `Appearance` | base colour, emissive colour, roughness, metalness, an optional material `BlobRef` | edits |
  | `Light` | kind, colour, intensity, range, a spot light's inner and outer cone angles (radians from the local -z axis, 0 ≤ inner ≤ outer ≤ π/2; exactly 0 for other kinds, so unused angles never hash two ways; an edit outside these is refused `Invalid` naming the angle; a `SetRow` changes the kind and the angles together, while field by field a spot becomes a point light by zeroing inner, then outer, then setting the kind) | edits |
  | `Hand` | the owning `MemberKey`, side, grip, engaged, mode, strength (8.1) | created by world logic at admission, with its strength; then inputs of its owner only (section 8), which never write the strength |
  | `SimSize` | the simulation size of 3.5 | set at creation; edits |
  | `Protection` | the owning `MemberKey` (7.8) | edits by its owner |
  | a script's **tag** (slice D) | nothing: zero size, one per script version, its id derived like the script's other components (7.9) | edits with the object's edit right (attach = add, detach = remove); a script's create gives the new object its tag; a commit re-keys it |

  **Which engine components scripts may write** (@coordinator-opus; slice D), only on objects
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
  | `SimSize` | no | it chooses the object's region level (3.5, M4); writes come with that work |
  | `Protection` | no | rewriting `owner` would be an ownership change through logic (@iter-bench's question) |
  | another script's components or tag | no | a script writes only its own components and the engine ones marked here |

- **The registry is world state** [I]. Today it is configuration shared by every peer [V `registry.rs:216-218`]. With
  a logic edit in M1 [U, U1], a script that declares components changes the schema at a frame. So:
  - engine components are registered by a line's first branch from the origin, under their reserved ids (2.3);
  - a script's components are registered by the edit that commits the script, at that frame;
  - the registry is a world table (section 5.4), hashed, and rolled back with everything else;
  - a component id never changes its description. A changed schema gets a new id, and a migration is an edit.
- **Component ids are derived too** (@chunks-reviewer's F7). `ComponentId` widens to 128 bits. Engine components keep
  small fixed values below 2^32. A script's component gets `u128::from_le_bytes(BLAKE3("pe.component" ‖ V ‖ name ‖
  schema hash)[..16])` (the schema hash is 6.6's BLAKE3 hash of the description, which has no id in it, so the
  derivation is not circular: pe-sim's own XXH64 schema hash covers the id and is not used here; @iter-bench), where **V is the content hash of one script version**, the hash of its source archive (one blob,
  logic.md 4.1a; @coordinator-opus, @iter-bench), not the stable asset
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
- **A depth limit** (@chunks-reviewer's Q2 on pe-api, via @coordinator-opus): an object may be at
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
- **How they travel:** an edit that introduces a blob names its hash and length. An inline-class blob's bytes travel
  inside the edit (below), and so do a carried simulation blob's (below, up to 960 KiB). A larger one's travel
  alongside on the bulk lane (S4), or are fetched by hash from any member that has them.
- **Inline blobs** (@coordinator-opus's ruling, on @fable-reviewer's advice; @engine-architect's S1/S2
  shapes). The inline class is defined by size alone: a blob of 1 to 256 bytes (`MAX_INLINE_BLOB`),
  display and simulation alike, which covers every name (pe-api caps a name below 256 bytes). "None" and the empty
  blob are not of the class; every store answers the empty blob's reference with no bytes.
  - **In the entry:** `Transaction.inline` carries the bytes of the inline-class blobs its ops introduce, inside the
    signed, hashed payload (7.1), in ascending order of hash, none twice, at most 2 KiB in all (`MAX_INLINE_BYTES`).
    An op introduces a blob when a `Create` row, a `SetRow` or a `SetField` of a blob field writes its reference; a
    `Copy` introduces none (its rows' blobs are on every machine that has the source). There is no dedup against
    the store: the bytes are always carried, and the store dedups by hash.
  - **At apply:** over 2 KiB is `TooLarge { limit: 2048 }`, and a blob no op introduces is `Invalid`, on every
    machine alike. An applied edit puts the bytes into the machine's store; a refused one stores nothing.
  - **Two limits** (@coordinator-opus, amending the ruling's point 3, after @fable-reviewer): pe-api
    refuses any edit over 1 KiB, predicting a forward edit's inverse at the dry run too ("split this change; undoing
    it would be too large"); pe-world's hard cap is twice that, so an inverse whose old values grew through a race
    still fits. An undo still over 2 KiB is refused ("undo this in parts"); there is no split undo.
  - **Undo:** an outcome's inverse carries the bytes of the blobs it restores, from the store. A delete's inverse is
    `Undelete`, which names objects, not blobs, and carries none: the tombstone keeps the rows, the tombstones are
    imported state (6.6), and the tombstone's expiry is the undo window.
  - **Every reference with its bytes** (phase 3, after @api-eng's hand-over, main aa9cfc9c; @api-eng
    found every pe-api and pe-app write path carrying them): an inline-class reference an op introduces without its
    bytes is `Invalid`, at apply and at the dry run alike, and changes nothing. A `Copy` introduces none (above). An
    undo on a machine that lacks a restored blob's bytes (it never received them) carries the reference without
    them and is refused the same way; pe-api reports it (@design-reviewer's Low). World logic's own drafts (genesis,
    author `MemberKey::LOGIC`) are exempt: every machine builds them from its own code and store, so their encoding,
    entry ids and pinned roots stay as they were [V `a_reference_without_its_bytes_is_refused_and_the_dry_run_agrees`].
    Two guards keep the exemption from reaching a received entry (@design-reviewer): genesis is the
    only author of LOGIC drafts, and pe-agree takes entries only from session members; and genesis sets LOGIC's
    permission bits to 0 once it is built [V `preset.rs` `ball_pit_on`], so a received entry claiming the all-zero
    author is refused `NoPermission` first. The second guard matters because the all-zero key is a small-order
    point, which a non-strict signature check doesn't reject.
  - **In an import** (6.5; S2 v1.8): `CutImport.blobs` carries the bytes of the inline-class blobs the imported rows
    reference, a tombstone's kept rows included when the tombstones table is imported, since `Undelete` restores
    them (`Tree::export_blobs` gives them). Each must be one an imported row references, in ascending order of hash
    (`BadBlob(i)` otherwise). A missing inline-class blob that this machine lacks too refuses it (`MissingBlob`): a
    *simulation* blob since without it the joiner could step differently, and a *display* blob (a name) since nothing
    fetches an inline blob later, so the name would stay blank for good (phase 3b; @design-reviewer's Low).
    pe-diverge's repair carries each donor's `export_blobs` with its records, and treats `MissingBlob` as the donor's
    fault, so the ladder asks the next donor (divergence.md 4.2 step 3)
    [V `a_joiner_gets_the_names_of_the_state_it_imports`]. If no donor holds a display blob, the repair can't
    complete, where before it converged with a blank name. That can't happen within builds from phase 3a on, since
    every machine that applied a reference received its bytes; it can with state from an earlier build
    (@design-reviewer).
  - **Kept:** the store deletes nothing today [V, `BlobStore`]. When pruning comes, an inline-class blob is kept
    while any live or tombstoned row references it, even after its entry has left the log's retention, since a joiner's
    import is then the only way it travels.
  - **Known gap (M1):** display blobs over 256 bytes (meshes to draw, textures) still travel by reference only, and
    nothing fetches them yet (@item:primordial-engine/blob-transport-the-bytes-of-names-and-meshes-ano). A simulation
    blob over 960 KiB is refused (carried blobs, below) until that item lifts it.
- **Carried simulation blobs** (@coordinator-opus Q1; the CCD part 2 dependency,
  @item:primordial-engine/simulation-blobs-travel-in-their-entry-mesh-and-h). A blob a `SimBlobRef` field references,
  of 257 bytes to 960 KiB (`MAX_CARRIED_BLOB`), travels inside the entry that introduces it, like the inline class.
  So a mesh or a hull collider reaches every member and every joiner, and no machine builds a body that another
  can't. `inline::is_carried(r, sim)` is the rule: an inline-class reference from any blob field, or a carried one
  from a `SimBlobRef` field.
  - **In the entry:** in `Transaction.inline`, the same list as the inline class and in the same order (ascending
    hash, none twice). Its class is its length: over 256 bytes is carried. An entry whose blobs are all 256 bytes or
    less encodes exactly as before, so no entry id or pinned root moves. An op introduces a carried blob as it
    does an inline one (a `Create` row, a `SetRow`, or a `SetField` of a `SimBlobRef` field); a `Copy` introduces
    none.
  - **At apply:** inline-class bytes over 2 KiB, or carried bytes over 960 KiB in all (`MAX_CARRIED_BYTES`), are
    `TooLarge` with that cap as the limit. A carried blob no op introduces is `Invalid`, and so is a carried reference
    an op introduces without its bytes. A display blob over 256 bytes in the list is one no op introduces, so it is
    `Invalid` too. A simulation reference an op writes that is over 960 KiB is `TooLarge { limit: MAX_CARRIED_BLOB }`:
    its bytes can't travel in an entry, and nothing fetches them yet, so a machine without them would build a
    different body (@chunks-reviewer's M1). Each verdict is the same on every machine, and the dry run gives the same
    one. An applied edit
    puts the bytes into the machine's store; a refused one stores nothing
    [V `tests/carried.rs`].
  - **Why 960 KiB, not 1 MiB** [V]: an entry's payload is at most `MAX_PAYLOAD`, 1 MiB + 4 KiB, and an edit at most
    that, less the room for 8 re-issues (`MAX_EDIT_BYTES`). A 1 MiB blob and its `Create` don't fit. 960 KiB leaves
    64 KiB for the rest of the edit, with no change to the payload cap or the transport (option (a)). That is about
    40,000 mesh triangles [I]; a 64-point hull is 776 bytes.
  - **Two limits:** pe-api refuses a forward edit whose carried bytes, or its predicted inverse's, exceed 768 KiB
    (@api-eng), so an undo whose old meshes grew through a race still fits under pe-world's 960 KiB. An undo still
    over it is refused ("undo this in parts").
  - **Undo:** an outcome's inverse carries the carried blobs it restores, from the store, as it does inline ones.
    So each kept outcome whose inverse restores a mesh holds a copy of up to 960 KiB for as long as it is kept
    (@chunks-reviewer's L1). An inverse that names bytes already in the store, instead of copying them, is the later
    fix if editing large meshes makes that cost show.
  - **In an import:** `Tree::export_blobs` gives the carried blobs the imported rows reference too, live rows and
    tombstones alike. A joiner that lacks one refuses the import with `MissingBlob`, since without it it could step
    differently; the repair then asks the next donor. pe-diverge sends one record's blobs across as many replies as
    they need (@diverge-eng): neither an object's carried blobs nor the tombstones table is bounded by the
    per-entry cap.
  - **Head-of-line cost** (the decision of 2026-11-17 09:10Z): a carried blob makes its entry long, and the entry
    lane is ordered, so every later entry waits behind it. A 960 KiB entry on a 1 Mbit/s link holds the lane about
    8 s [I]; on a 10 Mbit/s link, under 1 s. That is the price of agreement without a fetch. pe-api's lower limit
    bounds an agent's single edit, and a larger mesh waits for the blob transport item above.
- **Blobs are of two uses, with two rules** [I, after @chunks-reviewer's F1]. A component field declares which, by
  its kind: `BlobRef` for display, `SimBlobRef` for simulation. The enforcement point is the reader: pe-physics and
  pe-sdk read bytes only through `SimBlobRef` fields, and pe-sdk gives logic no way to read a display blob's bytes
  (@chunks-reviewer's N5; a note for WS-logic).
  - **Display blobs** (a mesh to draw, textures, names, materials) may be missing. The entry applies, the `BlobRef`
    is stored as given, and the hash stays exact. Only drawing waits: a mesh is drawn as its bounding box. Shared
    simulation never reads a display blob, so no two machines can compute differently because of one.
  - **Simulation blobs** (a collision hull, a script's source archive, anything shared logic or physics reads) are
    part of the entry for agreement. Small ones (up to 960 KiB, which covers hulls and most meshes) travel *inside* the edit entry, so
    any machine that has the entry has the bytes. A script's source always does: `CommitScript` carries the archive
    itself, at most `MAX_SOURCE_BYTES` (7.9), and applying the commit puts it into the machine's blob store, on every
    machine, so each can build the version; a refused commit stores nothing, and what a rollback leaves behind is
    store garbage, pruned with the store (@coordinator-opus). Meshes and hulls up to 960 KiB travel in
    their entry as carried blobs (above); larger ones are refused until the blob transport comes. With it, larger
    ones follow the rule of a late entry: a machine that lacks the bytes at the entry's frame simulates as if that change had not arrived yet, which
    is a prediction, and rewinds and replays from that frame when the bytes arrive. It does not vote for a cut that
    includes the entry until it holds the bytes. Nobody waits; only that machine's view lags.
    - **In v1, before the blob transport** (@coordinator-opus, R1, CCD part 2 D): a machine missing a
      mesh or hull blob holds no vote for it, since nothing fetches the bytes yet, so a hold could never be released
      and at n = 2 it would stall finality. It leaves the body unbuilt and counts it as absent
      (`Unbuildable::BlobAbsent`, `ReplicaImpl::absent_blobs`), not as a fault; a repair import brings the bytes.
      The vote hold above starts with the blob transport.
  - **A compiled module is never a blob** (@iter-bench, under Q3c [U]: no machine runs another's
    build). The script's source is the simulation blob; each machine builds its own module from it, a local
    product that is never stored in the world, sent or fetched. Activation after each machine's own compile is
    WS-logic's, on the same late-entry path.
- So D17's "a missing artifact pauses that peer" narrows to: nothing pauses; a machine lacking a simulation blob
  lags as if behind on entries, and a machine lacking a display blob draws a stand-in.
- **Limits:** a blob is at most 64 MiB in M1; a transaction introduces at most 256 MiB [I, a guard against
  flooding, to tune].

### 3.5 Simulation size

The user (via @coordinator-opus; file 16, 6.4): every object has a nonzero "size" used
purely for simulation distance, with no bearing on collision, and it gets a reasonable default at creation. The user
accepted the details below (via @coordinator-opus; file 16, 6.5):

- **Every `World` object has a `SimSize` row,** in metres: positive and finite. It never affects physics, drawing or
  collision; WS-regions reads it to choose the object's level (M4). Levels are the cross-scale design's size bands;
  file 16 calls them layers, a name that now belongs to the late post-MVP feature of file 22 (refound plan 5.3).
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
- **Saved objects keep their relative size**. A saved object (an inventory item, M5) records its
  `SimSize` divided by the saving player's scale; spawning it sets `SimSize` to that ratio times the spawning
  player's scale. So an object saved by a giant and spawned by a mouse is, for simulation distance, the same size
  relative to each.
- It is an ordinary component: hashed, versioned (7.3), and written by plain edits.

### 3.6 Field units

A field's description may name its unit (@coordinator-opus's ruling, on @fable-reviewer's advice). The
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

**Colours** (@coordinator-opus's ruling, on @fable-reviewer's advice). A field that holds a
colour says so in its description: `FieldDesc.color`, a `pe_sim::ColorSpace`. The only space is `LinearSrgb`: linear
light, sRGB (Rec. 709) primaries, and straight (not premultiplied) alpha in a fourth element. The mark changes nothing
about the stored floats or the simulation. It tells a reader how they are meant, so WorldApi reads and writes the field
as a colour (api.md 4.4: hex or an explicit space, never a bare array).
- **Only on 3 or 4 `F32` elements,** never with a unit and never on padding; registration refuses it anywhere else.
- **Hashed:** it is part of the description, in the high bits of the field's kind byte (6.6) and in pe-sim's schema
  hash, so it is in a script component's id (7.9). A field without one has the byte it always had, so no unmarked
  description changes.
- **The engine's colours:** `Appearance.base` and `Appearance.emissive`, and `Light.color`. Marking them changed the
  engine registry's descriptions, so every world's root changed once with this mark (the pit's pinned frame-0 root in
  pe-world's tests/preset.rs was regenerated); nothing else pinned or saved held one.
- **Scripted components** mark a field the same way (main d7933dc1, @iter-bench) [V, read]: a field of pe-sdk's
  `LinearRgb` or `LinearRgba` is marked in the manifest's field byte, pe-logic gives the mark to the component's
  `FieldDesc`, and binding a script to an existing component compares colour spaces with the rest of the fields.

### 3.7 Field modes: synchronized, decoupled and desynchronized (design, not built)

The user's rulings of 2026-11-24, from @fable-reviewer's file 21 (`fable-reviewer/review/21-draw-systems.md`
v14, sections 2.1, 2.3, 2.7 and 2.8; the rulings verbatim on
@item:primordial-engine/draw-systems-define-the-exact-restrictions-of-pe and in `<notes>`
under the 2026-11-07 QR (b) entry). They extend the 2026-11-07 localization rulings (`user-design-decisions.md`
362-374). Nothing here is built; building needs its own go after the friend test. The local rule that decides
which code is local, and draw systems themselves, are logic.md's (section 7); how a drawn picture is made is
render.md's.

**Kinds of value** (file 21, 2.1). Every value a world holds is one of three; the third is new:

| Kind of value | Computed | Lasts | May affect the shared world |
|---|---|---|---|
| **Synchronized** (the default) | every machine, on the step | saved | it is the shared world |
| **Unsynchronized, local simulated** (2026-11-07) | one machine, on the step | remembered where marked | only as that player's detected and sent input |
| **Unsynchronized, per frame** (a *presentation override*) | one machine, per display frame | one frame | never |

- **A presentation override** is the value the picture uses this frame in place of a field's own value. The field
  keeps its own value, and everything but the picture reads that. An override is never remembered, never a
  collider and never a source of input; it is not a row, so it is outside every root (5.2). It reuses the
  framework's per-player mark and its two ways of choosing a mode (a component on the field, or a script's schema).
- **`Local` objects (3.1)** are where personal UI lives, and UI is personal by default (the user).

**The three field modes** (file 21 v14, 2.3; ruled 2026-11-24). The user's definitions, verbatim (
the names):
"synchronized; field is the same for every player. decoupled: field is unique per player, but the value of any
particular player can still be known by all players. desynchronized; field is unique per player and totally
private from every other player, for performance / secrecy"

How often each is used, verbatim, which defaults and examples follow: "it shouldmbe noted that in
typical use, most fields should be synchronized, a small but notable minority can be decoupled for performance and
localdraw rendering related functionality, and a very tiny select few edge cases are fully desynchronized to
enforce performance or privacy constraints"

| Mode | Stored as | What is sent | A shared script reads | A private script reads |
|---|---|---|---|---|
| **Synchronized** | one value for everyone | nothing for a script's write; a player's write once, as input | the value | the value |
| **Decoupled** | one local value per player | nothing until a shared script reads it; then each player's changes, as their input | the published values: latest, or the selector (logic.md 7.1) | its own player's local value |
| **Desynchronized** | one local value per player | never anything | refused when the script is committed | its own player's local value |

A private script is a local one under logic.md 7.1's local rule.
- **They are the three of before**, verbatim: "so really there are three types of fields; shared,
  private unlocked (published) and private locked (private). so we have the same three from before". The names
  replace the field labels shared, published and private of. That ruling's positioning statement
  stands: Resonite has two states, published and private, and the engine's "main advancement is adding the third
  "shared" state: simulated locally, synchronized via signed contrqts on world state and corrected via rollback
  when invalidated" (file 21 v14, 2.3).
- **The mechanism underneath** (accepted by the user): a field is stored one of
  two ways, one value for everyone or one local value per player. A per-player field can have a
  **published feed**: its player's changes, sent as that player's input. The feed switches on automatically when a
  shared script reads the field. Input is the only crossing between the two ways of storing.
  - A decoupled field keeps its live local value apart from what was last published from it. A private slider can
    be at 0.8 on Alice's machine while the last published value is 0.6: her HUD reads 0.8, and a shared script
    reads 0.6 or the selector's result. If the reader goes away, publishing stops and the record keeps 0.6
    (@astra-deliberator).
  - "Synchronized" is one logical value; which machines hold it depends on what they have loaded.
- **The lock** makes a per-player field desynchronized: "never publish this field directly", off by default
  (ruled "2a"). It serves performance as well as secrecy, in the user's definition.
  - [I] It binds scripts already reading the field, not only new ones. Whoever may change the field's settings
    may unlock it.
  - [I] Values sent before locking stay in the record, and a script allowed to read the value has its own rules
    for what it outputs.
- **How a shared script reads a decoupled field** (the plain read, the selector, the explicit reads and the
  private read) is logic.md 7.1, point 2.
- **What a player can send is three things, and only one is a field** (@astra-deliberator):
  - a stream that is only watched, such as voice and the poses used for display. It never enters the record;
  - a recorded input: an ordered event such as a button press, a sample such as a gameplay hand pose, or a file
    recorded as a hash with its bytes pinned;
  - a retained value, which is a decoupled field's published value.

  Tracked hands, files, device readings, local web calls and locally computed data all cross as input, and there
  is no second path.
- [I] **A value every machine computes about a player,** such as a score, is a synchronized field filed under that
  player, not the player's local value. A player's local value is written on that player's machine, and only that
  machine publishes from it.
- **Values from a player's device, files or local web addresses** need that player's grant in every mode
  (logic.md 7.2).

The governing principle, verbatim: "we want to minimize the amount of published content to only
what is absolutely necessary, and keep asmmuch as possible shared or private."

**The rules of the earlier three-class model stand** (file 21, 2.3, with @astra-deliberator's five refinements).
In them, "published" means what a feed or a player's write puts in the record, and "private" a per-player field's
local value.
- **A published value is stored the same for everyone,** and its only writer is a player's input: Q10's "local
  data sent as input becomes shared".
- **The mode belongs to a field or an input channel,** not to an object. One controller can hold a pose only its
  player sees, a grip it publishes and a synchronized score; the object shows a summary.
- **Publish the cause, not the result.** A small cause (a pose, a button, a parameter) is published and every
  machine simulates the consequence as shared state. A result is published only when its cause can't be shared: a
  file's contents, a device reading, a private computation.
- **Watching has two strengths** (Q5): *seen only* is a lossy stream, not saved or replayed, which shared code
  can't read; *reacted to* is a value in the record, signed, ordered, saved and replayed.
- **Continuous samples merge to one per step; events don't.** A press and a release inside one step are two
  ordered events, each with an identity and recorded once.
- **Replay uses the record:** it never rereads a file, repeats a web request or publishes again.
- **A file is named by hash and length** (3.4), which doesn't make it available. If shared logic uses the bytes,
  they are pinned for replay and joiners, and the reference takes effect at an agreed step; a machine that
  downloads late catches up while the others go on.
- **A published value is a signed claim the world still validates:** type, range, size, rate and the player's
  permissions. It can fill only a field declared as fed by a player. Rate and size budgets are judged from the
  record, never from arrival time, so every machine drops the same inputs.
- **Owning a published pose gives no authority over a collision's outcome.** A published body drives shared
  contacts at its recorded poses, and shared physics decides the result, as it already does for hand inputs (8.1).
- **A late joiner** imports the last recorded value with the rest of the state, and nothing of the source's
  private state. When the source leaves, the field keeps its last value and its "live" flag goes off (Q1 (a)).

**Who may write a published value** (file 21, 2.7; the user):
- **Any permitted writer by default,** as often as they like; each write is recorded once and the last in the
  agreed order wins. That is rule 3 (7.4), and every machine checks permissions again at the ordered frame.
- **The engine's special feeds** (a player's pose, their voice source) accept only the player they are attached
  to and can't be handed over.
- **A field can be restricted to named players.** [I] Changing the list is a separate permission from writing the
  value, and each sample is tied to the list it was made under, so a late sample can't write after the list
  changes. An author's continuous feed is a field restricted to one player.
- **A write from the inspector is a published write:** that player's signed input in the record. The field stays
  synchronized. A write made by shared code is not published; every machine computes it.
- **Two writes in one step** follow the agreed in-frame order (agreement.md). [I] Where the order decides
  something of value, use rule 2's precondition (7.3) or a hold (7.2); "first wins" is the hold rule only.
- [I] **A private script that writes a synchronized field each step** would send one input per step from every player,
  and the copies would fight. It should publish the cause instead.

**Outside sources and secret code** (file 21, 2.8; the user):
- **There is no secret mode.** Hidden values are desynchronized fields ("secret state is just private fields"). Hidden logic isn't world content at all.
- **Secret code is an outside source:** it runs in its owner's own engine and reaches a world as a websocket or an
  HTTP feed does, through a published input channel the world has agreed to. The world chooses whether to accept
  outside sources; it never holds, builds or runs the code.
- **One mark for every outside source.** It doesn't say whether the source is a websocket, HTTP or engine code.
  The label naming the feeding player, and whether that source is live, stays beside it.
- **The engine offers no messaging service;** authors build their own (against fable's recommendation). [I] An
  author should know that a relayed connection's encryption protects each link only, not a message from the
  relay.
- [I] **The contract is public:** the input's format and the rule that validates it. The local-access grants
  (logic.md 7) and the writer rules above apply. Others can verify who sent an input and that it passes
  validation, never what computed it. Fair hidden moves need commit and reveal: publish only the digest
  `H(round, player, choice, secret_nonce)`, with a fresh secret random number, then reveal the choice and the
  number, and shared code checks the digest. Withholding a reveal can't be prevented; a forfeit is the game's own
  rule. A reveal is sent only once its step is final, and a retry may resend the same signed reveal, whose effect
  happens once. Across a leave the owner keeps their private saved data and the keys of outstanding reveals.
- [I] **Nothing of a source enters the world:** no module, private rows or debug output in commits, joins, saves,
  the inspector, captures or telemetry. A desynchronized field is only as secret as its storage, which is open
  (12).

## 4. Coordinates: `Pos`, `Offset` and the wall

### 4.1 The user's rulings

-, verbatim: "1. yes, and instead, just require them to intentionally subtract the origin position
  (which is a world constant always available) if they really want that 2. no warning or test, enforce this via the
  type system 3. is there a difference? in resonite those are the same things".
-: coordinates go past the 64-bit range without saturating (plan 5.2, R16).

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
- **No silent saturation** [U]. Today `normalize` clamps at ±2^62 [V `coords.rs:83-108`]. Instead:
  - moving a `Pos` returns `Result<Pos, OutOfRange>` (4.2); every path that writes a position into the world (an
    edit, an input, a physics write-back, a spawn) turns an error into a refusal of that entry. For physics, as
    @vdw-physics confirmed: when a write-back would leave the range (or pe-physics's own 10^6 m from its physics
    origin), the pose is not written; the body is set back to its last valid pose, the one still in `Transform`,
    with zero velocity and its history reset, and an event is raised, at the same frame on every replica;
  - a non-finite offset is a refusal, not 0 [V today it becomes 0, `coords.rs:85`].
- **A staged limitation of** (@chunks-reviewer's L7): from M1 to M3, a position beyond the `i64`
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
    ├── push windows       each member's recent pushes, for the push budget (8.1)
    └── field modes, feed  per-player fields (logic.md 7.4)
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
  - the faulted mark: per asset, not per object (@coordinator-opus, item 4). A fault stops all of
    the script's systems; the mark is set after the stage (9.2a) and cleared by the asset's next commit.

  Beside an asset's row, its **pending commit** (logic.md 6.2a; 7.9): the committed version, its author, hashes and
  components, the commit frame and deadline, the voters at the commit and each one's counted attestation or
  `BuildFailed`, or, once voided, the void's frame, kind and failures, kept until the asset's next commit. A first
  version has a pending commit and no row until it activates.

  No module bytes are in the world (3.4). The byte layout is 6.6's.
- **Governance:** the tables only. M1 stores the member list (keys and roles), the role table (permission bits) and
  the ban list, all hashed. The one permission M1 checks is `EDIT` (section 7.6). Roles follow the user's rulings,
  with no M1 simplification (@coordinator-opus):
  - **a session** admits every player as an admin by default, and only a session configured for it admits joiners
    as less [U; @coordinator-opus]. One admin always exists, the last can't give the
    role up without passing it on, and the session closes when the last admin leaves (the user's rule; not built,
    2.4). M1's joiner is an admin;
  - the templates below are WS-gov's, for a session configured to admit joiners as less:
  - a private world's template gives the admin role to its creator only;
  - others join under the home-world ruling [U]: friends as Guest, a role that has `EDIT` for the
    slice's editing; the confidants of the world's admins as admins;
  - public worlds start strangers with no admin rights [U, Q6]; that template is WS-gov's (M3).
- **No separate world-ownership state** (the user, 19:50Z, via @coordinator-opus): anyone with the
  admin role is a co-owner of the world, so "owner" in a world means "an admin", and the world model stores no
  ownership table or certificate chain. (An object's `Protection` owner, 7.8, is a different thing: the member who
  created or imported that object.) How admins reconvene after a split is WS-gov's and WS-diverge's.

  How M1 decides who is a friend or a confidant (the relationship data of) is WS-gov's and WS-app's;
  the M1 demo needs neither, since its joiner is an admin.
- **Claims and cooldowns:** ported from pe-session's tables (plan 2.2: `session/store.rs` → pe-world)
  [V `crates/pe-session/src/session/store.rs:28-55`], keyed by `ObjectId` and `MemberKey` instead of indices.
- **Tombstones:** section 7.5.
- **Inputs** (8.1): each member's grab cooldowns (with the claims above), each hand's pending pose pack, and each
  member's push window (its pushes in the last second).
- **Lines:** the table is second in the tables root (6.2) but holds no entries in M1, so nothing in it can differ.

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
                               claims, cooldowns, tombstones, hand packs, push windows, field modes, feed)
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
  setting within a supported range, 60 Hz by default (the user).
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
  pe-world's `write_physics` overwrites only the `BodyState` rows whose bytes changed when the bodies are the
  column's, and rewrites the column when a body joined or left. The result, row versions included, is the full
  rewrite's, so only the changed rows' objects lose their cached hash: an untouched 500-ball session changed 3,547
  of the 298,960 rows written over 592 roots, all in its first 0.7 s, and a settled pit changes none (one push wakes
  2-4 balls); a played-in pit is unmeasured (L3a, @vdw-physics, main d6e25ec5; replica.md 5.3) [V the equality:
  `write_physics_writes_only_changed_rows_with_the_full_rewrites_result`; the counts: @vdw-physics's measurement on
  diag/root-split-3].
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
  fetched ones, and stores the inline blobs the import carries (3.4). Every record's path is checked, before
  anything is applied, against the quorum-agreed root of a final cut, never against the sender's claim. It is a
  restore of part of a root; WS-diverge drives it and pe-replica replays forward from it.
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

- **The record ceiling, R_max** (@coordinator-opus, after @fable-reviewer and
  @diverge-eng): no record is longer than 256 MiB (`tree::MAX_RECORD_BYTES`). A record here is an object's record or
  a table's. pe-diverge pulls a record over about 1 MiB in numbered pieces, which it can check only once the record
  is whole against its leaf hash, so it refuses any claimed record length above R_max. pe-world is the backstop: an
  edit that would leave an object's record past it (a row it writes, or an object it creates), or the tombstones
  table's (an edit that grows it), is refused `TooLarge { limit: MAX_RECORD_BYTES }` and changes nothing. The lengths
  are counted without building the records (`tree::record_len`, `tree::tombstones_record_len`), equal to the
  encoders' [V `tests/record_cap.rs`]. The ceiling is far above use: the tombstones table, all of one minute's
  deletes in one record, reaches it after about six copy-then-deletes of the largest copy (65,536 objects at about
  650 B each [I]). It is a ceiling, not a bound on the table: deletes stay free below it, and nothing expires early.
  - **Known gap:** a script's `Destroy` also makes tombstones (9.2a), outside any edit, so nothing refuses one. A
    script that destroys that much in one minute is far past its own command limits [I, not checked against pe-logic's]. If it ever comes near,
    the next delete is refused until tombstones expire; an undelete, which shrinks the table, is not. Script
    activation's re-key (5.3) also rewrites rows outside any edit, live and tombstoned, possibly into a larger
    component; it is bounded by `MAX_REKEY_ROWS` rows' growth, tiny next to 256 MiB (@chunks-reviewer's L2).
  - The other tables grow with members and scripts, each under its own limits. One leaf per tombstone is set aside
    until a measurement shows the cost of re-hashing and re-sending the whole table (the same ruling).
- **The frame** in the world root is the frame the state is at: `World::frame()`, the next frame to step, which is
  the cut's frame when the root is taken at a cut (9.2's convention).
- **Config hash** = `H("pe.config" ‖ config bytes)`. The config bytes are everything 5.1 lists beside the frame,
  since each can change a later shared outcome (@design-reviewer's Medium on ecd11ef0), in this order:
  1. the seed (`u64`): it breaks ties in pe-log's order and seeds logic's random streams;
  2. the world's line id (32 bytes): it feeds every id derivation. In M1 it is the M1 marker's line (2.4), and all
     zero for a world that has none yet (the `lines` table holds the branch record, 2.4);
  3. the M1 marker (2.4): a byte 0, or 1 and the 32-byte line id;
  4. pe-agree's settings record, `lp(bytes)`: the quorum setting and the cut cadence, which decide which frames are
     cuts and what agreement is. pe-agree owns the format and pe-world never parses it; the bytes live in the world
     (app.md 4 step 1: world settings live in the world, and a joiner takes them with its state), set by the creator
     at genesis and changed only by an agreed entry at a frame (no such entry in M1: below) (@engine-architect);
  5. the tick rate (`u32` Hz) and the depth limit (`u32`);
  6. gravity (3 × `f32` bits) and the physics origin (4.5's canonical `Pos` encoding);
  7. the input settings of 8.1: `claim_ms`, `cooldown_ms`, `push_budget`, `reach_mm`, `slack_mm`, each `u32`;
  8. the **blob-shapes** setting: a byte 0 or 1, anything else refused, as the marker's is. It gates both meshes
     and hulls, the shapes built from simulation blobs (3.2): on, pe-replica builds them; off, each is
     `Unbuildable::Shape`. Set when the world is founded (`World::set_blob_shapes`, a trusted genesis call like
     gravity's), never by an edit in v1, and off in every preset until the ruled criteria hold: a two-machine run
     with a mesh body, a join and a repair included; an absent blob handled as its own local reason; and the join
     transfer within the join limits (@coordinator-opus point 4, after @vdw-physics and
     @fable-reviewer). Import decodes it
     exactly, like every item: config bytes without it are refused, with no default. When world saves exist,
     "absent means off" is the intended default for a save from before it. **Open point:** before saves exist,
     decide whether the byte stays as a world's real choice or goes once meshes are always on; an item, once
     appended, stays for good.

  A new setting is appended, never inserted, and that is a world-format version change. The agent-powers slice
  (8.1) appends, after item 8, `max_claims` (without and with the right), `max_chunkloaders`, `max_push_dv` and
  `max_push_speed`, in that order; its per-member override row extends the governance value, also a format change.
- **The config record** (S2 v1.3's `LeafKey::Config`, @engine-architect): the config bytes, with the
  proof `[Node::World]`; it checks as `H("pe.config" ‖ bytes) == World.config`. Import decodes it exactly (every
  item, nothing after, each value one the world's setters accept: a rate of 30 - 120 Hz, a depth limit of
  1 - 4,096, finite gravity, input settings within 8.1's bounds) and sets every setting from it, the seed included.
  So a joiner imports into a bare world (engine registry, default settings, built at the world's rate, below), and
  an import without the record leaves the importer's config, which the root check catches.
- **Simulation settings live in the world, and the replica reads them from it** (@coordinator-opus,
  after @design-reviewer's Medium on ce6b13f0 and @fable-reviewer). Every setting that changes what a
  simulation computes is in the config record, under the root, and the replica reads it from the world it holds.
  It keeps no copy taken at construction. Today that means the rate, gravity, the physics origin, the blob-shapes
  setting (item 8) and 8.1's input settings (the push and speed limits among them); physics settings that a world
  may choose (CCD switches, solver settings) join the config the same way, appended, while those fixed by the
  engine build are not world state. **In M1 a world chooses none beyond the rate, gravity, the origin and the
  blob-shapes setting, which stays off** (@vdw-physics). The Box3D
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
  - **No change mid-session in M1.** No M1 entry can change gravity, the origin, the rate or the blob-shapes
    setting: pe-world's setters for them are trusted calls for genesis, and no edit, input or governance op writes
    the config. A later slice that
    adds such an entry applies it at its frame, replays included.
  - **Tests:** a 30 Hz world into a 60 Hz bare joiner (refused at the welcome and at import); non-default gravity
    into a default joiner; non-default gravity, an import, then a rung-1 replay on the joiner, its root compared with
    the host's; and one test that sets every simulation setting to a non-default value, imports into a bare joiner,
    steps both sides and compares roots.
- **The tables**, in this order (the `LeafKey::Table` index is the position): 0 registry, 1 lines, 2 modules,
  3 governance, 4 claims, 5 cooldowns, 6 tombstones, 7 hand packs, 8 push windows, 9 field modes, 10 feed. 6.2's list
  gains hand packs and push windows, which 8.1 already puts in the root, and the last two (logic.md 7.4, per-player
  fields): both are empty in a world with no per-player field, but they are always in the tables node, so adding
  them moved every world's root once (@logic-eng, U3). The lines table is empty unless the world branched (2.4), as in all of M1's
  sessions.
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
    lp(unit)`, the unit in 3.6's canonical spelling, empty for none. The kind byte holds the kind's number in its
    low five bits and the field's colour space (3.6) in its high three: 0 none, 1 `LinearSrgb`. The kind
    numbering is frozen: U8 0, U16 1, U32 2, U64 3, I8 4, I16 5, I32 6, I64 7, F32 8, F64 9, Bool 10, ObjectRef 11,
    BlobRef 12, SimBlobRef 13, Pos 14, MemberKey 15, EntryRef 16, Hash 17; a new kind takes the next number, up to 31;
  - lines (the branch record, 2.4; empty for a world that never branched): key `[1]` holds parent line (32) ‖
    `u32` count ‖ per chain in chain order, chain (32) ‖ member (32) ‖ head seq `u64`; key `[2] ‖ chain ‖ seq` (seq
    big-endian, so rows sort by chain then seq) holds the sealed original's entry id (32) ‖ payload hash (32). Key
    `[0]` is kept for the line list `Node.born_on` indexes (2.3). It imports like any table; a record whose sealed
    rows lie outside the suffix, or that has sealed rows without key `[1]`, does not decode;
  - modules (slice D; pending commits since logic.md 6.2a): key the script asset id (32); value `flags u8` (bit 0: a
    current row, bit 1: a pending commit; at least one), then `u32 len ‖ current row` if present, then the pending
    commit if present. The current row is `version entry (32) ‖ version op u32 ‖ source (hash 32 ‖ len u64) ‖
    toolchain (32) ‖ sdk (32) ‖ author_module (32) ‖ component count u32 ‖ the version's component ids, u128 each,
    strictly ascending (the tag among them) ‖ activate_at u64 ‖ faulted (a byte 0, or 1 ‖ frame u64 ‖ kind u8)`. V is
    `source.hash`, stored once (@chunks-reviewer's L1). Decoding is strict: a repeated or out-of-order id, a faulted
    byte other than 0 or 1, or a kind that is not one of pe-logic's `Fault::code()` values 1 to 13 (logic.md 3.6) does
    not decode; `HostStack` has no code, since it is one machine's own fault and never world state. The pending commit
    is `version entry (32) ‖ op u32 ‖ author (32) ‖ source (hash 32 ‖ len u64) ‖ toolchain (32) ‖ sdk (32) ‖
    author_module (32) ‖ count u32 ‖ component ids (u128 LE, ascending) ‖ c u64 ‖ D u64 ‖ count u32 ‖ voters (32
    each, ascending) ‖ count u32 ‖ attestations (voter 32 ‖ stamp u64, ascending, voters only) ‖ count u32 ‖ failures
    (voter 32 ‖ stamp u64 ‖ reason u8, `BuildFailure`'s code 1 to 5; ascending, voters only, none that also attested)
    ‖ void u8 (0 none, 1 Unattested, 2 RekeyBlockedAtActivation, 3 RekeyTooLarge, 4 AuthorRevokedAtActivation, 5
    QuorumUnreachable) ‖ its frame u64 if not 0`. A world with no scripts has no entries, so its root is unchanged.
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
    as f64 bits`, in N·s exactly as stored (@chunks-reviewer's M2: no conversion to mN·s);
  - field modes: key `component u128 ‖ field index u16 ‖ scope u8 (0 component-wide, 1 one object) ‖ object id
    (zeros when scope is 0)`, big-endian like every key, 35 bytes; value `mode u8 (1 decoupled, 2 desynchronized) ‖
    selector u8 (0 latest, 1 any, 2 all, 3 sum, 4 average, 5 min, 6 max, 7 member, 8 holder, 9 owner) ‖ member (32,
    zeros unless the selector is 7)`, 34 bytes. A decoder refuses any other code, and member bytes on a selector
    that names no member. At most 16 object rows per object;
  - feed: key `object id ‖ component u128 ‖ field index u16 ‖ member (32)`, 66 bytes; value `the published bytes
    (the field's size, at most 64) ‖ frame u64 ‖ index u32` (the input's place in that frame's applied order,
    little-endian).
  - tombstones' value gains, only when the object had object-scope field modes: `‖ mode count u32 (≥ 1) ‖ per mode
    row, in key order: key (35) ‖ value (34)`. A tombstone without any keeps its old bytes.
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
    pub inline: Vec<Vec<u8>>,             // the inline-class ones' bytes, ascending by hash, ≤ 1 KiB (3.4)
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
  holds all of that member's permissions (the user's moderation answer, @coordinator-opus), so
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
  a `Templates` root that the schema lists (the plane's box, sphere, capsule and box stack, 2.4), and creating from
  one is a `Copy`.
- **Template folders are inert** (`pe_world::templates`; @coordinator-opus, option (a) with
  @fable-reviewer's conditions).
  - A **template folder** is any root whose `Name` is exactly `Templates`, also when there are several. pe-api then
    finds no template by name, but each folder stays inert.
  - Every object of a folder's subtree is **inert**. It isn't physical (`World::physical`), so it has no body or
    `BodyState`, no collisions and no gravity. A `Grab` or `Push` of it is refused `OutOfReach`, and
    `World::pick_grab` never picks it. A copy out of a folder is live, and starts at rest.
  - **One predicate.** pe-world's rule and pe-api's template lookup and schema call one function,
    `templates::is_folder_name` (`is_folder_name_ref` on the name's blob reference, so no bytes are needed).
  - **Inertness changes by edit.** Renaming a root to or from `Templates`, or moving an object into or out of a
    folder, makes its subtree's bodies inert or live. The entry's footprint covers them: a reparent's subtree as
    before, and a renamed root in `Footprint::regrouped`, whose subtree pe-replica treats as touched. Bodies made
    inert lose their `BodyState` at the next cut, so they come back at rest. An edit that would make a held object
    inert or live is refused `Held`, as an edit of a held object's parent is (7.2). Renaming away and back agrees on
    two replicas, one hearing of it late, and the bodies come back at rest [V `crates/pe-replica/tests/plane.rs`,
    `crates/pe-world/tests/plane.rs`].
  - **The name rule is interim.** Its intended successor is an explicit marker (a component or flag) read by the
    same predicate, so only that function changes.
  - **Folders** (the `Folder` engine component, id 14; design domain-eng d1 C5, @coordinator-opus):
    that marker, for My Domain. Everything under an object with a `Folder` row is inert at any depth, as a template
    folder's subtree is; the folder object itself stays live, a crate you can carry. `World::inert` reads both rules,
    and adding or removing a `Folder` row puts that object in `Footprint::regrouped` as a rename does
    [V `crates/pe-world/tests/folder.rs`]. Only a world whose preset holds folders registers it (the home, 2.5), so
    every other world's registry and root are unchanged; a `Folder` row in such a world is refused as any unknown
    component's is. The plane keeps its `Templates` name rule. `Folder.flags` is reserved: an edit writing anything
    but 0 is refused, so a flag given a meaning later finds no old world holding junk in it.
  "Double the cube" is a `Copy` too. A copy is copy-on-write in storage (file 04 A1: new ids over the same records)
  [I; an optimisation, invisible in the hash].
- Field writes name a field by its index in the description, and give its bytes. WorldApi turns names into indices
  against the schema at the author's frame.
- **Inline bytes** are the payload's last section, present only when non-empty, so a transaction without them
  encodes as it did before they existed (3.4). A section that is present but empty (a count of 0) is malformed: one
  encoding per transaction.
- **Limits** per transaction [I, to tune]: 4,096 ops, 1 MiB of payload apart from blobs, 2 KiB of inline blob bytes
  (pe-api: 1 KiB; 3.4), 960 KiB of carried simulation blob bytes (pe-api: 768 KiB; 3.4), the whole edit within
  `MAX_EDIT_BYTES`, and 65,536 created
  objects (`MAX_NEW_OBJECTS`), counting each `Create` and every object of each `Copy`'s subtree; `Undelete`
  restores objects that were in the world before, so it does not count. Over a limit is a
  refusal, not a truncation; the object cap's is `TooLarge { limit }`. No edit leaves a record past 256 MiB
  (R_max, 6.6). pe-world's constants are the one
  authority, which pe-api uses, and they are raised only with a measured reason (@coordinator-opus).
- **The world's shape limit** (@coordinator-opus, after @fable-reviewer; the M1
  recording, 2026-11-20). The world holds at most 520 collision shapes (`MAX_SHAPES`): every `Shape` row, whatever
  object carries it, with a body or not. An edit that would leave more, and more than the world had, is refused
  whole as `TooManyShapes { limit, count }`, where `count` is what it would leave (7.6 step 5). An edit that adds
  none, such as a delete, a move, a colour or a swap of one shape for another, passes even past the limit. Every edit
  path is counted, since the check compares the world before and after the whole transaction: `Create`, `SetRow`,
  `Copy` and `Undelete`. Nothing else makes a `Shape` row today. A hand carries none (8), and a script can't make
  one, because a script's create carries only one of its own components and `pe.Shape` isn't script-writable (7.9).
  Why 520 [V, engine-architect's harness, 2026-11-20]: with every ball awake, the user's laptop (i7-8650U, the
  slowest M1 machine) steps 520 shapes in about half a frame period. It took 7.2 ms a frame at 500 balls, 8.5 ms at
  550, 10.0 ms at 650 and 13.9 ms at 850 (headless, 60 Hz, cuts every 6 frames), and the recording fell behind real
  time from about 650. The number is a constant of the build, so the build id's source hash covers it (app.md 4) and
  every member of a session runs the same rule. A world stored past one build will need it in a rules version (the
  release-compatibility item).
  - **x100 for the friend test 11-27** (user, a stress test; @coordinator-opus):
    `MAX_SHAPES` is 52,000. A world that fills it is expected to crawl on either machine, by design.
  - **The world's own limit, changeable mid-session.** A world may carry a `Limits { max_shapes }` row (engine
    component 13, registered only by presets that set it). Its first row in id order is the limit, capped at
    `SHAPES_CEILING` (2^22 - 4,096, Box3D's shape index); a world without one uses `MAX_SHAPES`. The plane carries
    it on a root of its own, `World settings`, the last object of its genesis, set to `MAX_SHAPES`. Changing it is an
    ordinary edit, so every member switches at that edit's frame. Only an admin member may change a `Limits` row
    (`NoPermission` otherwise), and a value of 0 or past the ceiling is refused `Invalid`. The check reads the limit
    after the edit, so one edit may raise it and add shapes. A limit lowered below the count removes nothing, and
    only edits that add shapes are refused.
- **A copy's size** (@design-reviewer's Low 2 on pe-api, @coordinator-opus). `Copy` carries `size`,
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
    first (@coordinator-opus: option (b), since a release is an input, not an edit op, and cannot
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
- **The instance tag** (@coordinator-opus, after @fable-reviewer). Beside each row's version sits a
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

The user's rule: "depends on the order of events. if the delete occurred first, then the add-on is
orphaned. if the delete occured second, then everything is destroyed."

- **Delete second:** `Delete` destroys the object and its whole subtree, add-ons included, as today
  [V `store.rs:131-153`].
- **No edit deletes a hand** (@coordinator-opus): a `Delete` whose subtree holds a `Hand`, anyone's,
  the author's own included, is refused as `Invalid` naming `Hand`, as `Copy` is (7.6). Hands are made and removed
  only by logic: `admit` and the leave (2.4).
- **No edit moves a hand** (@coordinator-opus): a `Reparent` of an object that has a
  `Hand`, or a `SetRow`, `SetField` or `RemoveRow` of its `Transform`, `Body` or `Shape` (the rows a hold covers,
  7.2), is refused the same way, whoever's hand it is. A hand moves only by its member's input, and never becomes a
  body physics would move (8). An object may still be moved under a hand, a subtree that merely holds a hand may
  still move (the hand goes with it), and the hand's other rows are an edit's like any object's. In M1 hands are
  roots, so nothing is above one; revisit the subtree rule with the avatar root (3.5), which will parent them.
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
  (@coordinator-opus). A merge older than that reads them back from the world record (M3,
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
4. **Each op, in order,** against the state just before it, which includes the earlier ops of the same transaction:
   its targets exist (or orphan, 7.5); no protection is broken (7.8) and no hold (7.2); the components are
   registered; the rows have the right size and valid values; positions are in range (4.3); no cycle; the depth limit
   holds (3.3); `Hand` rows are not written by edits (they are inputs), nor is a hand moved or its `Transform`,
   `Body` or `Shape` written (7.5); `Node`, `BodyState` and row versions are never written by edits. Protections and
   holds are checked here rather than in step 2 because an op may name an object an earlier op created, or lift a
   protection a later op relies on; a transaction that does the latter passes only if its author may lift that
   protection (@coordinator-opus).
5. **The world's shape limit** (7.1), on the state after every op and after the inline-blob check (3.4): a
   transaction that would leave more than `MAX_SHAPES` shapes, and more than the world had before it, is refused
   whole as `TooManyShapes { limit, count }`, with no op index.
6. **All or nothing.** The ops apply to a copy-on-write working state; any failure discards it. Nothing outside the
   transaction sees a partial result, and the reported reason is the first failing step's, with the op's index when
   an op failed (@world-model's nits on 796a5a31).

- **One pure function** (@api-eng's O4). `World::apply(state, frame, entry) -> (state', Outcome)` is the only edit
  path. The ordered step, the author's local prediction and a dry run all call it, so they cannot disagree.
- **A dry run** is the same path, run on a scratch copy of the latest state and discarded. It changes nothing and
  returns the outcome the live run would give at that state.
- **As built** (world/edit-v1, `crates/pe-world/src/edit.rs` and `apply.rs`) [I; each to the reviewers, and the
  multi-way ones to @coordinator-opus]:
  - the codec is `edit.rs`'s: little-endian, one-byte tags, flags 0 or 1, `u16`-prefixed text, `u32`-prefixed bytes
    and lists, ids as `u128`. It is strict, so every accepted payload has exactly one encoding;
  - a `Version` precondition on a row that does not exist compares against the all-zero version, so it holds only
    if the author read "no row", or a row only ever stored by a trusted write (`World::insert`) and never edited,
    which reads zero too. No edit-writable row on a running world's non-hand objects is stored that way (the preset
    is an edit; admission and physics write only rows edits can't), so pe-api's guard on a plain `add` reads absence
    (@api-eng, F6) [I]. Its `Stale` then reports `current` and `winner` as zero, which means "absent": once
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

The user (V1): "simple protections can exist for objects the same way resonite functions". With
@api-eng (its O7) [I]:

- A `Protection` row names an owner (`MemberKey`). It protects its object and the object's subtree.
- **Who may protect an object** (@coordinator-opus's ruling U5, filling a gap in V1 that
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

logic.md's N4, N5 and N13, under @coordinator-opus's rulings (with @fable-reviewer's third shape), and @design-reviewer's and
@chunks-reviewer's reviews of r1. A script attaches to an object by
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
  on objects carrying its tag; the owner's consent to the tag covers the writes, on protected objects they own too. Each written engine row passes `World::check_script_row(component, object, bytes)`, which runs
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
  - **`script` is an `AssetTarget`** (@coordinator-opus, after @fable-reviewer):
    `Existing(id)`, an asset the world knows (a current row or a pending record, a voided first version's included,
    so its author can re-commit it), or `New(k)`, a new asset whose id the world derives from the creating entry,
    `script::asset_created(line, author, chain, seq, k) = Hash("pe.asset", line ‖ author ‖ chain ‖ seq u64 ‖ k u32)`,
    from the same coordinates as `ObjectId::created` under its own domain tag, so an asset id never equals an object
    id. No op names a new asset's id, so a modified client can neither squat on another's pending id nor reuse a
    destroyed one; `Existing` with an id the world doesn't know is refused `Invalid`. The id is known before the
    entry is signed: the client computes it with `script::asset_created` from the coordinates it is about to sign.
    `k` is free, as for objects' `New(k)`. A re-sent entry gets a new seq, so a new id, its dependents rewritten by
    `k` (api.md 9.4). The asset design's shared entries derive from the creating entry's id instead
    (assets-on-objects.md, BLAKE3(entry id | index)); both are unique per entry and index, and the coordinates are
    used here because they are known before signing. Wire form: `u8` 0 then the id (32), or 1 then `k` u32.
  - **The source comes first, before any op of the transaction** (logic.md 4.1a; @coordinator-opus). `archive` is the version's source, a canonical archive, carried inside the entry; `source` is
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
  - `activate_at` must equal the entry's frame stamp (logic.md 6.1); anything else is `Invalid`. Only the submitting
    machine knows the stamp, so a client leaves it at 0 and that machine sets it as it stamps the entry
    (`Transaction::set_activation(frame)`, which stamps every `CommitScript` of the transaction; api.md 7).
  - `base: None` makes a new script and is refused as `Stale` if the asset has a row. `base: Some(v)` must equal
    the row's version, or the op is refused as `Stale { precondition: OWN_CHECK, current, winner }`, with the row's
    version as `current` and its entry as `winner`: the loser rebases (R7).
  - **V is the source archive's hash** (`source.hash`). A commit whose V any other asset has ever had is refused as
    `Invalid` ("same version as asset X"), since the two would share every id, tag included (ruled item 2 and: any V it ever had; to revisit with logic on objects, where a copy is a
    reference). The check is exact and costs no history: V's tag id is registered, under another asset.
  - **V equal to the base's V** (a toolchain or SDK change only) re-keys nothing and moves no row version: the op
    only rewrites the modules row (L4).
  - **The op writes a pending commit, not the row** (logic.md 6.2a; @coordinator-opus): version = this entry and op, the author, the source, the hashes, the component ids, `c` = this
    frame, the deadline `D = c + attest_window(source.len, hz)`, the voters at `c` (the governance members now, in
    key order; `q` is the quorum over their count), and the author's own attestation stamped `c` if the author is a
    voter. Nothing re-keys and nothing runs yet. A second commit to the asset while one is pending is refused
    `Stale`, with the pending commit's entry as `winner`. The re-key's limits (`MAX_REKEY_ROWS`, the loss rule) are
    judged now, so the author hears at once, and again at activation.
  - **`AttestBuild { script, version, module }`** is a voter's statement that its build of the pending V passed
    every check of logic.md 6.2 step 3 and gave `module`. It is the only op of its transaction, with no
    preconditions, and needs no `EDIT`, only a place among the pending commit's voters. It is refused `Stale` if no
    commit of that V is pending (none, another V, activated or voided), and `Invalid` for a module other than the
    registered `author_module`, a non-voter's, or a voter's second. A refused attestation changes nothing; its
    signed entry stays in the log as the evidence. A counted one records the voter and its stamp, and supersedes the
    voter's own counted `BuildFailed`.
  - **`BuildFailed { script, version, reason, toolchain, sdk, module }`** is a voter's statement that its build of V
    failed, after its local retries (logic.md 6.2 step 3, 6.2a item 2; the user's build-asymmetry rulings). `reason` is a `BuildFailure`: `Compile` (1), `Refused` (2, 6.2b's dep-info check),
    `Cap` (3), `Mismatch` (4) or `Local` (5); `toolchain` and `sdk` are the failing machine's hashes, and `module`
    the hash a `Mismatch` built, present exactly for a `Mismatch` and never the registered `author_module`, or the
    failure is `Invalid` (@chunks-reviewer's L1). It never carries compiler output: the entry goes to every member
    and stays in the log, and a compiler's message can carry local paths. A slow build is never a failure; nothing in
    the world times one out. Like `AttestBuild` it stands alone in its transaction and needs no `EDIT`.
    - **On the pending commit at V** it is counted, one per voter, with its stamp and reason; `Invalid` for a
      non-voter's, a voter's second, or one after the voter's counted attestation ("a failure after a success is
      refused"). The voter's later `AttestBuild` supersedes it while the commit is pending.
    - **For the asset's current version** it is a record only and changes nothing: the log's statement that this
      member split off (logic.md 6.2 step 3; Q2 (a)). The split itself is the member's own leave (2.4), posted by its
      replica; pe-world never removes a member. `Invalid` from a non-member.
    - **Otherwise** it is `Stale`, as an attestation is: no V pending or current, or a voided commit's (nothing
      changes a void).
  - **`end_frame(f)` activates, then voids** (in asset order). A pending commit with at least `q` counted
    attestations, so `F` is the `q`-th smallest counted stamp, activates at the end of `F`: the re-key below, its
    limits judged again for the author, who must still hold `EDIT`, and then the asset's row (version = the commit,
    the hashes, the component ids, `activate_at = F + 1`, no faulted mark), so the new version runs from `F + 1`. A
    re-key now over `MAX_REKEY_ROWS`, or now losing data on an object its committer may not edit, voids the commit
    instead, as `RekeyTooLarge` or `RekeyBlockedAtActivation`, and an author who has lost `EDIT` since the commit
    voids it as `AuthorRevokedAtActivation`: revoking someone's rights stops their pending code too
    (@coordinator-opus). Otherwise **the shared void rule** (logic.md 6.2a item 4, for a leave and a
    failed build) voids it as `QuorumUnreachable` once its counted attestations, plus the voters at `c` who are still
    members and have neither attested nor failed, number fewer than `q`; at n = 2 the other member's first
    `BuildFailed` voids it at that frame's end. Otherwise a commit at its deadline `f == D` is voided as
    `Unattested`. Activation is tested first, so a quorum completed at `D` activates. A voided commit keeps its
    record (the frame, why, and its counted failures) until the asset's next commit replaces it, and the old version
    stays bound. `end_frame` returns what it did (`FrameEnd`: each activation with its re-key report; each void
    (`Voided`) with its kind, its blocking objects for `RekeyBlockedAtActivation`, and who could not build it and
    why, in key order, which is Q1 (c)'s notice to the author; each named by asset and commit), which pe-replica
    publishes as S5's `FrameView.activations`. **A known limit:** any member holding an affected object can void
    another's commit anywhere in `c..F`, where today's rule allowed it only at `c`.
  - **The author's prediction** (logic.md 6.2a option (i); @vdw-physics):
    `World::activate_for_prediction(asset, frame)` performs `end_frame`'s activation of one pending commit without
    the quorum, right after `end_frame(frame)` on the author's own replica, for a history that is only a prediction
    and never one whose roots are voted on. It returns `Option<ActivationResult>`, the type `FrameEnd` gives each
    activation: `Some(Ok(report))` is exactly what a quorum completed at `frame` would do, the pending record
    included; `None` with nothing live pending; and `Some(Err(why))`, a refusal (a blocked or too-large re-key, a
    revoked author), changes nothing and leaves the commit pending, never voided.
  - **The deadline** is `attest_window(len, hz) = ceil(2 · T_module · hz)` frames, `T_module = W_fg + len / R_thin +
    T_build` in integer milliseconds (2 s, 1,000 B/s, 20 s; divergence.md 4.2, @iter-bench): the
    lamp's 1,213 bytes give 2,786 frames at 60 Hz, the 256 KiB cap 34,098.
- **Re-keying** (ruled item 1). A commit with a base whose V differs moves every row
  of the old version's components to the new version's same-named component, object by object, **the tag rows
  included** (or every attached object would detach), at the commit's activation (`end_frame(F)`, logic.md 6.2a),
  all at once, so no frame sees half-moved rows. Each moved row takes the commit as its row version, so an edit that
  read the old row is stale.
  - **Field mapping**, deterministic and identical on every peer: a field with the same name, kind, `count` and
    unit copies its value; a new field, or a same-name field whose kind, count or unit changed, starts at zero
    (a unit is meaning, never copied silently:, L3); a dropped field is gone; a component whose name
    disappears loses its rows. Padding fields (names starting `_`) are always zero and are never a loss. A moved
    row the new description's checks refuse refuses the commit.
  - **A loss is a dropped field or component, or a zeroed field, whose value was not zero** (a field that was zero
    loses nothing). A loss on an object the committer could not
    edit (held by another member, or protected under 7.8 by another owner) refuses the commit, naming those
    objects; elsewhere it applies. Lossless moves apply on every object, others' and protected ones included. Why: every M1 script is world-level, the user's "linked script", so changing it in
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
  - **What it costs is reported.** The activation's report (`FrameEnd`, logic.md 6.2a) lists the objects moved,
    the row count, and each component and field zeroed or dropped; a dry run (pe-api) shows the same before the
    commit, with the objects that would block.
  - **The refusal names the objects:** a commit whose re-key is blocked is refused as `Reason::RekeyBlocked {
    objects, more }` (S2 v1.6): the first 16 blocking objects in id order, and how many more. At activation a block
    voids the commit instead (`RekeyBlockedAtActivation`, logic.md 6.2a), since no entry is there to refuse; the
    void names the objects the same way (`FrameEnd`'s `Blocked`, built by the same helper).

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
  them the same way, with the rest of 2.4's leave.
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
    /// A release that throws (below): ends the claim as `Release` does and gives the object the velocity the hand
    /// gave it, in the world frame, `linear` at its centre of mass in m/s and `angular` in rad/s.
    Throw { object: ObjectId, linear: [f32; 3], angular: [f32; 3] },
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
them. It never writes the row's `strength` (a hand's strength, below). The whole pack is kept in a world table,
`hand_packs` (hand -> stamp, poses, hold; in the root, so a rollback re-applies the same poses).

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
@coordinator-opus, @fable-reviewer concurring).
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
- **A throw** (`Throw`; the weak-throw fix, @coordinator-opus, with @fable-reviewer's amendments) [I, for review] is a release that carries the velocity the holder's hand gave the object as it let
  go: `linear` is the hand's linear velocity plus its angular velocity × the offset from the hand to the object's
  centre of mass, and `angular` the hand's angular velocity, both in the world frame. The entry doesn't say how the
  client measured it; in M1 pe-app takes the ring's "now" sample at the real opening and logs it with the peak over
  the last ~50 ms of each throw, so a window can be tuned later with no format change. Without it a thrown object
  left with whatever the hold's spring had given it, which lagged the hand. A residual stays [I]: the opening is
  detected late, so the object leaves about 10 cm late along the swing (½·a·t² at 25 m/s² over 90 ms).
  - **In pe-world:** only the holder's throw counts. Anyone else's changes nothing and notes `NoEffect`, like a
    `Release`; a non-finite value refuses it (`Invalid`), and the claim stays. The holder's ends the claim exactly as
    `Release` does, and reaches the physics as a `Thrown` in the entry's footprint [V `tests/input.rs`,
    `only_the_holders_throw_ends_the_claim_and_reaches_the_physics`].
  - **In the physics** (pe-replica, as the entry applies, the way a push goes; the 11-27 kit's rules, the user's
    rulings of 2026-11-26, "remove any maximum throw speed (for v1); the grip strength should be what
    governs maximum throw speed", and, "keep it straightforward and predictable even with the tunneling
    issues"): if a hand's grab joint holds the object, its velocity becomes the throw's, as far as the hold's force
    could have changed it in a frame: the change from its solver velocity is at most `F dt / m`, `F` the holder's
    strength. There is **no speed cap** and no other gate: not the object's distance from its place in the hand, nor
    a wall between them. A light object a hand carries leaves at the hand's velocity (an 18 g ball thrown at 25 m/s
    on a 0.6 m arc leaves at 25.0 [V @vdw-physics]); a heavy one that lags keeps most of what its hold gave it. The
    spin is the throw's, under Box3D's own limit of 45 degrees a tick (replica.md 3.5). The joint is gone before the
    frame's physics step (@engine-architect). With no joint from a hand the object keeps its solver
    velocity, as a `Release` leaves it. A thrown object faster than continuous collision is measured to hold for
    (50 m/s against walls, 8.1's pushes) may pass through a wall or the floor; that is accepted for now. Until
    2026-11-26 a throw was capped at `max_push_speed` (and its spin at `max_push_speed / R`) and voided when the
    object was blocked or past the break-away distance, which lost fast throws whole (an 18 g ball thrown at 25 m/s
    on a 0.6 m arc left at 3.5 m/s).
  - **Order:** a `Release` and a `Throw` are entries like any other, applied in agreed order. The first of them by
    the holder ends the claim; a later one finds no claim and notes `NoEffect`, so a release after a throw takes
    nothing back, and a throw after a release sets no velocity.
  - A `Release` sets no velocity; an agent with no hand releases, or pushes. A throw whose velocity doesn't match the
    hand's visible motion is a trust question (the trust survey's entry); a check against the recorded hand motion
    can come after M1.
- **A hand's mode** (D18b; @coordinator-opus, after @vdw-physics). Whether a hand has
  a physics body changes the simulation, so every peer reads it from the world, never from a local toggle; and the
  user switches it within a session, so it is not a frozen config item (6.6). It is a **personal comfort choice,
  per member, on the hand**, like a VR turning mode: `Hand.mode`, 0 = **closed hand**: physical only while the
  hand is closed (its input's `engaged`, the grasp; the default), 1 = **always within reach**: always physical
  within reach (the grab reach of this section's **Reach** bullet: `reach_mm` from the hand's target, plus
  `slack_mm`). The labels are @coordinator-opus's, so nobody reads mode 0 as D18b's swept reach
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
  - **Hands are not bullets by default** (the user answer 3; [I, draft for @fable-reviewer]): world
    logic builds a member's hand bodies without the bullet sweep (3.2), and nothing in M1 turns it on. Once avatars
    exist (refound plan, piece 10), a player may mark their hand colliders as bullets in a **full** custom avatar,
    at their own cost like any bullet's. A **fallback** avatar's strict format has no such field, so a fallback's
    hands are never bullets, and a world that restricts players to fallbacks has no bullet hands.
  - **A hand that holds something has no cap** (the user's ruling of 2026-11-26): its body reaches
    its target every frame, so a held object follows a swing at any speed and a throw leaves at the hand's speed. Its
    teleport test is the jump away from its own motion (replica.md 3.3), so a fast swing is never placed. Empty
    hands, and pushes that aren't a held object or a throw, keep `max_push_speed` (@fable-reviewer).
    The rest of this bullet is an **empty** hand's.
  - **A hand moving faster than the cap moves at the cap** (@coordinator-opus, option (b)): its body
    trails the target and catches up, so it still pushes and throws at up to `max_push_speed`; there is no cliff
    where hard throws begin. Hard throws top out at the cap, 10 m/s by default and adjustable (8.1's pushes). The
    cap limits the motion of the hand **and everything it holds**: a wrist turn that would carry a held object
    faster than the cap is slowed too (@design-reviewer's Low on 0913f962).
  - **Only a teleport skips collision:** the body is placed at its target without collision, carrying what the hand
    holds, when the target moves farther than the teleport distance in one frame (a tracking glitch or a teleport)
    **or the body trails its target by more than it** (@design-reviewer's Medium on 0913f962). The second bounds the
    lag: a hand kept faster than the cap never trails by more than the teleport distance, so the body never sweeps
    through the scene far from the hand everyone sees. For a held object the separation is **the move the hand would
    carry it this frame**, from where it is, keeping its offset in the hand. **Where a placement puts it**
    (@design-reviewer's L1 on 980e6ba7, @coordinator-opus): a light hold, one whose force cap comes from
    the object's mass (`m (A + g)` at most the strength, the tight hold below), at its place in the hand, since the hand
    could lift it there anyway, so a light object never keeps a lag through a placement; a heavy one, a hold whose cap
    is the strength, at its offset, so a placement never lifts what the strength can't. At 110 N a hold is heavy from
    about 0.52 kg, so an object the hand can lift (up to 11.2 kg) but not speed up at `A` keeps its lag through a
    placement too, as ruled (@coordinator-opus; @world-model's note on 5659a804). **A held object never
    passes through a wall** (@coordinator-opus): pinned against one, its carried move stays the hand's own
    small move, so it is never placed; the bare kinematic hand may pass through, the held object stays pinned by the
    hold's spring, and springs back when the way clears. **A trailing placement carries a held object only if no static
    geometry lies between** where the object is and where the placement would put it (@coordinator-opus,
    option (b), after @vdw-physics measured a pinned ball carried through a 10 cm wall by an overrunning hand's
    placement): a cast of the held object's shape against static shapes only, along its carried move, whose yes/no
    answer does not depend on query order; if it hits, the object stays where it is, held by the spring, and the hand
    goes on. As built (@vdw-physics, ws/hold-reach 9d3a0270), it is Box3D's mover cast: spheres and capsules are swept
    as themselves, a hull as the capsule that encloses it, an over-approximation that errs towards not carrying through,
    the side the wall ruling chose: a hull whose own shape would pass close by a wall's edge may be held back
    (@design-reviewer's Low 1, @coordinator-opus; the capsule inside it would have let its corners clip
    the edge; the enclosing capsule runs along the longest side of the hull's bounding box, its radius the farthest any
    hull point lies from that axis), and a shape the object already touches is passed unless the move goes into it, so a
    ball resting on the floor can still be carried along it, and so can a box whose enclosing capsule overlaps the floor
    (measured [V @vdw-physics, ws/hold-reach f710c9ac]: a 10×10×30 cm box held on the floor, the hand at 15 m/s along
    it, moves with the hand's body at every trailing placement). If the physics fork has no shape cast, five rays
    stand in, any hit counting: one from the object's centre and four from the points at ±r along two axes
    perpendicular to the move, `r` the bounding radius, the axes chosen deterministically from the move's direction
    and the world's up (world x when the move is vertical) (@design-reviewer's nits), and their limits are stated:
    rays this sparse can miss a post thinner than their spacing, and it can pass beside a wall's end or corner, or
    through an opening narrower than the object, leaving part of the object overlapping the wall for the solver's
    push-out to settle (@design-reviewer's Low, adopted by @coordinator-opus). A real teleport (the
    target jumping past the teleport distance) still carries it at its current offset, through whatever lies between.
    **Which placement is which:** a placement is a trailing one when the body moved at the cap the frame before (at
    least 99.9 % of it, at the body or at any hold, read from the kinematic velocity in the image); a body that was
    not slowed reached its target, so a jump now is the target's own, a real teleport. **So a real teleport during an
    overrun counts as a trailing placement:** what the hand holds is not carried through a wall, and its spring brings
    it after the hand (@vdw-physics's flagged limit, accepted by @coordinator-opus as the safer side).
    **A hand held steadily above the cap cycles:** it trails, is placed, and trails again (at 15 m/s the lag grows by
    5 m/s and reaches 0.5 m in about 0.1 s), so its contacts come in bursts, and each placement carries what it holds
    without collision and gives it no velocity from the jump. That keeps "nothing moves faster than the cap" (plus the
    solver's push-out, below); tests expect bursts, not a continuous push (@design-reviewer, approving r2).
  - **The hold is tight, and its force is capped by the holder's strength** (the user's rulings of 2026-11-19:
    "grabbing an object that's light enough should keep it tightly constrained to your hand's position"; "players should have a strength property per char controller that governs how much force they can
    apply to held objects to move them"; a blocked held object stays held and shows apart from the
    hand; @coordinator-opus, after @vdw-physics's sweep). The spring that pulls a held
    object to its place in the hand is stiff, 60 Hz (Box3D's own stiffness for rigid joints at the engine's 1/240 s
    sub-step), critically damped, and its force is at most the holder's strength (the user's ruling of 2026-11-26, option (c): "the grip strength should be what governs"; until then `min(strength, m (A + g))`, `m`
    the held mass, as the sub-points below describe). With the grip alone a hold presses a light object into what
    blocks it with the whole strength: an 18 g ball held against the floor sinks 43 mm, and one held against a 2 cm
    wall goes through at 1 m/s [V @vdw-physics, pe-replica `measure_push_through_at_the_grip_s_strength`], accepted
    for now (a stop at fixed surfaces, which held it to 2-5 mm, waits on a backlogged item):
    - **`Hand.strength`**, in newtons, is the player's strength: M1's stand-in for the character controller's, where
      it moves later. World logic writes it at admission, 110 N, the user's value (confirmed): a default player moves about 0.5-1 kg with no lag, and 110 N holds 1 kg tight up to a
      100 m/s² flick. No input writes it, and no edit (a `Hand` row is not editable,
      7.6); it is in the hand's leaf, and an import carries it. Both hands of a member have the same. A row from a
      build before it reads its former padding as 0 N, a hand that holds nothing up; builds of different layouts
      never share a session.
    - **`A` = 200 m/s²** (provisional; pe-replica's `HOLD_ACCELERATION`) is the most the hold speeds up what it holds
      beyond holding it up: what a light object needs to follow a hand speeding up at `A`. Without it a stiff spring
      pushed an 18 g ball at up to strength / m, 6,000 m/s², which Box3D's 30 Hz soft contacts can't hold: it sank
      43-47 mm into the floor and was pushed through a 2 cm wall at every world rate. With it the ball sinks 1.5 mm
      and is never pushed through [V @vdw-physics, pe-physics `examples/hold_sweep.rs`].
    - **So a light object follows the hand within millimetres,** leaves at its speed and sags 0.07 mm held still: an
      18 g ball trails a hand speeding up at 100 m/s² by 2.6, 1.0 and 0.6 mm at 30, 60 and 120 Hz (about 9 cm with
      M1's 5 Hz spring), and at 200 m/s² by 13.5, 22.8 and 7.3 mm [V]. At the 30 Hz world rate, whose hand changes
      speed every 33 ms, flicks past about 100 m/s² trail more (19 mm at 150 m/s² in the sweep). **A heavier object
      lags** once the hand asks for more than the strength: at 110 N and 60 Hz a 100 m/s² flick trails 1.0 mm up to
      0.5 kg, 18 mm at 1 kg and 92 mm at 2 kg; 8 kg is lifted and held, and a hand can't lift 11.2 kg or more, which
      stays on the floor, still held [V]. A hand of 60 N doesn't lift 8 kg [V].
    - A blocked held object stays held; the display draws it where it is once it is more than 5 cm from its place
      (render.md D4's break-away).
    - On an awake stack the tight hold costs what M1's did: 400 balls stirred by a held ball step in 1.58, 1.13 and
      0.88 ms at 30, 60 and 120 Hz, against 1.63, 1.11 and 0.89 ms with M1's hold and 1.64, 1.13 and 0.87 ms with the
      hand alone [V, interleaved in one process, idle priority, not a clean timing run].
  - **The hold's spring is bounded, speed-aware** (@coordinator-opus, decision 2; its floor): the separation the spring that pulls a held object to its place in the hand sees is clamped as a
    whole, and then only its part supporting the object against the world's gravity (6.6, a fixed direction, so the
    clamp does not depend on order) is raised to a floor. Write `e` for the object's error, from the object to its place
    (the way the spring pulls), `ĝ` for gravity's unit direction, and `up(x) = max(0, −x·ĝ)` for the upward, supporting
    part of a vector `x`. With `reach = max(0, min(max_push_speed − v_hold, √(2 a |e|)) / k)`, where `v_hold` is the
    hand's own speed at the hold (its turn included, ω·r), `k` Box3D's spring rate (105.6 per second for the 60 Hz
    spring at 30, 60 and 120 Hz alike, whose sub-step is 1/240 s; 14.7 for M1's 5 Hz spring), and `a` the deceleration
    the hold's force cap `F` gives its object along the way it comes, gravity's part included, `a = max(0, F / m − g (ĝ
    · ê))` with `ê` the error's direction (the tight hold, @vdw-physics: the spring asks for a speed of about `k` times
    the error it sees, and a hold capped near its object's weight would otherwise ask for one it can't stop from; an 8
    kg ball lifted by 110 N came up at 2.6 m/s, went 12 cm past its place and fell 54 cm below it before rising again,
    and now settles at its place [V]): first `e1 = clamp(e, reach)`, `e`'s length clamped to `reach`; then, if `up(e1)`
    is below `min(up(e), s_min)`, the difference is added along `−ĝ`, so the spring sees `e' = e1 − max(0, min(up(e),
    s_min) − up(e1)) · ĝ`, recomputed every frame from the current state. It sees at most `reach` plus a support of at
    most `s_min`, in every direction: clamping the parts along and across gravity each on its own would let a diagonal
    error come back at up to √2 times the cap (14 m/s for a ball pinned below and beside its place, then released)
    (@design-reviewer's Medium on ws/hold-reach 9d3a0270, adopted by @coordinator-opus). Without gravity
    `e' = clamp(e, reach)`. **The floor `s_min`** is `(A + g) / ω²`, with ω = 2π · 60 Hz the spring's own stiffness, not
    its bias rate `k`: the separation at which the spring's pull on a light object reaches the hold's force cap (1.48 mm
    at 9.81 m/s²), so the support may use the whole cap to keep its object up while catching up takes the rest (the
    tight hold, @vdw-physics). A ball held still in the air sits `g / ω²` = 0.07 mm below its place at 30, 60 and 120 Hz
    [V]. With M1's 5 Hz spring and 1,000 N the floor was 3 · g/ω² ≈ 0.030 m, a margin set from @vdw-physics's
    straight-up and straight-down measurements so the spring could lift a sagging or knocked object back up rather than
    leave it hanging at the balance point (@design-reviewer, adopted by @coordinator-opus); under the force cap that floor starved the support, and a ball sank 4.3 cm below its place in a 2 s
    overrun, against 0.4 cm with this one [V]. In a world without gravity `s_min` = 0 and the clamp is the plain one.
    Without the floor the bound reaches zero whenever the hold moves at the cap, the spring stops holding the object up
    and it falls out of the hand; a floor in every direction instead would let the spring pull it down on top of
    gravity, and a clamp on the error's whole length would shrink its upward part with a long trail and let it sag
    (2.9-5.5 cm measured, @vdw-physics). The floor's only excess over the cap is the support it needs, at most about
    `s_min · k` (0.16 m/s at 1.48 mm; 0.44 m/s at 0.03 m with M1's spring, measured then at most 0.24 m/s, and only
    upward, where a floor in every direction reached 10.27-10.50 m/s [V @vdw-physics]), inside "the cap plus push-out"
    (@design-reviewer's Medium and its shape, adopted by @coordinator-opus).
    Free fall under gravity is not a push. So a held object never exceeds the cap to catch up: it trails a hand kept
    above the cap and catches up once the hand slows, and a pinned object springs back at most at the cap. The tight
    hold's stiff spring closes the error it may see within a frame (`k · dt` over 1), so a pinned pit ball comes back at
    about its reach a frame: 2.8, 4.7 and 6.4 m/s at 30, 60 and 120 Hz [V] (8.2-9.0 m/s with M1's 5 Hz spring, against
    10-50 m/s unbounded, held back 1-6 m). How is the hand-body spec's (@vdw-physics: the hand-side anchor's position,
    never its frame's rotation, moved towards the object for the step only, so images, digests and rollbacks see the
    original anchor).
  - **The teleport distance** is `max(0.5 m, 2 · max_push_speed / rate)` [I, 0.5 m a starting value to tune]: at
    least twice the cap's distance per frame at the world's rate, so a band of trailing remains at any rate (at
    20 Hz and 10 m/s the cap alone is 0.5 m a frame).
  - **As measured** (@vdw-physics, ws/hands 0eebdfca, 60 Hz, 18 g pit balls [V]): "the cap" holds up to the
    physics engine's push-out:
    - **Swept objects** reach up to 10.48 m/s: a body driven at exactly the cap that penetrates is pushed out at
      Box3D's contact speed (3 m/s). The bound tests assert is `max_push_speed` plus the contact speed.
    - **A held object in a sustained overrun** peaked at 11.8 m/s with the hold's spring unbounded (its overshoot
      after each placement); the speed-aware bound below brought it to 9.9 m/s (9.97 at 120 Hz) with M1's 5 Hz spring
      [V @vdw-physics, ws/hold-reach], and with the tight hold it peaks at 8.5, 10.47 and 10.06 m/s at 30, 60 and
      120 Hz [V].
    - **Throws** leave at the hand's speed up to the cap: with the tight hold a pit ball thrown at 4, 8, 11 and 14 m/s
      reads 3.84, 7.91, 9.86 and 9.86 m/s one frame after its release, gravity having taken 0.16 m/s of it [V] (M1's
      5 Hz spring, before the hold bound, gave 4.00, 8.23, 10.43 and 10.67 m/s, and 9.3 or 9.5 m/s from an 11 or
      14 m/s hand with the bound). **The
      0.5 m edge:** a throw at hand speed `v` over the cap trails by about `(1 − cap/v) · travel`, and once that
      passes the teleport distance it is placed in its release frame, leaving nearly at rest: 20 m/s over 1.33 m (0.56
      m behind as measured, the hand speeding up; leaves at 0.66 m/s), and by the same estimate 15 m/s over 1.6 m or
      12 m/s over 3 m (@design-reviewer, [I]). M1 keeps 0.5 m (@coordinator-opus): such throws are rare,
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
    ball with the hand at 1.5× the cap for 2 s: **sideways**, it sags no more than about 5 mm below its place's
    height and never falls below the hand (4.6, 4.0 and 3.7 mm at 30, 60 and 120 Hz [V @vdw-physics]); **straight
    up**, it rises the whole time at no more than about the cap and never falls; **straight down**, the spring adds no
    downward pull beyond the speed-aware reach (with the reach at 0, the ball falls freely until it is below its place,
    then is held up), with gravity's fall reported apart. In every run its speed exceeds the cap by at most about half
    a metre a second, gravity's fall aside (measured with the tight hold at 60 Hz [V]: 10.47, 10.02 and 10.61 m/s, the
    last with gravity's fall). **It trails by about one placement cycle's lag:** each placement carries the light ball
    to its place in the hand with no velocity, and the force cap takes about three frames to bring it back up to the
    cap, so it trails 0.20-0.23 m at 30, 60 and 120 Hz and no more [V]. Carried at its offset, as before the light-hold
    rule above, it kept its lag through every placement: after 2 s it trailed 1.1-3.3 m, and at 30 Hz it sagged 38 mm
    [V]. **A heavy hold keeps its offset:** a 12 kg ball a hand of 110 N can't lift stays on the floor under the
    raised hand, and a 1 m teleport of the hand carries it 1 m along the floor, not up to its place [V]. A ball pinned
    at 45° below and beside its place, and at 45°
    above and beside it, released with the hand still: its top speed, the larger of its speed at a frame's end and
    the distance it moves in a frame, is at most the cap plus about `s_min · k` (measured with the tight hold: 4.65
    m/s from a corner below, 4.85 m/s from a shelf above; 8.39 and 8.57 m/s with M1's 5 Hz spring [V @vdw-physics,
    f710c9ac]) (@vdw-physics's follow-up; @coordinator-opus; @design-reviewer). **The tight
    hold's tests** (@coordinator-opus): a pit ball follows a hand speeding up at 100 m/s² within 4 mm
    and at its speed, at 30, 60 and 120 Hz; a ball held 20 cm into the floor sinks less than 3 mm, doesn't creep aside
    and comes out with the hand, at each rate; a ball held into a walled pile pushes it gently (no pile ball over
    1 m/s; measured 0.65 m/s), the pile stays in its box and the ball above its lower layer, and a hand input of the
    press arriving late replays to the same roots; a hand of 110 N lifts 8 kg to its place, one of 60 N doesn't.
- **Expiry at `end_frame(f)`.** Every claim whose `until <= f + 1` ends, and its holder gets a cooldown on that
  object until `f + 1 + cooldown frames`; cooldowns past their `until` end. A disconnected holder's claim simply
  expires, freeing the subtree. Claims and cooldowns are world tables, in the root (6, 12.11).
- **Same-frame grabs** of one object by two members: the first in agreed order wins and the second is refused
  `Held`, by the engine's fixed order.

**Pushes** (@coordinator-opus).
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
  @coordinator-opus). This is not optional; it gates opening a world to strangers.
- **Agent powers** (user rulings, relayed by @coordinator-opus, overruling the hand-less gate of;
  @item:primordial-engine/agent-actions-within-the-owning-user-s-simulatio). This settles the strangers-gate item on
  hand-less grabs. The design below was decided by @coordinator-opus (with
  @fable-reviewer's and @design-reviewer's review of 95fc4bfc). It is built in a later slice; slice B
  has none of it (below).
  - **What an agent may do.** Any action at any speed and distance, moving instantly and grabbing objects of any
    size, as long as the object lies within its owning user's simulation range. That is the one bound on where an
    agent acts. Agents may set up "chunkloaders" that force regions loaded, only with their owning user's approval
    (M4 regions). The sim-range check is not in slice B (@engine-architect places it in the plan).
  - **Agent powers are a per-member right, granted by the world's admins.** Admins and the members they trust have
    it; guests don't unless an admin grants it. A person acting as their own agent is the same member, so the right
    is checked on the member, never on a claimed actor (an `Input` names none).
    - A role bit `perm::AGENT` (5.4; `1 << 1`, stored but not checked until agent powers are built) gives the
      default: the admin role has it, Guest doesn't. **M1's joiner has it** (@coordinator-opus): the
      genesis record admits both the opener and the joiner `ADMIN | EDIT | AGENT` (2.4;; until
      @engine-architect's join branch lands, the interim genesis gives the joiner `EDIT | AGENT`), since M1's slice
      needs either machine to edit through an agent.
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
  - **Speed-change caps on pushes** (push tunnelling; @coordinator-opus, @fable-reviewer). The N·s budget protects heavy bodies; these protect light ones, which one budget-sized push
    would send at about 110 m/s, through walls (@design-reviewer's arithmetic on the 18 g ball).
    - **Per push:** `|impulse| / m` may not exceed `max_push_dv`. **Speed cap** (@design-reviewer's Medium on
      5da33763, accepted by @coordinator-opus): the push is refused if `|v + Δv|` would exceed
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
    - **The tested envelope, and above it** (@coordinator-opus, option (b) for M1, on @fable-reviewer's
      advice). The engine sets no hard speed bound: one would narrow the user's "any speed" ruling, and
      would not close the hole, since throws, collisions and scripts can still take a small body past it.
      - **Tested** [V, @vdw-physics's widened sweep, physics/ccd 1cda443e, 73 aims per case]: spheres of radius down to
        0.5 cm against walls down to 1 cm thick, at 30, 60 and 120 Hz, CCD on: nothing fails at 50 m/s or less, at any
        tested rate, size or wall thickness (deepest press-in 1.3 cm). Above that, two failures, classed apart by
        @vdw-physics's rerun: small balls can **tunnel** through thin (1 cm) walls (or end embedded in
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
  its reach is checked (`OutOfReach`), though not `Held` (@design-reviewer's Low 1).
- **The client's pick** (@coordinator-opus and its ruling, after @fable-reviewer): `grab()` applies
  through `grab_check`, its checks in the order above, read-only, and `World::pick_grab` asks the same function which
  object a closing hand should name (app.md 3, step 2). It takes bodies marked `grabbable`, hands excepted, whose
  **real shape** is within `reach_mm` alone of the target the client is about to send (11-27 ruling,
  after the friend test's "if one of them is large, i cant grab anything": the bounding sphere of a
  platform contains a hand above it). A real shape is a sphere, a capsule, or the oriented box of `size` (times the
  scale on each axis) for a box, hull or mesh; each lies inside its bounding sphere, so a pick is always in a
  `Grab`'s reach. They are ordered by the gap between the hand's sphere (`HAND_RADIUS` 0.09 m about the target) and
  the shape, 0 for everything it touches, then the smaller bounding radius (a ball in the hand beats the platform
  it lies on), then the nearer centre, then the lower id. `grab()`'s own reach check keeps the bounding spheres
  (reach plus slack), which a pick's choice always passes. `grab()` itself checks
  neither `grabbable` nor, for `hand: None`, reach: a gap docketed for after M1. `until` only ever grows. A
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
- **The apply order of a frame** (@coordinator-opus, option (a); @fable-reviewer's conditions are requirements):
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

### 8.2 Player bodies and the Move input (friend-test locomotion, L2)

The friend test's first-person locomotion (the user; engine-architect/friend-test-replan.md section 6,
piece L2, @item:primordial-engine/locomotion-l2-pe-world-a-player-body-per-member-t). pe-world holds the body and
its input; pe-physics moves it (L1's `move_player`, @vdw-physics); pe-replica's drive calls one per body per frame
(@engine-architect). All [I] until the friend test runs.

- **Only presets that ask for bodies get them.** The plane registers the engine component `Player` (id 12) at
  genesis, through `SimStore::register_engine`; the ball pit never does, so its registry, its members (two root
  hands) and its roots are M1's. The registry table carries `Player` to a joiner like any description (6.6).
- **The body** is `ObjectId::body(line, member, 0, frame)`, made by world logic in `World::admit` with the hands: a
  root whose `Transform` is at the feet (the spawn slot's feet, on the floor), with a `Player` row and a `SimSize`.
  It is 3.5's avatar root: the hands are its children, at the same offsets from the feet as M1's root hands. It has
  no `Shape` or `Body` row: pe-physics builds the player's capsule from its own tuning when it sees a `Player` row
  (`add_player`). Players are walls to each other there (the friend test of 11-27, the user's,
  superseding 3a's pass-through; replica.md 9's mover), and a player's own hands keep it from shoving what they
  reach for (point 3).
- **Its drawn shape** (@engine-architect) is the body's one other child,
  `ObjectId::body(line, member, 1, frame)`: a `Transform` 0.85 m above the feet, a capsule `Shape` (r 0.25 m,
  half-height 0.6 m) and an `Appearance`, with no `Body`, so it is never physical. It is what the view draws of
  another player. `add_player` takes the physical capsule from `PLAYER` tuning, never from this child. The body root
  never carries a `Body` row: if it did, the drawn capsule would become its compound collider and count in `bounds()`
  for reach (@chunks-reviewer). The body never turns, so its frame is the world's axes
  at its feet. A returning member keeps its body.
- **A body is placed like a hand** (7.5, 7.6; @chunks-reviewer's I1 and M1): no edit writes or removes a `Player`
  row, and none writes a body's `Transform`, `Body` or `Shape`, reparents, deletes, copies or tags it, the owner's
  own edits included. Each is refused `Invalid` naming `Player`. No script writes a body's rows. Only world logic
  and the mover place a body; a respawn or a teleport is a later logic action. The same holds for the drawn shape,
  whose look (`Appearance`) stays editable, as a hand's is. No edit creates, copies or reparents anything directly
  under a body, so every child of a body is logic's: its hands and its drawn shape. Under a hand is allowed.
- **`Player`** holds the owner and the newest Move sample: the stick (i8 steps of 1/127 after the dead zone), the
  stick's yaw, the cumulative heading (a wrapping u32, 2^32 one turn, positive turning left, 0 facing -Z), the press
  count `jumps`, the head's rig-frame pose and whether it is tracked. Three fields are derived by pe-world, from rows
  alone, so every machine has them and rollback restores them:
  - `jump`, 1 only on the frame `jumps` rose: the mover's one-shot jump. The first sample a body gets adopts the count
    without a jump, so a held or re-sent sample never jumps twice;
  - `prev_head` and `prev_heading`, what the step used last frame, rolled forward by `end_frame`: the mover's
    room-scale step is the difference, rotated by the heading;
  - `moved` and `prev_feet`, how far the feet moved over the last frame, at most `MAX_MOVED` (0.5 m [I]: about the
    mover's top speed plus a fall's over one step), so a respawn or a bad mover step can't open a hand's reach
    (`prev_feet` is `pos - ORIGIN`, 4.2).
- **`Input::Move { body, samples, hold }`** (tag 6): the owner's samples, packed and held like a hand pack (8.1),
  1-16 samples. Sample `j` is what the step reads at `stamp + j`; the last is held `hold` more frames, and after that
  the stick is zero (the body stops, the heading and head stay). Any other member's is refused `NotYourHand`
  [I: pe-seams has no reason of its own for a body]. A sample is refused `Invalid` for a stick axis of -128, a head
  more than 50 m from the rig's origin on an axis, or a non-unit head rotation. `MoveSample::quantise_stick` is the
  senders' quantiser: the stick's length clamped to 1, each axis rounded to 1/127, symmetric (@xr-eng). The samples are raw values, never a velocity (the trinket seam). The rate is a
  hand pack's: one pending pack per body, which each Move entry replaces, of at most 16 samples. A per-author entry
  budget would be pe-agree's, and it has none yet [V: pe-agree core.rs 310].
- **`Input::BodyHand { hand, poses, hold }`** (tag 7): `Hand`'s pack for a hand whose parent is the author's body,
  each pose an `Offset` and rotation in the body's frame, at most 4 m from the feet on an axis. The sender computes it
  from the heading and head pose it put in that frame's Move (the two must agree, or the hand and the body it hangs
  from disagree by a frame's turn); receivers only read the result, so no trig runs on the step side. A world-frame
  `Hand` input still works on a body's hand (converted to the body's frame), and each kind of input ends the other's
  pending pack. Both are in phase 0 with `Hand` (8.1): `input_phase` now names tags 0, 6 and 7. A world without bodies
  has no such entries, so its frames apply as before.
- **The frame, as pe-replica's drive runs it** (@engine-architect, L3): each body's `Player` row into L1's
  `MoverInput` (`walk_axes` in pe-physics turns heading plus stick yaw into world axes with the fork's trig), then
  `move_player` in id order, then the hand targets from the returned pose times each hand's local transform, then the
  step. The body's pose is written back to its `Transform` like any bound body's. `World::players()` gives the rows
  in id order; `World::body_of(member)` the body.
- **The void** (the user's): a body whose feet the move leaves more than `players::VOID_DEPTH` (30 m)
  below the domain's origin, the plane's floor top, is put back at the world's respawn point
  (`WorldModel::respawn_point`: pe-world's `World::join_spawn`, the lowest free spawn slot), at rest: pe-replica
  removes the player and adds it again there, so its velocity, ground and jump timers start over, and writes that
  pose. It happens inside the step and reads only hashed state, so every replica respawns the body at the same frame
  and place, and a rollback or a replay repeats it. The hands follow as from any teleport (8.1). A world model without
  a respawn point (the default) lets the body fall on. Other bodies that fall are not put back [V
  `crates/pe-replica/tests/players.rs`].
- **Reach** (7.2, @engine-architect): a grab by a body's hand is judged before this frame's step moves
  the body, so its reach adds the body's `moved`, about |v|·dt (7 cm at 4 m/s), at most `MAX_MOVED`. A known
  looseness that a later reorder of the input phase around the mover could remove. Every other reach path
  (`pick_grab`) measures through `world_transform`, which composes the body. A push names no hand: its `at` is a
  world point checked against the object's bound. A throw's velocity is the sender's world velocity (`Throw`
  carries it), so it already includes the body's; pe-world adds nothing.
- **Leave** (2.4): a body's hand's children become roots keeping their world pose; then the leaver's body goes,
  with its drawn shape and its Move pack. `LeaveReport::bodies` names each body, then the rest of its subtree. A rejoin makes a new body (the id includes the frame).
- **In the root** (6.2): pending `BodyHand` and `Move` packs sit in the hand-packs table under 17-byte keys (the id
  and the tag byte 1 or 2), in key order, and in the older state hash only when there are any. A world without
  bodies has exactly the tables, and the root, it had.
- **Not yet:** platform carry (ruled out for the first mover); the course to platform on (L7); pe-api's
  body in `view_objects` and an agent's reach from the body (L5, @api-eng); the view's rig (`local_rig` = body + R(heading)
  · (-head offset), pe-app, L3).

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
  earlier "last 8 final cuts" is withdrawn, since every limit is a duration.
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
    @design-reviewer's correction). The set is computed when the stage starts. The logic stages run
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
    leave none (after @design-reviewer's Medium 1). An object created and removed in the same stage
    leaves none either: nothing of it existed before the stage, so there is nothing to undelete (r2 Low 1);
  - gives each created object its `SimSize`: 1 m times its spawner's world scale (3.5);
  - sets each faulting asset's faulted mark to `(frame, kind)`. The frame's first fault wins, a mark already set is
    kept, and the modules row's version does not move, so the author's next `base` still matches;
  - returns the physics effects: written `Transform`s of bodies and removed or orphaned bodies, which pe-replica
    acts on as it does for an edit's footprint.
- The runner's row writes and its restores mark leaf caches stale through pe-sim (6.4), as any write does.
- **What runs** is pe-replica's to derive from the modules rows of the frame it simulates (@iter-bench): after a commit applies at frame F, or an import replaces the modules table, it builds and binds
  each listed version (`Loaded::bind` with the ids derived from V and the manifest, the tag's id, and the engine
  table); after a rollback before F it puts back the version the row names then. `World::modules()` lists the
  rows. **A new version runs from frame F's own logic stages:** the commit is an entry, entries run before logic,
  and the re-key has already moved the rows (@chunks-reviewer's r2 L1). A machine still building it binds its old
  module under the new version's ids (logic.md 6.2, @coordinator-opus): every component the edit left
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
- **The read side is built** (world/read-v1, agreed with @api-eng): `World::snapshot` gives an
  `Arc<WorldSnapshot>` that shares the world's copy-on-write buffers, with `roots` and `all` in id order, `children` in
  their order, `info` (name, parent, object and structural versions, creator, `held_by`, `protected_by`, component
  types), `fields` decoded by name (padding hidden; a `Transform`'s place is one `pos` field, a `Pos` for a root and an
  `Offset` in its frame's space otherwise), `world_transform`, `offset_in(id, frame)` and `distance`. pe-app adapts
  it to pe-api's port.
- **For edits and scripts** the snapshot also gives `members` and `perms`, `claim` and `protected_by`, a row's
  instance tag for a `RemoveRow` (`row_tag(id, name)`, or `row_tag_of(id, ComponentId)` where a name doesn't single
  out the component, as with every script's `pe.tag`), the same by id for a row's fields (`fields_of(id,
  ComponentId)`), an object's components (`components_on`) and a description (`component_desc`), since a name finds
  only the first registered component of that name and the registry keeps every script version's, the modules
  table (`modules()`) and the pending commits (`pending()`, voided ones kept as their record), and `dry_run`: the
  outcome the edit path would give a transaction from a draft, changing nothing (7.6), with `dry_run_state` also
  returning the world after it. A new script's asset id is computed by the client with `script::asset_created`
  (7.9).

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
   - [V] tests/covers.rs, on `pe_world::test_util` (feature `test-util`): thirteen kinds of damage (a component row,
     governance, a claim, a script component row, a module version, `BodyState`, a pending attestation, the
     registry, a cooldown, a tombstone, a hand pack, a push window, and a row version alone with its bytes
     unchanged) each move the root, a walk from the root names
     exactly the damaged object or table, and every table but the empty `LINES` is reached. tests/tree.rs adds the
     seed, a setting (the depth limit), pe-agree's settings record, `physics_global` and the frame.
   - Waits for `Local` objects, which M1 doesn't make (6.6): a change to one changes nothing.
   - The repair half (rungs 1 and 2 cure each kind on the real replica) is @diverge-eng's, on the same hook.
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
5. **`pos.cell(level)` and the wall** (4.2): kept. @coordinator-opus ruled: the user's concern was
   precision loss, and exact integer cells cannot lose it [I, the coordinator's reading]; the liaison tells the user,
   who may correct it.
10. **Simulation size** (3.5): accepted by the user.
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
12. **A child's creation moves its parent's version** (@fable-reviewer). So an agent that recolours
    `/Table` with an `ObjectVersion` precondition is refused whenever anyone adds a child to it. If that proves
    common, the parent's own rows and its child list could carry separate versions. Measure before changing it.
    A row `Version` precondition already avoids it.
13. **Field modes (3.7), open from file 21's section 12:**
    - the default sizes and rates for published values, and the shape of the "fed by" declaration;
    - where per-player values the world remembers (Q11) are stored. They must stay on the owner's machine for a
      desynchronized field to be secret;
    - the exact rule for a contact a player sent from a predicted world that the agreed state then refutes (the
      ball was never there). Contacts are checked on the step against the agreed state;
    - a private script's writes reach the world as its player's input in the 2026-11-07 design. Today edits reach
      it only through the API, and no script-side path has been checked in code.
