#!/bin/sh
#
# What a build produces for this board: the root filesystem (which carries the
# kernel), a boot image whose only route to the board is `fastboot boot`, the
# lk2nd release, and the two scripts that write them.
#
# No partition table, no card image. The board keeps the GPT it ships with --
# aboot, lk2nd, the modem and the calibration partitions are on it -- and what
# this writes is one partition, userdata, with an ext2 filesystem that lk2nd
# reads the kernel out of and the kernel then mounts as its root. pmOS nests two
# more partitions inside that one because its initramfs looks for a pmOS_boot
# partition; nothing here wants a boot partition.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)

KERNEL="$BINARIES_DIR/Image.gz"
DTB="$BINARIES_DIR/msm8916-thwc-ufi001c.dtb"
RAMDISK="$TARGET_DIR/boot/initramfs.cpio.gz"
ROOTFS="$BINARIES_DIR/rootfs.ext2"
IMAGE="$BINARIES_DIR/cp2cl-ufi001c.img"

for f in "$KERNEL" "$DTB" "$RAMDISK" "$ROOTFS"; do
	[ -f "$f" ] || { echo "post-image: $f is missing" >&2; exit 1; }
done

# 15.6 MB, and the boot partition has 15.5 MB left after lk2nd, so this cannot be
# flashed. It is here for the case where lk2nd will not read extlinux.conf: the
# same kernel and the same ramdisk in one image that USB can hand over without
# any filesystem being involved, and the ramdisk finds the same rootfs by the
# same label.
python3 "$BOARD_DIR/mkbootimg.py" \
	--kernel "$KERNEL" \
	--ramdisk "$RAMDISK" \
	--dtb "$DTB" \
	--cmdline "earlycon console=ttyMSM0,115200n8" \
	--output "$BINARIES_DIR/boot.img"

# lk2nd, fetched rather than kept in the tree. It is the bootloader the stock
# aboot starts, and a board that is being flashed already has it -- but a build
# that cannot put it back is a build that cannot recover a board, so the release
# image travels with the package. Pinned by version and hash: "whatever the
# release page had that day" is not something to flash into a bootloader.
LK2ND_VERSION=23.1
LK2ND_BASE="https://github.com/msm8916-mainline/lk2nd/releases/download/$LK2ND_VERSION"
LK2ND_SHA256=8f5f788b45b96447517ff121f20841e6bfc95a5d7c4b48fdedb855d90f593144
LK2ND="$BINARIES_DIR/lk2nd-msm8916.img"

if [ ! -f "$LK2ND" ]; then
	echo "post-image: fetching lk2nd $LK2ND_VERSION" >&2
	curl -fsSL "$LK2ND_BASE/lk2nd-msm8916.img" -o "$LK2ND.part" || {
		echo "post-image: lk2nd download failed" >&2
		rm -f "$LK2ND.part"
		exit 1
	}
	echo "$LK2ND_SHA256  $LK2ND.part" | sha256sum -c - >/dev/null || {
		echo "post-image: lk2nd hash mismatch" >&2
		rm -f "$LK2ND.part"
		exit 1
	}
	mv "$LK2ND.part" "$LK2ND"
fi

cp "$ROOTFS" "$IMAGE"

cat > "$BINARIES_DIR/flash-edl.sh" <<'EOF'
#!/bin/sh
# Writes this build to a UFI001C that is in EDL mode (9008).
#
#   ./flash-edl.sh
#
# One write, to one partition. The board keeps its partition table, lk2nd, the
# modem and the calibration data; what gets replaced is the root filesystem in
# userdata, and that filesystem is where lk2nd finds the kernel.
#
# lk2nd has to be on the boot partition already, once, and this script does not
# put it there:
#
#   edl w boot lk2nd-msm8916.img
#
# `edl` is bkerler's, the one the EDL mode of these sticks is usually driven
# with: https://github.com/bkerler/edl
set -eu
edl w userdata cp2cl-ufi001c.img
edl reset
EOF
chmod 0755 "$BINARIES_DIR/flash-edl.sh"

cat > "$BINARIES_DIR/flash.sh" <<'EOF'
#!/bin/sh
# Writes this build to a UFI001C that is running lk2nd's fastboot.
#
#   ./flash.sh           root filesystem, which carries the kernel
#   ./flash.sh --boot    load boot.img over USB for one boot, writing nothing
#   ./flash.sh --lk2nd   reflash lk2nd itself, then exit
#
# The default partition is `userdata`, which is where the rootfs has room on
# both tables this board is seen with; pass `system` for a table that has one.
# The ramdisk finds the rootfs by its volume label, so where the bytes land only
# decides where they land.
#
# There is no `fastboot flash boot boot.img`: the image is 15.6 MB and the boot
# partition has 15.5 MB left after lk2nd. `--boot` is the way to use it, and
# `boot` is where lk2nd itself goes — 428 KB, which is the first 512 KiB of the
# partition and nothing more.
#
# When lk2nd boots nothing at all, its own log is what says why:
#   fastboot oem log && fastboot get_staged log.txt
set -eu

case "${1:-}" in
--lk2nd)
	fastboot flash boot lk2nd-msm8916.img
	;;
--boot)
	fastboot boot boot.img
	;;
*)
	fastboot flash "${1:-userdata}" cp2cl-ufi001c.img
	fastboot reboot
	;;
esac
EOF
chmod 0755 "$BINARIES_DIR/flash.sh"

rm -rf "$BINARIES_DIR/ufi001c"
mkdir -p "$BINARIES_DIR/ufi001c"
cp "$IMAGE" "$BINARIES_DIR/boot.img" "$BINARIES_DIR/flash.sh" \
	"$BINARIES_DIR/flash-edl.sh" "$LK2ND" "$BINARIES_DIR/ufi001c/"

tar -czf "$BINARIES_DIR/cp2cl-ufi001c.tar.gz" -C "$BINARIES_DIR/ufi001c" \
	cp2cl-ufi001c.img boot.img flash.sh flash-edl.sh lk2nd-msm8916.img
