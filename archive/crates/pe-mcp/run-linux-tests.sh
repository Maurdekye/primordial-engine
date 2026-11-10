#!/usr/bin/env bash
# Runs pe-mcp's tests on Linux without a Linux toolchain: build static musl test binaries on Windows
# (linked by Rust's bundled lld, as scripts/run-linux-physics.sh does), then run them inside WSL.
#
#   bash crates/pe-mcp/run-linux-tests.sh build   # on Windows (Git Bash), through the org's heavy.sh lock
#   bash crates/pe-mcp/run-linux-tests.sh run     # on Windows: runs the binaries in WSL Ubuntu-24.04 (via `inner`)
#   bash crates/pe-mcp/run-linux-tests.sh glibc   # on Windows: native glibc build and test inside WSL, from a
#                                                 # copy of HEAD in ~/pe-mcp-glibc (via `glibc-inner`)
#
# The test binaries carry Windows paths from compile time; PE_MCP_EXE and PE_MCP_DIR point them at
# the Linux paths instead.
set -euo pipefail
cd "$(dirname "$0")/../.."
T=x86_64-unknown-linux-musl
case "${1:-}" in
build)
    mkdir -p target/$T
    cargo test -p pe-mcp --no-run --target $T --config "target.$T.linker=\"rust-lld\"" --message-format=json \
        | python -c "import sys,json
for l in sys.stdin:
    try: d=json.loads(l)
    except Exception: continue
    if d.get('reason')=='compiler-artifact' and d.get('executable') and d['target']['kind']!=['bin']: print(d['executable'])" \
        > target/$T/pe-mcp-tests.txt
    cat target/$T/pe-mcp-tests.txt
    ;;
run)
    # wsl.exe re-splits a command line on spaces, and this repository's path has one: re-enter this
    # script inside Linux instead of passing a command string.
    MSYS_NO_PATHCONV=1 wsl.exe -d Ubuntu-24.04 --cd "$(pwd -W)" -- bash crates/pe-mcp/run-linux-tests.sh inner
    ;;
inner)
    uname -srm
    export PE_MCP_EXE="$PWD/target/$T/debug/pe-mcp" PE_MCP_DIR="$PWD/crates/pe-mcp"
    fail=0
    while read -r exe; do
        b="$(wslpath "$exe")"
        echo "== $(basename "$b")"
        "$b" --test-threads=2 || fail=1
    done < <(tr -d '
' < "target/$T/pe-mcp-tests.txt")
    exit $fail
    ;;
glibc)
    rm -f target/pe-mcp-head.tar
    git archive --format=tar -o target/pe-mcp-head.tar HEAD
    MSYS_NO_PATHCONV=1 wsl.exe -d Ubuntu-24.04 --cd "$(pwd -W)" -- bash crates/pe-mcp/run-linux-tests.sh glibc-inner "$(git rev-parse --short HEAD)"
    ;;
glibc-inner)
    source ~/.cargo/env 2>/dev/null || true
    W=~/pe-mcp-glibc
    mkdir -p "$W"
    # Replace the sources, keep the target dir. Touch everything: archived files carry the commit's
    # time, which could be older than a previous build's outputs and leave a stale binary.
    find "$W" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
    tar -xf target/pe-mcp-head.tar -C "$W"
    find "$W" -path "$W/target" -prune -o -type f -exec touch {} +
    cd "$W"
    echo "# $(uname -srm); $(rustc --version); $(gcc --version | head -1); sources: HEAD ${2:-?}"
    export CARGO_BUILD_JOBS=2
    nice -n 19 cargo test -p pe-mcp -- --test-threads=2
    ;;
*)
    echo "usage: $0 build|run|glibc" >&2
    exit 2
    ;;
esac
