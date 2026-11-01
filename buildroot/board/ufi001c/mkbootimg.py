#!/usr/bin/env python3
"""Writes an Android boot image (header v0) for lk2nd.

The board does not boot from this: the kernel is a file in the root filesystem,
which lk2nd reads through extlinux.conf. This image exists for `fastboot boot`,
which hands it over USB and never touches a partition -- the boot partition is
16 MiB, lk2nd is at offset 0, and an image this size does not fit after it.

The offsets are the ones device-generic-zhihe in pmaports declares for this board
(deviceinfo_flash_offset_*) and the page size lk2nd reads a boot image with. The
one that is not copied from there is the ramdisk address: ramdisk_offset is
0x02000000 in pmaports, but this kernel is 41.6 MB once decompressed and lk2nd
runs it from 0x80080000, so a ramdisk at 0x82000000 would be inside it.
0x84000000 is 64 MiB into DRAM -- still below the 512 MiB the board has, and
22 MiB clear of where the decompressed kernel ends.

The device tree is appended to the kernel and counted in kernel_size, which is
what every working boot image for this board does (`cat Image.gz foo.dtb`).
Header v0 has no field for a separate DTB.
"""
import argparse
import struct

PAGE = 2048
BASE = 0x80000000
KERNEL_OFFSET = 0x00080000
RAMDISK_OFFSET = 0x04000000
SECOND_OFFSET = 0x00F00000
TAGS_OFFSET = 0x01E00000


def pad(data: bytes, page: int = PAGE) -> bytes:
    rem = len(data) % page
    return data if rem == 0 else data + b"\0" * (page - rem)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--kernel", required=True, help="Image.gz; lk2nd decompresses it")
    ap.add_argument("--ramdisk", required=True, help="initramfs, gzipped cpio")
    ap.add_argument("--dtb", required=True)
    ap.add_argument("--cmdline", required=True)
    ap.add_argument("--output", required=True)
    args = ap.parse_args()

    with open(args.kernel, "rb") as f:
        kernel = f.read()
    with open(args.dtb, "rb") as f:
        kernel += f.read()
    with open(args.ramdisk, "rb") as f:
        ramdisk = f.read()

    header = struct.pack(
        "<8s10I16s512s32s1024s",
        b"ANDROID!",
        len(kernel), BASE + KERNEL_OFFSET,
        len(ramdisk), BASE + RAMDISK_OFFSET,
        0, BASE + SECOND_OFFSET,           # second: none
        BASE + TAGS_OFFSET,
        PAGE,
        0,                                  # header_version
        0,                                  # os_version
        b"cp2cl".ljust(16, b"\0"),
        args.cmdline.encode().ljust(512, b"\0"),
        b"\0" * 32,
        b"\0" * 1024,
    )
    assert len(header) == 1632, len(header)

    with open(args.output, "wb") as f:
        f.write(pad(header))
        f.write(pad(kernel))
        f.write(pad(ramdisk))


if __name__ == "__main__":
    main()
