"""The web app's card store: card images in one directory, managed through flashpod's library.

Every operation on a card is flashpod.card's (create, flash, flash firmware only, format, init, rebuild, scan,
list/remove/add songs); this module only keeps the directory, runs one operation per card at a time on a
worker thread with its progress, and confines what a request can name:

* a card is NAME.img in the card directory (NAME: letters, digits, space, . _ -), with NAME.json beside it
  holding the last scan (kept by the image's size and mtime, so a busy card still shows its last state);
* songs come from the card's upload folder (DIR/.uploads/NAME/, what the page sent) or from a folder under
  the music root given at start (--music-root); nothing outside those two and the card directory is read;
* firmware is a flashpod catalog entry, or an .ipsw / firmware file in one of the firmware folders given at
  start (--firmware-dir), e.g. a cimage.

flashpod is imported from --flashpod (a clean checkout of its main, ~/source/flashpod-main by default); its
API major version must be the one this was written for. Nothing here writes a device: the burn command is
only shown (flashpod burn, run by David himself).
"""
import json, os, re, shutil, sys, threading, time, traceback

AUDIO_EXT = (".mp3", ".m4a", ".m4b", ".aac", ".wav", ".aif", ".aiff", ".flac")
NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9 ._-]{0,63}$")
API_MAJOR = "1"
DEFAULT_DIR = os.path.expanduser("~/emupod-cards")
DEFAULT_FLASHPOD = os.environ.get("EMUPOD_FLASHPOD", "")      # "": the installed flashpod


class CardError(Exception):
    """A request the store refuses, with a message for the page."""


class Cards:
    def __init__(self, root=DEFAULT_DIR, flashpod=DEFAULT_FLASHPOD, music_root=None, firmware_dirs=()):
        self.root = os.path.abspath(root)
        self.music_root = os.path.abspath(music_root) if music_root else None
        self.firmware_dirs = [os.path.abspath(d) for d in firmware_dirs if d]
        os.makedirs(self.root, exist_ok=True)
        if flashpod and flashpod not in sys.path: sys.path.insert(0, flashpod)
        from flashpod import card                       # noqa: E402  (flashpod's library, its own checkout)
        self.fp = card
        v = card.version()
        if str(v.get("api", "")).split(".")[0] != API_MAJOR:
            raise RuntimeError("flashpod API %s at %s; the card page was written for %s.x" % (v.get("api"), flashpod, API_MAJOR))
        self.fp_version = v
        self.ops = {}                                   # name -> the running or last operation's state
        self.lock = threading.Lock()
        self.booted = None                              # name of the card the emulator runs, set by the web app
        self.booted_keep = None                         # whether its writes are kept on the card

    # ------------------------------------------------------------------ the stock card
    STOCK = "stock"
    STOCK_BYTES = 10 * 10 ** 9
    STOCK_OSOS = "98251add80f99732142532814c4ad5b95ee08ab844dd988458a6fa254e1c7c97"     # Apple's 1.5

    def card_names(self):
        names = [f[:-4] for f in sorted(os.listdir(self.root)) if f.endswith(".img") and NAME_RE.match(f[:-4])]
        return sorted(names, key=lambda n: (n != self.STANDARD, n))                # the standard disk first

    # David, 2026-10-18: the standard disk is a tiny read-only card (Apple's 1.5, an empty library), listed with the
    # others. The iPod boots it when no other card is asked for; its writes are never kept; the only things that can
    # be done to it are looking (scan, plan) and copying it (duplicate) to get a card that can be changed.
    STANDARD = "Standard disk"
    STANDARD_BYTES = 195313 * 512                                      # 100.0 MB (decimal), a whole number of sectors
    READONLY_OPS = ("scan", "plan", "duplicate")

    def readonly(self, name): return name == self.STANDARD

    def ensure_standard(self):
        """Make the standard disk if it is missing (100 MB, about 32 MB on disk). Returns its name,
        or None if it could not be made (the app then runs the old disk)."""
        final = self.path(self.STANDARD, must_exist=False)
        size = max(self.STANDARD_BYTES, -(-self.fp.min_card_bytes() // 512) * 512)
        if os.path.exists(final):
            if os.path.getsize(final) == size: return self.STANDARD
            os.chmod(final, 0o644); os.unlink(final)                   # a managed file of another size: made again
        try:
            path = os.path.join(self.root, ".standard.building.img")
            if os.path.exists(path): os.unlink(path)
            self.fp.create_blank(path, size)
            entry = next(e for e in self.fp.firmware_catalog() if e.get("osos_sha256") == self.STOCK_OSOS)
            self.fp.flash(path, self.fp.fetch_firmware(entry)); self.fp.init(path, "iPod")
            info = self.fp.inspect(path).to_dict()
            if not (info.get("layout_ok") and (info.get("itunesdb") or {}).get("state") == "ok"):
                raise CardError("the standard disk does not check out: %s" % (info.get("problems"),))
            os.chmod(path, 0o444)
            os.rename(path, final)
            try: self.scan(self.STANDARD)
            except Exception: pass
            return self.STANDARD
        except Exception as e:
            print("[cards] cannot make the standard disk: %s: %s" % (type(e).__name__, e), flush=True)
            return None

    def ensure_stock(self, wait=False):
        """David, 2026-10-16: with NO card in the store, make one called "stock": a 10 GB card with Apple's 1.5 and
        an empty iTunesDB. With any card present, do nothing; never overwrite. Runs as the card's operation (the
        page shows its progress); `wait` blocks until it ends. Returns the op, or None if not needed."""
        if [n for n in self.card_names() if n != self.STANDARD]: return None
        r = self.start(self.STOCK, "stock", {})
        if wait:
            while self.ops[self.STOCK]["state"] == "running": time.sleep(0.2)
        return self.ops[self.STOCK]

    def _op_stock(self, name, a, progress, cancelled):
        final = self.path(name, must_exist=False)
        if os.path.exists(final): raise CardError("%s exists: not overwritten" % name)
        # Built under a hidden name (not a card name, so never listed) and renamed into place only once it checks
        # out: a restart part way leaves no half-made "stock" that would stop it being made again.
        path = os.path.join(self.root, ".stock.building.img")
        if os.path.exists(path): os.unlink(path)          # this method's own leftover from an interrupted run
        progress("create"); self.fp.create_blank(path, self.STOCK_BYTES)
        entry = next(e for e in self.fp.firmware_catalog() if e.get("osos_sha256") == self.STOCK_OSOS)
        progress("firmware"); fw = self.fp.fetch_firmware(entry, progress=lambda d, t: progress("download", d, t))
        progress("flash"); self.fp.flash(path, fw)
        progress("init"); self.fp.init(path, "iPod")
        info = self.fp.inspect(path).to_dict()                  # check it is what it should be
        f, db = info.get("firmware") or {}, info.get("itunesdb") or {}
        ok = (f.get("osos") or {}).get("sha256") == self.STOCK_OSOS and db.get("state") == "ok" and db.get("tracks") == 0
        if not ok: raise CardError("the stock card does not check out: firmware %s, iTunesDB %s" % (f.get("build"), db))
        if os.path.exists(final): raise CardError("%s appeared meanwhile: not overwritten" % name)
        os.rename(path, final)
        return dict(created=name, firmware=f.get("build"), tracks=0)

    # ------------------------------------------------------------------ the last booted card (David, 2026-10-19)
    def _last_file(self): return os.path.join(self.root, ".last-card.json")

    def save_last(self, name, keep):
        """Remember the card the iPod booted (and whether its writes are kept): the next start boots it."""
        try:
            tmp = self._last_file() + ".tmp"
            with open(tmp, "w") as f: json.dump(dict(card=name, keep=bool(keep), at=time.time()), f)
            os.replace(tmp, self._last_file())
        except OSError: pass

    def load_last(self):
        """dict(card, keep) of the last booted card if it is still in the store, else None."""
        try:
            d = json.load(open(self._last_file()))
            if isinstance(d.get("card"), str) and os.path.isfile(self.path(d["card"], must_exist=False)): return d
        except (OSError, ValueError, CardError): pass
        return None

    # ------------------------------------------------------------------ names and paths
    def path(self, name, must_exist=True):
        if not isinstance(name, str) or not NAME_RE.match(name) or name.endswith(".img"):
            raise CardError("not a card name: %r" % (name,))
        p = os.path.join(self.root, name + ".img")
        if must_exist and not os.path.isfile(p): raise CardError("no card named %r" % name)
        return p

    def uploads(self, name):
        return os.path.join(self.root, ".uploads", name)

    def _sidecar(self, name): return os.path.join(self.root, name + ".json")

    # ------------------------------------------------------------------ listing and scans
    def list(self):
        out = []
        for name in self.card_names(): out.append(self.summary(name))
        return dict(root=self.root, cards=out, flashpod=self.fp_version, booted=self.booted, booted_keep=self.booted_keep,
                    music_root=self.music_root, firmware=self.firmware_choices())

    def summary(self, name):
        p = self.path(name)
        op = self.ops.get(name)
        op = dict(op) if op else None                  # the operation FIRST: one that reads as done has already written its scan, so the details read next are its own
        st = os.stat(p)
        side = self._read_side(name)
        fresh = side and side.get("size") == st.st_size and side.get("mtime") == st.st_mtime
        return dict(name=name, size=st.st_size, on_disk=st.st_blocks * 512, mtime=st.st_mtime,
                    scan=side.get("scan") if side else None, scan_fresh=bool(fresh),
                    scanned=side.get("scanned") if side else None, op=op,
                    booted=self.booted == name, uploads=self._upload_list(name), readonly=self.readonly(name))

    def _read_side(self, name):
        try: return json.load(open(self._sidecar(name)))
        except (OSError, ValueError): return None

    def scan(self, name):
        """flashpod's inspect(), cached beside the image. A busy card (the emulator holds it) keeps its last."""
        p = self.path(name)
        try: info = self.fp.inspect(p).to_dict()
        except self.fp.CardBusy:
            s = self.summary(name); s["busy"] = True; return s
        st = os.stat(p)
        tmp = self._sidecar(name) + ".tmp"
        with open(tmp, "w") as f:
            json.dump(dict(size=st.st_size, mtime=st.st_mtime, scanned=time.time(), scan=info), f)
        os.replace(tmp, self._sidecar(name))
        return self.summary(name)

    def tracks(self, name):
        try: return self.fp.tracks(self.path(name))
        except self.fp.FlashpodError as e: raise CardError(_msg(e))

    # ------------------------------------------------------------------ firmware and music sources
    def firmware_choices(self):
        out = [dict(id="catalog:" + e["file"], label="%s %s (%s)%s" % (e.get("maker") or "Apple", e.get("version"), e.get("build_date"),
                    "" if e.get("local") else ", downloads"), default=bool(e.get("default")))
               for e in self.fp.firmware_catalog() if "1G" in (e.get("models") or [])]
        for d in self.firmware_dirs:                    # the folder and one level below it, no deeper
            for root, dirs, files in os.walk(d):
                for f in sorted(files):
                    if f.endswith((".ipsw", ".bin.gz")) or f.startswith("Firmware"):
                        full = os.path.join(root, f)
                        out.append(dict(id="file:" + full, label=os.path.relpath(full, d) + " (" + os.path.basename(d) + ")"))
                dirs[:] = [x for x in sorted(dirs) if not x.startswith(".")] if root == d else []
        return out

    def _firmware(self, fid, progress=None):
        if isinstance(fid, str) and fid.startswith("catalog:"):
            return self.fp.fetch_firmware(fid[8:], progress=progress)
        if isinstance(fid, str) and fid.startswith("file:"):
            p = os.path.abspath(fid[5:])
            if not any(p.startswith(d + os.sep) for d in self.firmware_dirs) or not os.path.isfile(p):
                raise CardError("not an offered firmware file")
            return p
        raise CardError("choose a firmware")

    def music(self, rel=""):
        """One level of the music root: folders and audio files, for the page's folder picker."""
        base = self._music_path(rel)
        dirs, files = [], []
        for f in sorted(os.listdir(base), key=str.lower):
            if f.startswith("."): continue
            full = os.path.join(base, f)
            if os.path.isdir(full): dirs.append(f)
            elif f.lower().endswith(AUDIO_EXT): files.append(f)
        return dict(root=self.music_root, path=os.path.relpath(base, self.music_root).replace(".", "", 1) if base == self.music_root else os.path.relpath(base, self.music_root),
                    dirs=dirs, files=files)

    def _music_path(self, rel):
        if not self.music_root: raise CardError("no music folder was given to the web app (--music-root)")
        p = os.path.realpath(os.path.join(self.music_root, rel or ""))
        if p != self.music_root and not p.startswith(self.music_root + os.sep): raise CardError("outside the music folder")
        if not os.path.exists(p): raise CardError("no such folder")
        return p

    def _sources(self, name, src):
        """The paths an add takes: the card's uploads, or folders/files under the music root."""
        if src.get("kind") == "uploads":
            d = self.uploads(name)
            if not os.path.isdir(d) or not os.listdir(d): raise CardError("nothing uploaded for this card")
            return [d]
        if src.get("kind") == "music":
            paths = [self._music_path(r) for r in (src.get("paths") or [])]
            if not paths: raise CardError("choose a folder or files")
            return paths
        if src.get("kind") == "both":                    # what was uploaded from the phone and what was chosen on the host
            paths = [self._music_path(r) for r in (src.get("paths") or [])]
            d = self.uploads(name)
            if os.path.isdir(d) and os.listdir(d): paths.insert(0, d)
            if not paths: raise CardError("no songs chosen")
            return paths
        raise CardError("no songs chosen")

    def _upload_list(self, name):
        d = self.uploads(name)
        try: return sorted(os.listdir(d))
        except OSError: return []

    def upload(self, name, filename, stream, length, limit=2 * 1024 ** 3):
        """Write one uploaded file into the card's upload folder (name sanitised; at most `limit` bytes)."""
        self.path(name)
        if self.readonly(name): raise CardError("the standard disk is read-only: duplicate it to get a card you can change")
        base = os.path.basename(filename or "").strip()
        base = re.sub(r"[^A-Za-z0-9 ._()&,'+-]", "_", base)[:120]
        if not base or base.startswith("."): raise CardError("not a file name")
        if not base.lower().endswith(AUDIO_EXT): raise CardError("not an audio file the iPod can play (%s)" % ", ".join(AUDIO_EXT))
        if length > limit: raise CardError("too large")
        d = self.uploads(name); os.makedirs(d, exist_ok=True)
        tmp = os.path.join(d, "." + base + ".part")
        left = length
        with open(tmp, "wb") as f:
            while left > 0:
                b = stream.read(min(left, 1 << 20))
                if not b: break
                f.write(b); left -= len(b)
        if left: os.unlink(tmp); raise CardError("upload cut short")
        os.replace(tmp, os.path.join(d, base))
        return dict(ok=True, file=base, uploads=self._upload_list(name))

    def clear_uploads(self, name):
        self.path(name)
        shutil.rmtree(self.uploads(name), ignore_errors=True)      # the page's own uploads for this card
        return dict(ok=True)

    # ------------------------------------------------------------------ operations (worker threads)
    def start(self, name, op, args):
        """Start an operation on a card. Returns at once; the page polls the card's `op`."""
        fn = getattr(self, "_op_" + op, None)
        if fn is None: raise CardError("no such operation: %s" % op)
        if op not in ("create", "stock"): self.path(name)
        if self.readonly(name) and op not in self.READONLY_OPS:
            raise CardError("the standard disk is read-only: duplicate it to get a card you can change")
        else: self.path(name, must_exist=False)
        if self.booted == name and op not in ("plan", "scan", "rename") and not (self.readonly(name) and op == "duplicate"):
            raise CardError("the emulator is running this card; boot another (or none) first")
        with self.lock:
            cur = self.ops.get(name)
            if cur and cur.get("state") == "running": raise CardError("%s is busy: %s" % (name, cur["op"]))
            st = self.ops[name] = dict(op=op, state="running", started=time.time(), stage="", done=0, total=0, item="",
                                      cancel=False, result=None, error=None)
        def progress(stage, done=0, total=0, item=""):
            st.update(stage=stage, done=done, total=total, item=os.path.basename(str(item or "")))
        def run():
            try: st["result"] = fn(name, args, progress, lambda: st["cancel"])
            except CardError as e: st["error"] = str(e)
            except self.fp.FlashpodError as e: st["error"] = _msg(e); st["code"] = getattr(e, "code", None)
            except Exception as e: st["error"] = "%s: %s" % (type(e).__name__, e); st["trace"] = traceback.format_exc()[-2000:]
            if op not in ("delete", "rename"):                 # the card's details are read again BEFORE the operation reads as done, so the page redraws with them
                st["stage"] = "scan"
                for _ in range(15):                            # the image can still change a moment after a write (flashpod's close): read it again until the details are current
                    try:
                        r = self.scan(name if op != "duplicate" else args["to"])
                        if not r.get("busy") and r.get("scan_fresh"): break          # read again until the saved details match the image as it is now
                    except Exception: pass
                    time.sleep(0.4)
            st["state"] = "error" if st["error"] else "done"; st["ended"] = time.time()
        threading.Thread(target=run, daemon=True).start()
        return dict(ok=True, op=st)

    def cancel(self, name):
        st = self.ops.get(name)
        if st and st.get("state") == "running": st["cancel"] = True
        return dict(ok=True)

    def _op_create(self, name, a, progress, cancelled):
        size = int(a.get("size") or 0)
        if size < self.fp.min_card_bytes(): raise CardError("too small: a card needs at least %d MB" % (self.fp.min_card_bytes() // 2**20 + 1))
        if size > 2 * 1024 ** 4: raise CardError("too large")
        self.fp.create_blank(self.path(name, must_exist=False), size)
        return dict(created=size)

    def _op_flash(self, name, a, progress, cancelled):
        """Firmware only (David, 2026-10-16: writing firmware never erases the card): flashpod's flash_firmware,
        which keeps the partitions and the music. A card not yet partitioned is refused: use partition."""
        fw = self._firmware(a.get("firmware"), progress=lambda d, t: progress("download", d, t))
        progress("flash")
        return self.fp.flash_firmware(self.path(name), fw).to_dict()

    def _op_partition(self, name, a, progress, cancelled):
        """Partition and format a card that needs it: flashpod's full layout (MBR, firmware partition, FAT32) and an
        empty library. The layout includes a firmware partition, so it gets the catalog's default (Apple 1.5);
        the Firmware section changes it afterwards."""
        entry = next((e for e in self.fp.firmware_catalog() if e.get("default")), None) or \
            next(e for e in self.fp.firmware_catalog() if e.get("osos_sha256") == self.STOCK_OSOS)
        fw = self.fp.fetch_firmware(entry, progress=lambda d, t: progress("download", d, t))
        progress("partition and format"); r = self.fp.flash(self.path(name), fw).to_dict()
        progress("library"); self.fp.init(self.path(name), (a.get("ipod_name") or "iPod")[:60])
        return r

    def _op_format(self, name, a, progress, cancelled):
        """A fresh FAT32 on the data partition (erases the music), the firmware partition kept; then an empty library."""
        r = self.fp.format_data(self.path(name)).to_dict()
        if a.get("init", True): self.fp.init(self.path(name), (a.get("ipod_name") or "iPod")[:60])
        return r

    def _op_init(self, name, a, progress, cancelled):
        return self.fp.init(self.path(name), (a.get("ipod_name") or "iPod")[:60], overwrite=bool(a.get("overwrite"))).to_dict()

    def _op_rebuild(self, name, a, progress, cancelled):
        return self.fp.rebuild(self.path(name), (a.get("ipod_name") or "iPod")[:60], progress=progress).to_dict()

    def _op_scan(self, name, a, progress, cancelled):
        s = self.scan(name)                              # the summary itself holds this op: return only the outcome
        return dict(busy=bool(s.get("busy")), fresh=bool(s.get("scan_fresh")))

    def _op_plan(self, name, a, progress, cancelled):
        return self.fp.plan_add(self.path(name), self._sources(name, a), progress=progress, cancelled=cancelled).to_dict()

    def _op_add(self, name, a, progress, cancelled):
        policy = dict(self.fp.DEFAULT_POLICY)
        if a.get("full") in ("stop", "fit"): policy["full"] = a["full"]
        if a.get("ceiling") in ("refuse", "fit", "force"): policy["ceiling"] = a["ceiling"]
        r = self.fp.add(self.path(name), self._sources(name, a), policy=policy, progress=progress, cancelled=cancelled).to_dict()
        if a.get("kind") in ("uploads", "both") and not cancelled(): shutil.rmtree(self.uploads(name), ignore_errors=True)
        return r

    def _op_remove(self, name, a, progress, cancelled):
        ids = [int(x) for x in (a.get("ids") or [])]
        if not ids: raise CardError("choose songs to remove")
        return dict(removed=self.fp.remove(self.path(name), ids=ids))

    def _op_duplicate(self, name, a, progress, cancelled):
        to = self.path(a.get("to"), must_exist=False)
        if os.path.exists(to): raise CardError("%r exists" % a.get("to"))
        progress("copy")
        with open(self.path(name), "rb") as src:
            import fcntl
            fcntl.flock(src, fcntl.LOCK_SH | fcntl.LOCK_NB)                    # flashpod's read lock
            subprocess_cp(src.name, to)
        os.chmod(to, 0o644)                                                     # the standard disk is read-only; a copy is not
        return dict(copied=a.get("to"))

    def _op_rename(self, name, a, progress, cancelled):
        to = self.path(a.get("to"), must_exist=False)
        if os.path.exists(to): raise CardError("%r exists" % a.get("to"))
        os.rename(self.path(name), to)
        return self.rename_meta(name, a["to"])

    def rename_meta(self, name, to):
        """Everything about a renamed card except its image: the saved scan, its upload folder, its operation, which card
        is running or was booted last. (The offline app moves the image itself, by copying it.)"""
        if os.path.exists(self._sidecar(name)): os.rename(self._sidecar(name), self._sidecar(to))
        if os.path.isdir(self.uploads(name)): os.rename(self.uploads(name), self.uploads(to))
        self.ops[to] = self.ops.pop(name, None)
        if self.booted == name: self.booted = to
        last = self.load_last()
        if last and last["card"] == name: self.save_last(to, last.get("keep", True))
        return dict(renamed=to)

    def _op_delete(self, name, a, progress, cancelled):
        if a.get("confirm") != name: raise CardError("type the card's name to delete it")
        os.unlink(self.path(name))
        for f in (self._sidecar(name),):
            if os.path.exists(f): os.unlink(f)
        shutil.rmtree(self.uploads(name), ignore_errors=True)
        return dict(deleted=name)

    def burn_command(self, name):
        p = self.path(name)
        return dict(find="lsblk -o NAME,SIZE,MODEL,TRAN,RM", burn="flashpod burn '%s' /dev/sdX" % p,
                    note="Run these yourself in a terminal; check the device twice. The web app never writes a device.")


def subprocess_cp(src, dst):
    """A sparse copy of a card image (cp keeps the holes)."""
    import subprocess
    r = subprocess.run(["cp", "--sparse=always", "--", src, dst], capture_output=True, text=True)
    if r.returncode: raise CardError("copy failed: " + r.stderr.strip())


def _msg(e):
    d = getattr(e, "details", None)
    return "%s%s" % (e, (" (%s)" % ", ".join("%s: %s" % kv for kv in d.items())) if d else "")
