#!/usr/bin/env bash
# Determinism matrix (design 23.4 P2-6; user ruling 2026-10-30 19:58Z: local machines only, no code
# leaves the user's machines). Runs scripts/run_determinism.sh on every local machine and compares
# the digests of every output (scripts/matrix_digest.py); fails on any difference and names the first
# differing output and line.
#
# usage: scripts/run_matrix.sh [win] [glibc] [musl] [laptop]      (default: all four)
# Run it from Git Bash on the Windows desktop, at low priority, so every run below inherits it:
#   PE_HEAVY=<heavy.sh> cmd //c start "" //low //b //wait bash scripts/run_matrix.sh
# Columns:
#   win     this PC, x86_64-pc-windows-msvc, built and run here
#   glibc   this PC's WSL (Ubuntu-24.04), x86_64-unknown-linux-gnu, built and run in WSL
#           (needs rustup and build-essential in the distro)
#   musl    this PC's WSL, x86_64-unknown-linux-musl: static binaries cross-built here with rust-lld
#           (no C toolchain), run in WSL; the wasm guests are built natively in WSL
#   laptop  the Intel laptop, x86_64-pc-windows-msvc, built and run there over ssh (no load limit)
# The WSL and <laptop> columns run the committed HEAD (shipped as a git bundle), in their own checkout
# at another path; uncommitted changes reach only the win column.
#
# Settings (environment, all optional):
#   PE_PARTS        determinism parts (default "physics wasm mcp session r18 hooks"; add "history"
#                   for the perturbation-history study, about 25 minutes per machine; p2p is left
#                   out because its UDP runs can differ between runs under loss, spike report 5b)
#   PE_HEAVY        a wrapper every local cargo command and WSL job runs through: on the desktop,
#                   the org's heavy-job lock (_teams/1827/heavy.sh)
#   PE_WSL_DISTRO   default Ubuntu-24.04
#   PE_WSL_DIR      checkout directory under the WSL home, default pe-matrix
#   PE_LAPTOP_SSH   ssh command for <laptop>, default
#                   "ssh -o BatchMode=yes -p <port> -i ~/.ssh/<key> <user>@<laptop address>"
#                   (Tailscale direct; the remote-<laptop> skill has the RustDesk fallback)
#   PE_LAPTOP_DIR   working folder on <laptop>, default C:/Users/<user>/pe-matrix; its cargo home and
#                   build targets stay there between runs (delete the folder to clean up)
#   PE_COMPARE_ONLY  run nothing: check and compare the outputs already in spikes/out/matrix
#   PE_RUSTC        toolchain for the WSL and <laptop> runs, default the channel pinned in rust-toolchain.toml
# Outputs: spikes/out/matrix/<column>/ (run.log, env.txt, o/digests.txt; the rest of o/, the full
# outputs, is not committed) and spikes/out/matrix/matrix.txt, the comparison.
set -u
cd "$(dirname "$0")/.."
COLS=${*:-win glibc musl laptop}
PARTS=${PE_PARTS:-physics wasm mcp session r18 hooks}
DISTRO=${PE_WSL_DISTRO:-Ubuntu-24.04}
WDIR=${PE_WSL_DIR:-pe-matrix}
NSSH=${PE_LAPTOP_SSH:-ssh -o BatchMode=yes -o ConnectTimeout=15 -p <port> -i $HOME/.ssh/<key> <user>@<laptop address>}
NDIR=${PE_LAPTOP_DIR:-C:/Users/<user>/pe-matrix}
TC=${PE_RUSTC:-$(sed -nE 's/^channel = "([^"]+)".*/\1/p' rust-toolchain.toml)}
GB='"C:\Program Files\Git\bin\bash.exe"'   # <laptop>'s ssh shell is cmd.exe; Git Bash runs the job
M=spikes/out/matrix
REV=$(git rev-parse HEAD)
heavy() { if [ -n "${PE_HEAVY:-}" ]; then bash "$PE_HEAVY" "$@"; else "$@"; fi; }
wsl_run() { MSYS_NO_PATHCONV=1 wsl.exe -d "$DISTRO" -- "$@"; }
# A Windows path as WSL sees it (long names: WSL cannot open 8.3 short names).
towsl() { local w; w=$(cygpath -m -l "$1"); printf '/mnt/%s%s\n' "$(echo "${w:0:1}" | tr A-Z a-z)" "${w:2}"; }
mkdir -p $M
# PE_COMPARE_ONLY=1: run nothing; check and compare the outputs already in spikes/out/matrix.
RUNCOLS=$COLS; [ -n "${PE_COMPARE_ONLY:-}" ] && RUNCOLS=""
# Every column must run the same commit (review of 3b07aa5): refuse uncommitted changes outside spikes/out.
if [ -n "$RUNCOLS" ] && { ! git diff --quiet HEAD -- . ':!spikes/out' || [ -n "$(git ls-files --others --exclude-standard -- . ':!spikes/out')" ]; }; then
  echo "run_matrix: uncommitted changes outside spikes/out; commit them first (the other columns run HEAD $REV)"
  exit 2
fi
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
[ -n "$RUNCOLS" ] && { git bundle create -q "$TMP/pe.bundle" HEAD || exit 1; }

# The job a WSL or <laptop> column runs in bash, from its working folder, as `bash job.sh build` (fetch
# the bundle into the checkout src/, check out REV, build) and then `bash job.sh run` (run the
# determinism set into <column>-out/o). Two phases, so each holds the heavy-job lock for less time.
job() {  # $1 column, $2 bundle path as that machine sees it, $3 extra setup (shell lines)
  cat <<SH
set -u
if [ "\$1" = build ]; then
  rm -rf $1-out && mkdir -p $1-out/o
  [ -d src/.git ] || git init -q src
  cd src
  git fetch -q "$2" HEAD && git -c advice.detachedHead=false checkout -q --force FETCH_HEAD || exit 1
  git clean -qfdx -e target -e spikes/wasm-guests/target
else
  cd src
fi
$3
if [ "\$1" = build ]; then
  { uname -srm; grep -m1 "model name" /proc/cpuinfo 2>/dev/null; rustc -vV; git rev-parse HEAD; } > ../$1-out/env.txt 2>&1
  PE_BUILD_ONLY=1 bash scripts/run_determinism.sh $PARTS > ../$1-out/build.log 2>&1
  echo "EXIT \$?" >> ../$1-out/build.log
else
  PE_OUT=\$(cd ../$1-out/o && pwd) bash scripts/run_determinism.sh $PARTS > ../$1-out/run.log 2>&1
  echo "EXIT \$?" >> ../$1-out/run.log
fi
SH
}

collect() {  # $1 column, $2 local directory holding run.log, env.txt and o/
  rm -rf $M/$1 && mkdir -p $M/$1 && cp -r "$2"/. $M/$1/
  python -I scripts/matrix_digest.py digest $M/$1/o > /dev/null
  echo "$1: $(grep -h "^SUMMARY" $M/$1/run.log) $(tail -1 $M/$1/run.log)"
}

for c in $RUNCOLS; do
  echo "== $c"
  case $c in
    win)
      rm -rf $M/win && mkdir -p $M/win/o
      { uname -srm; echo "${PROCESSOR_IDENTIFIER:-}"; rustc -vV; echo "$REV (working tree)"; } > $M/win/env.txt 2>&1
      PE_OUT=$M/win/o PE_HEAVY=${PE_HEAVY:-} bash scripts/run_determinism.sh $PARTS > $M/win/run.log 2>&1
      echo "EXIT $?" >> $M/win/run.log
      python -I scripts/matrix_digest.py digest $M/win/o > /dev/null
      echo "win: $(grep -h "^SUMMARY" $M/win/run.log) $(tail -1 $M/win/run.log)"
      ;;
    glibc|musl)
      WT=$(towsl "$TMP")
      extra="export PATH=\$HOME/.cargo/bin:\$PATH RUSTUP_TOOLCHAIN=$TC CARGO_BUILD_JOBS=2 PE_PYTHON=python3"
      if [ $c = musl ]; then
        # Static musl binaries, linked by Rust's bundled lld: built here, so WSL needs no C toolchain.
        T=x86_64-unknown-linux-musl
        L="target.$T.linker=\"rust-lld\""
        rustup target add --toolchain "$TC" $T > /dev/null 2>&1
        heavy cargo build --release -q --target $T --config "$L" -p pe-spike-physics -p pe-spike-session-sim \
          -p pe-spike-p2p -p pe-spike-wasm-hotswap -p pe-mcp || exit 1
        heavy cargo build -q --target $T --config "$L" -p pe-spike-physics || exit 1
        mkdir -p "$TMP/musl/release" "$TMP/musl/debug"
        cp target/$T/release/pe-spike-{physics,session-sim,p2p,wasm-hotswap} target/$T/release/pe-mcp "$TMP/musl/release/"
        cp target/$T/debug/pe-spike-physics "$TMP/musl/debug/"
        extra="$extra
rm -rf ../musl-bin && cp -r $WT/musl ../musl-bin && chmod +x ../musl-bin/*/*
export PE_BIN=\$(cd ../musl-bin/release && pwd) PE_DEBUG_BIN=\$(cd ../musl-bin/debug && pwd)"
      fi
      job $c "$WT/pe.bundle" "$extra" > "$TMP/job-$c.sh"
      # One script path per command line: heavy.sh hands it to cmd.exe, which must not see "&&" or
      # quoted spaces; MSYS2_ARG_CONV_EXCL keeps Git Bash from rewriting the /mnt path.
      for ph in build run; do
        printf 'mkdir -p ~/%s && cd ~/%s && nice -n 19 bash %s %s\n' "$WDIR" "$WDIR" "$WT/job-$c.sh" $ph > "$TMP/$ph-$c.sh"
        MSYS2_ARG_CONV_EXCL=/mnt/ heavy wsl.exe -d "$DISTRO" -- bash "$WT/$ph-$c.sh"
      done
      rm -rf "$TMP/back" && mkdir -p "$TMP/back"
      wsl_run bash -c "cd ~/$WDIR/$c-out && tar -cf - ." | tar -xf - -C "$TMP/back"
      collect $c "$TMP/back"
      ;;
    laptop)
      $NSSH "$GB -c \"mkdir -p $NDIR && cat > $NDIR/pe.bundle && sha256sum $NDIR/pe.bundle\"" < "$TMP/pe.bundle" > "$TMP/remote-sha"
      [ "$(cut -c1-64 "$TMP/remote-sha")" = "$(sha256sum "$TMP/pe.bundle" | cut -c1-64)" ] || { echo "laptop: bundle copy failed"; exit 1; }
      job laptop "$NDIR/pe.bundle" "export CARGO_HOME=$NDIR/cargo-home RUSTUP_TOOLCHAIN=$TC" > "$TMP/job-laptop.sh"
      $NSSH "$GB -c \"cd $NDIR && bash -s build\"" < "$TMP/job-laptop.sh"
      $NSSH "$GB -c \"cd $NDIR && bash -s run\"" < "$TMP/job-laptop.sh"
      rm -rf "$TMP/back" && mkdir -p "$TMP/back"
      $NSSH "$GB -c \"cd $NDIR/laptop-out && tar -cf - .\"" | tar -xf - -C "$TMP/back"
      collect laptop "$TMP/back"
      ;;
    *) echo "unknown column $c"; exit 2 ;;
  esac
done

# Fail on a column that is missing or whose own run failed, not only on a difference between columns
# (review of 3b07aa5, M2 and M3): equal failures on every machine must not pass.
specs=""
bad=""
for c in $COLS; do
  if [ ! -f $M/$c/o/digests.txt ]; then bad="$bad $c:missing"; continue; fi
  specs="$specs $c=$M/$c/o"
  for l in run build; do
    f=$M/$c/$l.log
    if [ ! -f $f ]; then
      [ $l = build ] && [ $c = win ] && continue   # the win column builds inside its run
      bad="$bad $c:no-$l.log"
      continue
    fi
    [ "$(tail -1 $f)" = "EXIT 0" ] || bad="$bad $c:$l-exit"
  done
  grep -q "^SUMMARY: determinism set PASSED" $M/$c/run.log 2>/dev/null || bad="$bad $c:not-passed"
done
if [ -n "$specs" ]; then python -I scripts/matrix_digest.py compare $specs > $M/matrix.txt; st=$?; else st=1; fi
if [ -n "$bad" ]; then echo "MATRIX: FAIL, columns failed or missing:$bad" >> $M/matrix.txt; st=1; fi
cat $M/matrix.txt
exit $st
