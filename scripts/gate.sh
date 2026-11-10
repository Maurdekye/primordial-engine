#!/usr/bin/env bash
# The landing gate: the native-code audit (the "100% Rust" ruling, D1) and a secret scan, on a commit
# before it lands on main. The reference-transaction hook (scripts/hooks/) runs it on every update of
# main, and the hosted CI workflow (.github/workflows/gate.yml) on every push and pull request.
#
#   bash scripts/gate.sh [--base <rev>] [--full-history] [--report <folder>] [<rev>]
#
# <rev> (default HEAD) is the commit to land; --base (default main) is the commit it lands on. The gate
#   1. exports the tree of <rev> to a temporary folder: only what is committed counts;
#   2. native audit: scripts/audit_native.py --check over that tree, every Cargo workspace in it, with the
#      allow file scripts/native_allow.txt (a crate in no audited workspace fails too);
#   3. secret scan: gitleaks (the version scripts/install_gitleaks.sh pins) over the added lines of every
#      commit in <base>..<rev>, over those commits' messages, and over the whole tree of <rev>; with
#      --full-history, over every commit <rev> reaches instead of <base>..<rev>;
#   4. publication check (scripts/publish_check.sh), when this clone names its term file: the tree of <rev>,
#      and, blocking, the added lines and messages of <base>..<rev> (identities are left to the publishing
#      step, which maps them). The terms are local files that must never be published, so hosted CI
#      skips this step; a whole-history run reports the old history's terms without failing on them (the
#      user kept that history, 2026-10-31 11:49Z).
# It takes the audit script, the allow file and .gitleaks.toml from <rev>, so a landing is judged by the
# rules it carries; when it changes them, the gate lists the files, and the landing's reviewer approves
# those lines with the rest. Inline `gitleaks:allow` comments are ignored.
#
# Exit 0 pass, 1 fail, 2 error. Findings print with the secret redacted. --report <folder> keeps the
# audit's markdown and the scans' JSON (redacted) there.
# Environment: PE_GITLEAKS (a gitleaks binary), PE_PYTHON (a Python 3.10+), PE_GATE_WRAP (a command line
# to run the audit's cargo calls under, such as the org's build-slot wrapper; read with eval, so quote
# paths in it as in a shell), PE_PUBLISH_TERMS and PE_PUBLISH_ALLOW (the term file and the allowlist;
# default: git config pe.gate.terms and pe.gate.allow).
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
G() { git "$@"; }
sha=$(G rev-parse -q --verify "$rev^{commit}") || die "$rev is not a commit"
zero=0000000000000000000000000000000000000000
if [ "$full" = 1 ] || [ -z "$base" ] || [ "$base" = "$zero" ]; then
  range=$sha bsha= what="every commit $rev reaches"
else
  bsha=$(G rev-parse -q --verify "$base^{commit}") || die "--base $base is not a commit"
  range="$bsha..$sha" what="$(G rev-list --count "$bsha..$sha") commit(s) in ${bsha:0:10}..${sha:0:10}"
fi

case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) win=1 ;; *) win=0 ;; esac
native() { if [ "$win" = 1 ]; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
py=${PE_PYTHON:-}
if [ -z "$py" ]; then
  if [ "$win" = 1 ]; then py=python; else py=$(command -v python3 || command -v python) || die "no python3"; fi
fi

# gitleaks: PE_GITLEAKS, this checkout's or the main checkout's .tools, then PATH; the pinned version only.
want=$(G show "$sha:scripts/install_gitleaks.sh" 2> /dev/null | sed -n 's/^VERSION=//p')
[ -n "$want" ] || die "$rev has no scripts/install_gitleaks.sh naming the pinned gitleaks VERSION"
exe=gitleaks; [ "$win" = 1 ] && exe=gitleaks.exe
gl=${PE_GITLEAKS:-}
if [ -z "$gl" ]; then
  for d in "$(git rev-parse --show-toplevel 2> /dev/null)" "$(dirname "$gitdir")"; do
    [ -n "$d" ] && [ -x "$d/.tools/gitleaks-$want/$exe" ] && { gl="$d/.tools/gitleaks-$want/$exe"; break; }
  done
fi
[ -n "$gl" ] || gl=$(command -v gitleaks 2> /dev/null) || true
[ -n "$gl" ] || die "gitleaks $want not found: run bash scripts/install_gitleaks.sh (or set PE_GITLEAKS)"
v=$("$gl" version 2> /dev/null) || die "$gl does not run"
[ "$v" = "$want" ] || die "$gl is gitleaks $v; the gate pins $want (bash scripts/install_gitleaks.sh)"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
x="$tmp/tree"
mkdir "$x"
G archive --format=tar "$sha" | tar -x -C "$x" || die "could not export the tree of $rev"
[ -f "$x/scripts/audit_native.py" ] || die "$rev has no scripts/audit_native.py; the gate needs it"
[ -f "$x/.gitleaks.toml" ] || die "$rev has no .gitleaks.toml; the gate needs it"

echo "gate: checking ${sha:0:10} ($rev); secret scan over $what, their messages and the tree"
if [ -n "$bsha" ]; then
  changed=$(G diff --name-only "$bsha" "$sha" -- .gitleaks.toml scripts/native_allow.txt scripts/audit_native.py \
    scripts/gate.sh scripts/install_gitleaks.sh scripts/hooks .github/workflows)
  if [ -n "$changed" ]; then
    echo "gate: NOTE: this landing changes the gate's own rules or scripts; its reviewer approves these lines:"
    echo "$changed" | sed 's/^/gate:   /'
  fi
fi

fail=0
# 1. The native audit (cargo metadata and cargo tree; no build).
wrap=()
if [ -n "${PE_GATE_WRAP:-}" ]; then eval "wrap=($PE_GATE_WRAP)"; fi
${wrap[@]+"${wrap[@]}"} "$py" -I "$(native "$x/scripts/audit_native.py")" --check --tree "$(native "$x")" \
  --md "$(native "$tmp/native-audit.md")" > "$tmp/native.out" 2>&1
rc=$?
grep -E "^audit_native: (FAIL|PASS|ERROR|note)" "$tmp/native.out" | sed 's/^/gate: /'
case $rc in
  0) echo "gate: native audit PASS" ;;
  1) echo "gate: native audit FAIL"; fail=1 ;;
  *) tail -20 "$tmp/native.out" | sed 's/^/gate:   /'; die "the native audit could not run (exit $rc)" ;;
esac

# 2. The secret scan. Exit 3 means leaks; anything else but 0 is an error.
cfg=$(native "$x/.gitleaks.toml")
GLF=(--no-banner --no-color --redact --ignore-gitleaks-allow --exit-code 3 -c "$cfg" -f json)
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
scan commits "$gl" git --log-opts="$range" "$(native "$gitdir")"
G log --format='commit %H%n%n%B' "$range" > "$tmp/messages.txt" || die "git log failed"
scan messages "$gl" stdin < "$tmp/messages.txt"
# From inside the export, so the tree scan reports repository paths (what .gitleaks.toml's paths match).
cd "$x" || die "cd $x"
scan tree "$gl" dir .
cd "$here" || die "cd $here"

# 3. The publication check, against this repository's objects with the check script <rev> carries.
terms=${PE_PUBLISH_TERMS:-$(git config --get pe.gate.terms 2> /dev/null)}
allowf=${PE_PUBLISH_ALLOW:-$(git config --get pe.gate.allow 2> /dev/null)}
if [ -z "$terms" ]; then
  echo "gate: publication check skipped: no term file (git config pe.gate.terms; hosted CI has none)"
else
  [ -f "$terms" ] || die "the term file $terms does not exist"
  [ -z "$allowf" ] || [ -f "$allowf" ] || die "the allowlist $allowf does not exist"
  [ -f "$x/scripts/publish_check.sh" ] || die "$rev has no scripts/publish_check.sh"
  pc() {  # log name, then publish_check.sh's arguments
    local name=$1; shift
    (cd "$x" && export GIT_DIR="$gitdir" PE_PUBLISH_TERMS="$(native "$terms")" &&
      if [ -n "$allowf" ]; then export PE_PUBLISH_ALLOW="$(native "$allowf")"; else export PE_PUBLISH_ALLOW="$(native "$tmp/no-allowlist")"; fi &&
      bash scripts/publish_check.sh "$@") > "$tmp/$name.log" 2>&1
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

"$py" -I - "$(native "$tmp")" <<'PYEOF'
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
