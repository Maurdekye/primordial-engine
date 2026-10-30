"""iter-bench: time the edit -> build -> hot-swap -> first-frame loop on the generated guest.

For each configuration: one untimed warm build, 3 no-op builds (cargo's floor), then REPS edits
that alternate a constant between two values (each build sees a real change). If a runtime is
given, a live `iter-host serve` process is running the sim; after each build it is told to swap
to the new .wasm and reports read/compile/instantiate/migrate/first-tick times. The loop time is
measured from just before the source write to the swap reply.

usage (from the repo root; hold the pe-bench-slot slot):
  python -I scripts/iter_bench_loop.py --set main|levers|cranelift|scale [--reps N] [--guests DIR] [--tag T]

Raw results: spikes/out/iter-bench/loop-<set>[-<tag>].jsonl (one JSON object per build).
"""

import json
import os
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BENCH = os.path.join(ROOT, "spikes", "iter-bench")
OUT = os.path.join(ROOT, "spikes", "out", "iter-bench")
HOST = os.path.join(BENCH, "target", "host", "release", "iter-host.exe")
TIMER = os.path.join(BENCH, "target", "tools", "release", "rustc-timer.exe")
WASM = "wasm32-unknown-unknown"
NATIVE = "x86_64-pc-windows-msvc"

EDITS = {
    "leaf": ({"split": "sys_movement/src/lib.rs", "mono": "logic_mono/src/sys_movement.rs"},
             "let friction = 0.980; // ITER-EDIT", "let friction = 0.981; // ITER-EDIT"),
    "core": ({"split": "pe_api/src/math.rs", "mono": "logic_mono/src/api/math.rs"},
             "let k = 3.0; // ITER-EDIT-CORE", "let k = 3.001; // ITER-EDIT-CORE"),
}
PKG = {"split": "logic_wasm", "mono": "logic_mono", "module": "mod_movement", "native": "logic_native"}
RT_WASMI = "wasmi-eager-fuel"
RT_CL = "wasmtime-cranelift-fuel-nancanon"


def C(name, shape, profile, edit="leaf", rt=RT_WASMI, env=None, target=WASM, toolchain=None, cargo_extra=(),
      breakdown=False):
    return dict(name=name, shape=shape, profile=profile, edit=edit, rt=rt, env=env or {}, target=target,
                toolchain=toolchain, cargo_extra=list(cargo_extra), breakdown=breakdown)


SETS = {
    "main": [
        C("split dev", "split", "dev"),
        C("split dev-nodbg", "split", "dev-nodbg"),
        C("split dev-o1", "split", "dev-o1"),
        C("split rel-fast", "split", "rel-fast"),
        C("split release", "split", "release"),
        C("mono dev", "mono", "dev"),
        C("mono dev-o1", "mono", "dev-o1"),
        C("mono rel-fast", "mono", "rel-fast"),
        C("mono release", "mono", "release"),
        C("module dev (build only)", "module", "dev", rt=None),
        C("module rel-fast (build only)", "module", "rel-fast", rt=None),
        C("module release (build only)", "module", "release", rt=None),
        C("split dev core-edit", "split", "dev", edit="core"),
        C("mono dev core-edit", "mono", "dev", edit="core"),
        C("split rel-fast core-edit", "split", "rel-fast", edit="core"),
        C("split release core-edit", "split", "release", edit="core"),
        C("split rel-fast cranelift-rt", "split", "rel-fast", rt=RT_CL),
        C("split release cranelift-rt", "split", "release", rt=RT_CL),
    ],
    "levers": [
        C("split dev incr=0", "split", "dev", env={"CARGO_INCREMENTAL": "0"}),
        C("mono dev incr=0", "mono", "dev", env={"CARGO_INCREMENTAL": "0"}),
        C("split rel-fast incr=0", "split", "rel-fast", env={"CARGO_INCREMENTAL": "0"}),
        C("split dev cgu=1", "split", "dev", env={"CARGO_PROFILE_DEV_CODEGEN_UNITS": "1"}),
        C("split dev +breakdown", "split", "dev", rt=None, breakdown=True),
        C("split rel-fast +breakdown", "split", "rel-fast", rt=None, breakdown=True),
        C("split release +breakdown", "split", "release", rt=None, breakdown=True),
        C("mono dev +breakdown", "mono", "dev", rt=None, breakdown=True),
    ],
    "breakdown": [
        C("split dev +breakdown", "split", "dev", rt=None, breakdown=True),
        C("split rel-fast +breakdown", "split", "rel-fast", rt=None, breakdown=True),
        C("split release +breakdown", "split", "release", rt=None, breakdown=True),
        C("mono dev +breakdown", "mono", "dev", rt=None, breakdown=True),
        C("mono release +breakdown", "mono", "release", rt=None, breakdown=True),
    ],
    "cranelift": [
        C("native dev llvm stable", "native", "dev", rt=None, target=NATIVE),
        C("native dev llvm nightly", "native", "dev", rt=None, target=NATIVE, toolchain="nightly"),
        C("native dev cranelift nightly", "native", "dev", rt=None, target=NATIVE, toolchain="nightly",
          cargo_extra=["-Zcodegen-backend"], env={"CARGO_PROFILE_DEV_CODEGEN_BACKEND": "cranelift"}),
        C("wasm32 dev cranelift nightly", "split", "dev", rt=None, toolchain="nightly",
          cargo_extra=["-Zcodegen-backend"], env={"CARGO_PROFILE_DEV_CODEGEN_BACKEND": "cranelift"}),
    ],
    "scale": [
        C("split dev", "split", "dev"),
        C("split rel-fast", "split", "rel-fast"),
        C("split release", "split", "release"),
        C("mono dev", "mono", "dev"),
        C("mono rel-fast", "mono", "rel-fast"),
        C("mono release", "mono", "release"),
    ],
}


def profile_dir(p):
    return "debug" if p in ("dev",) else p


def stats(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return None
    return {"median": round(statistics.median(xs), 4), "min": round(min(xs), 4), "max": round(max(xs), 4), "n": len(xs)}


def main():
    args = sys.argv[1:]
    which = args[args.index("--set") + 1] if "--set" in args else "main"
    reps = int(args[args.index("--reps") + 1]) if "--reps" in args else 8
    guests = os.path.abspath(args[args.index("--guests") + 1]) if "--guests" in args else os.path.join(BENCH, "guests")
    tag = args[args.index("--tag") + 1] if "--tag" in args else ""
    tgt_dir = os.path.join(BENCH, "target", "guests" if not tag else "guests-" + tag)
    os.makedirs(OUT, exist_ok=True)
    out_path = os.path.join(OUT, f"loop-{which}{'-' + tag if tag else ''}.jsonl")
    outf = open(out_path, "w", encoding="utf-8", newline="\n")

    def rec(o):
        outf.write(json.dumps(o) + "\n")
        outf.flush()

    meta = {"kind": "meta", "set": which, "reps": reps, "guests": guests, "time": time.strftime("%Y-%m-%dT%H:%M:%S"),
            "rustc": subprocess.run(["rustc", "-vV"], capture_output=True, text=True, cwd=guests).stdout.strip(),
            "git": subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True, cwd=ROOT).stdout.strip()}
    # Process-spawn cost on this machine (what each cargo -> rustc hop pays at least).
    sp = []
    for _ in range(40):
        t = time.perf_counter()
        subprocess.run([TIMER, "--noop"])
        sp.append((time.perf_counter() - t) * 1e3)
    meta["spawn_noop_exe_ms"] = stats(sp)
    rec(meta)
    print(f"spawn of a no-op exe from Python: median {meta['spawn_noop_exe_ms']['median']:.2f} ms", flush=True)

    for cfg in SETS[which]:
        run_config(cfg, reps, guests, tgt_dir, rec)
    outf.close()
    print(f"raw: {out_path}")


def run_config(cfg, reps, guests, tgt_dir, rec):
    shape = cfg["shape"]
    files, a, b = EDITS[cfg["edit"]]
    src = os.path.join(guests, files["mono" if shape == "mono" else "split"])
    original = open(src, encoding="utf-8").read()
    assert a in original, (src, a)
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = tgt_dir
    env.update(cfg["env"])
    timer_log = None
    if cfg["breakdown"]:
        timer_log = os.path.join(tgt_dir, "rustc-timer.jsonl")
        env["RUSTC_WRAPPER"] = TIMER
        env["RUSTC_TIMER_LOG"] = timer_log
    cmd = ["cargo"] + (["+" + cfg["toolchain"]] if cfg["toolchain"] else []) + \
        ["build", "-q", "--target", cfg["target"], "--profile", cfg["profile"], "-p", PKG[shape]] + cfg["cargo_extra"]

    def build():
        t = time.perf_counter()
        r = subprocess.run(cmd, cwd=guests, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace")
        return time.perf_counter() - t, r

    def write(text):
        with open(src, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(text)

    serve = None
    try:
        dt, r = build()
        if r.returncode != 0:
            rec({"kind": "config_error", "config": cfg["name"], "cmd": " ".join(cmd), "stderr": r.stderr[-3000:]})
            print(f"{cfg['name']:34s} BUILD FAILED: {r.stderr.strip().splitlines()[-1] if r.stderr.strip() else '?'}",
                  flush=True)
            return
        warm = dt
        noop = [build()[0] for _ in range(3)]
        art_dir = os.path.join(tgt_dir, cfg["target"], profile_dir(cfg["profile"]))
        wasm = os.path.join(art_dir, PKG[shape] + ".wasm")
        if cfg["rt"]:
            serve = subprocess.Popen([HOST, "serve", wasm, cfg["rt"], "2000"], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, text=True)
            ready = json.loads(serve.stdout.readline())
            assert ready["kind"] == "ready", ready
        rows = []
        cur = original
        for k in range(reps):
            cur = cur.replace(a, b) if a in cur else cur.replace(b, a)
            if timer_log and os.path.exists(timer_log):
                os.remove(timer_log)
            t0 = time.perf_counter()
            write(cur)
            bt, r = build()
            if r.returncode != 0:
                rec({"kind": "build_error", "config": cfg["name"], "stderr": r.stderr[-3000:]})
                print(f"{cfg['name']}: build error", flush=True)
                return
            swap = None
            if serve:
                serve.stdin.write(f"swap {wasm}\n")
                serve.stdin.flush()
                swap = json.loads(serve.stdout.readline())
            loop = time.perf_counter() - t0
            row = {"kind": "iter", "config": cfg["name"], "rep": k, "build_s": round(bt, 4),
                   "loop_s": round(loop, 4), "swap": swap}
            if os.path.exists(art_dir):
                p = wasm if cfg["target"] == WASM else os.path.join(art_dir, PKG[shape] + ".dll")
                row["artifact_bytes"] = os.path.getsize(p) if os.path.exists(p) else None
            if timer_log and os.path.exists(timer_log):
                calls = [json.loads(x) for x in open(timer_log, encoding="utf-8")]
                row["rustc"] = [{"crate": c["crate"], "ms": c["ms"]} for c in calls if c["crate"]]
                row["rustc_sum_ms"] = round(sum(c["ms"] for c in calls), 1)
                row["rustc_calls"] = len(calls)
                row["_argv"] = [(c["cwd"], c["argv"]) for c in calls if c["crate"]]
            rows.append(row)
            rec({k2: v for k2, v in row.items() if k2 != "_argv"})
        # Direct rustc replay (no cargo): re-run the rustc commands cargo issued for the last edit.
        replay = []
        if timer_log and rows and rows[-1].get("_argv"):
            # The log is in completion order and cargo pipelines rustc processes; replay in dependency order.
            order = ["pe_api", "sys_", "logic_game", "logic_", "mod_"]
            argvs = sorted(rows[-1]["_argv"], key=lambda x: next(
                i for i, p in enumerate(order) if x[1][x[1].index("--crate-name") + 1].startswith(p)))
            renv = {k2: v for k2, v in env.items() if k2 not in ("RUSTC_WRAPPER", "RUSTC_TIMER_LOG")}
            for k in range(reps):
                cur = cur.replace(a, b) if a in cur else cur.replace(b, a)
                t0 = time.perf_counter()
                write(cur)
                ok = True
                for cwd, argv in argvs:
                    rr = subprocess.run(argv, cwd=cwd, env=renv, capture_output=True, text=True)
                    ok &= rr.returncode == 0
                dt = time.perf_counter() - t0
                replay.append(dt)
                rec({"kind": "replay", "config": cfg["name"], "rep": k, "direct_rustc_s": round(dt, 4), "ok": ok,
                     "crates": [x[1][x[1].index("--crate-name") + 1] for x in argvs]})
        summ = {"kind": "summary", "config": cfg["name"], "cmd": " ".join(cmd), "env": cfg["env"],
                "warm_build_s": round(warm, 3), "noop_build_s": stats(noop),
                "build_s": stats([x["build_s"] for x in rows]), "loop_s": stats([x["loop_s"] for x in rows]),
                "artifact_bytes": rows[-1].get("artifact_bytes") if rows else None, "rt": cfg["rt"]}
        if serve:
            for f in ("compile_ms", "instantiate_ms", "migrate_ms", "first_tick_ms", "total_ms"):
                summ["swap_" + f] = stats([x["swap"][f] for x in rows])
        if timer_log:
            summ["rustc_sum_ms"] = stats([x.get("rustc_sum_ms") for x in rows])
            summ["rustc_crates"] = rows[-1].get("rustc")
            summ["direct_rustc_s"] = stats(replay)
        rec(summ)
        line = f"{cfg['name']:34s} build {summ['build_s']['median']:7.3f}s [{summ['build_s']['min']:.3f}-{summ['build_s']['max']:.3f}]" \
               f"  noop {summ['noop_build_s']['median']:.3f}s"
        if serve:
            line += f"  swap {summ['swap_total_ms']['median']:8.2f}ms (compile {summ['swap_compile_ms']['median']:.1f}," \
                    f" tick1 {summ['swap_first_tick_ms']['median']:.1f})  loop {summ['loop_s']['median']:.3f}s"
        if timer_log:
            line += f"  rustc-sum {summ['rustc_sum_ms']['median']:.0f}ms" + \
                    (f"  direct-rustc {summ['direct_rustc_s']['median']:.3f}s" if replay else "")
        line += f"  {summ['artifact_bytes']} B"
        print(line, flush=True)
    finally:
        write(original)
        if serve:
            try:
                serve.stdin.write("quit\n")
                serve.stdin.flush()
                serve.wait(timeout=10)
            except Exception:
                serve.kill()


if __name__ == "__main__":
    main()
