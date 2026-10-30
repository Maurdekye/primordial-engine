"""Summarize spikes/out/chunks/*.log (R16/R17 spike) into the tables used by docs/r16-chunks-results.md.

Usage: python -I scripts/chunks_summary.py [spikes/out/chunks]
Every RESULT line is key=value pairs; '### ... build X' headers name the build that printed it.
"""
import re, statistics, sys, collections, pathlib

d = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "spikes/out/chunks")

def parse(name):
    p = d / name
    if not p.exists():
        return []
    rows, build = [], None
    for line in p.read_text(encoding="utf-8", errors="replace").splitlines():
        m = re.search(r"build (\S+)", line)
        if line.startswith("###") and m:
            build = m.group(1)
        if line.startswith("RESULT"):
            parts = line.split()
            kv = {"_kind": parts[1], "_build": build}
            for t in parts[2:]:
                if "=" in t:
                    k, v = t.split("=", 1)
                    kv[k] = v
            rows.append(kv)
    return rows

def med(xs):
    return statistics.median(xs) if xs else float("nan")

# Physics cost: median and range over the interleaved rounds.
cost = parse("cost.log")
g = collections.defaultdict(list)
for r in cost:
    key = (r["_build"], r.get("bodies", r.get("scene")))
    g[key].append(r)
print("## cost (median over rounds [min-max])")
print("| build | scene | step ms | save ms | restore ms | image bytes |")
for (b, sc), rs in sorted(g.items()):
    st = [float(r["step_ms"]) for r in rs]
    sv = [float(r["save_ms"]) for r in rs if "save_ms" in r]
    rt = [float(r["restore_ms"]) for r in rs if "restore_ms" in r]
    print(f"| {b} | {sc} | {med(st):.3f} [{min(st):.3f}-{max(st):.3f}] | {med(sv):.3f} | {med(rt):.3f} | {rs[0]['image_bytes']} |")

# A/B: per-block minimum over rounds, averaged over blocks.
ab = collections.defaultdict(list)
build = None
p = d / "ab.log"
if p.exists():
    for line in p.read_text(encoding="utf-8").splitlines():
        if line.startswith("###"):
            build = line.split()[-1]
        if line.startswith("RESULT ab"):
            sc = re.search(r"scene=(\S+)", line).group(1)
            ab[(sc, build)].append([float(x) for x in re.search(r"blocks=(\S+)", line).group(1).split(",")])
    print("\n## ab (minimum envelope, ms per step)")
    for (sc, b), runs in sorted(ab.items()):
        env = [min(r[i] for r in runs) for i in range(len(runs[0]))]
        print(f"| {sc} | {b} | {sum(env) / len(env):.3f} | rounds={len(runs)} |")

for name in ["regions.log", "players.log", "streaming.log", "avatars.log"]:
    rows = parse(name)
    if rows:
        print(f"\n## {name}")
        for r in rows:
            print(r["_build"], " ".join(f"{k}={v}" for k, v in r.items() if not k.startswith("_")))
