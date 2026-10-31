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
| `r14/` | 15 | R14 settled handoff: ok, abort, reject, throttle, forge (16 attacks, P2-5's dishonest-majority world W included), late, crash, chain (P2-5: A -> B -> C -> A), chain-epoch (a provenance chain across a view change), and ok/abort/reject/chain in reverse peer order; plus P2-5's two controls, forge-no-provenance and doublespend |
| `ballpit/` | 5 | the 500-ball pit: 60 Hz, no loss, input delay 4, 90 Hz, two hands |
| `review/` | 7 | round-2 and round-3 review repros: ackfork, lateequiv, evict, outage, badvote, liar, crashstall |
| `p2-4/` | 11 | P2-4's view change (design 10.8): twosilent, twosilent5 (both quorum rules), withheld, colluders, badproposer, bridge (both rules), join, refwithhold, vcscale-64, vcscale-256; and handowner (design 8.4, review HO-1) |
| `d80/` | 5 | the D80 admin layer: kickwar, kickpartition, lastadmin, adminclose, joinrules |
| `p2-4-review/` | 34 | P2-4's review repros, each run with its fix switched off and then as shipped: fork9, jointiming, bogusprecommit, futureack (round 2); ghostkick, selfkickhead, ackgap, latejoin, capchurn, deepfork, cutbackdate, joinattests, slowadmin, kickbackdate, lagprecommit (round 3); strikeframe, aheadshield, capstale, lagchurn, votechurn, adminvoid, joinsquat, joinskip (round 4); joinwedge, slowref, prevotefork (round 5, and round 7's PF-1 control); adminfork (round 6); capfake, fakeadmin, adminwedge (round 7, each a halt with its fix off); the reports fakemix (round 6); the checks aheadlink and fakeref; and the control onesided (VC-1) |
| `ownership/` | 1 | world ownership (design 12.5, consent-only): forged and rogue certificates and removals under rule A refused, a backup key acting as its owner, a second sale in one frame refused as stale, every replica equal to the replay |
| `faults/` | 7 | fault kinds the phase-1 set did not use: partition with heal, partition with eviction (and the minority side), asymmetric links, one capped link, every link capped without and with the R18 controller, crash and recovery |
| `common/` | - | includes only (not run) |
| `spike-only.txt` | - | committed logs not mirrored as scenario files yet (see below) |
| `not-scenarios.txt` | - | committed logs of the spike's property tests (`randsched`), never scenarios (see below) |

All but `faults/` are the suite of `scripts/run_session_sim.sh` (82 scenarios and the three controls of
P2-5 and P2-4, whose logs are in `spikes/out/session-sim/p2-5-controls/` and `p2-4-controls/`); their
logs are identical to the committed ones. The `faults/` logs are new in P2-9. All of them are
identical on Windows and on Linux (WSL2, glibc): `spikes/out/session-sim/regress-windows.txt`,
`regress-linux.txt`.

A committed log without a scenario file yet is listed in `scenarios/spike-only.txt`: a full
`regress` lists it as spike-only instead of failing on it as an orphan. The list only shrinks: a
listed name that has a scenario file, or no committed log, is an error. It is empty now: every
committed log has its scenario file. (`handowner` was held for a while: the spike's check covered
6 frames past the lowest confirmed one, which `check_world` flags as UNCONFIRMED. P2-4's round 4
fixed the spike.)

`scenarios/not-scenarios.txt` lists the committed logs that are not scenarios at all: the spike's
property tests (`randsched`, P2-4 round 5's randomized schedule generator, which checks its own
invariants over 12 seeds). The spike suite runs and checks them; `regress` neither mirrors them nor
counts them as orphans, and reports each as "not a scenario". A listed name with a scenario file,
or with no committed log, is an error.

The spike's review repros run once with their fix switched off (`fixes_off`), printing an
informational line (`before_fix`), and once as shipped. A run with its fix switched off may stop
at an assertion: that is what it shows, so it is not counted as a failure. Five of them,
`joinattests`, `lagprecommit`, `adminvoid`, `joinsquat` and `fakeref`, leave out a touch the spike
plans past the end of the run (frame 600 of 593, or 900 of 833): it never fires, and the tool
refuses an action its member never reaches.

A control run (`control = true`) switches a protection off to show what it prevents, so it is
expected to print failures such as NOT CONSERVED. `regress` still compares its log, but does not
count those failures. It does require the run to show at least one failure marker, or the text in
`control_marker`, so that `--bless` cannot quietly turn a control into a pass. `golden = "p2-5-controls/r14-doublespend"` names a committed log other than
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
- no checking report (`check_world`, `verdict`, `before_fix`, `check` or `handowner`) at all,
  unless the part is kept for a later one (`keep`);
- a `handowner` report naming a run that no part up to its own keeps;
- an unknown fix in `fixes_off`, quorum rule, join-rule target or relation, or `check` case.

## File format

A scenario is a TOML file. Unknown keys are errors. Times are milliseconds of virtual time
(`_ms`), unless a key ends in `_us`; frames are frames of the world concerned.

```toml
description = "..."                 # shown by `list`
include = ["../common/pile.toml"]   # merged underneath, in order: tables key by key, other values
                                    # (arrays too) replaced whole
end_ms = 12500                      # or end_us (not both); the run steps 1 ms at a time up to here
peers = [1, 2, 3]                   # simulator (tick) order; default: every member and joiner,
                                    # ascending
intro = ["  first proposer {proposer@1}"]   # templated lines printed before the run
quiet_panics = true                 # print nothing for a panic (the spike's run_quiet)
keep = "forged"                     # keep this part's run for a later report (`handowner`)

[peer.2]
skew_us = 1500                      # clock skew

[world.1]
members = [1, 2, 3]                 # or members_range = [1, 256]
joiners = [4]                       # follow the world without being members (they may ask to join)
follow_joiners = false              # the members do not follow them (default: they do)
scene = { pile = { side = 8, layers = 4 } }   # or { ballpit = { balls = 500, hands = 2 } }, or "empty"
epoch_ms = 100                      # frame 0
hz = 60                             # dt = 1/hz; default 60
window = 120                        # SimConfig overrides: window, seed, horizon_slack, check_every,
seed = 0xA                          #   hand_predict, suspect_after
routes = [2]                        # R14 genesis route list
quorum = "majority"                 # D80 quorum rule; default "bft"
ack_heads = false                   # acks without heads (design 25.5.2); default true
ack_every_frames = 30               # every replica acks this often
fixes_off = ["VC2"]                 # review fixes switched off: VC2..VC5, VCR1..VCR9, VCR12, HO1,
                                    #   VC3R1..VC3R4, VC3R6, VC3R7, LAGKEEP, VC4R1..VC4R6, VC3R3S, VC6R1, VC6R3,
                                    #   VC6R4, PF1
guests = { role = 1, admins = [1] } # role 1 with no permissions for everyone but the admins
owners = [7]
join_rules = { rules = [{ target = "admin", rel = "friend", role = 1 }], fallback = 1 }
ownership = { keys = [10], rule = "all", nonce = 1 }   # world ownership genesis (12.5); rule all, any or k

[world.1.plan]                      # actions per member; within a frame, in file order
1 = [
    { at = 60, touch = { obj = 10, impulse = [0.0, 6.0, 0.0] } },          # grab = true for a held grab
    { at = 100, every = 5, count = 12, touch = { obj = 40, impulse = [0.0, 0.3, 0.0], grab = true } },
    { at = 160, carry = { transfer = 7, obj = 40, dest = 2 } },
    { at = 134, portal = { open = false } },
    { at = 200, backdated = { obj = 50, impulse = [0.0, 6.0, 0.0], back = 20 } },
    { at = 0, hand = { obj = 500, target_mm = [0, 200, 0], active = true, ahead = 0 } },
    { ballpit_sweep = { ahead = 0 } },                                       # the scripted gesture
    { hand_path = { obj = 100, from_frame = 60, to_frame = 240, from_x = -0.6, to_x = 0.6, y_mm = 400, z = 0.0 } },
    { at = 120, kick = { target = 2 } },                                     # D80 admin entries
    { at = 240, set_role = { member = 2, role = 0 } },
    { at = 125, define_role = { role = 1, perms = 0 } },
    { at = 60, owner = { change = "add", key = 20, signers = [10] } },       # an ownership certificate
]
3 = []                              # an empty plan clears an included one

[world.1.member.1]
throttle_ms = 250                   # background world from throttle_from_ms (design 22.3)
throttle_from_ms = 3000
adapt = true                        # R18 congestion control
ack_every_ms = 1000                 # this member acks this often
withhold_vc = true                  # Byzantine: casts no view-change entry
byz_ally = true                     # Byzantine: backs every vote and proposal, applies no decision
byz_no_precommit = true             # Byzantine: prevotes but never precommits
quiet_until_member = true           # a joiner that authors no ack before it is admitted

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

[[join]]                            # P2-4 join request, sent once at its time
at_ms = 2000
member = 5
to = [1, 2, 3, 9]
from = 9                            # sent by 9 in 5's name (default: by the member itself)
sig = 0                             # a forged signature (default: the member's genuine one)
attests = [{ rel = "friend", with = 1 }]   # rel: friend, confidant, invited; world default 1
attests_to = [1, 2]                 # only these get the attestations
log_from = 1                        # then member 1 sends the joiner every entry it holds

[[part]]                            # several runs in one log, each merged onto the rest of the file
```

### Fault rules: `[[fault]]`

Each rule is checked when a message is sent. The first rule whose conditions all hold, and that
has matches left, decides the message's fate: `drop = true`, `delay_ms = N` on top of the link
delay, or `deliver = true` (so no later rule matches it). Conditions:

- `after_ms`, `before_ms`: send time, inclusive and exclusive.
- `world`.
- `from`, `to`, `involves`: a member id or a list; `involves` means sent by or to.
- `partition = [[1, 2], [3]]`: sender and receiver are in different groups. Members in no group are unaffected.
- `msg`: `entry`, `fetch`, `batch`, `join` (a join request), or an entry payload: `touch`, `hand`, `ack`, `vote`, `propose`, `prevote`, `precommit`, `kick`, `setrole`, `definerole`, `attest`, `depart`, `arrive`, `verdict`, `portal`, `provenance`, `ownership`.
- `stamp`, `seq`: chain entries only.
- `author`: chain entries authored by this member (or list), whoever sends them.
- `round`: `propose`, `prevote` or `precommit` entries of this round; `nil = true` (or `false`):
  `prevote` or `precommit` entries for nil (or for a value).
- `times = N`: the rule applies to at most N messages.

With `delay_grows = true` (and `after_ms`), the delay grows with the send time t to
t - after_ms + delay_ms: a link growing slower (p2-4-slowref).

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
  straight to the members in `to`, once. It fires `when = "exists"`,
  `when = { conf_above = { at = 1, frame = 200 } }` or `when = { at_ms = 2600 }`. `say` is printed
  when it fires. With `hide_from = [3]` (and `hide_until_ms`), the copy never reaches those members.
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

- `hide_refs`: from `from_ms`, `member`'s acks reference no more of anyone's chain
  (`freeze_others`); at `until_ms` it references them again and, with `ally_until = true`, stops
  backing everything (`byz_ally` off).
- `forge_proposal`: `member`, which must be the first proposer, shows `to` a reset in place of its
  first proposal; `to` never receives its genuine proposals.
- `fork_branch`: review VC-2's fork (session-sim p24r.rs `fork_run`): `member` forks its chain at
  `at_frame`, branch A to `a_to`, a longer branch B to `b_to`; silent `live_ms` after the fork;
  `backdate = true` adds a backdated touch on A, `detect_ms` makes `a_to` find the fork late.
- `precommit`: from `from_ms`, `member` signs a precommit for no proposal with `boundary` (or `ahead`
  frames past its clock) and sends it to `to`: once, or with `every_epoch = true` once per epoch
  (value `value ^ epoch`). `while_member_at = 1`: only while member 1 still has it as a member.
  `polka = ["0xDEADBEEF00000001"]`: the prevote ids it names, as hex strings (TOML integers stop
  at i64).
- `fake_ack_epoch`: from `from_ms`, `member`'s acks claim `epoch`, or `ahead` epochs past its own.
- `ghost_vote`: at `at_ms`, `member` votes against `target` for a kick `kick` that does not exist.
- `self_kick_vote`: once `member` holds a kick against itself, it votes on it naming `frozen`.
- `backdated_kick`: once `member` reaches `at_frame`, it signs a kick of `target` backdated `back`
  frames.
- `rotate_entries`: from `from_ms`, `member` sends each of `to` only its own entries whose sequence
  number modulo the length of `to` is that member's index in it.
- `churn_vote`: from `from_ms` to `until_ms`, in every epoch, `member` votes against `target`
  (whom everyone hears), forcing a reset.
- `void_admin`: review VC3R-3: `member` signs a horizon-void SetRole and proposes a change listing it.
- `fake_refs`: from `from_ms`, `member`'s acks claim `n` more of `of`'s chain than it holds.
- `go_silent`: from `from_ms`, once `member` holds an entry of its own of kind `holds_own`
  (`prevote` or `nonnil_prevote`), every message it sends is lost; it still receives and ticks
  (the spike's crash after a prevote).
- `mix_refs`: review VC5R-2: from `from_ms`, every `every_ms` `member` alternates between acks
  naming a fake id at its current seq of `of`'s chain and acks naming a stale real head, re-frozen
  (`freeze_others`) at a fake turn at least `refresh_ms` after the last.
- `admin_fork`: review VC5R-1: at `at_ms`, admin `member` forks one seq, a SetRole giving `target`
  role `role` to `a_to` and an Attest in its place to `b_to` (never the SetRole), proposes a change
  listing the SetRole, and prevotes nil to `prevote_to`.
- `equivocate_prevote`: review VC4R-1: from `from_ms`, once round-0 proposer `member` holds its
  round-0 prevote and proposal, it sends the proposal's value to `value_to` and nil to `nil_to` as
  two versions of that prevote; its genuine round-0 prevotes reach nobody.
- `noop_setrole`: review VC6R-4: from `from_ms` (while a member at `while_member_at`), every
  `every_ms` `member` signs a SetRole giving itself `role`, sent to `to`.
- `admin_wedge`: review VC6R-3 (p24r4.rs `adminwedge`): at `at_ms` `member` sends `a_to` a SetRole
  giving itself `role` and keeps a fork of it (an Attest at the same seq); once every one of
  `others` holds its own precommit, it crashes (everything it sends is lost), and it sends the
  fork to `part` at `part_ms` and to `others` at `others_ms`, through any fault. `part` must be 4,
  or 3 if 4 is the round-0 proposer, as in the spike.
- `hand_forgery`: review HO-1: `member` sends `to` a grab of another member's `hand` at `grab_ms`,
  a departure of it at `carry_ms`, and a touch carrying a state for it at `plant_ms`.

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

  `verdict`, `membership`, `view_changes`, `admin` and `before_fix` take `honest = [1, 2, 3]`,
  `honest = { members_at = 3 }` (the membership member 3 holds at the end of the run), or
  `honest = { members_at = 2, among = [1, 2, 3] }` (those of `among` in it).
- `view_changes`: every decision of each member: epoch, frame, round, removed.
- `admitted`: each member's admissions and `joiner`'s touches.
- `admin`: per member, the admin entries' outcomes (`outcomes = true`) and/or its members, roles,
  admins and close frame (`roles = true`).
- `confs`: each member's confirmed and simulated frame on one line.
- `before_fix`: the line a run with its fix switched off ends with (`label`, `fix`).
- `fork_info`: where `fork_branch` forked and how long branch B is.
- `check`: the spike's one-off checks, ported verbatim: `bridge`, `kickwar`, `kickpartition`,
  `lastadmin`, `adminclose`, `joinrules`, `vcscale`, `selfkickhead`, `capchurn`, `joinattests`,
  `slowadmin`, `kickbackdate`, `lagprecommit`, `aheadlink`, `onesided`, `removed`, `strikeframe`,
  `strikeframe_total` (over kept runs), `aheadshield`, `fakeref`, `capstale`, `lagchurn`,
  `votechurn` (it samples a confirmation lag before each step, every 100 ms), `votechurn_off`,
  `adminvoid`, `joinskip`, `joinwedge`, `slowref`, `fakemix`, `adminfork`, `prevotefork`,
  `prevotefork_off`, `prevotefork_pf`, `capfake`, `fakeadmin`, `adminwedge` (the round-7 cases report
  liveness: every listed member's frame within a window of its confirmed frame). The fields each reads are listed in scenario.rs (`Report::Check`).
- `handowner`: review HO-1 over three kept runs (`forged`, `clean`, `fixoff`).
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
- The R14 reports: `r14_forged`, `r14_jobs`, `r14_crash`, `r14_conservation`, `r14_forge_conservation`, `r14_lag`, for `r14 chain`, `r14_chain_jobs`, `r14_chain_outcomes` and `r14_chain_conservation`, and `r14_chain_epoch`.
- `ballpit`.

Templates (`say`, `line`, `intro`) substitute the following; `{{` and `}}` are literal braces:

- `{t_ms}`.
- `{conf@M}`, `{frame@M}`, `{now@M}`.
- `{member@M:X}`, `{members@M}` (a set), `{member_list@M}` (a list), `{evicted@M}`.
- `{touches@M:A}`.
- `{proposer@M}` (the first proposer), `{role@M:X}`, `{removed@M:X}` (when M removed X, in s,
  as an Option), `{removed_s@M:X}` (the same as "2.50 s" or "never"), `{resets@M}`, `{strikes@M}`,
  `{votes@M}` (the targets of M's own votes).
- For a behaviour's entry: `{x.stamp}`, `{x.seq}`, `{x.target}`, `{x.confirmed@M}`.

## Not yet

- **`pe-session` (P2-3).** The driver moves to it when it exists.
- **Not ported:** the spike's exploratory subcommands that are not in the suite (`snap`,
  `observer`, `capped`).
