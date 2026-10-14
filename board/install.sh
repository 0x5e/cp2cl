#!/bin/bash
# Install everything in board/. Run it on the board, as root, from anywhere.
#
# What is under board/ mirrors the filesystem it lands on, so what is here is what
# is installed: board/etc/hostapd/hostapd.conf is /etc/hostapd/hostapd.conf and
# board/etc/systemd/system/cp2cl-ap.service is
# /etc/systemd/system/cp2cl-ap.service.
#
# This is the firmware half, and the only thing in it is the access point: the
# board's radio, up at boot, which is what there is to reach the board on. The USB
# device is not the board's — the program composes it, together with the FunctionFS
# instance it runs on. Nothing is installed from here for that, and nothing that an
# arrangement which came before left behind is taken down either: this script
# installs what is in this tree and stops there.
#
# The program is not installed by anything: it is a binary that is copied over and
# run in the foreground. It needs no unit and no settings file.
set -eu

[ "$(id -u)" = 0 ] || { echo "install: run as root" >&2; exit 1; }

HERE=$(cd "$(dirname "$0")" && pwd)

# The one file the operator tunes. Everything else is ours and is replaced on
# every run; this one is copied back afterwards if it was already there, so a
# second install does not undo an edit.
KEEP="etc/hostapd/hostapd.conf"
kept=$(mktemp -d)
for f in $KEEP; do
    if [ -e "/$f" ]; then
        mkdir -p "$kept/$(dirname "$f")"
        cp -a "/$f" "$kept/$f"
    fi
done

cp -a "$HERE/etc/." /etc/

for f in $KEEP; do
    if [ -e "$kept/$f" ]; then
        cp -a "$kept/$f" "/$f"
        echo "install: kept the existing /$f"
    fi
done
rm -r "$kept"

systemctl daemon-reload
systemctl enable cp2cl-ap.service cp2cl-ap-dhcp.service

# A gadget composed by an arrangement that came before is still on the controller
# until the board is rebooted, and there is one controller. Taking it off here
# would be taking down the link the board may well be reached on.
echo "install: done. The board's half is the access point alone, at 192.168.4.1;"
echo "install: the USB link that was here is gone with it. Reboot before starting"
echo "install: cp2cl — until then a gadget from the old arrangement holds the"
echo "install: controller, and the program cannot put its own on it."
