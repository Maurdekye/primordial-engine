# pe-mcp: Claude Code drives a peer (P2-8 acceptance run), 2026-10-30

Claude Code 2.1.292 (model `claude-sonnet-5-5`, headless `-p`) ran the scripted scenario in
[`scenario.md`](scenario.md) against `pe-mcp` on Windows, once over each transport. The pipe run used the
binary built from commit 50a39a8. The HTTP run used 50a39a8 plus the then-uncommitted schema clean-up
described below, which was committed unchanged in 5a1568c. The engine launched the harness, as design 17.1 describes:

```text
pe-mcp serve --pipe --principal claude-code -- claude -p "<scenario.md>" --mcp-config cfg-pipe.json \
    --strict-mcp-config --setting-sources project --allowedTools "mcp__pe__*" --output-format stream-json --verbose --model sonnet
pe-mcp serve --http --principal claude-code -- claude ... --mcp-config cfg-http.json ...
```

`--strict-mcp-config --setting-sources project`, run from an empty directory, load no user settings, hooks
or MCP servers: the init record lists only the `pe` server and Claude Code's two built-in plugins. The
token reached Claude Code only through `PE_MCP_TOKEN` in its environment, and the configs refer to it as
`${PE_MCP_TOKEN}`. Neither transcript contains it.

The two `.jsonl` files are trimmed from Claude Code's stream-json output, for publication: they keep the
`pe` tool list, every assistant text and tool call, every tool result, and the final outcome (turns, wall
time, cost, the agent's report). Session ids, message ids, local paths, token usage, rate-limit events,
thinking blocks and Claude Code's own environment listing were removed.

| | pipe (`run-pipe.jsonl`) | HTTP (`run-http.jsonl`) |
|---|---|---|
| Transport | `\\.\pipe\primordial-mcp-…` via `pe-mcp bridge` | `http://127.0.0.1:53375/mcp` (IPv6 too), bearer token |
| Result | PASS, 28 turns, 81 s wall, USD 0.256 | PASS, 28 turns, 72 s wall, USD 0.224 |
| Tool calls | 26 `pe` calls (11 of the 12 tools) | the same 26, in the same order |
| Highest object (step 3) | object 28, a sphere, y 1.2494 | the same |
| After the touch (step 4) | accepted, final; object 28 at [1.08, 48.63, -2.23] | the same |
| Crash member 4 (step 5) | peer paused in the 2nd 1000 ms step (speculation 125 > window 120); member 4 evicted in the 4th (t = 6000 ms, epoch 1, cut seq 62) | the same |
| Slow lossy link + touch (step 6) | rejected, final: `claimed` by member 2 | the same (until frame 365) |
| `scenario_check` (step 7) | replicas_equal true, replay_equal true (live 1, 2, 3) | the same |
| Deliberate error (step 8) | `unknown_object` | the same |
| Final `conf` | 461 | 461 |

All of the above was measured. The two runs went through different transports in separate Claude sessions.
Every number they share is equal, because the same `scenario_load` seed gives the same session (design 17.3).

The run-1 stderr (`run-pipe.stderr.txt`) shows Claude Code's schema validator warning about schemars'
`uint32`/`uint64` formats, which are not JSON Schema formats. The server now strips them (`clean_schema` in
`src/server.rs`, guarded by a test), and the HTTP run, made after that change, shows no warnings.

`scenario_fault` was not called with `partition` here; `tests/tools.rs` covers it. `world_hashes` is the
one tool the agent did not call; the tests cover it.

[`tests-windows.log`](tests-windows.log) holds the full `cargo test -p pe-mcp` run on Windows (debug, low
priority), at 5ff2117: 46 tests. [`tests-linux-wsl.log`](tests-linux-wsl.log) holds the same 44 tests on Linux: WSL2 Ubuntu-24.04,
kernel 6.6.87.2, static musl binaries built on Windows by `crates/pe-mcp/run-linux-tests.sh`. They pass,
including the Unix-socket tests and `serve --pipe -- pe-mcp bridge` end to end.
[`tests-linux-glibc.log`](tests-linux-glibc.log) holds the native glibc build in WSL: rustc 1.99.1, gcc
13.3, from a copy of HEAD 5ff2117. All 46 pass. (`tests-linux-wsl.log`, the static musl run, is older: 44 tests
at 5a1568c.)
