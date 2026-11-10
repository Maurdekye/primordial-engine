# Re-founding plan: Primordial Engine above the determinism base

Status: the build plan for staffing, 2026-11-10. Written by @engine-architect for @coordinator-opus; @design-reviewer
reads it before staffing starts. Nothing in it is built. Base: main 757219f, the review's base (main has not moved
since [V]).

The order it plans is the user's (2026-11-10, m08995ecb2eab): "we are proceeding with @fable-reviewer 's rewrite plan
first: halt all work continuing the current version of the engine, and begin the suggested rewrite. on finishing, bring
the changes to @fable-reviewer to review them, and a checkpoint to me." Docket:
@item:primordial-engine/re-found-primordial-engine-above-the-determinism.

**The governing principle** (the user, mfc7e0ddc4137, with the instructions that followed, as the liaison put it):
- local-first and immediate: "everything reacts immediately for the local user as often as possible";
- others see an eventually consistent world "within a reasonable timeframe";
- the local user is held up as rarely as possible, and "no player should ever wait for other players' actions to
  finish" (mf7daea6446ca);
- divergences are repaired with as little visibility as possible;
- when split groups reconvene, they merge deterministically, like a git merge.

Inputs, each read in full for this plan:
- The independent review, `_teams/1847/fable-review/`:
  - 00, the summary;
  - 01-09, the defects not to carry over;
  - 10, the decision re-runs;
  - 11, the proposed architecture, as updated through the user's answers of 2026-11-10;
  - 12, the Part B finality follow-up;
  - 13, the Q3 and Q4 follow-up;
  - 14, divergence and recovery;
  - 15, machines without a compiler, and a public world with no admin reachable;
  - 16, simulation distance as the Open World Orchestrator does it;
  - 17, coordinates for script authors.
- The user's decisions in `_shared/user-design-decisions.md`, through 19:27Z on 2026-11-10, and the earlier rulings
  that still apply.
- The repository at 757219f: crate sizes and dependencies, which I counted for this plan.

Marks: **[V]** I verified it in the repository or the named record; **[V file N]** @fable-reviewer verified it (review
file N) and I did not re-run it; **[I]** my inference or judgement. Every size is soft: engineer-weeks for one
experienced engineer, as in file 11. Read them as relative sizes. Review, build slots and integration bound the
calendar time, not writing.

## 0. The plan on one page

- **The vertical slice comes first.** M1: see, touch in VR and edit through an agent, between two machines. Then:
  - M2, an agent authors game logic, and each machine activates code changes after its own compile (Q3);
  - M3, sessions that survive real life: join, leave, splits and merges, thin links, guarded mode, eight players;
  - M4, regions under the Open World Orchestrator's loading rule;
  - M5, the platform.
- **M0 comes before M1, and it is short.** It lays the ground every workstream needs: the archive, one small interface
  crate (the seams), the gate on hosted runners, the first model of agreed cuts, and three feasibility checks.
- **What is kept, ported and archived.**
  - Kept unchanged: the determinism base, which is the Box3D fork, pe-core, pe-physics and pe-sim (110,857 lines [V]).
  - Ported into new crates: about 3,900 lines of pe-session (chains, signatures, order, the rollback loop, fetch and
    state transfer).
  - Archived: the old session core and its harness, the protocol spike with Part B, and 2,333 evidence files.
- **Divergence and recovery are built in from M1.** The five systems of file 14 are:
  - a display layer that hides corrections;
  - bounded rollback;
  - a hash tree with scoped repair;
  - a git-style merge;
  - fault guards.

  The merge engine lands in M2, because Q3's long compiles need it, and is extended to split groups in M3.
- **Seventeen workstreams.** Each owns its own paths, and six seams join them. Fourteen of the workstreams are needed
  for M1.
- **Tests are properties, not blessed logs.**
  - The agreed-cut rules are written first as a model that a checker exhausts at small sizes.
  - The review's three probes are tests from the first week.
  - Every safety rule has a control that bites: a mutation that a test must catch.
- **Every ruling of the user's carries over.** Section 5 says where each lands. The values still open with the user
  (the simulation distance and the layer rules of file 16, and the coordinate questions of file 17) are parameters,
  not blockers (section 6).
- **All 25 live docket items are triaged:** 17 carry, 7 park and 1 drops.
- **The plan needs decisions:** 4 from the user and 13 from the coordinator (section 9).

## 1. Milestones

What every milestone has:
- **A demo** the user can watch.
- **Acceptance tests** that run without a person.
- **A checkpoint record:** what was built, how it was verified, and what is left.

The governing principle applies to every milestone. For each place in the design where a user could be held up
(file 11, 3.0, its table), each milestone measures two delays: when the others see an action, and when it becomes
final.

### M0. Ground (about 1-2 weeks, before and alongside the start of M1)

**Demo:**
- The one-command gate passes on four hosted platforms on the restructured workspace.
- The kept physics records are bit-identical on Windows, Linux, Linux ARM64 and macOS ARM64, the first run on the last
  two.
- The cuts model checks exhaustively at three members.

**Work:**
1. **The archive** (WS-archive): tag, move and delete, as listed in section 2.4. It lands first.
2. **Seams v0** (WS-seams): the `pe-seams` crate with the six seams (section 3.2), and a skeleton of every M1 crate
   compiling against it.
3. **The gate** (WS-ci, @provenance-eng's item @item:primordial-engine/landing-gates-run-the-native-code-audit-and-a-se):
   - `scripts/gate.sh`;
   - the native-code audit with an allowlist;
   - a secret scan;
   - a hosted workflow.
4. **Model v0** (WS-model): agreed cuts at fixed membership, three or four members, one faulty member.
5. **The harness skeleton** (WS-harness): the deterministic network model and the scenario runner, retargeted at the
   seams. The three probes are written as tests; they fail until the core exists.
6. **Feasibility checks,** a few days each:
   - QUIC with pure-Rust cryptography (WS-net);
   - OpenXR with the system loader loaded at run time, drawing through wgpu (WS-xr);
   - a wgpu window drawing the ball pit from a world snapshot (WS-render).

**Acceptance:**
- `gate.sh` exits non-zero on any of these, and passes on the restructured main:
  - a planted native dependency, or a planted secret;
  - a formatting or lint failure;
  - a failing test, or a failing model check.
- The hosted runs on windows-x64, linux-x64, linux-arm64 and macos-arm64 pass pe-core's, pe-sim's and pe-physics's
  tests, and the physics records match on all four. A mismatch is a finding: until it is fixed, M1's two machines must
  share a platform.
- **The model, exhaustively at three members with one faulty author:**
  - it checks agreement, validity and "no final frame is undone";
  - the d1 mutation must give a counterexample. In it, a member votes for a cut without validating each author's
    chain in order.
- Each feasibility check reports go or no-go, with evidence. A no-go comes back to the coordinator, with alternatives,
  before any M1 work depends on it.

**Size:** about 8-13 in all, mostly in parallel:
- the archive, 0.5-1;
- the seams, 1;
- the gate, 1-2;
- the model, 2-3;
- the harness skeleton, 1-2;
- the three feasibility checks, 1-1.5 each.

### M1. The vertical slice: see, touch in VR, edit through an agent, between two machines

**Demo** (a script, recorded on video with logs):
1. **A creates a world.** On machine A, the user's PC with a headset through OpenXR, A creates a world from the
   ball-pit template: a floor, walls, 500 balls, a table and two lamps. The world is data in the world model, not a
   hard-coded scene.
2. **B joins.** Machine B, a second physical machine, joins from an invite:
   - the invite carries A's key and reachable addresses;
   - the connection is direct QUIC, over a LAN or the user's own VPN, with no helper nodes;
   - B uses a desktop window, or a headset if one is available;
   - before B joins, the two machines compare their engine versions and a short physics self-test.
3. **Both see the same world.** Both machines draw it with wgpu. The headset runs at its own rate (90 Hz target), and
   the simulation steps at a fixed rate whatever the display rate (R2).
4. **Both touch it.**
   - A's tracked hands push and grab balls; hand entries now carry position, rotation and grip.
   - B sees it within about one network delay.
   - B's desktop hand, a mouse ray, does the same.
   - When both grab one ball, the earlier grab wins, and the loser's ball slides out of its grip with a short cue
     (R5; file 14, 4.1).
   - Corrections to what the other player touched are blended by the display layer; a player's own hands and what
     they hold are never corrected.
5. **An agent edits it.** An agent (Claude Code over MCP, on A) is asked to "put a red cube on the table", "make the
   balls blue", "double the cube" and "remove the left lamp".
   - It reads the world (`world.query`), changes it in one transaction (`world.apply`), and looks at the result
     (`view.capture`).
   - Both machines show each change.
   - An edit made against a stale version is refused, with its reason and the change that won.
6. **Repair.** A deliberate fault is injected on B: one ball's state is altered. The next cut's hash tree finds that
   ball, repair rungs 1 and 2 restore it (file 14, 4.3), and a fault bundle is written.
7. **B leaves,** and A carries on alone with no pause.
8. **Throughout,** both machines' world roots match at every final cut. An offline replay of the log reproduces the
   final root.

**Acceptance tests,** automatic and on the gate unless marked:
- **The demo's script in CI.** It runs as a two-process session over real QUIC on one machine, and checks:
  - the roots are equal at every final cut;
  - the live run equals the replay;
  - no final frame is undone, and no process aborts;
  - B's leave costs zero paused frames.
- **The probes.** probe-d1 passes against pe-agree (section 4.2).
- **Own hands only.** A member's input moves only its own hands: a forged hand entry is refused on every machine and
  the history is unchanged. This is the rule of @item:primordial-engine/session-validation-a-member-moves-only-its-own-h.
- **World edits:**
  - create, delete, set a field and reparent work;
  - a transaction applies all or nothing, and a dry run changes nothing;
  - a stale precondition is refused, with a reason that names the winner;
  - every refusal is reported over MCP.
- **The display layer:**
  - the drawn state converges on the simulated state within the stated blend time;
  - the error of a player's own hands, and of what they hold, is exactly zero;
  - a sound or particle effect caused by an event fires once, even when the event is replayed.
- **What the hash covers.** Everything that matters is inside it: world fields, governance and claims. A divergence
  injected into any one of them is found by the hash tree and repaired by rungs 1 and 2.
- **The guards.** A peer with a different engine version, or a failing physics self-test, is told why before it
  joins.
- **The agent.** `view.capture` returns an image; headless tests render with a software adapter. The agent's whole
  script runs in CI against the two-process session.
- **Rendering.** The display runs at 72, 90 and 120 Hz against a simulation at 60 and 90 Hz, and the simulation never
  depends on the frame rate.
- **VR, in CI:** the OpenXR frame loop runs against a simulated runtime, where one is available [I].
- **VR, on the headset** (by hand, recorded):
  - the frame time at 90 Hz with 500 balls and one remote hand at 60 ms one-way delay;
  - dropped frames, and judder under rollback;
  - the user's verdict on hand feel: hands physical only when engaged (D18b), beside hands always physical within
    reach.
- **Two physical machines** (by hand, once per checkpoint): the demo's script, with both machines' roots logged at
  every cut.
- **Measured and reported, not yet gated:**
  - how long before the other machine sees an action, and before it is final, as distributions over 20 seeds and 5
    delays;
  - the display layer's counters: corrections per minute, mean visible error, deepest rollback, rungs used.

**Size:** about 33-47 in all [I]:
- the renderer and VR with hands, 8-12 (file 11's slice 1, without the UI lab, which follows M1);
- the display layer, 1-2;
- the world model v1 with its hash tree, WorldApi and MCP, 8-11 (file 11's slice 2);
- agreement for M1, 4-5; transport for M1, 3-4; the replica, 2-3;
- repair rungs 1 and 2, the guards and the fault bundle, 2-3;
- the harness, the probes and the properties, 3-4;
- integration, 2-3.

### M2. Authoring: an agent writes and runs game logic, and every machine runs it after its own compile

**Demo:**
- **The agent writes a toy.** It writes a small toy in Rust: a button that spawns a ball each time it is pressed, a bin
  whose lamp turns green while a ball is in it, and a score kept in a component field.
  - `code.check` compiles it to wasm and validates it.
  - `code.test` runs it in a fork with random rollbacks, and once more far from the origin (file 17, 3.6).
  - The agent commits it as an edit with a base version and an agreed frame, and it runs as systems over the store.
- **Activation (Q3, as the user answered it, mf7daea6446ca):**
  - A runs the change at once.
  - B keeps running the old code, compiles the module from source with its own toolchain, then rewinds to the agreed
    frame and replays. Its objects jump once to the new behaviour (Q3a).
  - No machine runs another's build (Q3c).
- **A long compile (Q3b).** Then the same with a large module whose compile on B takes more than 2 s. B performs a
  world-line merge with main: main hands B back the state it diverged from, everything is re-simulated under the new
  rules (B's own actions included), and B's changes are merged in.
- **Exact replay.** A rollback across the swap is exact, and the inspector shows the score field.
- **A per-player script, under QR (b):** a lamp that is red only for A opens a door, and the door's opening is A's
  signed input.
- **Positions for script authors (file 17).** Scripts handle positions through the exact `Pos` type and the `Offset`
  type, in object-local frames. A script that would lose precision far from the origin does not compile, or fails the
  far-origin test.

**Acceptance:**
- **The same module bytes** give the same fuel and trap points on all four hosted platforms.
- **Code swaps:**
  - a rollback across a swap equals a replay;
  - a swap with an unchanged schema needs no migration;
  - a system whose inputs did not change is skipped on a re-simulated frame, and the result is unchanged.
- **Activation:**
  - the owner never waits;
  - B's view matches the owner's root at the first cut after B's compile;
  - a machine never loads a module it did not compile itself;
  - a compile over 2 s ends in a merge with main, after which the roots are equal and none of B's actions is lost
    without a reason;
  - the 2 s threshold is a parameter.
- **The per-player mode:**
  - an effect that names a stale version it read is refused;
  - a script declared "once for the world" gives one effect, not one per player.
- **Script hygiene:**
  - hidden state in statics is caught by the author-side test (random rollbacks and instance resets), as a structured
    error;
  - the far-origin test reports the first value that differs, and the line that produced it.
- **Measured:**
  - edit to first frame on the author's machine;
  - edit to the other machine's first frame, by compile time;
  - logic cost at 10,000 entities against the 1 ms budget.

**Size:** about 13-20:
- the logic runtime, 6-9 (file 11);
- the per-player mode, 2-3;
- the script-author coordinates, 2-3 (file 17);
- the merge engine v1 (one branch against the main line), 3-5 [I].

**After M1, in parallel with M2:** WS-render builds the UI lab (file 08, A2). It draws the launcher, one dense surface
and one agent panel, with the look's parameters tunable live by an agent.

### M3. Sessions that survive real life

**Demo:** five players on at least three machines, in a public world.
- A stranger joins without admin rights (Q6 of the first round). Its attempt to strip everyone's roles is refused
  (probe capture-close).
- A player leaves, with no pause.
- **Two players drop at once.**
  - The other three keep playing, and their actions show as not yet agreed.
  - The quorum stays a strict 2/3, with no kick exception (Q5, mf7daea6446ca), so an admin's kick of the two cannot
    be decided without them.
  - If the two return within a minute, the lines merge automatically.
  - If not, at one minute the three continue as a world line of their own, where finality resumes [I: my reading of
    "dropped / diverges" with the user's 19:09Z instruction]. The two lines may be re-merged later, by choice (M5).
    This is probe twosilent5-kick.
- **The network splits three and two.**
  - Both sides keep playing.
  - When they reconvene within a minute, the histories merge by the deterministic rules of file 14 (4.4). A deletion
    against an add-on is decided by the order of events (m35138bafc46a).
  - Corrections show only where the rules could not keep both sides' results.
- **Every admin becomes unreachable** in the public world. The world enters guarded mode with a visible notice, and a
  countdown starts (the owner sets it; 30 s by default). When it runs out, the world closes with its state saved
  (mb09ab8952a38).
- **Thin links.** A player whose link has diverged for more than 2 s performs the world-line merge with main (as in
  Q3b). A player on a 16 kbit/s link follows by state at cuts, and the others never pause.
- **Late joining.** A late joiner starts from a cut, watches until it has caught up, then acts. It verifies the
  governance it is handed.
- **Relays.** Two players behind strict routers connect through a third member acting as relay; no helper node exists.

**Acceptance:**
- probe-twosilent5-kick and probe-capture-close pass (section 4.2).
- **Merge determinism:** every member computes the same merged history and root from the same entry sets, whatever
  the arrival order. It is a property of the model and of the harness.
- **The one-minute bound.**
  - Unconfirmed play never lasts longer than one minute.
  - At the bound, the line diverges, and both lines know it.
  - Memory and replay time stay within what one minute needs: 3,600 frames at 60 Hz (file 14).
- **The full change record.** Every entry since the world's creation is kept, together with signed states at
  intervals. History below a final cut leaves live memory for the world record and peer caches, and is never deleted
  (the user, 19:09Z).
- **Guarded mode:**
  - every machine starts and stops the countdown on the same frame, judged from the shared log;
  - the risky rights are suspended for the remaining players;
  - the saved state is the state at closing.
- **Thin links,** in the shapes of the old thin-link cells (8-32 kbit/s downlinks, 2-8 players; the cell list moves
  with the harness):
  - the others never pause;
  - the thin player's delays and merges are reported.
- **Eight players at 90 Hz, every one acting.** Finality and visibility delays are reported as distributions.
- **The repair ladder:**
  - rungs 3-5 (reload a cell, rejoin, quarantine) each have a test;
  - a background self-check replays a random recent stretch and compares it with the recorded hashes.
- **Fault isolation:**
  - no byte string from one member aborts a replica (property tests);
  - a flooding member's excess entries are refused, and memory stays bounded.
- "Fewer than 0.01% dropped on acceptable connections" (m509a0128e064) is measured, not assumed.

**Size:** about 16-25:
- agreement, 4-6 more;
- transport, 3-4 more;
- the merge engine v2 (split groups) and the one-minute rule, 3-5 [I];
- the world record, 2-3;
- governance as entries, with guarded mode, 2-4;
- the harness, 2-3.

### M4. Regions, under the Open World Orchestrator's loading rule

The user's instruction (m5adc22d1388d) is to treat the simulation distance as the Open World Orchestrator does.
File 16 maps it onto this engine:
- **Layers.** Layers by object size, each twice the one below, are the loading index.
- **Chunks.** Each player loads a box of whole chunks per layer, about 125 at most.
- **The player's scale** selects which layers count.
- **Per machine.** Each machine holds its own set.
- **Freezing.** Chunks nobody is near freeze. Their state lives in the world record and in peer caches (Q4,
  mf7daea6446ca).
- **Notice of arrivals.** An arrival comes as an entry in the log of a chunk already loaded, never from a scan.
- **The unit of agreement.** Agreement and simulation run per group of things in contact, across layers, not per
  cell (file 16, C4). The domain id that every entry carries from M1 names that group.

**Parameters, kept open until the user answers file 16** (section 6):
- **The distance:** the OWO's ratio, about 70 m at player scale, or 1 km.
- **How the player's scale counts:** a window of layers, as in the OWO, or a ratio taken literally, as the user's
  mb09ab8952a38 reads.
- **When an object's layer is set:** by its current size, or fixed at placement.
- **A far player** who has not loaded the small things touching a large object: they may show it from the near
  players' agreed state.

**First, the evidence** (file 11, section 6):
- a two-dimensional protocol simulator with real player movement, run across the parameters above;
- a measurement, with a person's hands, of two things of nearly equal size in adjacent layers.

**Demo:**
- The world is several kilometres across.
- Two players far apart simulate disjoint sets of chunks.
- A ball thrown out of everyone's range stays frozen where it left it.
- Player B carries the ball back into A's range, and A learns of it from an arrival entry.
- A player walking into an area takes its state from those already there.
- Large objects simulate and draw from farther away than small ones.

**Acceptance:**
- **No polling:** counters show zero scans, and a player far from everything sends nothing about it.
- **Hand-overs:** an object moving between contact groups is handed over at a cut.
- **Coordinates** go past the 64-bit range without saturating (user ruling m7e85960c864a).
- **Each parameter** can be changed per world, and the tests pass at every value the user is choosing between.
- **A cost budget per machine,** measured at each value of the distance.

**Size:** 4-6 for the skeleton and the evidence, then 10-16 (file 11).

### M5. The platform

Each track writes its short specification before its code.
- **Saves, backups and merges of world copies,** derived from the world model and the world record. A save is a
  signed, named root.
- **Re-merging later, by choice.** Two lines that diverged more than a minute ago can be merged at any later time, as
  the user asked (19:09Z): the full record, the same rules, consent from an admin of each, and a preview computed in a
  copy first (file 14, 4.4). One rule is still open with the user: for long divergences, may the later change win,
  object by object?
- **Several worlds and portals.** Carrying between worlds is copy then remove (Q7).
- **People:** identities, invites and live vouching; bans, and perma-bans saved with the world. Received avatars and
  objects get simple Resonite-style protections (V1, mbe87e750ca35).
- **Player voice.** No design exists yet; it meets the Rust ruling at the codec.
- **Avatars:** standard formats first (VRM, glTF), then Resonite import as far as the licence research allows.
- **Engine releases and updates.** No design exists yet.
- **The interface:** the UI layer with the Gnosarium look as a skin, the tour, and agent panels.
  - The first-party agent loop runs on an API key, because providers allow subscriptions only through their own
    binaries (file 11, 3.7).
  - Subscription users go through the optional adapters.
  - Guests may use a subscription login, with a plain warning (A1), and Google models are allowed with a warning (A2).
  - The business model is parked (B1).

**Size:** 25-40 (file 11).

### Checkpoints and review

- **Two user checkpoints (decision U2):**
  - after M1, the slice;
  - after M3, which completes the re-founding: everything the old engine did, rebuilt on the new base, plus the slice
    and authoring.
- **@fable-reviewer reviews each milestone's landed changes,** but never anything it wrote itself (the liaison's rule).
  It wrote the designs in files 14-17, so @design-reviewer reviews WS-diverge's specification and the parts of the
  others built from those files.
- **Every landing is reviewed** by a second agent before it reaches main, as now.

## 2. What is kept, rewritten and archived

Line counts are of `.rs` files at 757219f [V].

### 2.1 Kept: the determinism base

| Path | Lines | What happens to it |
|---|---|---|
| `crates/box3d-fork` | 102,929 | **Kept.** WS-state adds incremental world images in M2 (file 11, 3.1). Excluded from the format and lint gates |
| `crates/pe-core` | 1,636 | **Kept.** WS-world widens object ids to 128 bits and makes a world's id the hash of its signed genesis (M1). `WorldPos` becomes the base of the script-facing `Pos` (file 17, M2). The cell index promotes past 64 bits (the format in M1, the rest in M4) |
| `crates/pe-physics` | 3,422 | **Kept.** WS-state moves the physics bench in from `spikes/physics`, runs the floating-point guard on the simulation thread, and in M2 wires one shared worker pool (D2). Its test records move from `spikes/out/vdw` into the crate |
| `crates/pe-sim` | 2,870 | **Kept and evolved** (about 500 lines of internals, file 04 A6). It gains: block copy-on-write with dirty bits and cached hashes; blob-backed variable-length fields; several components of one type per object; a persistence class; a registry versioned by frame |

### 2.2 Kept in part: ported into new crates, behind the seams

M0 moves pe-session (17,016 lines) to `archive/`. The parts below are ported from there; the rest stays archived.

| From pe-session | Lines | Ported to | Changed on the way |
|---|---|---|---|
| `sign.rs` | 456 | pe-log | Unchanged in substance; every hash is 256 bits |
| `session/entry.rs` | 924 | pe-log (the envelope), pe-world (edit and input payloads), pe-agree (protocol entries) | Payloads are typed per entry class. The governance, transfer and view-change payloads are not ported |
| `session/log.rs`: the chains and the order | about 700 of 1,037 | pe-log | An entry past a gap in its author's chain is a prediction only. It never becomes final until the chain validates in order: the d1 fix, by construction. The confirmation code (about 300 lines) is not ported |
| `session/sim.rs` | 823 | pe-replica | It builds bodies from world-model components, not three fixed scenes. Incremental images and background re-simulation come later |
| `session/fetch.rs` | 189 | pe-log | Unchanged in substance |
| `session/transfer.rs` | 431 | pe-agree, pe-world | Joining starts from a final cut; state is fetched as a subtree by hash |
| `session/replay.rs` | 183 | pe-harness | The offline replay behind the "live equals replay" property |
| `session/store.rs` | 204 | pe-world | Claims and cooldowns become world-model tables |
| `session/seam.rs` | 204 | pe-seams | An input to the new seams, not a port |

**Not ported:**
- `session/membership.rs` (2,620 lines), the view change and the governance rules. The rules carry over as requirements
  of governance as entries.
- `session/evict.rs` (317) and `session/check.rs` (36). The hash tree replaces the check.
- `session/mod.rs` (994), the 110-field `WorldSim`.
- `peer.rs` (1,208), rewritten in pe-agree.
- `session/indicators.rs` (341).
- `ownership.rs` and `ownership/` (4,050).
- `provenance.rs` (496).
- `session/r14.rs` (202).
- `lib.rs` (480).

**The other crates ported in part:**

| Path | Lines | What happens to it |
|---|---|---|
| `crates/pe-session-harness`, `tools/session-sim` | 7,868 + 6,509 | Moved to `archive/`. WS-harness ports into pe-harness the in-process network model, the scenario format, randsched and the scenarios that still mean something. Properties replace the golden-log comparison (`regress.rs`) |
| `crates/pe-mcp` | 4,059 | Moved to `archive/`. WS-api re-creates pe-mcp from its transports, tokens and error envelope (about 930 lines survive, file 06), with a new backend on pe-api and no dependency on any harness |

### 2.3 Rewritten

| Path | Lines | What happens to it |
|---|---|---|
| `crates/pe-app` | 2,976 | Moved to `archive/`. WS-app writes a new application crate under the same name. Clock sync keeps the fault-tolerant midpoint, drops the frequency loop, converges before frame 0, steers gradually (file 14, 4.2), and moves to pe-net. QUIC replaces `wire.rs` |
| `docs/design.md` and the protocol, region and MCP documents | 9,599 + others | Moved to `docs/archive/`. In their place: a short specification per subsystem in `docs/spec/`, one-page decision records in `docs/decisions/`, and the user's requirements in the user's own words in `docs/requirements.md` (file 09, A13) |

### 2.4 Archived

The archive point is a tag, `archive/v0-prototype`, on main just before WS-archive's landing: 757219f, if nothing lands
first. Everything archived stays readable at the tag.

| Path | Lines or files | How it is archived |
|---|---|---|
| `spikes/sim-core`, `spikes/session-sim` | 27,432 | Removed. Part B is not ported |
| `spikes/physics`, `p2p`, `wasm-hotswap`, `ballpit-viz`, `physics-in-wasm` | 4,368 | Removed, because each depends on `spikes/sim-core` [V]. The physics bench is ported into pe-physics first. ballpit-viz's wgpu code and wasm-hotswap stay readable at the tag, for WS-render and WS-logic |
| `crates/pe-session`, `crates/pe-session-harness`, `tools/session-sim`, `crates/pe-mcp`, `crates/pe-app` | 38,428 | Moved to `archive/`, outside the workspace and the gate, so their owners can port from them. Deleted at M3 |
| `spikes/out` | 2,333 files, 34 MB | Removed, except the records that kept tests read: `spikes/out/vdw` moves to `crates/pe-physics/tests/records/` [V: pe-physics's tests read it] |
| `scripts/` | — | Removed: the scripts that drive only archived code, for example `run_r18*.sh`, `run_session_sim.sh`, `regress_session_sim.sh`, `compare_*.sh`, `session_log_norm.py`, `scripts/p2-3/`, `run-p2p.sh`, `run_ballpit_viz.sh` and `run_perturbation_history.sh`. WS-archive checks each script's inputs. Kept: `audit_native.py`, the `publish_*` scripts, the determinism-matrix scripts, `rebless_physics_records.sh`, and the wasm and iteration-benchmark scripts |
| `spikes/chunks`, `wasm-guests`, `iter-bench`, `stack-audit` | — | Kept as references outside the workspace, as now (file 11, section 5) |
| The documents on the kept base: `vdw-simulation`, `parallel-physics`, `d58-double-precision`, `p2-1-core-sim`, `p2-2-physics`, `p2-6-p2-7-matrix`, `iteration-benchmark` | — | Kept in `docs/` |

After the archive, the workspace holds:
- the four kept crates;
- the skeletons of the new M1 crates: pe-seams, pe-world, pe-log, pe-agree, pe-replica, pe-net, pe-api, pe-mcp,
  pe-render, pe-xr, pe-diverge, pe-app and pe-harness;
- the model, `models/cuts`.

No old protocol code is built or gated in it.

## 3. Workstreams

### 3.1 Who owns what

Only a path's owner edits it.

| Workstream | Owns these paths | Its M1 deliverable | Later |
|---|---|---|---|
| WS-seams (@engine-architect) | `crates/pe-seams`; the root `Cargo.toml`; `docs/spec/00-overview.md`; the index of `docs/decisions/`; `docs/requirements.md` | Seams v0 (M0); a review of every seam change | New seam versions |
| WS-archive | Its one landing in M0 (section 2.4) | The archive | Deletes `archive/` at M3 |
| WS-ci (@provenance-eng's item) | `scripts/gate.sh`, `scripts/audit_native.py`, the secret scan, `.github/workflows/` | The gate (M0) | The tier-2 jobs each milestone adds |
| WS-model | `models/cuts/` | Model v0 (M0); v1 with join and leave (M1) | v2: one branch merging with the main line (M2); quorum loss, the one-minute bound and split groups (M3); per-group agreement across layers (M4) |
| WS-agree | `crates/pe-log`, `crates/pe-agree`, `docs/spec/agreement.md` | Cuts at fixed membership; admitting by entry; leave; join from a cut | Quorum loss with unconfirmed play, the one-minute bound and the divergence that follows it (with WS-diverge); the protocol side of member relays (M3) |
| WS-world | `crates/pe-world`, `crates/pe-sim`, `crates/pe-core`, `docs/spec/world-model.md` | The object tree, components and blobs; the hash tree over everything that matters (fields, governance, claims, from M2 logic state and module versions); edit entries with their three conflict rules; 128-bit ids; the coordinate format | `Pos` and `Offset` under the scripts (M2); the world record, keeping every entry from creation (M3); saves (M5) |
| WS-state | `crates/pe-replica`, `crates/pe-physics`, `crates/box3d-fork`, `docs/spec/replica.md` | The rollback loop over world-model bodies; replay from the last agreed cut (repair rung 1) | Incremental images, background re-simulation, the worker pool, skipping unchanged systems and islands (M2); a history-free physics state (later) |
| WS-net | `crates/pe-net`, `docs/spec/transport.md` | Direct QUIC connections from an invite; datagrams and streams; the rate budget v0; the clock with gradual steering; the version and self-test comparison at connection | Member relays and hole punching; the helper-node protocol (M3) |
| WS-api | `crates/pe-api`, `crates/pe-mcp`, `docs/spec/api.md` | WorldApi (schema, query, apply, watch, history); the MCP tools; `view.capture` | `fork.*`, `code.check` and `code.test` (M2) |
| WS-render | `crates/pe-render`, `docs/spec/render.md` | The window; the scene renderer, drawing from published snapshots; the display layer (file 14, 4.1); the desktop hand; offscreen capture | The UI lab (after M1); the UI layer and skin (M5) |
| WS-xr | `crates/pe-xr`, `docs/spec/xr.md` | The OpenXR session, stereo through pe-render, tracked hands into hand entries, 90 Hz pacing | Tracking for avatars (M5) |
| WS-app | `crates/pe-app` | The application: threads, wiring, invites, configuration, the demo script | The launcher (M5) |
| WS-harness | `crates/pe-harness`, with its scenarios and the probes | The network model, the scenario runner, the properties, the three probes, randsched | Long runs and distributions (M3) |
| WS-diverge | `crates/pe-diverge` and its tests; `docs/spec/divergence.md` | The repair ladder's rungs 1 and 2; the fault bundle; the counters; the display layer's requirements, handed to WS-render | The merge engine v1 (one branch against the main line, M2) and v2 (split groups, M3); rungs 3-5; the background self-check (M3) |
| WS-logic (M2) | `crates/pe-logic`, `crates/pe-sdk`, `docs/spec/logic.md` | — | M2, including activation after each machine's own compile, and `Pos` and `Offset` for scripts |
| WS-gov (M3) | `crates/pe-gov`, `docs/spec/governance.md` | — | M3, including guarded mode |
| WS-regions (M4) | `crates/pe-regions`, `docs/spec/regions.md`, the region simulator | — | M4 |

Rules:
- **A change to someone else's path** goes through its owner: ask, or propose a patch that the owner lands.
- **Manifests.** WS-seams creates every M1 crate's skeleton in M0, so nobody else edits the root `Cargo.toml`. Each
  crate's dependencies go in its own manifest.
- **Cargo.lock.** A conflict there is resolved by taking main's lock file and re-running a build on the branch.
- **Tests live in the crate they test.** Other crates use pe-harness as a library for their own tests.
- **Builds.** Each agent works in its own worktree with its own target directory (team rule of 2026-11-01), and runs
  builds through `heavy.sh`.
- **`archive/` is read-only** for everyone.

### 3.2 The seams

Six seams in `pe-seams` let every workstream code against them from the first week, with mocks for any side that is
not built yet. Appendix A sketches them; WS-seams fixes v0 in M0 with each owner.

| Seam | Between | What crosses it |
|---|---|---|
| S1 Entries | everyone | The signed envelope: author, sequence number, the previous entry's hash, domain id, frame stamp, class (input, edit, governance or protocol), typed payload, signature. Hashes are 256 bits and object ids 128 bits. The domain id is there from the start, so the regions work needs no format change: one domain per world until M4, then one per group of things in contact (file 16, C4) |
| S2 Replica | pe-agree and pe-replica, pe-world, pe-diverge | Step a frame with its ordered entries; save and restore a root; the root hash; each entry's outcome (applied, or refused with a reason) and any permission check at a frame; the objects that differ between two roots, for repair and merging |
| S3 Agreement | pe-agree and pe-app, pe-harness, pe-diverge | Submit a local entry; receive a message; tick. What to simulate now. The newest final cut. Events: voided entries with reasons; joins and leaves; quorum lost; unconfirmed time; diverged; merged |
| S4 Transport | pe-net and pe-agree, through pe-app | Send by class (protocol, action, pose, repair, bulk), in that priority; receive; statistics per link. A pose that cannot be sent is superseded, never queued |
| S5 Frame view | pe-replica and pe-render, pe-xr, pe-api | A read-only view published once per frame: objects, simulated and drawn poses, appearance, the events that trigger one-shot effects. It also carries the hand inputs from pe-xr and the desktop hand |
| S6 WorldApi | pe-api and pe-mcp, pe-app, and later the inspector and editor | Query, apply (a transaction, live or dry-run, later in a fork), watch with a cursor, and history, with fork and check from M2. Handlers read published snapshots and queue their transactions, so they never block the simulation (file 06, A1) |

- **Agreement and transport separate.** pe-agree knows the payload classes but nothing of what they mean, and the
  replica never decides finality.
- **Sans-IO.** pe-agree, pe-diverge and pe-net's protocol logic take time and messages as inputs and return outputs,
  so the harness can drive them deterministically with no sockets or clocks [I; quinn-proto is built this way].
- **Threads** are for WS-app to decide with WS-state, WS-render and WS-xr:
  - the simulation thread owns the replica and the agreement;
  - the render thread reads the published frame view;
  - a network thread runs QUIC;
  - MCP handlers run on their own threads, reading snapshots;
  - deep replays and merges run on a worker, on a second copy that is swapped in (file 14, 4.2).

### 3.3 Order of work

```
M0  WS-archive lands first (days 1-3)
    WS-seams v0 ──────────────┐           WS-ci: the gate (independent)
    WS-model v0               │           feasibility: QUIC, OpenXR + wgpu, wgpu window
                              ▼
M1  WS-world ──► WS-api ───────────────────────┐
       │   └───► WS-render (display layer) ──► WS-xr ──┤
       └───────► WS-state ─────────────────────┤
    WS-model ──► WS-agree ─────────────────────┼──► WS-app: integration ──► two-machine run
    WS-net ────────────────────────────────────┤
    WS-diverge: rungs 1-2, fault bundle ───────┤
    WS-harness: probes, properties ────────────┘
M2  WS-logic (needs WS-world's fields and WS-api's forks)
    WS-model v2 ──► WS-diverge: merge engine v1 (needed by Q3b)
    WS-state: incremental images, background replay;  WS-render: the UI lab
M3  WS-agree + WS-model + WS-diverge (quorum loss, the one-minute bound, split groups);
    WS-world (the world record); WS-gov (guarded mode); WS-net (relays)
M4  WS-regions (the simulator first)
M5  the platform tracks
```

- **Week 1:** every M1 workstream can start against `pe-seams` and mocks. An arrow marks where real code replaces a
  mock.
- **The likely critical path [I]:**
  - VR with rendering and the display layer, the longest single track;
  - the agreement core, which follows its model.
- **WS-agree's start.** It ports pe-log's chains at once, while the cut rules wait for model v0's check. So the code
  follows a checked model, not the other way round.
- **The merge engine** is modelled (v2) before it is built, in M2, and it is the same engine for:
  - a machine back from a long compile;
  - a machine back from a bad link;
  - split groups reconvening (file 14, 4.4).

### 3.4 Staffing notes (the coordinator's call)

- **Agents whose records fit [I]:**
  - WS-render: @ballpit-viz, who wrote the wgpu video renderer;
  - WS-state: @vdw-physics, who did the fork's van der Waals work;
  - WS-net: @shader-lang, who did pe-app, the clock and R18;
  - WS-ci: @provenance-eng, already on the landing gates;
  - WS-harness, WS-agree, or WS-regions in M4: @world-chunks, who did P2-3, the session harness and repair.
- **Reviewers:**
  - the model, agreement and WS-diverge: @design-reviewer;
  - the other workstreams: @chunks-reviewer and @shader-reviewer;
  - each milestone: @fable-reviewer.
- **Build slots will bound throughput.** About thirteen agents share `heavy.sh`'s two slots. Iterate with
  `cargo check -p <crate>`, keep full builds for the gate, and leave the platform matrix to hosted CI.
- **Windows and the headset need a signed-in session.** Place WS-render and WS-xr in one (Session 0 has no desktop);
  their headless tests render offscreen.

## 4. Test strategy

### 4.1 Principles

1. **Properties instead of blessed logs** (file 09, A11). Every scenario checks:
   - all honest replicas have equal roots at every final cut;
   - a live replica equals an offline replay of the final log;
   - no final frame is ever undone;
   - once faults stop, progress resumes within a stated bound;
   - no input from any member aborts a replica: bad data is refused with a reason, and its author is reported;
   - live memory stays bounded over long sessions, while the full change record is kept.
2. **The model before the code.**
   - The agreed-cut rules, and then the merge rules, are written as an executable model and checked exhaustively at
     small sizes before pe-agree and pe-diverge depend on them (file 11, 3.3 and 3.9).
   - The model then checks the code: pe-agree's event traces from random schedules are replayed through the model's
     transition rules, and any step the model forbids fails the test.
3. **A control that bites, for every safety rule:** a mutation that turns the rule off, which the checker or a test
   must catch. A rule whose control passes is not tested.
4. **What the user feels is tested too.** The display layer's error and correction counters are measured in every
   network scenario.
5. **Determinism across platforms** on hosted runners, after every landing (tier 2).
6. **Measurements are distributions** over at least 20 seeds and 5 delays, with every player acting, with loss-only
   congestion, and at 90 Hz (file 01, A7).
7. **One implementation.** No spike re-implements the protocol (file 09, A12).

### 4.2 The review's probes become tests

| Probe | What it showed on the old code | The test of the new code | Where |
|---|---|---|---|
| probe-d1 (`appendix/evidence/probe-d1-unlinked_confirm.rs`) | A member withholds chain entry k+1, then sends k+2, back-dated to frame 97 (at or before its own declared horizon), past the hole. Honest member V confirms frame 100 with that entry in effect, and aborts when the hole fills: "would roll back to frame 97, below its confirmed frame 100" [V file 01, `probe-d1.log`] | The same schedule against pe-agree. V may simulate k+2 as a prediction. No honest member votes for a cut that includes it until k+1 is held and the chain validates in order. Once k+1 arrives, every member judges k+2 void by the same rule. No final frame rolls back, nothing aborts, and every replica ends equal to the others and to the replay. Also the joiner variant: a pending joiner at admission (`membership.rs:2294-2297` in the old code) | A pe-harness test; the model's validity property; the mutation "vote without chain validation", which must give a counterexample |
| probe-capture-close (`probe-capture-close.toml`) | In a default session where everyone is an admin, member 4 defines a role with no permissions, gives it to members 1-3, and goes silent. The session is left with no admin and closes for everyone [V file 07, `probe-g3-capture-close.log`] | **Public template:** member 4 joined without admin rights, so its DefineRole and SetRole are refused ("no permission"). The roles are unchanged, the three carry on, and all replicas are equal. **All-admin variant** (every member made an admin): no admin is reachable, so the ruled path for that case follows. In a public world it is guarded mode, the owner's countdown, then closing with the state saved (mb09ab8952a38), and the owner can restore the roles. For a private world, WS-gov's specification states whether the same applies or the session closes with its last admin, as m716adebd0591 has it. Whether demoting every other admin needs a guard of its own is WS-gov's proposal, as @item:primordial-engine/governance-defaults-strangers-start-without-admi asks. It goes to the user if it changes a ruling | A pe-harness test, written in M0 and failing until M3 |
| probe-twosilent5-kick (`probe-twosilent5-kick.toml`) | Five members; two go silent at 3 s; admin 1 kicks both at 6 s. No decision: 616 paused frames at 15 s [V file 07, `probe-h1-twosilent5-kick.log`] | **The three are never frozen:** zero paused frames, and their actions are marked unconfirmed. The kick is an ordinary entry under the strict 2/3 quorum (Q5, no exception), so it stays undecided without the two. **If the two return within a minute,** the lines merge automatically. **If not,** at one minute the three continue as their own world line, where finality resumes, and both lines can be re-merged later. **Controls:** stopping play below quorum must bring back the paused frames, and a merge that drops one side's actions must fail the merge property | A pe-harness test; model properties and their mutations |

### 4.3 Other defects the review verified, as tests of the new code

| Defect | Test |
|---|---|
| No leave entry: every exit pauses everyone for about 1.5 s, and a two-player session stalls for good (file 07, H2; file 01, defect 7) | A member that quits authors a leave, and the others drop it at the next cut with zero paused frames. In a pair, the survivor never stops |
| Invariant checks abort the process (file 01, defect 5) | Property tests feed arbitrary bytes and entries from one member. No replica panics, and the member is reported |
| The log is never pruned (file 01, defect 6) | A two-hour session with eight members: live memory stays bounded, and entries below the last final cut move to the world record (never deleted, the user's instruction of 19:09Z) |
| Nothing bounds one member's entries (file 01, defect 8) | A flooding member's entries beyond its rate are refused, and memory stays bounded |
| 64-bit hashes where votes sign (file 01, medium) | By type: `pe-seams` has only 256-bit hashes |
| Governance sits outside the hashed state, so a joiner cannot verify it (file 07, H3; file 14, F5) | A lying member hands a joiner a wrong ban list; the joiner catches it against the cut's root |
| A sync-check mismatch is only logged (file 01; file 04, defect 4) | A divergence is injected in one object on one replica. The hash tree finds that object, and repair rungs 1 and 2 restore equality (M1). Rungs 3-5 follow in M3 |
| The congestion controller read an oracle (file 01, defect 10) | Network scenarios run through pe-net's real congestion path; the harness has no oracle input |
| The world cannot be edited (file 04, defect 1) | M1's edit tests |
| Different engine versions or a faulty physics build in one session (file 14, F1 and F6) | The connection guard refuses the join, with the reason (M1) |
| "Every peer compiles" sat on everyone's critical path (file 03, defect 2) | Activation after each machine's own compile: nobody waits (M2) |

### 4.4 The model of agreed cuts, and of merging

The mechanism, from file 11 (3.3):
- Every few frames, the next member in a seeded rotation proposes a cut: a frame and, for each author, the last entry
  included.
- Members vote with one hash in their next acknowledgement, and a strict 2/3 quorum makes the cut final.
- Entries inside a final cut are final. An entry stamped before the cut and not in it is void for everyone.
- The cut carries the state hash, so it is also a checkpoint.

**The rules below quorum:**
- Play continues unconfirmed.
- At one minute the line diverges.
- Lines that meet again within the minute merge by file 14's rules (4.4): one order by frame stamp, ties broken by
  the author's key, and the engine's ordinary rules in replay.
- A machine more than 2 s behind merges with the main line by the same rules (Q3b).

**The tool:** Stateright, a model checker written in Rust (decision C3) [I that it fits; confirmed in M0]. TLA+ is the
alternative.

**Scope, by version:**
- **v0 (M0):**
  - fixed membership of three or four members;
  - one faulty author (it equivocates, withholds or back-dates), or one silent member;
  - messages delayed, reordered, lost or duplicated;
  - a few cuts deep.
- **v1 (M1):** admitting by entry, leave, join from a cut.
- **v2 (M2):** one branch merging with the main line: a long compile, or a bad link.
- **v3 (M3):** quorum loss, unconfirmed play, the one-minute bound, split groups and their merge.

**Safety properties:**
- Agreement: no two honest members finalize different cuts at the same height.
- Validity: a final cut includes only entries whose chain validates in order, and the void set is the same everywhere.
- No final frame is undone.
- **Merge determinism:** the same entry sets give the same merged order and outcomes, whatever the arrival order.
- **Merge keeps work:** an action survives the merge unless the ordinary rules would have refused it with no split.
- **Voiding and merging fit together:** entries made during a split are never voided by the tolerance rule merely
  because they were apart [I; to design in v3].
- S6, from the widened random-schedule work: on clean links, an honest member never votes to remove or demote an
  honest member that has not crashed.

**Liveness, bounded:**
- With a quorum honest and connected, a cut becomes final within a bounded number of proposer turns.
- Below quorum, no honest member's play stops, and the line diverges at one minute.

**Controls that bite:**
- voting without chain validation (the d1 defect);
- stopping play below quorum (the h1 freeze);
- accepting a proposer's cut without checking it;
- ordering a merge by arrival;
- voiding the entries made during a split.

### 4.5 The scenario suite

- **Sorting.** WS-harness sorts the 120 old scenario files [V count]:
  - kept and judged by the properties: those that are still meaningful schedules (faults, the ball pit, bans, silent
    members, the D80 admin rules);
  - left in the archive: those that test removed mechanisms (thin-link levels and Part B, view-change internals, the
    ownership book, provenance, cross-world certificates).
- **randsched,** with its widening, generates random Byzantine schedules against pe-agree, with the spike's seed
  budget.
- **The divergence catalogue** (file 14, section 2: P1-P9, S1-S4 and F1-F8) gets one scenario per row, judged by the
  properties and by the display layer's counters.

### 4.6 The gate

One command, `scripts/gate.sh`, in two tiers.

**Tier 1, on every landing** (locally, through `heavy.sh`, before the merge):
- the format check, and clippy with warnings denied, on every crate except `box3d-fork`;
- `cargo test` on the workspace;
- the model's fast profile: three members, minutes;
- the harness's fast set of properties, and the probes;
- the native-code audit with its allowlist, and the secret scan (@provenance-eng's item);
- the existing publication check.

**Tier 2, on hosted runners (Q12),** on the public repository after each push, and nightly:
- the whole gate on windows-x64, linux-x64, linux-arm64 and macos-arm64;
- cross-platform determinism: each runner writes digests (physics records, replica replays, and from M2 logic fuel and
  trap points), and a final job requires them to be bit-identical;
- the model's deep profile (four or five members), randsched seeds, long runs, the network distributions.

A failed tier-2 run blocks the next landing until it is fixed or explained (decision C7).

**Graphics in CI.** Render tests run headless with a software adapter (WARP on Windows, lavapipe on Linux) and compare
captures within a tolerance [I]. OpenXR's frame loop runs against a simulated runtime, where one is available [I]. Feel
in the headset is the user's to judge, at the checkpoints.

## 5. Carry-over of the user's rulings

### 5.1 The rulings of 2026-11-10

| The user's ruling | Where it lands | When | Tested by |
|---|---|---|---|
| The governing principle: local-first and immediate; others eventually consistent; the local user held up as rarely as possible; "no player should ever wait for other players' actions to finish" (mfc7e0ddc4137, mf7daea6446ca) | A rule for every workstream. Each place where a user can be held up (file 11, 3.0) gets a stated delay budget and a measurement | Every milestone | The delay distributions of each milestone |
| 1. The vertical slice comes first (Q1) | M1 | M1 | M1's demo and tests |
| 2. One quorum mechanism replaces unanimous confirmation, the view change and Part B, keeping the user's rulings (Q2) | pe-agree (agreed cuts); `models/cuts` | The model in M0, the code in M1, complete in M3 | Section 4.4 |
| 3. Code activation (Q3, mf7daea6446ca): the owner runs a change at once, and every other machine rewinds and replays when its own compile finishes. Objects keep running the old code, then jump (Q3a). A divergence over 2 s becomes a world-line merge with main (Q3b). No machine runs another's build (Q3c) | pe-logic, with the merge engine of pe-diverge | M2 | M2's activation tests |
| Standalone, compiler-less headsets: excluded from the prototype; any later support comes through our own deterministic cross-platform compiler, not follow-by-state; own-PC builds are moot (mb09ab8952a38) | Not built (section 6) | — | — |
| 4. Regions (Q4): only what is near the player simulates. The radius depends on the object's size relative to the player's; a frozen area's state lives in the world record and peer caches (mf7daea6446ca); the simulation distance follows the Open World Orchestrator (m5adc22d1388d) | pe-regions, as in M4: layers as the loading index, per-machine chunk sets, frozen chunks, arrival entries instead of scans, agreement per group in contact. The domain id is in every entry from M1. The values still open are parameters (section 6) | M4; the format from M1 | M4's tests |
| 5. The removal quorum stays 2/3, strictly, with no kick exception (Q5) | pe-agree: a strict 2/3. Below quorum, play continues unconfirmed and reconvenes by merge (the 18:58Z instruction), so nobody is frozen | M3 | probe-twosilent5-kick |
| Unconfirmed play below quorum lasts one minute at worst, then the session diverges (m35138bafc46a). The full change record is kept from creation, and diverged worlds may be re-merged later by choice (19:09Z) | pe-agree and pe-diverge (the bound); WS-world (the world record); M5 (re-merging later) | M3; re-merging in M5 | The one-minute and record tests (M3) |
| A delete against an add-on on two branches is decided by the order of events: delete first, the add-on is orphaned; delete second, everything is destroyed (m35138bafc46a) | pe-diverge's merge: the ordinary replay in event order | M2-M3 | A merge scenario for each order |
| 6. Public worlds start strangers without admin rights (Q6 of the first round) | pe-gov: the public template's joining role has no admin permissions. Governance is ordinary entries over the hashed world state | M3 | probe-capture-close |
| No admin reachable (Q6 of the follow-ups, mb09ab8952a38): guarded mode, then a countdown the owner sets (30 s by default), then the world closes with its state saved | pe-gov | M3 | The guarded-mode tests |
| 7. Carrying between worlds is copy then remove; a rare duplicate is acceptable; no lease (Q7) | M5's worlds track: the carrier creates the object in the destination as an ordinary permitted edit, then removes it from the source once a cut in the destination holds it | M5 | A crash mid-carry leaves at most a duplicate, never a loss |
| 8. QR (b): only the owning player's machine runs their per-player script copy (Q8) | pe-logic's per-player mode. It runs once per eligible player, on that player's machine, on predicted state, and its effects on shared state are that player's signed inputs. Three mitigations stay within the ruling (file 11, 3.5): an effect names the versions it read and is refused if they are stale; each effect is declared "once for the world" or "once per player"; per-player state may live in the player's profile store | M2 | M2's per-player tests |
| 9. Helper nodes are optional and never assumed (Q9) | pe-net: an invite carries keys and reachable addresses; any member can act as rendezvous and relay; the helper protocol is written down, and the node list ships empty | Direct in M1; relays in M3 | Every test runs with no helper |
| 10. Hosted CI runners are allowed (Q12) | Tier 2 of the gate | M0 | — |
| "explore the variety of ways that this line can diverge, and systems we should design to help return it back promptly and with as little visibility or impact on the user's experience as possible" (18:56:50Z) | File 14's five systems: the display layer (WS-render), bounded rollback (WS-state), the hash tree and the repair ladder (WS-world, WS-diverge), the merge (WS-diverge), the guards (WS-net, WS-diverge) | M1-M3 | The divergence catalogue's scenarios |
| "when the worlds reconvene, treat it like a git merge where every action is a commit to the main tree, but with deterministic rules for resolving it whenever possible" (18:58Z) | pe-diverge's merge engine, modelled first | M2-M3 | Merge determinism; "merge keeps work" |
| Precision-safe coordinates for code at extreme distances (19:14Z) | pe-sdk: `Pos` and `Offset` types, object-local frames, the far-origin test and the checker (file 17), on pe-core's `WorldPos` | M2 | The far-origin test |
| Legal (mbe87e750ca35): guests may use a subscription login with a warning (A1); Google models with a warning (A2); the business model parked (B1); no contact with YDMS (R1); simple Resonite-style protections for received avatars and objects (V1) | M5's agent-panel and people tracks | M5 | — |
| Q10 and Q11: remove the Resonite note; remove the user's e-mail from the public history | Outside the rewrite: @item:primordial-engine/public-repo-cleanup-remove-the-resonite-note-and; the research in @item:primordial-engine/research-avatar-licences-and-ai-providers-terms-o | Running | — |

### 5.2 Earlier rulings the new design keeps

All of the user's earlier requirements still apply (m83bc23ef13ec). These are the ones the rewrite touches most.

| The ruling | Where it lands |
|---|---|
| R1: no host and no central authority | Proposers rotate and hold no authority: they propose, the others verify, and a silent one is skipped. "Main" in a merge is the quorum's line, not a host |
| R2: a fixed timestep, independent of frame rate | pe-core's tick clock; pe-render interpolates. M1 tests display rates against tick rates |
| R3: a deterministic local simulation with rollback; traffic only on interaction | Kept. Entries flow only on interaction; cut votes flow while anyone acts |
| R4: Box3D; Rust and wgpu | Kept |
| R5: the soonest interaction wins. Same-frame order follows the seeded rank (D37), and only grabs claim | pe-log's order and claims, unchanged. The same order decides merges |
| "An author never sees its own interaction corrected" (m21f5bdac80b3); an interaction carries the new state with a canonical frame (m84a607d3a701) | pe-replica, unchanged; the display layer never blends a player's own hands or what they hold. The review's proposal to carry state as a check (file 02, B1) was never put to the user and is not adopted |
| R6: sync checks, and repair from the earliest frame | The hash tree over objects (pe-world) finds what differs; the repair ladder (pe-diverge) fixes only that |
| R7: live edits, source control, rebase, the 10 s seat | One edit entry with a base version (pe-world). A stale one is refused with its reason, and the seat is the path that refusal takes. Fields in M1; code in M2 |
| R8: MCP for state, logic and shaders | WorldApi and the small tool set (pe-api, pe-mcp) |
| R10: full VR | pe-xr, in M1 |
| R11 and the rulings on logic, shaders and assets as objects: copy-on-write ("b, 1 always"); linked scripts only when set up explicitly; scope U; plain Delete unlinks; an in-game editor, but not the main way | pe-world (M1) and pe-logic (M2), from @item:primordial-engine/logic-shaders-and-assets-as-objects-unified-hier |
| R14: several worlds, portals | M5 |
| R15: replaceable physics | Hooks as systems, from M2. Whole-engine replacement is stated with its real limit: about 50-200 active bodies with rollback (file 02) |
| R16 and R17: huge worlds; culling by size; avatar culling | M4, with the coordinate ruling (m7e85960c864a) |
| R18: detect collapse and recover; keep congestion with the thin player | Covered by cuts, the link budget and the merge, in M3:<br>- a slow member leaves the quorum and follows by state at cuts;<br>- a member diverged for more than 2 s merges with the main line;<br>- each link gets its own rate budget in pe-net;<br>- late actions past the tolerance are void, decided by the cut, and the engine sets the tolerance from measured links;<br>- "fewer than 0.01% dropped on acceptable connections" is measured |
| The premise: distributed verification of a change record (m3c8512748b8c, answered "all current") | Cuts are the verified record, and the full record is kept. The order comes from the authors' frame stamps; the earlier change wins a conflict; each scale keeps its own record |
| Trust players; admins kick (the trust survey) | Hardening covers crashes and corruption only; griefing is the admins' to handle. Equivocation is the one anti-cheat (file 14, 4.5) |
| D80 and the admin rules: admins plus 2/3; admins are equal; a role that can assign roles counts as an admin; kicking an admin needs its own permission; the last admin cannot release the role; admins may kick and ban while paused; a session closes with its last admin (m716adebd0591) | pe-gov (M3), as rules over the hashed state. In public worlds the guarded-mode rule decides how a session without an admin closes |
| Perma-bans are saved with the world (m5c4b84d186a8) | Fields of the world's access list, written by ordinary governance entries (M5, saves) |
| D11: every peer compiles from source | Kept, and now with its timing: each machine compiles for itself and activates at the agreed frame (Q3) |
| D12: "wasmi now, Cranelift later" (mc164bb1fac63) | The review recommends Cranelift from the first logic build (file 03): wasmi misses the logic budget 6 to 10 times, and re-simulation multiplies it. M2 needs the user's answer (decision U3) |
| D13: WGSL for now | Kept |
| 100% Rust, with operating-system, driver and runtime boundaries loaded dynamically (md35ba688f6b4) | QUIC with pure-Rust cryptography; OpenXR's loader loaded at run time, never compiled in; the audit as a gate; one register of boundaries (file 09, A8) |
| D2: parallel physics, bit-exact; D58: double precision; van der Waals | Kept in the fork; the worker pool is wired in M2 |
| D4: the 90 Hz target; D50: the memory and CPU slider | 90 Hz is measured in M1. The slider's positions become policies over incremental images (M2) |
| D23: eight players for the MVP, aiming higher | M3 tests eight; regions (M4) are the way beyond |
| The sync model's user answers (v11): the synchronized line; publication is automatic and never the player's concern (m27cdafe8fcfa); QR (b); QH2 (c); a held paddle visible only to its holder (Q13 a) | Fields and their modes in pe-world (M1-M2), and pe-logic (M2). The org's proxy records give way to a held shared body visible per player (decision C9) |
| The cross-scale answers: Q6 (c), delayed reads; Q8 (d), a giant's peers host native-scale objects; "readings ok" | M4: delayed images couple the layers where needed. How Q8 (d) meets the OWO's loading rule is part of file 16's open points |
| The look for now; the Gnosarium name and theme; the built-in harness ("likely opencode") | M5. The look becomes a skin over a plain layout (file 08, A1). The harness is chosen by a short spike against a first-party agent loop, which needs an API key |

## 6. Open with the user, and excluded

Q3, the one point the coordinator listed as out of scope, has since been answered (mf7daea6446ca), and M2 plans it.

**Still open with the user, kept as parameters, none blocking M1:**
- **Simulation distance, from file 16:**
  - C1, the distance: the OWO's ratio (about 70 m at player scale), 1 km, or the OWO's ratio as a default each world
    may change;
  - C2, how the player's scale counts: a window of layers, or the ratio taken literally;
  - C3, whether a far player may show a large object from the near players' agreed state;
  - C5, whether an object's layer follows its current size or is fixed at placement.

  M4's simulator runs across all of them.
- **Merging, from file 14 (section 7):**
  - for long divergences merged later, may the later change win, object by object, where an exact replay would take
    too long (M5)?
  - is a number wanted for visible error (a target for the display layer's tests)?
- **Script positions, from file 17 (section 6):**
  - a hard wall around absolute numbers;
  - whether the far-origin test blocks a commit or only warns;
  - the default local frame.

  M2 builds them with the reviewer's defaults behind settings.
- **A reasonable timeframe** (file 11, 3.0): the user's number. Every milestone reports the measured delays meanwhile.

**Excluded from the prototype:** standalone headsets and other machines without a compiler (mb09ab8952a38). The design
keeps nothing that would make them impossible later. Their path is the project's own deterministic cross-platform
compiler, which this plan does not schedule.

None of these is built before it is answered. Each one's place in the plan is noted where it lands.

## 7. Triage of the docket

There are 25 live items (not archived); I read them on 2026-11-10 at 19:02Z [V]. The coordinator counted 27; if two
more exist, I can't see them. The last column is where the item goes, and why.

| # | Item | Status, owner | Verdict | Where, and why |
|---|---|---|---|---|
| 1 | @item:primordial-engine/re-found-primordial-engine-above-the-determinism | in_progress, coordinator-opus | Carry | The rewrite's umbrella item. Give each workstream its own child item |
| 2 | @item:primordial-engine/primordial-engine-p2p-deterministic-rollback-gam | blocked, engine-architect | Carry | The product item, holding the user's requirements since 10-30. This plan replaces its phase plan. Its blocked reason (the capacity pause of 11-02) is stale: make it in_progress, with the re-founding as its current phase |
| 3 | @item:primordial-engine/landing-gates-run-the-native-code-audit-and-a-se | open, provenance-eng | Carry | M0, WS-ci: the gate for the restructured workspace, after WS-archive lands; tiers 1 and 2 |
| 4 | @item:primordial-engine/public-repo-cleanup-remove-the-resonite-note-and | open, shader-lang | Carry as it is | Ordered by the user (Q10, Q11) and independent of the rewrite. Make its one force-push before the archive landing is first published, so that history is published once |
| 5 | @item:primordial-engine/research-avatar-licences-and-ai-providers-terms-o | in_progress, gnosarium-ux | Carry as it is | Feeds M5: avatars, the agent loop's API key, the warnings the user asked for (mbe87e750ca35) |
| 6 | @item:primordial-engine/full-independent-review-of-primordial-engine-fab | deploy_ready, coordinator-opus | Carry, to done | Delivered, and the user has answered its questions. Close it once the coordinator judges its follow-ups (files 12-17) complete |
| 7 | @item:primordial-engine/critical-an-unvalidated-entry-can-be-confirmed-t | backlogged, world-chunks | Carry | M0-M1: probe-d1 as a test of pe-agree, and a mutation in the model (section 4.2). The old code stays unfixed |
| 8 | @item:primordial-engine/governance-defaults-strangers-start-without-admi | backlogged, ballpit-viz | Carry | M3, WS-gov: strangers without admin rights; guarded mode when no admin is reachable; capture-close. Its kick fix is superseded: the user ruled a strict 2/3 with no exception, and the three survivors are freed by unconfirmed play and the one-minute divergence. Its two probes are written as tests in M0 |
| 9 | @item:primordial-engine/session-validation-a-member-moves-only-its-own-h | deploy_ready, engine-architect | Carry | M1: "an input moves only its author's own hands" becomes a validity rule in pe-world, with its test. The old branch's code never lands; supersede this item with WS-world's |
| 10 | @item:primordial-engine/verified-governance-for-state-transfer-joiners-b | backlogged, coordinator-opus | Carry | M3: governance lives in the hashed world state, so a joiner checks it against the cut's root. Its lying-donor test carries over |
| 11 | @item:primordial-engine/clock-sync-frequencies-converge-before-frame-0-s | backlogged, coordinator-opus | Carry | M1, WS-net: keep the fault-tolerant midpoint, drop the frequency loop, converge before frame 0, and then steer gradually (files 01 and 14). Restate its acceptance for pe-net |
| 12 | @item:primordial-engine/r16-chunk-coordinates-i64-backed-extending-to-ar | backlogged, coordinator-opus | Carry | The format in M1 (WS-world), `Pos` for scripts in M2 (file 17), the rest in M4 (WS-regions): a cell index that promotes past the 64-bit range and never saturates (m7e85960c864a) |
| 13 | @item:primordial-engine/logic-shaders-and-assets-as-objects-unified-hier | backlogged, coordinator-opus | Carry | M2, WS-logic and WS-world: its user rulings are requirements of the logic specification, and its open points are answered there, under systems over the store |
| 14 | @item:primordial-engine/owner-s-client-writes-perma-bans-into-the-world-s | backlogged, coordinator-opus | Carry | M5, saves: perma-bans become fields of the world's access list, written by ordinary governance entries. PB-1 and PB-2 follow from the log's order |
| 15 | @item:primordial-engine/port-the-randsched-widening-and-s6-to-pe-session | backlogged, engine-architect | Carry, re-scoped | M0-M1, WS-harness and WS-model: the widened randsched runs against pe-agree, and S6 becomes a property of the model. Nothing is ported to pe-session |
| 16 | @item:primordial-engine/gnosarium-the-flagship-game-on-the-primordial-en | blocked on the user, coordinator-opus | Carry as it is | The game's identity. The user has parked the business model (mbe87e750ca35); the interface direction feeds M5 |
| 17 | @item:primordial-engine/ball-pit-video-matrix-re-rendered-on-the-current | deploy_ready, ballpit-viz | Carry as it is | Built and approved; delivering it is the liaison's. Later videos come from pe-render, one per milestone, with the commit printed on them (file 09, A15) |
| 18 | @item:primordial-engine/localize-congestion-to-the-thin-link-player-othe | blocked, engine-architect | Park | Part B is not ported. Its goal, that nobody stalls because of one slow link, is an M3 acceptance test: the slow member follows by state at cuts, or merges with the main line after 2 s. Its plan stays on `docs/partb-pe-plan` (222d887), and the spike at the archive tag |
| 19 | @item:primordial-engine/admin-indicators-holding-the-session-and-implaus | blocked, provenance-eng | Park | Its remaining parts need Part B. The idea, showing who is holding the session, carries into M3's display of the session's state (file 08, A6), next to the "not yet agreed" mark |
| 20 | @item:primordial-engine/pe-app-feed-r18-a-link-signal-delay-gradient-est | blocked, shader-lang | Park | QUIC's congestion control and a rate budget per link replace the R18 level ladder and its estimator. Its clock-drift findings go to WS-net |
| 21 | @item:primordial-engine/r16-neighbourhood-loading-and-provisional-arriva | deploy_ready, world-chunks | Park | The OWO loading rule (M4) supersedes it. Its spike's measurements are cost data (file 13, B.7) |
| 22 | @item:primordial-engine/kickliar-fed2-2-player-survivor-s-confirmation-t | backlogged, coordinator-opus | Park | A defect of Part B and the old core. The property it breaks (a survivor's finality never waits on removed members) becomes an M3 test of cuts |
| 23 | @item:primordial-engine/r18-controller-retune-for-the-3-player-thin-cell | backlogged, coordinator-opus | Park | Tuning of the replaced level ladder |
| 24 | @item:primordial-engine/thin-links-space-an-idle-member-s-acks-while-d56 | backlogged, coordinator-opus | Park | Tuning of the replaced level ladder (D56) |
| 25 | @item:primordial-engine/trading-lease-only-one-live-instance-of-a-world-t | backlogged, engine-architect | Drop | The user ruled "there is no lease" (Q7, md2fa23dfa774). Park it instead, if old items should be parked rather than dropped even here |

## 8. Risks, and the evidence that settles each

| Risk | What settles it | When |
|---|---|---|
| QUIC without ring or aws-lc may not build, or the pure-Rust cryptography may be young | The feasibility check: quinn-proto with rustls and a pure-Rust provider builds, passes the native audit, and carries a datagram and a stream between two processes, on Windows and Linux | M0 |
| OpenXR with wgpu needs Vulkan interop below wgpu's public API, and the loader must be loaded at run time | The feasibility check: an OpenXR session (the user's runtime on Windows; a simulated one in CI if possible) shows a stereo frame drawn by wgpu | M0 |
| The display layer can lie: a blended object is drawn where it is not (file 14, 6) | Thresholds tuned with a person in VR at the M1 checkpoint; the error counters in every scenario | M1 |
| Finality on bad links: under cuts it depends on a proposer's view of a quorum (file 11, 9) | Measured as distributions in the shapes of the old thin-link cells | M3 |
| Merging physics after a split: a contact group may spread so far that a merge replays a whole pile (file 14, 6) | Model v2 and v3 come before the code. A small experiment comes before the merge is promised. The one-minute bound caps the replay at 3,600 frames | M2-M3 |
| A merge, or an activation after a slow compile, may need a long replay that blocks a frame | Replays run on a second copy in the background and are swapped in (file 14, 4.2) | M2 |
| Rollback cost at 90 Hz in VR: a full keyframe costs about as much as a step (file 02) | M1 measures the ball pit at 90 Hz; incremental images follow in M2 | M1-M2 |
| Builds that differ across machines break activation after each machine's own compile (file 03: identical output shown only for six small modules on two x86-64 machines) | Tier 2 compares module bytes from the same source on all four platforms; a mismatch is a fault the hash tree catches, repaired by rungs 1-2 | M2 |
| Two objects of nearly equal size in adjacent layers (files 11 and 16) | M4's measurement with a person's hands, before the build | M4 |
| About thirteen agents behind `heavy.sh`'s two slots | Iterate with `cargo check -p`; full builds in the gate; the matrix on hosted CI. The coordinator sets the slots | Throughout |
| Agents in Session 0 have no desktop, for windows or the headset | Place WS-render and WS-xr in a signed-in session; headless tests render offscreen | M0-M1 |
| The model's state space at five members with splits and merges | Small bounds, symmetry reduction, and a deep profile run nightly | M0-M3 |
| Determinism has never run on ARM or macOS (file 02) | Tier 2 in M0 | M0 |

## 9. Decisions this plan needs

**For the user**, through the coordinator and the liaison. I don't settle these myself.

| # | Decision | My proposal |
|---|---|---|
| U1 | What "edit through an agent" covers in the slice | World edits (create, delete, set fields, appearance) in M1, and logic authoring in M2. The alternative, a small logic edit in M1 too, adds about 6-9 engineer-weeks before the first checkpoint |
| U2 | What "on finishing" means, for @fable-reviewer's review and the user's checkpoint | Checkpoints after M1 (the slice) and M3 (the re-founding complete); @fable-reviewer reviews each milestone's landings |
| U3 | D12: "wasmi now, Cranelift later" (the user's own words) | Cranelift from M2's first build, with metering injected into the module, as file 03 recommends. Activation after each machine's own compile replays logic on every machine, which makes the interpreter's cost felt more |
| U4 | The second machine for the slice, and whether it has a headset | The user's choice. The slice needs only a desktop window on the second machine |

**For the coordinator:** engineering choices the org owns. Each gets a decision record once ruled.

| # | Decision | My proposal |
|---|---|---|
| C1 | The crate map and who owns what | Section 3.1 |
| C2 | How to archive | Tag `archive/v0-prototype`; the five crates being ported from go to `archive/` until M3; the rest is removed from the tree; `spikes/out` is removed except the records kept tests read; the old documents go to `docs/archive/` (section 2.4) |
| C3 | The model checker | Stateright (Rust), so no new toolchain and the model's types can mirror pe-agree's. TLA+ is the alternative |
| C4 | The renderer | Write a small wgpu renderer for M1, with a short design document first; evaluate adopting one before M5 (file 09, A5) |
| C5 | Transport | quinn-proto with rustls and a pure-Rust cryptography provider, subject to M0's check. Datagrams for live entries; streams for repair, state and assets |
| C6 | OpenXR | The `openxr` crate with the system loader loaded at run time. OpenXR's loader and runtime go in the register of boundaries |
| C7 | The gate's two tiers | Tier 1 locally before each landing; tier 2 on hosted runners after each push. A red tier 2 blocks the next landing |
| C8 | When the merge engine is built | One engine in pe-diverge, modelled first: v1 (one branch against the main line) in M2, because Q3b and bad links need it; v2 (split groups) in M3 |
| C9 | The sync model's proxy records (org-designed) | Replace them with a held shared body visible per player (file 04, A3, tier 1). Private dynamics only if the user asks for them |
| C10 | The fork and the format and lint gates | Exclude `box3d-fork` |
| C11 | Agent edits (D28a was a default) | Agent edits apply at once, labelled as the agent's and revertible in one step; confirmation stays available as an option |
| C12 | The UI lab | Right after M1, in parallel with M2 (WS-render) |
| C13 | pe-session's kept parts | Port them into new crates, rather than keep the crate in place, so the old core is never built in the new workspace |

## Appendix A. A sketch of the seams (v0 is fixed in M0)

A sketch, not code: names and shapes only. WS-seams fixes v0 with the owners.

```rust
// S1 Entries
pub struct Hash(pub [u8; 32]);            // BLAKE3; the only hash type
pub struct ObjectId(pub u128);            // world-scoped
pub struct DomainId(pub u64);             // one per world until M4; then one per group of things in contact
pub enum Class { Input, Edit, Governance, Protocol }
pub struct Entry {
    pub author: MemberId, pub seq: u64, pub prev: Option<Hash>,
    pub domain: DomainId, pub stamp: Frame, pub class: Class,
    pub payload: Bytes,                   // decoded by the class's owner
}
pub struct SignedEntry { pub entry: Entry, pub sig: Signature }

// S2 Replica: one domain's deterministic state machine
pub trait Replica {
    fn step(&mut self, frame: Frame, ordered: &[SignedEntry]) -> Vec<Outcome>; // Applied | Refused(reason)
    fn save(&mut self) -> Root;           // cheap: structural sharing
    fn restore(&mut self, root: &Root);
    fn root_hash(&self) -> Hash;
    fn permits(&self, who: MemberId, what: Permission) -> bool;      // governance is in the hashed state
    fn differing(&self, other: &Hash) -> Vec<ObjectId>;              // for repair and merging
}

// S3 Agreement: sans-IO
pub trait Agreement {
    fn submit(&mut self, local: Entry) -> SignedEntry;
    fn receive(&mut self, from: PeerId, msg: &[u8]) -> Result<(), Fault>;
    fn tick(&mut self, now: Frame) -> Vec<Outgoing>;
    fn to_simulate(&self, from: Frame) -> Vec<&SignedEntry>;   // final + unconfirmed, in agreed order
    fn final_cut(&self) -> &Cut;
    fn events(&mut self) -> Vec<Event>;   // Voided, Joined, Left, QuorumLost, Unconfirmed(time), Diverged, Merged
}

// S4 Transport: sans-IO queues over QUIC
pub enum Lane { Protocol, Action, Pose, Repair, Bulk }   // priority order
pub trait Transport {
    fn send(&mut self, to: PeerId, lane: Lane, bytes: Bytes);   // an unsent Pose is superseded
    fn poll(&mut self) -> Option<(PeerId, Bytes)>;
    fn link(&self, peer: PeerId) -> LinkStats;
}

// S5 Frame view: published once per frame, read-only
pub trait FrameView {
    fn frame(&self) -> Frame;
    fn objects(&self) -> &[ViewObject];   // id, simulated pose, drawn pose, shape, appearance
    fn effects(&self) -> &[EffectEvent];  // keyed by the causing event, so a replay never repeats them
}

// S6 WorldApi
pub trait WorldApi {
    fn schema(&self) -> Schema;
    fn query(&self, q: Query) -> QueryResult;
    fn apply(&self, tx: Transaction, mode: Mode) -> ApplyResult;   // Live | DryRun | (M2) InFork
    fn watch(&self, filter: Filter, cursor: Cursor) -> Changes;
    fn history(&self, q: HistoryQuery) -> History;                 // and revert
}
```

## Appendix B. Where file 14's divergence catalogue lands

File 14 (section 2) lists the ways the world line diverges. Each row gets a scenario (section 4.5).

| Rows | Kind | Systems that bring it back | Owners | Milestone |
|---|---|---|---|---|
| P1, P2 | Late remote actions; guessed remote hands | Rollback and replay; the display layer's blending; dead reckoning extended to orientation | WS-state, WS-render, WS-net | M1 |
| P3, P4 | A lost grab; a stale edit | The soonest wins; a soft release with a cue; a refusal naming the winner, with an offer to re-apply | WS-world, WS-render, WS-api | M1 |
| P5, P9 | An action dropped as late; clock drift | Gradual clock steering; the drop told plainly | WS-net, WS-render | M1 |
| P6, P6b | Someone's code change compiling here (Q3) | Replay from the agreed frame; past 2 s, the world-line merge with main | WS-logic, WS-diverge | M2 |
| P7 | Entering an area whose state has not arrived | The last known state at once; fetch only what differs; fade in | WS-regions, WS-render | M4 |
| P8 | A per-player script acted on a guess later undone | Effects name what they read and are refused if stale | WS-logic | M2 |
| S1 | A slow member far behind | It leaves the quorum and follows by state at cuts; past 2 s, it merges with the main line | WS-agree, WS-diverge | M3 |
| S2, S3 | A split; too few for a quorum | Unconfirmed play for up to a minute, then a git-style merge or a divergence; re-merging later by choice | WS-agree, WS-diverge, WS-world | M3 (re-merging, M5) |
| S4 | A crash and return, or a new joiner | State at a cut plus the recent log; watch first, act when caught up | WS-agree, WS-world | M1 (joining), M3 |
| F1, F3, F6 | Physics or builds that differ; different engine versions | Guards at connection and start-up; the hash tree; the repair ladder; the fault bundle | WS-net, WS-diverge, WS-world | M1-M2 |
| F2 | A logic module that behaves differently on two machines | Metering in the module's bytes; logic state and module versions in the hash | WS-logic, WS-world | M2 |
| F4 | Memory or disk corruption | The background self-check; the repair ladder | WS-diverge | M3 |
| F5 | State outside the hash | Nothing that matters stays outside: governance, claims, logic state and module versions are all hashed | WS-world | M1-M2 |
| F7 | Equivocation, or false state for a newcomer | Two versions of one entry prove against their author; newcomers fetch by a quorum-signed hash | WS-agree | M1-M3 |
| F8 | A protocol defect | The model first, and properties; no aborts | WS-model, WS-harness | M0 onward |

## Appendix C. Sources

- Review: `_teams/1847/fable-review/` files 00-17, and `appendix/evidence/` (the probes and their logs).
- The user's rulings: `_shared/user-design-decisions.md`, cited by message id above.
- Docket: the 25 live items listed in section 7, read on 2026-11-10.
- Repository: main 757219f; `Cargo.toml`; `crates/`, `spikes/`, `tools/`, `scripts/`, `docs/`.
