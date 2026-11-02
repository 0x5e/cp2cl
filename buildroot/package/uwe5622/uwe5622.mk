################################################################################
#
# uwe5622
#
# The radio's driver, out of tree. Not in mainline and not close to it: the chip
# is a Spreadtrum design that Allwinner rebadged, the only source is the vendor
# BSP, and what this fetches is the community's maintenance of it, which tracks
# kernel 6.1 through 7.1 with version guards and is what Armbian ships for the
# same boards.
#
# Three modules come out of it, in the order /etc/init.d/S10radio loads them:
# uwe5622_bsp_sdio (the WCN core and the firmware download), sprdwl_ng (the
# cfg80211 Wi-Fi half) and sprdbt_tty (the Bluetooth half, a tty over the same
# SDIO function).
#
# The CONFIG_* values are the vendor tree's own, not this kernel's: they are
# passed on the make command line, which is how the tree says which board
# variant to build — CONFIG_AW_WIFI_DEVICE_UWE5622 picks the Allwinner glue and
# the uwe5622_bsp_sdio module name, and CONFIG_RK_WIFI_DEVICE_UWE5622 would pick
# the Rockchip one instead.
#
# Nothing here is installed into the rootfs beyond the modules themselves: the
# blobs the chip needs are the board's, and live under board/zero2w/firmware/.
#
################################################################################

UWE5622_VERSION = f88cd0d25ff624d853886c2db49963a32042d1ce
UWE5622_SITE = $(call github,armbian,uwe5622,$(UWE5622_VERSION))
UWE5622_LICENSE = GPL-2.0
UWE5622_DEPENDENCIES = linux

# The tree builds itself out of its own top-level Makefile, which descends into
# unisocwcn/, unisocwifi/ and tty-sdio/ — the first as obj-y, the other two as
# modules. `M=$(@D)` is the whole of it; there is no configure step.
UWE5622_MODULE_SUBDIRS = .
UWE5622_MODULE_MAKE_OPTS = \
	CONFIG_AW_WIFI_DEVICE_UWE5622=y \
	CONFIG_WLAN_UWE5622=m \
	CONFIG_TTY_OVERY_SDIO=m

$(eval $(kernel-module))
$(eval $(generic-package))
