# The inspector: the hierarchy and the object inspector, on WorldApi

Status: **draft 1, a UX proposal.** Written by @gnosarium-ux. The inspector is a client of `WorldApi`
(`docs/spec/api.md`, @api-eng) and calls nothing an agent's tools can't call (`docs/ux/api-surface.md`). Nothing
in it is built. In the plan, it belongs with WS-render's UI lab, which comes after M1 (refound-plan, C12).
Writing it now settles what the API must give it.

Marks: **[R]** a user ruling, cited; **[P]** my proposal; **[A]** to agree with @api-eng.

## 0. On one page

- **Two panes and a history tab, desktop first and plain** [R: review file 08, A3, "design the dense surfaces
  first, plainly, on desktop"; A1, layout separate from skin]:
  - the **hierarchy**: the object tree;
  - the **inspector**: one object's components and fields;
  - **history**: that object's changes.

  In VR the same layout becomes a world-space panel. The Gnosarium look is a skin on top, added later.
- **Everything comes from the schema.** Each field's widget is chosen from its type and annotations in
  `world_schema`: units, ranges, sync mode, protection, read-only. A new component type needs no inspector code
  [P].
- **Editing is a `world_apply` with the version the inspector shows.** If someone changed the field first, the
  player's change is refused with the winner's name and a Re-apply button. Nothing is silently overwritten [R:
  file 11, 3.2; file 14, P4].
- **Live.** The panes follow `world_watch`. A field someone else is changing shows who, as it changes [P].
- **Unsynced fields carry a shape, not a colour** [R: Q12 a]. **Protected objects say so in their header** [R:
  V1]. **Agents' changes are marked in violet,** the colour reserved for agents [R: gnosarium palette, ruled
  2026-10-30 19:21Z].
- **Positions are shown relative to the parent by default.** Absolute world coordinates appear only when the
  player picks "from world origin" on purpose [R: file 17 section 7].

## 1. Layout (desktop)

```
┌ Hierarchy ───────────────┐┌ Inspector ────────────────────────────────────────────┐
│ ⌕ filter                 ││ ▣ Red cube                    o:3f2a…   [⋯]            │
│ ▾ Ball pit               ││   under Table · Alice's agent (Claude Code), 2 min ago │
│   ▸ Floor                ││   ⛨ Protected by Alice: others can't wear, copy, save  │
│   ▸ Walls                ││     or export it in this engine.                       │
│   ▸ Balls (500)          ││ ─ Transform ─────────────────────────────────── [⋯]   │
│   ▾ Table                ││   Position  [ parent ▾ ]  x 0.000  y 0.800  z 0.000 m  │
│     ● Red cube       ◆   ││   Rotation               x 0      y 0      z 0   °     │
│     ● Red cube (copy)◆   ││   Scale                  x 1      y 1      z 1         │
│   ▸ Lamp (right)         ││ ─ Appearance ─────────────────────────────────────    │
│                          ││   Color     ■ #d22b2b                                  │
│                          ││   Glow  ⊚   0.0          (per player)                  │
│                          ││ ─ Body ───────────────────────────────────────────    │
│ [+ Create]               ││   Kind      dynamic ▾    Mass  1.00 kg                 │
│                          ││ [+ Add component]                [History]             │
└──────────────────────────┘└───────────────────────────────────────────────────────┘
```

- `◆` (violet) marks an object an agent changed in the last few minutes. `⊚` is the per-player mark on an unsynced
  field (section 4.2).
- The panes are resizable and dockable, and as large as their content [R: file 08, A1]. Text is at least the UI
  lab's measured legible size in VR.

## 2. The hierarchy

- **What it shows.** The object tree in order, names as written, and a count on folded groups ("Balls (500)").
- **Big groups don't load whole.** A folded node costs nothing; an unfolded one pages its children through
  `world_query` (`child_of`, `limit`, `cursor`) [A: cursors over children].
- **Filter** (`⌕`) uses `world_query`'s selectors: a name, `has: Component`, or "near me". The matches show with
  their ancestors.
- **Drag to reparent:** a `world_apply` `reparent`, keeping the world pose by default. Holding Alt keeps the local
  pose. **Drag with Ctrl** duplicates.
- **Context menu:** Duplicate, Delete, Rename, Add child, Protect / Remove protection (owner only), Copy id, Show
  history.
- **Marks on rows:** the violet agent mark `◆`; `⛨` for protected; a dimmed row for an object outside the
  player's view distance (M4, regions); a faint hand icon while someone holds it.
- **Selection is shared with the world.** Picking a row highlights the object in the world, and picking an object
  in the world (the desktop hand, or a laser in VR) selects its row [P].

## 3. The inspector

### 3.1 The header

- The object's icon, name (click to rename), short id (click to copy the full id), and a `[⋯]` menu.
- **The last change:** "under Table · Alice's agent (Claude Code), 2 min ago", using the producer display label of
  api-surface 6.1. Clicking it opens history at that change.
- **Protection, when present** [R: V1]: "⛨ Protected by Alice: others can't wear, copy, save or export it in this
  engine." For the owner the line adds a **Remove protection** button. The wording states the limit, as Resonite's
  does, because it is a protection of the engine's own paths and not of the data (api-surface 5).

### 3.2 Components and fields

- One fold per component, in the object's order, each with its own `[⋯]` menu: Remove, Reset to default, Copy
  values, Paste values, Show in schema.
- **Widgets come from the schema** [P; the annotations are api-surface 3.2]:

| Schema | Widget |
|---|---|
| number with `x-unit` | a number field with the unit after it; drag to scrub; a slider when a range is given |
| integer | a number field with steps |
| bool | a toggle |
| enum | a drop-down |
| string | a text field; multi-line when long |
| colour (`x-unit: srgb`) | a swatch with a picker; hex shown |
| position (`relative_to`/`offset`) | three fields with a frame selector (3.3) |
| rotation | three angles in degrees; a quaternion on request |
| object reference | the target's name as a link, with a picker and drag-and-drop from the hierarchy |
| blob (mesh, texture, source) | a preview and size, with Replace and (owner, unprotected) Export |
| `x-read-only` | the value, greyed, with "set by the engine" on hover |

- **Editing.** A field commits on Enter, on leaving the field, or at the end of a drag, as one `world_apply`
  naming the version the inspector showed. Scrubbing a number sends one change at the end of the drag, not one
  per pixel. The player sees the value move at once, locally [R: "everything reacts immediately for the local
  user"].
- **Undo** (Ctrl+Z) reverts the player's own last change through `history_revert`, and is refused, with the
  winner, if the field has moved on since [P].

### 3.3 Positions and frames

- Positions show in the **parent's** frame by default, which is Resonite's local position [R: file 17 section 7.3,
  "frames follow Resonite"].
- The frame selector offers: parent; another object (pick it); me; and **from world origin**. The last one shows a
  small note, "absolute: precision falls far from the origin", the first time it is picked in a session [P, a
  player-facing reading of file 17's hard wall; the API echoes the frame too].
- Changing the frame changes only what is shown, never the stored value.

### 3.4 Live values and other people's edits

- The inspector follows the object through `world_watch` with a filter on its id, so its fields update live [P].
- **Physics motion** updates the pose fields at a low rate (about 4 Hz), from the frame view, not from `watch`
  [A: api-surface 3.6 excludes motion from watch].
- **Someone else's edit to a field** flashes the field briefly with their name: "Bob, just now". If the player was
  typing in that field, their text stays, and a small "changed by Bob" note appears beside it. On commit, the
  player's change is refused as stale (3.5), so they decide with both values in view.

### 3.5 A refused change

- A toast under the field: "Your change to Color was refused: Bob changed it first." with **Re-apply** (the same
  change against the new version) and **Show** (history at Bob's change) [P, the same as api-surface 6.3].
- For `not_permitted` and `protected`, the toast gives the reason and no Re-apply.
- The field returns to the shared value with a short reverse fade, so it never seems to jump at random.

## 4. Marks

### 4.1 Agents

- An object an agent changed shows the violet `◆` in the hierarchy, and the inspector's last-change line names the
  agent, for a few minutes after the change. Fields the agent set show a thin violet bar at their left edge for the
  same time [P].
- Hovering the bar: "Set by Alice's agent (Claude Code) · 'Put a red cube on the table' · 2 min ago · Revert".

### 4.2 Unsynced fields [R: Q12 a; design from world-state-sync.md 4.6, settled]

- Unsynced fields carry the **per-player mark** `⊚` (a split ring, two offset rings) beside the field and a broken
  underline, in the skin's gold. Synced fields carry no mark.
- The hover names the field's driver and what it reads, or says "Your own value · remembered" for a remembered
  per-player field (Q11 a).
- **Make per-player** is a field-menu item for builders. For a field the simulation reads it is disabled, with the
  reason: "The simulation reads this field, so it must stay the same for everyone."

### 4.3 Protection

- `⛨` on the hierarchy row and the header line (3.1). Non-owners see Duplicate, Export and "Wear" disabled in
  menus, each with the reason on hover: "Protected by Alice" [R: V1].

## 5. The history tab

- The object's changes, newest first: the `summary` words, the producer display label, the time, and whether it is
  final (a small check when final; "pending" in grey before). A **Revert** button on each line the viewer may
  revert [P; api-surface 6.4].
- An agent transaction is one line with its label, which expands to its operations.
- **Filter:** this object and its subtree / this object only; by member; agent changes only.
- The same list for the whole world is the world history card (api-surface 6.4). The inspector's tab is that list
  filtered to one object.

## 6. In VR (after the UI lab)

- The same two panes on a world-space panel the player can grab, scale and pin. The hierarchy and inspector can be
  split onto two panels [P].
- Picking is by laser from either hand; scrubbing a number is a pinch-and-drag along the field.
- The text size and contrast are the UI lab's to measure [R: file 08, A2].

## 7. What it needs from WorldApi

All of it is in api-surface.md, so the inspector needs no private path [P]:
- `world_schema` with `x-unit`, `x-sync`, `x-protected` and `x-read-only`;
- `world_query` with `child_of`, cursors and per-object versions;
- `world_apply` with `if_version`, `reparent` `keep`, `duplicate` and the protection refusals;
- `world_watch` filtered by object;
- `history_list` filtered by object, and `history_revert`;
- the frame view (S5) for live poses, and the producer display labels.

Open for @api-eng:
1. paging children with a cursor;
2. a per-object `world_watch` filter that is cheap enough to keep open for every selected object.
