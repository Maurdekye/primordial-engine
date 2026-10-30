All eight steps ran. The only oddity is step 6, where the touch was rejected because member 2 had claimed the box.

| Step | Tools called | Result | Pass/fail |
|---|---|---|---|
| 1 | session_info | Driving member 1, scopes read/interact/control, 32 objects | pass |
| 2 | scenario_load | Loaded with the requested parameters (pile 4x4x2, seed 2026) | pass |
| 3 | world_step, world_get_state | Frame 59, conf 53. Highest y was object 28 (sphere, y≈1.2494) | pass |
| 4 | world_propose_interaction, world_step, world_interaction_status, world_get_object | Ticket 1 on object 28 was accepted and final. Object 28 ended at position [1.08, 48.63, -2.23] | pass |
| 5 | scenario_fault, world_step ×4, session_sync_status ×4, session_members ×4 | Peer paused at crash step 2 (4000 ms; speculation 125 against a window of 120). Member 4 evicted at crash step 4 (6000 ms, epoch 1, cut seq 62) | pass |
| 6 | scenario_fault, world_propose_interaction, world_step | Link 1↔2 set to 200 ms and 10% loss. Touch on box 0 (ticket 2) ended rejected and final, reason "claimed" by member 2 | pass* |
| 7 | scenario_check | replicas_equal true, replay_equal true (live members 1, 2, 3) | pass |
| 8 | world_propose_interaction (object 999) | Error code `unknown_object` ("no live object 999") | pass |

*The rejection is a normal outcome of the claim rules, and the step only asked for the final state. I didn't retry on another box.

RESULT: PASS evicted_at_ms=6000 final_conf=461 check=true/true error_code=unknown_object
