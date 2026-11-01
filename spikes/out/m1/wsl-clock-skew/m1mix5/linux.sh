cd "/mnt/e/Libraries/Desktop/Primordial Engine/.worktrees/app/target/m1mix5"
uname -srm
L=$HOME/pe-app-glibc/target/release/pe-app
timeout 100 $L run --session session.json --key member-3.key --me 3 --touches-per-s 3 --faults f3.json --no-link-signal --out out -v > run-3-linux.log 2>&1 &
timeout 100 $L run --session session.json --key member-4.key --me 4 --touches-per-s 3 --crash-at-ms 15000 --no-link-signal --out out -v > run-4-linux.log 2>&1 &
wait
