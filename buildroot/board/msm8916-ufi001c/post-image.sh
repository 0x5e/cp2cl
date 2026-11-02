#!/bin/sh
#
# What a build produces for this board: the root filesystem, which is also where
# the kernel and the ramdisk are.
#
# No partition table, no card image. The board keeps the GPT it ships with --
# aboot, lk2nd, the modem and the calibration partitions are on it -- and what
# this writes is one partition, userdata, with an ext2 filesystem that lk2nd
# reads the kernel out of and the kernel then mounts as its root. pmOS nests two
# more partitions inside that one because its initramfs looks for a pmOS_boot
# partition; nothing here wants a boot partition.
#
# Nothing else is produced: this image is the deliverable, and `dist/` carries
# it compressed.
set -eu

# The image is named for the board, after the directory this script sits in:
# unlike the card boards there is one product, and it is flashed on its own.
BOARD=$(basename "$(cd "$(dirname "$0")" && pwd)")

KERNEL="$BINARIES_DIR/Image.gz"
DTB="$BINARIES_DIR/msm8916-thwc-ufi001c.dtb"
RAMDISK="$TARGET_DIR/boot/initramfs.cpio.gz"
ROOTFS="$BINARIES_DIR/rootfs.ext2"
IMAGE="$BINARIES_DIR/cp2cl-$BOARD.img"

for f in "$KERNEL" "$DTB" "$RAMDISK" "$ROOTFS"; do
	[ -f "$f" ] || { echo "post-image: $f is missing" >&2; exit 1; }
done

cp "$ROOTFS" "$IMAGE"
