#!/bin/sh
# Generate legal demo media (ffmpeg test pattern + sine tone; no third-party content).
# Output: demo-media/addon/test-pattern.mp4 (served/seeded by test-addon)
#         demo-media/library/{test-pattern-local.mp4,clips/tone.webm} (local library demo)
set -eu
out="${1:-demo-media}"
mkdir -p "$out/addon" "$out/library/clips"
gen() { ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=duration=$1:size=640x360:rate=25" \
  -f lavfi -i "sine=frequency=440:duration=$1" -shortest -c:v "$2" -c:a "$3" "$4"; }
gen 120 libx264 aac "$out/addon/test-pattern.mp4"
gen 120 libx264 aac "$out/library/test-pattern-local.mp4"
gen 30 libvpx-vp9 libopus "$out/library/clips/tone.webm"
echo "fixtures written to $out"
