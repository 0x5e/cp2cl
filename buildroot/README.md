# What `cp2cl` needs from the system

Values only. Everything listed is what `board/` and the three
`configs/*_defconfig` in this tree already provide; another rootfs has to provide
the same. Where the boards differ is sections 4 and 5.

## 1. Kernel

### 1.1 Command line

    root=/dev/mmcblk0p2
    rootfstype=ext4
    rw
    rootwait
    quiet

No `console=`.

### 1.2 Boot, modules, filesystems, network

    CONFIG_MODULES=y
    CONFIG_DEVTMPFS=y
    CONFIG_DEVTMPFS_MOUNT=y
    CONFIG_PROC_FS=y
    CONFIG_SYSFS=y
    CONFIG_TMPFS=y
    CONFIG_EXT4_FS=y
    CONFIG_VFAT_FS=y
    CONFIG_NLS_CODEPAGE_437=y
    CONFIG_NLS_ISO8859_1=y
    CONFIG_NLS_UTF8=y
    CONFIG_INET=y
    CONFIG_IPV6=y

### 1.3 USB gadget — CarLife

    CONFIG_CONFIGFS_FS=y
    CONFIG_USB_GADGET=y
    CONFIG_USB_LIBCOMPOSITE=y
    CONFIG_USB_CONFIGFS=y
    CONFIG_USB_CONFIGFS_F_FS=y
    CONFIG_USB_MUSB_HDRC=y
    CONFIG_USB_MUSB_DUAL_ROLE=y
    CONFIG_NOP_USB_XCEIV=y

Dual role is the mode selection's default with both `USB` and `USB_GADGET` on,
so `kernel.fragment` does not set it. It is the mode the port has to be in: the
program writes `peripheral` to the controller's `mode` file before it composes
the gadget.

The platform glue, `CONFIG_USB_MUSB_SUNXI`, *depends on* `NOP_USB_XCEIV`,
`PHY_SUN4I_USB` and `EXTCON` rather than selecting them: with any one missing it
is dropped, and the gadget goes with it, leaving one `Value requested for … not
in final .config` line in the build log.

### 1.4 Wi-Fi — CarPlay

    CONFIG_WIRELESS=y
    CONFIG_WLAN=y
    CONFIG_CFG80211=y
    CONFIG_MAC80211=y
    CONFIG_RFKILL=y
    CONFIG_MMC=y

### 1.5 Bluetooth

    CONFIG_BT=y
    CONFIG_BT_BREDR=y
    CONFIG_BT_RFCOMM=y
    CONFIG_BT_HCIUART=y
    CONFIG_BT_HCIUART_BCM=y
    CONFIG_SERIAL_DEV_BUS=y

### 1.6 Board: Allwinner H3 + AP6212

    CONFIG_MMC_SUNXI=y
    CONFIG_BRCMUTIL=m
    CONFIG_BRCMFMAC=m
    CONFIG_BRCMFMAC_SDIO=y
    CONFIG_SERIAL_8250=y
    CONFIG_SERIAL_8250_NR_UARTS=8
    CONFIG_SERIAL_8250_RUNTIME_UARTS=8
    CONFIG_USB_MUSB_SUNXI=y
    CONFIG_PHY_SUN4I_USB=y
    CONFIG_SUNXI_SRAM=y

Device tree nodes the above bind to, in `sun8i-h3-nanopi-duo2.dtb`:

    mmc1              SDIO, the Wi-Fi half of the chip
    uart2             serdev, the Bluetooth half
    usb_otg           musb, the port CarLife enumerates on
    de, mmc0          boot medium

## 2. Software and libraries

### 2.1 Processes

    /usr/bin/cp2cl              uid 0, one instance, started with setsid
    dbus-daemon --system        before bluetoothd
    bluetoothd                  BlueZ, system bus
    hostapd                     driver=nl80211, one interface
    dnsmasq                     DHCP, and DNS that answers NXDOMAIN
    dropbear                    optional, tcp/22
    busybox                     sh, init, insmod, ip, find, dmesg, start-stop-daemon

### 2.2 Libraries

    libc                        musl or glibc
    libdbus-1.so.3              BlueZ IPC
    libglib-2.0.so.0            bluetoothd only

Not linked: OpenSSL, libgio, libgobject, X11, DRM, ALSA.

### 2.3 Firmware

    /lib/firmware/brcm/brcmfmac43430-sdio.bin
    /lib/firmware/brcm/brcmfmac43430-sdio.txt
    /lib/firmware/brcm/BCM43430A1.hcd
    /lib/firmware/regulatory.db
    /lib/firmware/regulatory.db.p7s

Optional, absent here: `brcmfmac43430-sdio.clm_blob` — brcmfmac warns and continues.

### 2.4 Modules, in load order

    /lib/modules/$(uname -r)/kernel/drivers/net/wireless/broadcom/brcm80211/brcmutil/brcmutil.ko
    /lib/modules/$(uname -r)/kernel/drivers/net/wireless/broadcom/brcm80211/brcmfmac/brcmfmac.ko
    /lib/modules/$(uname -r)/kernel/crypto/{ecc,ecdh_generic}.ko
    /lib/modules/$(uname -r)/kernel/net/bluetooth/{bluetooth,rfcomm}.ko
    /lib/modules/$(uname -r)/kernel/drivers/bluetooth/{btbcm,hci_uart}.ko

No `depmod`, no `kmod`: all of them are `insmod`-ed by path from
`/etc/init.d/S10radio`, which lists them in the order they have to be loaded.
6.18 also ships `brcmfmac-bca.ko`, `brcmfmac-cyw.ko`, `brcmfmac-wcc.ko` beside
the core; none is loaded on this chip.

### 2.5 Sockets

    tcp  0.0.0.0:80             status page
    tcp  [::]:ephemeral         AirPlay RTSP
    udp  [::]:ephemeral         AirPlay audio, control, timing
    unix /run/dbus/system_bus_socket

`AF_INET6` is mandatory, not optional.

## 3. Everything else

### 3.1 Mounts

    /                     ext4 or ext2, rw
    /proc                 proc
    /sys                  sysfs
    /sys/kernel/config    configfs
    /dev                  devtmpfs
    /dev/pts              devpts
    /dev/shm              tmpfs
    /run                  tmpfs
    /tmp                  tmpfs
    /dev/usb-ffs/carlife  functionfs — created and mounted by cp2cl itself

### 3.2 Paths written

    /etc/hostname             S12firstboot, first boot only
    /etc/hosts                127.0.1.1, first boot only
    /etc/hostapd/*.conf       ssid=, first boot only
    /persist/cp2cl/*          config.json and pairings.json, linked into
                              /etc/cp2cl; real files there on a board with none
    /dev/usb-ffs/carlife
    /sys/devices/platform/soc/1c19000.usb/musb-hdrc.2.auto/mode    peripheral
    /run/cp2cl.pid
    /var/log/boot.log
    /var/log/dmesg.log
    /var/log/state.log
    /var/log/radio.log
    /var/log/ap.log
    /var/log/hostapd.log
    /var/log/cp2cl.log
    /var/log/cp2cl.err        stderr only: a panic, not the log
    /var/log/messages         busybox syslogd, no args

`/var/log` has to be a real directory on a writable filesystem, not the
`/var/log -> /tmp` symlink Buildroot's skeleton installs.

### 3.3 Paths read

    /etc/hostapd/*.conf       interface=, ssid=, wpa_passphrase=, channel=
    /run/hostapd.pid          written by S55ap once the access point is on air
    /proc/stat
    /proc/meminfo
    /proc/modules
    /proc/sys/kernel/hostname
    /sys/class/net
    /sys/class/udc
    /sys/kernel/config/usb_gadget
    /sys/class/net/wlan0/address
    /sys/class/bluetooth/hci0/address   read once the controller is there

### 3.4 Privileges

    uid 0
    CAP_SYS_ADMIN             mount(2) and umount2(2) for functionfs
    CAP_NET_BIND_SERVICE      tcp/80

### 3.5 Start order

    S00diag  S10radio  S12firstboot  S30dbus-daemon  S40bluetoothd
    S50cp2cl  S55ap

No udev: device nodes are devtmpfs's, made by the kernel as devices are probed,
and the cable's hotplug events are read straight off the kernel's own netlink
group. That is why the UFI001C's `S05firmware` reads partition names out of
sysfs — there is no `/dev/disk/by-partlabel` for them to come from.

### 3.6 Not needed

    systemd, OpenSSL, wpa_supplicant, avahi/mDNS, ntp,
    DRM, framebuffer, ALSA, USB mass storage, DNS resolver, udev

Leftovers in this image, doing nothing: `S11modules` (no `/etc/modules`),
`S40network` (no interfaces), `S50crond` (no crontabs). `S80dnsmasq` is deleted
by `post-build.sh`: it would start a second dnsmasq after the one in
`S55ap`, from `/etc/dnsmasq.conf`, which sets no `dhcp-range`.

### 3.7 Traps, one line each

    CONFIG_WIRELESS / CONFIG_WLAN   menuconfigs, off in sunxi_defconfig; when off
                                    CFG80211, MAC80211 and BRCMFMAC are dropped
                                    silently and the build still succeeds
    CONFIG_BRCMFMAC=m               as =y it probes at device_initcall, before
                                    /lib/firmware exists, and never retries
    CONFIG_IPV6=y                   without it the AirPlay bind fails with
                                    EAFNOSUPPORT and cp2cl exits at startup
    /sys, /proc, /dev/pts in fstab  without /sys, /sys/class/net is empty and
                                    wlan0 is invisible to every check
    dr_mode = "otg"                 leaves the port to the ID pin on PG12, which
                                    nothing drives; musb sits in OTG_STATE_UNDEFINED
                                    and never pulls up. cp2cl writes `peripheral`
                                    to the controller's `mode` to settle it

## 4. UFI001C — MSM8916 + WCN3620

Same program, different plumbing. What differs from the NanoPi:

    board           `msm8916-ufi001c`, named for the SoC: 001C is one board that
                    several vendors sell under their own names, and mainline's
                    device tree is named for the vendor whose sample was
                    submitted (thwc) — a name that is wrong on anyone else's
                    unit

    storage         eMMC sdhc_1 = mmcblk0; the kernel lives in the rootfs, not
                    in the boot partition (see below)
    USB             ChipIdea, role switched by pm8916_usbin extcon. There is no
                    `mode` file to write; the port is a device as soon as it is
                    plugged into a host
    Wi-Fi           WCN3620, driven by wcn36xx behind qcom_wcnss_pil, 2.4 GHz 1x1
    Bluetooth       btqcomsmd on the same chip, HCI over rpmsg
    firmware        the WCNSS image ships in the rootfs, at /lib/firmware/wcnss;
                    S05firmware links it into a tmpfs and points
                    firmware_class.path there. The WLAN NV blob comes off
                    persist, where this board's calibration lives; the modem
                    partition is not read at all
    modules         qcom_wcnss_pil and btqcomsmd through kmod's modprobe, in the
                    order modules.dep gives; busybox's insmod is not enough here
    LEDs            led-r/led-g/led-b, function power/wlan/wan
    rootfs          ext2, not ext4 — lk2nd reads the kernel out of it and has no
                    extent support — found by the ramdisk through its volume
                    label, read from the superblock before anything is mounted:
                    root=UUID= is not something the kernel understands, and the
                    label does not depend on what the rootfs has installed. The
                    label is written once, in the defconfig; post-build.sh turns
                    it into the hex constant the ramdisk compares against
    image           cp2cl-msm8916-ufi001c.img, the rootfs itself, written with
                    `edl w userdata` on a board in EDL mode. The stock GPT stays:
                    lk2nd, aboot, modem and the calibration partitions are on it
    growth          the image is 128 MB, the partition is measured in GiB; the
                    first boot after a flash grows the mounted filesystem to the
                    end of it with the red LED blinking, then reboots, and
                    Wi-Fi, Bluetooth and the program only start on that second
                    boot. The marker in /var/lib keeps later boots from
                    repeating it

The boot partition is why the kernel is in the rootfs. It is 16 MiB on the stock
table, lk2nd occupies the first 512 KiB of it, and the boot image that would have
followed is 15.6 MiB — 0.1 MiB more than what is left, with no room for the next
kernel version. lk2nd's other route needs none of it: it scans every partition
over 16 MiB, mounts it as ext2, and boots the `/extlinux/extlinux.conf` it finds:

    default buildroot

    label buildroot
        linux /boot/Image.gz
        fdt /boot/dtbs/qcom/msm8916-thwc-ufi001c.dtb
        initrd /boot/initramfs.cpio.gz
        append earlycon console=ttyMSM0,115200n8

Two consequences of that choice. The rootfs has to be ext2, because that is the
only filesystem lk2nd's reader understands, and it has to keep clear of extents:
the reader maps `i_block[]` directly and rejects any ro_compat bit beyond
sparse_super and large_file. compat and incompat bits it never looks at, so
resize_inode is allowed here and extents are not. No journal comes with ext2; a
rootfs that always mounts beats one that needs fsck after a power cut.

The image is 128 MB because that is what has to travel over EDL, while userdata is
measured in GiB. That space is not left unused: mke2fs is called with
`-E resize=16G`, which reserves the blocks needed to describe more block groups,
and S12firstboot runs `resize2fs` on the first boot, where the board's image
carries the tool, to extend the mounted filesystem to the end of the partition.
It records the outcome in `/var/lib/rootfs-resized`, and that file is what makes
this the first boot only — delete it to grow again. The restart at the end of
that script is for the name it writes in the same pass. Online growth is the
only kind available here — the root filesystem is
in use — and it is also the only kind lk2nd survives: a filesystem grown without
reserved GDT blocks is converted to meta_bg, whose descriptor layout the reader
cannot follow, and the next boot finds no kernel.

`boot.img` is still built, for `fastboot boot` only — it cannot be flashed. When
lk2nd boots nothing, its own log says why: `fastboot oem log && fastboot
get_staged log.txt`.

## 5. Orange Pi Zero 2W — H618 + UWE5622

Same program, different plumbing, and one part of it outside mainline. What
differs from the NanoPi:

    storage         microSD on mmc0 = mmcblk0, the same two-partition card
    persist         16 MiB SPI NOR on spi0, all one MTD device: `S13spiflash`
                    formats it ext4 once (detached) and mounts it at /persist
    USB             port 0 (PHY0, musb) is the socket that also powers the
                    board, and its device tree says `dr_mode = "peripheral"`:
                    the head unit enumerates us there, with no `mode` file to
                    write and no ID pin in the way. Port 1 (PHY1, EHCI/OHCI) is
                    a host — wired CarPlay goes there, a phone charges there
    Wi-Fi           UWE5622 behind SDIO on mmc1, 802.11a/b/g/n/ac 1x1. No
                    mainline driver: `package/uwe5622` builds the out-of-tree
                    one, and `patches/linux/0004` describes the bus and its
                    power sequence, which 6.18 does not
    Bluetooth       the same chip, as a tty over that SDIO function.
                    `sprdbt_tty` publishes /dev/ttyBT0 and `S10radio` puts the
                    HCI line discipline on it with `hciattach_opi -s 1500000`;
                    BlueZ's own hciattach has no `sprd` type, so the vendor
                    binary is in `board/orangepi-zero2w/firmware/`
    firmware        wcnmodem.bin and wifi_2355b001_1ant.ini are opened by path
                    rather than through request_firmware: installed in
                    /lib/firmware/uwe5622/ with a symlink beside it, one at each
                    place the driver looks
    access point    5 GHz: hw_mode=a, channel 149, 40 MHz, 802.11n. 802.11ac is
                    off on purpose
    LEDs            one, `green:status` on PC13. The red power LED sits on the
                    supply rail and no GPIO reaches it: the program finds no
                    `red:` device and logs `led.unavailable`, and the first
                    boot's blink does nothing
    image           sdcard.img — SPL at 8 KiB, a 32 MB FAT boot partition, a
                    96 MB ext4 root. The boot half is four times the NanoPi's
                    because the arm64 image is 13.5 MB gzipped where the armv7
                    one is a megabyte or two. The root is 51 MB used of 96 — it
                    was 61 while eudev was in it — and 24 MB of that is the 836
                    modules arm64's defconfig leaves on: the list at the end of
                    `board/orangepi-zero2w/kernel.fragment` is what cuts them
                    down from 1059
    libc            glibc, where the other two boards are musl: `hciattach_opi`
                    is a glibc binary and there is no other tool that speaks
                    `sprd`. cp2cl is statically linked either way

Eight things that cost an evening each:

    * The pwrseq clock has to be named `ext_clock`, which is what
      mmc-pwrseq-simple asks for; any other name is an optional clock that is
      silently skipped, and the chip comes up without its 32 kHz sleep clock.
      The pin it fans out on (PG10) has to be muxed to the clock function.
    * `CONFIG_DEBUG_INFO` stopped being a bool in 6.10 — it is a choice, and
      arm64's defconfig picks DWARF. The `is not set` form this tree used to
      carry was dropped without a word and vmlinux kept the DWARF, 166 MB
      against 53; `board/common/kernel.fragment` says `CONFIG_DEBUG_INFO_NONE`
      instead. Nothing on the card changes — Image is an objcopy of the
      loadable sections and the modules install stripped — but the build tree
      does, at every rebuild.
    * The generic defconfig leaves a thousand modules on and they do not fit
      the root this board started with, 64 MB then: mkfs stops partway through
      the tree with `Could not allocate block`, after everything else has
      built. The list at the end of `board/orangepi-zero2w/kernel.fragment` is
      what brings it back to the NanoPi's sizes.
    * U-Boot's arm64 image is packed by binman with TF-A's bl31.bin: without
      `BR2_TARGET_UBOOT_NEEDS_ATF_BL31` the build fails after the whole of
      U-Boot has compiled, with `is missing external blobs and is
      non-functional: atf-bl31`. That is also where the board's PSCI comes from
      — the H616 has no secure firmware of its own.
    * The kernel builds `certs/extract-cert` against the host's `openssl/bio.h`,
      which a container does not have: the build dies ten minutes in, on a host
      program, with the error in a parallel job's output. Buildroot's
      `BR2_LINUX_KERNEL_NEEDS_HOST_OPENSSL` is the fix.
    * The driver reads its blobs by absolute path, not through the firmware
      loader, and there is no console to say a path was wrong.
    * Channel 149 needs the regulatory database: without `wireless-regdb` the
      world domain has no CN and hostapd refuses the channel. Nothing falls back
      to 2.4 GHz on its own — that is `hw_mode=g` and `channel=11` in
      `/etc/hostapd/hostapd.conf`.
    * The board is powered from the head unit over the same 5 V rail the host
      socket's VBUS comes from. A phone that draws more than the head unit
      allows browns the board out; the first thing to check if it reboots when
      the cable goes in.
