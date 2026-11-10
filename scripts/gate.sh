#!/usr/bin/env bash
# The landing gate: the native-code audit (the "100% Rust" ruling, D1), a secret scan and, where this clone
# names its term files, the publication check, on a commit before it lands on main. The reference-transaction
# hook (scripts/hooks/, installed by scripts/install_gate_hook.sh) runs it on every update of main, and the
# hosted CI workflow (.github/workflows/gate.yml) on every push and pull request. docs/landing-gates.md
# describes it.
#
#   bash scripts/gate.sh [--base <rev>] [--full-history] [--report <folder>] [<rev>]
#
# <rev> (default HEAD) is the commit to land; --base (default main) is the commit it lands on.
#
# The gate never judges a change to itself with the changed code (coordinator ruling m81aa9eb7a675). When
# there is a base, the gate's code comes from it:
#   * this script (it hands over to the base's copy at once), scripts/audit_native.py, scripts/install_gitleaks.sh
#     (the pinned gitleaks and the SHA-256 of its binary), the hook, the publication check's scripts;
#   * the rules of .gitleaks.toml, and the cargo and rustup configuration files the audit runs with.
# Only the allow files come from <rev>: scripts/native_allow.txt, and the [[allowlists]] entries of
# .gitleaks.toml. A landing that changes any of these files gets a NOTE naming them, and its reviewer approves
# those lines. A change to the gate's code takes effect from the next landing. With --full-history, or when the
# base has no gate yet, the code comes from <rev>.
#
# The steps:
#   1. export the tree of <rev>, byte for byte, with no attributes applied (export-ignore, eol conversion or a
#      filter can't hide or change a file), and check the export against git ls-tree. A submodule, a symbolic
#      link, or a path that can't be written as committed fails the gate;
#   2. secret scan: gitleaks over the added lines of every commit in <base>..<rev> (a merge as its diff to its
#      first parent), over those commits' messages, and over the export; with --full-history, over every commit
#      <rev> reaches. No .gitleaksignore counts: one in the tree fails the gate. Inline `gitleaks:allow`
#      comments are ignored;
#   3. native audit: scripts/audit_native.py --check over the export (every Cargo workspace in it) with <rev>'s
#      allow file. For this step the export's cargo and rustup configuration files are the base's (a committed
#      .cargo/config.toml can name a program for cargo to run);
#   4. publication check (scripts/publish_check.sh), when this clone names its term file: the tree of <rev>,
#      and, blocking, the added lines and messages of <base>..<rev> (identities are left to the publishing step,
#      which maps them). The terms are local files that must never be published, so hosted CI skips this step;
#      a whole-history run reports the old history's terms without failing on them (the user kept that
#      history, 2026-10-31 11:49Z).
# Every git command runs with in-tree attributes off (GIT_ATTR_SOURCE is the empty tree), so a committed
# .gitattributes can't change what a scan reads.
#
# Exit 0 pass, 1 fail, 2 error. Findings print with the secret redacted. --report <folder> keeps the audit's
# markdown and the scans' JSON (redacted) there.
# Environment: PE_GITLEAKS (a gitleaks binary; it must be the pinned build), PE_PYTHON (a Python 3.10+),
# PE_GATE_WRAP (a command line to run the audit under, such as the org's build-slot wrapper; read with eval,
# so quote paths in it as in a shell), PE_PUBLISH_TERMS and PE_PUBLISH_ALLOW (the term file and the
# allowlist; default: git config pe.gate.terms and pe.gate.allow).
set -u
die() { echo "gate: ERROR: $*" >&2; exit 2; }

base=main rev= full=0 report=
while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || die "--base needs a revision"; base=$2; shift 2 ;;
    --full-history) full=1; shift ;;
    --report) [ $# -ge 2 ] || die "--report needs a folder"; report=$2; shift 2 ;;
    -*) die "unknown option $1 (usage: scripts/gate.sh [--base <rev>] [--full-history] [--report <folder>] [<rev>])" ;;
    *) [ -z "$rev" ] || die "one revision only"; rev=$1; shift ;;
  esac
done
rev=${rev:-HEAD}
here=$(pwd)
if [ -n "$report" ]; then mkdir -p "$report" && report=$(cd "$report" && pwd) || die "can't make $report"; fi

# Revisions resolve in the current worktree (its own HEAD); gitleaks reads the shared object store.
gitdir=$(git rev-parse --path-format=absolute --git-common-dir 2> /dev/null) || die "not in a git repository"
# In-tree attributes off for every git command: export-ignore, -diff, eol or a filter in a committed .gitattributes
# would otherwise change what the gate reads. GIT_ATTR_SOURCE needs git 2.40 or later; an older git ignores it
# without a word, so the gate refuses to run on one.
gv=$(git version) || die "git does not run"
case "$gv" in
  "git version 2."[4-9][0-9]*|"git version 2."[1-9][0-9][0-9]*|"git version "[3-9]*) ;;
  *) die "$gv is older than 2.40, which the gate needs (GIT_ATTR_SOURCE keeps a committed .gitattributes from hiding lines)" ;;
esac
GIT_ATTR_SOURCE=$(git hash-object -t tree /dev/null) || die "git hash-object failed"
export GIT_ATTR_SOURCE
G() { git "$@"; }
sha=$(G rev-parse -q --verify "$rev^{commit}") || die "$rev is not a commit"
zero=0000000000000000000000000000000000000000
if [ "$full" = 1 ] || [ -z "$base" ] || [ "$base" = "$zero" ]; then
  range=$sha bsha= what="every commit $rev reaches"
else
  bsha=$(G rev-parse -q --verify "$base^{commit}") || die "--base $base is not a commit"
  range="$bsha..$sha" what="$(G rev-list --count "$bsha..$sha") commit(s) in ${bsha:0:10}..${sha:0:10}"
fi

# The gate's code: the base's, when the base has a gate.
code=$sha
if [ -n "$bsha" ] && G cat-file -e "$bsha:scripts/gate.sh" 2> /dev/null; then code=$bsha; fi
# Hand over to the code's copy of this script, unless this is it.
if [ "${PE_GATE_CODE:-}" != "$code" ]; then
  export PE_GATE_REV_NAME=$rev
  self=$(mktemp -d) || die "no temporary folder"
  G show "$code:scripts/gate.sh" > "$self/gate.sh" || { rm -rf "$self"; die "can't read scripts/gate.sh at ${code:0:10}"; }
  if [ -n "$bsha" ]; then set -- --base "$bsha"; else set -- --full-history; fi
  if [ -n "$report" ]; then set -- "$@" --report "$report"; fi
  PE_GATE_CODE=$code bash "$self/gate.sh" "$@" "$sha"
  rc=$?
  rm -rf "$self"
  exit $rc
fi

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) win=1 glos=windows ;;
  Linux) win=0 glos=linux ;;
  Darwin) win=0 glos=darwin ;;
  *) die "unsupported OS $(uname -s)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) glarch=x64 ;;
  aarch64|arm64) glarch=arm64 ;;
  *) die "unsupported CPU $(uname -m)" ;;
esac
native() { if [ "$win" = 1 ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
py=${PE_PYTHON:-}
if [ -z "$py" ]; then
  if [ "$win" = 1 ]; then py=python; else py=$(command -v python3 || command -v python) || die "no python3"; fi
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The gate's code files, from $code.
c="$tmp/code"
mkdir -p "$c/scripts"
for f in scripts/audit_native.py scripts/install_gitleaks.sh .gitleaks.toml; do
  G show "$code:$f" > "$c/$f" 2> /dev/null || die "${code:0:10} has no $f; the gate needs it"
done

# gitleaks: PE_GITLEAKS, this checkout's or the main checkout's .tools, then PATH. Whichever it finds must be
# the build install_gitleaks.sh pins, checked by its SHA-256: a landing could commit a .tools folder.
want=$(sed -n 's/^VERSION=//p' "$c/scripts/install_gitleaks.sh")
pin=$(sed -n "s/^BIN_SHA256_${glos}_${glarch}=\([0-9a-f]\{64\}\)\$/\1/p" "$c/scripts/install_gitleaks.sh")
[ -n "$want" ] || die "${code:0:10}'s scripts/install_gitleaks.sh names no gitleaks VERSION"
[ -n "$pin" ] || die "${code:0:10}'s scripts/install_gitleaks.sh pins no gitleaks binary for ${glos}_${glarch}"
exe=gitleaks; [ "$win" = 1 ] && exe=gitleaks.exe
gl=${PE_GITLEAKS:-}
if [ -z "$gl" ]; then
  for d in "$(git rev-parse --show-toplevel 2> /dev/null)" "$(dirname "$gitdir")"; do
    [ -n "$d" ] && [ -x "$d/.tools/gitleaks-$want/$exe" ] && { gl="$d/.tools/gitleaks-$want/$exe"; break; }
  done
fi
[ -n "$gl" ] || gl=$(command -v gitleaks 2> /dev/null) || true
[ -n "$gl" ] || die "gitleaks $want not found: run bash scripts/install_gitleaks.sh (or set PE_GITLEAKS)"
got=$("$py" -I -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' \
  "$(native "$gl")") || die "can't read $gl"
[ "$got" = "$pin" ] || die "$gl is not the pinned gitleaks $want build for ${glos}_${glarch} (SHA-256 $got, pinned $pin); run bash scripts/install_gitleaks.sh"

# 1. The export of <rev>: every blob byte for byte, no attributes, checked against git ls-tree.
x="$tmp/tree"
"$py" -I - "$sha" "$(native "$x")" > "$tmp/export.out" 2>&1 << 'PYEOF'
import os, re, subprocess, sys, tempfile
sha, dest = sys.argv[1], sys.argv[2]


def git(args):
    r = subprocess.run(["git", *args], capture_output=True)
    if r.returncode != 0:
        print(f"git {' '.join(args)} failed: {r.stderr.decode('utf-8', 'replace').strip()}")
        sys.exit(2)
    return r.stdout


RESERVED = re.compile(r"(?i)^(con|prn|aux|nul|com[0-9]|lpt[0-9])(\..*)?$")
BADCH = set('<>:"|?*\\') | {chr(i) for i in range(32)}
problems, entries, seen = [], [], {}
for rec in git(["ls-tree", "-r", "-z", "--full-tree", sha]).split(b"\0"):
    if not rec:
        continue
    meta, raw = rec.split(b"\t", 1)
    mode, kind, oid = meta.decode().split(" ")
    try:
        path = raw.decode("utf-8")
    except UnicodeDecodeError:
        problems.append(f"{raw!r}: a path that isn't UTF-8")
        continue
    if mode == "160000":
        problems.append(f"{path}: a submodule; its files aren't in this repository, so the gate can't check them")
        continue
    if mode == "120000":
        problems.append(f"{path}: a symbolic link; the gate checks regular files only")
        continue
    if kind != "blob" or mode not in ("100644", "100755"):
        problems.append(f"{path}: a {kind} of mode {mode}, which the gate can't export")
        continue
    if any(p in ("", ".", "..") or p.lower() == ".git" or p[-1] in ". " or RESERVED.match(p) or set(p) & BADCH
           for p in path.split("/")):
        problems.append(f"{path}: a path that can't be written on Windows as committed")
        continue
    if path.lower() in seen:
        problems.append(f"{path}: differs from {seen[path.lower()]} only in case (one hides the other on Windows)")
        continue
    seen[path.lower()] = path
    entries.append((oid, path))
if not problems:
    with tempfile.TemporaryFile() as ids:
        ids.write(b"".join(oid.encode() + b"\n" for oid, _ in entries))
        ids.seek(0)
        proc = subprocess.Popen(["git", "cat-file", "--batch"], stdin=ids, stdout=subprocess.PIPE)
        for oid, path in entries:
            head = proc.stdout.readline().split()
            if len(head) != 3 or head[0].decode() != oid or head[1] != b"blob":
                print(f"{path}: git cat-file --batch answered {head!r}")
                sys.exit(2)
            data = proc.stdout.read(int(head[2]))
            proc.stdout.read(1)
            target = os.path.join(dest, *path.split("/"))
            try:
                os.makedirs(os.path.dirname(target), exist_ok=True)
                with open(target, "xb") as f:
                    f.write(data)
            except OSError as e:
                problems.append(f"{path}: can't be written as committed ({e.strerror or e})")
        proc.stdout.read()
        if proc.wait() != 0:
            print("git cat-file --batch failed")
            sys.exit(2)
    found = sum(len(files) for _, _, files in os.walk(dest))
    missing = [p for _, p in entries if not os.path.isfile(os.path.join(dest, *p.split("/")))]
    if found != len(entries) or missing:
        problems.append(f"the export holds {found} files where git ls-tree lists {len(entries)}"
                        + (f"; missing: {', '.join(missing[:5])}" if missing else ""))
for p in problems:
    print(f"export: {p}")
if problems:
    sys.exit(1)
print(f"export: {len(entries)} files, as committed")
PYEOF
rc=$?
case $rc in
  0) ;;
  1) sed 's/^export: /gate:   /' "$tmp/export.out"
     echo "gate: FAIL for ${sha:0:10}: its tree can't be checked as committed (above)"; exit 1 ;;
  *) sed 's/^/gate:   /' "$tmp/export.out" | tail -10; die "could not export the tree of $rev" ;;
esac
for f in scripts/native_allow.txt; do [ -f "$x/$f" ] || die "$rev has no $f; the gate needs it"; done

name=${PE_GATE_REV_NAME:-$rev}
echo "gate: checking ${sha:0:10}$([ "$name" = "$sha" ] || echo " ($name)") with the gate's code as of ${code:0:10}; secret scan over $what, their messages and the tree"
if [ -n "$bsha" ] && [ "$code" = "$sha" ]; then
  echo "gate: NOTE: the base ${bsha:0:10} has no gate yet, so this landing is judged by its own code"
fi
if [ -n "$bsha" ]; then
  allowd=$(G diff --name-only "$bsha" "$sha" -- .gitleaks.toml scripts/native_allow.txt)
  coded=$(G diff --name-only "$bsha" "$sha" -- scripts/gate.sh scripts/audit_native.py scripts/install_gitleaks.sh \
    scripts/install_gate_hook.sh scripts/hooks scripts/publish_check.sh scripts/publish_history.py \
    scripts/publish_allow.py .github/workflows ':(glob,icase)**/.cargo/config' ':(glob,icase)**/.cargo/config.toml' \
    ':(glob,icase)**/rust-toolchain' ':(glob,icase)**/rust-toolchain.toml')
  if [ -n "$allowd" ]; then
    echo "gate: NOTE: this landing changes the gate's allow files, which apply to it; its reviewer approves these lines:"
    echo "$allowd" | sed 's/^/gate:   /;s/\.gitleaks\.toml$/.gitleaks.toml (its [[allowlists]] entries apply now; any other change from the next landing)/'
  fi
  if [ -n "$coded" ] && [ "$code" = "$bsha" ]; then
    echo "gate: NOTE: this landing changes the gate's code; it is judged by the base's, the change applies from the next landing (hosted CI runs the landing's workflow at once), and its reviewer approves it:"
    echo "$coded" | sed 's/^/gate:   /'
  elif [ -n "$coded" ]; then
    echo "gate: NOTE: this landing changes the gate's code, which judges it (the base has none); its reviewer approves it:"
    echo "$coded" | sed 's/^/gate:   /'
  fi
fi

fail=0
# 2. The secret scan. Exit 3 means leaks; anything else but 0 is an error.
# .gitleaks.toml: the base's rules, and <rev>'s [[allowlists]] entries, checked to hold nothing else.
"$py" -I - "$(native "$c/.gitleaks.toml")" "$(native "$x/.gitleaks.toml")" "$(native "$tmp/gitleaks.toml")" \
  "${sha:0:10}" > "$tmp/config.out" 2>&1 << 'PYEOF'
import re, sys
base_path, rev_path, out_path, rev_name = sys.argv[1:5]
HEADER = re.compile(r"^\s*(\[\[?)\s*([A-Za-z0-9_.\-]+)\s*(\]\]?)\s*(#.*)?$")
KEYS = {"description", "targetRules", "condition", "paths", "regexTarget", "regexes", "stopwords", "commits"}
KEYLINE = re.compile(r"^\s*([A-Za-z]+)\s*=(.*)$")
STRING = re.compile(r"'''(?:(?!''').)*'''|'[^'\n]*'|\"(?:[^\"\\\n]|\\.)*\"")


def blocks(lines, track_strings):
    """Splits TOML lines at table headers: [(is an [[allowlists]] block, [(line number, line)])]."""
    out, cur, kind, open_ml = [], [], False, None
    for n, line in enumerate(lines, 1):
        m = HEADER.match(line) if open_ml is None else None
        if m:
            out.append((kind, cur))
            cur, kind = [], m.group(1, 2, 3) == ("[[", "allowlists", "]]")
        cur.append((n, line))
        if track_strings:
            for q in ("'''", '"""'):
                if open_ml in (None, q) and line.count(q) % 2:
                    open_ml = q if open_ml is None else None
    out.append((kind, cur))
    return out


def tokens(text, depth):
    """The array depth after `text` if it holds only strings, commas, one level of brackets and an end
    comment; None if it holds anything else."""
    i = 0
    while i < len(text):
        ch = text[i]
        if ch in " \t\r":
            i += 1
        elif ch == "#":
            break
        elif ch in "[]":
            depth += 1 if ch == "[" else -1
            if depth not in (0, 1):
                return None
            i += 1
        elif ch == ",":
            i += 1
        else:
            m = STRING.match(text, i)
            if not m:
                return None
            i = m.end()
    return depth


def check(block):
    depth = 0
    for n, line in block[1:]:
        if depth == 0:
            if not line.strip() or line.strip().startswith("#"):
                continue
            m = KEYLINE.match(line)
            d = tokens(m.group(2), 0) if m and m.group(1) in KEYS else None
        else:
            d = tokens(line, depth)
        if d is None:
            return f"line {n}: {line.strip()[:120]!r}"
        depth = d
    return f"line {block[-1][0]}: an array that isn't closed" if depth else None


try:
    base = open(base_path, encoding="utf-8").read().splitlines()
except OSError as e:
    print(f"can't read the base's .gitleaks.toml: {e}")
    sys.exit(2)
try:
    rev = open(rev_path, encoding="utf-8").read().splitlines()
except FileNotFoundError:
    rev = []
kept = [line for allow, blk in blocks(base, True) if not allow for _, line in blk]
added, count = [], 0
for allow, blk in blocks(rev, False):
    if allow:
        err = check(blk)
        if err:
            print(f"{rev_name}'s .gitleaks.toml, [[allowlists]] at line {blk[0][0]}, {err}: the gate takes only "
                  f"[[allowlists]] entries from a landing, one key per line ({', '.join(sorted(KEYS))}), "
                  f"with string values or arrays of strings")
            sys.exit(2)
        added += [line for _, line in blk]
        count += 1
with open(out_path, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(kept + ["", f"# The [[allowlists]] entries of {rev_name}'s .gitleaks.toml (scripts/gate.sh)"]
                      + added) + "\n")
print(f"{count} [[allowlists]] entries from {rev_name}")
PYEOF
rc=$?
[ $rc = 0 ] || { sed 's/^/gate:   /' "$tmp/config.out"; die "the secret scan's configuration could not be built (exit $rc)"; }
# No ignore file counts. gitleaks reads a .gitleaksignore at the folder -i names (an empty one here) and at the
# root of the folder it scans, which for the message scan is the working folder: so the commit and message scans
# run from the empty folder; one committed in <rev> fails the gate, and is moved aside so it can't hide the tree
# scan's findings; one in git's folder, the commit scan's source, is an error.
mkdir "$tmp/no-ignore"
[ ! -e "$gitdir/.gitleaksignore" ] || die "$gitdir/.gitleaksignore exists; the commit scan would honour it, and the gate honours no ignore file"
ignf=$(cd "$x" && find . -iname .gitleaksignore -type f | sed 's|^\./||')
if [ -n "$ignf" ]; then
  echo "$ignf" | while IFS= read -r f; do
    echo "gate: $f: FAIL: the gate honours no .gitleaksignore; put an exception in an [[allowlists]] entry of .gitleaks.toml"
    mv "$x/$f" "$x/$f.refused"
  done
  fail=1
fi
GLF=(--no-banner --no-color --redact --ignore-gitleaks-allow --exit-code 3 -c "$(native "$tmp/gitleaks.toml")"
  -i "$(native "$tmp/no-ignore")" -f json)
scan() {  # name, then the gitleaks command line (its report goes to $tmp/<name>.json)
  local name=$1; shift
  "$@" "${GLF[@]}" -r "$(native "$tmp/$name.json")" > "$tmp/$name.log" 2>&1
  local r=$?
  case $r in
    0) echo "gate: secret scan of the $name: no leaks" ;;
    3) echo "gate: secret scan of the $name: LEAKS"; fail=1 ;;
    *) sed 's/^/gate:   /' "$tmp/$name.log" | tail -10; die "gitleaks failed on the $name (exit $r)" ;;
  esac
}
# A merge shows as its diff to its first parent: git log -p shows none for a merge by default, so a secret that
# only a merge adds would pass.
G log --format='commit %H%n%n%B' "$range" > "$tmp/messages.txt" || die "git log failed"
cd "$tmp/no-ignore" || die "cd $tmp/no-ignore"
scan commits "$gl" git --log-opts="--diff-merges=first-parent $range" "$(native "$gitdir")"
scan messages "$gl" stdin < "$tmp/messages.txt"
# From inside the export, so the tree scan reports repository paths (what .gitleaks.toml's paths match).
cd "$x" || die "cd $x"
scan tree "$gl" dir .
cd "$here" || die "cd $here"

# 3. The native audit (cargo metadata and cargo tree; no build), with the base's cargo and rustup configuration.
"$py" -I - "$code" "$sha" "$(native "$x")" > "$tmp/cargo-config.out" 2>&1 << 'PYEOF'
import os, re, subprocess, sys
code, sha, dest = sys.argv[1:4]
CONFIG = re.compile(r"(?i)(^|/)(\.cargo/config(\.toml)?|rust-toolchain(\.toml)?)$")


def configs(rev):
    out = subprocess.run(["git", "ls-tree", "-r", "-z", "--full-tree", rev], capture_output=True, check=True).stdout
    found = {}
    for rec in out.split(b"\0"):
        if rec:
            meta, path = rec.split(b"\t", 1)
            path = path.decode("utf-8", "replace")
            if CONFIG.search(path):
                found[path] = meta.decode().split(" ")[2]
    return found


b, r = configs(code), configs(sha)
for path in sorted(set(b) | set(r)):
    if b.get(path) == r.get(path):
        continue
    target = os.path.join(dest, *path.split("/"))
    if os.path.exists(target):
        os.remove(target)
    if path in b:
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as f:
            f.write(subprocess.run(["git", "cat-file", "blob", b[path]], capture_output=True, check=True).stdout)
    print(path)
PYEOF
rc=$?
[ $rc = 0 ] || { sed 's/^/gate:   /' "$tmp/cargo-config.out"; die "could not put the base's cargo configuration in place (exit $rc)"; }
if [ -s "$tmp/cargo-config.out" ]; then
  echo "gate: the audit ran with the base's version of (absent where the base has none):"
  sed 's/^/gate:   /' "$tmp/cargo-config.out"
fi
wrap=()
if [ -n "${PE_GATE_WRAP:-}" ]; then eval "wrap=($PE_GATE_WRAP)"; fi
${wrap[@]+"${wrap[@]}"} "$py" -I "$(native "$c/scripts/audit_native.py")" --check --tree "$(native "$x")" \
  --allow "$(native "$x/scripts/native_allow.txt")" --md "$(native "$tmp/native-audit.md")" > "$tmp/native.out" 2>&1
rc=$?
grep -E "^audit_native: (FAIL|PASS|ERROR|note)" "$tmp/native.out" | sed 's/^/gate: /'
case $rc in
  0) echo "gate: native audit PASS" ;;
  1) echo "gate: native audit FAIL"; fail=1 ;;
  *) tail -20 "$tmp/native.out" | sed 's/^/gate:   /'; die "the native audit could not run (exit $rc)" ;;
esac

# 4. The publication check, against this repository's objects, with the base's check scripts, run from a folder
#    that holds only them.
terms=${PE_PUBLISH_TERMS:-$(git config --get pe.gate.terms 2> /dev/null)}
allowf=${PE_PUBLISH_ALLOW:-$(git config --get pe.gate.allow 2> /dev/null)}
if [ -z "$terms" ]; then
  echo "gate: publication check skipped: no term file (git config pe.gate.terms; hosted CI has none)"
else
  [ -f "$terms" ] || die "the term file $terms does not exist"
  [ -z "$allowf" ] || [ -f "$allowf" ] || die "the allowlist $allowf does not exist"
  for f in scripts/publish_check.sh scripts/publish_history.py scripts/publish_allow.py; do
    G show "$code:$f" > "$c/$f" 2> /dev/null || die "${code:0:10} has no $f"
  done
  : > "$tmp/no-allowlist"
  pc() {  # log name, then publish_check.sh's arguments
    local name=$1; shift
    (cd "$c" && export GIT_DIR="$gitdir" PE_PUBLISH_TERMS="$(native "$terms")" \
      PE_PUBLISH_ALLOW="$(native "${allowf:-$tmp/no-allowlist}")" && bash scripts/publish_check.sh "$@") > "$tmp/$name.log" 2>&1
  }
  pc terms-tree --ref "$sha"; r=$?
  case $r in
    0) echo "gate: publication check of the tree: $(tail -1 "$tmp/terms-tree.log" | sed 's/^publish_check: //')" ;;
    1) echo "gate: publication check of the tree: FAIL"; grep -v "^allowed: " "$tmp/terms-tree.log" | sed 's/^/gate:   /'; fail=1 ;;
    *) sed 's/^/gate:   /' "$tmp/terms-tree.log" | tail -5; die "the publication check could not run on the tree (exit $r)" ;;
  esac
  if [ -n "$bsha" ]; then
    PE_PUBLISH_HISTORY_TERMS=$(native "$terms") pc terms-history --history --ignore-identity "$range"; r=$?
    case $r in
      0) echo "gate: publication check of the new commits: no term in their added lines or messages" ;;
      1) echo "gate: publication check of the new commits: FAIL"
         grep -E "^\s+[0-9a-f]{10}[: ]|\[FAIL\]" "$tmp/terms-history.log" | sed 's/^/gate:   /'; fail=1 ;;
      *) sed 's/^/gate:   /' "$tmp/terms-history.log" | tail -5; die "the publication check could not run on the history (exit $r)" ;;
    esac
  else
    echo "gate: publication check of the history: not run on a whole-history check (the old history's terms are reported by publish_check.sh --history, never failed)"
  fi
fi

"$py" -I - "$(native "$tmp")" << 'PYEOF'
import json, os, sys
t = sys.argv[1]
for name in ("commits", "messages", "tree"):
    p = os.path.join(t, name + ".json")
    try:
        found = json.load(open(p, encoding="utf-8")) or []
    except (OSError, ValueError):
        continue
    for f in found:
        where = f.get("File") or "(message)"
        commit = (f.get("Commit") or "")[:10]
        print(f"gate:   {name}: {f['RuleID']} at {where}:{f.get('StartLine')}"
              + (f" in commit {commit}" if commit else "") + f" (fingerprint {f.get('Fingerprint')})")
PYEOF

if [ -n "$report" ]; then
  cp "$tmp/native-audit.md" "$tmp"/*.json "$report"/ 2> /dev/null
  echo "gate: reports kept in $report"
fi
if [ "$fail" != 0 ]; then
  echo "gate: FAIL for ${sha:0:10}: not landable. Fix the finding, or accept a reviewed exception with its reason"
  echo "gate:   (scripts/native_allow.txt for native code; an [[allowlists]] entry in .gitleaks.toml for a false secret;"
  echo "gate:   the publication allowlist, through scripts/publish_allow.py, for a reviewed line that only looks like a term)"
  exit 1
fi
echo "gate: PASS for ${sha:0:10}"
