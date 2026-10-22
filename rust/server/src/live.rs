//! The iPod running: one emulator thread drives the machine in real time (web.py's Live and recording.Driver, the same behaviour),
//! and the HTTP threads read what it publishes. Commands from the page (/cmd) queue up and go in between ticks.
use pp5002::{Config, Machine};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const W: usize = 160;
/// a key release waits until the key has been down this many ticks (the firmware misses shorter presses)
const MIN_HOLD_STEPS: u64 = 6;
/// the buttons: port 0, one bit each
const KEYS: [(&str, u32); 5] = [("ff", 1 << 0), ("select", 1 << 1), ("play", 1 << 2), ("rew", 1 << 3), ("menu", 1 << 4)];
/// the event lines: (name, port, mask)
const LINES: [(&str, usize, u32); 4] = [("hold", 0, 1 << 5), ("firewire", 2, 1 << 4), ("cna", 2, 1 << 7), ("battery", 2, 1 << 3)];
/// the volume the iPod starts at, of 127 (90%; the firmware's own is 53)
pub const START_VOLUME: u32 = 114;
const GLYPHS: &str = include_str!("../../../emupod/clock_glyphs.json");

pub fn now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The card the iPod boots: its name, image path, and whether its changes are kept.
#[derive(Clone, Debug)]
pub struct Card {
    pub name: String,
    pub path: String,
    pub keep: bool,
}

// ------------------------------------------------------------------------------------------------ sound to the browsers

/// One page's sound stream (/audio?cid=): blocks of s16le stereo, at most 200 queued (the oldest go first).
pub struct Client {
    pub q: Mutex<ClientQ>,
    pub cond: Condvar,
}
#[derive(Default)]
pub struct ClientQ {
    pub blocks: VecDeque<Vec<u8>>,
    pub sent: u64,
    pub dropped: u64,
    pub live: bool,
    /// (when, bytes) of the writes in the last few seconds, for the rate
    pub writes: VecDeque<(Instant, usize)>,
    /// (when, ms) of slow writes (over 50 ms) in the last few seconds
    pub slow: VecDeque<(Instant, u64)>,
}

#[derive(Default)]
pub struct Audio {
    pub clients: Mutex<HashMap<String, Arc<Client>>>,
}
impl Audio {
    pub fn feed(&self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        for c in self.clients.lock().unwrap().values() {
            let mut q = c.q.lock().unwrap();
            if q.blocks.len() >= 200 {
                if let Some(old) = q.blocks.pop_front() {
                    q.dropped += old.len() as u64;
                }
            }
            q.blocks.push_back(b.to_vec());
            c.cond.notify_all();
        }
    }
    pub fn listen(&self, cid: &str) -> Arc<Client> {
        let c = Arc::new(Client { q: Mutex::new(ClientQ { live: true, ..Default::default() }), cond: Condvar::new() });
        if let Some(old) = self.clients.lock().unwrap().insert(cid.to_string(), c.clone()) {
            old.q.lock().unwrap().live = false;
            old.cond.notify_all();
        }
        c
    }
    pub fn forget(&self, cid: &str, c: &Arc<Client>) {
        let mut m = self.clients.lock().unwrap();
        if m.get(cid).map(|x| Arc::ptr_eq(x, c)).unwrap_or(false) {
            m.remove(cid);
        }
    }
    pub fn end_all(&self) {
        for c in self.clients.lock().unwrap().values() {
            c.q.lock().unwrap().live = false;
            c.cond.notify_all();
        }
    }
    /// what /ping reports for one page's stream (web_audio.client_stats)
    pub fn stats(&self, cid: &str) -> Value {
        let m = self.clients.lock().unwrap();
        let Some(c) = m.get(cid) else { return Value::Null };
        let mut q = c.q.lock().unwrap();
        let now = Instant::now();
        while q.writes.front().map(|w| now - w.0 > Duration::from_secs(3)).unwrap_or(false) {
            q.writes.pop_front();
        }
        while q.slow.front().map(|w| now - w.0 > Duration::from_secs(5)).unwrap_or(false) {
            q.slow.pop_front();
        }
        let rate = q.writes.iter().map(|w| w.1).sum::<usize>() as f64 / 3.0;
        let queued: usize = q.blocks.iter().map(|b| b.len()).sum();
        json!({"sent": q.sent, "dropped": q.dropped, "queued": queued, "rate_bps": rate.round(),
               "blocked_ms": q.slow.iter().map(|s| s.1).max().unwrap_or(0), "live": q.live})
    }
}

// ------------------------------------------------------------------------------------------------ what the page reads

pub struct Pub {
    pub phase: String,
    pub step: u64,
    pub emu_us: u64,
    pub hold: u8,
    pub backlight: u8,
    pub audio: bool,
    pub clicks: u64,
    pub np: Option<(u32, u32)>,
    pub keys: Vec<String>,
    pub lines: Vec<(String, u8)>,
    pub recent: VecDeque<Value>,
    pub ver: u64,
    pub notice: String,
    pub card: Option<Card>,
    pub dir: String,
    /// sha1 of the screen's levels, 12 hex digits, and the screen packed 4 pixels a byte
    pub digest: Option<String>,
    pub packed: Vec<u8>,
    pub px: Vec<u8>,
    pub cover: Option<(String, Vec<u8>)>,
    pub ipod: Option<Value>,
    sig: String,
}

pub struct Shared {
    pub st: Mutex<Pub>,
    pub cond: Condvar,
    pub cmds: Mutex<VecDeque<Value>>,
    pub audio: Audio,
    /// the sound on this machine's own output ("audio on host")
    pub host: crate::host_audio::HostAudio,
    /// the iPod's counters once a second, the last five minutes: a page that opens (or reloads) draws its charts from them
    pub hist: Mutex<VecDeque<Value>>,
}

impl Shared {
    pub fn new(notice: &str, card: Option<Card>, host: crate::host_audio::HostAudio) -> Arc<Shared> {
        Arc::new(Shared {
            st: Mutex::new(Pub {
                phase: "booting".into(),
                step: 0,
                emu_us: 0,
                hold: 0,
                backlight: 0,
                audio: false,
                clicks: 0,
                np: None,
                keys: vec![],
                lines: vec![],
                recent: VecDeque::new(),
                ver: 0,
                notice: notice.to_string(),
                card,
                dir: String::new(),
                digest: None,
                packed: vec![],
                px: vec![],
                cover: None,
                ipod: None,
                sig: String::new(),
            }),
            cond: Condvar::new(),
            cmds: Mutex::new(VecDeque::new()),
            audio: Audio::default(),
            host,
            hist: Mutex::new(VecDeque::new()),
        })
    }

    /// The /state answer (Live.poll): waits up to `wait` s while the screen is still `since`, the state still version `ver` and
    /// the iPod still running; the packed screen (base64) comes with it when it is not the one the page has.
    pub fn poll(&self, since: &str, ver: &str, wait: f64) -> Value {
        let deadline = Instant::now() + Duration::from_secs_f64(wait.clamp(0.0, 20.0));
        let mut st = self.st.lock().unwrap();
        loop {
            let same = st.digest.as_deref().unwrap_or("") == since && st.ver.to_string() == ver && st.phase != "ended";
            let now = Instant::now();
            if !same || now >= deadline {
                break;
            }
            st = self.cond.wait_timeout(st, deadline - now).unwrap().0;
        }
        let mut v = state_json(&st);
        v["jack"] = self.host.state(self.audio.clients.lock().unwrap().len());
        if let Some(d) = &st.digest {
            if d != since {
                use base64::Engine;
                v["px"] = Value::String(base64::engine::general_purpose::STANDARD.encode(&st.packed));
            }
        }
        v
    }
}

pub fn state_json(st: &Pub) -> Value {
    let mut lines = Map::new();
    for (k, v) in &st.lines {
        lines.insert(k.clone(), json!(v));
    }
    json!({
        "phase": st.phase, "step": st.step, "emu_us": st.emu_us, "inputs": 0, "screens": 0, "hold": st.hold, "audio": st.audio,
        "clicks": st.clicks, "np": st.np.map(|(a, b)| json!([a, b])).unwrap_or(Value::Null), "keys": st.keys, "lines": lines,
        "recent": st.recent, "shots": [], "dir": st.dir, "label": "", "ver": st.ver, "notice": st.notice,
        "card": st.card.as_ref().map(|c| json!({"name": c.name, "keep": c.keep})).unwrap_or(Value::Null),
        "backlight": st.backlight, "screen": st.digest,
    })
}

// ------------------------------------------------------------------------------------------------ the driver

struct Driver {
    m: Machine,
    screen: pp5002::lcd::Screen,
    step_n: u64,
    down: HashMap<String, u64>,
    hold: u8,
    changed: bool,
    digest: String,
}

fn digest(px: &[u8]) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(px);
    h.digest().to_string()[..12].to_string()
}

impl Driver {
    fn step(&mut self) {
        self.m.settle(1, 1 << 30);
        self.step_n += 1;
        self.update_screen();
    }
    fn update_screen(&mut self) {
        let log = std::mem::take(&mut self.m.s.lcd_log);
        if self.screen.update(&log) {
            self.changed = true;
            self.digest = digest(&self.screen.px);
        }
    }
    fn edge(&mut self, port: usize, mask: u32, level: bool) -> bool {
        self.m.set_pin(port, mask, level);
        self.m.edge(port, mask)
    }
    fn ready(&self, c: &Value) -> u64 {
        if c["do"] == "key" && c["phase"] == "up" {
            if let Some(d) = c["key"].as_str().and_then(|k| self.down.get(k)) {
                return d + MIN_HOLD_STEPS;
            }
        }
        self.step_n
    }
    /// One command from the page; the event record for `recent`, or None if it changed nothing.
    fn apply(&mut self, c: &Value) -> Option<Value> {
        let at = (self.step_n, self.m.now(), self.digest.clone());
        let done = self.inject(c)?;
        let mut rec = json!({"step": at.0, "emu_us": at.1, "screen": at.2});
        if let (Some(r), Some(d)) = (rec.as_object_mut(), done.as_object()) {
            for (k, v) in d {
                r.insert(k.clone(), v.clone());
            }
        }
        Some(rec)
    }
    fn inject(&mut self, c: &Value) -> Option<Value> {
        let n = c["n"].as_i64().or_else(|| c["n"].as_f64().map(|f| f as i64)).unwrap_or(0);
        match c["do"].as_str()? {
            "wheel" if n != 0 => {
                let seq: [(u32, bool); 4] =
                    if n > 0 { [(0x80, true), (0x40, true), (0x80, false), (0x40, false)] } else { [(0x40, true), (0x80, true), (0x40, false), (0x80, false)] };
                for _ in 0..n.unsigned_abs().min(400) {
                    for (mask, level) in seq {
                        self.edge(0, mask, level);
                        self.step();
                    }
                }
                Some(json!({"do": "wheel", "n": n}))
            }
            "edges" if n != 0 => {
                let n = n.clamp(-400, 400);
                for _ in 0..n.unsigned_abs() {
                    let (a, b) = (self.m.pin(0, 0x80), self.m.pin(0, 0x40));
                    let (mask, level) = if n > 0 {
                        match (a, b) { (false, false) => (0x80, true), (true, false) => (0x40, true), (true, true) => (0x80, false), (false, true) => (0x40, false) }
                    } else {
                        match (a, b) { (false, false) => (0x40, true), (false, true) => (0x80, true), (true, true) => (0x40, false), (true, false) => (0x80, false) }
                    };
                    self.edge(0, mask, level);
                    self.step();
                }
                Some(json!({"do": "edges", "n": n}))
            }
            "key" => {
                let k = c["key"].as_str()?;
                let mask = KEYS.iter().find(|x| x.0 == k)?.1;
                let down = match c["phase"].as_str()? { "down" => true, "up" => false, _ => return None };
                if down == self.down.contains_key(k) {
                    return None;
                }
                self.edge(0, mask, !down);
                if down {
                    self.down.insert(k.to_string(), self.step_n);
                } else {
                    self.down.remove(k);
                }
                Some(json!({"do": "key", "key": k, "phase": if down { "down" } else { "up" }}))
            }
            "line" => {
                let name = c["name"].as_str()?;
                let &(_, port, mask) = LINES.iter().find(|x| x.0 == name)?;
                let level = c["level"].as_i64().unwrap_or(0) != 0;
                if self.m.pin(port, mask) == level || !self.edge(port, mask, level) {
                    return None;
                }
                if name == "hold" {
                    self.hold = level as u8;
                }
                Some(json!({"do": "line", "name": name, "level": level as u8}))
            }
            _ => None,
        }
    }
    fn stats(&mut self) -> Value {
        let s = &self.m.s;
        let mut v = json!({
            "now_us": self.m.now(), "cpu_instr": s.cpu_instr, "cop_instr": s.cop_instr, "cpu_skipped": s.skipped[0], "cop_skipped": s.skipped[1],
            "cpu_ipus": s.cfg.ipus, "cop_ipus": s.cfg.cop_ipus, "ata_cmds": s.ata.n_cmds, "ata_reads": s.ata.n_reads, "ata_writes": s.ata.n_writes,
            "ata_sectors_read": s.ata.sectors_read, "ata_sectors_written": s.ata.sectors_written, "lcd_writes": s.lcd_count,
            "audio_fiqs": s.audio.fiqs, "audio_late": s.audio.late, "ticks": s.ticks, "halts": s.sleeps, "clicks": s.clicks.len(),
            "cop_asleep": s.cop_asleep as u8, "irq_pending": s.pending, "sig_sent": s.sig_sent, "sig_taken": s.sig_taken,
            "cop_wakes": s.cop_wakes, "cop_sleeps": s.cop_sleeps, "irqs": s.irqs,
            "host_cpu_s": thread_cpu_s(), "wall": (now_s() * 1000.0).round() / 1000.0, "underruns": 0,
        });
        if let Some(u) = self.m.fw_underruns() {
            v["fw_underruns"] = json!(u);
        }
        v
    }
}

/// CPU time this thread has used, in seconds (the "host" figure beside Emulator speed)
fn thread_cpu_s() -> f64 {
    let Ok(s) = std::fs::read_to_string("/proc/thread-self/stat") else { return 0.0 };
    // the fields after the command name (which is in parentheses and may hold spaces): utime is the 12th, stime the 13th
    let Some(rest) = s.rfind(')').map(|i| &s[i + 2..]) else { return 0.0 };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let t = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
    (t(11) + t(12)) / 100.0
}

/// The volume the firmware set, as the gain the page's sound gets (web_audio.gains): silent at 0x2f and below, 0 dB at 0x7b.
fn gain(v: u32) -> f64 {
    if v <= 0x2f { 0.0 } else { 10f64.powf((v as f64 - 123.0) / 20.0).min(1.0) }
}

/// (elapsed s, remaining s) read off Now Playing's time row with the firmware's own glyphs (recording.read_clock), or None.
fn read_clock(px: &[u8], glyphs: &HashMap<String, String>) -> Option<(u32, u32)> {
    let rows = 111..120;
    let cols: Vec<bool> = (0..W).map(|x| rows.clone().any(|y| px[y * W + x] >= 2)).collect();
    let (mut left, mut right) = (String::new(), String::new());
    let mut any = false;
    let mut x = 0;
    while x < W {
        if cols[x] {
            let s = x;
            while x < W && cols[x] {
                x += 1;
            }
            let mut key = String::new();
            for y in rows.clone() {
                for i in s..x {
                    key.push_str(if px[y * W + i] >= 2 { "01" } else { "00" });
                }
            }
            let ch = glyphs.get(&key)?;
            if s < 80 { left.push_str(ch) } else { right.push_str(ch) }
            any = true;
        }
        x += 1;
    }
    if !any {
        return None;
    }
    let secs = |t: &str| -> Option<u32> {
        let (m, s) = t.trim_start_matches('-').split_once(':')?;
        Some(m.parse::<u32>().ok()? * 60 + s.parse::<u32>().ok()?)
    };
    Some((secs(&left)?, secs(&right)?))
}

fn pack(px: &[u8]) -> Vec<u8> {
    px.chunks(4).map(|p| (p[0] << 6) | (p[1] << 4) | (p[2] << 2) | p[3]).collect()
}

/// Why the emulator thread stopped: a command from the page ("restart" with its body, "quit").
pub type Next = Value;

/// Boot `card` and run it in real time until a restart or quit command; returns that command.
pub fn run(shared: &Arc<Shared>, card: &Card) -> Result<Next, String> {
    let osos = crate::card::read_osos(&card.path).map_err(|e| format!("the card's firmware: {e}"))?;
    let cfg = Config {
        cop_ipus: 26,
        interleave_playing: 300,
        interleave_settle: 200_000,
        audio_fpf: None,
        start_volume: Some(START_VOLUME),
        persist: card.keep,
        slice_clock: true,
        lazy_map: true,
        ..Config::default()
    };
    let m = Machine::new(&osos, &card.path, cfg).map_err(|e| format!("{}: {e}", card.path))?;
    let glyphs: HashMap<String, String> = serde_json::from_str(GLYPHS).unwrap_or_default();
    let mut d = Driver { m, screen: pp5002::lcd::Screen::default(), step_n: 0, down: HashMap::new(), hold: 0, changed: false, digest: String::new() };
    d.m.boot();
    d.update_screen();
    d.hold = d.m.pin(0, 1 << 5) as u8;
    {
        let mut st = shared.st.lock().unwrap();
        st.phase = "running".into();
        st.card = Some(card.clone());
        st.step = 0;
        st.recent.clear();
        st.ver += 1;
        st.dir = chrono_name();
    }
    let mut pending: VecDeque<Value> = VecDeque::new();
    let (wall0, emu0) = (Instant::now(), d.m.now());
    let (mut wall0, mut emu0) = (wall0, emu0);
    let mut last_clicks = d.m.s.clicks.len();
    let mut clicks_total: u64 = 0;
    loop {
        pending.extend(shared.cmds.lock().unwrap().drain(..));
        while let Some(c) = pending.front() {
            if d.ready(c) > d.step_n {
                break;
            }
            let c = pending.pop_front().unwrap();
            match c["do"].as_str().unwrap_or("") {
                "restart" | "quit" => return Ok(c),
                "jack" => {
                    // the sound to this machine's output (plugged) or back to the browsers; the iPod is not told
                    let on = c["plugged"].as_bool().unwrap_or(false);
                    shared.host.plug(on);
                    if shared.host.plugged() {
                        shared.audio.end_all();
                    }
                    let mut st = shared.st.lock().unwrap();
                    st.recent.push_back(json!({"t": "jack", "plugged": on, "step": d.step_n}));
                    while st.recent.len() > 8 {
                        st.recent.pop_front();
                    }
                }
                _ => {
                    if let Some(rec) = d.apply(&c) {
                        let mut st = shared.st.lock().unwrap();
                        st.recent.push_back(rec);
                        while st.recent.len() > 8 {
                            st.recent.pop_front();
                        }
                    }
                }
            }
        }
        d.step();
        let (gl, gr) = (gain(d.m.s.hp[0]), gain(d.m.s.hp[1]));
        let samples = d.m.s.audio.take();
        if !samples.is_empty() {
            let mut b = Vec::with_capacity(samples.len() * 2);
            for (i, s) in samples.into_iter().enumerate() {
                let g = if i & 1 == 0 { gl } else { gr };
                let x = if g == 1.0 { s } else { (s as f64 * g) as i16 };
                b.extend_from_slice(&x.to_le_bytes());
            }
            shared.audio.feed(&b);
            shared.host.feed(&b);
        }
        let n = d.m.s.clicks.len();
        if n != last_clicks {
            clicks_total += n.saturating_sub(last_clicks) as u64;
            last_clicks = n;
        }
        // publish
        {
            let mut st = shared.st.lock().unwrap();
            st.step = d.step_n;
            st.emu_us = d.m.now();
            st.hold = d.hold;
            st.audio = d.m.s.audio.on;
            st.backlight = d.m.backlight() as u8;
            st.clicks = clicks_total;
            let mut keys: Vec<String> = d.down.keys().cloned().collect();
            keys.sort();
            st.keys = keys;
            st.lines = LINES.iter().map(|&(k, p, m)| (k.to_string(), d.m.pin(p, m) as u8)).collect();
            let mut wake = false;
            if d.changed {
                d.changed = false;
                st.px = d.screen.px.clone();
                st.packed = pack(&d.screen.px);
                st.digest = Some(d.digest.clone());
                st.np = read_clock(&d.screen.px, &glyphs);
                wake = true;
            }
            let jack = shared.host.state(0);
            let sig = format!("{}|{}|{}|{:?}|{:?}|{:?}|{}|{}", st.clicks, st.audio, st.backlight, st.keys, st.lines,
                              st.recent.back().map(|v| v.to_string()), jack["plugged"], jack["error"]);
            if sig != st.sig {
                st.sig = sig;
                st.ver += 1;
                wake = true;
            }
            if d.step_n % 25 == 0 {
                st.ipod = Some(d.stats());
            }
            drop(st);
            if wake {
                shared.cond.notify_all();
            }
        }
        // real time: sleep when ahead; when far behind, start counting again from now instead of racing to catch up
        let ahead = (d.m.now() - emu0) as f64 / 1e6 - wall0.elapsed().as_secs_f64();
        if ahead > 0.02 {
            std::thread::sleep(Duration::from_secs_f64(ahead.min(0.05)));
        } else if ahead < -0.5 {
            wall0 = Instant::now();
            emu0 = d.m.now();
        }
    }
}

fn chrono_name() -> String {
    // the session's start, as web.py names it (%Y%m%d-%H%M%S, local time is not needed: the page only shows it)
    let t = now_s() as i64;
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}
