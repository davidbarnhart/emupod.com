#!/usr/bin/env python3
"""FireWire disk mode in the server (emupod web): the page's FireWire switch puts the iPod into disk mode, and a host writes and reads it
as a block device (NBD), through the iPod's own disk-mode program. Needs Apple's firmware, which this repository does not carry, so it
runs only when given it (and skips otherwise):

    EMUPOD_IPSW15=iPod_1.1.5.ipsw python3 tests/test_fwdisk.py

Emulator-checked on synthetic cards (tools/synth_card.py: the ipsw's firmware partition and an empty FAT32), made in a temporary folder;
the server runs through tools/run_slotted.py where it exists. No root: the host side is this test's own NBD client and qemu-img (the kernel's nbd device
is not used).
  kept card: cable in -> disk mode -> the host writes a file (a FAT write mtools made on a copy, sent as the sectors it changed) and a
    changed osos -> what the host reads back, through the firmware, is what it wrote; qemu-img compares the whole export with the
    expected card -> cable out -> the card file is the expected card byte for byte, mtools finds the file, and the iPod boots on to the
    menu it showed before
  built-in card (changes discarded): the same writes are seen in disk mode, still seen after cable out and in again, and the file on disk
    never changes
"""
import hashlib, json, os, shutil, socket, struct, subprocess, sys, tempfile, time, urllib.request
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "rust", "target-own"))
SLOTTED = os.path.join(ROOT, "tools", "run_slotted.py")
WRAP = [sys.executable, SLOTTED, "--"] if os.path.exists(SLOTTED) else []


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Nbd:
    """A minimal NBD client (fixed newstyle, NBD_OPT_GO, simple replies)."""

    def __init__(self, port, name="ipod"):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=180)
        hello = self.recv(18)
        assert hello[:16] == b"NBDMAGICIHAVEOPT", hello
        self.s.sendall(struct.pack(">I", 3))
        n = name.encode()
        self.s.sendall(struct.pack(">QII", 0x49484156454F5054, 7, 4 + len(n) + 2) + struct.pack(">I", len(n)) + n + struct.pack(">H", 0))
        self.size = None
        while True:
            magic, opt, typ, ln = struct.unpack(">QIII", self.recv(20))
            data = self.recv(ln)
            if typ == 1:
                break
            if typ & 0x80000000:
                raise RuntimeError("NBD_OPT_GO refused: %#x %r" % (typ, data))
            if typ == 3 and struct.unpack(">H", data[:2])[0] == 0:
                self.size, self.flags = struct.unpack(">QH", data[2:12])
        self.handle = 0

    def recv(self, n):
        b = b""
        while len(b) < n:
            c = self.s.recv(n - len(b))
            if not c:
                raise EOFError("connection closed")
            b += c
        return b

    def request(self, typ, off, ln, payload=b""):
        self.handle += 1
        self.s.sendall(struct.pack(">IHHQQI", 0x25609513, 0, typ, self.handle, off, ln) + payload)
        magic, err, h = struct.unpack(">IIQ", self.recv(16))
        assert magic == 0x67446698 and h == self.handle, (magic, h)
        if err:
            return err, None
        return 0, self.recv(ln) if typ == 0 else b""

    def read(self, off, ln):
        err, data = self.request(0, off, ln)
        if err:
            raise IOError("NBD read at %d: errno %d" % (off, err))
        return data

    def write(self, off, data):
        err, _ = self.request(1, off, len(data), data)
        if err:
            raise IOError("NBD write at %d: errno %d" % (off, err))

    def close(self):
        try:
            self.s.sendall(struct.pack(">IHHQQI", 0x25609513, 0, 2, 0, 0, 0))
        finally:
            self.s.close()


def fat_offset(img):
    with open(img, "rb") as f:
        mbr = f.read(512)
    return struct.unpack_from("<I", mbr, 446 + 16 + 8)[0] * 512


def changed_runs(a, b):
    """The runs of 512-byte sectors where files a and b differ: [(first sector, bytes of b)]."""
    runs = []
    with open(a, "rb") as fa, open(b, "rb") as fb:
        k = 0
        while True:
            x, y = fa.read(1 << 20), fb.read(1 << 20)
            if not y:
                break
            for i in range(0, len(y), 512):
                if x[i:i + 512] != y[i:i + 512]:
                    lba = k + i // 512
                    if runs and runs[-1][0] + len(runs[-1][1]) // 512 == lba and len(runs[-1][1]) < (1 << 20):
                        runs[-1] = (runs[-1][0], runs[-1][1] + y[i:i + 512])
                    else:
                        runs.append((lba, y[i:i + 512]))
            k += len(y) // 512
    return runs


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for c in iter(lambda: f.read(1 << 20), b""):
            h.update(c)
    return h.hexdigest()


def marked_osos(card, out):
    """`card` with the last byte of its osos changed and its checksum fixed (test_aupd's marked firmware, on a whole card)."""
    shutil.copyfile(card, out)
    with open(out, "r+b") as f:
        f.seek(63 * 512)
        fw = bytearray(f.read(8 << 20))
        base = struct.unpack_from("<I", fw, 0x104)[0] + 0x200
        for off in range(base, base + 400, 40):
            if fw[off + 4:off + 8][::-1] == b"osos":
                dev, ln = struct.unpack_from("<2I", fw, off + 12)
                k = dev + ln - 1
                old = fw[k]
                fw[k] ^= 1
                ck = struct.unpack_from("<I", fw, off + 28)[0]
                struct.pack_into("<I", fw, off + 28, (ck + fw[k] - old) & 0xffffffff)
                f.seek(63 * 512)
                f.write(fw)
                return
    raise ValueError("no osos")


def holds(img, want, off, payload, menv):
    """What `img` holds of the host's writes: (the firmware partition is the written one, mtools reads the host's file back, the files on
    it that `want` does not have: the iPod's own writes since)."""
    with open(img, "rb") as a, open(want, "rb") as b:
        a.seek(63 * 512); b.seek(63 * 512)
        fw = a.read(off - 63 * 512) == b.read(off - 63 * 512)
    got = subprocess.run(["mtype", "-i", "%s@@%d" % (img, off), "::FWNOTE.TXT"], capture_output=True, env=menv).stdout
    ls = lambda p: set(subprocess.run(["mdir", "-/", "-b", "-i", "%s@@%d" % (p, off), "::"], capture_output=True, text=True, env=menv).stdout.split())
    return fw, got == payload, sorted(ls(img) - ls(want))


class Server:
    def __init__(self, exe, cards, card, tmp, builtin, extra=()):
        self.port, self.nbd_port = free_port(), free_port()
        self.log = open(os.path.join(tmp, "server-%s.log" % card.replace(" ", "_")), "w")
        args = WRAP + [exe, "web", "--host", "127.0.0.1", "--port", str(self.port),
                "--cards", cards, "--nbd", "127.0.0.1:%d" % self.nbd_port, "--demo-card", builtin, *extra]
        if card:
            args += ["--card", card]
        self.p = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT, env={**os.environ, "XDG_DATA_HOME": tmp})

    def state(self):
        with urllib.request.urlopen("http://127.0.0.1:%d/state?since=&ver=&wait=0" % self.port, timeout=30) as r:
            return json.loads(r.read())

    def cmd(self, c):
        req = urllib.request.Request("http://127.0.0.1:%d/cmd" % self.port, data=json.dumps(c).encode(), headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read())

    def wait(self, pred, what, timeout=180):
        end = time.time() + timeout
        last = None
        while time.time() < end:
            try:
                last = self.state()
                if pred(last):
                    return last
            except OSError:
                pass
            time.sleep(0.3)
        raise TimeoutError("%s: last state %s" % (what, json.dumps({k: last.get(k) for k in ("phase", "fwdisk", "lines")}) if last else None))

    def ipod(self, after):
        """The machine's counters, as /ping reports them, once they are newer than wall time `after`."""
        end = time.time() + 30
        while time.time() < end:
            with urllib.request.urlopen("http://127.0.0.1:%d/ping" % self.port, timeout=30) as r:
                v = json.loads(r.read())["ipod"]
            if v and v.get("wall", 0) > after:
                return v
            time.sleep(0.2)
        raise TimeoutError("no fresh counters")

    def cable(self, inserted):
        self.cmd({"do": "line", "name": "cna", "level": 0 if inserted else 1})

    def stop(self):
        # the slot wrapper execs the server (same pid): SIGTERM stops it at once (nothing is being burned)
        self.p.terminate()
        try:
            self.p.wait(30)
        except subprocess.TimeoutExpired:
            self.p.kill()
        self.log.close()


def main():
    ipsw = os.environ.get("EMUPOD_IPSW15")
    if not ipsw or not os.path.exists(ipsw):
        print("skipped: set EMUPOD_IPSW15 to an iPod_1.1.5.ipsw"); return 0
    bad = 0

    def check(name, ok, detail=""):
        nonlocal bad
        print(("ok   " if ok else "FAIL ") + name + (": " + detail if detail else ""), flush=True); bad += not ok

    subprocess.run(["cargo", "build", "--release", "--manifest-path", os.path.join(ROOT, "rust", "server", "Cargo.toml")], check=True,
                   capture_output=True, env={**os.environ, "CARGO_TARGET_DIR": TARGET})
    exe = os.path.join(TARGET, "release", "emupod")
    menv = {**os.environ, "MTOOLS_SKIP_CHECK": "1"}
    with tempfile.TemporaryDirectory() as tmp:
        cards = os.path.join(tmp, "cards")
        os.makedirs(cards)
        pristine = os.path.join(tmp, "pristine.img")
        subprocess.run([sys.executable, os.path.join(ROOT, "tools", "synth_card.py"), ipsw, pristine], check=True, capture_output=True)
        # what the host will write: a file (mtools on a copy: its FAT, directory and data sectors) and a changed osos
        want = os.path.join(tmp, "want.img")
        marked_osos(pristine, want)
        note = os.path.join(tmp, "note.txt")
        payload = b"written over FireWire, through the iPod's disk-mode program\n" * 700
        open(note, "wb").write(payload)
        off = fat_offset(want)
        subprocess.run(["mcopy", "-i", "%s@@%d" % (want, off), note, "::FWNOTE.TXT"], check=True, env=menv)
        runs = changed_runs(pristine, want)
        # and the whole firmware partition again, in 1 MB requests: writes long enough for page-table ORBs (256 sectors each)
        with open(want, "rb") as f:
            f.seek(63 * 512)
            fwpart = f.read(off - 63 * 512)
        runs = [(63 + k // 512, fwpart[k:k + (1 << 20)]) for k in range(0, len(fwpart), 1 << 20)] + runs
        print("the host writes %d sectors in %d runs" % (sum(len(r[1]) // 512 for r in runs), len(runs)))

        for name, kept in (("synth", True), ("Rainbow Road", False)):
            card = os.path.join(cards, name + ".img")
            if os.path.exists(card):
                # the built-in card, which the first server made (read-only) from --demo-card
                os.chmod(card, 0o644)
                os.remove(card)
            shutil.copyfile(pristine, card)
            before = sha(card)
            srv = Server(exe, cards, name, tmp, pristine)
            try:
                s0 = srv.wait(lambda s: s.get("phase") == "running" and s.get("screen"), "boot")
                check("[%s] boots, its changes %s" % (name, "kept" if kept else "discarded"), s0["card"]["keep"] == kept, json.dumps(s0["card"]))
                time.sleep(2)
                menu = srv.state()["screen"]
                srv.cable(True)
                s = srv.wait(lambda s: s["fwdisk"]["mode"] == "ready", "disk mode")
                check("[%s] cable in: disk mode, a host logged in" % name, s["fwdisk"]["blocks"] == os.path.getsize(card) // 512, json.dumps(s["fwdisk"]))
                c = Nbd(srv.nbd_port)
                check("[%s] the export is the card's size" % name, c.size == os.path.getsize(card), "%s bytes" % c.size)
                t = time.time()
                for lba, data in runs:
                    c.write(lba * 512, data)
                wt = time.time() - t
                written = sum(len(d) // 512 for _, d in runs)
                back = all(c.read(lba * 512, len(data)) == data for lba, data in runs)
                check("[%s] the host reads back, through the firmware, what it wrote" % name, back, "writes took %.1f s" % wt)
                c.close()
                i1 = srv.ipod(time.time())
                # the disk-mode machine's drive: everything the host moved went through the program's ATA commands
                check("[%s] the program wrote the host's sectors to the drive (its ATA writes)" % name, i1["ata_sectors_written"] >= written,
                      "%d sectors written by the program, %d by the host" % (i1["ata_sectors_written"], written))
                t = time.time()
                r = subprocess.run(["qemu-img", "compare", "-f", "raw", "-F", "raw", "nbd://127.0.0.1:%d/ipod" % srv.nbd_port, want],
                                   capture_output=True, text=True)
                check("[%s] qemu-img: the whole export is the expected card" % name, r.returncode == 0,
                      (r.stdout + r.stderr).strip() + " (%.1f s)" % (time.time() - t))
                i2 = srv.ipod(time.time())
                check("[%s] ... and the program read all of it from the drive (its ATA reads)" % name,
                      i2["ata_sectors_read"] - i1["ata_sectors_read"] >= os.path.getsize(card) // 512,
                      "%d sectors" % (i2["ata_sectors_read"] - i1["ata_sectors_read"]))
                srv.cable(False)
                s = srv.wait(lambda s: s["fwdisk"]["mode"] == "normal" and s.get("phase") == "running", "cable out")
                s = srv.wait(lambda s: s.get("screen") == menu, "the menu again", timeout=60)
                check("[%s] cable out: the iPod boots on to the menu it showed before" % name, s["screen"] == menu)
                if kept:
                    fw, f, own = holds(card, want, off, payload, menv)
                    check("[%s] the card file holds the host's writes: the written firmware partition, and the file mtools reads back" % name,
                          fw and f, "firmware %s, file %s; the iPod's own files since: %s" % (fw, f, own))
                else:
                    srv.cable(True)
                    srv.wait(lambda s: s["fwdisk"]["mode"] == "ready", "disk mode again")
                    got = os.path.join(tmp, "got.img")
                    r = subprocess.run(["qemu-img", "convert", "-f", "raw", "-O", "raw", "nbd://127.0.0.1:%d/ipod" % srv.nbd_port, got],
                                       capture_output=True, text=True)
                    fw, f, own = holds(got, want, off, payload, menv) if r.returncode == 0 else (False, False, r.stderr)
                    os.remove(got)
                    check("[%s] cable out and in again: the export still holds the session's writes" % name, fw and f,
                          "firmware %s, file %s; the iPod's own files since: %s" % (fw, f, own))
                    srv.cable(False)
                    srv.wait(lambda s: s["fwdisk"]["mode"] == "normal", "cable out again")
                    check("[%s] the card file never changed" % name, sha(card) == before)
            except Exception as e:
                check("[%s] ran" % name, False, repr(e))
            finally:
                srv.stop()
                if bad:
                    print(open(srv.log.name).read()[-3000:])
    print("PASS" if not bad else "FAIL: %d" % bad)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
