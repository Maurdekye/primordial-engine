# Agent run on current main (M1 evidence gap 3)

Measured on main aa2680b (pe-app with continuous clock sync and the P2-10 merge); binaries and Claude
Code version in `provenance.txt`. Windows loopback, release build. The same run as
`../agent-windows-rebase/` (0d792e8): four pe-app peers in the ball pit (12000 frames), member 4
scripted to drop out at 30 s, members 2-4 touching twice a second, and Claude Code (Sonnet 5.5, headless,
`pe` tools only) driving member 1 over the named pipe through `pe-mcp bridge`, with
`agent-scenario.md` as the prompt:

```bash
pe-app run --session s/session.json --key s/member-1.key --me 1 --out out -v --mcp-pipe --principal claude-code -- \
  claude -p "$(cat agent-scenario.md)" --mcp-config cfg-pipe.json --strict-mcp-config --setting-sources project \
  --allowedTools "mcp__pe__*" --output-format stream-json --verbose --model sonnet
```

| | Result |
|---|---|
| Agent | PASS, 23 turns, USD 0.137; 21 `pe` calls (3 world_propose_interaction, 5 world_step, scenario_check) |
| Hand | object 50 moved to y = 0.4, still 0.4 after 3 s (the held target), then to (0.3, 0.3) and released |
| View change | member 4 removed at about 33 s (epoch 1, removed [4]) at all three survivors; the agent saw it at 38 s |
| scenario_check | refused on the live peer: `no_scenario_control` |
| Histories | members 1-3 ALL EQUAL over 12000 frames, each equal to its offline replay, 864 checks ok, 0 bad |

`agent.jsonl` is trimmed from Claude Code's stream-json output for publication, as in
`../agent-windows-rebase/`: the `pe` tool list, every assistant text and tool call, every tool result
and the outcome. Session ids, message ids, local paths, token usage, rate-limit events, thinking blocks
and the environment listing are removed. The token reached Claude Code only through `PE_MCP_TOKEN` in
its environment. `cfg-pipe.json` shows the config with pe-mcp's path relative; the run used the build's
absolute path.
