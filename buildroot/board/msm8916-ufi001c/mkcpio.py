#!/usr/bin/env python3
"""Packs the initramfs lk2nd hands the kernel.

It ends up in two places: /boot/initramfs.cpio.gz in the rootfs, which is what
extlinux.conf names, and inside boot.img, for `fastboot boot`. Both are the same
bytes.

The ramdisk holds exactly three things: /init, a busybox, and the musl loader
busybox is linked against. Nothing on the build host is assumed -- no cpio, no
gen_init_cpio -- because the Buildroot container has neither and installing one
just to concatenate four files is not worth a package.

  mkcpio.py --output initramfs.cpio.gz \\
      --file /init=board/msm8916-ufi001c/initramfs/init \\
      --file /bin/busybox=target/bin/busybox \\
      --file /lib/libc.so=target/lib/libc.so \\
      --symlink /lib/ld-musl-aarch64.so.1=libc.so
"""
import argparse
import gzip
import os
import stat

# newc magic, then 13 ASCII-hex fields of 8 characters each, then the name.
HEADER = "070701"
FIELDS = ("ino", "mode", "uid", "gid", "nlink", "mtime", "filesize",
          "devmajor", "devminor", "rdevmajor", "rdevminor", "namesize", "check")


class Entry:
    def __init__(self, name, mode, data=b"", link=None):
        self.name = name
        self.mode = mode
        self.data = data
        self.link = link


def add_parents(entries, path, seen):
    parts = path.strip("/").split("/")[:-1]
    for i in range(len(parts)):
        d = "/" + "/".join(parts[: i + 1])
        if d not in seen:
            seen.add(d)
            entries.append(Entry(d, stat.S_IFDIR | 0o755))


def render(entries):
    # nlink of a directory counts its subdirectories; cpio readers show it, the
    # kernel does not care, but a wrong count makes `ls` inside the ramdisk lie.
    subdirs = {}
    for e in entries:
        if not stat.S_ISDIR(e.mode) or e.name == "/":
            continue
        parent = os.path.dirname(e.name) or "/"
        subdirs[parent] = subdirs.get(parent, 0) + 1

    out = bytearray()
    ino = 1
    for e in entries:
        name = b"." if e.name == "/" else e.name.lstrip("/").encode()
        if stat.S_ISDIR(e.mode):
            nlink = 2 + subdirs.get(e.name, 0)
            payload = b""
        elif e.link is not None:
            nlink = 1
            payload = e.link.encode()
        else:
            nlink = 1
            payload = e.data
        if e.link is not None:
            mode = stat.S_IFLNK | 0o777
        else:
            mode = e.mode
        values = dict(
            ino=ino, mode=mode, uid=0, gid=0, nlink=nlink,
            mtime=0, filesize=len(payload),
            devmajor=0, devminor=0, rdevmajor=0, rdevminor=0,
            namesize=len(name) + 1, check=0,
        )
        out += (HEADER + "".join("%08x" % values[f] for f in FIELDS)).encode()
        out += name + b"\0"
        out += b"\0" * (-len(out) % 4)
        out += payload
        out += b"\0" * (-len(out) % 4)
        ino += 1

    trailer = b"TRAILER!!!\0"
    out += (HEADER + "".join("%08x" % v for v in (0, 0, 0, 0, 1, 0,
                                                 0, 0, 0, 0, 0,
                                                 len(trailer), 0))).encode()
    out += trailer
    out += b"\0" * (-len(out) % 512)
    return bytes(out)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--output", required=True)
    ap.add_argument("--file", action="append", default=[], metavar="DEST=SRC")
    ap.add_argument("--symlink", action="append", default=[], metavar="DEST=TARGET")
    ap.add_argument("--no-gzip", action="store_true")
    args = ap.parse_args()

    entries = []
    seen = set()
    for spec in args.file:
        dest, src = spec.split("=", 1)
        add_parents(entries, dest, seen)
        st = os.stat(src)
        with open(src, "rb") as f:
            data = f.read()
        entries.append(Entry(dest, stat.S_IFREG | stat.S_IMODE(st.st_mode), data))
    for spec in args.symlink:
        dest, target = spec.split("=", 1)
        add_parents(entries, dest, seen)
        entries.append(Entry(dest, stat.S_IFLNK | 0o777, link=target))

    entries.sort(key=lambda e: e.name)
    blob = render(entries)

    with open(args.output, "wb") as f:
        f.write(blob if args.no_gzip else gzip.compress(blob, 9, mtime=0))


if __name__ == "__main__":
    main()
