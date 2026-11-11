# WorldApi and the MCP tools (WS-api)

Status: draft v1, 2026-11-10, by @api-eng. Nothing in it is built. Owner of this file, `crates/pe-api` and
`crates/pe-mcp`: WS-api. Docket: @item:primordial-engine/ws-api-worldapi-pe-api-the-mcp-adapter-pe-mcp-an.

v1 folds in @world-model's answers (`docs/spec/world-model.md`, branch `world/spec`, adbf8c7), @engine-architect's
answers on the seams (m90e54e5d332d), and @coordinator-opus's rulings U3 and U5.

How the surface reads to an agent and to a player (descriptions, messages, cues, the inspector) is @gnosarium-ux's:
`docs/ux/api-surface.md` and `docs/ux/inspector.md` (branch `ux/api-surface`, 75615ce; 70fb723 approved by
@chunks-reviewer). This file is the authority on types and semantics. It adopts that draft's names, shapes and
wording except where section 13 says otherwise.

Sources: the re-founding plan (`docs/refound-plan.md`, b610464: section 1 M1, 3.1, 3.2 S6, appendix A); the review,
`_teams/1847/fable-review/` files 06, 08, 11 (3.2, 3.7), 14 (P4) and 17 (7); the old `crates/pe-mcp` at 757219f;
the user's decisions of 2026-11-10 in `_shared/user-design-decisions.md`.

Marks: **[V]** verified in the named file or record; **[I]** my judgement; **[agree: X]** needs owner X's agreement
before it is fixed (section 12 lists them).

## 1. What it is for

- **One typed, in-process API over the live world**, which every consumer calls: the MCP server, pe-app's own
  tools, and later the inspector, the editor and the first-party agent loop. "Every capability a human has also
  exists as an MCP tool" then holds by construction (file 06, A1).
- **M1 needs it for the slice's step 5** (plan section 1): an agent on machine A reads the world (`world_query`),
  changes it in one transaction (`world_apply`) and looks at the result (`view_capture`); both machines show the
  change; an edit against a stale version is refused with its reason and the change that won.
- **And for the small logic edit the user added to M1** (U1, mc40fc6379bc6): the agent checks and commits a script
  through the same surface (section 7).

What it replaces [V file 06, sections 1-3]: the old surface could change two things (an impulse, the agent's own
hand); its seam depended on the harness; the live peer's main loop ran on pe-mcp's thread, and `world_step` blocked
every other call for up to 10 s; there was no transaction, change feed, undo or image.

## 2. Rules every part of it keeps

1. **It never blocks the simulation.** Handlers read a published snapshot and hand transactions to the simulation
   thread through a bounded queue. Nothing in pe-api takes a lock the simulation thread waits on (file 06, A1).
2. **It never makes its caller wait for agreement** (the governing principle, mfc7e0ddc4137). A transaction applies
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
7. **Positions are never absolute by accident** (file 17, 7; m06d2643578d7). Every position the API takes or gives
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
- **Transform**: a child's position, rotation and scale are relative to its parent, as in Resonite (file 17,
  m06d2643578d7). A root's position is an exact `Pos` (world-model.md 4.4; here 4.4).

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
- **A precondition applies to the object an op changes, not to a parent it attaches to** (coordinator
  m8e173fe643ad, with fable-reviewer). `create`, `reparent` and `duplicate` put no precondition on the parent:
  pe-world's own rules decide, so concurrent creates under one parent both succeed, the cycle check still applies,
  and a create or reparent under a parent deleted earlier in the agreed order is orphaned (world-model.md 7.5).
  Protections apply as world-model.md 7.8 defines them: they guard taking, not touching (coordinator
  mb7d6d98f96dd, the user's Resonite-style rule). A non-owner with `edit` may add children under a protected
  object, but not copy it, change its protection, or protect anything inside it. The object being reparented, and a duplicate's source, keep theirs.
  - An orphaned create or reparent says so in its outcome: `orphaned`, with the new root's id and its place.
  - Future, when someone needs it: an opt-in `parent_version` on `create`, `reparent` and `duplicate`.
- **Unknown fields are refused, not ignored.** An op carrying a field its type doesn't take (say `if_version` on a
  `create`) is refused as `invalid_argument`, so nothing an agent writes is silently dropped. Every JSON surface
  that takes a transaction does this check (in M1, the MCP tools).

### 4.3 Ids, names and references

- Ids travel as strings: objects `"o:"` and 32 hex digits; entries `"e:"` and 64 hex digits (256-bit hashes, S1);
  transactions `"x:"` and 64 hex digits (the entry id of the transaction's edit entry); members `"m:…"`; a world's line
  `"l:"` and 64 hex digits (world-model.md 2.3: a world is a line, and `LineId` replaces `WorldId`). Hex digits are
  lowercase, with no sign: each id has exactly one spelling, so ids may be compared as text.
- **Paths**: `"/Table/Lamp (left)"` addresses by names from the root; names need not be unique, so a path matching
  more than one object is refused as `ambiguous`, listing the matches.
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
    snapshot; the entry carries the parent-relative value.
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
- **A `world_origin` offset** is `pos - Pos::ORIGIN`, an `f64` per axis in metres (the user's deliberate escape,
  m06d2643578d7). Its precision falls with distance from the origin: about 1e-10 m at 1,000 km, about 1 mm at
  10^13 m (world-model.md 4.2). The schema says so; for precision anywhere, use the exact `pos` or an offset from a
  nearby object. Positions beyond the i64 chunk range are refused in M1-M3 (world-model.md 4.3).
- **Rotations**: `"rotation_deg": [x, y, z]`, Euler angles in degrees in Resonite's order (Unity's `eulerAngles`:
  applied Z, then X, then Y), or `"rotation_quat": [x, y, z, w]`. Both are accepted; the store holds quaternions;
  results give degrees, and the quaternion when asked.
  - They are taken in the world's own frame, which is y-up (gravity (0, -10, 0)), with the active rotation q·v·q*,
    so +90° about +y takes +z to +x, as in Unity [V @vdw-physics, mc38dddbb681e: `pe-physics/src/config.rs:37`,
    `box3d-fork/src/math_functions/quat.rs:13-20`]. The API applies no mirror: the same numbers mean the same
    rotation as in Unity's maths.
  - Physics has no handedness; which way +z is drawn is pe-render's and pe-xr's convention. If the world is drawn
    right-handed with -z forward (OpenXR's), then data brought in from Resonite (an import, M5) mirrors z: a position
    (x, y, z) becomes (x, y, -z) and Euler angles (ex, ey, ez) become (-ex, -ey, ez), in the same order. That belongs
    to the importer, not to this API [I].
- **Colours**: `"#rrggbb"` or `"#rrggbbaa"` (sRGB), or `{"srgb": [r, g, b, a]}` in 0-1.
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
- An agent acts with all of its member's permissions in the world (coordinator, m220b6210535b); its token's scopes
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
join as admins. Every admin is a co-owner; there is no world-owner field (10.5). Also: the world's line id and name; the
members present; the local frame, the newest final frame and the tick rate; the link state in the
player's words (`fine`, `thin`, `some actions dropped`, `catching up`, `watching only`; file 08, A6); the player's
scale; the API's limits (9.3); and `kept`, whether an admin of the world is present (file 18, 2.2; gnosarium-ux
draft 6). The user ruled (file 18, 6-7): a session with no admin keeps nothing unless an admin joins before it ends,
shown with a permanent mark. So with no admin present, `kept: false`, and the players see "Nothing here is being
kept".

While this machine is catching up (a joiner watches first and acts once caught up, file 14 S4), `apply` refuses
with `not_ready`.

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
  types only. `"*"` returns everything.
- `frame`: the frame positions are given in (4.4), default `"parent"`.
- `children`: how many levels of children to inline (default 0).
- `limit` (default 50, at most 256) and `cursor`. A result cut by the limit carries `more`, a cursor for the next page.
  Over `child_of`, pages follow the children's order, which serves the inspector's paged hierarchy (inspector.md 7,
  ask 1).
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
                    "Appearance": {"color": "#d22b2b"}}},
    {"op": "set", "where": {"has": "Ball"}, "max": 1000, "field": "Appearance.color", "value": "#2b5bd2"},
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
| `add` / `remove` | Adds or removes a component (`component`, with initial `fields` for `add`) | |
| `reparent` | A new `parent`, and an `index` among its children | `keep`: `"world_pose"` (default) or `"local_pose"`; refused as `invalid` if it would make a cycle |
| `duplicate` | A copy-on-write copy of a subtree, new ids over the same records ("b, 1 always", R11 rulings; file 11, 3.2) | `protected` for a non-owner (V1) |
| `delete` | An object and its subtree | Plain Delete unlinks a linked script (R11 rulings) |
| `code` | The logic edit of U1 (section 7) | |

How they map onto pe-world's ops (world-model.md 7.1): `create` is `Create`, or with a `template` a `Copy` of the
template object (templates are objects under a `Templates` root) followed by its `set`s; `set` is `SetField` per
field; `add` is `SetRow`; `remove` is `RemoveRow`; `reparent` is `Reparent` with `keep_world` from `keep`;
`duplicate` is `Copy`; `delete` is `Delete`; `code` is the `Register` and `CommitScript` pe-logic builds (section
7). Default and explicit `if_version`s become `ObjectVersion` preconditions in the payload's precondition list;
`$ref`s become `New(k)` targets.

- **Targets**: `id` takes an id, a path or a `$ref`. Instead of `id`, `set`, `add`, `remove` and `delete` take a
  `where` selector with a required `max`. The selector is resolved against the base snapshot on this machine into
  explicit ids; the entry carries the ids, never the selector, so every machine applies the same change. More
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
reparents whose parent was gone, 4.2), and, when refused or dropped, the refusal (5.3.3). Versions in it enter the connection's table.

| `status` | Meaning |
|---|---|
| `queued` | Accepted by the API, not yet stepped (`wait: "none"`, or the wait ran out) |
| `accepted` | Applied on this machine at frame F, its stamp. Others see it within about one network delay. Not final |
| `final` | Applied by the ordered step at F, and a final cut covering F includes it without voiding it (never the frame alone). It can no longer be undone except by a new change |
| `refused` | Refused, with a reason. `final: false` while this machine's check refused it before ordering or the refusal is still predicted; `final: true` once F is final |
| `dropped` | Voided by a final cut (late, or back-stamped; S3 v1.1), so no replica applies it: its local effect is undone everywhere. A change stamped below a final cut's frame that the cut does not include is still pending, not dropped, until a later cut includes or voids it |

- An `accepted` change can still become `refused` when an earlier conflicting change from another machine arrives
  and the replica re-steps (file 14, P4). That is the stale edit seen from the loser.
- Every change of status after `apply` returns is reported through `watch` (`mine`, 5.4), so no refusal is silent.

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
| `invalid` | A value of the wrong type or out of range; an unknown component or field; a read-only field; a cycle; a `duplicate` of a subtree an earlier op of the same change adds to, moves or deletes from | `field`, `rule` (from the schema), `given` |
| `in_change` | An op names a `$ref` this transaction does not create | `ref` |
| `too_many` | A selector matched more than `max` | `count`, `max` |
| `not_permitted` | The world's permissions | `permission`, `role` |
| `protected` | V1: a protected object, for anyone but its owner (10.4) | `owner`, `action` |
| `held` | A held object (or an object under it) may not be deleted, nor its `Transform`, `Body`, `Shape` or parent edited, while a player holds it; other edits, such as a colour, pass (world-model.md 7.2) | `held_by` |
| `too_large` | Past the size limits (9.3) | `limit` |
| `rate_limited` | Past the principal's rate (9.3) | `retry_after_ms` |
| `not_ready` | This machine is still catching up | — |
| `compile` | A `code` op whose source does not compile on the author's machine. Only at submission: no entry is made (section 7) | `diagnostics` |
| `dropped` | The change arrived too late and was undone (status `dropped`) | — |

- `message` is one sentence, engine-written, safe to show a player; peer text in it is quoted (rule 6). Its wording
  is @gnosarium-ux's (api-surface.md 3.5). `next` is for agents; a person's interface shows buttons instead
  (Re-apply, Show).
- `refused` is a property of the transaction, not of the call, but pe-mcp returns a refused `apply` with
  `isError: true` (6.3), so an agent cannot miss it.

#### 5.3.4 Dry runs

`dry_run` runs the same resolution, validation and pure edit function on a copy of the base snapshot, off the
simulation thread, and returns the outcome it would have had there (`status: "accepted"` or the refusal, with
`dry_run: true` first) and the diff: objects created and deleted, and for each written field its value before and
after. Created objects show as their `$ref`s only, since a dry run takes no sequence number. It changes nothing and
sends nothing. It does not step physics or logic: what a new body then does is a fork's question (M2) [I].

### 5.4 `watch`

A change feed with a cursor. `Filter`:
- `where`: a selector, as in a query, narrowing everything below to those objects. With ids, a filter costs a lookup
  per change, so an inspector may keep one open per selected object (inspector.md 7, ask 2).
- `changes`: edits by anyone: `{change, by, producer, label, frame, final, created: [...], deleted: [...],
  reparented: [...], set: [{id, field, value?}]}`. Values are included only for fields `fields` names, or 500
  recoloured balls would flood the agent.
- `mine`: status changes of this caller's own transactions, with the refusal envelope (always on).
- `session`: members joining and leaving, this machine catching up, and the newest final frame when it moves.
- `motion`: off by default. **Simulation is not a change**: bodies moving and logic-written fields do not appear in
  `changes`. An agent that wants motion queries poses. `motion: {"every_ms": 250}` with a `where` of at most 16 ids
  adds sampled poses of those objects [I].
- `final_only: true` holds changes back until final, at the cost of about one cut's delay.

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

- Without a type: the list of component types `{type, summary}` and the world's templates `{name, summary}`, and the
  schema `version`. Small on purpose.
- With a type: a JSON Schema per field, with `x-` annotations standard validators ignore (gnosarium-ux 3.2):
  `x-unit` (`m`, `deg`, `kg`, `s`, `srgb`, …); `x-sync` (`synced`, or `unsynced` when M2 brings per-player fields; the
  inspector marks unsynchronized fields with a shape, Q12); `x-protected` (part of a protection); `x-read-only` (the
  engine writes it; an edit is refused as `invalid`).
- With a template: what it holds.
- `version` is a hash of the registry, which is versioned by frame (plan 2.1); a logic edit can add a component type
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
  only, and revokes every token when it exits; `pe-mcp bridge` relays stdio to the pipe.

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
  `capture_unavailable` (no renderer or no adapter), `timeout`, `session_ended`, `internal`. Unknown tools and
  malformed requests are JSON-RPC errors (kept).
- **A refused transaction** (5.3.3) is also returned with `isError: true`, its envelope as the structured content:
  `code` and `message` at the top, the rest in `details`. Clients then show it as a failure, which it is for the
  agent; pe-api's types still keep the two apart.
- No tool argument may panic the server: a panic in a handler is caught and returned as `internal`, and the world is
  untouched, because handlers hold only a snapshot.

## 7. The logic edit in M1 (U1)

The user added a small logic edit to M1 (mc40fc6379bc6); the coordinator brought Cranelift into M1 for it (19:52Z).
WS-logic (@iter-bench) owns what a script is, how it compiles and how it runs (`docs/spec/logic.md`, branch
`ws/logic`, 75ac956, sections 4 and 6). This section gives the API's slots, as agreed with @iter-bench (me99701e9972d).

- **`code_check`**: compiles a script's source to wasm through pe-logic and validates it. It returns rustc's JSON
  diagnostics (file, line, column, severity, message, code) plus pe-logic's own refusals as kinds (`import`,
  `feature`, `manifest`, `reserved_name`); on success also the module hash, the declared systems and components, and
  the build time. It changes nothing. Builds run one at a time per machine at idle priority, so a check may queue
  behind a commit's build; it runs on a worker through the logic port, never the simulation thread, and the call's
  deadline bounds it.
- **A script is an asset, not an object.** It is bound to objects through a `Script` component.
- **The `code` op** of `apply`: `{"op": "code", "id": <script asset id, or a $ref for a new script>, "source":
  "<text>" | "b:<hash>", "attach": [objects to bind]}`. In M1 the text is one file, `lib.rs`, depending only on
  pe-sdk; a blob tree (`"b:…"`) for multi-file crates comes later, and the schema keeps both forms. Its
  precondition is the script's current version (the hash of its source tree): two agents committing on one base get
  one winner and one `stale_version`.
- **Compiling is checked once, at submission.** The author's machine compiles and validates before it submits; a
  failure refuses the `code` op as `compile`, with the diagnostics, and no entry is made. Once the entry exists, its
  outcome is the same everywhere: `accepted`, then `final` at the cut, or `stale_version` by its precondition. An
  outcome is world state, so it cannot depend on one machine's compiler.
- **Activation follows Q3** (mf7daea6446ca): the author's machine runs the change at once; every other machine
  compiles the same source itself and activates at the agreed frame (`activate_at`, stamped by the simulation thread
  at submission; the API never sets it); no machine runs another's build (Q3c). If another machine's own build fails
  or differs, that is a local fault on that machine, reported with both module hashes and recovered as a diverged
  member (WS-diverge). It is not a refusal, and the author's outcome does not change.
- Component types a script declares appear in `schema` once it is active, with a new schema version.
- `code.test` (a fork with random rollbacks) and `fork.*` are M2.

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

Rules, agreed with @ballpit-viz (mcd68d4325ea7; WS-render writes them into pe-render's spec):
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

## 9. Inside pe-api

### 9.1 Snapshots and retention

- After each frame, the simulation thread publishes one value through the snapshot port: the world snapshot
  (pe-world's, cheap by structural sharing, plan 2.1), the frame, the newest final frame, and the frame's change
  record: the edit entries stepped, with their outcomes (including re-stepped frames, whose newest verdict wins) and
  their inverses (5.5).
- pe-api keeps the newest published value, the newest final one, and those of the last 2 s (a parameter) for `at`,
  `base` and dry runs. A handler holds its snapshot only for the call.
- The change stream behind `watch` and the history index are built from the change records. The stream keeps the
  last 60 s or 100,000 items, whichever is less; history keeps the session's changes within a memory cap (parameters)
  [I].

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
   when its frame is final.

Step 3 checks against the base and step 5 against the present: the present wins. The replica's ordered check is the
one that counts (2, rule 3).

### 9.3 Limits

A token bucket per principal (file 06, D38 re-run), defaults [I]: 20 transactions per second with a burst of 40; per
transaction pe-world's limits, 4,096 ops (as written and after selector expansion), 65,536 new objects (copies
and templates count their whole subtree; pe-world owns the bound) and 1 MiB of payload apart from blobs
(world-model.md 7.1); at most 1,000,000 objects examined by one change's `where` selectors, each distinct selector
once [I]; 4 queries and 2 long polls in flight per principal. pe-agree's per-member entry rule (plan 4.3) bounds the member as a whole, whatever its principals do.

## 10. Principals, scopes and safety

### 10.1 Scopes

| Scope | Allows |
|---|---|
| `read` | `session_info`, `world_schema`, `world_query`, `world_watch`, `history_list`, `view_capture` |
| `edit` | `world_apply`, `history_revert`, `code_check` |

There is no `control` scope on a live world: pe-harness drives time through its own API, not through MCP. The old
`interact` scope (an impulse, the agent's own hand) is not carried; an agent changes the world by edits. An agent
driving a hand, if wanted, comes back later as an input op [I].

### 10.2 Tokens and principals

A token maps to one principal: a name, the member it acts for, its scopes, its producer label (set at the
connection; gnosarium-ux open point 6), and the option `confirm` (off by default, C11), which holds each transaction
for the owner's approval in pe-app before step 4 of 9.2. Rights never come from arguments.

### 10.3 Secrets

- The member's secret key is held by pe-app, never in a harness's environment, and never written where a harness's
  file tools can read it (file 06, M5). The token is the only secret a harness holds.
- pe-app's launch profiles remove the token from a harness's tool subprocesses where the harness allows it (kept from
  the old README's limits).
- The built-in agent loop runs on an API key, and a guest may use a subscription login with a plain warning (A1, A2):
  that is M5's agent-panel track. WorldApi is the same for both.

### 10.4 Protections (V1)

The user's ruling (mbe87e750ca35): received avatars and objects get simple Resonite-style protections. pe-world
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

## 11. Seam S6 in pe-seams

@engine-architect accepted (m90e54e5d332d), and writes pe-seams v0 itself, after the archive lands:
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
    winning (9.1), published with the frame view and the world-model snapshot. S5's `ViewObject` carries each
    object's edit version and its simulated pose as a `Pos`;
  - **S2**: `World::apply(frame, &entry) -> Outcome`, `Applied { created, overwrote, notes, inverse }` or
    `Refused { op, reason }` (5.5);
  - **S3**: `submit` is called by the simulation thread with the payload pe-api built; pe-api never calls S3;
  - **S1**: the edit payload is pe-world's `Transaction { actor, preconditions, ops, blobs }` plus `label`
    (world-model.md 7.1). pe-api turns `if_version`s into `ObjectVersion` preconditions in its separate list;
    created ids derive from (world, key, chain id, seq, k).

## 12. Open points, and whose they are

Settled with their owners: O1-O7 (@world-model, world-model.md at adbf8c7), O8 and O9 (@engine-architect,
m90e54e5d332d), O10 (@iter-bench, me99701e9972d), O12 (`Actor::Agent { harness, principal, requested_by }`,
world-model.md r8 at 7730167), O11 (captures, @ballpit-viz, mcd68d4325ea7), O6b (rotations: y-up, Unity's maths, no mirror at the API; @vdw-physics,
mc38dddbb681e), U4 (root positions, with @world-model and @gnosarium-ux). Still open:

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

In `crates/pe-api` and `crates/pe-mcp`, against mock ports, and in the two-process session through pe-harness:

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
- **The agent's demo script in CI:** "put a red cube on the table", "make the balls blue", "double the cube", "remove
  the left lamp", with a stale case (another member edits the lamp between the agent's query and apply) and a held
  case (the lamp held by hand), and the logic edit, against the two-process session, checked by query and capture on
  both machines.
- **Logic edit:** a source that fails `code_check` is refused as `compile` at submission and makes no entry; a
  committed script's components appear in `world_schema` with a new version.
- **Measured and reported:** apply to `accepted` (target: one frame plus the queue, p99 under 50 ms [I]); apply to the
  other machine's first frame showing it; apply to `final`; query time at 500 and 10,000 objects; capture time.

## 15. Later

- M2: `in_fork` and `fork_*`; `code_test`; per-player fields in the schema; `shader_validate` and `asset_import` once
  their owners exist.
- M3: history from the world record since creation; governance in `session` and the schema.
- The world-line visualization tool any admin can take out (file 19,
  @item:primordial-engine/world-line-visualization-tool-braided-glowing-li) reads branches and merges through
  `history`; its per-player summaries come from pe-world and pe-diverge, not from this API.
- M5: the first-party agent loop and the agent panels call WorldApi in-process; the inspector (`docs/ux/inspector.md`)
  is a WorldApi client with no private path.
