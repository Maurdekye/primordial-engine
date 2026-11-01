You are an AI agent driving one peer (member 1) of a live Primordial Engine session over MCP: four peer processes exchanging signed UDP datagrams in real time. Use only the `pe` MCP tools, one call at a time, and check each result before going on. `world_step` here does not advance a simulation: on a live peer it WAITS the given milliseconds of real time while the peer keeps running.

1. Call session_info. Note the backend, the member you drive, and the members.
2. Call session_sync_status. If `conf` is 0 the session has not started yet: call world_step with ms 1000 and check again, at most 10 times.
3. Call world_get_state with limit 32 and pick the object with the highest position y. Touch it with world_propose_interaction (action "touch", impulse [0, 20, 0]). Then world_step 1500 ms and confirm with world_interaction_status that it is accepted and final. Read the object with world_get_object.
4. Call session_members. Member 4 is scripted to drop out of the session at 30 s of session time. Call world_step with ms 3000 and then session_members, repeating (at most 15 times) until member 4 shows an eviction. Report the session time (session_info time_ms) at which you first saw it evicted, and the decision (epoch, removed) from session_members.
5. Touch any box with impulse [3, 5, 0], world_step 1500 ms, and report that interaction's final state.
6. Call scenario_check. On a live peer it must fail: report the error code.

Finish with a table (step, tools called, result, pass/fail) and one final line exactly of the form
RESULT: <PASS or FAIL> evicted_seen_at_ms=<n> decision_epoch=<n> removed=<list> final_conf=<n> error_code=<code>
