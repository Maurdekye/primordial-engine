"""Summarise an r18-capped.sh run: per member, from out/link-<id>.csv and out/summary-<id>.json.

    python -I crates/pe-app/r18-report.py OUT_DIR [FROM_MS]

Prints, after FROM_MS (default 3000, past the cap's start): uplink kbit/s, time at each congestion level,
the signal against the cap's real queue (member 1, whose uplinks are capped), confirmation lag, and
the summary's drops, checks and loss.
"""
import csv
import json
import os
import sys

d = sys.argv[1]
t0 = int(sys.argv[2]) if len(sys.argv) > 2 else 3000
out = os.path.join(d, "out")
for name in sorted(os.listdir(out)):
    if not (name.startswith("link-") and name.endswith(".csv")):
        continue
    m = name[5:-4]
    rows = [r for r in csv.DictReader(open(os.path.join(out, name))) if int(r["t_ms"]) >= t0]
    if not rows:
        print(f"member {m}: no rows after {t0} ms")
        continue
    kb = [float(r["kbit_s"]) for r in rows]
    lv = [int(r["level"]) for r in rows]
    sig = [float(r["signal_ms"]) for r in rows if r["signal_ms"]]
    tq = [float(r["true_queue_ms"]) for r in rows]
    lag = [int(r["conf_lag"]) for r in rows]
    share = {k: round(100 * lv.count(k) / len(lv)) for k in sorted(set(lv))}
    s = json.load(open(os.path.join(out, f"summary-{m}.json")))
    t = s["traffic"]
    link = s.get("link", {})
    print(f"member {m}: {len(rows)} rows from {t0} ms")
    print(f"  uplink kbit/s mean {sum(kb) / len(kb):.1f}, max {max(kb):.1f}")
    print(f"  level share % {share}, max {max(lv)}")
    if sig:
        print(f"  signal ms mean {sum(sig) / len(sig):.1f}, max {max(sig):.1f}; real cap queue ms mean {sum(tq) / len(tq):.1f}, max {max(tq):.1f}")
    print(f"  confirmation lag frames mean {sum(lag) / len(lag):.1f}, max {max(lag)}")
    print(f"  conf {s['conf']}/{s['frames']}, checks {s['checks_ok']}/{s['checks_bad']}, rollbacks {s['rollbacks']}, cap drops {t.get('datagrams_dropped_by_cap', 0)}, CoDel drops {t.get('datagrams_dropped_by_codel', 0)}, hand entries {link.get('hand_entries', 0)}, rx (received, lost) {link.get('rx', {})}")
