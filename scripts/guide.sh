#!/bin/sh
# Builds the coded streams the car is shown while the phone's screen stream is
# not open, one per screen size the guide ships for.
#
# Run by hand, and commit the results. The board takes `assets/guide/*.h264` as
# they are: its build has neither a renderer nor an encoder, and the one moment
# the guide matters is the one moment a missing file would be a black screen.
#
# `guide/index.html` is the source, and the only one: every size is a fraction of
# the panel's width, so nothing is laid out per resolution.
#
# macOS only, which is where the guide is built.
set -eu
cd "$(dirname "$0")/.."

# WebKit, through the Swift that comes with the command line tools: the same
# engine the page is written against, and nothing to install.
SHOT=scripts/shot.swift

# WebKit keeps its cache and local storage under $HOME. Pointed at the project
# so a run leaves the machine's own library alone.
SANDBOX_HOME="$PWD/guide/.tmp/home"

# 16:9 and 24:9, which is what car head units are. Portrait is not a shape the
# guide is shown in.
SIZES="1280x480 1280x720 1920x720 1920x1080"

OUT=assets/guide
SHOTS=guide/shots

mkdir -p "$OUT" "$SHOTS" "$SANDBOX_HOME"
for size in $SIZES; do
  width=${size%x*}
  height=${size#*x}

  CFFIXED_USER_HOME="$SANDBOX_HOME" \
    swift "$SHOT" "$PWD/guide/index.html" "$width" "$height" "$SHOTS/$size.png"

  # A second of pictures: one keyframe and the skipped frames after it. A stream
  # of nothing but keyframes costs a keyframe a frame — 27 KB against 43 KB for
  # the second — and the panel is static, so the skipped frames are nearly free.
  #
  # scenecut=0 is not optional. On a still picture x264 finds no new scene and
  # stretches the keyframe interval to 250, so the loop wraps onto skipped frames
  # that reference a keyframe the car no longer has, and the picture tears.
  #
  # bf=0 keeps the structure close to what the phone's encoder sends, which is
  # what the car's decoder has to switch between.
  ffmpeg -hide_banner -loglevel error -y -loop 1 -i "$SHOTS/$size.png" \
    -t 1 -r 30 -pix_fmt yuv420p -c:v libx264 -profile:v high -preset slow \
    -crf 18 -bf 0 -x264-params keyint=30:min-keyint=30:scenecut=0 \
    -f h264 "$OUT/$size.h264"
done

ls -l "$OUT"
