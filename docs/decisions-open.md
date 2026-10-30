# Primordial Engine: decisions, what is built, and the main risks

As of commit 64c028d (main), 30 October 2026. This is a summary of design.md section 24 (and of the R16
and R17 decisions in sections 25 and 26) for the user. Section numbers point into
[design.md](design.md).

Evidence tags: **measured** = run on this machine (spike logs in `spikes/out/`); **modelled** = computed
from measured parts; **inferred** = design reasoning, not run.

## A. Decisions still needing you

Grouped by your priorities (optimization, fast iteration, stability, ease of use, accessibility). D22
comes first because it sets the order of everything else. New since the last report: **D72, D73, D57**.

### Order of work

| ID | Question | Options and their main cost | Recommendation | Evidence |
|---|---|---|---|---|
| D22 | In what order do we build, and what is the first milestone? | **Phases 2 to 6 as planned (23.2):** MCP server from phase 2; region-scoped rollback, two worlds with portals, and the proposer-based view change in phase 3; VR straddling of portals in phase 5. **VR earlier:** if VR is the headline, it delays networking. **Live editing before real networking:** if the AI-authoring loop matters most, it delays multiplayer. | Phases as planned | Inferred (23.2) |

### Optimization

| ID | Question | Options and their main cost | Recommendation | Evidence |
|---|---|---|---|---|
| D60 | How does one world hold many players who are spread out? | **(a) Regions:** each area simulates on its own and merges when players approach; physics starts 10 s after you arrive in empty space (spawn points and portals have no wait). **(b)** A 3 s wait: ordinary network hiccups then stall the whole session more often. **(c)** Arrivals stay tied to their previous region for 10 s. **Or no regions:** everyone simulates everything, about 16 players at most in one place. | (a), built in phase 3 | Modelled: 0 late cases in 33 (25.5). Measured: a late input costs 15-18x less re-simulation in its own region than in one flat world (25.10) |
| D2 | How many moving physics objects per world in the MVP? | **About 500:** a 6-frame rollback costs about 5 ms. **1,000:** 10-13 ms, over the 8 ms per-frame budget. **Parallel Box3D:** large work, and it must stay bit-exact. | About 500, then regions (D60) | Measured (4.4, 6.5) |
| D4 | Simulation rate, and how long a peer may run ahead unconfirmed? | **60 Hz, 90 Hz per VR-heavy world:** 90 Hz costs 1.5x the CPU (ball pit: 2.8 to 4.0 ms of re-simulation per frame, inside an 11 ms frame). **90 Hz everywhere.** Run-ahead limit **2 s**, or **1 s** (less memory, earlier pauses when the network stalls). | 60 Hz default, 90 Hz per world; 2 s | Measured (5, 7.5) |
| D73 | Under congestion, how does your own hand stay exactly where you put it? | **(a) Send several frames of hand positions per message:** your hand stays exact; others roll back a little deeper; confirmation lags up to 8 frames, and a cheater's room to backdate grows from about 4 to about 9 frames, only while congested. **(b) Keep thinning:** your physics hand lags your real hand by up to 8 frames (65 mm on average at 32 kbit/s): network-dependent input delay. **(c) Never thin your own hand much:** needs more bandwidth, so slow links are cut off sooner. | (a) | Measured lag (reviewer, 10.9); costs inferred |
| D57 | What game-logic memory is saved for rollback? | **(a) All of it:** hidden state cannot desync; about 1.1 MB per logic module, and about 0.1 ms per snapshot to find changed pages, because the runtime cannot track them. **(b) A declared area, everything else wiped every tick:** smaller snapshots, but about 1 MB wiped per module per tick and a strict memory layout for logic code. **(c) A declared area plus checks:** cheapest, but a mistake desyncs silently. | (a) | Image size measured (d1); costs inferred (7.4) |
| D23, D62 | How many players in one session, and close together? | **Up to 8 per session, all connected directly:** ack traffic grows with each member and is over budget from 3 members until compact acks exist (D64). **16-32 with relaying:** more protocol work in phase 3. Regions (D60) lift the session limit for spread-out players. **Crowds in one place:** cap active players per region at 16 and give the rest a few frames of input delay, or no cap (everyone's CPU grows with the crowd). | 8 for the MVP; crowd cap 16 | Measured: crowds cannot be made cheap by regions (25). Ack sizes measured (7.6) |
| D50 | What should the memory-versus-CPU slider offer? | **Four positions: Speed, Balanced (default), Lean, Minimum.** Balanced holds 8.3 MB instead of 47.7 MB while confirmation stalls for about 2 s, at the same CPU. **Two expert numbers** (snapshot spacing and depth). **A memory budget in MB.** | Four positions | Measured memory, modelled CPU (6.8) |
| D49 | How many worlds may one player have open? | **4 active and 8 loaded,** background worlds update in bursts of up to 250 ms and are parked after 2 minutes unseen. **No limits:** memory and CPU grow with every world. **One world at a time:** short of R14. | 4 and 8 | Measured: throttling a world never affects its other members (22.3) |
| D65, D66, D67 | Avatar culling defaults (R17) | Size-on-screen tiers with budgets of **16 full avatars and 8 avatar lights** per device, friends and the speaker first. **Avatar bodies push things?** Hands only by default; bodies or feet per world. **Per-avatar show, impostor and block,** with shader and particle caps on by default. | As listed | Measured in `spikes/chunks` (26.6) |
| D68, D70, D71 | Live-editable physics (R15): what ships when, and who may change it? | **Hooks** (materials, one-way platforms) in phase 3; **deeper changes** (contact edits, custom joints) in our Box3D fork in phase 4; **a whole replacement engine** allowed, with an active-body cap of about 48 (about 200 once logic runs on Cranelift). **Two rights:** hooks, and replace. **Alternatives:** no replacement engine; one right for everything; physics changes only between sessions. | As listed | Hook cost measured (27.5); caps inferred |

### Fast iteration

| ID | Question | Options and their main cost | Recommendation | Evidence |
|---|---|---|---|---|
| D11 | Who compiles game-logic edits? | **The author's machine compiles,** and peers run the shared module, identified by its hash: 0.45 s from edit to first frame. Then: **mirror the pinned Rust toolchain ourselves,** or accept rustup and crates.io as a soft central point. **Every peer compiles:** strongest trust, but every machine needs about 1 GB of toolchain, and edits land slower. | Author compiles; mirror the toolchain | Measured (iteration benchmark, 13.2) |
| D14a-c | Merge rules for live code (R7) | **One logic module is the unit:** edits to different modules combine without recompiling, and edits to the same module conflict. **Merge seat:** limited to the conflicting modules; your 10 s starts when you reach the front of the queue. **A commit that hits a held seat** is refused with a retry. **Alternatives:** strict linear history (every conflict means rebuild and resubmit); one global seat; 10 s counted from the refusal; queue such commits instead. | As listed | Inferred (15.3, 15.4) |

### Stability

| ID | Question | Options and their main cost | Recommendation | Evidence |
|---|---|---|---|---|
| D72 | Two players act on one object at once: may the engine undo one player's action? | **(a) Only grabs can lose:** two grabs within one latency, and the loser's grab is undone. **(b) Never refuse a physical action:** both apply, and two holders pull against each other. Costs: tug of war; anyone can pull a held object out of another player's hand (the griefing claims prevented), unless a world turns on exclusive holding or limits who may grab. Portal carries remain the one exception. **(c) Keep claims:** the loser sees their touch undone after one latency, against your rule that an author is never corrected. | (b) | Claims measured (session-sim); costs inferred (8.4) |
| D21 | Who may join a session in the MVP? | **Trusted or invite-only.** The protocol bounds backdating, proves double-dealing and evicts by majority; two colluders in four can still stall a session (safely). **Strangers from day one:** needs the phase-3 view change and per-object provenance checks first. | Trusted or invite-only | Measured: colluders stall without forking (9.3) |
| D45, D48 | Objects crossing between worlds (R14) | **Settled handoff:** others see the object about 0.15-0.2 s after it crosses, and it is never corrected. **Optimistic arrival:** 0.15 s sooner, but it is occasionally taken back, visibly. **Logic on carried objects** travels as data and runs only where the destination allows that code; or it runs everywhere it goes (worlds then run code they never approved). | Settled handoff; data only | Measured (22.6) |
| D10 | Who owns a world? | **The creator's key,** which can add co-owners, with an optional k-of-n rule for permission changes. **A required multi-owner rule.** **A majority of present members.** | Creator | Inferred (12) |

### Ease of use

| ID | Question | Options and their main cost | Recommendation | Evidence |
|---|---|---|---|---|
| D18a, D20 | Platforms and VR runtimes for the MVP | **Windows desktop and PC VR first** (SteamVR and Meta through OpenXR on Vulkan), Linux second, macOS flat-screen later. **Linux first.** **macOS in the MVP.** D3D12 on Windows instead of Vulkan. | Windows first | Read in sources (18) |
| D18b | Do VR hands always collide? | **Only when reaching for something:** cheap. **Always:** every hand becomes a constant stream of rollbacks, like the ball pit. | Only when engaged | Measured cost of a hand (7.5) |
| D61 | Players who scale up or down (R16) | **After a scale-up,** the simulated area grows: (a) the first 4 km at once, the rest at 100 m/s (x3 fully instant; x5.5 wakes distant objects over about 51 s); (b) always at 100 m/s (about 92 s); (c) 16 km at once (x9 instant, but regions must sit much further apart). **Giants among small players:** (a) exact same-frame interaction, small players also simulate the giant; (b) the giant's effects reach small objects 0.17 s late; (c) one shared region (a giant pulls everyone within about 380 km together). Gravity scales with the player by default; real gravity per world. | (a) and (a) | Modelled (25.6) |
| D43, D44 | Presence in several worlds, and portals | **A player present elsewhere** shows as a dimmed avatar with a badge, voice only where they choose. Or an away marker, an agent-driven avatar, or hidden. **Portals:** live portal views, at most 2 at once; or textures everywhere (blurrier); or streamed previews (shows a world to people it never admitted). | As listed | Inferred (22.1, 22.4) |
| D25, D25a, D26, D27a-c, D27e-h, D28, D28a, D29, D30, D30a, D30b, D31 | Agent panels (R13): safety defaults | Everything starts closed: your own panel without shell or web, panels private and locked, one sandboxed session per guest, per-turn limits, guests' commits wait for your approval, agent commits in shared worlds need confirmation, messages from other panels land in a separate inbox, text only, "ask me" for new connections, and only members of the same world session may ask. Friends are lists of keys kept on your devices, with no server. **Each costs some convenience;** the open alternatives let injected text act with your rights. | Closed by default | Inferred (21); not yet tested |
| D27d, D34 | Who pays for guests using your AI? | **A separate guest API key with a hard spend limit,** never your subscription. **Guests bring their own harness and key.** Question: do you want players to share paid AI capacity at all? We have not checked whether provider terms allow sharing subscriptions. | Guest key | Inferred (21.4) |
| D32 | Orgtree mail hub in the game | May the engine include a client for the hub's protocol (license and permission unverified)? A separate hub identity per player? Hub use only on private networks or behind TLS, since the hub has none? Agents never see the hub secret. | A separate identity, agents never holding the secret, first contact approved by you. The client itself only if the hub's terms allow it | Inferred (21.7) |
| D9, D35, D35a, D35b | Finding other players without servers | **Invites, local network, peer exchange and the public BitTorrent DHT,** plus volunteer relays and 4-word session codes; hosts announce a relay, not their IP. Questions: is relying on the public DHT and its bootstrap hosts acceptable? May an optional community relay serve players behind strict NATs? **Our own servers:** reliable, but a central point. **Invites and LAN only:** purest, worst to use. | DHT and volunteer relays | Read in sources (10.7, 11) |
| D19 | Resonite avatar import | Is parsing Resonite's own package format allowed by its terms? Work clean-room from exported files only? Are users responsible for avatar licenses when avatars travel to other peers? Should v1 include basic 3-point IK, which VR needs to wear an avatar at all? | Clean-room, glTF export preferred, license notice, basic IK | Inferred (19); no Resonite export on this machine |
| D1, D33 | What counts as an allowed non-Rust part? | Confirm: system components (GPU drivers, OpenXR, OS libraries) are fine when loaded at runtime. Gray areas: the Linux Wayland C shim; inline assembly in Rust crates (crypto, and wasmi's math); Windows speech recognition for voice input (D33). **Stricter:** X11 only on Linux, no assembly, no voice in the MVP. | Allow system components; inline assembly is Rust under your wording, and wasmi's is exactly rounded. The Wayland shim and Windows speech are your call | Audited (20) |

## B. Decisions you already made

| Topic | Your ruling | When |
|---|---|---|
| Spec and first step | The spec was complete; first a design document with feasibility spikes, for your review | 08:22Z (mf3a74747f292) |
| Priorities | Optimization, then fast iteration, stability, ease of use, general accessibility | 09:26Z (m59873bb6c65e, md82c979df50d, mcc12ae981c82) |
| 100% Rust | Engine and live game logic in Rust; no C or C++ compiled in. Shaders are exempt | 08:28Z (md35ba688f6b4, m4941630ba08d), 08:29Z (me2dc0c5dd765) |
| Physics library | The pure-Rust Box3D port, not C bindings | 08:28Z (mdeaf1393cbf5) |
| D12 logic runtime | wasmi everywhere now; Cranelift everywhere later, once wasmtime's C file is patched out and a fuel test passes on Windows, Linux and macOS | 15:10Z (mc164bb1fac63) |
| D13 shader language | WGSL only for now; Slang was evaluated and can be revisited | 15:40Z (m2cc3a7bbde68) |
| AI-forward | AI through MCP is the main author of code; machine-friendliness matters more than human ergonomics | 08:29Z (m255872a78955) |
| Mobile and standalone VR | Not in the MVP | 08:26Z (mfd61ee315bc0) |
| Resonite import scope | Avatar model and textures only, to start | 08:34Z (mb7a284842b5c) |
| Merge seat (R7) | Lasts 10 s | 08:51Z (direct message) |
| What an interaction sends (R3) | The resulting state plus the frame it happened on; every receiver ends at the same state | 09:30Z (m84a607d3a701, m03d2cee24def) |
| D37 same-frame ties | Any stable order; the architect picks the key (we use a per-frame shuffle, fair over time) | 09:31Z (m1f60f1f2f4eb) |
| Programmable systems | Logic runs locally on every peer and syncs only when disturbed | 09:39Z (m4a2a56c6323b) |
| Core goal | Every reaction immediate and physical for every player, everything in sync, minimal CPU and memory | 13:35Z (m389f4953435e) |
| D41 input delay | Not the answer: only a per-world opt-in, default 0 | 13:35Z (m389f4953435e) |
| Memory slider | Expose a memory-versus-CPU slider (D50 proposes its positions) | 13:36Z (m1a9698a5b37d) |
| van der Waals | Scale-dependent contact radius on by default; harmonic mean of the two sizes; small worlds use length units, not scaled tolerances | 14:20Z (mb9e06693543a) |
| Massive worlds (R16) | Double-precision coordinates, chunks of about 1 km, players of any scale, size-based distance culling for dozens to hundreds of players | 14:08-14:11Z (m1d7d6529a4bc, m3f7ef0e1f879, m5f7a478fee66) |
| Avatar culling (R17) | Built into the engine | 14:11Z (md5c9cdb98555) |
| D53 recovery (R18) | Recover from network collapse; never replay old input visibly | 14:29Z (m21c767333ba1) |
| D42 lateness | Nothing is re-timed, and an author never sees its own action corrected; the late receiver pays | 14:31Z (m21f5bdac80b3) |

## C. Decisions with no real choice left

The reviews or measurements left one sensible option. You can still overrule any of them.

* **D36, D54, D55 (confirmation and eviction):** a frame counts as confirmed only when every member provably
  holds it, and cheaters are evicted only with provable votes. Every faster variant we tried let honest
  players fork. Cost: about 8.5 frames (0.14 s) to confirm on a good link, more under jitter and loss.
* **D7 (network splits):** only a side with a strict majority continues; **a two-player session pauses**
  when the other player drops. The alternative can fork both sides.
* **D6:** two peers with the same inputs but different results both re-simulate, then pause; neither wins.
* **D46 (trust between worlds):** a majority of a world signs each object transfer, and worlds trade only
  along configured routes. Assumes each partner world has an honest majority.
* **D47:** no physics across a portal; each world stays independent (R14).
* **D8 (transport):** UDP with Noise encryption and our own reliability, which the spike already uses.
  quinn with pure-Rust crypto is the other 100%-Rust option (more integration work); iroh would need an
  exception for `ring`.
* **D53, D56, D42 parameters:** the congestion controller, its thresholds and demotion of a member that
  cannot keep up. Fixed rates collapsed under the measured caps.
* **D58:** 64-bit chunk coordinates with double-precision physics (2-10% slower) follow your
  double-precision ruling; it replaces D3.
* **D59, D64:** size-based simulation relevance (whole-chunk activation would wake about 25 million objects
  within 1 km), and compact acks at scale (per-member acks would cost 3.3 MB/s per player at 256).
* **D63:** a hashed grid instead of the octree you suggested: faster, and 2.4-4x smaller in both tests.
* **D69:** physics hooks are batched, under 2% of a step against 7-12% one call at a time.
* **D51, D52:** no per-island snapshots for now (the physics API cannot restore an island exactly);
  remote hands are predicted by dead reckoning (4-18x less error for observers).
* **D39, D40:** presence streams such as avatar poses also flow (otherwise avatars freeze), and an
  interaction carries the touched objects' state plus a digest of what they touch.
* **D14d, D15, D16, D17:** a module that runs out of fuel is disabled at that frame; our own deterministic
  component store instead of bevy_ecs; BLAKE3 hashes; a peer missing a new asset pauses only itself.
* **D5, D38:** "soonest wins" means the earliest frame, then the per-frame shuffle (your ruling); the claim
  rules depend on D72. **D24** was merged into D1.

## Built and designed

**Built: working spike code, with tests or scenario suites**
* Deterministic Box3D in Rust, with our van der Waals fork on by default: reference goldens, a 600-frame
  trace equal across release, debug and other build settings, snapshot and rollback.
* The session protocol: signed entries, confirmation, cheater proofs and eviction, removal of one crashed
  member, demotion, transfers between two worlds, congestion control. 30 scenarios in the session
  simulator all match their offline replays; a two-process UDP version also matches.
* The ball pit (500 balls and a hand), the snapshot study behind the slider, and the ball-pit video
  matrix renderer.
* Live game logic in WebAssembly (wasmi): hot swap at a fixed frame, rollback across it, and runaway code
  stopped by fuel. Modules are byte-identical when built at different paths.
* An MCP stub, the iteration benchmark (0.45 s edit to first frame), physics-hook cost measurements, and
  the R16 and R17 measurements (coordinates, relevance, spatial index, region rollback, avatar tiers).

**Designed only: text, not code**
* The engine itself: renderer, VR, scene graph and components, assets, Resonite import, MCP server,
  source control and merge seats, permissions, discovery and NAT traversal, encrypted transport.
* Agent panels (R13), multi-world rendering and portals (R14), physics layers beyond hooks (R15).
* Regions and groups for massive worlds (R16): modelled, not built.
* Logic-state snapshots (D57), compact acks, and relaying for larger sessions.
* **Phase-3 prerequisites:** the proposer-based view change (several failures or colluders at once,
  about the size of round 3's two largest fixes), region-scoped rollback, and compact acks.

## Top 5 risks

1. **Determinism beyond one machine:** with the default physics (van der Waals on) it is verified on this
   PC only; the Intel and Linux runs came before that change, and ARM and macOS never ran.
2. **CPU at scale:** the ball pit already needs about 2.8 ms of re-simulation per 60 Hz frame and goes
   over budget at 90 Hz or 100 ms latency; more than about 500 moving objects needs region-scoped
   rollback, which is designed, not built.
3. **Sessions can stall:** two colluders, or two members failing at once, stop a session (safely, without
   forking) until the phase-3 view change exists.
4. **Network growth:** ack traffic grows with every member and exceeds the budget from 3 members until
   compact acks exist, and two players need at least about 12 kbit/s each way.
5. **Agent panels:** sharing your AI harness with guests opens prompt injection and credential exposure;
   the sandboxing is designed but untested, and provider terms for sharing are unchecked.
