"""Median over par-bench repeats: step ms and rollback (restore + 6 frames) ms per size and
thread count, plus speed-up vs 1 thread. Usage: python scripts/par_bench_summary.py rep1 rep2 ..."""
import sys, statistics, collections
step = collections.defaultdict(list); rb = collections.defaultdict(list)
coll = collections.defaultdict(list); solve = collections.defaultdict(list)
for path in sys.argv[1:]:
    for line in open(path, encoding='utf-8'):
        parts = line.split('|')
        head = parts[0].split()
        if len(parts) < 5 or len(head) != 2 or not head[0].isdigit():
            continue
        n, t = int(head[0]), int(head[1])
        step[(n, t)].append(float(parts[1].split()[1]))
        coll[(n, t)].append(float(parts[2].split()[1]))
        solve[(n, t)].append(float(parts[2].split()[2]))
        rb[(n, t)].append(float(parts[4].split()[0]))
med = lambda d, k: statistics.median(d[k])
sizes = sorted({k[0] for k in step}); threads = sorted({k[1] for k in step})
print("| Bodies | " + " | ".join(f"{t} thr" for t in threads) + " | best speed-up |")
print("|---" * (len(threads) + 2) + "|")
for n in sizes:
    base = med(step, (n, 1)); best = min(threads, key=lambda t: med(step, (n, t)))
    cells = [f"{med(step,(n,t)):.2f}" for t in threads]
    print(f"| {n} | " + " | ".join(cells) + f" | {base/med(step,(n,best)):.1f}x at {best} |")
print()
print("Rollback re-sim, restore + 6 frames (ms):")
print("| Bodies | " + " | ".join(f"{t} thr" for t in threads) + " |")
print("|---" * (len(threads) + 1) + "|")
for n in sizes:
    print(f"| {n} | " + " | ".join(f"{med(rb,(n,t)):.1f}" for t in threads) + " |")
print()
print("Narrow phase / solver at the largest size (ms):")
n = sizes[-1]
print(" ".join(f"{t}: {med(coll,(n,t)):.2f}/{med(solve,(n,t)):.2f}" for t in threads))
