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
# root of the checkout, which must be at <rev> with no uncommitted change, and no untracked file under a triggered
# check's paths; every triggered check runs, and each result prints. A check passes when its command exits 0 and,
# if the check has an `expect` line, a line of the command's output matches that regex (so a test filter that
# matches no test can't pass).
# A command may name {crates}. It becomes `-p <package>` for each workspace member (the members of the root
# Cargo.toml, as the landing has them) whose folder holds a path that triggers the check, or for every member
# when the diff touches a path on the check's `all` lines, less the check's `skip` packages. When none is left,
# the check has nothing to run.
# The table and this script come from the base, like the gate's code (it hands over to the base's copy at once),
# so a landing can't drop or skip a check it triggers; a change to either applies from the next landing. When the
# base has no table yet, the landing's own is used, with a NOTE. The landing's own table is read too: a landing
# whose table is malformed fails, since that table would stop every landing after it.
# Environment: PE_TIER1_WRAP, a command line to run each command under, such as the org's build-slot wrapper
# (read with eval, so quote paths in it as in a shell).
# Exit 0 if every triggered check passes (without --run: after listing), 1 if one fails or the landing's own
# table is refused (in both modes), 2 on an error.
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
own=0
if tab=$(git show "$bsha:$table" 2> /dev/null); then
  :
elif tab=$(git show "$sha:$table" 2> /dev/null); then
  echo "tier1: NOTE: $(short "$bsha") has no $table yet; the landing's own table applies"
  own=1
else
  echo "tier1: neither $(short "$bsha") nor $(short "$sha") has $table: nothing to check"
  exit 0
fi

# parse_table <text>: reads a table. "check <name>" starts a check; "why", "when", "all", "needs", "skip", "run"
# and "expect" lines belong to the last one. It fills names, whens, alls, needs, skips (packages,
# space-separated), runs and expects. On an error it sets perr and returns 1.
pkgre='^[A-Za-z0-9_-]+$'
parse_table() {
  local text=$1 n=-1 lineno=0 line key rest w
  names=() whens=() alls=() needs=() skips=() runs=() expects=()
  while IFS= read -r line || [ -n "$line" ]; do
    lineno=$((lineno + 1))
    line=${line%$'\r'}
    set -f; set -- $line; set +f
    [ $# -gt 0 ] || continue
    case "$1" in '#'*) continue ;; esac
    key=$1; shift
    rest=${line#*"$key"}; rest=${rest#"${rest%%[![:space:]]*}"}
    if [ "$key" = check ]; then
      [ $# = 1 ] || { perr="line $lineno: 'check' takes one name"; return 1; }
      for w in ${names[@]+"${names[@]}"}; do
        [ "$w" != "$1" ] || { perr="line $lineno: a second check named $1"; return 1; }
      done
      n=$((n + 1)); names[n]=$1; whens[n]= alls[n]= needs[n]= skips[n]= runs[n]= expects[n]=
      continue
    fi
    [ $n -ge 0 ] || { perr="line $lineno: '$key' before the first 'check'"; return 1; }
    [ -n "$rest" ] || { perr="line $lineno: '$key' needs a value"; return 1; }
    case "$key" in
      why) ;;
      when) whens[n]="${whens[n]} $rest" ;;
      all) alls[n]="${alls[n]} $rest" ;;
      needs) needs[n]="${needs[n]:+${needs[n]}; }$rest" ;;
      skip)
        for w in "$@"; do
          [[ $w =~ $pkgre ]] || { perr="line $lineno: 'skip' takes package names, and '$w' isn't one"; return 1; }
        done
        skips[n]="${skips[n]} $*" ;;
      run)
        [ -z "${runs[n]}" ] || { perr="line $lineno: check ${names[n]} has two 'run' lines"; return 1; }
        runs[n]=$rest ;;
      expect)
        [ -z "${expects[n]}" ] || { perr="line $lineno: check ${names[n]} has two 'expect' lines"; return 1; }
        expects[n]=$rest ;;
      *) perr="line $lineno: unknown keyword '$key' (check, why, when, all, needs, skip, run, expect)"; return 1 ;;
    esac
  done <<< "$text"
  for w in ${names[@]+"${!names[@]}"}; do
    [ -n "${whens[w]}" ] || { perr="check ${names[w]} has no 'when' line"; return 1; }
    [ -n "${runs[w]}" ] || { perr="check ${names[w]} has no 'run' line"; return 1; }
    case "${runs[w]}" in
      *'{crates}'*) ;;
      *) [ -z "${alls[w]}${skips[w]}" ] ||
           { perr="check ${names[w]}: 'all' and 'skip' need {crates} in its 'run' line"; return 1; } ;;
    esac
  done
  return 0
}

# The landing's own table, when it differs from the base's, must read cleanly: it judges the landings after it.
lerr=
if [ $own = 0 ] && ltab=$(git show "$sha:$table" 2> /dev/null) && [ "$ltab" != "$tab" ]; then
  parse_table "$ltab" || lerr=$perr
fi
parse_table "$tab" || die "$table: $perr"

# member_dirs <commit>: prints the [workspace] members of the commit's root Cargo.toml, one folder per line.
# Returns 1 when it names none (or there is no Cargo.toml), 2 when a member is a pattern rather than a folder.
member_dirs() {
  local man line d section= inm=0 found=0 qre='"([^"]*)"' hre='^[[:space:]]*\[' mre='^[[:space:]]*members[[:space:]]*='
  man=$(git show "$1:Cargo.toml" 2> /dev/null) || return 1
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line%%#*}
    if [ $inm = 0 ]; then
      if [[ $line =~ $hre ]]; then section=$(printf '%s' "$line" | tr -d '[:space:]'); continue; fi
      [ "$section" = '[workspace]' ] && [[ $line =~ $mre ]] || continue
      inm=1; line=${line#*=}
    fi
    while [[ $line =~ $qre ]]; do
      d=${BASH_REMATCH[1]}
      case "$d" in *[*?[]*) return 2 ;; esac
      printf '%s\n' "${d%/}"; found=1
      line=${line#*"\"$d\""}
    done
    case "$line" in *']'*) break ;; esac
  done <<< "$man"
  [ $found = 1 ]
}

# The workspace members as the landing has them (each member's folder and its package name), and the base's
# member folders. Read once, the first time a triggered check names {crates}.
mdirs=() mpkgs=() bdirs=() mread=0
read_members() {
  [ $mread = 0 ] || return 0
  mread=1
  local list rc d name
  list=$(member_dirs "$sha"); rc=$?
  [ $rc != 2 ] || die "the root Cargo.toml of $(short "$sha") names a member by a pattern; {crates} needs each member's folder"
  [ $rc = 0 ] || die "can't read the workspace members from the root Cargo.toml of $(short "$sha")"
  while IFS= read -r d; do mdirs+=("$d"); done <<< "$list"
  for d in "${mdirs[@]}"; do
    name=$(git show "$sha:$d/Cargo.toml" 2> /dev/null |
      sed -n '/^\[package\]/,/^\[/s/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
    [[ $name =~ $pkgre ]] || die "can't read the package name of the workspace member $d"
    mpkgs+=("$name")
  done
  if list=$(member_dirs "$bsha"); then
    while IFS= read -r d; do bdirs+=("$d"); done <<< "$list"
  fi
}

# expand <check index> <its triggering paths>: sets crates to the check's -p list, notes to how it was chosen
# and the packages it leaves out, and orphans to the triggering paths that the landing still has in a folder
# that is a member at the base but not in the landing (it changes a crate it drops from the workspace, so no
# check would build those files). Returns 1 when no package is left.
expand() {
  local i=$1 paths=$2 k p sel all=0 allhit g specs d m
  read_members
  if [ -n "${alls[i]}" ]; then
    specs=()
    set -f; for g in ${alls[i]}; do specs+=(":(glob)$g"); done; set +f
    allhit=$(git -c core.quotepath=off diff --name-only --no-renames "$bsha" "$sha" -- "${specs[@]}") ||
      die "git diff failed for check ${names[i]}"
    [ -z "$allhit" ] || all=1
  fi
  crates= notes=
  for k in "${!mdirs[@]}"; do
    p=${mpkgs[k]} sel=$all
    if [ $sel = 0 ]; then
      while IFS= read -r g; do
        case "$g" in "${mdirs[k]}"/*) sel=1; break ;; esac
      done <<< "$paths"
    fi
    [ $sel = 1 ] || continue
    case " ${skips[i]} " in *" $p "*) notes="$notes; $p skipped"; continue ;; esac
    crates="$crates -p $p"
  done
  orphans=
  while IFS= read -r g; do
    [ -n "$g" ] || continue
    m=0
    for d in "${mdirs[@]}"; do case "$g" in "$d"/*) m=1; break ;; esac; done
    [ $m = 0 ] || continue
    for d in ${bdirs[@]+"${bdirs[@]}"}; do
      case "$g" in "$d"/*) ! git cat-file -e "$sha:$g" 2> /dev/null || orphans="$orphans $g ($d)"; break ;; esac
    done
  done <<< "$paths"
  [ $all = 0 ] || notes="every member, for a path on its 'all' lines$notes"
  notes=${notes#; }
  [ -z "$crates" ] || return 0
  [ -n "$notes" ] || notes="no workspace member holds a path that triggers it"
  return 1
}

if [ $run = 1 ]; then
  [ "$(git rev-parse HEAD)" = "$sha" ] || die "--run needs the checkout at $(short "$sha"); HEAD is $(short HEAD)"
  top=$(git rev-parse --show-toplevel) || die "--run needs a working tree"
  cd "$top" || die "can't enter $top"
  dirty=$(git status --porcelain --untracked-files=no) || die "git status failed"
  [ -z "$dirty" ] || die "--run needs the landing as committed; the checkout has uncommitted changes:
$(printf '%s\n' "$dirty" | head -5)"
  out=$(mktemp) || die "no temporary file"
  trap 'rm -f "$out"' EXIT
fi
wrap=(); if [ -n "${PE_TIER1_WRAP:-}" ]; then eval "wrap=($PE_TIER1_WRAP)"; fi

echo "tier1: $(short "$sha") against $(short "$bsha"): ${#names[@]} check(s) in the table"
refused=0
if [ -n "$lerr" ]; then
  echo "tier1: FAIL: the landing's own $table has an error, which would stop every landing after it: $lerr"
  refused=1
fi
hit=0 passed=0 failed=0 empty=0
for i in ${names[@]+"${!names[@]}"}; do
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
  cmd=${runs[i]}
  case "$cmd" in
    *'{crates}'*)
      left=1; expand "$i" "$paths" || left=0
      if [ -n "$orphans" ]; then
        refused=1
        echo "tier1: ${names[i]}: FAIL: the landing changes files in a folder that is a workspace member at the base" \
          "but not in the landing, so no check builds them:$orphans"
      fi
      if [ $left = 0 ]; then
        empty=$((empty + 1)); echo "tier1: ${names[i]}: nothing to run: $notes"
        continue
      fi
      [ -z "$notes" ] || echo "tier1:   {crates}: $notes"
      cmd=${cmd//'{crates}'/${crates# }} ;;
  esac
  [ $run = 1 ] || { echo "tier1:   run: $cmd"; continue; }
  stray=$(git -c core.quotepath=off status --porcelain --untracked-files=all -- "${specs[@]}" | sed -n 's/^?? //p')
  [ -z "$stray" ] || die "untracked files under check ${names[i]}'s paths would be tested with the landing:
$(printf '%s\n' "$stray" | head -5)"
  [ -z "${needs[i]}" ] || echo "tier1:   needs: ${needs[i]}"
  echo "tier1:   running: $cmd"
  ${wrap[@]+"${wrap[@]}"} bash -c "$cmd" 2>&1 | tee "$out"; rc=${PIPESTATUS[0]}
  if [ "$rc" != 0 ]; then
    failed=$((failed + 1)); echo "tier1: ${names[i]}: FAIL (exit $rc)"
  elif [ -n "${expects[i]}" ] && ! grep -qE -- "${expects[i]}" "$out"; then
    failed=$((failed + 1)); echo "tier1: ${names[i]}: FAIL (exit 0, but no line of its output matches: ${expects[i]})"
  else
    passed=$((passed + 1)); echo "tier1: ${names[i]}: PASS"
  fi
done
nothing=; [ $empty = 0 ] || nothing=", $empty with nothing to run"
if [ $run = 0 ]; then
  echo "tier1: $hit check(s) triggered$nothing (listed only; --run runs them)"
  [ $refused = 0 ]; exit
fi
echo "tier1: $hit check(s) triggered: $passed passed, $failed failed$nothing"
[ $failed = 0 ] && [ $refused = 0 ]
