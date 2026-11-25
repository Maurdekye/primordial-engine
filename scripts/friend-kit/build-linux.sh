#!/usr/bin/env bash
# Builds the friend kit (pe-app for Linux, with run.sh, collect-logs.sh and the README) from a commit, inside WSL's
# Ubuntu 24.04. Its glibc (2.39) is older than current Fedora's, so the binary runs there.
#
# usage, from Windows' Git Bash, under the team's heavy.sh (one cargo build):
#   MSYS_NO_PATHCONV=1 bash "<team folder>/heavy.sh" wsl.exe -d Ubuntu-24.04 -- \
#     bash <repo>/scripts/friend-kit/build-linux.sh <full sha> [jobs]
#
# The sources are a git checkout in WSL's home (~/pe-friend-kit/src), fetched from the repo on S:, so a rebuild
# touches only the files that changed (an archive copy would reset every mtime, or keep stale ones). The cargo
# target is ~/pe-friend-kit/target, on WSL's own disk. The kit is written to ~/pe-friend-kit/out and copied to
# <work>\friend-kit\pe-friend-kit-<sha12>.tar.gz.
#
# Optional, for a trial build (@audio-eval's Steam Audio trial):
#   PE_KIT_FEATURES   cargo features for pe-app, e.g. steam-audio-trial; the kit's name gets them as a suffix
#   PE_KIT_EXTRA      files (WSL paths, space-separated) copied into the kit beside pe-app, e.g. libphonon.so
#                     and its LICENSE.md and THIRDPARTY.md. They are never in Git.
set -euo pipefail
sha=${1:?usage: build-linux.sh <full sha> [jobs]}
jobs=${2:-3}
repo=${PE_KIT_REPO:-<repo>}
drop=${PE_KIT_DROP:-<work>/friend-kit}
base=$HOME/pe-friend-kit
# run.sh, collect-logs.sh and the README: the built commit's, or (a commit from before the kit, e.g. a trial branch)
# the ones beside this script.
here=$(cd "$(dirname "$0")" && pwd)
export PATH=$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin

mkdir -p "$base"
[ -d "$base/src/.git" ] || git init -q "$base/src"
# The repo on S: is owned by another user as WSL sees it; this fetch only reads it.
git -C "$base/src" -c safe.directory='*' fetch -q "$repo" "$sha"
git -C "$base/src" -c advice.detachedHead=false checkout -q --force FETCH_HEAD
# pe-app's build id hashes every file under crates/, untracked ones too, so a stray file would make the friend's
# build refuse to join (@engine-architect's L1 on S6). The target is outside src.
git -C "$base/src" clean -ffdxq
[ "$(git -C "$base/src" rev-parse HEAD)" = "$sha" ] || { echo "build-linux: checked out the wrong commit" >&2; exit 2; }

cd "$base/src"
uname -srm
rustc --version
ldd --version | sed -n 1p
# The kits' features: scripts/friend-kit/features.txt at this commit (comments and blank lines skipped), unless
# PE_KIT_FEATURES is set. `|| true`: under pipefail, a file of comments only (or none, before the kit) is no features,
# not an exit (an all-comment file killed both builders silently at d4f1313f).
features=${PE_KIT_FEATURES-$({ sed -e 's/#.*//' scripts/friend-kit/features.txt 2>/dev/null || true; } | xargs)}
if [[ $features =~ (^|[[:space:],])(pe-app/)?steam-audio-trial($|[[:space:],]) ]]; then
  # A trial kit without the library would run silent with nothing said at build time.
  [[ " ${PE_KIT_EXTRA:-} " =~ (^|[[:space:]/])libphonon\.so[[:space:]] ]] || {
    echo "build-linux: the steam-audio-trial kit needs Valve's libphonon.so in PE_KIT_EXTRA" >&2; exit 2; }
  python3 -B -I scripts/audit_steam_audio_trial.py
fi
feat=()
[ -z "$features" ] || feat=(--features "$features")
CARGO_TARGET_DIR=$base/target nice -n 19 cargo build --release --locked -p pe-app -j "$jobs" "${feat[@]}"

short=${sha:0:12}${features:+-${features//[ ,]/-}}
out=$base/out/pe-friend-kit-$short
rm -rf "$out"
mkdir -p "$out"
cp "$base/target/release/pe-app" "$out/"
kit=scripts/friend-kit
[ -e "$kit/run.sh" ] || { kit=$here; echo "build-linux: $sha has no scripts/friend-kit; using $kit's"; }
cp "$kit/run.sh" "$kit/collect-logs.sh" "$kit/README.md" "$out/"
chmod +x "$out/pe-app" "$out/run.sh" "$out/collect-logs.sh"
extra=()
for f in ${PE_KIT_EXTRA:-}; do cp "$f" "$out/"; extra+=("$(basename "$f")"); done
echo "$sha${features:+ features: $features}" > "$out/COMMIT"
(cd "$base/out" && tar czf "pe-friend-kit-$short.tar.gz" "pe-friend-kit-$short")
mkdir -p "$drop"
cp "$base/out/pe-friend-kit-$short.tar.gz" "$drop/"
echo "build-linux: kit $drop/pe-friend-kit-$short.tar.gz"
(cd "$out" && sha256sum pe-app "${extra[@]}")
sha256sum "$drop/pe-friend-kit-$short.tar.gz"
