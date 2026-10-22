#!/usr/bin/env python3
"""The iPod core as WebAssembly (rust/wasm, driven by emupod/offline/ipod_core.js) against the native Rust core.

    python3 tests/test_offline.py [--flashpod DIR]   (skipped without node, the built wasm, flashpod or ffmpeg)

Builds a scratch card with flashpod (Apple's 1.5 and three synthetic songs, never anyone's music), then plays the menus
and a song on both: under Node (tests/offline/run.mjs, the wasm core on an in-memory disk) and natively through
emupod.emu (backend rust, the clean v2 settings). The clock, every sample of sound, the clicks, the volumes and the screen
must be identical. Build the wasm: cargo build --release --target wasm32-unknown-unknown -p emupod-wasm (in rust/).
"""
import os, shutil, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FP = sys.argv[sys.argv.index("--flashpod") + 1] if "--flashpod" in sys.argv else os.path.expanduser("~/source/flashpod-main")
WASM = os.path.join(ROOT, "rust/target/wasm32-unknown-unknown/release/emupod_wasm.wasm")
REF = r'''
import sys, zlib, os
sys.path.insert(0, %r)
os.environ["EMUPOD_RS_V2"] = "1"
from emupod import emu, recording as R
E = emu.open(emu.Config(backend="rust", disk=sys.argv[1], osos="card", interleave_playing=300, interleave_settle=200_000,
                        audio_fpf="auto", cop_ipus=26, start_volume=102))
d = R.Driver(E); d.boot()
ev = []
for l in open(%r):
    w = l.split()
    if len(w) >= 3 and w[0].isdigit(): ev.append((int(w[0]), w[1], w[2:]))
i = crc = n = 0; vols = set()
while d.step_n < 2400:
    while i < len(ev) and ev[i][0] <= d.step_n:
        _, k, a = ev[i]; i += 1
        d.apply({"do": "key", "key": a[0], "phase": a[1]} if k == "key" else {"do": "edges", "n": int(a[0])})
    d.step()
    b = E.audio.take(); crc = zlib.crc32(b, crc); n += len(b)
    v = E.audio.volume(); vols.add("%%d,%%d" %% (v["left"], v["right"]))
print("step", d.step_n, "now", E.now_us(), "audio", n, "crc", "%%08x" %% crc, "clicks", E.click_count(), "vols", " ".join(sorted(vols)), d.screen.digest())
''' % (ROOT, os.path.join(ROOT, "rust/pp5002/tests/play.script"))


def main():
    if not (shutil.which("node") and shutil.which("ffmpeg") and os.path.isdir(FP) and os.path.exists(WASM)):
        print("skipped: needs node, ffmpeg, flashpod and the built wasm"); return
    sys.path.insert(0, FP)
    from flashpod import card as fp
    with tempfile.TemporaryDirectory() as t:
        songs = os.path.join(t, "songs"); os.makedirs(songs)
        for k, hz in enumerate((440, 523, 660)):
            subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=%d:duration=20" % hz, "-ar", "44100", "-ac", "2",
                            "-b:a", "128k", "-metadata", "title=Tone %d" % hz, "-metadata", "artist=Test", "-metadata", "album=Tones",
                            os.path.join(songs, "tone%d.mp3" % hz)], check=True)
        img = os.path.join(t, "card.img")
        fp.create_blank(img, 200 * 10 ** 6)
        e = next(e for e in fp.firmware_catalog() if e.get("osos_sha256", "").startswith("98251add"))
        fp.flash(img, fp.fetch_firmware(e)); fp.init(img, "iPod")
        fp.add(img, [os.path.join(songs, f) for f in sorted(os.listdir(songs))])
        py = subprocess.run([sys.executable, "-c", REF, img], capture_output=True, text=True)
        if py.returncode: print(py.stderr[-1500:]); sys.exit("FAIL: the native run died")
        js = subprocess.run(["node", os.path.join(ROOT, "tests/offline/run.mjs"), img], capture_output=True, text=True)
        if js.returncode: print(js.stderr[-1500:]); sys.exit("FAIL: the wasm run died")
        a, b = py.stdout.strip().splitlines()[-1], " ".join(js.stdout.strip().splitlines()[-1].split()[:-1])
        print("native:", a); print("wasm:  ", b)
        if a != b: sys.exit("FAIL: the wasm core ended in a different state")
        print("OK: the WebAssembly core matches the native core: clock, sound, clicks, volumes and screen")


main()
