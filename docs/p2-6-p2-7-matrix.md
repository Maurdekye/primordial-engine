# P2-6 and P2-7: the local determinism matrix and byte-identical guest builds

Owner: @design-reviewer. Reviewer: @chunks-reviewer. Design: 23.4 (the packages), 13.2 (the rules).

**User ruling, 2026-10-30 19:58Z:**
* CI runs on local machines only: this PC's Windows and WSL Ubuntu-24.04, plus <laptop>.
* Nothing leaves the user's machines.
* ARM is deferred.

## One command

```bash
PE_HEAVY="<team folder>/heavy.sh" cmd //c start "" //low //b //wait bash scripts/run_matrix.sh   # win glibc musl laptop
```

### Columns

Each column runs `scripts/run_determinism.sh` with the parts physics, wasm, mcp, session, r18 and hooks.

| Column | Machine | Target | Built | Run |
|---|---|---|---|---|
| `win` | this PC (Ryzen 7 9700X), Windows 11 | x86_64-pc-windows-msvc | here | here |
| `glibc` | this PC, WSL2 Ubuntu 24.04 | x86_64-unknown-linux-gnu | in WSL | in WSL |
| `musl` | this PC, WSL2 Ubuntu 24.04 | x86_64-unknown-linux-musl, static, linked by rust-lld | here (no C toolchain); wasm guests natively in WSL | in WSL |
| `laptop` | Intel i7-13620H laptop, Windows 11 | x86_64-pc-windows-msvc | there, over ssh (Tailscale) | there |

### How the columns run

* **Checkouts:** the WSL and <laptop> columns run the committed HEAD, shipped as a git bundle. Each has its own checkout, at a different path from this PC's:
  * WSL: `~/pe-matrix/src`;
  * <laptop>: `C:/Users/<user>/pe-matrix/src`, with a folder-local cargo home.
* **Toolchain:** every column uses the channel pinned in `rust-toolchain.toml` (1.99.0).
* **Machine-load rule:**
  * Local cargo commands go through `PE_HEAVY`, the org lock.
  * WSL jobs run at nice 19 with 2 jobs, in two locked phases (build, then run).
  * <laptop> is unthrottled.

### What it compares

**The digests.** `scripts/matrix_digest.py` digests every output of every column.
* **Hashed raw:** data files, meaning traces, snapshot images, wasm modules and their hash lists, and the MCP transcript.
* **Hashed after normalization:** logs. Normalization masks only wall-clock (CPU) timings, by the phrases the tools print them with: "in 349.8ms", "step avg", "(… us/frame)", "resim frames N (… ms)", "sim CPU", "worst tick" and so on.
  * These were found by diffing two machines' raw outputs.
  * Virtual-clock results are compared: R18 queue and recovery times, session-sim event times, and configured latencies (review of 3b07aa5, M1).
  * Also masked: cargo progress lines, column padding, the output directory echoed in logs, and sha256sum's Windows `*` marker.

**Validating the normalization:** this PC's committed d79f160 records against <laptop>'s rerun. 246 of 247 outputs were equal. The one difference was a stale committed log, which is reported on the cross-machine determinism re-check item.

**The comparison** (`matrix_digest.py compare`) prints one row per output and one column per machine. The run fails on any of these (review of 3b07aa5, M2 and M3), and names the first differing output and its first differing normalized line, or byte offset:
* a difference between columns;
* a requested column with no outputs;
* a column whose own build or determinism set failed, even when every column failed the same way.

`run_matrix.sh` also refuses a dirty tree, so every column runs one commit. `PE_COMPARE_ONLY=1` re-checks existing outputs without running anything.

### Outputs

* **Committed:**
  * `spikes/out/matrix/matrix.txt`;
  * each column's `env.txt` and `run.log`;
  * each column's `o/digests.txt`.
* **Local only:** the full outputs, in `o/`.

## Result

`spikes/out/matrix/matrix.txt`, 2026-10-30/31 [V]:

**MATRIX: PASS, 72 outputs equal on 4 machines (win, glibc, musl, laptop).**

* **Commits:** win, glibc and musl ran at 2fa62ae; laptop at 35ea997, which differs only in the digest tool and docs.
* **Every column:** `SUMMARY: determinism set PASSED`, with these checks:
  * engine and C goldens, release and debug;
  * the 600-frame trace `80839f4a54133e13` (with `--upstream`, `7903109aab28ed9b`), release equal to debug;
  * snapshot images;
  * rollback T1-T5;
  * wasm d1-d4;
  * each of the 6 modules byte-identical at two checkout paths, with no local path inside;
  * MCP 10/10;
  * the session-sim suite (every scenario ALL EQUAL, conserved, 0 false checks);
  * R18 caps;
  * R15 hook counts.
* **Across machines:** the six modules have the same raw SHA-256 on all four columns:
  * logic_v1 `ae109ced…`, logic_v2 `39191011…`, logic_runaway `4bbde67e…`;
  * physics_in_wasm `ee57b239…`;
  * logic_mono `8a87f7de…`, logic_wasm `1196c2f6…`.

  Each column built its own modules: twice on Windows (two CPUs) and twice on Linux, each at its own checkout path.

## P2-7: what differed between Windows and Linux, and the fixes

The details and hashes are in `spikes/out/p2-7/cross-os-findings.txt`; the rules are in design 13.2.

The modules are:
* the four spike guests: logic_v1, logic_v2, logic_runaway and physics_in_wasm;
* two larger crates: logic_mono, the 6.4k-line iter-bench guest as one crate, and logic_wasm, the same code as nine path-linked crates.

All six are now built by `scripts/build_wasm_guests.sh`.

| Finding | Effect | Fix |
|---|---|---|
| `stable` resolved to 1.99.1 in WSL and 1.99.0 on Windows | A different compiler | `rust-toolchain.toml` pins `channel = "1.99.0"` (minimal profile, clippy and rustfmt) |
| rustc writes workspace members' source paths relative to the workspace, with the host separator (`logic_mono\src\…` vs `logic_mono/src/…`), and the root `--remap-path-prefix` does not touch them | logic_mono, logic_wasm and physics_in_wasm differed in their panic-location strings | `sepmap` in `build_wasm_guests.sh`: on Windows, one remap per source directory, deepest last |
| Cargo's crate hash for a workspace member nested below the root (`spikes/sim-core`, `crates/box3d-fork`) differs between hosts, and it shows in the mangled names | physics_in_wasm differed only in its `name` custom section; its canonical hash was equal | `strip = "symbols"` in every guest profile: committed modules carry no name section |

### Profiles

The cheaper `rel-fast` profile (opt 3, no LTO, 16 codegen units, incremental) is reproducible as well:
* across the two OSes (logic_mono and logic_wasm);
* at two paths;
* after an incremental edit and revert.

This is what the build-time table needs.

## Build-time table for the activation lead

Measured with `python -I scripts/guest_build_times.py --reps 3` on the Intel laptop, unthrottled. The data is in `spikes/out/p2-7/build-times-laptop.txt`, and the table is in design 13.2.

The conclusions:
* **Release (LTO) is too slow for the lead.** A one-line edit of the 6.4k-line module takes about 2.3 s with release, against the 0.5 s lead.
* **`rel-fast` fits.** The same edit takes about 0.5 s. So committed modules should be built with a pinned `rel-fast`-like profile, the same one the author's own loop uses.
* **Joining:** cold builds take 2.4 s for the 6.4k-line module and 19.4 s for physics_in_wasm.

This PC's timings were not taken: the user was in VR, and builds ran at idle priority.

## Changes

* **New scripts:**
  * `scripts/run_matrix.sh`, the one command;
  * `scripts/matrix_digest.py`, digests and comparison;
  * `scripts/guest_build_times.py`, the build-time table.
* **`scripts/run_determinism.sh`:**
  * `PE_OUT`, `PE_TARGET`, `PE_BIN`, `PE_DEBUG_BIN`, `PE_BUILD_ONLY` and `PE_HEAVY`, so a matrix run never touches the committed records;
  * the six modules;
  * a raw-hash check at two paths for each module, where it used to count canonical hashes;
  * a local-path check for every module.

  `run_session_sim.sh`, `run_r18.sh` and `run_perturbation_history.sh` take `PE_BIN` and `PE_OUT`.
* **`scripts/build_wasm_guests.sh`:**
  * the separator remaps;
  * the larger crates;
  * `PE_HEAVY`.
* **Profiles:** `strip = "symbols"` in `profile.wasm-guest` (root), the spike guests' `release`, and the iter-bench `release` (also in `scripts/iter_bench_gen.py`).
* **`rust-toolchain.toml`** is pinned to 1.99.0.
* **Design:** 13.2 (the rules, the evidence and the build-time table) and 23.4 (P2-6 and P2-7 status).

### Not done

* ARM64 (deferred by the user).
* The `p2p` part. Its UDP runs under loss can differ between runs (spike report 5b).
* The `history` part (about 25 minutes per machine). `PE_PARTS` can add it. It was run on <laptop> against d79f160, where every end hash was equal.
