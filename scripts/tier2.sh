#!/usr/bin/env bash
# Tier 2's platform job (docs/landing-gates.md, "Tier 2"; docs/refound-plan.md 4.6). On one platform: the kept
# determinism base's tests in release and in debug, every other crate's tests, and clippy with warnings denied on
# every crate but box3d-rust, as tier 1 runs it. Then the RESULT lines the tests printed, per profile, become this
# platform's digests. .github/workflows/gate.yml runs it on windows-x64, linux-x64, linux-arm64 and macos-arm64,
# and a final job requires the four platforms' digests to be the same (scripts/tier2_digests.py). It runs the same
# way on a developer's machine.
#
#   bash scripts/tier2.sh [--free-space] <out-folder>
#
# Run it from the root of a checkout. Every step runs even after one fails, and each prints its result; <out-folder>
# gets each step's log, and digests/release.txt and digests/debug.txt (the RESULT lines, sorted). --free-space
# deletes the release build once its tests have run, since a hosted runner's disk holds only so much. Builds go
# without debug information and without incremental state (CARGO_PROFILE_DEV_DEBUG=0, CARGO_INCREMENTAL=0, unless
# set): neither changes what the code computes, and the target folder is a fraction of the size.
# A RESULT line is a contract (coordinator m5fa7906e0a43): a test prints one only for a value that must be the same
# on every platform, never a timing or anything that depends on the machine.
# On GitHub Actions each failed step is also written as an error annotation that names its failing tests and errors,
# since GitHub's public API serves annotations (scripts/ci_status.py --run reads them) and logs need a login.
# Exit 0 if every step passes, 1 if one fails, 2 on an error.
set -u
die() { echo "tier2: ERROR: $*" >&2; exit 2; }
free=0
if [ "${1:-}" = --free-space ]; then free=1; shift; fi
[ $# = 1 ] || die "usage: bash scripts/tier2.sh [--free-space] <out-folder>"
out=$1
[ -f Cargo.toml ] && [ -f scripts/tier2.sh ] || die "run it from the root of a checkout"
mkdir -p "$out/digests" || die "can't make $out/digests"
export CARGO_PROFILE_DEV_DEBUG=${CARGO_PROFILE_DEV_DEBUG:-0} CARGO_INCREMENTAL=${CARGO_INCREMENTAL:-0}
export CARGO_TERM_COLOR=${CARGO_TERM_COLOR:-never}
kept=(-p box3d-rust -p pe-core -p pe-sim -p pe-physics)
others=(--workspace --exclude box3d-rust --exclude pe-core --exclude pe-sim --exclude pe-physics)
where=${PE_TIER2_PLATFORM:-$(uname -s)-$(uname -m)}
cr=$(printf '\r')
fails=()

annotate() {  # step name, exit code, log: the failed step, with its failing tests and errors, as one public error
  # annotation (GitHub keeps 10 per workflow step, and this script is one step). Its lines are joined with %0A, the
  # escaped newline; a property's value may hold no comma or colon unescaped, so the title has neither.
  [ "${GITHUB_ACTIONS:-}" = true ] || return 0
  local lines
  # Failing tests by name, and errors with their place. Not "---- <test> stdout ----" or "panicked at": with
  # --show-output a passing test's output has the same header, and a should_panic test panics on purpose.
  lines=$(grep -E "^test .* FAILED$|^error(\[[A-Za-z0-9_]+\])?:|^ +--> " "$3" | tr -d "$cr" |
    head -40 | sed 's/%/%25/g' | awk '{ printf "%s%%0A", $0 }')
  echo "::error title=tier 2 $where $1::the step failed (exit $2)%0A$lines"
}

step() {  # name, then the command; its output is shown and kept in <out>/<name>.log
  local name=$1 t0 rc dt; shift
  t0=$(date +%s)
  echo "tier2: --- $name: $*"
  "$@" 2>&1 | tee "$out/$name.log"
  rc=${PIPESTATUS[0]}
  dt=$(( $(date +%s) - t0 ))
  if [ "$rc" = 0 ]; then
    echo "tier2: ok   $name ($dt s)"
  else
    echo "tier2: FAIL $name (exit $rc, $dt s)"
    fails+=("$name")
    annotate "$name" "$rc" "$out/$name.log"
  fi
}

echo "tier2: on $where, $(rustc -V 2> /dev/null || echo 'no rustc'), from $(git rev-parse --short=10 HEAD 2> /dev/null || echo '?')"
# --no-fail-fast: every test binary runs even after one fails (cargo stops at the first by default), and
# --keep-going: clippy checks every crate it can even after one fails, so one run shows every failure.
step kept-release cargo test --locked --no-fail-fast --release "${kept[@]}" -- --show-output
if [ $free = 1 ]; then
  tdir=$(cargo metadata --format-version 1 --no-deps --locked 2> /dev/null |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's|\\\\|/|g')
  if [ -n "$tdir" ] && [ -d "$tdir/release" ]; then rm -rf "$tdir/release" && echo "tier2: freed $tdir/release"; fi
fi
step kept-debug cargo test --locked --no-fail-fast "${kept[@]}" -- --show-output
step others-debug cargo test --locked --no-fail-fast "${others[@]}" -- --show-output
step clippy cargo clippy --locked --keep-going --workspace --exclude box3d-rust --all-targets --no-deps -- -D warnings

# The digests: every RESULT line the tests printed (with --show-output, a passing test's own output prints too),
# per profile. A failing test's output prints as well, so a value that differs shows up in the comparison.
digest() {  # profile, then the logs
  local p=$1; shift
  cat "$@" 2> /dev/null | tr -d "$cr" | grep '^RESULT ' | LC_ALL=C sort > "$out/digests/$p.txt"
  echo "tier2: digests/$p.txt: $(wc -l < "$out/digests/$p.txt") RESULT line(s)"
}
digest release "$out/kept-release.log"
digest debug "$out/kept-debug.log" "$out/others-debug.log"

if [ ${#fails[@]} -gt 0 ]; then
  echo "tier2: FAIL on $where: ${fails[*]}"
  exit 1
fi
echo "tier2: PASS on $where"
