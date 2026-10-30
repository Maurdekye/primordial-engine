"""iter-bench: turn the raw JSONL results in spikes/out/iter-bench/ into Markdown tables.

usage: python -I scripts/iter_bench_report.py > spikes/out/iter-bench/tables.md
"""

import json
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "spikes", "out", "iter-bench")


def load(name):
    p = os.path.join(OUT, name)
    if not os.path.exists(p):
        return []
    return [json.loads(x) for x in open(p, encoding="utf-8") if x.strip().startswith("{")]


def loop_table(name, title):
    rows = [o for o in load(name) if o["kind"] == "summary"]
    errs = [o for o in load(name) if o["kind"] == "config_error"]
    if not rows and not errs:
        return
    print(f"### {title} (`{name}`)\n")
    print("| config | build s median [min-max] | no-op cargo s | swap ms (compile / first tick) | edit-to-first-frame s | artifact bytes |")
    print("|---|---|---|---|---|---|")
    for o in rows:
        b = o["build_s"]
        swap = (f"{o['swap_total_ms']['median']:.1f} ({o['swap_compile_ms']['median']:.1f} / "
                f"{o['swap_first_tick_ms']['median']:.1f})") if "swap_total_ms" in o else "-"
        loop = f"{o['loop_s']['median']:.3f}" if "swap_total_ms" in o else "-"
        print(f"| {o['config']} | {b['median']:.3f} [{b['min']:.3f}-{b['max']:.3f}] | {o['noop_build_s']['median']:.3f} | "
              f"{swap} | {loop} | {o['artifact_bytes']} |")
    for o in errs:
        last = [ln for ln in o["stderr"].splitlines() if ln.startswith("error")][:1]
        print(f"| {o['config']} | FAILED: {last[0] if last else '?'} | | | | |")
    print()


def breakdown_table():
    rows = [o for o in load("loop-breakdown.jsonl") if o["kind"] == "summary"]
    if not rows:
        return
    print("### cargo vs rustc (`loop-breakdown.jsonl`, RUSTC_WRAPPER=rustc-timer)\n")
    print("| config | cargo build s | sum of rustc ms | rustc per crate (last edit) | direct rustc replay, no cargo, s |")
    print("|---|---|---|---|---|")
    for o in rows:
        per = ", ".join(f"{c['crate']} {c['ms']:.0f}" for c in o["rustc_crates"] or [])
        print(f"| {o['config']} | {o['build_s']['median']:.3f} | {o['rustc_sum_ms']['median']:.0f} | {per} | "
              f"{o['direct_rustc_s']['median']:.3f} |")
    print()


def runtime_tables():
    r1 = load("runtime-matrix-release-run1.jsonl")
    r2 = load("runtime-matrix-release-run2.jsonl")
    if not r1:
        return
    by = lambda rs, kind: {(o["rt"], o.get("n"), o.get("call")): o for o in rs if o["kind"] == kind}
    t1, comp, hc, mig, det = by(r1, "tick"), by(r1, "compile"), by(r1, "hostcall"), by(r1, "migrate"), by(r1, "determinism")
    det2 = by(r2, "determinism")
    t2 = by(r2, "tick")
    rts = [o["rt"] for o in r1 if o["kind"] == "compile"]
    nat = {n: t1[("native", n, None)]["tick_ms_median"] for n in (1000, 10000)}
    print("### Logic throughput, release wasm (`runtime-matrix-release-run1/2.jsonl`)\n")
    print("| runtime | tick ms n=1k (run1/run2) | tick ms n=10k (run1/run2) | x native (10k) | p10-p90 10k | fuel/frame 10k |")
    print("|---|---|---|---|---|---|")
    for rt in rts:
        a, b = t1[(rt, 1000, None)], t1[(rt, 10000, None)]
        a2, b2 = t2.get((rt, 1000, None), {}), t2.get((rt, 10000, None), {})
        print(f"| {rt} | {a['tick_ms_median']:.3f} / {a2.get('tick_ms_median', 0):.3f} | {b['tick_ms_median']:.3f} / "
              f"{b2.get('tick_ms_median', 0):.3f} | {b['tick_ms_median'] / nat[10000]:.1f} | "
              f"{b['tick_ms_p10']:.2f}-{b['tick_ms_p90']:.2f} | {b['fuel_per_frame'] or '-'} |")
    print()
    print("### Compile, instantiate, host calls, state migration (run1)\n")
    print("| runtime | compile ms median [min-max] | instantiate ms | host call rand_u32() ns | host call terrain_height(f32,f32) ns | save / load ms (n=10k, 890 KB) |")
    print("|---|---|---|---|---|---|")
    for rt in rts:
        c = comp[(rt, None, None)]
        m = mig[(rt, 10000, None)]
        print(f"| {rt} | {c['compile_ms_median']:.2f} [{c['compile_ms_min']:.2f}-{c['compile_ms_max']:.2f}] | "
              f"{c['instantiate_ms_median']:.4f} | {hc[(rt, None, 'rand_u32()')]['ns_per_call_median']:.1f} | "
              f"{hc[(rt, None, 'terrain_height(f32,f32)')]['ns_per_call_median']:.1f} | "
              f"{m['save_ms_median']:.3f} / {m['load_ms_median']:.3f} |")
    print()
    print("### Determinism (n=2000, 300 frames, digest every 10 frames + per-frame fuel)\n")
    print("| runtime | final digest run1 | run2 (separate process) | 2 instances identical (run1/run2) | fuel total run1 / run2 | NaN probe |")
    print("|---|---|---|---|---|---|")
    for rt in rts:
        d, e = det[(rt, 2000, None)], det2.get((rt, 2000, None), {})
        print(f"| {rt} | {d['final_digest']} | {e.get('final_digest')} | {d['two_instances_identical']} / "
              f"{e.get('two_instances_identical')} | {d['fuel_total']} / {e.get('fuel_total')} | {d['nan_probe']} |")
    print()


def simple(name, title, cols):
    rs = load(name)
    if not rs:
        return
    print(f"### {title} (`{name}`)\n")
    print("| " + " | ".join(c for c, _ in cols) + " |")
    print("|" + "---|" * len(cols))
    for o in rs:
        if o["kind"] == "error":
            print(f"| {o['rt']} | ERROR {o['error']} |")
            continue
        vals = []
        for _, f in cols:
            v = f(o) if callable(f) else o.get(f)
            vals.append(f"{v:.4g}" if isinstance(v, float) else str(v))
        print("| " + " | ".join(vals) + " |")
    print()


def main():
    print("<!-- generated by scripts/iter_bench_report.py from spikes/out/iter-bench/*.jsonl -->\n")
    loop_table("loop-main.jsonl", "Iteration loop, 6.4k-LOC guest")
    loop_table("loop-levers.jsonl", "Levers")
    breakdown_table()
    loop_table("loop-cranelift.jsonl", "rustc_codegen_cranelift")
    loop_table("loop-scale-x3.jsonl", "Iteration loop, 17k-LOC guest (scale 3)")
    loop_table("loop-scale-x8.jsonl", "Iteration loop, 45k-LOC guest (scale 8)")
    runtime_tables()
    comp = [("runtime", "rt"), ("wasm bytes", "wasm_bytes"), ("compile ms median", "compile_ms_median"),
            ("instantiate ms", "instantiate_ms_median")]
    tick = [("runtime", "rt"), ("n", "n"), ("tick ms median", "tick_ms_median"), ("fuel/frame", "fuel_per_frame")]
    for name, title in [("runtime-matrix-rel-s.jsonl", "opt-level s wasm"), ("runtime-matrix-dev.jsonl", "dev (unoptimized) wasm"),
                        ("runtime-matrix-x8-release.jsonl", "45k-LOC guest, release wasm"),
                        ("runtime-matrix-winch-nancanon.jsonl", "Winch NaN-canonicalization re-run, release wasm")]:
        rs = load(name)
        if not rs:
            continue
        print(f"### Runtime matrix, {title} (`{name}`)\n")
        print("| runtime | wasm bytes | compile ms | instantiate ms | tick ms n=1k | tick ms n=10k |")
        print("|---|---|---|---|---|---|")
        for o in rs:
            if o["kind"] == "compile":
                t = {x["n"]: x["tick_ms_median"] for x in rs if x["kind"] == "tick" and x["rt"] == o["rt"]}
                print(f"| {o['rt']} | {o['wasm_bytes']} | {o['compile_ms_median']:.2f} | {o['instantiate_ms_median']:.4f} | "
                      f"{t.get(1000, 0):.3f} | {t.get(10000, 0):.3f} |")
        print()
    rs = load("systems-release.jsonl")
    if rs:
        print("### Per-system cost, n=10k, release wasm, each mask run alone (`systems-release.jsonl`)\n")
        rts = list(dict.fromkeys(o["rt"] for o in rs))
        systems = list(dict.fromkeys(o["system"] for o in rs))
        val = {(o["rt"], o["system"]): o["tick_ms_median"] for o in rs}
        print("| system | " + " | ".join(rts) + " |")
        print("|---|" + "---|" * len(rts))
        for s in systems:
            print(f"| {s} | " + " | ".join(f"{val[(r, s)]:.3f}" for r in rts) + " |")
        print()
    simple("physics-winch-nancanon.jsonl", "Physics, Winch NaN-canonicalization re-run",
           [("runtime", "rt"), ("compile ms (1 sample)", "compile_ms"), ("ms/frame median of 3", "ms_per_frame_median"),
            ("digest == native", "digest_matches_native")])
    simple("physics-runtimes.jsonl", "Phase-1 Box3D pile compiled to wasm32, 600 frames (native: 0.31 ms/frame, phase 1)",
           [("runtime", "rt"), ("compile ms (1 sample)", "compile_ms"), ("ms/frame median of 3", "ms_per_frame_median"),
            ("digest == native", "digest_matches_native")])


if __name__ == "__main__":
    main()
