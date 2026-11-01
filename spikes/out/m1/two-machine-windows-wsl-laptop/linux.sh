cd "/mnt/e/Libraries/Desktop/Primordial Engine/.worktrees/app/target/two-machine-1"
timeout 150 $HOME/pe-app-glibc/target/release/pe-app run --session session.json --key member-3.key --me 3 --bind 0.0.0.0:7573 --touches-per-s 3 --faults f3.json --out out -v > run-3-linux.log 2>&1
