# WorldApi and the MCP tools (WS-api)

Status: draft v1, 2026-11-10, by @api-eng. Nothing in it is built. Owner of this file, `crates/pe-api` and
`crates/pe-mcp`: WS-api. Docket: @item:primordial-engine/ws-api-worldapi-pe-api-the-mcp-adapter-pe-mcp-an.

v1 folds in @world-model's answers (`docs/spec/world-model.md`, branch `world/spec`, adbf8c7), @engine-architect's
answers on the seams, and @coordinator-opus's rulings U3 and U5.

How the surface reads to an agent and to a player (descriptions, messages, cues, the inspector) is @gnosarium-ux's:
`docs/ux/api-surface.md` and `docs/ux/inspector.md` (branch `ux/api-surface`, 75615ce; 70fb723 approved by
@chunks-reviewer). This file is the authority on types and semantics. It adopts that draft's names, shapes and
wording except where section 13 says otherwise.

Sources: the re-founding plan (`docs/refound-plan.md`, b610464: section 1 M1, 3.1, 3.2 S6, appendix A); the review,
`<notes>` files 06, 08, 11 (3.2, 3.7), 14 (P4) and 17 (7); the old `crates/pe-mcp` at 757219f;
the user's decisions of 2026-11-10 in `<notes>`.

Marks: **[V]** verified in the named file or record; **[I]** my judgement; **[agree: X]** needs owner X's agreement
before it is fixed (section 12 lists them).

## 1. What it is for

- **One typed, in-process API over the live world**, which every consumer calls: the MCP server, pe-app's own
  tools, and later the inspector, the editor and the first-party agent loop. "Every capability a human has also
  exists as an MCP tool" then holds by construction (file 06, A1).
- **M1 needs it for the slice's step 5** (plan section 1): an agent on machine A reads the world (`world_query`),
  changes it in one transaction (`world_apply`) and looks at the result (`view_capture`); both machines show the
  change; an edit against a stale version is refused with its reason and the change that won.
- **And for the small logic edit the user added to M1** (U1): the agent checks and commits a script
  through the same surface (section 7).

What it replaces [V file 06, sections 1-3]: the old surface could change two things (an impulse, the agent's own
hand); its seam depended on the harness; the live peer's main loop ran on pe-mcp's thread, and `world_step` blocked
every other call for up to 10 s; there was no transaction, change feed, undo or image.

## 2. Rules every part of it keeps

1. **It never blocks the simulation.** Handlers read a published snapshot and hand transactions to the simulation
   thread through a bounded queue. Nothing in pe-api takes a lock the simulation thread waits on (file 06, A1).
2. **It never makes its caller wait for agreement** (the governing principle). A transaction applies
   at once on the local machine, as a prediction, like a player's own action. Its finality, or a later refusal,
   arrives through `watch` (or through `apply` when the caller asks it to wait).
3. **One set of rules.** A dry run, the local prediction and every machine's ordered step validate a transaction
   with the same pure function of pe-world. The API adds no rule of its own beyond its rate and size limits.
4. **Nothing the API returns is a promise until it is final.** Every result names the frame it read and the newest
   final frame.
5. **Rights come from the token, never from arguments** (kept from the old pe-mcp [V `src/auth.rs`]). The world's own
   permissions are checked again by every machine at the ordered frame.
6. **Peer-authored text is data.** Object names and string fields written by other players reach an agent only
   inside JSON string values of structured results. An engine-written `message` quotes peer text, never splices it
   in bare (file 06, low L-findings).
7. **Positions are never absolute by accident** (file 17, 7). Every position the API takes or gives
   is an offset from a named frame; the world origin is a frame the caller must name on purpose (4.4).

## 3. Where it sits

```
 MCP client (Claude Code, ...)            inspector / editor (later)         pe-app's own tools
          │  stdio bridge, pipe, loopback HTTP       │                               │
          ▼                                          ▼                               ▼
     pe-mcp ── thin adapter ──────────►  S6 WorldApi (trait in pe-seams), implemented by pe-api
                                                     │
               ┌─────────────────────────────────────┼─────────────────────────┬──────────────────┐
               ▼                                     ▼                         ▼                  ▼
        snapshot port                          edit port                 capture port        logic port
   (latest published world and the       (bounded queue into the     (pe-render's offscreen   (pe-logic's
    frame's change record)                simulation thread)          path, via pe-app)        check; U1)
```

- **pe-api depends on** pe-seams (S1 ids and entries, S6) and pe-world (the snapshot type, the schema registry and the
  pure edit function). It does not depend on pe-replica, pe-agree, pe-net, pe-render or pe-logic: those reach it
  through the four ports, which pe-app wires (section 11). Its tests use mock ports.
- **pe-mcp depends on** pe-seams and pe-api only, plus rmcp, tokio and axum. It never depends on pe-harness; the
  harness drives a session through the same WorldApi.
- **Threads** (plan 3.2; WS-app decides with WS-state and WS-render): MCP handlers run on tokio worker threads and
  call WorldApi through `spawn_blocking`; the simulation thread drains the edit queue once per frame; captures run
  where pe-render puts them (section 8).

## 4. The data the API speaks

### 4.1 Objects, components, fields

These are pe-world's (world-model.md 3); the API exposes them as they are.

- **Object**: a 128-bit id, a parent, an ordered list of children, a name, the member who created or imported it
  (`creator`, write-once; world-model.md 7.8), and components.
- **Component**: a fixed-size typed row, named by its type (`Transform`, `Shape`, `Body`, `Appearance`, `Light`,
  `Name`, `Protection`, and a logic module's own types). An object may hold several of one type; the second is
  `Appearance#1`, by its instance number (world-model.md 3.1).
- **Field**: `Component.field`. The API names fields; pe-api turns names into field indices against the schema at
  the author's frame (world-model.md 7.1). Variable-length data (a name, a source text) is a blob in the store and a
  string or `"b:…"` hash at the API.
- **Derived, read-only fields** appear in the schema with `x-read-only`: a body's `mass_kg` (mass comes from each
  shape's density, world-model.md 3.2) and the simulated velocities. Writing one is refused as `invalid`, naming the
  field to write instead (`Shape.density`).
- **Transform**: a child's position, rotation and scale are relative to its parent, as in Resonite (file 17). A root's position is an exact `Pos` (world-model.md 4.4; here 4.4).

### 4.2 Versions and the three conflict rules

The world model has one edit entry with three conflict rules (file 11, 3.2). The API maps them so:

| Rule | Where it applies | What the API does |
|---|---|---|
| Exclusive holds | Grabs, which are inputs, not edits | Not an API operation in M1; a held object shows `held_by` in query results |
| A precondition on the version read | Every op that changes one object it names, by default | The op carries `if_version`; it is refused as `stale_version` if the object's version changed |
| A plain write | `"if_version": "any"`, and ops with a `where` selector | The latest in the agreed order wins |

- **An object's version** is pe-world's object version: the entry and op index of the last edit that changed any of
  its rows, its parent or its children (world-model.md 7.3). The API's default check is pe-world's `ObjectVersion`
  precondition.
- **Only edit entries change a version.** Simulation (physics poses, logic systems writing fields) does not, or every
  precondition on a rolling ball would fail (agreed, world-model.md 7.3). An agent that read a ball and recolours it
  while it rolls is not refused. Moving an object by hand (a grab) is an input, not an edit: it does not change the
  version either, and while the object is held some edits are refused as `held` (5.3.3).
- **Per object, not per row, in M1.** It is simpler to explain, and the refusal still names the field that changed.
  pe-world also keeps per-row versions and a `Content` precondition (world-model.md 7.3); the token is opaque
  (`"v:…"`), so a per-row default can come later without changing the surface [gnosarium-ux open point 2].
- **The caller's own changes update its table.** When the caller's transaction is accepted, the new versions of
  every object it changed, parents whose children changed included, enter the connection's table, so an agent is
  never refused for a change it made itself.
- **The check uses entry ids, not frames.** A late entry stamped before the reader's frame, which the reader never
  saw, still makes the version differ, so it is caught.
- **The default `if_version`**: when an op names an object and gives no `if_version`, pe-api fills in the version
  this connection last saw for that object (in any query, watch or apply result). If this connection never saw it,
  the version at the transaction's base snapshot is used. `"if_version": "any"` makes a plain write. Agents get the
  safe rule without copying versions about (gnosarium-ux, 3.4).
  - The table of versions seen is kept per connection, for at most 100,000 objects, oldest dropped first [I]. A
    dropped object falls back to the base snapshot's version.
- **Selector ops are plain writes.** A `where` op ("make the balls blue") is expanded into explicit ids on this
  machine and writes them with `if_version: "any"` (gnosarium-ux, 3.4).
- **Explicit versions win.** An explicit `if_version` replaces the default for that object; two different explicit
  versions for one object in one change are refused as `invalid`, naming the later op.
- **A precondition applies to the object an op changes, not to a parent it attaches to** (coordinator, with fable-reviewer). `create`, `reparent` and `duplicate` put no precondition on the parent:
  pe-world's own rules decide, so concurrent creates under one parent both succeed, the cycle check still applies,
  and a create or reparent under a parent deleted earlier in the agreed order is orphaned (world-model.md 7.5).
  Protections apply as world-model.md 7.8 defines them: they guard taking, not touching (coordinator, the user's Resonite-style rule). A non-owner with `edit` may add children under a protected
  object, but not copy it, change its protection, or protect anything inside it. The object being reparented, and a duplicate's source, keep theirs.
  - An orphaned create or reparent says so in its outcome: `orphaned`, with the new root's id and its place.
  - Future, when someone needs it: an opt-in `parent_version` on `create`, `reparent` and `duplicate`.
- **A player's write is a published write** (the user, 2026-11-24; @fable-reviewer's
  file 21 v12, 2.7; design, not built). A write a player makes from the inspector, or through any WorldApi caller
  acting for them, bypasses the shared avenue, so it is published: that player's signed input in the record, sent
  once. The field stays shared, and every machine holds the same value afterwards (world-model.md 3.7). Every
  `apply` is already its member's signed entry, so this changes no operation [I]. What is new:
  - **any permitted writer** is the default: who may write is the world's permissions, and writes that pass meet
    the conflict rules above, a plain write's last in the agreed order winning;
  - **the engine's special feeds** (a player's pose, their voice source) are fixed to their player, and no edit
    writes them;
  - **a field may be restricted to named players.** A write by anyone else is refused, [I] as `not_permitted`
    with the field named. Changing the list is its own permission;
  - **the default stays the version check;** the blind write stays opt-in (`"if_version": "any"`). Where the
    in-frame order of two writes decides something of value, the version check or a grab decides it, never the tie.
- **Unknown fields are refused, not ignored.** An op carrying a field its type doesn't take (say `if_version` on a
  `create`) is refused as `invalid_argument`, so nothing an agent writes is silently dropped. Every JSON surface
  that takes a transaction does this check (in M1, the MCP tools). A query's unknown field is refused the same way
  (`select` for `where` would otherwise read the root's children).

### 4.3 Ids, names and references

- Ids travel as strings: objects `"o:"` and 32 hex digits; entries `"e:"` and 64 hex digits (256-bit hashes, S1);
  transactions `"x:"` and 64 hex digits (the entry id of the transaction's edit entry); members `"m:…"`; a world's line
  `"l:"` and 64 hex digits (world-model.md 2.3: a world is a line, and `LineId` replaces `WorldId`). Hex digits are
  lowercase, with no sign: each id has exactly one spelling, so ids may be compared as text.
- **Paths**: `"/Table/Lamp (left)"` addresses by names from the root; names need not be unique, so a path matching
  more than one object is refused as `ambiguous`, listing the matches.
- **Component type names** (ruling, after @fable-reviewer; `pe-api/src/names.rs`). Two
  scripts can declare the same type name, every version of a script registers its own types, and under copy-on-write
  every forked copy of a script will declare its own `Spin`, so a bare name is resolved where it is used:
  - **Visible types:** the engine's (`Node` left out) and those of each script's active version. A pending, voided
    or replaced version's types are never shown and never resolve. A version's tag (`pe.tag`) is never shown; only
    `attach` and `detach` (section 7) write it.
  - **A call that names an object** (`set`, `add`, `remove`) resolves a bare name among the
    engine's types and those of the scripts attached to that object: those whose active tag it carries, including
    attaches and detaches by earlier ops of the same change. One match resolves. Two are refused as `invalid`,
    listing the qualified forms. An engine name that an attached script also declares counts as two, so a later
    engine type can never silently rebind a saved name. No match where some script declares the name is refused
    with "attach the script first".
  - **A call with no object** (a query's `has`, `schema`): a read takes every match. `schema` with a `type` that
    names more than one is refused, listing the qualified forms. A write always names an object.
  - **A query's projection** reads each selected object by that object's names. Where a bare name is ambiguous on an
    object, it reads every match, keyed by full form (`{"Spin@a:<A>": …, "Spin@a:<B>": …}`), and a field list
    applies to each. Elsewhere the key is the bare name. Never silent, never refused for one object of many (ruling). Every key is the type as the object's `types` names it, however it was asked, so two requests for
    one component answer once, with both field lists.
  - **Qualified forms** are always accepted, whatever is attached: `Spin@a:<hex>` with the script's id or any unique
    prefix of at least 4 hex digits, in either case (as git takes commit prefixes), and `Transform@engine`. A prefix that matches more
    than one script is refused, listing them.
  - **Responses** name a type bare where the bare name resolves to it on that object, and by its full form
    (`Spin@a:` and 64 hex digits, or `Name@engine`) where it doesn't: an object's `types`, a `*` projection's keys,
    a template's `holds`, `watch`'s `set`. `schema`'s list gives each script type's bare name with its `script`. An
    attach or detach shows in `watch` as `pe.tag@a:<64 hex>` and in `history` as "attached script …".
  - **An agent's saved bare name** may become ambiguous later, when a second script declaring the name is attached
    to the object. It then gets the refusal with the qualified forms, never a write to the wrong type.
- **New objects in a transaction** get a local reference, `"as": "$cube"`, which later ops in the same transaction
  may use. The result maps each reference to its id and version.
- **Ids of created objects are known before ordering**: an id is BLAKE3 over (the authoring line's id, author key,
  chain id, chain sequence number, index k) (world-model.md 2.1, r9; @engine-architect's S1), so the author's machine and every other
  machine derive the same one. A `Copy` of an n-object subtree takes n consecutive indices in depth-first order.

### 4.4 Values in JSON

- **Positions** are offsets from a named frame: `{"relative_to": <frame>, "offset": [x, y, z]}`, in metres. A frame
  is `"parent"` (the default for a child's `Transform.position`), an object id or path (that object's space), `"me"`
  (the head of the player the caller acts for), or `"world_origin"`. The world origin is never a default, and a
  result that uses it says so, so a transcript shows when an agent worked in absolute coordinates (gnosarium-ux 4;
  file 17, 7).
  - A write in any frame but `"parent"` is converted to the parent's frame on the author's machine, against the base
    snapshot; the entry carries the parent-relative value. So a position whose object or an ancestor an earlier op of
    the same change re-parents (or gives or takes a `Transform`), whose frame or `relative_to` object an earlier op
    moves (a `"parent"`-relative write excepted: it is stored as given), or whose `relative_to` is a `$ref` made in
    the change, is refused as not yet available rather than converted against a frame that no longer holds; set it in
    a second change [I].
  - A query result gives positions in the frame the query asks for (`"frame"`, default `"parent"`).
- **A root object has no parent** (U4, agreed with @world-model and @gnosarium-ux). Its `Transform` holds an exact
  `Pos` (world-model.md 4.4):
  - by default it is returned as the exact `Pos`, labelled `"pos"`: per axis, the chunk index as an unbounded decimal
    integer in a string (so no JSON reader rounds it), and the offset within the chunk, an `f64` in `[0, 1024)`:
    `{"pos": {"chunk": ["0", "0", "-3"], "offset": [512.25, 1.5, 3.0]}}`. It is lossless anywhere and needs no change
    when the world's bound grows (M4);
  - asked for, it is also given relative to an object, to `"me"`, or to `"world_origin"`;
  - setting it takes an exact `"pos"`, an offset from a named object, or, named on purpose, an offset from
    `"world_origin"`. `"parent"` for a root is refused as `invalid` ("{object} has no parent; name a frame.");
  - the API never takes or returns a position as three plain absolute floats (world-model.md 9.4).
- **A `world_origin` offset** is `pos - Pos::ORIGIN`, an `f64` per axis in metres (the user's deliberate escape). Its precision falls with distance from the origin: about 1e-10 m at 1,000 km, about 1 mm at
  10^13 m (world-model.md 4.2). The schema says so; for precision anywhere, use the exact `pos` or an offset from a
  nearby object. Positions beyond the i64 chunk range are refused in M1-M3 (world-model.md 4.3).
- **Rotations**: `"rotation_deg": [x, y, z]`, Euler angles in degrees in Resonite's order (Unity's `eulerAngles`:
  applied Z, then X, then Y), or `"rotation_quat": [x, y, z, w]`. Both are accepted; the store holds quaternions;
  results give degrees, and the quaternion when asked.
  - They are taken in the world's own frame, which is y-up (gravity (0, -10, 0)), with the active rotation q·v·q*,
    so +90° about +y takes +z to +x, as in Unity [V @vdw-physics: `pe-physics/src/config.rs:37`,
    `box3d-fork/src/math_functions/quat.rs:13-20`]. The API applies no mirror: the same numbers mean the same
    rotation as in Unity's maths.
  - Physics has no handedness; which way +z is drawn is pe-render's and pe-xr's convention. If the world is drawn
    right-handed with -z forward (OpenXR's), then data brought in from Resonite (an import, M5) mirrors z: a position
    (x, y, z) becomes (x, y, -z) and Euler angles (ex, ey, ez) become (-ex, -ey, ez), in the same order. That belongs
    to the importer, not to this API [I].
- **Colours** (rulings): a field pe-sim marks a colour (`FieldDesc::color`:
  linear light, sRGB primaries, straight alpha; `Appearance.base` and `.emissive`, `Light.color`, and any script's
  `LinearRgb`/`LinearRgba`) takes `"#rrggbb"`, `"#rrggbbaa"` (only on a colour with alpha), `{"srgb": [r, g, b(,
  a)]}` in 0-1, or `{"linear": [...]}` with exactly the field's count. A bare list is refused `invalid`, since it
  says no colour space. An `srgb` value outside 0-1 (beyond 1e-6 of rounding) is refused `invalid`: written in 0-255,
  as many tools do, every channel would clamp to white (ruling), so hex is the one 0-255 form, and
  `[1, 0, 0]` means 1.0, 0.0, 0.0. sRGB input is quantised to 8 bits per channel (round(v × 255)) and converted
  through pe-sdk's one 256-entry table (`SRGB8_TO_LINEAR_BITS`), so a script's hex and the API's give the same bits
  on every machine. Alpha is a/255, linear, and an alpha left out is 1. `{"linear": [...]}` passes through exact,
  over 1 included. A read shows lowercase hex only when the stored bits are exactly what that hex converts to, with
  an alpha of ff left out; otherwise it shows `{"linear": [...]}`. So writing a read back never changes a colour.
- **Scale**: a multiplier per axis. Metres are the world's metres, not the player's (the player's own scale is in
  `session_info`).
- **Numbers** are parsed once, on the author's machine, into the field's binary type; the entry carries those bits,
  so every machine applies the same value. Results print floats in their shortest exact form, so a value read and
  written back is unchanged.
- **Schemas carry no non-standard `format` values** (the old server stripped schemars' `uint32`, because Claude
  Code's validator warned on them; gnosarium-ux 3.2).

### 4.5 Who did it: the member and the producer label

- Every edit entry is signed with the member's own key, by the engine (file 06, A3: no agent certificates). The
  harness never holds the key (10.3).
- The entry carries an **engine-written producer label**, set from the connection's token, never by the caller.
  In the payload it is pe-world's `Actor` (world-model.md 7.1, r8 at 7730167): `Person`, or `Agent { harness (at
  most 64 bytes), principal: the author's member key, requested_by: None until M5 }`; a `principal` other than the
  entry's author is refused as `invalid`. `requested_by` is for a guest who prompted an owner's agent (A1, M5). At the
  API it reads `{"kind": "hand" | "agent", "harness"?: "claude-code", "principal"?: "…", "requested_by"?: "m:…"}`,
  where `principal` is the token's local principal name, which stays on this machine and is not in the entry. pe-app
  renders the label in the player's words, for example "Alice's agent (Claude Code)" (gnosarium-ux 6.1).
- An agent acts with all of its member's permissions in the world (coordinator); its token's scopes
  (10.1) may narrow what it can call on this machine.
- The payload also carries the transaction's `label` (at most 200 bytes) for history and the players' view.
- It is an honest client's statement, not a proof: a modified client could forge it (file 06, M9).
- **Agent edits apply at once, labelled and revertible in one step** (coordinator C11). Confirmation before applying
  is a token option, off by default (10.2).

## 5. The operations

The trait is S6, in `pe-seams` (section 11). Every method may be called from many threads at once (`&self`, `Send +
Sync`). Only `watch` with a wait, `apply` with a wait, and `capture` block, each until a deadline the caller gives.

```rust
pub trait WorldApi: Send + Sync {
    fn session(&self, c: &Caller) -> SessionInfo;
    fn schema(&self, c: &Caller, q: &SchemaQuery) -> Result<Schema, ApiError>;
    fn query(&self, c: &Caller, q: &Query) -> Result<QueryResult, ApiError>;
    fn apply(&self, c: &Caller, tx: Transaction, opts: &ApplyOpts) -> Result<Outcome, ApiError>;
    fn watch(&self, c: &Caller, f: &Filter, cursor: Option<&Cursor>, wait: Duration) -> Result<Changes, ApiError>;
    fn history(&self, c: &Caller, q: &HistoryQuery) -> Result<History, ApiError>;
    fn revert(&self, c: &Caller, change: TxId, opts: &ApplyOpts) -> Result<Outcome, ApiError>;
    fn capture(&self, c: &Caller, r: &CaptureRequest, deadline: Instant) -> Result<Capture, ApiError>;
    fn code_check(&self, c: &Caller, r: &CodeCheck) -> Result<Diagnostics, ApiError>;   // U1, section 7
}
```

`Caller` is built by the adapter from the token: principal, member, scopes, producer label, and the connection's
table of versions seen (4.2). `ApiError` is a failure of the call itself (6.3); a refused transaction is an
`Outcome`, not an `ApiError`.

### 5.1 `session`

The caller's principal, member, producer label, the permissions its role has (in the role system's words, so an
agent can tell in advance what will be refused). In M1 roles follow world-model.md 5.4 (coordinator, U3): a private
world's creator is its only admin; friends join as Guest, a role with `EDIT`; the confidants of the world's admins
join as admins. Every admin is a co-owner; there is no world-owner field (10.5). Also: the world's line id and name; every
member of the world with its role, each marked whether it is here, then anyone here whose admission hasn't stepped
yet, as a watcher; the local frame, the newest final frame and the tick rate; the link state in the
player's words (`fine`, `thin`, `some actions dropped`, `catching up`, `watching only`; file 08, A6); the player's
scale; the API's limits (9.3); and `kept`, whether an admin of the world is present (file 18, 2.2; gnosarium-ux
draft 6). The user ruled (file 18, 6-7): a session with no admin keeps nothing unless an admin joins before it ends,
shown with a permanent mark. So with no admin present, `kept: false`, and the players see "Nothing here is being
kept".

While this machine is catching up (a joiner watches first and acts once caught up, file 14 S4), `apply` refuses
with `not_ready`.

**Verdicts on a diverged replica** (coordinator; S5 v1.5):
- `final_frame` is the frame of the replica's agreed cut: the verdict of a change stamped below it is final and
  agrees with every honest replica, and it never goes backwards. A cut at frame `f` covers the changes stamped
  below `f` (S3), so a change stamped at `final_frame` itself is not yet final.
- `session` carries `health`, read from the view, never inferred from two frame numbers: `agreed`, or
  `{"repairing": {"since": frame}}`. `watch`'s `session` items report each change of it as `health_changed`:
  `repairing` before the rest of its view's news, `agreed` after the repair's verdicts.
- `session` lists `scripts_not_running`: each script this machine can't run, with `script` (`"a:…"`), `object`
  (for a bind that fails on one object; left out when the script can't run at all), `since_frame` (the frame it
  stopped running from, or never ran from, here) and `why` (its module isn't held on this machine, or it won't
  bind, and the reason). The field is left out when the list is empty. pe-app also logs each one. An entry leaves
  the list once the script runs here, or once its version stops being active (superseded, unbound, or its objects
  deleted). `health: agreed` only says that the machines agree on the world, not that every script runs here
  (coordinator: a script skipped without a word is what once hid a lamp that never ran on one
  machine; S6 v1.9). For a script that writes only display state, such as a lamp's glow, which no root covers,
  this list is the only sign. One that writes simulated state also makes this machine diverge, and health then
  reads `repairing` within a cut or so.

### 5.2 `query`

Reads one snapshot. `Query`:
- `at`: `"latest"` (default: the newest published frame, which is predicted), `"final"` (the newest final cut), or a
  frame still retained (9.1).
- `where`, a selector, one of:
  - `{"id": "o:…"}` or `{"ids": [...]}`; `{"path": "/Table/*"}` (`*` one level, `**` any depth);
  - `{"name": "Lamp"}` (exact) or `{"name_like": "lamp"}` (case-insensitive substring);
  - `{"has": "Ball"}`, objects with that component;
  - `{"near": {"of": <object or "me">, "within_m": 2}}`;
  - `{"child_of": <object>}` (direct children, in their order) or `{"under": <object>}` (the whole subtree);
  - `{"all": [ ... ]}`, the conjunction.
  Omitted: the root's children.
- `fields`: component names, or `Component.field`. Omitted: id, name, parent, version and the list of component
  types only. Named: those, with the list of types still given, so a reader always sees every component an object
  has (F6 of the M1 redo). `"*"` returns everything, its components' names as keys.
- `frame`: the frame positions are given in (4.4), default `"parent"`.
- `children`: how many levels of children to inline (default 0).
- `limit` (default 50, at most 256) and `cursor`. A result cut by the limit carries `more`, a cursor for the next page.
  Over `child_of`, pages follow the children's order, which serves the inspector's paged hierarchy (inspector.md 7,
  ask 1).
- **A page's size cap** (S6 v1.11; agent tooling of the M1 recordings, where `fields: ["*"]` on the pit gave 312
  and 619 KB, over Claude Code's limit for one tool result, so the harness saved it to a file the agent never read or
  had to parse): a page holds at most 48 KB of objects (`PAGE_BYTES`). One cut by size ends early, carries `more`,
  and says so in `cut`: how many objects it holds, how many more match, and to page on, ask for fewer `fields`, or
  use `summary`. A single object over the cap as asked is `invalid_argument` on `fields`.
- **`summary: true`** (S6 v1.11): the cheap first look at many objects. Instead of `objects`, `summary` gives every
  match's `count`, `types` (how many matches carry each type), `bounds` (the box around their positions, in `frame`,
  default `"world_origin"`) and a page of `objects` as `{id, name}` (`limit` default and most 1000, under the same
  byte cap, with `more`).
- `count_only: true` returns the count and no objects.

The result: `frame`, `final_frame`, `objects` (each with `id`, `name`, `parent`, `version`, `creator`, `held_by` if
held, `protected_by` if protected, and the `components` asked for), and `more`. Every version returned enters the
connection's table (4.2).

Poses and velocities of bodies come from the replica's frame view (S5), not from the store's derived `BodyState`,
which is stale between cuts (world-model.md 3.2). So `query` reads the published snapshot and the frame view of the
same frame together.

### 5.3 `apply`

```json
{ "label": "Put a red cube on the table",
  "mode": "live",
  "wait": "local",
  "base": "latest",
  "ops": [
    {"op": "create", "as": "$cube", "parent": "/Table", "name": "Red cube", "template": "box",
     "components": {"Transform": {"position": {"relative_to": "parent", "offset": [0, 0.8, 0]}},
                    "Appearance": {"base": "#d22b2b"}}},
    {"op": "set", "where": {"has": "Ball"}, "max": 1000, "field": "Appearance.base", "value": "#2b5bd2"},
    {"op": "duplicate", "id": "$cube", "as": "$copy", "parent": "/Table"},
    {"op": "set", "id": "$copy", "field": "Transform.position",
     "value": {"relative_to": "$cube", "offset": [0.3, 0, 0]}},
    {"op": "delete", "id": "o:…", "if_version": "v:…"}
  ] }
```

**Operations in M1:**

| Op | Does | Notes |
|---|---|---|
| `create` | A new object under `parent`, optionally from a `template` (5.3.1), with `components` | Gets an id at once (4.3) |
| `set` | One field (`field`, `value`) or several (`fields: {...}`) | `value: null` resets to the default |
| `add` / `remove` | Adds or removes a component (`component`, with initial `fields` for `add`) | `add` on a component the object already has is refused unless `replace: true` (below) |
| `reparent` | A new `parent`, and an `index` among its children | `keep`: `"world_pose"` (default) or `"local_pose"`; refused as `invalid` if it would make a cycle |
| `duplicate` | A copy-on-write copy of a subtree, new ids over the same records ("b, 1 always", R11 rulings; file 11, 3.2). Bulk (S6 v1.11): `where` (with `max`) names many sources, `count` makes that many copies of each (default 1, at most 1000), and `step` (metres, in the source's parent's frame) places copy k at the source plus k steps | `protected` for a non-owner (V1). `as` names a single copy only. Without `step`, a copy sits where its source does. Past the world's limits (its shapes, 520 in M1) the whole change is refused as `too_large` |
| `delete` | An object and its subtree | Plain Delete unlinks a linked script (R11 rulings) |
| `code` | The logic edit of U1 (section 7) | |

How they map onto pe-world's ops (world-model.md 7.1): `create` is `Create`, or with a `template` a `Copy` of the
template object (templates are objects under a `Templates` root) followed by its `set`s; `set` is `SetField` per
field; `add` is `SetRow` (with a row `Version` precondition unless `replace`); `remove` is `RemoveRow`; `reparent` is `Reparent` with `keep_world` from `keep`;
`duplicate` is `Copy`; `delete` is `Delete`; `code` is the `Register` and `CommitScript` pe-logic builds (section
7). Default and explicit `if_version`s become `ObjectVersion` preconditions in the payload's precondition list;
`$ref`s become `New(k)` targets.

- **`add` never replaces by surprise** (S6 v1.10; F6 of the M1 redo, coordinator after
  @astra-deliberator and @fable-reviewer). `add` writes a whole row: the fields given, every other field zero.
  - On a component the object already has, it is refused as `invalid`, before anything is built. "Already has"
    means the row as the op would find it: a preset's or template's, one a copy brought, or one an earlier op of
    the same change added or removed.
  - The refusal names the component (`field`) and every object of the op that has it (`details.objects`, each with
    its `current` values), and its `next` says to use `set`, or to repeat the `add` with `replace: true`. With a
    `where`, one such object refuses the whole change.
  - A plain `add` on an existing object carries a row precondition: the row's version as this change read it, the
    zero version if the row was absent (world-model.md 7.6). It holds whatever `if_version` says and survives a
    re-send (9.4), so of two plain adds racing, the second is refused (`stale_version`, "Someone added a … component
    to this object since you read it") rather than overwriting the first. After a `remove` in the same change, the
    guard is the removed row's version, so a stale remove can't turn the `add` into an overwrite.
  - `replace: true` replaces the component if it is there (adds it if not): the fields not given become zero. The
    outcome's `diff.set` lists each field it changes, `before` and `after` (other edits' field diffs: not yet),
    and an undo restores the old row.
  - `attach` is unchanged.
- **Blob fields and names** (S6 v1.11; the M1 recordings' `create`s copied a query's output, `"material": null` and
  `"Name": {"text": …}` included, and were refused as "not wired"):
  - a blob field (a mesh, a material) takes `null`, for none, its default, so a query's output can be given back as
    it is. Any other value is refused as `invalid`: the API can't set an asset's content yet;
  - `Name.text` takes the name as a string: `set` renames (its bytes carried inline, at most 255 bytes of UTF-8,
    5.3), and gives a Name to an object without one; a `create`'s `components` may carry `Name` only with the same
    text as its `name`. A name, given either way, is never empty and has no `/`, since paths are made of names (5.1).
- **Targets**: `id` takes an id, a path or a `$ref`. Instead of `id`, `set`, `add`, `remove`, `duplicate` and
  `delete` take a `where` selector with a required `max`. The selector is resolved against the base snapshot on this
  machine into explicit ids; the entry carries the ids, never the selector, so every machine applies the same change. More
  matches than `max` is refused as `too_many`. "Make the balls blue" is one op that becomes 500 writes in one entry;
  the expanded transaction must stay within pe-world's 4,096 ops (9.3).
- **All or nothing.** Ops apply in order; if any is refused, none applies, and the outcome names the first refused
  op by index.
- **Preconditions**: 4.2. An op on a `$ref` created in the same transaction has none.
- **`base`**: the snapshot selectors, paths and frames are resolved against, and the fallback for versions: `"latest"`
  (default) or a retained frame.

`ApplyOpts`:
- `mode`: `live` (default) or `dry_run`; in M2, `in_fork`.
- `wait`: `"local"` (default), which returns once this machine has applied it or refused it; `"final"`, which returns
  once it is final, refused or dropped; `"none"`, which returns at once with `queued`. `timeout_ms` bounds the wait
  (default 2,000, at most 10,000); past it the result is the state reached so far.

#### 5.3.1 Templates

A world carries templates as data (the ball-pit template holds a floor, walls, balls, a table and lamps; plan M1 step
1). `create` with `template` copies one, then applies `components` over it. `schema` lists the world's templates and
what each holds, so an agent can make "a box" without knowing every component a visible, physical box needs
[agree: @world-model].

#### 5.3.2 The outcome, and the life of a change

An `Outcome`: `status`, `change` (the transaction id), `frame` (its stamp, once it has one), `final_frame`, `dry_run`
(first, when true), `created` (each `$ref` with its id and version), `changed` (ids with their new versions), `count`
(objects written or created; a copy or a template counts every object it makes), `orphaned` (creates and
reparents whose parent was gone, 4.2), `resent_as` (the entry that carried it, when the re-send backstop re-sent it,
9.4), and, when refused or dropped, the refusal (5.3.3). Versions in it enter the connection's table.

| `status` | Meaning |
|---|---|
| `queued` | Accepted by the API, not yet stepped (`wait: "none"`, or the wait ran out) |
| `accepted` | Applied on this machine at frame F, its stamp. Others see it within about one network delay. Not final |
| `final` | Applied by the ordered step at F, a final cut covering F includes it without voiding it (never the frame alone), and `final_frame` (5.1) is past F, so this machine's verdict is the quorum's. It can no longer be undone except by a new change |
| `refused` | Refused, with a reason. `final: false` while this machine's check refused it before ordering or the refusal is still predicted; `final: true` once a final cut includes it and `final_frame` is past F |
| `dropped` | Voided by a final cut (late, or back-stamped; S3 v1.1), or recorded by the replica as left out of the final order, so no replica applies it: its local effect is undone everywhere. Whichever says it first is told; the other adds nothing. A change stamped below a final cut's frame that the cut does not include is still pending, not dropped, until a later cut includes or voids it. A change of this machine's API voided as late is not dropped but re-sent (9.4): it is `dropped` only if no re-send of it applied within the one-minute bound, and is then held for its author to apply or discard (`held`) |
| `unknown` | In the final order, but stepped here on a state a repair replaced (5.1): whether it happened is not known here. `repaired_at` names the cut's frame; re-read the state by query. Final: it never changes again |

- An `accepted` change can still become `refused` when an earlier conflicting change from another machine arrives
  and the replica re-steps (file 14, P4). That is the stale edit seen from the loser.
- Every change of status after `apply` returns is reported through `watch` (`mine`, 5.4), so no refusal is silent.
- **A re-sent change keeps its id.** The caller's `change` stays the one `apply` returned; `resent_as` names the
  entry now carrying it, and `frame`, `created` and `changed` are that entry's. Its created objects have new ids
  (ids derive from the entry, 4.3), which `created` gives under the caller's `$names` (9.4).
- **A `code` change reports twice** (7): once when its entry is final, as any change, and once more when its
  version activates or is voided (`activated`/`voided` in `mine`, 7). 9.4's "one notice" covers only the re-send
  backstop, which never takes code.

#### 5.3.3 Refusals

One envelope for every refusal, in `apply`'s outcome, in `watch`, and in a dry run:

```json
{ "status": "refused", "final": false, "change": "x:…",
  "code": "stale_version",
  "message": "Bob moved \"Lamp (left)\" after you read it.",
  "op": 4,
  "object": {"id": "o:…", "name": "Lamp (left)"},
  "field": "Transform.position",
  "read": "v:…", "now": "v:…",
  "winner": {"change": "x:…", "frame": 18347, "by": "m:… (Bob)",
             "producer": {"kind": "hand"}, "label": "Moved Lamp (left)", "value": {"relative_to": "parent", "offset": [1.2, 0, 0.4]}},
  "next": "Read it again with world_query and decide whether your change still applies." }
```

| `code` | When | Details |
|---|---|---|
| `stale_version` | The object's version is not the one named; or a `duplicate`'s source subtree changed size since the base (pe-world's own size check, `OWN_CHECK`) | `read`, `now`, `field`, `winner` with the winning value. A copy's size check names no winner: the message says the subtree copied changed, so read it again |
| `not_found` | An id, path or `$ref` names nothing | The deleting change as `winner`, when known |
| `ambiguous` | A path matches several objects | `matches` (at most 10) |
| `invalid` | A value of the wrong type or out of range; an unknown component or field; a read-only field; a cycle; a `duplicate` of a subtree an earlier op of the same change adds to, moves or deletes from; a push past the world's speed-change cap | `field`, `rule` (from the schema), `given`; for a push, `limit_mm_per_s` |
| `in_change` | An op names a `$ref` this transaction does not create | `ref` |
| `too_many` | A selector matched more than `max` | `count`, `max` |
| `not_permitted` | The world's permissions; a grab past the most objects one member may hold (`max_claims`), or out of reach | `permission`, `role`; for a grab, `limit` |
| `protected` | V1: a protected object, for anyone but its owner (10.4); a script change whose re-key would lose data on objects its author may not edit (world-model.md 7.9) | `owner`, `action`; for a re-key, `objects` (the first 16) and `more` |
| `held` | A held object (or an object under it) may not be deleted, nor its `Transform`, `Body`, `Shape` or parent edited, while a player holds it; other edits, such as a colour, pass (world-model.md 7.2) | `held_by` |
| `too_large` | Past the size limits (9.3), or past the world's shape limit | `limit`; for the shape limit also `count`, the shapes the change would leave |
| `rate_limited` | Past the principal's rate (9.3) | `retry_after_ms` |
| `not_ready` | This machine is still catching up; or its frame clock is catching up with its own last stamp (agreement's `BelowOwnStamp`: retry in a moment) | — |
| `compile` | A `code` op whose source does not compile on the author's machine. Only at submission: no entry is made (section 7) | `diagnostics` |
| `dropped` | The change arrived too late and was undone (status `dropped`); for this machine's own changes, only once re-sending it for a minute failed, and then held (9.4) | `resent_as`, when it was re-sent; `held`, the batch to apply or discard |

- `message` is one sentence, engine-written, safe to show a player; peer text in it is quoted (rule 6). Its wording
  is @gnosarium-ux's (api-surface.md 3.5). `next` is for agents; a person's interface shows buttons instead
  (Re-apply, Show).
- `refused` is a property of the transaction, not of the call, but pe-mcp returns a refused `apply` with
  `isError: true` (6.3), so an agent cannot miss it.
- **Every code is tested** (G8 of the pull-ahead plan): each pe-world refusal reason maps through an arm of its own,
  never the wildcard (pe-api `every_world_refusal_reason_has_its_own_mapping`), and every refusal and error code
  reaches an MCP client as `isError: true` with this envelope (pe-mcp `tests/refusals.rs`, whose exhaustive lists
  fail to build when S6 gains a code).

#### 5.3.4 Dry runs

`dry_run` runs the same resolution, validation and pure edit function on a copy of the base snapshot, off the
simulation thread, and returns the outcome it would have had there (`status: "accepted"` or the refusal, with
`dry_run: true` first) and the diff: objects created and deleted, and for each written field its value before and
after. Created objects show as their `$ref`s only, since a dry run takes no sequence number. It changes nothing and
sends nothing. It does not step physics or logic: what a new body then does is a fork's question (M2) [I].

### 5.4 `watch`

A change feed with a cursor. `Filter`:
- `where`: a selector, as in a query, narrowing everything below to those objects. It is matched once per call
  against the newest snapshot, so an object deleted before the call is named by its id [I]. With ids, a filter costs a lookup
  per change, so an inspector may keep one open per selected object (inspector.md 7, ask 2).
- `changes`: edits by anyone: `{change, by, producer, label, frame, final, created: [...], deleted: [...],
  reparented: [...], set: [{id, field, value?}]}`. Values are included only for fields `fields` names, or 500
  recoloured balls would flood the agent.
- `mine`: status changes of this caller's own transactions, with the refusal envelope (always on). A change the
  re-send backstop takes up (9.4) reports nothing more until its final outcome, which comes as one item, with
  `resent_as`. After a repair
  (5.1), each affected change of the caller's resolves once, explicitly, as `unknown` with `repaired_at` (the cut
  it was repaired to), never left pending: whether it happened is not known, so the agent re-reads the state by
  query. A client matching on status must handle `unknown`.
- `session`: members joining and leaving, this machine catching up, the newest final frame when it moves, and the
  replica's health when it changes (`health_changed`, 5.1).
- `motion`: off by default. **Simulation is not a change**: bodies moving and logic-written fields do not appear in
  `changes`. An agent that wants motion queries poses. `motion: {"every_ms": 250}` with a `where` of at most 16 ids
  adds sampled poses of those objects [I].
- `final_only: true` holds changes back until final, at the cost of about one cut's delay.
- **In M1** finality is decided only for this machine's own changes (`mine`), from the final cuts (5.3.2), so a
  `changes` item's `final` is always false, and `final_only` and `motion` are refused as `invalid_argument` (not yet
  available). A per-entry decision for everyone's changes needs each stepped entry's chain and seq (later).

Behaviour:
- **Cursor**: an opaque position in this process's change stream. Without one, `watch` returns a cursor for "now"
  and nothing else. With `wait`, it long-polls until something matches or the wait ends (at most 30 s). It waits on
  the caller's thread, never the simulation's.
- **Retention**: the stream keeps a bounded window (9.1). A cursor older than it is refused as `cursor_expired`, with a
  fresh cursor; the caller re-reads by query.
- **Rollback**: when a re-step changes what a frame did, the feed reports it as a new item for the same change (its
  status moves in `mine`; an edit that no longer happened is `retracted`). It never rewrites items already delivered.
- Every version a watch result carries enters the connection's table (4.2).

### 5.5 `history` and `revert`

- `history` lists changes, newest first: `{change, by, producer, label, frame, final, status, summary}`. The `summary`
  is engine-written ("Created Red cube under Table"; capped). Filters: objects (and optionally their subtrees), member,
  producer kind, a frame range, the caller's own. Paged by cursor.
- **Revert** builds the inverse of one change and applies it as a new change, labelled "Revert: <label>", with its own
  entry and attribution: a delete for each create; an `Undelete` of each deleted subtree from its tombstones, within
  the tombstone window (world-model.md 7.5); the prior rows for each set, add and remove; the old parent and index
  for each reparent.
  - The inverse comes from the ordered step: `Outcome::Applied` carries `inverse: Vec<Op>` (world-model.md 7.7;
    accepted for S2 by @engine-architect). pe-api keeps the inverses with the history. A delete older than the
    tombstone window cannot be reverted, and `revert` says so (`invalid`, with the window).
  - Its preconditions are the versions the reverted change left. If anyone changed those objects since, the revert is
    refused as `stale_version`, naming that change, so it never silently overwrites later work. `"if_version":
    "any"` in `ApplyOpts` reverts anyway.
  - Reverting another member's change needs the permissions its ops need; there is no separate revert right in M1.
  - In M1, history covers the session's lifetime on this machine, within a memory cap (9.1). From M3 the world record
    keeps every entry since the world's creation (the user, 19:09Z), and history reads it.
- `revert` takes `apply`'s options, so a revert can be previewed with `dry_run`.

### 5.6 `schema`

- Without a type: the list of visible component types `{name, summary}`, a script's with its `script` (`"a:…"`;
  4.3; S6 v1.8, `Brief.script`), the world's templates `{name, summary}`, and the schema `version`. Small on purpose.
- `type` takes a name as a read with no object does (4.3). The answer names a script's type by its full form.
- With a type: a JSON Schema per field, with `x-` annotations standard validators ignore (gnosarium-ux 3.2):
  `x-unit` (the registry's unit in its canonical spelling, world-model.md 3.6: `m`, `kg/m^3`, `1/s`, `rad`, …; absent
  for a field without one); `x-sync` (`synced`, or `unsynced` when M2 brings per-player fields; the
  inspector marks unsynchronized fields with a shape, Q12); `x-protected` (part of a protection); `x-read-only` (the
  engine writes it; an edit is refused as `invalid`); `x-color` (`linear-srgb`, on a colour field, whose schema is
  a `oneOf` of the forms 4.4 takes).
- With a template: what it holds.
- `version` is a hash of the registry, of each script's active version, and of the template list (so `since`
  notices a template added, removed or renamed, and a version's types appearing at its activation, not at its
  commit, which registers them), which is versioned by frame (plan 2.1); a logic edit can add a component type
  (section 7). `schema` with `since: <version>` returns `unchanged` when it matches. A schema change also appears in
  `watch` (`session`).

## 6. The MCP adapter (pe-mcp)

### 6.1 What it keeps from the old crate

From `crates/pe-mcp` at 757219f, about 930 lines (file 06; [V] by reading `src/`):
- the transports: stdio (an embedded bench); a named pipe on Windows with a protected DACL, a random name and a
  first-instance flag; a Unix socket in a `0700` directory with a peer-uid check; loopback HTTP with exact `Host`
  checks, `Origin` refusal, no CORS and a bearer token on every request (`pipe.rs`, `http.rs`, `host.rs`, `main.rs`);
- the tokens: 256 random bits, constant-time comparison, idle expiry, revocation when the harness exits, never logged
  (`auth.rs`);
- the error envelope: a tool error is a tool result with `isError: true` and `{code, message, field?, details?}` as
  its structured content and as its text (`model.rs`);
- the launch model: `pe-mcp serve -- CMD` issues a fresh token, starts the harness with the token in its environment
  only, and revokes every token when it exits; `pe-mcp bridge` relays stdio to the pipe. The harness inherits
  standard output and error. It inherits standard input only under `pe-mcp serve`, where it speaks MCP over that
  stdio. pe-app's `--agent` gets none, because pe-app reads its own console there (B's reply code, `quit`). On
  Windows, a child that shared a pipe pe-app was reading could hang at start-up (the self-test agent did, in about
  half its runs).

Dropped: the backends, the engine thread, the simulator view, the test-bench tools (`world_step`, `scenario_*`),
the `interact` scope, and the server text that called a live peer a simulator (file 06, M6). pe-mcp runs no engine
loop.

The serve side moves into pe-app: pe-app owns the world and the token store, and starts pe-mcp's transports as a
library. `pe-mcp bridge` stays a small binary [I; agree: @engine-architect, WS-app].

### 6.2 The tools

Tool names use underscores: the Anthropic API accepts only `[a-zA-Z0-9_-]` in tool names [V: the old README,
"Tools"]. The plan's `world.query` is `world_query` on the wire.

| Tool | WorldApi | Scope | Beyond a direct call |
|---|---|---|---|
| `session_info` | `session` | read | — |
| `world_schema` | `schema` | read | — |
| `world_query` | `query` | read | — |
| `world_apply` | `apply` | edit | A refused outcome is returned with `isError: true` (6.3) |
| `world_watch` | `watch` | read | `wait_ms` at most 30,000 |
| `history_list` | `history` | read | — |
| `history_revert` | `revert` | edit | As `world_apply` |
| `world_held` | `held` | edit | `{held, action}` (9.4); answers `{outcomes}` |
| `view_capture` | `capture` | read | An MCP image content block (PNG) plus a structured part (section 8) |
| `code_check` | `code_check` | edit | Section 7 |

- **The arguments and results are WorldApi's types**, serialized with serde; their JSON schemas are generated by
  schemars, without non-standard `format` values. A test keeps a committed `schema/tools.json` current, as in the old
  crate.
- **The tool list is small on purpose**: nine tools against the old design's seventy (file 06, M8). The whole list
  (names, descriptions, argument schemas) stays **under 5,000 tokens**, measured with Claude Code's tokenizer and
  recorded here; a test fails if the serialized list grows past the budget's byte proxy (gnosarium-ux 3.9).
- **The server's `instructions`** are gnosarium-ux's text (api-surface.md 2): a live, shared world; time runs on its
  own; read before you write; changes apply all or nothing and name the version read; look with `view_capture`;
  positions are relative to something; changes are signed and labelled and anyone can revert them. pe-mcp fills in
  the member and producer at connection.
- **Descriptions** are gnosarium-ux's (api-surface.md 3), two or three sentences each.
- `tools/list` shows only what the caller's scopes allow; a call outside them returns `forbidden` with `details.need`
  (kept).
- **No resources and no subscriptions in M1.** `world_watch` is the change feed [I: a long poll works in every
  client; resource subscriptions may come later].

### 6.3 Errors

- **A failed call** (the arguments or the call itself) uses the envelope of 6.1, with `isError: true`. Codes:
  `invalid_argument` (with `field`), `forbidden`, `cursor_expired`, `snapshot_gone` (a frame no longer retained),
  `capture_unavailable` (no renderer or no adapter), `rate_limited`, `timeout`, `session_ended`, `unavailable` (a
  service this call needs isn't here or can't serve now: no build service, a player joining, 7), `internal`. Unknown
  tools and malformed requests are JSON-RPC errors (kept).
- **A refused transaction** (5.3.3) is also returned with `isError: true`, its envelope as the structured content:
  `code` and `message` at the top, the rest in `details`. Clients then show it as a failure, which it is for the
  agent; pe-api's types still keep the two apart.
- No tool argument may panic the server: a panic in a handler is caught and returned as `internal`, and the world is
  untouched, because handlers hold only a snapshot.

## 7. The logic edit in M1 (U1)

The user added a small logic edit to M1; the coordinator brought Cranelift into M1 for it (19:52Z).
WS-logic (@iter-bench) owns what a script is, how it compiles and how it runs (`docs/spec/logic.md`, branch
`ws/logic`, 75ac956, sections 4 and 6). This section gives the API's slots, as agreed with @iter-bench.

- **`code_check`**: compiles a script's source to wasm through pe-logic and validates it. It returns rustc's JSON
  diagnostics (file, line, column, severity, message, code) plus pe-logic's own refusals as kinds (`import`,
  `feature`, `manifest`, `reserved_name`); on success also the module hash, the declared systems and components, and
  the build time. It changes nothing. Builds run one at a time per machine at idle priority, so a check may queue
  behind a commit's build; it runs on a worker through the build port (7.1), never the simulation thread, and the call's
  deadline bounds it.
- **A script is an asset, not an object.** It is bound to objects through a `Script` component.
- **The `code` op** of `apply`: `{"op": "code", "id": <script asset id, or a $ref for a new script>, "source":
  "<text>" | "b:<hash>", "attach": [objects to bind]}`. In M1 the text is one file, `lib.rs`, depending only on
  pe-sdk; a blob tree (`"b:…"`) for multi-file crates comes later, and the schema keeps both forms. Its
  precondition is the script's current version (the hash of its source tree): two agents committing on one base get
  one winner and one `stale_version`.
  - **A script's id** is an asset id, written `"a:<64 hex>"`, apart from object ids (ruling, after
    @fable-reviewer). A new script's id is never chosen by the client: its `$name` maps to `New(k)`
    as for objects, and pe-world derives the id from the creating entry (line, author, chain, seq, k) under its own
    domain tag, so an asset id never equals an object id and nobody can squat on a pending id or reuse a destroyed
    one (@world-model's). The outcome returns it in `scripts` (`$name` to `{ id, version }`, S6 v1.7), beside
    `created`, which stays objects only. pe-api finds the id after the entry applies as the asset whose pending
    commit is that entry's. The backstop never re-sends a code entry in M1 (9.4, `resend::is_code`); if a later
    milestone does, the re-sent entry derives a new id and its dependents are rewritten by k, as 9.4 rewrites
    created objects.
  - **`attach` binds only an active version** (ruling, after @fable-reviewer).
    Attaching adds the script's tag row, and pe-world takes a tag only of an asset's current, active version
    (world-model.md 7.9). On a script with an active version, `code`'s `attach` adds that version's tag in the same
    entry, and the activation's re-key moves it to the new version; if the commit is voided, the objects keep the old
    version. On a script with no active version, `attach` is refused: a pending first version as `not_ready`
    ("attach after it activates"), a voided first version as `invalid` ("commit again"; there is nothing to wait
    for). The agent attaches after its `mine` item says `activated`.
  - **`attach` and `detach` ops** (S6 v1.7): `{"op": "attach", "script": "a:…", "to": [objects], "mode":
    "linked"}` adds the active version's tag to each object (`SetRow`, empty row), and `{"op": "detach", "script",
    "from": [objects]}` removes it (`RemoveRow`); detaching leaves the script's other rows on the object. Each needs
    the object's edit right, checked by pe-api before submission and by pe-world at apply. `mode` is required and M1
    accepts only `"linked"`: every tagged object follows the script's later commits. Copy-on-write local logic
    becomes the default later, as `"local"` (the user's asset ruling), and requiring the field now keeps today's
    calls meaning the same then. A commit that names its first holder, with pe-world attaching at activation, is
    the logic-on-objects item's, not M1's.
  - **Editors show the two steps.** A person's editor commits, shows "waiting for the script to activate", and
    offers the attach once the version is active; it queues nothing silently on the person's behalf.
- **Compiling is checked once, at submission.** The author's machine compiles and validates before it submits; a
  failure refuses the `code` op as `compile`, with the diagnostics (a margin, 7.1, is named as such), and no entry
  is made. Once the entry exists, its
  outcome is the same everywhere: `accepted`, then `final` at the cut, or `stale_version` by its precondition. An
  outcome is world state, so it cannot depend on one machine's compiler.
- **Activation is by attestation** (logic.md 6.2a): every voting machine builds the same source itself, no machine
  runs another's build (Q3c), and the version activates at the frame its quorum of attestations reaches, or the
  commit is voided (`Unattested`, `QuorumUnreachable`). The commit counts as its author's attestation.
  `CommitScript.activate_at` is the entry's own stamp: pe-api leaves it at 0, "set by the submitting machine's step
  2", and step 2 sets it in every transaction it drains (@engine-architect). A dry run sets it to the
  frame it applies at. A version's activation or void reaches pe-api through S5 v1.8's `FrameView.activations`
  (@engine-architect, @item:primordial-engine/s5-v1-8-frameview-activations-the-code-commit-re): per (asset,
  version), `Activated { rekeyed }` or `Voided { why, objects, more, failed }`, with `activations_retracted` for
  records a re-simulation no longer produces. It is world state, the same on every machine, and the source of
  `history`'s entry and of Q1 (c)'s "who couldn't build it and why" (`failed`, by `BuildFailure::name()`).
- **The author is told, in M1** (rulings; @design-reviewer): from
  `FrameView.activations` at final frames only, one more `mine` item to the commit's principal under the commit's
  `change` id: `activated` (`frame`, the objects re-keyed) or `voided` (`kind`: `unattested`,
  `quorum_unreachable` or the world's other kinds; `frame`; `failed`: each member who couldn't build it, with the
  reason's name, never compiler output). `history` shows it as the change's status. Without it an agent would see
  `final` for a version that never ran.
- Component types a script declares appear in `schema` once it is active, with a new schema version, each with its
  `script`. Agents name them as 4.3 says: bare among the scripts attached to the object, else `Spin@a:<hex>`.
- `code.test` (a fork with random rollbacks) and `fork.*` are M2.

### 7.1 The build port and the machine lane

Agreed with @vdw-physics and @engine-architect. pe-api doesn't depend on pe-logic, so
it never links the compiler or the runtime; pe-app gives it a handle to the machine's one build service.

```rust
pub trait Builds: Send + Sync {
    /// Build and validate a source archive (logic.md 4.1a) as its author's machine: blocking, on an API worker
    /// thread, never the simulation thread. `deadline` bounds the wait in the queue.
    fn build(&self, archive: &[u8], deadline: Instant) -> Result<Built, BuildRefused>;
}
pub struct Built {
    pub module: Hash,          // the metered bytes' hash, Script::module_hash(): exactly CommitScript.author_module
    pub toolchain: Hash,
    pub sdk: Hash,             // as CommitScript carries them
    pub components: Vec<ComponentDesc>, // as declared, under V's ids
    pub systems: Vec<String>,
    pub warnings: Vec<Diagnostic>,
    pub build_ms: u32,
    pub peak_bytes: Option<u64>, // None until pe-logic measures a build's peak (6.2a item 5)
}
pub enum BuildRefused {
    Failed(Vec<Diagnostic>),                 // rustc's, or pe-logic's kinds
    Margin { which: Margin, measured: u64 }, // the author's margins: 15 s (Q4 (b)), 512 MiB (M1's cap)
    Timeout,                                 // the deadline passed in the queue
    Unavailable(String),
}
```

- **Given at `Api::new`** as `Option<Arc<dyn Builds>>`; with none, `code_check` and the `code` op answer
  `unavailable`. pe-app starts the build service before it creates the ports.
- **While a player joins** (app.md 4, "A world with scripts: an M1 limit"): from the joiner's ready until the new
  epoch's first final cut, or the join's end if it fails, a live script commit (a `code` op's, or a revert's) is
  `unavailable` ("a player is joining") before anything is built or submitted. pe-app says so in
  `SessionFacts::joining`, and its drain settles a script commit queued before the window as
  `Receipt::Unavailable`, which the caller gets as `unavailable` too. `code_check` and dry runs still answer, and
  other changes go ahead. The re-send
  backstop never re-sends a script commit (9.4), so none reaches the window that way (@world-model).
- **One build at a time per machine**, on the build service's thread: the replica's builds of others' pending
  commits first (they have a deadline D), then API builds, first in, first out within each. A queued API build
  whose deadline has passed leaves the queue unbuilt; one that has started finishes into the cache, so a retry
  answers at once.
- **One cache, by V** (the archive's hash; enough because the machine's toolchain and SDK are fixed for the
  process's life, and the commit carries both hashes for the attesters' comparison), shared by `code_check`, the `code` op and the build thread, so a check
  and then a commit of the same source build once. A committed V stays until its commit activates or is voided;
  checks keep the last 16 [I]. The build thread loads a module into the runner for a pending commit, and for an
  active version whose module it hasn't loaded, the author's included, so a check that is never committed costs the
  runner nothing. An active version is loaded without an attestation, since it needs none. The author never sees
  its own commit pending: its prediction (logic.md 6.2a, option (i)) activates the commit at the end of the
  commit's frame, before any snapshot the build thread reads, so the author's module is loaded as an active version
  (@vdw-physics). Until @item:primordial-engine/m1-blocker-binding-an-activated-script-to-an-obj the thread read
  pending commits only, and the author's runner never held its own module. A script that still can't run here is
  listed in `session` (5.1).
- **Margins** are applied by the service when building as author (every `Builds::build` is one; the replica's
  attesting builds don't come through this port). A margin refusal names the margin, never `compile`. `build_ms`
  and `peak_bytes` measure the build itself at the pinned job count (logic.md 6.2b), never the time in the queue:
  an author queued behind others' attesting builds is not refused at 15 s. The service times the build itself today, so the 15 s
  margin applies now; the 512 MiB margin applies once pe-logic reports the peak of the build's process tree (a job
  object on Windows, `RUSAGE_CHILDREN` on Unix; @iter-bench's), and until then `peak_bytes` is `None` and no memory
  margin is applied (@vdw-physics).
- **Diagnostics** are rustc's JSON messages with paths remapped to `/script/...`, once pe-logic returns them
  (@iter-bench: `BuildError::Rustc { text, diagnostics }`). Until then a failure is one `rustc`
  diagnostic holding the text.
- **The machine lane**: `EditQueue::submit_machine(tx)` for the build worker's `AttestBuild` and `BuildFailed`
  (pe-app's build worker calls it; pe-replica never submits). No principal, no bucket, no `mine`, never re-sent
  (a voided attestation is the build worker's to post again while D allows); `history` shows it as the machine's.
  The re-post is state-based, so it needs no void plumbing and covers a void from any cause: once the final frame
  passes its attestation's stamp, if the pending commit's `attested` in the final snapshot lacks this member and D
  is still ahead, the worker posts again (@design-reviewer; at n = 2 one late-voided attestation would
  otherwise leave a good commit `unattested` at D).
  Step 2 drains it ahead of the backstop's re-sends and fresh changes: an attestation has a deadline, and nothing an
  API change depends on is a machine entry, so moving them up breaks no order.

## 8. `view_capture`

An image of the world, so an agent can see its work (file 06, M8; plan M1 step 5).

`CaptureRequest` (gnosarium-ux, 3.8):
- `from`, one of:
  - `{"player": "me"}` (default) or `{"player": <member>}`: that player's current head view, drawn on this machine
    from the shared world. It shows only what this machine draws, so nothing private (Q13 a: an object visible only
    to its holder does not appear). Allowed for any member (gnosarium-ux open point 7; "openness over privacy").
  - `{"orbit": <object>, "yaw_deg", "pitch_deg", "distance_m"}`: a camera looking at an object. Without
    `distance_m`, the object's bounds frame it.
  - `{"at": <position>, "look_at": <object or position>, "fov_deg"?}`: a camera placed exactly.
- `size`: `[w, h]`, default `[768, 432]`, at most `[1920, 1080]`.
- `highlight`: ids to outline; `label: true` writes each highlighted object's name beside it.
- `visible_list: true`: the objects drawn, `[{id, name, rect: [x, y, w, h]}]`, largest first, at most 50, so an agent
  can tie what it sees to ids it can edit.
- `at`: as in a query. The image is drawn from that snapshot's **simulated** poses, not the display layer's blended
  ones, so it shows what the world holds.

`Capture`: the PNG, `frame`, the camera used (`at`, `look_at`, `fov_deg`), and `visible` when asked.

Rules, agreed with @ballpit-viz (WS-render writes them into pe-render's spec):
- A capture never costs the headset or the window a frame. It runs on the render thread's own device, into its own
  offscreen target, slotted between frames; a second device would duplicate every mesh and texture. Only the readback
  is deferred: the map completes on a later frame, so a capture never stalls a present. If the headset is at its
  frame budget, the capture waits a frame. The caller's deadline bounds the wait (`timeout` past it).
- Measured in WS-render's M0 check (m0/feasibility-xr, cf7a3bb): about 40 lit bodies at 1280x720, 4.5-5.2 ms with
  readback on an RTX 3080 Ti, with no window and no desktop; two draws give identical pixels [V @ballpit-viz].
- In M1, `visible_list` comes from projected bounding boxes without occlusion, and the result says so
  (`visible_approx: true`). An id pass (an R32Uint target with the same draw) can follow when the UI lab needs it.
- Headless tests render with a software adapter (WARP on Windows, lavapipe on Linux; plan 4.6). With no adapter at
  all, `capture_unavailable`.
- At most one capture in flight per principal; another gets `rate_limited`.

**The capture port in M1** (@coordinator-opus):
- **pe-api** (`capture.rs`) resolves the request in the newest snapshot: object references (paths, ids) to ids,
  positions (4.4's forms, from `world_origin` or an object; a camera has no `parent`) to world positions, and the
  highlighted objects' names for `label`. It keeps the rules: `at` other than `latest` is `invalid_argument` (M1 draws
  the newest frame only; views kept by frame wait until someone needs them), the size bound, the read scope, one in
  flight per principal. It calls the port `Captures::capture(Order, deadline)` and answers with the camera as
  `{"relative_to": "world_origin", "offset"}` and `visible` largest first, at most 50, named, with
  `visible_approx: true`.
- **Aims:** `player: "me"` (or my member) is this machine's own view, the window's or the headset's left eye, at the
  capture's aspect. Another player's view is `capture_unavailable` until heads exist. An orbit stands `yaw_deg`
  (default 30) about world +y from the object's world +z side (its rotation is ignored) and `pitch_deg` (default 20,
  within ±89) above it, at `distance_m` (at most 10 km) or three bounding radii (at least 0.5 m), with a 50° vertical
  field of view. A camera that would stand outside the world's range is refused `invalid_argument` on `from`. A placed
  camera looks from `at` at an object or a position, with `fov_deg` (default 50, within 1-170). A placed `at` given
  relative to an object resolves when the request arrives, while the look-at object and an orbit's object resolve in
  the frame drawn, so for a moving object the two can differ by a frame or two.
- **pe-render** (`capture_service.rs`): a `CaptureQueue` the port's caller blocks on, and a `CaptureServer` that the
  loop drawing on this machine calls once a frame, after its present: the desktop window (`DesktopConfig.captures`),
  the headset (`VrSink::with_captures`), or pe-app's headless thread. It resolves each object's pose in the view it
  draws, so the camera and the picture agree. At most one capture is in flight per loop, and an order past its
  deadline is answered `timeout` and never drawn.
- **Headless:** with neither window nor headset, pe-app's thread (`pe_app::capture::spawn_headless`) opens a GPU
  context only at the first order. Tests take a software adapter (WARP, lavapipe) and report themselves skipped
  without one. With no adapter at all an order is `capture_unavailable`, and the next one tries again. A headless
  machine draws no view of its own, so `player` is `capture_unavailable` there with "capture with orbit or at".

## 9. Inside pe-api

### 9.1 Snapshots and retention

- After each frame, the simulation thread publishes one value through the snapshot port: the world snapshot
  (pe-world's, cheap by structural sharing, plan 2.1), the frame, the newest final frame, and the frame's change
  record: the edit entries stepped, with their outcomes (including re-stepped frames, whose newest verdict wins) and
  their inverses (5.5).
- pe-api keeps the newest published value and those of the last 2 s (a parameter) for `latest`, `at`, `base` and
  dry runs; `final` is not kept but asked for (below). A handler holds its snapshot only for the call.
- The snapshot is S2's `Replica::world_snapshot`, pe-world's `WorldSnapshot` at the replica's frame, which pe-app
  downcasts once per published view (S2 v1.7; @coordinator-opus). pe-app takes it only when it
  publishes a new view (`Arc::ptr_eq` on `view()` says it is new), and S2 publishes a view only at the newest frame
  the replica has completed, never during a re-simulation or a replay, so every kept snapshot is of the tip. While the
  replica trails the present, the tip is behind it (S2 v1.10; F3's S5). The frame view stays render-only.
- `final` is S2's `Replica::agreed_snapshot()`: pe-world's snapshot of the state at the replica's last agreed cut,
  exactly at the final frame, never lagging it, and never a state a repair replaced (S2 v1.7; @coordinator-opus, replacing the 2026-11-13 decision D2 (a)). Before the first agreed cut it is the frame-0 snapshot.
  Its frame is in the result, as for every read.
- A kept snapshot belongs to the timeline it was taken on (a snapshot at frame f is the state before f is stepped).
  pe-app drops the kept snapshots above each advance's `rolled_back_to` (S2's `Advance`: the frame of the state the
  replica restored, which also covers a late hand or input entry that leaves no change record), and all of them on
  a `Diverged` or `Damaged` finalize or a repair (rung 1's replay or rung 2's import) (app.md 3.1). Otherwise `at`
  could answer a state the replica replaced. Until a fresh snapshot is kept, `at` for the dropped frames answers
  `snapshot_gone`.
- `final` and the statuses agree: a change reads `final` only once a cut includes it *and* the final frame (Cv's,
  the same cut `final` answers from) is past it (5.3.2), so a change told final is always in `final`'s state.
- The change stream behind `watch` and the history index are built from the change records. The stream keeps the
  last 60 s or 100,000 items, whichever is less; history keeps the session's changes within a memory cap (parameters)
  [I]: `ChangeLog` holds up to 10,000 entries, and pe-app keeps each edit's decoded contents (`WorldSource::edit`)
  as long as `ChangeLog` holds the entry, oldest dropped first, up to 10,000 entries and 64 MiB of payloads [I]
  (app.md 3.1). Past either cap, history describes an older change without its contents.

### 9.2 The path of one `apply`

1. The adapter builds the `Caller`; pe-api checks the scope and the rate.
2. pe-api resolves paths, `$ref`s and selectors against the base snapshot, converts positions to parent frames,
   parses values into their binary types, fills in default `if_version`s, and checks the schema.
3. It runs pe-world's pure edit function on a copy of the base: permissions (including V1), versions, cycles. A
   refusal here returns at once, before anything is signed or sent. A dry run stops here and returns the diff.
4. It hands the edit payload (pe-world's typed ops with their expected versions, the producer label, the label) to
   the edit queue. The queue is bounded; when it is full, `rate_limited` with a retry time.
5. The simulation thread drains the queue at the start of its next frame. For each transaction it stamps the frame,
   submits the entry to agreement (S3 `submit`, which signs it with the member key) and steps it as a local
   prediction. It never waits on pe-api.
6. The outcome appears in that frame's change record; an `apply` waiting on the transaction returns it.
7. Agreement orders the entry; every machine's replica steps it; the status becomes `final`, `refused` or `dropped`
   when its frame is final, or `unknown` if a repair replaced the state it was stepped on (5.1). A change voided as
   late is re-sent before it is called `dropped` (9.4).

Step 3 checks against the base and step 5 against the present: the present wins. The replica's ordered check is the
one that counts (2, rule 3).

### 9.3 Limits

A token bucket per principal (file 06, D38 re-run), defaults [I]: 20 transactions per second with a burst of 40; per
transaction pe-world's limits, 4,096 ops (as written and after selector expansion), 65,536 new objects (copies
and templates count their whole subtree; pe-world owns the bound) and 1 MiB of payload apart from blobs
(world-model.md 7.1); at most 1,000,000 objects examined by one change's `where` selectors, each distinct selector
once [I]; 4 queries and 2 long polls in flight per principal. pe-agree's per-member entry rule (plan 4.3) bounds the member as a whole, whatever its principals do.

**The world's shape limit** (world-model.md 7.1). The world holds at most 520 collision shapes (pe-world's
`MAX_SHAPES`), the bound the slowest M1 machine was measured to keep up with. A change that would leave more, and more
than the world has, is refused `too_large` at its dry run, before anything is signed. The refusal gives `limit` and
`count`, and its `next` says how many shapes to delete first. A change that adds no shape always passes.

**Names travel inside the entry** (world-model.md 3.4; rulings). A name is at most 255
bytes of UTF-8, so it is always of pe-world's inline class (at most 256 B), and its bytes go into the signed entry
(`Transaction::inline`, each once, in ascending order of hash). Every machine that has the entry or the state can
then show it. A longer name is refused `invalid` on its op. pe-api refuses a change that carries more than 1 KiB of
names `too_large` ("split this change"), and it refuses one whose undo would carry more than 1 KiB, predicted at its
dry run from the inverse pe-world records ("undoing this change … split this change"). A delete's undo names objects
rather than bytes, so it carries none. A `revert` may carry up to pe-world's hard cap of 2 KiB per entry, which
leaves room for an inverse grown through a race. Past that cap the revert is refused `too_large`: "undo this in
parts". There is no split undo. Blobs over 256 B (hulls, meshes) don't travel inline; they wait for fetch (M1's
known gaps).

### 9.4 The re-send backstop

"Delayed, never lost" holds for edits whatever the cause (coordinator, rule 3; agreement.md 3, "An
outage's entries are delayed, never void as late"). Agreement's outage rule keeps an outage's entries from being void
as late, but a few can still be: a member at n >= 4 cut off while the others finalize past its stamps, or entries
from a member whose link is slower than the lag cap (agreement.md 3 measured 2-5 of 397 edits on a 1.2 s link).
pe-api re-sends those edits. It is the one place every edit source passes through, an agent's MCP calls and a
person's inspector or editor alike; pe-app's step 2 drains the queue as before, and hand entries never pass through
pe-api, so they are never re-sent (placed with @engine-architect).

- **What starts it**: a change submitted through this machine's API voided with `VoidReason::Late`, by a final cut
  (`confirm_cut`'s voids, which carry their reason) or by the replica's `Dropped { reason: Late }` record, whichever
  says it first. Never a world refusal: pe-world's verdict stands and the author is told why. Never `BackStamped`:
  once pe-agree refuses an own entry stamped below its chain's last stamp before signing it (@world-chunks,
  `BelowOwnStamp`), an honest member never makes one, so that void means a misbehaving or forked author, and
  re-sending would launder it. Never a change of another machine, and never a `code` change in M1 [I]: a script
  commit's `activate_at` must be its own stamp, and an attestation belongs to its build, so the author commits
  again.
- **What it re-sends, in order**: the voided change, then this machine's later API changes that depend on it, in
  their original order (by seq on the member's chain). A later change depends on it by content, not by its refusal
  (@design-reviewer's M2): it names an object the voided change created, or a version `{entry, op}` it wrote, in a
  target, a precondition, an `Undelete` or an `ObjectRef` field of a row or value. pe-world doesn't check that an
  `ObjectRef` names a live object [V: no such check in pe-world's apply], so a change that only stores the id is
  accepted, and only content finds it. Followed transitively, so a change depending on a dependent joins too.
  - **A refused dependent is re-sent**, rewritten (below), after the change it depends on.
  - **An accepted dependent is not re-sent**: it applied, and re-sending would make its creates twice. If it stored
    a reference to an object the voided change created, a follow-up re-points it: a plain write of just the
    `ObjectRef` fields that held such a reference, rewritten, each as its own field write, so it overwrites nothing
    else of the row that another member set since (@design-reviewer's code-review Low 1); under its producer,
    labelled "Re-point after a re-send: <label>". The
    follow-up goes after the re-send it points at, takes no notice in `mine` (the dependent's own outcome stands),
    and appears in `changes`. It passes the same later-word rule: a reference the author re-set later is not
    written over.
  - A dependent found once the re-send is under way (its refusal, or its step, arrives later) joins it after the
    ones already sent.
  - A dependent already re-sent, whose re-send is refused because a change before it went back to waiting (that
    one's re-send was void as late too), goes again after it: at once if it names that change in a target, a
    precondition, an `Undelete` or a script's base (that refusal can't be undone), otherwise once the refusal is
    final (@design-reviewer's code-review Low 2). A change only joins once: one the backstop already gave its final
    notice is settled.
- **The author's later word wins** (@design-reviewer's M1). Before a re-send is queued, each of its ops that this
  machine's later accepted API changes already overwrote is dropped, so a re-send never undoes the same author's
  later work: a field set later (`SetField` on that field, the whole row, the row removed, the object deleted); a
  whole row written later or removed; a `remove` of a row written later; a `reparent` of an object reparented
  later. A row written whole by the re-send whose fields were set later keeps those later values (pe-api patches
  them into the row; it knows each field's place from the schema at submission). If no op is left, nothing is sent
  and the change is `dropped`, its message saying the author's later changes had replaced all of it.
  - **Preconditions are kept, and rewritten, never dropped** (@design-reviewer's M1′): a precondition on X may
    guard ops on Y. Where the newest version of object X in the latest snapshot was written by one of this
    machine's later accepted API changes, an object precondition is moved to that version. The guard then still
    means "nobody else has touched X since"; the author's own later writes don't trip it; another member's write
    after them still refuses it as `stale_version`. A row precondition is not moved [I]: one on a row the author
    rewrote later refuses the re-send as `stale_version`, the safe side (revert's inverse is its main user, and a
    revert re-sent after its author's own later edit of that row should be refused).
  - Writes by other members are not protected: a re-send applies over them, within its preconditions. That is the
    accepted price the ruling names below.
- **Each re-send is a new entry** with the same transaction, through the edit queue, taken by step 2 at the frame it
  drains and stamped with it, with the next seq on the member's chain (@engine-architect). One edit a frame in queue
  order keeps the order. Before a re-send is queued it is rewritten to the entries now carrying the changes before
  it: an object an earlier change created (its id is derived from that entry's chain, seq and index k, 4.3) becomes
  the same index k of the re-sent entry, and a version `{entry, op}` it wrote becomes the re-sent entry's, at that
  op's place in the re-sent entry (ops the later-word rule dropped move the rest up), in targets, preconditions,
  `Undelete` and `ObjectRef` fields of rows and values. A version written by an op that was dropped is left as it
  was: the author's own later change wrote there instead, so what names it is refused as `stale_version`
  (@design-reviewer's code-review M1). A dependent waits in pe-api until
  the entry before it has its seq, normally the next frame. A dependent is rewritten from its original transaction
  each time, to the newest entries carrying the changes before it, so a second re-send of either stays consistent.
- **Preconditions still apply.** A re-send carries its preconditions, rewritten as above, and nothing else is
  relaxed: if someone changed the object since, it is refused as `stale_version` like any change, and that refusal
  is its outcome.
- **Back-off.** The first re-send goes at once. If a re-send is voided as late again, or agreement refuses it at
  submission (`TooLate`, or `BelowOwnStamp` once it exists), the next waits 0.5 s, then 1, 2, 4, 8 and 16 s, then
  every 16 s [I]. Re-sends keep the original order: one that waits holds back the dependents after it. It stops at
  the one-minute bound (refound-plan, "The one-minute bound": unconfirmed play never lasts longer than a minute),
  counted from the change's first submission (coordinator, (a)). A re-send already in agreement is
  left to its cut.
- **Held, never discarded** (coordinator, (c), after @fable-reviewer). A change still
  unapplied when automatic re-sending stops, at the bound or because its void was learned past it (a member at
  n >= 4 cut off for more than a minute learns its voids on reconnecting; @design-reviewer's L2), goes with its
  dependents into a **held batch**, in original order, kept as 9.4 would re-send them.
  - **One batch per outage**: a batch stays open while the backstop has re-sends waiting; changes that reach the
    bound meanwhile join it. Its handle is `"h:"` and 16 hex digits.
  - **The notice**: each change in it reports its one final outcome, `dropped`, with `held` (the batch's handle)
    and the message "Arrived too late and wasn't applied; it is held, so you can apply it now or discard it." A
    person's interface shows one notice per batch ("N edits from the outage weren't applied", Apply now and
    Discard; @gnosarium-ux's to word); `session` lists the open batches (`held`: handle, count, labels).
  - **Apply now** (`held` with `action: "apply"`; pe-mcp's `world_held`): the batch's changes go to the queue in
    order as ordinary new submissions of their own principals, prepared as a re-send is (rewritten, the later-word
    rule, preconditions kept and moved), with a fresh one-minute bound and the backstop again. It answers each
    change's outcome as `apply` would (`wait: "local"`), under the change's original id with `resent_as`, and
    `mine` reports each once more. It doesn't take from the principal's bucket again.
  - **Discard** (`action: "discard"`): the batch is gone; each change's `dropped` stands.
  - **Each principal acts on its own changes** (@design-reviewer's Low): `held` applies or discards only the
    caller's own changes in the batch (submitted under its principal; `edit` scope), in their order, and the rest
    stay held for their principals. A batch from one outage can hold an agent's and a person's changes together,
    and section 10 keeps principals apart. A change of the caller's that depends on another principal's held change
    is refused at apply, normally as `not_found`, until that principal applies theirs.
  - **Kept for the session, within a size limit, not a time limit**: at most 64 MiB of transactions and 64
    batches on this machine [I]. Past it the oldest batch is discarded, with a `session` event
    (`held_discarded`, its handle and count) to each principal with changes in it, `count` being theirs as in
    `held`, and the changes' `dropped` standing. Acting on a handle that is unknown,
    already applied, discarded or let go, or that holds none of the caller's changes, answers `invalid_argument`
    on `handle`.
- **One notice, with only the final outcome.** From the void on, `watch`'s `mine` reports nothing for the change,
  for its dependents (once they join) or for the re-sent entries. When a re-send is final, refused, `unknown` or
  given up, the change reports once, under its original `change` id, with `resent_as` and the re-sent entry's
  `frame`, `created` and `changed` (its versions enter the connection's table). `apply` with `wait: "final"` waits
  through the re-sends, within its own timeout; `history` lists the change once, under its original id, with
  the status of its newest re-send. `changes` is the world's record and reports every entry as it is: the void retracts the
  original's item, and the re-sent entry appears as its own change.
  - **Voids first** (@design-reviewer's L3): pe-api takes a publication's voids (and the cut's, which come before
    it) before the same publication's refusals and steps, so a dependent refused in the record that reports the
    void joins before `mine` could report it.
- **Its created objects get new ids.** A re-sent entry derives new ids for the objects it creates; the final notice
  maps the caller's `$names` to them. Until a re-send applies, the objects the voided change made are absent, so a
  new `apply` naming one is refused as `not_found` at once (step 3), before anything is queued.
- **Kept, and bounded.** pe-api keeps each of its own submissions' transactions (shared, not copied) until a final
  cut settles it, within 64 MiB [I]; past that the oldest let go, and a late void of one of those is `dropped` as
  before. A re-send counts against the member's entry rate in agreement, not against the principal's bucket again
  (9.3): the change was charged once.
- **The slow-link residual.** A re-send is stamped fresh and crosses the same link, so on a link slower than the lag
  cap it can be void again; recovery is by chance until pe-app stamps a slow member's entries ahead
  (@item:primordial-engine/pe-app-stamps-a-slow-member-s-entries-ahead-by-i, @engine-architect). The back-off and the
  bound keep it from churning meanwhile.
- **It should rarely fire** once agreement's outage rule is in. The frame a re-sent change applies at differs from
  the one the author saw, and it applies over other members' writes made since, within its preconditions: the
  accepted price of never losing work in the rare case (@design-reviewer's worry, accepted in the ruling).

## 10. Principals, scopes and safety

### 10.1 Scopes

| Scope | Allows |
|---|---|
| `read` | `session_info`, `world_schema`, `world_query`, `world_watch`, `history_list`, `view_capture` |
| `edit` | `world_apply`, `history_revert`, `world_held`, `code_check` |

There is no `control` scope on a live world: pe-harness drives time through its own API, not through MCP. The old
`interact` scope (an impulse, the agent's own hand) is not carried; an agent changes the world by edits. An agent
driving a hand, if wanted, comes back later as an input op [I].

### 10.2 Tokens and principals

A token maps to one principal: a name, the member it acts for, its scopes, its producer label (set at the
connection; gnosarium-ux open point 6), and the option `confirm` (off by default, C11), which holds each transaction
for the owner's approval in pe-app before step 4 of 9.2. Rights never come from arguments.

**`confirm` is not built in M1** (coordinator). Until it is, a live `apply` or `revert` on a `confirm`
token is refused `not_permitted` ("Confirmation isn't available yet; ask the owner to issue a token without it"),
and dry runs still answer: it fails closed, never applying without the approval the token promises. Open for when it
is built: a held status in S6, pe-app's approval port and UI, and what an unanswered approval does (refuse at
`apply`'s deadline, or stay held past it and report through `watch`).

### 10.3 Secrets

- The member's secret key is held by pe-app, never in a harness's environment, and never written where a harness's
  file tools can read it (file 06, M5). The token is the only secret a harness holds.
- pe-app's launch profiles remove the token from a harness's tool subprocesses where the harness allows it (kept from
  the old README's limits).
- The built-in agent loop runs on an API key, and a guest may use a subscription login with a plain warning (A1, A2):
  that is M5's agent-panel track. WorldApi is the same for both.

### 10.4 Protections (V1)

The user's ruling: received avatars and objects get simple Resonite-style protections. pe-world
holds the rule (world-model.md 7.8); the API reports it:
- a `Protection` row names an owner (a member key) and protects its object and subtree;
- **who may protect** (coordinator, U5): only the object's creator (the author of its creating entry) or the member
  who imported it may add a `Protection`, and only with themselves as owner. Anyone else's attempt, `EDIT` or not, is
  refused as `protected`. Auto-protection applies on import and creation only; wearing never claims or protects
  anyone else's avatar;
- for anyone but the owner, `apply` refuses with `protected`: `duplicate` of a protected object or of a subtree
  holding one; `remove` of its `Protection` or `set` of its owner; adding a `Protection` inside someone else's
  protected subtree. From M5 also wearing, saving, exporting and fetching its assets;
- ordinary editing (moving, recolouring, deleting) stays under the role's permissions, so an admin can still delete
  a griefing object;
- an agent acting for the owner acts as the owner;
- every machine checks it at the ordered step, since it is world state. Like Resonite's, it stops the engine's own
  paths, not a modified client.

### 10.5 No world owner

The user (19:50-19:51Z, via @fable-reviewer): there is no separate "world ownership" state; every admin is a
co-owner. So no API result has a world-owner field: `session` reports members and roles only, and history shows
members. The `confirm` option of 10.2 holds a transaction for the person whose agent it is, not for a world owner.

### 10.6 Agents don't move their user unless the user allows it

The ruling (coordinator; refound-plan's range bound): the agent interface offers no "move my user"
action (locomotion, a teleport, or a bodiless member's view position) unless the owning user switches it on locally.
It is a client setting of that user ("let agents move me"), off by default, and nothing in the world depends on it.
pe-app reports it in `SessionFacts::agents_move_user`. With it off (coordinator, on @fable-reviewer's
advice):

- **"The user"** is the caller's avatar root, every tracked node under it (the head and both hands: the objects
  whose `Hand` row the caller owns), and all of the root's ancestors (a vehicle, a folder). The head's own
  descendants and the root's other children are not part of it.
- **Refused** `not_permitted`, naming the op and the setting, for an `apply` or `revert` from an agent token
  (producer `agent`); a dry run refuses the same way, since the change itself is what isn't allowed:
  - a write, add or removal of a `Transform` on any object of the set;
  - a write, add or removal of a `Body` or `Shape` on it, which would move it through physics (velocity,
    `BodyState`, is not writable by edits at all);
  - re-parenting any object of the set;
  - deleting any object of the set. Deleting an ancestor is refused too: pe-world's `Delete` destroys the whole
    subtree (world-model.md 7.5), so it would delete the user's avatar with it, and the orphan rule covers only
    edits made under an already-deleted parent.
  - Hand-less grabs and pushes aimed at the set are to be refused the same way; they are not API operations in M1.
- **Not covered:** physical contact, such as a ball pushed into the user or a platform they stand on without being
  parented to it, and components that drive a `Transform` from logic. The check covers direct writes and inputs only.
- **A local courtesy, not a protection.** pe-api checks it on the owner's machine, against the ancestry as that
  machine sees it when the change is made. Nothing checks it at the ordered step, and a modified client skips it.
- **Only the caller's own user.** Moving other players is a matter for world rights and protections (10.4).
- **A person's own tools** (producer `hand`) are not agents, so the inspector and editor move their user as before.
- **A member with no head object** leaves this inert: a bodiless member's view is moved only by pe-app, so any
  pe-app control that lets an agent move that view checks the same `agents_move_user` setting.

## 11. Seam S6 in pe-seams

@engine-architect accepted, and writes pe-seams v0 itself, after the archive lands:
- **In pe-seams:** the `WorldApi` trait of section 5 and its request and result types: `Caller`, `Query`, the
  selector, `Transaction` and its ops, `ApplyOpts`, `Outcome`, the refusal codes, `Filter`, `Changes`, `Cursor`,
  `HistoryQuery`, `CaptureRequest`, `Capture`, `CodeCheck`, `Diagnostics`, `ApiError`, with a self-describing
  `Value` so pe-seams does not depend on pe-world. pe-mcp, pe-app, the inspector and pe-harness code against S6 with
  a mock.
  - A compiled draft of these types and the trait is `crates/pe-api/src/s6.rs` on `ws/api`, for @engine-architect to
    take or reshape. It carries serde and schemars derives, because pe-mcp's tool schemas are generated from them.
    pe-seams depends only on pe-core, so I propose those derives behind an optional `serde` feature that pe-api and
    pe-mcp enable; the draft uses JSON's data model as the `Value` [I; @engine-architect's call].
- **In pe-api, not the seam:** the four ports of section 3, as traits pe-app implements (accepted, O9). They use S2,
  S3 and S5 types and pe-world's snapshot, and only pe-api and pe-app see them.
- **From the other seams, all accepted:**
  - **beside S5**: a per-frame change record, the edit entries stepped with outcomes and inverse ops, newest verdict
    winning (9.1), published with the frame view and the world-model snapshot (`Replica::world_snapshot`, S2
    v1.7). S5's `ViewObject` carries each object's edit version and its simulated pose as a `Pos`;
  - **S2**: `World::apply(frame, &entry) -> Outcome`, `Applied { created, overwrote, notes, inverse }` or
    `Refused { op, reason }` (5.5);
  - **S3**: `submit` is called by the simulation thread with the payload pe-api built; pe-api never calls S3;
  - **S1**: the edit payload is pe-world's `Transaction { actor, preconditions, ops, blobs }` plus `label`
    (world-model.md 7.1). pe-api turns `if_version`s into `ObjectVersion` preconditions in its separate list;
    created ids derive from (world, key, chain id, seq, k).
  - **S6 v1.6** (pe-seams v1.9, @engine-architect): `Outcome.resent_as: Option<TxId>` (5.3.2, 9.4); for held
    batches, `Outcome.held`, `WorldApi::held(caller, handle, action) -> Vec<Outcome>`, `SessionInfo.held` and the
    `held_discarded` session event.
  - **S6 v1.8** (@engine-architect, 88ea0a5c): `Brief.script: Option<String>`, a script component type's script as
    `"a:<64 hex>"`, left out for the engine's types (4.3, 5.6).
  - **S6 v1.9** (@engine-architect): `SessionInfo.scripts_not_running: Vec<ScriptNotRunning { script, since_frame,
    why }>`, left out when empty (5.1).
    **Beside S3**, in pe-api's own ports: `ChangeLog::confirm_cut` takes each void with its `VoidReason`, which
    pe-app copies from agreement's `Voided` events.
  - **S6 v1.10** (@engine-architect, coordinator): `Op::Add.replace: bool`, default false and left out
    when false (5.3). Without it, `add` on a component the object has is refused as `invalid`.
  - **S6 v1.11** (agent tooling, user ruling): `Query.summary: bool`, `QueryResult.cut: Option<String>`
    and `QueryResult.summary: Option<QuerySummary { count, types, bounds: Option<Bounds { relative_to, min, max }>,
    objects: Vec<ObjectBrief> }>` (5.2); `Op::Duplicate.count: Option<u32>` and `step: Option<[f64; 3]>` (5.3).

## 12. Open points, and whose they are

Settled with their owners: O1-O7 (@world-model, world-model.md at adbf8c7), O8 and O9 (@engine-architect), O10 (@iter-bench), O12 (`Actor::Agent { harness, principal, requested_by }`,
world-model.md r8 at 7730167), O11 (captures, @ballpit-viz), O6b (rotations: y-up, Unity's maths, no mirror at the API; @vdw-physics), U4 (root positions, with @world-model and @gnosarium-ux). Still open:

| # | Point | Whose |
|---|---|---|
| O8b | pe-seams' optional `serde` feature, or mirrored types in pe-mcp | @engine-architect |

## 13. Against `docs/ux/api-surface.md` (75615ce)

Adopted: the underscore names; the selectors; `as` references; `where` with a required `max`; selector ops as plain
writes; the default `if_version` from the connection's last read; `"any"` for a plain write; `wait`; `reparent`
`keep`; positions as `{relative_to, offset}` with `world_origin` never a default; a root's position as the exact
`"pos"` with chunk indices as decimal strings (its syntax, `{"pos": {"chunk": [..], "offset": [..]}}`, is fixed
here as drafted); rotations in degrees or a quaternion; colours; the refusal envelope with `winner` and `next`, and
its messages, including the draft's wording for the five codes this spec added; the `code_check` description;
motion excluded from `world_watch`; a 30 s long poll; revert with preconditions; the capture camera forms and
`visible_list`; the 5,000-token budget; the server's instructions; V1 as a `Protection` row under U5; the producer
label set at the connection; captures of another player's view allowed; mass shown as a derived, read-only
`mass_kg`, set through density.

Settled here, and taken into the draft from 519f04f on:
- **Statuses**: `queued`, `accepted`, `final`, `refused` (with `final`), `dropped`.
- **Versions are per object** in M1, opaque, so per row can follow.
- **`invalid` covers** type, range, unknown fields, read-only fields and cycles; added `ambiguous`, `held`,
  `too_large`, `not_ready` and `compile` (at submission only).
- **A ninth tool, `code_check`,** for the M1 logic edit (U1).
- **Selectors gain** `ids`, `path` and `under`; `near.of` takes `"me"`.
- **`motion`** is an opt-in on `world_watch` for at most 16 named objects.
- **Paging children** (inspector.md 7, ask 1): `child_of` with `cursor`, in the children's order. **A per-object
  watch** (ask 2): a `where` with ids costs a lookup per change, cheap enough to keep open per selected object.

One correction for the draft's walk-through (api-surface.md 7, step 7): Bob moving the left lamp **by hand** is a
grab, an input, so it does not change the lamp's version. If Bob still holds it, the agent's delete is refused as
`held`; if he has let go, the delete applies. The stale-version case needs an edit, for example Bob changing the
lamp in the inspector between the agent's query and its apply.

## 14. Tests (M1)

In `crates/pe-api` and `crates/pe-mcp`, against mock ports, and with two machines: in CI, two members in one test
process over pe-app's in-process network (`crates/pe-app/src/agent_demo.rs`), and the two-process session of app.md 6:

- **The edit tests of plan M1:** create, delete, set, add, remove, reparent and duplicate work; a transaction applies
  all or nothing (a refused third op leaves the first two unapplied, on both machines); a dry run changes nothing (the
  root hash is unchanged, nothing is sent) and its diff equals the live change's; a stale precondition is refused with
  the winner's change, member, producer and value; every refusal, at once or after ordering, reaches the agent over
  MCP.
- **Concurrency:** two agents on two machines set one field from one read: exactly one `final`, one `stale_version`
  naming the other, on both machines; a rolling ball is recoloured without a refusal; a selector resolves to the same
  ids on every machine; a late earlier entry the reader never saw still makes its edit stale.
- **Never blocking:** a simulation stepping at 90 Hz misses no frame while 8 clients run queries, long polls, applies
  and captures at their limits (the frame-time distribution is reported); a slow client cannot hold a snapshot past
  its call.
- **Watch:** across cursors no item is lost or repeated, under rollback too; `retracted` appears when a re-step undoes
  a predicted edit; an expired cursor is refused with a fresh one.
- **Revert:** a revert of each op kind restores the prior root hash; a revert after a later change is `stale_version`.
- **Positions:** a value written relative to another object and read back relative to the parent agrees; nothing
  returns `world_origin` unless asked; a root's `"pos"` round-trips exactly at chunk indices past 2^53; `"parent"`
  for a root is `invalid`.
- **Protections:** a non-owner's `duplicate` of a protected object is refused on every machine; the owner's agent
  succeeds; a member who neither created nor imported an object cannot protect it.
- **Holds:** while a player holds an object, an agent's delete or move of it is `held` and its recolouring applies.
- **Capture:** the ball pit is captured headless through a software adapter within the deadline, and `visible` holds
  the cube the agent just made.
- **MCP:** the kept transport and token tests move over (HTTP host and origin rules, pipe DACL, handshake, revocation,
  scope filtering); the schema file is current; the tool list stays within its budget; no tool argument panics the
  server (property tests over arbitrary JSON).
- **The agent's demo script in CI** (`agent_demo.rs`; app.md 6 runs the same script as two processes). M1's ball pit
  has no table and one lamp, so the CI form puts the cube in the pit and removes that lamp: "put a red cube on the table", "make the balls blue", "double the cube", "remove
  the left lamp", with a stale case (another member edits the lamp between the agent's query and apply) and a held
  case (the lamp held by hand), and the logic edit, against the two-process session, checked by query and capture on
  both machines.
- **Logic edit:** a source that fails `code_check` is refused as `compile` at submission and makes no entry; a
  committed script's components appear in `world_schema` with a new version.
- **Measured and reported:** apply to `accepted` (target: one frame plus the queue, p99 under 50 ms [I]); apply to the
  other machine's first frame showing it; apply to `final`; query time at 500 and 10,000 objects; capture time.

## 15. Later

- M2: `in_fork` and `fork_*`; `code_test`; per-player fields in the schema, with each field's class (shared,
  private, published), its "fed by" declaration and writer list (world-model.md 3.7); `shader_validate` and `asset_import` once
  their owners exist.
- M3: history from the world record since creation; governance in `session` and the schema.
- The world-line visualization tool any admin can take out (file 19,
  @item:primordial-engine/world-line-visualization-tool-braided-glowing-li) reads branches and merges through
  `history`; its per-player summaries come from pe-world and pe-diverge, not from this API.
- M5: the first-party agent loop and the agent panels call WorldApi in-process; the inspector (`docs/ux/inspector.md`)
  is a WorldApi client with no private path.
