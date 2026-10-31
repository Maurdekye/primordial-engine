"""P2-3 S6b: move the view change's state out of WorldSim into ViewChangeMembership (field `vcm`).

usage: python -I p23_seam.py <dir with mod.rs and the other session modules>

Deterministic and idempotent, so it can be applied to the base and theirs splits of a spike
session.rs before a three-way merge of a later protocol round.
"""
import os
import re
import sys

D = sys.argv[1]
F = ['members', 'epoch', 'lagging', 'proofs', 'liars', 'frozen', 'votes', 'evicted', 'cut_ids', 'cut_done', 'vc',
     'vc_later', 'proposal_ids', 'decided', 'vc_log', 'withhold_vc', 'followers', 'join_requests', 'admitted',
     'join_caps', 'ack_epoch_from', 'ref_advanced', 'withhold_checked', 'byz_ally', 'byz_no_precommit', 'roles',
     'role_defs', 'join_rules', 'join_attests', 'owners', 'admin_pending', 'admin_outcomes', 'closed_at',
     'fake_ack_epoch', 'newer_since', 'adopting', 'strikes', 'entry_wants', 'entry_asked', 'value_wants',
     'value_asked']
# Fields that exist in a given version (a later round may add or drop some).
p = os.path.join(D, 'mod.rs')
s = open(p, encoding='utf-8').read()
if 'pub struct ViewChangeMembership' not in s:
    a = s.index('pub struct WorldSim {')
    b = s.index('\n}\n', a)
    lines = s[a:b].split('\n')
    keep, moved = [], []
    i = 0
    while i < len(lines):
        j = i
        while j < len(lines) and re.match(r'    (///|#\[)', lines[j]):
            j += 1
        m = re.match(r'    (pub )?([a-z_]+):', lines[j]) if j < len(lines) else None
        if m and m.group(2) in F:
            moved += lines[i:j + 1]
        else:
            keep += lines[i:j + 1]
        i = j + 1
    present = [f for f in F if re.search(r'\n    (pub )?%s:' % f, s[a:b])]
    keep.insert(len(keep), '    /// The membership and view-change state (P2-3 S6b): read through [`Membership`], written by')
    keep.append('    /// the view change.')
    keep.append('    pub vcm: ViewChangeMembership,')
    vcm = ['', '/// Everything the view change (10.8, P2-4) and the D80 admin layer keep: members, epochs, votes,',
           '/// cuts, joins, roles. The protocol core reads it through [`Membership`]; the view change',
           '/// (membership.rs, evict.rs) writes its fields. A plain struct the view change extends.',
           '#[derive(Clone)]', 'pub struct ViewChangeMembership {'] + moved + ['}']
    s = s[:a] + '\n'.join(keep) + '\n}\n' + '\n'.join(vcm) + s[b + 2:]
    # initializers in new()
    a = s.index('    pub fn new(world_id')
    b = s.index('\n    }\n', a)
    body = s[a:b].split('\n')
    out, inits, at = [], [], None
    for ln in body:
        m = re.match(r'            ([a-z_]+): ', ln)
        if m and m.group(1) in present:
            if at is None:
                at = len(out)
                out.append(None)
            inits.append('    ' + ln)
        else:
            out.append(ln)
    out[at] = '            vcm: ViewChangeMembership {\n' + '\n'.join(inits) + '\n            },'
    s = s[:a] + '\n'.join(out) + s[b:]
    open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('ok')
