#!/usr/bin/env bash
# Installs the gitleaks build that the landing gate (scripts/gate.sh) pins, checked against its SHA-256.
#
#   bash scripts/install_gitleaks.sh [<folder>]
#
# Puts gitleaks (gitleaks.exe on Windows) in <folder>/.tools/gitleaks-<VERSION>/; <folder> defaults to the
# root of the main checkout of this repository (the parent of git's common directory), which the gate
# searches from any worktree. It downloads only the gitleaks project's own GitHub release, into a fresh
# temporary folder, and refuses an archive whose SHA-256 is not the one pinned below. The pins were read
# on 2026-11-10 from the release's gitleaks_8.30.1_checksums.txt and from GitHub's asset digests, which
# agree. gitleaks is a Go program: a landing tool, never built into the engine, so D1 doesn't apply.
# To move to a newer gitleaks, change VERSION and the pins together, and rescan main's whole history
# (scripts/gate.sh --full-history main) in the same change.
set -euo pipefail
VERSION=8.30.1

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) os=windows ext=zip exe=gitleaks.exe ;;
  Linux) os=linux ext=tar.gz exe=gitleaks ;;
  Darwin) os=darwin ext=tar.gz exe=gitleaks ;;
  *) echo "install_gitleaks: unsupported OS $(uname -s)" >&2; exit 2 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "install_gitleaks: unsupported CPU $(uname -m)" >&2; exit 2 ;;
esac
asset="gitleaks_${VERSION}_${os}_${arch}.${ext}"
case "$asset" in
  gitleaks_8.30.1_windows_x64.zip)     want=d29144deff3a68aa93ced33dddf84b7fdc26070add4aa0f4513094c8332afc4e ;;
  gitleaks_8.30.1_windows_arm64.zip)   want=b95f5e4f5c425cedca7ee203d9afd29597e692c4924a12ed42f970537c72cc0f ;;
  gitleaks_8.30.1_linux_x64.tar.gz)    want=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb ;;
  gitleaks_8.30.1_linux_arm64.tar.gz)  want=e4a487ee7ccd7d3a7f7ec08657610aa3606637dab924210b3aee62570fb4b080 ;;
  gitleaks_8.30.1_darwin_x64.tar.gz)   want=dfe101a4db2255fc85120ac7f3d25e4342c3c20cf749f2c20a18081af1952709 ;;
  gitleaks_8.30.1_darwin_arm64.tar.gz) want=b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5 ;;
  *) echo "install_gitleaks: no pinned SHA-256 for $asset" >&2; exit 2 ;;
esac

if [ $# -ge 1 ]; then
  root=$1
else
  common=$(git rev-parse --path-format=absolute --git-common-dir)
  root=$(dirname "$common")
fi
dest="$root/.tools/gitleaks-$VERSION"
if [ -x "$dest/$exe" ] && [ "$("$dest/$exe" version 2>/dev/null)" = "$VERSION" ]; then
  echo "install_gitleaks: $dest/$exe is already gitleaks $VERSION"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
url="https://github.com/gitleaks/gitleaks/releases/download/v$VERSION/$asset"
echo "install_gitleaks: downloading $url"
curl -fsSL --retry 3 -o "$tmp/$asset" "$url"
if command -v sha256sum > /dev/null; then got=$(sha256sum "$tmp/$asset" | cut -c1-64); else got=$(shasum -a 256 "$tmp/$asset" | cut -c1-64); fi
if [ "$got" != "$want" ]; then
  echo "install_gitleaks: REFUSED: $asset has SHA-256 $got, the pinned one is $want" >&2
  exit 1
fi
mkdir -p "$tmp/x"
if [ "$ext" = zip ]; then
  unzip -q "$tmp/$asset" "$exe" -d "$tmp/x"
else
  tar -xzf "$tmp/$asset" -C "$tmp/x" "$exe"
fi
mkdir -p "$dest"
mv -f "$tmp/x/$exe" "$dest/$exe"
chmod +x "$dest/$exe"
v=$("$dest/$exe" version)
[ "$v" = "$VERSION" ] || { echo "install_gitleaks: the installed binary reports version $v, not $VERSION" >&2; exit 1; }
echo "install_gitleaks: installed gitleaks $VERSION (SHA-256 of $asset checked) at $dest/$exe"
