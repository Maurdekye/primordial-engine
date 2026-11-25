#!/usr/bin/env bash
# Packs the friend test's logs into one file to send back.
#   ./collect-logs.sh          the latest run
#   ./collect-logs.sh all      every run
set -eu
root=${PE_KIT_LOGS:-$HOME/pe-friend-test/logs}
[ -d "$root" ] || { echo "no logs yet in $root (run ./run.sh first)"; exit 1; }
cd "$root"
if [ "${1:-}" = all ]; then
  runs=$(ls -1)
else
  runs=$(ls -1 | tail -1)
fi
out=$HOME/pe-friend-test/pe-logs-$(date -u +%Y%m%dT%H%M%SZ).tar.gz
# shellcheck disable=SC2086
tar czf "$out" $runs
echo "logs packed: $out ($(du -h "$out" | cut -f1)). Send this file back."
