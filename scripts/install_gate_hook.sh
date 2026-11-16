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
# main being created. It refuses to delete main, and to move it when main's tip can't be read though git gives
# an old value for it or main is there. Every other update returns at once with no process started, and one that
# leaves main where it is (a reset to HEAD, merge --abort, stash, pack-refs) after one or two git calls.
# PE_GATE=off lets one command through unchecked, in an emergency only; whoever uses it reports it to the
# coordinator in the same turn.
if [ "$1" != prepared ]; then
  while read -r _; do :; done
  exit 0
fi
refuse() {  # $1: what this update does
  if [ "${PE_GATE:-on}" = off ]; then
    echo "gate (hook): PE_GATE=off: $1, UNCHECKED: emergencies only; report it to the coordinator now" >&2
    exit 0
  fi
  echo "gate (hook): REFUSED: $1; main stays where it is (to drop the gate: git config --unset core.hooksPath)" >&2
  exit 1
}
input= old= new= hit=
while read -r o n r; do
  if [ "$r" = refs/heads/main ]; then old=$o new=$n hit=1; continue; fi
  input="$input$o $n $r
"
done
[ -n "$hit" ] || exit 0                               # main isn't in this update
tip=$(git rev-parse -q --verify refs/heads/main) || tip=   # main's tip before this update: git holds it locked
if [ -z "$tip" ]; then
  # Main is created, unless it is there (a loose file or a packed line) but can't be read: then a forced update,
  # where git gives zeros as the old value, would be judged by the new commit's own hook.
  common=$(git rev-parse --path-format=absolute --git-common-dir) && [ ! -f "$common/refs/heads/main" ] &&
    ! grep -q ' refs/heads/main$' "$common/packed-refs" 2> /dev/null || refuse "moving main while its tip can't be read"
  case "$old" in *[!0]*) refuse "moving main from $old, git's old value, while its tip can't be read" ;; esac
  case "$new" in *[!0]*) ;; *) exit 0 ;; esac         # main doesn't exist and isn't created: nothing happens
  from=$new                                           # main is created
else
  [ "$tip" != "$new" ] || exit 0                      # main stays where it is: nothing lands
  case "$new" in *[!0]*) ;; *)
    # A deletion, unless pack-refs (git gc) is dropping main's loose file after writing main to packed-refs: then
    # main keeps its tip, git gives that tip as the old value, and nothing holds packed-refs.lock (git takes it for
    # every deletion before this hook runs).
    common=$(git rev-parse --path-format=absolute --git-common-dir) && [ "$old" = "$tip" ] &&
      [ ! -e "$common/packed-refs.lock" ] && grep -qx "$tip refs/heads/main" "$common/packed-refs" 2> /dev/null &&
      exit 0
    refuse "deleting main" ;;
  esac
  old=$tip from=$tip
fi
input="$input$old $new refs/heads/main
"
# Temporary files go to the folder git config pe.gate.tmpdir names, when it exists, as in the hook and the gate.
gtmp=$(git config --get pe.gate.tmpdir) && [ -d "$gtmp" ] && export TMPDIR="$gtmp" TMP="$gtmp" TEMP="$gtmp"
tmp=$(mktemp -d) || refuse "moving main with no temporary folder for its hook"
trap 'rm -rf "$tmp"' EXIT
git show "$from:scripts/hooks/reference-transaction" > "$tmp/hook" 2> /dev/null ||
  refuse "moving main, whose hook would come from $from, which has no scripts/hooks/reference-transaction"
printf '%s' "$input" | sh "$tmp/hook" "$@"
EOF
chmod +x "$dir/reference-transaction.new"
mv -f "$dir/reference-transaction.new" "$dir/reference-transaction"
git config core.hooksPath "$dir"
echo "install_gate_hook: core.hooksPath is $dir; the landing gate now checks every update of main"
echo "install_gate_hook: undo with: git config --unset core.hooksPath"
