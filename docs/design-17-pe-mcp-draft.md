# Draft for design 17 and 23.4 (P2-8, pe-mcp): for @engine-architect

What P2-8 built and measured, as edits to the design text. Tags follow the design's convention. `[V pe-mcp]`
means verified by `crates/pe-mcp`'s tests and the recorded Claude Code runs in `spikes/out/mcp/` (branch
`p2/mcp`), on Windows, plus the test suite on Linux (WSL2, static musl).

## 17.1 Where it lives: replace the last two bullets ("Library", "Spike coverage")

* **Library:** `rmcp` 3.8.0 (official Rust SDK): server, macros, schemars, stdio and Streamable HTTP
  [V pe-mcp].
* **Built:** `crates/pe-mcp` replaces `spikes/mcp-stub`. Claude Code 2.1.292 drove a peer through a scripted
  scenario over the pipe and over HTTP, and both runs passed with identical results [V pe-mcp].
* **Pipe and socket specifics** [V pe-mcp; on Linux, the tests only]:
  * Windows: a protected DACL with one ACE, full access for the current user's SID
    (`D:P(A;;GA;;;<sid>)`, read back from the pipe in a test). The first instance is created with
    `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a second server on the same name fails. Remote clients are refused,
    and the name is random.
  * Unix: the socket lives in a fresh `0700` directory under `$XDG_RUNTIME_DIR`, the socket file is `0600`,
    and the peer's uid (SO_PEERCRED) must equal the engine's.
  * Token: the bridge's first line is `PE-MCP-AUTH <token>`, and the engine answers `OK` or
    `ERR unauthorized` and closes. The token is re-checked on every call, so revocation and expiry act on
    open connections.
  * The token reaches the harness only through its environment (`PE_MCP_TOKEN`), which harness MCP configs
    can reference (`${PE_MCP_TOKEN}`).
* **HTTP specifics** [V pe-mcp]: the server runs stateless (no `Mcp-Session-Id`), so authority comes from
  each request's token alone. Tests cover a foreign `Host`, a suffix-attack `Host`, a second `Host`
  header, a refused and an allowed `Origin`, preflight, the absence of CORS headers, a token that is
  missing, wrong, sent in the wrong scheme or sent in the URL, and revocation.

## 17.2 Tool and resource catalog: add before the table

* **Tool names use underscores:** `world_get_state`, not `world.get_state`. The Anthropic API accepts only
  `^[a-zA-Z0-9_-]{1,64}$` in tool names [I: the API's documented pattern, not re-checked here; Claude Code
  passed underscore names through unchanged in both runs, V pe-mcp]. The table keeps the dotted names as the
  catalogue's spelling.
* **Phase 2 implements** [V pe-mcp]: `session_info`, `world_get_state`, `world_get_object`,
  `world_propose_interaction`, `world_interaction_status`, `world_step`, `world_hashes`,
  `session_members`, `session_sync_status`, `scenario_load`, `scenario_fault`, `scenario_check`, and the
  resources `primordial://world/state`, `primordial://session/members` and `primordial://session/sync`.
  The full schemas are in `crates/pe-mcp/schema/tools.json`.
* **Phase-2 scopes** (the token decides them): `read`, `interact` (propose) and `control` (`world_step` and
  `scenario_*`, simulator only: a live peer's clock is real time). `tools/list` shows only what the
  token's scopes allow. The session kinds of the table below map onto scope sets when panels arrive.
* **Errors:** a tool error carries `{code, message, field?, details?}`. The codes are `invalid_argument`,
  `unknown_object`, `unknown_ticket`, `forbidden`, `rate_limited`, `not_ready`, `peer_crashed`,
  `no_scenario_control`, `session_faulted` and `internal`. An unknown tool or a malformed request is a
  JSON-RPC error.
* **Interaction tickets:** proposing returns a ticket. Its status is `pending`, `accepted`, `rejected`
  (with reason, such as `claimed` with its holder and end frame) or `void`, plus `final` once the stamp is
  confirmed. Before that, a rollback can still change it. At most 64 unresolved interactions per principal
  (`rate_limited`) is the first per-session rate limit of 17.4.
* **Ordering:** the peer runs MCP calls one at a time, in arrival order. Calls a client sends in parallel
  have no defined order between them [I].

## 17.3 Deterministic forks: add

* The phase-2 simulator backend already shows the property forks rely on. The same `scenario_load`
  parameters and seed, with the same calls, give identical hashes: tested across two engines, and seen in
  two Claude Code runs over different transports that agreed on every reported number [V pe-mcp].

## 17.5 AI-friendliness: add

* Tool schemas carry no non-standard JSON Schema formats. schemars' `uint32` and `uint64` made Claude Code's
  validator warn on every tool, so the server strips them and keeps the `minimum` bounds [V pe-mcp].

## 23.4 P2-8: status line

* Built on branch `p2/mcp`: 44 tests pass on Windows and on Linux (WSL2, static musl binaries built on
  Windows), and Claude Code drove a peer over the pipe and over HTTP on Windows. Not yet: Claude Code on
  Linux, a native glibc build (it waits for the Linux host), resource subscriptions, and
  `schema.components`. **The P2-3 seam** is `crates/pe-mcp/src/backend.rs` (`DrivenPeer`, plus
  `ScenarioControl` for the simulator only). `pe-session` needs, per driven member: author an interaction
  at the present frame and broadcast it, an entry's verdict and whether it is confirmed, frame and `conf`,
  object state, hashes and check hashes, membership with evictions, sync counters, and the protocol events
  log.
