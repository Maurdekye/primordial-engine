# Review response: design review of 8a403ea + b108161

Review: @design-reviewer, `review-phase1-b108161.md` (35 findings: 4 Critical, 8 High, 11 Medium,
12 Low). Section numbers below are those of the revised [design.md](design.md). Evidence tags follow the
design: **[V]** a run on this machine (log named), **[S]** read in source or `--help`, **[I]** design only.

**Summary:** 33 fixed and 1 closed by another agent's run (M5). One is fixed in part, with the remainder
put to the user: H5's tie-break is deliberately predictable, per the user's 09:32Z ruling. None were
refuted. The spike fixes are in `spikes/sim-core/src/session.rs`, `spikes/session-sim` and the rewritten
`spikes/p2p`; all their runs are reproducible with `scripts/run_session_sim.sh` and
`scripts/run-p2p.sh`.

## Base findings (8a403ea)

| ID | Sev. | Resolution | Where | How | Evidence |
|---|---|---|---|---|---|
| C1 | Critical | Fixed (design + spike) | 6.4, 10.8, 12.2, 15.2 | Admission and finality are computed from signed acks (each member's horizon and holdings), never from local arrival time. An entry stamped at or before the confirmed frame is a duplicate or a provable violation. Otherwise it is admitted and rolled back to. Commits and ACL entries become final at confirmation. Persistent stalls lead to a strict-majority view change | [V session-sim c1]: the reviewer's frame-50 input arriving at frames 169/171 now converges in all three replicas (700 frames, equal to offline replay, 50/50 checks); the "before" run reproduces the fork |
| C2 | Critical | Fixed (design + spike) | 9.3, 10.3, 10.8 | Entry id = hash of the entry; per-member hash chains. Two signed entries with one (member, seq) are a proof: both are voided and a majority vote evicts the member. Acks carry held ids, so disagreement blocks confirmation until it is resolved. A rejoin gets a new MemberId, so seqs never restart | [V session-sim c2]: detected from acks, both versions voided, member evicted within ~10 frames; honest histories identical and equal to replay. The "before" run reproduces the permanent divergence |
| H1 | High | Fixed (design) | 13.2, 15.3, D14a | The merge unit is the wasm module: commits to different modules combine already-signed artifacts with no recompilation, and the same module conflicts. Only the committer compiles, and `compile_failed` is advisory. Peers reject only on deterministic checks (validation, canonical-hash mismatch) | design; canonical hash [V M8 row] |
| H2 | High | Fixed (design) | 15.4, D14b, D14c | Seats are scoped to the conflicting modules or paths; other commits proceed. A `seat_held_by` refusal gets no seat. The 10 s runs from the front of the scope's queue. With confirmation-based finality, service time drops to about 1.1 s per seat | design |
| H3 | High | Fixed (design; majority eviction in spike) | 10.6, 10.8, 12.2, 15.2, D7 | Membership epochs: only a strict majority of the current epoch confirms or changes membership, so at most one partition side progresses. The minority pauses at the speculation limit and rejoins by state transfer; confirmed history is never discarded. Two-member sessions pause (D7) | [V session-sim c2] majority eviction; partitions themselves [I], not simulated |
| H4 | High | Fixed (design + spike) | 9 | (1) Hash comparisons happen only at confirmed frames, where every member holds the same entries, so a mismatch can only mean true nondeterminism. Missing entries are found by gap detection before confirmation, so misclassification is impossible. (2) Check hashes ride in every ack, so loss repairs itself. (3) Check frames hash the full world image plus tables | [V session-sim loss]: 52/52 checks per member pair at 10% loss (phase 1: 28 and 27 of 31); [V p2p-v2 loss10] |
| H5 | High | Fixed in part; the remainder is a user decision | 5, 8.2, 8.3, 8.5, 10.4, D37, D42, D21 | Published horizons are binding, so backdating behind one's own horizon is provable and voided. The effective frame is bounded by the lower median of the other members' arrival frames minus a 250 ms allowance. Drift control works in both directions. RTT is measured by the receiver. **Not adopted:** an unpredictable per-frame tie seed. The user's 09:32Z ruling asks for a stable, non-grindable order, and the per-frame permutation from admission-ordered ids and the epoch seed is that. Its predictability is stated in D37. Cheating is bounded, not prevented (D21) | [V session-sim backdate]: part 1 voids all backdated entries; part 2 (lagged horizons) re-stamps them +7 frames, i.e. the gain is bounded by about L minus latency |
| M1 | Medium | Fixed | 6.3, 6.5, 2.1, D2 | Re-simulation saves only keyframes (every 4 frames). Rollback depth is now latency plus the ack interval (the 45-frame check delay is gone). The budget is restated with measured re-sim cost, and D2 drops to about 500 bodies per world. Region-scoped rollback moves to phase 3 | [V session-sim]: depths 4 (60 ms) to 14 (2% loss); ball pit 2.76 ms of re-sim per frame at 500 balls |
| M2 | Medium | Fixed (design) | 5, 18.3, 18.4, D4, D18b | Render at the predicted display time by extrapolation plus smoothing; locally held objects attach to the late-latched hand; engagement uses a swept, velocity-scaled test. 90 Hz is a VR-driven per-world choice with measured cost | ball pit at 90 Hz [V]: 3.95 ms re-sim per 11.1 ms frame |
| M3 | Medium | Fixed (design + spike) | 8.4, 13.4, D38 | Plain impulses claim once and never renew; grabs renew while held; a 10-frame cooldown follows a lapsed claim; world logic never claims; agents are rate-limited | [V session-sim claims]: the reviewer's lock-out (0/600 accepted) becomes 360/600, and the infrequent toucher still gets 9/21 |
| M4 | Medium | Fixed (design) | 12.3, 17.1, 17.2, 17.4, D28a | Delegation is a human action in the engine-owned approval layer; admin tools are not MCP tools. The user key is outside the MCP-reachable path. Peer-authored strings are marked untrusted. Agent commits in shared worlds need confirmation by default. MCP is stdio over an ACL'd pipe by default; the HTTP fallback is loopback-only with Host/Origin checks and per-session rotating tokens | [S MCP spec 2025-06-18] quoted in 17.1 |
| M5 | Medium | Closed (cross-machine run by @design-reviewer) | 4.1, spike-report | The Intel i7-13620H laptop (no AVX-512) gave goldens, all four traces, rollback T1-T5 and the d4 digest bit-identical to the Ryzen desktop. The spike report records the ruled item as met for x86-64 (two CPUs) and open for ARM and macOS. Every new spike runs from one script | design-reviewer's laptop determinism run; `scripts/run_session_sim.sh`, `scripts/run-p2p.sh` |
| M6 | Medium | Fixed (design) | 13.6, D14d | Host calls charge fuel in proportion to their cost. Fuel exhaustion disables the module at that frame as rollback state (it rolls back with the sim). Reverting is an ordinary commit | design |
| M7 | Medium | Fixed (design) | 9.3, D6, D7 | No member's state or side is privileged. Two-peer nondeterminism means re-simulating from the last confirmed frame, then pausing and reporting; partitions follow strict majority | design |
| M8 | Medium | Fixed [V] | 13.2 | A module's identity is SHA-256 over the module with custom sections removed (`name`, `producers`, `target_features`); builds also remap path prefixes | [V spikes/out/wasm-canonical-hash.txt]: `physics_in_wasm.wasm` built from two paths has raw hashes `589e97…` and `051eee…`, and canonical hash `66633b43…` for both |
| L1 | Low | Fixed | 20.1, D1 | The MSVC CRT and link.exe, glibc, static musl's C and compiler-rt are listed; D1 recommends `linux-gnu` | design |
| L2 | Low | Fixed (design) | 4.3 rule 9 | A per-step finite-state assertion, plus an MXCSR check and reset on the sim thread each tick | design |
| L3 | Low | Fixed (design) | 7.4, 13.4 | Commit-time checks refuse mutable globals other than `__stack_pointer` and table mutation. Sync state is the host's component columns plus a declared arena | design |
| L4 | Low | Fixed in spike | 10.3; spikes/p2p | The UDP spike accepts an entry only from its authenticated author (it counts forgeries); production signs each entry | [V p2p-v2] "forged entries 0", with the check in code |
| L5 | Low | Fixed (labels) | 8.5 | Fairness under real skew is marked [I]. The tie distribution itself is measured | [V session-sim ties]: 41/50/59 of 150 |
| L6 | Low | Fixed | 5, 10.4; spikes | Clocks are monotonic with a slewed offset; the spikes use `Instant` (`session::now_us`) | code |
| L7 | Low | Fixed | 8.3, 10.3, D37 | MemberIds are assigned in admission order by the membership log, and `tie_rank` hashes all 32 bits | code (`tie_rank`) |
| L8 | Low | Fixed (design) | 13.2, D9, D11 | Vendored dependencies plus `Cargo.lock` in the world repo; no build scripts or proc macros in logic crates (except the SDK's); mirroring the toolchain is a user question | design |
| L9 | Low | Fixed | 7.7, 18.6, 19.3, 24 | D14 split into D14a-d and D18 into D18a-b; D24 merged into D1; D2 restated with re-sim cost. Added decisions: R3 traffic (D39), finality (D36), identity and ties (D37), claims (D38), agent trust (D25a, D28a). The IK scope question is in D19; WebP is dropped from v1; 18.6 is tagged [I] | design |

## R13 findings (b108161)

| ID | Sev. | Resolution | Where | How | Evidence |
|---|---|---|---|---|---|
| R13-C1 | Critical | Fixed (design) | 15.6, 21.3, 21.1 | The SCM rejects harness-configuration paths (`.claude/`, `.mcp.json`, `CLAUDE.md`, `AGENTS.md`, `.codex/`, `.gemini/`, `GEMINI.md`, `.agents/`, …), with alias-proof name matching. Panel workspaces are materialized (never a checkout) with those paths stripped and ancestors checked. Launch profiles use `--restricted` (owner) or `--bare --restricted --tools ""` (restricted sessions), plus `--strict-mcp-config`; never Codex `--dangerously-bypass-hook-trust` | [S `claude --help` 2.1.292, `codex --help` 0.160.0; harness docs] |
| R13-C2 | Critical | Fixed (design) | 21.6, 21.7, D30, D30a, D30b | Inbound panel and hub traffic goes to a contained inbox session per connection or correspondent, never into the owner's private session. Forwarding is per message, with auto-forward off. "Ask agent" is a decider session capped at the owner's ceiling (default text only). The worm risk is analysed | design |
| R13-H1 | High | Fixed (design) | 21.5, D27c, D27g | The wrong Codex claims are corrected: `exec` has no `-a`, and `read-only` does not confine reads. Codex is not unlockable until gate 9b proves containment; it would then run under a separate OS identity with a guest-only key | [S `codex exec --help`] |
| R13-H2 | High | Fixed (design) | 21.5, 21.9 gate 9a | The Claude Code restricted session: `--bare --restricted --tools ""`, `CLAUDE_CONFIG_DIR` holding only a guest key, `disableAllHooks`, `--permission-prompts none`, under a separate OS identity. It fails closed on the `system/init` tool list | [S `claude --help`; docs] |
| R13-H3 | High | Fixed (design) | 21.3, 21.5, D27b | One session, one MCP token and one key per guest. Rights bind to the token, never to an agent-supplied id. Guests never see each other's turns | design |
| R13-M1 | Medium | Fixed (design) | 21.5, D27d, D27e, D34 | Guest-only API keys with provider-side hard limits; per-turn tool-call and token caps at the adapter. `--max-budget-usd` is quoted as print-mode only. D34 now asks the user whether sharing a subscription is allowed and offers no "none" option | [S `claude --help`] |
| R13-M2 | Medium | Fixed (design) | 21.5 (revocation), 21.9 gate 9g | Revocation is a seven-step sequence: stop input, invalidate the token synchronously, kill the process tree, cancel pending items, close streams, publish a signed `GuestRevoked` at a confirmed frame, delete files | design |
| R13-M3 | Medium | Fixed (design) | 10.7 item 1, 11 item 6, D35a, D35b | Codes become nameplate + password + check word. The nameplate is a DHT rendezvous; the password is used only inside a PAKE with the host, and nothing derived from it is published. Online guesses are capped at 10 per code. The arithmetic is in 10.7 | design |
| R13-L1 | Low | Fixed (design) | 21.5 (input handling) | Structured, role-separated inputs; one principal per session, so there is no in-band wrapper to forge. Engine events never arrive as user text. Controls, bidi, zero-width and tag characters are stripped, and marker-like lines escaped | design |
| R13-L2 | Low | Fixed (design) | D35 | Relay metadata exposure (who talks to whom, IPs, timing, selective dropping) is stated; relays are discovered through DHT records and peer exchange and chosen per peer | design |
| R13-L3 | Low | Fixed | D27a-h, D30, D34 | D27 is split into eight decisions; D30 states the click-accept consequence; D34's "none" alternative is replaced by an explicit question | design |

## Found while fixing (not in the review)

The new spikes found and fixed these defects in the first draft of the fixes:
* **Rollback clamped to the confirmed frame.** Wrong: confirmation means every member *holds* the entries,
  not that this member already simulated them. A member whose sim ran ahead diverged [V session-sim c1,
  first run].
* **Check hashes sent before a pending re-simulation finished** produced false "nondeterminism"
  [V session-sim diverged, first run].
* **Majority rule for the arrival bound.** With 3 members, one lossy receiver could re-stamp an honest
  entry. The bound now uses the lower median of the other members' arrivals.
* **Claims without a cooldown.** Non-renewing impulses moved the monopoly to whoever touches every frame;
  the 10-frame cooldown fixes it [V session-sim claims].
