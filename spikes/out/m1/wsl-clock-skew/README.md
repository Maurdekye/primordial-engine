# The WSL2 VM's clock runs 2-9 % slow against Windows (measured, this PC)

Inside one WSL2 process (Ubuntu-24.04, kernel 6.6.87.2, clocksource tsc), `time.monotonic()` advanced
30.002 s while Windows measured 30.719 s (ratio 0.977, idle machine). During pe-app runs the receivers'
one-way delay to the WSL members grew by 25-90 ms per second (`link-1.csv`, column rx_q_ms, senders 3
and 4), at the same rate for both WSL members and not at all for the Windows member 2 (its scripted
30 ms delay only).

pe-app syncs its clock once, at the start, with the leader; the R18 stamp uses the sender's local
clock. Both assume clock rates within about 100 ppm, so on a Windows + WSL session:

| run (4 members: 1-2 Windows, 3-4 WSL; loss on 2 and 3) | result |
|---|---|
| `m1mix4`: R18 on, member 4 drops out at 15 s, 30 s | the skew reads as a queue growing to 1.1 s; members 3 and 4 go to level 5; five empty view changes, then 4 removed and then **3 (honest) removed**; members 1-2 ALL EQUAL |
| `m1mix5`: the same with `--no-link-signal` on every member | only 4 removed; members 1-3 ALL EQUAL (member 3 ends 47 frames behind member 1, with 0 rollbacks of its own) |
| `m1mix6`: signal off, no drop-out, 2 minutes | ALL EQUAL with all four, but **215 empty view-change decisions** from 36 s on and repeated speculation-limit pauses at member 1 |
| `m1win7`: the same 2 minutes with all four members on Windows | 0 view changes, 0 pauses, ALL EQUAL |

Earlier mixed runs (30 s, `mixed-windows-wsl-*`) passed because the drift had not built up yet.
