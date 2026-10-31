#!/usr/bin/env bash
# Publication check: greps every file of a commit (default HEAD) for terms that must not be
# published, case-insensitively, prints each hit and exits 1 if there is any (2 on an error).
# usage: scripts/publish_check.sh [--ref <ref>]          the tree of <ref> (default HEAD)
#        scripts/publish_check.sh --history <range>...    the history over <range> (see below)
# Run it before every push, on the ref being pushed. It reads the committed tree, so an uncommitted
# local fix doesn't hide a hit that is still committed.
#
# The terms are read from scripts/publish_terms.local, or the file named by PE_PUBLISH_TERMS. That
# file is gitignored: a list of the names to keep out would publish those names. Copy
# scripts/publish_terms.example to it and fill it in. Without a terms file, or with an empty one,
# the check fails rather than passing.
# Format, one term per line: `fixed <text>` matches the text anywhere; `word <text>` matches it as a
# whole word only. Blank lines and lines starting with # are ignored.
#
# --history <range>... (git log revision arguments, e.g. `main` or `origin/main..main`): scans every
# commit's added lines (git log -p) and its message, author and committer, with
# scripts/publish_history.py. It reports the hits per term. It fails only on the terms of a second
# list, scripts/publish_history_terms.local or PE_PUBLISH_HISTORY_TERMS (same format, gitignored),
# which is empty when the file is missing: the history the user ruled to keep (2026-10-31 11:49Z) is
# reported, not failed.
set -u
cd "$(dirname "$0")/.."
terms=${PE_PUBLISH_TERMS:-scripts/publish_terms.local}
[ -f "$terms" ] || { echo "publish_check: no terms file at $terms (copy scripts/publish_terms.example)"; exit 2; }

if [ "${1:-}" = --history ]; then
  shift
  [ $# -gt 0 ] || { echo "publish_check: --history needs a revision range"; exit 2; }
  exec python -I scripts/publish_history.py "$terms" "${PE_PUBLISH_HISTORY_TERMS:-scripts/publish_history_terms.local}" "$@"
fi

ref=HEAD
case "${1:-}" in
  "") ;;
  --ref) [ $# -eq 2 ] || { echo "usage: scripts/publish_check.sh [--ref <ref>] | --history <range>..."; exit 2; }; ref=$2 ;;
  *) echo "usage: scripts/publish_check.sh [--ref <ref>] | --history <range>..."; exit 2 ;;
esac
git rev-parse -q --verify "$ref^{tree}" > /dev/null || { echo "publish_check: $ref is not a commit or tree here"; exit 2; }

n=0 hits=0
while IFS= read -r line || [ -n "$line" ]; do
  line=${line%$'\r'}
  case "$line" in ''|'#'*) continue ;; esac
  kind=${line%% *} text=${line#* }
  case "$kind" in
    fixed) w="" ;;
    word) w="-w" ;;
    *) echo "publish_check: bad line in $terms: $line"; exit 2 ;;
  esac
  n=$((n + 1))
  out=$(git grep -n -i -F $w -e "$text" "$ref" --); rc=$?
  case $rc in
    0) printf '%s\n' "$out"; hits=$((hits + $(printf '%s\n' "$out" | wc -l))) ;;
    1) ;;
    *) echo "publish_check: git grep failed (exit $rc) for a term of $terms"; exit 2 ;;
  esac
done < "$terms"

[ $n -gt 0 ] || { echo "publish_check: $terms lists no terms"; exit 2; }
if [ $hits -gt 0 ]; then
  echo "publish_check: FAIL, $hits line(s) of $ref match one of $n terms"
  exit 1
fi
echo "publish_check: PASS, no file of $ref matches any of $n terms"
