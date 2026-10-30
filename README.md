# Primordial Engine

A peer-to-peer game engine in Rust: deterministic fixed-step simulation with rollback (Box3D via the
pure-Rust `box3d-rust` port), wgpu graphics, live-editable Rust game logic (WASM) and WGSL shaders under
a git-style source-control layer, AI-first through MCP, VR via OpenXR.

**Status: phase 1 (design + feasibility spikes), revised after design review. Nothing beyond the spikes is built yet.**

* [docs/design.md](docs/design.md): architecture, requirement traceability, decisions needing the user
* [docs/spike-report.md](docs/spike-report.md): what was run, results, verified vs inferred, risks
* [docs/review-response.md](docs/review-response.md): every design-review finding and how it was resolved
* [docs/iteration-benchmark.md](docs/iteration-benchmark.md): edit-to-running latency and logic runtimes (by @iter-bench)

## Spikes

| Crate | What it shows |
|---|---|
| `spikes/sim-core` | Shared harness: pile scene, snapshot/restore, rollback sim; `session.rs` = the session protocol core (state + canonical frame, ack-based confirmation, equivocation proofs, claims, tie-break, R14 escrow) |
| `spikes/physics` | (a) box3d-rust vs C goldens, per-frame hash traces; (b) rollback tests, bench |
| `spikes/p2p` | (c-v2) two serverless UDP peers running the session protocol, with the state+frame acceptance check |
| `spikes/session-sim` | In-process session simulator: the review fixes, the ball pit, two worlds with a cross-world handoff (R14) |
| `spikes/iter-bench` | Iteration-latency and logic-runtime benchmark: generated guests, wasmi/wasmtime host |
| `spikes/wasm-hotswap` + `spikes/wasm-guests` | (d) Rust->WASM game logic hot-swap in wasmi, fuel, rollback across swaps |
| `spikes/mcp-stub` | Minimal MCP server (JSON-RPC over stdio) reading world state, proposing interactions |
| `spikes/stack-audit` + `scripts/audit_native.py` | Native-code audit of the candidate dependency stack (100% Rust rule) |

```bash
cargo build --workspace --release
./target/release/pe-spike-physics goldens
./target/release/pe-spike-physics rollback
bash scripts/run-p2p.sh latency60 "--delay-ms 60 --jitter-ms 20" "--delay-ms 60 --jitter-ms 20"
bash scripts/run_session_sim.sh
(cd spikes/wasm-guests && cargo build --release --target wasm32-unknown-unknown) && ./target/release/pe-spike-wasm-hotswap
```
