#!/bin/sh
#
# Runs after the overlays are laid down and before the image is made.
#
# What both boards need — /var/log as a directory, /var/run as a symlink, the key
# permissions, the glib2 tools nothing links — is in board/common/post-build.sh.
# What is here is the firmware: three files that belong to this chip, copied with
# a check, so a board whose blobs are missing fails here instead of booting into
# a radio that never appears.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)
BOARD=$(basename "$BOARD_DIR")

. "$BOARD_DIR/../common/post-build.sh"

# Not from a package: brcmfmac-sdio-firmware carries every chip's blob and the
# generic NVRAM that goes with them, and the NVRAM is the part that has to match
# the board — the antenna path, the clock, the MAC. These three come off the
# board itself, from the vendor image, under /lib/firmware/ap6212/; mainline
# wants them in /lib/firmware/brcm/ under the names below.
for f in brcmfmac43430-sdio.bin brcmfmac43430-sdio.txt BCM43430A1.hcd; do
	if [ ! -f "$BOARD_DIR/firmware/$f" ]; then
		echo "post-build: board/$BOARD/firmware/$f is missing" >&2
		exit 1
	fi
	install -D -m 0644 "$BOARD_DIR/firmware/$f" "$TARGET_DIR/lib/firmware/brcm/$f"
done
