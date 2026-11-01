# Continuous, fault-tolerant clock sync (design 10.4): evidence

pe-app at p2/app d398e86 (`crates/pe-app/src/clock.rs`), Windows release unless noted, measured. The
problem it fixes is in `../wsl-clock-skew/`: the WSL2 VM's clock runs 2-9 % slow against Windows, and
a sync done once at the start broke Windows + WSL sessions.

| Run | Result |
|---|---|
| `mixed-windows-wsl-2min/`: members 1-2 Windows, 3-4 native Linux in WSL, loss and delay on 2 and 3, R18 on, 7200 frames | **ALL EQUAL with all four, 0 membership decisions, 0 pauses, 0 failed checks.** Before the fix the same 2 minutes made 215 empty view changes (R18 off) or removed an honest member within 30 s (R18 on). The members split the skew: Windows run at -17 500 ppm, WSL at +18 000-18 400 ppm. Member 1 spends 54 % of the ticks at R18 level 0 |
| `cap80/`: member 1's uplinks capped to 80 kbit/s from 2 s, R18 on, 1800 frames | ALL EQUAL. Level shares {1: 17, 2: 55, 3: 7, 4: 6, 5: 14} %, real queue mean 37 / max 396 ms, lag max 30 frames, 0 cap drops. Before the clock fix (`../../r18-memory/windows/cap80-after`): {1: 19, 2: 56, 3: 7, 4: 5, 5: 13} %, 25 / 353 ms, 31 frames, 0 drops |
| `single4-ftm/`, `single4-median/`: 4 members; member 4 shows member 1 alone a clock running 5 % fast | Both ALL EQUAL, 0 membership decisions; member 1's frequency stays within 5 ppm (the lie is trimmed). At n = 4 the plain median equals the fault-tolerant midpoint (review note) |
| `collude7-ftm/`: 7 members (t = 2). Members 6 and 7 run 5 % slow (`--clock-skew-ppm -50000`); members 4 and 5 report to them a clock 1 s behind and as slow as theirs, and to 1-3 one 1 s ahead and 5 % fast | **ALL EQUAL with all seven, 0 membership decisions.** Members 1-5 run within 12 ppm of their raw clocks; 6 and 7 at +26 300 ppm (the midpoint of the honest range; the remaining 2.6 % is taken up by the offset slew) |
| `collude7-median/`: the same with `--clock-median` | **The honest slow pair is removed** at 25 s (epoch 1, removed [6, 7]): the median pins their frequency at 0 until then. Members 1-5 ALL EQUAL |
| `loopback-windows/`, `tests-windows.log` | `m1-loopback.sh` ALL EQUAL; pe-session + pe-mcp + pe-app tests: 179 pass |
| `linux-wsl.log` | WSL2 native glibc: build, pe-app and pe-mcp tests, `m1-loopback.sh` ALL EQUAL (exit 0 under `set -e`; the cargo output is interleaved) |

`args.txt` in each fault run lists every member's switches; `decisions.txt` counts the membership
decisions per member; the status lines in `run-*.log` show each member's offset, frequency and
round-trip excess every 2 s.

Found and fixed on the way (each by a run here): fitting a peer's slewed or rate-corrected clock made
two members' frequencies oscillate (unit test), so frequencies come from raw clocks; the scripted delay
was scheduled on the rate-corrected clock and released on local time (a fast member held its
datagrams longer and longer); R18's round-trip clamp needed a margin under the 10 ms calm level and to
count an unanswered ping's age at once; and the end-of-run replay now waits for the linger, since a
WSL member blocked for 4 s by it was removed.
