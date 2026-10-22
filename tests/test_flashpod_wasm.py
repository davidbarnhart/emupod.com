#!/usr/bin/env python3
"""flashpod's Rust card library, built for WebAssembly (rust/flashpod-wasm), against the same library built natively (its `fpcore` binary).

    python3 tests/test_flashpod_wasm.py          (skipped if the wasm, fpcore, node, ffmpeg or a firmware is missing)

Both make a small card from nothing the same way (create, flash a firmware, init, add three synthetic songs) with the same ids and clock
(FLASHPOD_SEED / FLASHPOD_NOW and the command's "seed"/"now"), and the two images must be identical, byte for byte. flashpod's own harness
(scripts/rust_parity.py in the flashpod repo) proves the native library against the Python one; this proves the wasm build and its glue.
Build: cd rust && cargo build --release --target wasm32-unknown-unknown -p flashpod-wasm; fpcore: see docs/OFFLINE_IOS.md."""
import hashlib, os, shutil, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WASM = os.path.join(ROOT, "rust/target/wasm32-unknown-unknown/release/flashpod_wasm.wasm")
FPCORE = os.path.join(ROOT, "rust/target/release/fpcore")
FW = os.path.expanduser("~/.cache/flashpod/firmware/iPod_1.1.5_2005_02_18.bin.gz")
SEED, NOW, SIZE = "12345", "1760000000", 150_000_000


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for c in iter(lambda: f.read(1 << 20), b""): h.update(c)
    return h.hexdigest()


def main():
    need = [(WASM, "the wasm"), (FPCORE, "fpcore"), (FW, "a firmware in flashpod's cache")]
    for p, what in need:
        if not os.path.exists(p): print("skipped: %s is missing (%s)" % (what, p)); return
    if not (shutil.which("node") and shutil.which("ffmpeg")): print("skipped: needs node and ffmpeg"); return
    with tempfile.TemporaryDirectory() as t:
        songs = []
        for hz, name in ((440, "A"), (523, "B"), (660, "C")):
            f = os.path.join(t, name + ".mp3"); songs.append(f)
            subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=%d:duration=6" % hz, "-ar", "44100", "-ac", "2", "-b:a", "128k",
                            "-metadata", "title=Song " + name, "-metadata", "artist=Tester", "-metadata", "album=Tones", f], check=True)
        native, wasm = os.path.join(t, "native.img"), os.path.join(t, "wasm.img")
        env = dict(os.environ, FLASHPOD_SEED=SEED, FLASHPOD_NOW=NOW)
        for argv in ([FPCORE, "create-blank", native, str(SIZE)], [FPCORE, "flash", native, FW], [FPCORE, "init", native],
                     [FPCORE, "add", native, "--no-expand"] + songs):
            r = subprocess.run(argv, env=env, capture_output=True, text=True)
            if r.returncode: sys.exit("FAIL: %s\n%s%s" % (" ".join(argv[:3]), r.stdout, r.stderr))
        r = subprocess.run(["node", os.path.join(ROOT, "tests/offline/flashpod_run.mjs"), WASM, FW, wasm, SEED, NOW, str(SIZE)] + songs, capture_output=True, text=True)
        if r.returncode: sys.exit("FAIL: the wasm run died\n%s%s" % (r.stdout, r.stderr))
        print("wasm:", r.stdout.strip())
        a, b = sha(native), sha(wasm)
        print("native image", a[:16], "| wasm image", b[:16], "|", os.path.getsize(native), os.path.getsize(wasm), "bytes")
        if a != b: sys.exit("FAIL: the wasm build made a different card")
    print("OK: the WebAssembly build of flashpod's Rust library makes the same card as the native build, byte for byte")


main()
