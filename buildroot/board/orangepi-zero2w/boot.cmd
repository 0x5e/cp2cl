setenv bootargs root=/dev/mmcblk0p2 rootfstype=ext4 rw rootwait quiet
load mmc 0:1 ${kernel_addr_r} Image.gz
load mmc 0:1 ${fdt_addr_r} sun50i-h618-orangepi-zero2w.dtb
booti ${kernel_addr_r} - ${fdt_addr_r}
