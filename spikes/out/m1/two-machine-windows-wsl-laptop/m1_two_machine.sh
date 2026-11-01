#!/usr/bin/env bash
# m1_two_machine.sh PHASE [OUT] : the M1 mixed run across two physical machines (run from the app worktree).
#   build-laptop  ship HEAD as a bundle to the laptop and build pe-app there (release)
#   run OUT [EXTRA4] members 1-2: this PC's Windows; 3: this PC's WSL (behind `pe-app relay`); 4: the laptop.
#                 Member 2 drops out at 15 s; loss and delay on 3 and 4. EXTRA4: extra args for member 4.
# Needs scripts/matrix.local.sh (PE_LAPTOP_SSH, PE_LAPTOP_DIR) and a WSL build at ~/pe-app-glibc (linux.sh).
set -u
source scripts/matrix.local.sh
GB='"C:\Program Files\Git\bin\bash.exe"'
NSSH=$PE_LAPTOP_SSH; NDIR=$PE_LAPTOP_DIR/m1
TC=$(sed -nE 's/^channel = "([^"]+)".*/\1/p' rust-toolchain.toml)
case $1 in
build-laptop)
  T=$(mktemp -d); git bundle create -q "$T/pe.bundle" HEAD
  $NSSH "$GB -c \"mkdir -p $NDIR && cat > $NDIR/pe.bundle\"" < "$T/pe.bundle"
  $NSSH "$GB -c \"cd $NDIR && bash -s\"" <<SH
set -u
[ -d src/.git ] || git init -q src
cd src && git fetch -q ../pe.bundle HEAD && git -c advice.detachedHead=false checkout -q --force FETCH_HEAD || exit 1
export CARGO_HOME=$PE_LAPTOP_DIR/cargo-home RUSTUP_TOOLCHAIN=$TC
cargo build --release -p pe-app > ../build.log 2>&1; echo "build exit \$?"
git rev-parse --short HEAD; sha256sum target/release/pe-app.exe | cut -c1-64; uname -srm
SH
  rm -rf "$T" ;;
run)
  D=$2; X4=${3:-}
  W="$(pwd)"
  PC=$(tailscale ip -4 | head -1); LT=$(echo "$NSSH" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -1)
  WIP=$(wsl.exe -d Ubuntu-24.04 -- hostname -I | tr -d '\r' | awk '{print $1}')
  EB="<local path>/AppData/Roaming/Orgtree v2/data/scratch/primordial-engine/shader-lang/pe-app-bin/pe-app.exe"
  cp target/release/pe-app.exe "$EB"
  rm -rf "$D"; mkdir -p "$D"
  "$EB" init --addrs $PC:7571,$PC:7572,$PC:7573,$LT:7574 --frames 3600 --size 4 --out "$D" | tail -1
  echo '[{"at_ms": 3000, "loss_pct": 5, "delay_ms": 25, "jitter_ms": 10}]' > "$D/f3.json"
  echo '[{"at_ms": 2000, "loss_pct": 10, "delay_ms": 30, "jitter_ms": 10}]' > "$D/f4.json"
  { echo "code: $(git rev-parse --short HEAD)"
    echo "windows: $(sha256sum target/release/pe-app.exe | cut -c1-64)"
    echo "wsl: $(wsl.exe -d Ubuntu-24.04 -- bash -c 'sha256sum ~/pe-app-glibc/target/release/pe-app; cd ~/pe-app-glibc && uname -srm' | tr -d '\r' | cut -c1-64 | tr '\n' ' ')"
    echo "laptop: $($NSSH "$GB -c \"cd $NDIR/src && git rev-parse --short HEAD && sha256sum target/release/pe-app.exe | cut -c1-64 && uname -srm\"" | tr -d '\r' | tr '\n' ' ')"
    echo "members: 1-2 this PC (Windows), 3 this PC (WSL2, via relay), 4 the laptop (Windows), over Tailscale; all bind 0.0.0.0:PORT (a socket bound to this PC's own Tailscale address missed local traffic to it)"
  } > "$D/provenance.txt"
  $NSSH "$GB -c \"rm -rf $NDIR/run && mkdir -p $NDIR/run\""
  for f in session.json member-4.key f4.json; do $NSSH "$GB -c \"cat > $NDIR/run/$f\"" < "$D/$f"; done
  WD="/mnt/e/Libraries/Desktop/Primordial Engine/.worktrees/app/$D"
  cat > "$D/linux.sh" <<SH
cd "$WD"
timeout 150 \$HOME/pe-app-glibc/target/release/pe-app run --session session.json --key member-3.key --me 3 --bind 0.0.0.0:7573 --touches-per-s 3 --faults f3.json --out out -v > run-3-linux.log 2>&1
SH
  "$EB" relay --listen 0.0.0.0:7573 --to $WIP:7573 --for-s 160 2> "$D/relay.log" &
  (MSYS_NO_PATHCONV=1 wsl.exe -d Ubuntu-24.04 -- bash "$WD/linux.sh" > "$D/wsl.out" 2>&1 &)
  $NSSH "$GB -c \"cd $NDIR/run && timeout 150 ../src/target/release/pe-app.exe run --session session.json --key member-4.key --me 4 --bind 0.0.0.0:7574 --touches-per-s 3 --faults f4.json $X4 --out out -v > run-4-laptop.log 2>&1\"" < /dev/null &
  for m in 1 2; do
    X=""; [ $m = 2 ] && X="--crash-at-ms 15000"
    timeout 150 "$EB" run --session "$D/session.json" --key "$D/member-$m.key" --me $m --bind 0.0.0.0:757$m --touches-per-s 3 $X --out "$D/out" -v > "$D/run-$m-windows.log" 2>&1 &
  done
  wait
  sleep 3
  $NSSH "$GB -c \"cd $NDIR/run && tar -cf - run-4-laptop.log out\"" | tar -xf - -C "$D"
  for f in "$D"/run-*.log; do echo "$(basename $f): $(grep -c 'membership decision' $f) decisions; $(grep 'membership decision' $f | tail -1 | sed 's/.*decision: //')"; done | tee "$D/decisions.txt"
  "$EB" compare "$D/out" | tee "$D/compare.txt" | tail -5
  # Mask addresses and user paths in everything written.
  find "$D" -type f \( -name '*.log' -o -name '*.txt' -o -name '*.json' -o -name '*.sh' -o -name '*.out' \) -print0 | xargs -0 sed -i -E "s#$PC#<pc-tailscale>#g; s#$LT#<laptop-tailscale>#g; s#/home/[^/]+/#<home>/#g; s#[A-Z]:[/\\\\]Users[/\\\\][^/\\\\]+[/\\\\]#<local path>/#g"
  tail -2 "$D/relay.log" ;;
esac
