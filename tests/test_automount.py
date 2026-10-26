#!/usr/bin/env python3
"""Automatic mounting in the server (emupod web --automount SOCKET): when the iPod enters disk mode the server asks the mount service to
mount its disk, and when the FireWire cable is pulled it asks for the unmount first and lets the cable out only once that worked. Needs
Apple's firmware, which this repository does not carry, so it runs only when given it (and skips otherwise):

    EMUPOD_IPSW15=iPod_1.1.5.ipsw python3 tests/test_automount.py

No root: the mount service is a stand-in on a Unix socket (the real one, `emupod mount-service`, needs root to attach and mount). It
speaks the service's requests and does what the kernel would do with the disk: on attach it connects to the export and reads the
partition table through the iPod; on detach it writes and flushes through that connection (an unmount's last writeback), which works only
while the iPod is still in disk mode. A synthetic card (tools/synth_card.py), in a temporary folder.
  no service yet: the page is told how to set it up, and the cable works as before
  the service appears: cable in -> mounted (the stand-in read the iPod's partition table) -> the disk is busy: the cable stays in and
    the page names who holds it -> no longer busy: unmounted (its last writes reached the iPod), then the cable is out and the iPod
    restarts -> cable in again -> mounted -> the user unmounts it (the desktop's Eject): the iPod is ejected (its screen changes to
    OK to disconnect) and the cable comes out without another unmount -> cable in -> mounted -> the server is stopped: it unmounts first
  a service for another address: the page says how to fix it, and nothing is mounted
"""
import json, os, socket, struct, subprocess, sys, tempfile, threading, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_fwdisk import Nbd, Server, ROOT, TARGET


class FakeService:
    """The mount service's requests, one JSON line each way, on a Unix socket."""

    def __init__(self, path, nbd_port, nbd_addr=None):
        self.path, self.nbd_port = path, nbd_port
        self.addr = nbd_addr or "127.0.0.1:%d" % nbd_port
        self.log, self.busy, self.conn = [], False, None
        self.unmounted = False  # the user unmounts it (the desktop's Eject): the next hello finds it gone and lets the device go
        self.l = socket.socket(socket.AF_UNIX)
        self.l.bind(path)
        self.l.listen(4)
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                c, _ = self.l.accept()
            except OSError:
                return
            with c:
                line = c.makefile().readline()
                c.sendall((json.dumps(self.answer(json.loads(line))) + "\n").encode())

    def answer(self, r):
        d = r.get("do")
        if d == "hello":
            if self.unmounted and self.conn:
                self.conn.close()
                self.conn = None
                self.log.append(("unmounted by the user",))
            return {"ok": True, "proto": 1, "version": "stand-in", "nbd": self.addr, "at": "/media/emupod",
                    "attached": "/dev/nbd9p2" if self.conn else None}
        if d == "attach":
            self.conn = Nbd(self.nbd_port)
            mbr = self.conn.read(0, 512)
            part = next((i + 1 for i in range(4) if mbr[446 + 16 * i + 4] in (0x0b, 0x0c)), None)
            self.log.append(("attach", mbr[510:512] == b"\x55\xaa" and part == 2))
            return {"ok": True, "dev": "/dev/nbd9p%d" % part, "at": "/media/emupod"}
        if d == "detach":
            if self.busy and not r.get("force"):
                self.log.append(("busy",))
                return {"ok": False, "error": "busy", "holders": ["bash (4242)"]}
            ok = None
            if self.conn and not r.get("force"):
                # the unmount's last writeback: the partition table, as it is, then a flush
                try:
                    mbr = self.conn.read(0, 512)
                    ok = self.conn.request(1, 0, 512, mbr)[0] == 0 and self.conn.request(3, 0, 0)[0] == 0
                except (OSError, EOFError):
                    ok = False
            if self.conn:
                self.conn.close()
                self.conn = None
            self.log.append(("detach", bool(r.get("force")), ok))
            return {"ok": True}
        return {"ok": False, "error": "unknown request"}

    def close(self):
        self.l.close()


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
    with tempfile.TemporaryDirectory() as tmp:
        cards = os.path.join(tmp, "cards")
        os.makedirs(cards)
        card = os.path.join(cards, "synth.img")
        subprocess.run([sys.executable, os.path.join(ROOT, "tools", "synth_card.py"), ipsw, card], check=True, capture_output=True)
        builtin = os.path.join(tmp, "builtin.img")
        subprocess.run(["cp", card, builtin], check=True)
        mount = lambda s: (s.get("fwdisk") or {}).get("mount") or {}
        cna = lambda s: (s.get("lines") or {}).get("cna")
        mode = lambda s: (s.get("fwdisk") or {}).get("mode")

        # a Unix socket's path has room for about 100 bytes: a long TMPDIR gets one in /tmp
        sockdir = tmp if len(tmp) < 80 else tempfile.mkdtemp(prefix="emupod-", dir="/tmp")
        sock = os.path.join(sockdir, "mount.sock")
        srv = Server(exe, cards, "synth", tmp, builtin, ["--automount", sock])
        svc = None
        try:
            s = srv.wait(lambda s: s.get("phase") == "running" and s.get("screen"), "boot")
            m = mount(s)
            check("no service: the page is told how to set it up", m.get("state") == "off" and "setup-automount" in (m.get("setup") or ""), json.dumps(m))
            svc = FakeService(sock, srv.nbd_port)
            srv.wait(lambda s: mount(s).get("state") == "idle", "the service is seen", 10)
            check("the service appears: seen", True)
            time.sleep(1)

            srv.cable(True)
            s = srv.wait(lambda s: mount(s).get("state") == "mounted", "mounted")
            check("cable in: the disk is mounted", mode(s) == "ready" and mount(s).get("at") == "/media/emupod" and mount(s).get("dev") == "/dev/nbd9p2", json.dumps(s["fwdisk"]))
            check("... the service read the iPod's partition table through the iPod", svc.log == [("attach", True)], repr(svc.log))

            svc.busy = True
            r = srv.cmd({"do": "line", "name": "cna", "level": 1})
            check("cable out while mounted: held for the unmount", r.get("held") is not None, json.dumps(r))
            s = srv.wait(lambda s: mount(s).get("state") == "busy", "busy", 30)
            time.sleep(2)
            s = srv.state()
            check("the disk is busy: the cable stays in and the page names who holds it",
                  cna(s) == 0 and mode(s) == "ready" and "bash (4242)" in (mount(s).get("error") or ""), json.dumps(s["fwdisk"]))

            svc.busy = False
            srv.cable(False)
            s = srv.wait(lambda s: mode(s) == "normal" and s.get("screen"), "back from disk mode", 120)
            check("no longer busy: unmounted, its last writes reached the iPod, then the cable came out",
                  svc.log[-1] == ("detach", False, True) and cna(s) == 1, repr(svc.log[-2:]))
            srv.wait(lambda s: mount(s).get("state") == "idle", "idle again", 10)

            srv.cable(True)
            srv.wait(lambda s: mount(s).get("state") == "mounted", "mounted again")
            check("cable in again: mounted again", svc.log[-1] == ("attach", True), repr(svc.log[-1:]))

            # the desktop's Eject: the user unmounts it; the iPod is sent the eject (SCSI START STOP UNIT) and shows OK to disconnect
            before = srv.state()["screen"]
            svc.unmounted = True
            s = srv.wait(lambda s: mode(s) == "ejected", "ejected", 30)
            time.sleep(2)
            s = srv.state()
            check("the user unmounts it (the desktop's Eject): the iPod is ejected, its screen changes, the cable is still in",
                  mount(s).get("state") == "ejected" and s["screen"] != before and cna(s) == 0 and ("unmounted by the user",) in svc.log,
                  json.dumps(s["fwdisk"]))
            r = srv.cmd({"do": "line", "name": "cna", "level": 1})
            srv.wait(lambda s: mode(s) == "normal" and s.get("screen"), "back from disk mode", 120)
            check("... then the cable comes out without another unmount", svc.log[-1] == ("unmounted by the user",), repr(svc.log[-2:]))
            svc.unmounted = False

            srv.cable(True)
            srv.wait(lambda s: mount(s).get("state") == "mounted", "mounted once more")
            n = len(svc.log)
            srv.stop()
            check("the server is stopped: it unmounts first (while the iPod takes the last writes)", svc.log[n:] == [("detach", False, True)], repr(svc.log[n:]))
        except Exception as e:
            check("ran", False, repr(e))
        finally:
            srv.stop()
            if svc:
                svc.close()
            os.path.exists(sock) and os.remove(sock)

        # a service set up for another address than this server's
        srv = Server(exe, cards, "synth", tmp, builtin, ["--automount", sock])
        svc = FakeService(sock, srv.nbd_port, "127.0.0.1:1")
        try:
            srv.wait(lambda s: s.get("phase") == "running" and s.get("screen"), "boot")
            time.sleep(1)
            srv.cable(True)
            s = srv.wait(lambda s: mount(s).get("state") == "error", "the mismatch is seen")
            check("a service for another address: the page says how to fix it, nothing is mounted",
                  "--nbd 127.0.0.1:%d" % srv.nbd_port in mount(s)["error"] and svc.log == [], mount(s)["error"])
            r = srv.cmd({"do": "line", "name": "cna", "level": 1})
            srv.wait(lambda s: mode(s) == "normal" and s.get("screen"), "back from disk mode", 120)
            check("... and the cable comes out as before", True)
        except Exception as e:
            check("ran (another address)", False, repr(e))
        finally:
            srv.stop()
            svc.close()
            os.path.exists(sock) and os.remove(sock)
            sockdir != tmp and os.rmdir(sockdir)
            if bad:
                print(open(srv.log.name).read()[-3000:])
    print("PASS" if not bad else "FAIL: %d" % bad)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
