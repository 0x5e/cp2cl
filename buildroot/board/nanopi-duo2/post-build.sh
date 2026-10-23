#!/bin/sh
#
# Runs after the overlays are laid down and before the image is made.
#
# The firmware is the one thing that does not come from an overlay: three files
# that belong to this chip, copied with a check, so a board whose blobs are
# missing fails here instead of booting into a radio that never appears.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)
BOARD=$(basename "$BOARD_DIR")

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

# The skeleton makes /var/log a symlink to /tmp, and /tmp is a tmpfs: a log
# written there is gone at the next boot, and the run that ended badly is the one
# that has to be explained. On this root it is a directory on the disk.
rm -rf "$TARGET_DIR/var/log"
install -d -m 0755 "$TARGET_DIR/var/log"

# Where the program keeps the identity it presents to the phone. Created here so
# the first run does not have to, and owned by root rather than by the program.
install -d -m 0755 "$TARGET_DIR/var/lib"

# The skeleton gives /var/run as a directory of its own, while everything that
# writes a pid file expects the /run the kernel mounts as a tmpfs — and a
# directory from the image would survive a reboot with stale pids in it.
rm -rf "$TARGET_DIR/var/run"
ln -s ../run "$TARGET_DIR/var/run"

# The overlays are rsynced with a --chmod that leaves every file 0644, and
# dropbear refuses a host key anyone but its owner can read.
chmod 600 "$TARGET_DIR/etc/dropbear/dropbear_ed25519_host_key"

# bluez5-utils selects libglib2 unconditionally, and glib2 installs command line
# tools and a GIO that nothing here links: 3.3 MB of the rootfs. Listed by name
# rather than globbed by directory so that a package which later needs one of
# these fails loudly instead of shipping a rootfs with a hole in it.
for f in usr/bin/gio usr/bin/gdbus usr/bin/gsettings usr/bin/gresource \
	 usr/bin/gapplication usr/bin/gi-compile-repository \
	 usr/bin/gi-decompile-typelib usr/bin/gi-inspect-typelib \
	 usr/lib/libgio-2.0.so usr/lib/libgobject-2.0.so \
	 usr/lib/libgirepository-2.0.so; do
	rm -f "$TARGET_DIR/$f"*
done

# The dnsmasq package installs a start script of its own, which runs at S80 from
# /etc/dnsmasq.conf — after S20ap has already started the one instance this board
# wants, on wlan0, with the range the phone is handed. The binary stays.
rm -f "$TARGET_DIR/etc/init.d/S80dnsmasq"

# Buildroot writes the getty line into /etc/inittab whatever the port is, and an
# empty port leaves `getty -L "" 0 vt100`: it fails to open a device that has no
# name, init respawns it five times, and the boot ends with "respawning too
# fast". The image starts no getty — dropbear is the way in — so the line goes.
sed -i '/GENERIC_SERIAL/d' "$TARGET_DIR/etc/inittab"
