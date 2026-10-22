"""The public emulator interface (emupod.emu) on the Rust core (rust/pp5002, through rust/pybind).

`emu.open(Config(..., backend="rust"))` (or EMUPOD_BACKEND=rust in the environment) returns a RustEmulator: the calls a
live session and the recorder need (boot, tick, keys, the wheel, lines, the LCD log, sound, clicks, memory) with the
Python core's signatures, and NotImplementedError for the rest (hooks, fork, fw_call, ...: they need the Unicorn core).

The proxy turns on the Rust core's compatibility flags (`slice_clock`, `lazy_map`) so that old tests and recordings
see the clock and the audio the old core gave; EMUPOD_RS_V2=1 turns them off (the clean v2 behaviour, see
docs/RUST_CORE_PLAN.md). Build the module with rust/build_py.sh."""
import os, sys

from . import paths

KEYS = ("select", "menu", "play", "ff", "rew")
LINES = ("hold", "firewire", "cna", "battery")
_LINE_PINS = {"firewire": (2, 1 << 4), "cna": (2, 1 << 7), "battery": (2, 1 << 3), "hold": (0, 1 << 5)}
_UNSUPPORTED = ("extra", "legacy", "fat", "fast", "clock", "cpu_model", "patch_reboot", "flashpod", "firewire_cable")


def _module():
    try:
        from . import emupod_rs                      # an installed wheel carries the core inside the package
        return emupod_rs
    except ImportError:
        pass
    d = os.path.dirname(os.path.abspath(__file__))
    for p in (d, os.path.join(d, "..", "rust", "target", "release")):
        sys.path.insert(0, p) if p not in sys.path else None
    try:
        import emupod_rs
    except ImportError as e:
        raise RuntimeError("the Rust core is not built (%s): run rust/build_py.sh" % e)
    return emupod_rs


class _LogView:
    """The LCD controller log as recording.Screen reads it: truthy when there is something new, iterable, and
    `del log[:]` after it. The entries come out of the machine as they are read."""
    def __init__(self, m): self._m = m
    def __bool__(self): return self._m.lcd_pending() > 0
    def __iter__(self): return iter(self._m.lcd_take())
    def __delitem__(self, _): pass


class _RustScreen:
    """recording.Screen with the controller log decoded in Rust: the same px, digest() and packed(), without the Python loop."""
    def __init__(self, m): self._m = m; self._px = bytes(160 * 128); self._digest = None
    def update(self):
        if not self._m.lcd_update(): return False
        self._px = self._m.lcd_px(); self._digest = None
        return True
    @property
    def px(self): return self._px
    def digest(self):
        if self._digest is None:
            import hashlib; self._digest = hashlib.sha1(self._px).hexdigest()[:12]
        return self._digest
    def packed(self):
        p = self._px
        return bytes((p[i] << 6) | (p[i + 1] << 4) | (p[i + 2] << 2) | p[i + 3] for i in range(0, 160 * 128, 4))
    def png(self, scale=3):
        from .recording import png
        return png(self._px, scale)


class _LCD:
    def __init__(self, m):
        self._m = m; self.log = _LogView(m); self._kept = []
    def make_screen(self): return _RustScreen(self._m)
    def write_count(self): return self._m.lcd_count()
    def frame(self):
        from .recording import Screen
        s = Screen(self._kept); s.update(); return bytes(s.px)
    def digest(self):
        from .recording import Screen
        s = Screen(self._kept); s.update(); return s.digest()


class _Audio:
    sample_rate = 44100
    def __init__(self, m): self._m = m
    @property
    def enabled(self): return bool(self._m.audio_on())
    def start_times(self): return list(self._m.audio_start_times())
    def underruns(self): return self._m.underruns()
    def volume(self):
        l, r = self._m.volume()
        db = lambda v: None if v <= 0x2f else v - 0x79
        return dict(left=l, right=r, left_db=db(l), right_db=db(r))
    def take(self): return self._m.audio_take()
    def take_scaled(self, gl, gr): return self._m.audio_take_scaled(gl, gr)


class RustEmulator:
    """The booted (or bootable) iPod on the Rust core. Get it from emu.open()."""

    def __init__(self, config):
        for k in _UNSUPPORTED:
            v = getattr(config, k)
            if v not in (None, False, [], "virtual"):
                raise NotImplementedError("the Rust core does not support Config.%s" % k)
        env = config.env()
        self.config = config
        v2 = os.environ.get("EMUPOD_RS_V2") == "1"
        fpf = config.audio_fpf
        kw = dict(skip_loops=os.environ.get("EMUPOD_RS_NO_SKIP") != "1", trace_io=os.environ.get("EMUPOD_RS_TRACE_IO") == "1", ack_on_read=os.environ.get("EMUPOD_RS_ACK_ON_READ") != "0", irq_mask=os.environ.get("EMUPOD_RS_IRQ_MASK") == "1", keep_cop_reset_state=os.environ.get("EMUPOD_RS_KEEP_COP_STATE") == "1", iram_kb=int(os.environ.get("EMUPOD_RS_IRAM_KB", "0")), persist=config.disk_persist, interleave_playing=config.interleave_playing or 0,
                  keys_up=True if config.keys_up is None else bool(config.keys_up),
                  plugged=bool(config.plugged), idle_sleep=config.idle_sleep, cop=config.cop,
                  start_volume=config.start_volume, slice_clock=not v2, lazy_map=not v2,
                  fpf=0 if fpf == "auto" else int(fpf or 8))
        for k in ("ipus", "cop_ipus", "quantum", "interleave", "interleave_settle"):
            if getattr(config, k) is not None: kw[k] = getattr(config, k)
        a = config.ata
        kw.update(ata_bsy=a.bsy_reads, ata_cmd_us=a.cmd_us, ata_block_us=a.block_us, ata_lba48=bool(a.lba48))
        self._osos, self._disk = os.path.abspath(env["OPENPOD_OSOS"]), os.path.abspath(env["OPENPOD_DISK"])
        self._m = _module().Machine(self._osos, self._disk, **kw)
        self.lcd, self.audio = _LCD(self._m), _Audio(self._m)
        self._core_idle = {}

    # ---------------------------------------------------------------- lifecycle and time
    def boot(self, max_iters=None): return self._m.boot()
    def now_us(self): return self._m.now_us()
    def tick(self): self._m.tick()
    def run_for(self, us):
        end = self.now_us() + us
        while self.now_us() < end: self.tick()
    def run_until(self, cond, max_us=60_000_000):
        end = self.now_us() + max_us
        while self.now_us() < end:
            if cond(self): return True
            self.tick()
        return cond(self)
    def ticks(self): return self._m.ticks()
    def halts(self): return self._m.halts()
    def tick_period_us(self): return self._m.tick_period_us()
    def settle(self, max_iters=300, quiet=12): return self._m.settle(max_iters, quiet)

    # ---------------------------------------------------------------------- input
    def _port(self, key):
        if key not in KEYS: raise ValueError("no such key: %r" % key)
        from .recording import KEYS as _K
        return _K[key]

    def _key(self, key, level):
        port, mask = self._port(key)
        self._m.set_pin(port, mask, bool(level)); self._m.edge(port, mask)

    def key_down(self, key, timing="thumb"):
        if timing == "core":
            port, mask = self._port(key)
            self._core_idle[key] = int(self._m.pin(port, mask))
            self._m.set_pin(port, mask, not self._core_idle[key]); self._m.edge(port, mask)
            for _ in range(8): self.tick()
            return
        if timing != "thumb": raise ValueError("timing: 'thumb' or 'core', not %r" % (timing,))
        self._key(key, 0)

    def key_up(self, key, timing="thumb"):
        if timing == "core":
            port, mask = self._port(key)
            idle = self._core_idle.pop(key, None)
            if idle is None: idle = 1 - int(self._m.pin(port, mask))
            self._m.set_pin(port, mask, bool(idle)); self._m.edge(port, mask)
            for _ in range(8): self.tick()
            return
        if timing != "thumb": raise ValueError("timing: 'thumb' or 'core', not %r" % (timing,))
        self._key(key, 1)

    def key_level(self, key, level, timing="thumb"):
        port, mask = self._port(key)
        self._m.set_pin(port, mask, bool(level)); self._m.edge(port, mask)
        if timing == "core":
            for _ in range(8): self.tick()

    def press(self, key, hold_ticks=8, timing="thumb"):
        self.key_down(key, timing)
        if timing != "core":
            for _ in range(hold_ticks): self.tick()
        self.key_up(key, timing)

    def hold(self, key, us):
        self.key_down(key); self.run_for(us); self.key_up(key)

    def wheel(self, detents, tick=None, timing="thumb"):
        if timing != "thumb": raise NotImplementedError("the Rust core has only the thumb wheel")
        seq = ((0x80, 1), (0x40, 1), (0x80, 0), (0x40, 0)) if detents > 0 else \
              ((0x40, 1), (0x80, 1), (0x40, 0), (0x80, 0))
        for _ in range(abs(detents)):
            for mask, level in seq:
                self._m.set_pin(0, mask, bool(level)); self._m.edge(0, mask); (tick or self.tick)()

    def wheel_edges(self, n, tick=None):
        m = self._m
        for _ in range(abs(n)):
            a, b = int(m.pin(0, 0x80)), int(m.pin(0, 0x40))
            if n > 0: mask, level = {(0, 0): (0x80, 1), (1, 0): (0x40, 1), (1, 1): (0x80, 0), (0, 1): (0x40, 0)}[(a, b)]
            else: mask, level = {(0, 0): (0x40, 1), (0, 1): (0x80, 1), (1, 1): (0x40, 0), (1, 0): (0x80, 0)}[(a, b)]
            m.set_pin(0, mask, bool(level)); m.edge(0, mask); (tick or self.tick)()

    def click_count(self): return self._m.click_count()
    def backlight(self): return self._m.backlight()

    def reset_lines(self, **levels):
        for name, level in levels.items():
            port, mask = _LINE_PINS[name]; self._m.set_pin(port, mask, bool(level))

    def line(self, name, level):
        port, mask = _LINE_PINS[name]
        if self._m.pin(port, mask) == bool(level): return False
        self._m.set_pin(port, mask, bool(level)); return bool(self._m.edge(port, mask))

    def gpio_level(self, port, mask): return int(self._m.pin(port, mask))
    def set_line_quiet(self, name, level):
        port, mask = _LINE_PINS[name]; self._m.set_pin(port, mask, bool(level))
    def line_levels(self): return {k: int(self._m.pin(*_LINE_PINS[k])) for k in LINES}

    # ---------------------------------------------------------------------- memory
    def mem_read(self, addr, n, core="cpu"):
        if core != "cpu": raise NotImplementedError("the Rust core reads memory through the CPU's map only")
        return bytes(self._m.mem_read(addr, n))
    def mem_write(self, addr, data, core="cpu"):
        if core != "cpu": raise NotImplementedError("the Rust core writes memory through the CPU's map only")
        self._m.mem_write(addr, bytes(data))
    def reg(self, name, core="cpu"):
        if core != "cpu": raise NotImplementedError("the Rust core reads the CPU's registers only")
        r = self._m.cpu_regs()
        names = ["r%d" % i for i in range(13)] + ["sp", "lr", "pc", "cpsr"]
        alias = {"r13": "sp", "r14": "lr", "r15": "pc"}
        return r[names.index(alias.get(name.lower(), name.lower()))]

    def effective(self):
        c = self.config; a = c.ata
        return dict(core="rust", osos=self._osos, ipsw=c.ipsw, disk=self._disk, disk_persist=bool(c.disk_persist),
                    fat=None, legacy=False, clock="virtual", ipus=c.ipus or 13, cop_ipus=c.cop_ipus or 13,
                    quantum=c.quantum or 1000, interleave=c.interleave or 20, cop=bool(c.cop), plugged=bool(c.plugged),
                    keys_up=True if c.keys_up is None else bool(c.keys_up), idle_sleep=bool(c.idle_sleep),
                    audio_fpf=c.audio_fpf or "8", extra=[],
                    ata=dict(bsy_reads=a.bsy_reads, cmd_us=a.cmd_us, block_us=a.block_us, lba48=bool(a.lba48) or None))

    def stats(self):
        """Cumulative counters (iPod time, instructions per core, disk commands and sectors, screen writes, sound interrupts, ticks, clicks),
        the base of the web app's iPod Statistics."""
        return dict(self._m.stats())

    def events(self, kind):
        if kind == "click": return list(self._m.click_times())
        raise NotImplementedError("events(%r) needs the Unicorn core" % (kind,))

    def __getattr__(self, name):
        if name in ("fork", "fw_call", "add_hook", "remove_hook", "scratch", "where", "disk", "cpu", "cop", "selected",
                    "screen", "item_count", "choose", "play", "play_first", "irq_mask_writes", "cop_alive", "set_reg"):
            raise NotImplementedError("Emulator.%s needs the Unicorn core (backend='python')" % name)
        raise AttributeError(name)
