"""History half of scripts/publish_check.sh (--history): scans the commits of a revision range for
terms that must not be published, in each commit's added lines (git log -p; merges as git's
combined diff, so only what a merge itself introduced) and in its message, author and committer.

usage: python -I scripts/publish_history.py <terms file> <fail-terms file> <revision args>...

Both files use publish_check.sh's format (`fixed <text>` / `word <text>`, # comments). Every term of
either file is reported with its counts. Only the terms of the fail-terms file fail the check (exit
1, each hit printed); a missing fail-terms file is an empty list. Exit 2 on an error. Python rather
than grep: msys grep aborts on input as large as `git log -p`.
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
            sys.exit(f"publish_history: no terms file at {path}")
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


def git(args):
    r = subprocess.run(["git", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if r.returncode != 0:
        print(f"publish_history: git {args[0]} failed: {r.stderr.decode('utf-8', 'replace').strip()}")
        sys.exit(2)
    return r.stdout.decode("utf-8", "replace")


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        sys.exit(2)
    report = read_terms(sys.argv[1], True)
    fail = read_terms(sys.argv[2], False)
    revs = sys.argv[3:]
    if not report:
        print(f"publish_history: {sys.argv[1]} lists no terms")
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
    # combined diff); a line is added when its prefix holds a '+'.
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
        elif width and "+" in line[:width] and "-" not in line[:width]:
            text = line[width:]
            for t in terms:
                if match[t](text):
                    added[t].append((sha, path, text))

    print(f"publish_history: {commits} commits in {' '.join(revs)}")
    failed = 0
    for t in terms:
        kind, text = t
        shas = list(dict.fromkeys(c for c, _, _ in added[t]))
        fields = sorted({f for _, f in meta[t]})
        mshas = list(dict.fromkeys(c for c, _ in meta[t]))
        tag = "FAIL" if t in failing and (added[t] or meta[t]) else ("checked" if t in failing else "reported")
        listed = " ".join(c[:7] for c in shas[:8]) + (f" +{len(shas) - 8} more" if len(shas) > 8 else "")
        print(f"  [{tag}] {kind} {text!r}: {len(added[t])} added line(s) in {len(shas)} commit(s)"
              + (f" ({listed})" if shas else "")
              + (f"; in the {'/'.join(fields)} of {len(mshas)} commit(s)" if mshas else ""))
        if t in failing:
            for c, p, line in added[t]:
                print(f"    {c[:10]}:{p}: {line.strip()[:200]}")
            for c, f in meta[t]:
                print(f"    {c[:10]} ({f})")
            failed += len(added[t]) + len(meta[t])
    if failed:
        print(f"publish_history: FAIL, {failed} hit(s) of the {len(fail)} fail term(s)")
        sys.exit(1)
    print(f"publish_history: PASS, no hit of the {len(fail)} fail term(s); the other terms are reported only")


main()
