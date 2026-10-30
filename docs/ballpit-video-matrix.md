# Ball-pit video matrix: what the engager and an observer actually see

Branch `viz/ballpit-matrix`, based on 3d0f98a. Spike 5d showed that the pose-source model for the
ball pit costs about 15 kB/s and converges bit-identically. These videos show what the two players
see while that happens, frame by frame, under different network conditions and protocol
configurations.

## Outputs

These are kept out of git (42 MB). Regenerate them with `bash scripts/run_ballpit_viz.sh`, which
takes about 2 minutes.

| File | What it is |
|---|---|
| `docs/media/ballpit-matrix/ballpit-matrix-grid.mp4` | **The main deliverable.** 3840x2160, 60 fps, 8.5 s, 13 MB. Columns are the three configurations; rows are the six network conditions. Each cell shows the engager on the left and the observer on the right |
| `docs/media/ballpit-matrix/cells/<cell>.mp4` | One video per cell, 1920x576, 60 fps, about 1.5 MB each. Same content with a fuller overlay |
| `docs/media/ballpit-matrix/summary.md` | The table below |
| `docs/media/ballpit-matrix/metrics/<cell>.csv` | Every overlay number, per display tick and per peer (committed) |
| `docs/media/ballpit-matrix/verification.txt` | End-state hashes, replay digests and protocol counters for each cell (committed) |
| `docs/media/ballpit-matrix/adapter.txt` | The GPU that rendered the videos: an NVIDIA GeForce RTX 3080 Ti, through Vulkan, headless. No software fallback was used |

Each video covers 1.6 s to 10.1 s of the run. The gesture runs from 2.1 s to 5.77 s: the hand
descends, sweeps a sine across the pit for 3 s, then rises. That leaves 1.6 s before the gesture
for the settled pit and 4.3 s after it for convergence.

## How the views are produced (verified vs inferred)

**Verified: the displays come from the real protocol code.** Each cell runs on the session
simulator (`spikes/session-sim`) with its virtual clock and byte-encoded messages. The ball-pit
scenario is the one from spike 5d: 500 balls, the same scene, and the same hand script, now
`ballpit_sweep` in the session-sim library. That script was moved out of `main.rs` unchanged.
After the move and the new bandwidth cap, `scripts/run_session_sim.sh` reproduces every
scenario log byte for byte, apart from the measured CPU timings. At every 60 Hz display tick of
each peer, the recorder reads the transform of every ball and both hands from that peer's
`WorldSim`, after its own predictions, rollbacks and corrections. Nothing is interpolated,
smoothed or idealized. What you see is that peer's state at the start of its frame, the moment a
game would render it.

* **Engager** = member 1, who moves the hand. **Observer** = member 2. Their clocks are skewed
  +1 ms and -1 ms, as in spike 5d.
* **Pose-source** cells are the spike's protocol unchanged. Each frame, the engager authors one
  44-byte Hand entry, stamped `now + input delay`. The observer predicts "the hand holds still",
  then rolls back to a keyframe (every 4 frames) and re-simulates when the entry lands. Acks go out
  every 2 frames. All late arrivals within a frame are coalesced into one rollback; this is option
  (c), already on in the spike. That is why rollback coalescing is not a separate axis here.
* **Naive state streaming** is new code, written to the spike's own definition of the naive
  baseline. Every frame, the engager sends the full state of every object whose state changed:
  56 B per object, plus a 16 B header per datagram, in datagrams of at most 1,200 B. The observer
  runs no simulation; it draws the newest state it has received for each object. It does no
  interpolation or extrapolation, keeps no reliability layer, and applies no keyframes. Both peers
  run the same local simulation until frame 90 (1.6 s), when the pit is settled, so streaming
  starts from an identical state. A production naive design would add interpolation and
  reliability. These cells show the spike's baseline, not a tuned product.
* **Network.** One-way base latency plus 0 to 10 ms of uniform jitter per datagram, 1% random
  loss, and every datagram costs its payload plus 52 B. The **bandwidth cap**, new in `Net`
  (`Cap`/`Shaper`), is a per-direction token bucket: rate R, a burst of max(1,500 B, R x 20 ms),
  and a FIFO queue. A datagram that would wait more than 500 ms is tail-dropped.
* **Rendering.** wgpu 30 runs headless on the RTX 3080 Ti with 4x MSAA and a shadow map. Balls are
  coloured by where they spawned, in 16 patches, so displaced balls stand out. The engager's hand
  is the orange sphere on an arm; the grey sphere is member 2's parked hand, which exists in the
  scene but never moves. The file is encoded by ffmpeg (libx264, CRF 20 per cell, CRF 22 for the
  grid).

**Annotations, not part of either peer's display.**

* The translucent white ghost marks where the engager's controller is right now, from the
  scripted target. It makes input delay and observer lag visible.
* **Magenta** marks a ball that just *snapped*: its drawn displacement differs from what its own
  drawn velocity explains over the sim frames its state advanced (|Δx − v·dt·Δframes| > 1 cm,
  where Δframes is the displayed sim-frame difference, or for the naive observer, that ball's
  newest-update frame difference). The tint fades over 15 frames.
* **Dark grey** marks a ball that is *frozen* this tick: its drawn state did not advance
  (Δframes = 0) although its drawn velocity is above 0.06 m/s (1 mm per frame). This happens
  when a pose peer stalls, or when a naive observer has no new update for that ball. A frozen
  ball is not a snap. When it resumes, the jump counts as a snap only if it differs from its own
  velocity extrapolated over the skipped frames.
* The bar under each 3D view is the timeline, with the gesture shown in orange.

**Calibrating the snap detector (verified).** In the six pose-source cells where the engager
never rolls back, its own display registers 70 snapped ball-frames in the whole run. They come
from contact pushes, most of them at ticks 141-148, when the scripted hand jumps 0.6 m sideways
at the end of its descent. That jump is a property of the 5d script and is kept as it is. So 70 is
the detector's noise floor; anything above it is a correction (a rollback, or a state update that
disagrees with what the peer drew). Freezes are counted separately. The 1 cm threshold equals a
0.6 m/s velocity error at 60 Hz. It is fitted to this pit and gesture, not a general constant.

*Revision after review (V-1):* the first version predicted with v·dt alone. It assumed one sim
frame per display tick, so it counted frozen ticks as snaps, inflating the capped naive cells and
the pose-d4 64 kbit/s cells by up to about 3.5x. The detector now uses Δframes, and freezes have
their own tint and count. The pose-source cells that never stall did not change, including the
300 ms engager's 8,437.

**Overlay metrics (all measured).**

* **tx/rx** are wire bytes per second over the last second. **tx** counts what the peer
  *offered*, including datagrams a full queue then dropped.
* **queue** is the backlog of the peer's outgoing bottleneck; **q-drops** counts datagrams
  dropped there.
* **rollbacks/s** and **re-sim CPU**: re-sim is the measured CPU time on this machine, averaged
  per display frame over the last second. It is the only number that varies between runs, by up
  to about ±50% here, because other agents shared the machine.
* **snapshots** is the bytes of rollback keyframes held (`WorldSim::ring_bytes`, new). The naive
  engager needs no snapshots. The naive observer holds a 32 kB state table.
* **confirmed lag** is the displayed frame minus the confirmed frame. "SIM STALLED" means the
  peer hit the 120-frame speculation limit and froze.
* **Divergence** is the observer's displayed ball positions compared with the engager's
  displayed positions *at the same instant*. It is what two players would see if they compared
  screens.

## The matrix, and why this slice

The full product of the axes (3 configurations x 4 latencies x 3 bandwidths) is 36 cells. This
matrix uses 18:

* **A latency sweep at unlimited bandwidth:** 0, 50, 150 and 300 ms one-way, for every
  configuration. This isolates what latency alone does. 150 ms crosses nothing; 300 ms crosses
  the protocol's 250 ms late allowance (15 frames), which changes the engager's own view (see
  below).
* **A bandwidth sweep at 50 ms:** 256 kbit/s and 64 kbit/s, for every configuration. 256 kbit/s
  is about 3 times what pose-source needs and 1/26 of what naive needs. 64 kbit/s is just
  *below* what pose-source needs as built. Under the cap, the engager offers about 80 kbit/s
  towards the observer during the sweep: 60 Hand datagrams plus 30 acks per second, each with
  52 B of overhead.
* Bandwidth is not crossed with the other latencies. The cap's effect, queueing, adds to latency
  and does not interact with it otherwise. Crossing them would double the matrix without showing
  anything new.
* **Configurations:** naive state streaming, labelled "no rate control, no resend" in the
  videos; pose-source with input delay 0; pose-source with
  input delay 4, which **adds 67 ms (+4 frames) of latency to the engager's own hand, by
  design**. Its rows and labels say so.

## Results per cell

"Hand lag" is how far behind the controller the drawn hand sits, during the steady sweep. It
includes the 1-frame render pipeline. "Converged" is the time after the gesture ends (5.77 s)
until no ball differs by more than 1 cm for the rest of the video. "Final state" for pose-source
compares full-state hashes and replay digests; for naive, it compares the observer's table with
the engager's state.

| Cell | Engager hand lag behind controller | Observer hand lag | Engager rollbacks/s (sweep) | Observer rollbacks/s, re-sim ms/frame (sweep) | Engager tx (sweep) | Observer tx (sweep) | Max queue, queue drops | Peak snapshot memory | Balls >1 cm off at the observer: sweep mean / peak | Snapped ball-frames: engager / observer | Frozen ball-ticks: engager / observer | Converged after gesture end | Final state |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `naive_0ms_unl` | 1.0 frames (17 ms) | 1.8 frames (30 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 13.2 / 54 | 70 / 655 | 0 / 1838 | 0.08 s | identical |
| `naive_50ms_unl` | 1.0 frames (17 ms) | 4.8 frames (80 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 39.0 / 94 | 70 / 655 | 0 / 1838 | 0.20 s | identical |
| `naive_150ms_unl` | 1.0 frames (17 ms) | 10.8 frames (180 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 56.6 / 114 | 70 / 655 | 0 / 1838 | 0.30 s | identical |
| `naive_300ms_unl` | 1.0 frames (17 ms) | 19.8 frames (331 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 78.8 / 138 | 70 / 655 | 0 / 1838 | 0.45 s | identical |
| `naive_50ms_256k` | 1.0 frames (17 ms) | 32.0 frames (533 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 500 ms, 3959 | 0.0 MB | 218.8 / 372 | 70 / 248 | 0 / 34781 | never (within 4.3 s) | DIFFERENT (367 balls off, max 1.65 m) |
| `naive_50ms_64k` | 1.0 frames (17 ms) | 31.1 frames (519 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 500 ms, 4063 | 0.0 MB | 233.1 / 388 | 70 / 111 | 0 / 18624 | never (within 4.3 s) | DIFFERENT (387 balls off, max 1.87 m) |
| `pose-d0_0ms_unl` | 1.0 frames (17 ms) | 1.0 frames (17 ms) | 0.0 | 0.5, 0.01 | 8.3 kB/s | 3.3 kB/s | 0 ms, 0 | 3.5 MB | 0.1 / 11 | 70 / 70 | 0 / 0 | by gesture end | identical |
| `pose-d0_50ms_unl` | 1.0 frames (17 ms) | 3.8 frames (63 ms) | 0.0 | 51.0, 2.35 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 16.2 / 105 | 70 / 1826 | 0 / 0 | by gesture end | identical |
| `pose-d0_150ms_unl` | 1.0 frames (17 ms) | 9.8 frames (164 ms) | 0.5 | 49.3, 5.20 | 10.6 kB/s | 3.7 kB/s | 0 ms, 0 | 18.7 MB | 57.3 / 129 | 329 / 3063 | 0 / 0 | by gesture end | identical |
| `pose-d0_300ms_unl` | 1.0 frames (17 ms) | 18.8 frames (314 ms) | 19.7 | 46.6, 7.23 | 18.0 kB/s | 35.3 kB/s | 0 ms, 0 | 33.5 MB | 116.6 / 222 | 8437 / 2260 | 0 / 0 | 0.47 s | identical |
| `pose-d0_50ms_256k` | 1.0 frames (17 ms) | 3.8 frames (63 ms) | 0.0 | 51.0, 2.13 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 16.2 / 105 | 70 / 1826 | 0 / 0 | by gesture end | identical |
| `pose-d0_50ms_64k` | 1.0 frames (17 ms) | 17.3 frames (289 ms) | 7.0 | 43.9, 5.74 | 9.8 kB/s | 13.6 kB/s | 500 ms, 783 | 54.8 MB | 105.1 / 243 | 4506 / 2077 | 2 / 0 | 2.97 s | identical |
| `pose-d4_0ms_unl` | 5.0 frames (83 ms) | 5.0 frames (83 ms) | 0.0 | 0.0, 0.00 | 8.3 kB/s | 3.3 kB/s | 0 ms, 0 | 3.5 MB | 0.0 / 0 | 70 / 70 | 0 / 0 | by gesture end | identical |
| `pose-d4_50ms_unl` | 5.0 frames (83 ms) | 5.0 frames (84 ms) | 0.0 | 0.8, 0.07 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 2.0 / 84 | 70 / 202 | 0 / 0 | by gesture end | identical |
| `pose-d4_150ms_unl` | 5.0 frames (83 ms) | 9.8 frames (164 ms) | 0.5 | 49.3, 3.40 | 10.6 kB/s | 3.7 kB/s | 0 ms, 0 | 18.7 MB | 44.0 / 126 | 315 / 2583 | 0 / 0 | by gesture end | identical |
| `pose-d4_300ms_unl` | 5.0 frames (83 ms) | 18.8 frames (314 ms) | 0.7 | 46.6, 9.93 | 18.0 kB/s | 3.8 kB/s | 0 ms, 0 | 33.2 MB | 98.2 / 218 | 555 / 2955 | 0 / 0 | by gesture end | identical |
| `pose-d4_50ms_256k` | 5.0 frames (83 ms) | 5.0 frames (84 ms) | 0.0 | 0.8, 0.06 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 2.0 / 84 | 70 / 202 | 0 / 0 | by gesture end | identical |
| `pose-d4_50ms_64k` | 5.0 frames (83 ms) | 17.7 frames (294 ms) | 6.2 | 29.8, 5.02 | 10.0 kB/s | 9.8 kB/s | 500 ms, 770 | 55.0 MB | 87.9 / 270 | 3615 / 2573 | 1531 / 1400 | 4.00 s | identical |

All 12 pose-source cells: both members' histories equal an offline replay of the final log, and
the end-state hashes are identical (`verification.txt`).

## What to look for

1. **Engager immediacy (pose-source, delay 0).** The engager's hand sits 1 frame behind the
   controller at every latency and bandwidth. Its own balls react at once. Below 250 ms, the
   engager never rolls back (one hand). Delay 4 trades that away: the hand trails the ghost by 5
   frames (83 ms) everywhere, including at 0 ms.
2. **The observer's view of the hand is always about one-way latency behind**, in every
   configuration: 1 / 3.8 / 9.8 / 18.8 frames at 0 / 50 / 150 / 300 ms. Physics cannot do better.
   What differs is the balls:
   * **Pose-source:** the observer predicts with the hand held still, so balls the hand is
     about to hit stay put until the entry lands, then **snap** into place (magenta). It does
     this about 50 times a second at 50 ms and above, re-simulating about 2-2.4 ms per frame at
     50 ms, 3.4-5.2 ms at 150 ms and 7-10 ms at 300 ms (measured CPU; noisy). Once the entries land, the observer's balls
     match the engager's *frame*. But the comparison is with what the engager shows *now*, so
     balls near the hand are off by up to about 10-25 cm while it moves: a mean of 16 balls at
     50 ms and 57 at 150 ms.
   * **Delay 4** hides latency up to its 67 ms: at 50 ms, the observer rolls back 0.8 times a
     second, against 51, and averages 2 balls off, against 16. Past the delay, it behaves like
     delay 0.
   * **Naive (unlimited):** the observer simply draws the engager's world one-way latency late.
     It never mispredicts, but every moving ball is stale, so more balls are off at the same
     instant than with pose-source (39 vs 16 at 50 ms). Where jitter or loss delays a frame's
     update, the ball freezes for a tick (1,838 frozen ball-ticks) and occasionally snaps (655
     ball-frames). It costs **845 kB/s**, against **8.8 kB/s** for pose-source.
3. **300 ms crosses the late allowance (pose-source, delay 0).** Every Hand entry reaches the
   observer more than 250 ms after its stamp. The observer's late reports then **re-stamp** all
   222 entries later (design H5), so the agreed history has the hand moving later than the
   engager drew it. The engager rolls back its *own* history 107 times (up to 76 frames deep).
   Its own pit visibly snaps: 8,437 snapped ball-frames against the floor of 70. Its acks also
   swell with late lists, so the observer offers 35 kB/s against 3.7 kB/s at 150 ms. Delay 4
   avoids all of this at 300 ms: the entries arrive 14 frames late, under the 15-frame allowance.
4. **Bandwidth.**
   * At 256 kbit/s, pose-source is identical to unlimited.
   * Naive collapses: 95% of its datagrams are tail-dropped and the observer's pit freezes in
     patches (dark grey: 34,781 frozen ball-ticks at 256 kbit/s and 18,624 at 64 kbit/s, against
     248 and 111 snaps). Its end state is **never** correct: 367 balls off, up to 1.65 m, and no repair,
     because the naive scheme only resends balls that move again.
   * At 64 kbit/s, pose-source as built also exceeds the cap, and the result is the most
     instructive cell:
     * 500 ms of bufferbloat and about 780 queue drops lead to re-stamping (151 entries), and the
       acks swell further.
     * Both peers hit the speculation limit and **freeze** ("SIM STALLED"). With delay 4, about
       1,400-1,500 ball-ticks per peer are frozen while moving.
     * The re-stamped history makes the engager's hand **replay part of its sweep after the
       player finished the gesture**. Balls are flung out of the pit.
     * The final state is still bit-identical on both peers, about 3 s (delay 0) or 4 s
       (delay 4) after the gesture.

     So the sync guarantee held, but realtime reactivity broke.
5. **Memory.** Snapshot memory grows with how far the confirmed frame lags behind:
   * 3.5 MB at 0 ms;
   * 8 MB at 50 ms;
   * 19 MB at 150 ms;
   * 33 MB at 300 ms;
   * 55 MB when congested.

   Each keyframe is about 0.2 MB for 500 balls (the world image plus tables), and one is held
   for every 4 frames of unconfirmed history.

## Findings for the protocol (inferred from these runs, not fixed here)

* **False nondeterminism alarms.** `WorldSim::evaluate_checks` compares a remote check hash with
  the local one even while a local re-simulation is pending. When a late report re-stamps an
  entry, the comparison uses a stale local hash and counts `checks_bad`. This happened in the
  150 ms cells after a loss repair, and in the 64 kbit/s cells. A one-line experiment, skipping
  evaluation while `dirty_from` is set, took every one of these cells to 0 bad checks. The
  histories really are identical.
* **Re-stamping at high latency moves the engager's own past.** With a 15-frame allowance, any
  peer more than 250 ms one-way away makes every entry from a fast-acting peer re-stamp, so the
  author's own view gets corrected. An adaptive input delay (stamp ahead by the measured one-way
  delay minus the allowance) or a larger allowance would avoid it. Delay 4 already does at 300 ms.
* **Late lists in acks grow without bound under sustained lateness,** which feeds congestion
  (13.6 kB/s offered at 64 kbit/s). Acks could carry the late reports only once, or compactly.
* **Pose-source does not fit 64 kbit/s as built.** One datagram per Hand entry plus acks at
  30 Hz comes to about 80 kbit/s with overhead. Batching two entries per datagram, or piggybacking
  acks, would roughly halve it.
