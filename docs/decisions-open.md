# Primordial Engine: decisions, what is built, and the main risks

As of 31 October 2026, after your decisions of 18:42Z-22:54Z on 30 October. Your rulings on world
ownership (D87, D88) arrive with the ownership branch. This summarises design.md section 24
(and the R16 and R17 decisions in sections 25 and 26). Section numbers point into [design.md](design.md).

Evidence tags: **measured** = run on this machine (spike logs in `spikes/out/`); **modelled** = computed
from measured parts; **inferred** = design reasoning, not run.

## A. Still open

From the protocol: three questions about admin permissions, raised by the P2-4 review (VCR-10) and put to
you as V1-V3 by @coordinator-opus. A role holding only the kick permission can kick the last admin, which
closes the session; the roles permission can grant Admin, so it equals Admin; and removing an honest but
slow sole admin closes the session for good. Recommended: only admins kick admins; only admins grant
Admin, and the roles permission grants only lesser roles; keep closing on the last admin's timeout, with a
longer grace period.

From your agent-panel rulings of 21:50Z-22:54Z: seven proposals, with @coordinator-opus. Each has options and
a recommendation in design.md section 24:
* **D81** the built-in speech models: which pure-Rust runtime, which model size, CPU or GPU (a spike first);
* **D82** a guest's or confidant's turn carries their own gaze and pointing, never yours;
* **D83** guidance annotations last until their author clears them, are never saved with the world, and
  admins may clear them;
* **D84** which settings your agent changes at once (with undo) and which need your confirmation;
* **D85** spoken replies are made on the panel owner's machine and streamed like a voice;
* **D86** an open panel shows others the conversation and summaries, never raw tool output;
* **D89** confidants' turns use your panels in Restricted mode unless you untick it, or get your panel's
  full profile, as ruled.

## B. Your rulings

| Topic | Your ruling | When |
|---|---|---|
| Spec and first step | The spec was complete; first a design document with feasibility spikes | 08:22Z (mf3a74747f292) |
| Priorities | Optimization, then fast iteration, stability, ease of use, general accessibility | 09:26Z |
| 100% Rust | Engine and live game logic in Rust; no C or C++ compiled in. Shaders are exempt | 08:28-08:29Z |
| Physics library | The pure-Rust Box3D port, not C bindings | 08:28Z (mdeaf1393cbf5) |
| AI-forward | AI through MCP is the main author of code | 08:29Z (m255872a78955) |
| Mobile and standalone VR | Not in the MVP | 08:26Z (mfd61ee315bc0) |
| Resonite import scope | Avatar model and textures only, to start | 08:34Z (mb7a284842b5c) |
| Merge seat (R7) | Lasts 10 s | 08:51Z |
| What an interaction sends (R3) | The resulting state plus the frame it happened on | 09:30Z (m84a607d3a701) |
| D37 same-frame ties | Any stable order, the architect's choice (a per-frame shuffle) | 09:31Z (m1f60f1f2f4eb) |
| Programmable systems | Logic runs locally on every peer and syncs only when disturbed | 09:39Z (m4a2a56c6323b) |
| Core goal | Every reaction immediate for every player, everything in sync, minimal CPU and memory | 13:35Z (m389f4953435e) |
| D41 input delay | Only a per-world opt-in, default 0 | 13:35Z |
| Memory slider | A memory-versus-CPU slider | 13:36Z (m1a9698a5b37d) |
| van der Waals | On by default; harmonic mean; small worlds use length units | 14:20Z (mb9e06693543a) |
| Massive worlds and avatars (R16, R17) | Double precision, 1 km chunks, any player scale, size-based culling; built-in avatar culling | 14:08-14:11Z |
| D53 recovery (R18) | Recover from network collapse; never replay old input visibly | 14:29Z (m21c767333ba1) |
| D42 lateness | Nothing is re-timed; an author never sees its own action corrected | 14:31Z (m21f5bdac80b3) |
| D12 logic runtime | wasmi everywhere now; Cranelift later | 15:10Z (mc164bb1fac63) |
| D13 shaders | WGSL only for now | 15:40Z (m2cc3a7bbde68) |
| D60 regions | Players load the region they are in and nearby regions: provisional arrival and following | 18:42Z |
| D2 physics budget | Parallel: multi-threaded Box3D in our fork, bit-exact across thread counts, not a 500-body cap | 18:44Z |
| D4 simulation rate | Configurable per world: 90 Hz target, 60 Hz fallback | 18:46Z |
| D49 open worlds | No limits | 18:51Z |
| D23, D62 session size | 8 players for the MVP; larger sessions a planned target; crowd cap of 16 as recommended | 18:51Z |
| D11 who compiles | Every peer compiles | 18:53Z |
| D21 who may join | Strangers from day one | 18:56Z |
| D72 two players, one object | Option (a): only grabs can lose | 18:56Z |
| D18a, D20 platforms | Linux out of the box, alongside Windows | 18:57Z |
| D27a, D34 unlocked panels | Guests get unconditional access, as though they were you; the agent is told who is sending. This replaces D27b, D27d, D27f, D27g, D27h and D28 (separate guest sessions, keys, approvals and reduced rights) | 19:00Z |
| Everything else in the old Table A | As recommended (section C) | 19:11Z (m160ec0e51f3e) |
| D32 messaging and the hub | All in-game messaging over our P2P network; the hub only as an optional bridge to Orgtree and Hubchat | 19:20Z |
| D32, refined | Every player and every agent panel gets a Hubchat identity automatically; engine panels are a fourth, purple mailhub client type | 19:21Z |
| D74 build mismatch | The peer pauses and the author gets a report; the peer may opt in to run the author's module for that commit | 19:58Z |
| D75 toolchain | Shared between peers and verified by hash, with the official Rust servers as the fallback | 19:58Z |
| D76 90 to 60 Hz | The world owner switches, warned by the engine | 19:58Z |
| D77 Linux voice | None in the MVP (the VR keyboard instead); **superseded at 21:51Z** by the built-in speech model on both systems (D33, below) | 19:58Z |
| D78 default hub | No default hub | 19:58Z |
| D79 double spend by a world that held the object | Detect only: caught by fork proofs when the copies meet, then the world is cut off; no birth-world registry, not even as an opt-in | 20:07Z |
| D80 who may change a session's member list | Session admins: an admin's signed kick removes anyone; the view change's two-thirds quorum only commits and orders changes. Always at least one admin, and the last cannot step down without handing on; everyone is an admin by default; the session closes when no admin is left. Admin is the only built-in role; join rules are an ordered matrix with the fixed row "an owner joins as Admin" | 22:11-22:51Z |
| D33, D77 voice | Voice prompting with one local, offline Whisper-class model built into the engine, on Windows and Linux; Windows speech recognition only as a lighter fallback. The models ship in the installer | 21:50-21:51Z, 22:23Z |
| Spatial context and guidance tools | Panels know where you look, what you look at and where you point; tools to point at objects, select areas and draw shapes for the agent | 21:52Z |
| D25 launch harnesses | Claude Code, Codex and Antigravity, through one generic harness layer, as Orgtree supports them | 21:54-21:55Z |
| Spoken replies | A text-to-speech tool the agent is encouraged to use after a spoken prompt; spoken to people nearby by default; your world mic stays open while you prompt (a setting can mute it) | 22:02Z, 22:23Z |
| Transcript and settings | Consecutive tool calls collapse into one row (details behind an advanced setting); game settings change by talking to your agent; guests cannot change your settings | 22:03Z, 22:28Z |
| D26 open panels | Openness first: people nearby hear prompts and replies, see guidance and watch agents work. A private-panel switch, off by default. Six things stay closed: the Locked default, approvals, credentials, inboxes, direct messages and eye tracking | 22:28Z, 22:34Z, 22:54Z |
| D27a confidants | Friends you mark as confidants may use your panels as you, and the agent is told; they do not confirm your approvals | 22:34Z, 22:54Z |
| Logic build profile (13.2) | "Fast builds": committed modules use `rel-fast` (no LTO) | 22:47Z |
| Where CI runs (P2-6) | Local machines only: this PC's Windows and WSL, plus <laptop>; ARM deferred | 19:58Z |
| Linux toolchain (P2-7) | rustup and build-essential may be installed in WSL Ubuntu-24.04 | 19:58Z |

The times are yours; @coordinator-opus relayed most of these rulings.

## C. Taken as recommended (19:11Z)

* **Physics and simulation:** D3 with D58 (64-bit chunk coordinates, double-precision physics); D50 (the
  slider: Speed, Balanced by default, Lean, Minimum); D51 (no per-island snapshots yet); D52 (dead
  reckoning for remote hands); D57 (all of a logic module's memory is snapshotted); D59 (size-based
  relevance); D61 (after a scale-up, 4 km wakes at once and the rest at 100 m/s; giants act on small
  players in the same frame); D63 (hashed grid); D68-D71 (physics hooks in phase 3, deeper changes in
  phase 4, a body cap for replacement engines, two separate physics rights).
* **Protocol:** D5, D6, D7 (a two-player session pauses when one drops), D36, D38 (a grab renews while
  held, no hold cap), D39, D40, the D42 and D53 parameters, D46, D47, D54-D56, D64 (compact acks at
  scale), D73 (packed hand targets).
* **Networking and discovery:** D8 (UDP with Noise encryption); D9 and D35 (invites, LAN, peer exchange,
  the public DHT, volunteer relays, 4-word session codes, a code's creator announces a relay).
* **Live code and source control:** D10 (the creator owns a world); D14a-d (one module is the merge unit,
  seats limited to the conflict, 10 s from the front of the queue); D15 (our own component store); D16
  (BLAKE3); D17 (a peer missing an asset pauses only itself).
* **Worlds:** D43 (a player present elsewhere shows as a dimmed avatar), D44 (live portal views), D45
  (settled handoff), D48 (logic on carried objects runs only where allowed).
* **VR, avatars, platforms:** D18b (hands collide only when reaching); D19 (clean-room import of exported
  files, glTF preferred, a license notice, basic 3-point IK); D33 (Windows speech recognition, revised at 21:51Z to a built-in model on both systems); D65-D67
  (avatar budgets, hands push by default, per-avatar safety controls); D1 (system components and Rust
  inline assembly allowed; the Wayland C shim turned out not to be compiled at all; D24 was merged into D1).
* **Agent panels:** D25, D25a (your own panel without shell or web by default), D26 (reversed at
  22:28Z: open by default), D27c and D27e (for
  inbox and decider sessions), D28a (agent commits in shared worlds need confirmation), D29-D31 (including D30a and D30b: incoming panel messages go to a separate inbox, text only by default).
* **Plan:** D22 (phases as planned), re-planned in section 23.2 for your rulings.

## Built and designed

**Built: working spike code, with tests or scenario suites**
* Deterministic Box3D in Rust, with our van der Waals fork on by default: reference goldens, a 600-frame
  trace equal across release, debug and other build settings, snapshot and rollback. Byte-identical on
  this PC, on <laptop> (Intel) and as a Linux build, the wasm modules included.
* The session protocol: signed entries, confirmation, cheater proofs, transfers between two worlds,
  congestion control, and (phase 2) the view change: several members removed at once, withheld or
  forked votes, colluders, a cheating proposer, a member that withholds references, and joins. Every
  scenario in the session simulator matches its offline replay; a two-process UDP version also matches.
* Per-object provenance for transfers between worlds (P2-5), checked against each world's epoch
  certificates.
* The ball pit (500 balls and a hand), the snapshot study behind the slider, and the ball-pit video
  matrix renderer.
* Live game logic in WebAssembly (wasmi): hot swap at a fixed frame, rollback across it, and runaway code
  stopped by fuel. Modules are byte-identical when built at different paths on one machine.
* The iteration benchmark (0.45 s edit to first frame), physics-hook cost measurements, and the R16 and
  R17 measurements (coordinates, relevance, spatial index, region rollback, avatar tiers).

**Built in phase 2, on main**
* The engine's core crates: ids, the fixed-step clock, bit-exact maths, random streams, hashing, and the
  sim store with copy-on-write snapshots and a checked state transfer (P2-1).
* The engine's MCP server, `pe-mcp`: Claude Code drove a simulated peer through it over a named pipe and
  over HTTP, with identical results (P2-8).
* Multi-threaded Box3D in our fork, bit-exact across thread counts (D2).
* The local determinism matrix (this PC's Windows and WSL, and <laptop>) and byte-identical logic builds
  across Windows and Linux, with the pinned toolchain (P2-6, P2-7).
* Provenance chains for objects carried between worlds (P2-5), and the session simulator as a tool (P2-9).

**Designed only: text, not code**
* The engine itself: renderer, VR, scene graph and components, assets, Resonite import, MCP server,
  source control and merge seats, permissions, discovery and NAT traversal, encrypted transport.
* Agent panels (R13) with signed sender attribution, cross-session messaging and relay mailboxes, the
  Orgtree bridge, multi-world rendering and portals (R14), physics layers beyond hooks (R15).
* Regions with provisional arrival and following (R16): modelled, not built.
* Every-peer builds (D11), logic-state snapshots (D57), compact acks, and relaying for larger sessions.
* **Built in the spike, not yet in the engine:** the view change and provenance chains, both MVP
  prerequisites since D21; they move into `pe-session` next (P2-3).

## Top 5 risks

1. **Determinism on ARM and macOS:** main is byte-identical on this PC, on <laptop> (Intel) and as a Linux
   build, but ARM and macOS have never run. Parallel physics must also stay bit-exact across thread
   counts.
2. **CPU at 90 Hz and at scale:** the ball pit already needs about 2.8 ms of re-simulation per 60 Hz frame;
   90 Hz costs 1.5x that. Parallel physics is the chosen answer, and its feasibility is still being
   studied.
3. **Strangers from day one:** the view change now removes several failed members, colluders and
   cheaters in the simulator, and with D80 settled a session cannot fork while at most about a third of
   its members cheat, proven or not. Above that, a partition plus one-sided proofs can fork it (measured
   as a control). Cheating stays bounded, not prevented.
4. **Every peer compiles:** byte-identical builds across machines and operating systems are unproven,
   joining a world means building its code, and every peer compiles other people's code (in a sandbox).
5. **Unlocked panels:** guests act with your rights and your AI account, which you accepted; prompt
   injection by a guest can do whatever your panel can. Confidants hold the same rights at all your
   panels. Open panels show prompts and replies to everyone nearby, so an agent that reads a secret can
   repeat it; the filter for secrets is best-effort.
