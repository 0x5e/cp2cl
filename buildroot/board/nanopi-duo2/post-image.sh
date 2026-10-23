#!/bin/sh
#
# One card image: the SPL where the H3 BootROM looks, a FAT boot partition, and
# the root filesystem. What goes in each is board/nanopi-duo2/genimage.cfg.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)
GENIMAGE_TMP="$BUILD_DIR/genimage.tmp"

rm -rf "$GENIMAGE_TMP"

genimage \
	--rootpath "$TARGET_DIR" \
	--tmppath "$GENIMAGE_TMP" \
	--inputpath "$BINARIES_DIR" \
	--outputpath "$BINARIES_DIR" \
	--config "$BOARD_DIR/genimage.cfg"
