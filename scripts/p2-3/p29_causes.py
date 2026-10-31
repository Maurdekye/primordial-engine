"""P2-9 spike comparison: for every committed spike log, why pe-session's golden differs from it.

Three logs per scenario: the spike's (spikes/out/session-sim), pe-session's under the spike's claim
rule (PE_CLAIM_RULE=spike: target/r7-spike/session-sim-pe for the harness suite, target/r7-spike-tool
for the tool's faults/), and pe-session's golden (spikes/out/session-sim-pe, D72).
  spike -> pe(spike rule): what pe-session's types change. Equal once entry ids, transfer ids and
  digests are masked: "ids and hashes" (SHA-256 entry ids, hash-derived transfer ids, the state and
  check hash composition). Equal only once byte counts are masked too: plus "wire size".
  pe(spike rule) -> pe golden: "D72 touch acceptance".
usage: python -I p29_causes.py <repo>   (prints a Markdown table and a summary)
"""
import os
import re
import sys

R = sys.argv[1]
SP = os.path.join(R, 'spikes/out/session-sim')
PE = os.path.join(R, 'spikes/out/session-sim-pe')
PES = [os.path.join(R, 'target/r7-spike/session-sim-pe'), os.path.join(R, 'target/r7-spike-tool')]
CAPPED = {'fault-capped', 'fault-capped-collapse', 'fault-capped-r18'}


def timings(l):
    l = re.sub(r'[0-9]+(\.[0-9]+)? ?(ms|us|µs|s)\b', 'T', l)
    return l.replace('(X ms)', '(T)')


def norm(l, bytes_too):
    l = timings(l)
    l = re.sub(r'\b[0-9a-f]{16}\b', 'ID', l)
    l = re.sub(r'T?[0-9]{15,20}\b', 'ID', l)
    if bytes_too:
        l = re.sub(r'[0-9]+ B\b', 'NB', l)
        l = re.sub(r'[0-9]+ B/s', 'NB/s', l)
    l = re.sub(r'\[([^\[\]]*)\]', lambda m: '[' + ', '.join(sorted((e.strip() for e in re.findall(r'\s*("[^"]*"|[^,]+)', m.group(1))))) + ']', l)
    m = re.match(r'^(.*?: )(m\d+s\d+@.*)$', l)
    if m:
        l = m.group(1) + ', '.join(sorted(s.strip() for s in re.split(r', (?=m\d+s\d+@)', m.group(2))))
    return l


def read(p):
    return open(p, encoding='utf-8').read().rstrip('\n').split('\n')


def cls(a, b):
    if [timings(x) for x in a] == [timings(x) for x in b]:
        return 'identical'
    if sorted(norm(x, False) for x in a) == sorted(norm(x, False) for x in b):
        return 'ids'
    if sorted(norm(x, True) for x in a) == sorted(norm(x, True) for x in b):
        return 'ids+bytes'
    return 'differ'


def first_diff(a, b):
    na, nb = sorted(norm(x, True) for x in a), sorted(norm(x, True) for x in b)
    sa, sb = set(na), set(nb)
    x = next((l for l in na if l not in sb), '')
    y = next((l for l in nb if l not in sa), '')
    return x[:140], y[:140]


rows, tally = [], {}
for root, _, fs in os.walk(SP):
    for f in sorted(fs):
        if not f.endswith('.log'):
            continue
        rel = os.path.relpath(os.path.join(root, f), SP).replace('\\', '/')
        name = rel[:-4]
        pe = os.path.join(PE, rel)
        pes = next((os.path.join(d, rel) for d in PES if os.path.exists(os.path.join(d, rel))), None)
        if not os.path.exists(pe):
            rows.append((name, 'no pe-session golden', '')); continue
        s, g = read(os.path.join(SP, rel)), read(pe)
        if cls(s, g) == 'identical':
            causes = ['none (identical)']
        elif pes is None:
            causes = ['NO SPIKE-RULE RUN']
        else:
            p = read(pes)
            a, b = cls(s, p), cls(p, g)
            causes = []
            if name in CAPPED:
                causes.append('wire size: caps scaled (fault-capped section)')
            elif a == 'ids':
                causes.append('ids and hashes')
            elif a == 'ids+bytes':
                causes.append('ids and hashes; wire size')
            elif a == 'differ':
                x, y = first_diff(s, p)
                causes.append(f'OTHER: `{x}` -> `{y}`')
            if b != 'identical':
                causes.append('D72 touch acceptance')
        c = '; '.join(causes)
        tally[c] = tally.get(c, 0) + 1
        rows.append((name, c, ''))
print('| scenario | cause of the difference from the spike |')
print('|---|---|')
for n, c, _ in rows:
    print(f'| {n} | {c} |')
print()
for c, n in sorted(tally.items(), key=lambda x: -x[1]):
    print(f'- {n}: {c}')
print(f'- total: {len(rows)}')
