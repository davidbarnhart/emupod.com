"""The firmware a card would boot: its osos, read from the card image's firmware partition the way the
iPod's boot ROM finds it. Config(osos="card") boots it (emu.open).

Layout (flashpod writes it; ipod_flash.py validate_firmware / walk_directory): MBR entry 0 is the
firmware partition (type 0). At its start, "{{~~" (the boot block); at +0x100 the "]ih[" volume header,
whose word at +0x104 points to the directory and half-word at +0x10a is the format. The LIVE directory
is one sector past the pointer: 40-byte entries (id, type, devOffset, len, addr, entryOffset, checksum,
vers, loadAddr). The osos payload sits at its absolute devOffset from the partition start (one sector
further in format 3) and its checksum is the byte sum.
"""
import hashlib, os, struct

SECTOR = 512


class CardFirmwareError(ValueError):
    """The card has no bootable osos; the message says why."""


def read_osos(image):
    """(osos bytes, info dict) from a card image. Raises CardFirmwareError."""
    with open(image, "rb") as f:
        mbr = f.read(SECTOR)
        if len(mbr) < SECTOR or mbr[510:512] != b"\x55\xaa":
            raise CardFirmwareError("no partition table (sector 0 has no MBR signature)")
        ptype = mbr[446 + 4]
        start, count = struct.unpack_from("<II", mbr, 446 + 8)
        if ptype != 0 or not start or not count:
            raise CardFirmwareError("partition 1 is not a firmware partition (type 0x%02x, start %d, %d sectors)"
                                    % (ptype, start, count))
        base = start * SECTOR
        f.seek(base); head = f.read(0x200)
        if head[:4] != b"{{~~" or head[0x100:0x104] != b"]ih[":
            raise CardFirmwareError("the firmware partition is empty (no boot block or volume header)")
        ptr, = struct.unpack_from("<I", head, 0x104)
        fmt, = struct.unpack_from("<H", head, 0x10a)
        f.seek(base + ptr + SECTOR); d = f.read(10 * 40)
        for off in range(0, len(d) - 39, 40):
            if d[off:off + 4] == b"\0\0\0\0": break
            typ = d[off + 4:off + 8][::-1]
            _id, dev, length, addr, _entry, cksum, vers, load = struct.unpack_from("<8I", d, off + 8)
            if typ != b"osos": continue
            f.seek(base + dev + (SECTOR if fmt == 3 else 0)); osos = f.read(length)
            if len(osos) != length:
                raise CardFirmwareError("the osos runs past the end of the image")
            if sum(osos) & 0xffffffff != cksum:
                raise CardFirmwareError("osos checksum mismatch: stored 0x%08x, got 0x%08x"
                                        % (cksum, sum(osos) & 0xffffffff))
            return osos, dict(format=fmt, dev_offset=dev, length=length, load_addr=addr, vers=vers,
                              sha256=hashlib.sha256(osos).hexdigest())
    raise CardFirmwareError("no osos in the firmware directory")


def osos_file(image, cache_dir):
    """read_osos(image) written to cache_dir/card-osos-<sha16>.bin (the core loads osos from a file).
    Returns (path, info)."""
    osos, info = read_osos(image)
    os.makedirs(cache_dir, exist_ok=True)
    path = os.path.join(cache_dir, "card-osos-%s.bin" % info["sha256"][:16])
    if not os.path.exists(path):
        tmp = path + ".%d.tmp" % os.getpid()
        with open(tmp, "wb") as f: f.write(osos)
        os.replace(tmp, path)
    return path, info
