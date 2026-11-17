#!/usr/bin/env bash
# Self-test of scripts/tier1.sh (tier 1 of the landing checks). In a throwaway clone it plants a base whose table
# has one check, with a command that leaves a marker, and checks that a landing touching the check's paths
# triggers it and runs it, one that doesn't touch them doesn't, a landing that drops the check from its own table
# still runs it (the table comes from the base), so does one whose runner would skip it (the runner hands over to
# the base's copy), a failing command fails the run and so does one that exits 0 without its expected output,
# --run refuses a checkout that isn't at the landing, has uncommitted changes or untracked files under a
# triggered check's paths (and doesn't mind them elsewhere), a base without a table uses the landing's with a
# NOTE, and a malformed table is an error. Then, for a check whose command names {crates}: it holds the touched
# workspace members (by folder, so a path in pe-net-crypto isn't pe-net's) less the skipped packages, every
# member for a path on an 'all' line, and nothing to run when none is left; a landing that drops a member while
# changing its files fails (one that removes the member with its folder doesn't); and a landing whose own table
# is malformed (one that adds an exemption: there is no such keyword) fails, listing too.
# It runs no cargo, so it needs no build slot.
#
#   bash scripts/test_tier1.sh [<rev>]          (default HEAD; <rev> must have scripts/tier1.sh)
#
# Exit 0 if every case behaves, 1 if one doesn't (its output is printed), 2 on an error.
set -u
die() { echo "test_tier1: ERROR: $*" >&2; exit 2; }
rev=${1:-HEAD}
sha=$(git rev-parse -q --verify "$rev^{commit}") || die "$rev is not a commit"
git cat-file -e "$sha:scripts/tier1.sh" 2> /dev/null || die "$rev has no scripts/tier1.sh"
common=$(git rev-parse --path-format=absolute --git-common-dir)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git clone -q --shared --no-checkout "$common" "$tmp/repo" || die "clone failed"
R="$tmp/repo"
g() { git -C "$R" -c user.name="tier1 test" -c user.email="tier1-test@example.invalid" -c core.hooksPath=/dev/null "$@"; }
g checkout -q --detach "$sha" || die "checkout failed"
export TIER1_TEST_OUT="$tmp/out"
mkdir -p "$TIER1_TEST_OUT"
table() {  # $1: the command's exit code, $2: what it prints (the check expects "marker-ok")
  printf '%s\n' '# a test table' 'check marker' '  why  the test' '  when docs/zz-tier1/** **/zz-cfg.toml' \
    '  needs nothing' "  run  echo ran > \"\$TIER1_TEST_OUT/marker\"; echo ${2:-marker-ok}; exit $1" \
    '  expect ^marker-ok$'
}
table 0 > "$R/scripts/tier1_checks.txt"; g add -A; g commit -q -m "base: one check"; base=$(g rev-parse HEAD)
branch() { g checkout -q -f --detach "$base"; }
commit() { g add -A; g commit -q -m "$1"; g rev-parse HEAD; }
branch; mkdir -p "$R/docs/zz-tier1"; echo a > "$R/docs/zz-tier1/a.md"; touches=$(commit "touches")
branch; echo b >> "$R/README.md"; untouched=$(commit "untouched")
branch; mkdir -p "$R/docs/zz-tier1"; echo c > "$R/docs/zz-tier1/c.md"
printf '# emptied\n' > "$R/scripts/tier1_checks.txt"; dropper=$(commit "drops the check")
branch; mkdir -p "$R/docs/zz-tier1"; echo s > "$R/docs/zz-tier1/s.md"
awk '{ print } $0 == "table=scripts/tier1_checks.txt" { print "echo \"tier1: skipped everything\"; exit 0" }' \
  "$R/scripts/tier1.sh" > "$tmp/runner" && cat "$tmp/runner" > "$R/scripts/tier1.sh"
grep -q "skipped everything" "$R/scripts/tier1.sh" || die "could not plant the skipping runner"
skipper=$(commit "a runner that would skip its checks after the hand-over")
branch; mkdir -p "$R/sub"; echo x > "$R/sub/zz-cfg.toml"; nested=$(commit "nested glob")
branch; table 3 > "$R/scripts/tier1_checks.txt"; g add -A; g commit -q -m "failing base"; fbase=$(g rev-parse HEAD)
mkdir -p "$R/docs/zz-tier1"; echo f > "$R/docs/zz-tier1/f.md"; failing=$(commit "touches, under a failing check")
branch; table 0 "0 tests ran" > "$R/scripts/tier1_checks.txt"; g add -A; g commit -q -m "a check that runs nothing"
ebase=$(g rev-parse HEAD)
mkdir -p "$R/docs/zz-tier1"; echo e > "$R/docs/zz-tier1/e.md"; empty=$(commit "touches, under a check that runs nothing")
g checkout -q -f --detach "$sha"; g rm -q scripts/tier1_checks.txt; g commit -q -m "no table"; notab=$(g rev-parse HEAD)
table 0 > "$R/scripts/tier1_checks.txt"; mkdir -p "$R/docs/zz-tier1"; echo n > "$R/docs/zz-tier1/n.md"
newtab=$(commit "brings the table")
branch; printf 'check bad\n  when docs/**\n  run true\n  wen typo\n' > "$R/scripts/tier1_checks.txt"
g add -A; g commit -q -m "malformed"; malformed=$(g rev-parse HEAD)
echo z > "$R/README.md"; overmal=$(commit "lands on a malformed table")

pass=0 bad=0
# case name, the landing, its base, run|list, the exit code wanted, then the regexes its output must match
# (a leading ! means must not match). A run checks out the landing first; the marker is cleared before each case.
expect() {
  local name=$1 land=$2 on=$3 mode=$4 want=$5; shift 5
  rm -f "$TIER1_TEST_OUT/marker"
  g checkout -q -f --detach "$land" || die "checkout of $land failed"
  local args=(--base "$on"); [ "$mode" = run ] && args+=(--run)
  (cd "$R" && bash scripts/tier1.sh "${args[@]}" "$land") > "$tmp/out.log" 2>&1; local rc=$? ok=1 p
  [ $rc = "$want" ] || ok=0
  for p in "$@"; do
    case "$p" in
      '!'*) ! grep -qE -- "${p#!}" "$tmp/out.log" || ok=0 ;;
      marker) [ -f "$TIER1_TEST_OUT/marker" ] || ok=0 ;;
      nomarker) [ ! -e "$TIER1_TEST_OUT/marker" ] || ok=0 ;;
      *) grep -qE -- "$p" "$tmp/out.log" || ok=0 ;;
    esac
  done
  if [ $ok = 1 ]; then
    pass=$((pass + 1)); echo "test_tier1: ok   $name (exit $rc)"
  else
    bad=$((bad + 1)); echo "test_tier1: BAD  $name (exit $rc, wanted $want)"; sed 's/^/    /' "$tmp/out.log"
  fi
}
expect lists-a-triggered-check "$touches" "$base" list 0 "marker: triggered by 1 path\(s\): docs/zz-tier1/a.md" nomarker
expect runs-a-triggered-check "$touches" "$base" run 0 "marker: PASS" "1 check\(s\) triggered: 1 passed, 0 failed" marker
expect skips-an-untouched-check "$untouched" "$base" run 0 "marker: not triggered" "0 check\(s\) triggered" nomarker
expect table-from-the-base "$dropper" "$base" run 0 "marker: triggered" "marker: PASS" marker
expect runner-from-the-base "$skipper" "$base" run 0 "marker: PASS" "!skipped everything" marker
expect glob-spans-folders "$nested" "$base" run 0 "triggered by 1 path\(s\): sub/zz-cfg.toml" marker
expect failing-check-fails "$failing" "$fbase" run 1 "marker: FAIL \(exit 3\)" "0 passed, 1 failed" marker
expect exit-0-without-expected-output-fails "$empty" "$ebase" run 1 "marker: FAIL \(exit 0, but no line of its output matches" marker
expect base-without-table "$newtab" "$notab" run 0 "NOTE: .* has no scripts/tier1_checks.txt yet" "marker: PASS" marker
expect malformed-table "$overmal" "$malformed" list 2 "unknown keyword 'wen'"

# {crates}: a check whose command names it, over the workspace members of the landing's root Cargo.toml.
ctable() {  # the one check, which echoes what {crates} became
  printf '%s\n' '# a test table' 'check crates' '  why  the test' '  when crates/** rust-toolchain.toml' \
    '  all  rust-toolchain.toml' '  skip box3d-rust' \
    "  run  echo \"expanded:{crates}:\" > \"\$TIER1_TEST_OUT/marker\"; echo \"expanded:{crates}:\"; echo marker-ok" \
    '  expect ^marker-ok$'
}
touch_() { local f; for f in "$@"; do mkdir -p "$R/$(dirname "$f")"; echo "# tier1 test" >> "$R/$f"; done; }
g checkout -q -f --detach "$sha"; ctable > "$R/scripts/tier1_checks.txt"
g add -A; g commit -q -m "base: a {crates} check"; cbase=$(g rev-parse HEAD)
cbranch() { g checkout -q -f --detach "$cbase"; }
cbranch; touch_ crates/pe-world/src/lib.rs crates/pe-core/src/lib.rs; ctwo=$(commit "touches two crates")
cbranch; touch_ crates/box3d-fork/Cargo.toml crates/pe-net-crypto/src/lib.rs
cskip=$(commit "touches a skipped crate and a prefix-named one")
cbranch; touch_ crates/box3d-fork/Cargo.toml; cnone=$(commit "touches only a skipped crate")
cbranch; touch_ rust-toolchain.toml; call=$(commit "touches a path on the 'all' line")
cbranch; touch_ crates/pe-world/src/lib.rs
sed -i 's/^  skip box3d-rust$/&\n  exempt pe-core @someone 1 lint/' "$R/scripts/tier1_checks.txt"
grep -q '^  exempt pe-core' "$R/scripts/tier1_checks.txt" || die "could not plant the exemption"
cbadown=$(commit "touches a crate, and its own table adds an exemption (no such keyword)")
cbranch; sed -i '/"crates\/pe-xr"/d' "$R/Cargo.toml"; touch_ crates/pe-xr/src/lib.rs
grep -q '"crates/pe-xr"' "$R/Cargo.toml" && die "could not drop pe-xr from the members"
cgone=$(commit "drops a member, changes a file in its folder")
cbranch; sed -i '/"crates\/pe-xr"/d' "$R/Cargo.toml"; g rm -r -q crates/pe-xr
crm=$(commit "removes a member with its folder")
expect crates-expands-the-touched-members "$ctwo" "$cbase" run 0 "expanded:-p pe-core -p pe-world:$" "crates: PASS" \
  "!\{crates\}:" marker
expect crates-leaves-out-skipped "$cskip" "$cbase" run 0 "expanded:-p pe-net-crypto:$" "\{crates\}: box3d-rust skipped" \
  marker
expect crates-nothing-left-to-run "$cnone" "$cbase" run 0 "crates: nothing to run: box3d-rust skipped" \
  "1 check\(s\) triggered: 0 passed, 0 failed, 1 with nothing to run" nomarker
expect crates-all-line-means-every-member "$call" "$cbase" run 0 "every member, for a path on its 'all' lines" \
  "expanded:-p pe-core -p pe-sim -p pe-physics .*-p pe-app:$" "!expanded:.*box3d-rust" marker
expect own-table-malformed-fails "$cbadown" "$cbase" run 1 \
  "landing's own scripts/tier1_checks.txt has an error.*unknown keyword 'exempt'" "expanded:-p pe-world:$" marker
expect own-table-malformed-fails-listing-too "$cbadown" "$cbase" list 1 \
  "landing's own scripts/tier1_checks.txt has an error" nomarker
expect crates-dropped-member-changed-fails "$cgone" "$cbase" run 1 \
  "crates: FAIL: the landing changes files in a folder that is a workspace member at the base but not in the landing" \
  "crates/pe-xr/src/lib.rs \(crates/pe-xr\)" "crates: nothing to run" nomarker
expect crates-member-removed-with-its-folder "$crm" "$cbase" run 0 \
  "crates: nothing to run: no workspace member holds a path that triggers it" "!FAIL" nomarker
# --run with the checkout elsewhere: tier1.sh refuses rather than test the wrong tree.
g checkout -q -f --detach "$base"
(cd "$R" && bash scripts/tier1.sh --base "$base" --run "$touches") > "$tmp/out.log" 2>&1; rc=$?
if [ $rc = 2 ] && grep -q "needs the checkout at" "$tmp/out.log"; then
  pass=$((pass + 1)); echo "test_tier1: ok   run-needs-the-landing-checked-out (exit $rc)"
else
  bad=$((bad + 1)); echo "test_tier1: BAD  run-needs-the-landing-checked-out (exit $rc)"; sed 's/^/    /' "$tmp/out.log"
fi
# --run tests the landing as committed: it refuses uncommitted changes, and untracked files under a triggered
# check's paths, and doesn't mind untracked files elsewhere.
dirty_case() {  # name, the exit code wanted, the regex its output must match, then the command that dirties
  local name=$1 want=$2 re=$3; shift 3
  g checkout -q -f --detach "$touches"; rm -f "$TIER1_TEST_OUT/marker"
  (cd "$R" && "$@")
  (cd "$R" && bash scripts/tier1.sh --base "$base" --run "$touches") > "$tmp/out.log" 2>&1; local rc=$?
  if [ $rc = "$want" ] && grep -qE -- "$re" "$tmp/out.log"; then
    pass=$((pass + 1)); echo "test_tier1: ok   $name (exit $rc)"
  else
    bad=$((bad + 1)); echo "test_tier1: BAD  $name (exit $rc, wanted $want)"; sed 's/^/    /' "$tmp/out.log"
  fi
  g clean -qfd > /dev/null; g checkout -q -f --detach "$touches"
}
dirty_case run-refuses-uncommitted-changes 2 "uncommitted changes" sh -c 'echo dirty >> README.md'
dirty_case run-refuses-untracked-under-a-check 2 "untracked files under check marker's paths" \
  sh -c 'echo stray > docs/zz-tier1/stray.md'
dirty_case run-allows-untracked-elsewhere 0 "marker: PASS" sh -c 'echo note > zz-notes.txt'
echo "test_tier1: $pass ok, $bad bad"
[ $bad = 0 ]
