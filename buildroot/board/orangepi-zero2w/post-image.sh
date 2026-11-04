#!/bin/sh
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)

. "$BOARD_DIR/../common/post-image.sh"

mkbootscr "$BOARD_DIR/boot.cmd" arm64
cardimage "$BOARD_DIR/genimage.cfg"
