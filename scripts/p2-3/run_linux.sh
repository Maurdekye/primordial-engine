#!/usr/bin/env bash
# P2-3 on Linux: static musl builds of pe-session's tests and of the session harness, run inside WSL;
# then every scenario (run_session_sim.sh --pe) with its logs compared with the Windows logs at HEAD
# (timings masked). Every cargo job and the WSL runs go through the org-wide lock (heavy.sh). Never
# runs `wsl --shutdown`. Logs: target/linux-out/session-sim-pe/, target/linux-run-full.log.
# usage: scripts/p2-3/run_linux.sh   (from Git Bash on Windows, with WSL)
set -u
cd "$(dirname "$0")/../.."
heavy="/c/Users/<user>/AppData/Roaming/Orgtree v2/data/scratch/primordial-engine/_teams/1827/heavy.sh"
low() { bash "$heavy" "$@"; }
target=x86_64-unknown-linux-musl
root=$(pwd -W 2>/dev/null || pwd)
cfg="target.$target.linker=\"rust-lld\""
echo "commit $(git rev-parse --short HEAD), working tree changes: $(git status --porcelain -- crates scripts | wc -l)"
low cargo build -q --release --offline --target $target --config "$cfg" -p pe-session-harness || exit 1
low cargo test -q --release --offline --target $target --config "$cfg" -p pe-session --no-run --message-format=json > target/.linux-tests.json || exit 1
bins=$(python -I -c "
import json, sys
sys.stdout.reconfigure(newline='\\n')
for l in open(sys.argv[1], encoding='utf-8'):
    try: m = json.loads(l)
    except ValueError: continue
    if m.get('reason') == 'compiler-artifact' and m.get('executable') and m['profile']['test']:
        print(m['executable'].replace(chr(92), '/'))
" target/.linux-tests.json)
rm -f target/.linux-tests.json
while IFS= read -r b; do
  rel=${b#"$root/"}
  low wsl.exe -e sh -c "cd \"\$(wslpath '$root')\" && ./$rel -q 2>&1 | grep -E '^test result|panicked|FAILED'" < /dev/null
done <<< "$bins"
wsl.exe -e sh -c "uname -srm" < /dev/null
rm -rf target/linux-out
low wsl.exe -e bash -c "cd \"\$(wslpath '$root')\" && PE_BIN=target/$target/release PE_OUT=target/linux-out bash scripts/run_session_sim.sh --pe > target/linux-run-full.log 2>&1; tail -1 target/linux-run-full.log" < /dev/null
SKIP='^fault-' BASE=spikes/out/session-sim-pe MINE=target/linux-out/session-sim-pe bash scripts/compare_session_logs.sh HEAD
