#!/usr/bin/env bash
# Enables the landing gate's hook (scripts/hooks/reference-transaction) for this clone and all its worktrees.
#
#   bash scripts/install_gate_hook.sh            undo: git config --unset core.hooksPath
#
# It writes a small trampoline into git's own folder (<git common dir>/pe-gate-hooks/), outside every working
# tree, so that no merge, checkout or landing can change or delete it, and points core.hooksPath at that
# folder. On every ref update the trampoline runs scripts/hooks/reference-transaction as committed at main's
# old tip (at the new commit when main is created), so the hook comes from the base like the rest of the
# gate's code (scripts/gate.sh). core.hooksPath replaces git's own hooks folder, so this refuses to run while
# that folder holds a hook (git's .sample files aside) or core.hooksPath points elsewhere.
set -euo pipefail
common=$(git rev-parse --path-format=absolute --git-common-dir)
dir="$common/pe-gate-hooks"
cur=$(git config --get core.hooksPath || true)
if [ -n "$cur" ] && [ "$cur" != "$dir" ]; then
  echo "install_gate_hook: core.hooksPath is already $cur; unset it first (git config --unset core.hooksPath)" >&2
  exit 2
fi
for h in "$common"/hooks/*; do
  case "$h" in *.sample|*'/*') continue ;; esac
  echo "install_gate_hook: $h is a hook of its own, which core.hooksPath would turn off; move it first" >&2
  exit 2
done
mkdir -p "$dir"
cat > "$dir/reference-transaction.new" << 'EOF'
#!/bin/sh
# The landing gate's trampoline, written by scripts/install_gate_hook.sh (undo: git config --unset core.hooksPath).
# It lives outside every working tree, so no landing can change or delete it. It runs
# scripts/hooks/reference-transaction as committed at main's old tip (at the new commit when main is
# created), with the same argument and input. Every other update returns at once, with no process started.
if [ "$1" != prepared ]; then
  while read -r _; do :; done
  exit 0
fi
input= from=
while read -r old new ref; do
  input="$input$old $new $ref
"
  [ "$ref" = refs/heads/main ] || continue
  case "$new" in *[!0]*) ;; *) continue ;; esac
  case "$old" in *[!0]*) from=$old ;; *) from=$new ;; esac
done
[ -n "$from" ] || exit 0
tmp=$(mktemp -d) || { echo "gate (hook): REFUSED: no temporary folder" >&2; exit 1; }
trap 'rm -rf "$tmp"' EXIT
if ! git show "$from:scripts/hooks/reference-transaction" > "$tmp/hook" 2> /dev/null; then
  echo "gate (hook): REFUSED: main stays where it is: $from has no scripts/hooks/reference-transaction (to drop the gate: git config --unset core.hooksPath)" >&2
  exit 1
fi
printf '%s' "$input" | sh "$tmp/hook" "$@"
EOF
chmod +x "$dir/reference-transaction.new"
mv -f "$dir/reference-transaction.new" "$dir/reference-transaction"
git config core.hooksPath "$dir"
echo "install_gate_hook: core.hooksPath is $dir; the landing gate now checks every update of main"
echo "install_gate_hook: undo with: git config --unset core.hooksPath"
