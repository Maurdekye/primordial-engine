#!/usr/bin/env bash
# Records every ball-pit matrix cell on the session simulator, renders the engager and observer
# views with wgpu, and encodes them with ffmpeg (default C:/ffmpeg/bin/ffmpeg.exe).
# Outputs: docs/media/ballpit-matrix/{ballpit-matrix-grid.mp4, cells/*.mp4, metrics/*.csv,
# summary.md, verification.txt, adapter.txt}. See docs/ballpit-video-matrix.md.
set -eu
cd "$(dirname "$0")/.."
cargo build --release -q -p pe-ballpit-viz
./target/release/pe-ballpit-viz "$@"
