#!/usr/bin/env python3
"""The offline app's card manager in a real browser: make a card on the "phone" (OPFS), partition it, write a firmware from the
server's cache, boot it in the iPod page, then change the card the iPod is running through the Cards frame (the iPod lets go,
the card is changed, the iPod takes it again).

    python3 tests/offline/cards_flow.py https://HOST:PORT/offline/

Needs Playwright (a Python that has it; emupod itself does not) and the app served over HTTPS (OPFS needs a secure context) by a
web app that has the flashpod firmware cache: it answers /api/firmware/FILE. Exits 1 when a step fails."""
import sys
URL = sys.argv[1] if len(sys.argv) > 1 else "https://localhost:8444/offline/"
import time
from playwright.sync_api import sync_playwright
with sync_playwright() as p:
    import os
    if os.environ.get("CDP"):                                    # a real Chrome to drive instead: CDP=http://localhost:9222 (an Android phone's, over adb forward)
        b=p.chromium.connect_over_cdp(os.environ["CDP"]); ctx=b.contexts[0]; pg=ctx.pages[0]
    else:
        b=p.chromium.launch(); ctx=b.new_context(viewport={"width":420,"height":1100}); pg=ctx.new_page()
    logs=[]; pg.on("pageerror",lambda e: logs.append("PAGEERROR "+str(e))); pg.on("dialog",lambda d: d.accept())
    pg.goto(URL+"cards.html"); pg.wait_for_selector("#newtile",timeout=60000)
    api=lambda fr,method,path,body=None: fr.evaluate("async ([m,p,b])=>{const r=await fetch(p,{method:m,body:b?JSON.stringify(b):undefined});return [r.status,await r.json()]}",[method,path,body])
    def wait(fr,card,t=240):
        for i in range(t):
            pg.wait_for_timeout(500); c=[x for x in api(fr,"GET","/api/cards")[1]["cards"] if x["name"]==card][0]; o=c.get("op") or {}
            if o.get("state") in ("done","error"): return o
    for verb,body in (("create",{"size":300000000}),("partition",{})):
        api(pg,"POST","/api/cards/c3/"+verb,body); print(verb,wait(pg,"c3").get("state"))
    fw=lambda ver:[f["id"] for f in api(pg,"GET","/api/cards")[1]["firmware"] if f["label"].startswith("Apple %s "%ver)][0]
    api(pg,"POST","/api/cards/c3/flash",{"firmware":fw("1.2")}); print("flash 1.2",wait(pg,"c3").get("state"))
    pg.evaluate("localStorage.setItem('emupod.card','c3');localStorage.setItem('emupod.keep','0')")
    pg.goto(URL+"index.html"); pg.wait_for_timeout(6000)
    st=lambda: pg.evaluate("fetch('/state').then(r=>r.json()).then(s=>({phase:s.phase,emu_us:s.emu_us,card:s.card&&s.card.name}))")
    print("iPod booted c3:",st()); 
    pg.click("#cardsbtn"); pg.wait_for_selector("#cardsframe",timeout=10000); pg.wait_for_timeout(3000)
    fr=[f for f in pg.frames if "cards.html" in f.url][0]
    print("cards in frame; c3 booted flag:",[ (c["name"],c["booted"]) for c in api(fr,"GET","/api/cards")[1]["cards"]])
    a=st(); r=api(fr,"POST","/api/cards/c3/then",{"op":"flash","args":{"firmware":fw("1.4")}}); print("then ->",r[0],r[1].get("ok"))
    o=wait(fr,"c3"); print("op after then:",o.get("state"),o.get("error"),str(o.get("result"))[:60])
    pg.wait_for_timeout(4000)
    s1=st(); pg.wait_for_timeout(2000); s2=st(); print("iPod before:",a,"\\n after reopen:",s1,"->",s2,"(advancing: %s)"%(s2["emu_us"]>s1["emu_us"]))
    c=[x for x in api(fr,"GET","/api/cards")[1]["cards"] if x["name"]=="c3"][0]; build=(c["scan"].get("firmware") or {}).get("build"); print("firmware on c3 now:",build)
    ok=build=="stock 1.4" and s2["emu_us"]>s1["emu_us"]; print("OK" if ok else "FAIL"); sys.exit(0 if ok else 1)
    print("\n".join(logs[-4:])); b.close()
