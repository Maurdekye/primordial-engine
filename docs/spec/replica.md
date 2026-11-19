# Replica: pe-replica, pe-physics and the Box3D fork (WS-state)

Status: v1.0, 2026-11-17, @vdw-physics. This version describes the replica as built on main c9e2829a; 8's and 8.2's
notes on the hello were brought up to date when the hello's runner comparison landed (@engine-architect). v0.2
(2026-11-10, branch ws/state cb8826d9) was the design before pe-replica existed, and it never landed. The section
numbers are v0.2's, because code on main cites them; what changed is the text under them. A design that is not built
is marked **Not built**.

The seams are pe-seams's: S2 (the replica) through v1.8 and S5 (the frame view) through v1.8, with their rulings in
`crates/pe-seams/RULINGS.md`. Where this file and a seam's doc comment differ, the seam is right, and this file is to
fix.

Aligned with, on main:
- @world-model's `docs/spec/world-model.md`: its components (3.2), hash tree and import (6.2-6.6), claims (7.2),
  hands and pushes (8, 8.1), and the `replica` calls (9.2). Its 8.1 is the authority for the hands' speed cap and the
  hold bound, which 3.3 summarizes.
- @iter-bench's `docs/spec/logic.md`: the stage order and logic's effects (3.5, 9.2a), and the script binding and
  activation (6.2, 6.2a, 10.1).
- @diverge-eng's `docs/spec/divergence.md`: physics is repaired by restoring the whole image (4.2a, R-H1).
- @engine-architect's `docs/spec/app.md` 3: the driver of each simulation tick (8).
- @api-eng's `docs/spec/api.md` 9.1: what pe-api reads from the replica (8).
- @coordinator-opus's rulings on WS-diverge 15: deterministic merges are enough; untouched groups
  need not be bit-exact.

Marks: **[V]** verified on main c9e2829a, or in the record named; **[I]** design or inference.

## 1. What the replica is

- **One domain's deterministic state machine, with its rollback loop.** There is one domain per world until M4, but
  every seam carries a domain id from the start (plan, S1).
- **Inputs:** the domain's entries in agreed order, from S3's `EntrySource`, and the newest final cut.
- **Outputs:**
  - each entry's outcome;
  - the root hash at cut frames;
  - the frame view (S5);
  - the exports that repair and joining need (8);
  - read-only snapshots of the live state and of the agreed state `Cv`, for pe-api (8);
  - the frames at which this machine must hold its votes (`vote_holds`, 8).
- **It never:**
  - decides finality, order or voids (pe-agree does that);
  - reads a clock in the state path;
  - waits for the network;
  - aborts on input.
- **It owns:**
  - the order of work inside a frame (3);
  - building bodies from world-model components, and the hands' bodies (3.2, 3.3);
  - keyframes and rollback (4);
  - the physics part of the root (5);
  - repair rung 1 (6.1);
  - the physics part of rung 2 and of joining (6.2);
  - the logic stages' place in the frame, run through a runner it is given (3, steps 3 and 8);
  - what the frame view contains (7).
- **It does not own:**
  - the store, the edit and conflict rules, governance rules and the hash tree (WS-world);
  - agreement and voids (WS-agree);
  - the repair ladder's policy and the fault bundle (WS-diverge);
  - building, checking and attesting scripts (WS-logic and pe-app's build service);
  - drawing and blending (WS-render).

## 2. State

A replica's state at a frame is everything the next step reads:

1. **The world model** (pe-world). It holds:
   - objects, components and blobs;
   - governance;
   - claims and push windows;
   - the `Hand` rows: each hand's mode, grip and engaged flag, its input's span, and the input before it. A hand's
     target is its object's `Transform`, which its inputs write;
   - the modules table: one row per script, naming its module, its version and its component ids, with the frame it
     activates at (logic.md 6.2);
   - the world's settings: its rate, gravity and physics origin (world-model.md 6.6).

   pe-world's tree hashes it.
2. **The physics world** (pe-physics over the fork): Box3D's whole world. That means:
   - bodies and shapes, the hands' bodies among them (3.3);
   - contacts with their manifolds and warm starts;
   - the constraint graph's colours, islands and solver sets;
   - sleep timers, the broad-phase trees, joints (the grab joints among them) and the id pools.

   It is hashed as one canonical image (5.2).
3. **Nothing else.** Counters, timings, keyframes, held roots, caches, the frame view and the runner's compiled
   modules are not state. None of them may change a result.

**Invariant R1:** the physics world's body table is a function of the world model. Exactly the physical objects (2.1)
and the physical hands (3.3) have bodies, keyed by object id. Every frame's step rebuilds the hands' bodies from the
rows (3.3), and a rollback restores the image they are in, so the property holds by construction. **Not built:** a
run-time check of it, which v0.2 asked of debug builds [V: no such check in pe-replica].

### 2.1 Physical objects: the components the replica reads

These are WS-world's components (world-model.md 3.2) [V `crates/pe-world/src/components.rs`]. WS-world owns the
schema, and WS-state owns what the components mean to the physics.

| Component | Who writes it | What the replica does with it |
|---|---|---|
| `Shape` | edits | One shape of the body: a box, sphere or capsule with its sizes, in the object's local frame, and its density in kg per cubic metre |
| `Body` | edits | The body's kind (static, kinematic or dynamic), friction, restitution, linear and angular damping, rolling resistance, gravity scale, and the bullet, may-sleep and grabbable flags |
| `Transform` | edits; pe-replica writes back the simulated pose | Read when a body is built or teleported; written after every step for each body that moved |
| `BodyState` | pe-replica only, at cut frames | Derived, never read by the step (2.3) |
| `Hand` | world logic creates the hand; pe-world writes the row from its owner's inputs | The hand's body and drive (3.3). A hand object has no `Body`: its body is the replica's own |
| Claims | pe-world's rule 1 (world-model.md 7.2) | Mirrored as grab joints (3.3) |

The `Body` fields v0.2 asked for are pe-world's now, and a `Shape` carries its density, so no mass is converted
[V `components.rs`, `Body` and `Shape`].

**Compound bodies** are built [V `crates/pe-replica/src/peworld.rs`, its "Shapes" note]: a body's shapes are its own
`Shape`, then those of its compound children (the descendants reached without passing another `Body`) in id order.
Each is placed in the body's frame by composing the local transforms from the body down to it, and scaled by its
object's world scale. pe-physics takes at most 64 shapes on one body [V `crates/pe-physics/src/desc.rs`,
`MAX_SHAPES_PER_BODY`].

Rules for shapes, as built [V `peworld.rs`]:
- **Boxes scale per axis.**
- **A sphere or capsule under non-uniform scale, a mesh, a hull, or an unknown kind cannot be built yet,** and the
  object has no body. pe-world's shape kinds are box, sphere, capsule, mesh and hull; there is no plane [V
  `components.rs`]. Meshes and hulls are **not built**.
- **Such an object is still physical, and each attempt to build it is a fault** (was O12; [V `peworld.rs`,
  `bodies.rs` and tests/peworld.rs, added after c9e2829a]). So is an unknown body kind, or a body with no world pose.
  `WorldModel::body` says why (`Unbuildable`: `Kind`, `Shape` or `Pose`), and `physical()` lists exactly the objects
  whose `body` is not `Ok(None)`. The replica tries at its start, when an entry or a logic stage makes such an object
  physical (`Created`), and when a body it had can no longer be built (`Rebuilt`, which removes the body); one that
  stays unbuildable counts no more. The physics is what it would be without the count: the object has no body.

Bodies are simulated in the domain's physics frame: pe-physics takes positions as `pos - origin`, the form
world-model.md 4.2 allows. The origin is the world's setting (world-model.md 6.6), which every physics world the
replica builds reads, at its start and at each import (6.2). A pose further than pe-physics's `MAX_POSITION`, 10^6 m
[V `desc.rs`], from the origin is not written back (3, step 7). M4 moves the origin per domain.

### 2.2 Ids

- **Object ids are 128 bits,** and pe-physics keys bodies by them [V `crates/pe-core/src/ids.rs`, `ObjectId`]. Its
  image and transfer formats carry 16-byte ids: `IMAGE_VERSION` 3 and `TRANSFER_VERSION` 2 [V
  `crates/pe-physics/src/image.rs`].
- **Joint keys** are the caller's to choose. A grab's joint is keyed from the held object's id, which one claim at a
  time holds (3.3) [V `crates/pe-replica/src/hands.rs`].

### 2.3 `BodyState` is derived

- **pe-replica writes it for every bound body at each cut frame,** before the root is taken (3, step 10). It holds
  the linear and angular velocity, the awake flag, the sleep timer, and the body's physics digest (5.1) [V
  `crates/pe-replica/src/replica.rs`, `physics_rows`].
- **A hand's body has no row:** the world model has no `Body` for it. `physics_global` still covers it (5.1).
- **The step never reads it, and no edit writes it.** The physics world is the authority for velocities and sleep.
  So a keyframe whose `BodyState` is a few frames old restores correctly, and re-simulating across a cut writes it
  again.
- An agent that wants to change a velocity uses a push input, or an edit op that pe-world hands to the replica as a
  physics change. Never a `BodyState` write.
- **`BodyState` is for the hash only.** Readers take live poses and velocities from the frame view (7) or the
  world model's snapshot (8), never from `BodyState` (pe-seams RULINGS.md 10).

## 3. One frame

Every replica runs exactly this for frame f: `ReplicaImpl::step` [V `crates/pe-replica/src/replica.rs`]. It is the
domain's history function, and the one path for every step: live, in a re-simulation, in rung 1's replay, and in the
re-step for a root at a cut. The stages follow pe-seams's `Stage::ORDER`: entries, logic before the physics, the
physics, logic after the physics, events.

A world with scripts needs a runner: a replica built without one does not step a frame of a world that has module
rows. Nothing changes, and each frame it stops at counts one fault however often an advance meets it there (once for
the live history and once for a re-step off it); it never steps such a world as if it had no scripts (`crate::logic`;
8.2). Otherwise:

1. **The floating-point guard:** `pe_core::fpenv::check_and_reset()` on the simulation thread. A changed environment
   is reset and counted (`fp_resets` in the view's counters), and the step goes on. The environment is per thread:
   pe-logic calls the same function at every entry into a guest (@iter-bench's N11), and its resets come back with the
   stage's result into the same counter.
2. **The entries of frame f, in apply order:** by the world model's phase, then agreed order (`apply_order`). pe-world's
   phase is its `input_phase`, so a frame's hand inputs apply before its other entries, and a grab's reach is measured
   after the frame's hand movement (world-model.md 8.1). The replica never re-sorts entries within a phase.
   - **pe-world decodes and applies every entry** (world-model.md 9.2) [V `crates/pe-replica/src/peworld.rs`]:
     - edits through `World::apply_edit`, the one edit path: all or nothing, with preconditions and the three
       conflict rules;
     - inputs through `World::apply_input`: the `Hand` rows, grabs and claims, pushes, own hands only;
     - a `Leave` (a Protocol entry, `pe_seams::LEAVE`) through `World::leave` at its stamp;
     - other Governance and Protocol entries are `Applied` with no effects, as pe-world applies none yet.
   - **Each entry has one outcome,** pe-world's: applied, or refused with a reason. A refused entry changes nothing,
     the same way on every replica.
   - **With each applied entry, the replica gets its physics effects** (`Effects`, 8.1): the objects whose
     physics-relevant rows changed, and the pushes. PeWorld derives them from pe-world's footprint by comparing each
     affected body's row before and after the entry.
   - **The replica acts on them at once, in entry order,** before the next entry: it reconciles bodies (3.2) and
     applies the pushes through pe-physics's bounded `push`. So an object created by one entry can be pushed by the
     next.
3. **Logic, before the physics** (`Stage::LogicPrePhysics`): the runner runs the world's active scripts over the
   store (`World::logic_stage`; logic.md 6.2, 9.2a). The physics effects of what they changed are reconciled as an
   entry's are, and the events they emit become the frame's script effects (7). This stage starts the frame for the
   runner: modules that arrived since the last frame are taken here, so both stages of a frame see the same modules
   (8.2).
4. **Drives** (3.3): each hand's body is rebuilt from its `Hand` row and the physics state, driven towards its
   target, and the grab joints are made to mirror the claims (`hands::sync`). It also gives each hold's bound for
   this step.
5. **The physics step:** `PhysicsWorld::step_holding(dt, reaches)`, with dt one over the world's tick rate and
   ceil(240 / rate) sub-steps (3.5). The holds' bounds move their joints' anchors for this step only (3.3). One
   worker in M1; the shared pool comes in M2 (D2).
6. **Carried checks** (3.4): **not built.**
7. **Write-back:** the pose of every body that moved (awake before or after the step), in id order, into its
   `Transform`, in one batch (`World::write_poses`), so pe-world converts each pose against the frame's final parents.
   Hands' bodies are the replica's own and are not written.
   - A pose pe-world refuses (outside world-model.md 4.3's range), or further than 10^6 m from the physics origin, is
     not written. The body is stopped at its last valid pose, the one still in its row, by `set_state` with zero
     velocity, and the stop is counted (`ReplicaImpl::stopped`). That rule applies at the same frame on every replica,
     so the physics and the world model stay equal (R1) and nothing saturates. **Not built:** an S5 event for it.
8. **Logic, after the physics** (`Stage::LogicPostPhysics`): the scripts see the stepped poses. The replica reconciles
   the stage's effects at once, as in step 3, so its changes are in the physics before the next frame's step. M1's
   scripts read no velocities; in M2, systems that need them get a read-only physics query declared in their
   manifest, not a row (@iter-bench).
9. **End of frame:**
   - pe-world's `maintain_transforms()` repairs any out-of-limit `Transform` that trusted code wrote, and returns how
     many it repaired; each counts as a fault (3.1);
   - `World::end_frame(f)` expires claims and push windows, writes each pending hand pack's pose for f + 1, and
     decides the frame's code commits (logic.md 6.2a): a pending version that a quorum of its voters has attested
     activates, running from f + 1, and one that can no longer activate is voided. A commit never activates in an
     entry's step. The replica keeps those decisions as S5's activation records (7).
10. **At a cut frame f + 1:** the replica writes `BodyState` and `physics_global` (`write_physics`, 5.1), then takes
    the root.
11. **Events and bookkeeping, for a live step only:** the frame's script effects and activation records are kept for
    the view, with the outcomes, the ids of the entries stepped and the root, and a keyframe when the ring's policy
    takes the frame or it is a cut frame (4.1). A step off the live history (a root at a cut computed by re-stepping)
    records nothing (4.2, 5.3).

The frame view is published once an advance has reached the present, with no replay under way, and never during a
re-simulation (7).

### 3.1 No input aborts a replica

- **Every number from an entry is checked before Box3D sees it,** against pe-physics's limits
  (`crates/pe-physics/src/desc.rs`, `limits`): positions, sizes, densities, materials, speeds, damping, pushes,
  gravity and shape counts. A value out of bounds is refused by pe-world or pe-physics.
- **A refused physics call is a fault,** counted and reported (`ReplicaImpl::faults`). It never panics and never
  stops the frame [V `crates/pe-replica/src/bodies.rs`]. So is each attempt to build a physical object the world
  model cannot build (2.1).
- **Internal invariants are checked and reported as faults, not panics.** One of them is the repair count of step 9:
  a repair never passes silently (@chunks-reviewer's review of pe-sim, via @world-model).
- **Not built:** the property test that arbitrary payload bytes and entries from one member never panic a replica
  (P5, 4.3).

### 3.2 Reconciling bodies with the world model

For each applied entry, and for each logic stage, every object in the effects is handled in id order
(`bodies::reconcile`) [V `crates/pe-replica/src/bodies.rs`, `world.rs`'s `Change`]:

| Change | What the replica does to the physics |
|---|---|
| `Created`: a physical object is created, or gains `Body` and `Shape` | `add_body` at its world transform, at rest. One the world model cannot build gets no body, and a fault (2.1) |
| `Removed`: deleted, or it stops being physical | `remove_body`; its joints go with it, and pe-world releases its claims |
| `Rebuilt`: its `Body` or `Shape` fields change, or its world scale does | Removed and added again, keeping the pose and velocities a moving body has in the physics. Its contacts are recreated, the same on every replica. A static body takes its row's pose, which is how an ancestor's move reaches it. One that can no longer be built is removed, and that is a fault (2.1) |
| `Teleported`: an edit sets its `Transform`, or that of an ancestor | `set_state`: exactly the new pose, velocities kept, awake, contact history reset. A static body cannot be moved in place, so it is rebuilt at the new pose |
| `Velocity`: an edit op sets its velocity | `set_velocity` |
| `Reparented` | Its world pose is kept, and nothing changes in the physics |

A body whose shapes and world pose one entry both changed gets `Rebuilt`, then `Teleported`, since a rebuild keeps a
moving body's physics pose. A hand input writes its hand's `Transform` for its own frame, so a body under the hand
moves with it on that frame. A hand pack's later poses are written by `end_frame`, which gives no effects, so such a
body does not follow them yet (@design-reviewer's Medium on e6d3ee9c; 11). Grabs hold by claims, not parenting, and
M1's ball pit puts no body under a hand.

A held object's pose, shape, body and parent cannot change by edit: pe-world refuses those edits with `Held`
(world-model.md 7.2). So no rebuild ever happens under a hand's grab.

### 3.3 Hands, grips and grabs

The rules are world-model.md 8.1's (the speed cap, the teleport distance and the hold bound); the replica's side is
`crates/pe-replica/src/hands.rs`, whose module notes give the detail [V]. In short:

- **A hand** is an object with a `Hand` row, made with its twin by world logic at the member's admission
  (`World::admit`; world-model.md 8). It has no `Body` in the world model: its body is the replica's own, a kinematic
  sphere of radius `HAND_RADIUS` (9 cm) under the hand object's id. It exists only while the hand is physical, and is
  rebuilt every frame from the row and the physics state, so a rollback or a replay repeats it exactly.
- **pe-world applies hand inputs.** It refuses one whose author does not own the hand (`NotYourHand`), and writes the
  hand's `Transform` (its target) and its `Hand` row: grip and the engaged flag, the span, and the input before. As in
  the old packs (D73(a)), one input may carry poses for several frames, and `end_frame` writes each pending one. The
  replica only drives the body from the rows, so a refused input never reaches the physics.
- **When a hand is physical** is its mode (D18b, per member; @coordinator-opus): "closed
  hand", the default, while the hand is engaged; "always within reach" always. A hand that holds something stays
  physical, so its grab has a body to hold by. A hand that is not physical has no body and passes through everything.
  A body that appears is placed at its target, not swept there, and overlapping bodies are pushed out gently by
  Box3D's soft contacts.
- **Within an input's span** the body is driven to its target in one frame by `set_target`, so it pushes what it meets
  at the speed it moves. The move is measured for the hand and for everything it holds, and the farthest point
  decides (world-model.md 8.1, option (b); @coordinator-opus):
  - **faster than the cap,** `MAX_HAND_PUSH_SPEED` (10 m/s, 8.1's `max_push_speed` at its default), the body moves
    only that far this frame. It trails its target and catches up when the hand slows, and a throw leaves at up to
    the cap;
  - **beyond the teleport distance** (0.5 m, or two frames at the cap below 40 Hz), the body is placed at its target
    with no velocity, and each object its claims hold goes with it, keeping its rotation relative to the hand: a
    light hold (`hands::follows_tightly`, its force cap the object's mass's) to its place in the hand, a heavy one at
    its offset, so a placement lifts nothing the strength can't (`hands::placed`; world-model.md 8.1). A
    trailing placement carries a held object only where no static geometry lies in its way
    (`PhysicsWorld::static_in_way`), so a held object never passes through a wall (@coordinator-opus);
    a real teleport carries it through.
- **After the span, with no newer input:** an input that came as part of a stream (within its predecessor's span
  plus 100 ms, `COAST`) coasts at its last velocity, its speed at most 3 m/s (`COAST_SPEED`), until 100 ms after the
  span. Then the body goes back to the target and stays there. Only the frames in the hand's row decide, so a
  re-simulation repeats them (D52). The angular velocity is not capped while coasting (O4).
- **Grabs are pe-world's rule 1** (world-model.md 7.2): the soonest in apply order wins, and a later grab is refused
  naming the holder. Its reach is checked against the hand's position after the frame's hand movement.
- **Grab joints mirror the claims,** after the hands are driven. A claim by a hand with a body, on an object with one,
  gets a grab joint from the hand to the object, keyed from the object's id. It holds the object where it was in the
  hand when the joint was made: a spring pulls it to that place (`grab_spring`: `HOLD_HZ`, 60 Hz, critically damped),
  and an angular drive (`GRAB_TORQUE`, 100 N m) keeps its rotation relative to the hand. A joint whose claim has gone
  is removed. One whose claim a renewing grab moved to another of its author's hands is made again from that hand. An
  agent's grab has no hand, so it gets no joint. The joints are in the physics image, so a rollback restores them.
- **A hold is tight, and its force is capped** (world-model.md 8.1, the tight hold; @coordinator-opus):
  the spring's force is at most `min(strength, m (A + g))`, with the strength the holding hand's `HandRow::strength`
  (pe-world's `Hand.strength`, 110 N by default), `m` the held object's mass when the joint is made and A
  `HOLD_ACCELERATION` (200 m/s^2, provisional). So a light object follows the hand within millimetres, a heavy one lags
  once the hand asks more than the strength, and a blocked light one is never pushed through what blocks it. The
  strength is clamped to 0 to 10,000 N (`MAX_STRENGTH`) before the physics sees it.
- **A hold's spring is bounded** (world-model.md 8.1): for each step it sees at most the error that the room under the
  cap leaves, `(max_push_speed - v_hold) / k`, and at most the error whose speed, `k` times it, the hold can brake
  from, `sqrt(2 a e)`, with `e` the object's distance from its place and `a` the deceleration its force cap gives it
  along the way it comes, gravity's part included. So a held object kept from its place comes back no faster than
  the cap, and one whose weight nears the strength settles at its place instead of overshooting. The part of the
  error that holds the object up keeps up to `support(g)`, `(A + g) / omega^2`. The bound moves the joint's anchor on
  the hand for the step only (`step_holding`), so it is never in an image.
- **A lost race:** the loser's predicted hold disappears in its next corrected frame. The view's `held_by` names the
  winner, and the entry's outcome (in the view's `changes`, 7) names the refusal.
- **Pushes:** a physical hand pushes by contact, with no entry. A push input (world-model.md 8) is an impulse pe-world
  returns as a physics effect, applied before the step (3, step 2).

The constants are M1's defaults until the user's verdict on hand feel at the headset test [I].

### 3.4 Carried state is a check, not an overwrite

**Not built.** `Effects` has the slot for it (`carried`, a `CarriedFx`: the object and the author's 32-byte digest)
[V `crates/pe-replica/src/world.rs`], but no world model fills it and the replica compares nothing. The design, as
the user ruled it (Q15: "we go with your recommendation"; file 11, B4), replaces the old overwrite:
- **What is carried:** an interaction carries the touched body's digest (5.1), as the author's replica computed it
  after stepping frame f. The interactions are grab begins and push inputs. A continuing hold carries nothing
  (@coordinator-opus on O1).
- **No replica writes it.** The simulation never reads it.
- **The check:** each replica keeps its own digest of that body after f, recomputed on each re-simulation of f. When
  the cut covering f is final, it compares the two. A difference is an event, the author's prediction was wrong,
  counted for the display layer and offered to pe-diverge's fault bundle; it changes no state. It is expected when a
  late, earlier-stamped action reached the body first. A real fault shows in the cut's root anyway, and the ladder
  repairs it.
- **What the change gains** (file 02, B1): no reset of the body's contacts at every touch; no erasing of another
  player's earlier effect; no way to place a body anywhere by carrying a state.
- **What it costs, as the user accepted:** the author's own body can be corrected when a late, earlier action reaches
  it. The display layer still never corrects what a player holds on their screen (7).

### 3.5 The tick rate, and limits in seconds

The user's rulings: the simulation steps at a fixed tick that is never tied to the display, with "60
default, fully configurable in a range per world". The range is 30 to 120 Hz in whole hertz, with
ceil(240 / rate) sub-steps: the user chose the proposal of 13 ("30–120 Hz (rec.)").
- **The rate is a whole number of hertz,** a world setting (`SimRate`). dt is `1.0 / rate` in `f32`: one division,
  the same on every replica. M1 runs a world at one rate: the welcome checks it, and an import refuses another
  (`ImportError::RateMismatch`, 6.2). A change of rate within a world is **not built**.
- **Every limit is a duration, never a frame count** (@coordinator-opus). A bound that every replica
  must decide alike becomes frames by `pe_seams::time::frames_ceil(d, rate)`: rounded up, in exact integer arithmetic
  (pe-seams RULINGS.md 12). No replica converts one in floating point. 100 ms is 6 frames at 60 Hz and 9 at 90 Hz.
- **The sub-step count follows the rate:** ceil(240 / rate), at least 1, which keeps the sub-step rate at 240 Hz or
  more [V `crates/pe-physics/src/config.rs`, `with_tick_rate`; measured in 13.1]. Every physics world the replica
  builds takes it from the rate (`physics_config`).
- **What depends on the rate in the fork** [V, in `crates/box3d-fork/src/`]:
  - the step caps contact stiffness at an eighth of the sub-step rate, against the world's 30 Hz (`world/step.rs`,
    `types/world.rs`): below a 240 Hz sub-step rate, contacts are softer;
  - a body turns at most 45 degrees per tick (`solver/integrate.rs`);
  - collision runs once per tick, before the sub-steps (`world/step.rs`). Continuous collision sweeps a fast body
    against static shapes only, unless it is a bullet (`solver/continuous.rs`). So between dynamic and kinematic
    bodies, hands included, a lower rate lets a fast or thin body pass through another (13.2);
  - sleep waits 0.5 s, a duration already (`constants.rs`).

## 4. Rollback

### 4.1 Keyframes

- **A keyframe** is the state at the start of a frame: the world model's copy-on-write snapshot (`WorldModel::save`)
  and the physics `WorldImage` of the same instant [V `crates/pe-replica/src/replica.rs`, `Key`].
- **The ring's policy** is local and changes no result [V `crates/pe-replica/src/ring.rs`]. It is the old loop's
  `Log(4)` with its spacing set in time: a keyframe every `rate / 15` frames (4 at 60 Hz, about 67 ms) for the newest
  4 spacings, then about two per doubling of age, so memory grows with the logarithm of the unconfirmed span. Every cut
  frame gets a keyframe too, and each advance trims the ring by the policy, with or without a finalize.
- **Always kept:** the keyframe at `Cv` (the newest final cut my root matched), which rung 1 replays from, and the
  one at the floor (the newest cut I finalized). The floor's is the exception during a rung-1 replay, which replaces
  the history above `Cv`, the floor's keyframe included (`Ring::rewind_to`), until the next `finalize` pins the floor
  again; an abandoned replay puts it back from the floor's held snapshot (6.1).
- **Held roots are not keyframes.** A root at a cut is the state with exactly the cut's entries, which the live
  history's keyframe at that frame need not share. The replica keeps each as its own snapshot (world state, physics
  image and transfer) until its cut is final or superseded, and a rollback never takes one away (5.3, 8). Where the
  live history did step exactly the cut's entries, the snapshot is a copy of the ring's keyframe at the cut's frame,
  with the transfer the live take_root serialized (5.4).
  - **Thinned in a long stall** (was O19; @coordinator-opus, with @chunks-reviewer's points) [V `replica.rs`, `release`]. A finality stall votes at every cut and finalizes none, and a snapshot
    costs about 1.15 MB at 200 balls and 3.84 MB at 500 [V, a diagnostic run, `<notes>`]: up to 11 and
    38 MB a second at 10 cuts a second. So every voted root stays, as my vote, but its snapshot only where something
    reads it before the cut is final, and each vote thins them, not only `finalize`:
    - the resume (5.3) needs the newest: the ring's policy over the cut grid keeps every voted cut of the newest four
      cut intervals, then two per doubling of age;
    - a root split's reference member serves its root at the split's cut (divergence.md 5, 11.1), so each cut a rung-1
      replay was asked for keeps its snapshot until a finalize at or above it, whether the replay ran or not;
    - an import at a cut whose snapshot is gone takes the empty state as its base (6.2);
    - a snapshot kept for anything else (the floor's, `Cv`'s, the finals', the diverged root, my last import's) stays.
  - **Measured** [V, `measure_vote_cost_in_a_stall` in release, 200 balls at 60 Hz, K = 8, votes from 16 to 3000]: at
    most 17 snapshots over 374 votes, against one per vote without the thinning, and every vote after the first still
    re-stepped 8 frames (median 5.5 ms). A 10-minute stall at 60 Hz with the 0.1 s cut interval stays within about 27
    [I, the policy's bound]: about 100 MB at 500 balls. Tests [V]: `a_long_stall_holds_logarithmically_many_snapshots`,
    `an_import_at_a_cut_whose_snapshot_was_thinned_takes_the_full_transfer` and
    `a_root_split_s_cut_keeps_its_snapshot_while_the_stall_thins_the_rest`.
- **Not built:** compacting pe-physics's geometry registry after a trim, which the old loop did (review P22-12)
  [V: pe-replica never calls `compact_images`].

### 4.2 Re-simulation

- **When it happens:**
  - the source reports that frame f's entries changed (`EntrySource::take_changed_from`: a late entry, a void or a
    reorder);
  - the local member's prediction of its own code commit ended (logic.md 6.2a's option (i); 8.2), and the world model
    asks for its commit frame to be stepped again (`WorldModel::take_replay_from`).

  The replica restores the newest keyframe at or before f, and re-steps to the present with the entries as they now
  are.
- **The floor.** f is never below the floor, nor below the cut of my last import (`base_frame`), whose history the
  replica no longer has (6.2), nor, while a rung-1 replay runs, below the replay's cut frame (`rollback_floor`). A
  change reported below `base_frame` is a protocol fault: counted, and not acted on below it, and nothing panics. The
  one exception is expected: after a `finalize` re-based the live history on the floor (6.1), a report inside the
  re-based span, from its first differing frame g up to the floor, is excused in the first advance after it.
- **Bounded work per call** (S2 v1.1): `set_budget` gives the simulated time one call may step, at least two frames.
  A rollback, a re-simulation or a rung-1 replay that does not fit resumes on the next call, and the results never
  depend on the split [V test `the_budget_never_changes_the_result`]. Two steps are not budgeted yet (@design-reviewer's
  Lows on 3d13ff88): the re-step for a root at a cut off the live history (5.3), and rung 1's check of `Cv` (6.1).
- **In M1, re-simulation is synchronous on the simulation thread.**
  - The render thread never waits. It draws the last published view, and a player's own hands come from tracking.
  - In M1 the depth is about one network delay, 60 to 100 ms: 4 to 6 frames at 60 Hz [I].
  - An entry 2 s late costs 2 s of stepping: 120 frames at 60 Hz, about 60 ms at 500 balls [I, from file 02's bench].
    The cost of a given lateness grows with the tick rate (13).
  - The view counts the deepest rollback and the frames re-simulated (7).
- **In M2, a second copy re-simulates in the background.** **Not built.**
  - A deep re-simulation runs on a copy on a worker thread while the live copy keeps predicting. The copy is swapped
    in when it reaches the present.
  - The live copy's own inputs are entries in the corrected history, so nothing is lost.
  - Q3b's world-line merges need it: up to a minute of simulated time (3,600 frames at 60 Hz).
- **Skipping unchanged systems and islands** is M2's (file 14, 4.2).
- **A frozen place's catch-up** (the user; file 16, 6.7) is M4's.
  - When a player returns to a place nobody had loaded, the replica simulates it again from its saved state, for up
    to one minute after it froze, and never longer. The limit is a world setting, and one of its values is "no
    limit".
  - It takes no player input, so it is deterministic, and its result is agreed like any other state.
  - It runs on the M2 background copy, with a budget, so it never stalls a frame.

### 4.3 The properties the tests check

The tests run the replica over the mock world and the real pe-physics (`crates/pe-replica/tests/replica.rs`), and over
pe-world (`tests/peworld.rs`, `tests/logic.rs`) [V]:

| # | Property | Tests |
|---|---|---|
| P1 | Two replicas fed the same entries have equal roots at every cut frame | `equal_entries_give_equal_roots` |
| P2 | A replica that gets entries late, in any order, ends with the same roots as one that got them on time | `late_entries_roll_back_to_the_same_roots` |
| P3 | Restoring the state at a cut and re-stepping with the final log reproduces the roots | `a_cuts_root_takes_exactly_its_entries`, `rung_one_repairs_a_transient_fault`, `a_vote_resumes_from_the_root_held_at_the_previous_cut`, `a_vote_never_resumes_from_a_held_root_with_other_entries`, `a_vote_never_resumes_from_a_root_held_off_the_grid`, `a_long_stall_holds_logarithmically_many_snapshots` |
| P4 | No rollback goes below the final cut | `finalize_moves_the_floor_and_nothing_below_it_changes`, `a_change_below_the_floor_outside_the_rebased_span_is_still_a_fault` |
| P5 | No input from any member panics a replica; bad input is refused with a reason | **Not built** as a property test (3.1) |
| P6 | The worker count changes nothing | M2 (D2) |
| P7 | The call budget changes nothing (S2 v1.1) | `the_budget_never_changes_the_result` |

**Controls** (plan 4.1): every landing names a red control, a prepared commit that breaks what the landing adds and
fails exactly the named tests, which the integrator runs before the green one (`<notes>`). The
control behind 5.2 and 6.2 is a standing test at the physics level: pe-physics's `tests/digest.rs` imports the whole
transfer, which stays exact for 600 frames, and per-body state into a world built in id order, which must differ
within them [V].

## 5. Roots

### 5.1 The root hash

**The root is pe-world's root** (world-model.md 6): one BLAKE3 tree, 256 bits, the only hash size in S1. Its world node
holds the frame, the config's hash, the tables' hash and each domain's node; a domain's node holds its objects' root
and its `physics_global` [V `crates/pe-world/src/tree.rs`, `world_node`, `domain_node`]. So a root names one state of
the world at one frame.

pe-replica puts the physics into it in two places, both written at each cut frame just before the root is taken
(`write_physics`; 3, step 10):
- **`BodyState`, per bound body** (2.3). Its digest is pe-physics's per-body digest [V
  `crates/pe-physics/src/digest.rs`]: the body's pose bits, its velocities, its awake flag and sleep timer, and the
  touching contacts it shares with every body of a higher object id, manifold by manifold and point by point. Each
  pair is covered once, by its lower id.
- **`physics_global`, in the domain's node:** pe-physics's full-state check hash (`PhysicsWorld::check_hash`), BLAKE3
  over the domain's canonical transfer bytes. It covers the whole physics world: the hands' bodies, the grab joints,
  and a contact with a body that has no object id (a grab's anchor), which no per-body digest covers.

### 5.2 Why `physics_global` is the whole image

- **Box3D's next step reads history that no per-object record holds** [V `crates/pe-physics/src/digest.rs`'s notes,
  and in `crates/box3d-fork/src/`]:
  - body and contact ids come from free-list pools (`id_pool.rs`);
  - a contact's colour in the constraint graph is the first colour in which neither of its bodies is already used, so
    it depends on which contacts began touching earlier, and on body ids (`constraint_graph.rs`);
  - the joint colours, solver-set membership and order, islands, the broad-phase trees and move buffer, the caches of
    contacts that are not touching, and sensor state.
- **So equal per-body and per-pair states do not give equal futures; equal canonical images do.** Measured: a fresh
  world given every body's exact state diverges within 600 frames, and one given the whole transfer stays exact [V
  `crates/pe-physics/tests/digest.rs`]. This is @diverge-eng's requirement R-H1: equal roots must give equal futures.
- None of that state can be split cleanly out of the fork's serialization. So the complete cover is the hash of the
  whole canonical image.
- **The per-body digests localize; `physics_global` makes the root complete:**
  - a differing ball shows in its own `BodyState` leaf too, so a walk down the tree finds which body it is: rung 2
    fetches exactly that object's leaf and the physics image [V `crates/pe-diverge/tests/kinds.rs`];
  - a difference only in `physics_global` means equal values with a different solver history. That is rare, and the
    whole-image restore repairs it (6.2).

### 5.3 Cut frames, and the cost

- **Cuts lie on a grid:** frames that are multiples of K, where K is the cut interval, a duration agreed with WS-agree,
  in frames by 3.5's rule (`ReplicaConfig::cut_every`). The replica computes the root at every grid frame it steps,
  live or re-simulated, and keeps those roots from the floor to the present, with the ids of the entries each frame
  stepped.
- **A root at a cut** (`root_at_cut`, 8) needs the state with exactly the cut's entries. The replica starts from the
  newest keyframe at or before the first frame whose live entries are not exactly the cut's (`first_difference`), or
  at the cut's frame when they all are, and re-steps to the cut's frame through the same frame function, off the live
  history; then it puts the live state back. When the live history stepped exactly the cut's entries, the root is
  the live one, taken with no restore (5.4). The re-step is not budgeted yet (4.2): normally one or two cut intervals.
- **A vote's re-step resumes from a held root** (was O13) [V `replica.rs`, `walk` and `resume_from`; added after
  c9e2829a]. It starts instead from my root held at an earlier voted cut on the grid, the newest one above that
  keyframe, when two things hold. The root was computed on the same lineage: the same base (the floor, or my last
  import), with no rung-1 replay since. And that cut's entries from the base up to its frame are exactly this cut's.
  A held root records both: the lineage, and a BLAKE3 digest of its cut's entry ids, frame by frame from the base, in
  apply order. One pass over this cut's entries, with no physics, finds the first difference and the digests to
  compare. Determinism makes the root the same as a full re-step's. `finalize` always re-steps from the first
  difference, since its re-base on the floor (4.2) takes that re-step's records from there; at a cut the live history
  matches it takes the live root (5.4).
  `ReplicaImpl::scratch().frames` counts the frames re-stepped.
  - **Two limits** (@chunks-reviewer's L1 and L2 on 3aa8180d). A root held off the grid is never resumed from: it was
    saved after its rows were written (`take_root`), which a step through that frame never writes, and no honest
    voter accepts such a cut (`Reject::OffGrid`). And whatever changes what a frame steps to, for the same state and
    entries, must start a new lineage, or a vote would resume from a root stepped the old way. The catch-up after a
    vote hold is one (8.2, not built).
  - **Why** [V, diagnostic branch diag/stall-root 5d83d592, `<notes>`]: entries that are void
    in every cut's ancestry stay in the live history until a final cut voids them. So while finality stalls, every
    root re-stepped from the same early frame, at a cost that grew with the distance, about 0.75 ms a frame.
  - **Measured** [V, `measure_vote_cost_in_a_stall` in release on a loaded machine at idle priority: the M1 ball pit
    of 200 balls at 60 Hz, K = 8, an entry at frame 10 that no cut covers, a vote at every cut from 16 to 600]. With
    the resume, each vote after the first re-stepped 8 frames, in 5-11 ms (median 7.4 ms), whatever its distance; all
    74 votes re-stepped 600 frames in 0.57 s. Without it, each re-stepped from frame 0: 22,792 frames in 15.9 s, up to
    438 ms for the vote at 600 (about 0.7 ms a frame). Over 374 votes from 16 to 3000, with the thinning of 4.1, each
    vote after the first still re-stepped 8 frames (median 5.5 ms); the last 50 votes' median was 0.5 ms above the
    first 50's, the walk over the cut's entries from the floor.
- **Cost per cut** [V, @engine-architect's two-member sessions at 500 balls, on the laptop and the desktop, with the
  diagnostic timers of branch diag/root-split bedac9fe; `<notes>`]. It had been estimated at 1-2 ms.
  - A take_root costs 9.5-10 ms:
    - the per-body digests and `physics_global`, 3.7-4.2 ms. Most of that is serializing the canonical transfer
      (1.59 MB at 500 balls) and hashing it;
    - pe-world's `write_physics`, 4.9-5.9 ms. It rewrote every `BodyState` row then, so every object's leaf was
      rehashed, sleeping balls' included;
    - the root, 0.3-0.4 ms.
  - At 200 balls, alone on the laptop, a take_root costs 3.1 ms.
  - With a cut every 8 frames at 60 Hz that is about 1.2 ms a frame of the live history. The cost per second follows
    the cut interval, not the tick rate.
  - **Writing only the rows that changed** (L3a, @coordinator-opus). When the bodies are the ones the
    column holds, pe-world's `write_physics` overwrites only the rows whose bytes changed, so only those objects'
    leaves are rehashed; when a body joined or left, it rewrites the column as before. The result is the same either
    way, row versions included.
    - How many rows change [V, 2026-11-18, two headless pe-apps on one machine, diag/root-split-3]: in an untouched
      500-ball session, 3,547 of the 298,960 rows that 592 take_roots wrote changed, all in the first 0.7 s, before the
      pit fell asleep. One push into the settled pit wakes 2 to 4 balls: the preset sets its 0.12 m balls 0.15 m
      apart in two layers, so each ball or two-ball stack is its own island. A played-in pit, with balls heaped
      against the walls, is unmeasured; larger islands wake whole.
    - M2's incremental images cache the hashes of unchanged chunks, and sleeping bodies' digests.
- **pe-physics's per-frame XXH64 state hash** (`state_hash`) stays internal, as a cheap "did anything change" check.
  Nobody votes with it.

### 5.4 A cut the live history matches: the live root and the kept transfer

[V `crates/pe-replica/src/replica.rs`, `reuse_live_root`, `take_root`; @coordinator-opus; added after
c9e2829a. The kept grid frames: @coordinator-opus]
- **Why** [V, `<notes>`]. In @engine-architect's two-member sessions at 500 balls, every cut a vote or
  `finalize` asked about matched the live history. Each still re-did the live step's work at that frame: it saved the
  live physics, restored the very keyframe the live step had saved there, took the root again and put the live state
  back. That is 18.5 ms a root on the laptop (24.4 ms with its display on) and 16.9 ms on the desktop. Votes and
  finalizes were 55 % of the laptop's busy time.
- **The live root.** A cut matches when every frame from the base to the cut stepped exactly the cut's entries, in
  apply order (`from == at`, the same test 5.3's re-step starts from). The live step then took the root at the cut's
  frame, from the same state and through the same frame function. A vote and `finalize` take that root
  (`reuse_live_root`), with no restore, re-step or hash.
  - Its snapshot (4.1) is the one already held for that root (as at `finalize` after a vote). Otherwise it is the
    keyframe the live history took at the cut's frame right after that take_root wrote its rows, kept with the
    transfer the take_root serialized (below). It is not the ring's: the ring thins it past its young window.
  - Where the live history took no root at the cut's frame, or neither snapshot source is there, the root is
    re-stepped as in 5.3.
- **Not under a vote hold at or below the cut** (`held_at_or_below`). The live history runs my own predicted code
  commit from c + 1 (logic.md 6.2a's option (i); 8.2), and a re-step never does. So a live root there is not the
  cut's root, and the re-step decides, as before. Test: `a_vote_hold_below_the_cut_keeps_the_live_root_out`.
- **The transfer is serialized once.** `physics_global` is the check hash of the canonical transfer (5.1), so every
  take_root serializes it. take_root now keeps those bytes and their hash (`Taken`), and a computed root's snapshot
  takes them instead of serializing and hashing the same state again. That saves about 4 ms a root at 500 balls.
- **Kept until the floor passes, for at most 16 grid frames** (`LIVE_KEPT`). Each live grid frame keeps its transfer
  and its keyframe until the floor passes its frame, so a late vote or `finalize` still takes the live root.
  - Why [V, 2026-11-18]: with only the newest four grid frames' transfers and the ring's keyframe, a matched vote more
    than 16 frames late missed the live root at 3 grid frames in 4, and one more than 24 frames late at all of them.
    Past 16 frames the ring keeps every 8th frame only, and pe-app cuts every 6. In a two-member session on one
    machine at idle priority, 204 of 538 matched votes took the live root; the rest re-stepped 2, 4 or 6 frames, at
    7 ms a root.
  - Past 16 (finality stalled: an outage, a stuck member), the oldest goes, and a cut there is re-stepped as in 5.3.
  - Memory: a kept frame holds its transfer (1.57 MB at 500 balls) and, once the ring has thinned that keyframe, its
    image, which is never larger than the transfer that serializes it. That is at most 50 MB at 500 balls. An image is
    shared (`Arc`), not copied, by the ring, the kept frames and the held snapshots. The kept world snapshots add
    their own copies of the columns the live state wrote since each was taken, since a shared column is copied whole
    on its first write: at 500 balls the `BodyState` and `Transform` columns are 32 kB and 40 kB, about 0.1 MB a frame
    with the hash cache's map [I, from the row sizes] (@chunks-reviewer's nit on 02ee50d6).
  - Tests: `a_vote_30_frames_late_takes_the_live_root`, `past_the_kept_grid_frames_a_late_vote_re_steps`;
    `ReplicaImpl::live_kept()` counts the kept frames.
- **The same values.** Nothing given out changes: the roots, transfers and check hashes are the same, so no build
  identity changes [I]. The live root at a matched cut is the root of the keyframe the old path restored, and the
  kept bytes are the same function of the same physics state.
  - `ReplicaConfig::reuse_live` off takes the old paths.
  - `the_live_root_and_the_kept_transfer_give_what_a_re_step_gives` runs both through a session-shaped run (late
    entries, rollbacks, a stall, a re-base on the floor) and compares every root, transfer and check hash.
  - `ReplicaImpl::scratch().reused` counts the roots taken from the live history.
- **Expected saving** [I, from the sessions' measured parts]:
  - the laptop from 64 % busy to about 31 %, or from 79 % to about 34 % with its display on;
  - the desktop from 65 % to about 36 %.
  - M1's bar is the laptop with its display on, at 500 balls, under 50 % busy with no hold over 2 ticks. It is
    measured on the laptop after this lands.

## 6. Repair

### 6.1 Rung 1: replay locally from my last agreed cut

It is triggered when a cut at frame g is final with agreed root H and my `root(g)` differs: `finalize` answers
`Diverged`, and pe-diverge asks for `replay_to(&cut, source)` (S2 v1). As built [V
`crates/pe-replica/src/replica.rs`, `replay_to`]:

1. **Start from `Cv`,** my last agreed cut, at frame a ≤ g: the newest final cut where my root equalled the agreed
   one. Its keyframe stays pinned in the ring (4.1). S3 keeps its entries from a on (S3 v1.2: `finalize` tells the
   source `keep_final_from(cv, floor)` whenever either moves). The answer is `NotKept`, before anything is restored,
   when there is no `Cv` yet (before the first agreed `finalize`), when the cut is below `Cv`, when S3 has trimmed
   past a, or when `Cv` is held from before my last import (its physics image is the old world's); the ladder then
   goes on to rung 2.
2. **Check `Cv`'s state,** recomputing everything the root rests on and ignoring every cached value: pe-world's tree
   `verify` rehashes the rows and tables from scratch (world-model.md 6.4), and the restored physics world's check hash
   must equal the `physics_global` held with it, and the world's root `Cv`'s. A stored hash alone proves nothing about
   the bytes behind it. If anything differs, the saved state itself is damaged: the live state is put back, the answer
   is `Damaged`, and the ladder goes on to rung 2. The check is O(world) in one call, outside the budget (4.2).
3. **Re-step from a,** with exactly the cut's entries below the cut's frame, and take `root(g)`. The live history starts
   again at a, so the replay continues to the present with the source's entries, within the budget across calls
   (`Pending` until it reaches g), and becomes the live state, whatever the root.
   - **Equal to H:** repaired. pe-diverge calls `finalize` again for the cut, which now agrees and moves `Cv`. The view
     marks what changed `CORRECTED` (7).
   - **Different:** the fault comes back from good state. Either it is deterministic on this machine (a different
     build or CPU, F1 or F3), or the entries differ. Rung 1 has failed, and pe-diverge goes on with both roots.
   - **A frame S3 no longer keeps** met during the replay below the floor abandons it (`NotKept` when asked again, no
     fault), and the live state goes back to the floor's snapshot, so the member keeps simulating while rung 2 goes
     ahead (@design-reviewer). One at or above the floor stops it (`Damaged` if the cut's root needed
     it).

It works at a final cut or at a cut not final yet (a root split's), moves neither `Cv` nor the floor, uses no network,
and costs the steps from a to the present. With two players and no majority, both run rung 1 from the last cut they
share; that rule is WS-diverge's (file 14, 4.3). Tests: `rung_one_repairs_a_transient_fault` and the trim cases of
`tests/replica.rs` [V].

**`finalize` and the floor** (S2 v1): `finalize(&FinalCut, source)` is the only call that moves `Cv` and the floor.
Agreed: the cut becomes the floor and `Cv`. Diverged: the floor moves, `Cv` stays, my root at the cut stays held until
a later call agrees, and the view's health is `Repairing` (7). When the live history below the cut's frame stepped
other entries than the cut's (a late entry the cut left out, voided as it went final), the live state goes back to
the floor's cut-exact snapshot at once, with the cut-exact outcomes below the floor, and the next call re-simulates
(`rebase_on_floor`; @chunks-reviewer's M1 on 40f9cca4, option (a) by @coordinator-opus). Test:
`a_late_entry_voided_as_the_cut_goes_final_is_no_fault_and_leaves_no_wrong_state` [V].

### 6.2 What rung 2 and joining get from the replica

- **The world-model part** is pe-world's: node hashes by path, export by `LeafKey`, and `World::import`
  (world-model.md 6.5-6.6). `World::import` checks every record against the target root before anything applies, and
  takes every setting from the world's config record (`LeafKey::Config`, S2 v1.3).
- **The physics part, in M1, is the donor's whole canonical transfer at the cut:** S2's `physics_image(root)`, one blob
  with its check hash (pe-seams ruling 5), in @diverge-eng's 4.2a. There is no exact per-body setter in the fork, and
  WS-state will not add one: 5.2 is why.
- **`import_at_cut(group, import, source)`** (S2 v1.2, round 3) [V `replica.rs`, `import_at_cut`]:
  - **Checked whole before the live state changes:** every record against `import.target_root` (the quorum's root, or
    a root split's reference root); the physics transfer against the target root's `physics_global` (`PhysicsCheck`
    otherwise; pe-physics's `from_transfer` refuses a transfer whose check hash differs); and the patched state's root
    against the target (`RootMismatch`, which also covers absent leaves and a missing config record). A failed check
    changes nothing.
  - **The imported world's settings, never mine** (world-model.md 6.6; @coordinator-opus): the new
    physics world takes the imported gravity and origin. A world runs at one rate in M1, so a rate other than the
    replica's is refused, `RateMismatch`, a fault to report and never a rate to adopt (S2 v1.4; @coordinator-opus). Tests [V]:
    `a_joiner_takes_gravity_and_origin_from_the_world_it_imports_and_rung_one_agrees_with_the_host` and
    `an_import_of_a_world_at_another_rate_is_refused_and_changes_nothing`.
  - **The base** is my own snapshot at the cut when I hold one, else the empty state. A voted cut whose snapshot was
    thinned (4.1) has none: `tree` then gives pe-diverge nothing to diff against, so it fetches every leaf, and the
    full transfer lands on the empty state [V `an_import_at_a_cut_whose_snapshot_was_thinned_takes_the_full_transfer`].
  - **Inline blobs** (S2 v1.8; world-model.md 3.4, @coordinator-opus): `CutImport.blobs` carries the
    bytes of the small blobs (1 to 256 bytes) the imported rows reference, from the donor's `Tree::export_blobs`.
    A blob no imported row references, or one out of hash order, is `BadBlob(i)`; a missing small blob, simulation
    or display (a name), this machine lacks too is `MissingBlob` (inline blobs phase 3b). pe-world checks both
    inside `World::import`, so a failure changes nothing.
  - **Then the live state is the imported one at the cut's frame,** in a new physics world, and re-simulates to the
    present through the replay, returning my root at the cut. It moves neither `Cv` nor the floor, and never rehashes
    the rest of the state as a side effect: damage behind a stale cache shows as `RootMismatch` instead of being fixed
    silently (@diverge-eng).
  - **pe-physics restores a transfer only into a new world,** whose images are not the old world's. So the ring starts
    again at the cut, the snapshots held from before keep their records and transfers for exports but are never
    restored again, and until the floor reaches the cut the replica keeps no history below it (`base_frame`): there,
    roots answer `Damaged` (rung 2 again), changes are faults, and no replay starts from an older `Cv`.
  - **Records** (S5 v1.5): each entry of the cut's order from my last agreed cut up reads `Unknown` here, since my
    verdicts were made on the wrong state, and one I stepped that the cut leaves out reads `Dropped`. What the views
    carried below the cut is retracted (7).
- **A divergence in any bound body restores the whole physics image,** along with every world-model row. That keeps
  the body table equal to the world model (R1). Tested with real damage: pe-diverge's `tests/kinds.rs` changes a
  body's velocity in the replica's own physics world, through pe-replica's test-only hook (feature `test-util`, never
  shipped: `damage_body`, `rekeyframe`), and rung 1 or rung 2 cures it; rung 2's import builds the new physics world
  from the image, never from fetched `BodyState` rows [V].
- **Joining** is the same import at the newest final cut, into an empty world: `WorldModel::empty`, a bare world with
  the engine's registry and default settings but this machine's rate. Test [V]:
  `a_joiner_imports_the_whole_state_from_empty`.
- **In M2, incremental images make the physics image a tree of content-addressed chunks.** Rung 2 then fetches only
  the chunks that differ and assembles the donor's exact bytes. It is still exact, and its traffic follows what
  changed. **Not built.**
- **Size:** about 1.2 MB uncompressed for M1's 500 balls [I, from file 02's 2.3-2.5 kB per body].

### 6.3 What waits for history-free physics

- **No per-object or per-group physics restore.** Importing some bodies' state into a world with other history
  diverges again (5.2). The group selector (`GroupSel`) is accepted and not used: every import is the whole domain.
- **History-free physics state** is the way to per-group restore, step and merge: file 02's proposal A2 and file 11's
  3.1, "later", 6-10 weeks [I, file 02].
- **What it means for the merge (M2-M3).** @coordinator-opus ruled that deterministic is enough. Every
  machine must reach the same merged state. A group only one side touched must keep its visible result, within a
  stated pose tolerance, and the display layer hides sub-visible differences. History-free physics stays "later".
  - **S2's group selector** (with @diverge-eng): `Domain`, or `Objects(set)`.
    - A group of physics-free rows restores and patches per group.
    - A group with any bound body is restored and replayed as the whole domain, physics and those rows together,
      because physics can restore and step only the whole domain, and the body table must equal the world model at
      every frame (R1).
  - **The risk to measure [I].** In a whole-domain replay, the other side's activity can give an untouched group's new
    contacts other ids, and so other colours (5.2). In an active pile that can grow past the tolerance within a
    minute. A harness scenario measures the drift: a moving ball pit and a resting stack, merged after 1 s, 10 s and
    60 s. **Not measured yet.**
  - **If the drift exceeds the tolerance,** a smaller fork change than history-free physics may be enough [I, to
    check]: create and colour new contacts in a canonical order (by shape pair), not in contact-id order. A group's
    colours would then depend only on its own contacts.

## 7. The frame view: what the replica puts in S5

pe-seams fixes the view's shape (`crates/pe-seams/src/view.rs`, S5 through v1.8). The replica fills it after an advance
that reaches the present with no replay under way, never during a re-simulation, and publishes it as an
`Arc<FrameView>` that nothing changes afterwards [V `crates/pe-replica/src/replica.rs`, `publish`]:
- **Objects** come from the world model (`WorldModel::view_objects`): for pe-world, every object with a `Shape`, a
  `Light` or a `Hand`, at its world transform, with its shape in its own units (the view's scale scales it), its
  appearance, light, hand owner and holder [V `peworld.rs`]. An object with no world transform is not drawn, and a hand
  without a `Shape` is drawn as a `HAND_RADIUS` sphere. The replica adds what only it knows:
  - **velocities,** from the physics, for objects with a body;
  - **`OWN_HAND`** on the local member's hands, and **`HELD_LOCAL`** on what one of them holds: the display draws
    those from tracking and never corrects them;
  - **`CORRECTED`** on any other object whose pose at the last published frame a replay, a repair or a merge changed.
    The replica compares the poses it published with the re-simulated ones when it comes back to that frame; the
    re-simulation is deterministic, so an unchanged object compares equal bit for bit. So interpolation can tell a
    correction from fast motion (divergence.md 11.3). A rung-2 import at or past the last published frame
    re-simulates nothing below it, so its changes are not marked; imports are below the present, so this is rare.
- **`held_by`** names the holder, from the claims table: the member and the hand, or no hand for an agent's grab. The
  grab joint holds the place the object had in the hand when the joint was made (3.3).
- **`changes`:** a `StepRecord` for each entry's outcome as this replica stepped it, complete at the final frame (S5
  v1.4-v1.6): a later record for the same entry supersedes an earlier one, a stepped entry the final order leaves out
  reads `Dropped` with S3's reason, and after a rung-2 import an affected entry reads `Unknown` (6.2). Entry outcomes
  (a grab won or lost, an edit applied or refused) reach readers here, not as effects.
- **`final_frame` and `health`:** the frame of `Cv`, and `Agreed`, or `Repairing` since a frame while a cut I
  finalized diverged or was damaged (S5 v1.5).
- **Effects** (S5 ruling 25), each with an `EffectKey` from its cause, so a replay never repeats one: the scripts'
  events, `EffectCause::Logic` with `EffectKind::Script` (logic.md 9.2a). Each frame's effects come from its latest
  step, and `resimulated` marks one that arose in a re-simulation of its frame. `retracted` lists the keys of effects
  already carried that a later step no longer produced; the replica keeps the carried keys from the base frame to the
  present. **Not built:** effects for entry outcomes (`EffectCause::Entry`; the outcomes are in `changes`), contact
  begins (`EffectCause::Physics`), and `trigger_arrived` (always `None`).
- **Activation records** (S5 v1.8): what became of each code commit at the end of a frame, keyed by its script asset
  and version: activated (with the objects its re-key moved) or voided (with why, the objects that blocked a re-key,
  and every counted build failure). A record published later with the same key supersedes the earlier one, and
  `activations_retracted` lists keys a later step no longer produced. While the author predicts its own commit (8.2),
  its view carries the predicted activation at the commit's frame; when the prediction ends, the replay from that
  frame supersedes the record with the rule's, or retracts it. Meanwhile the vote holds keep the final frame below the
  prediction, so pe-api never tells it as final. pe-api's change log reads these records to tell an author what
  became of their change.
- **Counters,** cumulative over the replica's life: `corrections` (advances that re-simulated), the deepest rollback,
  the frames re-simulated, and the floating-point guard's resets from both callers (3, step 1).
- **`sim_time`** is the shared clock's host time of the view's frame (`ReplicaConfig::clock`), so every reader shares
  one time base. The replica leaves `local_rig` at its default; pe-app sets the rig.

A player's own hands are drawn from tracking (WS-render, WS-xr). The view still carries the hand's simulated pose.

## 8. The S2 seam, as pe-seams fixed it

pe-seams's `Replica` trait (`crates/pe-seams/src/replica.rs`) is S2, through v1.7. `ReplicaImpl` implements it over
a `WorldModel` (8.1) and pe-physics [V `crates/pe-replica/src/replica.rs`], and pe-replica owns the driver (pe-seams
ruling 3). The calls, and what the replica does behind each:
- **`Driver::drive(replica, agree, present, self_test) -> DriveReport`** is the S2/S3 glue of each simulation tick,
  as app.md 3 (@engine-architect) shapes it [V `crates/pe-replica/src/drive.rs`]. Once per tick it:
  1. advances the replica to `present`;
  2. reconciles the replica's `vote_holds` with S3's `hold_votes_from` and `release_votes_from` (S2 v1.7), level-
     triggered, so a partial advance, a rollback that removed a commit, or a rebuilt replica never leaves a hold behind;
  3. for each cut S3 wants a root for, calls `root_at_cut`, then `vote` with the caller's self-test verdict, once per
     cut. A cut at or past a hold waits, unasked, until the hold is released;
  4. calls `finalize` when S3's final cut changed, or was not ready last time.

  Messages, local input, S3's `tick` and pe-diverge stay outside, in pe-app and the harness. pe-harness's `SeamNode`
  still runs its own copy of this loop, which places no vote holds, until it moves to this driver (@harness-eng).
- **`set_budget(simulated)`** bounds each call's stepping (S2 v1.1; 4.2).
- **`advance(present, source) -> Advance`** steps to `present`: it rolls back first if the source reports a change
  (4.2), and again if the author's prediction ended (8.2). It reports the frames stepped and re-simulated, the
  rollback, and whether it caught up, and it publishes the view when it did (7).
- **`outcome(&entry)`** is the outcome my live history gave the entry, and its frame; `None` for one not applied yet,
  in a frame an abandoned replay never reached, or above the floor and below my last import's cut.
- **`root_at_cut(&Cut, source)`** is my root at the cut's frame with exactly the cut's entries in effect (5.3), held
  until the cut is final or superseded. It answers `Pending` or `NotYetSimulated` before the replica gets there,
  `NotKept` below the floor, and `Damaged` where no root of mine will come (a frame no longer kept, a stop, or below my
  last import): rung 2 is due. A cut may cover void entries that `entries_at` leaves out, so the replica never answers
  "missing entries"; that rule is S3's (@coordinator-opus).
- **`finalize(&FinalCut, source)`** moves `Cv` and the floor (6.1); it may be called again for the same cut after a
  repair.
- **`replay_to(&cut, source)`** is rung 1 (6.1).
- **`tree(&root)`** gives pe-world's tree calls on a held root: `node`, `export`, `object_hashes` and `verify`
  (ruling 1), over S2's paths by nibble prefix, with `Node::hash` the one hash definition. The roots held are the
  floor, `Cv`, the final cuts within `T_keep` (1.5 s by default, and at least the last 2; ruling 7), my root at a cut I
  diverged at until a later `finalize` agrees, and my root at each cut I voted on that is not final yet.
- **`physics_image(&root)`** is one blob with its check hash (ruling 5; 6.2).
- **`import_at_cut(group, import, source)`** is rung 2's and a joiner's import (6.2).
- **`view()`** is the last published frame view (7).
- **`world_snapshot()` and `agreed_snapshot()`** (S2 v1.7) are the world model's read-only snapshots of the live state
  and of the state at `Cv`, with its frame. pe-app hands them to pe-api once per published view: the first answers
  pe-api's live reads, the second its `final_()`, so it never lags the final frame and never shows a prediction
  (api.md 9.1; @coordinator-opus).
- **`vote_holds()`** (S2 v1.7) is the frames from which this machine must not vote yet (8.2).
- **`runner_identity()` and `module(&hash)`** (S2 v1.7) are the script runner's identity and the metered bytes of a
  module it holds (8.2), meant for the hello and the fault bundle (divergence.md 6). The hello compares the runner's
  identity without calling them: pe-app's build id carries a hash of the identity of the settings its runner starts
  with, and a test checks it against what that runner reports (app.md 6; pe-app net.rs `build_id`,
  @item:primordial-engine/pe-app-s-hello-compares-the-script-runner-s-iden). **Not built:** the fault bundle's
  record, O18 (11); nothing calls `runner_identity()` or `module(&hash)` yet [V: no caller outside pe-replica].
- **The group selector:** `GroupSel::Domain` or `Objects`. Every import acts as `Domain` until history-free physics
  exists (6.3).
- **Unchanged since the proposal:**
  - permission checks happen inside the step, as outcomes, by pe-world's rules on the replica's state at that frame.
    So the rules have one owner, WS-world (@design-reviewer's L6);
  - keyframes stay inside the replica. pe-agree and pe-diverge never handle them; they see roots, images, exports and
    outcomes.

### 8.1 What pe-replica calls on the world model

pe-world and pe-replica are both behind S2, so they agreed these calls between them (ruling 9): the `WorldModel` trait
[V `crates/pe-replica/src/world.rs`]. `PeWorld` implements it over pe-world's `World` (world-model.md 9.2; its
`replica` calls) [V `crates/pe-replica/src/peworld.rs`], and `MockWorld` is a small world model for tests and the
harness, without scripts [V `crates/pe-replica/src/mock.rs`].
- **The step:**
  - `phase(&entry)` gives the entry's place in apply order (3, step 2): pe-world's `input_phase`;
  - `apply(frame, &entry) -> (Outcome, Effects)` runs once per entry. `Effects` is empty for a refused entry.
    Otherwise it holds the objects whose physics-relevant rows changed, in id order, each with its `Change` (3.2); the
    pushes (`PushFx`: an object, an impulse in newton seconds, and a point or the centre of mass); and the carried
    check's slot, which nothing fills yet (3.4);
  - `logic_stage(frame, stage, runner) -> LogicFx` runs one logic stage (8.2): the physics effects of what the scripts
    changed, their events, and local counts (host faults, floating-point resets, missing modules);
  - `end_frame(frame) -> Vec<Activation>` ends the frame (3, step 9), with its code commits' records (7).
- **What the physics reads:**
  - `body(id) -> Option<BodyRow>`: an object's body kind, fields, world pose and shapes (its own and its compound
    children's, each with its density; 2.1). `None` means it is not physical;
  - `physical()`: every physical object, for building a world from scratch;
  - `hands()`: every `Hand` row in id order, with its target pose (the hand object's world transform), grip, engaged
    flag, stamp, span, the input before, and mode (3.3);
  - `claims()`: the claims table, each claim with its hand (3.3);
  - `settings()`: the world's rate, gravity and physics origin (2.1, 6.2).
- **What the replica writes:**
  - `write_poses(&poses) -> Vec<ObjectId>` writes back the bodies that moved, in one batch, and returns those it
    refused (3, step 7);
  - `maintain_transforms() -> u32` returns the repair count (3, step 9);
  - `write_physics(&rows, global)` writes every bound body's `BodyState` and the domain's `physics_global` at a cut
    frame, before the root (5.1).
- **State:**
  - `root()`, after `write_physics` at a cut frame;
  - `save()` and `restore(&saved)`: the copy-on-write snapshot (world-model.md 5.2), for keyframes and held roots;
  - `tree(&saved)` gives the S2 tree calls on a saved root;
  - `empty()` is the world a joiner imports into, and `import(&saved, &import)` is rung 2's and a joiner's world-model
    part, checked against the target root before anything applies (6.2).
- **For the view and pe-api:** `view_objects()` (7), and `snapshot(final_frame)` and `snapshot_of(&saved,
  final_frame)`, the read-only snapshots behind `world_snapshot` and `agreed_snapshot` (8).
- **For scripts** (8.2): `scripted()`, whether the world has module rows; `set_local(member)`, the local member, whose
  commits the world model predicts; `take_replay_from()`, the frame to step again from when a prediction ended;
  `vote_holds()`; and `set_scratch(bool)`, set while the replica steps off its live history, so nothing about the
  prediction changes there.

### 8.2 Scripts in the replica

The replica steps a world's scripts; building, checking and attesting them is pe-app's build service, and the rules
are logic.md 6.2 and 6.2a. As built [V `crates/pe-replica/src/logic.rs`, `logic/runner.rs`, `peworld.rs`]:
- **The runner is injected** (@coordinator-opus, with @fable-reviewer's conditions): a
  `ScriptRunner` the replica is given (`ReplicaImpl::with_runner`), so pe-replica needs no pe-logic to step a world
  without scripts. The `logic` feature adds pe-logic's wasmtime runner, `LogicRunner`; the step is the same code with
  or without it. A run is a pure function of the module bytes, the state it is given and the fuel: no clock, no host
  randomness, nothing kept from one frame to the next. Every runner the repo builds passes `logic::conformance`.
- **Identity:** a runner reports the canonical bytes of S2's `RunnerIdentity`: the runtime and its version, every
  setting that can change a result, and the runner's own build. pe-app's hello compares runners: its build id
  carries a hash of this identity (8, app.md 6). **Not built:** a fault bundle's record of them (O18).
- **Which scripts run:** every row of the modules table whose activation frame is at or before the frame, in asset
  order. A script runs only where the runner holds the row's module and it binds under the row's ids: the ids its
  version gives each of the manifest's components, the tag among them, and the engine components of logic.md 3.2. A
  script that does not run on this machine is reported by the stage as missing, a build fault on this machine,
  counted in `ReplicaImpl::script_counts`.
- **Modules arrive ready, off the simulation thread:** pe-app's build service builds a pending version, checks it, and
  loads the module through a `ModuleInbox` on its own thread, so the simulation thread never compiles one (loading takes
  5 to 84 ms against a 13.9 ms frame at 72 Hz [V logic.md 4.2, 5.2a]). The runner takes what has arrived at the start of
  a frame (3, step 3), so both stages of a frame see the same modules. When a module arrives is this machine's
  timing, so a take can land inside a re-simulation.
- **Activation is inside the window** (logic.md 6.2a): a commit makes a pending version, which activates at the end of
  a frame once a quorum of its voters has attested their builds, or is voided, never in an entry's step (3, step 9).
  At n = 2 both members hold the module at the activation frame, since each attests only after loading it.
- **The author predicts its own commit** (logic.md 6.2a's option (i); @coordinator-opus): from the
  frame c of an edit of the local member's that leaves a version pending, its new code runs from c + 1 in every step
  and replay of c while the prediction stands, and the machine holds its votes from c + 1 (`vote_holds`). Another
  member's attestation or build failure of the version, a `Leave`, or reaching the deadline ends it; the replica then
  steps again from c under the rule, at once. A step off the live history never predicts. One prediction at a time.
- **Events** become S5 effects whose keys are stable across replays (7).
- **Not built** (after M1, @item:primordial-engine/pe-replica-build-asymmetry-at-3-players-vote-hol): vote holds for a
  version whose module this machine lacks at its activation frame. At n = 2 none is needed; at n ≥ 3 a member whose
  build is slow or failed would step that frame without the script. The catch-up that steps again from F once the
  module arrives must also start a new lineage for the resume (5.3): roots held at or past F were stepped without the
  script, though their cuts' entries still match (@chunks-reviewer's L1 on 3aa8180d).

## 9. pe-physics and the fork

**Done:**
- **M0:** the vdw records moved into `crates/pe-physics/tests/records/vdw` with the archive, and `spikes/physics` went
  to `archive/`.
- **Early M1:** the physics bench and its record generator moved into pe-physics (main b83e68db, approved by
  @chunks-reviewer at 08669b78): `examples/records.rs`, the per-body digests, and `scripts/rebless_physics_records.sh`
  and `check_par_determinism.sh` rewritten for the generator. That lifted the freeze on physics changes.
- **In M1:**
  - 128-bit object ids (2.2);
  - the sub-step count from the world's rate, ceil(240 / rate) (`PhysicsConfig::with_tick_rate`; 3.5, 13), 4 at
    60 Hz, so no record moved;
  - the setters a replica needs (`set_state`, `set_pose`, `set_velocity`, `set_target`), with `set_state` resetting a
    body's contact history through the fork's recorded op `body_reset_history`, so carried state is exact;
  - `compact_images`, and images that carry their registry id;
  - holds (3.3): `step_holding` with `HoldReach`, `grab_spring_rate`, `grab_anchor`, `static_in_way` and
    `joint_objects` [V `crates/pe-physics/src/world.rs`, `tests/holds.rs`];
  - continuous collision against static geometry, Box3D's default, pinned by `tests/ccd.rs` and held to M1's ball pit
    at the push caps' speeds (@coordinator-opus's push-tunnelling ruling, part (c));
  - the fork is never formatted (`crates/box3d-fork/rustfmt.toml`), and tier 1's clippy skips it (C10).

**Not done:** a read-out of contact impulses, for the measurement in 10.

**In M2:**
- incremental images in the fork;
- one shared worker pool (D2; `docs/parallel-physics.md`, findings 7.1 and 7.5);
- background re-simulation;
- skipping unchanged islands.

**Later:** history-free physics state.

**Gates:** pe-physics's and the fork's tests stay green, and the fork stays outside the format and lint gates (C10).

## 10. A measurement owed: replayed contact impulses (file 16, 6.6)

**Status: not done.** pe-physics has no read-out of contact impulses yet (9). The rest of this section is v0.2's
plan, unchanged.

**The question.** Machine X holds a large object L but not the small thing S that touches it. X applies the
influence the holders recorded ("at frame F, this impulse at this point") instead of simulating the contact. How
close does X's L stay to the holders' L, and how often must X take the agreed state? WS-regions must not build on
6.6 before this is answered.

**Method:** the pe-physics bench, release build, one worker, 20 seeds.
1. **Scenes:**
   - a 2 m box resting on the ground, hit by 1 to 20 balls of 6 cm;
   - a stack of crates on a moving platform;
   - a slab like a ship's deck, with 100 crates sliding.
2. **The holders** simulate everything. They record, per frame and per pair (S, L), the contact impulse on L: the
   normal and friction impulses at each manifold point, summed over sub-steps, with the point. That needs a
   pe-physics read-out of the manifold impulses; the fork's manifold points hold them [I, to check].
3. **Machine X** simulates L and the ground only, and applies the recorded impulses in one of three ways:
   - as `ImpulseAt` at the start of the frame;
   - as a force over the frame;
   - per sub-step, which needs a hook in the fork.
4. **Measured:**
   - L's position and rotation error against the holders' L, per frame: mean, 95th percentile and maximum over 10 s;
   - both with X taking the agreed state at every cut (every 133 ms: K = 8 at 60 Hz) and without;
   - the bytes per second of influence entries, against sending L's state.

**What it decides:**
- whether influence entries keep corrections below the display layer's blend threshold (file 14, 4.1; the number is
  question 3 there);
- where "send state instead, when that is smaller" should switch over.

**Expected [I]:** not exact. The holders solve the contact impulse together with L's other contacts, across sub-steps.
The error should be small for an occasional impulse, and grow with how often impulses come.

## 11. Open points

| # | Point | For |
|---|---|---|
| O4 | The angular coast cap for hands: a coasting hand's turn is not capped (3.3) | WS-state, measured with WS-xr |
| O5 | Grip thresholds: when an input is engaged. The reach check is pe-world's now (world-model.md 8.1) | WS-xr |
| O6 | The physics frame's origin per domain, beyond 10^6 m | M4 (WS-regions) |
| O8 | Carried checks (3.4): designed and ruled, not built. Whether M1 needs them | @coordinator-opus |
| O9 | A run-time check of R1 (2) | WS-state |
| O10 | P5's property test: arbitrary entries never panic a replica (3.1, 4.3) | WS-state |
| O11 | A body under a hand does not follow a pack's later poses: `end_frame` returning effects (a seam change, @engine-architect's call), or refusing a `Body` under a `Hand`, with a multi-pose test (@design-reviewer's Medium on e6d3ee9c; 3.2) | WS-state, WS-world |
| O14 | Vote holds for a version whose module this machine lacks at its activation frame, and a new lineage for the resume at their catch-up (8.2, 5.3) | WS-state, after M1, before sessions of 3 or more |
| O15 | The view's missing parts (7): effects for entry outcomes and contact begins, `trigger_arrived`, and an event for a body stopped at the write-back (3, step 7) | WS-state, with WS-render |
| O16 | Meshes and hulls (2.1), and compacting the geometry registry after a trim (4.1) | WS-state, WS-world |
| O17 | The grab spring's overshoot after a block and after a placement (@design-reviewer's Low on the hold bound) | WS-state, at the headset test |
| O18 | The fault bundle records the runner's identity and the modules the replica held (`runner_identity`, `module`; divergence.md 6, 8) | WS-diverge |

Settled since v0.2:
- **The cut grid** (was O2): cuts lie at multiples of K, the cut interval in frames (5.3).
- **A physical object the world model cannot build** (was O12) has no body, and each attempt to build it is a fault
  (2.1).
- **A vote's re-step resumes from a held root** whose cut's entries match, on the same lineage (was O13; 5.3): the
  pe-replica half of @item:primordial-engine/finality-stall-past-the-lag-cap-with-no-lead-n-2.
- **Held roots in a long stall are thinned** like the ring's keyframes, with every voted root kept (was O19; 4.1;
  @coordinator-opus).
- **The hands** follow world-model.md 8.1: the speed cap, with a hand faster than the cap moving at the cap (option
  (b), @coordinator-opus); the teleport distance; a held object never carried through a wall by a
  trailing placement; and the hold bound (3.3).
- **A joiner's physics settings** come from the world it imports, and a world runs at one rate in M1 (6.2;
  @coordinator-opus).
- **The script runner is injected,** and a replica without one refuses a world with scripts (8.2; @coordinator-opus).
- **The tick rate is a world setting, 30 to 120 Hz in whole hertz, 60 by default, with ceil(240 / rate) sub-steps,
  and every limit is a duration** (the user; @coordinator-opus). That was O7. See 3.5 and 13.
- **The frame view** gives what `docs/spec/render.md` 12 asks of S5 (7).
- **The floating-point guard** also runs at every entry into a logic guest (@iter-bench's N11; 3, step 1).
- **Carried state is a check, not an overwrite** (the user, Q15). A continuing hold carries nothing
  (@coordinator-opus). See 3.4; not built (O8).
- **The stage order** of 3 is confirmed by @iter-bench (WS-logic): logic reconciles once per stage.
- **Untouched groups after a merge** need to be deterministic, not bit-exact (@coordinator-opus). The
  drift is to measure (6.3).
- **No exact per-body setter.** A physics divergence restores the whole image (6.2); agreed with @world-model and
  @diverge-eng.
- **`physics_global` is the whole canonical image hash,** and the per-body digests localize (5.1); agreed with
  @world-model.

## 12. M1's work, as it stands

1. **M0:** the records moved with the archive (9). Then, first in M1, the bench and record generator moved into
   pe-physics: main b83e68db.
2. **This specification,** and S2 and S5 in pe-seams v0 (main f3c1e6a7). This file lands as v1.0, describing what was
   built.
3. **pe-replica against pe-seams:** stepping, keyframes and rollback, roots, the frame view, and properties P1-P4 and
   P7 with their controls: done. Hands and grabs: done, with pe-world (main 432a325d and the hold bound ecda6974).
   Carried checks (O8) and P5 (O10): not built.
4. **128-bit ids** in pe-physics, and its sub-step count from the rate: done (9).
5. **Rung 1, the cut export and import,** and @diverge-eng's 600-frame test and its control: done. The 600-frame test
   runs at the physics level (5.2).
6. **Integration with pe-world** (its inputs, main f66dae4f; the world's physics settings, a810930a; its hash tree,
   602494fa; the leave, 0468dfdc) **and with pe-agree** (the driver, 8): done.
7. **The script binding** (logic.md 6.2, 10.1), for n = 2: done (main be748884, a02e1209, ed948b35, fe8fa3ca, 2dff384f
   and 20438f9e). The holds for sessions of 3 or more come after M1 (O14).
8. **The contact-impulse measurement** (10): not done. It is owed before WS-regions builds on 6.6.

Landing: code after review by @chunks-reviewer or @design-reviewer (the latter for anything that touches agreement
or rollback), through @integration-queue with a prepared red control; docs-only changes may be self-integrated. Every
cargo job runs through `<notes>`.

## 13. The tick rate's range: WS-state's side of the proposal (ruled)

This is the record of the proposal the user ruled on. The rule is built (3.5, 9).

@coordinator-opus asked @engine-architect and WS-state for the supported range's bounds, with a reason for each. This is the physics side. @engine-architect's joint proposal adds that agreement sets
no tighter bound and that bandwidth is a cost, not a bound.

**Proposed, and ruled by the user: 30 to 120 Hz in whole hertz, 60 by default, with ceil(240 / rate)
sub-steps.**

Measured with `crates/pe-physics/examples/rates.rs` (branch ws/state, commit 7e14b9f1; not on main): release
build, one worker, Windows x86-64. It ran under the build
lock at idle priority while other agents' jobs ran, so costs are upper bounds and noisy (about ±30 %; file 02's quiet
bench stepped the 500-ball pit about twice as fast). Every number in the tables is [V]; the conclusions are [I].

### 13.1 The sub-step rule: ceil(240 / rate)

Box3D caps contact stiffness at an eighth of the sub-step rate, against the world's 30 Hz (3.5). So the sub-step rate
must stay at 240 Hz or more. With today's fixed 4 sub-steps, that holds only from 60 Hz up. The rule gives 12
sub-steps at 20 Hz, 8 at 30, 4 at 60 and 72, 3 at 90, 2 at 120 and 144, and 1 at 240.

Balls of 4 cm radius dropped into a box and settled for 5 s; the penetration between touching balls, mean and maximum,
in mm:

| Rate (Hz) | 500 balls, 4 sub-steps | 500 balls, the rule | 4,000 balls, 4 sub-steps | 4,000 balls, the rule |
|---|---|---|---|---|
| 20 | 5.01, 25.0 | 1.01, 4.4 | 7.40, 45.6 | 1.91, 12.7 |
| 30 | 2.75, 16.4 | 0.99, 4.6 | 4.54, 31.5 | 1.76, 10.8 |
| 45 | 1.51, 7.3 | 0.94, 4.6 | 2.54, 15.3 | 1.66, 12.3 |
| 60 | 0.87, 4.1 | the same | 1.64, 11.1 | the same |
| 90 | 0.93, 4.7 | 0.92, 5.0 | 1.63, 10.4 | 1.63, 10.8 |
| 120 | 0.89, 5.2 | 0.92, 4.1 | 1.63, 9.9 | 1.62, 11.3 |
| 240 | 0.91, 4.3 | 0.89, 4.4 | 1.64, 9.6 | 1.60, 11.7 |

- **The rule holds the 60 Hz result at every rate from 20 to 240 Hz.** Four fixed sub-steps sink the balls into each
  other three times deeper at 30 Hz.
- **Above 240 Hz of sub-steps, nothing improves:** the stiffness is the world's 30 Hz from there.
- **At 60 Hz the rule gives 4,** so the committed records and goldens do not move.
- **Its cost** is within this machine's noise below 60 Hz, and it saves time above: at 120 Hz, 106 against 133 ms of
  CPU per simulated second for 500 balls.
- No ball left the box or fell through the floor in any run. Whether the pile slept within 5 s varied without a trend
  (one island sleeps all or nothing).

### 13.2 The low bound: 30 Hz

Box3D finds contacts once per tick, and sweeps a fast body only against static shapes unless it is a bullet (3.5).
Shots from 8 starting phases, with no gravity: the highest speed in m/s at which all 8 hit (the first speed tried with
a miss in brackets):

| Rate (Hz) | 20 | 30 | 45 | 60 | 72 | 90 | 120 | 144 | 240 |
|---|---|---|---|---|---|---|---|---|---|
| A ball of 8 cm at a resting ball | 2 (3) | 3 (4) | 4 (5) | 6 (8) | 6 (8) | 8 (10) | 12 (15) | 15 (20) | 20 or more |
| A kinematic hand, 10 cm across, through a resting ball | 2 (3) | 3 (4) | 4 (5) | 6 (8) | 6 (8) | 8 (10) | 15 (20) | 15 (20) | 20 or more |
| The control: the same ball as a bullet | all | all | all | all | all | all | all | all | all |

- **A body passes through another above about (the two sizes plus 2 cm) times the rate:** 0.1 m × the rate for two
  8 cm balls. The sub-step count changes nothing here.
- **At 30 Hz,** a ball thrown faster than 3 m/s can pass through another ball, and so can a hand swatting at 4 m/s.
  Below 30 Hz that happens at an ordinary hand speed, 2 to 3 m/s. A body also turns at most 45 degrees a tick: 3.75
  turns a second at 30 Hz.
- **So 30 Hz is the lowest rate at which hands and gentle throws work.** It suits large, slow worlds: it costs half
  of 60 Hz per simulated second (13.3), and a quarter under rollback.
- **Bullets remove this at any rate** (the control row), at the price of sweeping each fast body against dynamic
  bodies too. Bullets are manual opt-in only, never set by the engine (world-model.md 3.2), so marking a
  released object as a bullet while it is fast is not an option.

### 13.3 The high bound: 120 Hz

**Stepping cost** with the rule, in ms of CPU per simulated second, while the balls fall (the first 2 s, everything
awake):

| Rate (Hz) | 20 | 30 | 45 | 60 | 72 | 90 | 120 | 144 | 240 |
|---|---|---|---|---|---|---|---|---|---|
| 500 balls | 49 | 52 | 65 | 67 | 82 | 91 | 106 | 129 | 205 |
| 4,000 balls | 533 | 669 | 1,088 | 1,355 | 1,307 | 1,762 | 2,138 | 2,269 | 3,820 |

**Rollback multiplies it** [I, the model]. Remote entries arrive every tick, each for a frame about one network delay
d old. So every tick re-steps d × rate frames, plus the new one: rate × (1 + d × rate) steps per simulated second.
- The re-simulated span is a duration, but the steps in it grow with the rate. So the cost grows with the square of
  the rate.
- At d = 100 ms with the per-tick costs above (500 balls), the worst case is a steady re-simulation. Relative to
  60 Hz, it costs 0.44 times at 30 Hz, 1.9 times at 90 Hz, 2.9 times at 120 Hz, 4.2 times at 144 Hz and 11 times at
  240 Hz.
- On this loaded machine, 120 Hz is about 1.4 s of CPU per second for the M1 ball pit, and 60 Hz about 0.5 s. On a
  quiet machine, about half.
- **So above 60 Hz a world's scene budget shrinks fast.** At 120 Hz it is about a third of 60 Hz's.
- Above 120 Hz the cost passes four times 60 Hz's. Meanwhile the gain is only for bodies faster than 12 m/s, which
  bullets handle at any rate.
- The display rate gives no reason to go higher: rendering is decoupled from the tick.
- M2's background re-simulation and island skipping (4.2) lower the factor, but not its quadratic shape.

### 13.4 What the range changed elsewhere

- **pe-physics takes the rate's sub-step count** (`PhysicsConfig::with_tick_rate`). The constant `SUBSTEPS` stays as
  the default rate's count, 4 at 60 Hz [V `crates/pe-physics/src/config.rs`]. That was a change to the physics, so it
  landed after the record generator (9).
- The committed physics records and goldens are at 60 Hz with 4 sub-steps, which the rule keeps, so none moved.
- Every limit in this file is a duration (3.5), so none depends on the rate.
