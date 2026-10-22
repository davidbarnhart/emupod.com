#!/usr/bin/env python3
"""The offline app's card manager in a real browser (Chromium through Playwright): make a card, partition it, write a firmware from the
server and one the person adds, upload songs and add them, list them, duplicate, rename, delete, reload (the cards must still be there),
boot a card and rescan it while the iPod runs it.

    python3 tests/offline/cards_manage.py https://HOST:PORT/offline/

Needs Playwright, ffmpeg, flashpod's firmware cache (~/.cache/flashpod/firmware) and the app over HTTPS from a web app with /api/firmware.
CDP=http://localhost:9222 drives an existing Chrome (the emulated Android phone) instead. Exits 1 on the first failed check."""
import base64, os, shutil, subprocess, sys, tempfile, time
from playwright.sync_api import sync_playwright

URL = sys.argv[1] if len(sys.argv) > 1 else "https://localhost:8444/offline/"
FW = os.path.expanduser("~/.cache/flashpod/firmware/iPod_1.1.5_2005_02_18.bin.gz")
fails = []


def check(ok, what):
    print(("ok   " if ok else "FAIL ") + what)
    if not ok: fails.append(what)


def main():
    if not (shutil.which("ffmpeg") and os.path.exists(FW)): print("skipped: needs ffmpeg and flashpod's cached firmware"); return
    with tempfile.TemporaryDirectory() as t:
        songs = []
        for hz, name in ((440, "Tone A"), (523, "Tone B"), (660, "Tone C")):
            f = os.path.join(t, name + ".mp3"); songs.append(f)
            subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=%d:duration=8" % hz, "-ar", "44100", "-ac", "2", "-b:a", "128k",
                            "-metadata", "title=" + name, "-metadata", "artist=Tester", f], check=True)
        fwbytes = base64.b64encode(open(FW, "rb").read()).decode()
        with sync_playwright() as p:
            if os.environ.get("CDP"): b = p.chromium.connect_over_cdp(os.environ["CDP"]); ctx = b.contexts[0]; pg = ctx.pages[0]
            else: b = p.chromium.launch(); ctx = b.new_context(viewport={"width": 420, "height": 1000}); pg = ctx.new_page()
            pg.on("dialog", lambda d: d.accept())
            pg.goto(URL + "cards.html"); pg.wait_for_selector("#newtile", timeout=60000)
            api = lambda m, path, body=None: pg.evaluate("async ([m,p,b])=>{const r=await fetch(p,{method:m,body:b?JSON.stringify(b):undefined});return [r.status,await r.json()]}", [m, path, body])

            def op(card, verb, body=None):
                s, j = api("POST", "/api/cards/%s/%s" % (card, verb), body or {})
                if s != 200: return {"state": "error", "error": j.get("error")}
                for _ in range(300):
                    cs = [c for c in api("GET", "/api/cards")[1]["cards"] if c["name"] == card or c["name"] == (body or {}).get("to")]
                    o = [c["op"] for c in cs if c.get("op") and c["op"]["op"] == verb]
                    if o and o[-1]["state"] != "running": return o[-1]
                    pg.wait_for_timeout(200)
                return {"state": "timeout"}

            def names(): return sorted(c["name"] for c in api("GET", "/api/cards")[1]["cards"])
            def card(n): return [c for c in api("GET", "/api/cards")[1]["cards"] if c["name"] == n][0]
            fwid = lambda ver: [f["id"] for f in api("GET", "/api/cards")[1]["firmware"] if f["label"].startswith("Apple %s " % ver)][0]

            check(op("a1", "create", {"size": 200000000})["state"] == "done", "create a 200 MB card")
            check(op("a1", "create", {"size": 200000000})["state"] == "error", "creating it again is refused")
            check(op("a1", "partition")["state"] == "done", "partition and format (firmware from the server)")
            check((card("a1")["scan"]["firmware"] or {}).get("build") == "stock 1.5", "the card reads back as Apple 1.5")
            check(op("a1", "flash", {"firmware": fwid("1.3")})["state"] == "done" and (card("a1")["scan"]["firmware"] or {}).get("build") == "stock 1.3", "write firmware 1.3")
            for f in songs:
                data = base64.b64encode(open(f, "rb").read()).decode()
                s = pg.evaluate("async ([b,n])=>{const bin=Uint8Array.from(atob(b),c=>c.charCodeAt(0));const r=await fetch('/api/cards/a1/upload?file='+encodeURIComponent(n),{method:'PUT',body:new Blob([bin])});return r.status}", [data, os.path.basename(f)])
                check(s == 200, "upload " + os.path.basename(f))
            o = op("a1", "add", {"kind": "uploads"}); check(o["state"] == "done" and len(o["result"]["added"]) == 3, "add the three songs: %s" % (o.get("error") or len(o["result"]["added"])))
            tr = api("GET", "/api/cards/a1/tracks")[1]["tracks"]; check(sorted(x["title"] for x in tr) == ["Tone A", "Tone B", "Tone C"], "the song list: %s" % [x["title"] for x in tr])
            check(card("a1")["uploads"] == [], "the uploads are used up")
            o = op("a1", "remove", {"ids": [tr[0]["id"]]}); check(o["state"] == "done" and len(api("GET", "/api/cards/a1/tracks")[1]["tracks"]) == 2, "remove a song")
            o = op("a1", "rebuild"); check(o["state"] == "done" and len(api("GET", "/api/cards/a1/tracks")[1]["tracks"]) == 2, "rebuild the library from the card's files")
            # a firmware file of the person's own (the cached .bin.gz stands in for an ipsw)
            r = pg.evaluate("async b=>{const bin=Uint8Array.from(atob(b),c=>c.charCodeAt(0));const r=await fetch('/api/firmware?file=mine.bin.gz',{method:'PUT',body:new Blob([bin])});return [r.status,await r.json()]}", fwbytes)
            check(r[0] == 200, "add a firmware file of my own: %s" % r[1])
            bad = pg.evaluate("async ()=>{const r=await fetch('/api/firmware?file=junk.ipsw',{method:'PUT',body:new Blob([new Uint8Array(5000)])});return r.status}"); check(bad == 400, "junk is refused as firmware")
            mine = [f["id"] for f in api("GET", "/api/cards")[1]["firmware"] if f["id"].startswith("file:")]
            check(op("a1", "flash", {"firmware": mine[0]})["state"] == "done" and (card("a1")["scan"]["firmware"] or {}).get("build") == "stock 1.5", "write the firmware I added")
            check(op("a1", "duplicate", {"to": "a2"})["state"] == "done" and names() == ["a1", "a2"], "duplicate")
            check(len(api("GET", "/api/cards/a2/tracks")[1]["tracks"]) == 2, "the copy has the songs")
            check(op("a1", "rename", {"to": "a3"})["state"] == "done" and names() == ["a2", "a3"], "rename")
            check(op("a3", "rename", {"to": "a2"})["state"] == "error", "renaming onto an existing card is refused")
            check(op("a2", "delete", {"confirm": "a2"})["state"] == "done" or names() == ["a3"], "delete")
            check(names() == ["a3"], "one card left")
            pg.reload(); pg.wait_for_selector("#newtile", timeout=60000); pg.wait_for_timeout(1500)
            check(names() == ["a3"] and len(api("GET", "/api/cards/a3/tracks")[1]["tracks"]) == 2, "after a reload the card and its songs are still there")
            # boot it and rescan it from the Cards frame while the iPod runs it
            pg.evaluate("localStorage.setItem('emupod.card','a3');localStorage.setItem('emupod.keep','0')"); pg.goto(URL + "index.html"); pg.wait_for_timeout(6000)
            st = lambda: pg.evaluate("fetch('/state').then(r=>r.json()).then(s=>({phase:s.phase,emu_us:s.emu_us}))")
            s1 = st(); pg.click("#cardsbtn"); pg.wait_for_selector("#cardsframe"); pg.wait_for_timeout(2500)
            fr = [f for f in pg.frames if "cards.html" in f.url][0]
            fapi = lambda m, path, body=None: fr.evaluate("async ([m,p,b])=>{const r=await fetch(p,{method:m,body:b?JSON.stringify(b):undefined});return [r.status,await r.json()]}", [m, path, body])
            fapi("POST", "/api/cards/a3/scan", {})
            for _ in range(50):
                c = [x for x in fapi("GET", "/api/cards")[1]["cards"] if x["name"] == "a3"][0]
                if (c.get("op") or {}).get("state") in ("done", "error"): break
                pg.wait_for_timeout(200)
            check(c["op"]["state"] == "done", "rescan the card the iPod runs: %s" % c["op"].get("error"))
            pg.wait_for_timeout(1500); s2 = st(); check(s2["emu_us"] > s1["emu_us"], "the iPod kept running")
            b.close()
    if fails: sys.exit("FAILED: %d check(s)" % len(fails))
    print("OK: the card manager works on the Rust library")


main()
