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
# Install it only once the gate is on main: the trampoline refuses every update of main while main's tip has no
# scripts/hooks/reference-transaction, so this refuses to run until then. It also needs git 2.40 or later, as the
# gate does.
set -euo pipefail
gv=$(git version)
case "$gv" in
  "git version 2."[4-9][0-9]*|"git version 2."[1-9][0-9][0-9]*|"git version "[3-9]*) ;;
  *) echo "install_gate_hook: $gv is older than 2.40, which the gate needs (GIT_ATTR_SOURCE)" >&2; exit 2 ;;
esac
for f in scripts/hooks/reference-transaction scripts/gate.sh; do
  if ! git cat-file -e "refs/heads/main:$f" 2> /dev/null; then
    echo "install_gate_hook: main has no $f yet; install the hook once the gate is on main (until then it would refuse every update of main)" >&2
    exit 2
  fi
done
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
# It lives outside every working tree, so no landing can change or delete it. When main moves to another
# commit, it runs scripts/hooks/reference-transaction as committed at main's old tip (at the new commit when
# main is created), with the same argument and input, except that main's line names that tip as its old value:
# for a forced update (update-ref without an old value, branch -f) git gives zeros there, which would read as
# main being created. Every other update returns at once with no process started, and one that leaves main where
# it is (a reset to HEAD, merge --abort, stash) after one git call.
if [ "$1" != prepared ]; then
  while read -r _; do :; done
  exit 0
fi
input= old= new=
while read -r o n r; do
  if [ "$r" = refs/heads/main ]; then old=$o new=$n; continue; fi
  input="$input$o $n $r
"
done
case "$new" in *[!0]*) ;; *) exit 0 ;; esac        # main isn't moving to a commit (not in this update, or deleted)
if tip=$(git rev-parse -q --verify refs/heads/main); then   # main's tip before this update: git holds it locked
  [ "$tip" != "$new" ] || exit 0                      # main stays where it is: nothing lands
  old=$tip from=$tip
else
  from=$new                                           # main is created
fi
input="$input$old $new refs/heads/main
"
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
