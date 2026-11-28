"""Applies publication replacement rules to the tracked text files of a checkout, in place, as
scripts/publish_clean.py applies them to every text blob of the published copy: each rule over the
whole text, in file order. Binary files (a NUL in the first 8 KiB) and allowlisted lines
(scripts/publish_allow.py) are left as they are. It is idempotent: a second run changes nothing.

Use it on a branch so its text matches what main holds after a cleaning rule was added (the changed
lines then merge cleanly), and on main itself for a fix-forward. It only rewrites files; commit the
result yourself.

usage: python -I scripts/publish_apply.py <rules file> [--allow <allowlist>] [--check] [<path>...]
  <rules file>  publish_clean.py's format: `regex:<python regex>==><replacement>`, # comments.
  --check       change nothing; list the files the rules would change and exit 1 if there are any.
  <path>...     limit to these tracked paths (default: every tracked file).
Exit 0 when done (or nothing to change), 1 with --check when something would change, 2 on an error.
"""
import hashlib
import os
import re
import subprocess
import sys


def die(msg):
    print(f"publish_apply: {msg}")
    sys.exit(2)


def read_rules(path):
    rules = []
    try:
        lines = open(path, "rb").read().splitlines()
    except OSError as e:
        die(f"can't read the rules: {e}")
    for line in lines:
        line = line.rstrip(b"\r")
        if not line.strip() or line.startswith(b"#"):
            continue
        if not line.startswith(b"regex:") or b"==>" not in line:
            die(f"bad rule (want regex:<pattern>==><replacement>): {line.decode('utf-8', 'replace')}")
        pattern, replacement = line[6:].rsplit(b"==>", 1)
        try:
            rules.append((re.compile(pattern), replacement))
        except re.error as e:
            die(f"bad regex {line.decode('utf-8', 'replace')}: {e}")
    if not rules:
        die("the rules file holds no rules")
    return rules


def read_allow(path):
    hashes = set()
    if path and os.path.isfile(path):
        for line in open(path, encoding="utf-8").read().splitlines():
            if line.strip() and not line.startswith("#"):
                hashes.add(line.split(" ", 1)[0])
    return hashes


def line_hash(line):
    return hashlib.sha256(line[:-1] if line.endswith(b"\r") else line).hexdigest()


def clean(data, rules, allowed):
    def replace(text):
        for rx, rep in rules:
            text = rx.sub(rep, text)
        return text
    pieces = data.split(b"\n")
    keep = [bool(allowed) and line_hash(p) in allowed for p in pieces]
    if not any(keep):
        return replace(data)
    out, run = [], []
    for p, k in zip(pieces, keep):
        if k:
            if run:
                out.append(replace(b"\n".join(run)))
                run = []
            out.append(p)
        else:
            run.append(p)
    if run:
        out.append(replace(b"\n".join(run)))
    return b"\n".join(out)


def bare_crs(data):
    return data.count(b"\r") - data.count(b"\r\n")


def clean_file(data, rules, allowed):
    """clean() on the text git stores. A checkout with core.autocrlf (Windows) holds CRLF files whose
    blobs are LF: a rule that joins lines would eat the LF and leave the CR (rustfmt refuses a bare CR in
    a doc comment). So a CRLF file is cleaned in its LF form, as publish_clean.py sees the blob, and
    written back with CRLF if every line had it (a mixed file is written in the LF form, which is what
    git stores anyway). Returns None if the result would hold a bare CR the input didn't."""
    if b"\r\n" not in data:
        new = clean(data, rules, allowed)
    else:
        lf = data.replace(b"\r\n", b"\n")
        new = clean(lf, rules, allowed)
        if new == lf:
            return data
        if b"\n" not in data.replace(b"\r\n", b""):
            new = new.replace(b"\n", b"\r\n")
    return None if bare_crs(new) > bare_crs(data) else new


def main():
    args = sys.argv[1:]
    if not args:
        die("usage: publish_apply.py <rules file> [--allow <allowlist>] [--check] [<path>...]")
    rules = read_rules(args.pop(0))
    allow, check, paths = None, False, []
    while args:
        a = args.pop(0)
        if a == "--allow":
            allow = args.pop(0) if args else die("--allow needs a file")
        elif a == "--check":
            check = True
        else:
            paths.append(a)
    allowed = read_allow(allow)
    top = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True)
    if top.returncode:
        die("not inside a git checkout")
    os.chdir(top.stdout.strip())
    ls = subprocess.run(["git", "ls-files", "-z", "--", *paths], capture_output=True, check=True).stdout
    changed, bad = [], []
    for name in [p.decode("utf-8", "surrogateescape") for p in ls.split(b"\0") if p]:
        if not os.path.isfile(name) or os.path.islink(name):
            continue
        data = open(name, "rb").read()
        if b"\0" in data[:8192]:
            continue
        new = clean_file(data, rules, allowed)
        if new is None:
            bad.append(name)
        elif new != data:
            changed.append((name, new))
    if bad:
        for name in bad:
            print(f"publish_apply: {name}: the rules would leave a bare CR (a CR without its LF)")
        die(f"{len(bad)} file(s) would get a bare CR; nothing was changed")
    if not check:
        for name, new in changed:
            open(name, "wb").write(new)
    changed = [name for name, _ in changed]
    for name in changed:
        print(("would change " if check else "changed ") + name)
    print(f"publish_apply: {len(changed)} file(s) {'to change' if check else 'changed'}")
    sys.exit(1 if check and changed else 0)


if __name__ == "__main__":
    main()
