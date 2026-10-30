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

---

# Round 2: re-verification of 3d0f98a

Review: @design-reviewer, `review-round2/review-round2.md`. The review found 32 new findings (4 Critical,
8 High, 14 Medium, 6 Low) and left 10 round-1 findings partial. The response is on branch `round2-fixes`,
on top of the perturbation-history study (442286f).

Evidence tags are as above. "Fixed in design" means the design now states the rule, but no spike exercises
it [I].

**Summary:**
* **All 4 Criticals are fixed, with spike evidence.** The reviewer's four repros now run as session-sim
  scenarios, and `scripts/run_session_sim.sh` fails on any fork, panic or false check. It passes:
  * 25 scenarios, every replica bit-identical to its offline replay, 0 false checks;
  * a `r14 forge` scenario, in which every proofless relay does nothing.
* **Of 8 Highs:**
  * fixed with evidence: NEW-5, NEW-7;
  * fixed in design: NEW-8, NEW-9, NEW-10, and D12-1/D12-2 (D12 is now a user decision);
  * fixed in part: NEW-6.
* **Mediums and Lows:** 15 fixed or fixed in design, 5 deferred with reasons, 3 fixed in part. None
  refuted.
* **The user's 14:31Z rule, "an author never sees its own interaction corrected",** settled the
  re-stamping questions (H5, NEW-1, NEW-2, viz-2): nothing is ever re-stamped.

**What the fixes cost:**
* **Finality lag doubled:** confirmation takes 8.5 frames (142 ms) instead of 4.2 on 50 ms links, because
  it waits for one more ack round (D54).
* **Ball-pit traffic grew 28% per message** (acks became larger chain entries). Per-tick batching more
  than recovers it: 11.9 kB/s for both directions together, against 14.7 in 3d0f98a.

## Round-1 findings left partial

| ID | Status | How | Evidence |
|---|---|---|---|
| C1 | **Fixed** | Every decision input is a chain entry (acks included); confirmation uses stable data only; validity is chain-local (6.4, 8.2, 10.8) | [V session-sim ackfork, outage: honest members equal, equal to replay; c1: 116-frame rollback, no fork] |
| C2 | **Fixed** | An equivocation after confirmation keeps the stable version. Eviction uses a cut from the voters' frozen heads. A rollback below the confirmed frame is an assertion failure (9.3, D55) | [V lateequiv, evict, c2: no panic, equal] |
| H4 | **Fixed** | Check hashes travel inside ack entries, so a two-faced hash is an equivocation. The image is made canonical before hashing (NEW-7). The suite fails on any bad check | [V every scenario 0 bad; r14-throttle 18/0 and 15/0] |
| H5 | **Fixed in part, by user rule** | No re-stamping. Backdating is bounded chain-locally by binding horizons and a 2-frame horizon slack, to about 4 frames. A member that lags its horizon only stalls confirmation. Residual: a cheater can still pick, within about 4 frames, a frame where `tie_rank` favours it (8.2, D42) | [V backdate parts 1 and 2] |
| L2 | **Deferred** | Release builds should check that the state is finite at check frames (one pass over body states). This is not yet a design rule. Reason: it only matters once ARM joins the matrix (rule 8); it goes in with the CI matrix | — |
| L3 | **Decision** | Arena-only logic snapshots are unenforceable. D57 recommends whole-linear-memory snapshots with dirty pages | [I] |
| R13-C1 | **Fixed in design** | Toolchain config reserved and invoked by pinned path (NEW-8); workspace re-scanned before each launch (NEW-10); allowlist plus absolute paths (NEW-19); exact ancestor files (NEW-18) | 15.6, 21.10 |
| R13-C2 | **Fixed in design** | The private session sends nothing without payload approval, auto-forward is removed, and forwards go only into restricted turns (NEW-9) | 21.10 |
| R13-H2 | **Fixed in design** | Exact `@builtin` plugin allowlist per pinned version, hook events fatal, canary files in gate 9a (NEW-11) | 21.10 |
| R13-L3 | **Fixed in part** | D27d now offers a real alternative (a guest brings their own harness). With auto-forward removed, D30's "a careless accept is cheap" holds again. D34's wording is unchanged | 24 |

## New findings (Part 2)

| ID | Sev. | Status | How | Evidence |
|---|---|---|---|---|
| NEW-1 | Critical | **Fixed** | Acks, votes and check hashes are chain entries. Confirmation is the minimum over members of the horizon in that member's stable prefix + 1. Late reports are gone, and no decision depends on per-receiver data (10.8, D54) | [V session-sim ackfork: 9 evicted, its touch void at both members, equal; outage: equal after a 6.5 s one-way outage] |
| NEW-2 | Critical | **Fixed** | Effective frame = stamp, so eviction cannot move anything. After a proof, only non-stable entries are withdrawn. The cut is the common prefix of frozen heads, and the evicted member stops counting after its last valid horizon. `mark_dirty` below the confirmed frame is a fatal assertion (9.3, D55) | [V lateequiv: stable version kept, 9 cut at seq 103 from frame 206; evict: touch stays at 60; no panic in any scenario] |
| NEW-3 | Critical | **Fixed** | A Verdict needs a certificate: a strict majority of B's members' signatures over exactly that outcome. It may be relayed by anyone (22.6, D46) | [V r14 forge: proofless commit and the carrier's false rejection both `RejectedNoProof`; conserved] |
| NEW-4 | Critical | **Fixed** | An Arrive needs a transfer id derived from its Depart and a certificate from a strict majority of A's members; the Depart names its destination (22.6) | [V r14 forge: mint without a departure `RejectedNoProof`] |
| NEW-5 | High | **Fixed** | Transfer id = hash(source world, Depart id). Proofless Arrives do not consume the id | [V r14 forge: squat `RejectedNoProof`, real arrival accepted] |
| NEW-6 | High | **Fixed in part** | Self-certifying relays let any member of both worlds finish a transfer; refunds need B's certified "not received". With nobody in both worlds the object waits, which is inherent (two generals). Doc defect corrected (22.10) | [I: the spike's carrier is still the only relayer; a crashed member is removed by the general view change, also I] |
| NEW-7 | High | **Fixed** | The check hash covers the image after a restore-and-reserialize round trip (9.1) | [V r14-throttle: 18 ok / 0 bad (A), 15 / 0 (B); was 42 / 24] |
| NEW-8 | High | **Fixed in design** | `.cargo`, `rust-toolchain` and `rust-toolchain.toml` are reserved. cargo and rustc run by pinned absolute path with `RUSTUP_TOOLCHAIN`, `CARGO_HOME` and `--config` overrides, under the restricted identity. Rule 7 and 13.2 no longer reference a repo toolchain file (15.6) | [I] |
| NEW-9 | High | **Fixed in design** | 21.10 | [I] |
| NEW-10 | High | **Fixed in design** | 21.10 | [I] |
| NEW-11 | Medium | **Fixed in design** | 21.10: exact plugin names from the reviewer's init listing | [I; plugin names V reviewer] |
| NEW-12 | Medium | **Deferred** | Reason: the throttle metric and the relay latency (+75 ms) need a throttle-aware measurement. Acks still go out unthrottled, so lag stays insensitive. A throttled member can author state-free entries; carried-state touches need a caught-up sim. Queued with the R14 implementation phase | — |
| NEW-13 | Medium | **Deferred** | Reason: a per-client CPU total and a GPU budget for portals belong with R16's measured per-region costs (world-chunks, §25) and D49's limits. Worlds are independent Box3D instances and can step on separate threads [I] | — |
| NEW-14 | Medium | **Fixed in design** | One decider session per request (21.10) | [I] |
| NEW-15 | Medium | **Fixed in part** | Budgets are now stated per member count and latency band: the 90 Hz and 100 ms rows are flagged over budget; batching is measured; p99 ticks are in 6.8 | [V ballpit-90hz, observer-100, r18 capped, snap; I for 3-4 members] |
| NEW-16 | Medium | **Fixed in design** | Non-contacts go to the manual inbox, with a concurrency cap. Linux containment needs a uid or a mount namespace plus Landlock. macOS containment is a gate (21.10) | [I] |
| NEW-17 | Medium | **Decision** | D57 | [I] |
| NEW-18 | Medium | **Fixed in design** | 21.10 | [I] |
| NEW-19 | Medium | **Fixed in design** | Allowlist, absolute paths, `NoDefaultCurrentDirectoryInExePath=1` (15.6) | [I] |
| NEW-20 | Medium | **Fixed in design** | Feature allowlist, gated on the observed tool list (21.10) | [I] |
| NEW-21 | Low | **Deferred** | Reason: ACL, budget, deadline and component-policy paths need the real ACL tables. Arrival placement should be at the portal, with record validation (finite, bounded velocity) and global-id references. All of this is for the R14 implementation phase | — |
| NEW-22 | Low | **Deferred** | Reason: D48's data path. Foreign modules are pinned by canonical hash and their data treated as untrusted input; to be written into D48 with R15's module trust | — |
| NEW-23 | Low | **Fixed** | 6.4 now cites the current lag (8.5 frames); 9.2's "0 mismatches" now holds for every scenario and is enforced by the script; 8.2's finality is enforced in code | [V] |
| NEW-24 | Low | **Fixed in design** | Job object at creation; revocation applies "after F" (21.10) | [I] |
| NEW-25 | Low | **Fixed in part** | Colliding normalized sibling names are invalid (15.6); D27d gains the bring-your-own-harness alternative. Per-key guest caps are deferred to the guest-admission work | [I] |

## Iteration benchmark and D12 (Part 3)

| ID | Sev. | Status | How | Evidence |
|---|---|---|---|---|
| D12-1 | High | **Fixed in design** | The per-OS split is withdrawn: a session cannot mix runtimes. D12 is now a user decision: wasmi everywhere now, Cranelift everywhere once patched and fuel-tested, runtime-independent fuel, or a user exception. Rule 7 is restated as "same engine version and sim-relevant config, verified by the matrix" (4.3, 13.3) | [V reviewer fuelprobe] |
| D12-2 | High | **Fixed in design** | States that wasmtime's `helpers.c` compiles on Linux and macOS whatever the features, with the two ways out (a `[patch]`, or a `no_std` custom platform) | [V reviewer] |
| D12-3 | Medium | **Deferred** | Reason: `audit_native.py` must also flag `asm!` and `global_asm!`. This is a script change, queued with the CI matrix; 13.3 states the gap | — |
| D12-4 | Medium | **Fixed** | 2.1 now has separate rows for local build and swap, and for edit-to-every-peer (about 1.05 s); load numbers added | [V components] |
| D12-5 | Medium | **Fixed in design** | The swap compiles off-thread during the activation lead and is cached by canonical hash; Winch first is the alternative (13.3) | [V reviewer timings] |
| D12-6 | Medium | **Fixed in design** | Budgets use the `rel-fast` ratios (wasmi 9.8x, Cranelift 2.1x) and the annotation-free compile time (13.3) | [V reviewer] |
| D12-7 | Low | **Deferred** | Reason: an ARM NaN probe, asserting cross-runtime equality, and stating in 13.6 that epoch interruption is nondeterministic all go with the CI determinism matrix | — |

## Ball-pit matrix findings (@ballpit-viz, 2996d39)

| ID | Status | How | Evidence |
|---|---|---|---|
| viz-1 | **Fixed** | Check hashes are compared only for confirmed frames the member has finished simulating, never with a re-simulation pending (9.2) | [V 0 bad checks in every scenario] |
| viz-2 | **Fixed, by user rule** | Nothing is re-stamped: a late receiver pays with a deeper rollback, and the author's history never changes (8.2). Adaptive input delay stays opt-in only | [V r18 capped: author rollbacks 0 and own entries moved 0, in every run] |
| viz-3 | **Fixed** | Ack late-lists are gone; acks carry only heads | [V] |
| viz-4 | **Fixed** | Per-tick batching, with acks on frame ticks, cut the uplink from 77 to 65 kbit/s. The congestion controller keeps 32-64 kbit/s caps real-time with queues under 100 ms (10.9) | [V r18 capped] |

## User inputs folded in during round 2

* **"An author never sees its own interaction corrected" (14:31Z):** 8.2, D42, 10.9.
* **R18 recovery (14:29Z):** 10.9, D53, D56. Acceptance: the capped cells recover. Measured at 64, 48 and
  32 kbit/s with the adaptive transport: 0 stalled frames, 0 drops, at most a 94 ms queue, and back to real
  time at once [V].
* **vdW notes (14:49Z, 14:40Z):** with vdW on, a lossy session's outcome depends on network timing, while
  every peer stays deterministic and equal. "Contended" should mean causally connected (7.3).

## Found while fixing (round 2)

* **Stability must count references to one's own chain.** The first build skipped them, and confirmation
  never advanced.
* **A hole in a chain hides that chain's acks.** Under 10% loss, gap detection from acks alone deadlocked;
  an entry that arrives past a hole must reveal the hole itself.
* **An eviction's cut may need versions fetched after the votes,** so the eviction is retried when they
  arrive.
* **Batching saves little unless acks share the frame's datagram.**

## Round 2 follow-ups (branch `round2-followups`)

These are the deferred items that needed no new subsystem, done while round 3 runs. The branch sits on
top of R15 (d948898).

| ID | Status | How | Evidence |
|---|---|---|---|
| L2 | **Fixed** | Every build checks at each check frame that all body states are finite (one pass), counts failures and names the first bad object. The suite fails on any (4.3 rule 9) | [V session-sim: 0 in every scenario] |
| D12-3 | **Fixed** | `audit_native.py` flags Rust files with `asm!`, `global_asm!` or `naked_asm!` (a text scan, inspected by hand). 20.2 lists the packages: wasmtime's unwinder, `cranelift-native`, the icache-coherence crate, `sha2`, and `rustix` on Linux. wasmi's `libm` uses `sqrtss`/`sqrtsd` and runtime-detected `vfmadd`, all exactly rounded, so the path taken cannot change a result | [V `spikes/out/audit-inline-asm.md`; S libm source] |
| D12-7 | **Fixed in part** | 13.6 now states that only fuel may stop a module at a frame: epoch interruption is timer-driven, so it is only a backstop outside the session. The ARM NaN probe and a cross-runtime equality assertion still go with the CI matrix | [I] |
| NEW-12 | **Fixed** | The throttle scenario now measures what the reviewer asked for. 22.3 separates state-free entries (stamped at the present, no simulation needed) from entries that carry state (one catch-up burst first). The harness forces that burst. Results, throttled against unthrottled: identical relay timeline (2,766 / 2,964 / 3,197 / 3,354 ms), with the Verdict authored at frame 185 while the throttled sim stood at 175; the throttled member's touch kept its stamp (frame 260); its check hashes arrived 10.2 frames after their check frame against 10.0; and A's digest is identical | [V session-sim r14 ok, r14 throttle] |
| NEW-21 | **Fixed in part** | B validates every arriving record: all values finite, rotation norm within 1%, speed at most 200 m/s and spin at most 2,000 rad/s, otherwise `RejectedInvalid` (22.6). ACL, budget, deadline, component policy, several relayers, placement at the portal and global-id references remain for the R14 implementation | [V r14 forge: a certified record with a NaN position is `RejectedInvalid`; conserved; ALL EQUAL] |
| NEW-22 | **Fixed in design** | 22.8 and D48: foreign modules are pinned by canonical hash; carried bytes are untrusted input, size-capped by the engine before logic sees them and validated by B's module; a record that does not parse stays inert | [I] |
| NEW-13 | **Still deferred** | It needs R16's measured per-region costs, which merge with `chunks/r16` | — |
| NEW-23 | **Fixed** (remaining stale numbers) | 15.2 cited 70-80 ms confirmation, and 6.3 and 22.3 cited a 4-5-frame lag; both now use the round-2 figure of 8.5 frames (about 140 ms). The confirmation term in the priorities list, 2.1, 15.2 and 15.4 is now 0.14 s. The suite runs 25 scenarios, not 26 | [V session-sim r14] |

---

# Round 3: `round2-fixes` (ed347ac), R15 (d948898) and the follow-ups (3b02a68)

Review: @design-reviewer, `review-round3.md` (29 new findings: 3 Critical, 5 High, 13 Medium, 8 Low; of
round 2, NEW-6 open and 17 partial). The response is on branch `round3-fixes`, in four commits:
* f060e30: the Criticals (accepted by the reviewer, `review-round3-f060e30.md`);
* cecda4b: crash removal, a crashed carrier, and the residuals of the f060e30 review;
* 9b0be49: recovery below 20 kbit/s, and demotion;
* the final commit: R15-3, the Mediums and Lows, and three decisions written as options for the user.

**Summary:**
* **All 3 Criticals and all 5 Highs are fixed**, with spike evidence except R15-3, a design fix.
* **The suite now has 30 scenarios.** Every replica equals its offline replay, every transferred object is
  conserved, with no panic and 0 false checks. The script now also fails on a lost or duplicated object;
  it would have missed one during this work.
* **Three findings are user decisions, written as options with a recommendation:** D72 (R3-3), D73 (R3-10)
  and D57 (L3, NEW-17).
* **What colluders and multiple failures can still do: stall, never fork.** The proposer-based view change
  that would remove several members at once is a named prerequisite for phase 3 (23.2), as the coordinator
  decided.

## Part 1: the confirmation model

| ID | Sev. | Status | How | Evidence |
|---|---|---|---|---|
| R3-1 | Critical | **Fixed** (accepted) | A vote counts only if its frozen head extends every reference its voter published before the vote; otherwise it is a provable lie, and void. References after a voter's own vote no longer count. Votes are used only once they are stable among the remaining members, so a voter that forks its vote can stall but not split anyone. Stability never shrinks (9.3, D55) | [V badvote: no panic, honest members equal and equal to the replay, a safe stall; liar: lie detected, 9 evicted with a cut that keeps the confirmed touch] |
| R3-2 | High | **Fixed** (one member at a time) | A member silent for 180 frames is voted out with R3-1's eviction rules; a silence vote is withdrawn if the member comes back before removal is decided; two-member worlds pause (D7) (10.8). Several at once: the proposer-based view change, a phase-3 prerequisite (23.2) | [V crashstall: 72 paused frames, then normal lag, equal; r14 crash] |
| R3-3 | Medium | **Decision D72** | Options: claims for grabs only; no refusal of any physical interaction (recommended); or keep claims and state the exception | [I] |
| NEW-2, C2 | — | **Closed** | By R3-1 | [V badvote, liar] |

## Part 2: R14

| ID | Sev. | Status | How | Evidence |
|---|---|---|---|---|
| R3-4 | Critical | **Fixed** (accepted) | Each world trades only with the worlds on its route list, part of its genesis config. A departure elsewhere is `RejectedNoRoute` and the object stays (22.6, 22.7, D46) | [V r14 forge] |
| R3-5 | Critical | **Fixed** (accepted, given D46's assumption) | Arrivals only from routed worlds; an already-live global id is refused. D46 now states the honest-majority assumption, and offers birth-world binding plus a provenance chain for untrusted partners [I]. Binding a global id's prefix to the source world was not adopted, because objects travel on (A to B to C) | [V r14 forge] |
| R3-6 | High | **Fixed** | Unknown destinations refused. A late arrival is refused, and the certified refusal refunds the object; deadlines are capped at 3,600 destination frames | [V r14 forge, r14 late] |
| R3-7 / NEW-6 | High | **Fixed** for worlds of three or more | Every member of both worlds relays a pending transfer; each world removes a crashed carrier. Two-member worlds pause (D7); with nobody else in both worlds the object waits (two generals) | [V r14 crash: conserved, all equal] |
| R3-8 | Medium | **Fixed** | The signed fact covers every field of the record, and the deadline | [V r14 forge: a changed shape kind is `RejectedNoProof`] |
| R3-8b | Medium | **Fixed in design** | Certificates keyed to a membership epoch, each change certified by the previous epoch; part of the phase-3 view change | [I] |
| R14 lows | Low | **Fixed** | 22.10 no longer claims reverse-order digests equal forward for abort; the keyless `member_sig` is labelled a model | [V] |
| f060e30 residual 1 | Medium | **Fixed in design** | D46 states the honest-majority assumption | — |
| f060e30 residual 2 | Low | **Fixed, differently** | Deadlines are capped. Rejected arrivals are forgotten after their deadline and 3,600 frames; resolved escrows after 3,600 frames. **Accepted transfer ids are kept**: pruning them, as proposed, would let a late replay of an accepted transfer be certified "rejected" and refund an object that did arrive | [V r14 late; I for the attack] |

## Part 3: R18 and the cost of the fixes

| ID | Sev. | Status | How | Evidence |
|---|---|---|---|---|
| R3-9 | High | **Fixed** | Repairs yield to the queue: retry backoff from 60 ms to 2 s, replies of at most 16 entries, no answer into a congested uplink, datagrams split at 1,200 bytes. The controller gains two coarser levels, on shared ticks. Demotion is built: a member whose horizon trails the present by more than W/2 for 3 s is voted out. The two-member floor is about 12 kbit/s each way (10.9) | [V r18: 16 kbit/s, a 64-to-16 step and 4 members on a shared 64 kbit/s uplink stay real time with 0 stalls; 12 kbit/s recovers in 19 s; a slow third member is demoted; 8 kbit/s with two members stays collapsed, below the floor] |
| R3-10 | Medium-High | **Decision D73** | Options: pack k targets per entry (recommended); keep thinning; or never thin the author below stride 2 | [I] |
| R3-11 | Medium | **Fixed in design** | 7.6 states the ack sizes and idle upload per member count, over budget from 3 members; compact acks (seqs plus one digest) and small region logs are the fix [I] | [V reviewer] |
| R3-12 | Medium | **Fixed** (stated) | 2.1 now says the 150 ms target became 200 ms in ed347ac, and that lag exceeds it under jitter and loss (p95 333 ms with 2% loss); the snapshot-memory row states the growth | [V reviewer] |
| R3-13 | Low-Medium | **Fixed** | 2.1 compares like for like: 13.6 against 14.7 kB/s, 7% less; ack bytes grew 53% | [V reviewer] |
| R3-14 | Low | **Fixed** | 25 scenarios corrected in 3b02a68 (30 now); 10.9 states that two hands and the direct queue signal are not covered | — |

## Part 4a: the perturbation-history study

| ID | Status | How | Evidence |
|---|---|---|---|
| PH-1 | **Fixed** | Rerun on the current protocol: at 26 frames of lag, Every(4) holds 10.5 MB and base-only costs +79% modelled CPU, not +17%. 6.3, 6.8, D50 and spike report 5f updated | [V snap, round 3] |
| PH-2 | **Fixed in design** | At 100 ms one-way the Balanced default needs its adaptive depth: rollbacks reach 24 frames, past the window (6.8, D50) | [V reviewer] |
| PH-3 | **Fixed in design** | Narrowed to fixed-offset deltas; general compression halves images, so the knob may compress cold images [I] | [V reviewer] |
| PH-4 | **Fixed** | 6.9 and 5f say the tests show the API's limit, not that per-island snapshots cannot work | — |
| PH-5 | **Fixed** | Explained: with no re-stamping, an unthinned stream leaves no confirmed frame without an entry, so hold and dead reckoning end in the same state; with thinning the rule still shapes history (6.10) | [V observer: identical end hashes] |
| PH-6 | **Fixed** | Measured: after a change of motion, dead reckoning's mean error stays lower, but its p95 and maximum match or slightly exceed holding's (6.10) | [V observer, round 3] |
| PH-7 | **Fixed** | The CPU columns are labelled a model [C], with the per-operation costs and their provenance stated; the re-simulation table matches its committed log | — |
| PH-8 | **Fixed** | Logs regenerated by the script; the stale `*-window.log` files removed | [V] |

## Part 4b: R13 items

| ID | Status | How |
|---|---|---|
| NEW-8 | **Fixed in design** | `--config` also empties `build.rustflags`, `target.*.rustflags`, `[env]` and every `credential-provider`; workspaces live under an engine-owned root outside the home directory, and a build fails closed on any ancestor `.cargo/config` (15.6) [I] |
| NEW-9 | **Fixed** | The stale "off by default" auto-forward text is gone from 21.6 and D30a; D30 says what a careless accept costs |
| NEW-10 | **Fixed in design** | The engine refuses writes outside the allowlist, not only to reserved paths; restricted sessions have no shell tool, and the workspace is re-scanned after every turn (21.10) [I] |
| NEW-14 | **Fixed** | The stale "one decider" line now says one per request |
| NEW-18 | **Fixed in design** | The ancestor files are listed per harness, confirmed by gate 9a's canaries [I] |
| NEW-19 | **Fixed in design** | A restricted session's PATH holds only engine-owned directories [I] |
| NEW-20 | **Fixed in design** | Gate 9b turns off every default-on feature except the adapter's needs; the nine the reviewer found are named |
| R3-15 | **Fixed in design** | Each decider is scoped to one request by a token; at most two run per panel; queued requests time out to deny [I] |

## Part 5: R15

| ID | Sev. | Status | How |
|---|---|---|---|
| R15-1 | Medium | **Fixed in design** | 27.6: hooks run at narrowphase time; a recycled contact keeps its verdict; a hook that must see every step marks its contacts non-recyclable, a fork feature. Call counts to be re-measured once vdW is merged |
| R15-2 | Low | **Fixed in design** | The mixers have no context: their trampoline reads a thread-local current world, set around each step |
| R15-3 | High | **Fixed in design** | The layer-4 fallback restores the guest's image from the start of the failing frame and exports from it, else falls back to the previous engine's image at the activation frame. It never reads a confirmed frame; a runtime change is a world-format version bump (27.4) |
| R15-4 | Medium | **Fixed** | D70's caps recomputed to fit 8 ms with re-simulation: about 48 bodies under wasmi and 200 under Cranelift, conservative because they scale down linearly [I] |

## Decisions written as options (for the user)

* **D72** (R3-3): claims and "an author never sees its own interaction corrected". Recommended: no refusal
  of any physical interaction; grabs become soft constraints; exclusivity is a world-logic opt-in.
* **D73** (R3-10): thinned hand entries and the author's own hand. Recommended: pack k per-frame targets
  into one entry.
* **D57** (L3, NEW-17): logic-state snapshots. Recommended: all of linear memory, dirty-page diffs, hashed
  whole at check frames.

## Found while fixing (round 3)

* **The suite did not check conservation.** An `r14 late` regression lost an object while the suite
  reported success. It now fails on "NOT CONSERVED".
* **Pruning a rejected arrival at its deadline lost the refusal** before B attested it. Rejected arrivals
  are now kept 3,600 frames as well.
* **A silence vote needs a safe withdrawal,** or one member's false suspicion stalls everyone. Withdrawal is
  allowed only while the voter has not seen a complete set of standing votes.
* **Freezing a liar's chain would block the eviction it lied in.** So a lying vote is void, but the liar is
  not frozen; removing it waits for the phase-3 view change.
* **Demotion changed backdate part 2:** a member lagging its horizon on purpose is now removed, where before
  it stalled confirmation for good.
* **A congested member must not be mistaken for a crashed one:** c1's congested member is silent for
  1.95 s, so suspicion waits 3 s.

## Round 3, final review (11185cc approved): folded in at integration

@design-reviewer approved 11185cc for integration (`review-round3-final.md`) and agreed with both
deliberate deviations (keeping accepted transfer ids; a liveness check instead of birth binding). Its
remaining notes, done on the `integration` branch:

| Note | Done |
|---|---|
| D72(b) must name its griefing cost | Added: anyone can pull a held object out of another player's hand, which claims prevented (M3); worlds can turn on exclusive holding or limit who may grab |
| D73(a) must name its backdating cost | Added: at k = 8 under congestion, hand-target packs widen the backdating and `tie_rank` window from about 4 to about 9 frames |
| D57(a) must name its hashing cost | Added: wasmi has no dirty-page tracking [S], so every page is hashed at each keyframe, about 1.1 MB per module |
| Accepted transfer ids grow inside rollback state | Design (22.7): once confirmed, they move to an append-only set outside the rollback state; B may drop one once A certifies the transfer resolved [I] |
| Found at integration: M8 (module identity) | **Fixed [V].** With the box3d fork as a path dependency, `physics_in_wasm` embedded the checkout path in its panic locations, and cargo's path-derived crate hashes permuted its functions, so its canonical hash depended on where it was built. Paths are now remapped and the module builds in the root workspace; built at two paths, all four modules are byte-identical (spike report 6.5, design 13.2) |
| Found at integration: `r14 forge` | **Fixed [V].** A test-harness bug: it took the first escrow in A's map, which vdW physics reordered, and panicked. It now takes the pending escrow and fails if any attack is not tried; the 9 outcomes are unchanged |
| R15-1: re-measure hook call counts with vdW recycling | Re-measured [V]: on the active phase-1 pile hooks fire 13-25% **more** often than with upstream recycling (256 bodies: 160 pre-solve calls and 319 mixes per step, was 141 and 281), because the scaled radius of its bodies (0.4 m half-extent) is below upstream's fixed one. Per-contact guest hooks now cost 7-12% of the step under wasmi, batched under 2%. 27.5, 27.6 and spike report 5h updated |
