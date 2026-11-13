#!/usr/bin/env bash
# Prints QUIET when no heavy.sh slot is held and no cargo or rustc process is running: the condition for a clean
# timing window (coordinator). For a command watchdog; prints BUSY otherwise.
# PE_HEAVY_DIR: the folder holding heavy.sh and its .heavy-lock* slots (machine-specific, so not written here).
D="${PE_HEAVY_DIR:?set PE_HEAVY_DIR to the folder holding heavy.sh}"
held=$(ls -d "$D"/.heavy-lock* 2>/dev/null | wc -l)
if [ "$held" -ne 0 ]; then echo "BUSY locks=$held"; exit 0; fi
# Bounded, so the watchdog never sees a hung check (a slow tasklist counts as busy).
list=$(timeout 30 tasklist 2>/dev/null) || { echo "BUSY tasklist-timeout"; exit 0; }
busy=$(printf '%s\n' "$list" | grep -ciE '^(cargo|rustc)\.exe')
if [ "$busy" -eq 0 ]; then echo QUIET; else echo "BUSY procs=$busy"; fi
