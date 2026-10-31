"""P2-3 S6b, part 2: inside `impl WorldSim` blocks only, `self.<view-change field>` becomes
`self.vcm.<field>` (also across a line break: `self\n    .votes`). Idempotent.

usage: python -I p23_seam_rename.py <dir>
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
D = sys.argv[1]
F = ['members', 'epoch', 'lagging', 'proofs', 'liars', 'frozen', 'votes', 'evicted', 'cut_ids', 'cut_done', 'vc',
     'vc_later', 'proposal_ids', 'decided', 'vc_log', 'withhold_vc', 'followers', 'join_requests', 'admitted',
     'join_caps', 'ack_epoch_from', 'ref_advanced', 'withhold_checked', 'byz_ally', 'byz_no_precommit', 'roles',
     'role_defs', 'join_rules', 'join_attests', 'owners', 'admin_pending', 'admin_outcomes', 'closed_at',
     'fake_ack_epoch', 'newer_since', 'adopting', 'strikes', 'entry_wants', 'entry_asked', 'value_wants',
     'value_asked']
PAT = re.compile(r'\bself(\s*)\.(?!vcm\.)(' + '|'.join(F) + r')\b(?!\s*\()')
BLOCK = re.compile(r'^impl WorldSim \{\n.*?^\}\n', re.S | re.M)


def in_worldsim(t):
    out, i = [], 0
    for m in BLOCK.finditer(t):
        out.append(t[i:m.start()])
        out.append(PAT.sub(lambda x: 'self' + x.group(1) + '.vcm.' + x.group(2), m.group(0)))
        i = m.end()
    out.append(t[i:])
    return ''.join(out)


for f in sorted(os.listdir(D)):
    if f.endswith('.rs'):
        q = os.path.join(D, f)
        t = open(q, encoding='utf-8').read()
        t2 = in_worldsim(t)
        if t2 != t:
            open(q, 'w', encoding='utf-8', newline='\n').write(t2)
print('ok')
