#!/usr/bin/env bash
cd "E:/Libraries/Desktop/Primordial Engine/.worktrees/p2-3"
B=target/le/seeds/b/pe-spike-session-sim.exe; O=target/le/seeds
rows=("up-k8-m2:--slow 1 --slow-kbps 8 --members 2" "up-k8-m4:--slow 1 --slow-kbps 8 --members 4" "up-k12-m4:--slow 1 --slow-kbps 12 --members 4" "down-k8-m3:--slow 3 --slow-kbps 8 --members 3" "down-k8-m4:--slow 3 --slow-kbps 8 --members 4" "down-k12-m4:--slow 3 --slow-kbps 12 --members 4" "down-k12-m8:--slow 3 --slow-kbps 12 --members 8" "down-k16-m8:--slow 3 --slow-kbps 16 --members 8")
for mode in off on; do
  for seed in 46000 48000 50000 52000 54000; do
    for r in "${rows[@]}"; do
      n=${r%%:*}; a=${r#*:}
      if [ $mode = off ]; then L=1; else L=0; fi
      PE_NO_LIVE=$L PE_R18_DELAY_US=$seed $B capped --kbps 256 $a --end 30 --adaptive-only > $O/$mode-$n-s$seed.log 2>&1
    done
  done
done
echo DONE
