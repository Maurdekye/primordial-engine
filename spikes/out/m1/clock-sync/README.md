# Continuous, fault-tolerant clock sync (design 10.4): evidence

pe-app at p2/app 4634137 (`crates/pe-app/src/clock.rs`), Windows release unless noted, measured. The
problem it fixes is in `../wsl-clock-skew/`: the WSL2 VM's clock runs 2-9 % slow against Windows, and
a sync done once at the start broke Windows + WSL sessions.

The rule as built: every member pings every live member every 500 ms from its start. The frequency is
re-selected each round (10.4 item 5): each peer's session frequency in this member's raw units is
(1 + ρ_j)(1 + r_j) - 1 (ρ_j the fitted raw-clock ratio, r_j the correction its pong reports), and r
moves 25 % of the way to the fault-tolerant midpoint of those values, own included, plus 2 % toward the
midpoint of the raw clocks (the anchor, proposed for 10.4; without it the common frequency wandered by
up to 7.5 % on a Windows + WSL run). Offsets slew toward the fault-tolerant midpoint of the predicted
offsets. R18 stamps on the rate-corrected clock, and a report standing 1.5 s above the reporter's
round-trip excess + 5 ms is capped there.

| Run | Result |
|---|---|
| `mixed-windows-wsl-2min/`: members 1-2 Windows, 3-4 native Linux in WSL, loss and delay on 2 and 3, R18 on, 7200 frames | **ALL EQUAL with all four, 0 membership decisions, 0 pauses, 0 failed checks.** Before the fix the same 2 minutes made 215 empty view changes (R18 off) or removed an honest member within 30 s (R18 on). The frequencies stay within 0.1 % of true time but for one WSL skew episode (55-80 s), which the members split and recover from |
| `cap80-1/`, `cap80-2/`: member 1's uplinks capped to 80 kbit/s from 2 s, R18 on, 1800 frames | ALL EQUAL. Real queue mean 22 / 19 ms, max 305 / 288 ms, lag max 24 / 25 frames, 0 cap drops. Before the clock fix (`../../r18-memory/windows/cap80-after`): 25 / 353 ms, 31 frames, 0 drops |
| `skew4-ftm/`: member 4's clock runs 9 % slow (`--clock-skew-ppm -90000`), 1800 frames | ALL EQUAL, 0 membership decisions, 0 pauses (the start-up transient, review M2) |
| `single4-ftm/`, `single4-median/`: member 4 shows member 1 alone a clock running 5 % fast | Both ALL EQUAL, 0 decisions; the lie is trimmed. At n = 4 the plain median equals the fault-tolerant midpoint |
| `collude7-ftm/`: 7 members (t = 2). Members 6 and 7 run 5 % slow; members 4 and 5 report to them a clock 1 s behind and as slow as theirs, and to 1-3 one 1 s ahead and 5 % fast | **ALL EQUAL with all seven, 0 decisions.** Session frequencies: members 1-5 at 1.00003 of true time, 6-7 at 0.99657 (pure consensus put all at 1.00045; the frequency chosen once left a 2.6 % split) |
| `collude7-median/`: the same with `--clock-median` | **The honest slow pair is removed** (epoch 1, removed [6, 7]). Members 1-5 ALL EQUAL |
| `loopback-windows/`, `tests-windows.log` | `m1-loopback.sh` ALL EQUAL; pe-session + pe-mcp + pe-app tests: 180 pass |
| `linux-wsl.log` | WSL2 native glibc: build, pe-app and pe-mcp tests, `m1-loopback.sh` ALL EQUAL (exit 0 under `set -e`; the cargo output is interleaved) |

Start-up (unit test `the_start_up_gap_stays_small_at_up_to_9_percent_skew`, two members): the session
clocks differ by at most 11 / 24 / 43 ms at 2.3 / 5 / 9 % skew and by under 5 ms from second 12-13.

`args.txt` in each fault run lists every member's switches; `decisions.txt` counts membership decisions
and pauses per member; the status lines in `run-*.log` show each member's offset, frequency and
round-trip excess every 2 s.

Found and fixed on the way (each by a run here): fitting slopes of the peers' corrected clocks made two
members' frequencies oscillate (unit test), so frequencies come from raw ratios and reported
corrections; the scripted delay was scheduled on the rate-corrected clock and released on local time;
R18's round-trip clamp needed a margin under the 10 ms calm level, a real-time in-flight term, and a
grace (an unlucky ping phase held back the 80 kbit/s run's first raise: 480 ms against 394, bisected);
a 6 s frequency span read the first congestion's one-way queue as +0.76 %; and the end-of-run replay
now waits for the linger, since a WSL member blocked for 4 s by it was removed.
