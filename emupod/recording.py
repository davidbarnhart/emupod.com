"""Drive the emulator one tick at a time, record what was pressed and what was shown, and replay it.

The live web front panel (web.py) and `emupod replay` share this module, so a recording replays
through exactly the code that made it. The emulator is deterministic in the iPod's time: given
the same firmware, disk and boot, the same inputs injected at the same STEP give the same
screens. A step is one timer tick (~10.2 ms of iPod time): `settle(max_iters=1)`. A recording
therefore stores, for every input, the step it was injected before; wall-clock time is kept only
for people. See docs/RECORDINGS.md for the format.
"""
import hashlib, json, os, queue, struct, subprocess, sys, threading, time, zlib
from . import paths

FORMAT = "emupod-recording/1"
W, H = 160, 128
QUIET_STEPS = 12          # an LCD unchanged this many ticks is a settled screen (settle()'s default)
MIN_HOLD_STEPS = 6        # a key is held at least this long (~60 ms); an earlier release waits
SCALE = 3                 # screen PNGs are written 3x (480 x 384), nearest-neighbour
GREY = (200, 207, 192), (140, 148, 132), (80, 86, 74), (20, 24, 18)   # LCD levels 0 (white) .. 3

# The five keys: port A bits 0-4, down while LOW (ipod_emu_ui.py, KEYS_UP). MEASURED 2026-10-16 on Now Playing
# (stock 1.5): bit 2 pauses (the sound stops), bits 0 and 3 change track with the sound going on, bit 0 forward.
KEYS = {"ff": (0, 1 << 0), "select": (0, 1 << 1), "play": (0, 1 << 2), "rew": (0, 1 << 3), "menu": (0, 1 << 4)}
# The names used before that, by every recording made until then: "play" was bit 0 (it skipped to the next
# track, David 2026-10-16), "rew" bit 2 (the real Play/Pause) and "ff" bit 3 (the real Rew). A recording says
# which map its key names follow (recording.json "keymap"); one without it follows this one.
KEYS_BEFORE_2026_10_16 = {"play": (0, 1 << 0), "select": (0, 1 << 1), "rew": (0, 1 << 2), "ff": (0, 1 << 3), "menu": (0, 1 << 4)}
KEYMAP = "measured-2026-10-16"


def key_from_old(name):
    """The key, by today's names, on the line an old (pre-2026-10-16) key name pressed."""
    line = KEYS_BEFORE_2026_10_16[name]
    return next(k for k, v in KEYS.items() if v == line)
LINES = ("hold", "firewire", "cna", "battery")


def sha256_file(path, n=16):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""): h.update(b)
    return h.hexdigest()[:n]


def emulator_env(gen="gen1"):
    """The standard run's environment (as `emupod play`): unplugged, music disk, virtual clock."""
    wp = paths.work_paths(gen)
    env = {"OPENPOD_OSOS": wp["osos"], "OPENPOD_IPSW": paths.ipsw_path(gen), "OPENPOD_DISK": wp["disk"]}
    return env


def _emu_version():
    from . import emu
    return emu.version()["emupod"]


def load_emulator(disk, osos="card", extra=None, timing=None):
    """Open the emulator in this process through the public API (emupod.emu), booting the card image `disk`
    (`osos` "card": the firmware on the card; a path: another image, e.g. a cimage), `extra` (FILE@ADDR) mapping
    emulator-only memory and `timing` other emu.Config fields (a recording's meta["emulator"]["timing"],
    e.g. LIVE_TIMING). Returns the emu.Emulator."""
    from . import emu
    x = []
    if extra:
        f, a = extra.rsplit("@", 1); x = [(f, int(a, 0))]
    return emu.open(emu.Config(osos=osos, disk=disk, extra=x, **(timing or {})))


# The web app's timing: longer turns for the two cores once a song is playing, so the emulator keeps up with
# the iPod's sound (0.75x real time without, about 1.5x with; the sound is the same sample for sample). A
# recording made with it says so (meta["emulator"]["timing"]) and replay uses it.
LIVE_TIMING = dict(interleave_playing=300, interleave_settle=200_000, audio_fpf="auto", cop_ipus=26, ata_fast=True,
                   start_volume=114)           # 90%, at David's word (the firmware's own start is 53, 42%)


class Screen:
    """The LCD, decoded incrementally from the emulator's controller log (lcd_render's state
    machine, kept between calls). Consumed log entries are dropped so a long session stays small."""

    def __init__(self, log):
        self.log = log
        self.idx, self.ib, self.db, self.addr = None, [], [], 0
        self.px = bytearray(W * H)                       # 2-bit levels, 0 = white

    def update(self):
        log = self.log
        if not log: return False
        for a, v in log:
            if a == 0xc0001008:
                self.ib.append(v & 0xff)
                if len(self.ib) == 2: self.idx = (self.ib[0] << 8) | self.ib[1]; self.ib = []
            elif a == 0xc0001010:
                self.db.append(v & 0xff)
                if len(self.db) == 2:
                    word = (self.db[0] << 8) | self.db[1]; self.db = []
                    if self.idx == 0x11: self.addr = word
                    elif self.idx == 0x12: self._put(self.addr, word); self.addr += 32
            elif a == 0xc0001000:
                self.ib = []; self.db = []
        del log[:]
        return True

    def _put(self, addr, word):
        y, wcol = addr >> 5, (addr & 0x1f) - 1
        if not (0 <= y < H) or wcol < 0: return
        row = y * W
        for b in range(8):
            x = (19 - wcol) * 8 + b
            if 0 <= x < W: self.px[row + x] = (word >> (14 - 2 * b)) & 3

    def digest(self):
        return hashlib.sha1(bytes(self.px)).hexdigest()[:12]

    def packed(self):
        """4 pixels a byte, first pixel in the high bits: 5,120 bytes a frame."""
        p = self.px
        return bytes((p[i] << 6) | (p[i + 1] << 4) | (p[i + 2] << 2) | p[i + 3] for i in range(0, W * H, 4))

    def png(self, scale=SCALE):
        return png(self.px, scale)


_CHAN = [bytes.maketrans(bytes(range(4)), bytes(GREY[v][c] for v in range(4))) for c in range(3)]      # level -> one colour channel


def png(px, scale=SCALE):
    """An LCD frame (160 x 128 levels 0-3) as an RGB PNG, scaled up nearest-neighbour. Built from whole colour planes with slice
    assignments (about 3 ms on a Pi 4; the per-pixel version took 35 ms and stalled the emulator's thread)."""
    px = bytes(px)
    planes = [px.translate(t) for t in _CHAN]
    row_len = W * scale * 3
    out = bytearray()
    for y in range(H):
        line = bytearray(row_len)
        for c in range(3):
            plane = planes[c][y * W:(y + 1) * W]
            for k in range(scale): line[k * 3 + c::scale * 3] = plane
        out += (b"\0" + line) * scale
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", W * scale, H * scale, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(out), 3)) + chunk(b"IEND", b""))


def read_png(path):
    """The levels (160 x 128, 0-3) of a screen PNG this module wrote (png(): RGB, filter 0, scaled)."""
    data = open(path, "rb").read()
    pos, idat, w = 8, b"", None
    while pos < len(data):
        n, t = struct.unpack(">I4s", data[pos:pos + 8]); body = data[pos + 8:pos + 8 + n]; pos += 12 + n
        if t == b"IHDR": w, h = struct.unpack(">II", body[:8])
        elif t == b"IDAT": idat += body
    raw = zlib.decompress(idat); stride = 1 + 3 * w; scale = w // W
    level = {bytes(g): k for k, g in enumerate(GREY)}
    px = bytearray(W * H)
    for y in range(H):
        row = raw[y * scale * stride + 1:(y * scale) * stride + stride]
        for x in range(W):
            px[y * W + x] = level.get(bytes(row[x * scale * 3:x * scale * 3 + 3]), 0)
    return px


CLOCK_ROW = 90                  # native rows at and below this are Now Playing's bar and times (walk_diff's 270 at 3x)
TIME_ROWS = range(111, 120)     # the two times' glyphs (native), elapsed left of x 80, remaining right
GLYPHS_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "clock_glyphs.json")


def time_glyphs(px):
    """The glyphs on Now Playing's time row, left to right: (x, bitmap key)."""
    cols = [any(px[y * W + x] >= 2 for y in TIME_ROWS) for x in range(W)]
    out, x = [], 0
    while x < W:
        if cols[x]:
            s = x
            while x < W and cols[x]: x += 1
            out.append((s, bytes(1 if px[y * W + i] >= 2 else 0 for y in TIME_ROWS for i in range(s, x)).hex()))
        x += 1
    return out


def read_clock(px):
    """(elapsed s, remaining s) read with the firmware's own glyphs (clock_glyphs.json, learned from screens
    read by eye), or None. Any glyph not learned gives None, so it can never pass silently."""
    try: known = json.load(open(GLYPHS_FILE))
    except OSError: return None
    gs = time_glyphs(px)
    if not gs or any(k not in known for _, k in gs): return None
    left = "".join(known[k] for x, k in gs if x < 80); right = "".join(known[k] for x, k in gs if x >= 80)
    try:
        def secs(t): m, s = t.lstrip("-").split(":"); return int(m) * 60 + int(s)
        return secs(left), secs(right)
    except ValueError:
        return None


def learn_clock(pairs):
    """pairs: [(png path, "0:02", "-0:18")], the times read by eye. Adds their glyphs to clock_glyphs.json."""
    try: known = json.load(open(GLYPHS_FILE))
    except OSError: known = {}
    for path, l, r in pairs:
        gs = time_glyphs(read_png(path)); text = [c for c in l + r if c != " "]
        assert len(gs) == len(text), (path, len(gs), text)
        for (_, k), c in zip(gs, text):
            assert known.get(k, c) == c, ("conflict", path, c)
            known[k] = c
    with open(GLYPHS_FILE, "w") as f: json.dump(known, f, indent=0); f.write("\n")
    return known


def clock_only(a, b):
    """Two screens that differ only in the Now Playing bar/time rows, AND whose times both read with the same
    total (elapsed + remaining), so a wrong duration is never filed as clock (firmware's walk_clock rule)."""
    if a == b or a[:CLOCK_ROW * W] != b[:CLOCK_ROW * W]: return False
    ta, tb = read_clock(a), read_clock(b)
    return ta is not None and tb is not None and sum(ta) == sum(tb)


class Driver:
    """The emulator, stepped one tick at a time, with inputs injected between steps.

    Key edges are bare GPIO edges: the ticks that follow are the normal steps, so a person's
    timing is what the firmware sees. A wheel detent is four edges with one tick after each
    (~40 ms of iPod time): with none the firmware reads the lines after all four and loses the
    detent; ipod_emu_ui's own wheel() gives each edge eight ticks (a third of a second a detent,
    far too slow under a thumb). The firmware does not move one item a detent either way: the
    first detent after a pause often moves nothing (stock wheel(4) after a pause: 2 items)."""

    def __init__(self, e):
        self.e = e                                       # an emupod.emu.Emulator
        self.step_n = 0
        self.screen = e.lcd.make_screen() if hasattr(e.lcd, "make_screen") else Screen(e.lcd.log)
        self.down = {}                                   # key -> step it went down
        self.stable = None                               # digest of the last settled screen
        self._cand, self._quiet = None, 0
        self.hold = 0
        self.settled = []                                # screens that settled inside inject(), as step() returns them

    def boot(self):
        self.e.boot()
        self.screen.update()
        self.hold = self.e.line_levels()["hold"]

    def now_us(self):
        return self.e.now_us()

    def levels(self):
        """What each event line reads now (hold 1 = on; firewire/cna 0 = present; battery 1 = good)."""
        return self.e.line_levels()

    def step(self):
        """One tick. Returns a newly settled screen as a dict (step, emu_us, hash, px), or None."""
        self.e.tick()
        self.step_n += 1
        changed = self.screen.update()
        d = self.screen.digest() if changed or self._cand is None else self._cand
        if d != self._cand: self._cand, self._quiet = d, 0
        else: self._quiet += 1
        if self._quiet == QUIET_STEPS and d != self.stable:
            self.stable = d
            return dict(step=self.step_n, emu_us=self.now_us(), hash=d, px=bytes(self.screen.px))
        return None

    def ready(self, ev):
        """The step at which `ev` may be injected (a key release waits out MIN_HOLD_STEPS)."""
        if ev.get("do") == "key" and ev.get("phase") == "up" and ev.get("key") in self.down:
            return self.down[ev["key"]] + MIN_HOLD_STEPS
        return self.step_n

    def apply(self, ev):
        """inject(), stamped with where it happened: the step it went in before, the iPod's time
        and the screen then. Returns the input record (what a recording stores), or None."""
        at = dict(step=self.step_n, emu_us=self.now_us(), screen=self.screen.digest())
        px = bytes(self.screen.px)                       # the screen the input was given on, for its PNG
        done = self.inject(ev)
        return dict(at, px=px, **done) if done else None

    def inject(self, ev):
        """Apply one input. Returns the normalised event, or None if it changed nothing."""
        e, do = self.e, ev.get("do")
        if do == "wheel":
            n = int(ev.get("n", 0))
            if not n: return None
            def tick():
                shot = self.step()
                if shot: self.settled.append(shot)
            e.wheel(n, tick=tick)
            return dict(do="wheel", n=n)
        if do == "edges":                                # single wheel edges, as a freely turning wheel gives them
            n = max(-400, min(400, int(ev.get("n", 0))))
            if not n: return None
            def tick():
                shot = self.step()
                if shot: self.settled.append(shot)
            e.wheel_edges(n, tick=tick)
            return dict(do="edges", n=n)
        if do == "key" and ev.get("key") in KEYS and ev.get("phase") in ("down", "up"):
            k, phase = ev["key"], ev["phase"]
            if (phase == "down") == (k in self.down): return None      # already in that state
            (e.key_down if phase == "down" else e.key_up)(k)
            if phase == "down": self.down[k] = self.step_n
            else: self.down.pop(k, None)
            return dict(do="key", key=k, phase=phase)
        if do == "line" and ev.get("name") in LINES:
            name, level = ev["name"], int(ev.get("level", 0))
            if not e.line(name, level): return None                     # already at that level
            if name == "hold": self.hold = level
            return dict(do="line", name=name, level=level)
        return None


class NullRecorder:
    """What the server uses: it keeps nothing (no inputs, no screens, no notes). The Recorder below writes a recording directory for the test tools."""
    n_inputs = 0
    n_screens = 0
    def __init__(self, *a, **k): pass
    def save_meta(self, **extra): pass
    def write(self, rec): pass
    def input(self, rec, wall=None, page=None): pass
    def screen(self, shot): pass
    def note(self, drv, text): pass
    def close(self, drv): pass


class Recorder:
    """Writes a recording directory: recording.json, events.jsonl, screens/<digest>.png."""

    def __init__(self, out, gen="gen1", label="", card=None, timing=None, async_png=False):
        """`card`: dict(name, path, osos, keep) when the session runs a card from the card store, not the
        standard disk. Such a recording is marked private (a card may hold David's music): never a test.
        `timing`: the emu.Config timing fields the session ran with (LIVE_TIMING), which replay must use."""
        self.out = out
        os.makedirs(os.path.join(out, "screens"), exist_ok=True)
        wp = paths.work_paths(gen)
        if card:
            st = os.stat(card["path"])
            boot = "card %s: unplugged iPod, the card's own firmware, virtual clock; the iPod's writes %s" % (
                card["name"], "kept on the card" if card.get("keep") else "discarded")
            firmware = dict(source="the card's firmware partition", osos_sha256=sha256_file(card["osos"]))
            disk = dict(card=card["name"], size=st.st_size, mtime=st.st_mtime, keep=bool(card.get("keep")))
        else:
            boot = "standard: unplugged iPod, music disk, virtual clock"
            firmware = dict(ipsw=os.path.basename(paths.ipsw_path(gen)), osos_sha256=sha256_file(wp["osos"]))
            disk = dict(file=os.path.basename(wp["disk"]), sha256=sha256_file(wp["disk"]))
        self.meta = dict(
            format=FORMAT, gen=gen, label=label, created=time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            boot=boot, firmware=firmware, disk=disk, private=bool(card),
            emulator=dict(emupod=_emu_version(),
                          recording_sha256=sha256_file(os.path.abspath(__file__)), emupod_commit=_commit(),
                          **(dict(timing=dict(timing)) if timing else {})),
            step="one timer tick (settle(max_iters=1)); inputs are injected before the step numbered",
            keymap=KEYMAP, keys={k: "port %d bit %d" % (p, m.bit_length() - 1) for k, (p, m) in KEYS.items()},
            quiet_steps=QUIET_STEPS, min_hold_steps=MIN_HOLD_STEPS, screen_png_scale=SCALE)
        self.events = open(os.path.join(out, "events.jsonl"), "a")
        self._async = async_png
        if async_png:                                    # screens are encoded and written on a thread of their own, never on the emulator's
            self._pngq, self._pngseen = queue.Queue(), set()
            def writer():
                while True:
                    item = self._pngq.get()
                    if item is None: return
                    f, px = item
                    try:
                        tmp = f + ".tmp"
                        with open(tmp, "wb") as fh: fh.write(png(px))
                        os.replace(tmp, f)
                    except OSError: pass
            self._pngthread = threading.Thread(target=writer, daemon=True); self._pngthread.start()
        self.n_inputs = self.n_screens = 0
        self.save_meta()

    def save_meta(self, **extra):
        self.meta.update(extra)
        tmp = os.path.join(self.out, "recording.json.tmp")
        with open(tmp, "w") as f: json.dump(self.meta, f, indent=2); f.write("\n")
        os.replace(tmp, os.path.join(self.out, "recording.json"))

    def write(self, rec):
        self.events.write(json.dumps(rec) + "\n"); self.events.flush()

    def _png(self, digest, px):
        rel = os.path.join("screens", digest + ".png")
        f = os.path.join(self.out, rel)
        if self._async:
            if digest not in self._pngseen and not os.path.exists(f):
                self._pngseen.add(digest); self._pngq.put((f, bytes(px)))
            return rel
        if not os.path.exists(f):
            with open(f, "wb") as fh: fh.write(png(px))
        return rel

    def input(self, rec, wall=None, page=None):
        """`rec` is Driver.apply()'s record; the screen it was given on is saved as a PNG too. `page`: the
        page's own times for it (sent, and a wheel command's detents), kept as they came."""
        self.n_inputs += 1
        rec = dict(rec); px = rec.pop("px")
        rec["png"] = self._png(rec["screen"], px)
        if page: rec["page"] = page
        self.write(dict(t="input", wall=round(wall or time.time(), 3), applied=round(time.time(), 3), **rec))

    def screen(self, shot):
        """`shot` is what Driver.step() returned for a newly settled screen."""
        self.n_screens += 1
        png = self._png(shot["hash"], shot["px"])
        self.write(dict(t="screen", step=shot["step"], emu_us=shot["emu_us"], wall=round(time.time(), 3),
                        hash=shot["hash"], png=png))

    def note(self, drv, text):
        self.write(dict(t="note", step=drv.step_n, emu_us=drv.now_us(), wall=round(time.time(), 3), text=text))

    def close(self, drv):
        self.write(dict(t="end", step=drv.step_n, emu_us=drv.now_us(), wall=round(time.time(), 3)))
        if self._async: self._pngq.put(None); self._pngthread.join(timeout=30)
        self.events.close()
        self.save_meta(ended=time.strftime("%Y-%m-%dT%H:%M:%S%z"), steps=drv.step_n, emu_us=drv.now_us(),
                       inputs=self.n_inputs, screens=self.n_screens)


def _commit():
    try:
        r = subprocess.run(["git", "-C", paths.ROOT, "rev-parse", "--short", "HEAD"], capture_output=True, text=True)
        dirty = subprocess.run(["git", "-C", paths.ROOT, "status", "--porcelain", "--", "emupod"],
                               capture_output=True, text=True).stdout.strip()
        return r.stdout.strip() + ("-dirty" if dirty else "")
    except OSError:
        return None


DEFAULT_FUNCS = os.path.expanduser("~/source/openpod-corpus/autopilot/funcs.json")
IRAM = (0x40000000, 0x40100000)


class Reach:
    """Which code a replay ran: every basic block on either core, folded into the carved function
    that contains it (any block counts, not only a function's entry), each credited to the input
    after which it FIRST ran (-1 = the boot). The low DRAM aliases are folded to 0x28000000. IRAM
    blocks are kept apart, keyed by address and the hash of their bytes, since the overlay loaded
    there decides what an IRAM address means."""

    def __init__(self, e, funcs_path, placed=None):
        if not funcs_path or not os.path.exists(funcs_path):
            raise FileNotFoundError("reach needs a carve ({\"funcs\": [{addr, size}]}): give --funcs FILE (the default, %s, "
                                    "is openpod's corpus and is not here)" % DEFAULT_FUNCS)
        import bisect
        self._bisect = bisect
        funcs = sorted((f["addr"], f["size"]) for f in json.load(open(funcs_path))["funcs"])
        self.starts = [a for a, _ in funcs]; self.ends = [a + n for a, n in funcs]
        self.funcs_path = funcs_path
        # A C image (tools/fw_cimage.py placed.json) runs some functions somewhere else: each
        # `trampolined` one at `at`, for `c_bytes`. Blocks there count toward the function they
        # stand for, `addr`, so a C image's reach compares with stock's.
        far = []
        if placed:
            for t in json.load(open(placed)).get("trampolined", []):
                far.append((int(t["at"], 16), int(t["at"], 16) + t["c_bytes"], int(t["addr"], 16)))
        far.sort(); self.far = far; self.far_starts = [a for a, _, _ in far]
        self.blocks = {}                                 # (addr, size, core) seen -> None
        self.credit = -1
        self.func_first = {}                             # function start -> input index
        self.iram_first = {}                             # (addr, bytes hash) -> input index
        self.stray_first = {}                            # block in no carved function -> input index
        from unicorn import UC_HOOK_BLOCK
        seen = self.blocks
        def hook(core, addr, size, value):
            k = (addr, size)
            if k in seen: return
            seen[k] = None
            self._note(core, addr, size)
        e.add_hook("block", hook)

    def _note(self, core, addr, size):
        if IRAM[0] <= addr < IRAM[1]:
            h = hashlib.sha1(core.mem_read(addr, max(size, 4))).hexdigest()[:8]
            self.iram_first.setdefault((addr, h), self.credit); return
        if addr < 0x04000000: addr += 0x28000000
        elif addr < 0x08000000: addr += 0x24000000
        j = self._bisect.bisect_right(self.far_starts, addr) - 1
        if j >= 0 and addr < self.far[j][1]:
            self.func_first.setdefault(self.far[j][2], self.credit); return
        i = self._bisect.bisect_right(self.starts, addr) - 1
        if i >= 0 and addr < self.ends[i]: self.func_first.setdefault(self.starts[i], self.credit)
        else: self.stray_first.setdefault(addr, self.credit)

    def report(self, inputs):
        per = {}
        for f, k in self.func_first.items(): per.setdefault(k, []).append(f)
        return dict(
            rule="any executed block counts toward the carved function containing it; low aliases folded",
            funcs=self.funcs_path, functions=len(self.func_first), iram_blocks=len(self.iram_first),
            stray_blocks=len(self.stray_first),
            by_input=[dict(input=k, action=(None if k < 0 else {x: inputs[k].get(x) for x in
                                                                 ("step", "do", "n", "key", "phase", "name", "level")}),
                           new_functions=["0x%08x" % f for f in sorted(fs)]) for k, fs in sorted(per.items())],
            all_functions=["0x%08x" % f for f in sorted(self.func_first)],
            iram=[dict(addr="0x%08x" % a, bytes_sha1=h, first_input=k) for (a, h), k in sorted(self.iram_first.items())],
            stray=["0x%08x" % a for a in sorted(self.stray_first)])


# RTXC's task table (decomp #834, tools/fw_tasks.py): 56-byte TCBs, stack base (low end) at +0x14, size at +0x18.
TCB_ARRAY, TCB_SIZE, TASK_COUNT = 0x004b5d18, 56, 0x003e50d0
MODES = {0x10: "usr", 0x11: "fiq", 0x12: "irq", 0x13: "svc", 0x17: "abt", 0x1b: "und", 0x1f: "sys"}


class StackDepth:
    """The lowest sp each RTXC task's stack reached. sp is sampled at every block entry on both cores, so a
    dip inside a block is missed: a LOWER BOUND on depth (firmware's sp-write hook is exact). The task
    table is read when the run ends. A stack is (base, base+size]: sp == base+size is that task's EMPTY stack
    (the next task's base). An sp outside every task stack is reported per core and processor mode (the
    exception stacks the reset code sets up are not in the table), with whether sp was ever 0 there."""

    def __init__(self, e):
        self.e = e
        lows = self.lows = {}                            # (core, mode) -> {sp>>4: lowest sp in that 16 B}
        def hook(core, addr, size, value):
            sp = core.reg("sp"); k = (core.name, core.reg("cpsr") & 0x1f)
            d = lows.get(k)
            if d is None: d = lows[k] = {}
            b = sp >> 4
            if sp < d.get(b, 1 << 32): d[b] = sp
        e.add_hook("block", hook)

    def report(self):
        cpu = self.e.cpu
        n = cpu.u32(TASK_COUNT)
        tasks = []
        for t in range(min(n, 256)):
            raw = cpu.mem_read(TCB_ARRAY + t * TCB_SIZE, TCB_SIZE)
            base, size = struct.unpack_from("<II", raw, 0x14)
            if size: tasks.append((t, base, size))
        seen = [(core, mode, sp) for (core, mode), d in self.lows.items() for sp in d.values()]
        out, claimed = [], set()
        for t, base, size in tasks:
            inside = [(sp, core, mode) for core, mode, sp in seen if base < sp <= base + size]   # top = empty
            claimed.update((core, mode, sp) for sp, core, mode in inside)
            low = min(inside) if inside else None
            out.append(dict(task=t, base="0x%08x" % base, size=size, top="0x%08x" % (base + size),
                            lowest_sp=("0x%08x" % low[0]) if low else None, headroom=(low[0] - base) if low else None,
                            used=(base + size - low[0]) if low else None, core=low[1] if low else None,
                            mode=MODES.get(low[2], hex(low[2])) if low else None))
        other = {}
        for core, mode, sp in seen:
            if (core, mode, sp) in claimed: continue
            o = other.setdefault("%s/%s" % (core, MODES.get(mode, hex(mode))), dict(zero_seen=False, lowest=None, highest=None))
            if sp == 0: o["zero_seen"] = True; continue
            o["lowest"] = sp if o["lowest"] is None else min(o["lowest"], sp)
            o["highest"] = sp if o["highest"] is None else max(o["highest"], sp)
        for o in other.values():
            for k in ("lowest", "highest"):
                if o[k] is not None: o[k] = "0x%08x" % o[k]
        return dict(rule="sp sampled at every block entry on both cores (a lower bound on depth); task stacks from the "
                         "RTXC TCB array at 0x%08x read at the end of the run; headroom = lowest sp - base, taking base "
                         "(+0x14) as the low end" % TCB_ARRAY,
                    task_count=n, tasks=out,
                    outside_task_stacks=dict(sorted(other.items())))


def _stack_out(sd, path, E, ok, report):
    if sd:
        r = sd.report()
        with open(path, "w") as f: json.dump(r, f, indent=1); f.write("\n")
        tight = sorted((t for t in r["tasks"] if t["headroom"] is not None), key=lambda t: t["headroom"])[:5]
        report["stack"] = dict(file=path, tasks=len(r["tasks"]),
                               tightest=[(t["task"], t["headroom"], t["size"]) for t in tight])
    return ok, report


SYNC_MAX_STEPS = 500            # how long a screen-synchronised replay waits for an input's screen (~5 s)


def replay(rec_dir, gen="gen1", out=None, verbose=True, reach=None, funcs=DEFAULT_FUNCS, osos=None, extra=None,
           placed=None, loose=False, sync=False, shift=0, stack=None):
    """Boot, inject each recorded input at its step, and compare the settled screens with the
    recording. Returns (ok, report dict). `out` writes the replay as a recording of its own;
    `reach` writes which code ran (see Reach) to that JSON file."""
    meta = json.load(open(os.path.join(rec_dir, "recording.json")))
    if meta.get("format") != FORMAT: raise ValueError("not an %s recording: %s" % (FORMAT, rec_dir))
    if meta.get("disk", {}).get("card"):
        raise ValueError("%s was recorded on card %r, whose state then is not kept; replay boots the standard disk "
                         "only" % (rec_dir, meta["disk"]["card"]))
    events = [json.loads(l) for l in open(os.path.join(rec_dir, "events.jsonl")) if l.strip()]
    inputs = [e for e in events if e["t"] == "input"]
    if meta.get("keymap") != KEYMAP:                     # an older recording: its key names meant other lines
        for e in inputs:
            if e.get("do") == "key" and e.get("key") in KEYS_BEFORE_2026_10_16: e["key"] = key_from_old(e["key"])
    INPUT_KEYS = ("do", "n", "key", "phase", "name", "level")
    want = [(e["step"], e["hash"]) for e in events if e["t"] == "screen"]
    end = next((e["step"] for e in events if e["t"] == "end"), None)
    if end is None: end = max([s for s, _ in want] + [e["step"] for e in inputs] + [0]) + QUIET_STEPS + 1

    timing = meta.get("emulator", {}).get("timing")
    E = load_emulator(gen, osos=osos, extra=extra, timing=timing)
    wp = paths.work_paths(gen)
    warn = []
    if osos: print("[replay] booting %s (sha %s) instead of the recording's firmware: a divergence is a difference "
                   "between the two images" % (osos, sha256_file(osos)), file=sys.stderr)
    if sha256_file(wp["disk"]) != meta["disk"]["sha256"]:
        warn.append("disk differs from the recording's (%s): rebuild it or expect divergence" % meta["disk"]["sha256"])
    if not osos and sha256_file(wp["osos"]) != meta["firmware"]["osos_sha256"]:
        warn.append("firmware differs from the recording's")
    for w in warn: print("[replay] WARNING:", w, file=sys.stderr)

    drv = Driver(E)
    rc = Reach(E, funcs, placed) if reach else None
    sd = StackDepth(E) if stack else None
    drv.boot()
    rec = Recorder(out, gen, label="replay of " + os.path.basename(os.path.normpath(rec_dir)), timing=timing) if out else None
    got, timing, at_input = [], [], []
    def settled(shot):
        got.append((shot["step"], shot["hash"]))
        if rec: rec.screen(shot)
    if sync:
        ok, report = _replay_sync(rec_dir, drv, inputs, want, end, settled, rec, rc, reach, warn, INPUT_KEYS, got, shift)
        return _stack_out(sd, stack, E, ok, report)
    i = 0
    while drv.step_n <= end:
        while i < len(inputs) and inputs[i]["step"] == drv.step_n:
            want_in = inputs[i]
            if drv.now_us() != want_in["emu_us"]: timing.append((i, want_in["emu_us"], drv.now_us()))
            if drv.screen.digest() != want_in["screen"]:
                at_input.append(dict(input=i, step=want_in["step"], recorded=want_in["screen"], replayed=drv.screen.digest()))
            if rc: rc.credit = i
            done = drv.apply({k: want_in[k] for k in INPUT_KEYS if k in want_in})
            if rec and done: rec.input(done)
            for shot in drv.settled: settled(shot)
            drv.settled.clear()
            i += 1
        shot = drv.step()
        if shot: settled(shot)
    if rec: rec.close(drv)
    first = next((k for k in range(max(len(want), len(got)))
                  if k >= len(want) or k >= len(got) or want[k] != got[k]), None)
    ok = first is None and i == len(inputs) and not at_input
    if loose:
        # Another image runs on its own timing (C is faster than Apple's code), so ticks are not
        # comparable: compare only the ORDER of the settled screens, repeats folded.
        import difflib
        def seq(xs):
            out = []
            for _, h in xs:
                if not out or out[-1] != h: out.append(h)
            return out
        a, b = seq(want), seq(got)
        sm = difflib.SequenceMatcher(None, a, b, autojunk=False)
        diffs = [(op, a[i1:i2], b[j1:j2], i1, j1) for op, i1, i2, j1, j2 in sm.get_opcodes() if op != "equal"]
        loose_rep = dict(recorded=len(a), replayed=len(b), matching=sum(i2 - i1 for op, i1, i2, _, _ in sm.get_opcodes()
                                                                        if op == "equal"),
                         differences=[dict(op=op, recorded=x[:6], replayed=y[:6], at_recorded=i1, at_replayed=j1)
                                      for op, x, y, i1, j1 in diffs[:20]])
        ok = not diffs and i == len(inputs)
    report = dict(ok=ok, inputs=len(inputs), injected=i, screens_recorded=len(want), screens_replayed=len(got),
                  first_divergence=None, input_screen_mismatches=at_input[:20], input_time_mismatches=len(timing),
                  warnings=warn)
    if loose: report["loose"] = loose_rep; report["ok"] = ok
    if rc:
        r = rc.report(inputs)
        with open(reach, "w") as f: json.dump(r, f, indent=1); f.write("\n")
        report["reach"] = dict(file=reach, functions=r["functions"], iram_blocks=r["iram_blocks"],
                               stray_blocks=r["stray_blocks"])
    if first is not None:
        report["first_divergence"] = dict(index=first, recorded=want[first] if first < len(want) else None,
                                          replayed=got[first] if first < len(got) else None)
    return _stack_out(sd, stack, E, ok, report)


def _screen_key(px):
    """What a settled screen is compared by: its pixels, except that a Now Playing screen whose times read is
    keyed by everything above the bar/time rows plus elapsed+remaining (so the second it shows does not count,
    the track length does)."""
    t = read_clock(px)
    if t is None: return hashlib.sha1(bytes(px)).hexdigest()[:12]
    return "np:%s:%d" % (hashlib.sha1(bytes(px[:CLOCK_ROW * W])).hexdigest()[:12], sum(t))


def _replay_sync(rec_dir, drv, inputs, want, end, settled, rec, rc, reach, warn, INPUT_KEYS, got, shift=0):
    """SCREEN-SYNCHRONISED replay, for another image whose timing differs (a C image runs ahead of Apple's
    code). Each input waits at least as long after the previous one as it did in the recording (pauses, long
    presses and idle sleep keep their length). An input the person gave on a SETTLED screen then waits up to
    SYNC_MAX_STEPS more for that exact screen; not reaching it is a divergence, unless the two differ only in
    the Now Playing bar/time rows with the same track length ('clock only'). An input given mid-redraw (the
    screen it saw never settled) is not synchronised: a half-drawn frame depends on timing. The input goes in
    either way. Then the whole sequence of settled screens is compared, Now Playing screens by _screen_key.
    `shift` delays every input by that many ticks (negative: earlier), a control for what timing alone changes."""
    import difflib
    events = [json.loads(l) for l in open(os.path.join(rec_dir, "events.jsonl")) if l.strip()]
    # Only the FIRST input after a screen settled is synchronised: that is the one the person gave in answer
    # to the screen. The rest of a burst (a wheel spin, a key's up edge) came before the firmware had shown
    # anything new, so it keeps its recorded gap; waiting for the old screen again would stretch the burst.
    anchor, last, fresh = [], None, False
    for e in events:
        if e["t"] == "screen": last, fresh = e["hash"], True
        elif e["t"] == "input": anchor.append(fresh and e["screen"] == last); fresh = False
    shots = []
    def run_one():
        shot = drv.step()
        if shot: settled(shot); shots.append(shot)
    waits, clock, diverged = [], [], []
    prev_rec = 0; prev_rep = drv.step_n + shift
    for i, want_in in enumerate(inputs):
        target = prev_rep + (want_in["step"] - prev_rec)
        while drv.step_n < target: run_one()
        extra = 0
        if anchor[i]:
            # a Now Playing screen counts as reached when only the second it shows differs
            png = os.path.join(rec_dir, want_in.get("png") or os.path.join("screens", want_in["screen"] + ".png"))
            try: want_key = _screen_key(read_png(png))
            except (OSError, ValueError, zlib.error): want_key = None
            np = want_key is not None and want_key.startswith("np:")
            while drv.screen.digest() != want_in["screen"] and extra < SYNC_MAX_STEPS:
                if np and _screen_key(drv.screen.px) == want_key: break
                run_one(); extra += 1
            if drv.screen.digest() != want_in["screen"]:
                png = os.path.join(rec_dir, want_in.get("png") or os.path.join("screens", want_in["screen"] + ".png"))
                item = dict(input=i, recorded_step=want_in["step"], step=drv.step_n, recorded=want_in["screen"],
                            replayed=drv.screen.digest(), action={k: want_in[k] for k in INPUT_KEYS if k in want_in})
                try: is_clock = clock_only(read_png(png), drv.screen.px)
                except (OSError, ValueError, zlib.error): is_clock = False
                if rec: item["replayed_png"] = rec._png(drv.screen.digest(), bytes(drv.screen.px))
                (clock if is_clock else diverged).append(item)
        waits.append(extra)
        if rc: rc.credit = i
        prev_rec, prev_rep = want_in["step"], drv.step_n
        done = drv.apply({k: want_in[k] for k in INPUT_KEYS if k in want_in})
        if rec and done: rec.input(done)
        for shot in drv.settled: settled(shot); shots.append(shot)
        drv.settled.clear()
    tail = (end - inputs[-1]["step"]) if inputs else end
    for _ in range(max(tail, 0)): run_one()
    if rec: rec.close(drv)
    # the settled screens, in order, repeats folded, Now Playing by track length rather than the second shown
    def fold(keys):
        out = []
        for k in keys:
            if not out or out[-1] != k: out.append(k)
        return out
    a = fold(_screen_key(read_png(os.path.join(rec_dir, e["png"]))) for e in events if e["t"] == "screen")
    b = fold(_screen_key(s["px"]) for s in shots)
    sm = difflib.SequenceMatcher(None, a, b, autojunk=False)
    seq_diffs = [dict(op=op, recorded=a[i1:i2][:6], replayed=b[j1:j2][:6], at_recorded=i1, at_replayed=j1)
                 for op, i1, i2, j1, j2 in sm.get_opcodes() if op != "equal"]
    ok = not diverged and not seq_diffs
    report = dict(ok=ok, mode="screen-sync", inputs=len(inputs), injected=len(inputs),
                  synchronised_inputs=sum(anchor), mid_redraw_inputs=len(anchor) - sum(anchor),
                  screens_recorded=len(want), screens_replayed=len(got),
                  extra_wait_steps=dict(total=sum(waits), max=max(waits or [0]),
                                        inputs_that_waited=sum(1 for w in waits if w)),
                  clock_only=len(clock), diverged=diverged[:30], diverged_count=len(diverged),
                  clock_only_items=clock[:10],
                  settled_sequence=dict(recorded=len(a), replayed=len(b), differences=seq_diffs[:20],
                                        difference_count=len(seq_diffs)),
                  warnings=warn)
    if rc:
        r = rc.report(inputs)
        with open(reach, "w") as f: json.dump(r, f, indent=1); f.write("\n")
        report["reach"] = dict(file=reach, functions=r["functions"], iram_blocks=r["iram_blocks"],
                               stray_blocks=r["stray_blocks"])
    return ok, report
