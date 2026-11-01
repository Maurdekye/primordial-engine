"""Makes the cleaned copy of a branch's history for publication (user ruling 2026-11-01 06:55Z): a
fresh clone in a new folder outside any git work tree, rewritten there by git filter-repo with the
replacement rules of a gitignored local file. The repository it is run from is never rewritten.

usage: <python with git-filter-repo> -I scripts/publish_clean.py <new folder> [--ref <branch>] [--build]
       (scripts/publish_clean.sh runs it with $PE_FILTER_PYTHON)

Rules: scripts/publish_replacements.local, or the file named by PE_PUBLISH_REPLACEMENTS (format:
scripts/publish_replacements.example). Each rule is `regex:<python regex>==><replacement>`, applied in
file order to every text blob (--replace-text) and every commit message (--replace-message). In a
binary blob (a NUL in its first 8 KiB, which filter-repo's text replacement skips), each match is
overwritten with as many 'x' bytes, so lengths and length-prefixed formats stay intact. Every commit
is kept, empty or not, and author and committer fields are not touched.

Then it verifies the copy, and exits 1 if any check fails (2 on an error):
  * the same number of commits, with every author and committer field (names, emails, dates) equal;
  * the tip tree equals the source tip's tree, or every difference is listed;
  * scripts/publish_history.py --ignore-identity --blobs, with every term of the terms file
    (PE_PUBLISH_TERMS or scripts/publish_terms.local) as a fail term, finds nothing in added lines,
    messages or any blob of the copy;
  * with --build, `cargo check --workspace --all-targets --offline` passes on the copy's tip
    (through $PE_HEAVY if set; target folder <new folder>.target).
It reports the commits whose ids, trees, own changes or messages changed.
"""

import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)


def die(msg, code=2):
    print(f"publish_clean: {msg}")
    sys.exit(code)


def git(args, cwd, check=True):
    r = subprocess.run(["git", *args], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and r.returncode != 0:
        die(f"git {' '.join(args[:2])} failed in {cwd}: {r.stderr.decode('utf-8', 'replace').strip()}")
    return r.stdout.decode("utf-8", "replace")


def read_rules(path):
    rules = []
    try:
        with open(path, "rb") as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        die(f"no replacement rules at {path} (copy scripts/publish_replacements.example)")
    for line in lines:
        line = line.rstrip(b"\r")
        if not line.strip() or line.startswith(b"#"):
            continue
        if not line.startswith(b"regex:") or b"==>" not in line:
            die(f"bad rule in {path} (want regex:<pattern>==><replacement>): {line.decode('utf-8', 'replace')}")
        pattern, replacement = line[6:].rsplit(b"==>", 1)
        try:
            rules.append((re.compile(pattern), replacement, line))
        except re.error as e:
            die(f"bad regex in {path}: {line.decode('utf-8', 'replace')}: {e}")
    if not rules:
        die(f"{path} holds no rules")
    return rules


def commit_records(cwd, rev):
    """{sha: (tree, identity, message)} for every commit reachable from rev."""
    out = git(["log", "--format=%H%x1f%T%x1f%an%x1f%ae%x1f%ad%x1f%cn%x1f%ce%x1f%cd%x1f%B%x1e", "--date=raw", rev], cwd)
    recs = {}
    for rec in out.split("\x1e"):
        rec = rec.strip("\n")
        if rec:
            f = rec.split("\x1f", 8)
            recs[f[0]] = (f[1], tuple(f[2:8]), f[8])
    return recs


def own_changes(cwd, rev):
    """{sha: raw diff lines of the commit itself (combined for merges)}."""
    out = git(["log", "--raw", "-c", "--no-abbrev", "--no-renames", "--format=%x1e%H", rev], cwd)
    changes = {}
    for block in out.split("\x1e"):
        if block.strip():
            sha, _, body = block.partition("\n")
            changes[sha] = sorted(line for line in body.split("\n") if line.startswith(":"))
    return changes


def tree_listing(cwd, rev):
    out = git(["ls-tree", "-r", "-z", rev], cwd)
    return dict(reversed(e.split("\t", 1)) for e in out.split("\0") if e)


def main():
    sys.stdout.reconfigure(line_buffering=True)
    args = sys.argv[1:]
    if not args or args[0].startswith("-"):
        print(__doc__)
        sys.exit(2)
    out_dir = os.path.abspath(args[0])
    ref, build = "main", False
    rest = args[1:]
    while rest:
        if rest[0] == "--ref" and len(rest) > 1:
            ref, rest = rest[1], rest[2:]
        elif rest[0] == "--build":
            build, rest = True, rest[1:]
        else:
            die(f"unknown argument {rest[0]}")

    try:
        import git_filter_repo as fr
    except ImportError:
        die("this Python has no git_filter_repo: make a venv outside the repository, "
            "`python -m venv <dir>` then `<dir>/Scripts/python -m pip install git-filter-repo`, "
            "and set PE_FILTER_PYTHON to that python")

    rules_path = os.environ.get("PE_PUBLISH_REPLACEMENTS", os.path.join(HERE, "publish_replacements.local"))
    terms_path = os.environ.get("PE_PUBLISH_TERMS", os.path.join(HERE, "publish_terms.local"))
    rules = read_rules(rules_path)
    if not os.path.isfile(terms_path):
        die(f"no terms file at {terms_path} (copy scripts/publish_terms.example)")

    # The copy must be a new folder outside every git work tree, this repository's above all.
    if os.path.exists(out_dir) and (not os.path.isdir(out_dir) or os.listdir(out_dir)):
        die(f"{out_dir} exists and is not an empty folder")
    parent = os.path.dirname(out_dir)
    if not os.path.isdir(parent):
        die(f"{parent} does not exist")
    inside = subprocess.run(["git", "rev-parse", "--is-inside-work-tree"], cwd=parent,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout.strip()
    if inside == b"true":
        die(f"{parent} is inside a git work tree; put the copy somewhere else")

    src_sha = git(["rev-parse", "--verify", f"refs/heads/{ref}^{{commit}}"], REPO).strip()
    print(f"publish_clean: cleaning {ref} at {src_sha[:10]} of {REPO} into {out_dir}")
    git(["clone", "-q", "--no-local", "--single-branch", "--branch", ref, REPO, out_dir], parent)
    if git(["rev-parse", "HEAD"], out_dir).strip() != src_sha:
        die("the clone's HEAD is not the source branch's commit")

    # filter-repo reads the rules itself; it takes no comments, so it gets a stripped copy (outside
    # the clone, next to it).
    stripped = out_dir + ".rules"
    with open(stripped, "wb") as f:
        f.write(b"".join(line + b"\n" for _, _, line in rules))

    def mask_binary(blob, _metadata):
        if b"\0" in blob.data[:8192]:
            for rx, _, _ in rules:
                blob.data = rx.sub(lambda m: b"x" * len(m.group(0)), blob.data)

    os.chdir(out_dir)
    options = fr.FilteringOptions.parse_args([
        "--replace-text", stripped, "--replace-message", stripped,
        "--prune-empty", "never", "--prune-degenerate", "never", "--quiet"])
    fr.RepoFilter(options, blob_callback=mask_binary).run()
    os.remove(stripped)
    os.chdir(REPO)

    # Verification.
    failures = []
    commit_map = {}
    with open(os.path.join(out_dir, ".git", "filter-repo", "commit-map")) as f:
        for line in f.read().split("\n")[1:]:
            if line.strip():
                old, new = line.split()
                commit_map[old] = new
    src = commit_records(REPO, src_sha)
    dst = commit_records(out_dir, "HEAD")
    print(f"  commits: {len(src)} before, {len(dst)} after")
    if len(src) != len(dst) or set(commit_map) != set(src) or set(commit_map.values()) != set(dst):
        failures.append("the commit sets do not correspond one to one")
    src_changes, dst_changes = own_changes(REPO, src_sha), own_changes(out_dir, "HEAD")
    ids = trees = msgs = owns = idents = 0
    for old, new in commit_map.items():
        if old not in src or new not in dst:
            continue
        ids += old != new
        trees += src[old][0] != dst[new][0]
        msgs += src[old][2] != dst[new][2]
        owns += src_changes.get(old) != dst_changes.get(new)
        idents += src[old][1] != dst[new][1]
    print(f"  changed: {ids} commit ids, {trees} trees, {owns} commits' own changes, {msgs} messages")
    print(f"  author and committer fields changed: {idents}")
    if idents:
        failures.append(f"{idents} commits' author or committer fields changed")

    src_tip, dst_tip = tree_listing(REPO, src_sha), tree_listing(out_dir, "HEAD")
    differ = sorted(p for p in set(src_tip) | set(dst_tip) if src_tip.get(p) != dst_tip.get(p))
    print(f"  tip tree: " + ("equal to the source tip's" if not differ else f"{len(differ)} paths differ"))
    for p in differ:
        print(f"    differs: {p}")
    if differ:
        failures.append("the tip tree differs from the source tip's")

    r = subprocess.run([sys.executable, "-I", os.path.join(HERE, "publish_history.py"), "--ignore-identity",
                        "--blobs", terms_path, terms_path, "HEAD"], cwd=out_dir)
    if r.returncode != 0:
        failures.append(f"publish_history.py on the copy exited {r.returncode}")

    if build:
        env = dict(os.environ, CARGO_TARGET_DIR=out_dir + ".target")
        cmd = ["cargo", "check", "--workspace", "--all-targets", "--offline", "-q"]
        heavy = os.environ.get("PE_HEAVY")
        if heavy:
            cmd = [os.environ.get("PE_BASH", "bash"), heavy, *cmd]
        r = subprocess.run(cmd, cwd=out_dir, env=env)
        print(f"  build of the copy's tip: {'ok' if r.returncode == 0 else f'FAILED (exit {r.returncode})'}")
        if r.returncode != 0:
            failures.append("the copy's tip does not build")

    if failures:
        for f in failures:
            print(f"publish_clean: FAIL: {f}")
        sys.exit(1)
    print(f"publish_clean: PASS, the cleaned copy is {out_dir} (branch {ref}, tip {git(['rev-parse', 'HEAD'], out_dir).strip()[:10]})")


main()
