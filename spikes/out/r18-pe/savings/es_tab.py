import os, statistics as st
O = "target/es/sweep"
rows = ["k8-m2","up-k8-m4","up-k12-m4","down-k8-m3","down-k12-m4","down-k16-m4","down-k12-m8","down-k16-m8","down-k24-m8"]
D = [46000, 48000, 50000, 52000, 54000]
def parse(p):
    for line in open(p, encoding="utf-8", errors="replace"):
        if line.startswith("RESULT   batched+adaptive"):
            f = [x.strip() for x in line.split("|")]
            b = f[8]; b = None if b == "never" else float(b.split()[0]) / 1000
            return [int(x) for x in f[4].split("/")], b, f[9].endswith("EQUAL")
    return None, None, None
print(f"{'row':12} {'what-if':7} {'others stalled per member, 46..54 ms':40} {'mean':>6}  back s 46..54 ms")
for r in rows:
    thin = 3 if r.startswith("down") else 1
    for w in [0, 4]:
        ss, bs, eq = [], [], []
        for d in D:
            p = f"{O}/w{w}-{r}-d{d}.log"
            if not os.path.exists(p): continue
            a, b, e = parse(p)
            if a is None: continue
            oth = [x for i, x in enumerate(a) if i + 1 != thin]
            ss.append(sum(oth) / len(oth)); bs.append(b); eq.append(e)
        if not ss: continue
        print(f"{r:12} {('sigs free' if w else 'base'):7} {' '.join(f'{x:6.0f}' for x in ss):40} {st.mean(ss):6.0f}  {' '.join('-' if b is None else f'{b:.1f}' for b in bs)}{'' if all(eq) else '  (not all EQUAL)'}")
