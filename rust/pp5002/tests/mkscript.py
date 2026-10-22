#!/usr/bin/env python3
"""Make an input script that plays the first song: drive the menus on the Python core with its own feedback
(selected()), and write each input at the tick it was applied. Format, one per line:
    <step> edges <n> | <step> key <name> down|up | end <step>
The script then replays the same on both machines (trace_python.py, src/bin/trace.rs).

    python3 mkscript.py > play.script [play_ticks]
"""
import os, sys
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
sys.path.insert(0, ROOT)
from emupod import emu, recording as R

play_ticks = int(sys.argv[1]) if len(sys.argv) > 1 else 1500
e = emu.open(emu.Config())
drv = R.Driver(e)
drv.boot()
out = []


def inject(ev):
    if ev["do"] == "key":
        out.append("%d key %s %s" % (drv.step_n, ev["key"], ev["phase"]))
    else:
        out.append("%d edges %d" % (drv.step_n, ev["n"]))
    drv.inject(ev)


def idle(n):
    for _ in range(n): drv.step()


def press(key):
    inject(dict(do="key", key=key, phase="down")); idle(8); inject(dict(do="key", key=key, phase="up")); idle(60)


def goto(index):
    for _ in range(60):
        s = e.selected()
        if s == index or s < 0: return
        inject(dict(do="edges", n=6 if s < index else -6)); idle(30)


idle(100)
PATH = (0, 0, 1, 1, 1, 0)       # Language, iPod: Music, Music: Artists, the artist, the album, the song
for k, index in enumerate(PATH):
    goto(index); press("select"); idle(200 if k < 5 else 100)
idle(play_ticks)
out.append("end %d" % drv.step_n)
print("\n".join(out))
print("audio on: %s, frames: %d" % (e.audio.enabled, len(e.audio.samples()) // 2), file=sys.stderr)
