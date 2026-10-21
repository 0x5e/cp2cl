# What U-Boot runs: the kernel and the device tree off the FAT partition, and a
# command line that names the squashfs partition and nothing else. There is no
# console= on it on purpose — every line the kernel prints to a serial port is
# time the boot spends talking to a port nobody is watching.
setenv bootargs root=/dev/mmcblk0p2 rootfstype=squashfs ro rootwait quiet
load mmc 0:1 ${kernel_addr_r} zImage
load mmc 0:1 ${fdt_addr_r} sun8i-h3-nanopi-duo2.dtb
bootz ${kernel_addr_r} - ${fdt_addr_r}
