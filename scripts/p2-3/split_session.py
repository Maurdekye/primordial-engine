import os, re, subprocess, sys
src = sys.argv[1] if len(sys.argv) > 1 else 'crates/pe-session/src/session.rs'
OUT = sys.argv[2] if len(sys.argv) > 2 else 'crates/pe-session/src/session'
L = open(src, encoding='utf-8').read().split('\n')
n = len(L)
def ln(i): return L[i-1]
def find(pat, start=1):
    for i in range(start, n+1):
        if re.match(pat, ln(i)): return i
    raise SystemExit('not found: ' + pat)
def back(i):
    while i > 1 and re.match(r'\s*(///|#\[)', ln(i-1)): i -= 1
    return i
impl_start = find(r'^impl WorldSim \{$')
impl_end = find(r'^\}$', impl_start)
assert impl_end < find(r'^// ---- offline reference')
top = [  # (line, dest)
    (1, 'mod'),
    (find(r'^pub type MemberId'), 'entry'),
    (back(find(r'^pub const NO_ROUND')), 'membership'),
    (back(find(r'^pub struct Entry \{')), 'entry'),
    (find(r'^// ---- R14 proofs'), 'r14'),
    (find(r'^// ---- scenes'), 'sim'),
    (find(r'^// ---- rollback state'), 'mod'),
    (impl_start, None),
]
inner = [
    (impl_start + 1, 'mod'),
    (find(r'^    // ---- snapshots', impl_start), 'sim'),
    (find(r'^    // ---- log ', impl_start), 'log'),
    (back(find(r'^    fn on_proof\(', impl_start)), 'evict'),
    (find(r'^    // ---- view change', impl_start), 'membership'),
    (back(find(r'^    fn want_from\(', impl_start)), 'fetch'),
    (back(find(r'^    fn evaluate_checks\(', impl_start)), 'check'),
    (find(r'^    // ---- R14 attestations', impl_start), 'r14'),
    (find(r'^    // ---- simulation', impl_start), 'sim'),
    (impl_end, None),
]
after = (impl_end + 1, find(r'^// ---- offline reference'))
out = {k: {'top': [], 'impl': []} for k in ['mod', 'entry', 'membership', 'log', 'evict', 'fetch', 'check', 'r14', 'sim', 'replay']}
for (a, d), (b, _) in zip(top, top[1:]):
    out[d]['top'].append(L[a-1:b-1])
for (a, d), (b, _) in zip(inner, inner[1:]):
    out[d]['impl'].append(L[a-1:b-1])
assert all(ln(i).strip() == '' for i in range(after[0], after[1]))
out['replay']['top'].append(L[after[1]-1:])
covered = sum(len(c) for v in out.values() for k in v.values() for c in k) + 2 + (after[1] - after[0])
assert covered == n, (covered, n)

docs = {
    'entry': 'Entries, messages and their byte encoding; the same-frame tie rank.',
    'membership': 'Membership: epochs, quorum rules, equivocation proofs and the view change (P2-4, design 10.8).',
    'log': 'The entry log: authoring, insertion, hash chains, validation, stable data and the confirmed frame.',
    'evict': 'Suspicion, frozen heads, eviction votes and lying-vote checks (design 10.8, review rounds 2-3).',
    'fetch': 'Fetch repair: requests for missing chain ranges, with backoff, and their answers.',
    'check': 'Sync checks: comparing check hashes at confirmed, fully simulated frames.',
    'r14': 'R14 cross-world transfers: facts, attestations, certificates and P2-5 provenance.',
    'sim': 'Scenes, keyframes and state hashes, frame application and stepping, rollback.',
    'replay': 'The offline reference: re-simulating a world from a final log.',
}
def fix_vis(lines, in_impl):
    r = []
    inherent = False
    for s in lines:
        if in_impl:
            s = re.sub(r'^    fn ', '    pub(super) fn ', s)
        else:
            if s.startswith('impl'):
                inherent = ' for ' not in s
            s = re.sub(r'^(fn|const|struct|enum) ', r'pub(super) \1 ', s)
            if inherent:
                s = re.sub(r'^    fn ', '    pub(super) fn ', s)
        r.append(s)
    return r
def strip_blank(ch):
    while ch and ch[0].strip() == '': ch = ch[1:]
    while ch and ch[-1].strip() == '': ch = ch[:-1]
    return ch
os.makedirs(OUT, exist_ok=True)
for name, v in out.items():
    if name == 'mod': continue
    parts = ['//! ' + docs[name], '', 'use super::*;', '']
    for ch in v['top']:
        parts += strip_blank(fix_vis(ch, False)) + ['']
    if v['impl']:
        parts.append('impl WorldSim {')
        for i, ch in enumerate(v['impl']):
            if i: parts.append('')
            parts += strip_blank(fix_vis(ch, True))
        parts.append('}')
        parts.append('')
    open(f'{OUT}/{name}.rs', 'w', encoding='utf-8', newline='\n').write('\n'.join(parts))
# mod.rs
head = strip_blank(out['mod']['top'][0])
mods = ['entry', 'membership', 'log', 'evict', 'fetch', 'check', 'r14', 'sim', 'replay']
layout = ['//!', '//! Layout (P2-3 S1, a structure-preserving port of the spike core at b01ebdf): this module holds', '//! the state types and [`WorldSim`]; each submodule adds the methods for one concern:']
for m in mods:
    layout.append(f'//! * `{m}`: ' + docs[m][0].lower() + docs[m][1:])
# insert layout after the //! block
k = max(i for i, s in enumerate(head) if s.startswith('//!')) + 1
head = head[:k] + layout + head[k:]
body = head + [''] + [f'mod {m};' for m in mods] + [''] + [f'pub use {m}::*;' for m in mods] + ['']
for ch in out['mod']['top'][1:]:
    body += strip_blank(ch) + ['']
body.append('impl WorldSim {')
body += strip_blank(out['mod']['impl'][0])
body += ['}', '']
open(f'{OUT}/mod.rs', 'w', encoding='utf-8', newline='\n').write('\n'.join(body))
if len(sys.argv) <= 1:
    os.remove(src)
print({k: sum(len(c) for c in v['top']) + sum(len(c) for c in v['impl']) for k, v in out.items()})
