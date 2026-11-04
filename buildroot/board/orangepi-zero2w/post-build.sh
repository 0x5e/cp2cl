#!/bin/sh
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)
BOARD=$(basename "$BOARD_DIR")

. "$BOARD_DIR/../common/post-build.sh"

for f in wcnmodem.bin wifi_2355b001_1ant.ini bt_configure_pskey.ini bt_configure_rf.ini; do
	if [ ! -f "$BOARD_DIR/firmware/$f" ]; then
		echo "post-build: board/$BOARD/firmware/$f is missing" >&2
		exit 1
	fi
	install -D -m 0644 "$BOARD_DIR/firmware/$f" "$TARGET_DIR/lib/firmware/uwe5622/$f"
	ln -sf "uwe5622/$f" "$TARGET_DIR/lib/firmware/$f"
done

if [ ! -f "$BOARD_DIR/firmware/hciattach_opi" ]; then
	echo "post-build: board/$BOARD/firmware/hciattach_opi is missing" >&2
	exit 1
fi
install -D -m 0755 "$BOARD_DIR/firmware/hciattach_opi" "$TARGET_DIR/usr/bin/hciattach_opi"
