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
# On GitHub Actions each failed step is also written as an error annotation that names its failing tests, with their
# panic messages, and its errors, with what a failed linker said, since GitHub's public API serves annotations
# (scripts/ci_status.py --run reads them) and logs need a login.
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
  local errors tests lines panics linker
  # Errors with their place, then failing tests by name, each line once: an error that many tests hit (a crate that
  # can't be built) would otherwise fill the annotation. The errors come first because each failed test binary has
  # one ("error: test failed, to rerun pass ..."), so every binary that failed is named inside what GitHub keeps,
  # however many tests did (run 44208935602: 29 names on macOS, and the last binary's error line was cut). A place
  # ("  --> file:line") is kept only on the line right after its error: rustc gives each warning one too, and 40 of
  # those would fill the list before any binary's line (chunks-reviewer's L1, m5d2ff383e4d1).
  errors=$(tr -d "$cr" < "$3" | awk '
    /^error(\[[A-Za-z0-9_]+\])?:/ { print; e = 1; next }
    e && /^ +--> / { print }
    { e = 0 }' | awk '!seen[$0]++' | cut -c1-300 | head -40)
  tests=$(grep -E "^test .* FAILED$" "$3" | tr -d "$cr" | awk '!seen[$0]++' | head -40)
  # Then each failing test's panic or error, from libtest's "failures:" section: it opens with "failures:", holds
  # a "---- <test> stdout ----" block for each failed test, and closes with a second "failures:" over their names.
  # A passing test's output (--show-output) comes under "successes:" instead, so a should_panic test's panic, which
  # is on purpose, stays out. A test's own output may come before its panic, so the panic is looked for. Each line
  # at most 300 characters: a panic can be one line of several kB (a {:?}-printed BuildError with the linker's
  # command line), which would leave no room for the next test's (chunks-reviewer mf5f63ce0278a).
  panics=$(tr -d "$cr" < "$3" | awk '
    /^running [0-9]+ tests?$/ || /^test result: / { inf = 0; next }
    /^failures:$/ { inf = !inf; next }
    !inf { next }
    /^---- .* stdout ----$/ { print; k = 0; next }
    /^thread .* panicked at / || /^Error: / { print; k = 6; next }
    k > 0 && NF { print; k-- }' | cut -c1-300 | head -40)
  # And what a linker that failed said. rustc gives it in the "= note:" lines after "error: linking with ...
  # failed", continued on lines indented by ten spaces. A test that prints a build error with {:?} (pe-logic's
  # BuildError keeps those notes since 8b197117) has the same lines on one, its newlines and quotes escaped: such a
  # line is unfolded first. Two notes are left out: the linker's whole command line (it starts with a quote, or on
  # Linux with an environment assignment) and the one saying arguments were omitted. Each line once, at most 300
  # characters and 10 lines. They come first: GitHub keeps an annotation's first 4096 characters, and on tier 2's
  # macOS runs (2026-11-18) the failing tests and their panics filled those before the linker's words.
  linker=$(tr -d "$cr" < "$3" | awk '
    /error: linking with .*\\n/ {
      s = substr($0, index($0, "error: linking with "))
      while ((i = index(s, "\\n")) > 0) {
        t = substr(s, 1, i - 1); gsub(/\\"/, "\"", t); print t
        s = substr(s, i + 2)
      }
      gsub(/\\"/, "\"", s); print s
      next
    }
    { print }' | awk '
    /^error: linking with .* failed/ { lk = 1; skip = 0; next }
    !lk { next }
    /^  = note: / {
      s = substr($0, 11)
      skip = (s ~ /^"/ || s ~ /^[A-Za-z_][A-Za-z0-9_]*=/ || s ~ /^some arguments are omitted/)
      if (!skip) print "linker: " s
      next
    }
    /^          [^ ]/ { if (!skip) print "linker: " substr($0, 11); next }
    /^ *$/ || /^  \|/ { next }
    { lk = 0 }' | awk '!seen[$0]++' | cut -c1-300 | head -10)
  lines=$(printf '%s\n%s\n%s\n%s\n' "$linker" "$errors" "$tests" "$panics" | sed '/^$/d' | sed 's/%/%25/g' |
    awk '{ printf "%s%%0A", $0 }')
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
