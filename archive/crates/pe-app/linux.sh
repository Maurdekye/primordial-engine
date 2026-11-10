#!/usr/bin/env bash
# Native Linux (glibc) build, tests and the M1 loopback scenario inside WSL, from a copy of HEAD.
#
#   bash crates/pe-app/linux.sh        # on Windows (Git Bash); re-enters itself in WSL as `inner`
#
# The copy goes to ~/pe-app-glibc (target dir kept between runs); every file is touched, because
# archived files carry the commit's time and could leave a stale binary.
set -euo pipefail
cd "$(dirname "$0")/../.."
case "${1:-}" in
"")
    mkdir -p target
    rm -f target/pe-app-head.tar
    git archive --format=tar -o target/pe-app-head.tar HEAD
    MSYS_NO_PATHCONV=1 wsl.exe -d Ubuntu-24.04 --cd "$(pwd -W)" -- bash crates/pe-app/linux.sh inner "$(git rev-parse --short HEAD)"
    ;;
inner)
    source ~/.cargo/env 2>/dev/null || true
    W=~/pe-app-glibc
    mkdir -p "$W"
    find "$W" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
    tar -xf target/pe-app-head.tar -C "$W"
    find "$W" -path "$W/target" -prune -o -type f -exec touch {} +
    cd "$W"
    echo "# $(uname -srm); $(rustc --version); $(gcc --version | head -1); sources: HEAD ${2:-?}"
    export CARGO_BUILD_JOBS=2
    nice -n 19 cargo build --release -p pe-app
    nice -n 19 cargo test -p pe-app -p pe-mcp -- --test-threads=2
    file target/release/pe-app
    bash crates/pe-app/m1-loopback.sh target/release/pe-app /tmp/pe-m1 7471
    ;;
esac
