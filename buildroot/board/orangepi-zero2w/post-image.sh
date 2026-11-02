#!/bin/sh
#
# One card image: the SPL where the H618 BootROM looks, a FAT boot partition, and
# the root filesystem. What goes in each is board/orangepi-zero2w/genimage.cfg.
#
# Turning boot.cmd into the script U-Boot runs, and running genimage, are in
# board/common/post-image.sh, as they are for the NanoPi; what is here is this
# board's two files and its architecture, `arm64` where the NanoPi says `arm`.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)

. "$BOARD_DIR/../common/post-image.sh"

mkbootscr "$BOARD_DIR/boot.cmd" arm64
cardimage "$BOARD_DIR/genimage.cfg"
