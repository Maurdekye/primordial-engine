"""Summary of the two-PC c-to-F sessions (A on the desktop, B on reneglade). usage: python summarize.py <dir>"""
import glob
import os
import json
import statistics as st
import sys

D = sys.argv[1] if len(sys.argv) > 1 else '.'


def lines(pattern):
    out = []
    for f in sorted(glob.glob(D + '/' + pattern)):
        # pe-app's own notices (a member's connection dropping) share stdout; only JSON lines are data.
        out += [(f, json.loads(l)) for l in open(f, encoding='utf-8') if l.startswith('{')]
    return out


def summ(name, xs):
    xs = sorted(xs)
    return f"{name}: n={len(xs)} median {st.median(xs):.0f} [{xs[0]:.0f}-{xs[-1]:.0f}]"


rows = [r for _, r in lines('session*/A.jsonl')]
out = []
for cold in (True, False):
    r = [x for x in rows if x['cold'] == cold]
    out.append('COLD' if cold else 'WARM')
    out.append('  ' + summ('F-c frames', [x['F_minus_c_frames'] for x in r]))
    out.append('  ' + summ('F-c ms', [x['F_minus_c_ms'] for x in r]))
    out.append('  ' + summ('author call ms', [x['author_call_ms'] for x in r]))
    out.append('  NotReady retries: ' + str([x['not_ready_retries'] for x in r]))
    for k in ['A_corrected', 'A_final']:
        out.append('  ' + summ(k + ' ms after call', [x['wall_ms_after_call'][k] for x in r]))
    out.append('  prediction from c+1: ' + str(all(x['A_prediction_runs_from'] == x['c'] + 1 for x in r)))

# B's own view, per session: pending (with c) and active (runs from F + 1), by frame and B's clock.
agree, b_ms, voids, n = True, [], 0, 0
for f in sorted(glob.glob(D + '/session*/B.jsonl')):
    sess = os.path.basename(os.path.dirname(f))
    a_rows = {x['c']: x for _, x in lines(sess + '/A.jsonl')}
    bl = [b for _, b in lines(sess + '/B.jsonl')]
    pend = {b['asset']: b for b in bl if b['event'] == 'pending'}
    voids += sum(1 for b in bl if b['event'] == 'void')
    for b in bl:
        if b['event'] == 'active':
            p = pend[b['asset']]
            n += 1
            b_ms.append(b['t_ms'] - p['t_ms'])
            agree &= a_rows[p['c']]['F'] == b['runs_from'] - 1
out.append(f"B: {n} activated, {voids} voids; B's own pending-to-active: " + summ('ms', b_ms))
out.append("B's F equals A's for every commit: " + str(agree))
s = '\n'.join(out)
print(s)
open(D + '/summary.txt', 'w', encoding='utf-8', newline='\n').write(s + '\n')
