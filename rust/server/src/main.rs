//! emupod: the iPod emulator behind the web page you open on your phone, as one program.
//!
//!     emupod web [--host 127.0.0.1] [--port 8797] [--cards ~/emupod-cards] [--card NAME] [--discard] [--notice TEXT]
//!                [--nbd 127.0.0.1:10809 | off] [--iscsi 127.0.0.1:3260 | off] [--iscsi-secret FILE] [--orb-sectors N] [--automount SOCKET | off]
//!     sudo emupod setup-automount [--user NAME] [--nbd 127.0.0.1:10809] [--at /media/emupod]   (the mount service; see mount_service)
//!     sudo emupod remove-automount
//!
//! It serves the same page and answers the same requests as the Python server (emupod/web.py), so the page is shared.
mod automount;
mod burn;
mod card;
mod cards;
mod cover;
mod disk;
mod host_audio;
mod live;
mod iscsi;
mod mount_service;
mod nbd;

use cards::Store;
use live::{Card, Shared};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("../../../emupod/pages/ipod.html");
const CARDS_PAGE: &str = include_str!("../../../emupod/pages/cards.html");
const CHICAGO: &[u8] = include_bytes!("../../../emupod/site/chicagoflf.ttf");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// SIGTERM/SIGINT seen: the server stops once no card is being burned or changed (a stop mid-burn would leave a half-written card).
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn on_stop(_: i32) {
    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
}
extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

struct Opts {
    host: String,
    port: u16,
    cards: String,
    card: Option<String>,
    discard: bool,
    notice: String,
    music_root: Option<String>,
    firmware_dirs: Vec<String>,
    jack: bool,
    audio_cmd: Option<String>,
    demo_card: Option<String>,
    /// where the iPod in disk mode is served as a block device (None: not at all)
    nbd: Option<String>,
    /// where it is served as an iSCSI target (None: not at all), and the file with its CHAP secret
    iscsi: Option<String>,
    iscsi_secret: Option<String>,
    orb_sectors: u32,
    /// the mount service's socket (None: no automatic mounting)
    automount: Option<String>,
}

fn usage() -> ! {
    eprintln!("usage: emupod web [--host 127.0.0.1] [--port 8797] [--cards DIR] [--card NAME] [--discard] [--music-root DIR] [--firmware-dir DIR]... [--jack] [--audio-cmd CMD] [--notice TEXT] [--nbd ADDR:PORT|off] [--iscsi ADDR:PORT|off] [--iscsi-secret FILE] [--orb-sectors N] [--automount SOCKET|off]\n       sudo emupod setup-automount [--user NAME] [--nbd ADDR:PORT] [--at DIR]\n       sudo emupod remove-automount");
    std::process::exit(2)
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".into())
}

fn parse() -> Opts {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|a| a == "--version").unwrap_or(false) {
        println!("emupod {VERSION}");
        std::process::exit(0)
    }
    match args.first().map(|a| a.as_str()) {
        Some("web") => {}
        // the mount service, and setting it up (root)
        Some("mount-service") => std::process::exit(mount_service::run(&args[1..])),
        Some("setup-automount") => std::process::exit(mount_service::setup(&args[1..])),
        Some("remove-automount") => std::process::exit(mount_service::remove(&args[1..])),
        _ => usage(),
    }
    let mut o = Opts { host: "127.0.0.1".into(), port: 8797, cards: format!("{}/emupod-cards", home()), card: None, discard: false, notice: String::new(), music_root: None, firmware_dirs: vec![], jack: false, audio_cmd: None, demo_card: None,
                      nbd: Some("127.0.0.1:10809".into()), iscsi: Some("127.0.0.1:3260".into()), iscsi_secret: None, orb_sectors: pp5002::sbp2::MAX_SECTORS,
                      automount: Some(mount_service::SOCKET.into()) };
    let mut i = 1;
    while i < args.len() {
        let val = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| usage());
        match args[i].as_str() {
            "gen1" => {}
            "--host" => { o.host = val(i); i += 1 }
            "--port" => { o.port = val(i).parse().unwrap_or_else(|_| usage()); i += 1 }
            "--cards" => { o.cards = val(i).replacen('~', &home(), 1); i += 1 }
            "--card" => { o.card = Some(val(i)); i += 1 }
            "--notice" => { o.notice = val(i); i += 1 }
            "--music-root" => { o.music_root = Some(val(i).replacen('~', &home(), 1)); i += 1 }
            "--firmware-dir" => { o.firmware_dirs.push(val(i).replacen('~', &home(), 1)); i += 1 }
            "--audio-cmd" => { o.audio_cmd = Some(val(i)); i += 1 }
            "--jack" => o.jack = true,
            "--nbd" => { o.nbd = Some(val(i)).filter(|v| v != "off"); i += 1 }
            "--automount" => { o.automount = Some(val(i)).filter(|v| v != "off"); i += 1 }
            "--iscsi" => { o.iscsi = Some(val(i)).filter(|v| v != "off"); i += 1 }
            "--iscsi-secret" => { o.iscsi_secret = Some(val(i).replacen('~', &home(), 1)); i += 1 }
            "--orb-sectors" => { o.orb_sectors = val(i).parse().ok().filter(|n| (1..=pp5002::sbp2::MAX_SECTORS).contains(n)).unwrap_or_else(|| usage()); i += 1 }
            "--demo-card" => { o.demo_card = Some(val(i).replacen('~', &home(), 1)); i += 1 } // the built-in card from a file, not the demo site
            "--flashpod" => i += 1, // the Python server's: flashpod is built in here
            "--discard" => o.discard = true,
            "-h" | "--help" => usage(),
            other => { eprintln!("emupod: unknown option {other}"); usage() }
        }
        i += 1;
    }
    o
}

/// The card to boot: --card, else the one booted last (.last-card.json), else the built-in card.
fn pick_card(o: &Opts, store: &Store) -> Card {
    let (name, keep) = match &o.card {
        Some(n) if store.path(n, true).is_ok() => (n.clone(), !o.discard),
        Some(n) => {
            eprintln!("[web] no card {n:?} in {}: booting the built-in card", store.root.display());
            (cards::BUILTIN.to_string(), false)
        }
        None => match store.load_last() {
            Some((n, k)) => (n, k && !o.discard),
            None => (cards::BUILTIN.to_string(), false),
        },
    };
    card_of(store, &name, keep)
}

fn card_of(store: &Store, name: &str, keep: bool) -> Card {
    Card { name: name.to_string(), path: store.root.join(format!("{name}.img")).display().to_string(), keep: keep && name != cards::BUILTIN }
}

// ------------------------------------------------------------------------------------------------ HTTP

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn send(req: Request, code: u16, ctype: &str, body: Vec<u8>) {
    let r = Response::from_data(body).with_status_code(code).with_header(header("Content-Type", ctype)).with_header(header("Cache-Control", "no-store"));
    let _ = req.respond(r);
}

fn send_json(req: Request, code: u16, v: &Value) {
    send(req, code, "application/json", v.to_string().into_bytes())
}

fn query(url: &str) -> std::collections::HashMap<String, String> {
    url.split_once('?').map(|(_, q)| q).unwrap_or("").split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// /audio?cid=: the sound as it plays, s16le stereo at 44,100 Hz, chunked; a keepalive of 4 zero bytes after 15 s of nothing.
struct AudioStream {
    shared: Arc<Shared>,
    cid: String,
    c: Arc<live::Client>,
    buf: Vec<u8>,
    pos: usize,
    last: Instant,
}
impl Read for AudioStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut q = self.c.q.lock().unwrap();
            while q.blocks.is_empty() && q.live && Instant::now() < deadline {
                // the state's lock is never taken while holding this queue's: the emulator thread holds the clients' list while it takes
                // each queue, and /state holds the state while it counts the clients (all three waiting on each other froze the server)
                drop(q);
                if self.shared.st.lock().unwrap().phase == "ended" {
                    return Ok(0);
                }
                q = self.c.q.lock().unwrap();
                if !q.blocks.is_empty() || !q.live {
                    break;
                }
                q = self.c.cond.wait_timeout(q, Duration::from_millis(500)).unwrap().0;
            }
            if !q.live {
                return Ok(0);
            }
            let gap = self.last.elapsed();
            if gap > Duration::from_millis(50) && !q.blocks.is_empty() && q.sent > 0 {
                q.slow.push_back((Instant::now(), gap.as_millis() as u64));
            }
            self.buf = if q.blocks.is_empty() { vec![0; 4] } else { q.blocks.drain(..).flatten().collect() };
            self.pos = 0;
            q.sent += self.buf.len() as u64;
            let n = self.buf.len();
            q.writes.push_back((Instant::now(), n));
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        self.last = Instant::now();
        Ok(n)
    }
}
impl Drop for AudioStream {
    fn drop(&mut self) {
        self.c.q.lock().unwrap().live = false;
        self.shared.audio.forget(&self.cid, &self.c);
    }
}

/// An hour of 8 kHz 8-bit mono silence (the page loops it so iOS shows the lock-screen controls), with Range support.
fn silence(req: Request) {
    const N: u64 = 8000 * 3600;
    let total = 44 + N;
    let mut head = Vec::with_capacity(44);
    head.extend_from_slice(b"RIFF");
    head.extend_from_slice(&((36 + N) as u32).to_le_bytes());
    head.extend_from_slice(b"WAVEfmt ");
    head.extend_from_slice(&16u32.to_le_bytes());
    head.extend_from_slice(&[1, 0, 1, 0]);
    head.extend_from_slice(&8000u32.to_le_bytes());
    head.extend_from_slice(&8000u32.to_le_bytes());
    head.extend_from_slice(&[1, 0, 8, 0]);
    head.extend_from_slice(b"data");
    head.extend_from_slice(&(N as u32).to_le_bytes());
    let range = req.headers().iter().find(|h| h.field.equiv("Range")).map(|h| h.value.as_str().to_string());
    let (start, end, code) = match range.as_deref().and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')) {
        Some((a, b)) => {
            let s: u64 = a.parse().unwrap_or(0).min(total - 1);
            let e: u64 = b.parse().unwrap_or(total - 1).min(total - 1).min(s + 4 * 1024 * 1024 - 1);
            (s, e.max(s), 206)
        }
        None => (0, total - 1, 200),
    };
    let body: Vec<u8> = (start..=end).map(|i| if i < 44 { head[i as usize] } else { 0x80 }).collect();
    let mut r = Response::from_data(body).with_status_code(code).with_header(header("Content-Type", "audio/wav"))
        .with_header(header("Accept-Ranges", "bytes")).with_header(header("Cache-Control", "public, max-age=86400"));
    if code == 206 {
        r = r.with_header(header("Content-Range", &format!("bytes {start}-{end}/{total}")));
    }
    let _ = req.respond(r);
}

fn handle(mut req: Request, shared: Arc<Shared>, store: Arc<Store>, burner: Arc<burn::Burner>) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("/").to_string();
    let q = query(&url);
    match (req.method().clone(), path.as_str()) {
        (Method::Get, "/") | (Method::Get, "/index.html") => send(req, 200, "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        (Method::Get, p) if p == "/cards" || p.starts_with("/cards/") => send(req, 200, "text/html; charset=utf-8", CARDS_PAGE.as_bytes().to_vec()),
        (Method::Get, "/silence.wav") => silence(req),
        (Method::Get, "/chicagoflf.ttf") => send(req, 200, "font/ttf", CHICAGO.to_vec()),
        (Method::Get, "/screen.png") => {
            let png = {
                let mut st = shared.st.lock().unwrap();
                let key = st.digest.clone().unwrap_or_default();
                match &st.cover {
                    Some((k, png)) if *k == key => png.clone(),
                    _ => {
                        let png = cover::png(if st.px.is_empty() { None } else { Some(&st.px) });
                        st.cover = Some((key, png.clone()));
                        png
                    }
                }
            };
            send(req, 200, "image/png", png)
        }
        (Method::Get, "/ping") => {
            let ipod = shared.st.lock().unwrap().ipod.clone().unwrap_or(Value::Null);
            let audio = shared.audio.stats(q.get("cid").map(|s| s.as_str()).unwrap_or(""));
            send_json(req, 200, &json!({"t": (live::now_s() * 1000.0).round() / 1000.0, "audio": audio, "ipod": ipod}))
        }
        (Method::Get, "/state") => {
            let wait = q.get("wait").and_then(|w| w.parse::<f64>().ok()).unwrap_or(0.0);
            let v = shared.poll(q.get("since").map(|s| s.as_str()).unwrap_or(""), q.get("ver").map(|s| s.as_str()).unwrap_or(""), wait);
            send_json(req, 200, &v)
        }
        (Method::Get, "/audio") => {
            let cid = q.get("cid").cloned().unwrap_or_default();
            let c = shared.audio.listen(&cid);
            let stream = AudioStream { shared: shared.clone(), cid, c, buf: vec![], pos: 0, last: Instant::now() };
            let r = Response::new(200.into(), vec![header("Content-Type", "application/octet-stream"), header("Cache-Control", "no-store"),
                                                   header("X-Sample-Rate", "44100")], stream, None, None);
            let _ = req.respond(r);
        }
        (Method::Get, "/api/audio/devices") => send_json(req, 200, &shared.host.devices()),
        (Method::Post, "/api/audio/device") => {
            let mut s = String::new();
            let _ = req.as_reader().take(1 << 16).read_to_string(&mut s);
            let id = serde_json::from_str::<Value>(&s).ok().and_then(|v| v["id"].as_str().map(|x| x.to_string())).unwrap_or_default();
            match shared.host.set_device(&id) {
                Ok(v) => send_json(req, 200, &v),
                Err(e) => send_json(req, 400, &json!({"error": e})),
            }
        }
        (Method::Get, "/api/burn/disks") => {
            // the removable disks, each checked against the room the card's files need
            let card = cards::pct_decode(q.get("card").map(|s| s.as_str()).unwrap_or(""));
            let size = store.path(&card, true).ok().and_then(|p| burn::needed(&p).ok()).unwrap_or(0);
            send_json(req, 200, &burn::disks(size))
        }
        (Method::Get, "/api/burn/status") => send_json(req, 200, &burner.status()),
        (Method::Get, "/api/stats/history") => {
            let snaps: Vec<Value> = shared.hist.lock().unwrap().iter().cloned().collect();
            send_json(req, 200, &json!({"snaps": snaps}))
        }
        (Method::Post, "/api/burn") => {
            let mut s = String::new();
            let _ = req.as_reader().take(1 << 16).read_to_string(&mut s);
            let b: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
            let (card, device) = (b["card"].as_str().unwrap_or(""), b["device"].as_str().unwrap_or(""));
            let r = store.path(card, true).map_err(|e| match e { cards::ApiErr::Card(m) | cards::ApiErr::Other(m) => m, cards::ApiErr::NotFound => "not found".into() })
                .and_then(|p| burner.start(card, &p, device));
            match r {
                Ok(v) => send_json(req, 200, &v),
                Err(m) => send_json(req, 400, &json!({"error": m})),
            }
        }
        (Method::Get, p) if p.starts_with("/api/firmware/") => {
            // a catalog firmware, for the offline app (it checks the catalog's sha256 itself)
            let file = cards::pct_decode(&p["/api/firmware/".len()..]);
            match store.fetch_firmware(&file, None) {
                Ok(b) => send(req, 200, "application/octet-stream", b),
                Err(_) => send_json(req, 404, &json!({"error": format!("no such firmware: {file}")})),
            }
        }
        (Method::Put, p) if p.starts_with("/api/cards/") && p.ends_with("/upload") => {
            let name = cards::pct_decode(p.trim_start_matches("/api/cards/").trim_end_matches("/upload"));
            let file = cards::pct_decode(q.get("file").map(|s| s.as_str()).unwrap_or(""));
            let len = req.body_length().unwrap_or(0) as u64;
            let r = store.upload(&name, &file, req.as_reader(), len);
            match r {
                Ok(v) => send_json(req, 200, &v),
                Err(cards::ApiErr::Card(m)) => send_json(req, 400, &json!({"error": m})),
                Err(cards::ApiErr::Other(m)) => send_json(req, 500, &json!({"error": m})),
                Err(cards::ApiErr::NotFound) => send_json(req, 404, &json!({"error": "not found"})),
            }
        }
        (m, p) if p.starts_with("/api/") => {
            let method = match m { Method::Get => "GET", Method::Post => "POST", Method::Put => "PUT", _ => "OTHER" };
            let mut body = Value::Null;
            if method == "POST" {
                let mut s = String::new();
                let _ = req.as_reader().take(1 << 20).read_to_string(&mut s);
                if !s.trim().is_empty() {
                    match serde_json::from_str(&s) {
                        Ok(v) => body = v,
                        Err(_) => return send_json(req, 400, &json!({"error": "bad json"})),
                    }
                } else {
                    body = json!({});
                }
            }
            let (code, v) = cards::api(&store, method, p, &q, &body);
            send_json(req, code, &v)
        }
        (Method::Post, "/cmd") => {
            let mut body = String::new();
            let _ = req.as_reader().take(1 << 20).read_to_string(&mut body);
            let Ok(mut c) = serde_json::from_str::<Value>(&body) else { return send_json(req, 400, &json!({"ok": false, "error": "bad json"})) };
            let ok = matches!(c["do"].as_str(), Some("wheel" | "edges" | "key" | "line" | "restart" | "jack"));
            if ok {
                if let Some(t) = c["text"].as_str() {
                    c["text"] = Value::String(t.chars().take(2000).collect());
                }
                if c["do"] == "line" && c["name"] == "cna" && c["level"].as_i64().unwrap_or(0) != 0 {
                    // the FireWire cable out: a mounted disk is unmounted first
                    let (sh, c) = (shared.clone(), c.clone());
                    if shared.mount.unplug(Box::new(move || sh.cmds.lock().unwrap().push_back(c))) {
                        return send_json(req, 200, &json!({"ok": true, "held": "unmounting first"}));
                    }
                }
                shared.cmds.lock().unwrap().push_back(c);
            }
            send_json(req, 200, &json!({"ok": ok}))
        }
        _ => send_json(req, 404, &json!({"error": "not found"})),
    }
}

fn main() {
    let o = parse();
    let store = match Store::new(&o.cards, o.music_root.as_deref(), &o.firmware_dirs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("emupod: the card folder {}: {e}", o.cards);
            std::process::exit(1)
        }
    };
    if let Err(e) = store.ensure_builtin(o.demo_card.as_deref()) {
        eprintln!("[cards] cannot get the built-in card ({}): {e}", cards::BUILTIN);
    }
    let mut card = pick_card(&o, &store);
    let mut notice = o.notice.clone();
    let data_dir = std::env::var("XDG_DATA_HOME").ok().filter(|s| !s.is_empty()).map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(home()).join(".local/share")).join("emupod");
    let host = host_audio::HostAudio::new(o.audio_cmd.as_deref(), data_dir.join("audio-device.json"));
    if o.jack {
        host.plug(true);
    }
    let shared = Shared::new(&notice, Some(card.clone()), host);
    *shared.orb_sectors.lock().unwrap() = o.orb_sectors;
    if let Some(a) = &o.nbd {
        match nbd::listen(shared.nbd.clone(), a) {
            Ok(()) => eprintln!("[nbd] disk mode is served at nbd://{a}/{}", nbd::EXPORT),
            Err(e) => eprintln!("[nbd] not serving disk mode: {e}"),
        }
        if let Some(sock) = &o.automount {
            shared.mount.start(sock.into(), shared.nbd.clone());
        }
    }
    if let Some(a) = &o.iscsi {
        // the CHAP secret: never logged; the page and the mount service read it from the file
        let path = o.iscsi_secret.as_ref().map(std::path::PathBuf::from).unwrap_or_else(|| data_dir.join("iscsi-secret"));
        match iscsi::secret(&path).and_then(|chap| iscsi::listen(shared.nbd.clone(), a, chap)) {
            Ok(()) => eprintln!("[iscsi] disk mode is served at iscsi://{a}/{}/0 (CHAP user {}, secret in {})", iscsi::TARGET, iscsi::CHAP_USER, path.display()),
            Err(e) => eprintln!("[iscsi] not serving disk mode: {e}"),
        }
    }
    {
        let shared = shared.clone();
        *store.restart.lock().unwrap() = Some(Box::new(move |c: Value| shared.cmds.lock().unwrap().push_back(c)));
    }
    let burner = Arc::new(burn::Burner::default());
    unsafe {
        signal(15, on_stop); // SIGTERM (systemd stop, kill)
        signal(2, on_stop); // SIGINT (Ctrl-C)
    }
    {
        let (shared, store, burner) = (shared.clone(), store.clone(), burner.clone());
        std::thread::spawn(move || {
            while !STOP.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            let mut told = false;
            while burner.status()["state"] == "running" || store.list()["cards"].as_array().map(|a| a.iter().any(|c| c["op"]["state"] == "running")).unwrap_or(false) {
                if !told {
                    eprintln!("[web] stopping once the card being written is done");
                    told = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            shared.mount.shutdown();
            shared.cmds.lock().unwrap().push_back(json!({"do": "quit"}));
        });
    }
    {
        // the statistics history: one snapshot a second, five minutes kept
        let shared = shared.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let snap = shared.st.lock().unwrap().ipod.clone();
            if let Some(s) = snap {
                let mut h = shared.hist.lock().unwrap();
                if h.back().map(|b| b["wall"] != s["wall"]).unwrap_or(true) {
                    h.push_back(s);
                    while h.len() > 300 {
                        h.pop_front();
                    }
                }
            }
        });
    }
    let addr = format!("{}:{}", o.host, o.port);
    let server = match Server::http(&addr) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("emupod: cannot listen on {addr}: {e}");
            std::process::exit(1)
        }
    };
    {
        let (server, shared, store, burner) = (server.clone(), shared.clone(), store.clone(), burner.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let (shared, store, burner) = (shared.clone(), store.clone(), burner.clone());
                std::thread::spawn(move || handle(req, shared, store, burner));
            }
        });
    }
    eprintln!("[web] http://{addr}/");
    let mut remember = true;
    loop {
        if !std::path::Path::new(&card.path).exists() {
            card = card_of(&store, cards::BUILTIN, false);
        }
        // the card's lock: shared when its changes are thrown away, exclusive when they are written back (flashpod checks the same)
        let lock = std::fs::File::open(&card.path).and_then(|f| {
            let r = if card.keep { f.try_lock() } else { f.try_lock_shared() };
            r.map(|_| f).map_err(|e| std::io::Error::other(e.to_string()))
        });
        let lock = match lock {
            Ok(f) => f,
            Err(e) if card.name != cards::BUILTIN => {
                notice = format!("Card \u{201c}{}\u{201d} is in use by another program ({e}); the built-in card was booted instead.", card.name);
                card = card_of(&store, cards::BUILTIN, false);
                remember = false;
                continue;
            }
            Err(e) => {
                eprintln!("emupod: the built-in card cannot be opened: {e}");
                std::process::exit(1)
            }
        };
        eprintln!("[web] booting {:?}{}", card.name, if card.keep { "" } else { " (changes discarded)" });
        *store.booted.lock().unwrap() = Some((card.name.clone(), card.keep));
        if remember {
            store.save_last(&card.name, card.keep);
        }
        shared.st.lock().unwrap().notice = notice.clone();
        let next = match live::run(&shared, &card) {
            Ok(n) => n,
            Err(e) if card.name != cards::BUILTIN => {
                // a card whose firmware does not start: the built-in card, and the page says why
                eprintln!("[web] {:?} did not start: {e}", card.name);
                notice = format!("Card \u{201c}{}\u{201d} did not start: {e}", card.name);
                drop(lock);
                card = card_of(&store, cards::BUILTIN, false);
                remember = false;
                continue;
            }
            Err(e) => {
                eprintln!("emupod: {e}");
                std::process::exit(1)
            }
        };
        drop(lock);
        *store.booted.lock().unwrap() = None;
        shared.audio.end_all();
        if next["do"] == "quit" {
            break;
        }
        // restart: the same card ("Restart the iPod"), or the one the Cards page asked for
        if let Some(name) = next["card"].as_str() {
            card = card_of(&store, name, next["keep"].as_bool().unwrap_or(true));
        }
        remember = next["remember"] != false;
        notice = String::new();
        let mut st = shared.st.lock().unwrap();
        st.phase = "booting".into();
        st.ver += 1;
        drop(st);
        shared.cond.notify_all();
        eprintln!("[web] restarting {:?}", card.name);
    }
}
