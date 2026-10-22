//! The iPod's sound on this machine's own output ("audio on host", the headphone jack plugged in), as emupod/web_audio.py does it: the
//! samples (raw s16le stereo, 44.1 kHz) are piped to `aplay`, or to `pacat` for a chosen PulseAudio/PipeWire output, or to the
//! operator's own --audio-cmd. The player starts when sound comes and is let go after IDLE_RELEASE s of silence, so the output (a DAC
//! another player may share) is held only while the iPod makes sound. After each gap PRIME s of silence go first: the emulator runs
//! just ahead of the wall clock, and that is the buffer the sound then plays from.
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RATE: usize = 44100;
const FRAME: usize = 4;
const BUSY_TRIES: usize = 6;
const IDLE_RELEASE: Duration = Duration::from_secs(8);
const PRIME: f64 = 0.4;

fn default_cmd() -> Vec<String> {
    ["aplay", "-q", "-t", "raw", "-f", "S16_LE", "-r", "44100", "-c", "2", "-B", "500000"].iter().map(|s| s.to_string()).collect()
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file())).unwrap_or(false)
}

fn run_out(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The outputs a person can choose: PulseAudio/PipeWire sinks if `pactl` is there, else ALSA devices from `aplay -l`.
/// (backend, [(id, label, alsa card, alsa device)])
fn list_devices() -> (Option<&'static str>, Vec<(String, String, String, String)>) {
    if which("pactl") {
        if let Some(out) = run_out("pactl", &["list", "sinks"]) {
            let (mut sinks, mut name) = (vec![], None::<String>);
            for line in out.lines() {
                let t = line.trim();
                if let Some(n) = t.strip_prefix("Name: ") {
                    name = Some(n.to_string());
                } else if let (Some(d), Some(n)) = (t.strip_prefix("Description: "), name.take()) {
                    sinks.push((n, d.to_string(), String::new(), String::new()));
                }
            }
            if !sinks.is_empty() {
                return (Some("pulse"), sinks);
            }
        }
    }
    if which("aplay") {
        if let Some(out) = run_out("aplay", &["-l"]) {
            let mut devs = vec![];
            for line in out.lines() {
                // card 1: Pro [Some DAC], device 0: USB Audio [USB Audio]
                let Some(rest) = line.strip_prefix("card ") else { continue };
                let parse = || -> Option<(String, String, String, String, String, String)> {
                    let (card, rest) = rest.split_once(": ")?;
                    let (cname, rest) = rest.split_once(" [")?;
                    let (clabel, rest) = rest.split_once("], device ")?;
                    let (dev, rest) = rest.split_once(": ")?;
                    let (_dname, rest) = rest.split_once(" [")?;
                    let dlabel = rest.strip_suffix(']')?;
                    Some((card.into(), cname.into(), clabel.into(), dev.into(), String::new(), dlabel.into()))
                };
                if let Some((card, cname, clabel, dev, _, dlabel)) = parse() {
                    devs.push((format!("plughw:CARD={cname},DEV={dev}"), format!("{clabel} ({dlabel})"), card, dev));
                }
            }
            if !devs.is_empty() {
                return (Some("alsa"), devs);
            }
        }
    }
    (None, vec![])
}

/// Who has an ALSA playback device open: (pid, program name), from /proc/asound/cardN/pcmDp/subS/status.
fn alsa_holder(card: &str, dev: &str) -> Option<(u32, String)> {
    let dir = format!("/proc/asound/card{card}/pcm{dev}p");
    let mut subs: Vec<_> = std::fs::read_dir(&dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    subs.sort();
    for s in subs {
        let Ok(txt) = std::fs::read_to_string(s.join("status")) else { continue };
        if let Some(pid) = txt.lines().find_map(|l| l.trim().strip_prefix("owner_pid").and_then(|r| r.trim_start_matches([' ', '\t', ':']).trim().parse::<u32>().ok())) {
            let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "another program".into());
            return Some((pid, name));
        }
    }
    None
}

struct State {
    plugged: bool,
    error: Option<String>,
    device: String,
    backend: Option<String>,
    tx: Option<SyncSender<Option<Vec<u8>>>>,
    own_pids: Vec<u32>,
    /// which plug-in this is: a writer gives up the jack only while it is still the current one
    gen: u64,
}

pub struct HostAudio {
    st: Arc<Mutex<State>>,
    cmd: Vec<String>,
    custom: bool,
    store: std::path::PathBuf,
}

impl HostAudio {
    /// `cmd`: --audio-cmd (the operator's own player), else aplay/pacat; `store`: where the chosen output is kept.
    pub fn new(cmd: Option<&str>, store: std::path::PathBuf) -> HostAudio {
        let (mut device, mut backend) = (String::new(), None);
        if let Ok(t) = std::fs::read_to_string(&store) {
            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                device = v["id"].as_str().unwrap_or("").to_string();
                backend = v["backend"].as_str().map(|s| s.to_string());
            }
        }
        let custom = cmd.is_some();
        HostAudio {
            st: Arc::new(Mutex::new(State { plugged: false, error: None, device, backend, tx: None, own_pids: vec![], gen: 0 })),
            cmd: cmd.map(|c| c.split_whitespace().map(|s| s.to_string()).collect()).unwrap_or_else(default_cmd),
            custom,
            store,
        }
    }

    pub fn plugged(&self) -> bool {
        self.st.lock().unwrap().plugged
    }
    /// The page's `jack`: {plugged, error, listeners, device}.
    pub fn state(&self, listeners: usize) -> Value {
        let s = self.st.lock().unwrap();
        json!({"plugged": s.plugged, "error": s.error, "listeners": listeners, "device": s.device})
    }
    /// New sound from the emulator (the iPod's volume already applied).
    pub fn feed(&self, b: &[u8]) {
        if let Some(tx) = &self.st.lock().unwrap().tx {
            let _ = tx.try_send(Some(b.to_vec()));
        }
    }

    fn player_cmd(&self, s: &State) -> Vec<String> {
        if self.custom || s.device.is_empty() {
            return self.cmd.clone();
        }
        if s.backend.as_deref() == Some("pulse") {
            return vec!["pacat".into(), "--playback".into(), format!("--device={}", s.device), "--raw".into(), "--format=s16le".into(),
                        format!("--rate={RATE}"), "--channels=2".into(), "--latency-msec=300".into(), "--client-name=emupod".into()];
        }
        let mut c = default_cmd();
        c.splice(1..1, ["-D".to_string(), s.device.clone()]);
        c
    }

    /// Plug in (the sound to this machine's output) or unplug (to the browsers).
    pub fn plug(&self, on: bool) {
        let mut s = self.st.lock().unwrap();
        if on == s.plugged {
            return;
        }
        if on {
            let cmd = self.player_cmd(&s);
            if !which(&cmd[0]) {
                s.error = Some(format!("{} not found on this machine", cmd[0]));
                return;
            }
            s.error = None;
            let (tx, rx) = sync_channel(400);
            s.tx = Some(tx);
            s.gen += 1;
            let (st, gen) = (self.st.clone(), s.gen);
            std::thread::spawn(move || writer(st, gen, rx, cmd));
        } else if let Some(tx) = s.tx.take() {
            let _ = tx.try_send(None);
        }
        s.plugged = on;
    }

    pub fn devices(&self) -> Value {
        let (backend, devs) = list_devices();
        let s = self.st.lock().unwrap();
        let devs: Vec<Value> = devs.into_iter().map(|(id, label, card, dev)| {
            if backend == Some("alsa") {
                let holder = alsa_holder(&card, &dev).map(|(pid, name)| if s.own_pids.contains(&pid) { "emupod".to_string() } else { name });
                json!({"id": id, "label": label, "holder": holder})
            } else {
                json!({"id": id, "label": label})
            }
        }).collect();
        json!({"backend": backend, "devices": devs, "current": s.device, "custom": self.custom,
               "custom_command": if self.custom { Value::String(self.cmd.join(" ")) } else { Value::Null }})
    }

    /// Choose the output ("" is the system default); if the sound is on the host it moves there at once.
    pub fn set_device(&self, dev: &str) -> Result<Value, String> {
        if self.custom {
            return Err(format!("the app was started with its own player (--audio-cmd): {}", self.cmd.join(" ")));
        }
        let (backend, devs) = list_devices();
        if !dev.is_empty() && !devs.iter().any(|d| d.0 == dev) {
            return Err(format!("no such output: {dev:?}"));
        }
        let was = self.plugged();
        if was {
            self.plug(false);
        }
        {
            let mut s = self.st.lock().unwrap();
            s.device = dev.to_string();
            s.backend = backend.map(|b| b.to_string());
        }
        if let Some(d) = self.store.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&self.store, json!({"id": dev, "backend": backend}).to_string());
        if was {
            self.plug(true);
        }
        Ok(self.devices())
    }
}

fn busy_message(st: &Arc<Mutex<State>>) -> String {
    let device = st.lock().unwrap().device.clone();
    let mut who = None;
    let (backend, devs) = list_devices();
    if backend == Some("alsa") {
        for d in devs {
            if device.is_empty() || d.0 == device {
                if let Some((_, n)) = alsa_holder(&d.2, &d.3) {
                    who = Some(n);
                    break;
                }
            }
        }
    }
    format!("The host's audio output is busy{}. An ALSA output serves one program at a time: it is free when that program is idle or paused \
             (many players can close the output when idle), or choose another output.", who.map(|w| format!(": {w} has it")).unwrap_or_default())
}

/// The writer: pipes the sound to the player, started when sound comes, let go after IDLE_RELEASE of silence.
fn writer(st: Arc<Mutex<State>>, gen: u64, rx: Receiver<Option<Vec<u8>>>, cmd: Vec<String>) {
    let mut proc: Option<Child> = None;
    let mut primed = false;
    let mut last = Instant::now();
    let give_up = |msg: String| {
        let mut s = st.lock().unwrap();
        s.error = Some(msg);
        // only if this writer is still the one plugged in
        if s.gen == gen && s.plugged {
            s.tx = None;
            s.plugged = false;
        }
    };
    let stop = |proc: &mut Option<Child>| {
        if let Some(mut p) = proc.take() {
            st.lock().unwrap().own_pids.retain(|&x| x != p.id());
            drop(p.stdin.take());
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(2) {
                if let Ok(Some(_)) = p.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = p.kill();
            let _ = p.wait();
        }
    };
    let start = || -> Result<Child, String> {
        let mut err = String::new();
        for attempt in 0..BUSY_TRIES {
            let mut p = Command::new(&cmd[0]).args(&cmd[1..]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()
                .map_err(|e| format!("{}: {e}", cmd[0]))?;
            std::thread::sleep(Duration::from_millis(300));
            if let Ok(None) = p.try_wait() {
                st.lock().unwrap().own_pids.push(p.id());
                return Ok(p);
            }
            err.clear();
            if let Some(mut e) = p.stderr.take() {
                let mut b = vec![0u8; 400];
                let n = e.read(&mut b).unwrap_or(0);
                err = String::from_utf8_lossy(&b[..n]).trim().to_string();
            }
            if !err.to_lowercase().contains("busy") {
                return Err(format!("{} stopped{}", cmd[0], if err.is_empty() { String::new() } else { format!(": {err}") }));
            }
            if attempt + 1 < BUSY_TRIES {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        Err(busy_message(&st))
    };
    loop {
        match rx.recv_timeout(Duration::from_secs_f64(PRIME / 2.0)) {
            Err(RecvTimeoutError::Timeout) => {
                primed = false;
                if proc.is_some() && last.elapsed() >= IDLE_RELEASE {
                    stop(&mut proc);
                }
            }
            Err(RecvTimeoutError::Disconnected) | Ok(None) => break,
            Ok(Some(data)) => {
                last = Instant::now();
                if proc.is_none() {
                    match start() {
                        Ok(p) => proc = Some(p),
                        Err(m) => {
                            give_up(m);
                            break;
                        }
                    }
                }
                let p = proc.as_mut().unwrap();
                let w = p.stdin.as_mut().map(|i| {
                    if !primed {
                        i.write_all(&vec![0u8; (RATE as f64 * PRIME) as usize * FRAME])?;
                    }
                    i.write_all(&data)
                });
                primed = true;
                if let Some(Err(_)) = w {
                    let mut err = String::new();
                    if let Some(mut e) = p.stderr.take() {
                        let mut b = vec![0u8; 400];
                        let n = e.read(&mut b).unwrap_or(0);
                        err = String::from_utf8_lossy(&b[..n]).trim().to_string();
                    }
                    give_up(if err.to_lowercase().contains("busy") { busy_message(&st) } else {
                        format!("{} stopped{}", cmd[0], if err.is_empty() { String::new() } else { format!(": {err}") })
                    });
                    break;
                }
            }
        }
    }
    stop(&mut proc);
}
