#!/usr/bin/env bash
# P2-3 S4: shows that the claim rule (D72, 8.4) is the one change S4 makes, and what it changes.
# 1. Runs every scenario on pe-session with the spike's claim rule (PE_CLAIM_RULE=spike): every log
#    must equal the committed logs of the commit before S4 (ref, default 8668173), timings masked.
# 2. Runs both rules with every touch's outcome dumped (PE_DUMP_OUTCOMES) and counts, per scenario,
#    the touches whose outcome changed, by kind: pushes the spike rule refused that D72 accepts, and
#    anything else (a consequence of those: other bodies moved, so later touches met other states).
# Output in spikes/out/claim-rules/. usage: scripts/compare_claim_rules.sh [ref]
set -u
cd "$(dirname "$0")/.."
REF=${1:-8668173}
O=spikes/out/claim-rules
rm -rf $O; mkdir -p $O
PE_CLAIM_RULE=spike PE_OUT=$O/spike bash scripts/run_session_sim.sh --pe > $O/spike-summary.txt 2>&1
mask() { sed -E 's/[0-9]+(\.[0-9]+)? ?(ms|us|µs|s)\b/T/g'; }
same=0; differ=0
for f in $(git ls-tree -r --name-only "$REF" spikes/out/session-sim-pe/ | grep '\.log$'); do
  rel=${f#spikes/out/session-sim-pe/}
  if diff -q <(git show "$REF:$f" | mask) <(mask < "$O/spike/$rel") > /dev/null; then same=$((same+1)); else echo "DIFFER (spike rule) $rel"; differ=$((differ+1)); fi
done
echo "step 1: spike claim rule vs $REF: $same logs identical, $differ different (timings masked)"
PE_DUMP_OUTCOMES=1 PE_CLAIM_RULE=spike PE_OUT=$O/spike-out bash scripts/run_session_sim.sh --pe > /dev/null 2>&1
PE_DUMP_OUTCOMES=1 PE_OUT=$O/d72-out bash scripts/run_session_sim.sh --pe > /dev/null 2>&1
python -I - "$O" <<'PY'
import os, sys, re, collections
o = sys.argv[1]
def load(d):
    r = {}
    for root, _, fs in os.walk(d):
        for f in fs:
            if f.endswith('.log'):
                for l in open(os.path.join(root, f), encoding='utf-8'):
                    if l.startswith('OUTCOME '):
                        _, label, w, m, s, kind, out = l.rstrip('\n').split(' ', 6)
                        r[(os.path.relpath(os.path.join(root, f), d), label, w, m, s)] = (kind, out)
    return r
a, b = load(o + '/spike-out'), load(o + '/d72-out')
per = collections.defaultdict(collections.Counter)
for k in sorted(set(a) | set(b)):
    x, y = a.get(k), b.get(k)
    if x == y:
        per[k[0]]['same'] += 1
    elif x and y and x[0] == 'push' and re.match(r'Rejected(Claimed|Cooldown)', x[1]) and y[1] == 'Accepted':
        per[k[0]]['push refused -> accepted'] += 1
    else:
        per[k[0]]['other: %s %s -> %s' % ((x or y)[0], x[1].split(' ')[0] if x else 'absent', y[1].split(' ')[0] if y else 'absent')] += 1
with open(o + '/outcome-changes.txt', 'w', encoding='utf-8', newline='\n') as out:
    tot = collections.Counter()
    for f in sorted(per):
        c = per[f]; tot.update(c)
        out.write('%s: %s\n' % (f, ', '.join('%s %d' % kv for kv in sorted(c.items()))))
    out.write('TOTAL: %s\n' % ', '.join('%s %d' % kv for kv in sorted(tot.items())))
print(open(o + '/outcome-changes.txt', encoding='utf-8').read())
PY
