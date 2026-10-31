#!/usr/bin/env bash
# P2-9 on P2-3, Linux: a static musl build of tools/session-sim (on pe-session), run inside WSL with
# `regress` against the committed pe-session logs. Paths are passed explicitly: the tool's defaults
# are compiled in as Windows paths. Every cargo job and the WSL run go through the org-wide lock
# (heavy.sh); never runs `wsl --shutdown`. Output: target/linux-regress.txt.
# usage: scripts/p2-3/run_linux_regress.sh   (from Git Bash on Windows, with WSL)
set -u
cd "$(dirname "$0")/../.."
heavy="/c/Users/<user>/AppData/Roaming/Orgtree v2/data/scratch/primordial-engine/_teams/1827/heavy.sh"
low() { bash "$heavy" "$@"; }
target=x86_64-unknown-linux-musl
root=$(pwd -W 2>/dev/null || pwd)
cfg="target.$target.linker=\"rust-lld\""
echo "commit $(git rev-parse --short HEAD), working tree changes: $(git status --porcelain -- crates tools scripts | wc -l)"
low cargo build -q --release --offline --target $target --config "$cfg" -p pe-session-sim || exit 1
rm -rf target/linux-out/session-sim-tool
low wsl.exe -e bash -c "cd \"\$(wslpath '$root')\" && uname -srm && target/$target/release/pe-session-sim regress --jobs 2 --scenarios tools/session-sim/scenarios --expected spikes/out/session-sim-pe --out target/linux-out/session-sim-tool > target/linux-regress.txt 2>&1; echo \"exit \$?\" >> target/linux-regress.txt" < /dev/null
tail -2 target/linux-regress.txt
