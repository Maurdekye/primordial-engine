#!/usr/bin/env bash
# Runs the Linux (static musl) build of the physics spike inside WSL and writes its trace.
set -e
cd "$(dirname "$0")/.."
B=target/x86_64-unknown-linux-musl/release/pe-spike-physics
uname -srm
grep -m1 "model name" /proc/cpuinfo
"$B" goldens
"$B" trace 600 spikes/out/trace-linux-musl-release.txt
"$B" rollback
