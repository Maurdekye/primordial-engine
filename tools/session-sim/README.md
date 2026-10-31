# pe-session-sim: the session simulator as a test tool

Design 23.4, P2-9. Scenarios live in files, faults are declared rather than coded, and one command
runs every scenario and compares each log with the committed one.

The simulator is still the spike's: `spikes/session-sim` (`Net`, `Sim`, `Peer`) and
`spikes/sim-core` (`WorldSim`, the protocol). It is in-process, runs on a virtual clock and is
deterministic. This tool drives it unchanged, so a scenario file reproduces the spike's
hand-written scenario bit for bit. When P2-3's `pe-session` lands, the driver moves to it, and the
scenario files and the regression command stay.

## Commands

```
scripts/regress_session_sim.sh            # build, then run every scenario and compare (the one command)

pe-session-sim regress [--only a,b] [--jobs N] [--bless] [--scenarios DIR] [--expected DIR] [--out DIR]
pe-session-sim run tools/session-sim/scenarios/phase1/c1.toml    # one scenario; log to stdout
pe-session-sim check <file.toml>...                              # load and build only
pe-session-sim list                                              # every scenario, with its description
```

`regress` runs each scenario as a child process and writes its log to `target/session-sim/<name>.log`.
It then compares the log line by line with `spikes/out/session-sim/<name>.log`. Only the four CPU
timings the logs print are masked: `resim frames N (<t> ms)`, `re-sim <t> ms per rendered sim
frame`, `(<t> ms over the window)` and `step avg <t> ms;`.

`regress` fails if any of these happens:
- a log differs, or a log is new;
- a run exits with an error, including a caught panic (`run` exits 3);
- a committed log has no scenario (an orphan; checked on full runs only);
- a check reaches frames that are not yet confirmed at every checked replica (`UNCONFIRMED`);
- anything the old `scripts/run_session_sim.sh` failed on: `MISMATCH`, `DIFFER`, `FAIL`, `PANIC`,
  `panicked`, `NOT CONSERVED`, `checks ok N bad [1-9]` or `nonfinite [1-9]`. `--bless` writes this
run's logs over the committed ones; use it for a new scenario or an intended change, and review
the diff. `--jobs` defaults to 1 (machine-load rule).

## Scenarios

| Folder | Logs | What |
|---|---|---|
| `phase1/` | 8 | c1, c2, state-frame, diverged, loss, ties, claims, backdate |
| `r14/` | 14 | R14 settled handoff: ok, abort, reject, throttle, forge (16 attacks, P2-5's dishonest-majority world W included), late, crash, chain (P2-5: A -> B -> C -> A), and ok/abort/reject/chain in reverse peer order; plus P2-5's two controls, forge-no-provenance and doublespend |
| `ballpit/` | 5 | the 500-ball pit: 60 Hz, no loss, input delay 4, 90 Hz, two hands |
| `review/` | 7 | round-2 and round-3 review repros: ackfork, lateequiv, evict, outage, badvote, liar, crashstall |
| `ownership/` | 1 | world ownership (design 12.5, consent-only): forged and rogue certificates and removals under rule A refused, a backup key acting as its owner, a second sale in one frame refused as stale, every replica equal to the replay |
| `faults/` | 7 | fault kinds the phase-1 set did not use: partition with heal, partition with eviction (and the minority side), asymmetric links, one capped link, every link capped without and with the R18 controller, crash and recovery |
| `common/` | - | includes only (not run) |

All but `faults/` are the suite of `scripts/run_session_sim.sh` (32 scenarios and P2-5's two controls,
whose logs are in `spikes/out/session-sim/p2-5-controls/`); their logs are identical to the committed
ones. The `faults/` logs are new in P2-9. All of them are identical on Windows and on Linux (WSL2,
glibc): `spikes/out/session-sim/regress-windows.txt`, `regress-linux.txt`.

A control run (`control = true`) switches a protection off to show what it prevents, so it is
expected to print failures such as NOT CONSERVED. `regress` still compares its log, but does not
count those failures. `golden = "p2-5-controls/r14-doublespend"` names a committed log other than
`<file stem>.log`.

## Validation

`check`, `run` and `regress` refuse a scenario that is well-formed but meaningless:
- an id that names no peer, or no member of the world concerned, in `peers`, `links`, `caps`,
  faults, crashes, Byzantine behaviour or reports;
- `peers` that leave out a world member;
- an empty time window, a time after the end of the run, or `recover_ms` not after `at_ms`;
- a member in two partition groups;
- `loss_pct` above 100, `hz`, `kbps`, `every`, `count` or `times` of 0;
- both `end_ms` and `end_us`;
- a plan action at a frame its member never reaches;
- an object the scene doesn't have, unless that one action sets `missing_ok = true` and says
  why, as one touch in `phase1/backdate.toml` does to reproduce the spike;
- a `check_world` over frames the run doesn't simulate;
- no checking report (`check_world` or `verdict`) at all.

## File format

A scenario is a TOML file. Unknown keys are errors. Times are milliseconds of virtual time
(`_ms`), unless a key ends in `_us`; frames are frames of the world concerned.

```toml
description = "..."                 # shown by `list`
include = ["../common/pile.toml"]   # merged underneath, in order: tables key by key, other values
                                    # (arrays too) replaced whole
end_ms = 12500                      # or end_us (not both); the run steps 1 ms at a time up to here
peers = [1, 2, 3]                   # simulator (tick) order; default: every member, ascending

[peer.2]
skew_us = 1500                      # clock skew

[world.1]
members = [1, 2, 3]
scene = { pile = { side = 8, layers = 4 } }   # or { ballpit = { balls = 500, hands = 2 } }
epoch_ms = 100                      # frame 0
hz = 60                             # dt = 1/hz; default 60
window = 120                        # SimConfig overrides: window, seed, horizon_slack, check_every,
seed = 0xA                          #   hand_predict, suspect_after
routes = [2]                        # R14 genesis route list
owners = { keys = [10], rule = "all", nonce = 1 }   # world ownership genesis (12.5); rule all, any or k

[world.1.plan]                      # actions per member; within a frame, in file order
1 = [
    { at = 60, touch = { obj = 10, impulse = [0.0, 6.0, 0.0] } },          # grab = true for a held grab
    { at = 100, every = 5, count = 12, touch = { obj = 40, impulse = [0.0, 0.3, 0.0], grab = true } },
    { at = 160, carry = { transfer = 7, obj = 40, dest = 2 } },
    { at = 134, portal = { open = false } },
    { at = 200, backdated = { obj = 50, impulse = [0.0, 6.0, 0.0], back = 20 } },
    { at = 0, hand = { obj = 500, target_mm = [0, 200, 0], active = true, ahead = 0 } },
    { ballpit_sweep = { ahead = 0 } },                                       # the scripted gesture
    { at = 60, owner = { change = "add", key = 20, signers = [10] } },       # an ownership certificate
]
3 = []                              # an empty plan clears an included one

[world.1.member.1]
throttle_ms = 250                   # background world from throttle_from_ms (design 22.3)
throttle_from_ms = 3000
adapt = true                        # R18 congestion control

[net]
seed = 7                            # link RNG (loss, jitter)
link = { delay_ms = 30, jitter_ms = 10, loss_pct = 1 }      # every link not in `links`
links = [{ from = 1, to = 2, delay_ms = 150, jitter_ms = 30 }]  # asymmetric or per pair
cap = { kbps = 64 }                 # token bucket on every link; burst_bytes 4000, max_queue_ms 500
caps = [{ from = 3, to = 1, kbps = 12 }]
batching = true                     # R18: one datagram per member, peer and tick
count_ms = [2100, 5760]             # count traffic in this window only

[sim]
reverse_order = true                # tick peers in reverse order
carry_deadline_frames = 2           # R14 carrier deadline; default 600
provenance = false                  # P2-5 control only: arrivals skip provenance checks

[[part]]                            # several runs in one log, each merged onto the rest of the file
```

### Fault rules: `[[fault]]`

Each rule is checked when a message is sent. The first rule whose conditions all hold, and that
has matches left, decides the message's fate: `drop = true`, or `delay_ms = N` on top of the link
delay. Conditions:

- `after_ms`, `before_ms`: send time, inclusive and exclusive.
- `world`.
- `from`, `to`, `involves`: a member id or a list; `involves` means sent by or to.
- `partition = [[1, 2], [3]]`: sender and receiver are in different groups. Members in no group are unaffected.
- `msg`: `entry`, `fetch`, `batch`, or an entry payload: `touch`, `hand`, `ack`, `vote`, `unvote`, `attest`, `depart`, `arrive`, `verdict`, `portal`, `ownership`.
- `stamp`, `seq`: chain entries only.
- `times = N`: the rule applies to at most N messages.

A silent member (every message to and from it lost) is `involves = 3` with `drop = true`.

### Crashes: `[[crash]]`

```toml
[[crash]]
member = 1
at_ms = 2850
recover_ms = 4000                   # optional
```

A crashed peer receives, ticks and sends nothing. It resumes with its state from before the crash.

### Byzantine behaviour: `[[byzantine]]`

Each behaviour acts before or after the step of a millisecond, where the spike's hand-written
loops did. `name` and `after` chain behaviours: a behaviour with `after = "x"` starts once
behaviour `x` has fired.

- `equivocate`: `member` signs a second version of one of its own entries (`entry = "first"` or
  `"first_touch"`) with the payload replaced by `touch` and no carried state. It sends the copy
  straight to the members in `to`, once. It fires `when = "exists"` or
  `when = { conf_above = { at = 1, frame = 200 } }`. `say` is printed when it fires.
- `fork_chain`: `member` shows the members in `to` a second chain of acks only, one every
  `every_ms`. With `hide_real = true`, they never receive its real chain from it.
- `fork_vote`: `member` re-signs its first vote with frozen head `frozen = [seq, id]` and sends
  the copy to `to`. With `hide_real = true`, its genuine votes reach nobody.
- `lie_in_votes`: `member`'s votes carry a forged frozen head (session.rs `lie_in_votes`).
- `lag_horizon`: `member` publishes ack horizons `frames` behind the truth (session.rs `horizon_lag`).
- `r14_forgeries`: the R14 attacks, each at its time: `attacks = [{ at_ms = 2000, attack = "depart_to_own_world" }, ...]`.
  The attacks are `depart_to_own_world`, `depart_to_unknown_world`, `mint_from_own_world`,
  `mint_without_departure`, `nan_record`, `squat_transfer`, `proofless_verdict`,
  `relabel_record` and `false_rejection`, and P2-5's attacks by W: `w_mint_live_id`,
  `w_mint_unborn_id`, `w_forge_earlier_hop`, `w_splice_hop`, `w_refund_setup` (watches every
  millisecond from its time until it fires), `w_refund_export`, `carry_back`, `w_resurrect` and
  `w_doublespend`. List them in the spike's order. They assume r14's worlds: A = 1, B = 2, D = 4,
  E = 5, W = 6.

Backdated touches are a plan action (`backdated`).

An `owner` plan action publishes a world-ownership certificate built on the member's present
ownership head and signed by `signers` (spike signatures: any member can sign as any key). `change` is
`add`, `leave` or `remove` (rule C only) with `key`, `set_recovery` (`key` is the owner, `recovery` its
backup key, or none to clear it), `set_rule` (`rule`) or `merge` (joins every head). An owner's
recovery key signs as the owner (consent-only ownership, OT = (a)). The world needs `owners`. A
certificate applies at its entry's frame, where the session attests it.

### Reports: `[[report]]`

Reports print after the run, in file order, in the spike's formats:

- `check_world`: replicas of `world` against each other and an offline replay of the final log, over `frames`. Options: `skip`, and `skip_evicted_at`. It prints `UNCONFIRMED` (a failure) if `frames` passes the lowest confirmed frame of the checked replicas.
- `expect`: a condition on one replica: `conf_max`, `conf_min` and/or the exact `members`. It prints OK or FAIL.
- `verdict`: honest members against an offline replay, plus the false-check count and any panic. As in the spike, it checks up to the lowest simulated frame, speculative frames included.
- `ownership`: each replica's owner view (owners, rule, heads, DISPUTED, recovery keys) against an offline replay of `world`'s final log over `frames`; every certificate in frame order, applied or refused with the reason; each fork proof, checked as a third party holding only the chain prefix would check it; and with `at = [f, ...]`, the replay's view after frame f - 1.
- `stats`.
- `membership`.
- `acceptance`.
- `traffic`.
- `events`.
- `first_entry_frame`.
- `entries`.
- `evicted`.
- `outcomes`: `mode = "wins"` or `"tally"`.
- `touches`.
- `phase1_rule`: c1 or c2 before the fix, on the old RollbackSim.
- `line`: one templated line.
- The R14 reports: `r14_forged`, `r14_jobs`, `r14_crash`, `r14_conservation`, `r14_forge_conservation`, `r14_lag`, and for `r14 chain`, `r14_chain_jobs`, `r14_chain_outcomes` and `r14_chain_conservation`.
- `ballpit`.

Templates (`say`, `line`) substitute the following:

- `{t_ms}`.
- `{conf@M}`, `{frame@M}`, `{now@M}`.
- `{member@M:X}`, `{members@M}`, `{evicted@M}`.
- `{touches@M:A}`.
- For a behaviour's entry: `{x.stamp}`, `{x.seq}`, `{x.target}`, `{x.confirmed@M}`.

## Not yet

- **P2-4's view-change scenarios.** These are twosilent, withheld, colluders, badproposer, bridge,
  join and vc256, plus badvote now recovering. They are being written as spike subcommands on
  branch `viewchange`. They need new Byzantine kinds: withholding view-change votes, and forging or
  forking proposal and approval entries. They also need a log-only scene, and possibly a join
  message. These get ported to files when P2-4 lands.
- **`pe-session` (P2-3).** The driver moves to it when it exists.
- **Not ported:** the spike's exploratory subcommands that are not in the suite (`snap`,
  `observer`, `capped`).
