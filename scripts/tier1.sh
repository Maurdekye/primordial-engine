#!/usr/bin/env bash
# Tier 1 of the landing checks: the commands a landing must pass because of the paths it touches, from the table
# in scripts/tier1_checks.txt (docs/landing-gates.md, "Tier 1"). Integrators run it on the commit they hand in,
# in a checkout of it, under the org's build-slot wrapper:
#
#   bash scripts/tier1.sh [--base <rev>] [--run] [<rev>]
#
# <rev> (default HEAD) is the landing; --base (default main) is the commit it lands on. It lists the checks that
# the landing's diff (the tree of <base> against the tree of <rev>, renames as a deletion and an addition)
# triggers, with the paths that trigger them. With --run it also runs each triggered check's command from the
# root of the checkout, which must be at <rev>; every triggered check runs, and each result prints.
# The table and this script come from the base, like the gate's code (it hands over to the base's copy at once),
# so a landing can't drop or skip a check it triggers; a change to either applies from the next landing. When the
# base has no table yet, the landing's own is used, with a NOTE.
# Environment: PE_TIER1_WRAP, a command line to run each command under, such as the org's build-slot wrapper
# (read with eval, so quote paths in it as in a shell).
# Exit 0 if every triggered check passes (without --run: after listing), 1 if one fails, 2 on an error.
set -u
die() { echo "tier1: ERROR: $*" >&2; exit 2; }
usage="usage: scripts/tier1.sh [--base <rev>] [--run] [<rev>]"
args=("$@")
base=main rev= run=0
while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || die "--base needs a revision ($usage)"; base=$2; shift 2 ;;
    --run) run=1; shift ;;
    -*) die "unknown option $1 ($usage)" ;;
    *) [ -z "$rev" ] || die "one revision only ($usage)"; rev=$1; shift ;;
  esac
done
rev=${rev:-HEAD}
bsha=$(git rev-parse -q --verify "$base^{commit}") || die "$base is not a commit"
sha=$(git rev-parse -q --verify "$rev^{commit}") || die "$rev is not a commit"
short() { git rev-parse --short=10 "$1"; }
if [ "${PE_TIER1_CODE:-}" != "$bsha" ] && git cat-file -e "$bsha:scripts/tier1.sh" 2> /dev/null; then
  code=$(mktemp) || die "no temporary file"
  git show "$bsha:scripts/tier1.sh" > "$code" || { rm -f "$code"; die "can't read the base's scripts/tier1.sh"; }
  PE_TIER1_CODE=$bsha bash "$code" "${args[@]}"; rc=$?
  rm -f "$code"
  exit $rc
fi
table=scripts/tier1_checks.txt
if tab=$(git show "$bsha:$table" 2> /dev/null); then
  :
elif tab=$(git show "$sha:$table" 2> /dev/null); then
  echo "tier1: NOTE: $(short "$bsha") has no $table yet; the landing's own table applies"
else
  echo "tier1: neither $(short "$bsha") nor $(short "$sha") has $table: nothing to check"
  exit 0
fi

# The table: "check <name>" starts a check; "why", "when", "needs" and "run" lines belong to the last one.
names=() whens=() needs=() runs=()
n=-1 lineno=0
while IFS= read -r line || [ -n "$line" ]; do
  lineno=$((lineno + 1))
  line=${line%$'\r'}
  set -f; set -- $line; set +f
  [ $# -gt 0 ] || continue
  case "$1" in '#'*) continue ;; esac
  key=$1; shift
  rest=${line#*"$key"}; rest=${rest#"${rest%%[![:space:]]*}"}
  if [ "$key" = check ]; then
    [ $# = 1 ] || die "$table line $lineno: 'check' takes one name"
    n=$((n + 1)); names[n]=$1; whens[n]= needs[n]= runs[n]=
    continue
  fi
  [ $n -ge 0 ] || die "$table line $lineno: '$key' before the first 'check'"
  [ -n "$rest" ] || die "$table line $lineno: '$key' needs a value"
  case "$key" in
    why) ;;
    when) whens[n]="${whens[n]} $rest" ;;
    needs) needs[n]="${needs[n]:+${needs[n]}; }$rest" ;;
    run) [ -z "${runs[n]}" ] || die "$table line $lineno: check ${names[n]} has two 'run' lines"; runs[n]=$rest ;;
    *) die "$table line $lineno: unknown keyword '$key' (check, why, when, needs, run)" ;;
  esac
done <<< "$tab"
for i in "${!names[@]}"; do
  [ -n "${whens[i]}" ] || die "$table: check ${names[i]} has no 'when' line"
  [ -n "${runs[i]}" ] || die "$table: check ${names[i]} has no 'run' line"
done

if [ $run = 1 ]; then
  [ "$(git rev-parse HEAD)" = "$sha" ] || die "--run needs the checkout at $(short "$sha"); HEAD is $(short HEAD)"
  top=$(git rev-parse --show-toplevel) || die "--run needs a working tree"
  cd "$top" || die "can't enter $top"
fi
wrap=(); if [ -n "${PE_TIER1_WRAP:-}" ]; then eval "wrap=($PE_TIER1_WRAP)"; fi

echo "tier1: $(short "$sha") against $(short "$bsha"): ${#names[@]} check(s) in the table"
hit=0 passed=0 failed=0
for i in "${!names[@]}"; do
  specs=()
  set -f
  for g in ${whens[i]}; do specs+=(":(glob)$g"); done
  set +f
  paths=$(git -c core.quotepath=off diff --name-only --no-renames "$bsha" "$sha" -- "${specs[@]}") ||
    die "git diff failed for check ${names[i]}"
  if [ -z "$paths" ]; then
    echo "tier1: ${names[i]}: not triggered"
    continue
  fi
  hit=$((hit + 1))
  count=$(printf '%s\n' "$paths" | wc -l | tr -d ' ')
  echo "tier1: ${names[i]}: triggered by $count path(s): $(printf '%s\n' "$paths" | head -3 | tr '\n' ' ')"
  [ $run = 1 ] || { echo "tier1:   run: ${runs[i]}"; continue; }
  [ -z "${needs[i]}" ] || echo "tier1:   needs: ${needs[i]}"
  echo "tier1:   running: ${runs[i]}"
  ${wrap[@]+"${wrap[@]}"} bash -c "${runs[i]}"; rc=$?
  if [ $rc = 0 ]; then
    passed=$((passed + 1)); echo "tier1: ${names[i]}: PASS"
  else
    failed=$((failed + 1)); echo "tier1: ${names[i]}: FAIL (exit $rc)"
  fi
done
if [ $run = 0 ]; then
  echo "tier1: $hit check(s) triggered (listed only; --run runs them)"
  exit 0
fi
echo "tier1: $hit check(s) triggered: $passed passed, $failed failed"
[ $failed = 0 ]
