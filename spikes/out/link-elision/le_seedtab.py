import os, re, statistics as st, sys
O = "target/le/seeds"
rows = ["up-k8-m2","up-k8-m4","up-k12-m4","down-k8-m3","down-k8-m4","down-k12-m4","down-k12-m8","down-k16-m8"]
def parse(p):
    for line in open(p, encoding="utf-8", errors="replace"):
        if line.startswith("RESULT   batched+adaptive"):
            f = [x.strip() for x in line.split("|")]
            st_ = [int(x) for x in f[4].split("/")]
            b = f[8]; b = None if b == "never" else float(b.split()[0]) / 1000
            # others' stalls: every member but the thin one (1 for up, 3 for down)
            return st_, b
    return None, None
print(f"{'row':12} {'mode':4} {'others stalled (mean per member) by delay 46-54 ms':58} {'mean':>6} {'back s by delay 46-54 ms':44} never")
for r in rows:
    thin = 1 if r.startswith("up") else 3
    for mode in ["off", "on"]:
        ss, bs = [], []
        for s in [46000, 48000, 50000, 52000, 54000]:
            p = f"{O}/{mode}-{r}-s{s}.log"
            if not os.path.exists(p): continue
            a, b = parse(p)
            if a is None: continue
            oth = [x for i, x in enumerate(a) if i + 1 != thin]
            ss.append(sum(oth) / len(oth)); bs.append(b)
        if not ss: continue
        bstr = " ".join("-" if b is None else f"{b:.1f}" for b in bs)
        print(f"{r:12} {mode:4} {' '.join(f'{x:6.0f}' for x in ss):58} {st.mean(ss):6.0f} {bstr:44} {sum(b is None for b in bs)}")
