# What `cp2cl` needs from the system

Values only. Everything listed is what `board/` and the two `configs/*_defconfig`
in this tree already provide; another rootfs has to provide the same. Where the
two boards differ is section 4.

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

    /etc/hostname             S12name, first boot only
    /etc/hosts                127.0.1.1, first boot only
    /etc/hostapd/*.conf       ssid=, first boot only
    /etc/cp2cl/pairings.json
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

    S00diag  S10radio  S12name  S30dbus-daemon  S40bluetoothd  S50cp2cl  S55ap

### 3.6 Not needed

    udev, systemd, kmod, OpenSSL, wpa_supplicant, avahi/mDNS, ntp,
    DRM, framebuffer, ALSA, USB mass storage, DNS resolver

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

    storage         eMMC sdhc_1 = mmcblk0; the kernel lives in the rootfs, not
                    in the boot partition (see below)
    USB             ChipIdea, role switched by pm8916_usbin extcon. There is no
                    `mode` file to write; the port is a device as soon as it is
                    plugged into a host
    Wi-Fi           WCN3620, driven by wcn36xx behind qcom_wcnss_pil, 2.4 GHz 1x1
    Bluetooth       btqcomsmd on the same chip, HCI over rpmsg
    firmware        not in the rootfs: S05firmware links wcnss.mdt, its fragments
                    and WCNSS_qcom_wlan_nv.bin off the stock modem/persist
                    partitions and points firmware_class.path at them
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
    image           cp2cl-ufi001c.img, the rootfs itself, written with
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
and S01resizefs runs `resize2fs` on the first boot to extend the mounted
filesystem to the end of the partition, then reboots. It writes
`/var/lib/rootfs-resized` before doing so, and that file is what makes it the first
boot only — delete it to grow again. Online growth is the only kind available here — the root filesystem is
in use — and it is also the only kind lk2nd survives: a filesystem grown without
reserved GDT blocks is converted to meta_bg, whose descriptor layout the reader
cannot follow, and the next boot finds no kernel.

`boot.img` is still built, for `fastboot boot` only — it cannot be flashed. When
lk2nd boots nothing, its own log says why: `fastboot oem log && fastboot
get_staged log.txt`.
