# pe-app: the application (WS-app)

Owner: @engine-architect. Draft 4, 2026-11-13: what the code slices settled (the network hook, step 2 once per frame
and its pacing, the M1 interim's two-machine start, the demo's commands). Draft 3, 2026-11-11 (@design-reviewer's
reviews of drafts 1 and 2; pe-render's desktop view, @ballpit-viz).
Plan: docs/refound-plan.md 3.1 and 3.2 (threads are WS-app's to decide with WS-state, WS-render and WS-xr). Seams:
crates/pe-seams (S1-S6, RULINGS.md).

## 0. In short

- **Seven kinds of thread.** The main thread runs winit's event loop and, on the desktop, draws. The simulation
  thread owns agreement, the replica and divergence repair. The network thread owns the socket. In VR, pe-xr's XR
  thread draws the eyes. MCP handlers run on a small runtime pe-app starts. A worker runs self-tests, and an I/O
  thread writes files.
- **The simulation thread never waits** on rendering, the network or the API. It waits only for its own next frame
  or for new input, and every hand-over is a bounded queue or an atomic swap.
- **pe-app is a binary over libraries.** Each crate keeps its own loop logic; pe-app spawns threads, connects their
  queues, and orders startup and shutdown. The loop that joins S2 and S3 is pe-replica's `drive`, so the harness runs
  the same code (section 3).

## 1. Threads

| Thread | Runs | Owns | Spawned with |
|---|---|---|---|
| Main | winit's event loop; on the desktop, pe-render's `DesktopView`: pe-app forwards every window event to its `window_event` (true means close) and calls its `about_to_wait`, which sets winit's control flow and requests a redraw only when it will draw; `RedrawRequested` draws one frame (pacing, the desktop hand, the `Presenter`, effects, the present) | the window, the desktop GPU context, the desktop hand | the process |
| Simulation | the tick of section 3 | pe-agree's `Agreement`, pe-replica's `Replica` (with pe-world and pe-logic inside), pe-diverge's state | a stack of pe-logic's `Settings::thread_stack()` (`max_wasm_stack` + 2 MiB; logic.md 5.1), because it runs wasm guests. The floating-point guard holds at every guest entry (N11) |
| Network | pe-net's `Core` over the socket (transport.md 7) | the socket, QUIC, the lanes, the rate budget, the clock | pe-net's runner |
| XR (VR mode) | pe-xr's frame loop (xr.md 7.1); it is the render thread in VR | the OpenXR session, the XR GPU context, the hand ring | pe-xr's runner |
| MCP | pe-mcp's transports on a tokio runtime of 2 workers; handlers call WorldApi through `spawn_blocking` (WS-api O9) | the token store (pe-app's, O9) | pe-app |
| Worker | the physics self-test, when pe-diverge asks for one (M1); from M2 also background re-simulation and merges on a second replica copy | its own scratch physics world | the simulation thread's stack, because from M2 it runs guests |
| I/O | writing fault bundles (M1) and saves (M5) | the data directory | pe-app |

- **One code path for the event loop.** winit's loop must run on the main thread on some platforms (render.md 3);
  pe-app runs it there on every platform, so there is one order of events to test.
- **The event loop never spins** (@shader-reviewer's L1 on pe-render part 2a).
  `DesktopView::about_to_wait` holds the rule for pe-app and pe-render's own demo loop alike: `Poll` while there is a
  frame to draw, since each drawn frame blocks in Fifo's present; `Wait` while the window is minimized or occluded,
  until the event that ends it; `WaitUntil` one period ahead before the first frame view and after a failed acquire,
  since the simulation's publish does not wake winit. pe-app leaves the view's `run_for` unset.
- **In VR mode** the main thread only pumps window events and input. The XR thread draws the eyes and, as a setting,
  the desktop mirror (render.md 10).
- **Nothing slow runs on the simulation thread.** A self-test that takes more than a frame runs on the worker, and its
  verdict feeds a later vote; file writes go to the I/O thread through a bounded queue.

## 2. Shared values and queues

| Value | From | To | How |
|---|---|---|---|
| The frame view (S5) | simulation | main or XR thread | pe-render's `FrameSlot`: one atomic swap of an `Arc<FrameView>` |
| The world snapshot, the frame, the newest final frame, the change record | simulation | pe-api's handlers | pe-api's snapshot port (api.md 9.1), published once per frame |
| Edits | pe-api's handlers | simulation | pe-api's bounded edit queue; full means `rate_limited` to the caller (api.md 9.2) |
| Hand samples | desktop hand (main thread) or pe-xr's ring | simulation | `HandFeed::hand_at(hand, host_time_of(frame))` (S5) |
| Messages in and out | network | simulation | pe-net's bounded single-producer queues of `Received` and `(peer, lane, bytes)` (transport.md 7) |
| Network events and pe-net's control end | network | simulation | each `NetEvent` (a join request, a link up or down) reaches the simulation's `NetHook` in step 1; the hook holds pe-net's `NetControl`, sends its commands (admit, refuse) and polls its lifecycle outcomes once a tick |
| The host clock | pe-app | every thread | one monotonic clock giving `HostTime` (QPC on Windows, `CLOCK_MONOTONIC` elsewhere), made once and handed to pe-render's `DesktopConfig`, pe-net's runner, pe-xr and the simulation thread, so every time is on one base |
| The session clock | network | simulation, XR, main | pe-net's clock model behind `FrameClock` (S4), read lock-free |
| Effects to play | main or XR thread | the audio sink (M1: counted and logged only), pe-xr's haptics | a bounded queue |
| Self-test requests and verdicts | simulation, worker | worker, simulation | a bounded queue each way |
| Fault bundles to write | simulation | I/O | a bounded queue |

A full outgoing queue is a local fault, counted and reported, never a stall (transport.md 7).

## 3. One simulation tick

At host time `now`, with `p = clock.frame_at(now)` (no tick before frame 0):
1. **Messages in.** Drain the network inbox, at most 1,024 messages a tick (`MAX_IN_PER_TICK`), so a flood cannot
   stall the tick. The rest wait in the network handle for the next tick: in pe-net's queue, or in the handle's own
   once `events()` has taken them (transport.md 7). So the cap bounds a tick's work, not memory; the peers' link
   budgets and QUIC's flow control bound that. Protocol and Action go to `Agreement::receive`; Repair goes to
   pe-diverge; Bulk goes to the blob store (not yet wired: the Repair and Bulk lanes). A `Fault` is counted and
   traced, never fatal. Each transport event goes to the `NetHook` (`on_event`), which also runs once a tick
   (`on_tick`) for what reaches it other than as an event. The hook owns pe-net's control end: in the M1 interim
   (section 4) @shader-lang's `MemberHook` admits a known member's new connection, refuses anyone else's, and counts
   and logs pe-net's lifecycle outcomes (@design-reviewer's Low on 5ca43067).
2. **Local input, once per frame.** On the first tick at frame `p`, read both hands with
   `hand_at(hand, clock.host_time_of(p))`, take at most one edit from pe-api's edit queue (not yet wired: pe-app
   does not drain it yet; it comes with pe-api's wiring), and build the frame's entries
   (pe-world's input and edit codecs), all on the member's one chain. Each goes through `Agreement::submit`, which
   signs it. A second tick in the same frame (a wake by the inbox or the edit queue) submits nothing. So a member
   makes at most two hand entries and one edit a frame, which pe-agree's rate budget passes at any world rate
   (agreement.md 2); a longer edit queue waits a frame per edit. While `Agreement::backlogged()` is true (after a
   stall, more of this member's own entries unacknowledged by its peers than a burst's worth; S3 v1.5), the frame
   makes no hand entries, which would be refused or void as late; edits are never held back, and
   `SimStats::hands_paced` counts the frames. Peers see none of its hand entries meanwhile, so a backlog that
   outlasts a claim's lapse may release what the member holds.
3. **Agreement's time.** `Agreement::tick(now, clock)`; its outgoing messages go to the network outbox.
4. **The replica.** pe-replica's `Driver::drive(replica, agree, p + 1, self_test)` (WS-state, @vdw-physics; @harness-eng's DESIGN.md 3.2). `present` is exclusive, so frame `p`, whose local entries step 2 just
   submitted, is simulated. It runs `advance`, answers `wants_roots` with `root_at_cut` and `vote` (one vote per cut),
   and passes `final_cut()` to `finalize` each time it changes. Its `DriveReport` holds `advance`, `votes`,
   `no_root` (the cuts with no root yet, each with its `RootError`; a `Pending` one is retried on the next call) and
   `finalized` (each `finalize` result; `NotReady` is retried too), so the driver needs no change for S2 v1.1
   (@vdw-physics).
5. **Divergence.** pe-app drains S3's events and hands pe-diverge **one ordered list**: the events drained before
   `drive`, with each of the report's `finalize` results placed right after the `CutFinal` of its cut (first in the
   list if that `CutFinal` came in an earlier tick, because the cut's entries were late), then the events drained after
   `drive`. So a `CutFinal` at height h, its result, and a `RootSplit` at h + 1 reach pe-diverge in that order.
   pe-diverge returns actions, each routed in this tick:
   - Repair-lane messages: to the outbox;
   - `Revote { cut, root }`: to `Agreement::vote`, with the current self-test verdict;
   - `RunSelfTest`: to the worker's queue. When the verdict comes back, it is the `self_test` that later votes and
     `drive` calls carry;
   - `WriteBundle`: to the I/O thread's queue.

   Its repairs run through S2 v1 (`replay_to`, `import_at_cut`, then `finalize` again; RULINGS.md "S2 v1"), within
   the replica's budget (below).
6. **Publish.** `FrameSlot::publish(replica.view())`, the snapshot port, and the effects queue.
7. **Wait** until `clock.host_time_of(p + 1)`, or until the inbox or the edit queue wakes the thread, whichever is
   first. Before frame 0 the wait is at least one frame period, since `host_time_of` means nothing while the clock
   converges; the network thread wakes the simulation thread when it converges.

**Bounded work per tick.** A deep rollback, or a repair's replay of up to `W_fg` (2 s, 120-240 frames), done in one
tick would stall the simulation for hundreds of milliseconds, and with it agreement's votes and messages for every
other member. So the replica re-simulates at most a budget of frames per call, expressed as simulated time (a local
setting, deterministic for the harness; pe-app may adjust it from the measured step cost), and resumes on the next
call. Until it has reached the present again it publishes no new frame view, so the display keeps drawing, and
extrapolating, the last one, while agreement keeps ticking and voting. The budget is never below two frames of
simulated time at the world's rate, because real time adds a frame per tick and a budget of one frame never catches
up (S2's floor; @design-reviewer's Low on 549f4ca4); pe-app raises it from the measured step cost, never below that
floor. Each hold is counted, with its length; the objects that jump when the view resumes are blended and, above
`E_cue`, cued like any correction (render.md 8, D2-D3). A replay or an import that does not fit in one
call returns "pending" and is called again on a later tick. The budget is part of S2 (`Advance::caught_up`,
`RootError::Pending`, `ImportError::Pending`). M2's second copy then keeps the old prediction moving during a deep
replay instead of holding the view.

The harness's node driver runs steps 1 to 5 with its own network and clock, through the same `drive`, so the loop
that ships is the loop the properties check.

## 4. Startup and shutdown

**Startup**, in this order:
1. **Configuration.** The local settings file (TOML): the member key's path, the data directory, desktop or VR,
   whether MCP is on and its transports, the network ports. The world to open, or an invite to join. World settings
   (the seed, the simulation rate as a `SimRate`, the cut cadence as a duration, and the rest) live in the world, not
   here, so two members never open one world with different settings; a joiner takes them with the world's state.
2. **The member key** (pe-net-crypto's identity), derived from this machine's identity seed. In M1 the seed is the
   file `identity.seed` in the per-user data folder, made from the OS's randomness on first use:
   `%LOCALAPPDATA%\Primordial Engine` on Windows (per machine, never the roaming profile, which could carry one
   identity to two machines), `~/Library/Application Support/Primordial Engine` on macOS, and elsewhere
   `$XDG_DATA_HOME/primordial-engine`, else `~/.local/share/primordial-engine`. It is never the working directory,
   where a checkout could commit it. `--identity <path>` names another file.
3. **GPU contexts, one at a time, on the main thread:** the XR context first in VR mode, then the window's.
   Creating contexts concurrently crashed D3D12 and WARP in 6 of 15 runs (render.md 4; @ballpit-viz); pe-render's
   creation lock covers its own calls, and pe-app never creates two from different threads.
4. **The network thread.** Open or dial the invite; on admission the welcome carries the world's epoch and rate, and
   the clock converges before frame 0 (transport.md 5.4).
5. **The replica:** a new world from the origin or a preset, an opened save (M5), or for a joiner the state at the
   newest final cut, fetched on the Bulk lane and imported (`import_at_cut`), then replayed to the present.
6. **The simulation thread**, then the XR thread (VR) or the main thread's drawing (desktop), then the MCP transports.

**The M1 interim** (@coordinator-opus; pe-app's `net`, @shader-lang's, and `genesis`): pe-agree has no
join from a cut yet, so the second machine becomes a member at genesis, and steps 4 and 5 run as one exchange.
- **A** (`--open`) prints an invite and waits for B's reply code. It checks that the reply answers its own invite
  and that the invite has not expired, then dials B. On B's first join request it builds the genesis record: the
  build, both member keys with their roles (A `ADMIN | EDIT | AGENT`, B `EDIT | AGENT`; @coordinator-opus), the world's rate, cut interval and gravity, the preset and its arguments, a fresh nonce from the
  OS, the epoch and the frame-0 roots, the world's as built and the replica's. The epoch puts frame 0 `EPOCH_LEAD`
  (2 s) after that request, so that B's clock can converge first: the aim, not a guarantee. The session's line is
  derived from the record. A admits B with a welcome that carries the record.
- **The paste.** When the user pastes B's reply code by hand, B's first join request comes from its early dial,
  already closed, so the lead must also cover A's dial and handshake. That is why B accepts a frame 0 up to 20 s in
  the past and replays from it. If the paste takes longer than `T_welcome` (10 s), A closes B's early dial with
  close code 8; B waits through it, and A's own dial carries the session.
- **B** (`--join <invite>`) prints its reply code for A and dials. The welcome names the fixed pre-genesis line that
  every hello carries; B derives the session's line from the record itself, never from the welcome (transport.md
  6.4). B checks that the record names exactly itself and the invite's key, the build, that the welcome's epoch and
  rate equal the record's and the rate its own, and, once its clock has converged, the epoch (at most 20 s past and
  10 s ahead). It then builds the world from the record and refuses, by name, a frame-0 root that differs from A's.
- **Anyone else's join request is refused** ("not admitted"). When pe-agree's join from a cut lands, only B's path
  changes: it imports the state at a final cut (step 5) instead of building genesis.
- **The consensus settings and the voters** (the gate's item 1, main bf521179): the record's `agree` is this build's
  `NodeConfig::consensus_hash` (@world-chunks), the hash of the settings every member must share. B's check
  compares it before building anything; `Genesis::start` compares the configuration it runs on both machines, and
  net.rs again before `Node::new`, so a member with other settings is refused by name (`GenesisError::Agree`)
  instead of refusing the other's entries, votes or cuts later. `start` also refuses a world whose governance members
  aren't the record's, which are pe-agree's voters (`GenesisError::Governance`), since logic.md 6.2a's voter
  snapshot relies on every member voting in M1 (@design-reviewer).

**Shutdown**, in reverse:
1. Stop MCP.
2. Submit a leave entry, and no local entry after it. The simulation and network threads keep ticking while pe-app
   waits, because the leaver's own vote may be needed for the cut that includes the leave (at n = 2, q = 2).
3. Stop waiting as soon as a final cut includes the leave, after 2 s (a duration), or at once when no other member is
   connected. Then the others drop this member without a pause (plan 4.3).
4. Stop the simulation, worker, XR and network threads; flush the I/O thread; release the GPU.

## 5. What pe-app needs from each crate

| Crate | Needs |
|---|---|
| pe-replica (@vdw-physics) | A constructor from a world config and a seed, taking the frame clock (`ReplicaConfig::clock`, so the view's `sim_time` is on the shared time base); `Driver::drive` over `&mut dyn Replica` and `&mut dyn Agreement` (section 3, step 4); the per-call budget of section 3 |
| pe-agree (@world-chunks) | A constructor from the member key, the line and the world config; `Agreement` (S3) |
| pe-diverge (@diverge-eng) | Its state machine with `on_event` over the ordered list of section 3, `receive` and `tick(now, clock)`, returning its actions (Repair messages, `Revote`, `RunSelfTest`, `WriteBundle`); a repair that is pending continues on later ticks |
| pe-net (@shader-lang) | `NetHandle`, the simulation's `Transport`, with its `frame_clock`; `control()`, a `NetControl` for commands and lifecycle outcomes (the hook's; section 3, step 1); `spawn_with`'s arrival callback, which wakes the simulation thread; the invite, reply and dial (pe-net-crypto and `Core`; transport.md 6) |
| pe-render (@ballpit-viz) | Part 1: `GpuContext`, `FrameSlot`, `Presenter`, the desktop hand's `HandFeed`. Part 2: `window::surface_and_context(Arc<Window>)` (it takes the creation lock itself), `DesktopView` for the main thread with `window_event` and `about_to_wait`, and `gpu::creation_lock()` public for pe-xr |
| pe-xr (@xr-eng) | A runner for the XR thread that takes the GPU context, a `FrameSlot` reader and the clock, and returns its `HandFeed` |
| pe-api, pe-mcp (@api-eng) | The snapshot port and the edit queue; WorldApi; pe-mcp's transports started and stopped as a library |
| pe-logic (@iter-bench) | `Settings::thread_stack()` |

## 6. The M1 demo

**The desktop player** (B, U4) moves only the camera's eye within the rig (WASD and right-drag, pe-render), and
touches with the desktop hand: `Hand::Right` from pe-render's `DesktopHandFeed`, read through `HandFeed` like pe-xr's
hands. Moving the rig itself, an input entry, is after M1: the demo does not need it.

The demo is the plan's M1 sequence (docs/refound-plan.md section 1, M1), as one script per machine: A opens the ball
pit and prints an invite; B joins on the desktop; both see and touch the balls; an agent on A edits the lamp script
through MCP; B's lamp changes at the agreed frame. The script drives pe-app through its configuration and MCP only,
so the same script is the two-process acceptance test.

**The commands** (the M1 interim, until the configuration of section 4 replaces them):
- A: `pe-app --vr --open --advertise <A's address>:4433`. It prints an invite, then reads B's reply code on standard
  input.
- B: `pe-app --join <invite> --advertise <B's address>:4433`. It prints its reply code for A, then joins.

`--listen` is the UDP address to bind (default `0.0.0.0:4433`). `--advertise`, which may be repeated, is the address
the invite or the reply names. With the default listen address it is required, since `0.0.0.0` names no machine;
between the user's machines over Tailscale it is the machine's Tailscale address. `--seconds <n>` ends a run after
`n` seconds. Without `--open` or `--join` the world runs alone.

**Finality at n = 2.** The quorum of two members is both (q = 2): a cut is final only when both have voted for it.
So finality follows the slower machine, and while either machine stalls or its link is down, nothing new becomes
final on either, though both keep simulating their predictions.
- **A stall shorter than pe-net's idle timeout** ends when both vote again, within the view timeout reached by then
  (it grows by half after each timed-out view, up to 10 s). In M1 `t_idle` is 30 s, a stated setting, so that a
  Wi-Fi blip or a Tailscale path switch does not end the session (@coordinator-opus). The entries a
  member made meanwhile reach the other when the link is back. Its hand entries are paced while it is backlogged
  (section 3, step 2), and its edits are delayed, never lost, and final once a cut covers them. An entry stamped
  below a cut that was already final when it arrived is void as late; at n = 2 that is rare, since no cut becomes
  final while one member is cut off.
- **A longer outage ends the session for both.** pe-net closes a link silent for more than `t_idle`, and in M1
  nothing dials again: pe-net and pe-app have no redial. It is as if a process had ended, and going on takes a new
  session with a new invite. The redial comes once pe-agree has return (model v1); the hook already welcomes a
  member that connects again.
- **pe-agree v0 has no leave, return or restart** (agreement.md 8). Once either process ends, the other never
  finalizes again. Restarting the ended process does not help: it would start its chain again at seq 1 under the
  same key, which its peer sees as a second chain or an equivocation. Start a new session.

## 7. Tests for M1

- **No waiting:** the simulation keeps its rate with the render thread stalled, with the network inbox full, and with
  the edit queue full (each a test with a mock on the other side).
- **No stall in a repair:** during a rung-1 replay of `W_fg`, and during a rollback of `W_fg`, every tick ends on
  time, votes and messages keep flowing, and the frame view resumes within the catch-up time the budget gives: at
  the floor budget of two frames the replica gains one frame per tick, so a `W_fg` backlog catches up within `W_fg`.
  That bound is exact, so the test allows two ticks of slack (@design-reviewer).
- **Startup without a GPU:** headless capture works, so CI runs the two-process test.
- **Shutdown:** the leave reaches the other member, which pauses for zero frames.
- **The two-process session** of section 6, on Windows and Linux.

## 8. Open points

- The audio sink: M1 counts and logs effects; the sound library is chosen later, under the 100% Rust rule.
- Where saves live and when they are written (M3, M5).
