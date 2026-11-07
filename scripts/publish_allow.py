"""Allowlist for scripts/publish_check.sh: lets an exact, reviewed line through a term check.

Measured output sometimes holds a number that happens to equal a term (2026-11-07: a millisecond
figure in a simulation log equal to a digits-only term). Rather than weaken the term, one exact line
of one file is allowed: the allowlist holds the file's path and the SHA-256 of that line, never the
term. A changed line has another hash and fails again. Every allowed hit is still reported.

The allowlist is scripts/publish_allow.local, or the file named by PE_PUBLISH_ALLOW. It is gitignored
like the terms: a published list of which lines match a term would point at the term. Format, one
entry per line: `<sha256 of the line> <path>`, the line being the file's bytes between two line
breaks, without the \\n and a trailing \\r. Blank lines and lines starting with # are ignored.
`python -I scripts/publish_allow.py hash <path> <line number>` prints the entry for a committed line
of HEAD's tree (or of --ref <ref>).

usage (from publish_check.sh): git grep -z -n ... <ref> | python -I scripts/publish_allow.py filter <ref>
  prints each hit as `<ref>:<path>:<n>:<line>`, or `allowed: <ref>:<path>:<n> (line sha256 <12 hex>)`
  for an allowed one.
"""

import hashlib
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def allow_path():
    return os.environ.get("PE_PUBLISH_ALLOW", os.path.join(HERE, "publish_allow.local"))


def load(path=None):
    """{(path, sha256 hex)}; empty when the file is missing."""
    path = path or allow_path()
    entries = set()
    try:
        with open(path, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        return entries
    for line in lines:
        line = line.rstrip("\r")
        if not line.strip() or line.startswith("#"):
            continue
        digest, _, p = line.partition(" ")
        if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest) or not p:
            print(f"publish_allow: bad line in {path}: {line}")
            sys.exit(2)
        entries.add((p, digest))
    return entries


def line_hash(line):
    """SHA-256 hex of one line's bytes, without its line ending."""
    if line.endswith(b"\n"):
        line = line[:-1]
    if line.endswith(b"\r"):
        line = line[:-1]
    return hashlib.sha256(line).hexdigest()


def main():
    args = sys.argv[1:]
    if args[:1] == ["filter"] and len(args) == 2:
        ref = args[1].encode()
        allowed = load()
        for rec in sys.stdin.buffer.read().split(b"\n"):
            if not rec:
                continue
            parts = rec.split(b"\0", 2)
            if len(parts) != 3:
                # Not `name\0n\0text`, e.g. git grep's "Binary file <ref>:<path> matches": always a
                # hit, printed as is (the allowlist holds text lines only).
                print(rec.decode("utf-8", "replace"))
                continue
            name, n, text = parts
            path = name[len(ref) + 1:] if name.startswith(ref + b":") else name
            digest = line_hash(text)
            shown = f"{args[1]}:{path.decode('utf-8', 'replace')}:{n.decode()}"
            if (path.decode("utf-8", "replace"), digest) in allowed:
                print(f"allowed: {shown} (line sha256 {digest[:12]})")
            else:
                print(f"{shown}:{text.decode('utf-8', 'replace').rstrip(chr(13))}")
        return
    if args[:1] == ["hash"] and len(args) in (3, 5):
        ref = "HEAD"
        if len(args) == 5 and args[3] == "--ref":
            ref = args[4]
        data = subprocess.run(["git", "show", f"{ref}:{args[1]}"], capture_output=True, check=True).stdout
        line = data.split(b"\n")[int(args[2]) - 1]
        print(f"{line_hash(line)} {args[1]}")
        return
    print(__doc__)
    sys.exit(2)


main()
