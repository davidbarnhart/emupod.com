#!/usr/bin/env python3
"""Build the offline iPod: a folder of static files (default work/offline/) that runs the whole emulator in a browser.

    python3 tools/build_offline.py [OUT]

index.html is the web app's own page (emupod.web.PAGE) with shim.js in front of it, so the page's code runs as it is
and the shim answers what the server would. Needs the wasm: cd rust && cargo build --release --target
wasm32-unknown-unknown -p emupod-wasm. Serve OUT over HTTPS (or from localhost): service workers and OPFS need it.
"""
import os, re, shutil, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)


def main(out):
    from emupod import web
    wasm = os.path.join(ROOT, "rust/target/wasm32-unknown-unknown/release/emupod_wasm.wasm")
    if not os.path.exists(wasm): sys.exit("build the wasm first (see the docstring)")
    os.makedirs(out, exist_ok=True)
    src = os.path.join(ROOT, "emupod/offline")
    for f in ("shim.js", "worker.js", "store.js", "ipod_core.js", "sw.js", "manifest.webmanifest", "cardsvc.js", "cardmgr.js", "flashpod.js", "cardshost.js", "cardsrpc.js", "probe.html"):
        shutil.copy(os.path.join(src, f), os.path.join(out, f))
    cards_app(out)
    build = stamp(out)
    shutil.copy(wasm, os.path.join(out, "emupod_wasm.wasm"))
    shutil.copy(os.path.join(ROOT, "emupod/clock_glyphs.json"), os.path.join(out, "clock_glyphs.json"))
    page = web.PAGE
    # the page's absolute addresses become relative, so the app can live in any folder
    for a, b in (("'/silence.wav", "'./silence.wav"), ("'/screen.png", "'./screen.png"), ('href="/cards"', 'href="cards.html"'),
                 ("history.replaceState(null,'','/')", "history.replaceState(null,'','index.html')")):
        if a not in page: sys.exit("the page changed: %r not found" % a)
        page = page.replace(a, b)
    page = page.replace("<title>emupod</title>", '<title>emupod</title><link rel="manifest" href="manifest.webmanifest">'
                        '<link rel="apple-touch-icon" href="icon-180.png"><meta name="apple-mobile-web-app-title" content="emupod"><style>#aout{display:none}</style>'          # one device: no choice between "here" and "on the host"
                        '<script src="shim.js"></script><script src="cardshost.js"></script>', 1)
    if "--demo-site" in sys.argv: page = demo_page(page)
    open(os.path.join(out, "index.html"), "w").write(page)
    shutil.copy(os.path.join(ROOT, "emupod/site/chicagoflf.ttf"), os.path.join(out, "chicagoflf.ttf"))      # the buttons' font (the page asks for it)
    for n in (180, 192, 512): shutil.copy(os.path.join(src, "icon-%d.png" % n), os.path.join(out, "icon-%d.png" % n))
    shutil.copy(os.path.join(src, "icon-180.png"), os.path.join(out, "icon.png"))
    if "--demo-site" in sys.argv:
        for f in ("install.html", "sortsmillgoudy.woff2", "OFL-SortsMillGoudy.txt"): shutil.copy(os.path.join(ROOT, "emupod/site", f), os.path.join(out, f))
    if "--demo-card" in sys.argv: demo_card(out, sys.argv[sys.argv.index("--demo-card") + 1])
    elif "--no-stock" not in sys.argv: stock(out)
    # the page loops a silent clip under its real sound (it keeps the phone's audio session alive); the service worker makes a long one once it
    # is active, but the very first visit asks before that: a short real file, so there is no 404
    import wave
    with wave.open(os.path.join(out, "silence.wav"), "wb") as w: w.setnchannels(1); w.setsampwidth(2); w.setframerate(8000); w.writeframes(b"\0\0" * 8000)
    print("build", build)
    print("built %s: %d files, %.1f MB" % (out, len(os.listdir(out)), sum(os.path.getsize(os.path.join(out, f)) for f in os.listdir(out)) / 1e6))


def cards_app(out):
    """The card manager on the phone: the hosted Cards page as cards.html (it finds its /api in cardsrpc.js), and the card store
    (cardmgr.js, in a Worker) over flashpod's Rust library built for WebAssembly (flashpod_wasm.wasm, from rust/flashpod-wasm)."""
    from emupod import cards_page
    wasm = os.path.join(ROOT, "rust/target/wasm32-unknown-unknown/release/flashpod_wasm.wasm")
    if not os.path.exists(wasm): sys.exit("build flashpod's wasm first: cd rust && cargo build --release --target wasm32-unknown-unknown -p flashpod-wasm")
    shutil.copy(wasm, os.path.join(out, "flashpod_wasm.wasm"))
    page = cards_page.CARDS_PAGE
    mark = '<script>\n"use strict";'
    if mark not in page or "<title>emupod cards</title>" not in page: sys.exit("the cards page changed: its script start or title was not found")
    page = page.replace("<title>emupod cards</title>", '<title>emupod cards</title><link rel="manifest" href="manifest.webmanifest"><link rel="apple-touch-icon" href="icon-180.png">', 1)
    page = page.replace(mark, '<script src="cardshost.js"></script><script src="cardsrpc.js"></script>\n' + mark, 1)
    open(os.path.join(out, "cards.html"), "w").write(page)


def stamp(out):
    """A build id (a hash of the app's own files, and the page) into shim.js and sw.js: a new build is a new service worker, which
    replaces the phone's cached files, and a report from a phone says which build it was."""
    import hashlib, time
    h = hashlib.sha1()
    for f in sorted(os.listdir(os.path.join(ROOT, "emupod/offline"))):
        h.update(open(os.path.join(ROOT, "emupod/offline", f), "rb").read())
    for f in ("web.py", "cards_page.py"): h.update(open(os.path.join(ROOT, "emupod", f), "rb").read())
    build = time.strftime("%m%d-%H%M-") + h.hexdigest()[:6]
    for f in ("shim.js", "sw.js"):
        p = os.path.join(out, f); t = open(p).read()
        open(p, "w").write(t.replace("__BUILD__", build))
    return build


def demo_page(page):
    """The public demo: the player on a white page under an "emuPod" heading (Sorts Mill Goudy; every button, here and in the Session sheet, in ChicagoFLF), with "Run your own" at the left of the bottom row (where Cards
    was) and Session at the right, both a little inside the iPod's edges. "Run your own" opens the install page as a sheet over the player: the iPod
    keeps running and playing underneath (a link to another page would stop it)."""
    css = ("<style>"
           "@font-face{font-family:'Sorts Mill Goudy';src:url(sortsmillgoudy.woff2) format('woff2');font-weight:400;font-display:block}"
           "@font-face{font-family:'ChicagoFLF';src:url(chicagoflf.ttf) format('truetype');font-weight:400;font-display:block}"
           ":root{color-scheme:light;--bg:#fff;--fg:#000;--mute:#6c6c72;--card:#f2f2f4;--line:#d4d4d8;--acc:#0a66c2;--chrome:126px;--ipw:min(calc(100vw - 20px),430px,calc((100dvh - var(--chrome) - env(safe-area-inset-top))/1.65))}"
           "@media (max-height:620px){:root{--chrome:117px}.bar{padding-top:10px!important}}"
           ".ipod{--w:var(--ipw)!important}"                                          # a larger player: the heading and the button row take less room than the page they replaced
           "html.light,html.light body{background:#fff!important;color:#000!important}"
           "html.dark,html.dark body{background:#000!important;color:#ececee!important}"
           "#brand{width:var(--ipw);height:54px;display:flex;align-items:flex-end;margin:calc(6px + env(safe-area-inset-top)) auto 0;padding-left:calc(var(--ipw) * .07);box-sizing:border-box;text-align:left}"
           "#brand span{display:inline-block;position:relative;top:4px}"        # its baseline level with the bottom of the Hold switch (measured: 10px above it otherwise)
           "#brand{"
           "color:var(--fg);font:400 52px/1 'Sorts Mill Goudy',Georgia,serif;letter-spacing:-.01em}"   # the iPod's width, left-aligned 7% of the way in
           ".ipod{margin-top:10px!important}"      # a shadow only below: a player standing on a white table under an overhead light                                                                          # the same gap as between the iPod and the buttons below it
           ".bar{max-width:none!important;padding-left:0!important;padding-right:0!important;justify-content:space-between;color:var(--fg);"
           "width:calc(var(--ipw) - 20px);padding-top:14px!important}"             # a little inside the iPod's edges, a little lower
           ".bar .sp,#status,#devbtn,#cardsbtn{display:none!important}"
           ".bar .pill{background:var(--card);color:var(--fg);box-shadow:0 0 0 1px var(--line) inset;font:400 15px 'ChicagoFLF',Charcoal,sans-serif}"
           ".sheet .btn,.sheet .pill{font-family:'ChicagoFLF',Charcoal,sans-serif!important;font-weight:400!important;font-size:15px!important}"   # the Session sheet's buttons too
           ".notice{top:12px!important}"
           "#getsheet{position:fixed;inset:0;z-index:200;background:#fff;display:flex;flex-direction:column}#getsheet[hidden]{display:none}"
           "#getclose{align-self:flex-end;margin:calc(10px + env(safe-area-inset-top)) 12px 2px;border:0;border-radius:10px;height:34px;padding:0 16px;"
           "background:#f2f2f4;color:#000;font:400 15px 'ChicagoFLF',Charcoal,sans-serif;box-shadow:0 0 0 1px #d4d4d8 inset;cursor:pointer}"
           "#getframe{flex:1;border:0;width:100%;background:#fff}"
           "</style>")
    sheet = ('<div id="getsheet" hidden><button id="getclose" type="button">Close</button><iframe id="getframe" title="Run your own emupod"></iframe></div>'
             "<script>(function(){var sh=document.getElementById('getsheet'),fr=document.getElementById('getframe');"
             "function close(){sh.hidden=true}"
             "document.getElementById('runown').addEventListener('click',function(e){e.preventDefault();if(!fr.getAttribute('src'))fr.src='install.html';sh.hidden=false});"
             "document.getElementById('getclose').addEventListener('click',close);"
             "addEventListener('message',function(e){if(e.data==='emupod-close')close()})})()</script>")
    if "<body>" not in page or "</body>" not in page: sys.exit("the page changed: no <body>")
    run = '<a class="pill" id="runown" href="install.html">Run your own &rarr;</a>'
    page, n = re.subn(r'(<a class="pill" id="cardsbtn")', run.replace("\\", "\\\\") + r"\1", page, count=1)
    if n != 1: sys.exit("the page changed: no Cards button")
    # both fonts fetched first thing, and the heading kept hidden (font-display:block) until its own is in: no flash of a stand-in font
    if "</head>" not in page: sys.exit("the page changed: no </head>")
    page = page.replace("</head>", '<link rel="preload" href="sortsmillgoudy.woff2" as="font" type="font/woff2" crossorigin>'
                        '<link rel="preload" href="chicagoflf.ttf" as="font" type="font/ttf" crossorigin></head>', 1)
    page = page.replace("<body>", "<body>" + css + '<div id="brand"><span>emuPod</span></div>', 1)
    return page.replace("</body>", sheet + "</body>", 1)


def demo_card(out, img):
    """The demo's built-in card: IMG (flashpod 1.6 and the demo song, nothing of Apple's) gzip'd as stock.img.gz, which the app installs at the
    first run. `--demo-card IMG [--demo-name NAME]`."""
    import gzip, json
    name = sys.argv[sys.argv.index("--demo-name") + 1] if "--demo-name" in sys.argv else "Demo"
    with open(img, "rb") as src, open(os.path.join(out, "stock.img.gz"), "wb") as raw, \
            gzip.GzipFile(filename="", mode="wb", compresslevel=9, fileobj=raw, mtime=0) as dst: shutil.copyfileobj(src, dst)   # the same bytes every build
    import hashlib
    rev = hashlib.sha256(open(img, "rb").read()).hexdigest()[:12]            # a visitor's browser installs the card again when this changes
    json.dump(dict(name=name, file="stock.img.gz", size=os.path.getsize(img), rev=rev, note="The demo card."), open(os.path.join(out, "stock.json"), "w"))
    print("demo card: %.1f MB compressed (%.0f MB card)" % (os.path.getsize(os.path.join(out, "stock.img.gz")) / 1e6, os.path.getsize(img) / 1e6))


def stock(out):
    """The stock card for local testing: Apple's 1.5 on a minimum-size card with three synthetic 20-second songs, gzip'd as
    stock.img.gz (+ stock.json) beside the app, which installs it at the first run. It carries Apple's firmware: it is for
    testing on this network only, never to be published (docs/OFFLINE_IOS.md); --no-stock leaves it out."""
    import gzip, json, shutil, subprocess, tempfile
    fpdir = os.path.expanduser("~/source/flashpod-main")
    if not (os.path.isdir(fpdir) and shutil.which("ffmpeg")):
        print("no stock card: needs flashpod and ffmpeg"); return
    sys.path.insert(0, fpdir)
    from flashpod import card as fp
    with tempfile.TemporaryDirectory() as t:
        songs = []
        for hz, name in ((440, "Test Tone A"), (523, "Test Tone C"), (660, "Test Tone E")):
            f = os.path.join(t, name + ".mp3"); songs.append(f)
            subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=%d:duration=20,volume=0.5" % hz, "-ar", "44100",
                            "-ac", "2", "-b:a", "128k", "-metadata", "title=" + name, "-metadata", "artist=emupod", "-metadata", "album=Test Tones", f], check=True)
        img = os.path.join(t, "stock.img")
        size = -(-fp.min_card_bytes() // 512) * 512
        fp.create_blank(img, size)
        e = next(e for e in fp.firmware_catalog() if e.get("osos_sha256", "").startswith("98251add"))      # Apple's 1.5
        fp.flash(img, fp.fetch_firmware(e)); fp.init(img, "iPod"); fp.add(img, songs)
        with open(img, "rb") as src, open(os.path.join(out, "stock.img.gz"), "wb") as raw, \
                gzip.GzipFile(filename="", mode="wb", compresslevel=9, fileobj=raw, mtime=0) as dst: shutil.copyfileobj(src, dst)   # the same bytes every build
    json.dump(dict(name="Stock 1.5", file="stock.img.gz", size=size, note="Apple's 1.5 with three test songs (for local testing)."),
              open(os.path.join(out, "stock.json"), "w"))
    print("stock card: %.1f MB compressed (%.0f MB card)" % (os.path.getsize(os.path.join(out, "stock.img.gz")) / 1e6, size / 1e6))


main(sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "work/offline"))
