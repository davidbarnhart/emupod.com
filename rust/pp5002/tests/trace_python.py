#!/usr/bin/env python3
"""The Python core's checkpoints, in the format of src/bin/trace.rs, for comparison.

    python3 pp5002/tests/trace_python.py <script> <every>

The script is the one src/bin/trace.rs replays (tests/mkscript.py makes it): `<step> edges <n>`,
`<step> key <name> down|up`, `end <step>`."""
import os, struct, sys, zlib

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
sys.path.insert(0, ROOT)
from emupod import emu

script, every = sys.argv[1], int(sys.argv[2])
events, end = [], 0
for l in open(script):
    w = l.split()
    if len(w) == 2 and w[0] == "end": end = int(w[1])
    elif len(w) >= 3 and w[0].isdigit(): events.append((int(w[0]), w[1], w[2:]))
e = emu.open(emu.Config())
c = e._c


class Drv:
    """What recording.Driver does with these events, without its Screen (which consumes the LCD log we checksum)."""
    step_n = 0

    def step(self):
        e.tick(); self.step_n += 1

    def inject(self, ev):
        if ev["do"] == "edges": e.wheel_edges(ev["n"], tick=self.step)
        elif ev["phase"] == "down": e.key_down(ev["key"])
        else: e.key_up(ev["key"])

    def boot(self): e.boot()


drv = Drv()
lcd_seen = snd_seen = 0
lcd_crc = snd_crc = 0


def checkpoint(tag, full):
    global lcd_seen, lcd_crc, snd_seen, snd_crc
    log = e.lcd.log
    for a, v in log[lcd_seen:]:
        lcd_crc = zlib.crc32(struct.pack("<I", a), lcd_crc)
        lcd_crc = zlib.crc32(struct.pack("<I", v & 0xffffffff), lcd_crc)
    lcd_seen = len(log)
    s = e.audio.samples()
    if len(s) > snd_seen:
        snd_crc = zlib.crc32(s[snd_seen:].tobytes(), snd_crc)
        snd_seen = len(s)
    vol = e.audio.volume()
    regs = " ".join("%08x" % e.reg("r%d" % i) for i in range(13)) + " %08x %08x" % (e.reg("sp"), e.reg("lr"))
    line = "%s now=%d lcd=%d lcdcrc=%08x ticks=%d sleeps=%d pc=%08x cpsr=%08x cop_asleep=%d snd=%d sndcrc=%08x on=%d fiqs=%d hp=%d,%d %s" % (
        tag, e.now_us(), e.lcd.write_count(), lcd_crc, e.ticks(), e.halts(), e.reg("pc"), e.reg("cpsr"), int(c._cop["asleep"]),
        len(s) // 2, snd_crc, int(e.audio.enabled), c._audio["fiqs"], vol["left"], vol["right"], regs)
    if full:
        line += " dram=%08x iram=%08x" % (zlib.crc32(e.mem_read(0, 0x2000000)), zlib.crc32(e.mem_read(0x40000000, 0x100000)))
    print(line, flush=True)


drv.boot()
checkpoint("boot", True)
i = 0
while drv.step_n <= end:
    while i < len(events) and events[i][0] == drv.step_n:
        _, kind, arg = events[i]
        if kind == "edges": drv.inject(dict(do="edges", n=int(arg[0])))
        else: drv.inject(dict(do="key", key=arg[0], phase=arg[1]))
        i += 1
    drv.step()
    if drv.step_n % every == 0:
        checkpoint("t%d" % drv.step_n, drv.step_n % (every * 10) == 0)
