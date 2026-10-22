"""Where the emulated iPod's sound goes: the browser (the default) or an output of the machine the web app
runs on. The page's audio switch picks ("audio plays here" / "audio on host"); the recording and the API call
the host side "plugged", after the headphone jack it stands for.

The emulator thread hands every new block of samples to AudioOut.feed(). Each browser listening on /audio
gets them through its own small queue (a slow one loses the oldest, never the emulator's time). With the sound
on the host, a writer thread pipes them to `aplay` (or --audio-cmd), raw s16le stereo at 44.1 kHz. The emulator runs
just ahead of the wall clock, so after each silence the writer first sends PRIME seconds of silence: that is
the buffer the sound then plays from.
"""
import array, collections, glob, json, os, queue, re, shutil, subprocess, threading, time

RATE, FRAME = 44100, 4                          # stereo s16le
BUSY_TRIES = 6                                  # an output another program holds is tried this many times, a second apart, before giving up
IDLE_RELEASE = 8.0                              # seconds of silence after which the host's output is let go (see AudioOut._host)
PRIME = 0.4                                     # seconds of silence before sound that follows a gap (0.2 ran dry
                                                # after a song change, 2026-10-18)
LOUDEST = 123                                   # the firmware's highest headphone volume code (1.5, measured)


def gains(vol):
    """Left and right gains for the codec's headphone volume codes (emu.audio.volume()): 1.0 at the firmware's
    loudest, 1 dB less a step, 0 when muted."""
    return tuple(0.0 if v <= 0x2f else min(1.0, 10 ** ((v - LOUDEST) / 20)) for v in (vol["left"], vol["right"]))


def scale(data, gl, gr):
    """s16le stereo bytes, each channel times its gain."""
    if gl == gr == 1.0: return data
    a = array.array("h", data)
    a[0::2] = array.array("h", (int(x * gl) for x in a[0::2]))
    a[1::2] = array.array("h", (int(x * gr) for x in a[1::2]))
    return a.tobytes()


DEFAULT_CMD = ["aplay", "-q", "-t", "raw", "-f", "S16_LE", "-r", str(RATE), "-c", "2", "-B", "500000"]


def list_devices():
    """The host's audio outputs a person can choose: (backend, [dict(id, label)]). PulseAudio / PipeWire sinks if `pactl` is there, else
    ALSA cards from `aplay -l`; [] and None if neither."""
    if shutil.which("pactl"):
        try:
            out = subprocess.run(["pactl", "list", "sinks"], capture_output=True, text=True, timeout=5).stdout
            sinks, name = [], None
            for line in out.splitlines():
                t = line.strip()
                if t.startswith("Name: "): name = t[6:]
                elif t.startswith("Description: ") and name: sinks.append(dict(id=name, label=t[13:])); name = None
            if sinks: return "pulse", sinks
        except (OSError, subprocess.SubprocessError): pass
    if shutil.which("aplay"):
        try:
            out = subprocess.run(["aplay", "-l"], capture_output=True, text=True, timeout=5).stdout
            devs = []
            for line in out.splitlines():
                m = re.match(r"card (\d+): (\S+) \[(.*?)\], device (\d+): (.*?) \[(.*?)\]", line)
                if m: devs.append(dict(id="plughw:CARD=%s,DEV=%s" % (m.group(2), m.group(4)), label="%s (%s)" % (m.group(3), m.group(6)), card=m.group(1), dev=m.group(4)))
            if devs: return "alsa", devs
        except (OSError, subprocess.SubprocessError): pass
    return None, []


def alsa_holder(card, dev):
    """Who has an ALSA playback device open: dict(pid, name), or None if it is free. The kernel says so in
    /proc/asound/cardN/pcmDp/subS/status (an owner_pid line while it is open), readable by anyone."""
    for st in sorted(glob.glob("/proc/asound/card%s/pcm%sp/sub*/status" % (card, dev))):
        try: txt = open(st).read()
        except OSError: continue
        m = re.search(r"owner_pid\s*:\s*(\d+)", txt)
        if m:
            pid = int(m.group(1))
            try: name = open("/proc/%d/comm" % pid).read().strip()
            except OSError: name = "another program"
            return dict(pid=pid, name=name)
    return None


class AudioOut:
    def __init__(self, plugged=False, cmd=None, store=None):
        self.cmd = list(cmd or DEFAULT_CMD)
        self.custom = bool(cmd)                 # --audio-cmd: the operator's own player, not the dropdown's
        self.store = store                      # where the chosen output is kept, so a restart keeps it
        self.device, self.backend = "", None    # "" is the system default
        self.own_pids = set()                   # the player processes this server started
        try:
            if store and os.path.exists(store): d = json.load(open(store)); self.device, self.backend = d.get("id", ""), d.get("backend")
        except (OSError, ValueError): pass
        self.lock = threading.Lock()
        self.clients = set()
        self.stats = {}                         # client id -> what this server has sent that browser (see client_stats)
        self.plugged = False
        self.error = None
        self.host_q = None
        self.proc = None
        self.frames = 0                         # sound frames the emulator has produced this session
        if plugged: self.plug(True)

    # ------------------------------------------------------------------ from the emulator thread
    def feed(self, data, vol=None):
        """New sound from the emulator; `vol` the codec's headphone volume then (emu.audio.volume()), applied here:
        the samples are what the firmware sent, before the iPod's volume."""
        if not data: return
        if vol is not None: data = scale(data, *gains(vol))
        self.frames += len(data) // FRAME
        with self.lock:
            for q in list(self.clients):
                try: q.put_nowait(data)
                except queue.Full:
                    try:
                        lost = q.get_nowait(); q.put_nowait(data)
                        st = getattr(q, "stats", None)
                        if st is not None: st["dropped"] += len(lost)         # the browser is not taking the sound as fast as it is made
                    except (queue.Empty, queue.Full): pass
            if self.host_q is not None:
                try: self.host_q.put_nowait(data)
                except queue.Full: pass

    # ------------------------------------------------------------------ the jack
    def plug(self, on):
        """Plug in (sound to this machine's output) or unplug (sound to the browsers). Returns the state."""
        with self.lock:
            if on == self.plugged: return self.state()
            if on:
                cmd = self.player_cmd()
                if not shutil.which(cmd[0]):
                    self.error = "%s not found on this machine" % cmd[0]
                    return self.state()
                self.error = None
                self.host_q = q = queue.Queue(maxsize=400)
                threading.Thread(target=self._host, args=(q, cmd), daemon=True).start()
            else:
                q, self.host_q = self.host_q, None
                if q is not None: q.put(None)
            self.plugged = on
            return self.state()

    def state(self):
        return dict(plugged=self.plugged, error=self.error, listeners=len(self.clients), device=self.device)

    # ------------------------------------------------------------------ which output
    def player_cmd(self):
        """The command that plays raw s16le stereo, 44.1 kHz, from its stdin on the chosen output."""
        if self.custom or not self.device: return self.cmd
        if self.backend == "pulse":
            return ["pacat", "--playback", "--device=" + self.device, "--raw", "--format=s16le", "--rate=%d" % RATE, "--channels=2",
                    "--latency-msec=300", "--client-name=emupod"]
        return DEFAULT_CMD[:1] + ["-D", self.device] + DEFAULT_CMD[1:]

    def devices(self):
        backend, devs = list_devices()
        if backend == "alsa":
            for d in devs:
                h = alsa_holder(d.pop("card"), d.pop("dev"))
                if h and h["pid"] in self.own_pids: h = dict(h, name="emupod")
                d["holder"] = h["name"] if h else None
        return dict(backend=backend, devices=devs, current=self.device, custom=self.custom,
                    custom_command=" ".join(self.cmd) if self.custom else None)

    def set_device(self, dev):
        """Choose the host output ("" is the system default). If the sound is going to the host it moves there at once."""
        if self.custom: raise ValueError("the app was started with its own player (--audio-cmd): %s" % " ".join(self.cmd))
        backend, devs = list_devices()
        if dev and dev not in [d["id"] for d in devs]: raise ValueError("no such output: %r" % (dev,))
        was = self.plugged
        if was: self.plug(False)
        self.device, self.backend = dev, backend
        try:
            if self.store:
                os.makedirs(os.path.dirname(self.store), exist_ok=True)
                json.dump(dict(id=dev, backend=backend), open(self.store, "w"))
        except OSError: pass
        if was: self.plug(True)
        return self.devices()

    def _busy_message(self, cmd, err):
        """Why the output would not open, in words: who has it, and what to do."""
        who = None
        try:
            backend, devs = list_devices()
            if backend == "alsa":
                for d in devs:
                    if d["id"] == self.device or not self.device:
                        h = alsa_holder(d["card"], d["dev"])
                        if h: who = h["name"]; break
        except Exception: pass
        return ("The host's audio output is busy%s. An ALSA output serves one program at a time: it is free when that program is idle or paused "
                "(many players can close the output when idle), or choose another output." % (": %s has it" % who if who else ""))

    def _host(self, q, cmd):
        """Pipe the sound to the output program; a gap of more than PRIME/2 ends one stretch of sound. The program is started when
        sound first arrives and let go after IDLE_RELEASE seconds of silence, so the output (a DAC that another player, such as
        Lyrion's squeezelite, also uses) is held only while the iPod makes sound. An output another program holds is retried for a
        few seconds (it may be closing) and then reported with the program's name."""
        proc, primed, last = None, False, time.time()

        def stop():
            nonlocal proc, primed
            primed = False
            if proc is None: return
            p, proc = proc, None
            self.own_pids.discard(p.pid)
            try: p.stdin.close()
            except OSError: pass
            try: p.wait(timeout=2)
            except subprocess.TimeoutExpired: p.kill()

        def give_up(msg):
            self.error = msg
            with self.lock:
                if self.host_q is q: self.host_q = None; self.plugged = False

        def start():
            """Start the player; an output that is busy makes it exit at once. True when it is running."""
            nonlocal proc
            err = ""
            for attempt in range(BUSY_TRIES):
                try: p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                except OSError as e: give_up("%s: %s" % (cmd[0], e)); return False
                time.sleep(0.3)
                if p.poll() is None: proc = p; self.own_pids.add(p.pid); return True
                err = p.stderr.read(400).decode(errors="replace").strip() if p.stderr else ""
                try: p.stdin.close()
                except OSError: pass
                if "busy" not in err.lower(): give_up("%s stopped%s" % (cmd[0], ": " + err if err else "")); return False
                if attempt < BUSY_TRIES - 1: time.sleep(1.0)
            give_up(self._busy_message(cmd, err)); return False

        try:
            while True:
                try: data = q.get(timeout=PRIME / 2)
                except queue.Empty:
                    primed = False
                    if proc is not None and time.time() - last >= IDLE_RELEASE: stop()
                    continue
                if data is None: break
                last = time.time()
                if proc is None and not start(): return
                if not primed: proc.stdin.write(bytes(int(RATE * PRIME) * FRAME)); primed = True
                proc.stdin.write(data)
        except (BrokenPipeError, OSError):
            err = proc.stderr.read(400).decode(errors="replace").strip() if proc is not None and proc.stderr else ""
            give_up(self._busy_message(cmd, err) if "busy" in err.lower() else "%s stopped%s" % (cmd[0], ": " + err if err else ""))
        finally:
            stop()

    # ------------------------------------------------------------------ a browser listening
    def listen(self, write, alive, cid=None):
        """Stream to one browser: write(bytes) sends a chunk; returns when the browser goes, the jack is
        plugged or alive() turns false. The browser schedules the sound itself (it buffers about PRIME).
        `cid`: the browser's id for client_stats()."""
        q = queue.Queue(maxsize=200)
        st = q.stats = dict(sent=0, dropped=0, started=time.time(), hist=collections.deque(maxlen=64), blocked_ms=0.0, blocked_at=0.0)
        if cid: self.stats[cid] = st; st["q"] = q
        with self.lock: self.clients.add(q)
        try:
            idle = 0.0
            while alive() and not self.plugged:
                try: data = q.get(timeout=0.5)
                except queue.Empty:
                    idle += 0.5
                    if idle >= 15: write(bytes(FRAME)); idle = 0.0      # a silent frame: is the browser still there?
                    continue
                idle = 0.0
                while True:                                             # send what has queued up in one chunk
                    try: data += q.get_nowait()
                    except queue.Empty: break
                t0 = time.time(); write(data); t1 = time.time()
                st["sent"] += len(data); st["hist"].append((t1, st["sent"]))
                if t1 - t0 > 0.05: st["blocked_ms"] = max(st["blocked_ms"] * 0.5, (t1 - t0) * 1000); st["blocked_at"] = t1   # the socket would not take it: the link is slow
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            st["ended"] = time.time()
            with self.lock: self.clients.discard(q)

    def client_stats(self, cid):
        """What this server has done for the browser `cid`: bytes written to its socket (sent), bytes dropped because its queue
        overflowed (dropped), what is queued (queued, bytes), the rate it has been written at over the last 3 s (rate_bps), and the
        longest recent write that had to wait for the socket (blocked_ms): the signs of a link too slow for the stream."""
        st = self.stats.get(cid)
        if st is None: return None
        now = time.time(); h = [x for x in st["hist"] if now - x[0] <= 3.0]
        rate = (h[-1][1] - h[0][1]) / max(1e-6, h[-1][0] - h[0][0]) if len(h) > 1 else 0.0
        q = st.get("q")
        queued = sum(len(x) for x in list(q.queue)) if q is not None else 0
        recent = st["blocked_ms"] if now - st["blocked_at"] < 5.0 else 0.0
        return dict(sent=st["sent"], dropped=st["dropped"], queued=queued, rate_bps=round(rate), blocked_ms=round(recent), live="ended" not in st)

    def close(self):
        self.plug(False)
