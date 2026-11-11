"""Proposer choice by measurement (cuts model v0.2, coordinator condition 3).

A discrete-event simulation of the rule model v0 checks (chained Jolteon: votes to the next leader, a QC enters the
next view, q timeouts make a TC, a block is final when its child from the very next view is certified), with fixed
one-way link delays, a proposal cadence, and an adaptive timeout (3x the member's slowest round trip, doubling after
each consecutive timeout, reset by a QC). It measures finality latency and timeouts under three leader rules:

  rotation      leader(v) = v mod n
  keep          the leader of view v-1 keeps view v if v-1 ended with a QC, else the next member in rotation
  reputation    rotation, skipping the leader of the most recent view that timed out (DiemBFT v4 leader
                reputation with a window of one)

The simulation decides a view's outcome (QC or TC) globally, so every member derives the same leader. pe-agree must
derive it from the certified chain to get the same agreement. Safety is the model's business; this measures speed only.

Usage: python -I leader_sim.py   (prints a table; deterministic)
"""
import heapq

def run(n, d, rule, seconds=60.0, cadence=0.05, crashed=(), fixed_timeout=None):
    q = (n + (n - 1) // 3) // 2 + 1
    rtt = [max(d[i][j] + d[j][i] for j in range(n) if j != i) for i in range(n)]
    ev, seq = [], [0]
    def at(t, *e):
        seq[0] += 1
        heapq.heappush(ev, (t, seq[0]) + e)
    view = [0] * n                 # each member's current view
    votes, touts = {}, {}          # view -> set of voters / timers
    outcome = {}                   # view -> 'qc' | 'tc'
    leaders = {}
    proposed_at = {}               # view -> time the block of that view was proposed
    parent = {}                    # view -> parent view (the block of view p)
    high = [0] * n                 # highest certified view each member knows (0 = genesis)
    fails = [0] * n                # consecutive timeouts per member
    timed_out = [0] * n            # the last view each member timed out in: it no longer votes there
    final, lat, nto = set(), [], 0
    last_prop = [-1.0]

    def leader(v):
        if v in leaders:
            return leaders[v]
        if v == 1 or rule == 'rotation':
            l = (v - 1) % n
        elif rule == 'keep':
            prev = leader(v - 1)
            l = prev if outcome.get(v - 1) == 'qc' else (prev + 1) % n
        else:  # reputation
            bad = None
            for u in range(v - 1, 0, -1):
                if outcome.get(u) == 'tc':
                    bad = leader(u)
                    break
            l = (v - 1) % n
            if l == bad:
                l = (l + 1) % n
        leaders[v] = l
        return l

    def timeout_len(i):
        if fixed_timeout is not None:
            return fixed_timeout
        return min(3 * rtt[i] * (2 ** fails[i]), 10.0)

    def enter(i, v, t):
        if v <= view[i]:
            return
        view[i] = v
        at(t + timeout_len(i), 'timeout', i, v)
        if leader(v) == i and i not in crashed:
            at(max(t, last_prop[0] + cadence), 'propose', i, v)

    for i in range(n):
        enter(i, 1, 0.0)
    while ev:
        t, _, kind, *a = heapq.heappop(ev)
        if t > seconds:
            break
        if kind == 'propose':
            i, v = a
            if view[i] != v:
                continue
            last_prop[0] = t
            proposed_at[v] = t
            parent[v] = high[i]
            for j in range(n):
                at(t + (d[i][j] if j != i else 0.0), 'proposal', j, v, high[i])
        elif kind == 'proposal':
            j, v, qcv = a
            if j in crashed:
                continue
            high[j] = max(high[j], qcv)
            if qcv >= view[j]:
                enter(j, qcv + 1, t)
            if view[j] == v and timed_out[j] < v and outcome.get(v) is None:
                # The vote goes to the next leader as it would be if this view succeeds.
                outcome[v] = 'qc'
                leaders.pop(v + 1, None)
                nl = leader(v + 1)
                del outcome[v]
                leaders.pop(v + 1, None)
                at(t + (d[j][nl] if j != nl else 0.0), 'vote', nl, v, j)
        elif kind == 'vote':
            l, v, j = a
            s = votes.setdefault(v, set())
            s.add(j)
            if len(s) == q and v not in outcome:
                outcome[v] = 'qc'
                for i in range(n):
                    fails[i] = 0
                high[l] = max(high[l], v)
                # Two-chain: the block of v is certified; its parent is final if it is from view v - 1.
                p = parent.get(v, 0)
                if p and p == v - 1 and p not in final:
                    final.add(p)
                    lat.append(t - proposed_at[p])
                enter(l, v + 1, t)
        elif kind == 'timeout':
            i, v = a
            if view[i] != v or i in crashed:
                continue
            nto += 1
            timed_out[i] = v
            fails[i] += 1
            for j in range(n):
                at(t + (d[i][j] if j != i else 0.0), 'tmsg', j, v, i)
        elif kind == 'tmsg':
            j, v, i = a
            if j in crashed:
                continue
            s = touts.setdefault(v, set())
            s.add(i)
            if len(s) >= q and outcome.get(v) is None:
                outcome[v] = 'tc'
            if len(s) >= q:
                enter(j, v + 1, t)
    lat.sort()
    if not lat:
        return dict(final=0, p50=None, p95=None, timeouts=nto)
    return dict(final=len(lat) / seconds, p50=lat[len(lat) // 2], p95=lat[int(len(lat) * 0.95)], timeouts=nto)

def links(n, fast, slow_member=None, slow=None):
    d = [[fast] * n for _ in range(n)]
    if slow_member is not None:
        for j in range(n):
            if j != slow_member:
                d[slow_member][j] = d[j][slow_member] = slow
    return d

SCENARIOS = [
    ("n=2 even links 20 ms", 2, links(2, 0.020), ()),
    ("n=2 one slow member 150 ms", 2, links(2, 0.020, 1, 0.150), ()),
    ("n=3 even links 20 ms", 3, links(3, 0.020), ()),
    ("n=3 one slow member 150 ms", 3, links(3, 0.020, 2, 0.150), ()),
    ("n=3 one member crashed", 3, links(3, 0.020), (2,)),
    ("n=4 one member crashed", 4, links(4, 0.020), (3,)),
]

if __name__ == "__main__":
    print(f"{'scenario':32} {'rule':11} {'final/s':>8} {'p50 ms':>8} {'p95 ms':>8} {'timeouts':>9}")
    for name, n, d, crashed in SCENARIOS:
        for rule in ("rotation", "keep", "reputation"):
            r = run(n, d, rule, crashed=crashed)
            f = lambda x: '-' if x is None else f"{x * 1000:.0f}"
            print(f"{name:32} {rule:11} {r['final']:8.1f} {f(r['p50']):>8} {f(r['p95']):>8} {r['timeouts']:9d}")
    # A fixed timeout below the slow member's round trip, n = 2 (Fable's condition 1).
    for rule in ("rotation", "keep", "reputation"):
        r = run(2, links(2, 0.020, 1, 0.150), rule, fixed_timeout=0.2)
        print(f"{'n=2 slow 150 ms, fixed 200 ms':32} {rule:11} {r['final']:8.1f} {'-' if r['p50'] is None else round(r['p50']*1000):>8} {'':>8} {r['timeouts']:9d}")
