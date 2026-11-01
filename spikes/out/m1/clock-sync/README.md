# Continuous, fault-tolerant clock sync (design 10.4) and R18 under clock drift: evidence

pe-app at p2/app 00250f7 (`crates/pe-app/src/clock.rs`, `node.rs`), all measured. The problem the
clock sync fixes is in `../wsl-clock-skew/`: the WSL2 VM's clock runs 2-9 % slow against Windows, and a
sync done once at the start broke Windows + WSL sessions. This set answers review H1 (@shader-reviewer,
d835a94): on the 2-minute Windows + WSL run with no queue, R18 sat at level 5 for 49-60 % of ticks
(84-100 % after 50 s), with signals up to 532-712 ms.

## What changed for H1

* **R18's one-way estimates restart** at frame 0 and when the sender's frequency is first fitted (before
  that the whole skew drifts in: 9 % is 90 ms/s). The clamp's round-trip window restarts with them.
* **The round-trip clamp, redone.** A report's bound is the reporter's round-trip queueing (least of the
  last 4 round trips over the best since the restart), less the queue this member measures on that
  member's datagrams (the way back; its peak until the frequency is fitted), plus 5 ms. While a report
  of more than 10 ms stands above it, the reporter gets a probe every 100 ms, riding on a datagram going
  there anyway (alone after 250 ms). A probe reads as a queue when the lesser excess of it and the probe
  before, plus 5 ms, is at least 9/10 of the lesser of the reports they were sent against.
  * The report passes until the first verdict and while the latest verdict is a queue, for the report's
    own lag after one, and while the reports fall faster than any clock drifts (15 %/s): a draining
    queue. Otherwise it is clamped to the round trip's reading. An episode outlives a second below its
    bound.
  * Gone: the fixed grace, the in-flight term (H1 (a): a lost ping opened the clamp to ~400 ms), and
    the margin on a clamped value (H1 (b): jitter kept it above the 10 ms calm level).
* **Tried and dropped** (each measured, in the commit messages): correcting the delays by the fitted
  frequency difference (adds the fit's error, which congestion biases: 80 kbit/s queue to 667 ms);
  waiting for the frequency fits before frame 0, alone or with the frequency rounds running before it
  (the corrections then converge right after frame 0, or the common frequency wanders to -7.6 % before
  it; Windows members were raised).
* The bootstrap carries the leader's sample at its fitted frequency; the largest signal counts from
  frame 0.

## Runs

Members 1-2 on Windows, 3-4 native Linux in WSL2 unless noted; 7200 frames (2 minutes), R18 on, 3
touches/s. "Steps": member 2 gets 10 % loss and 30 ± 10 ms delay from 2 s, member 3 5 % and 25 ± 10 ms
from 3 s (session time). Level shares are of the controller's ticks; "clamped" counts node steps
(about one per ms) in which the clamp lowered the fed signal.

| Run | Level 0 share, members 1 / 2 / 3 / 4 | Largest signal (ms) | Clamped steps | Result |
|---|---|---|---|---|
| `mixed-windows-wsl-2min-1/` (steps) | 100 / 75.6 / 70.9 / 100 % | 13 / 46 / 47 / 19 | 54674 / 61485 / 98299 / 100853 | ALL EQUAL, 0 decisions, 0 pauses |
| `mixed-windows-wsl-2min-2/` (steps) | 100 / 100 / 79.2 / 100 % | 14 / 37 / 62 / 13 | 60976 / 56787 / 100505 / 92282 | ALL EQUAL, 0 decisions, 0 pauses |
| `mixed-windows-wsl-2min-3/` (steps) | 100 / 100 / 69.3 / 100 % | 18 / 39 / 47 / 14 | 59829 / 60186 / 107119 / 108894 | ALL EQUAL, 0 decisions, 0 pauses |
| `mixed-windows-wsl-2min-nostep/` (the same impairments from the start) | 100 / 100 / 100 / 100 % | 16 / 24 / 18 / 14 | 55790 / 55801 / 102573 / 109214 | ALL EQUAL, 0 decisions, 0 pauses |
| `control-all-windows-2min/` (steps; all four on Windows) | 100 / 100 / 100 / 100 % | 16 / 39 / 33 / 16 | 59296 / 914 / 615 / 59578 | ALL EQUAL |
| `three-members-2-windows-1-wsl/` (members 1-2 Windows, 3 WSL; impairments from the start) | 100 / 100 / 100 % | 14 / 21 / 24 | 56050 / 57694 / 101908 | ALL EQUAL, 0 decisions, 0 pauses |
| `skew4-ftm/`: member 4's clock 9 % slow, loopback, 1800 frames | 100 / 100 / 100 / 100 % | 0 / 0 / 0 / 13 | 0 / 0 / 0 / 19041 | ALL EQUAL, 0 decisions |

* **What is left (the stepped runs):** member 3 is the WSL member whose own uplink gets the scripted
  25 ± 10 ms step at 3 s. LEDBAT reads a delay step as queue (10.9's limits), so with no drift at all
  its reports read 25-35 ms, and member 2's signal (30 ms step) reads 30-36 ms; the all-Windows control
  peaks at 33 and 39 ms, just under the 40 ms trigger. Until the frequencies are fitted (about 10 s
  after the start), the VM's uncorrected drift drains the reverse step out of the way back's estimate,
  and the round trip then shows it as forward queue. A drifting report passes up to that, and member 3
  crossed 40 ms a few times between 3.7 and 10.2 s; D56's memory then held the level until 32-41 s.
  Member 2 crossed once, at the fit, in run 1 (level 0 again at 40 s). Without the steps every member
  stays at level 0 throughout. M1 accepts this as a documented limit (@coordinator-opus, 2026-11-01);
  the lever is frequencies that converge before frame 0, with item 5's rounds rate-limited to fresh
  corrections.
* **Session frequency:** the Windows members end 1.7-4.3 % slow of their own clocks and the WSL members
  1.5-3.2 % fast of theirs: with two native and two VM clocks the anchor's median is halfway.
* **Three members (2 native + WSL), the M1 demo's likely shape:** the session stays near native time.
  The Windows members' corrections stay within ±1.1 % (the WSL member's: +3.6 to +6.4 %, its clock
  swinging with a ~35 s period). At n = 3 the agreement step's midpoint is the midrange (t = 0), so the
  natives follow the VM's tracking lag by half, and the anchor (median, AN-1) pulls them back. In an
  earlier 3-member run at 1303495, with the WSL clock steadier, they stayed within ±0.3 %.

| Run | Result |
|---|---|
| `cap80-1/`, `cap80-2/`: member 1's uplinks capped to 80 kbit/s from 2 s, 1800 frames | ALL EQUAL. Real queue mean 20.6 / 22.2 ms, max 340 / 353 ms; lag max 24 / 24 frames; 0 cap drops; 0 clamped steps. At d835a94: 22 / 19 ms, max 305 / 288, lag 24 / 25 |
| `cap80-paired/`: the same, d835a94's binary (`ctl-2`) between two of this build's (`new-1`, `new-3`) in one sitting | new 12.8 / 18.3 ms mean, max 328 / 353, lag 25 / 25; d835a94 20.8 ms, max 343, lag 24; 0 clamps. Level shares split into two modes run to run (mostly level 2, or mostly 4-5 with a smaller queue) in both builds |
| `single4-ftm/`, `single4-median/`: member 4 shows member 1 alone a clock running 5 % fast | Both ALL EQUAL, 0 decisions, every member at level 0 throughout |
| `collude7-ftm/`: 7 members (t = 2). Members 6 and 7 run 5 % slow; members 4 and 5 report to them a clock 1 s behind and as slow as theirs, and to 1-3 one 1 s ahead and 5 % fast | **ALL EQUAL with all seven, 0 decisions,** every member at level 0 throughout. Members 6-7 correct by +4.90 %, so their session runs at 0.95 x 1.0490 = 0.99656 of true time against 1.00000 for 1-5: the 0.35 % split of review L1. R18 clamps 6 and 7 all session (largest signal 12-13 ms) |
| `collude7-median/`: the same with `--clock-median` | **The honest slow pair is removed** (epoch 1, removed [6, 7]). Members 1-5 ALL EQUAL |
| `loopback-windows/`, `tests-windows.log` | `m1-loopback.sh` ALL EQUAL; pe-session + pe-mcp + pe-app tests: 181 pass |
| `linux-wsl.log` | WSL2 native glibc: build, pe-app and pe-mcp tests, `m1-loopback.sh` ALL EQUAL |

`args.txt` in each fault run lists every member's switches and `decisions.txt` counts membership
decisions and pauses; `levels.txt` has the level shares. The status lines in the run logs show, every
2 s, each member's offset, frequency correction, and per peer the clamp's round-trip excess and way back.
