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

# Not from a package: brcmfmac-sdio-firmware carries every chip's blob and the
# generic NVRAM that goes with them, and the NVRAM is the part that has to match
# the board — the antenna path, the clock, the MAC. This board's comes from the
# vendor. Three files, half a megabyte.
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

# Same for the log partition. An overlay cannot carry it: it is an empty
# directory, and git does not keep those.
install -d -m 0755 "$TARGET_DIR/log"

# The log partition's one file at flash time — what to look at once the card is
# in a reader. It is also what keeps genimage from copying the whole images
# directory into a 512 KB filesystem, which is what an empty file list means.
BINARIES_DIR=${2:-${BINARIES_DIR:-.}}
mkdir -p "$BINARIES_DIR"
cat > "$BINARIES_DIR/log-readme.txt" <<'EOF'
cp2cl — log partition

Nothing here needs keeping. This is what the last boot had to say about itself,
written by /etc/init.d/S00diag on the board:

  boot.log      one block per boot: uptime, kernel command line, interfaces
  dmesg.log     the kernel ring buffer, from the top
  state.log     a sample every five seconds: interfaces, sdio devices, wlan0
  wifi.log      what S10wifi did — modules loaded, how long wlan0 took
  ap.log        hostapd and dnsmasq
  hostapd.log   hostapd's own log
  cp2cl.log     the program

If this file is all that is here, the kernel never reached userspace: the card
boots no further than U-Boot, and nothing above can help.
EOF

# The skeleton gives /var/run as a directory of its own, which on a read-only root
# is a directory nobody can write to. Point it at the tmpfs /etc/fstab mounts on
# /run, where dropbear and anything else that drops a pid file expects to be able
# to.
rm -rf "$TARGET_DIR/var/run"
ln -s ../run "$TARGET_DIR/var/run"

# The overlays are rsynced with a --chmod that leaves every file 0644, and
# dropbear refuses a host key that anyone but its owner can read. The key itself
# is in the overlay because it has to be in the image: the root is read-only, so
# there is nowhere to generate one at boot.
chmod 600 "$TARGET_DIR/etc/dropbear/dropbear_ed25519_host_key"

# bluez5-utils selects libglib2 unconditionally, and glib2 installs a set of
# command line tools and a GIO that nothing on this board links. What bluetoothd
# and bluetoothctl actually need is libglib-2.0 and libdbus; the rest is 3.3 MB
# of the rootfs. Listed by name rather than globbed by directory so that a
# package which later does need one of these fails loudly instead of quietly
# shipping a rootfs with a hole in it.
for f in usr/bin/gio usr/bin/gdbus usr/bin/gsettings usr/bin/gresource \
	 usr/bin/gapplication usr/bin/gi-compile-repository \
	 usr/bin/gi-decompile-typelib usr/bin/gi-inspect-typelib \
	 usr/lib/libgio-2.0.so usr/lib/libgobject-2.0.so \
	 usr/lib/libgirepository-2.0.so; do
	rm -f "$TARGET_DIR/$f"*
done

# Buildroot writes the getty line into /etc/inittab whatever the port is, and an
# empty port leaves `getty -L "" 0 vt100`: it fails to open a device that has no
# name, init respawns it five times, and the boot ends with "respawning too
# fast" on a console nobody asked for. The image deliberately starts no getty —
# dropbear over the access point is the way in — so the line goes.
sed -i '/GENERIC_SERIAL/d' "$TARGET_DIR/etc/inittab"
