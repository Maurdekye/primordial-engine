# Re-founding plan: Primordial Engine above the determinism base

Status: the build plan, 2026-11-10. Written by @engine-architect for @coordinator-opus. Nothing in it is built.
Base: main 757219f, the review's base. The plan has been revised twice:
- after the user's answers to its four questions (mc40fc6379bc6) and to the review's later questions;
- after @design-reviewer's read and the coordinator's rulings on its thirteen decisions (2026-11-10 20:05Z and
  after);
- with the user's last answers of the design phase (merging, frozen places, carried state as a check, no world
  owner).

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
  - 16, simulation distance as the Open World Orchestrator does it, through section 6.7;
  - 17, coordinates for script authors;
  - 18, no world owner: every admin is a co-owner.
- The user's decisions and the coordinator's rulings, in `_shared/user-design-decisions.md` to the end of the design
  phase on 2026-11-10 (its last entries are the no-world-owner rulings), and the earlier rulings that still apply.
- @design-reviewer's read of the first version (b610464), `design-reviewer/read-refound-plan-b610464.md`.
- The repository at 757219f: crate sizes and dependencies, which I counted for this plan.

Marks: **[V]** I verified it in the repository or the named record; **[V file N]** @fable-reviewer verified it (review
file N) and I did not re-run it; **[I]** my inference or judgement. Every size is soft: engineer-weeks for one
experienced engineer, as in file 11. Read them as relative sizes. Review, build slots and integration bound the
calendar time, not writing.

## 0. The plan on one page

- **The vertical slice comes first.** M1: see, touch in VR and edit through an agent, between two machines. The
  agent edits both the world and its logic, and each machine runs a logic change after its own compile (the user's U1
  and Q3). Then:
  - M2, authoring in depth: an agent builds a toy end to end, and the merge engine arrives;
  - M3, sessions that survive real life: join, leave, splits and merges, thin links, guarded mode, eight players;
  - M4, regions under the Open World Orchestrator's loading rule;
  - M5, the platform.
- **M0 comes before M1, and it is short.** It lays the ground every workstream needs: the archive, one small interface
  crate (the seams), the gate on hosted runners, the first model of agreed cuts, and four feasibility checks.
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

  The merge is one pure function. The model checks it before it is built, in M2, and it is extended to split groups
  in M3.
- **Seventeen workstreams.** Each owns its own paths, and six seams join them. Fifteen of the workstreams are needed
  for M1, staffed by twelve agents (section 3.4).
- **Tests are properties, not blessed logs.**
  - The agreed-cut rules are written first as a model that a checker exhausts at small sizes.
  - The review's three probes are tests from the first week.
  - Every safety rule has a control that bites: a mutation that a test must catch.
- **Every ruling of the user's carries over.** Section 5 says where each lands. The design phase is closed. The four
  points still open with the user, all from file 18, are parameters, not blockers (section 6).
- **All 27 live docket items are triaged:** 17 carry and 10 park.
- **Every decision the plan asked for has been made.** The user answered U1-U4, and the coordinator ruled C1-C13 with
  conditions (section 9).

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
- The cuts model checks exhaustively at three and four members.

**Work:**
1. **The archive** (WS-archive): tag, move and delete, as listed in section 2.4. It lands first.
2. **Seams v0** (WS-seams): the `pe-seams` crate with the six seams (section 3.2), and a skeleton of every M1 crate
   compiling against it.
3. **The gate** (WS-ci, @provenance-eng's item @item:primordial-engine/landing-gates-run-the-native-code-audit-and-a-se):
   - `scripts/gate.sh`;
   - the native-code audit with an allowlist;
   - a secret scan;
   - a hosted workflow.
4. **Model v0** (WS-model, @item:primordial-engine/m0-model-v0-of-agreed-cuts-in-stateright-ws-mode): agreed cuts at
   fixed membership, with the quorum formula and the fault model per size of section 4.4.
5. **The harness skeleton** (WS-harness): the deterministic network model and the scenario runner, retargeted at the
   seams. The three probes are written as tests; they fail until the core exists.
6. **Feasibility checks,** a few days each:
   - **QUIC** with pure-Rust cryptography (WS-net). The report names the cryptography provider's crate and its audit
     status (C5's condition).
   - **OpenXR** with the system loader loaded at run time, drawing through wgpu (WS-xr). The user's runtime is Virtual
     Desktop's (m2aaafe98cbbe), and each test on the real headset needs a go through the liaison.
   - **A wgpu window** drawing the ball pit from a world snapshot (WS-render).
   - **Cranelift** (WS-logic): wasmtime with its one C file patched out on Linux and macOS builds as pure Rust and
     passes the audit, and fuel and call-depth metering injected into a module gives the same stopping points on all
     four platforms.

**Acceptance:**
- `gate.sh` exits non-zero on any of these, and passes on the restructured main:
  - a planted native dependency, or a planted secret;
  - a formatting or lint failure;
  - a failing test, or a failing model check.
- The hosted runs on windows-x64, linux-x64, linux-arm64 and macos-arm64 pass pe-core's, pe-sim's and pe-physics's
  tests, and the physics records match on all four. A mismatch is a finding: until it is fixed, M1's two machines must
  share a platform.
- **The model checks exhaustively:**
  - agreement, validity and "no final frame is undone";
  - at three members, with crashes and author faults (withholding, back-dating);
  - at four members, with one Byzantine member.
- **The model's controls must each give a counterexample:**
  - **the d1 mutation,** in which a member votes for a cut without validating each author's chain in order;
  - **a quorum one too small** (q − 1), which must give split-brain.
- Each feasibility check reports go or no-go, with evidence. A no-go comes back to the coordinator, with alternatives,
  before any M1 work depends on it.

**Size:** about 9-14 in all, mostly in parallel:
- the archive, 0.5-1;
- the seams, 1;
- the gate, 1-2;
- the model, 2-3;
- the harness skeleton, 1-2;
- the four feasibility checks, 1-1.5 each.

### M1. The vertical slice: see, touch in VR, edit the world and its logic through an agent, between two machines

**Demo** (a script, recorded on video with logs):
1. **A creates a world.** On machine A, the user's PC with a headset through OpenXR, A creates a world from the
   ball-pit template: a floor, walls, 500 balls, a table and two lamps. The world is data in the world model, not a
   hard-coded scene.
2. **B joins.** Machine B, the user's second PC, joins from an invite:
   - the invite carries A's key and reachable addresses;
   - the connection is direct QUIC, over a LAN or the user's own VPN, with no helper nodes;
   - B uses a desktop window only, and VR runs on A (U4);
   - before B joins, the two machines compare their engine versions and a short physics self-test.
3. **Both see the same world.** Both machines draw it with wgpu. The headset runs at its own rate (90 Hz target), and
   the simulation steps at a fixed rate whatever the display rate (R2).
4. **Both touch it.**
   - A's tracked hands push and grab balls; hand entries now carry position, rotation and grip.
   - B sees it within about one network delay.
   - B's desktop hand, a mouse ray, does the same.
   - When both grab one ball, the earlier grab wins, and the loser's ball slides out of its grip with a short cue
     (R5; file 14, 4.1).
   - Corrections to what the other player touched are blended by the display layer. A player's own hands and what
     they hold are never corrected.
5. **An agent edits the world.** An agent (Claude Code over MCP, on A) is asked to "put a red cube on the table", "make
   the balls blue", "double the cube" and "remove the left lamp".
   - It reads the world (`world.query`), changes it in one transaction (`world.apply`), and looks at the result
     (`view.capture`).
   - Both machines show each change.
   - An edit made against a stale version is refused, with its reason and the change that won.
6. **The agent edits its logic** (U1). It writes a small script, for example "the right lamp blinks while a ball rests
   on the table".
   - `code.check` compiles it to wasm and validates it. Machine code comes from Cranelift, with metering injected into
     the module; it never runs on wasmi.
   - The agent commits it as an edit with a base version and an agreed frame.
   - A runs it at once.
   - B keeps the old behaviour and compiles the module from source with its own toolchain, holding its votes on cuts
     at or past the agreed frame until its build is ready. Then it rolls back to the agreed frame and replays, and its
     lamp jumps once to the new behaviour (Q3, Q3a).
   - No machine runs another's build (Q3c).
   - M1 builds this inside-the-window half of activation only. A compile that outlasts the window ends in a merge
     with main (Q3b), which comes in M2 (the coordinator's ruling, 20:22Z).
7. **Repair.** A deliberate fault is injected on B: one ball's state is altered. The next cut's hash tree finds that
   ball, repair rungs 1 and 2 restore it (file 14, 4.3), and a fault bundle is written.
8. **B leaves,** and A carries on alone with no pause.
9. **Throughout,** both machines' world roots match at every final cut. An offline replay of the log reproduces the
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
  Its control: with the check removed, the forge test must fail.
- **World edits:**
  - create, delete, set a field and reparent work;
  - a transaction applies all or nothing, and a dry run changes nothing;
  - a stale precondition is refused, with a reason that names the winner;
  - every refusal is reported over MCP.
- **Carried state is a check, not an overwrite** (B4; the user, mfedf5a6c3c5b). After re-simulation, an
  interaction's carried state is compared with the simulated one. It replaces it only when they differ for a reason
  other than an entry not yet held. Two players pushing one ball within one network delay both take effect.
- **The logic edit:**
  - the same module bytes give the same fuel and trap points on all four hosted platforms;
  - a machine never loads a module it did not compile itself, and no module runs on wasmi;
  - the owner never waits;
  - B's root equals A's at the first cut after B's compile;
  - a rollback across the swap equals a replay.
- **The compile window (measured, gated)** (the coordinator's ruling, 20:26Z):
  - M1's logic edit compiles in under 2 s on the second PC, so rollback inside the window covers it;
  - if it does not, the edit is made smaller; the merge does not move out of M2.
- **The display layer:**
  - the drawn state converges on the simulated state within the stated blend time;
  - no correction offset is ever applied to a player's own hands, or to what they hold;
  - a sound or particle effect caused by an event fires once, even when the event is replayed.
- **What the hash covers.** Everything that matters is inside it: world fields, governance, claims, logic state and
  module versions. A divergence injected into any one of them is found by the hash tree and repaired by rungs 1 and 2.
- **The guards.** A peer with a different engine version, or a failing physics self-test, is told why before it
  joins.
- **The agent.** `view.capture` returns an image; headless tests render with a software adapter. The agent's whole
  script runs in CI against the two-process session.
- **Rendering.** The display runs at 72, 90 and 120 Hz against a simulation at 60 and 90 Hz, and the simulation never
  depends on the frame rate.
- **VR, in CI:** the OpenXR frame loop runs against a simulated runtime, where one is available [I].
- **VR, on the headset** (by hand, recorded, on the Virtual Desktop runtime, each run with a go through the liaison):
  - the frame time at 90 Hz with 500 balls and one remote hand at 60 ms one-way delay;
  - dropped frames, and judder under rollback;
  - the user's verdict on hand feel: hands physical only when engaged (D18b), beside hands always physical within
    reach.
- **Two physical machines** (by hand, once per checkpoint): the demo's script, with both machines' roots logged at
  every cut.
- **The user's convergence targets, on good links** (meef4f248923b, answer 8), as distributions over 20 seeds and 5
  delays:
  - the other machine sees an action within 0.25 s;
  - no correction larger than a hand's width is shown in view without a cue.
- **Measured and reported, not yet gated:**
  - how long before an action is final;
  - edit to first frame on each machine, for the logic edit;
  - the display layer's counters: corrections per minute, mean visible error, deepest rollback, rungs used.

**Size:** about 39-56 in all [I]:
- the renderer and VR with hands, 8-12 (file 11's slice 1, without the UI lab, which follows M1);
- the display layer, 1-2;
- the world model v1 with its hash tree, WorldApi and MCP, 8-11 (file 11's slice 2);
- the logic edit, 6-9 (U1): Cranelift with injected metering, a first kind of system over the store, code commits,
  activation inside the window, `code.check`;
- agreement for M1, 4-5; transport for M1, 3-4; the replica, 2-3;
- repair rungs 1 and 2, the guards and the fault bundle, 2-3;
- the harness, the probes and the properties, 3-4;
- integration, 2-3.

### M2. Authoring in depth: an agent builds a toy end to end

**Demo:**
- **The agent builds a toy.** It writes a small toy in Rust: a button that spawns a ball each time it is pressed, a
  bin whose lamp turns green while a ball is in it, and a score kept in a component field.
  - `code.test` runs it in a fork with random rollbacks.
  - The agent commits it, and it runs as systems over the store on every machine.
  - The inspector shows the score field.
- **The merge engine v1.**
  - A third member (a process on either PC) diverges: its compile outlasts the window, or its link does, while the
    other two finalize.
  - It merges back by a three-way merge at main's final frontier, under the witnessed-order rule the user
    confirmed (mfc2bbd9d28ad; section 4.4).
  - This completes Q3b.
- **A per-player script, under QR (b):** a lamp that is red only for A opens a door, and the door's opening is A's
  signed input.
- **Native declarative components** (copy, map, smooth, timer, follow, look-at): the agent attaches one with no build
  at all.
- **Positions for script authors** (file 17, as the user answered it, m06d2643578d7):
  - a position is the exact `Pos` type, which cannot be turned into plain numbers;
  - the only way to absolute numbers is `pos - ORIGIN`, where `ORIGIN` is a world constant;
  - exact integer cells (`pos.cell(level)`) stay available, since integers cannot lose precision (the coordinator's
    reading, 20:17Z);
  - a saved offset must name what it is relative to, and offsets carry their space;
  - an object's own position, rotation and scale are relative to its parent, and everything else a script is handed
    is in the object's own space, as in Resonite;
  - the type system alone enforces this: there is no test and no warning for authors.

**Acceptance:**
- **Code swaps:**
  - a swap with an unchanged schema needs no migration;
  - a system whose inputs did not change is skipped on a re-simulated frame, and the result is unchanged.
- **The merge engine v1:**
  - it is built only after model v2 has checked it;
  - no final frame is undone;
  - the returning machine's work is kept wherever the deterministic rules allow, and every action that loses says
    why;
  - a witnessed action keeps its stamp, and an unwitnessed action never overturns something already final;
  - a component attached to an object that was deleted first is refused, and its author is told;
  - every member ends with the same root.
- **The per-player mode:**
  - an effect that names a stale version it read is refused;
  - a script declared "once for the world" gives one effect, not one per player.
- **Script hygiene:**
  - hidden state in statics is caught by the author-side test (random rollbacks and instance resets), as a structured
    error;
  - the position types hold: code that takes plain numbers from a `Pos` other than by `pos - ORIGIN`, saves a bare
    offset, or mixes offsets of two spaces does not compile (compile-fail tests of the SDK itself).
- **Measured:** logic cost at 10,000 entities against the 1 ms budget.

**Size:** about 11-17 [I]:
- the rest of the logic runtime (forks, `code.test`, schema changes, the toy end to end), 2-3;
- the per-player mode, 2-3;
- the script-author positions, 2-3 (file 17);
- the native declarative components, 2-3;
- the merge engine v1, 3-5.

**After M1, in parallel with M2:**
- WS-render builds the UI lab (file 08, A2). It draws the launcher, one dense surface and one agent panel, with the
  look's parameters tunable live by an agent.
- WS-state adds incremental world images and background re-simulation.

### M3. Sessions that survive real life

**Demo:** five players on at least three machines, in a public world.
- A stranger joins without admin rights (Q6 of the first round). Its attempt to strip everyone's roles is refused
  (probe capture-close).
- A player leaves, with no pause.
- **Two players drop at once.**
  - The other three keep playing, and their actions show as not yet agreed.
  - The quorum stays a strict 2/3, with no kick exception (Q5, mf7daea6446ca): at five members q = 4, so an admin's
    kick of the two cannot be decided without them.
  - If the two return within a minute, the lines merge automatically.
  - If not, at one minute the two sides become git-style branches (meef4f248923b, answer 5). Each re-founds its own
    membership, with a line id, and gets finality back under its own quorum.
  - A side with an admin is kept by its admins. A side with no admin lasts only while its players play (file 18).
  - The branches may be merged later, by choice (M5). This is probe twosilent5-kick.
- **The network splits three and two.**
  - Both sides keep playing.
  - When they reconvene within a minute, the histories merge by the deterministic rules of file 14 (4.4). A deletion
    against an add-on is decided by the order of events (m35138bafc46a).
  - Corrections show only where the rules could not keep both sides' results.
- **Every admin becomes unreachable** in the public world.
  - "Every admin unreachable" is a recorded fact, decided at a cut: no admin entry in the agreed record for the set
    time.
  - The world then enters guarded mode with a visible notice, and a countdown starts (the owner sets it; 30 s by
    default).
  - When the countdown runs out, the world closes with its state saved (mb09ab8952a38). Whether anything is kept
    when no admin is present at all is open with the user (file 18, question 3).
  - Below quorum there is no cut to decide the fact on, so it is decided on the line that re-founds its membership at
    the one-minute mark [I; WS-gov's specification states it].
- **Thin links.**
  - A player whose link has diverged for more than 2 s merges back with the main line, as in Q3b.
  - A player on a 16 kbit/s link keeps its vote; nobody demotes it.
  - Cuts wait for a quorum, not for that player. Its own view follows by state at cuts, and the others never pause.
- **Late joining.** A late joiner starts from a cut, watches until it has caught up, then acts. It verifies the
  governance it is handed.
- **An admin returns** after playing alone (ma9035760e0c1; file 18, 2.7). Reopening the world opens their own
  version. Another admin has it open, so the client joins them and merges, unless the returning admin chooses to
  stay forked.
- **Relays.** Two players behind strict routers connect through a third member acting as relay; no helper node exists.

**Acceptance:**
- probe-twosilent5-kick and probe-capture-close pass (section 4.2).
- **Merge determinism:** every member computes the same merged history and root from the same entry sets, whatever
  the arrival order. It is a property of the model and of the harness.
- **The one-minute bound.**
  - Unconfirmed play never lasts longer than one minute.
  - At the bound, each line re-founds its membership, and every member knows which line it is on.
  - Memory and replay time stay within what one minute needs: 3,600 frames at 60 Hz (file 14).
  - Control: with the bound raised, the bound test must fail.
- **The full change record.** Every entry since the world's creation is kept, together with signed states at
  intervals. History below a final cut leaves live memory for the world record and peer caches, and is never deleted
  (the user, 19:09Z). The admins keep it; a non-admin's copy is discarded when they leave (file 18).
- **Witnessed order:** a back-dated, unwitnessed action from a lone member never overturns a final action. Its
  control: honouring the unwitnessed stamp must fail the property.
- **No world owner** (file 18): every admin keeps their own version as they play; there are no ownership
  certificates; the old owner rules are gone; D87's version-control merging is kept.
- **Guarded mode:**
  - the countdown starts and stops at cuts, the same on every machine;
  - the risky rights are suspended for the remaining players;
  - the saved state is the state at closing.
- **Thin links,** in the shapes of the old thin-link cells (8-32 kbit/s downlinks, 2-8 players; the cell list moves
  with the harness):
  - the others never pause;
  - the thin player keeps its vote;
  - the thin player's delays and merges are reported.
- **Eight players at 90 Hz, every one acting.** Finality and visibility delays are reported as distributions.
- **The user's convergence target on poor links** (meef4f248923b, answer 8): others see an action within 1 s.
- **The repair ladder:**
  - rungs 3-5 (reload a cell, rejoin, quarantine) each have a test;
  - a background self-check replays a random recent stretch and compares it with the recorded hashes.
- **Fault isolation:**
  - no byte string from one member aborts a replica (property tests);
  - a flooding member's excess entries are refused, and memory stays bounded. Control: with the rate limit removed,
    the memory test must fail.
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
  mf7daea6446ca); a non-admin's cache lasts only for the session (file 18).
- **Missed time** (meef4f248923b, mfedf5a6c3c5b; file 16, 6.7). When a player returns to a frozen place, the time
  it missed is re-simulated, up to one minute after it froze, on a metered separate thread. The limit is a world
  setting, and one of its values is "no limit". The first machine to return computes it, and the result is agreed
  like any state.
- **Notice of arrivals.** An arrival comes as an entry in the log of a chunk already loaded, never from a scan.
- **The unit of agreement.** Agreement and simulation run per group of things in contact, across layers, not per
  cell (file 16, C4). The domain id that every entry carries from M1 names that group.

**The user's answers to file 16** (m1d4a95c6d4bd, ma27c73c50fd4, maa60620a6ced and mbcc1c5fd8d39; file 16,
sections 6.1-6.6):
- **The distance (C1 = c):** the OWO's ratio is the default, about 64-80 m for person-sized things, and each world
  may change it.
- **The player's scale (C2 = yes):** it selects a window of layers, 4 below to 30 above. It does not stretch the
  reach of ordinary objects; this replaces the literal-ratio reading of mb09ab8952a38.
- **Every object has a nonzero simulation size** (maa60620a6ced; file 16, 6.5). This replaces the layer floor:
  - it is one length, always greater than zero, and it decides the object's layer;
  - it has no effect on collision or on how the object looks;
  - every object is created with a default size: 1 m times the maker's scale (m8c42b4b093bd);
  - it is never below the object's real extent, and it may be set larger, up to the world's maximum;
  - a saved object's size is recorded relative to the player who saved it, and it spawns at the same size relative
    to the player who spawns it;
  - the size follows the object when it is scaled;
  - parts take the layer of the whole assembly.
- **Simulating and watching are a spectrum, not two states** (mbcc1c5fd8d39; file 16, 6.6, which replaces the switch
  of 6.2 and 6.4): "You simulate everything you have loaded. Things you have not loaded can still act on it, and
  those actions are sent to you."
  - **Producers:** the machines that hold both ends of an influence record its effect as an entry: an impulse at a
    frame, or a field set at a frame. For a script that reads something elsewhere, the entry carries the values read.
  - **Consumers:** a machine that lacks the source applies the entry at its frame, like a late remote input. A machine
    that holds both ends ignores it.
  - **What counts as an influence** (ma27c73c50fd4; file 16, 6.4): contact, a script elsewhere writing, a script
    reading elsewhere, a joint across layers. Links are recorded at run time by the machines that hold both ends.
  - **Recognition:** each linked group carries its lowest layer in the shared state, so a machine knows entries are
    owed to it.
  - **The check:** the holders' agreed state at each cut. A machine whose own result differs takes the agreed state.
  - **State instead of entries:** the holders send the object's state when that is smaller than the entries. It is
    an economy, not a mode.
  - **No fixed delay:** the display layer shows an object slightly in the past only when its corrections are
    frequent, by measurement.
  - **Things nobody has loaded** do not run and influence nothing.

**Before anyone builds on the spectrum:** WS-state measures how closely a replayed contact impulse reproduces the
holders' result (the coordinator's ruling, 19:36Z).

**First, the evidence** (file 11, section 6):
- a two-dimensional protocol simulator with real player movement, run at the default ratio and at 1 km;
- a measurement, with a person's hands, of two things of nearly equal size in adjacent layers;
- the replayed-impulse measurement above.

**Demo:**
- The world is several kilometres across.
- Two players far apart simulate disjoint sets of chunks.
- A ball thrown out of everyone's range freezes when nobody has it loaded. When a player returns, the minute it
  missed is re-simulated, so it has landed.
- Player B carries a ball back into A's range, and A learns of it from an arrival entry.
- A player walking into an area takes its state from those already there.
- Large objects simulate and draw from farther away than small ones.
- A far player has a ship loaded but not the crates on its deck. The far player simulates the ship, and the crates'
  contact arrives as influence entries. When the far player pushes the ship, it responds at once.
- A small control box out of a far player's range steers a large gate; the box's writes reach the far player as
  entries.
- An invisible script holder has the default simulation size and is simulated by every ordinary player in range.

**Acceptance:**
- **No polling:** counters show zero scans, and a player far from everything sends nothing about it.
- **Hand-overs:** an object moving between contact groups is handed over at a cut.
- **Coordinates** go past the 64-bit range without saturating (user ruling m7e85960c864a).
- **The ratio** is a world setting; the tests pass at the default and at 1 km.
- **Influences:**
  - each one arrives as an entry and is applied at its frame;
  - a machine that holds both ends ignores it;
  - every root stays equal, and switching between entries and state changes nothing in the result.
- **Sizes:** every object has a nonzero simulation size, an object created without one gets the default, the size
  follows scaling, and parts take the layer of the whole.
- **Missed time:** a frozen place re-simulated on return gives the same state on every machine, within the world's
  limit, without stalling a frame.
- **A cost budget per machine,** measured at the default ratio and at 1 km.

**Size:** 4-6 for the skeleton and the evidence, then 10-16 (file 11).

### M5. The platform

Each track writes its short specification before its code.
- **Saves, backups and merges of world copies,** derived from the world model and the world record. A save is a
  signed, named root, kept by the world's admins (file 18). A world with one admin lives on one machine, so a backup
  under the admin's control (a second machine of theirs, or an export) must be easy.
- **Re-merging later, by choice.** Two branches that diverged more than a minute ago can be merged at any later time
  (19:09Z), with the full record and the same rules (meef4f248923b, answers 6 and 7; file 14, 8.2):
  - an admin of each branch consents, after a preview computed in a copy;
  - the exact merge is the default;
  - a fast merge ("the later change wins, object by object") is an option, suggested for long histories.
- **Several worlds and portals.** Carrying between worlds is copy then remove (Q7).
- **People:** identities, invites and live vouching; bans, and perma-bans saved with the world by its admins.
  Received avatars and objects get simple Resonite-style protections (V1, mbe87e750ca35).
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

- **Two user checkpoints** (the user's answer to U2, mc40fc6379bc6):
  - after M1, the slice;
  - after M3, which completes the re-founding: everything the old engine did, rebuilt on the new base, plus the slice
    and authoring.
- **@fable-reviewer reviews each milestone's work as it lands,** but never anything it wrote itself (the liaison's
  rule). It wrote the designs in files 14-17, so other reviewers take the parts built from them:
  - WS-diverge's specification and code: @design-reviewer, with @shader-reviewer as its second reviewer;
  - the parts of the others built from files 14-17: @design-reviewer.
- **Every landing is reviewed** by a second agent before it reaches main, as now.

## 2. What is kept, rewritten and archived

Line counts are of `.rs` files at 757219f [V].

### 2.1 Kept: the determinism base

| Path | Lines | What happens to it |
|---|---|---|
| `crates/box3d-fork` | 102,929 | **Kept.** WS-state adds incremental world images (file 11, 3.1). Excluded from the format and lint gates |
| `crates/pe-core` | 1,636 | **Kept.** WS-world widens object ids to 128 bits and makes a world's id the hash of its signed genesis (M1). WS-world also owns the script-facing position types: `Pos`, the typed offsets and `ORIGIN`, built on `WorldPos` (file 17; used from M2). The cell index promotes past 64 bits (the format in M1, the rest in M4) |
| `crates/pe-physics` | 3,422 | **Kept.** WS-state moves the physics bench in from `archive/spikes/physics`, runs the floating-point guard on the simulation thread, and wires one shared worker pool (D2). Its test records move from `spikes/out/vdw` into the crate |
| `crates/pe-sim` | 2,870 | **Kept and evolved** (about 500 lines of internals, file 04 A6). It gains: block copy-on-write with dirty bits and cached hashes; blob-backed variable-length fields; several components of one type per object; a persistence class; a registry versioned by frame |

### 2.2 Kept in part: ported into new crates, behind the seams

M0 moves pe-session (17,016 lines) to `archive/`. The parts below are ported from there; the rest stays archived.

| From pe-session | Lines | Ported to | Changed on the way |
|---|---|---|---|
| `sign.rs` | 456 | pe-log | Unchanged in substance. Every hash is 256 bits, and the signed bytes bind the world id |
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
| `spikes/p2p`, `wasm-hotswap`, `ballpit-viz`, `physics-in-wasm` | 2,662 | Removed, because each depends on `spikes/sim-core` [V]. ballpit-viz's wgpu code and wasm-hotswap stay readable at the tag, for WS-render and WS-logic |
| `crates/pe-session`, `crates/pe-session-harness`, `tools/session-sim`, `crates/pe-mcp`, `crates/pe-app`, `spikes/physics` | 40,134 | Moved to `archive/`, outside the workspace and the gate, so their owners can port from them; `spikes/physics` is there for WS-state's port of the physics bench. Deleted at M3 |
| `spikes/out` | 2,333 files, 34 MB | Removed, except the records that kept tests read: `spikes/out/vdw` moves to `crates/pe-physics/tests/records/` [V: pe-physics's tests read it] |
| `scripts/` | — | Removed: the session, r18, thin-link, p2p, ballpit, chunks and regress scripts. Moved to `archive/scripts/`: the determinism-matrix, par-bench, rebless and guest-build scripts, which drive the archived physics bench; they come back with WS-state's port. Kept: `audit_native.py`, the `publish_*` scripts, the P2-1 and P2-2 test scripts, and the wasm and iteration-benchmark scripts |
| `spikes/chunks`, `wasm-guests`, `iter-bench`, `stack-audit` | — | Kept as references outside the workspace, as now (file 11, section 5) |
| The documents on the kept base: `vdw-simulation`, `parallel-physics`, `d58-double-precision`, `p2-1-core-sim`, `p2-2-physics`, `p2-6-p2-7-matrix`, `iteration-benchmark` | — | Kept in `docs/` |

After the archive, the workspace holds:
- the four kept crates;
- the skeletons of the new M1 crates: pe-seams, pe-world, pe-log, pe-agree, pe-replica, pe-net, pe-api, pe-mcp,
  pe-render, pe-xr, pe-diverge, pe-logic, pe-sdk, pe-app and pe-harness;
- the model, `models/cuts`.

No old protocol code is built or gated in it.

## 3. Workstreams

### 3.1 Who owns what

Only a path's owner edits it.

| Workstream | Owner | Owns these paths | Its M1 deliverable | Later |
|---|---|---|---|---|
| WS-seams | @engine-architect | `crates/pe-seams`; the root `Cargo.toml`; `docs/spec/00-overview.md`; the index of `docs/decisions/`; `docs/requirements.md` | Seams v0 (M0); a review of every seam change | New seam versions |
| WS-archive | @engine-architect | Its one landing in M0 (section 2.4) | The archive | Deletes `archive/` at M3 |
| WS-ci | @provenance-eng | `scripts/gate.sh`, `scripts/audit_native.py`, the secret scan, `.github/workflows/` | The gate (M0) | The tier-2 jobs each milestone adds; triage of red tier-2 runs (C7) |
| WS-model | @world-chunks | `models/cuts/` | Model v0 (M0); v1 with join and leave (M1) | v2: the merge against finality (M2, before merge v1); v3: quorum loss, the one-minute bound, membership re-founded per line, split groups (M3); per-group agreement across layers (M4) |
| WS-agree | @world-chunks, after WS-model v0 | `crates/pe-log`, `crates/pe-agree`, `docs/spec/agreement.md` | Cuts at fixed membership; admitting by entry; leave; join from a cut; holding votes until a build is ready | Quorum loss with unconfirmed play, the one-minute bound and its line ids; the protocol side of member relays (M3). It calls the merge; it does not own it |
| WS-world | @world-model | `crates/pe-world`, `crates/pe-sim`, `crates/pe-core`, `docs/spec/world-model.md` | The object tree, components and blobs; the hash tree over everything that matters (fields, governance, claims, logic state, module versions); edit entries with their three conflict rules; governance validity, until WS-gov exists; 128-bit ids; the coordinate format; every object's simulation size | `Pos`, the typed offsets and `ORIGIN` (M2); the world record, keeping every entry from creation (M3); saves (M5) |
| WS-state | @vdw-physics | `crates/pe-replica`, `crates/pe-physics`, `crates/box3d-fork`, `docs/spec/replica.md` | The rollback loop over world-model bodies, with per-group save, restore and step; replay from the last agreed cut (repair rung 1) | Incremental images, background re-simulation, the worker pool, skipping unchanged systems and islands (M2); the replayed-impulse measurement (before M4); a history-free physics state (later) |
| WS-net | @shader-lang | `crates/pe-net`, `docs/spec/transport.md` | Direct QUIC connections from an invite; datagrams and streams; the rate budget v0; the clock with gradual steering; the version and self-test comparison at connection | Member relays and hole punching; the helper-node protocol (M3) |
| WS-logic | @iter-bench | `crates/pe-logic`, `crates/pe-sdk`, `docs/spec/logic.md` | Cranelift with metering injected into the module; a first kind of system over the store; code commits with an agreed frame; activation inside the window | Forks and `code.test`, the per-player mode, native declarative components; the SDK re-exports WS-world's position types (M2) |
| WS-api | @api-eng | `crates/pe-api`, `crates/pe-mcp`, `docs/spec/api.md` | WorldApi (schema, query, apply, watch, history); the MCP tools; `view.capture`; `code.check` and commit | `fork.*` and `code.test` (M2) |
| WS-render | @ballpit-viz | `crates/pe-render`, `docs/spec/render.md` | The window; the scene renderer, drawing from published snapshots; the display layer (file 14, 4.1); the desktop hand; offscreen capture; the GPU context and view interface that pe-xr uses | The UI lab (after M1); the UI layer and skin (M5) |
| WS-xr | @xr-eng | `crates/pe-xr`, `docs/spec/xr.md` | The OpenXR session, stereo through pe-render, tracked hands into hand entries, 90 Hz pacing | Tracking for avatars (M5) |
| WS-app | @engine-architect, after WS-seams | `crates/pe-app` | The application: threads, wiring, invites, configuration, the demo script | The launcher (M5) |
| WS-harness | @harness-eng | `crates/pe-harness`, with its scenarios and the probes | The network model, the scenario runner, the properties, the three probes, randsched | Long runs and distributions (M3) |
| WS-diverge | @diverge-eng | `crates/pe-diverge` and its tests; `docs/spec/divergence.md` | The repair ladder's rungs 1 and 2; the fault bundle; the counters; the display layer's requirements, handed to WS-render | The merge function, a pure function it alone owns: v1 at main's final frontier (M2) and v2 for split groups (M3). Rungs 3-5, and the background self-check (M3) |
| WS-gov (M3) | to be staffed | `crates/pe-gov`, `docs/spec/governance.md` | — | M3: guarded mode; admins as co-owners, with no ownership certificates (file 18); moderator roles made by hand; an admin's agent holding that admin's permissions. It takes governance validity over from WS-world |
| WS-regions (M4) | to be staffed | `crates/pe-regions`, `docs/spec/regions.md`, the region simulator | — | M4 |

Rules:
- **A change to someone else's path** goes through its owner: ask, or propose a patch that the owner lands.
- **Manifests.** WS-seams creates every M1 crate's skeleton in M0, so nobody else edits the root `Cargo.toml`. Each
  crate's dependencies go in its own manifest.
- **Cargo.lock.** A conflict there is resolved by taking main's lock file and re-running a build on the branch.
- **Tests live in the crate they test.** Other crates use pe-harness as a library for their own tests.
- **Mutations live behind a `mutants` feature** in the crate whose rule they switch off. They are built only into the
  harness's mutation tests and never into production state; the old engine's fix switches were the mistake.
- **Builds.** Each agent works in its own worktree with its own target directory (team rule of 2026-11-01), and runs
  builds through `heavy.sh`.
- **`archive/` is read-only** for everyone.

### 3.2 The seams

Six seams in `pe-seams` let every workstream code against them from the first week, with mocks for any side that is
not built yet. Appendix A sketches them. WS-seams fixes v0 in M0, from the owners' proposals in their specifications.

| Seam | Between | What crosses it |
|---|---|---|
| S1 Entries | everyone | The signed envelope: author key, chain id (one chain per member per world for its life; a restart takes a new nonce), sequence number, the previous entry's hash, domain id, frame stamp, class (input, edit, governance or protocol), typed payload, signature. The signed bytes bind the world id (the genesis hash), so an entry cannot be replayed into another world. Hashes are 256 bits and object ids 128 bits. The domain id is there from the start, so the regions work needs no format change: one domain per world until M4, then one per group of things in contact (file 16, C4) |
| S2 Replica | pe-agree and pe-replica, pe-world, pe-diverge | Step a frame with its ordered entries; save, restore and step per group; the root hash; each entry's outcome (applied, or refused with a reason) and any permission check at a frame; a subtree-hash exchange, node hashes by path, through which repair and merging find what differs. The trait is object-safe, so the harness can wrap it |
| S3 Agreement | pe-agree and pe-app, pe-harness, pe-diverge | Submit a local entry; receive a message; tick on local monotonic time, with the frame from pe-net's sans-IO clock. What to simulate now. The newest final cut, which carries a line id. A hold on votes until a build is ready (Q3). Events, and one trace vocabulary shared with the model: proposals, votes and final cuts; voided and refused entries with reasons; equivocations; joins and leaves; quorum lost; unconfirmed time; diverged; merged; and "paused", which must never fire. pe-agree calls the merge function; it does not own it |
| S4 Transport | pe-net and pe-agree, through pe-app | Send by class (protocol, action, pose, repair, bulk), in that priority; receive; statistics per link. A pose that cannot be sent is superseded, never queued |
| S5 Frame view | pe-replica and pe-render, pe-xr, pe-api | A read-only view published once per frame: objects, simulated and drawn poses, appearance, flags for the local player's own hands and what they hold, the local rig transform, the events that trigger one-shot effects, and the frame's time on the shared time base. It also carries hand inputs, in the player's rig frame, from pe-xr and the desktop hand. Positions cross it only as exact places or offsets from the camera, never as raw absolute numbers. No GPU types: the GPU interface between pe-render and pe-xr lives in pe-render |
| S6 WorldApi | pe-api and pe-mcp, pe-app, and later the inspector and editor | Query, apply (a transaction, live or dry-run, later in a fork), watch with a cursor, and history, with fork and check from M2. Handlers read published snapshots and queue their transactions, so they never block the simulation (file 06, A1) |

- **Agreement and transport separate.** pe-agree knows the payload classes but nothing of what they mean, and the
  replica never decides finality.
- **One owner for the merge.** The merge is one pure function in pe-diverge, owned by WS-diverge and modelled by
  WS-model. pe-agree calls it, and pe-replica runs the replays it asks for.
- **Sans-IO.** pe-agree, pe-diverge and pe-net's protocol logic take time and messages as inputs and return outputs,
  so the harness can drive them deterministically, with no sockets, clocks or unseeded randomness [I; quinn-proto is
  built this way].
- **No panics on a peer's input.** A bad message is an error and an event naming its author, never a panic.
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
    WS-model v0               │           feasibility: QUIC, OpenXR + wgpu, wgpu window, Cranelift
                              ▼
M1  WS-world ──► WS-api ───────────────────────┐
       │   ├───► WS-render (display layer) ──► WS-xr ──┤
       │   └───► WS-logic (Cranelift, activation) ─────┤
       └───────► WS-state ─────────────────────┤
    WS-model ──► WS-agree ─────────────────────┼──► WS-app: integration ──► two-machine run
    WS-net ────────────────────────────────────┤
    WS-diverge: rungs 1-2, fault bundle ───────┤
    WS-harness: probes, properties ────────────┘
M2  WS-model v2 (the merge against finality) ──► WS-diverge: merge v1
    WS-logic: forks, code.test, per-player mode, declarative components;  WS-world: Pos, offsets, ORIGIN
    WS-state: incremental images, background replay;  WS-render: the UI lab
M3  WS-agree + WS-model v3 + WS-diverge (quorum loss, the one-minute bound, split groups);
    WS-world (the world record); WS-gov (guarded mode); WS-net (relays)
M4  WS-state: the replayed-impulse measurement; WS-regions (the simulator first)
M5  the platform tracks
```

- **Week 1:** every M1 workstream can start against `pe-seams` and mocks. An arrow marks where real code replaces a
  mock.
- **The likely critical path [I]:**
  - VR with rendering and the display layer, the longest single track;
  - the agreement core, which follows its model.
- **WS-agree's start.** @world-chunks writes model v0 first. pe-agree then ports pe-log's chains at once, while the cut
  rules follow the checked model. So the code follows a checked model, not the other way round.
- **The merge engine** is modelled (v2) before it is built, in M2. It is the same engine for:
  - a machine back from a long compile;
  - a machine back from a bad link;
  - split groups reconvening (file 14, 4.4).

### 3.4 Staffing (the coordinator's assignments, 19:29Z)

- **The builders:**
  - WS-seams, WS-archive and then WS-app: @engine-architect;
  - WS-model, then WS-agree: @world-chunks;
  - WS-net: @shader-lang;
  - WS-render: @ballpit-viz;
  - WS-state: @vdw-physics;
  - WS-ci: @provenance-eng;
  - WS-logic: @iter-bench;
  - WS-api: @api-eng;
  - WS-world: @world-model;
  - WS-xr: @xr-eng;
  - WS-harness: @harness-eng;
  - WS-diverge: @diverge-eng.
- **WS-gov and WS-regions** are staffed when M3 and M4 come.
- **Reviewers:**
  - the model, agreement and WS-diverge: @design-reviewer, with @shader-reviewer as WS-diverge's second reviewer;
  - the other workstreams: @chunks-reviewer and @shader-reviewer;
  - each milestone's work as it lands: @fable-reviewer.
- **Build slots will bound throughput.** Twelve agents share `heavy.sh`'s two slots.
  - Iterate with `cargo check -p <crate>`.
  - Tier 1 of the gate covers only the crates a landing touches.
  - Branch builds may use hosted runners (the coordinator's ruling, 20:05Z).
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
   - live memory stays bounded over long sessions, while the full change record is kept;
   - the user's convergence targets: others see an action within 0.25 s on good links and 1 s on poor ones, and no
     correction larger than a hand's width is shown in view without a cue (meef4f248923b, answer 8).
2. **The model before the code.**
   - The agreed-cut rules, and then the merge rules, are written as an executable model and checked exhaustively at
     small sizes before pe-agree and pe-diverge depend on them (file 11, 3.3 and 3.9).
   - The model then checks the code: pe-agree's event traces from random schedules are replayed through the model's
     transition rules, in the trace vocabulary they share, and any step the model forbids fails the test.
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
| probe-d1 (`appendix/evidence/probe-d1-unlinked_confirm.rs`) | A member withholds chain entry k+1, then sends k+2, back-dated to frame 97 (at or before its own declared horizon), past the hole. Honest member V confirms frame 100 with that entry in effect, and aborts when the hole fills: "would roll back to frame 97, below its confirmed frame 100" [V file 01, `probe-d1.log`] | The same schedule against pe-agree, at three members. V may simulate k+2 as a prediction. No honest member votes for a cut that includes it until k+1 is held and the chain validates in order. Once k+1 arrives, every member judges k+2 void by the same rule. No final frame rolls back, nothing aborts, and every replica ends equal to the others and to the replay. Also the joiner variant: a pending joiner at admission (`membership.rs:2294-2297` in the old code) | A pe-harness test; the model's validity property; the mutation "vote without chain validation", which must give a counterexample |
| probe-capture-close (`probe-capture-close.toml`) | In a default session where everyone is an admin, member 4 defines a role with no permissions, gives it to members 1-3, and goes silent. The session is left with no admin and closes for everyone [V file 07, `probe-g3-capture-close.log`] | **Public template:** member 4 joined without admin rights, so its DefineRole and SetRole are refused ("no permission"). The roles are unchanged, the three carry on, and all replicas are equal. **All-admin variant** (every member made an admin): no admin is reachable, so the ruled path for that case follows. In a public world it is guarded mode, the owner's countdown, then closing with the state saved (mb09ab8952a38), and the owner can restore the roles. For a private world, WS-gov's specification states whether the same applies or the session closes with its last admin, as m716adebd0591 has it. Whether demoting every other admin needs a guard of its own is WS-gov's proposal, as @item:primordial-engine/governance-defaults-strangers-start-without-admi asks. It goes to the user if it changes a ruling | A pe-harness test, written in M0 and failing until M3 |
| probe-twosilent5-kick (`probe-twosilent5-kick.toml`) | Five members; two go silent at 3 s; admin 1 kicks both at 6 s. No decision: 616 paused frames at 15 s [V file 07, `probe-h1-twosilent5-kick.log`] | **The three are never frozen:** zero paused frames, and their actions are marked unconfirmed. The kick is an ordinary entry under the strict 2/3 quorum (q = 4 at n = 5; Q5, no exception), so it stays undecided without the two. **If the two return within a minute,** the lines merge automatically. **If not,** at one minute each side re-founds its membership as its own line; the three's line gets finality back under its own quorum, and both lines can be re-merged later. **Controls:** stopping play below quorum must bring back the paused frames, and a merge that drops one side's actions must fail the merge property | A pe-harness test; model properties and their mutations |

### 4.3 Other defects the review verified, as tests of the new code

| Defect | Test |
|---|---|
| No leave entry: every exit pauses everyone for about 1.5 s, and a two-player session stalls for good (file 07, H2; file 01, defect 7) | A member that quits authors a leave, and the others drop it at the next cut with zero paused frames. In a pair, the survivor never stops. Control: with the leave ignored, paused frames must return |
| Invariant checks abort the process (file 01, defect 5) | Property tests feed arbitrary bytes and entries from one member. No replica panics, and the member is reported |
| The log is never pruned (file 01, defect 6) | A two-hour session with eight members: live memory stays bounded, and entries below the last final cut move to the world record (never deleted, the user's instruction of 19:09Z) |
| Nothing bounds one member's entries (file 01, defect 8) | A flooding member's entries beyond its rate are refused, and memory stays bounded. Control: with the rate limit removed, the memory test must fail |
| 64-bit hashes where votes sign (file 01, medium) | By type: `pe-seams` has only 256-bit hashes |
| Governance sits outside the hashed state, so a joiner cannot verify it (file 07, H3; file 14, F5) | A lying member hands a joiner a wrong ban list; the joiner catches it against the cut's root |
| A sync-check mismatch is only logged (file 01; file 04, defect 4) | A divergence is injected in one object on one replica. The hash tree finds that object, and repair rungs 1 and 2 restore equality (M1). Rungs 3-5 follow in M3 |
| The congestion controller read an oracle (file 01, defect 10) | Network scenarios run through pe-net's real congestion path; the harness reports only what a real endpoint can observe |
| The world cannot be edited (file 04, defect 1) | M1's edit tests |
| Different engine versions or a faulty physics build in one session (file 14, F1 and F6) | The connection guard refuses the join, with the reason (M1) |
| "Every peer compiles" sat on everyone's critical path (file 03, defect 2) | Activation after each machine's own compile: nobody waits (M1) |

### 4.4 The model of agreed cuts, and of merging

The mechanism, from file 11 (3.3):
- Every few frames, the next member in a seeded rotation proposes a cut: a frame and, for each author, the last entry
  included.
- Members vote with one hash in their next acknowledgement, and the quorum makes the cut final.
- Entries inside a final cut are final. An entry stamped before the cut and not in it is void for everyone.
- The cut carries the state hash, so it is also a checkpoint, and it carries a line id.

**The quorum, "2/3 strict"** (the coordinator's ruling, 20:05Z, carrying D80 option (b) over as it was implemented):
- t = floor((n − 1) / 3) members tolerated as unproven-faulty, and q = floor((n + t) / 2) + 1 votes make a cut final.
- So q is 2 at n = 2 and 3; 3 at n = 4; 4 at n = 5 and 6; 5 at n = 7.

**The fault model per size:**
- Byzantine members are modelled only at four members or more.
- At three members, the model checks crashes and author faults: withholding and back-dating.

**A slow member keeps its vote** (no demotion for slow links stands). Cuts wait for a quorum, not for that member, and
"follows by state" describes its own view only.

**A machine still building a code change** holds its votes on cuts at or past the change's agreed frame until its build
is ready (Q3; the coordinator's ruling, 20:22Z).

**Below quorum:**
- Play continues unconfirmed.
- At one minute, each line re-founds its own membership, takes a new line id, and both lines know which they are.
- Lines that meet again within the minute merge.

**The merge against finality** (the coordinator's ruling, 20:05Z, its reading of Q3b and the 18:58Z instruction,
passed to the liaison so that the user can correct it):
- It is a three-way merge at main's final frontier. The base is the state the returning machine diverged from, "theirs"
  is the returning machine's replay under the new rules, and "ours" is main's final state.
- The deterministic rules of file 14 (4.4) resolve conflicts: one order by frame stamp with ties broken by the
  author's key, then the engine's ordinary rules.
- **Witnessed order** (the user confirmed it, mfc2bbd9d28ad; file 14, 9.1): an action that another member
  acknowledged within the normal bound of its stamp keeps its stamp; an unwitnessed action cannot overturn something
  already final. The known residual: in a split of one against one, neither side has a witness, so a lone cheater
  can back-date by up to the one-minute limit.
- **A component attached to an object deleted first** is refused, with a notice to its author (file 14, 9.2).
- No final frame is ever undone.
- Model v2 checks these rules before merge v1 is built.

**The tool:** Stateright, a model checker written in Rust (C3). Its tier-1 profile is time-bounded (C3's condition).

**Scope, by version:**
- **v0 (M0):** fixed membership of three and four members, with the fault model above. Messages are delayed,
  reordered, lost or duplicated, a few cuts deep.
- **v1 (M1):** admitting by entry, leave, join from a cut, and the vote hold during a build.
- **v2 (M2):** one branch merging with the main line at main's final frontier: a long compile, or a bad link.
- **v3 (M3):** quorum loss, unconfirmed play, the one-minute bound with membership re-founded per branch and line
  ids, split groups and their merge.

**Safety properties:**
- Agreement: no two honest members finalize different cuts at the same height on the same line.
- Validity: a final cut includes only entries whose chain validates in order, and the void set is the same everywhere.
- No final frame is undone.
- **Merge determinism:** the same entry sets give the same merged order and outcomes, whatever the arrival order.
- **Merge keeps work:** an action survives the merge unless the ordinary rules would have refused it with no split.
- **Witnessed order:** an unwitnessed action never changes the outcome of a final action.
- **Voiding and merging fit together:** entries made during a split are never voided by the tolerance rule merely
  because they were apart [I; settled in v2 and v3].
- **Lines know themselves:** after the one-minute bound, every member knows which line it is on, and no cut of one
  line counts on the other.
- S6, from the widened random-schedule work: on clean links, an honest member never votes to remove or demote an
  honest member that has not crashed.

**Liveness, bounded:**
- With a quorum honest and connected, a cut becomes final within a bounded number of proposer turns.
- Below quorum, no honest member's play stops. At one minute the line diverges, and each line's finality resumes
  under its own membership.

**Controls that bite:**
- voting without chain validation (the d1 defect);
- a quorum one too small, q − 1, which must give split-brain;
- stopping play below quorum (the h1 freeze);
- accepting a proposer's cut without checking it;
- ordering a merge by arrival;
- honouring an unwitnessed back-dated stamp against a final action;
- voiding the entries made during a split;
- raising the one-minute bound.

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
- **A fast stand-in replica** (a hashed key-value state machine inside pe-harness) lets the agreement properties and
  randsched run without physics.

### 4.6 The gate

One command, `scripts/gate.sh`, in two tiers.

**Tier 1, on every landing** (locally, through `heavy.sh`, before the merge; branch builds may also run on hosted
runners). It covers the crates the landing touches:
- the format check, and clippy with warnings denied, on every crate except `box3d-fork`;
- `cargo test` on those crates;
- the model's fast profile, bounded in time;
- the harness's fast set of properties, and the probes;
- the native-code audit with its allowlist, and the secret scan (@provenance-eng's item);
- the existing publication check.

**Tier 2, on hosted runners (Q12),** on the public repository after each push, and nightly:
- the whole gate on windows-x64, linux-x64, linux-arm64 and macos-arm64;
- cross-platform determinism: each runner writes digests (physics records, replica replays, logic fuel and trap
  points), and a final job requires them to be bit-identical;
- the model's deep profile (four or five members), randsched seeds, long runs, the network distributions.

A failed tier-2 run blocks the next landing until it is fixed or explained. @provenance-eng triages red tier-2 runs
(C7's condition).

**Graphics in CI.** Render tests run headless with a software adapter (WARP on Windows, lavapipe on Linux) and compare
captures within a tolerance [I]. OpenXR's frame loop runs against a simulated runtime, where one is available [I]. Feel
in the headset is the user's to judge, at the checkpoints.

## 5. Carry-over of the user's rulings

### 5.1 The rulings of 2026-11-10

| The user's ruling | Where it lands | When | Tested by |
|---|---|---|---|
| The governing principle: local-first and immediate; others eventually consistent; the local user held up as rarely as possible; "no player should ever wait for other players' actions to finish" (mfc7e0ddc4137, mf7daea6446ca) | A rule for every workstream. Each place where a user can be held up (file 11, 3.0) gets a stated delay budget and a measurement | Every milestone | The delay distributions of each milestone |
| 1. The vertical slice comes first (Q1) | M1 | M1 | M1's demo and tests |
| The plan's four questions (mc40fc6379bc6): U1, a logic edit in M1; U2, checkpoints after M1 and M3, with @fable-reviewer reviewing each milestone's work as it lands; U3, Cranelift; U4, the second PC with a desktop window only, VR on this PC | M1's demo and size; the checkpoints; WS-logic in M1 with Cranelift, by the coordinator's ruling (19:52Z), so the logic edit never ships on wasmi | M1, M3 | M1's logic tests |
| 2. One quorum mechanism replaces unanimous confirmation, the view change and Part B, keeping the user's rulings (Q2) | pe-agree (agreed cuts); `models/cuts` | The model in M0, the code in M1, complete in M3 | Section 4.4 |
| 3. Code activation (Q3, mf7daea6446ca): the owner runs a change at once, and every other machine rewinds and replays when its own compile finishes. Objects keep running the old code, then jump (Q3a). A divergence over 2 s becomes a world-line merge with main (Q3b). No machine runs another's build (Q3c) | pe-logic: activation inside the window in M1 (the coordinator's ruling, 20:22Z), with M1's edit measured to compile in under 2 s on the second PC (20:26Z); the merge function of pe-diverge for longer divergences, a three-way merge at main's final frontier (the coordinator's reading) | M1 (inside the window), M2 (the merge) | M1's logic tests; merge v1's tests |
| Standalone, compiler-less headsets: excluded from the prototype; any later support comes through our own deterministic cross-platform compiler, not follow-by-state; own-PC builds are moot (mb09ab8952a38) | Not built (section 6) | — | — |
| 4. Regions (Q4): only what is near the player simulates; a frozen area's state lives in the world record and peer caches (mf7daea6446ca); missed time is re-simulated on return, up to one minute after freezing, a world setting, on a metered thread (meef4f248923b, mfedf5a6c3c5b); sizes default to 1 m times the maker's scale, never below the real extent, settable larger (m8c42b4b093bd); the simulation distance follows the Open World Orchestrator (m5adc22d1388d). The OWO's ratio is the default, changeable per world (C1); the player's scale selects a window of layers (C2) (m1d4a95c6d4bd); every object has a nonzero simulation size (maa60620a6ced); simulating and watching are a spectrum, with influences from unloaded things sent as entries (mbcc1c5fd8d39), any influence counting, script logic included (ma27c73c50fd4) | pe-regions, as in M4: layers as the loading index, per-machine chunk sets, frozen chunks, arrival entries instead of scans, agreement per group in contact, and influence entries for whatever acts from outside a machine's loaded set. The domain id is in every entry from M1. The size's default and limits are parameters (section 6) | M4; the format from M1 | M4's tests |
| 5. The removal quorum stays 2/3, strictly, with no kick exception (Q5) | pe-agree: t = floor((n − 1) / 3), q = floor((n + t) / 2) + 1 (section 4.4). Below quorum, play continues unconfirmed and reconvenes by merge (the 18:58Z instruction), so nobody is frozen | M3 | probe-twosilent5-kick |
| Unconfirmed play below quorum lasts one minute at worst, then the session diverges (m35138bafc46a) into git-style branches (meef4f248923b). The full change record is kept from creation, and diverged worlds may be re-merged later by choice (19:09Z), with an admin of each consenting after a preview; the exact merge by default, a fast merge optional (meef4f248923b) | pe-agree and pe-diverge (the bound, membership re-founded per line, line ids); WS-world (the world record); M5 (re-merging later) | M3; re-merging in M5 | The one-minute and record tests (M3) |
| A delete against an add-on on two branches is decided by the order of events: delete first, the add-on is orphaned; delete second, everything is destroyed (m35138bafc46a) | pe-diverge's merge: the ordinary replay in event order | M2-M3 | A merge scenario for each order |
| The merge rules (mfc2bbd9d28ad): witnessed order, with its one-against-one residual; a component attached to an object deleted first is refused, with a notice to its author | pe-diverge's merge, modelled in v2 | M2-M3 | The witnessed-order property and its control |
| Convergence targets (meef4f248923b, answer 8): others see an action within 0.25 s on good links and 1 s on poor ones; no correction larger than a hand's width in view without a cue | A property of every network scenario (pe-harness) | M1 (good links), M3 (poor links) | The delay and correction distributions |
| Carried state is a check, not an overwrite (Q15, B4; mfedf5a6c3c5b) | pe-replica | M1 | M1's carried-state test |
| No world owner (m6987a48ed7f0, ma9035760e0c1; file 18): every admin is a co-owner and keeps their own version; non-admins keep nothing after leaving; rejoining reopens your version and merges with an admin who has it open, unless you stay forked | WS-gov (admins, no ownership certificates), WS-world (the record kept by admins), WS-diverge (the rejoin merge) | M3; saves in M5 | The returning-admin scenario |
| Moderation (meef4f248923b, answers 13 and 14): no moderator role by default, though one can be made by hand; an admin's agent holds that admin's permissions | WS-gov; M5's agent panels | M3, M5 | — |
| 6. Public worlds start strangers without admin rights (Q6 of the first round) | pe-gov: the public template's joining role has no admin permissions. Governance is ordinary entries over the hashed world state | M3 | probe-capture-close |
| No admin reachable (Q6 of the follow-ups, mb09ab8952a38): guarded mode, then a countdown (30 s by default, now an admin's world setting), then the world closes with its state saved | pe-gov. "Unreachable" is a recorded fact decided at a cut. Whether anything is kept with no admin present is file 18's question 3 | M3 | The guarded-mode tests |
| 7. Carrying between worlds is copy then remove; a rare duplicate is acceptable; no lease (Q7) | M5's worlds track: the carrier creates the object in the destination as an ordinary permitted edit, then removes it from the source once a cut in the destination holds it | M5 | A crash mid-carry leaves at most a duplicate, never a loss |
| 8. QR (b): only the owning player's machine runs their per-player script copy (Q8) | pe-logic's per-player mode. It runs once per eligible player, on that player's machine, on predicted state, and its effects on shared state are that player's signed inputs. Three mitigations stay within the ruling (file 11, 3.5): an effect names the versions it read and is refused if they are stale; each effect is declared "once for the world" or "once per player"; per-player state may live in the player's profile store | M2 | M2's per-player tests |
| 9. Helper nodes are optional and never assumed (Q9) | pe-net: an invite carries keys and reachable addresses; any member can act as rendezvous and relay; the helper protocol is written down, and the node list ships empty | Direct in M1; relays in M3 | Every test runs with no helper |
| 10. Hosted CI runners are allowed (Q12) | Tier 2 of the gate, and branch builds | M0 | — |
| "explore the variety of ways that this line can diverge, and systems we should design to help return it back promptly and with as little visibility or impact on the user's experience as possible" (18:56:50Z) | File 14's five systems: the display layer (WS-render), bounded rollback (WS-state), the hash tree and the repair ladder (WS-world, WS-diverge), the merge (WS-diverge), the guards (WS-net, WS-diverge) | M1-M3 | The divergence catalogue's scenarios |
| "when the worlds reconvene, treat it like a git merge where every action is a commit to the main tree, but with deterministic rules for resolving it whenever possible" (18:58Z) | pe-diverge's merge function, modelled first | M2-M3 | Merge determinism; "merge keeps work" |
| Precision-safe coordinates for code at extreme distances (19:14Z), answered (m06d2643578d7): a hard wall; plain numbers only by subtracting the world's origin constant; enforced by the type system alone, with no test or warning; frames as in Resonite. Exact integer cells stay available (the coordinator's reading, 20:17Z, not objected to) | pe-core and pe-world (WS-world): `Pos`, typed offsets, `ORIGIN`, `pos.cell(level)`, on `WorldPos` (file 17, 7); re-exported by pe-sdk | M2 | Compile-fail tests of the SDK |
| The VR runtime (m2aaafe98cbbe): "Virtual Desktop" | WS-xr's runtime for real-headset tests, each with a go through the liaison | M0-M1 | — |
| Legal (mbe87e750ca35): guests may use a subscription login with a warning (A1); Google models with a warning (A2); the business model parked (B1); no contact with YDMS (R1); simple Resonite-style protections for received avatars and objects (V1) | M5's agent-panel and people tracks | M5 | — |
| Q10 and Q11: remove the Resonite note; remove the user's e-mail from the public history | Outside the rewrite: @item:primordial-engine/public-repo-cleanup-remove-the-resonite-note-and (its force-push is done); the research in @item:primordial-engine/research-avatar-licences-and-ai-providers-terms-o | Done or running | — |

### 5.2 Earlier rulings the new design keeps

All of the user's earlier requirements still apply (m83bc23ef13ec). These are the ones the rewrite touches most.

| The ruling | Where it lands |
|---|---|
| R1: no host and no central authority | Proposers rotate and hold no authority: they propose, the others verify, and a silent one is skipped. "Main" in a merge is the quorum's line, not a host |
| R2: a fixed timestep, independent of frame rate | pe-core's tick clock; pe-render interpolates. M1 tests display rates against tick rates |
| R3: a deterministic local simulation with rollback; traffic only on interaction | Kept. Entries flow only on interaction; cut votes flow while anyone acts |
| R4: Box3D; Rust and wgpu | Kept |
| R5: the soonest interaction wins. Same-frame order follows the seeded rank (D37), and only grabs claim | pe-log's order and claims, unchanged. The same order decides merges |
| "An author never sees its own interaction corrected" (m21f5bdac80b3); an interaction carries the new state with a canonical frame (m84a607d3a701) | Narrowed by the user's answer to B4 (mfedf5a6c3c5b): the carried state is a check, not an overwrite, so an author's body may be corrected by a late earlier action. An author's own action is still applied at once and never refused, and the display layer never applies a correction offset to a player's own hands or what they hold (pe-replica, M1) |
| R6: sync checks, and repair from the earliest frame | The hash tree over objects (pe-world) finds what differs, through the subtree-hash exchange; the repair ladder (pe-diverge) fixes only that |
| R7: live edits, source control, rebase, the 10 s seat | One edit entry with a base version (pe-world). A stale one is refused with its reason, and the seat is the path that refusal takes. Fields and the first code in M1 |
| R8: MCP for state, logic and shaders | WorldApi and the small tool set (pe-api, pe-mcp) |
| R10: full VR | pe-xr, in M1 |
| R11 and the rulings on logic, shaders and assets as objects: copy-on-write ("b, 1 always"); linked scripts only when set up explicitly; scope U; plain Delete unlinks; an in-game editor, but not the main way | pe-world (M1) and pe-logic (M1-M2), from @item:primordial-engine/logic-shaders-and-assets-as-objects-unified-hier |
| R14: several worlds, portals | M5 |
| R15: replaceable physics | Hooks as systems, from M2. Whole-engine replacement is stated with its real limit: about 50-200 active bodies with rollback (file 02) |
| R16 and R17: huge worlds; culling by size; avatar culling | M4, with the coordinate ruling (m7e85960c864a) |
| R18: detect collapse and recover; keep congestion with the thin player | Covered by cuts, the link budget and the merge, in M3:<br>- a slow member keeps its vote, and nobody demotes it;<br>- cuts wait for a quorum, not for it, and its own view follows by state at cuts;<br>- a member diverged for more than 2 s merges with the main line;<br>- each link gets its own rate budget in pe-net;<br>- late actions past the tolerance are void, decided by the cut, and the engine sets the tolerance from measured links;<br>- "fewer than 0.01% dropped on acceptable connections" is measured |
| The premise: distributed verification of a change record (m3c8512748b8c, answered "all current") | Cuts are the verified record, and the full record is kept. The order comes from the authors' frame stamps; the earlier change wins a conflict; each scale keeps its own record |
| Trust players; admins kick (the trust survey) | Hardening covers crashes and corruption only; griefing is the admins' to handle. Equivocation is the one anti-cheat (file 14, 4.5) |
| D80 and the admin rules: admins plus 2/3; admins are equal; a role that can assign roles counts as an admin; kicking an admin needs its own permission; the last admin cannot release the role; admins may kick and ban while paused; a session closes with its last admin (m716adebd0591) | pe-gov (M3), as rules over the hashed state, with the quorum formula of section 4.4. In public worlds the guarded-mode rule decides how a session without an admin closes. With no world owner (file 18), the owner rules are overturned: the signed ownership chain, Q27 and the owner kick rule, O1-O5. D87's version-control merging is kept, with "all co-owners" read as all admins |
| Perma-bans are saved with the world (m5c4b84d186a8) | Fields of the world's access list, written by ordinary governance entries and kept by the admins (M5, saves) |
| D11: every peer compiles from source | Kept, and now with its timing: each machine compiles for itself and activates at the agreed frame (Q3) |
| D12: "wasmi now, Cranelift later" (mc164bb1fac63) | Answered by U3, Cranelift. By the coordinator's ruling (19:52Z), Cranelift starts in M1, with metering injected, and no logic ships on wasmi |
| D13: WGSL for now | Kept |
| 100% Rust, with operating-system, driver and runtime boundaries loaded dynamically (md35ba688f6b4) | QUIC with pure-Rust cryptography, its provider named in M0; OpenXR's loader loaded at run time, never compiled in; Cranelift with wasmtime's C file patched out; the audit as a gate; one register of boundaries (file 09, A8) |
| D2: parallel physics, bit-exact; D58: double precision; van der Waals | Kept in the fork; the worker pool is wired by WS-state |
| D4: the 90 Hz target; D50: the memory and CPU slider | 90 Hz is measured in M1. The slider's positions become policies over incremental images (M2) |
| D23: eight players for the MVP, aiming higher | M3 tests eight; regions (M4) are the way beyond |
| The sync model's user answers (v11): the synchronized line; publication is automatic and never the player's concern (m27cdafe8fcfa); QR (b); QH2 (c); a held paddle visible only to its holder (Q13 a) | Fields and their modes in pe-world (M1-M2), and pe-logic (M2). The org's proxy records give way to a held shared body visible per player, on the condition that only its holder sees it, as Q13 (a) has it (C9) |
| The cross-scale answers: Q1 (c), a far player reads a large object from those near it; Q6 (c), delayed reads; Q8 (d), a giant's peers host native-scale objects; "readings ok" | M4. Q6 (c) is replaced: the delay applied only to followed objects (ma27c73c50fd4), and then the spectrum of file 16 (6.6) removed the fixed delay altogether (mbcc1c5fd8d39). Q1 (c) lives on as influence entries. WS-regions' specification states how Q8 (d) fits, and takes it to the user if it changes the answer |
| The look for now; the Gnosarium name and theme; the built-in harness ("likely opencode") | M5. The look becomes a skin over a plain layout (file 08, A1). The harness is chosen by a short spike against a first-party agent loop, which needs an API key |

## 6. Open with the user, and excluded

Q3, the one point the first version listed as out of scope, has since been answered (mf7daea6446ca). M1 and M2 plan it.

The design phase is closed: the user has answered every question the review asked. The simulation size's details
(m8c42b4b093bd), the later merge (meef4f248923b), visible error and "a reasonable timeframe" (the convergence
targets, answer 8) are all settled above.

**Still open with the user, kept as parameters, none blocking M1** (file 18, section 5; WS-gov's specification builds
each as a setting):
- whether the admin already in a world gets a say when a returning admin's version merges into it;
- who adds and removes admins (recommended: 2/3 of the admins, a sole admin deciding alone);
- whether a session with no admin keeps nothing, with a permanent "nothing here is being kept" mark;
- whether join rules that named "a world owner" test any admin of the world (recommended), or only those present.

**One reading still to confirm:** the coordinator's reading of the merge against finality (a three-way merge at
main's final frontier) has gone to the liaison so that the user can correct it. Model v2 checks it, with the
witnessed-order rule the user confirmed, before merge v1 is built.

**Excluded from the prototype:** standalone headsets and other machines without a compiler (mb09ab8952a38). The design
keeps nothing that would make them impossible later. Their path is the project's own deterministic cross-platform
compiler, which this plan does not schedule.

None of these is built before it is answered. Each one's place in the plan is noted where it lands.

## 7. Triage of the docket

There are 27 live items (not archived). I read 25 of them on 2026-11-10 at 19:02Z [V]; the coordinator parked the two
I could not see (rows 26 and 27), and its ruling put item 25 to park (20:05Z). The last column is where each item
goes, and why.

| # | Item | Status, owner | Verdict | Where, and why |
|---|---|---|---|---|
| 1 | @item:primordial-engine/re-found-primordial-engine-above-the-determinism | in_progress, coordinator-opus | Carry | The rewrite's umbrella item. Each workstream has its own child item |
| 2 | @item:primordial-engine/primordial-engine-p2p-deterministic-rollback-gam | blocked, engine-architect | Carry | The product item, holding the user's requirements since 10-30. This plan replaces its phase plan. Its blocked reason (the capacity pause of 11-02) is stale: make it in_progress, with the re-founding as its current phase |
| 3 | @item:primordial-engine/landing-gates-run-the-native-code-audit-and-a-se | open, provenance-eng | Carry | M0, WS-ci: the gate for the restructured workspace, after WS-archive lands; tiers 1 and 2 |
| 4 | @item:primordial-engine/public-repo-cleanup-remove-the-resonite-note-and | open, shader-lang | Carry as it is | Ordered by the user (Q10, Q11). Its one force-push is done, before the archive landing, as the plan wanted |
| 5 | @item:primordial-engine/research-avatar-licences-and-ai-providers-terms-o | in_progress, gnosarium-ux | Carry as it is | Feeds M5: avatars, the agent loop's API key, the warnings the user asked for (mbe87e750ca35) |
| 6 | @item:primordial-engine/full-independent-review-of-primordial-engine-fab | deploy_ready, coordinator-opus | Carry, to done | Delivered, and the user has answered its questions. Close it once the coordinator judges its follow-ups (files 12-17) complete |
| 7 | @item:primordial-engine/critical-an-unvalidated-entry-can-be-confirmed-t | backlogged, world-chunks | Carry | M0-M1: probe-d1 as a test of pe-agree, and a mutation in the model (section 4.2). The old code stays unfixed |
| 8 | @item:primordial-engine/governance-defaults-strangers-start-without-admi | backlogged, ballpit-viz | Carry | M3, WS-gov: strangers without admin rights; guarded mode when no admin is reachable; capture-close. Its kick fix is superseded: the user ruled a strict 2/3 with no exception, and the three survivors are freed by unconfirmed play and the one-minute divergence. Its two probes are written as tests in M0 |
| 9 | @item:primordial-engine/session-validation-a-member-moves-only-its-own-h | deploy_ready, engine-architect | Carry | M1: "an input moves only its author's own hands" becomes a validity rule in pe-world, with its test and its control. The old branch's code never lands; supersede this item with WS-world's |
| 10 | @item:primordial-engine/verified-governance-for-state-transfer-joiners-b | backlogged, coordinator-opus | Carry | M3: governance lives in the hashed world state, so a joiner checks it against the cut's root. Its lying-donor test carries over |
| 11 | @item:primordial-engine/clock-sync-frequencies-converge-before-frame-0-s | backlogged, coordinator-opus | Carry | M1, WS-net: keep the fault-tolerant midpoint, drop the frequency loop, converge before frame 0, and then steer gradually (files 01 and 14). Restate its acceptance for pe-net |
| 12 | @item:primordial-engine/r16-chunk-coordinates-i64-backed-extending-to-ar | backlogged, coordinator-opus | Carry | WS-world: the format in M1 and `Pos` in M2 (file 17); WS-regions: the rest in M4. A cell index that promotes past the 64-bit range and never saturates (m7e85960c864a) |
| 13 | @item:primordial-engine/logic-shaders-and-assets-as-objects-unified-hier | backlogged, coordinator-opus | Carry | M1-M2, WS-logic and WS-world: its user rulings are requirements of the logic specification, and its open points are answered there, under systems over the store |
| 14 | @item:primordial-engine/owner-s-client-writes-perma-bans-into-the-world-s | backlogged, coordinator-opus | Carry | M5, saves: perma-bans become fields of the world's access list, written by ordinary governance entries and kept by the admins; with no world owner (file 18), an admin's client writes them. PB-1 and PB-2 follow from the log's order |
| 15 | @item:primordial-engine/port-the-randsched-widening-and-s6-to-pe-session | backlogged, engine-architect | Carry, re-scoped | M0-M1, WS-harness and WS-model: the widened randsched runs against pe-agree, and S6 becomes a property of the model. Nothing is ported to pe-session |
| 16 | @item:primordial-engine/gnosarium-the-flagship-game-on-the-primordial-en | blocked on the user, coordinator-opus | Carry as it is | The game's identity. The user has parked the business model (mbe87e750ca35); the interface direction feeds M5 |
| 17 | @item:primordial-engine/ball-pit-video-matrix-re-rendered-on-the-current | deploy_ready, ballpit-viz | Carry as it is | Built and approved; delivering it is the liaison's. Later videos come from pe-render, one per milestone, with the commit printed on them (file 09, A15) |
| 18 | @item:primordial-engine/localize-congestion-to-the-thin-link-player-othe | blocked, engine-architect | Park | Part B is not ported. Its goal, that nobody stalls because of one slow link, is an M3 acceptance test: the slow member keeps its vote, cuts wait for a quorum rather than for it, and after 2 s it merges with the main line. Its plan stays on `docs/partb-pe-plan` (222d887), and the spike at the archive tag |
| 19 | @item:primordial-engine/admin-indicators-holding-the-session-and-implaus | blocked, provenance-eng | Park | Its remaining parts need Part B. The idea, showing who is holding the session, carries into M3's display of the session's state (file 08, A6), next to the "not yet agreed" mark |
| 20 | @item:primordial-engine/pe-app-feed-r18-a-link-signal-delay-gradient-est | blocked, shader-lang | Park | QUIC's congestion control and a rate budget per link replace the R18 level ladder and its estimator. Its clock-drift findings go to WS-net |
| 21 | @item:primordial-engine/r16-neighbourhood-loading-and-provisional-arriva | deploy_ready, world-chunks | Park | The OWO loading rule (M4) supersedes it. Its spike's measurements are cost data (file 13, B.7) |
| 22 | @item:primordial-engine/kickliar-fed2-2-player-survivor-s-confirmation-t | backlogged, coordinator-opus | Park | A defect of Part B and the old core. The property it breaks (a survivor's finality never waits on removed members) becomes an M3 test of cuts |
| 23 | @item:primordial-engine/r18-controller-retune-for-the-3-player-thin-cell | backlogged, coordinator-opus | Park | Tuning of the replaced level ladder |
| 24 | @item:primordial-engine/thin-links-space-an-idle-member-s-acks-while-d56 | backlogged, coordinator-opus | Park | Tuning of the replaced level ladder (D56) |
| 25 | @item:primordial-engine/trading-lease-only-one-live-instance-of-a-world-t | backlogged, engine-architect | Park | The user ruled "there is no lease" (Q7, md2fa23dfa774), so nothing in the plan needs it. Parked, not dropped, as old items are (the coordinator's ruling) |
| 26 | @item:primordial-engine/pe-session-per-entry-wire-savings-signatures-pe-a | parked by the coordinator | Park | Wire savings in the old session core |
| 27 | @item:primordial-engine/r18-on-scenario-suite-pass-harness-batching-cost | parked by the coordinator | Park | The old harness's R18 batching cost |

## 8. Risks, and the evidence that settles each

| Risk | What settles it | When |
|---|---|---|
| QUIC without ring or aws-lc may not build, or the pure-Rust cryptography may be young | The feasibility check: quinn-proto with rustls and a pure-Rust provider builds, passes the native audit, and carries a datagram and a stream between two processes, on Windows and Linux. The report names the provider's crate and its audit status | M0 |
| OpenXR with wgpu needs interop below wgpu's public API, and the loader must be loaded at run time | The feasibility check: an OpenXR session (on the user's Virtual Desktop runtime only with a go through the liaison; a simulated one in CI if possible) shows a stereo frame drawn by wgpu | M0 |
| Cranelift needs wasmtime's one C file patched out on Linux and macOS, and metering must stop at the same points everywhere | The feasibility check (WS-logic), on all four hosted platforms | M0 |
| M1's logic edit may compile too slowly on the second PC for rollback inside the window | The measured acceptance line in M1: under 2 s, or the edit is made smaller | M1 |
| The display layer can lie: a blended object is drawn where it is not (file 14, 6) | Thresholds tuned with a person in VR at the M1 checkpoint; the error counters in every scenario | M1 |
| Finality on bad links: under cuts it depends on a proposer's view of a quorum (file 11, 9) | Measured as distributions in the shapes of the old thin-link cells | M3 |
| The merge against finality: replaying a returning machine's actions into final frames would undo them, and voiding them would lose work (@design-reviewer's H1) | The coordinator's ruling (a three-way merge at main's final frontier), checked by model v2 before merge v1 is built | M2 |
| Merging physics after a split: a contact group may spread so far that a merge replays a whole pile (file 14, 6) | Model v2 and v3 come before the code. A small experiment comes before the merge is promised. The one-minute bound caps the replay at 3,600 frames | M2-M3 |
| A replayed contact impulse may not reproduce the holders' result (file 16, 6.6) | WS-state's measurement before anyone builds on the spectrum; the holders' agreed state corrects any difference | Before M4 |
| A merge, or an activation after a slow compile, may need a long replay that blocks a frame | Replays run on a second copy in the background and are swapped in (file 14, 4.2) | M1-M2 |
| Rollback cost at 90 Hz in VR: a full keyframe costs about as much as a step (file 02) | M1 measures the ball pit at 90 Hz; incremental images follow in M2 | M1-M2 |
| Builds that differ across machines break activation after each machine's own compile (file 03: identical output shown only for six small modules on two x86-64 machines) | Tier 2 compares module bytes from the same source on all four platforms; a mismatch is a fault the hash tree catches, repaired by rungs 1-2 | M1 |
| Two objects of nearly equal size in adjacent layers (files 11 and 16) | M4's measurement with a person's hands, before the build | M4 |
| Twelve agents behind `heavy.sh`'s two slots | Iterate with `cargo check -p`; tier 1 covers only the crates a landing touches; branch builds and the platform matrix on hosted runners. The coordinator sets the slots | Throughout |
| Agents in Session 0 have no desktop, for windows or the headset | Place WS-render and WS-xr in a signed-in session; headless tests render offscreen | M0-M1 |
| The model's state space at five members with splits and merges | Small bounds, symmetry reduction, a time-bounded fast profile, and a deep profile run nightly | M0-M3 |
| Determinism has never run on ARM or macOS (file 02) | Tier 2 in M0 | M0 |
| With no world owner, a world with one admin lives on one machine, and being made an admin hands over the world for good (file 18, 4) | An easy backup under the admin's control (M5, saves); WS-gov's specification states who may add and remove admins (file 18's question 2) | M3, M5 |

## 9. Decisions

**The user's answers to the plan's four questions** (mc40fc6379bc6):

| # | Question | The user's answer |
|---|---|---|
| U1 | What "edit through an agent" covers in the slice | Add a logic edit to M1 (against my proposal, which put logic in M2; it adds about 6-9 engineer-weeks before the first checkpoint) |
| U2 | What "on finishing" means, for @fable-reviewer's review and the user's checkpoint | Checkpoints after M1 and M3, with @fable-reviewer reviewing each milestone's work as it lands |
| U3 | D12: "wasmi now, Cranelift later" | Cranelift from M2. The coordinator then brought it forward into M1 (19:52Z), so that M1's logic edit never ships on wasmi |
| U4 | The second machine for the slice | A second PC with a desktop window only; VR on this PC |

**The coordinator's rulings** (20:05Z, after @design-reviewer's read). C1-C13 are approved as proposed, with the
conditions shown:

| # | Decision | Ruled |
|---|---|---|
| C1 | The crate map and who owns what | Section 3.1, with one owner each: `Pos` and the offsets to WS-world; governance validity to WS-world until WS-gov exists; the merge to WS-diverge |
| C2 | How to archive | Tag `archive/v0-prototype`; the five crates being ported from go to `archive/` until M3; the rest is removed from the tree; `spikes/out` is removed except the records kept tests read; the old documents go to `docs/archive/` (section 2.4) |
| C3 | The model checker | Stateright, with its tier-1 profile time-bounded |
| C4 | The renderer | A small wgpu renderer for M1, with a short design document first; adopting one is evaluated before M5 (file 09, A5) |
| C5 | Transport | quinn-proto with rustls and a pure-Rust cryptography provider. The M0 check names the provider and its audit status |
| C6 | OpenXR | The `openxr` crate with the system loader loaded at run time; OpenXR's loader and runtime go in the register of boundaries |
| C7 | The gate's two tiers | Tier 1 locally before each landing, scoped to the crates it touches; tier 2 on hosted runners after each push; a red tier 2 blocks the next landing. @provenance-eng triages red tier-2 runs |
| C8 | When the merge engine is built | One merge function in pe-diverge: v1 in M2 and v2 in M3. Model v2 settles the merge against finality first |
| C9 | The sync model's proxy records | Replaced by a held shared body visible per player, only if it keeps Q13 (a): only the holder sees the held paddle |
| C10 | The fork and the format and lint gates | `box3d-fork` is excluded |
| C11 | Agent edits (D28a was a default) | Agent edits apply at once, labelled as the agent's and revertible in one step; confirmation stays available as an option |
| C12 | The UI lab | Right after M1, in parallel with M2 |
| C13 | pe-session's kept parts | Ported into new crates, so the old core is never built in the new workspace |

**The coordinator's other rulings**, folded in above:
- the quorum formula and the fault model per size (section 4.4; 20:05Z);
- the merge against finality (section 4.4; 20:05Z);
- no demotion of slow members (sections 1, 4.4 and 5.2; 20:05Z);
- one owner for the merge, and the additions to S2 (section 3.2; 20:05Z);
- guarded mode's "unreachable" as a recorded fact decided at a cut (M3; 20:05Z);
- tier 1 scoped to the crates a landing touches, with branch builds allowed on hosted runners (section 4.6; 20:05Z);
- @shader-reviewer as WS-diverge's second reviewer, and item 25 parked (20:05Z);
- exact integer cells under the position wall (M2; 20:17Z);
- M1 builds the inside-the-window half of activation, and M1's edit compiles in under 2 s on the second PC (M1;
  20:22Z and 20:26Z);
- the replayed-impulse measurement, owed by WS-state before anyone builds on the spectrum (M4; 19:36Z).

**@design-reviewer's Lows, folded in above:**
- the signed bytes bind the world id;
- cuts carry a line id;
- the missing controls are named (section 4.4);
- membership is re-founded per line at the one-minute mark (model v3).

## Appendix A. A sketch of the seams (v0 is fixed in M0)

A sketch, not code: names and shapes only. WS-seams fixes v0 from the owners' proposals, which are in their
specifications: @world-model's (S1, S2, S5), @diverge-eng's (S2-S5), @harness-eng's (S1-S3), @xr-eng's (S5) and
@iter-bench's (S2, S3, S5). Where they differ from this sketch, they decide v0.

```rust
// S1 Entries
pub struct Hash(pub [u8; 32]);            // BLAKE3; the only hash type
pub struct WorldId(pub Hash);             // the genesis hash; bound into every signature
pub struct ObjectId(pub u128);            // world-scoped
pub struct DomainId(pub u64);             // one per world until M4; then one per group of things in contact
pub struct LineId(pub Hash);              // a world line; a new one when a line re-founds its membership
pub enum Class { Input, Edit, Governance, Protocol }
pub struct Entry {
    pub author: MemberId, pub seq: u64, pub prev: Option<Hash>,
    pub domain: DomainId, pub stamp: Frame, pub class: Class,
    pub payload: Bytes,                   // decoded by the class's owner
}
pub struct SignedEntry { pub entry: Entry, pub sig: Signature }   // signs (world, entry)

// S2 Replica: one domain's deterministic state machine; object-safe
pub trait Replica {
    fn step(&mut self, frame: Frame, ordered: &[SignedEntry]) -> Vec<Outcome>; // Applied | Refused(reason)
    fn save(&mut self, group: GroupSel) -> Root;     // cheap: structural sharing; the whole domain or one group
    fn restore(&mut self, group: GroupSel, root: &Root);
    fn step_group(&mut self, group: GroupId, frame: Frame, ordered: &[SignedEntry]) -> Vec<Outcome>;
    fn root_hash(&self) -> Hash;
    fn node_hash(&self, path: &TreePath) -> Option<NodeHashes>;  // the subtree-hash exchange
    fn permits(&self, who: MemberId, what: Permission) -> bool;  // governance is in the hashed state
}

// S3 Agreement: sans-IO
pub trait Agreement {
    fn submit(&mut self, local: Entry) -> SignedEntry;
    fn receive(&mut self, from: PeerId, msg: &[u8]) -> Result<(), Fault>;  // never panics
    fn tick(&mut self, now: HostTime) -> Vec<Outgoing>;    // the frame comes from pe-net's sans-IO clock
    fn hold_votes_from(&mut self, frame: Frame);           // until this machine's build is ready (Q3)
    fn to_simulate(&self, from: Frame) -> Vec<&SignedEntry>;   // final + unconfirmed, in agreed order
    fn final_cut(&self) -> &Cut;          // height, frame, last entry per author, state hash, line id, votes
    fn events(&mut self) -> Vec<Event>;   // shares one trace vocabulary with the model
}
// The merge: one pure function in pe-diverge, called by pe-agree
pub fn merge(base: &Root, ours: &FinalFrontier, theirs: &Branch, rules: &Rules) -> MergeResult;

// S4 Transport: sans-IO queues over QUIC
pub enum Lane { Protocol, Action, Pose, Repair, Bulk }   // priority order
pub trait Transport {
    fn send(&mut self, to: PeerId, lane: Lane, bytes: Bytes);   // an unsent Pose is superseded
    fn poll(&mut self) -> Option<(PeerId, Bytes)>;
    fn link(&self, peer: PeerId) -> LinkStats;
}

// S5 Frame view: published once per frame, read-only; no GPU types
pub trait FrameView {
    fn frame(&self) -> Frame;
    fn time(&self) -> HostTime;           // the shared time base
    fn objects(&self) -> &[ViewObject];   // id, simulated pose, drawn pose, shape, appearance, own-hand and held flags
    fn rig(&self) -> RigTransform;        // the local player's rig, for cameras and late-latched hands
    fn effects(&self) -> &[EffectEvent];  // keyed by the causing event, so a replay never repeats them
}
pub struct HandInput { /* in the rig frame: hand, source, sampled_at, tracked, pose, velocities, grip,
                          closed, discontinuity; pe-xr's proposal */ }

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
| P6, P6b | Someone's code change compiling here (Q3) | Inside the window, replay from the agreed frame (M1); past it, the world-line merge with main (M2) | WS-logic, WS-diverge | M1, M2 |
| P7 | Entering an area whose state has not arrived | The last known state at once; fetch only what differs; fade in | WS-regions, WS-render | M4 |
| P8 | A per-player script acted on a guess later undone | Effects name what they read and are refused if stale | WS-logic | M2 |
| S1 | A slow member far behind | It keeps its vote; cuts wait for a quorum, not for it; its view follows by state at cuts; past 2 s, it merges with the main line | WS-agree, WS-diverge | M3 |
| S2, S3 | A split; too few for a quorum | Unconfirmed play for up to a minute, then a git-style merge under witnessed order, or the sides become branches that each re-found their membership; re-merging later by choice | WS-agree, WS-diverge, WS-world | M3 (re-merging, M5) |
| S4 | A crash and return, or a new joiner | State at a cut plus the recent log; watch first, act when caught up | WS-agree, WS-world | M1 (joining), M3 |
| F1, F3, F6 | Physics or builds that differ; different engine versions | Guards at connection and start-up; the hash tree; the repair ladder; the fault bundle | WS-net, WS-diverge, WS-world | M1 |
| F2 | A logic module that behaves differently on two machines | Metering in the module's bytes; logic state and module versions in the hash | WS-logic, WS-world | M1 |
| F4 | Memory or disk corruption | The background self-check; the repair ladder | WS-diverge | M3 |
| F5 | State outside the hash | Nothing that matters stays outside: governance, claims, logic state and module versions are all hashed | WS-world | M1 |
| F7 | Equivocation, or false state for a newcomer | Two versions of one entry prove against their author; newcomers fetch by a quorum-signed hash | WS-agree | M1-M3 |
| F8 | A protocol defect | The model first, and properties; no aborts | WS-model, WS-harness | M0 onward |

## Appendix C. Sources

- Review: `_teams/1847/fable-review/` files 00-18, and `appendix/evidence/` (the probes and their logs).
- @design-reviewer's read of b610464: `design-reviewer/read-refound-plan-b610464.md`.
- The user's rulings and the coordinator's: `_shared/user-design-decisions.md`, cited by message id above.
- Docket: the 25 live items listed in section 7, read on 2026-11-10.
- Repository: main 757219f; `Cargo.toml`; `crates/`, `spikes/`, `tools/`, `scripts/`, `docs/`.
