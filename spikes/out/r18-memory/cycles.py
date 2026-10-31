import csv, sys, os, statistics as st
base=sys.argv[1]
print("| run | cycles | period s (mean) | real queue ms mean / max | per-cycle queue peak ms (median) | level shares % |")
print("|---|---|---|---|---|---|")
for name in sorted(d for d in os.listdir(base) if os.path.isdir(os.path.join(base,d))):
    rows=list(csv.DictReader(open(os.path.join(base,name,'link-1.csv'))))
    rows=[r for r in rows if int(r['t_ms'])>=3000]
    lv=[int(r['level']) for r in rows]; t=[int(r['t_ms']) for r in rows]; q=[float(r['true_queue_ms']) for r in rows]
    # a cycle starts where the level rises after it last fell (or from the start's floor)
    starts=[]; last_dir=-1
    for i in range(1,len(lv)):
        if lv[i]>lv[i-1]:
            if last_dir<0: starts.append(i)
            last_dir=1
        elif lv[i]<lv[i-1]: last_dir=-1
    per=[(t[b]-t[a])/1000 for a,b in zip(starts,starts[1:])]
    peaks=[max(q[a:b]) for a,b in zip(starts,starts[1:])]
    sh={l:round(100*lv.count(l)/len(lv)) for l in range(6) if lv.count(l)}
    print(f"| {name} | {len(per)} | {st.mean(per):.1f} | {st.mean(q):.0f} / {max(q):.0f} | {st.median(peaks):.0f} | {sh} |")
