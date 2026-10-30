#!/usr/bin/env bash
# D2 clean-window driver: build, scaling bench (Part 2), then the determinism matrix (Part 1).
# Run once under the org lock at normal priority:
#   HEAVY_PRIO=normal bash <team folder>/heavy.sh bash scripts/run_par_window.sh
set -u
cd "$(dirname "$0")/.."
out=spikes/out/par
mkdir -p $out
{
  echo "# machine: $(powershell -NoProfile -Command '(Get-CimInstance Win32_Processor).Name') ($(nproc) logical), $(powershell -NoProfile -Command '(Get-CimInstance Win32_OperatingSystem).Caption')"
  echo "# git: $(git rev-parse --short HEAD) $(git status --short | wc -l) dirty files; rustc: $(rustc --version)"
  echo "# started $(date -u +%FT%TZ)"
} > $out/machine-win-desktop.txt
cargo build -p pe-spike-physics --release 2>&1 | tail -1
cargo build -p pe-spike-physics --release --features stage-profile --target-dir target/prof 2>&1 | tail -1
for rep in 1 2 3; do
  target/prof/release/pe-spike-physics.exe par-bench 240 500,1000,2000,4000 1,2,4,8,16 > $out/scaling-win-desktop-rep$rep.txt 2>&1
  tail -3 $out/scaling-win-desktop-rep$rep.txt | head -1
done
echo "# scaling done $(date -u +%FT%TZ)" >> $out/machine-win-desktop.txt
bash scripts/check_par_determinism.sh target/release/pe-spike-physics.exe release 1 2 3 4 8 16 32 > $out/determinism-win-desktop-release.txt 2>&1
tail -1 $out/determinism-win-desktop-release.txt
echo "# finished $(date -u +%FT%TZ)" >> $out/machine-win-desktop.txt
