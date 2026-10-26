#!/usr/bin/env python3
"""The iPod's own boot code, disk mode and flash-ROM update, run in the Rust core from reset. Needs Apple's firmware,
which this repository does not carry, so it runs only when given it (and skips otherwise):

    EMUPOD_IPSW15=iPod_1.1.5.ipsw [EMUPOD_AUPD14=aupd.bin of 1.4] [EMUPOD_AUPD_RW=an LBA48 read-write aupd] python3 tests/test_aupd.py

Every card is synthetic (tools/synth_card.py: the ipsw's firmware partition and an empty FAT32), made in a temporary folder; where
tools/run_slotted.py exists (this machine's emulator slots), every emulator run goes through it.
  - flash model unit tests (cargo test), and the chip assembled from the ipsw
  - chipcheck: reset -> boot block -> bootloader -> the card's osos (M1); the diskmode token -> the chip's disk-mode program, its event
    loop and its screen (M1, M2)
  - diskmode: bus connect, SBP-2 login, INQUIRY, READ CAPACITY, READ(10), WRITE(10) (M3)
  - diskmode DM_FW: a firmware partition written over FireWire, then a reset boots it (M4)
  - update (EMUPOD_AUPD14): a 1.4 chip, a pending 1.5 aupd, the updater rewrites the chip into the 1.5 one and resets (M5)
  - LBA48 (EMUPOD_AUPD_RW): on a 160 GB sparse card, the rw chip reads past 2^28 and stock 1.5 must not
"""
import os, struct, subprocess, sys, tempfile, zipfile
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "rust", "target-own"))
SLOTTED = os.path.join(ROOT, "tools", "run_slotted.py")
WRAP = [sys.executable, SLOTTED, "--"] if os.path.exists(SLOTTED) else []
BINS = ("chipcheck", "diskmode", "update")


def firmware_partition(ipsw):
    z = zipfile.ZipFile(ipsw)
    return z.read(next(n for n in z.namelist() if os.path.basename(n).startswith("Firmware")))


def image(fw, typ):
    """An image (osos, aupd) of a firmware partition, by its live directory (the header's pointer + 0x200)."""
    base = struct.unpack_from("<I", fw, 0x104)[0] + 0x200
    for off in range(base, base + 400, 40):
        if fw[off + 4:off + 8][::-1] == typ:
            dev, ln = struct.unpack_from("<2I", fw, off + 12)
            return fw[dev:dev + ln], off
    raise ValueError("no %r" % typ)


def marked(fw):
    """The partition as installed (synth_card's directory fixups), with one byte of osos changed and its checksum fixed: the firmware
    M4 writes."""
    sys.path.insert(0, os.path.join(ROOT, "tools"))
    import synth_card
    fw = bytearray(synth_card.fixup(fw, False))
    osos, off = image(bytes(fw), b"osos")
    dev, ln = struct.unpack_from("<2I", fw, off + 12)
    k = dev + ln - 1
    fw[k] ^= 1
    ck = struct.unpack_from("<I", fw, off + 28)[0]
    struct.pack_into("<I", fw, off + 28, (ck + (fw[k] - (fw[k] ^ 1))) & 0xffffffff)
    return bytes(fw)


def main():
    ipsw = os.environ.get("EMUPOD_IPSW15")
    if not ipsw or not os.path.exists(ipsw):
        print("skipped: set EMUPOD_IPSW15 to an iPod_1.1.5.ipsw"); return 0
    bad = 0

    def check(name, ok, detail=""):
        nonlocal bad
        print(("ok   " if ok else "FAIL ") + name + (": " + detail if detail else "")); bad += not ok

    def run(args, env=None, timeout=1800):
        p = subprocess.run(WRAP + args, capture_output=True, text=True,
                           timeout=timeout, env={**os.environ, **(env or {})})
        return p.returncode, p.stdout

    cargo = ["cargo", "test", "--release", "--manifest-path", os.path.join(ROOT, "rust", "pp5002", "Cargo.toml"), "--lib"]
    p = subprocess.run(cargo, capture_output=True, text=True, env={**os.environ, "CARGO_TARGET_DIR": TARGET})
    check("the Rust core's unit tests (flash, engine, LCD)", p.returncode == 0, p.stdout.strip().splitlines()[-1] if p.stdout.strip() else p.stderr[-300:])
    subprocess.run(["cargo", "build", "--release", "--manifest-path", os.path.join(ROOT, "rust", "pp5002", "Cargo.toml")]
                   + sum((["--bin", b] for b in BINS), []), check=True, capture_output=True, env={**os.environ, "CARGO_TARGET_DIR": TARGET})
    exe = {b: os.path.join(TARGET, "release", b) for b in BINS}
    fw = firmware_partition(ipsw)
    with tempfile.TemporaryDirectory() as tmp:
        aupd15 = os.path.join(tmp, "aupd15.bin")
        open(aupd15, "wb").write(image(fw, b"aupd")[0])
        card = os.path.join(tmp, "card.img")
        synth = [sys.executable, os.path.join(ROOT, "tools", "synth_card.py"), ipsw]
        subprocess.run(synth + [card], check=True, capture_output=True)

        rc, out = run([exe["chipcheck"], aupd15, card])
        print(out.rstrip())
        check("chipcheck (M1, M2)", rc == 0 and out.rstrip().endswith("PASS"))
        rc, out = run([exe["diskmode"], aupd15, card])
        print(out.rstrip())
        check("diskmode (M3: login, INQUIRY, READ CAPACITY, READ(10), WRITE(10))", rc == 0 and out.rstrip().endswith("PASS"))
        fwm = os.path.join(tmp, "fw-marked.bin")
        open(fwm, "wb").write(marked(fw))
        rc, out = run([exe["diskmode"], aupd15, card], {"DM_FW": fwm})
        print(out.rstrip())
        check("diskmode DM_FW (M4: a firmware written over FireWire boots after a reset)", rc == 0)

        old = os.environ.get("EMUPOD_AUPD14")
        if old and os.path.exists(old):
            pending = os.path.join(tmp, "pending.img")
            subprocess.run(synth + [pending, "--pending-aupd"], check=True, capture_output=True)
            rc, out = run([exe["update"], old, pending])
            print(out.rstrip())
            check("update (M5: the flash-ROM update from reset)", rc == 0 and out.rstrip().endswith("PASS"))
        else:
            print("skipped M5: set EMUPOD_AUPD14 to the aupd.bin of 1.4")

        rw = os.environ.get("EMUPOD_AUPD_RW")
        if rw and os.path.exists(rw):
            big = os.path.join(tmp, "big.img")
            subprocess.run(synth + [big, "--mb", "163840"], check=True, capture_output=True)
            far = (1 << 28) + 12345
            with open(big, "r+b") as f:
                f.seek(far * 512)
                f.write(bytes((k * 13 + 5) & 0xff for k in range(512)))
            rc, out = run([exe["diskmode"], rw, big], {"DM_FAR": str(far)})
            check("LBA48: the rw chip reads past 2^28", rc == 0, [l for l in out.splitlines() if "LBA %d" % far in l][0].strip())
            rc, out = run([exe["diskmode"], aupd15, big], {"DM_FAR": str(far)})
            refused = any(l.startswith(" FAIL  READ(10) at LBA %d" % far) for l in out.splitlines())
            check("LBA48: stock 1.5 does not read past 2^28", refused, "READ CAPACITY " + [l for l in out.splitlines() if "READ CAPACITY ==" in l][0].split("got ")[1])
        else:
            print("skipped LBA48: set EMUPOD_AUPD_RW to an LBA48 read-write aupd")
    print("PASS" if not bad else "FAIL: %d" % bad)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
