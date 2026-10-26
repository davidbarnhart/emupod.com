#!/usr/bin/env python3
"""Does a firmware hold the disk-manager client while Refresh Library crawls?

    python3 tests/test_diskmgr_hold.py OSOS_IPSW CARD.img [--expect hold|nohold]

The boot's own hold on the disk (client 1, released after 15 s) covers a crawl that starts soon after the boot, so the two firmware
look the same then. This idles 20 s at the main menu first, so the boot's hold has expired, then runs Refresh Library and reads every
message the disk manager sends to mailbox 5 (KS_send 0x2800de20, at any alias, with r0 == 5; the type word is at message + 0x18): type 2 is "no
client active, the drive may stop", type 4 is "a client is active". A firmware that holds a client through the crawl sends only 4.
Exit 0 when the result is what --expect says (default: hold; nohold is for the test3 image, the one that stalled on hardware).

The firmware is flashed into a COPY of CARD (flashpod's flash_firmware, from --flashpod or ~/source/flashpod-main); CARD is not
changed. Synthetic cards only (openpod's tools/refresh/make_big_card.py). Skipped (exit 0) when an input is missing."""
import os, shutil, sys, tempfile
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
KS_SEND, MBOX_DISKMGR_CLIENT_STOP = 0x2800de20, 5
ENTRY, LCD_UPDATE_RECT = 0x2837a140, 0x28007014               # Refresh Library's crawler, and the screen update it ends with
alias = lambda a: [a - 0x28000000, a - 0x24000000, a]         # DRAM shows at 0x0, 0x4000000 and 0x28000000; the firmware runs from any
fold = lambda a: a % 0x04000000 + 0x28000000
ALIASES = alias(KS_SEND)


def main(argv):
    args = [a for a in argv if not a.startswith("--")]
    expect = argv[argv.index("--expect") + 1] if "--expect" in argv else "hold"
    if expect in args: args.remove(expect)
    if len(args) < 2 or not all(os.path.exists(a) for a in args[:2]):
        print("skipped: needs a firmware (.ipsw) and a synthetic card image"); return 0
    fp = os.path.expanduser("~/source/flashpod-main")
    if not os.path.isdir(fp): print("skipped: needs flashpod's Python library at", fp); return 0
    sys.path.insert(0, fp)
    from flashpod import card as fpcard
    from emupod import emu, recording as R
    with tempfile.TemporaryDirectory() as t:
        img = os.path.join(t, "card.img"); shutil.copyfile(args[1], img)
        fpcard.flash_firmware(img, args[0])
        E = emu.open(emu.Config(backend="rust", disk=img, osos="card", **R.LIVE_TIMING))
        d = R.Driver(E); d.boot()
        run = lambda n: [d.step() for _ in range(n)]
        def press(k): E.key_down(k); run(8); E.key_up(k); run(60)
        press("select")                                           # the language, then the main menu
        t0 = E.now_us()
        while E.now_us() - t0 < 20_000_000: run(60)               # idle: the boot's hold (client 1, 15 s) runs out
        E.wheel_edges(40, tick=d.step); run(40)                   # down to Refresh Library, the last item
        m = E._e._m if hasattr(E, "_e") else E._m
        m.hook_start(ALIASES + alias(ENTRY) + alias(LCD_UPDATE_RECT), 1, 0x18)                          # r1 = the message; its type word is at +0x18
        press("select")
        t1 = E.now_us()
        while E.now_us() - t1 < 25_000_000: run(60)               # the crawl takes about 14 s of the iPod's time
        log = m.hook_log()
        entry = [tt for tt, pc, r, v in log if fold(pc) == ENTRY]
        if not entry: print("FAIL: the crawler was never entered"); return 1
        end = [tt for tt, pc, r, v in log if fold(pc) == LCD_UPDATE_RECT and tt >= entry[0] and ENTRY <= fold(r[14]) < ENTRY + 0x8000]
        t_in, t_out = entry[0], (end[0] if end else E.now_us())
        sends = [(tt, r[0], v) for tt, pc, r, v in log if pc in ALIASES and r[0] == MBOX_DISKMGR_CLIENT_STOP and t_in <= tt <= t_out]
        print("the crawl: %.2f s to %.2f s of the iPod's time" % (t_in / 1e6, t_out / 1e6))
        types = {}
        for _, _, v in sends: types[v] = types.get(v, 0) + 1
        print("%s: %d messages to mailbox 5 during the crawl, by type word: %s" % (os.path.basename(args[0]), len(sends), types))
        held = bool(sends) and set(types) == {4}
        verdict = "holds a client through the crawl (only type 4)" if held else "does NOT hold a client (types %s)" % sorted(types)
        ok = held == (expect == "hold")
        print(("OK: " if ok else "FAIL: ") + verdict + ("" if ok else "; expected " + expect))
        return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
