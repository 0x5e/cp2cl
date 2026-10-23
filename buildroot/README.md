# What `cp2cl` needs from the system

Values only. Everything listed is what `board/` and `configs/nanopi_duo2_defconfig`
in this tree already provide; another rootfs has to provide the same.

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
    dnsmasq                     DHCP only, --port=0
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

No `depmod`, no `kmod`: both are `insmod`-ed by path from `/etc/init.d/S10wifi`.
6.18 also ships `brcmfmac-bca.ko`, `brcmfmac-cyw.ko`, `brcmfmac-wcc.ko` beside
the core; none is loaded on this chip.

### 2.5 Sockets

    tcp  0.0.0.0:80             status page
    tcp  [::]:ephemeral         AirPlay RTSP
    udp  [::]:ephemeral         AirPlay audio, control, timing
    unix /run/dbus/system_bus_socket

`AF_INET6` is mandatory, not optional.

### 2.6 Environment

    XDG_DATA_HOME=/var/lib
    CP2CL_WIRELESS=1
    CP2CL_WIFI_IFACE=wlan0
    CP2CL_BT_ADAPTER=hci0
    CP2CL_LOG_LEVEL=info
    CP2CL_PPI=120
    CP2CL_INPUT_FILE=<unset>

## 3. Everything else

### 3.1 Mounts

    /                     ext4, rw
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

    /var/lib/cp2cl/pairings.json
    /dev/usb-ffs/carlife
    /sys/devices/platform/soc/1c19000.usb/musb-hdrc.2.auto/mode    peripheral
    /run/cp2cl.pid
    /var/log/boot.log
    /var/log/dmesg.log
    /var/log/state.log
    /var/log/wifi.log
    /var/log/ap.log
    /var/log/hostapd.log
    /var/log/cp2cl.log
    /var/log/messages         busybox syslogd, no args

`/var/log` has to be a real directory on a writable filesystem, not the
`/var/log -> /tmp` symlink Buildroot's skeleton installs.

### 3.3 Paths read

    /etc/hostapd/*.conf       interface=, ssid=, wpa_passphrase=, channel=
    /proc/stat
    /proc/meminfo
    /proc/modules
    /proc/sys/kernel/hostname
    /sys/class/net
    /sys/class/udc
    /sys/kernel/config/usb_gadget
    /sys/class/net/wlan0/address

### 3.4 Privileges

    uid 0
    CAP_SYS_ADMIN             mount(2) and umount2(2) for functionfs
    CAP_NET_BIND_SERVICE      tcp/80

### 3.5 Start order

    S00diag  S10wifi  S20ap  S30dbus-daemon  S40bluetoothd  S50cp2cl

### 3.6 Not needed

    udev, systemd, kmod, OpenSSL, wpa_supplicant, avahi/mDNS, ntp,
    DRM, framebuffer, ALSA, USB mass storage, DNS resolver

Leftovers in this image, doing nothing: `S11modules` (no `/etc/modules`),
`S40network` (no interfaces), `S50crond` (no crontabs). `S80dnsmasq` is deleted
by `post-build.sh`: it would start a second dnsmasq after the one in
`S20ap`, from `/etc/dnsmasq.conf`, which sets no `dhcp-range`.

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
    XDG_DATA_HOME                   unset with HOME=/ resolves to
                                    /.local/share/cp2cl and fails with EROFS
    dr_mode = "otg"                 leaves the port to the ID pin on PG12, which
                                    nothing drives; musb sits in OTG_STATE_UNDEFINED
                                    and never pulls up. cp2cl writes `peripheral`
                                    to the controller's `mode` to settle it
