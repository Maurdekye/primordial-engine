"""iter-bench: edit time of a script under pe-logic's build service (logic.md 4.2), incremental on vs off, split
into compile, link and the post-build load (metering pass + Cranelift compile + instance).

The build command mirrors crates/pe-logic/src/build.rs: its PROFILE and CODEGEN lists are read from that file, so
the flags are the service's own; only `incremental` (and CARGO_INCREMENTAL) differ between the two modes. Remaps
are left out (they change bytes, not time).

For each mode: one untimed build, 3 no-op builds (cargo's floor), then REPS edits that alternate a constant in an
existing function of the script (`wrapping_mul(3)` <-> `wrapping_mul(5)` in tally's `spin`), each a real change.
Timed from just before the source write to cargo's exit.
Then, outside the timed loop: one edit per mode rebuilt with RUSTC_BOOTSTRAP=1 -Z time-passes on the script crate
(rustc's own split of the time, link included), and `cargo run -p pe-logic --release --example load_time` on the
built module.

usage (repo root; under heavy.sh with HEAVY_PRIO=normal, holding every slot, on a quiet machine):
  python -I scripts/iter_bench_logic_edit.py --worktree <ws/logic checkout> [--reps N] [--tag T]

Raw: spikes/out/iter-bench/logic-edit[-T]/edit.jsonl (one object per build), time-passes-<mode>.txt,
load-time.jsonl, summary.json.
"""

import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cargo_output import target_dir

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WASM = "wasm32-unknown-unknown"
EDIT = ("x.wrapping_mul(3)", "x.wrapping_mul(5)")


def flags_from_build_rs(worktree):
    text = open(os.path.join(worktree, "crates", "pe-logic", "src", "build.rs"), encoding="utf-8").read()

    def block(name):
        m = re.search(r"const " + name + r": &\[&str\] = &\[(.*?)\n\];", text, re.S)
        assert m, name
        return [json.loads(s) for s in re.findall(r'^\s*("(?:[^"\\]|\\.)*"),', m.group(1), re.M)]

    return block("PROFILE"), block("CODEGEN")


def stats(xs):
    return {"median": round(statistics.median(xs), 4), "min": round(min(xs), 4), "max": round(max(xs), 4),
            "n": len(xs)} if xs else None


def main():
    a = sys.argv[1:]
    worktree = os.path.abspath(a[a.index("--worktree") + 1])
    reps = int(a[a.index("--reps") + 1]) if "--reps" in a else 10
    tag = a[a.index("--tag") + 1] if "--tag" in a else ""
    out = os.path.join(ROOT, "spikes", "out", "iter-bench", "logic-edit" + ("-" + tag if tag else ""))
    os.makedirs(out, exist_ok=True)
    work = os.path.join(out, "work")
    shutil.rmtree(work, ignore_errors=True)
    script = os.path.join(work, "tally")
    shutil.copytree(os.path.join(worktree, "crates", "pe-logic", "tests", "scripts", "tally"), script,
                    ignore=shutil.ignore_patterns("target"))
    sdk = os.path.join(worktree, "crates", "pe-sdk").replace("\\", "/")
    toml = os.path.join(script, "Cargo.toml")
    t = open(toml, encoding="utf-8").read()
    t = re.sub(r'pe-sdk = \{ path = "[^"]*" \}', 'pe-sdk = { path = "%s" }' % sdk, t)
    open(toml, "w", encoding="utf-8", newline="\n").write(t)
    src = os.path.join(script, "src", "lib.rs")
    orig = open(src, encoding="utf-8").read()
    assert EDIT[0] in orig, "the edit site moved"
    profile, codegen = flags_from_build_rs(worktree)
    rustc_v = subprocess.run(["rustc", "-V"], capture_output=True, text=True).stdout.strip()
    meta = {"rustc": rustc_v, "reps": reps, "profile": profile, "codegen": codegen,
            "heavy_prio": os.environ.get("HEAVY_PRIO", "idle"), "start": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    rows = []
    summary = {"meta": meta, "modes": {}}
    jsonl = open(os.path.join(out, "edit.jsonl"), "w", encoding="utf-8")
    jsonl.write(json.dumps({"meta": meta}) + "\n")
    wasm_paths = {}
    for mode in ("inc-on", "inc-off"):
        inc = "true" if mode == "inc-on" else "false"
        prof = [p.replace("incremental=false", "incremental=" + inc) for p in profile]
        tgt = target_dir(ROOT, "logic-edit/" + os.path.basename(out) + "/" + mode)
        env = dict(os.environ)
        env.pop("RUSTFLAGS", None)
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(codegen)
        env["CARGO_INCREMENTAL"] = "1" if inc == "true" else "0"
        env["RUSTC_WRAPPER"] = ""
        env["RUSTC_WORKSPACE_WRAPPER"] = ""
        env["CARGO_TARGET_DIR"] = tgt
        cmd = ["cargo", "rustc", "--release", "--lib", "--target", WASM, "--manifest-path", toml, "--offline",
               "--message-format=short"]
        for p in prof:
            cmd += ["--config", p]
        cmd += ["--", "-F", "unsafe_code"]

        def build(extra_env=None, extra_args=()):
            e = dict(env, **(extra_env or {}))
            t0 = time.perf_counter()
            r = subprocess.run(cmd + list(extra_args), env=e, capture_output=True, text=True, encoding="utf-8",
                               errors="replace")
            return time.perf_counter() - t0, r

        def write(text):
            with open(src, "w", encoding="utf-8", newline="\n") as fh:
                fh.write(text)

        write(orig)
        dt, r = build()
        assert r.returncode == 0, r.stderr[-2000:]
        noop = []
        for _ in range(3):
            dt, r = build()
            noop.append(dt)
        edits = []
        for i in range(reps):
            text = orig if i % 2 else orig.replace(EDIT[0], EDIT[1])
            t0 = time.perf_counter()
            write(text)
            r = subprocess.run(cmd, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace")
            dt = time.perf_counter() - t0
            assert r.returncode == 0, r.stderr[-2000:]
            edits.append(dt)
            row = {"mode": mode, "rep": i, "edit_to_built_s": round(dt, 4)}
            rows.append(row)
            jsonl.write(json.dumps(row) + "\n")
        for i, dt in enumerate(noop):
            jsonl.write(json.dumps({"mode": mode, "noop": i, "s": round(dt, 4)}) + "\n")
        # Breakdown, outside the timing: rustc's own pass times for one more real edit.
        write(orig.replace(EDIT[0], EDIT[1]) if reps % 2 == 0 else orig)
        _, r = build({"RUSTC_BOOTSTRAP": "1"}, ["-Z", "time-passes"])
        open(os.path.join(out, "time-passes-%s.txt" % mode), "w", encoding="utf-8").write(r.stderr)
        wasm_paths[mode] = os.path.join(tgt, WASM, "release", "tally.wasm")
        summary["modes"][mode] = {"edit_to_built_s": stats(edits), "noop_s": stats(noop)}
    write(orig)
    jsonl.close()
    # The post-build load on the built module (metering pass, Cranelift compile, instance).
    r = subprocess.run(["cargo", "run", "-q", "-p", "pe-logic", "--release", "--locked", "--example", "load_time", "--",
                        wasm_paths["inc-off"], "20"], cwd=worktree, capture_output=True, text=True, encoding="utf-8",
                       errors="replace")
    open(os.path.join(out, "load-time.jsonl"), "w", encoding="utf-8").write(r.stdout + r.stderr)
    loads = [json.loads(l) for l in r.stdout.splitlines() if l.startswith("{")]
    if loads:
        summary["load"] = {k: stats([x[k] for x in loads[1:] or loads]) for k in ("instrument_ms", "compile_ms",
                                                                                     "instantiate_ms")}
        summary["load"]["bytes"] = loads[0]["bytes"]
    summary["meta"]["end"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    json.dump(summary, open(os.path.join(out, "summary.json"), "w", encoding="utf-8"), indent=1)
    print(json.dumps(summary, indent=1))
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
