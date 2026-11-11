# pe-app: the application (WS-app)

Owner: @engine-architect. Draft 3, 2026-11-11 (@design-reviewer's reviews of drafts 1 and 2, mf5074c5ab2fd and
m94ee513a61ac; pe-render's desktop view, @ballpit-viz, mf78a9a2a2ecc). Plan: docs/refound-plan.md 3.1 and 3.2 (threads are WS-app's to decide
with WS-state, WS-render and WS-xr). Seams: crates/pe-seams (S1-S6, RULINGS.md).

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
- **The event loop never spins** (@shader-reviewer's L1 on pe-render part 2a, m22f2191207ba).
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
| The host clock | pe-app | every thread | one monotonic clock giving `HostTime` (QPC on Windows, `CLOCK_MONOTONIC` elsewhere), made once and handed to pe-render's `DesktopConfig`, pe-net's runner, pe-xr and the simulation thread, so every time is on one base |
| The session clock | network | simulation, XR, main | pe-net's clock model behind `FrameClock` (S4), read lock-free |
| Effects to play | main or XR thread | the audio sink (M1: counted and logged only), pe-xr's haptics | a bounded queue |
| Self-test requests and verdicts | simulation, worker | worker, simulation | a bounded queue each way |
| Fault bundles to write | simulation | I/O | a bounded queue |

A full outgoing queue is a local fault, counted and reported, never a stall (transport.md 7).

## 3. One simulation tick

At host time `now`, with `p = clock.frame_at(now)` (no tick before frame 0):
1. **Messages in.** Drain the network inbox. Protocol and Action go to `Agreement::receive`; Repair goes to
   pe-diverge; Bulk goes to the blob store. A `Fault` is counted and traced, never fatal.
2. **Local input.** Read both hands with `hand_at(hand, clock.host_time_of(p))`, drain the edit queue, and build the
   frame's entries (pe-world's input and edit codecs). Each goes through `Agreement::submit`, which signs it.
3. **Agreement's time.** `Agreement::tick(now, clock)`; its outgoing messages go to the network outbox.
4. **The replica.** pe-replica's `Driver::drive(replica, agree, p + 1, self_test)` (WS-state, @vdw-physics,
   m7e812664fbd8; @harness-eng's DESIGN.md 3.2). `present` is exclusive, so frame `p`, whose local entries step 2 just
   submitted, is simulated. It runs `advance`, answers `wants_roots` with `root_at_cut` and `vote` (one vote per cut),
   and passes `final_cut()` to `finalize` each time it changes. Its `DriveReport` holds `advance`, `votes`,
   `no_root` (the cuts with no root yet, each with its `RootError`; a `Pending` one is retried on the next call) and
   `finalized` (each `finalize` result; `NotReady` is retried too), so the driver needs no change for S2 v1.1
   (@vdw-physics, mc79c86c543b7).
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
2. **The member key** (pe-net-crypto's identity).
3. **GPU contexts, one at a time, on the main thread:** the XR context first in VR mode, then the window's.
   Creating contexts concurrently crashed D3D12 and WARP in 6 of 15 runs (render.md 4; @ballpit-viz); pe-render's
   creation lock covers its own calls, and pe-app never creates two from different threads.
4. **The network thread.** Open or dial the invite; on admission the welcome carries the world's epoch and rate, and
   the clock converges before frame 0 (transport.md 5.4).
5. **The replica:** a new world from the origin or a preset, an opened save (M5), or for a joiner the state at the
   newest final cut, fetched on the Bulk lane and imported (`import_at_cut`), then replayed to the present.
6. **The simulation thread**, then the XR thread (VR) or the main thread's drawing (desktop), then the MCP transports.

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
| pe-net (@shader-lang) | A runner that owns the network thread and returns its two queues and a `FrameClock` handle; the invite, dial and admission calls (transport.md 6) |
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

## 7. Tests for M1

- **No waiting:** the simulation keeps its rate with the render thread stalled, with the network inbox full, and with
  the edit queue full (each a test with a mock on the other side).
- **No stall in a repair:** during a rung-1 replay of `W_fg`, and during a rollback of `W_fg`, every tick ends on
  time, votes and messages keep flowing, and the frame view resumes within the catch-up time the budget gives: at
  the floor budget of two frames the replica gains one frame per tick, so a `W_fg` backlog catches up within `W_fg`.
  That bound is exact, so the test allows two ticks of slack (@design-reviewer, m81f8b1342bc9).
- **Startup without a GPU:** headless capture works, so CI runs the two-process test.
- **Shutdown:** the leave reaches the other member, which pauses for zero frames.
- **The two-process session** of section 6, on Windows and Linux.

## 8. Open points

- The audio sink: M1 counts and logs effects; the sound library is chosen later, under the 100% Rust rule.
- Where saves live and when they are written (M3, M5).
