#!/usr/bin/env bash
# Per-entry savings baseline: pe-session main (Part A in), open cells x {base, what-if 4: signatures free} x delays.
cd "E:/Libraries/Desktop/Primordial Engine/.worktrees/p2-3"
B=target/es/b/pe-session-harness.exe; O=target/es/sweep; mkdir -p $O
rows=("k8-m2:--kbps 8 --members 2" "up-k8-m4:--kbps 256 --slow 1 --slow-kbps 8 --members 4" "up-k12-m4:--kbps 256 --slow 1 --slow-kbps 12 --members 4" "down-k8-m3:--kbps 256 --slow 3 --slow-kbps 8 --members 3" "down-k12-m4:--kbps 256 --slow 3 --slow-kbps 12 --members 4" "down-k16-m4:--kbps 256 --slow 3 --slow-kbps 16 --members 4" "down-k12-m8:--kbps 256 --slow 3 --slow-kbps 12 --members 8" "down-k16-m8:--kbps 256 --slow 3 --slow-kbps 16 --members 8" "down-k24-m8:--kbps 256 --slow 3 --slow-kbps 24 --members 8")
for w in 0 4; do
  for d in 46000 48000 50000 52000 54000; do
    for r in "${rows[@]}"; do
      n=${r%%:*}; a=${r#*:}
      PE_R18_DELAY_US=$d $B capped $a --end 30 --adaptive-only --what-if $w --by-kind > $O/w$w-$n-d$d.log 2>&1
    done
  done
done
echo DONE
