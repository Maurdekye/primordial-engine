#!/usr/bin/env bash
# Builds the friend test's Windows side (the user's PC): pe-app.exe in release from a commit, with run-windows.ps1 and
# README-windows.md, into friend-kit\pe-friend-win-<sha12>\ beside the main clone, or $PE_KIT_DROP (friend-test S7).
#
# usage, from Git Bash in a worktree of the repo, under the team's heavy.sh (one cargo build):
#   bash "<team folder>/heavy.sh" bash scripts/friend-kit/build-windows.sh <full sha>
#
# It builds in the worktree it is run from, which must be at <sha> with nothing changed, untracked or ignored under
# crates/ and in the root Cargo.lock and Cargo.toml: pe-app's build id hashes all of them, ignored files included
# (@engine-architect, @design-reviewer's L1 on S7), and both machines' ids must agree. The first line
# pe-app prints ("pe-app ..., sources <id>") must be the same in both kits' logs.
# The same holds for the Steam Audio trial's audit files and this kit's own scripts, so the audit that runs and the
# launcher that ships are the commit's (@coordinator-opus; build-linux.sh's clean checkout does this).
# The Linux kit (build-linux.sh) is built from the same commit.
#
# For the Steam Audio trial (features.txt; docs/steam-audio-trial.md), give Valve's files, which are never in Git:
#   PE_KIT_EXTRA      files (Git Bash paths, space-separated) copied into the kit beside pe-app.exe: phonon.dll and
#                     its LICENSE.md and THIRDPARTY.md. run-windows.ps1 points PE_STEAM_AUDIO_LIB at phonon.dll.
#
# No path in this PC's user profile goes into the binary: cargo's home (the registry's sources, named by panic
# locations) is remapped to /cargo, and the build fails if the binary still names the profile (as in build-linux.sh,
# @coordinator-opus). The worktree's own path stays, because pe-logic and pe-app keep their crate
# folders.
set -euo pipefail
sha=${1:?usage: build-windows.sh <full sha>}
main=$(git rev-parse --show-toplevel)
main=${main%%/.worktrees/*}
drop=${PE_KIT_DROP:-$(dirname "$main")/friend-kit}
[ "$(git rev-parse HEAD)" = "$sha" ] || { echo "build-windows: this worktree isn't at $sha (it's $(git rev-parse HEAD))" >&2; exit 2; }
left=$(git status --porcelain --ignored -- crates Cargo.lock Cargo.toml scripts/friend-kit scripts/audit_steam_audio_trial.py \
  scripts/audit_native.py scripts/native_allow.txt scripts/native_allow_steam_audio.txt)
[ -z "$left" ] || { echo "build-windows: changed, untracked or ignored files would change the build id, the audit or the kit:" >&2; echo "$left" >&2; exit 2; }
unset CARGO
# The kits' features: scripts/friend-kit/features.txt at this commit (as build-linux.sh), unless PE_KIT_FEATURES is set.
features=${PE_KIT_FEATURES-$({ sed -e 's/#.*//' scripts/friend-kit/features.txt 2>/dev/null || true; } | xargs)}
if [[ $features =~ (^|[[:space:],])(pe-app/)?steam-audio-trial($|[[:space:],]) ]]; then
  # A trial kit without the library would run silent with nothing said at build time, and Apache-2.0 4(a) wants the
  # licence and notices beside it (docs/steam-audio-trial.md; @design-reviewer's L1).
  for need in phonon.dll LICENSE.md THIRDPARTY.md; do
    have=
    for f in ${PE_KIT_EXTRA:-}; do [ "$(basename "$f")" != "$need" ] || have=1; done
    [ -n "$have" ] || { echo "build-windows: the steam-audio-trial kit needs Valve's $need in PE_KIT_EXTRA" >&2; exit 2; }
  done
  python -B -I scripts/audit_steam_audio_trial.py
fi
feat=()
[ -z "$features" ] || feat=(--features "$features")
cargo_home=$(cygpath -w "${CARGO_HOME:-$USERPROFILE/.cargo}")
# RUSTFLAGS replaces every cargo config's build.rustflags and target.*.rustflags: none are set today [V,
# @design-reviewer's N1]. A flag added there later must be added here too, or the kit builds without it.
RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo" cargo build --release --locked -p pe-app "${feat[@]}"
target=$(cargo metadata --format-version 1 --no-deps | python -c "import json,sys; print(json.load(sys.stdin)['target_directory'])")
short=${sha:0:12}${features:+-${features//[ ,\/]/-}}
out=$drop/pe-friend-win-$short
rm -rf "$out"
mkdir -p "$out"
cp "$target/release/pe-app.exe" "$out/"
profile="$(cygpath -w "$USERPROFILE")\\"
# No -i: Git Bash's grep -i aborts on binary input (the team's scrub note); cygpath gives the profile's own case.
# The profile, and the bare user name for any other spelling (forward slashes, another case; @design-reviewer's L1).
named=$(grep -a -c -F -e "$profile" -e "$USERNAME" "$out/pe-app.exe" || true)
if [ "${named:-0}" != 0 ]; then
  echo "build-windows: pe-app.exe still names $profile or $USERNAME (on $named lines)" >&2
  exit 2
fi
here=$(cd "$(dirname "$0")" && pwd)
cp "$here/run-windows.ps1" "$here/README-windows.md" "$out/"
extra=()
for f in ${PE_KIT_EXTRA:-}; do cp "$f" "$out/"; extra+=("$(basename "$f")"); done
echo "$sha${features:+ features: $features}" > "$out/COMMIT"
echo "build-windows: $out"
(cd "$out" && sha256sum pe-app.exe "${extra[@]}")
