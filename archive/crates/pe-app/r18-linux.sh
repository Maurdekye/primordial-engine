#!/usr/bin/env bash
# R18 on native Linux (glibc) inside WSL, from a copy of HEAD: pe-app's tests, then r18-capped.sh with
# no cap, and with an 80 kbit/s cap with the link signal on and off. Outputs: target/r18-linux/.
#
#   bash crates/pe-app/r18-linux.sh        # on Windows (Git Bash); re-enters itself in WSL as `inner`
set -euo pipefail
cd "$(dirname "$0")/../.."
case "${1:-}" in
"")
    mkdir -p target
    rm -f target/pe-app-head.tar
    git archive --format=tar -o target/pe-app-head.tar HEAD
    MSYS_NO_PATHCONV=1 wsl.exe -d Ubuntu-24.04 --cd "$(pwd -W)" -- bash crates/pe-app/r18-linux.sh inner "$(git rev-parse --short HEAD)"
    ;;
inner)
    source ~/.cargo/env 2>/dev/null || true
    R=$(pwd)/target/r18-linux
    W=~/pe-app-glibc
    mkdir -p "$W" "$R"
    find "$W" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
    tar -xf target/pe-app-head.tar -C "$W"
    find "$W" -path "$W/target" -prune -o -type f -exec touch {} +
    cd "$W"
    echo "# $(uname -srm); $(rustc --version); sources: HEAD ${2:-?}"
    export CARGO_BUILD_JOBS=2
    nice -n 19 cargo build --release -p pe-app
    nice -n 19 cargo test --release -p pe-app -- --test-threads=2
    for run in "base 0 on 7501 900" "cap80-on 80 on 7511 1800" "cap80-off 80 off 7521 1800"; do
        set -- $run
        echo "== $1"
        nice -n 19 bash crates/pe-app/r18-capped.sh target/release/pe-app "$R/$1" "$2" "$3" "$4" "$5" || true
        python3 -I crates/pe-app/r18-report.py "$R/$1" || true
    done
    ;;
esac
