# pe-app: the application (WS-app)

Owner: @engine-architect. Draft 7, 2026-11-17: joining from a cut (section 4), which replaces the M1 interim's
two-member genesis. Draft 6, 2026-11-17: the graceful quit (section 4's shutdown) and the step budget's keep-up
floor (3). Draft 5, 2026-11-13: pe-api's ports (3.1). Draft 4, 2026-11-13: what the code slices settled (the network
hook, step 2 once per frame and its pacing, the M1 interim's two-machine start, the demo's commands). Draft 3,
2026-11-11 (@design-reviewer's reviews of drafts 1 and 2; pe-render's desktop view,
@ballpit-viz).
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
   pe-diverge; Bulk goes to the blob store (not yet wired: the Bulk lane). A `Fault` is counted and
   traced, never fatal. Each transport event goes to the `NetHook` (`on_event`), which also runs once a tick
   (`on_tick`) for what reaches it other than as an event. The hook owns pe-net's control end: in the M1 interim
   (section 4) @shader-lang's `MemberHook` admits a known member's new connection, refuses anyone else's, and counts
   and logs pe-net's lifecycle outcomes (@design-reviewer's Low on 5ca43067).
2. **Local input, once per frame.** On the first tick at frame `p` (the shown present `p_s` while an XR session
   draws, 3.3), read both hands with `hand_at(hand, clock.host_time_of(p + 1))`: a frame's view is the state at its
   end, after the step (pe-replica's frame after `advance`), so its hands go in at that moment and the simulated
   hand is where the real one is when the view says. Take at most one edit from pe-api's edit queue (3.1), and build
   the frame's entries, stamped at `p` plus stamp-ahead's lead (3.2), never below the chain's last stamp
   (pe-world's input and edit codecs), all on the member's one chain. Each goes through `Agreement::submit`, which
   signs it. A second tick in the same frame (a wake by the inbox or the edit queue) submits nothing. So a member
   makes at most two hand entries and one edit a frame, which pe-agree's rate budget passes at any world rate
   (agreement.md 2); a longer edit queue waits a frame per edit. A grasp's edge (below) adds its hand's grab or
   releases to that frame, which the budget's half-second burst absorbs. Before encoding an edit, step 2 sets each
   `CommitScript`'s `activate_at` to the frame it stamps the entry at (`Transaction::set_activation`): pe-api leaves
   it at 0, since only this machine knows the stamp, and pe-world refuses a commit whose `activate_at` is not its
   stamp (api.md 7.1, world-model.md 7.9). While `Agreement::backlogged()` is true (after a
   stall, more of this member's own entries unacknowledged by its peers than a burst's worth; S3 v1.5), the frame
   makes no hand entries, which would be refused or void as late; edits are never held back, and
   `SimStats::hands_paced` counts the frames. Peers see none of its hand entries meanwhile, so a backlog that
   outlasts a claim's lapse may release what the member holds. A member whose every other member left unconfirmed
   (its link dropped with its signed `Leave` in hand, E not final here; section 4) isn't paced: nobody is left to
   acknowledge its entries, and it was told it can keep playing (M1 redo F2). A link that comes back ends that.

   **A hand's grasp** (world-model.md 7.2, 8.1; @coordinator-opus and its ruling on the defaults,
   after @fable-reviewer). A hand entry whose `engaged` differs from the one last sent for that hand is the grasp's
   edge; the raw sample's changes are not.
   - **A close** (false to true) also sends a `Grab` for that hand, in the same frame, so pe-world measures its reach
     after the hand entry moved the hand. It names the object pe-world's `pick_grab` picks on the replica's present
     world: of the bodies marked `grabbable`, hands excepted, those a `Grab` would claim, the nearest by the distance
     from the target just sent to its nearest bounding sphere's surface, ties to the lowest id.
     - "Would claim" is `grab()`'s own checks (`grab_check`), so the pick can't drift from the world's rule. Not held
       by another member, nothing in its subtree held by one, not under the member's own cooldown; an object the
       member's other hand holds may be picked, and the grab moves the claim.
     - Within `reach_mm` alone: the slack is left for prediction error.
     - With nothing in reach, nothing is sent. A grab another member's claim beats in the agreed order is refused
       `Held`, and is not tried again while the hand stays closed.
   - **An open** (true to false), a lost hand's release included, sends a `Release` for each object that hand holds,
     and for its last close's grab, whose claim a view may not show yet. It leaves out what the other hand last
     grabbed: pe-world's `Release` ends the member's claim whichever hand holds it, so a ball passed between hands
     stays in the hand that took it, even before the present world shows the pass (@design-reviewer's Medium on
     82644261).
   - **A throw** (the weak-throw fix; @item:primordial-engine/throws-leave-at-the-hand-s-speed-letting-go-carr;
     world-model.md 8.1): where the feed gives the hand's velocities at the tick's real present (the moment the open
     was read, since the grasp is never forecast, 3.3), the open sends a `Throw` in place of each `Release`, carrying
     the object's velocity with the hand at that moment, in the world frame: the hand's linear velocity plus its spin ×
     the offset from the hand to the object, and the spin. The offset is to the object's origin, as the view gives it:
     its centre of mass for M1's balls; an object whose centre of mass is off its origin (a compound) is measured at
     its origin until pe-app reads the centre (world-model.md 8.1 asks for it; a follow-up). The velocity is the
     hand's relative to the rig: M1's rig doesn't move, and smooth locomotion would add the rig's own velocity
     (@design-reviewer on d59b0fc3). A throw whose values aren't all finite goes as a plain `Release` (a refused throw
     would leave the claim to lapse). Each throw is logged (`throw:`) with its speed and the fastest the object moved
     with the hand over the 50 ms before (`THROW_PEAK_WINDOW`), so a window can be tuned with no format change;
     `SimStats::hands` counts them (`throws`).
   - The desktop hand and VR's take this one path. An agent's hand-less grab is not part of it.
   - The end line counts each hand's grasps (`SimStats::hands`): its edges (`closes`, `opens`), the grabs and releases
     sent, and the grabs applied and refused by reason (`Held`, `OutOfReach`, `Cooldown`, ...), as the newest view
     tells them. The tally keeps every grab of the session, as `--check-replay` keeps every cut: small at M1's scale.
     A reason's name is its `Debug` name.
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
   - `RunSelfTest`: run inline while it takes under a frame on M1's machines (pe-physics's self-test: 3.0 ms in
     release on the M1 PC; @engine-architect times the laptop), otherwise on the worker. The verdict is `Passed` when
     the digest equals the start-up one, else `Failed` (divergence.md 5, step 2); it is the `self_test` that later
     votes and `drive` calls carry. pe-physics takes every digest after resetting the calling thread's floating-point
     mode to the default, as the replica does before every frame, and reports a mode that wasn't
     (`SelfTestDigest::mode_reset`: counted in `self_test_mode_resets` and said on the console; @coordinator-opus), so any thread may take it. The start-up digest is taken once per process, before the hello (when
     the endpoint binds, or at pe-diverge's first step without one), and that one value is both the hello's
     `selftest` and the start-up digest (G1, `crate::diverge::startup_self_test`; @design-reviewer's Low 1 on
     d8f37aa2). A self-test that can't run gives the hello random bytes, so every join is refused at the hello;
   - `WriteBundle`: to the I/O thread's queue (`BUNDLE_QUEUE`, 8). A full queue drops the bundle and counts it
     (`bundles_dropped`), never blocks. The `pe-io` thread writes it under its name in `<identity file>.bundles`,
     beside the identity file, so two instances on one PC never write one name twice.

   pe-diverge runs in a session (`Sim::with_diverge`); a world alone has no peer to split from. Before `drive`, pe-app
   keeps the cuts `wants_roots` names, by `CutRef`, until a cut past them is final: a `RootSplit` names only the
   `CutRef`, and pe-diverge replays to the whole cut. A `RootSplit` for a cut it never kept is counted
   (`splits_unknown`) and logged.

   Its repairs run through S2 v1 (`replay_to`, `import_at_cut`, then `finalize` again; RULINGS.md "S2 v1"), within
   the replica's budget (below).
6. **Publish.** `FrameSlot::publish(replica.view())`, the snapshot port and the change record (3.1), and the
   effects queue. Before the slot, an object one of this member's own `Release`s let go of (an applied record of a
   `Release` step 2 sent, in this or an earlier publication) is marked `ViewFlags::RELEASED_LOCAL` in a copy of the
   view, so the display hands it off instead of correcting it (render.md 8, D4). The mark stays until a final frame
   passes the release's stamp, or 2 s, so a view the display loses in a gap doesn't lose it (@design-reviewer's L1 on
   bb23fc58); an object held by this member again goes unmarked. Only pe-app knows which entries are its `Release`s,
   and the view's records don't name the object.
7. **Wait** until `clock.host_time_of(p + 1)`, or until the inbox or the edit queue wakes the thread, whichever is
   first. Before frame 0 the wait is at least one frame period, since `host_time_of` means nothing while the clock
   converges; the network thread wakes the simulation thread when it converges.

**Bounded work per tick.** A deep rollback, or a repair's replay of up to `W_fg` (2 s, 120-240 frames), done in one tick
would stall the simulation for hundreds of milliseconds, and with it agreement's votes and messages for every other
member. So the replica re-simulates at most a budget of frames per call, expressed as simulated time (a local setting,
deterministic for the harness; pe-app may adjust it from the measured step cost), and resumes on the next call. While it
is behind, its frame view is the newest frame it completed (S2 v1.10; F3's S5, "the fastest machine sets the pace"), one
view per tick, and a re-simulation publishes none until it is back at that frame. Between views the display keeps
drawing, and extrapolating within its cap, the last one, while agreement keeps ticking and voting. The budget is never
below two frames of simulated time at the world's rate, because real time adds a frame per tick and a budget of one
frame never catches up (S2's floor; @design-reviewer's Low on 549f4ca4). Nor is it below what reaching the present asks
of the call, as the last call measured it (the keep-up floor; @coordinator-opus, which replaced's advance plus one): the frames the frame clock's present advanced since the previous tick plus what the
last call left short, plus one, and at least the whole of what the last call needed, rollback included (S2's `frame()`
is the re-simulation frontier, so what a call leaves short counts what is left of a re-simulation).
- **The advance** covers ticks dearer than a frame period. A tick also takes messages in, signs the hand inputs and
  runs agreement, and when that alone outlasts a frame period, ticks run back to back and real time gains several
  frames on each: sized from stepping alone, the budget then let a replica fall further behind every tick though its
  frames were five times cheaper than real time (a debug build under load: 120 ms a tick besides 2.8 ms a frame).
- **The last call's need** covers late entries. A member whose entries arrive `L` frames late on every frame (its
  hands move, its link is slow) rolls the replica back about `L` frames on every tick. With only the advance plus
  one, a budget under `L + 1` never reached the present again: a 300 ms link froze a receiver's view for a whole
  30 s run, in the 200-ball pit at about 1 ms a frame, where the share fits 8 (@engine-architect's measurement,
  <notes>).

So a replica whose frames are cheaper than a period gains at least a frame on real time every tick, whatever the
rest costs, and one re-stepping late entries reaches the present on every tick that the work fits. The keep-up floor
is capped at 100 ms of stepping at the measured cost of a frame, and at `Budget::MAX` (4 s), so a machine that steps
slower than real time, one back from a long stall, or one re-stepping more than that every tick, holds and reports
it rather than making each tick longer than the last, and a 10 s stall never becomes one 600-frame tick that sends
no votes or timeouts. A tick that re-steps a lot runs long on the simulation thread, never the render thread, which
keeps drawing the last view. Above both floors pe-app raises
the budget from the measured step cost. Each hold is counted, with its length; the objects that jump when the view resumes are blended and, above
`E_cue`, cued like any correction (render.md 8, D2-D3). A replay or an import that does not fit in one
call returns "pending" and is called again on a later tick. The budget is part of S2 (`Advance::caught_up`,
`RootError::Pending`, `ImportError::Pending`). M2's second copy then keeps the old prediction moving during a deep
replay instead of holding the view.

The harness's node driver runs steps 1 to 5 with its own network and clock, through the same `drive`, so the loop
that ships is the loop the properties check.

### 3.1 pe-api's ports

pe-api reaches the world through ports that pe-app implements (api.md 3, 9). The simulation thread feeds them, and
the MCP thread's handlers read them (@coordinator-opus).
- **The snapshot port** (pe-api's `WorldSource`, pe-app's `ApiWorld`):
  - `latest()` and `at(f)`: each tick that publishes a new view also takes `Replica::world_snapshot()` (S2 v1.7;
    pe-world's `Arc<WorldSnapshot>`, downcast) and keeps it by frame, for the last 2 s (api.md 9.1's retention).
    A snapshot at frame f is the state before f is stepped. The kept snapshots above each advance's
    `rolled_back_to`, the frame of the state the replica restored, are dropped, and all of them on a repair, so none
    belongs to a timeline the replica replaced (D2; @api-eng's requirement on v1.8; @design-reviewer's Low 1).
    `latest()` is the newest; `at(f)` the one kept at `f`, if any.
  - `final_()`: `Replica::agreed_snapshot()` (S2 v1.7), the world at Cv, the newest final cut this replica matched:
    exactly that cut's entries, never a prediction, and left alone by rollbacks, replays and imports. So it never
    lags the final frame and survives a stall or a repair. Before the first agreed cut it is the frame-0 snapshot
    taken at startup, final by genesis (@coordinator-opus, replacing D2 for `final_()`; @api-eng's M2).
    In the M1 interim B builds frame 0 from the genesis record, so that holds on both machines; a joiner that
    imports at a cut (after model v1) answers with the imported state until its first agreed finalize. pe-api's
    statuses wait for Cv as well (api.md 9.1, 5.3.2), so a change it reports final is in `final_()`'s state, though
    agreement's final cut, which `confirm_cut` follows, may be ahead of Cv for a moment.
- **The change record:** with each new view, `ChangeLog::publish(changes, final_frame, health)` from the view. When
  agreement's final cut moves (S3's `EntrySource::final_cut`, never the `CutFinal` event, whose voids may come a
  tick or more later), one `ChangeLog::confirm_cut(cut, voided)`, with the entries of every `Voided` event drained
  since the last one, so an entry a cut includes and voids is never shown final. It relies on S3's ordering: a cut's
  `Voided` events are queued no later than the call in which `final_cut` first reports the cut (pe-agree queues them
  and then moves `final_cut`, in one call [V node.rs]; @design-reviewer's Medium on draft 5).
- **The edit port:** step 2 takes at most one `Submission` a frame from pe-api's `EditQueue`, makes an `Edit` entry
  on the member's chain from its transaction (pe-world's codec) and submits it, first in the frame, before the hand
  entries, so that a dry run made while the queue was empty names the seq the edit then gets (@api-eng's L1). It
  settles the ticket: `Receipt::Submitted { change, frame, chain, seq }`, or `Receipt::Refused` with agreement's
  reason. Edits are never paced. The queue's wake wakes the simulation thread (step 7).
- **What else `ApiWorld` answers:**
  - `draft_for`: the member's chain id and next seq, as step 2 last left them.
  - `edit(entry)`: an index of the edit entries the views' change records name, each decoded once and kept as long
    as pe-api's `ChangeLog` holds the entry, oldest dropped first, up to 10,000 entries (`changes::CAPACITY`) and 64
    MiB of payloads [I]. `history`, `revert` and `watch` read it; past either cap, history describes an older change
    without its contents (@api-eng's M1, @design-reviewer's Low 2).
  - `put_blobs`: into the machine's blob store, which every world and snapshot on the machine shares.
  - `member_name`: a short form of the key, until members have names.
  - `head_of`: none in M1, which has no head object, so `"me"` is refused by name. That also leaves the rule that
    agents don't move their user (api.md 10.6) with nothing to protect: re-check it when heads arrive (@api-eng's
    L3).
  - `session()`: the line; this machine's member and those whose links step 1 reports up, each once (@api-eng's
    L2); the link; the last advance's `caught_up`; the world's rate; and `agents_move_user` (off).
- **The MCP thread:** pe-mcp's `host::start` over `Api::new(ApiWorld, queue, log)` as an `Arc<dyn WorldApi>`, on a
  tokio runtime of 2 workers, with the pipe or loopback HTTP transport or both, and optionally the agent's command
  line (`--mcp-pipe`, `--mcp-http`, `--agent`). The token store is the one `host::start` makes; its tokens are
  revoked when the agent exits or pe-app stops. The world's history names the agent (its producer's harness) by
  `--agent-name`, given before `--agent`, or else by the command's first word, so an agent started through
  `cmd /c start` needs the name.
- **Not yet:** an edit's blobs reaching another machine's store. pe-world stores none from an applied transaction,
  and the Bulk lane isn't wired.

### 3.2 Stamp-ahead: a slow member's entries

A member whose entries reach the others later than the target stamps its own entries ahead of its present. They
then arrive above the cuts' frames, are never void as late, and cost the receivers little re-stepping (agreement.md
3; @coordinator-opus, after @fable-reviewer;
@item:primordial-engine/pe-app-stamps-a-slow-member-s-entries-ahead-by-i).
- **The stamp.** Step 2 stamps the frame's entries (the hands, the edit and the build worker's machine entries) at
  `max(p + lead, the chain's last stamp)`. So a shrinking lead never steps a stamp down, which pe-agree would refuse
  (`SubmitError::BelowOwnStamp`). An edit's code commits carry that stamp as their `activate_at` (step 2), or
  pe-world would refuse them. A hand's pose is the one sampled at `p`'s host time (up to a frame earlier while the
  lead sheds: the pose clock, below), so only this member's influence lags by the lead. pe-xr draws the member's own hands at the tracked pose, never the simulated one.
- **One signal, two controllers.** Each other member reports, in its votes and timeouts, the p95 lateness of this
  member's entries at their first arrival, relayed or direct (S3 v1.8 `own_lateness`, @world-chunks):
  - signed, negative when they arrive before their stamps, so a lead that is too large shows;
  - with the seq of the entry at the percentile, so the correction is exact: `delay = report + lead at seq`
    (`StampLead::lead_at`).

  The receivers' adaptive cut lag absorbs the delay first, up to the target, and the lead takes only the excess:
  `lead = delay - target + margin`, at least 0. On recovery the lead drains first, so the receivers' lag stays near
  the target until the lead is gone.
- **The target** is a setting, at most the lag's cap, and 0.1 s by default. Measured [V], in the
  200-ball pit on a 1.2 s link (<notes>):
  - with the target at the cap, a receiver re-steps about 57 frames for every entry against a budget of about 10,
    and its view was published on 583 of 1,800 ticks;
  - with the target at the floor, it re-steps nothing and its view equals the slow member's.

  At 0.1 s the receivers see a slow member's entries about 3 frames late, the cost of an ordinary link. A slower
  member pays its excess as its own input lag, as the user's thin-link rulings put it. LAN and Tailscale links, as in M1's first two-machine run, get no lead.
- **The largest report, clamped.** The lead follows the largest corrected report over the members, so the slowest
  receiver decides. It is clamped at `max_lead`, 3 s by default (a local setting), so a faulty member's inflated
  report can hold this member's input back by at most that. Past the clamp, this member's hand entries void as late
  at the members it is slowest to reach, and its edits go to pe-api's re-send backstop and its held batch (api.md
  9.4): the stated limit.
- **Lead changes** (@coordinator-opus, after @fable-reviewer, on @design-reviewer's
  Medium on a6b1035a):
  - The lead follows the largest lead the reports called for over about the last 3 s, as a jitter buffer follows the
    worst recent delay, so one spike doesn't cause cycles of rising and draining.
  - A rise takes effect at once.
  - A fall sheds one frame at a time, at one of two rates decided every frame:
    - while both hands are slow, one frame every `second / surplus` frames, clamped to 2..=30. A 3 s surplus drains
      in about 8 s, starting at half speed;
    - while either hand moves, at most one frame in 11, so the others see the hand at most 1.1 times its tracking
      speed and a strike lands at most about 10% harder. A 3 s lead then drains in about 34 s.

    There is no hold and no backstop. Under the old rate (one frame each half second) a 3 s drain took 90 s.
  - **The speed test** (`stamp::Motion`). A hand's speed is its fastest point's: the pose's linear speed plus its
    angular speed times the hand collider's radius (0.09 m), so a wrist flick counts. With hysteresis, a moving hand
    becomes slow below 0.25 m/s and a slow one moving above 0.35 m/s. A tracking jump counts as moving: the first pose
    after the hand was away, or one whose `discontinuity` changed. A hand the feed doesn't track sends no pose, so a
    shed can't speed it up: it counts as slow, as does a hand never seen (a desktop feed with one hand). pe-replica
    coasts a lost hand in world frames, which the stamps don't touch.
  - While it sheds, a frame's stamp can stand still. Its poses would only replace the last frame's at that stamp, so
    step 2 doesn't send them (`SimStats::poses_thinned`); a grasp that changed (a grab or a release) and an edit
    still go out, at the stamp.
  - **The pose clock** (`StampLead::pose_time`). Sampled at the present, the stamp after a shed would carry a pose two
    tracking frames on: one stamp in 11 at twice the speed. So each stamp advances the moment the poses are sampled at
    by at most `every / (every - 1)` frames (1.1 while a hand moves, 2 otherwise), never past the present. That
    spreads the frame a shed takes out over the stamps up to the next shed, and the poses trail the tracking by at
    most a frame while the lead drains. A frame whose stamp stood still keeps the last frame's moment, so a grasp it
    sends goes at the last pose.
  - The others see this member's hand move faster while the lead drains, never pause: at most 1.1 times its tracking
    speed while a hand moves, up to twice while both are slow (@fable-reviewer's reading, measured in section 7).
  - `SimStats::largest_lead`, `longest_drain` and `moving_frames` record the run's largest lead, its longest drain
    and the frames the speed test found a hand moving.
- **Entries in a receiver's future.** On a link fast one way and slow the other, or at a third member it reaches
  quickly, an entry stamped ahead arrives before its stamp. Receivers hold it without refusing it. pe-agree bounds
  a cut's frame by the voters' present plus the tolerance (`Reject::FrameInFuture`), never an entry's stamp [V,
  cuts.rs]. What bounds such entries is the per-author pending cap: at most `pending_s` (4 s) of the author's rate
  budget held beyond the final cut. That's 1,020 entries at 60 Hz, 5.7 s of two hands and an edit every frame. An
  entry stamped ahead is pending for about the lead, plus the target, plus a vote's latency, so a `max_lead` of 3 s
  fits with room for a vote of up to a second or so.
- **Attestations** go out at the stamp too, so a slow member's attestation counts later by up to its lead. `F`
  moves later by at most the lead, and the deadline `D` is at least 44 s after `c` (twice `T_module`, logic.md
  6.2a), far beyond `max_lead`.
- **Receivers re-step up to the target's worth every tick,** so they rely on the keep-up floor's last-call need
  (section 3) to keep their views at the present.

### 3.3 The shown present (B): the simulation at the moment the headset shows

The user's rulings (2026-11-19: "B" "no, just do B. thats how every other game manages
it, that asymmetry is fine" for the delay, one per session;
@item:primordial-engine/vr-run-the-local-simulation-at-the-moment-the-he). They supersede render.md 7's 2026-11-17
ruling for drawing the world.
- **Why.** Under Present the world was drawn at the engine's present and the head, hands and held objects at photon
  time, 46 ms ahead. A ball let go jumped back by that lead × its speed (render.md 8, D4).
- **The display lead** (`crate::shown::DisplayLead`): the VR sink records each drawn frame's lead under Shown (the
  runtime's predicted display time minus now). The simulation thread takes its 95th percentile over 2 s, so a few
  late frames don't move the present. With no frame for 2 s (no XR session, the headset off) it has none.
- **The shown present** `p_s = clock.frame_at(now + ahead)`, with `ahead` = the lead less the session's delay (never
  below 0) + one simulation period + 5 ms, the publication's margin (`shown::ahead_ns`); `p` itself with no lead. A
  view's time is its frame's end, so until the next tick's view is out, every display frame's moment lies before the
  newest view's, and VR interpolates instead of guessing (render.md 7, Shown).
- **What moves to `p_s`:** step 2 (the hands at the end of `p_s`, stamps from `p_s` through stamp-ahead, which
  still stacks a slow link's lead on top and never steps a stamp down), and step 4 (`drive` to `p_s`). The grasp is
  never forecast: pe-xr's ring gives the pose at the predicted display time and the grip read now (xr.md 6.2), so a
  grab or a release goes in at `p_s`, the shown lead after the real closing or opening.
- **What stays on the present `p`:** agreement (`tick(now)` sets pe-agree's present, so votes, cuts, deadlines and
  lateness reports, `late = present − stamp`, are the group's [V, pe-agree node.rs:1213, 2688-2691]: a member
  running ahead inflates nobody's stamp-ahead), the links, pe-diverge, the join and the edit queue's once-a-frame.
- **The delay** is the session's, one value, in the genesis record (`display_delay_ms`, format 2, at most 1 s). The
  opener chooses it with `--display-delay <ms>`, 0 by default; a joiner runs the record's. A world alone takes the
  option. The VR presenter draws the world that long before the moment shown (`DrawnTime::Shown`).
- **Sounds and haptics** wait until the drawn time reaches their frame (render.md 8, D6).
- **The end line:** `SimStats::shown_ahead`, `largest_shown_ahead` (frames) and `ticks_shown_ahead`.
- **Measured** [V, fake clock, pe-app's tests]:
  - Solo, a perfect forecast and a 46 ms lead at 144 Hz (`solo_play_on_the_shown_present_draws_every_frame_between_
    simulated_ones`): no correction, no extrapolation miss, no cue, no frame past the newest view, and the ball
    leaves the drawn hand 2.6 mm from it.
  - The cost of never forecasting a grasp: the release goes in about 90 ms after the real opening, so a hand already
    slowing carries the ball on. In that test (4 m/s, opened at the peak, slowing at 25 m/s²) it left at 2.61 m/s.
  - Mixed leads on a 20 ms one-way link (`mixed_display_leads_agree_on_the_race_and_the_world`): with one member
    ahead and one at the present, the one at the present wins a grab both close on at the same moment, and the one
    ahead rolls back 9 frames (re-simulating 1,720) while the other rolls back none; with both ahead, or neither,
    each rolls back 5 (760). Both machines agree every time.

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
4. **The network thread.** Open or dial the invite; on admission the welcome carries the session's epoch and rate,
   and a joiner's clock converges before its replica starts (transport.md 5.4).
5. **The replica:** a new world from the origin or a preset, an opened save (M5), or for a joiner the whole state at
   the cut E that admitted it: fetched from the inviter as pe-diverge's rung 2 fetches it from empty, on the Repair
   lane, imported (`import_at_cut`) into an empty replica, then simulated on to the present ("Joining from a cut",
   below).
6. **The simulation thread**, then the XR thread (VR) or the main thread's drawing (desktop), then the MCP transports.
   **Headless** (`--headless`, for tests and machines without a display; @coordinator-opus) makes no
   GPU context, window or XR thread: the main thread waits in 100 ms slices until `--seconds` passes or the console
   asks to quit, then runs the quit itself, as in VR. No hands move, since the desktop hand has no window to feed it.
   Nothing draws, so there is no capture queue: view_capture answers `capture_unavailable` at once (api.md 8).
   `--vr` and `--headless` exclude each other.

**Joining from a cut** (draft 7; it replaces the M1 interim's two-member genesis once
@item:primordial-engine/pe-agree-join-admit-by-a-pure-governance-fold-pe lands; agreement.md 8, "In pe-agree,
`Admit`"). A opens the world alone and plays; B joins later at a reconfiguring cut E, as every later joiner will. At
n = 2 nothing becomes final in the new epoch until B votes, so a B that never starts stops A's finality for good. A
therefore writes the `Admit` only once B has shown it can start (@design-reviewer).
- **A** (`--open`) builds the genesis record for itself alone: the build, its key with its role
  (`ADMIN | EDIT | AGENT`), the world's rate, cut interval and gravity, the preset and its arguments, a fresh nonce
  from the OS, the epoch (frame 0 at A's start) and the frame-0 roots. The session's line is derived from the record.
  pe-agree's `genesis_gov` is `GovState::genesis` over every record member with its role, one table like the world's
  (@world-chunks 3c44d34d). The world's root covers the same bits, so it needs no field of its own and no place in
  the consensus hash. A prints an invite: its key and addresses, the invite's id and expiry (pe-net-crypto's
  `Invite`), and the epoch it is made in with a hash of that epoch's sorted members, which B checks E's certificate
  against (agreement.md 8). A reads B's reply code on standard input, as before.
- **B** (`--join <invite>`) prints its reply code for A and dials. Its hello carries the build id, which now also
  names pe-agree's consensus hash (net.rs `build_id`; the crate's version alone doesn't change between commits). So
  equal build ids mean equal consensus settings, and a member with other settings is closed at the hello
  (`CloseCode::Build`), before any `Admit` exists.
- **The build id names the sources** (@coordinator-opus, after @fable-reviewer's view; @design-reviewer found that nothing named the rules pe-world, pe-replica and pe-physics compute by):
  - pe-app's build script (build.rs, `crate::sources`) hashes every file under `crates/`, with `Cargo.lock` and the
    root `Cargo.toml`. The hash's first 6 bytes are the id's first part: `pe-app src <hex>; scripts <hex>; core <hex>`.
  - It needs no git, is right for a dirty tree, ignores commits that change only documents, and is the same for
    debug and release builds and on every platform (`.gitattributes` checks every text file out with LF).
  - It says "same sources" only: it is not a security check, and not the compiler's or the CPU's identity.
  - pe-app's version is no longer a part, since the hashed Cargo.toml covers it, and three named parts then fit the
    hello's 64 bytes (transport.md 6.3). The console's first line prints the version, the source part, and the commit
    with whether the sources differed from it, as git told the build script (ignored files under `crates/` count,
    since the hash reads them): `pe-app 0.1.0, sources <hex> (commit <hex>, with local changes)`. A refusal at the
    hello prints both ids, so the differing part shows by its name.
  - A record that outlives the session must not keep this exact-source id: a release-compatibility rule replaces it
    before worlds are saved (M3; @item:primordial-engine/release-compatibility-replaces-the-exact-source-b).
- **A welcomes B before it admits it.** On B's hello A checks the build, the self-test, the invite's id and expiry,
  that B isn't a member, that no other join is in flight, and the world (next item). It then welcomes B at the
  transport (transport.md 6.4): the session's epoch and rate, the current epoch's members with their addresses, and
  in `agree` the genesis record. The welcome's line stays the fixed pre-genesis line both hellos carry: pe-net refuses
  a welcome naming another line than the hellos' (core.rs), and B's hello can't know the session's line, which B
  derives from the record. B isn't a member of agreement yet. Anyone else's join request is refused ("not
  admitted"), as is a second join while one is in flight. B checks A's hello the same way, with pe-net's guards
  (the protocol, the build, the self-test and the line), so either side's refusal can come first. Either way, B's
  join ends at once and names the mismatch, never at its timeout (K3,
  @item:primordial-engine/k3-a-joiner-names-its-own-self-test-refusal-inst).
- **B shows it can start, then A admits it.** B checks the welcome: the record's build and consensus hash against its
  own, the welcome's rate against the record's and its own, and that the welcome's members, sorted, hash to the
  invite's pin and include the invite's key. It derives the session's line from the record, never from the welcome
  (transport.md 6.4). It then connects to the members and converges its clock on them (transport.md 5.4: the
  welcome, the mesh, the samples, clock-ready), and tells A it is ready (`JoinReady`, a pe-app message on the
  Protocol lane). pe-net starts converging as soon as the welcome arrives, before these checks [V, @shader-lang], so a welcome that fails them ends the join even if the clock is ready by then, and B never sends
  `JoinReady`. Only then does A write `Admit { key: B, role }`, with the role `ADMIN | EDIT | AGENT` [pending:
  @coordinator-opus's ruling on the joiner's role, after @world-model]. A adds B to its own clock members
  (`set_clock_members`) only then, never at the welcome, so transport.md's rule that clock members are ready clocks
  (its draft 1.11) still holds now that the welcome lists every member: B's clock is ready by its `JoinReady`, and
  the members B converges on are running (@design-reviewer's Low 3). If the join fails, A sets them back. A B that
  isn't clock-ready within `T_ready` is a `ClockFault` (transport.md 5.6): A closes its link, and no `Admit` was
  written.
- **A world with scripts: an M1 limit** (@coordinator-opus, option (a) of). At B's
  hello, and again in the tick that writes the `Admit`, A refuses the join while its world holds any script version
  or pending commit, and says so to both players ("this world has scripts; joining it comes after M1"). A joiner's
  import carries no script archive [V, pe-world tree.rs `export_blobs`: inline-class blobs of objects and tombstones
  only], and a joiner that builds every active version first isn't built (logic.md 6.2b), so B would step without
  the scripts and split from A at each cut where they act [I]. Both checks are built [V: crate::join's
  `a_world_with_scripts_refuses_the_join_at_the_hello_and_at_the_admit`, a red for each]; the refusal's detail
  reaches B, and A's console says "<player> couldn't join: this world has scripts; joining it comes after M1."
  From B's ready (the `Admit` about to be written) until the new epoch's first final cut, or the join's end if it
  fails after its `Admit`, A's pe-api answers a live script commit `unavailable` ("a player is joining") before it
  builds or submits anything; `code_check` and dry runs still answer (api.md 7) [V: pe-api's
  `a_script_commit_waits_out_a_join`, crate::join's `Inviter::holds_script_commits`]. pe-api's flag is set in step 2,
  after step 1 may have made B ready, and step 2's drain holds back a script commit pe-api checked before that, so
  none is stamped with or after the `Admit` [V: `a_script_commit_drained_while_a_join_holds_commits_is_held_back`].
  The risk is a commit stamped between the `Admit` and E, which E's state would hold pending without its archive;
  one stamped after E reaches B as an ordinary entry. A commit made before B's ready goes ahead, and the `Admit`'s
  check then refuses the join: it counts A's own script commit stamped ahead and not yet stepped [V:
  `the_admit_s_check_counts_a_script_commit_stamped_ahead_and_not_yet_stepped`; @engine-architect's M1 on 1.6]. At
  n = 1 → 2 only A can commit; another member's commit in flight, at n > 2, is not counted. Carrying the archives
  and building them before stepping past E come after M1
  (@item:primordial-engine/build-asymmetry-after-m1-a-member-that-can-t-bui,
  @item:primordial-engine/blob-transport-the-bytes-of-names-and-meshes-ano).
- **The `Admit`'s world side** (S2): every replica applies an effective `Admit` at its entry's frame with
  `World::admit`: B's member row, its role and its hands, placed by @world-model's spawn rule [pending; provisional:
  the preset's next place in its row]. So the world at E, whose root E's certificate covers, holds B. It is
  pe-replica's arm beside the `Leave`'s, which also refuses a malformed `Admit` and one of a member, as S3 voids them
  (@coordinator-opus; app/join ae54e76a, @vdw-physics reviews) [V: pe-replica's
  `an_admit_admits_its_key_with_its_role_and_two_hands`, red without the arm].
- **While B joins,** A's console says "<player> is joining…" from the `Admit` until the new epoch's first final cut.
  That cut needs B's vote, so it waits for the whole join: E final at A and `JoinStart`, B's fetch and import of E's
  state, its simulation from E to the present, and its first vote. A plays on its predictions meanwhile; its own
  entries wait for B, and its hand entries pause once pe-agree's backlog holds them (section 3, step 2), as with a
  stalled link [I]. The join fails at A (@design-reviewer's Low 1):
  - at once when B's link closes before that cut: B gave up and quit, or crashed and its link idled out;
  - or when the join takes no step for `T_join` (30 s, a stated setting). Its steps are the `Admit`, `JoinStart`
    sent, `JoinImported` (a pe-app message on the Protocol lane that B sends when pe-diverge reports `Joined`) and
    that first final cut. Each starts the time again, so the fetch and the catch-up each get a whole `T_join`.

  A's console then says "<player> couldn't join. This session can't confirm changes any more: start a new one." If
  the cut becomes final after all, it says "<player> joined after all; changes are confirmed again." [V: pe-app's
  `a_join_held_past_t_join_fails_and_joins_after_all_once_the_cut_is_final`.] A join that fails before its `Admit`
  (B's link closes after the welcome, a step takes `T_join`, or the checks above) costs nothing: A closes B's link,
  sets its clock members back, says why ("<player> couldn't join: <why>."), and may take another reply. Nothing else
  undoes a failed join in M1: removing a member that never started comes with `Remove`. The fetch's time is set by
  the link, not by pe-diverge's serving budget [V, ladder.rs defaults; divergence.md 4.2, step 2]: a source serves
  each peer from an 8 MiB burst refilled at 4 MiB/s, and the requester paces to half that burst at a quarter less
  rate, so M1's 1.2 MB is never held back. Repair messages go out only while pe-net's delay guard holds
  (transport.md 4.3; lanes.rs `poll_stream`), so the fetch takes about 1 s at 10 Mbit/s and 10 s at 1 Mbit/s [I,
  from replica.md 6.2's size]. B's catch-up time isn't measured.
- **When E is final at A, and A's own state is verified there** (pe-diverge's `Cv` is E), A sends B pe-agree's
  start record. A is B's source for E's state, and pe-diverge serves only a root it verified, answering `NotHeld`
  before then, which fails a join whose one source A is. A replica still catching up after the start (500 balls
  built, then stepped) verifies E a little after agreement makes it final [V: the first two-process demo, E at
  frame 18; crate::join's `the_start_waits_until_the_inviter_s_state_is_verified_at_e`]. The record
  (`Node::epoch_start`: E's certificate, the new epoch's
  members and governance state, E's head entries and each chain's highest final stamp) as `JoinStart`, a pe-app
  message on the Protocol lane, whose messages are at most 1 MiB [V, pe-net lanes.rs `l_msg`]. [I: E's heads are one
  entry per chain, and a script commit's archive rides in its entry, up to 256 KiB; the world check keeps commits
  out of a join.] If `Node::epoch_start()` is `None` there (A no longer held E's certified chain when E became
  final; unlikely at n = 2, @design-reviewer's Low 2), A can't start B: it closes B's link and shows the failed-join
  line at once. B gives up when no `JoinStart` comes `T_join` after its `JoinReady`, and says it couldn't join.
- **B starts agreement at E:** `Node::join(config, start, pinned epoch, pinned members, …)` checks E's certificate
  against the pinned epoch's members under its signing domain, and starts B's agreement in the new epoch with E as
  its genesis (agreement.md 8). E is final already, so B votes from the first cut after it. E's frame must be at
  most 20 s past and 10 s ahead of B's present by its converged clock: the interim's window, now on E, since frame 0
  may be hours old. If the start's members differ from the welcome's, B gives its clock the start's
  (`set_clock_members`).
- **B's state at E.** B builds its replica empty (`WorldModel::empty` at the record's rate; replica.md 6.2,
  "Joining") and doesn't step it. pe-diverge fetches E's whole state from A on the Repair lane, keyed by E and
  `Node::start_root`, as rung 2 does when it holds no root of its own at a final cut: every leaf, the physics image
  and the inline blobs (pe-diverge ladder.rs, the `Damaged` path). `import_at_cut` checks every record against that
  root before applying any, so B's world at E is the one E's certificate covers [V for a mock world: pe-replica's
  `a_joiner_imports_the_whole_state_from_empty`]. B then refuses, by name, an imported world whose members aren't the
  start's [V: pe-app's `an_imported_world_whose_members_are_not_agreement_s_is_refused`, of the check], simulates on
  to the present with the new epoch's entries, and votes [V: pe-app's
  `a_joiner_joins_at_the_cut_that_admits_it_and_both_finalize`, 8 and 500 balls, over real UDP on loopback]. M1's 500 balls are about 1.2 MB,
  mostly the physics image [I, replica.md 6.2]. pe-diverge starts this fetch with `Diverge::join` (@diverge-eng; main c0fa79c9): no divergence bookkeeping, the inviter first and then E's
  voters as sources, and `Joined` or `JoinFailed` among its events. On `Joined` pe-app starts the driver and tells A
  (`JoinImported`). Until then B's replica publishes no view, so S5's health is unchanged: B's console says
  "joining…", and pe-api answers not-ready until the replica reaches the present. On `JoinFailed` B says it couldn't
  join and quits, and A sees its link close.
- **A keeps E's state until the join ends** (@design-reviewer's Low 2). At n = 2 nothing in the new epoch becomes
  final without B, so E stays A's newest final cut. pe-replica keeps the floor's snapshot, from which it serves E's
  tree and physics image by root (replica.md, "Always kept"), and pe-diverge keeps E's root servable, as it keeps
  the newest two (`note_servable`) [I, read, not run]. At n >= 3 the others finalize past E, and a source drops a
  root `T_keep` (1.5 s) older than its newest, so E can stop being servable while B fetches it: pe-diverge then
  reports `JoinFailed` (overtaken). Keeping E at the sources until the join ends, or joining at a newer cut, comes
  with joins at n >= 3, after M1.
- **The paste.** When the user pastes B's reply code by hand, B's first hello may come from its early dial. While
  the invite is open and no join is in flight, A holds a hello from a key that isn't a member undecided, since it
  may be the next reply's, and welcomes it when that reply comes. If the paste takes longer than `T_welcome` (10 s),
  A closes that dial with close code 8, B waits through it, and A's own dial carries the join. A welcomes B on
  whichever dial completes, and writes the `Admit` only after its `JoinReady`. A reply that answers another invite,
  names a member, or comes while a join is in flight is refused on A's console.
- **A quit during a join** (@design-reviewer's Low 1): A never writes its `Leave` while a join it admitted is in
  flight. A quit then waits, within the leave's bound (shutdown, step 2), for the new epoch's first final cut, and
  then leaves as usual. If B quits or fails before it starts, A is stranded as above.
- **Faults during a join.** At n >= 3, a member that hasn't switched yet faults B's first messages (`NotMember`,
  agreement.md 8's limits); at n = 2 it can't happen. pe-app only counts and logs faults: nothing drops a link over
  them.
- **The consensus settings and the voters** (the gate's item 1, main bf521179): the record's `agree` is this build's
  `NodeConfig::consensus_hash` (@world-chunks), the hash of the settings every member must share. B compares it at
  the welcome, before it says it's ready, and `Genesis::start` compares the configuration A runs, so a member with
  other settings is refused by name (`GenesisError::Agree`) instead of refusing the other's entries, votes or cuts
  later; the build id in every hello closes most such cases earlier still. A world's
  governance members must be pe-agree's voters: `start` refuses a genesis world whose members aren't the record's
  (`GenesisError::Governance`), and B an imported world whose members aren't the start's, since logic.md 6.2a's
  voter snapshot relies on every member voting in M1 (@design-reviewer).
- **M1's limits:** one join at a time, and none into a world with scripts (above). M1 is two machines, so B joins A
  alone; joins at n >= 3, where E can be overtaken, are untested and come after M1. The invite pins the epoch it was
  made in, so after any other membership change a joiner needs a new invite (agreement.md 8's one-epoch limit), and
  M1 makes none but the opener's: once B has joined, A refuses a third player's hello at once and its reply code,
  saying the players changed since the invite was made. Joins at n >= 3 also need every member to take the joiner's
  dial (transport.md 6.7): today a member other than the inviter admits only current members and refuses it
  (`not_admitted`), so the joiner's clock couldn't converge (@shader-lang).
  Names and meshes another member made before the join reach B as stand-ins
  (@item:primordial-engine/blob-transport-the-bytes-of-names-and-meshes-ano).

**Shutdown**, in reverse (draft 6: the graceful quit,
@item:primordial-engine/pe-app-m1-leave-a-graceful-quit-submits-leave-ke; rulings @coordinator-opus,
with @fable-reviewer's refinements, and):
1. Stop MCP and the build service's attester, so no new edit or attestation is queued.
2. **In a session with others** (`--open` or `--join`), leave gracefully (`SimThread::leave_and_stop`, `crate::leave`).
   A world alone just stops (step 4).
   - The simulation thread's next tick submits this member's `Leave` (a class Protocol entry whose payload is
     `pe_seams::LEAVE`) first in its frame, at the frame's stamp: the present plus stamp-ahead's lead, never below
     the chain's last stamp (3.2), since a lead can hold that stamp above the present. It is the chain's last
     entry: step 2 makes no hand entry or edit after it, and an edit still in pe-api's queue is refused
     `SubmitError::AfterLeave`, as agreement would refuse it.
   - The simulation and network threads keep ticking, so the leaver still votes E's suffix: at n = 2 the other member
     can't finish E without it (agreement.md 8; @design-reviewer's (b)).
   - The console says "leaving the session…". The wait ends when every remaining member has told this one that E is
     final there (S3's `leave_settled`), or at once when no other member's link is up. Waiting only for a final cut
     that includes the `Leave` isn't enough: whoever forms E's certificate has E final first, and a leaver that quit
     then stranded the other in 3 of 9 pe-agree runs.
   - **The bound adapts to the link:** eight smoothed view times, clamped to 5-15 s, overridable locally
     (`Sim::with_leave_wait`). pe-agree's own smoothed view time isn't in S3, so pe-app measures the same thing: the
     smoothed interval between S3's proposals (`AgreeEvent::Proposed`), weighted 1/8 as pe-agree weights its own. The
     bound is taken when the quit starts, so slower views during the wait don't stretch it. pe-agree's leavers
     quit 355-558 ms after their `Leave` on simulated links; a 1.2 s link's views give the 15 s cap [I].
   - A second quit request ends the wait at once: Ctrl-C on the console ends the process wherever the quit is.
   - **The console's own quit** (`crate::console`): the first Ctrl+C or Ctrl+Break, a typed `quit`, or closing the
     console window (SIGINT, SIGTERM or SIGHUP on unix), runs this same shutdown, once, as the end of a run does, and
     the console says "pe-app: quitting (…)…". Before there is a
     shutdown to run (setting up, or the opener waiting for the reply code), it ends the process at once. In the
     window, the request runs it (a close event on a thread of its own), since the window can't be closed from
     outside; in VR and headless, the main thread waits in 100 ms slices and runs it. Windows ends a process about 5 s after a
     close event, and the bound above can be longer, so in either mode a close waits 4.5 s and then logs "leave cut
     short". What that leaves is what the bound passing leaves: if the `Leave` went out, the others see "<player>
     left before the session could confirm it" or "left the session."; if it didn't, "<player>'s connection
     dropped".
   - A log-off or a shut-down doesn't reach pe-app: Windows sends those console events only to services and ends an
     interactive process first. pe-app simply ends, and the others see "<player>'s connection dropped", as after a
     crash. Catching them would take the window's WM_QUERYENDSESSION; that is post-M1.
   - On unix (tier 2), one thread takes the signals: a second signal waits until a desktop quit that SIGINT started
     has ended, and after SIGTERM or SIGHUP the "leave cut short" line can show although the quit goes on.
   - **The run's log** (`--log <file>`, `crate::log`; @coordinator-opus, after M1's dry run). Every
     line pe-app prints, standard output and standard error, pe-xr's and pe-mcp's lines and Box3D's warning
     included, also goes to a new UTF-8 file, each line prefixed with the seconds since the log opened. It opens
     before the session starts and never overwrites a file. Lines are written through, the file before the
     console, under one lock, so the file ends with the console's last line whichever way the process ends: a
     quit, a second request, a close, a panic (printed through the log in place of the standard hook) or
     `std::process::exit`. What native code writes straight to standard error (a GPU driver's or the OpenXR
     loader's messages), wasmtime's own messages before it aborts and the runtime's last words before an abort (a
     stack overflow) reach the console only. If a write to the file fails (a full disk, a drive gone), the console
     says once that the log stops there, and the log closes. With it the command line needs no pipe: Windows PowerShell 5.1 ends a program piped to `Tee-Object` at once
     on Ctrl+C, before its handler runs, so the leave and the end lines are lost [V, the dry run].
   - **The tick trace** (`PE_TICK_TRACE=<file>`, `crate::trace`; @coordinator-opus, after the M1
     recording's 2.13 s tick, which the run's totals couldn't place). Off unless the variable names a file, and then
     the simulation thread writes a new one: a `sec` line for each second of the frame clock (its ticks' phases,
     frames stepped and re-stepped, the worst tick, how far the replica and the final cut trail the present, the
     budget and the frame cost at its end, and in that second the hand entries agreement took (`inputs`) and refused
     (`refused`), the frames step 2 thinned while backlogged (`thinned`, F3 S1) and the ticks that published no view
     (`unpublished`)), and a `long`
     line for each tick over `PE_TICK_TRACE_MS` milliseconds
     (default 50), with every phase of it. Each line also gives the thread's CPU time beside its wall time, to tell a
     tick that worked from one that waited (descheduled, or blocked on a lock or the disk). Between ticks, `loop_ms`
     is the loop's own work after a tick (the console's lines, the quit's shared state), where a blocked console
     write (a selection in a QuickEdit console) shows, and `late_ms` how long past the end of its wait the next tick
     started, where a thread woken late shows (@design-reviewer's L1 on 108458b9); either one over the threshold
     also writes a `long` line. Off, a tick pays one branch. Windows counts thread time in clock ticks (about
     15.6 ms), so CPU time is coarse for short ticks.
   - **The cut log** (`PE_CUT_LOG=<file>`, `crate::cutlog`; the pull-ahead plan's 1.2, for the M1 acceptance "the
     demo's script, with both machines' roots logged at every cut"). Off unless the variable names a file, and then
     the simulation thread writes a new one, never overwriting. A `#` line gives the Unix time it opened, then each
     result of finalizing a final cut writes one line:
     `cut t=<s> line=<16 hex> height=<n> frame=<n> root=<64 hex> result=agreed|diverged|damaged [mine=<64 hex>]`.
     - `t` counts host seconds since the log opened, as `--log`'s lines do, so a line's Unix time is the `#` line's
       plus `t`. `line` is the cut's line id's first 8 bytes, and `root` is the root the cut is final with.
     - `result` is this machine's own: `agreed` when its root there is `root`, `diverged` with its own root as
       `mine`, or `damaged` when it holds none. A result of `NotReady` writes nothing, since the driver asks again;
       otherwise the driver finalizes each final cut once, so a cut has at most one line.
     - The log holds the cuts this machine's driver finalized, not every final cut (@design-reviewer's L1 on
       2218971f): each tick the driver finalizes only the newest final cut, so a cut that went final in the same tick
       as a newer one, or while an older one was still `NotReady`, gets no line. That happens most while a machine
       catches up, after a stall or a slow stretch, and two machines then skip different heights. A height missing
       from one log means it wasn't judged there, not a gap in agreement.
     - `diverged` and `damaged` need a cut final at a root this machine didn't vote (n >= 3), or its root there
       changing after its vote. At n = 2 every final cut needs both votes, so a split's cut goes final only after
       the repair has revoted the reference's root: it shows once, `agreed`, after a gap in `t`. The split itself is
       in `--log` (pe-diverge's `RootSplitStarted` and `RootSplitEnded`).
     - Two machines' logs match by `line`, `height` and `frame`, not by `t`: the self-test checker reads both, and
       at every cut both logged the roots must be equal and `agreed`; it counts the heights only one logged as
       skipped.
     - **Rollbacks** go in the same file (1.11; @coordinator-opus), so the self-test checker reads
       them beside the cuts: `rollback t=<s> to=<frame> depth=<n> present=<frame> changed=<frame>|- replay=<frame>|-
       at=[<author 8 hex>:<class>:<seq>,...] voided=[<author 8 hex>:<reason>,...]`.
       - `to` is the keyframe the replica restored, and `depth` how far it had stepped past it.
       - The two causes S2 v1.11 reports (@design-reviewer's M1 on 6408fdbe), each `-` when it took no part.
         `changed` is the frame whose entries agreement reported changed: a late entry, a void or a reorder. It is at
         or above `to`, since keyframes are a few frames apart (replica.md 4.1), except for a report below the floor.
         `replay` is the commit frame of this machine's own code commit, whose prediction ended (n >= 2).
       - `at` is the entries agreement now holds at `changed` (`*` marks this machine's own), and `voided` the voids
         of the same tick. A late entry at `changed`, a void, or a prediction's end says what the replica re-stepped
         for.
       - A member alone also says each rollback on the console, since nothing from another machine should roll it
         back.
       - **No line:** a finalize's rebase onto the floor (replica.md 6.1) re-steps the live history without a
         rollback. At n = 1 that happens at the last member's leave: agreement voids it (`LastMember`) after the live
         history stepped it [V, 1.11's probe]. The process exits about 20 ms later, so nobody sees it.
     - The log flushes at least once a second of host time while the simulation runs, even when no cut follows a
       line (as in a split), and at exit. Off, a tick pays one branch.
   - **The end line's lags** (G9): `simulation: SimStats {..}` ends with `deepest_rollback`, the most frames any
     published view rolled back, and `final_lag`, `FinalLag { ticks, mean, p99, max }`. That is how far the final
     cut's frame trails the present at the end of each tick once a cut is final (not while a joiner imports), in
     frames: about how long an action taken then takes to become final.
   - When the wait ends, the console says "left the session.", or, if the bound passed first, "Left before
     <player> confirmed it. <player> can keep playing, but their changes won't be shared from now on."
3. **The remaining member** keeps its link to the leaver after the epoch switch until the leaver disconnects or 30 s
   pass, since the leaver's `leave_settled` depends on its `Wire::EpochFinal`, sent each second while its old core is
   kept (@design-reviewer's (a)). Nothing in pe-app or pe-net drops a peer on an epoch change, and the confirmations
   keep the link from idling out [I]. When a member's link drops, its console says at once, in player words, what
   happened, with no wait for the idle close:
   - the member had already left (E final here): "<player> left the session.";
   - this machine holds the member's signed `Leave`, but E isn't final here: "<player> left before the session could
     confirm it. You can keep playing, but changes from now on won't be shared, and this world ends when you quit."
     Once every other member has left this way, its hands are no longer paced (section 3, step 2);
   - no `Leave` from it is in hand: "<player>'s connection dropped. They may come back; until then, changes wait to
     be confirmed."

   M1 keeps no world past any quit (saves are M5), so a stranded member loses nothing a normal quit doesn't; "delayed,
   never lost" is about edits within a live session. In M1 these lines go to the console only, in
   both modes. A window or headset overlay, and a "stranded" health state in S5 for pe-render to show, come later.
   The remaining member carrying on alone (at n = 2, a re-founding under a new line id) isn't M1: a candidate first
   M3 slice.
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

**Scripted hands** (scripted.rs; the pull-ahead plan's T1, @item:primordial-engine/self-test-tooling-scripted-hands-in-pe-app-t1-th)
let a self-test touch the balls with nobody at the machine, in a window or headless. `--hands <routine|file>` plays a
script from the start, and typing `hands <routine|file>` into the console plays one at that moment (`hands stop`
gives the hands back). While a script is set, its hands replace the desktop hand, through the same `HandFeed`; they
never replace VR's ring while an XR session runs. A script is one step per line for either hand (`rest`, `move`,
`reach top|first|pile`, `lift`, `swing`, `wait`, `close`, `open`, `drop`), with rig-frame points and seconds; a
`reach` picks its ball from the newest view when it starts and follows it. The routines: `idle`, `sweep` (through
the pile, side to side), `grab` (the top ball, lifted 0.6 m, held 2 s, let go), `throw` (the top ball, thrown
forward and up at about 4.5 m/s) and `share` (the lowest-id ball, so two members playing it at once take hold of the
same ball). Each step logs its start, and a `reach` logs the ball it chose. The script's hands go through step 2 like
any other: its edges send the grab and the release, and a fast opening a throw. [V, sim.rs's
`the_scripted_routines_grab_lift_and_throw_a_ball`: each routine closes, grabs and opens once; `throw` throws once.]

The demo is the plan's M1 sequence (docs/refound-plan.md section 1, M1), as one script per machine: A opens the ball
pit and prints an invite; B joins on the desktop; both see and touch the balls; an agent on A edits the lamp script
through MCP; B's lamp changes at the agreed frame. The script drives pe-app through its configuration and MCP only,
so the same script is the two-process acceptance test.

**The acceptance test** is `crates/pe-app/tests/demo.rs`: both machines run headless (section 4, step 6) on loopback,
and each agent is the test binary again, launched by `--agent`. A's agent commits pe-logic's lamp, binds it to the
preset's Lamp with the script's `Blink` and `Glow` (a period long enough that it stays on), waits for the warm glow,
then commits a version that glows blue. B's agent reads B's own Lamp through B's MCP and finds the first blue frame
among the kept ones: it must be `F + 2`, where `F` is the activation frame A's `mine` reports, and `F + 1` warm. The
new code runs from `F + 1` (logic.md 6.2 step 3), and a snapshot at frame f is the state before f is stepped (3.1),
so `F + 2` is the first state the new code wrote (@vdw-physics's reading of the first run with the bind fixed).
Touch isn't in it: headless has no hands, so a person checks touch in the demo itself. Last, A replays its session
from frame 0 (next paragraph), and every final cut's root must be the live one.

**The live run equals the replay** (the plan's M1 acceptance; replay.rs;
@item:primordial-engine/m1-acceptance-the-live-run-equals-the-replay-a-r; @coordinator-opus). `--check-replay` makes the opener keep its session's final log. The console's `replay` then replays
the log from frame 0, on a thread of its own, and prints one verdict line: "replay: the live run equals the replay: N
final cuts …", or the first final cut whose root differs, or why it couldn't replay.
- **The log** is the one the session already keeps. As the driver finalizes each final cut, the simulation first takes
  that cut's entries from S3: `entries_for_cut` at each frame since the previous final cut, with the cut and its root
  (phases.rs's `Timed`). It must come before the replica's finalize: that moves the replica's `Cv`, and agreement keeps
  its final log only from `Cv`. At n = 1, a cut is voted, final and finalized within one `drive`. Void entries aren't
  in it, since a final cut's `entries_for_cut` is its effective entries (S3 v1.4). Nothing is written to a file, and
  nothing is dropped: the log grows in memory for the whole session, so the check suits a demo-length run, not a long
  one (@design-reviewer's nit on cbd81a02).
- **The replay** starts from the genesis record's frame-0 replica (`Genesis::replay_replica`, whose root the record
  pins). That replica has no local member. A replay computes the agreed state, and a replica whose local member wrote
  a commit predicts it active until it's attested (logic.md 6.2a, option (i)): a replay with the opener local differed
  at the first final cut after the demo's first commit [V]. It steps straight through the log, every entry at its
  stamp, as pe-harness's s2replay does for P2. At each recorded final cut it compares `root_at_cut` with the live root,
  then finalizes that cut, so it holds no more state than the live replica.
- **Scripts:** the replay's runner, with the live runner's settings, holds every module the log commits. It loads
  each `CommitScript`'s `author_module` from the live replica's bytes (`Replica::module`, pe-logic's
  `Script::load_metered`).
- **Only the opener replays,** since its history starts at frame 0. A joiner starts at its import, and at n = 2 every
  final cut's root is both members' root, so the opener's replay covers the session. `--check-replay` needs `--open`,
  and excludes a test fault, whose replay would compute the fault again.

**The commands** (the M1 interim, until the configuration of section 4 replaces them):
- A: `pe-app --vr --open --advertise <A's address>:4433`. It prints an invite, then reads B's reply code on standard
  input.
- B: `pe-app --join <invite> --advertise <B's address>:4433`. It prints its reply code for A, then joins.

`--listen` is the UDP address to bind (default `0.0.0.0:4433`). `--advertise`, which may be repeated, is the address
the invite or the reply names. Without it, a socket bound to a specific address names that address. A socket bound to
every interface (the default `0.0.0.0`, or `[::]`) names this machine's reachable addresses, found by asking the OS
which local address routes to two probes, with no packet sent (`net::route_addresses`, friend-test S2,
@item:primordial-engine/friend-test-prototype-launch-into-vr-basic-menu-s):
- Tailscale's fixed resolver address (`net::ROUTE_PROBES_V4` and `_V6`), routed through the tailnet while Tailscale
  is up;
- a public address (1.1.1.1, or 2606:4700:4700::1111), routed by the default route, which gives the LAN or public
  address.

The tailnet's address is listed first; repeats, loopback and link-local addresses are dropped. pe-net dials an
invite's addresses 250 ms apart, so a machine on the same tailnet or LAN reaches the first one that works. None of
this depends on Tailscale: on another link the default route's address is the one listed, and `--advertise` names
any other. The probes can't see past a router, so a forwarded port needs `--advertise <public address>:<port>`.
Behind a full-tunnel VPN or a Tailscale exit node, both probes give the tunnel's address, so the LAN address isn't
named. Other VPNs aren't probed and need `--advertise` too. With no route at all (offline), the start fails and asks
for `--advertise`. `--seconds <n>` ends a run after
`n` seconds. Without `--open` or `--join` the world runs alone.

**Step 7's fault** (fault.rs; @coordinator-opus). The plan's step 7
injects a fault on B. Two runtime flags arm it, and they exclude each other. Neither is a cargo feature, so the
shipped build meets the feature policy.
- `--arm-fault`, which the demo uses: typing `fault` and Enter in B's console fires it at the next frame B steps,
  `F`, and the console names `F`. Without the flag, `fault` is refused on any machine. Every machine's console reads
  commands: `quit`, `hands`, `fault`, `replay`, `hand-mode` and `draw` (below), and on A also a joiner's reply code
  (debug builds also take `panic`, the panic path's test).
- `--inject-fault-at <F>`: it fires at session frame `F`, fixed at start. B's console prints the frame it joined at;
  the world runs at 60 Hz.

B's console says what it will do when a flag arms it, and says it again when the fault fires. Once `F` is set:
- At the end of frame `F`, B doubles one ball's radius in its own world (its `Shape` row): the highest dynamic sphere
  then. B draws that ball larger, and its leaf in B's hash tree differs. Nothing is sent, and A and the log don't
  change. A frame below B's first one never fires, and B's console says so when it joins.
- The fault repeats each time B steps `F` (live, in a re-simulation, in rung 1's replay), as on a machine that
  computes `F` wrongly. At n = 2 every divergence is a root split, and a split's first step is rung 1 on both members
  (divergence.md 5). A fault that happened only once would be cured there, and rung 2 would never run.
- From the first firing, B reports its physics self-test as `Failed`, by injection. At n = 2 with both self-tests
  passing, which member repairs is an arbitrary pick; this makes A the reference. B's self-test notice and every
  bundle B writes say that the failure is injected, and the notice gives the real self-test's result.
- So at the next cut the roots split, and B's rung 1 fails because its replay repeats the fault. A is the reference.
  B's rung 2 walks the tree to the ball, fetches A's record of it with the physics image, and imports both. B then
  draws the ball at its own size again. Both machines write the split's bundle; B also writes its rung-2 bundle.
- B's first rung 2 fails (section 7): A answers that it doesn't hold its root at the split cut. The cure comes from
  the second attempt, at the cut agreement proposes again a few frames later. So B's console shows a `RungFailed`
  before the cure (@item:primordial-engine/at-n-2-a-root-split-s-first-rung-2-repair-fails-t).
- At n = 2 none of these bundles can be replayed: a split's repair captures no replay evidence (divergence.md 6;
  @item:primordial-engine/replay-evidence-for-a-root-split-s-rung-2-repair). The demo shows them instead.

**The hand-feel switch** (the pull-ahead plan's 1.10, `crate::handfeel`; @coordinator-opus). The
headset test compares two hand feels in one session, switched by console commands that the operator types on the
user's word. The defaults are unchanged.
- `hand-mode closed|reach` sets how both of this member's hands take part in physics: pe-world's `HandMode` input,
  "closed hand" (D18b: physical only while the hand is closed, the default) or "always within reach". The next
  tick's step 2 sends it for both hands at that frame's stamp, and it waits while no hand of this member is in the
  view. If agreement refuses either entry (`TooLate` on a member behind, `BelowOwnStamp`), the next tick sends it to
  both hands again; a hand already in the mode doesn't change. Only a hand's owner may send it, so each machine
  switches its own player's hands. `SimStats` counts the entries (`hand_modes`, `hand_modes_refused`).
- `draw latched|simulated` sets where pe-render draws the own hands (xr.md 5.6): at the late-latched tracked pose,
  the default, or where the simulation has them. The presenter reads the switch every frame
  (`pe_render::display::OwnHandsSwitch`), on the desktop and in VR. The run's display counters mix the two modes, so
  in VR each switch logs the counters until then with the mode it ended ("display (vr): the own hands were drawn
  ... until now; the run so far: ..."); each mode's window is the difference between those lines and the end report.
  The desktop prints only its end report.
- Once either command has been given, the status line shows the pair, for example "Hands: always within reach ·
  drawn where simulated". A status of the simulation's own (a join's catch-up) takes the line while it's up, and the
  pair comes back after it.
- It is `hand-mode`, not `hands`: the scripted hands (T1) own `hands <routine|file|stop>`. A VR control for the user
  to switch alone waits until the headset test is planned (the ruling's option (b)).

**Finality at n = 2.** The quorum of two members is both (q = 2): a cut is final only when both have voted for it.
So finality follows the slower machine, and while either machine stalls or its link is down, nothing new becomes
final on either, though both keep simulating their predictions.
- **A stall shorter than pe-net's idle timeout** ends when both vote again, within the view timeout reached by then
  (it grows by half after each view that timed out on a member's own timer, up to 10 s; a view another member's
  timeout ended at once doesn't grow it, so at n = 2 it grows about every second timed-out view, agreement.md 7). In M1 `t_idle` is 30 s, a stated setting, so that a
  Wi-Fi blip or a Tailscale path switch does not end the session (@coordinator-opus). The entries a
  member made meanwhile reach the other when the link is back. Its hand entries are paced while it is backlogged
  (section 3, step 2), and its edits are delayed, never lost, and final once a cut covers them. An entry stamped
  below a cut that was already final when it arrived is void as late; at n = 2 that is rare, since no cut becomes
  final while one member is cut off.
- **A longer outage ends the session for both.** pe-net closes a link silent for more than `t_idle`, and in M1
  nothing dials again: pe-net and pe-app have no redial. It is as if a process had ended, and going on takes a new
  session with a new invite. The redial comes once pe-agree has return (model v1); the hook already welcomes a
  member that connects again.
- **pe-agree has no return or restart** (agreement.md 8; `Leave` and `Admit` are built). Once either process ends
  without leaving, the other never finalizes again. Restarting the ended process does not help: its key is still a
  member, so it can't be admitted again (an `Admit` of a member is void as `NoChange`). Start a new session.

## 7. Tests for M1

- **No waiting:** the simulation keeps its rate with the render thread stalled, with the network inbox full, and with
  the edit queue full (each a test with a mock on the other side).
- **No stall in a repair:** during a rung-1 replay of `W_fg`, and during a rollback of `W_fg`, every tick ends on
  time, votes and messages keep flowing, and the frame view resumes within the catch-up time the budget gives: at
  the floor budget of two frames the replica gains one frame per tick, so a `W_fg` backlog catches up within `W_fg`.
  That bound is exact, so the test allows two ticks of slack (@design-reviewer).
- **Ticks dearer than a frame period** (the keep-up floor, section 3): a fixed 100 ms a tick besides stepping still
  catches up when a frame costs less than a period, where the budget from stepping alone falls behind for good; a
  10 s stall never gives a tick more than the cap of stepping; frames dearer than a period keep every tick within the
  cap and hold; and the floor changes no state (a replica back from a stall publishes the same objects at the same
  frame as one ticked every frame, with the same root at a grid frame).
- **A slow member's entries** (stamp-ahead, 3.2), in release: on a fixed 1.2 s link and a jittered 0.9-1.5 s one,
  with B's hands sweeping and an edit every 10 frames, B's lead settles at the excess (68 and 85 frames measured), no
  entry of B's, hand or edit, is void as late once it has, and A's view stays at the present; without stamp-ahead the
  same fixed link voids 35 of B's entries. StampLead's unit tests cover the corrected reports, early arrivals (a
  negative report) shedding the lead, the slow fall, the largest report and the clamp.
- **A lead that drains** (3.2's lead changes, @coordinator-opus), in release, with two pe-world members
  on a 25 ms link [V]:
  - *A strike through a drain.* B's closed hands sweep at 0.63 m/s while its 3 s lead drains. A sees them at most
    1.10 times their tracking speed (0.691 against 0.628 m/s), and never slower. The sweep's first turn through the
    resting balls drives the fastest to 0.767 m/s. That's 1.06 times the most of four no-lead runs at sub-frame
    phases (0.678-0.724), and as hard as a no-lead run with the hands 10% faster (0.773). Over the whole drain the
    fastest ball reached 2.17 m/s, against 0.92-2.50 in the no-lead runs: later turns meet balls the earlier ones
    scattered, so those aren't compared.
  - *A wrist flick with a nearly still origin* (0.08 m/s, turning at 6 rad/s). The speed test finds the hands moving
    on 479 of 480 frames, sheds come at least 11 frames apart, and A sees the origin at 1.09 times its speed. Without
    the turn the hands are slow: sheds come 2 frames apart, and a 1 s lead is gone in 4.7 s.
  - *A player who never pauses.* A 3 s lead is gone at 34.05 s, with a hand moving on 2,159 of 2,160 frames. The
    first shed comes at 0.37 s; the first 175 frames go at one in 11 and the last 5 at the small surplus's slower
    rate.

  StampLead's and Motion's unit tests cover the two rates, the pose clock's steps, the hysteresis, the wrist flick and
  the jumps.
- **A dropout leaves the leads alone** (pe-agree's outage mark at 0.3 s, agreement.md 3), in release, with two
  machines over UDP on loopback: after a 0.5 s blackout, a Wi-Fi hiccup, both leads stay within 12 frames and no entry
  is void as late; after a 10 s one, the same holds as finality resumes. At the old 1 s mark, a 0.3 s blackout left
  leads of 20 and 23 frames and a 0.7 s one 49, each held for the 3 s window.
- **Late entries on every frame** (the keep-up floor's last-call need, section 3): entries 18 frames late on every
  tick no longer hold the view, in the model and with two pe-world members whose B reaches A 300 ms late; with them,
  a 10 s stall still never gives a tick more than the cap; and both members finalize the same roots and show the
  same objects once B's hands are still.
- **Startup without a GPU:** `--headless` makes no GPU context, so CI runs the two-process test (section 6). It
  takes no captures: view_capture there answers `capture_unavailable`.
- **Shutdown:** the leave reaches the other member, which pauses for zero frames.
- **Joining from a cut** (section 4), in release, two processes headless on loopback. A plays alone for 5 s, then B
  joins. Both finalize from E with equal roots, B's imported world at E holds B's member row, role and hands, and A's
  console shows the join begin and end. Reds, each failing that test alone: without the `Admit`'s world side, B's
  world has no hands for it; with a script in A's world, the join is refused before any `Admit`; and with a B that
  never starts (a test hook) and `T_join` shortened, A's console reports the failed join. Also, with B's fetch held
  past a shortened `T_join` and then let go, A takes the failed-join line back once the cut is final.
- **An injected fault** (section 6, step 7's fault): two pe-world members with pe-diverge over sim.rs's hub, member 1
  with the fault at frame 121.
  - It was release only while a loaded debug run never healed the split: agreement re-proposed the split cut at a newer
    frame every 6 frames, and each new episode overtook member 1's rung 2
    (@item:primordial-engine/at-n-2-a-root-split-s-first-rung-2-repair-fails-t). Since the split cut is pinned
    (agreement.md 3, L4': the split cut is proposed again unchanged, and each member's newest root for it is cast
    again in every pinned view), the split is one episode and the tests are back in the default set, with
    `a_fault_over_a_slow_link_heals_in_one_episode` (every message 50 ms late each way: one episode on each member,
    member 1 cured by rung 2, finality within 60 frames of the end).
  - [V] Member 1 draws the ball at 0.12 m and then at 0.06 m again, while member 0 never draws it changed. Both see
    a root split, which ends. Member 1 is cured by rung 2, and member 0 isn't repaired. Member 1's self-test fails by
    injection, while its real self-test passes. Both write bundles, and only member 1's say "BY INJECTION". Finality
    resumes on both. The fault's frame was stepped 3 times.
  - In every run (6 of 6), member 1's first rung 2, at the split cut, fails with one source fault (`source_faults`
    1, `busy_rotations` 0; member 0's `serve_refused` 0). A probe showed it was member 0's `NotHeld` for its root at
    that cut. The split's next episode, at the cut proposed again 6 frames later, cures it
    (@item:primordial-engine/at-n-2-a-root-split-s-first-rung-2-repair-fails-t, @design-reviewer's Low).
  - Reds, each failing that test:
    - with the flag ignored, nothing fires;
    - with a fault that fires only once, the split's rung 1 cures it, and no rung 2 runs.

  The same test with `--arm-fault` [V]: `fault`, typed before host frame 121's tick, fires at frame 121, the next
  frame member 1's replica steps, and the console names it. The repair is the same.

  fault.rs's unit tests cover:
  - one ball doubled at its frame only;
  - the same fault on a re-step;
  - no change without a fault;
  - a typed `fault` firing at the frame the simulation names, and refused when typed twice, without `--arm-fault`,
    or for `--inject-fault-at`'s fault.
- **The live run equals the replay** (section 6), two members from the M1 interim's two-member genesis record over
  sim.rs's hub, B's closed hands sweeping through the pit. [V] A's replay reaches every final cut's root: 96 final
  cuts from frame 0 to frame 582. The control drops one of B's entries from the kept log, and the replay differs at
  the first final cut after it. The demo test (section 6) adds B's join and both versions of the lamp.
  - With a void entry (@design-reviewer's L1 on cbd81a02): both members receive two edits on a second chain of B's,
    signed by B, the second back-stamped, both inside one cut interval. [V] The first final cut past them voids the
    second, which A's replica had stepped, and makes the first effective; the replay still reaches every final cut's
    root. The control puts the void entry back into the kept log, and the replay differs at exactly that cut. A late
    entry can't test this: it is stamped below its deciding cut's parent, whose frames the log took before it came.
- **The hand-feel switch** (section 6; the pull-ahead plan's 1.10). [V] A member alone over the stand-in: after
  `hand-mode reach`, the next frame sends one `HandMode` for each hand and the world has both hands' `Hand.mode`
  within reach; `hand-mode closed` sets them back, and the status line shows each pair. The control doesn't send the
  queued mode, and the world keeps the default. pe-render's presenter takes a `draw` switch at the next frame, and
  back. handfeel.rs's unit tests cover the commands, a refused command queuing nothing, and a mode put back while no
  hand is in the view.
- **The cut log** (`PE_CUT_LOG`, section 4; the pull-ahead plan's 1.2): two members over sim.rs's hub, each writing
  its own. [V] In 300 frames each logged 46 cuts, one line per cut finalized, all `agreed`, with the same root at
  every cut. The end line's finality lag read mean 16.5 and 16.0 frames, p99 and max 19, and member 0's deepest
  rollback was 4 frames. Through F1's fault run at n = 2, both logs carry the split cut (height 20, frame 126) once,
  `agreed`, at member 0's live root, with no `diverged` or `damaged` line [V]. The control removes the hook, and the
  log stays empty. cutlog.rs's unit tests cover the three line forms (`agreed`, `diverged` with `mine`, `damaged`),
  the poll's flush within a second with no cut after a line, `t` counted from the opening, and the refusal of an
  existing file.
- **The two-process session** of section 6, on Windows and Linux.

## 8. Open points

- The audio sink: M1 counts and logs effects; the sound library is chosen later, under the 100% Rust rule.
- Where saves live and when they are written (M3, M5).
