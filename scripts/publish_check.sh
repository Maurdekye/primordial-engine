#!/usr/bin/env bash
# Publication check: greps every tracked file (the working-tree copy) for terms that must not be
# published, case-insensitively, prints each hit and exits non-zero if there is any.
# usage: scripts/publish_check.sh            (run it before every push)
#
# The terms are read from scripts/publish_terms.local, or the file named by PE_PUBLISH_TERMS. That
# file is gitignored: a list of the names to keep out would publish those names. Copy
# scripts/publish_terms.example to it and fill it in. Without a terms file, or with an empty one,
# the check fails rather than passing.
# Format, one term per line: `fixed <text>` matches the text anywhere; `word <text>` matches it as a
# whole word only. Blank lines and lines starting with # are ignored.
set -u
cd "$(dirname "$0")/.."
terms=${PE_PUBLISH_TERMS:-scripts/publish_terms.local}
[ -f "$terms" ] || { echo "publish_check: no terms file at $terms (copy scripts/publish_terms.example)"; exit 2; }

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
  # The terms file itself is untracked, so git grep never searches it.
  if out=$(git grep -n -i -F $w -e "$text" -- . ':!scripts/publish_terms.local'); then
    printf '%s\n' "$out"
    hits=$((hits + $(printf '%s\n' "$out" | wc -l)))
  fi
done < "$terms"

[ $n -gt 0 ] || { echo "publish_check: $terms lists no terms"; exit 2; }
if [ $hits -gt 0 ]; then
  echo "publish_check: FAIL, $hits line(s) match one of $n terms"
  exit 1
fi
echo "publish_check: PASS, no tracked file matches any of $n terms"
