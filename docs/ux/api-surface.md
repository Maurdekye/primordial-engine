# The API's surface: how WorldApi and the MCP tools read to an agent and to a player

Status: **draft 1, a UX proposal for WS-api**, aligned with @api-eng's shapes of 19:31Z (mce7a3e3e5bd3). Written by @gnosarium-ux for @api-eng, who owns `pe-api`, `pe-mcp`
and `docs/spec/api.md`. Where this document and `api.md` disagree on a type or a rule, `api.md` wins. This document
covers names, wording, shapes as the agent sees them, defaults, and what players see. Nothing in it is built.

Base: main 926bc9d, the re-founding plan (`docs/refound-plan.md` on `docs/refound-plan`, b610464), the
independent review's files 06, 08, 11, 14 and 17, and the user's rulings in the org's decisions file through
2026-11-10.

Marks:
- **[R]** a user ruling, cited;
- **[P]** my proposal;
- **[A]** to agree with @api-eng (a shape or default that depends on how pe-api is built).

## 0. On one page

- **Tool names use underscores on the wire** (`world_query`, not `world.query`): the Anthropic API accepts only
  `[a-zA-Z0-9_-]` in tool names [V @api-eng, from the old pe-mcp].
- **The tools are few and schema-driven** [R: refound-plan S6; review file 06, A1-A2]. M1 has seven:
  `world_schema`, `world_query`, `world_apply`, `world_watch`, `history_list`, `history_revert` and
  `view_capture`, plus `session_info`. The old design's seventy tools would cost about 35-40k tokens in every
  agent context (review file 06, M4). **The target for the whole M1 tool list is under 5k tokens** [P].
- **The world is live, and the server says so first** [P]. Time runs whether or not the agent calls anything,
  other players act at the same time, and every change shows up for everyone. The old server told agents "Time
  only moves when you call world_step" on a live peer (review file 06, finding 6).
- **`world_apply` returns at once** [P, from the governing principle: "everything reacts immediately for the
  local user"]. It returns once the change is accepted on this machine, with `status: "accepted"`. Ordering and
  finality come later and are reported by `world_watch`, or by `apply` itself when the agent asks it to wait.
  Agent edits apply at once, labelled and revertible [R: coordinator decision C11].
- **A refusal is always explained.** It carries a machine code, one plain sentence, and, for a stale edit, the
  change that won: who made it, through which producer, and when [R: refound-plan M1 acceptance; review file 14,
  P4].
- **Positions are never absolute numbers by accident** [R: file 17 section 7, m06d2643578d7]. Every position the
  API takes or gives is an offset from a named frame. The world origin is a frame an agent can name, but must name
  on purpose.
- **Received avatars and objects are protected the way Resonite protects avatars** [R: V1, mbe87e750ca35].
  Non-owners can't wear, duplicate, save or export them, and can't remove the protection; the owner can. Every
  path to these actions goes through WorldApi, so an MCP agent is refused exactly as a player's hand is.
- **Players see who changed what** [P]. Every change carries its member and a producer label, such as "Alice's
  agent (Claude Code)". Objects an agent changes show a brief cue in the world, and every change can be reverted
  in one step from history.

## 1. Principles

1. **One surface for everyone.** The inspector, the in-game tools, the editor and the MCP server call the same
   `WorldApi`. "Every capability a human has also exists as an MCP tool" is true by construction [R: review file 11,
   3.7, carried by the plan]. The tool names here are also the names the inspector's actions log under, so a
   player reading history and an agent reading history see the same words [P].
2. **The schema is the documentation.** Component types, fields, units, ranges and sync mode come from
   `world_schema`, not from tool descriptions. A tool description says what the tool does and points to the
   schema. Adding a component type adds no tool [P].
3. **Answer the next question in the same response** [P]. A query result carries each object's version (so the
   next edit can name it); an apply result carries the new ids and versions; a refusal carries the winner. An agent
   should seldom need a call just to find out what its last call did.
4. **Small by default, more on request** [P]. Results list ids, names and the fields asked for. Whole objects,
   subtrees and history come only when asked, with limits and cursors.
5. **Plain words** [P]. Descriptions and messages use the words a player would use: "object", "parent",
   "component", "field". No internal protocol vocabulary (cuts, rungs, domains) appears in tool descriptions;
   where an agent needs it, it is explained in one clause.

## 2. The first thing an agent reads

The MCP server's `instructions` string, sent at connection [P; the text is for @api-eng to adopt or edit]:

> You are connected to a live, shared world in the Primordial Engine, as a member of this session.
>
> - Time runs on its own. Other players may be acting right now, and every change you make appears for everyone
>   in the session.
> - Read before you write: use `world_query` to find objects, and `world_schema` for what their components and
>   fields mean.
> - Change the world with `world_apply`: a list of operations applied all or nothing. Use `"mode": "dry_run"` to
>   check a change without making it.
> - An edit names the version of the object you read. If someone changed it since, the edit is refused and the
>   reply says who changed it and how. Read again and decide.
> - Look at your work with `view_capture`.
> - Positions are always relative to something: an object, its parent, you, or the world origin, which you name
>   on purpose. Distances are in metres, angles in degrees.
> - Your changes are signed as {member}, labelled "{producer}", and anyone can see and revert them in the
>   world's history.

`{member}` and `{producer}` are filled in at connection, for example "Alice" and "Alice's agent (Claude Code)".

## 3. The tools in M1

Each entry gives the description the agent sees (kept short; the token budget is in section 3.9), then the
argument and result shapes as the agent sees them in JSON. Types and exact field names are [A].

### 3.1 `session_info`

> Who is in this session, who you are, what you may do, and how current the world is.

Result:
```json
{
  "world": {"id": "w:…", "name": "Ball pit"},
  "me": {"member": "m:…", "name": "Alice", "producer": "Alice's agent (Claude Code)",
         "role": "Admin", "may": ["edit", "create", "delete", "revert_others"]},
  "members": [{"member": "m:…", "name": "Bob", "role": "Admin", "here": true}],
  "frame": 18342,
  "final_frame": 18334,
  "link": "fine"
}
```
- `link` uses the player-facing words of review file 08, A6: `fine`, `thin`, `some actions dropped`,
  `catching up`, `watching only` [P].
- `may` lists permissions in the role system's own words, so an agent can tell in advance what will be refused
  [A, with WS-gov from M3; in M1 everyone is an admin of the demo world].

### 3.2 `world_schema`

> The component types in this world: their fields, types, units, allowed values and whether each is synced. Ask
> for one type by name, or omit the name for a list.

- Without a name: a list of `{type, summary}` only, so the call stays small [P].
- With a name: a JSON Schema per field, plus these annotations, as `x-` keys so standard validators ignore them
  [P]:
  - `x-unit`: `m`, `deg`, `kg`, `s`, `srgb`, and so on;
  - `x-sync`: `synced` or `unsynced` (the user's dichotomy, Q10 a). Unsynced fields are shown in the inspector
    with a shape, not a colour (Q12 a);
  - `x-protected`: the field is part of a protection (section 5);
  - `x-read-only`: the engine writes it, for example a body's simulated velocity.
- Schemas carry no non-standard JSON Schema `format` values. The old server had to strip schemars' `uint32` and
  `uint64` because Claude Code's validator warned on every tool [V design.md 17.5]; keep that rule.

### 3.3 `world_query`

> Find objects. Select by id, name, component type, or nearness to another object, and choose which fields to
> return. Results include each object's version, which an edit names.

Arguments [P; the selector grammar is A]:
```json
{
  "where": {"name": "Table"} ,
  "fields": ["Transform", "Appearance.color"],
  "children": 0,
  "limit": 50,
  "cursor": null
}
```
- `where` takes one of:
  - `{"id": "o:…"}`;
  - `{"name": "Lamp"}`, matching a name exactly, or `{"name_like": "lamp"}`, case-insensitive;
  - `{"has": "Ball"}`, objects with that component;
  - `{"near": {"of": "o:…", "within_m": 2}}`;
  - `{"child_of": "o:…"}`;
  - `{"all": [ … ]}`, the conjunction of these.
- `fields` takes component names, or `Component.field`. Leaving it out returns names, ids, parents and versions
  only.
- `children`: how many levels of children to include (default 0).

Result:
```json
{
  "frame": 18342,
  "objects": [
    {"id": "o:…", "name": "Table", "parent": "o:…", "version": "v:…",
     "components": {"Transform": {"position": {"relative_to": "parent", "offset": [0, 0, 1.5]},
                                  "rotation_deg": [0, 90, 0], "scale": [1, 1, 1]}}}
  ],
  "more": "c:…"
}
```
- `version` changes whenever the object's own fields change [A: per object, or per component? Per object is
  simpler to explain; per component refuses fewer edits].
- `more` is a cursor when the limit cut the list; it is absent otherwise.
- A query reads the newest published snapshot. It never waits on the simulation [R: plan S6].

### 3.4 `world_apply`

> Change the world: a list of operations, applied all or nothing. Each edit names the version of the object you
> read; if someone has changed it since, the whole change is refused and the reply says who changed it. Use
> "dry_run" to check a change without making it.

Arguments:
```json
{
  "label": "Put a red cube on the table",
  "mode": "live",
  "wait": "local",
  "ops": [
    {"op": "create", "as": "$cube", "parent": "o:table", "name": "Red cube",
     "components": {"Transform": {"position": {"relative_to": "parent", "offset": [0, 0.8, 0]}},
                    "Shape": {"box_m": [0.2, 0.2, 0.2]},
                    "Appearance": {"color": "#d22b2b"},
                    "Body": {"kind": "dynamic", "mass_kg": 1}}},
    {"op": "set", "id": "o:lamp-left", "if_version": "v:…", "field": "Light.intensity", "value": 0.5}
  ]
}
```

**The operations** [P names; R for the set: review file 11, 3.2, "create, delete, set a field, reparent"]:

| op | Does | Notes |
|---|---|---|
| `create` | Creates an object with components under a parent | `as: "$name"` lets later ops in the same change refer to it |
| `code` | The logic edit the user added to M1 (U1, mc40fc6379bc6) | Its shape is WS-logic's and @api-eng's; the same envelope, statuses and refusals apply [A] |
| `set` | Sets one field, or several with `fields: {…}` | Takes a value checked against the schema |
| `add` / `remove` | Adds or removes a component | |
| `reparent` | Moves an object under a new parent, at an index | `keep: "world_pose"` (default) or `"local_pose"` |
| `duplicate` | Copies an object and its subtree | New ids over the same records (file 11, 3.2); refused on a protected object for a non-owner (section 5) |
| `delete` | Deletes an object and its subtree | |

**Many objects at once** [P]. "Make the balls blue" is one op, not 500:
```json
{"op": "set", "where": {"has": "Ball"}, "field": "Appearance.color", "value": "#2b5bd2", "max": 1000}
```
- The selector is expanded on this machine, against the snapshot the agent's change is made on, into explicit
  ids. The entry that travels carries the ids, never the selector, so every machine applies exactly the same
  change [A: this keeps the edit deterministic whatever the snapshot timing].
- `max` is required with `where`, as a guard against changing the whole world by mistake. The result reports the
  count.
- A selector edit can't name versions. It is a plain write: the latest write in the agreed order wins (review
  file 11, 3.2: "plain writes, the latest in the agreed order wins").

**Versions and the three conflict rules** [R: review file 11, 3.2]:
- **Edits with a precondition.** `if_version` names the version the agent read. If that version has changed by
  the edit's frame, the whole change is refused, with the winner.
- **Plain writes.** Without `if_version`, the latest write in the agreed order wins.
- **Exclusive holds** (grabs) are hands' business, not the API's in M1.
- **The default** [P, A]: the server fills in `if_version` from the agent's own last read of that object in this
  connection, unless the op says `"if_version": "any"`. Agents then get the safe rule without having to copy
  versions about, and a plain write stays one word away. This is the main default to agree with @api-eng.

**`mode`:**
- `live` (default): make the change.
- `dry_run`: check everything a live change would check: the schema, permissions, protections and versions. The
  reply is what a live change would have produced, and nothing is changed or sent. It says it is a dry run in its
  first field [P], so a transcript can never mistake one for the other.
- `in_fork`: M2, with `fork_*`.

**`wait`** [P]:
- `local` (default): return once the change is accepted on this machine. Others see it within about one network
  delay.
- `final`: return once it is final, or with `status: "pending"` after `timeout_ms` (default 2,000, at most
  10,000). It never blocks the simulation.

Result:
```json
{
  "status": "accepted",
  "change": "x:…",
  "frame": 18350,
  "created": {"$cube": {"id": "o:…", "version": "v:…"}},
  "changed": [{"id": "o:lamp-left", "version": "v:…"}],
  "count": 2
}
```

**The life of a change** [P words; R semantics from the plan and file 14]:

| `status` | Meaning | Where the agent hears it |
|---|---|---|
| `accepted` | Applied here at once (the ticket and the frame it was stamped); others see it within about one network delay | `apply` |
| `applied` | Ordered among everyone's changes and applied by the rules | `world_watch`, or `apply` with `wait: "final"` |
| `final` | In the agreed history for good | `apply` with `wait: "final"`, or `world_watch` |
| `refused` | Refused by the rules at its frame, on every machine alike | `apply` (checked here first), or `world_watch` if the conflict arrived later |
| `dropped` | Arrived too late to be included, and was undone | `world_watch` |
| `pending` | `wait: "final"` timed out before finality | `apply` |

An accepted change can still be refused when a conflicting change from another machine turns out to be earlier.
`world_watch` then reports it with the same refusal envelope, and the agent's change is undone on every machine
[R: soonest wins].

### 3.5 Refusals

One envelope for every refusal, from `apply` or `watch`. It is @api-eng's error envelope `{code, message,
field?, details?}`, returned with `isError: true`; `winner` and `next` below sit inside `details` [A]. Shown here
flattened for reading [P]:
```json
{
  "status": "refused",
  "change": "x:…",
  "code": "stale_version",
  "message": "Bob moved Lamp (left) after you read it.",
  "op": 1,
  "object": {"id": "o:lamp-left", "name": "Lamp (left)"},
  "winner": {"change": "x:…", "by": "Bob", "producer": "Bob (by hand)", "frame": 18347,
             "label": "Moved Lamp (left)"},
  "next": "Read Lamp (left) again with world_query and decide whether your change still applies."
}
```

| `code` | Plain message, as a template | `next` |
|---|---|---|
| `stale_version` | "{winner.by} changed {object} after you read it." | Read again and decide |
| `not_found` | "{object} no longer exists." (with the deleting change as `winner` when known) | Query again |
| `not_permitted` | "Your role ({role}) can't {action}." | Ask an admin, or don't |
| `protected` | "{object} is protected by {owner}. Only they can {action} it." | None; don't retry |
| `invalid` | "{field} must be {rule}; you gave {value}." | Fix the value (the schema has the rule) |
| `in_change` | "Op {n} refers to {ref}, which this change doesn't create." | Fix the change |
| `too_many` | "The selector matched {count} objects; `max` is {max}." | Narrow it, or raise `max` |
| `rate_limited` | "You're sending changes faster than {n} a second." | Retry after `retry_after_ms` |
| `dropped` | "The change reached the others too late and was undone." | Apply again if it still makes sense |

- `message` is one sentence, in the player's words, safe to show a player as it stands [P]. The engine writes it;
  it never repeats a string another peer wrote without quoting it (review file 06, low: "one peer-authored string
  reaches the agent unwrapped").
- `next` tells an agent what to do; a person's interface shows its own buttons instead (section 6.3).

### 3.6 `world_watch`

> What changed since your cursor: objects created, deleted and changed, by whom, and changes of yours that became
> final, were refused or were dropped. Filter by object or component. Waits up to wait_ms for something to happen.

Arguments: `{"cursor": "c:…", "where": {…}, "wait_ms": 5000, "limit": 200}`.
- Without a cursor, it returns a cursor for "now" and nothing else.
- `wait_ms` is capped at 30,000 [P]. A long poll is how an agent waits for something without burning calls.
- `where` uses `world_query`'s selector.

Result: `{"cursor": "c:…", "changes": [ … ], "mine": [ … ], "more": false}`.
- `changes`: `{"change", "by", "producer", "label", "frame", "final", "created": […], "deleted": […],
  "set": [{"id", "field", "value"}]}`. Values are included only for the fields the filter names [P: otherwise
  500 moving balls flood the agent].
- `mine`: the status changes of the agent's own changes (`final`, `refused`, `dropped`), with the refusal
  envelope.
- **Physics motion is not a change** [P, A]. Bodies moving under simulation don't appear in `changes`, or every
  watch would return 500 balls. An agent that wants motion asks `world_query` for poses.

### 3.7 `history_list` and `history_revert`

> `history_list`: the world's changes, newest first, filtered by object, member or time. Each says who made it,
> through what, and what it did.
>
> `history_revert`: undo one change, by making a new change that puts back what it changed. It is refused if
> those fields have changed again since.

- An entry: `{"change", "by", "producer", "requested_by"?, "label", "frame", "final", "summary"}`.
  - `summary` is generated by the engine, for example "Created Red cube under Table".
  - `requested_by` names a guest who prompted an owner's agent [R: design 17.4, carried; A1 ruling].
- A revert is an ordinary change, with its own entry, label ("Revert: Put a red cube on the table") and
  attribution. It uses preconditions: if a field it would restore has changed since, the revert is refused with
  the winner [P]. That way a revert never silently overwrites someone's later work.
- Reverting someone else's change needs the permission to edit what it touched. There is no separate "revert"
  right in M1 [P].
- Saves, backups and world-line merges (M2-M3) appear in the same history as named entries [P, A with WS-diverge].

### 3.8 `view_capture`

> An image of the world, from a player's view or from a camera you place. Optionally highlights objects and lists
> which objects are visible and where. Use it to check your work.

Arguments [P]:
```json
{
  "from": {"player": "me"},
  "size": [768, 432],
  "highlight": ["o:…"],
  "label": true,
  "visible_list": true
}
```
- `from` takes one of:
  - `{"player": "me"}` or `{"player": "Bob"}`: that player's current head view, drawn on this machine from the
    shared world. It shows only what this machine would draw; an unsynced object of Bob's doesn't appear [R: Q13 a,
    an unsynced engaged object is never visible to others].
  - `{"orbit": "o:…", "yaw_deg": 30, "pitch_deg": 20, "distance_m": 3}`: a camera looking at an object.
  - `{"at": {"relative_to": "o:…", "offset": [x, y, z]}, "look_at": "o:…"}`: a camera placed exactly.
- `size`: default 768 × 432, at most 1,920 × 1,080. A small default keeps the image cheap in an agent's context
  [P].
- `highlight`: draws an outline on these objects. `label: true` writes each highlighted object's name beside it.
- `visible_list`: returns `[{"id", "name", "rect": [x, y, w, h]}]` for the objects drawn, largest first, up to 50.
  This ties what the agent sees to ids it can edit [P].

Result: the image as MCP image content (PNG), plus
`{"frame", "camera": {"at", "look_at", "fov_deg"}, "visible": […]}`. The image is drawn from the published frame
view (S5) by pe-render's offscreen capture [R: plan WS-render, "offscreen capture"].

### 3.9 The token budget

- **Target:** the whole M1 tool list (names, descriptions and argument schemas) under 5,000 tokens. Measure it
  with the tokenizer of each harness the plan tests (Claude Code first) and record it in `api.md` [P].
- **How:** descriptions of two or three sentences; selectors and field shapes defined once in the schema and
  referenced; no examples inside schemas (examples live in this document and in `world_schema`'s output).

## 4. Positions, rotations, colours and units

- **Positions are offsets from a named frame** [R: file 17 section 7: a hard wall on absolute coordinates; absolute
  numbers only by subtracting the world origin, which is a world constant; frames follow Resonite]:
  `{"relative_to": <frame>, "offset": [x, y, z]}` in metres. The frames:
  - `"parent"`: the object's parent's space. This is Resonite's local position, and the default for `Transform`;
  - an object id: that object's space;
  - `"me"`: the agent's member's head (the player it acts for);
  - `"world_origin"`: the world's origin. Allowed, but never a default, and the result echoes it back, so a
    transcript shows when an agent worked in absolute coordinates.
- A result gives positions in the frame the query asked for (`"frame": "parent"` by default) [P].
- **Rotations:** `rotation_deg: [x, y, z]`, Euler angles in degrees in a stated order, both accepted and returned.
  A quaternion `rotation_quat: [x, y, z, w]` is accepted, and returned when asked [P; the order is A].
- **Colours:** `"#rrggbb"` sRGB, or `{"srgb": [r, g, b, a]}` as 0-1 floats [P].
- **Scale:** a multiplier, `[1, 1, 1]`. The user's unbounded player scale (2026-10-30, R16 addendum) means "1 m"
  is the world's metre, not the player's. `session_info` gives the player's own scale [P].

## 5. Protections for received avatars and objects (V1)

The user's ruling (2026-11-10, mbe87e750ca35): "received avatars and objects get simple Resonite-style
protections". It replaced the strict view-only scheme in the legal research report.

**What Resonite does** [V https://wiki.resonite.com/Component:SimpleAvatarProtection, edited 2026-09-16]: the
component "prevents all users, other than the single user listed on the component" from equipping, saving or
exporting the avatar, or grabbing its materials with the Material Tool. Only the listed user can remove it. The
page also says it "does not guarantee meshes, textures, etc. can't be grabbed through other methods, such as
using an inspector".

**The proposal** [P, A]:
- **A `Protection` component** on an object protects that object and its subtree. Its field `owner` names one
  player identity (the player's long-term key, not a session member id). It is synced and hashed like any field.
- **For anyone but the owner, WorldApi refuses** with `code: "protected"`:
  - wearing it as an avatar;
  - `duplicate`;
  - saving it to their inventory, or out of the world into a package;
  - export (`asset.export`, when it exists);
  - fetching its meshes and textures for any purpose but drawing them (the asset tools, later; not the renderer);
  - `remove` of the `Protection` component, and `set` of its `owner`.
- **Ordinary editing is not blocked** [P, an edge case for the user]: moving, re-colouring or deleting a protected
  object stays governed by role permissions, as for any object (Q33, editing others' objects allowed by default).
  Resonite's component protects copying, not editing. If the user wants "only the owner may edit it" too, that is
  a separate flag.
- **When it is added:**
  - automatically, to every avatar a player imports or wears, with that player as owner [P: Resonite's avatar
    creator does the same in practice; the user can turn it off per avatar];
  - by choice, to any object, through the inspector or `world_apply` (`add`), by its owner.
- **The owner's own agent acts as the owner.** An agent connected as Alice may duplicate Alice's protected
  objects.
- **Honest limit, stated in the inspector** [P]: like Resonite's, the protection stops the engine's own paths, not
  a modified client, because every peer has to draw the avatar and Gnosarium is open source. The inspector's
  wording: "Protected by Alice: others can't wear, copy, save or export it in this engine."
- The inspector shows a protected object's fields as usual, with the protection shown on the object's header
  (docs/ux/inspector.md).

## 6. What players see

### 6.1 Attribution: who did it, and through what

- Every change records its **member** (who signed it) and a **producer** label [R: review file 06, A3, carried by
  the plan: member-key signing plus a producer label]. The spec's label is machine-readable (for example
  `agent:claude via member A`, @api-eng); the engine turns it into the display words below [P]:
  - "by hand", for a person's own direct actions: hands, inspector, tools;
  - "{owner}'s agent ({harness})", for example "Alice's agent (Claude Code)";
  - "{owner}'s agent ({harness}), asked by {guest}", for a guest at an unlocked panel. The guest is
    `requested_by` [R: design 17.4; A1].
- The label is written by the engine from the connection, never by the agent. An agent can't claim to be "by
  hand" [P].
- Players read it in the history card and in the inspector's history tab. A modified client could lie about its
  own producer, so the label is an honest client's statement, not a proof (review file 06, A3, stated loss).

### 6.2 Agent edits in the world

- **A brief cue on what changed** [P]: an object an agent creates or changes shows a soft violet outline pulse for
  about 1.5 s, with a small tag carrying the producer label. Violet is the colour reserved for anything belonging
  to an AI agent [R: gnosarium palette, 2026-10-30 19:21Z]. Seen by everyone nearby, in keeping with "openness over
  privacy" (decisions, 2026-10-30 22:28Z: nearby players can watch agents work in real time). Each player can turn
  the cue off.
- **Selector edits** ("make the balls blue") show one tag at the group's centre, "Alice's agent: 500 balls", not
  500 tags.
- A change that is later refused or dropped plays the reverse cue briefly on the objects that revert, so nobody is
  left wondering why something changed back [P, from review file 14, P4, "a refused edit shows…"].

### 6.3 A person's refused edit

- The inspector's toast [P, from file 14, P4]: "Your change to Lamp (left) was refused: Bob moved it first."
  with two buttons, **Re-apply** (the same change against the new version, as a new change) and **Show** (selects
  the object and opens its history at the winning change).
- A lost grab is the hands' cue (WS-render), not a toast [R: plan M1 demo, step 4].

### 6.4 History and undo

- Every world has a history card: newest first, one line per change, in the `summary` words, with the member,
  producer and a **Revert** button where the viewer may revert it [P, from review file 08, A6, "a session history
  card with the reversing action"].
- An agent's transaction is one line, labelled with its `label` ("Put a red cube on the table"). It expands to the
  operations.

## 7. The M1 demo, as the agent sees it

The plan's step 5: "put a red cube on the table", "make the balls blue", "double the cube", "remove the left lamp".

1. `world_query {"where": {"name": "Table"}, "fields": ["Transform", "Shape"]}` returns the table, its version, its
   height.
2. `world_apply` with one `create` under the table at `offset [0, 0.8, 0]` (from the table's shape), red, dynamic.
   Returns `accepted`, the cube's id and version.
3. `view_capture {"from": {"orbit": "o:cube"}, "highlight": ["o:cube"]}` to check it.
4. `world_apply` with one `set` where `has: "Ball"`, `max: 1000`, `Appearance.color` blue. Returns `count: 500`.
5. `world_apply` with `duplicate` of the cube, then a `set` of the copy's `Transform` 0.3 m to the side, both in
   one change (`as: "$copy"`).
6. `world_query {"where": {"name_like": "lamp"}}` returns two lamps; the agent picks the left one by its position
   relative to `"me"`, and `world_apply` `delete`s it.
7. The stale-edit test (plan M1 acceptance): Bob moves the left lamp by hand between steps 6's query and apply. The
   apply is refused with `stale_version`, winner "Bob (by hand)", and the agent reads again.

## 8. Later surfaces this API must leave room for

- **M2:** `fork.create` / `fork.apply` / `fork.step` / `fork.drop`, `code.check`, `code.test`, `shader.validate`,
  `asset.import`. The same envelope, the same refusal codes, the same producer labels [R: plan WS-api M2].
- **Agent panels (M5)** [R: A1 and A2, mbe87e750ca35; plan section 5]:
  - **A1, the unlock warning** for a panel on a subscription login, shown before unlocking and as a line on the
    unlocked panel [P wording]: "This panel uses your {provider} subscription. Letting others prompt it is against
    {provider}'s terms, and they may suspend your account. You take that risk." The panel stays usable, and the
    warning can't be turned off for that panel while it is unlocked.
  - **A2, Google models:** the same pattern [P wording]: "Google's terms don't allow its models in apps used by
    people under 18. You take that risk."
  - Both name the clause and link the research report's section, so a player can read the source.
- **The inspector** (docs/ux/inspector.md, next) is a schema-driven view over `world_query`, `world_apply`,
  `world_watch` and `history_*`. It needs nothing the agent's tools don't have.

## 9. Open points for @api-eng

1. The default `if_version` (filled from the agent's last read) versus requiring agents to pass it (3.4).
2. A version per object or per component (3.3).
3. Whether physics motion is excluded from `world_watch` (3.6), and how an agent asks for motion.
4. The selector grammar's first set (3.3), and its expansion into ids on this machine (3.4).
5. The refusal codes as an enum in `pe-seams` or `pe-api` (3.5).
6. Where the producer label is set: at the MCP connection (my proposal) or per call.
7. Whether `view_capture` with `{"player": "Bob"}` is allowed, or only `"me"` and placed cameras. It shows the
   shared world as Bob's machine would draw it, nothing private (3.8). Allowing it is in keeping with "openness
   over privacy".
