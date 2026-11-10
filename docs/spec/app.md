# pe-app: the application (WS-app)

Owner: @engine-architect. Draft 1, 2026-11-10. Plan: docs/refound-plan.md 3.1 and 3.2 (threads are WS-app's to decide
with WS-state, WS-render and WS-xr). Seams: crates/pe-seams (S1-S6, RULINGS.md).

## 0. In short

- **Five kinds of thread.** The main thread runs winit's event loop and, on the desktop, draws. The simulation thread
  owns agreement, the replica and divergence repair. The network thread owns the socket. In VR, pe-xr's XR thread
  draws the eyes. MCP handlers run on a small runtime pe-app starts.
- **The simulation thread never waits** on rendering, the network or the API. It waits only for its own next frame
  or for new input, and every hand-over is a bounded queue or an atomic swap.
- **pe-app is a binary over libraries.** Each crate keeps its own loop logic; pe-app spawns threads, connects their
  queues, and orders startup and shutdown. The loop that joins S2 and S3 is pe-replica's `drive`, so the harness runs
  the same code (section 3).

## 1. Threads

| Thread | Runs | Owns | Spawned with |
|---|---|---|---|
| Main | winit's event loop; on the desktop, pe-render's `Presenter` per frame, paced by `Fifo` present | the window, the desktop GPU context, the desktop hand | the process |
| Simulation | the tick of section 3 | pe-agree's `Agreement`, pe-replica's `Replica` (with pe-world and pe-logic inside), pe-diverge's state | a stack of pe-logic's `Settings::thread_stack()` (`max_wasm_stack` + 2 MiB; logic.md 5.1), because it runs wasm guests. The floating-point guard holds at every guest entry (N11) |
| Network | pe-net's `Core` over the socket (transport.md 7) | the socket, QUIC, the lanes, the rate budget, the clock | pe-net's runner |
| XR (VR mode) | pe-xr's frame loop (xr.md 7.1); it is the render thread in VR | the OpenXR session, the XR GPU context, the hand ring | pe-xr's runner |
| MCP | pe-mcp's transports on a tokio runtime of 2 workers; handlers call WorldApi through `spawn_blocking` (WS-api O9) | the token store (pe-app's, O9) | pe-app |

- **One code path for the event loop.** winit's loop must run on the main thread on some platforms (render.md 3);
  pe-app runs it there on every platform, so there is one order of events to test.
- **In VR mode** the main thread only pumps window events and input. The XR thread draws the eyes and, as a setting,
  the desktop mirror (render.md 10).
- **Background re-simulation and merges** (M2) run on a worker that steps a second replica copy. It is spawned with
  the same stack as the simulation thread, because it also runs guests.

## 2. Shared values and queues

| Value | From | To | How |
|---|---|---|---|
| The frame view (S5) | simulation | main or XR thread | pe-render's `FrameSlot`: one atomic swap of an `Arc<FrameView>` |
| The world snapshot, the frame, the newest final frame, the change record | simulation | pe-api's handlers | pe-api's snapshot port (api.md 9.1), published once per frame |
| Edits | pe-api's handlers | simulation | pe-api's bounded edit queue; full means `rate_limited` to the caller (api.md 9.2) |
| Hand samples | desktop hand (main thread) or pe-xr's ring | simulation | `HandFeed::hand_at(hand, host_time_of(frame))` (S5) |
| Messages in and out | network | simulation | pe-net's bounded single-producer queues of `Received` and `(peer, lane, bytes)` (transport.md 7) |
| The session clock | network | simulation, XR, main | pe-net's clock model behind `FrameClock` (S4), read lock-free |
| Effects to play | main or XR thread | the audio sink (M1: counted and logged only), pe-xr's haptics | a bounded queue |

A full outgoing queue is a local fault, counted and reported, never a stall (transport.md 7).

## 3. One simulation tick

At host time `now`, with `p = clock.frame_at(now)` (no tick before frame 0):
1. **Messages in.** Drain the network inbox. Protocol and Action go to `Agreement::receive`; Repair goes to
   pe-diverge; Bulk goes to the blob store. A `Fault` is counted and traced, never fatal.
2. **Local input.** Read both hands with `hand_at(hand, clock.host_time_of(p))`, drain the edit queue, and build the
   frame's entries (pe-world's input and edit codecs). Each goes through `Agreement::submit`, which signs it.
3. **Agreement's time.** `Agreement::tick(now, clock)`; its outgoing messages go to the network outbox.
4. **The replica.** pe-replica's `drive(replica, agree, p, self_test)` (WS-state; @harness-eng's DESIGN.md 3.2):
   `advance` to `p`; answer `wants_roots` with `root_at_cut` and `vote`; and pass `final_cut()` to `finalize` each
   time it changes. It returns what it did, including each `Finalized`.
5. **Divergence.** Hand S3's events and each `Finalized` to pe-diverge. It runs the repair rungs through S2
   (`replay_to`, `import_at_cut`, then `finalize` again; RULINGS.md "S2 v1") and returns its Repair-lane messages,
   which go to the outbox.
6. **Publish.** `FrameSlot::publish(replica.view())`, the snapshot port, and the effects queue.
7. **Wait** until `clock.host_time_of(p + 1)`, or until the inbox or the edit queue wakes the thread, whichever is
   first.

The harness's node driver runs steps 1 to 5 with its own network and clock, through the same `drive`, so the loop
that ships is the loop the properties check.

## 4. Startup and shutdown

**Startup**, in this order:
1. **Configuration.** The local settings file (TOML): the member key's path, the data directory, desktop or VR,
   whether MCP is on and its transports, the network ports. The world to open, or an invite to join. World settings
   (the simulation rate as a `SimRate`, the cut cadence as a duration, and the rest) live in the world, not here.
2. **The member key** (pe-net-crypto's identity).
3. **GPU contexts, one at a time, on the main thread:** the XR context first in VR mode, then the window's.
   Creating contexts concurrently crashed D3D12 and WARP in 6 of 15 runs (render.md 4; @ballpit-viz); pe-render's
   creation lock covers its own calls, and pe-app never creates two from different threads.
4. **The network thread.** Open or dial the invite; on admission the welcome carries the world's epoch and rate, and
   the clock converges before frame 0 (transport.md 5.4).
5. **The replica:** a new world from the origin or a preset, an opened save (M5), or for a joiner the state at the
   newest final cut, fetched on the Bulk lane and imported (`import_at_cut`), then replayed to the present.
6. **The simulation thread**, then the XR thread (VR) or the main thread's drawing (desktop), then the MCP transports.

**Shutdown**, in reverse: stop MCP; submit a leave entry and wait for its cut, up to a duration (2 s by default),
so others drop this member without a pause (plan 4.3); stop the simulation, XR and network threads; release the GPU.

## 5. What pe-app needs from each crate

| Crate | Needs |
|---|---|
| pe-replica (@vdw-physics) | A constructor from a world config and a seed; `drive` as one function over `&mut dyn Replica` and `&mut dyn Agreement` (section 3, step 4) |
| pe-agree (@world-chunks) | A constructor from the member key, the line and the world config; `Agreement` (S3) |
| pe-diverge (@diverge-eng) | Its state machine with `on_event`, `receive` and `tick(now, clock)` returning Repair-lane messages; fault bundles handed to pe-app to write |
| pe-net (@shader-lang) | A runner that owns the network thread and returns its two queues and a `FrameClock` handle; the invite, dial and admission calls (transport.md 6) |
| pe-render (@ballpit-viz) | Done in part 1: `GpuContext`, `FrameSlot`, `Presenter`, the desktop hand's `HandFeed`. Still needed: a call that creates the window's surface from winit's window, and the creation lock exposed to pe-xr |
| pe-xr (@xr-eng) | A runner for the XR thread that takes the GPU context, a `FrameSlot` reader and the clock, and returns its `HandFeed` |
| pe-api, pe-mcp (@api-eng) | The snapshot port and the edit queue; WorldApi; pe-mcp's transports started and stopped as a library |
| pe-logic (@iter-bench) | `Settings::thread_stack()` |

## 6. The M1 demo

The demo is the plan's M1 sequence (docs/refound-plan.md section 1, M1), as one script per machine: A opens the ball
pit and prints an invite; B joins on the desktop; both see and touch the balls; an agent on A edits the lamp script
through MCP; B's lamp changes at the agreed frame. The script drives pe-app through its configuration and MCP only,
so the same script is the two-process acceptance test.

## 7. Tests for M1

- **No waiting:** the simulation keeps its rate with the render thread stalled, with the network inbox full, and with
  the edit queue full (each a test with a mock on the other side).
- **Startup without a GPU:** headless capture works, so CI runs the two-process test.
- **Shutdown:** the leave reaches the other member, which pauses for zero frames.
- **The two-process session** of section 6, on Windows and Linux.

## 8. Open points

- The audio sink: M1 counts and logs effects; the sound library is chosen later, under the 100% Rust rule.
- Where saves live and when they are written (M3, M5).
