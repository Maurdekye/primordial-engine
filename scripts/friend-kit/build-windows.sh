#!/usr/bin/env bash
# Builds the friend test's Windows side (the user's PC): pe-app.exe in release from a commit, with run-windows.ps1 and
# README-windows.md, into <work>\friend-kit\pe-friend-win-<sha12>\ (friend-test S7).
#
# usage, from Git Bash in a worktree of the repo, under the team's heavy.sh (one cargo build):
#   bash "<team folder>/heavy.sh" bash scripts/friend-kit/build-windows.sh <full sha>
#
# It builds in the worktree it is run from, which must be at <sha> with nothing changed, untracked or ignored under
# crates/ and in the root Cargo.lock and Cargo.toml: pe-app's build id hashes all of them, ignored files included
# (@engine-architect, @design-reviewer's L1 on S7), and both machines' ids must agree. The first line
# pe-app prints ("pe-app ..., sources <id>") must be the same in both kits' logs.
# The Linux kit (build-linux.sh) is built from the same commit.
set -euo pipefail
sha=${1:?usage: build-windows.sh <full sha>}
drop=${PE_KIT_DROP:-<work>/friend-kit}
[ "$(git rev-parse HEAD)" = "$sha" ] || { echo "build-windows: this worktree isn't at $sha (it's $(git rev-parse HEAD))" >&2; exit 2; }
left=$(git status --porcelain --ignored -- crates Cargo.lock Cargo.toml)
[ -z "$left" ] || { echo "build-windows: changed, untracked or ignored files would change the build id:" >&2; echo "$left" >&2; exit 2; }
unset CARGO
# The kits' features: scripts/friend-kit/features.txt at this commit (as build-linux.sh), unless PE_KIT_FEATURES is set.
features=${PE_KIT_FEATURES-$({ sed -e 's/#.*//' scripts/friend-kit/features.txt 2>/dev/null || true; } | xargs)}
feat=()
[ -z "$features" ] || feat=(--features "$features")
cargo build --release --locked -p pe-app "${feat[@]}"
target=$(cargo metadata --format-version 1 --no-deps | python -c "import json,sys; print(json.load(sys.stdin)['target_directory'])")
short=${sha:0:12}
out=$drop/pe-friend-win-$short
rm -rf "$out"
mkdir -p "$out"
cp "$target/release/pe-app.exe" "$out/"
here=$(cd "$(dirname "$0")" && pwd)
cp "$here/run-windows.ps1" "$here/README-windows.md" "$out/"
echo "$sha${features:+ features: $features}" > "$out/COMMIT"
echo "build-windows: $out"
sha256sum "$out/pe-app.exe"
