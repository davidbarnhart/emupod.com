#!/usr/bin/env python3
"""FireWire disk mode as an iSCSI target (emupod web --iscsi): the host's own SCSI commands go to the iPod's disk-mode program as SBP-2
ORBs. Needs Apple's firmware, which this repository does not carry, so it runs only when given it (and skips otherwise):

    EMUPOD_IPSW15=iPod_1.1.5.ipsw python3 tests/test_iscsi.py

Emulator-checked on a synthetic card (tools/synth_card.py), made in a temporary folder. No root. The data goes through a real
third-party initiator, qemu's (qemu-io and qemu-img over libiscsi: the qemu-block-extra package); the rest through this test's own
minimal initiator (a session held open, discovery, raw CDBs).
  before the cable: no login (the iPod is not in disk mode)
  cable in -> disk mode: discovery names the target; a wrong CHAP secret, no CHAP, and the wrong target are refused
  qemu-io writes a file (a FAT write mtools made on a copy, as the sectors it changed) and the whole firmware partition (1 MB writes:
    immediate data, then R2Ts) -> qemu-img compares the whole LUN with the expected card
  the iPod's own answers: INQUIRY names it (as a type 0 disk), VPD 83 and the caching page (made here), READ CAPACITY(10) and (16) give the card's size, an unknown opcode gives CHECK CONDITION
    with ILLEGAL REQUEST sense, a read past the end gives LBA OUT OF RANGE
  one host kind at a time: an NBD host is refused while an iSCSI session has the disk, and an iSCSI login while NBD has it
  START STOP UNIT (the host's eject) changes the iPod's screen
  cable out with a session open: the target closes it -> the iPod boots on to the menu -> the card file holds the host's writes
"""
import hashlib, os, random, shutil, socket, struct, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_fwdisk import ROOT, TARGET, Nbd, Server, changed_runs, fat_offset, free_port, holds, marked_osos

IQN = "iqn.2026-10.com.emupod:ipod"
# qemu's initiator retries a failing LUN for ever: a run that has not finished in this long has failed
QEMU_TIMEOUT = 300


class Refused(Exception):
    pass


class Iscsi:
    """A minimal iSCSI initiator (RFC 7143): one connection, CHAP login, SCSI commands with Data-In, R2T and Data-Out."""

    def __init__(self, port, secret=None, discovery=False, target=IQN, user="emupod", auth="CHAP,None", name="iqn.2026-10.com.emupod.test:init"):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=120)
        self.itt, self.cmdsn, self.expstatsn = 1, 1, 0
        self.isid = bytes([0x80, 0, 0x02, 0x3d, random.randrange(256), random.randrange(256)])
        keys = {"InitiatorName": name, "SessionType": "Discovery" if discovery else "Normal",
                "AuthMethod": auth}
        if not discovery:
            keys["TargetName"] = target
        r = self.login(0, 1, False, keys)
        if r.get("AuthMethod") == "CHAP":
            r = self.login(0, 1, False, {"CHAP_A": "5"})
            ident, chal = int(r["CHAP_I"]), bytes.fromhex(r["CHAP_C"][2:])
            resp = hashlib.md5(bytes([ident]) + (secret or "").encode() + chal).hexdigest()
            self.login(0, 1, True, {"CHAP_N": user, "CHAP_R": "0x" + resp})
        self.max_recv = 8192
        r = self.login(1, 3, True, {"HeaderDigest": "None", "DataDigest": "None", "MaxRecvDataSegmentLength": "65536",
                                    "ImmediateData": "Yes", "InitialR2T": "Yes", "FirstBurstLength": "65536", "MaxBurstLength": "262144",
                                    "ErrorRecoveryLevel": "0"})
        self.max_recv = int(r.get("MaxRecvDataSegmentLength", 8192))
        self.first = int(r.get("FirstBurstLength", 65536))

    def recv(self, n):
        b = b""
        while len(b) < n:
            c = self.s.recv(n - len(b))
            if not c:
                raise EOFError("connection closed")
            b += c
        return b

    def pdu(self):
        h = self.recv(48)
        n = struct.unpack(">I", h[4:8])[0] & 0xffffff
        d = self.recv((n + 3) // 4 * 4)[:n]
        if h[0] & 0x3f in (0x21, 0x23, 0x24, 0x26):
            self.expstatsn = struct.unpack(">I", h[24:28])[0] + 1
        return h, d

    def send(self, h, d=b""):
        h = bytearray(h)
        h[5:8] = len(d).to_bytes(3, "big")
        self.s.sendall(bytes(h) + d + b"\0" * (-len(d) % 4))

    def login(self, csg, nsg, transit, keys):
        h = bytearray(48)
        h[0] = 0x43
        h[1] = (0x80 if transit else 0) | csg << 2 | (nsg if transit else 0)
        h[8:14] = self.isid
        struct.pack_into(">IHHII", h, 16, self.itt, 0, 0, self.cmdsn, self.expstatsn)
        self.send(h, b"".join(("%s=%s" % kv).encode() + b"\0" for kv in keys.items()))
        r, d = self.pdu()
        if r[0] & 0x3f != 0x23:
            raise Refused("opcode %#x" % r[0])
        if r[36] or r[37]:
            raise Refused("login status %02x%02x" % (r[36], r[37]))
        return dict(k.split("=", 1) for k in d.decode().split("\0") if "=" in k)

    def cmd(self, cdb, rlen=0, wdata=None):
        """(status, data, sense)"""
        self.itt += 1
        h = bytearray(48)
        h[0] = 0x01
        h[1] = 0x80 | (0x40 if rlen else 0) | (0x20 if wdata else 0) | 1
        edtl = len(wdata) if wdata else rlen
        struct.pack_into(">IIII", h, 16, self.itt, edtl, self.cmdsn, self.expstatsn)
        h[32:32 + len(cdb)] = cdb
        self.cmdsn += 1
        imm = wdata[:self.first] if wdata else b""
        self.send(h, imm[:self.max_recv])
        data = bytearray(rlen)
        while True:
            r, d = self.pdu()
            op = r[0] & 0x3f
            if op == 0x25:
                off = struct.unpack(">I", r[40:44])[0]
                data[off:off + len(d)] = d
            elif op == 0x31:
                ttt, off, ln = struct.unpack(">I", r[20:24])[0], *struct.unpack(">II", r[40:48])
                k, sn = 0, 0
                while k < ln:
                    c = min(self.max_recv, ln - k)
                    o = bytearray(48)
                    o[0] = 0x05
                    o[1] = 0x80 if k + c == ln else 0
                    struct.pack_into(">II", o, 16, self.itt, ttt)
                    struct.pack_into(">I", o, 28, self.expstatsn)
                    struct.pack_into(">II", o, 36, sn, off + k)
                    self.send(o, wdata[off + k:off + k + c])
                    k += c
                    sn += 1
            elif op == 0x21:
                sense = d[2:2 + struct.unpack(">H", d[:2])[0]] if len(d) >= 2 else b""
                resid = struct.unpack(">I", r[44:48])[0] if r[1] & 0x02 else 0
                return r[3], bytes(data[:rlen - resid]), sense
            else:
                raise RuntimeError("opcode %#x during a command" % op)

    def send_targets(self):
        self.itt += 1
        h = bytearray(48)
        h[0] = 0x04
        h[1] = 0x80
        struct.pack_into(">IIII", h, 16, self.itt, 0xffffffff, self.cmdsn, self.expstatsn)
        self.cmdsn += 1
        self.send(h, b"SendTargets=All\0")
        r, d = self.pdu()
        return d.decode().split("\0")

    def logout(self):
        self.itt += 1
        h = bytearray(48)
        h[0] = 0x46
        h[1] = 0x80
        struct.pack_into(">IHHII", h, 16, self.itt, 0, 0, self.cmdsn, self.expstatsn)
        self.send(h)
        r, _ = self.pdu()
        self.s.close()
        return r[0] & 0x3f == 0x26 and r[2] == 0


def refused(fn):
    try:
        fn()
        return False
    except (Refused, EOFError, ConnectionError):
        return True


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
        want = os.path.join(tmp, "want.img")
        marked_osos(pristine, want)
        note = os.path.join(tmp, "note.txt")
        payload = b"written over iSCSI, through the iPod's disk-mode program\n" * 700
        open(note, "wb").write(payload)
        off = fat_offset(want)
        subprocess.run(["mcopy", "-i", "%s@@%d" % (want, off), note, "::FWNOTE.TXT"], check=True, env=menv)
        runs = changed_runs(pristine, want)
        with open(want, "rb") as f:
            f.seek(63 * 512)
            fwpart = f.read(off - 63 * 512)
        runs = [(63 + k // 512, fwpart[k:k + (1 << 20)]) for k in range(0, len(fwpart), 1 << 20)] + runs
        print("the host writes %d sectors in %d runs" % (sum(len(r[1]) // 512 for r in runs), len(runs)))

        card = os.path.join(cards, "synth.img")
        shutil.copyfile(pristine, card)
        secret_file = os.path.join(tmp, "iscsi-secret")
        srv = Server(exe, cards, "synth", tmp, pristine, extra=["--iscsi", "127.0.0.1:%d" % (port := free_port()), "--iscsi-secret", secret_file])
        url = lambda sec: "iscsi://emupod%%%s@127.0.0.1:%d/%s/0" % (sec, port, IQN)
        try:
            srv.wait(lambda s: s.get("phase") == "running" and s.get("screen"), "boot")
            secret = open(secret_file).read().strip()
            check("the CHAP secret file is made, mode 0600, not in the log", os.stat(secret_file).st_mode & 0o777 == 0o600 and len(secret) >= 12
                  and secret not in open(srv.log.name).read(), oct(os.stat(secret_file).st_mode & 0o777))
            time.sleep(2)
            menu = srv.state()["screen"]
            check("before the cable: no login", refused(lambda: Iscsi(port, secret)))
            srv.cable(True)
            s = srv.wait(lambda s: s["fwdisk"]["mode"] == "ready", "disk mode")
            blocks = os.path.getsize(card) // 512
            check("cable in: disk mode", s["fwdisk"]["blocks"] == blocks and s["fwdisk"]["iscsi"] == "127.0.0.1:%d" % port, str(s["fwdisk"]))
            d = Iscsi(port, discovery=True, auth="None")
            st = d.send_targets()
            check("discovery names the target", "TargetName=" + IQN in st and "TargetAddress=127.0.0.1:%d,1" % port in st, str(st))
            d.logout()
            check("a wrong CHAP secret is refused", refused(lambda: Iscsi(port, secret[:-1] + ("0" if secret[-1] != "0" else "1"))))
            check("no CHAP is refused", refused(lambda: Iscsi(port, auth="None")))
            check("the wrong target is refused", refused(lambda: Iscsi(port, secret, target=IQN + "x")))
            r = subprocess.run(["qemu-io", "-f", "raw", "-c", "read 0 512", url(secret[::-1])], capture_output=True, text=True, timeout=QEMU_TIMEOUT)
            check("qemu-io with a wrong secret is refused", r.returncode != 0 or "error" in (r.stdout + r.stderr).lower(), (r.stdout + r.stderr).strip()[-200:])

            # the data, through qemu's initiator: one qemu-io session, each run from a file
            cmds = []
            for i, (lba, data) in enumerate(runs):
                p = os.path.join(tmp, "run%d" % i)
                open(p, "wb").write(data)
                cmds += ["-c", "write -s %s %d %d" % (p, lba * 512, len(data))]
            t = time.time()
            r = subprocess.run(["qemu-io", "-f", "raw"] + cmds + [url(secret)], capture_output=True, text=True, timeout=QEMU_TIMEOUT)
            wt = time.time() - t
            check("qemu-io writes the runs over iSCSI", r.returncode == 0 and "error" not in (r.stdout + r.stderr).lower(),
                  "%.1f s; %s" % (wt, (r.stdout + r.stderr).strip()[-300:]))
            t = time.time()
            r = subprocess.run(["qemu-img", "compare", "-f", "raw", "-F", "raw", url(secret), want], capture_output=True, text=True, timeout=QEMU_TIMEOUT)
            check("qemu-img: the whole LUN is the expected card", r.returncode == 0, (r.stdout + r.stderr).strip() + " (%.1f s)" % (time.time() - t))

            c = Iscsi(port, secret)
            st, inq, _ = c.cmd(bytes([0x12, 0, 0, 0, 36, 0]), 36)
            check("INQUIRY: the iPod's own answer, as a type 0 disk (it says RBC, 0x0e)", st == 0 and b"Apple   iPod" in inq and inq[0] == 0, inq.hex())
            st, vpd, _ = c.cmd(bytes([0x12, 1, 0x83, 0, 255, 0]), 255)
            check("INQUIRY VPD 83: a T10 vendor ID from the iPod's own INQUIRY", st == 0 and vpd[1] == 0x83 and b"Apple   iPod" in vpd, vpd.hex())
            st, ms, _ = c.cmd(bytes([0x1a, 0, 0x08, 0, 255, 0]), 255)
            check("MODE SENSE caching page: no write cache", st == 0 and len(ms) >= 7 and ms[4] == 0x08 and ms[6] & 0x04 == 0, ms.hex())
            st, cap, _ = c.cmd(bytes([0x25] + [0] * 9), 8)
            st16, cap16, _ = c.cmd(bytes([0x9e, 0x10] + [0] * 8 + [0, 0, 0, 32, 0, 0]), 32)
            check("READ CAPACITY (10) and (16): the card's size", st == 0 and st16 == 0 and struct.unpack(">II", cap) == (blocks - 1, 512)
                  and struct.unpack(">QI", cap16[:12]) == (blocks - 1, 512), "%s / %s" % (cap.hex(), cap16[:12].hex()))
            st, _, sense = c.cmd(bytes([0xff, 0, 0, 0, 0, 0]))
            check("an unknown opcode: CHECK CONDITION, ILLEGAL REQUEST", st == 2 and len(sense) >= 14 and sense[2] & 0xf == 5, sense.hex())
            st, _, sense = c.cmd(bytes([0x28, 0, 0xff, 0xff, 0xff, 0, 0, 0, 1, 0]), 512)
            check("a read past the end: LBA OUT OF RANGE", st == 2 and len(sense) >= 14 and (sense[2] & 0xf, sense[12]) == (5, 0x21), sense.hex())
            st, data, _ = c.cmd(bytes([0x88, 0] + list((63).to_bytes(8, "big")) + [0, 0, 1, 0, 0, 0]), 256 * 512)
            check("READ(16) of 256 sectors: the firmware partition as written", st == 0 and data == fwpart[:256 * 512], "status %d, %d bytes" % (st, len(data)))
            st, _, _ = c.cmd(bytes([0x03, 0, 0, 0, 18, 0]), 18)
            check("REQUEST SENSE answers", st == 0)

            # one kind of host at a time
            check("an NBD host is refused while iSCSI has the disk", refused_nbd(srv.nbd_port))
            st, inq2, _ = c.cmd(bytes([0x12, 0, 0, 0, 36, 0]), 36)
            check("... and the iSCSI session still works", st == 0 and inq2 == inq)
            check("the page says iSCSI has the disk", srv.state()["fwdisk"].get("host") == "iscsi")
            check("a second initiator's session is refused", refused(lambda: Iscsi(port, secret, name="iqn.2026-10.com.emupod.test:other")))
            st, _, _ = c.cmd(bytes(6))
            check("... and the first session still works", st == 0)
            c2 = Iscsi(port, secret)
            gone = False
            try:
                c.cmd(bytes(6))
            except (EOFError, ConnectionError, OSError):
                gone = True
            st, _, _ = c2.cmd(bytes(6))
            check("the same initiator logging in again (a dropped connection) takes over: the old connection is closed", gone and st == 0)
            c = c2
            ok = c.logout()
            n = Nbd(srv.nbd_port)
            check("logout, then an NBD host attaches; an iSCSI login is refused while it has the disk", ok and refused(lambda: Iscsi(port, secret)))
            n.close()
            time.sleep(0.5)

            # the host's eject
            c = Iscsi(port, secret)
            before = srv.state()["screen"]
            st, _, _ = c.cmd(bytes([0x1b, 0, 0, 0, 0x02, 0]))
            time.sleep(3)
            after = srv.state()["screen"]
            check("START STOP UNIT (eject): GOOD, and the iPod's screen changes", st == 0 and after != before)
            srv.cable(False)
            s = srv.wait(lambda s: s["fwdisk"]["mode"] == "normal" and s.get("phase") == "running", "cable out")
            # until the target closes the connection, a command gets no medium (02/3A); within seconds it is closed
            closed, answers, end = False, set(), time.time() + 10
            while not closed and time.time() < end:
                try:
                    st, _, sense = c.cmd(bytes(6))
                    answers.add((st, sense[12] if len(sense) > 12 else None))
                    time.sleep(0.3)
                except (EOFError, ConnectionError, OSError):
                    closed = True
            check("cable out: the target closed the session (any command before that: no medium)", closed and answers <= {(2, 0x3a)}, str(answers))
            s = srv.wait(lambda s: s.get("screen") == menu, "the menu again", timeout=60)
            check("the iPod boots on to the menu it showed before (the firmware partition the host wrote)", s["screen"] == menu)
            fw, f, own = holds(card, want, off, payload, menv)
            check("the card file holds the host's writes: the firmware partition, and the file mtools reads back", fw and f,
                  "firmware %s, file %s; the iPod's own files since: %s" % (fw, f, own))
        except Exception as e:
            # (a qemu that timed out is killed by subprocess.run)
            check("ran", False, repr(e))
        finally:
            srv.stop()
            if bad:
                print(open(srv.log.name).read()[-3000:])
    print("PASS" if not bad else "FAIL: %d" % bad)
    return 1 if bad else 0


def refused_nbd(port):
    try:
        Nbd(port).close()
        return False
    except (RuntimeError, EOFError, ConnectionError):
        return True


if __name__ == "__main__":
    sys.exit(main())
