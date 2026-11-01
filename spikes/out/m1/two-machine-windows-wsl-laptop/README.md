# M1 mixed run across two physical machines (M1 evidence gap 2)

Measured on p2/m1-gaps 813400a (main aa2680b plus the equivocation fault and `pe-app relay`); every
binary's hash in `provenance.txt`. Four members over Tailscale, 3600 frames (60 s), 3 touches/s each:

| Member | Machine | OS | Faults |
|---|---|---|---|
| 1 | this PC | Windows | none |
| 2 | this PC | Windows | drops out at 15 s (`--crash-at-ms 15000`) |
| 3 | this PC | Linux (WSL2, native glibc build) | 5 % loss, 25 ± 10 ms from 3 s |
| 4 | the laptop | Windows | 10 % loss, 30 ± 10 ms from 2 s |

* **ALL EQUAL:** members 1, 3 and 4 finish all 3600 frames with the same digest, each equal to its
  offline replay, 274 checks ok, 0 bad (`compare.txt`). Members 3 and 4 lost 255 and 326 datagrams to
  their scripted faults.
* **The view change:** member 2 drops out at 15 s; every survivor decides epoch 1, removed [2], at about
  18.1 s (`decisions.txt`).
* **Reaching the WSL2 member:** in WSL2's default NAT mode the laptop can't reach it, so a relay on this
  PC forwards datagrams for member 3's session address into WSL (`pe-app relay`; done, 4843 datagrams, 1389163 bytes forwarded).
  Member 3 binds 0.0.0.0:7573 in WSL; its own datagrams leave through WSL2's NAT.
* Every member binds 0.0.0.0:PORT. On Windows, a socket bound to this PC's own Tailscale address did not
  receive this machine's local traffic to that address (measured with the relay), and the first
  attempt never started.
* Addresses are masked as `<pc-tailscale>` and `<laptop-tailscale>`, and user folders as `<home>` and
  `<local path>`. `m1_two_machine.sh` is the script that built pe-app on the laptop and ran this; the
  laptop's route comes from scripts/matrix.local.sh. `linux-wsl.log` is the WSL build of the same
  commit with its tests and `m1-loopback.sh`.
