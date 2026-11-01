#!/bin/sh
#
# Runs after the overlays are laid down and before the image is made.
#
# Two things are added to the tree here. One is a directory for the WCNSS blobs
# to land in: the blobs themselves come off the stock firmware partitions at
# boot (S05firmware), the same way postmarketOS gets them. The other is the
# kernel, which this board keeps inside the root filesystem rather than in the
# boot partition -- the comment below says why.
set -eu

BOARD_DIR=$(cd "$(dirname "$0")" && pwd)

. "$BOARD_DIR/../common/post-build.sh"

# msm-firmware-loader mounts a tmpfs here and links the blobs into it. The mount
# needs the directory to exist; the script does not make it.
install -d -m 0755 "$TARGET_DIR/lib/firmware/msm-firmware-loader"

# btmgmt is built but never installed. bluez lists it under noinst_PROGRAMS in
# the block that READLINE guards, so it is compiled -- and dropped -- whether or
# not --enable-tools is passed; BR2_PACKAGE_BLUEZ5_UTILS_TOOLS installs bluemoon,
# l2ping and friends instead. It is copied in because it is the only client that
# writes MGMT_OP_SET_SECURE_CONN, and that setting is how a pairing failure is
# attributed: with SC off the controller runs the legacy DHKey exchange, so a
# pairing that then works points at the P-256 path rather than at the agent.
btmgmt_installed=
for btmgmt in "$BUILD_DIR"/bluez5_utils-*/tools/btmgmt; do
	if [ -x "$btmgmt" ]; then
		install -m 0755 "$btmgmt" "$TARGET_DIR/usr/bin/btmgmt"
		btmgmt_installed=1
		break
	fi
done
if [ -z "$btmgmt_installed" ]; then
	echo "post-build: btmgmt was not built" >&2
	exit 1
fi

# The kernel goes into this filesystem, not into the boot partition. lk2nd sits
# at offset 0 there and 16 MiB - 512 KiB = 15.5 MiB is all that is left of it,
# which is 0.1 MiB short of the Image.gz + dtb + ramdisk this board needs. What
# lk2nd does have room for is a filesystem: any partition over 16 MiB, formatted
# ext2, with /extlinux/extlinux.conf in it, so the kernel travels with the rootfs.
#
# Here and not in post-image.sh, because this is the hook that runs before mkfs:
# what sits in TARGET_DIR at this point is what the ext2 image contains.
install -d -m 0755 "$TARGET_DIR/boot/dtbs/qcom" "$TARGET_DIR/extlinux"
install -m 0644 "$BINARIES_DIR/Image.gz" "$TARGET_DIR/boot/Image.gz"
install -m 0644 "$BINARIES_DIR/msm8916-thwc-ufi001c.dtb" \
	"$TARGET_DIR/boot/dtbs/qcom/msm8916-thwc-ufi001c.dtb"

# The ramdisk the kernel is handed. It does not find the kernel -- lk2nd does
# that -- it finds the root filesystem afterwards, which no root= can name: the
# partition is userdata on one table and system on another, and this build does
# not get to choose. It goes by the ext2 volume label instead. Three files: an
# init, busybox, musl.
#
# The label is one string that has to appear twice: in the superblock, where mkfs
# writes it, and in the ramdisk's init, as a hex constant to compare against.
# Buildroot exports BR2_CONFIG and a handful of directories to this script, but
# not the configuration itself, so the value is read back out of the file
# BR2_CONFIG names and substituted into the init rather than written down twice.
root_label=$(sed -n 's/^BR2_TARGET_ROOTFS_EXT2_LABEL="\(.*\)"$/\1/p' "$BR2_CONFIG")
if [ -z "$root_label" ]; then
	echo "post-build: BR2_TARGET_ROOTFS_EXT2_LABEL is not set" >&2
	exit 1
fi
root_label_hex=$(python3 - "$root_label" <<'PY'
import sys
label = sys.argv[1].encode()
if not 1 <= len(label) <= 16:
    sys.exit("post-build: label must be 1..16 bytes, got %d" % len(label))
print(label.ljust(16, b"\0").hex())
PY
)
init_file="$BUILD_DIR/ufi001c-init"
sed "s/@ROOT_LABEL_HEX@/$root_label_hex/" "$BOARD_DIR/initramfs/init" > "$init_file"
if grep -q '@ROOT_LABEL_HEX@' "$init_file"; then
	echo "post-build: the label was not substituted into init" >&2
	exit 1
fi

# The kernel executes /init, so it has to carry an executable mode into the
# ramdisk. Taken from the copy in BUILD_DIR rather than from the checkout: a
# build that chmods its own sources leaves the tree dirty and the next build
# depends on what the last one did.
chmod 0755 "$init_file"
python3 "$BOARD_DIR/mkcpio.py" --output "$TARGET_DIR/boot/initramfs.cpio.gz" \
	--file "/init=$init_file" \
	--file "/bin/busybox=$TARGET_DIR/bin/busybox" \
	--file "/lib/libc.so=$TARGET_DIR/lib/libc.so" \
	--symlink "/lib/ld-musl-aarch64.so.1=libc.so"

# What lk2nd parses. It understands label, default, kernel/linux, initrd, fdt,
# fdtdir, append and fdtoverlays, ignores everything else, and always boots the
# `default` label. The devicetree is named explicitly rather than by fdtdir
# because it is one board, and the path here is the one this tree installs.
cat > "$TARGET_DIR/extlinux/extlinux.conf" <<'EOF'
default buildroot

label buildroot
	linux /boot/Image.gz
	fdt /boot/dtbs/qcom/msm8916-thwc-ufi001c.dtb
	initrd /boot/initramfs.cpio.gz
	append earlycon console=ttyMSM0,115200n8
EOF

chmod 0644 "$TARGET_DIR/extlinux/extlinux.conf"
