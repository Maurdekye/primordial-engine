# pe-app

The headless M1 peer (design 23.4, P2-10). It runs one member of a session: the session protocol over
signed UDP, with pe-mcp attached, so an AI agent can drive it. Noise encryption and NAT traversal come
in phase 3.

## Layout

| File | What |
|---|---|
| `src/core.rs` | `PeerCore`, the sans-IO peer: one member's replica of one world and the protocol around it. It takes `receive(from, Msg, now)` and `tick(now)`, and gives `outbox()` (explicit recipients, class Live / Control / Repair), `events()`, `author(payload)` and `next_deadline()`. Times are on the session's agreed timeline. This is the seam agreed with @engine-architect; pe-session (P2-3, @world-chunks) takes it over |
| `src/wire.rs` | Datagrams: `"PE" v1, session id, sender, items, Ed25519 signature`. Every entry carries its author's own signature, so entries relayed in repair stay verifiable. An entry already held (same id) is not re-verified |
| `src/config.rs` | `session.json` (session id, scene, length, every member's id, UDP address and public key) and `member-<id>.key` (that member's secret key) |
| `src/node.rs` | The IO: socket, clock sync, faults, background touches, results, and pe-mcp's `DrivenPeer` |
| `src/main.rs` | The CLI |

## Use

```text
pe-app init --members 4 --host 127.0.0.1 [--base-port 7101] [--frames 1800] [--scene pile|ballpit] [--size 4] --out DIR
pe-app init --addrs 192.168.0.10:7101,192.168.0.11:7101,... --out DIR        # members on several machines
pe-app run --session DIR/session.json --key DIR/member-1.key --me 1 [--out OUT] [-v]
           [--faults FILE] [--crash-at-ms T] [--touches-per-s R] [--bind ADDR]
           [--mcp-pipe|--mcp-http [--scopes read,interact,control] [--principal NAME] -- HARNESS ARGS...]
pe-app compare OUT
```

* **The session starts** once every member is up. The lowest id leads: each member estimates its clock
  offset to the leader from ping round trips (it keeps the lowest-RTT sample), then reports Ready. The
  leader sends an epoch 1 s ahead.
* **A member stops** once every frame of the session is confirmed. It keeps acking for 3 s, so the others
  confirm too, then writes `OUT/hashes-<id>.txt` and `OUT/summary-<id>.json`. The summary holds the digest,
  an offline replay of its final log, membership, traffic and events.
* **`compare OUT` succeeds** when every summary's digest is equal, each equals its replay, every hash line
  matches, and no check hash failed.
* **Faults** apply to this member's outgoing datagrams, at session times:
  `[{"at_ms": 2000, "loss_pct": 15, "delay_ms": 40, "jitter_ms": 15}, {"at_ms": 9000, "to": 1, ...}]`.
  `--crash-at-ms` exits the process at once, without a word: a member dropping out.
* **Binding:** by default the member binds its session address, so a loopback session never listens on
  the network. Use `--bind 0.0.0.0:PORT` behind NAT.
* **With pe-mcp:** `--mcp-pipe` / `--mcp-http` start the pe-mcp transports for this peer, issue a token
  and launch the harness, as `pe-mcp serve` does.
  * On a live peer, `world_step` waits the given time while the peer runs, and the scenario tools return
    `no_scenario_control`.
  * When the session ends, pe-app exits, and the harness with it.

## Scripts

* `m1-loopback.sh EXE OUT [PORT]`: the M1 scenario on one machine. Four members, loss and delay on
  members 2 and 3 (worse on 2→1 from 9 s to 14 s), and member 4 dropping out at 6 s. Then `compare`.
* `linux.sh`: from Windows, copies HEAD into WSL and, there, does a native glibc build and runs the tests
  and `m1-loopback.sh`.

## Evidence (`spikes/out/m1/`)

| Run | Result |
|---|---|
| `loopback-windows/` | Windows, release. Member 4 removed about 3.3 s after its drop-out. Members 1–3 identical over 1200 frames, replay-equal, 0 failed checks |
| `agent-windows/` | Claude Code (Sonnet 5.5) drove member 1 of a live 4-peer session through pe-mcp over the pipe. Its touches were accepted and final, it saw member 4's eviction, and `scenario_check` was refused. Members 1–3 identical over 12000 frames |
| `linux-wsl.log` | WSL2 Ubuntu-24.04, native glibc. pe-app and pe-mcp tests pass, and `m1-loopback.sh` gives ALL EQUAL |
| `tests-windows.log` | pe-app and pe-mcp tests on Windows |

## Limits

* One world, static membership: crash removal works, joins are ignored.
* Faults are injected on the sender. Real loss between machines comes on top.
* The node ticks every millisecond; `next_deadline` is there for a sleeping IO.
* Repair traffic is not deferred on a long queue. Congestion control (R18) has a slot (`set_link`) but
  stays at level 0.
* Keys: a member's secret key is a plain hex file. Its folder's ACL is the only protection.
* Windows Firewall: on this PC, inbound UDP from WSL is blocked unless a rule allows it on the WSL adapter.
