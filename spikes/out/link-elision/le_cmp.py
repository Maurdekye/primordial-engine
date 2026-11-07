"""Link elision: every cell, elision on against off (= main). Usage: python -I le_cmp.py <off dir> <on dir>"""
import os, re, sys
a_dir, b_dir = sys.argv[1], sys.argv[2]
def ms(s):
    s = s.strip()
    return None if s == "never" else float(s.split()[0])
def parse(p):
    d = {"stalled": None, "back": None, "end": None, "lag": None, "feed": None, "wire": None, "later": []}
    for line in open(p, encoding="utf-8", errors="replace"):
        if line.startswith("RESULT   batched+adaptive"):
            f = [x.strip() for x in line.split("|")]
            d["wire"] = [float(x) for x in f[1].split("/")]
            d["stalled"] = [int(x) for x in f[4].split("/")]
            d["back"] = ms(f[8]); d["end"] = "EQUAL" if f[9].endswith("EQUAL") else "DIFFER"
        elif "mean confirmation lag per member" in line:
            d["lag"] = [float(x) for x in line.split(":")[-1].split("/")]
        elif "time on the state feed per member" in line:
            d["feed"] = [int(x) for x in line.split(":")[-1].replace("ms", "").split("/")]
        elif re.search(r"RESULT +(second gesture|gesture \d)", line):
            m = re.search(r"stalled frames during it ([0-9 /]+); back to real time after it ([^;]+);", line)
            d["later"].append((sum(int(x) for x in m.group(1).split("/")), ms(m.group(2))))
    return d
worse, better, same = [], [], []
for f in sorted(os.listdir(b_dir)):
    if not f.endswith(".log"): continue
    a, b = parse(os.path.join(a_dir, f)), parse(os.path.join(b_dir, f))
    if a["stalled"] is None: continue
    sa, sb = sum(a["stalled"]), sum(b["stalled"])
    ba, bb = a["back"], b["back"]
    inf = 1e9
    nv = lambda x: inf if x is None else x
    la, lb = (sum(a["lag"]) if a["lag"] else 0), (sum(b["lag"]) if b["lag"] else 0)
    later_a = sum(x[0] for x in a["later"]); later_b = sum(x[0] for x in b["later"])
    w = (sb > sa + 10 and sb > sa * 1.1) or (nv(bb) > nv(ba) + 500) or (b["end"] != "EQUAL" and a["end"] == "EQUAL") or (later_b > later_a + 10 and later_b > later_a * 1.1)
    g = (sa > sb + 10 and sa > sb * 1.1) or (nv(ba) > nv(bb) + 500) or (later_a > later_b + 10)
    lagd = "" if not a["lag"] else f" lag {la/len(a['lag']):.1f}->{lb/len(b['lag']):.1f}"
    line = f"{f[:-4]:24} stalled {sa}->{sb} back {ba}->{bb} later {later_a}->{later_b} {a['end']}->{b['end']}{lagd} wire1 {a['wire'][0]}->{b['wire'][0]}"
    (worse if w else better if g else same).append(line)
print(f"WORSE {len(worse)}"); print("\n".join(worse))
print(f"BETTER {len(better)}"); print("\n".join(better))
print(f"SAME (within 10 frames / 10 %, 500 ms) {len(same)}")
