You are testing the Primordial Engine's MCP server. Use only the `pe` MCP tools. Work through these steps in order, one tool call at a time, and check each result before you go on.

1. Call session_info. Note the member you drive and your scopes.
2. Call scenario_load with: members 4, me 1, scene "pile", size 4, delay_ms 30, jitter_ms 5, loss_pct 2, background_touches_per_s 3, seed 2026.
3. Advance 1000 ms with world_step. Read world_get_state with limit 32 and pick the object with the highest position y.
4. Touch that object with impulse [0, 20, 0]. Advance 1000 ms. Its interaction must be accepted and final: confirm with world_interaction_status, and read the object's new position with world_get_object.
5. Crash member 4 with scenario_fault. Then advance in 1000 ms steps, calling session_sync_status and session_members after each, until member 4 shows as evicted (at most 6 steps). Report the step at which the peer paused (if it did) and when member 4 was evicted.
6. Set the link between members 1 and 2 to delay_ms 200, loss_pct 10. Touch any box with impulse [3, 5, 0], advance 2000 ms, and report that interaction's final state.
7. Call scenario_check. replicas_equal and replay_equal must both be true.
8. Make one deliberate mistake: touch object 999. Report the error code you get.

Finish with a short report: a table with one row per step (step, tools called, result, pass/fail), then one final line exactly of the form
RESULT: <PASS or FAIL> evicted_at_ms=<n> final_conf=<n> check=<replicas_equal>/<replay_equal> error_code=<code>
