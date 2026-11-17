"""Tier 2's cross-platform determinism check (docs/refound-plan.md 4.6; docs/landing-gates.md, "Tier 2").

usage: python -I scripts/tier2_digests.py [--annotate] <platform folder>...

Each folder is one platform's output of scripts/tier2.sh, named for the platform, with or without a "tier2-" prefix
(its digests/release.txt and
digests/debug.txt hold the RESULT lines its tests printed, sorted). The first folder is the reference: hosted CI
gives windows-x64's, the platform the committed records were made on. For each profile, every other platform's
lines must be the same as the reference's, line for line. It prints each line that differs, and with --annotate
also writes them as GitHub error annotations, which the public API serves (scripts/ci_status.py --run).
A profile with no line at all on the reference fails too: a comparison of nothing is not a check.
Exit 0 if every platform has the same digests, 1 if one differs or has none, 2 on a usage error.
"""

import os
import sys
from collections import Counter

PROFILES = ("release", "debug")
SHOW = 20


def read(folder, profile):
    try:
        with open(os.path.join(folder, "digests", profile + ".txt"), encoding="utf-8") as f:
            return [line.rstrip("\r\n") for line in f if line.strip()]
    except FileNotFoundError:
        return None


def esc(s):
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def main():
    args = sys.argv[1:]
    annotate = "--annotate" in args
    folders = [a for a in args if a != "--annotate"]
    if len(folders) < 2 or any(a.startswith("--") for a in folders):
        print(__doc__)
        return 2
    names = [os.path.basename(os.path.normpath(f)).removeprefix("tier2-") for f in folders]
    bad = 0

    def report(profile, name, text, lines):
        nonlocal bad
        bad += 1
        print(f"tier2_digests: DIFFERENT: {profile}, {name}: {text}")
        for line in lines[:SHOW]:
            print(f"  {line}")
        if len(lines) > SHOW:
            print(f"  ... and {len(lines) - SHOW} more")
        if annotate:
            # One annotation per difference, its lines joined by escaped newlines (GitHub keeps 10 per step). A
            # property's value may hold no comma or colon unescaped; the title is built without either.
            print(f"::error title=tier 2 digests {profile} {name}::{esc(chr(10).join([text] + lines[:SHOW]))}")

    for profile in PROFILES:
        ref = read(folders[0], profile)
        if not ref:
            report(profile, names[0], f"the reference has no RESULT line ({'no digests file' if ref is None else 'empty'})", [])
            continue
        want = Counter(ref)
        same = []
        for folder, name in zip(folders[1:], names[1:]):
            got = read(folder, profile)
            if got is None:
                report(profile, name, "no digests (its tier-2 job ended before writing them)", [])
                continue
            have = Counter(got)
            if have == want:
                same.append(name)
                continue
            only_ref = sorted((want - have).elements())
            only_here = sorted((have - want).elements())
            report(profile, name, f"{len(only_ref)} of {names[0]}'s line(s) missing, {len(only_here)} line(s) not in it",
                   [f"{names[0]}: {x}" for x in only_ref] + [f"{name}: {x}" for x in only_here])
        if same:
            print(f"tier2_digests: {profile}: {', '.join(same)} the same as {names[0]} ({len(ref)} lines)")
    if bad:
        print(f"tier2_digests: FAIL: {bad} difference(s) above")
        return 1
    print(f"tier2_digests: PASS: {', '.join(names)} have the same digests")
    return 0


if __name__ == "__main__":
    sys.exit(main())
