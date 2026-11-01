cd "/mnt/e/Libraries/Desktop/Primordial Engine/.worktrees/app/target/merged/three-2win-1wsl"
uname -srm
L=$HOME/pe-app-glibc/target/release/pe-app
timeout 220 $L run --session session.json --key member-3.key --me 3 --touches-per-s 3 --faults f3.json --out out -v > run-3-linux.log 2>&1 &
wait
