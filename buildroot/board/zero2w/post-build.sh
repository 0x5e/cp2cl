#!/bin/sh
#
# Runs after the overlays are laid down and before the image is made.
#
# What every board needs — /var/log as a directory, /var/run as a symlink, the
# key permissions, the glib2 tools nothing links — is in
# board/common/post-build.sh.
# What is here is the radio: the four blobs the UWE5622 asks for, and the attach
# tool its Bluetooth half needs. Each is checked, so a board whose files are
# missing fails here instead of booting into a radio that never appears.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)
BOARD=$(basename "$BOARD_DIR")

. "$BOARD_DIR/../common/post-build.sh"

# The WCN core reads wcnmodem.bin by path rather than through request_firmware,
# first out of the directory UNISOC_FW_PATH_CONFIG names and then out of
# /lib/firmware; the Wi-Fi side composes its own name the same way —
# wifi_<chipid>_<ant>ant.ini, which is wifi_2355b001_1ant.ini on this chip. Both
# places carry a copy, because which of them the driver looks in is a build
# decision and not something to discover on a board with no console.
#
# The two bt_configure files are the chip's RF calibration. Nothing in the
# out-of-tree tree opens them by name, so they are installed for the Bluetooth
# half's own use rather than for a path this script can point at.
for f in wcnmodem.bin wifi_2355b001_1ant.ini bt_configure_pskey.ini bt_configure_rf.ini; do
	if [ ! -f "$BOARD_DIR/firmware/$f" ]; then
		echo "post-build: board/$BOARD/firmware/$f is missing" >&2
		exit 1
	fi
	install -D -m 0644 "$BOARD_DIR/firmware/$f" "$TARGET_DIR/lib/firmware/uwe5622/$f"
	ln -sf "uwe5622/$f" "$TARGET_DIR/lib/firmware/$f"
done

# BlueZ's hciattach knows no `sprd` type; the driver's Bluetooth half is a tty
# over SDIO, and this is the tool that puts the HCI line discipline on it. A
# prebuilt aarch64 binary, from the same out-of-tree set the driver comes from:
# there is no source in this tree to build it from.
if [ ! -f "$BOARD_DIR/firmware/hciattach_opi" ]; then
	echo "post-build: board/$BOARD/firmware/hciattach_opi is missing" >&2
	exit 1
fi
install -D -m 0755 "$BOARD_DIR/firmware/hciattach_opi" "$TARGET_DIR/usr/bin/hciattach_opi"
