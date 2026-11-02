#!/bin/sh
#
# What the boards that write a card image do the same way: turn boot.cmd into
# the script U-Boot runs, and lay the image out. Sourced by each board's
# post-image.sh, which names its own boot.cmd, genimage.cfg and architecture.
#
# Expects HOST_DIR, BINARIES_DIR, TARGET_DIR and BUILD_DIR, which Buildroot sets.

# The script U-Boot runs, built here rather than by host-uboot-tools, whose
# boot.scr is made once when that package is installed and never again: the
# images went on naming rootfstype=squashfs for two days after the root became
# ext4. The arguments are Buildroot's own.
mkbootscr() {
	"$HOST_DIR/bin/mkimage" -C none -A "$2" -T script \
		-d "$1" "$BINARIES_DIR/boot.scr" >/dev/null
}

# One image, laid out by the config the board passes in.
cardimage() {
	rm -rf "$BUILD_DIR/genimage.tmp"

	genimage \
		--rootpath "$TARGET_DIR" \
		--tmppath "$BUILD_DIR/genimage.tmp" \
		--inputpath "$BINARIES_DIR" \
		--outputpath "$BINARIES_DIR" \
		--config "$1"
}
