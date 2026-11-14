#!/bin/sh
#
# What this build has to throw away, decided per product.
#
# Buildroot decides what to rebuild from file timestamps, and a checkout
# rewrites every one of them, so this tree decides from content instead. The
# first version of that judgement hashed everything into one fingerprint and
# emptied the whole target tree plus the radio driver's build directory when it
# changed, which is a full recompile of the driver for a change that had nothing
# to do with it.
#
# So there is one fingerprint per product, and each only throws away what it
# decides:
#
#   tree     the generated .config and the board files — the contents of
#            target/, which is the union of every package's install.
#   kernel   the kernel patches and fragments — the kernel's source tree, which
#            is also what the modules are compiled against.
#   modules  the kernel's final .config, the kernel fingerprint, and the
#            driver's own patch — the driver's objects.
#   app      the binary `make app` staged, and the recipe that installs it —
#            the one file cp2cl puts on the card.
#
# `config` runs after `_defconfig`, when .config is current but the kernel has
# not been configured yet; `modules` runs after `linux-configure`, when it has,
# because the module fingerprint is the kernel's final .config.

set -eu

BUILD=/build
EXT=/src/buildroot

digest() {
	md5sum | cut -c1-32
}

# Rewrite the fingerprint in $1 and report whether it moved. A fingerprint that
# is not there yet is written and nothing is thrown away: the tree it describes
# was built before this judgement existed, and is as good as it was.
changed() {
	if [ ! -f "$1" ]; then
		printf '%s\n' "$2" > "$1"
		return 1
	fi
	if [ "$(cat "$1")" = "$2" ]; then
		return 1
	fi
	printf '%s\n' "$2" > "$1"
}

tree_hash() {
	cat "$BUILD/.config"
	find "$EXT/board" -type f | LC_ALL=C sort | xargs -r cat
}

kernel_hash() {
	find "$EXT/patches/linux" -type f | LC_ALL=C sort | xargs -r cat
	grep '^BR2_LINUX_KERNEL' "$BUILD/.config" || true
}

module_hash() {
	cat "$BUILD"/build/linux-[0-9]*/.config 2>/dev/null || true
	cat "$BUILD/.kernel-fingerprint" 2>/dev/null || true
	find "$EXT/package/uwe5622" -type f | LC_ALL=C sort | xargs -r cat
}

app_hash() {
	sha256sum "$EXT/../dist/app/cp2cl-linux-$1" 2>/dev/null || echo missing
	cat "$EXT/package/cp2cl/cp2cl.mk"
}

# A package whose options changed in .config keeps the binary it was configured
# for, and Buildroot has no way to say which packages a change was about. The
# difference is read here instead: every BR2_PACKAGE_ symbol that appeared or
# went away names a package whose stamps go. A sub-option names its package by
# prefix, so BLUEZ5_UTILS_MONITOR is bluez5_utils, and shorter prefixes are
# tried until a build directory matches.
reconfigure() {
	sed -n 's/^[-+]//p' "$1" \
		| sed 's/^# *//' \
		| awk '{print $1}' \
		| sed 's/=.*//' \
		| grep '^BR2_PACKAGE_' \
		| sort -u \
		| while read -r symbol; do
			name=$(echo "$symbol" | sed 's/^BR2_PACKAGE_//' | tr 'A-Z' 'a-z')
			while [ -n "$name" ]; do
				dir=$(ls -d "$BUILD/build/$name"-* 2>/dev/null | head -n1)
				if [ -n "$dir" ]; then
					echo "$dir"
					break
				fi
				case "$name" in
				*_*) name=${name%_*} ;;
				*) break ;;
				esac
			done
		done \
		| sort -u \
		| while read -r dir; do
			rm -f "$dir"/.stamp_configured "$dir"/.stamp_built \
				"$dir"/.stamp_installed "$dir"/.stamp_target_installed
			echo "cache: rebuild ${dir##*/}"
		done
}

# The tree was edited in place while debugging: nothing here can tell what,
# so every product goes — the same answer `make firmware FRESH=1` always gave.
fresh() {
	rm -rf "$BUILD/target" "$BUILD"/build/linux-[0-9]* "$BUILD"/build/uwe5622-*
	rm -f "$BUILD"/build/*/.stamp_target_installed "$BUILD"/build/cp2cl-*/.stamp_rsynced
	rm -f "$BUILD"/.tree-fingerprint "$BUILD"/.kernel-fingerprint \
		"$BUILD"/.module-fingerprint "$BUILD"/.app-fingerprint
}

case "${1:-}" in
config)
	board=${2:?board name}
	arch=${3:?architecture}

	if [ "${FRESH:-0}" = 1 ]; then
		fresh
	fi

	# The target tree is the union of every package's install, so it is what
	# goes when the configuration or the board files change: a file a package
	# no longer installs stays behind otherwise.
	if changed "$BUILD/.tree-fingerprint" "$(tree_hash | digest)"; then
		rm -rf "$BUILD/target"
		rm -f "$BUILD"/build/*/.stamp_target_installed
	fi

	if [ -f "$BUILD/.config.prev" ]; then
		diff -u "$BUILD/.config.prev" "$BUILD/.config" > "$BUILD/.config.diff" || true
		reconfigure "$BUILD/.config.diff"
	fi
	cp "$BUILD/.config" "$BUILD/.config.prev"

	# The kernel's source tree is patched once, at .stamp_patched, and a tree
	# that has one never looks at the patches again — a board whose device tree
	# was patched before a patch was added keeps the tree it was built with.
	if changed "$BUILD/.kernel-fingerprint" "$(kernel_hash | digest)"; then
		rm -rf "$BUILD"/build/linux-[0-9]*
	fi

	if [ "${FRESH:-0}" = 1 ] ||
		changed "$BUILD/.app-fingerprint" "$(app_hash "$arch" | digest)"; then
		rm -f "$BUILD"/build/cp2cl-*/.stamp_rsynced \
			"$BUILD"/build/cp2cl-*/.stamp_target_installed
	fi
	;;

modules)
	# The driver is compiled against the kernel tree, and the driver's objects
	# are not a dependency of the kernel's configuration in Buildroot: a
	# module built before the kernel was configured differently is installed
	# again as it is. struct net_device then means one thing to the module and
	# another to the kernel it loads into.
	if changed "$BUILD/.module-fingerprint" "$(module_hash | digest)"; then
		for dir in "$BUILD"/build/uwe5622-*; do
			[ -d "$dir" ] || continue
			rm -f "$dir"/.stamp_configured "$dir"/.stamp_built \
				"$dir"/.stamp_installed "$dir"/.stamp_target_installed
			find "$dir" \( -name '*.o' -o -name '*.ko' -o -name '*.mod' \
				-o -name '.*.cmd' -o -name modules.order \
				-o -name Module.symvers \) -delete
		done
	fi
	;;

*)
	echo "usage: $0 config <board> <arch> | $0 modules" >&2
	exit 2
	;;
esac
