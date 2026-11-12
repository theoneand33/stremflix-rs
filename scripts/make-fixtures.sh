#!/bin/sh
# Generate legal demo media (ffmpeg test pattern + sine tone; no third-party content).
# Output: demo-media/addon/{test-pattern.mp4,poster.jpg} (served/seeded by test-addon)
#         demo-media/library/{test-pattern-local.mp4,clips/tone.webm} (local library demo)
set -eu
out="${1:-demo-media}"
mkdir -p "$out/addon" "$out/library/clips"
gen() { ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=duration=$1:size=640x360:rate=25" \
  -f lavfi -i "sine=frequency=440:duration=$1" -shortest -c:v "$2" -b:v 400k -c:a "$3" "$4"; }
# Some distro builds (Fedora's ffmpeg-free) have no libx264; mpv plays any of these.
mp4=mpeg4
for c in libopenh264 libx264; do ffmpeg -hide_banner -encoders 2>/dev/null | grep -q " $c " && mp4=$c; done
gen 120 "$mp4" aac "$out/addon/test-pattern.mp4"
ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=240x360" -frames:v 1 "$out/addon/poster.jpg"
gen 120 "$mp4" aac "$out/library/test-pattern-local.mp4"
gen 30 libvpx-vp9 libopus "$out/library/clips/tone.webm"
echo "fixtures written to $out"
