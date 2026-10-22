"""`emupod web`: the iPod's front panel in a browser (phone first), recording every input and every
screen so the session can be replayed as a test (`emupod replay`).

One emulator runs in this process, paced to real time, stepped one timer tick at a time by
recording.Driver. Inputs posted by the page are injected between ticks and written down with the
tick they went in at; each screen that settles is written down too. Several pages may watch and
drive the same iPod. "New session" ends the recording and re-executes the server, so every
recording starts from a fresh boot.

Grown from openpod's tools/emu_web.py (claude/emu-webapp, f4e61bb): its incremental LCD decoder,
its key/line commands and its polling page. Its coverage accounting (carved functions reached,
beyond the automatic walks) is openpod tooling and is not carried here.
"""
import base64, json, os, queue, re, shutil, signal, struct, subprocess, sys, threading, time, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from . import paths, recording as R

NAME_RE = re.compile(r"^[A-Za-z0-9._-]{1,120}$")


GREY4 = [(182, 188, 169), (130, 136, 118), (80, 85, 70), (34, 38, 30)]
_cover = [None, b""]                                      # the digest of the screen last encoded, and its PNG


def cover_png(px, size=832, scale=5, key=None):
    """The LCD frame (160 x 128 levels 0-3) centred on a square of the iPod's face colour, as a 6-colour indexed
    PNG: what iOS shows as the cover on the lock screen and in Control Center. Encoded once per screen (`key`)."""
    import struct, zlib
    if key is not None and _cover[0] == key: return _cover[1]
    pad, w, h = 12, 160 * scale, 128 * scale
    x0, y0 = (size - w) // 2, (size - h) // 2
    left, right = bytes(x0 - pad), bytes(size - x0 - w - pad)            # index 0: the face
    face = b"\0" + bytes(size)
    edge = b"\0" + left + b"\1" * (w + 2 * pad) + right
    rows = []
    for y in range(size):
        if y0 <= y < y0 + h and px:
            if (y - y0) % scale == 0:
                r = (y - y0) // scale
                lcd = b"".join(bytes([2 + v]) * scale for v in px[r * 160:(r + 1) * 160])
                row = b"\0" + left + b"\1" * pad + lcd + b"\1" * pad + right
            rows.append(row)
        elif y0 - pad <= y < y0 + h + pad: rows.append(edge)
        else: rows.append(face)
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    plte = bytes([236, 236, 238, 59, 62, 56]) + b"".join(bytes(c) for c in GREY4)
    out = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 3, 0, 0, 0)) + chunk(b"PLTE", plte)
           + chunk(b"IDAT", zlib.compress(b"".join(rows), 6)) + chunk(b"IEND", b""))
    if key is not None: _cover[0], _cover[1] = key, out
    return out


def slug(text):
    return re.sub(r"[^A-Za-z0-9]+", "-", text).strip("-")[:40]


class Live:
    def __init__(self, gen, card=None, audio=None, notice=""):
        self.gen = gen
        self.audio = audio                               # web_audio.AudioOut: the sound, to the browsers or the host
        self.card = card                                 # dict(name, path, keep) when a card is booted
        name = time.strftime("%Y%m%d-%H%M%S")
        self.cmds = queue.Queue()
        self.cond = threading.Condition()
        self.frame = (None, None)                        # (digest, 2-bit pixels) of what the LCD shows now
        self.state = dict(phase="booting", step=0, emu_us=0, inputs=0, screens=0, hold=0, audio=False, clicks=0, np=None,
                          keys=[], lines={}, recent=[], shots=[], dir=name, label="", ver=0, notice=notice,
                          card=dict(name=card["name"], keep=card["keep"]) if card else None)
        self._sig = None
        self.next_action = None                          # "restart" (with a label) or "quit"
        self.ipod = None                                 # the emulator's counters (Rust core), refreshed every 25 steps: /ping hands them out
        self.frame_subs = set()                          # queues of /frames listeners (the test harness): every LCD change

    # --------------------------------------------------------------- the emulator thread
    def run(self):
        if self.card:                                    # a card from the card store, booting its own firmware
            from . import emu
            E = emu.open(emu.Config(disk=self.card["path"], osos="card", disk_persist=self.card["keep"], **R.LIVE_TIMING))
            self.card["osos"] = E.effective()["osos"]
        else:
            raise RuntimeError("no card to boot: the card store could not make or find one (see the [web] lines above)")
        drv = self.drv = R.Driver(E)
        drv.boot()
        rec = self.rec = R.NullRecorder()                 # nothing is recorded
        self._clicks = E.click_count()
        self._publish(drv)
        self.state["phase"] = "running"
        pending = []
        wall0, emu0 = time.monotonic(), drv.now_us()
        while True:
            try:
                while True: pending.append(self.cmds.get_nowait())
            except queue.Empty:
                pass
            stop = False
            while pending and drv.ready(pending[0]) <= drv.step_n:
                c = pending.pop(0)
                pg = c.get("_page") or {}
                if c["do"] in ("restart", "quit"):
                    self.next_action = c; stop = True; break
                if c["do"] == "jack":                    # where the sound goes; the iPod is not told
                    st = self.audio.plug(bool(c.get("plugged")))
                    self._recent(dict(t="jack", plugged=st["plugged"], step=drv.step_n)); continue
                done = drv.apply(c)
                self._note_clicks(drv)
                if done:
                    rec.input(done, wall=c.get("_wall"), page=dict(sent=pg.get("sent"), det=pg.get("det")) if pg.get("sent") else None)
                    self._recent({k: v for k, v in done.items() if k != "px"})
                for shot in drv.settled: self._shot(rec, shot)
                drv.settled.clear()
            if stop: break
            shot = drv.step()
            if self.audio:
                if hasattr(drv.e.audio, "take_scaled"):          # the Rust core applies the volume itself (cheap there, 4% of a Pi core in Python)
                    from .web_audio import gains
                    self.audio.feed(drv.e.audio.take_scaled(*gains(drv.e.audio.volume())), None)
                else: self.audio.feed(drv.e.audio.take(), drv.e.audio.volume())
            self._note_clicks(drv)
            if shot: self._shot(rec, shot)
            self._publish(drv)
            if drv.step_n % 25 == 0 and hasattr(drv.e, "stats"):
                try:
                    snap = dict(drv.e.stats()); snap["host_cpu_s"] = round(time.thread_time(), 3); snap["wall"] = round(time.time(), 3)
                    snap["underruns"] = 0; self.ipod = snap
                except Exception: self.ipod = None
            ahead = (drv.now_us() - emu0) / 1e6 - (time.monotonic() - wall0)
            if ahead > 0.02: time.sleep(min(ahead, 0.05))       # never faster than the real iPod
            elif ahead < -0.5: wall0, emu0 = time.monotonic(), drv.now_us()   # fell behind: do not race to catch up
        rec.close(drv)
        self.state["phase"] = "ended"
        with self.cond: self.cond.notify_all()

    def _note_clicks(self, drv):
        """The firmware's clicks since the last look: recorded ("iclick", at the step they were heard), and
        counted in the state, which the page plays."""
        n = drv.e.click_count()
        if n > self._clicks:
            self.rec.write(dict(t="iclick", step=drv.step_n, emu_us=drv.now_us(), wall=round(time.time(), 3),
                                n=n - self._clicks, total=n))
            self._clicks = n

    def _recent(self, ev):
        self.state["recent"] = (self.state["recent"] + [ev])[-8:]

    def _shot(self, rec, shot):
        rec.screen(shot)
        self.state["shots"] = (self.state["shots"] + [shot["hash"]])[-24:]

    def _publish(self, drv):
        st = self.state
        st.update(step=drv.step_n, emu_us=drv.now_us(), inputs=self.rec.n_inputs if hasattr(self, "rec") else 0,
                  screens=self.rec.n_screens if hasattr(self, "rec") else 0, hold=drv.hold, backlight=int(drv.e.backlight()),
                  audio=drv.e.audio.enabled, keys=sorted(drv.down), lines=drv.levels(), clicks=self._clicks,
                  jack=self.audio.state() if self.audio else None)
        sig = (st["clicks"], st["inputs"], st["screens"], st["audio"], st["backlight"], tuple(st["keys"]), tuple(sorted(st["lines"].items())),
               st["jack"] and (st["jack"]["plugged"], st["jack"]["error"]),
               st["label"], len(st["recent"]) and str(st["recent"][-1]))
        d = drv.screen.digest()
        if d != self.frame[0] or sig != self._sig:
            if sig != self._sig: self._sig = sig; st["ver"] += 1
            if d != self.frame[0]:
                self.frame = (d, bytes(drv.screen.px))
                if self.frame_subs:
                    rec = struct.pack("<QIdI", st["emu_us"], st["step"], time.time(), 5120) + drv.screen.packed()
                    for q in list(self.frame_subs):
                        try: q.put_nowait(rec)
                        except queue.Full: pass
                c = R.read_clock(drv.screen.px)                # Now Playing's elapsed and remaining seconds, if it is up
                st["np"] = list(c) if c else None
            with self.cond: self.cond.notify_all()

    # --------------------------------------------------------------- the web side
    def poll(self, since, ver, wait):
        deadline = time.monotonic() + wait
        with self.cond:
            while self.frame[0] == since and str(self.state["ver"]) == ver and self.state["phase"] != "ended":
                left = deadline - time.monotonic()
                if left <= 0: break
                self.cond.wait(left)
        d, px = self.frame
        out = dict(self.state, screen=d)
        if d and d != since and px:
            out["px"] = base64.b64encode(bytes((px[i] << 6) | (px[i + 1] << 4) | (px[i + 2] << 2) | px[i + 3]
                                               for i in range(0, len(px), 4))).decode()
        return out


def page_times(body):
    """The page's own times sent with a command (ms since the epoch, the page's clock): `sent`, each
    detent of a wheel command (`det`), and the clicks it played since the last command (`clicks`)."""
    def nums(v, width=None):
        out = []
        for x in (v if isinstance(v, list) else [])[:400]:
            if width and isinstance(x, list) and len(x) == width and all(isinstance(y, (int, float)) for y in x): out.append(x)
            elif not width and isinstance(x, (int, float)): out.append(x)
        return out
    sent = body.get("sent")
    return dict(sent=sent if isinstance(sent, (int, float)) else None, det=nums(body.get("det")), clicks=nums(body.get("clicks"), 4))


def serve(live, host, port, cards=None, cards_error=None):
    page = PAGE.encode()
    from .cards_page import CARDS_PAGE
    cpage = CARDS_PAGE.encode()

    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def log_message(self, *a): pass

        def _send(self, body, ctype, code=200, cache=False):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "public, max-age=31536000, immutable" if cache else "no-store")
            self.end_headers()
            self.wfile.write(body)

        def _silence(self):
            """8 kHz 8-bit mono, 3600 s, answered in ranges (Safari asks for them) without being kept in memory."""
            import struct
            n = 8000 * 3600
            hdr = (b"RIFF" + struct.pack("<I", 36 + n) + b"WAVEfmt " + struct.pack("<IHHIIHH", 16, 1, 1, 8000, 8000, 1, 8)
                   + b"data" + struct.pack("<I", n))
            total = len(hdr) + n
            start, end, code = 0, total - 1, 200
            r = self.headers.get("Range")
            if r and r.startswith("bytes="):
                a, _, b = r[6:].partition("-")
                try:
                    start = int(a or 0); end = min(int(b) if b else total - 1, total - 1, start + (4 << 20) - 1); code = 206
                except ValueError:
                    pass
            if start > end: start, end = 0, total - 1
            body = b"".join([hdr[start:end + 1] if start < len(hdr) else b"", b"\x80" * (end + 1 - max(start, len(hdr)))])
            self.send_response(code)
            self.send_header("Content-Type", "audio/wav")
            self.send_header("Accept-Ranges", "bytes")
            if code == 206: self.send_header("Content-Range", "bytes %d-%d/%d" % (start, end, total))
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "public, max-age=86400")
            self.end_headers()
            self.wfile.write(body)

        def _json(self, obj, code=200):
            self._send(json.dumps(obj).encode(), "application/json", code)

        def do_GET(self):
            path, _, qs = self.path.partition("?")
            q = dict(p.split("=", 1) for p in qs.split("&") if "=" in p)
            if path in ("/", "/index.html"):
                self._send(page, "text/html; charset=utf-8")
            elif path == "/offline" or path.startswith("/offline/"):    # the offline app (tools/build_offline.py), as static files
                if path == "/offline":
                    self.send_response(301); self.send_header("Location", "/offline/"); self.end_headers(); return
                import mimetypes
                rel = path[len("/offline/"):] or "index.html"
                f = os.path.realpath(os.path.join(paths.WORK, "offline", rel))
                if not f.startswith(os.path.join(paths.WORK, "offline") + os.sep) or not os.path.isfile(f):
                    self._send(b"not built or not found: python3 tools/build_offline.py", "text/plain", 404)
                else:
                    ct = "application/wasm" if f.endswith(".wasm") else mimetypes.guess_type(f)[0] or "application/octet-stream"
                    self._send(open(f, "rb").read(), ct)
            elif path == "/chicagoflf.ttf":              # the buttons' font
                self._send(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "site", "chicagoflf.ttf"), "rb").read(), "font/ttf")
            elif path == "/silence.wav":                 # an hour of silence the page loops under its real sound
                self._silence()
            elif path == "/screen.png":                  # the lock screen's cover: the iPod's screen on a square card
                self._send(cover_png(live.frame[1], key=live.frame[0]), "image/png")
            elif path == "/ping":                        # the page's latency probe, and what the server has sent it (Statistics, Session sheet)
                self._json(dict(t=round(time.time(), 3), audio=live.audio.client_stats(q.get("cid", "")) if live.audio else None, ipod=live.ipod))
            elif path == "/state":
                self._json(live.poll(q.get("since", ""), q.get("ver", ""), min(float(q.get("wait", "0") or 0), 20.0)))
            elif path == "/audio":                       # the sound, while it plays in the browser (web_audio.py)
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Cache-Control", "no-store")
                self.send_header("X-Sample-Rate", "44100")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                def write(b):
                    self.wfile.write(b"%x\r\n" % len(b) + b + b"\r\n"); self.wfile.flush()
                live.audio.listen(write, lambda: live.state["phase"] != "ended", cid=q.get("cid"))
                try: self.wfile.write(b"0\r\n\r\n")
                except OSError: pass
                self.close_connection = True
            elif path == "/frames":                      # every LCD change, as it happens (the test harness: emupod/harness)
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Cache-Control", "no-store")
                self.send_header("X-Frame-Record", "<QIdI emu_us step wall_time length, then length bytes of pixels (4 a byte); length 0 is a keepalive")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                q = queue.Queue(maxsize=2000)
                with live.cond: live.frame_subs.add(q)
                def write(b):
                    self.wfile.write(b"%x\r\n" % len(b) + b + b"\r\n"); self.wfile.flush()
                try:
                    px = live.frame[1]
                    if px:                                # the screen as it is now, first
                        from .recording import Screen
                        sc = Screen([]); sc.px = bytearray(px)
                        write(struct.pack("<QIdI", live.state["emu_us"], live.state["step"], time.time(), 5120) + sc.packed())
                    while live.state["phase"] != "ended":
                        try: write(q.get(timeout=1))
                        except queue.Empty: write(struct.pack("<QIdI", live.state["emu_us"], live.state["step"], time.time(), 0))   # keepalive
                except OSError: pass
                finally:
                    with live.cond: live.frame_subs.discard(q)
                self.close_connection = True
            elif path == "/cards" or path.startswith("/cards/"):
                self._send(cpage, "text/html; charset=utf-8")
            elif path.startswith("/api/"):
                self._api("GET", path, q)
            else:
                self._json(dict(error="not found"), 404)

        def do_PUT(self):                                # an upload: the raw file is the body
            path, _, qs = self.path.partition("?")
            q = dict(p.split("=", 1) for p in qs.split("&") if "=" in p)
            self._api("PUT", path, q)

        def _api(self, method, path, q):
            """The card store's API (emupod/cards.py). Errors come back as {error} with status 400."""
            from .cards import CardError
            parts0 = [urllib.parse.unquote(x) for x in path.split("/")[2:]]
            if parts0 == ["audio", "devices"] and method == "GET": return self._json(live.audio.devices())       # the host's audio outputs (Session sheet)
            if parts0 == ["audio", "device"] and method == "POST":
                n = int(self.headers.get("Content-Length", 0) or 0)
                try: return self._json(live.audio.set_device(str(json.loads(self.rfile.read(n) or b"{}").get("id", ""))))
                except ValueError as e: return self._json(dict(error=str(e)), 400)
            if cards is None: return self._json(dict(error=cards_error or "the card store is not available"), 503)
            parts = [urllib.parse.unquote(x) for x in path.split("/")[2:]]
            try:
                if method == "GET":
                    if len(parts) == 2 and parts[0] == "firmware":
                        # the flashpod catalog's firmware files, from this server's cache (downloaded on the first ask): the offline app
                        # builds its cards from them. Same origin as the app, so no CORS. TESTING ONLY: see memory offline-firmware-hosting.
                        try: path = cards.fp.fetch_firmware(parts[1])
                        except Exception as e: return self._json(dict(error="no such firmware: %s" % e), 404)
                        with open(path, "rb") as f: return self._send(f.read(), "application/octet-stream")
                    if parts == ["cards"]: return self._json(cards.list())
                    if parts == ["music"]: return self._json(cards.music(urllib.parse.unquote(q.get("path", ""))))
                    if len(parts) == 2 and parts[0] == "cards": return self._json(cards.summary(parts[1]))
                    if len(parts) == 3 and parts[0] == "cards" and parts[2] == "tracks": return self._json(cards.tracks(parts[1]))
                    if len(parts) == 3 and parts[0] == "cards" and parts[2] == "burn": return self._json(cards.burn_command(parts[1]))
                if method == "PUT" and len(parts) == 3 and parts[0] == "cards" and parts[2] == "upload":
                    n = int(self.headers.get("Content-Length", 0) or 0)
                    return self._json(cards.upload(parts[1], urllib.parse.unquote(q.get("file", "")), self.rfile, n))
                if method == "POST":
                    n = int(self.headers.get("Content-Length", 0) or 0)
                    try: body = json.loads(self.rfile.read(n) or b"{}")
                    except ValueError: return self._json(dict(error="bad json"), 400)
                    if parts == ["boot"]:                # restart the emulator on a card (or the standard disk)
                        name = body.get("card") or None
                        if name: cards.path(name)
                        if name and cards.readonly(name): body = dict(body, keep=False)
                        live.cmds.put(dict(do="restart", text="", card=name,
                                           keep=bool(body.get("keep", True)), _wall=time.time(), _page={}))
                        return self._json(dict(ok=True))
                    if len(parts) == 3 and parts[0] == "cards" and parts[2] == "then":
                        # an operation on the card the iPod is running: restart the iPod without it, run the operation,
                        # and put the iPod back on the card (main(): --then-op)
                        op = body.get("op")
                        if op not in ("init", "rebuild", "format", "partition", "flash", "add", "remove"):
                            return self._json(dict(error="no such operation: %s" % op), 400)
                        cards.path(parts[1])
                        live.cmds.put(dict(do="restart", text="", card=None, keep=True, _wall=time.time(), _page={},
                                           then=dict(card=parts[1], op=op, args=body.get("args") or {})))
                        return self._json(dict(ok=True))
                    if len(parts) == 3 and parts[0] == "cards":
                        if parts[2] == "cancel": return self._json(cards.cancel(parts[1]))
                        if parts[2] == "clear-uploads": return self._json(cards.clear_uploads(parts[1]))
                        return self._json(cards.start(parts[1], parts[2], body))
            except CardError as e:
                return self._json(dict(error=str(e)), 400)
            except Exception as e:
                return self._json(dict(error="%s: %s" % (type(e).__name__, e)), 500)
            self._json(dict(error="not found"), 404)

        def do_POST(self):
            if self.path.startswith("/api/"): return self._api("POST", self.path.partition("?")[0], {})
            n = int(self.headers.get("Content-Length", 0) or 0)
            try: body = json.loads(self.rfile.read(n) or b"{}")
            except ValueError: return self._json(dict(ok=False, error="bad json"), 400)
            if self.path != "/cmd": return self._json(dict(error="not found"), 404)
            do = body.get("do")
            ok = do in ("wheel", "edges", "key", "line", "restart", "jack")
            if ok:
                if do == "restart": body["text"] = str(body.get("text", ""))[:2000]
                body["_wall"] = time.time()
                body["_page"] = page_times(body)
                live.cmds.put(body)
            self._json(dict(ok=ok))

    httpd = ThreadingHTTPServer((host, port), H)
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def main(args):
    host = args.host or "127.0.0.1"
    cards, cards_error, card, held, std = None, None, None, None, None
    try:
        from .cards import Cards
        cards = Cards(args.cards, args.flashpod, args.music_root, args.firmware_dir or [])
        std = cards.ensure_standard()                    # first: both cards need the same firmware download, and two downloads into one cache file race
        if cards.ensure_stock(): print("[web] the store was empty: making the stock card (10 GB, Apple 1.5)", flush=True)
    except Exception as e:                               # the panel still works without the card store
        cards_error = "card store unavailable: %s: %s" % (type(e).__name__, e)
        print("[web] " + cards_error, flush=True)
    if cards and not args.card:                          # no card asked for: the one booted last, else the standard disk
        last = cards.load_last()
        if last: args.card = last["card"]; args.discard = not last.get("keep", True); print("[web] booting the last card, %r" % args.card, flush=True)
        elif std: args.card = cards.STANDARD
    if cards and cards.readonly(args.card): args.discard = True                              # its writes are never kept
    if args.card and cards:
        import fcntl
        try:
            path = cards.path(args.card)
            held = open(path, "rb")                     # flashpod's lock: exclusive when the iPod's writes are kept
            fcntl.flock(held, (fcntl.LOCK_SH if args.discard else fcntl.LOCK_EX) | fcntl.LOCK_NB)
            card = dict(name=args.card, path=path, keep=not args.discard)
            cards.booted, cards.booted_keep = args.card, not args.discard
            if not (args.no_remember or args.then_op): cards.save_last(args.card, not args.discard)
        except (BlockingIOError, Exception) as e:
            print("[web] cannot boot card %r (%s); booting the standard disk" % (args.card, e), flush=True)
            held = None
    from .web_audio import AudioOut
    audio = AudioOut(plugged=args.jack, cmd=args.audio_cmd.split() if args.audio_cmd else None, store=os.path.join(paths.WORK, "audio-device.json"))
    live = Live(args.gen, card=card, audio=audio, notice=args.notice)
    httpd = serve(live, host, args.port, cards, cards_error)
    signal.signal(signal.SIGTERM, lambda *_: live.cmds.put(dict(do="quit")))
    if args.then_op and cards:
        spec = json.loads(args.then_op)
        def then():
            """The card is free (the iPod runs without it): do the operation; if it worked, put the iPod back on the card."""
            ok = False
            try:
                cards.start(spec["card"], spec["op"], spec.get("args") or {})
                while True:
                    time.sleep(0.5)
                    st = cards.ops.get(spec["card"])
                    if not st or st["state"] != "running": break
                ok = bool(st) and st["state"] == "done"
            except Exception as e:
                print("[web] --then-op failed: %s: %s" % (type(e).__name__, e), flush=True)
            if ok:
                live.cmds.put(dict(do="restart", text="", card=spec["card"], keep=True, _wall=time.time(), _page={}))
        threading.Thread(target=then, daemon=True).start()
    print("[web] http://%s:%d/" % (host, args.port), flush=True)
    try:
        live.run()
    except KeyboardInterrupt:
        live.rec.close(live.drv)
        return
    except Exception:
        if not card or live.state["phase"] != "booting": raise
        # The card's firmware did not start in the emulator (Apple 1.0 stops at reset, 2026-10-18). Come back
        # on the standard disk and say so, rather than letting the service restart there without a word.
        import traceback; traceback.print_exc()
        fw = ""
        try: fw = (cards.summary(card["name"]).get("scan") or {}).get("firmware", {}).get("build") or ""
        except Exception: pass
        fw = fw.replace("stock ", "Apple ")
        live.next_action = dict(do="restart", text="", card=None, no_remember=True, notice="Card \u201c%s\u201d did not start: the emulator "
                                "could not run its firmware%s. The iPod is on the standard disk; write another firmware to "
                                "the card (Cards, then the card, then Firmware) and boot it again." % (card["name"], " (%s)" % fw if fw else ""))
        cards.booted = None
    finally:
        httpd.shutdown(); httpd.server_close()
        plugged = audio.plugged; audio.close()
    nxt = live.next_action or {}
    if nxt.get("do") == "restart":                       # a fresh boot for a fresh recording
        argv = [sys.executable, "-m", "emupod", "web", args.gen, "--port", str(args.port), "--host", host,
                "--cards", args.cards, "--flashpod", args.flashpod]
        if nxt.get("notice"): argv += ["--notice", nxt["notice"]]
        if nxt.get("then"): argv += ["--then-op", json.dumps(nxt["then"])]
        if args.music_root: argv += ["--music-root", args.music_root]
        if plugged: argv += ["--jack"]                   # the plug stays in across sessions
        if args.audio_cmd: argv += ["--audio-cmd", args.audio_cmd]
        for d in args.firmware_dir or []: argv += ["--firmware-dir", d]
        nc = nxt["card"] if "card" in nxt else (cards.booted if cards else args.card)   # "New session" keeps the card (by its
                                                                                       # current name); /api/boot chooses
        if nc:
            argv += ["--card", nc]
            if not nxt.get("keep", not args.discard): argv += ["--discard"]
        elif cards:                                      # "no card": the standard disk, asked for by name (else the last card would be picked)
            argv += ["--card", cards.STANDARD, "--discard"]
        if nxt.get("no_remember") or nxt.get("then"): argv += ["--no-remember"]
        os.chdir(paths.ROOT)
        os.execv(sys.executable, argv)


PAGE = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "pages", "ipod.html"), encoding="utf-8").read()   # shared with the Rust server
