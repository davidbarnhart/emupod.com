#!/usr/bin/env python3
"""A synthetic iPod card for emulator tests: a DOS MBR, the firmware partition at sector 63 holding an .ipsw's firmware (with the
install-time directory fixups Apple's updater makes, as flashpod and Rockbox's ipodpatcher write it), and an empty FAT32 data
partition after it. No music, nothing from a real card. The file is sparse.

  python3 tools/synth_card.py iPod_1.1.5.ipsw out.img [--mb 256] [--pending-aupd]

--pending-aupd leaves the aupd entry as the ipsw ships it (id 0: a flash-ROM update is pending), the state that makes the bootloader run
the card's updater.
"""
import argparse, os, struct, subprocess, sys, zipfile

SECTOR = 512
FW_START = 63
FW_BLOCKS = 65536


def firmware(path):
    with open(path, "rb") as f:
        head = f.read(2)
    if head == b"PK":
        z = zipfile.ZipFile(path)
        return z.read(next(n for n in z.namelist() if os.path.basename(n).startswith("Firmware")))
    return open(path, "rb").read()


def entries(fw, base):
    for off in range(base, base + 10 * 40, 40):
        if fw[off:off + 4] == b"\0\0\0\0":
            return
        yield off, fw[off + 4:off + 8][::-1]


def fixup(fw, pending_aupd):
    """osos/rsrc loadAddr -> 0xffffffff (nothing pending); aupd id 0 -> 1 and loadAddr -> its address (update performed), both
    directory copies (the header's and the one a sector later)."""
    fw = bytearray(fw)
    assert fw[:4] == b"{{~~" and fw[0x100:0x104] == b"]ih[", "not an iPod firmware volume"
    ptr, = struct.unpack_from("<I", fw, 0x104)
    for base in (ptr, ptr + 0x200):
        for off, typ in entries(fw, base):
            if typ == b"aupd":
                if not pending_aupd:
                    struct.pack_into("<I", fw, off + 8, 1)
                    struct.pack_into("<I", fw, off + 36, struct.unpack_from("<I", fw, off + 20)[0])
            else:
                struct.pack_into("<I", fw, off + 36, 0xffffffff)
    return bytes(fw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ipsw")
    ap.add_argument("out")
    ap.add_argument("--mb", type=int, default=256)
    ap.add_argument("--pending-aupd", action="store_true")
    a = ap.parse_args()
    fw = fixup(firmware(a.ipsw), a.pending_aupd)
    total = a.mb * 2048
    data_start = FW_START + FW_BLOCKS
    if total <= data_start + 70000:
        sys.exit("too small: a FAT32 partition needs about 35 MB after the firmware partition")
    mbr = bytearray(SECTOR)
    for off, typ, start, count in ((446, 0x00, FW_START, FW_BLOCKS), (462, 0x0b, data_start, total - data_start)):
        mbr[off + 1:off + 4] = mbr[off + 5:off + 8] = b"\xfe\xff\xff"
        mbr[off + 4] = typ
        struct.pack_into("<II", mbr, off + 8, start, count)
    mbr[510:512] = b"\x55\xaa"
    with open(a.out, "wb") as f:
        f.truncate(total * SECTOR)
        f.write(mbr)
        f.seek(FW_START * SECTOR)
        f.write(fw)
    subprocess.run(["/usr/sbin/mkfs.fat", "-F", "32", "-n", "IPOD", "--offset=%d" % data_start, a.out, str((total - data_start) // 2)],
                   check=True, stdout=subprocess.DEVNULL)
    print("%s: %d MB, firmware %d bytes at sector %d, FAT32 from sector %d%s" % (
        a.out, a.mb, len(fw), FW_START, data_start, ", aupd update pending" if a.pending_aupd else ""))


if __name__ == "__main__":
    main()
