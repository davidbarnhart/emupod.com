//! The card store: card images (NAME.img) in one folder, managed through flashpod's Rust library (flashpod-core's JSON commands, as
//! the offline app's cardmgr.js runs them), answering what emupod/cards.py answers under /api. Files beside the cards: NAME.json (the
//! card's last scan), .uploads/NAME/ (songs uploaded from the page), .last-card.json (the card booted last).
//!
//! An operation runs on its own thread and returns at once; the page polls the card's summary, whose `op` says how it is going.
use flashpod_core::api::{self, Host};
use flashpod_core::block::BlockDevice;
use flashpod_core::library::Hooks;
use flashpod_core::tags::ReadAt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The built-in card: the demo's Rainbow Road (flashpod 1.6 and one song), read-only. The iPod boots it when no other card is chosen,
/// and falls back to it when a card will not start; duplicating it gives a card that can be changed.
pub const BUILTIN: &str = "Rainbow Road";
/// where the built-in card comes from: the demo site's card (stock.json names it and its sha256 prefix)
const DEMO_BASE: &str = "https://emupod.com";
const STOCK_OSOS: &str = "98251add80f99732142532814c4ad5b95ee08ab844dd988458a6fa254e1c7c97"; // Apple's 1.5
const AUDIO_EXT: [&str; 8] = [".mp3", ".m4a", ".m4b", ".aac", ".wav", ".aif", ".aiff", ".flac"];
const OPS: [&str; 14] = ["create", "partition", "flash", "init", "rebuild", "format", "add", "remove", "duplicate", "delete", "rename", "scan", "plan", "resize"];

/// A refusal with a message for the page (400), or a failure (500).
pub enum ApiErr {
    Card(String),
    Other(String),
    NotFound,
}
impl From<io::Error> for ApiErr {
    fn from(e: io::Error) -> ApiErr {
        ApiErr::Other(format!("OSError: {e}"))
    }
}
fn card_err<T>(m: impl Into<String>) -> Result<T, ApiErr> {
    Err(ApiErr::Card(m.into()))
}

fn now_s() -> f64 {
    crate::live::now_s()
}

fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_alphanumeric() && b.iter().all(|&c| c.is_ascii_alphanumeric() || b" ._-".contains(&c)) && !n.ends_with(".img")
}

fn is_audio(f: &str) -> bool {
    let l = f.to_lowercase();
    AUDIO_EXT.iter().any(|x| l.ends_with(x))
}

fn mtime(m: &fs::Metadata) -> f64 {
    m.mtime() as f64 + m.mtime_nsec() as f64 / 1e9
}

// ------------------------------------------------------------------------------------------------ flashpod-core's host side

/// A card image as flashpod's block device.
pub struct FileDev(pub File);
impl BlockDevice for FileDev {
    fn len(&self) -> io::Result<u64> {
        use std::io::Seek;
        let m = self.0.metadata()?;
        if std::os::unix::fs::FileTypeExt::is_block_device(&m.file_type()) {
            // a real card: a device node's length is where its end is
            return (&self.0).seek(io::SeekFrom::End(0));
        }
        Ok(m.len())
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            match self.0.read_at(&mut buf[n..], off + n as u64)? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
        self.0.write_all_at(buf, off)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
}
/// No card: for the commands that need none (catalog, min_card_bytes, check_firmware).
struct NoDev;
impl BlockDevice for NoDev {
    fn len(&self) -> io::Result<u64> {
        Ok(0)
    }
    fn read_at(&mut self, _: u64, _: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
    fn write_at(&mut self, _: u64, _: &[u8]) -> io::Result<()> {
        Err(io::Error::other("no card"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn set_len(&mut self, _: u64) -> io::Result<()> {
        Err(io::Error::other("no card"))
    }
}

struct Src(File);
impl ReadAt for Src {
    fn size(&mut self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_at(buf, off)
    }
}

/// The songs an add reads are files on this machine (their full paths are the names); progress and cancel go to the card's op.
struct DiskHost {
    op: Option<Arc<Mutex<Value>>>,
}
impl Hooks for DiskHost {
    fn progress(&mut self, stage: &str, done: u64, total: u64, item: &str) {
        if let Some(op) = &self.op {
            let mut o = op.lock().unwrap();
            o["stage"] = json!(stage);
            o["done"] = json!(done);
            o["total"] = json!(total);
            o["item"] = json!(item);
        }
    }
    fn cancelled(&mut self) -> bool {
        self.op.as_ref().map(|o| o.lock().unwrap()["cancel"] == true).unwrap_or(false)
    }
}
impl Host for DiskHost {
    fn source(&mut self, path: &str) -> Option<Box<dyn ReadAt + '_>> {
        File::open(path).ok().map(|f| Box::new(Src(f)) as Box<dyn ReadAt>)
    }
}

/// flashpod's error as the page shows it: the message, and its details in parentheses.
fn fp_message(e: &Value) -> String {
    let mut m = e["message"].as_str().unwrap_or("flashpod failed").to_string();
    if let Some(d) = e["details"].as_object() {
        if !d.is_empty() {
            let parts: Vec<String> = d.iter().map(|(k, v)| format!("{k}: {}", v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string()))).collect();
            m = format!("{m} ({})", parts.join(", "));
        }
    }
    m
}

/// One flashpod command: its value, or (code, message).
fn run(dev: &mut dyn BlockDevice, cmd: Value, blob: &[u8], op: Option<Arc<Mutex<Value>>>) -> Result<Value, (String, String)> {
    let mut host = DiskHost { op };
    let a = api::run(&cmd, blob, dev, &mut host);
    if a["ok"] == true {
        Ok(a["value"].clone())
    } else {
        Err((a["error"]["code"].as_str().unwrap_or("error").to_string(), fp_message(&a["error"])))
    }
}

fn entropy() -> (u64, u64) {
    let mut b = [0u8; 8];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    (u64::from_le_bytes(b) & ((1 << 52) - 1), now_s() as u64)
}
fn with_entropy(mut v: Value) -> Value {
    let (seed, now) = entropy();
    v["seed"] = json!(seed);
    v["now"] = json!(now);
    v
}

fn sha256_hex(b: &[u8]) -> String {
    flashpod_core::firmware::sha256_hex(b)
}

// ------------------------------------------------------------------------------------------------ the store

pub struct Store {
    pub root: PathBuf,
    /// the folder songs are added from on this machine: the page's choice (.settings.json in the card folder), else --music-root
    pub music_root: Mutex<Option<PathBuf>>,
    pub firmware_dirs: Vec<PathBuf>,
    catalog: Vec<Value>,
    base_url: String,
    ops: Mutex<HashMap<String, Arc<Mutex<Value>>>>,
    /// the card the iPod runs and whether its changes are kept (the main loop sets it)
    pub booted: Mutex<Option<(String, bool)>>,
    /// commands for the iPod (a restart onto another card): the HTTP side's queue
    pub restart: Mutex<Option<Box<dyn Fn(Value) + Send>>>,
}

impl Store {
    pub fn new(root: &str, music_root: Option<&str>, firmware_dirs: &[String]) -> io::Result<Arc<Store>> {
        fs::create_dir_all(root)?;
        let base_url = serde_json::from_str::<Value>(flashpod_core::firmware::CATALOG_JSON)
            .ok().and_then(|v| v["base_url"].as_str().map(|s| s.to_string())).unwrap_or_default();
        Ok(Arc::new(Store {
            root: fs::canonicalize(root)?,
            music_root: Mutex::new(
                Self::saved_music_root(Path::new(root)).or_else(|| music_root.and_then(|m| fs::canonicalize(m).ok())),
            ),
            firmware_dirs: firmware_dirs.iter().filter_map(|d| fs::canonicalize(d).ok()).collect(),
            catalog: flashpod_core::firmware::catalog(),
            base_url,
            ops: Mutex::new(HashMap::new()),
            booted: Mutex::new(None),
            restart: Mutex::new(None),
        }))
    }

    pub fn path(&self, name: &str, must_exist: bool) -> Result<PathBuf, ApiErr> {
        if !valid_name(name) {
            return card_err(format!("not a card name: {name:?}"));
        }
        let p = self.root.join(format!("{name}.img"));
        if must_exist && !p.is_file() {
            return card_err(format!("no card named {name:?}"));
        }
        Ok(p)
    }
    fn sidecar(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.json"))
    }
    fn uploads(&self, name: &str) -> PathBuf {
        self.root.join(".uploads").join(name)
    }
    fn upload_list(&self, name: &str) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.uploads(name)).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.')).collect()).unwrap_or_default();
        v.sort();
        v
    }
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.root).map(|d| d.filter_map(|e| e.ok()).filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".img").filter(|s| valid_name(s)).map(|s| s.to_string())
        }).collect()).unwrap_or_default();
        v.sort_by(|a, b| (a != BUILTIN, a).cmp(&(b != BUILTIN, b)));
        v
    }

    pub fn min_card_bytes(&self) -> u64 {
        run(&mut NoDev, json!({"op": "min_card_bytes"}), &[], None).ok().and_then(|v| v.as_u64()).unwrap_or(32 << 20)
    }

    // ---- the last booted card
    pub fn save_last(&self, name: &str, keep: bool) {
        let p = self.root.join(".last-card.json");
        let tmp = self.root.join(".last-card.json.tmp");
        if fs::write(&tmp, json!({"card": name, "keep": keep, "at": now_s()}).to_string()).is_ok() {
            let _ = fs::rename(tmp, p);
        }
    }
    pub fn load_last(&self) -> Option<(String, bool)> {
        let v: Value = serde_json::from_str(&fs::read_to_string(self.root.join(".last-card.json")).ok()?).ok()?;
        let n = v["card"].as_str()?.to_string();
        self.path(&n, true).ok()?;
        Some((n, v["keep"].as_bool().unwrap_or(true)))
    }

    // ---- listing and scans
    fn read_side(&self, name: &str) -> Option<Value> {
        serde_json::from_str(&fs::read_to_string(self.sidecar(name)).ok()?).ok()
    }
    pub fn summary(&self, name: &str) -> Result<Value, ApiErr> {
        let p = self.path(name, true)?;
        let op = self.ops.lock().unwrap().get(name).map(|o| o.lock().unwrap().clone());
        let m = fs::metadata(&p)?;
        let side = self.read_side(name);
        let fresh = side.as_ref().map(|s| s["size"].as_u64() == Some(m.len()) && s["mtime"].as_f64().map(|t| (t - mtime(&m)).abs() < 1e-6).unwrap_or(false)).unwrap_or(false);
        let booted = self.booted.lock().unwrap().as_ref().map(|b| b.0 == name).unwrap_or(false);
        Ok(json!({"name": name, "size": m.len(), "on_disk": m.blocks() * 512, "mtime": mtime(&m),
                  "scan": side.as_ref().map(|s| s["scan"].clone()).unwrap_or(Value::Null), "scan_fresh": fresh,
                  "scanned": side.as_ref().map(|s| s["scanned"].clone()).unwrap_or(Value::Null), "op": op,
                  "booted": booted, "uploads": self.upload_list(name), "readonly": name == BUILTIN}))
    }
    pub fn list(&self) -> Value {
        let cards: Vec<Value> = self.names().iter().filter_map(|n| self.summary(n).ok()).collect();
        let b = self.booted.lock().unwrap().clone();
        json!({"root": self.root, "cards": cards, "flashpod": {"api": "1.0", "rust": true},
               "booted": b.as_ref().map(|x| x.0.clone()), "booted_keep": b.as_ref().map(|x| x.1),
               "music_root": *self.music_root.lock().unwrap(), "can_pick_music": true, "firmware": self.firmware_choices(), "can_resize": true, "can_burn": true})
    }
    /// flashpod's inspect, saved beside the image. A card the iPod holds is read as it is (a look never writes).
    pub fn scan(&self, name: &str) -> Result<Value, ApiErr> {
        let p = self.path(name, true)?;
        let f = File::open(&p)?;
        let info = run(&mut FileDev(f), json!({"op": "inspect", "path": p}), &[], None).map_err(|(_, m)| ApiErr::Card(m))?;
        let m = fs::metadata(&p)?;
        let tmp = self.root.join(format!(".{name}.json.tmp"));
        fs::write(&tmp, json!({"size": m.len(), "mtime": mtime(&m), "scanned": now_s(), "scan": info}).to_string())?;
        fs::rename(tmp, self.sidecar(name))?;
        Ok(info)
    }
    pub fn tracks(&self, name: &str) -> Result<Value, ApiErr> {
        let f = File::open(self.path(name, true)?)?;
        run(&mut FileDev(f), json!({"op": "tracks"}), &[], None).map_err(|(_, m)| ApiErr::Card(m))
    }
    pub fn burn(&self, name: &str) -> Result<Value, ApiErr> {
        let p = self.path(name, true)?;
        Ok(json!({"find": "lsblk -o NAME,SIZE,MODEL,TRAN,RM", "burn": format!("flashpod burn '{}' /dev/sdX", p.display()),
                  "note": "Run these yourself in a terminal; check the device twice. The web app never writes a device."}))
    }

    // ---- firmware
    fn cache_dir() -> PathBuf {
        let base = std::env::var("XDG_CACHE_HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache"));
        base.join("flashpod").join("firmware")
    }
    pub fn firmware_choices(&self) -> Vec<Value> {
        let preferred = self.default_firmware();
        let mut out: Vec<Value> = self.catalog.iter().filter(|e| e["models"].as_array().map(|m| m.iter().any(|x| x == "1G")).unwrap_or(false)).map(|e| {
            let file = e["file"].as_str().unwrap_or("");
            let local = Self::cache_dir().join(file).is_file();
            json!({"id": format!("catalog:{file}"), "label": format!("{} {} ({}){}", e["maker"].as_str().unwrap_or("Apple"), e["version"].as_str().unwrap_or("?"),
                   e["build_date"].as_str().unwrap_or("?"), if local { "" } else { ", downloads" }), "default": file == preferred})
        }).collect();
        for d in &self.firmware_dirs {
            let mut files = vec![];
            collect_firmware(d, d, 0, &mut files);
            files.sort();
            for f in files {
                let rel = f.strip_prefix(d).unwrap_or(&f).display().to_string();
                out.push(json!({"id": format!("file:{}", f.display()), "label": format!("{rel} ({})", d.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default())}));
            }
        }
        out
    }
    /// A catalog firmware's bytes: from flashpod's cache, else downloaded from flashpod's releases and checked against the catalog.
    pub fn fetch_firmware(&self, file: &str, op: Option<&Arc<Mutex<Value>>>) -> Result<Vec<u8>, ApiErr> {
        let Some(e) = self.catalog.iter().find(|e| e["file"] == file) else { return card_err(format!("no such catalog entry: {file}")) };
        let want = e["sha256"].as_str().unwrap_or("");
        let cached = Self::cache_dir().join(file);
        if let Ok(b) = fs::read(&cached) {
            if sha256_hex(&b) == want {
                return Ok(b);
            }
        }
        let url = format!("{}/{}", self.base_url, file);
        let resp = ureq::get(&url).call().map_err(|x| ApiErr::Card(format!("could not download {file}: {x}")))?;
        let total: u64 = resp.header("Content-Length").and_then(|s| s.parse().ok()).unwrap_or(e["size"].as_u64().unwrap_or(0));
        let mut r = resp.into_reader();
        let mut b = Vec::new();
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            let n = r.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            b.extend_from_slice(&chunk[..n]);
            if let Some(op) = op {
                let mut o = op.lock().unwrap();
                o["stage"] = json!("download");
                o["done"] = json!(b.len());
                o["total"] = json!(total);
                o["item"] = json!(file);
            }
        }
        if sha256_hex(&b) != want {
            return card_err(format!("{file} failed its checksum"));
        }
        let _ = fs::create_dir_all(Self::cache_dir());
        let tmp = cached.with_extension("part");
        if fs::write(&tmp, &b).is_ok() {
            let _ = fs::rename(tmp, &cached);
        }
        Ok(b)
    }
    fn firmware(&self, id: &Value, op: &Arc<Mutex<Value>>) -> Result<Vec<u8>, ApiErr> {
        let id = id.as_str().unwrap_or("");
        if let Some(f) = id.strip_prefix("catalog:") {
            return self.fetch_firmware(f, Some(op));
        }
        if let Some(p) = id.strip_prefix("file:") {
            let p = fs::canonicalize(p).map_err(|_| ApiErr::Card("not an offered firmware file".into()))?;
            if !self.firmware_dirs.iter().any(|d| p.starts_with(d) && p != *d) || !p.is_file() {
                return card_err("not an offered firmware file");
            }
            return Ok(fs::read(p)?);
        }
        card_err("choose a firmware")
    }
    /// The firmware new cards get, and the one the page offers first: flashpod's newest for the 1st generation (1.6 today: Apple's 1.5
    /// with its fixes, and FLAC), else the catalog's default, else Apple's 1.5.
    fn default_firmware(&self) -> String {
        let first_gen = |e: &&Value| e["models"].as_array().map(|m| m.iter().any(|x| x == "1G")).unwrap_or(false);
        self.catalog.iter().filter(first_gen).filter(|e| e["maker"] == "flashpod")
            .max_by_key(|e| e["build_date"].as_str().unwrap_or("").to_string())
            .or_else(|| self.catalog.iter().find(|e| e["default"] == true))
            .or_else(|| self.catalog.iter().find(|e| e["osos_sha256"] == STOCK_OSOS))
            .and_then(|e| e["file"].as_str()).unwrap_or("").to_string()
    }

    // ---- music and uploads
    fn music_path(&self, rel: &str) -> Result<PathBuf, ApiErr> {
        let Some(root) = self.music_root.lock().unwrap().clone() else { return card_err("no music folder is set: choose one on the Cards page") };
        let p = fs::canonicalize(root.join(rel.trim_start_matches('/'))).map_err(|_| ApiErr::Card("no such folder".into()))?;
        if !p.starts_with(&root) {
            return card_err("outside the music folder");
        }
        Ok(p)
    }
    pub fn music(&self, rel: &str) -> Result<Value, ApiErr> {
        let base = self.music_path(rel)?;
        let root = self.music_root.lock().unwrap().clone().unwrap_or_default();
        let (mut dirs, mut files) = (vec![], vec![]);
        for e in fs::read_dir(&base)?.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') {
                continue;
            }
            if e.path().is_dir() {
                dirs.push(n)
            } else if is_audio(&n) {
                files.push(n)
            }
        }
        dirs.sort_by_key(|s| s.to_lowercase());
        files.sort_by_key(|s| s.to_lowercase());
        let rel = base.strip_prefix(&root).map(|p| p.display().to_string()).unwrap_or_default();
        Ok(json!({"root": root, "path": rel, "dirs": dirs, "files": files}))
    }
    /// One uploaded song (the request body, `len` bytes) into the card's upload folder.
    pub fn upload(&self, name: &str, filename: &str, body: &mut dyn Read, len: u64) -> Result<Value, ApiErr> {
        self.path(name, true)?;
        if name == BUILTIN {
            return card_err("the built-in card is read-only: duplicate it to get a card you can change");
        }
        let base: String = filename.rsplit('/').next().unwrap_or("").trim().chars()
            .map(|c| if c.is_ascii_alphanumeric() || " ._()&,'+-".contains(c) { c } else { '_' }).take(120).collect();
        if base.is_empty() || base.starts_with('.') {
            return card_err("not a file name");
        }
        if !is_audio(&base) {
            return card_err(format!("not an audio file the iPod can play ({})", AUDIO_EXT.join(", ")));
        }
        if len > 2 << 30 {
            return card_err("too large");
        }
        let d = self.uploads(name);
        fs::create_dir_all(&d)?;
        let tmp = d.join(format!(".{base}.part"));
        let mut f = File::create(&tmp)?;
        let n = io::copy(&mut body.take(len), &mut f)?;
        drop(f);
        if n != len {
            let _ = fs::remove_file(&tmp);
            return card_err("upload cut short");
        }
        fs::rename(tmp, d.join(&base))?;
        Ok(json!({"ok": true, "file": base, "uploads": self.upload_list(name)}))
    }
    /// The song files an add or a plan takes: the card's uploads and/or folders and files under the music folder, folders walked.
    fn sources(&self, name: &str, a: &Value) -> Result<Vec<String>, ApiErr> {
        let kind = a["kind"].as_str().unwrap_or("");
        let mut roots: Vec<PathBuf> = vec![];
        let up = self.uploads(name);
        let has_up = !self.upload_list(name).is_empty();
        match kind {
            "uploads" => {
                if !has_up {
                    return card_err("nothing uploaded for this card");
                }
                roots.push(up)
            }
            "music" | "both" => {
                if kind == "both" && has_up {
                    roots.push(up);
                }
                for r in a["paths"].as_array().cloned().unwrap_or_default() {
                    roots.push(self.music_path(r.as_str().unwrap_or(""))?);
                }
                if roots.is_empty() {
                    return card_err(if kind == "music" { "choose a folder or files" } else { "no songs chosen" });
                }
            }
            _ => return card_err("no songs chosen"),
        }
        let mut files = vec![];
        for r in roots {
            walk_audio(&r, &mut files);
        }
        if files.is_empty() {
            return card_err("no songs in what was chosen");
        }
        Ok(files)
    }

    // ---- the music folder, chosen on the page
    fn saved_music_root(root: &Path) -> Option<PathBuf> {
        let v: Value = serde_json::from_str(&fs::read_to_string(root.join(".settings.json")).ok()?).ok()?;
        let p = PathBuf::from(v["music_root"].as_str()?);
        p.is_dir().then_some(p)
    }
    /// Set (or, with None, clear) the music folder, and keep the choice for the next start.
    pub fn set_music_root(&self, p: Option<&str>) -> Result<Value, ApiErr> {
        let p = match p.filter(|s| !s.is_empty()) {
            Some(s) => Some(fs::canonicalize(s).ok().filter(|x| x.is_dir()).ok_or_else(|| ApiErr::Card(format!("no such folder: {s}")))?),
            None => None,
        };
        let mut v: Value = fs::read_to_string(self.root.join(".settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(json!({}));
        if !v.is_object() {
            v = json!({});
        }
        v["music_root"] = json!(p);
        let tmp = self.root.join(".settings.json.tmp");
        fs::write(&tmp, v.to_string())?;
        fs::rename(tmp, self.root.join(".settings.json"))?;
        *self.music_root.lock().unwrap() = p;
        Ok(self.list())
    }
    /// The folders under `path` on this machine, for choosing the music folder (folder names and song counts only, never file
    /// contents); with no path, the usual places: home, /media, /mnt, /srv.
    pub fn folders(&self, path: &str) -> Result<Value, ApiErr> {
        let count = |d: &Path| fs::read_dir(d).map(|r| r.filter_map(|e| e.ok()).filter(|e| is_audio(&e.file_name().to_string_lossy())).count()).unwrap_or(0);
        if path.is_empty() {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let roots: Vec<Value> = [home.as_str(), "/media", "/mnt", "/srv"].iter().map(PathBuf::from).filter(|p| p.is_dir())
                .map(|p| json!({"name": p.display().to_string(), "path": p, "songs": count(&p)})).collect();
            return Ok(json!({"path": "", "parent": null, "dirs": roots, "songs": 0}));
        }
        let p = fs::canonicalize(path).map_err(|_| ApiErr::Card(format!("no such folder: {path}")))?;
        if !p.is_dir() {
            return card_err(format!("not a folder: {path}"));
        }
        let mut dirs: Vec<(String, PathBuf)> = fs::read_dir(&p)?.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect();
        dirs.sort_by_key(|d| d.0.to_lowercase());
        let dirs: Vec<Value> = dirs.into_iter().map(|(n, d)| json!({"name": n, "path": d, "songs": count(&d)})).collect();
        Ok(json!({"path": p, "parent": p.parent().map(|x| x.to_path_buf()), "dirs": dirs, "songs": count(&p)}))
    }

    // ---- operations
    /// Start an operation on a card; returns its state at once (the page polls the card).
    pub fn start(self: &Arc<Self>, name: &str, op: &str, a: Value) -> Result<Value, ApiErr> {
        if !OPS.contains(&op) {
            return card_err(format!("no such operation: {op}"));
        }
        self.path(name, op != "create")?;
        if name == BUILTIN && !matches!(op, "scan" | "plan" | "duplicate") {
            return card_err("the built-in card is read-only: duplicate it to get a card you can change");
        }
        let booted = self.booted.lock().unwrap().as_ref().map(|b| b.0 == name).unwrap_or(false);
        if booted && !matches!(op, "plan" | "scan" | "rename") && !(name == BUILTIN && op == "duplicate") {
            return card_err("the emulator is running this card; boot another (or none) first");
        }
        let st = Arc::new(Mutex::new(json!({"op": op, "state": "running", "started": now_s(), "stage": "", "done": 0, "total": 0, "item": "",
                                             "cancel": false, "result": null, "error": null})));
        {
            let mut ops = self.ops.lock().unwrap();
            if let Some(cur) = ops.get(name) {
                let c = cur.lock().unwrap();
                if c["state"] == "running" {
                    return card_err(format!("{name} is busy: {}", c["op"].as_str().unwrap_or("")));
                }
            }
            ops.insert(name.to_string(), st.clone());
        }
        let (me, name, op, st2) = (self.clone(), name.to_string(), op.to_string(), st.clone());
        std::thread::spawn(move || {
            let r = me.do_op(&name, &op, &a, &st2);
            {
                let mut o = st2.lock().unwrap();
                match &r {
                    Ok(v) => o["result"] = v.clone(),
                    Err(ApiErr::Card(m)) | Err(ApiErr::Other(m)) => o["error"] = json!(m),
                    Err(ApiErr::NotFound) => o["error"] = json!("not found"),
                }
            }
            // the card is read again BEFORE the operation reads as done, so the page redraws with its new details
            let failed = r.is_err();
            if !matches!(op.as_str(), "delete" | "scan") && !(failed && matches!(op.as_str(), "create" | "duplicate" | "rename" | "stock")) {
                st2.lock().unwrap()["stage"] = json!("scan");
                let target = if matches!(op.as_str(), "duplicate" | "rename") { a["to"].as_str().unwrap_or(&name).to_string() } else { name.clone() };
                let _ = me.scan(&target);
            }
            let mut o = st2.lock().unwrap();
            o["state"] = json!(if failed { "error" } else { "done" });
            o["ended"] = json!(now_s());
        });
        let v = st.lock().unwrap().clone();
        Ok(v)
    }
    pub fn op_state(&self, name: &str) -> Option<Value> {
        self.ops.lock().unwrap().get(name).map(|o| o.lock().unwrap().clone())
    }
    pub fn cancel(&self, name: &str) -> Value {
        if let Some(o) = self.ops.lock().unwrap().get(name) {
            let mut o = o.lock().unwrap();
            if o["state"] == "running" {
                o["cancel"] = json!(true);
            }
        }
        json!({"ok": true})
    }
    pub fn clear_uploads(&self, name: &str) -> Result<Value, ApiErr> {
        self.path(name, true)?;
        let _ = fs::remove_dir_all(self.uploads(name));
        Ok(json!({"ok": true}))
    }

    /// Open a card for writing, with flashpod's exclusive lock (the iPod holds a lock on the card it runs).
    fn open_rw(&self, name: &str) -> Result<FileDev, ApiErr> {
        let f = OpenOptions::new().read(true).write(true).open(self.path(name, true)?)?;
        if f.try_lock().is_err() {
            return card_err(format!("{name} is in use (the iPod or another program has it open)"));
        }
        Ok(FileDev(f))
    }
    fn fp(&self, dev: &mut FileDev, cmd: Value, blob: &[u8], st: &Arc<Mutex<Value>>) -> Result<Value, ApiErr> {
        run(dev, cmd, blob, Some(st.clone())).map_err(|(code, m)| {
            st.lock().unwrap()["code"] = json!(code);
            ApiErr::Card(m)
        })
    }
    fn ipod_name(a: &Value) -> String {
        a["ipod_name"].as_str().filter(|s| !s.is_empty()).unwrap_or("iPod").chars().take(60).collect()
    }

    fn do_op(self: &Arc<Self>, name: &str, op: &str, a: &Value, st: &Arc<Mutex<Value>>) -> Result<Value, ApiErr> {
        let progress = |stage: &str| {
            st.lock().unwrap()["stage"] = json!(stage);
        };
        match op {
            "create" => {
                let size = a["size"].as_f64().unwrap_or(0.0) as u64;
                let p = self.path(name, false)?;
                if p.exists() {
                    return card_err(format!("{name}.img already exists"));
                }
                let min = self.min_card_bytes();
                if size < min {
                    return card_err(format!("too small: a card needs at least {} MB", min / (1 << 20) + 1));
                }
                if size > 2 << 40 {
                    return card_err("too large");
                }
                let f = OpenOptions::new().read(true).write(true).create_new(true).open(&p)?;
                let r = run(&mut FileDev(f), json!({"op": "create", "size": size, "path": p}), &[], Some(st.clone()));
                if let Err((_, m)) = r {
                    let _ = fs::remove_file(&p);
                    return card_err(m);
                }
                Ok(json!({"created": size}))
            }
            "partition" => {
                let fw = self.fetch_firmware(&self.default_firmware(), Some(st))?;
                progress("partition and format");
                let mut dev = self.open_rw(name)?;
                let r = self.fp(&mut dev, json!({"op": "flash"}), &fw, st)?;
                progress("library");
                self.fp(&mut dev, with_entropy(json!({"op": "init", "name": Self::ipod_name(a)})), &[], st)?;
                Ok(r)
            }
            "flash" => {
                let fw = self.firmware(&a["firmware"], st)?;
                progress("flash");
                let mut dev = self.open_rw(name)?;
                self.fp(&mut dev, json!({"op": "flash_firmware"}), &fw, st)
            }
            "format" => {
                let mut dev = self.open_rw(name)?;
                let r = self.fp(&mut dev, json!({"op": "format_data"}), &[], st)?;
                if a["init"] != false {
                    self.fp(&mut dev, with_entropy(json!({"op": "init", "name": Self::ipod_name(a)})), &[], st)?;
                }
                Ok(r)
            }
            "init" => {
                let mut dev = self.open_rw(name)?;
                self.fp(&mut dev, with_entropy(json!({"op": "init", "name": Self::ipod_name(a), "overwrite": a["overwrite"] == true})), &[], st)
            }
            "rebuild" => {
                let mut dev = self.open_rw(name)?;
                self.fp(&mut dev, with_entropy(json!({"op": "rebuild", "name": Self::ipod_name(a), "save": true})), &[], st)
            }
            "scan" => {
                self.scan(name)?;
                Ok(json!({"busy": false, "fresh": true}))
            }
            "plan" => {
                let files = self.sources(name, a)?;
                let f = File::open(self.path(name, true)?)?;
                self.fp(&mut FileDev(f), with_entropy(json!({"op": "plan_add", "files": files, "assess": true})), &[], st)
            }
            "add" => {
                let files = self.sources(name, a)?;
                let full = a["full"].as_str().filter(|s| matches!(*s, "stop" | "fit")).unwrap_or("stop");
                let ceiling = a["ceiling"].as_str().filter(|s| matches!(*s, "refuse" | "fit" | "force")).unwrap_or("refuse");
                let mut dev = self.open_rw(name)?;
                let r = self.fp(&mut dev, with_entropy(json!({"op": "add", "files": files, "full": full, "ceiling": ceiling})), &[], st)?;
                if matches!(a["kind"].as_str(), Some("uploads" | "both")) && st.lock().unwrap()["cancel"] != true {
                    let _ = fs::remove_dir_all(self.uploads(name));
                }
                Ok(r)
            }
            "remove" => {
                let ids: Vec<String> = a["ids"].as_array().cloned().unwrap_or_default().iter()
                    .filter_map(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).map(|i| i.to_string()).collect();
                if ids.is_empty() {
                    return card_err("choose songs to remove");
                }
                let mut dev = self.open_rw(name)?;
                Ok(json!({"removed": self.fp(&mut dev, with_entropy(json!({"op": "remove", "what": ids})), &[], st)?}))
            }
            "duplicate" => {
                let to_name = a["to"].as_str().unwrap_or("");
                let to = self.path(to_name, false)?;
                if to.exists() {
                    return card_err(format!("{to_name:?} exists"));
                }
                progress("copy");
                let src = File::open(self.path(name, true)?)?;
                let _ = src.try_lock_shared();
                copy_sparse(&src, &to, st)?;
                fs::set_permissions(&to, fs::Permissions::from_mode(0o644))?;
                Ok(json!({"copied": to_name}))
            }
            "rename" => {
                let to_name = a["to"].as_str().unwrap_or("").to_string();
                let to = self.path(&to_name, false)?;
                if to.exists() {
                    return card_err(format!("{to_name:?} exists"));
                }
                fs::rename(self.path(name, true)?, &to)?;
                if self.sidecar(name).exists() {
                    let _ = fs::rename(self.sidecar(name), self.sidecar(&to_name));
                }
                if self.uploads(name).is_dir() {
                    let _ = fs::rename(self.uploads(name), self.uploads(&to_name));
                }
                {
                    let mut ops = self.ops.lock().unwrap();
                    if let Some(o) = ops.remove(name) {
                        ops.insert(to_name.clone(), o);
                    }
                }
                {
                    let mut b = self.booted.lock().unwrap();
                    if let Some(x) = b.as_mut() {
                        if x.0 == name {
                            x.0 = to_name.clone();
                        }
                    }
                }
                if let Some((last, keep)) = self.load_last_raw() {
                    if last == name {
                        self.save_last(&to_name, keep);
                    }
                }
                Ok(json!({"renamed": to_name}))
            }
            "resize" => self.resize(name, a["size"].as_f64().unwrap_or(0.0) as u64, st),
            "delete" => {
                if a["confirm"].as_str() != Some(name) {
                    return card_err("type the card's name to delete it");
                }
                fs::remove_file(self.path(name, true)?)?;
                let _ = fs::remove_file(self.sidecar(name));
                let _ = fs::remove_dir_all(self.uploads(name));
                Ok(json!({"deleted": name}))
            }
            _ => card_err(format!("no such operation: {op}")),
        }
    }
    /// A card made bigger or smaller: a new image of `size` bytes made by clone_card from this one, then put in its place. It can
    /// shrink as far as the files still fit.
    fn resize(&self, name: &str, size: u64, st: &Arc<Mutex<Value>>) -> Result<Value, ApiErr> {
        let src_path = self.path(name, true)?;
        let min = self.min_card_bytes();
        if size < min {
            return card_err(format!("too small: a card needs at least {} MB", min / (1 << 20) + 1));
        }
        if size > 2 << 40 {
            return card_err("too large");
        }
        let lock = File::open(&src_path)?;
        if lock.try_lock().is_err() {
            return card_err(format!("{name} is in use (the iPod or another program has it open)"));
        }
        let tmp = self.root.join(format!(".{name}.resizing.img"));
        let _ = fs::remove_file(&tmp);
        let r = (|| -> Result<Value, ApiErr> {
            let f = OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?;
            f.set_len(size)?;
            let set = |stage: &str, done: u64, total: u64, item: &str| {
                let mut o = st.lock().unwrap();
                o["stage"] = json!(stage);
                o["done"] = json!(done);
                o["total"] = json!(total);
                o["item"] = json!(item);
            };
            clone_card(&src_path, &mut FileDev(f), &set).map_err(ApiErr::Card)
        })();
        drop(lock);
        match r {
            Ok(v) => {
                fs::rename(&tmp, &src_path)?;
                Ok(v)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    fn load_last_raw(&self) -> Option<(String, bool)> {
        let v: Value = serde_json::from_str(&fs::read_to_string(self.root.join(".last-card.json")).ok()?).ok()?;
        Some((v["card"].as_str()?.to_string(), v["keep"].as_bool().unwrap_or(true)))
    }

    /// Put the built-in card in place if it is missing: from `local` (the card image, or its .gz) when given, else downloaded from the
    /// demo site (stock.json and the file it names), checked against the sha256 prefix stock.json gives, written with its zero blocks
    /// left as holes, and made read-only.
    pub fn ensure_builtin(&self, local: Option<&str>) -> Result<(), String> {
        let fin = self.root.join(format!("{BUILTIN}.img"));
        if fin.exists() {
            return Ok(());
        }
        let (raw, want): (Vec<u8>, Option<String>) = match local {
            Some(p) => (fs::read(p).map_err(|e| format!("{p}: {e}"))?, None),
            None => {
                let base = std::env::var("EMUPOD_DEMO_URL").unwrap_or_else(|_| DEMO_BASE.to_string());
                let meta: Value = ureq::get(&format!("{base}/stock.json")).call().map_err(|e| format!("{base}/stock.json: {e}"))?
                    .into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).ok_or("stock.json is not JSON")?;
                let file = meta["file"].as_str().ok_or("stock.json names no file")?;
                let mut b = Vec::new();
                ureq::get(&format!("{base}/{file}")).call().map_err(|e| format!("{base}/{file}: {e}"))?.into_reader().read_to_end(&mut b).map_err(|e| e.to_string())?;
                (b, meta["rev"].as_str().map(|s| s.to_string()))
            }
        };
        let img = if raw.starts_with(&[0x1f, 0x8b]) {
            let mut v = Vec::new();
            flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut v).map_err(|e| format!("the card's .gz: {e}"))?;
            v
        } else {
            raw
        };
        if let Some(w) = want {
            if !sha256_hex(&img).starts_with(&w) {
                return Err(format!("the downloaded card failed its check (sha256 should start {w})"));
            }
        }
        let tmp = self.root.join(".builtin.building.img");
        let _ = fs::remove_file(&tmp);
        let f = File::create(&tmp).map_err(|e| e.to_string())?;
        f.set_len(img.len() as u64).map_err(|e| e.to_string())?;
        for (i, blk) in img.chunks(4096).enumerate() {
            if blk.iter().any(|&b| b != 0) {
                f.write_all_at(blk, (i * 4096) as u64).map_err(|e| e.to_string())?;
            }
        }
        f.sync_all().map_err(|e| e.to_string())?;
        drop(f);
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444)).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &fin).map_err(|e| e.to_string())?;
        let _ = self.scan(BUILTIN);
        Ok(())
    }
}

/// The bytes of files on a card image's FAT partition (what a copy of it needs room for).
pub fn files_bytes(image: &Path) -> Result<u64, String> {
    let mut dev = FileDev(File::open(image).map_err(|e| e.to_string())?);
    let mut fat = flashpod_core::fat::open_image_fat(&mut dev).map_err(|e| e.message)?;
    let (mut total, mut todo) = (0u64, vec![String::new()]);
    while let Some(d) = todo.pop() {
        for e in fat.listdir(&d)?.unwrap_or_default() {
            let p = if d.is_empty() { e.name.clone() } else { format!("{d}/{}", e.name) };
            if e.attr & 0x10 != 0 { todo.push(p) } else { total += e.size as u64 }
        }
    }
    Ok(total)
}

/// Make `dst` (a blank image file or a real card, any size) a copy of the card image `src` the flashpod way: partition and format it
/// to its own size with the firmware the image carries, then copy every file of the image's FAT partition onto it as it is (the
/// iTunesDB with its songs, playlists and play counts unchanged), and check the songs read back the same. `set(stage, done, total,
/// item)` reports progress. Used by Resize (a new image) and Burn (a flash card).
pub fn clone_card(src_path: &Path, dst: &mut dyn BlockDevice, set: &dyn Fn(&str, u64, u64, &str)) -> Result<Value, String> {
    use flashpod_core::{card, fat, layout};
    let mut src = FileDev(File::open(src_path).map_err(|e| e.to_string())?);
    let old_size = src.len().map_err(|e| e.to_string())?;
    let size = dst.len().map_err(|e| e.to_string())?;
    // the firmware: the image's firmware partition, as flashpod would install it
    let mut mbr = vec![0u8; 512];
    src.read_at(0, &mut mbr).map_err(|e| e.to_string())?;
    let parts = layout::partitions(&mbr).ok_or("the card has no partition table: partition it first")?;
    let (fwp, _, _) = layout::layout_facts(&parts, old_size);
    let fwp = fwp.ok_or("the card has no firmware partition: partition it first")?;
    let mut raw = vec![0u8; (fwp.sectors * 512) as usize];
    src.read_at(fwp.start * 512, &mut raw).map_err(|e| e.to_string())?;
    while raw.last() == Some(&0) {
        raw.pop();
    }
    let fw = card::firmware_bytes(&raw, true).map_err(|e| format!("the card's firmware: {}", fp_message(&e.to_json())))?;
    // the files, with their folders
    let before = run(&mut FileDev(File::open(src_path).map_err(|e| e.to_string())?), json!({"op": "tracks"}), &[], None).ok();
    let mut sfat = fat::open_image_fat(&mut src).map_err(|e| e.message)?;
    let (mut dirs, mut files) = (vec![], vec![]);
    let mut todo = vec![String::new()];
    while let Some(d) = todo.pop() {
        for e in sfat.listdir(&d)?.unwrap_or_default() {
            let p = if d.is_empty() { e.name.clone() } else { format!("{d}/{}", e.name) };
            if e.attr & 0x10 != 0 {
                dirs.push(p.clone());
                todo.push(p);
            } else {
                files.push((p, e.size as u64));
            }
        }
    }
    dirs.sort();
    let used: u64 = files.iter().map(|f| f.1).sum();
    // partition and format, with the firmware
    set("partition and format", 0, used, "");
    card::flash(dst, fw, card::FlashOptions { lba48: size / 512 > layout::LBA28_MAX, max_data_gb: None, format: true, arm: false })
        .map_err(|e| fp_message(&e.to_json()))?;
    // copy the files
    {
        let mut dfat = fat::open_image_fat(dst).map_err(|e| e.message)?;
        for d in &dirs {
            dfat.mkdir(d)?;
        }
        let mut done = 0u64;
        for (p, n) in &files {
            set("copy", done, used, p);
            let data = sfat.read_file(p)?.unwrap_or_default();
            dfat.write_file(p, &data).map_err(|e| {
                if e.to_lowercase().contains("full") || e.to_lowercase().contains("space") {
                    format!("{} is too small for the {:.0} MB of music and files on this card", gbs_str(size), used as f64 / 1e6)
                } else {
                    e
                }
            })?;
            done += n;
        }
        dfat.sync()?;
    }
    dst.flush().map_err(|e| e.to_string())?;
    // the same songs on both
    set("check", used, used, "");
    let after = run(dst, json!({"op": "tracks"}), &[], None).ok();
    let n = |v: &Option<Value>| v.as_ref().and_then(|v| v["tracks"].as_array().map(|a| a.len()));
    if n(&before) != n(&after) {
        return Err(format!("the copy does not check out: {:?} songs on the card image, {:?} on the copy", n(&before), n(&after)));
    }
    Ok(json!({"size": size, "from": old_size, "files": files.len(), "songs": n(&after)}))
}

fn gbs_str(b: u64) -> String {
    if b >= 1_000_000_000 { format!("{:.0} GB", b as f64 / 1e9) } else { format!("{:.0} MB", b as f64 / 1e6) }
}

fn collect_firmware(root: &Path, d: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(d) else { return };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        let n = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if depth == 0 && !n.starts_with('.') {
                collect_firmware(root, &p, 1, out);
            }
        } else if n.ends_with(".ipsw") || n.ends_with(".bin.gz") || n.starts_with("Firmware") {
            out.push(p);
        }
    }
}

/// Every audio file under `p` (or `p` itself), sorted the way a folder lists them, hidden files and folders left out.
fn walk_audio(p: &Path, out: &mut Vec<String>) {
    if p.is_file() {
        if p.file_name().map(|n| is_audio(&n.to_string_lossy())).unwrap_or(false) {
            out.push(p.display().to_string());
        }
        return;
    }
    let Ok(rd) = fs::read_dir(p) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|x| !x.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(true)).collect();
    entries.sort_by_key(|x| x.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());
    for e in entries {
        walk_audio(&e, out);
    }
}

/// Copy a card image, leaving its zero blocks as holes (cp --sparse=always), with progress.
fn copy_sparse(src: &File, to: &Path, st: &Arc<Mutex<Value>>) -> Result<(), ApiErr> {
    let size = src.metadata()?.len();
    let out = OpenOptions::new().write(true).create_new(true).open(to)?;
    let r = (|| -> io::Result<()> {
        out.set_len(size)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut at = 0u64;
        while at < size {
            let n = src.read_at(&mut buf, at)?;
            if n == 0 {
                break;
            }
            // whole 4 KB blocks of zeros stay holes
            for (i, blk) in buf[..n].chunks(4096).enumerate() {
                if blk.iter().any(|&b| b != 0) {
                    out.write_all_at(blk, at + (i * 4096) as u64)?;
                }
            }
            at += n as u64;
            let mut o = st.lock().unwrap();
            o["stage"] = json!("copy");
            o["done"] = json!(at);
            o["total"] = json!(size);
        }
        out.sync_all()
    })();
    if let Err(e) = r {
        let _ = fs::remove_file(to);
        return Err(e.into());
    }
    Ok(())
}

/// What the page asks of the card store (web.py's _api): (status, JSON).
pub fn api(store: &Arc<Store>, method: &str, path: &str, q: &HashMap<String, String>, body: &Value) -> (u16, Value) {
    let parts: Vec<String> = path.split('/').skip(2).map(pct_decode).collect();
    let p: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
    let r: Result<Value, ApiErr> = (|| match (method, p.as_slice()) {
        ("GET", ["cards"]) => Ok(store.list()),
        ("GET", ["cards", n]) => store.summary(n),
        ("GET", ["cards", n, "tracks"]) => store.tracks(n),
        ("GET", ["cards", n, "burn"]) => store.burn(n),
        ("GET", ["folders"]) => store.folders(&pct_decode(q.get("path").map(|s| s.as_str()).unwrap_or(""))),
        ("POST", ["music-root"]) => store.set_music_root(body["path"].as_str()),
        ("GET", ["music"]) => store.music(&pct_decode(q.get("path").map(|s| s.as_str()).unwrap_or(""))),
        ("POST", ["boot"]) => {
            let card = body["card"].as_str().map(|s| s.to_string());
            if let Some(n) = &card {
                store.path(n, true)?;
            }
            let keep = body["keep"].as_bool().unwrap_or(true) && card.as_deref() != Some(BUILTIN) && card.is_some();
            let card = card.unwrap_or_else(|| BUILTIN.to_string());
            if let Some(f) = store.restart.lock().unwrap().as_ref() {
                f(json!({"do": "restart", "card": card, "keep": keep}));
            }
            Ok(json!({"ok": true}))
        }
        ("POST", ["cards", n, "then"]) => then_op(store, n, body),
        ("POST", ["cards", n, "cancel"]) => Ok(store.cancel(n)),
        ("POST", ["cards", n, "clear-uploads"]) => store.clear_uploads(n),
        ("POST", ["cards", n, op]) => store.start(n, op, body.clone()).map(|st| json!({"ok": true, "op": st})),
        _ => Err(ApiErr::NotFound),
    })();
    match r {
        Ok(v) => (200, v),
        Err(ApiErr::Card(m)) => (400, json!({"error": m})),
        Err(ApiErr::Other(m)) => (500, json!({"error": m})),
        Err(ApiErr::NotFound) => (404, json!({"error": "not found"})),
    }
}

/// An operation on the card the iPod runs: the iPod moves to the built-in card, the operation runs, and if it succeeds the iPod
/// boots the card again with its changes kept.
fn then_op(store: &Arc<Store>, name: &str, body: &Value) -> Result<Value, ApiErr> {
    let op = body["op"].as_str().unwrap_or("").to_string();
    if !["init", "rebuild", "format", "partition", "flash", "add", "remove", "resize"].contains(&op.as_str()) {
        return card_err("no such operation");
    }
    store.path(name, true)?;
    let (store, name, args) = (store.clone(), name.to_string(), body["args"].clone());
    if let Some(f) = store.restart.lock().unwrap().as_ref() {
        f(json!({"do": "restart", "card": BUILTIN, "keep": false, "remember": false}));
    }
    std::thread::spawn(move || {
        // wait for the iPod to let go of the card
        for _ in 0..200 {
            if store.booted.lock().unwrap().as_ref().map(|b| b.0 != name).unwrap_or(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        if store.start(&name, &op, args).is_err() {
            return;
        }
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            match store.op_state(&name) {
                Some(s) if s["state"] == "running" => continue,
                Some(s) if s["state"] == "done" => {
                    if let Some(f) = store.restart.lock().unwrap().as_ref() {
                        f(json!({"do": "restart", "card": name, "keep": true}));
                    }
                    break;
                }
                _ => break,
            }
        }
    });
    Ok(json!({"ok": true}))
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
