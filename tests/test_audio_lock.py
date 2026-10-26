#!/usr/bin/env python3
"""The server keeps answering while the iPod plays and pages listen and poll (emupod web). Needs Apple's firmware, which this repository
does not carry, so it runs only when given it (and skips otherwise):

    EMUPOD_IPSW15=iPod_1.1.5.ipsw python3 tests/test_audio_lock.py [SECONDS]

A synthetic card (tools/synth_card.py) gets one generated tone (ffmpeg's sine, 3 minutes) through the server's own card API; the iPod
plays it; then four /audio listeners read the sound (reconnecting under the same id every few seconds, as a page does) while sixteen
pollers ask /state and /ping as fast as they get answers, and the buttons rewind and pause and play again every few seconds, for SECONDS
(default 90). Every answer must come within 10 s, the sound must keep arriving, and the server must stop when asked.

What it guards: the sound's three locks (the clients' list, each listener's queue, the page state) taken in a cycle froze the server
the first time real music played with a page open (2026-10-24): the emulator thread held the list and waited for a queue, a listener
held its queue and waited for the state, /state held the state and waited for the list.
"""
import json, os, shutil, socket, subprocess, sys, tempfile, threading, time, urllib.request
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "rust", "target-own"))
SLOTTED = os.path.join(ROOT, "tools", "run_slotted.py")
WRAP = [sys.executable, SLOTTED, "--"] if os.path.exists(SLOTTED) else []
LISTENERS, POLLERS = 4, 16


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def main():
    ipsw = os.environ.get("EMUPOD_IPSW15")
    if not ipsw or not os.path.exists(ipsw) or not shutil.which("ffmpeg"):
        print("skipped: set EMUPOD_IPSW15 to an iPod_1.1.5.ipsw (and have ffmpeg)"); return 0
    seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 90
    exe = os.environ.get("EMUPOD_EXE")
    if not exe:
        subprocess.run(["cargo", "build", "--release", "--manifest-path", os.path.join(ROOT, "rust", "server", "Cargo.toml")], check=True,
                       capture_output=True, env={**os.environ, "CARGO_TARGET_DIR": TARGET})
        exe = os.path.join(TARGET, "release", "emupod")
    bad = 0

    def check(name, ok, detail=""):
        nonlocal bad
        print(("ok   " if ok else "FAIL ") + name + (": " + detail if detail else ""), flush=True); bad += not ok

    with tempfile.TemporaryDirectory() as tmp:
        cards, music = os.path.join(tmp, "cards"), os.path.join(tmp, "music")
        os.makedirs(cards); os.makedirs(music)
        card = os.path.join(cards, "tone.img")
        subprocess.run([sys.executable, os.path.join(ROOT, "tools", "synth_card.py"), ipsw, card], check=True, capture_output=True)
        builtin = os.path.join(tmp, "builtin.img")
        shutil.copyfile(card, builtin)
        subprocess.run(["ffmpeg", "-loglevel", "error", "-f", "lavfi", "-i", "sine=frequency=440:duration=180", "-ac", "2", "-b:a", "128k",
                        "-metadata", "title=Tone", "-metadata", "artist=Test", os.path.join(music, "tone.mp3")], check=True)
        port = free_port()
        log = open(os.path.join(tmp, "server.log"), "w")
        p = subprocess.Popen(WRAP + [exe, "web", "--host", "127.0.0.1", "--port", str(port), "--cards", cards, "--nbd", "off",
                                     "--demo-card", builtin, "--music-root", music, "--card", "Rainbow Road"],
                             stdout=log, stderr=subprocess.STDOUT, env={**os.environ, "XDG_DATA_HOME": tmp})
        url = "http://127.0.0.1:%d" % port

        def get(path, timeout=10):
            with urllib.request.urlopen(url + path, timeout=timeout) as r:
                return json.loads(r.read())

        def post(path, body):
            req = urllib.request.Request(url + path, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=30) as r:
                return json.loads(r.read())

        def wait(pred, what, timeout=120):
            end = time.time() + timeout
            while time.time() < end:
                try:
                    v = pred()
                    if v:
                        return v
                except OSError:
                    pass
                time.sleep(0.5)
            raise TimeoutError(what)

        def key(k):
            post("/cmd", {"do": "key", "key": k, "phase": "down"})
            time.sleep(0.3)
            post("/cmd", {"do": "key", "key": k, "phase": "up"})
            time.sleep(0.8)

        try:
            wait(lambda: get("/state?wait=0").get("phase") == "running", "boot")
            finished = lambda: (lambda o: o if o and o.get("state") != "running" else None)(get("/api/cards/tone").get("op"))
            post("/api/cards/tone/init", {})
            done = wait(finished, "the init")
            check("the card gets an iTunesDB (init)", done.get("state") == "done", json.dumps(done)[:200])
            post("/api/cards/tone/add", {"kind": "music", "paths": ["/"]})
            done = wait(lambda: (lambda o: o if o.get("op") == "add" else None)(finished() or {}), "the add")
            check("the card gets the tone through the server's card API", done.get("state") == "done", json.dumps(done)[:200])
            post("/api/boot", {"card": "tone"})
            wait(lambda: (lambda s: s["phase"] == "running" and s["card"]["name"] == "tone" and s.get("screen"))(get("/state?wait=0")), "boot the tone card")
            time.sleep(3)
            if os.environ.get("LOCK_CABLE"):
                # first a FireWire cable in and out: the machine is swapped for the chip's and back (David's freeze came after one)
                post("/cmd", {"do": "line", "name": "cna", "level": 0})
                wait(lambda: get("/state?wait=0")["fwdisk"]["mode"] == "ready", "disk mode", 60)
                post("/cmd", {"do": "line", "name": "cna", "level": 1})
                wait(lambda: (lambda s: s["fwdisk"]["mode"] == "normal" and s.get("screen"))(get("/state?wait=0")), "back from disk mode", 60)
                time.sleep(5)
                check("a FireWire cable in and out first", True)
            shots = os.environ.get("LOCK_SHOTS")

            def shot(tag):
                if shots:
                    with urllib.request.urlopen(url + "/screen.png", timeout=10) as r, open(os.path.join(shots, tag + ".png"), "wb") as f:
                        f.write(r.read())
            shot("0-menu")
            # a new card asks for the language first (English, the first); then the main menu's Shuffle Songs, three items down
            key("select"); shot("1-main")
            # (the wheel speeds up, so: to the end, Backlight, then up one item: the screen's first change)
            post("/cmd", {"do": "wheel", "n": 20}); time.sleep(0.8)
            last = get("/state?wait=0")["screen"]
            for _ in range(12):
                post("/cmd", {"do": "wheel", "n": -1}); time.sleep(0.5)
                if get("/state?wait=0")["screen"] != last:
                    break
            shot("2-shuffle")
            key("select"); shot("3-play")
            s = wait(lambda: (lambda s: s if s.get("np") and s.get("audio") else None)(get("/state?wait=0")), "the tone playing", 30)
            check("the iPod plays the tone", True, "now playing %s" % s["np"])
            # David's freeze: about 35 s into the song, rewind held
            wait(lambda: (get("/state?wait=0").get("np") or [0])[0] >= 35, "35 s of play", 60)
            post("/cmd", {"do": "key", "key": "rew", "phase": "down"}); time.sleep(2)
            post("/cmd", {"do": "key", "key": "rew", "phase": "up"}); time.sleep(2)
            s = get("/state?wait=0")
            check("at 35 s, rewind held 2 s: the server answers and the song goes on", bool(s.get("np")), "now playing %s" % s.get("np"))

            stop = threading.Event()
            heard = [0] * LISTENERS
            slowest = [0.0]
            errors = []

            def listen(i):
                # a page's stream: it reconnects under the same id now and then (a reload, a stalled stream), every 1-3 s here
                while not stop.is_set():
                    try:
                        with urllib.request.urlopen(url + "/audio?cid=lock%d" % i, timeout=10) as r:
                            until = time.time() + 1 + (i % 3)
                            while not stop.is_set() and time.time() < until:
                                b = r.read(4096)
                                if not b:
                                    break
                                heard[i] += len(b)
                    except Exception as e:
                        if not stop.is_set():
                            errors.append("listener %d: %r" % (i, e))
                            return

            def poll(i):
                # as pages do: /ping with the page's stream id (its stream's figures), and long polls of the state that wait for a change
                # (every waiting poll wakes at once when a new frame is published)
                since, ver = "", ""
                while not stop.is_set():
                    t = time.time()
                    try:
                        if i % 2:
                            get("/ping?cid=lock%d" % (i % LISTENERS))
                            limit = 10
                        else:
                            v = get("/state?since=%s&ver=%s&wait=5" % (since, ver), timeout=15)
                            since, ver = v.get("screen") or "", str(v.get("ver"))
                            limit = 15
                    except Exception as e:
                        errors.append("poller %d: %r" % (i, e))
                        return
                    took = time.time() - t
                    slowest[0] = max(slowest[0], took)
                    if took > limit:
                        errors.append("poller %d: %.1f s for an answer" % (i, took))
                        return

            def hands():
                # what a person does while it plays: rewind (hold the key), pause and play again; each stops and restarts the sound
                k = 0
                while not stop.is_set():
                    try:
                        if k % 2 == 0:
                            post("/cmd", {"do": "key", "key": "rew", "phase": "down"}); time.sleep(1.5)
                            post("/cmd", {"do": "key", "key": "rew", "phase": "up"})
                        else:
                            key("play"); time.sleep(1.0); key("play")
                    except Exception as e:
                        errors.append("buttons: %r" % e)
                        return
                    k += 1
                    time.sleep(2)

            ts = [threading.Thread(target=listen, args=(i,), daemon=True) for i in range(LISTENERS)]
            ts.append(threading.Thread(target=hands, daemon=True))
            ts += [threading.Thread(target=poll, args=(i,), daemon=True) for i in range(POLLERS)]
            for t in ts:
                t.start()
            t0, mid = time.time(), None
            while time.time() - t0 < seconds and not errors:
                time.sleep(1)
                if mid is None and time.time() - t0 > seconds / 2:
                    mid = list(heard)
            end = list(heard)
            stop.set()
            check("for %.0f s of play, every /state (long polls, 5 s) and /ping is answered in time" % seconds, not errors,
                  "; ".join(errors[:3]) or "slowest %.2f s" % slowest[0])
            check("... and every listener keeps hearing the sound", mid is not None and all(e > m for e, m in zip(end, mid)),
                  "bytes heard %s (at half time %s)" % (end, mid))
        except Exception as e:
            check("ran", False, repr(e))
        finally:
            p.terminate()
            try:
                p.wait(20)
                stopped = True
            except subprocess.TimeoutExpired:
                p.kill()
                stopped = False
            check("the server stops when asked (SIGTERM)", stopped)
            log.close()
            if bad:
                print(open(log.name).read()[-2000:])
    print("PASS" if not bad else "FAIL: %d" % bad)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
