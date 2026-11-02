# What U-Boot runs: the kernel and the device tree off the FAT partition.
#
# No console= on the command line: every line the kernel prints to a serial port
# is boot time spent talking to a port nobody is watching.
#
# rw rather than ro: the root is a filesystem to be changed on the board.
# errors=remount-ro is the belt to the journal's braces — a card pulled mid-write
# comes back read-only instead of corrupt.
#
# booti, not bootz, and a gzipped Image: the kernel is arm64, which has no
# zImage. U-Boot decompresses the payload itself, so the boot partition holds
# 11 MB instead of the 28 MB the plain Image takes.
setenv bootargs root=/dev/mmcblk0p2 rootfstype=ext4 rw rootwait quiet
load mmc 0:1 ${kernel_addr_r} Image.gz
load mmc 0:1 ${fdt_addr_r} sun50i-h618-orangepi-zero2w.dtb
booti ${kernel_addr_r} - ${fdt_addr_r}
