//! The iPod in a browser: `pp5002::Machine` behind a plain C interface for a Web Worker (see ../web/ipod.js).
//!
//! The host provides the disk (imports `host.disk_len`, `host.disk_read`, `host.disk_write`: in a Worker, an OPFS
//! file's synchronous access handle), and reads the screen, the sound and the counters from memory this module
//! exports. One machine per module instance.
use pp5002::ata::Disk;
use pp5002::lcd::Screen;
use pp5002::{card, Config, Machine};

#[link(wasm_import_module = "host")]
extern "C" {
    fn disk_len() -> f64;
    fn disk_read(off: f64, ptr: *mut u8, len: u32);
    fn disk_write(off: f64, ptr: *const u8, len: u32);
}

struct HostDisk;
impl Disk for HostDisk {
    fn len(&self) -> u64 {
        unsafe { disk_len() as u64 }
    }
    fn read_at(&self, buf: &mut [u8], off: u64) {
        unsafe { disk_read(off as f64, buf.as_mut_ptr(), buf.len() as u32) }
    }
    fn write_at(&self, buf: &[u8], off: u64) {
        unsafe { disk_write(off as f64, buf.as_ptr(), buf.len() as u32) }
    }
}
// the module is used by one thread (the Worker's)
unsafe impl Send for HostDisk {}
unsafe impl Sync for HostDisk {}

struct State {
    m: Machine,
    screen: Screen,
    screen_changed: bool,
    audio: Vec<u8>,
    error: String,
}
static mut STATE: Option<State> = None;
static mut ERROR: String = String::new();

#[allow(static_mut_refs)]
fn st() -> &'static mut State {
    unsafe { STATE.as_mut().expect("init first") }
}

/// Make the machine: boots nothing yet. `persist`: the firmware's disk writes go to the host disk (otherwise they
/// are kept in memory and lost); `start_volume` 0..127 or -1 for the firmware's own; `fpf` frames per audio
/// interrupt (0: the real rate). Returns 0, or 1 with `error_ptr`/`error_len` saying why.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn init(persist: u32, start_volume: i32, fpf: u32) -> i32 {
    unsafe { ERROR.clear() };
    let (osos, _addr) = match card::read_osos(&HostDisk) {
        Ok(x) => x,
        Err(e) => {
            unsafe { ERROR = e };
            return 1;
        }
    };
    let cfg = Config {
        persist: persist != 0,
        start_volume: if start_volume >= 0 { Some(start_volume as u32) } else { None },
        audio_fpf: if fpf == 0 { None } else { Some(fpf as u64) },
        interleave_playing: 300,
        interleave_settle: 200_000,
        cop_ipus: 26,
        ..Config::default()
    };
    let m = Machine::with_disk(&osos, Box::new(HostDisk), cfg);
    unsafe { STATE = Some(State { m, screen: Screen::default(), screen_changed: false, audio: Vec::new(), error: String::new() }) };
    0
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn error_ptr() -> *const u8 {
    unsafe { ERROR.as_ptr() }
}
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn error_len() -> u32 {
    unsafe { ERROR.len() as u32 }
}

fn pump_lcd(s: &mut State) {
    let log = std::mem::take(&mut s.m.s.lcd_log);
    if s.screen.update(&log) {
        s.screen_changed = true;
    }
}

#[no_mangle]
pub extern "C" fn boot() {
    let s = st();
    s.m.boot();
    pump_lcd(s);
}

/// One timer tick of iPod time (about 10 ms).
#[no_mangle]
pub extern "C" fn tick() {
    let s = st();
    s.m.settle(1, 1 << 30);
    pump_lcd(s);
}

/// The iPod's time since reset, in microseconds.
#[no_mangle]
pub extern "C" fn now_us() -> f64 {
    st().m.now() as f64
}

#[no_mangle]
pub extern "C" fn ticks() -> f64 {
    st().m.s.ticks as f64
}

/// The emulator's counters, one by index (the page's iPod Statistics; the names are in ipod_core.js, in this order):
/// 0 now_us, 1 cpu_instr, 2 cop_instr, 3 cpu_ipus, 4 cop_ipus, 5 ata_reads, 6 ata_writes, 7 ata_sectors_read,
/// 8 ata_sectors_written, 9 lcd_writes, 10 ticks, 11 audio_fiqs, 12 halts, 13 ata_cmds, 14 cop_skipped, 15 cpu_skipped,
/// 16 audio_late, 17 sig_sent, 18 sig_taken, 19 cop_wakes, 20 cop_sleeps, 21 irqs, 22 fw_underruns (-1: not 1.5's firmware).
#[no_mangle]
pub extern "C" fn stat(i: u32) -> f64 {
    if i == 22 {
        return match st().m.fw_underruns() { Some(u) => u as f64, None => -1.0 };
    }
    let m = &st().m;
    let s = &m.s;
    match i {
        0 => m.now() as f64,
        1 => s.cpu_instr as f64,
        2 => s.cop_instr as f64,
        3 => s.cfg.ipus as f64,
        4 => s.cfg.cop_ipus as f64,
        5 => s.ata.n_reads as f64,
        6 => s.ata.n_writes as f64,
        7 => s.ata.sectors_read as f64,
        8 => s.ata.sectors_written as f64,
        9 => s.lcd_count as f64,
        10 => s.ticks as f64,
        11 => s.audio.fiqs as f64,
        12 => s.sleeps as f64,
        13 => s.ata.n_cmds as f64,
        14 => s.skipped[1] as f64,
        15 => s.skipped[0] as f64,
        16 => s.audio.late as f64,
        17 => s.sig_sent as f64,
        18 => s.sig_taken as f64,
        19 => s.cop_wakes as f64,
        20 => s.cop_sleeps as f64,
        21 => s.irqs as f64,
        _ => 0.0,
    }
}

const KEYS: [(usize, u32); 5] = [(0, 1 << 1), (0, 1 << 4), (0, 1 << 2), (0, 1 << 0), (0, 1 << 3)]; // select menu play ff rew

/// A key (0 select, 1 menu, 2 play, 3 ff, 4 rew) goes down (1) or up (0): a bare edge.
#[no_mangle]
pub extern "C" fn key(k: u32, down: u32) {
    let s = st();
    let (port, mask) = KEYS[k as usize % 5];
    s.m.set_pin(port, mask, down == 0);
    s.m.edge(port, mask);
    pump_lcd(s);
}

/// `n` single quadrature edges of the wheel (+ clockwise), one tick after each, carrying on from where the two
/// lines are (what a freely turning wheel gives the firmware).
#[no_mangle]
pub extern "C" fn wheel_edges(n: i32) {
    let s = st();
    for _ in 0..n.unsigned_abs() {
        let (a, b) = (s.m.pin(0, 0x80), s.m.pin(0, 0x40));
        let (mask, level) = if n > 0 {
            match (a, b) {
                (false, false) => (0x80, true),
                (true, false) => (0x40, true),
                (true, true) => (0x80, false),
                (false, true) => (0x40, false),
            }
        } else {
            match (a, b) {
                (false, false) => (0x40, true),
                (false, true) => (0x80, true),
                (true, true) => (0x40, false),
                (true, false) => (0x80, false),
            }
        };
        s.m.set_pin(0, mask, level);
        s.m.edge(0, mask);
        tick();
    }
}

/// Hold switch: 1 on.
#[no_mangle]
pub extern "C" fn hold(on: u32) {
    let s = st();
    s.m.set_pin(0, 1 << 5, on != 0);
    s.m.edge(0, 1 << 5);
    pump_lcd(s);
}

// ---- the screen: 160 x 128 bytes of 0..3
#[no_mangle]
pub extern "C" fn screen_ptr() -> *const u8 {
    st().screen.px.as_ptr()
}
/// Whether the screen changed since the last call.
#[no_mangle]
pub extern "C" fn screen_changed() -> u32 {
    let s = st();
    std::mem::take(&mut s.screen_changed) as u32
}

// ---- sound: s16le stereo at 44,100 Hz
/// Moves the sound since the last call into a buffer; returns its length in bytes (read it at `audio_ptr`).
#[no_mangle]
pub extern "C" fn audio_take() -> u32 {
    let s = st();
    let v = s.m.s.audio.take();
    s.audio.clear();
    s.audio.reserve(v.len() * 2);
    for x in v {
        s.audio.extend_from_slice(&x.to_le_bytes());
    }
    s.audio.len() as u32
}
#[no_mangle]
pub extern "C" fn audio_ptr() -> *const u8 {
    st().audio.as_ptr()
}
#[no_mangle]
pub extern "C" fn audio_on() -> u32 {
    st().m.s.audio.on as u32
}
/// The codec's headphone volume codes: left | right << 8 (0x79 is 0 dB, 0x2f and below is mute).
#[no_mangle]
pub extern "C" fn volume() -> u32 {
    let s = st();
    s.m.s.hp[0] | (s.m.s.hp[1] << 8)
}
/// 1 when the player's backlight is on.
#[no_mangle]
pub extern "C" fn backlight() -> u32 {
    st().m.backlight() as u32
}
#[no_mangle]
pub extern "C" fn click_count() -> u32 {
    st().m.s.clicks.len() as u32
}

// ---- memory for the host to put things in
#[no_mangle]
pub extern "C" fn alloc(n: u32) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n as usize);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}
