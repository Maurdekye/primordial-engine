You are an AI agent driving one peer (member 1) of a live Primordial Engine session over MCP: four peer processes exchanging signed UDP datagrams in real time, in a ball pit with one kinematic hand per member. Use only the `pe` MCP tools, one call at a time, and check each result before going on. `world_step` here does not advance a simulation: on a live peer it WAITS the given milliseconds of real time while the peer keeps running.

1. Call session_info. Note the backend, the member you drive, and the members.
2. Call session_sync_status. If `conf` is 0 the session has not started yet: call world_step with ms 1000 and check again, at most 10 times.
3. Call world_get_state (page through it with offset if needed) and find the objects of kind "hand". Find your own hand: proposing a hand move on another member's hand fails with not_owner; your hand is the one that does not.
4. Move your hand: world_propose_interaction with action "hand", target_mm [0, 400, 0]. world_step 1000 ms, then confirm with world_interaction_status that it is accepted, and read the hand with world_get_object: its position y should be about 0.4 m (within 0.02).
5. The target is held until you change or release it: world_step 3000 ms and read the hand again. Report whether y is still about 0.4.
6. Move it to target_mm [300, 300, 0], world_step 1000 ms, and read it (expect x about 0.3, y about 0.3). Then release it: world_propose_interaction with action "hand", target_mm [300, 300, 0], active false.
7. Call session_members. Member 4 is scripted to drop out at 30 s of session time. Call world_step with ms 3000 and then session_members, repeating (at most 15 times) until member 4 shows an eviction. Report the session time (session_info time_ms) at which you first saw it evicted, and the decision (epoch, removed).
8. Call scenario_check. On a live peer it must fail: report the error code.

Finish with a table (step, tools called, result, pass/fail) and one final line exactly of the form
RESULT: <PASS or FAIL> hand_y=<y after step 4> held_y=<y after step 5> moved=<x,y after step 6> evicted_seen_at_ms=<n> decision_epoch=<n> removed=<list> final_conf=<n> error_code=<code>
