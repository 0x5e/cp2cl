#!/bin/sh
#
# One card image: the SPL where the H3 BootROM looks, a FAT boot partition, and
# the root filesystem. What goes in each is board/nanopi-duo2/genimage.cfg.
#
# Turning boot.cmd into the script U-Boot runs, and running genimage, are in
# board/common/post-image.sh, as they are for the Zero 2W; what is here is this
# board's two files and its architecture.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)

. "$BOARD_DIR/../common/post-image.sh"

mkbootscr "$BOARD_DIR/boot.cmd" arm
cardimage "$BOARD_DIR/genimage.cfg"
