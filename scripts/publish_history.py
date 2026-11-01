"""History half of scripts/publish_check.sh (--history): scans the commits of a revision range for
terms that must not be published, in each commit's added lines (git log -p; merges as git's
combined diff, so only what a merge itself introduced) and in its message, author and committer.

usage: python -I scripts/publish_history.py [--ignore-identity] [--blobs] <terms file> <fail-terms file> <revision args>...

Both files use publish_check.sh's format (`fixed <text>` / `word <text>`, # comments). Every term of
either file is reported with its counts. Only the terms of the fail-terms file fail the check (exit
1, each hit printed); a missing fail-terms file is an empty list. Exit 2 on an error. Python rather
than grep: msys grep aborts on input as large as `git log -p`.
  --ignore-identity  author and committer hits are reported but never fail (the commit email the
                     user accepted on 2026-10-31 11:49Z, which the published copy keeps).
  --blobs            also scan every blob reachable from the range, byte for byte, binary ones
                     included (the diff scan reads text diffs only).
"""

import re
import subprocess
import sys


def read_terms(path, required):
    try:
        with open(path, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        if required:
            print(f"publish_history: no terms file at {path}")
            sys.exit(2)
        return []
    terms = []
    for line in lines:
        line = line.rstrip("\r")
        if not line or line.startswith("#"):
            continue
        kind, _, text = line.partition(" ")
        if kind not in ("fixed", "word") or not text:
            print(f"publish_history: bad line in {path}: {line}")
            sys.exit(2)
        terms.append((kind, text))
    return terms


def matcher(kind, text):
    if kind == "fixed":
        needle = text.casefold()
        return lambda s: needle in s.casefold()
    pattern = re.compile(r"(?<![A-Za-z0-9_])" + re.escape(text) + r"(?![A-Za-z0-9_])", re.IGNORECASE)
    return lambda s: pattern.search(s) is not None


def bytes_matcher(kind, text):
    # ASCII case folding, as git grep -i does; the terms are ASCII.
    if kind == "fixed":
        needle = text.encode().lower()
        return lambda b: needle in b.lower()
    pattern = re.compile(rb"(?<![A-Za-z0-9_])" + re.escape(text.encode()) + rb"(?![A-Za-z0-9_])", re.IGNORECASE)
    return lambda b: pattern.search(b) is not None


def git(args):
    r = subprocess.run(["git", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if r.returncode != 0:
        print(f"publish_history: git {args[0]} failed: {r.stderr.decode('utf-8', 'replace').strip()}")
        sys.exit(2)
    return r.stdout.decode("utf-8", "replace")


def scan_blobs(revs, terms):
    """{term: [(path, binary)]} over every blob reachable from revs (each blob once, first path)."""
    paths = {}
    for line in git(["rev-list", "--objects", *revs]).split("\n"):
        sha, _, path = line.partition(" ")
        if sha and path:
            paths.setdefault(sha, path)
    match = {t: bytes_matcher(*t) for t in terms}
    hits = {t: [] for t in terms}
    p = subprocess.Popen(["git", "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    count = 0
    for sha, path in paths.items():
        p.stdin.write(sha.encode() + b"\n")
        p.stdin.flush()
        _, kind, size = p.stdout.readline().split()
        data = p.stdout.read(int(size))
        p.stdout.read(1)
        if kind != b"blob":
            continue
        count += 1
        binary = b"\0" in data[:8192]
        for t in terms:
            if match[t](data):
                hits[t].append((path, binary))
    p.stdin.close()
    if p.wait() != 0:
        print("publish_history: git cat-file failed")
        sys.exit(2)
    return count, hits


def main():
    args = sys.argv[1:]
    ignore_identity = blobs = False
    while args and args[0] in ("--ignore-identity", "--blobs"):
        ignore_identity |= args[0] == "--ignore-identity"
        blobs |= args[0] == "--blobs"
        args = args[1:]
    if len(args) < 3:
        print(__doc__)
        sys.exit(2)
    report = read_terms(args[0], True)
    fail = read_terms(args[1], False)
    revs = args[2:]
    if not report:
        print(f"publish_history: {args[0]} lists no terms")
        sys.exit(2)
    terms = list(dict.fromkeys(report + fail))
    failing = set(fail)
    match = {t: matcher(*t) for t in terms}
    added = {t: [] for t in terms}  # (commit, path, line)
    meta = {t: [] for t in terms}  # (commit, field)

    # Messages, authors and committers.
    log = git(["log", "--format=%H%x1f%an <%ae>%x1f%cn <%ce>%x1f%B%x1e", *revs])
    commits = 0
    for rec in log.split("\x1e"):
        rec = rec.strip("\n")
        if not rec:
            continue
        commits += 1
        sha, author, committer, message = rec.split("\x1f", 3)
        for t in terms:
            for field, value in (("author", author), ("committer", committer), ("message", message)):
                if match[t](value):
                    meta[t].append((sha, field))

    # Added lines. A hunk header of k '@' has k-1 prefix columns (2 for a diff, 3 for a two-parent
    # combined diff), one per parent. A line is added when every column is '+': a merge line with a
    # ' ' in some column already exists in that parent (--cc shows it only as conflict context).
    diff = git(["log", "-p", "--cc", "--no-color", "--no-ext-diff", "--no-textconv", "--format=%x1e%H", *revs])
    sha = path = None
    width = 0
    for line in diff.split("\n"):
        if line.startswith("\x1e"):
            sha, path, width = line[1:], None, 0
        elif line.startswith("diff "):
            width = 0
        elif width == 0 and line.startswith("+++ "):
            path = line[4:]
            path = path[2:] if path.startswith("b/") else path
        elif line.startswith("@@"):
            width = len(line) - len(line.lstrip("@")) - 1
        elif width and line[:width] == "+" * width:
            text = line[width:]
            for t in terms:
                if match[t](text):
                    added[t].append((sha, path, text))

    nblobs, bhits = scan_blobs(revs, terms) if blobs else (0, {t: [] for t in terms})

    print(f"publish_history: {commits} commits in {' '.join(revs)}" + (f", {nblobs} blobs" if blobs else ""))
    failed = 0
    for t in terms:
        kind, text = t
        counted = [m for m in meta[t] if not (ignore_identity and m[1] != "message")]
        shas = list(dict.fromkeys(c for c, _, _ in added[t]))
        fields = sorted({f for _, f in meta[t]})
        mshas = list(dict.fromkeys(c for c, _ in meta[t]))
        n = len(added[t]) + len(counted) + len(bhits[t])
        tag = "FAIL" if t in failing and n else ("checked" if t in failing else "reported")
        listed = " ".join(c[:7] for c in shas[:8]) + (f" +{len(shas) - 8} more" if len(shas) > 8 else "")
        nbin = sum(1 for _, b in bhits[t] if b)
        print(f"  [{tag}] {kind} {text!r}: {len(added[t])} added line(s) in {len(shas)} commit(s)"
              + (f" ({listed})" if shas else "")
              + (f"; in the {'/'.join(fields)} of {len(mshas)} commit(s)" if mshas else "")
              + (f"; in {len(bhits[t])} blob(s), {nbin} binary" if blobs else ""))
        if t in failing:
            for c, p, line in added[t]:
                print(f"    {c[:10]}:{p}: {line.strip()[:200]}")
            for c, f in counted:
                print(f"    {c[:10]} ({f})")
            for p, b in bhits[t]:
                print(f"    blob at {p}" + (" (binary)" if b else ""))
            failed += n
    if ignore_identity:
        print("publish_history: author and committer hits are reported only (--ignore-identity)")
    if failed:
        print(f"publish_history: FAIL, {failed} hit(s) of the {len(fail)} fail term(s)")
        sys.exit(1)
    print(f"publish_history: PASS, no hit of the {len(fail)} fail term(s); the other terms are reported only")


main()
