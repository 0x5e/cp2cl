#!/bin/sh
#
# Runs after Buildroot has laid the overlays down and before the image is made.
#
# The firmware is the one thing that does not come from an overlay: three files
# that belong to this chip, copied with a check, so a board whose blobs are
# missing fails here with a line naming them rather than booting into a radio
# that never appears.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)
BOARD=$(basename "$BOARD_DIR")

# Not from a package: brcmfmac-sdio-firmware carries every chip's blob and this
# board needs one chip's. Three files, half a megabyte.
#
# They come off the board itself, from the image it runs now: the vendor keeps
# them under /lib/firmware/ap6212/ with their own names, and mainline wants them
# in /lib/firmware/brcm/ under the names below. Copying them across is a
# `scp` and a rename; nothing about them is a build product.
for f in brcmfmac43430-sdio.bin brcmfmac43430-sdio.txt BCM43430A1.hcd; do
	if [ ! -f "$BOARD_DIR/firmware/$f" ]; then
		echo "post-build: board/$BOARD/firmware/$f is missing" >&2
		exit 1
	fi
	install -D -m 0644 "$BOARD_DIR/firmware/$f" "$TARGET_DIR/lib/firmware/brcm/$f"
done

# The root is a read-only squashfs, so this is the mount point for the tmpfs that
# /etc/fstab puts the program's log on — and it has to exist before mount -a.
install -d -m 0755 "$TARGET_DIR/var/log"
