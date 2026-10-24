#!/bin/sh
#
# One card image: the SPL where the H3 BootROM looks, a FAT boot partition, and
# the root filesystem. What goes in each is board/nanopi-duo2/genimage.cfg.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)
GENIMAGE_TMP="$BUILD_DIR/genimage.tmp"

# The script U-Boot runs, built from boot.cmd here rather than by
# host-uboot-tools, whose boot.scr is made once when that package is installed
# and never again: the images went on naming rootfstype=squashfs for two days
# after the root became ext4. The arguments are Buildroot's own.
"$HOST_DIR/bin/mkimage" -C none -A arm -T script \
	-d "$BOARD_DIR/boot.cmd" "$BINARIES_DIR/boot.scr" >/dev/null

rm -rf "$GENIMAGE_TMP"

genimage \
	--rootpath "$TARGET_DIR" \
	--tmppath "$GENIMAGE_TMP" \
	--inputpath "$BINARIES_DIR" \
	--outputpath "$BINARIES_DIR" \
	--config "$BOARD_DIR/genimage.cfg"
