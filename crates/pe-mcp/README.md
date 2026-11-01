# pe-mcp

The engine's MCP server (design 17): the phase-2 test and driver interface (design 23.4, P2-8). An agent
drives **one peer** of a session. That peer is either one member of the in-process scenario harness
(`crates/pe-session-harness`), with the other members running scripted plans over simulated links, or a
live `pe-app` peer over UDP. Both run P2-3's `pe-session` core behind the `DrivenPeer` trait
(`src/backend.rs`), with the same tools. Entry ids are 32 bytes, 64 hex digits.

Built on `rmcp` 3.8.0 (the official Rust SDK). The full tool schemas, as the server advertises them, are in
[`schema/tools.json`](schema/tools.json). A test keeps that file current; regenerate it with
`UPDATE_SCHEMA=1 cargo test -p pe-mcp --test tools schema_file_is_current`.

## Running it

```text
pe-mcp stdio [--scopes read,interact,control] [--principal NAME]
pe-mcp serve (--pipe | --http)... [--origin ORIGIN]... [--scopes ...] [--principal NAME] [--ttl-s N] -- CMD ARGS...
pe-mcp bridge
```

* **`serve`** is the engine side. It starts the engine and its transports, issues a fresh token, and launches
  `CMD` (the harness) with `PE_MCP_TOKEN` and `PE_MCP_PIPE` and/or `PE_MCP_URL` in its environment. When
  `CMD` exits, every token is revoked and `serve` exits with `CMD`'s exit code.
* **`bridge`** is what the harness runs (the design's `pe-app mcp`). It connects to `PE_MCP_PIPE` with
  `PE_MCP_TOKEN` and relays stdio.
* **`stdio`** is an embedded test bench: the engine runs in-process and MCP is spoken on stdio, as a fixed
  principal with the given scopes and no token, because the channel belongs to the parent process.

Claude Code over the pipe (the default transport):

```json
{"mcpServers": {"pe": {"type": "stdio", "command": "pe-mcp", "args": ["bridge"],
  "env": {"PE_MCP_PIPE": "${PE_MCP_PIPE}", "PE_MCP_TOKEN": "${PE_MCP_TOKEN}"}}}}
```

Over HTTP: `{"mcpServers": {"pe": {"type": "http", "url": "${PE_MCP_URL}", "headers": {"Authorization": "Bearer ${PE_MCP_TOKEN}"}}}}`.

Both are launched as `pe-mcp serve --pipe -- claude -p "..." --mcp-config cfg.json`. The token travels
only in the environment, never on a command line or in a URL. Recorded runs: `spikes/out/mcp/`.

## Transports and their rules (design 17.1)

| Transport | Reach | Authentication |
|---|---|---|
| stdio (`pe-mcp stdio`) | The parent process only | None: a fixed principal |
| Named pipe (Windows) | Protected DACL `D:P(A;;GA;;;<current user SID>)`; first instance created with `FILE_FLAG_FIRST_PIPE_INSTANCE` (the name cannot be squatted); remote clients refused; random name | First line `PE-MCP-AUTH <token>`, answered `OK` or `ERR unauthorized` (then closed). The token is re-checked on every call |
| Unix socket (Linux) | Fresh `0700` directory under `$XDG_RUNTIME_DIR`; socket `0600`; the peer's uid (SO_PEERCRED) must equal ours | As for the pipe |
| Streamable HTTP | `127.0.0.1` and `::1` only, ephemeral port. `Host` must be `127.0.0.1:<port>`, `[::1]:<port>` or `localhost:<port>`, else 403. A request with an `Origin` header gets 403 unless the origin is on the allow-list (`--origin`, empty by default). No `Access-Control-*` header is ever sent | `Authorization: Bearer <token>` on every request, else 401 with `WWW-Authenticate: Bearer`. Stateless: no `Mcp-Session-Id`, so a session id can never stand in for a token |

**Tokens** (`src/auth.rs`): 256 random bits from the OS, as 64 hex digits. They are compared in constant time
against every live token, and `Debug` never shows them. Each token maps to one principal and a set of
scopes, and rights come from that mapping, never from tool arguments. A token expires after its idle TTL
(`--ttl-s`, default 1 h), on revocation, and when the session ends (`serve`'s harness exits). Each launch
gets a new token.

Two limits of phase 2:
* **No re-issue.** A harness that stays idle past the TTL loses the server until `serve` launches it again.
  Re-issue on resume comes with the harness adapters (`pe-harness`).
* **The token is in the harness's environment,** so every subprocess the harness starts inherits it: its
  shell tool, for example, could reach the peer with the harness's scopes. Give a harness only the scopes it
  needs (`--scopes`). The launch profiles should remove `PE_MCP_TOKEN` from tool subprocesses where the
  harness allows it.

## Scopes

| Scope | Tools |
|---|---|
| `read` | `session_info`, `session_members`, `session_sync_status`, `world_get_state`, `world_get_object`, `world_hashes`, `world_interaction_status`; all resources |
| `interact` | `world_propose_interaction`, and `world_interaction_status` (it reads only the caller's own tickets) |
| `control` | `world_step`, `scenario_load`, `scenario_fault`, `scenario_check` |

`tools/list` and `resources/list` show only what the caller's scopes allow. Calling a tool outside them
returns `forbidden`, with `details.need` naming the scope.

These are phase-2 scopes. The session kinds of design 17.2 (private, unlocked panel, inbox, decider) map
onto scope sets once panels exist (phase 4). `control` exists only for the simulator: on a live peer the
clock is real time and there is no scenario to control.

## Tools

Design 17.2 writes names with dots (`world.get_state`). The tools use underscores, because the Anthropic API
accepts only `[a-zA-Z0-9_-]` in tool names. Frames are 60 Hz sim frames. `conf` is exclusive: every frame
below it is confirmed and can never be rolled back. Frame state hashes are 16 hex digits; check hashes (BLAKE3) are 64.

| Tool | Does | Errors besides `invalid_argument`, `forbidden` and `session_faulted` |
|---|---|---|
| `session_info` | Principal, scopes, the member you drive, members, scene, clock, frame, `conf`; `faulted` if the session stopped | none |
| `world_get_state` | Frame, `conf`, state hash, a page of objects (`offset`, `limit` ≤ 256, `next_offset`), the claims in force | none |
| `world_get_object` | One object: global id, kind, position, rotation, velocities, exact-state hash, claim | `unknown_object` |
| `world_propose_interaction` | Authors an interaction as your member through the player path: stamped with the present frame, ordered soonest-wins, rolled back when earlier entries arrive. `touch` or `grab` with `impulse` (magnitude ≤ 100 N·s), or `hand` with `target_mm` (ball-pit scenes; only your own member's hand), held there (re-sent at every pose slot) until a new target or `active: false`; the call waits for the next pose slot, so it may advance time by up to one stride (simulated in the simulator, real time on a live peer, where it blocks the engine thread). Returns a `ticket` | `unknown_object`, `not_owner` (another member's hand), `rate_limited` (64 unresolved), `not_ready` (paused at the speculation limit; step and retry), `peer_crashed` |
| `world_interaction_status` | `pending`, `accepted`, `rejected` (with `reason`: `claimed` plus `by_member` and `until_frame`, `cooldown`, `gone`, …) or `void`. `final` once the stamp is confirmed; until then a rollback can still change it | `unknown_ticket` (tickets belong to their principal) |
| `world_step` | Advances virtual time by `ms` (1..=10000) or `frames` (1..=600, exact). Reports frame, `conf`, rollbacks, re-simulated frames, `paused`, your interactions whose state changed, and protocol events (at most 50) | none |
| `world_hashes` | State hash after each frame in `[from, to)` (≤ 600 frames), and the check hashes compared with other members | none |
| `session_members` | Members with evictions (`cut_seq`, `from_frame`), epoch, vote counters | none |
| `session_sync_status` | Frame, clock, `conf`, speculation and its window, `paused`, `caught_up`, check hashes matched and mismatched, rollback counters | none |
| `scenario_load` | Starts a fresh session (tickets are cleared): `members` 2..=8, `me`, `scene` (`pile` with `size` 1..=8, or `ballpit` with 1..=500 balls), `delay_ms`, `jitter_ms`, `loss_pct`, `background_touches_per_s`, `background_grabs` (background touches are grabs: only a grab claims, design D72), `window`, `seed`. The same parameters and seed give the same run. A failed load leaves the session as it was. It also clears `session_faulted` | `no_scenario_control` (live peer) |
| `scenario_fault` | `crash` a member (permanent); set a `link` (`from`, `to`, delay, jitter, loss, `one_way`); `partition` into `groups`; `heal` | `no_scenario_control` |
| `scenario_check` | The oracle, over the frames every live member has confirmed: every live member's hashes against the driven peer's, and the driven peer's against an offline replay of its final log. Reports `first_divergence` | `no_scenario_control` |

**Resources:** `primordial://world/state` (every object), `primordial://session/members` and
`primordial://session/sync`, as JSON. They need `read`.

**Errors.** A tool error is a tool result with `isError: true`. Its `structuredContent`, and its text content
as the same JSON, is `{code, message, field?, details?}`, with `code` one of `invalid_argument`,
`unknown_object`, `unknown_ticket`, `forbidden`, `not_owner`, `rate_limited`, `not_ready`, `peer_crashed`,
`no_scenario_control`, `session_faulted` and `internal`. Arguments that do not deserialize are reported the
same way, as `invalid_argument`. An unknown tool or a malformed request is a JSON-RPC error. If the sim
panics, the call returns `internal`, and every later call returns `session_faulted` until `scenario_load`.

**Ordering.** The engine thread runs calls one at a time, in the order they reach it. Calls a client sends in
parallel have no defined order between them. An agent that needs "propose, then step" waits for the first
reply.

**Peer-authored text** (design 17.2) is not yet returned by any phase-2 tool: objects have no names, and
event strings are engine-generated. The `untrusted` wrapping arrives with the first tool that returns
peer-authored text.

## Tests

`cargo test -p pe-mcp` (on this machine, through the org's `heavy.sh` lock). On Linux, see `run-linux-tests.sh`.

* `src/auth.rs`: issue, authenticate, wrong and malformed tokens, idle TTL and refresh, revocation, redaction.
* `tests/tools.rs`: every tool over a real rmcp client/server pair, with its results and its errors. Also
  scope filtering, claims rejecting contending touches, the rate limit, crash → pause → eviction → recovery,
  a partition with a slow lossy link and heal, the driven peer crashing, determinism (two engines, same
  run), engine panic → `session_faulted` → reload, resources, and the schema file.
* `tests/http.rs`: the loopback rules. Loopback-only binding, IPv6, foreign `Host` values (including a
  suffix attack and a second `Host` header), `Origin` refused and allowed, preflight, no CORS, the token
  missing, wrong, in the wrong scheme or in the URL, revocation, and scopes coming from the token.
* `tests/pipe.rs`: the handshake (good, wrong, malformed and oversized), revocation on an open connection,
  two principals on one peer, the DACL read back from the pipe (Windows), socket and directory modes
  (Unix), and `serve --pipe -- pe-mcp bridge` end to end.

## Status

* Windows: built and tested; Claude Code drove it over the pipe and over HTTP (`spikes/out/mcp/`).
* Linux, in WSL2 Ubuntu-24.04:
  * A native glibc build (`run-linux-tests.sh glibc`): all 46 tests pass at 5ff2117
    (`spikes/out/mcp/tests-linux-glibc.log`).
  * Static musl binaries built on Windows (`run-linux-tests.sh build`, then `run`): the 44 tests of 5a1568c
    pass (`spikes/out/mcp/tests-linux-wsl.log`).
  * Both runs cover the Unix socket and `serve --pipe -- pe-mcp bridge` end to end. Claude Code was not run on
    Linux.
* Not yet: resource subscriptions (`primordial://world/state` with frame, commit and rejection events),
  `schema.components`, and the logic, shader, asset and panel tools (phases 3 and 4).
