//! The PortalPlayer PP5002 as emupod models it: two ARM7TDMI cores on one memory, and the devices the iPod's
//! firmware uses. A port of the machine in emupod/core/ipod_emu_ui.py, with its approximations kept exactly (the
//! clock inside a slice, the 20 us turns, one instruction per 1/13 us) so that a recording made on that core
//! replays on this one.

pub mod ata;
pub mod card;
pub mod lcd;

use armcore::{Bus, Core, Cpu};
pub use ata::Ata;
use std::collections::HashMap;

pub const ENTRY: u32 = 0x2800_012c;
pub const DRAM_SIZE: usize = 0x200_0000;
pub const IRAM_SIZE: usize = 0x10_0000;
pub const SAMPLE_RATE: u64 = 44100;
pub const FRAME: u32 = 5280;

// registers
const CPU_CTL: u32 = 0xcf00_4054;
const COP_CTL: u32 = 0xcf00_4058;
const PROC_STAT: u32 = 0xcf00_4050;
const PROC_SLEEP: u32 = 0xca;
const PROC_WAKE: u32 = 0xce;
const TIMER1_CFG: u32 = 0xcf00_1100;
const TIMER1_VAL: u32 = 0xcf00_1104;
const USEC_TIMER: u32 = 0xcf00_1110;
/// what the chip answers when the firmware asks for its name (the release that picks the size of the IRAM by it, 1.0 to 1.2)
const CHIP_ID: &[u8; 8] = b"PP5002E0";
const CLICKER: u32 = 0xcf00_4040;
const LRCK_PORT: u32 = 0xcf00_003c;
const LRCK_BIT: u32 = 0x40;
const PLL_CTL: u32 = 0xcf00_4010;
const PLL_LOCKED: u32 = 0x1000;
const MBX_STAT: u32 = 0xc800_4000;
const MBX_SET: u32 = 0xc800_4004;
const MBX_CLR: u32 = 0xc800_4008;
const SIGNAL: u32 = 0xc800_4010;
pub const SIGNAL_IRQ: u32 = 18;
const COP_INT_EN: u32 = 0xcf00_1034;
const COP_INT_DIS: u32 = 0xcf00_1038;
const AUDIO_IRQ: u32 = 5;
const AUDIO_STATUS: u32 = 0xc000_2404;
const AUDIO_OUT: u32 = 0xc000_2440;
const UNDERRUNS: u32 = 0x0454_731c;
const COP_ID: u32 = 0xaaaa_aaaa;
const CPU_ID: u32 = 0x5555_5555;

/// 1.5's factory volume: `mov r0, #0x6a` in the settings object's constructor, the start of every copy the menu and the player keep
/// (106 in the menu's half steps, 53 of 127, about 42%). The start volume replaces its constant before the first instruction runs, so the
/// firmware starts there everywhere at once and the wheel moves on from it. The store in the click routine follows.
const DEFAULT_VOL_AT: usize = 0x78ef0;
const DEFAULT_VOL_INSN: u32 = 0xe3a0_006a;
/// The player's backlight flag in 1.5's data (found by toggling Backlight in the menu and diffing memory: 0 off, 1 on). There is no backlight
/// hardware to read: the firmware only keeps this word, and the page draws the screen lit when it is set.
const BACKLIGHT_AT: usize = 0x41a050;
const CLICK_AT: u32 = 0x3750c;
const START_VOL_CODE: [u8; 0x44] = [
    0x24, 0x00, 0x90, 0xe5, 0x7f, 0x00, 0x50, 0xe3, 0x1c, 0x10, 0x94, 0xd5, 0x40, 0x1c, 0x81, 0xd2, 0x7c, 0x0c, 0x81, 0xd5, 0x03, 0x00,
    0x00, 0xda, 0x1c, 0x00, 0x94, 0xe5, 0x7f, 0x10, 0xa0, 0xe3, 0x40, 0x0c, 0x80, 0xe2, 0x7c, 0x1c, 0x80, 0xe5, 0x1c, 0x00, 0x94, 0xe5,
    0x40, 0x0c, 0x80, 0xe2, 0x80, 0x2c, 0x90, 0xe5, 0x7c, 0x1c, 0x90, 0xe5, 0x04, 0x00, 0xa0, 0xe1, 0xa5, 0xfe, 0xff, 0xeb, 0xa2, 0x00,
    0x00, 0xea,
];

pub struct Config {
    pub ipus: u64,
    pub cop_ipus: u64,
    pub quantum: u64,
    pub interleave: u64,
    pub interleave_playing: u64,
    pub interleave_settle: u64,
    /// frames per audio FIQ: Some(n) fixed (the default, 8), None the real rate ("auto")
    pub audio_fpf: Option<u64>,
    pub keys_up: bool,
    pub plugged: bool,
    pub idle_sleep: bool,
    pub cop: bool,
    pub ata_bsy: u32,
    pub ata_cmd_us: u64,
    pub ata_block_us: u64,
    pub ata_lba48: bool,
    pub persist: bool,
    /// compat: inside a slice the clock is "slice start + one us per timer read" (the old core's approximation)
    pub slice_clock: bool,
    /// compat: an access to unmapped memory makes a zeroed 1 MB page, as the old core did
    pub lazy_map: bool,
    /// skip spin loops exactly (armcore's `Core::run`): the result is the same, only faster
    pub skip_loops: bool,
    /// the volume (0..127) the first song starts at: the firmware's saved volume is replaced when 1.5's player code
    /// is found (docs: the Python core's OPENPOD_START_VOLUME)
    pub start_volume: Option<u32>,
    /// count every device (MMIO) read and write by address and core, for studying a firmware (`Shared::io_counts`); off by default
    pub trace_io: bool,
    /// the timer interrupt is cleared by reading TIMER1_VAL (0xcf001104), as on the chip; otherwise (the old Python core's way) by execution
    /// reaching 1.5's address 0x118ec, which no other firmware has
    pub ack_on_read: bool,
    /// deliver an interrupt to the CPU only if its bit is set in the CPU's enable word (study)
    pub irq_mask: bool,
    /// (study) with irq_mask: the status registers stay raw, the enable word only gates raising the interrupt
    pub irq_mask_raw: bool,
    /// (study) never keep decoded instructions from IRAM
    pub study_nocache: bool,
    /// experiment: keep what the coprocessor's run of the reset code left in memory (default: wipe it, as the Python core does)
    pub keep_cop_reset_state: bool,
    /// experiment: IRAM is this many KB, repeated through its 1 MB window (power of two); 0 = a flat 1 MB
    pub iram_kb: u32,
}
impl Config {
    /// The old Python core's behaviour, for validating against it (see docs/RUST_CORE_PLAN.md, "Compatibility").
    pub fn compat() -> Config {
        Config { slice_clock: true, lazy_map: true, ..Config::default() }
    }
}
impl Default for Config {
    fn default() -> Config {
        Config {
            ipus: 13,
            cop_ipus: 13,
            quantum: 1000,
            interleave: 20,
            interleave_playing: 0,
            interleave_settle: 1_000_000,
            audio_fpf: Some(8),
            keys_up: true,
            plugged: false,
            idle_sleep: true,
            cop: true,
            ata_bsy: 0,
            ata_cmd_us: 0,
            ata_block_us: 0,
            ata_lba48: false,
            persist: false,
            slice_clock: false,
            lazy_map: false,
            skip_loops: true,
            start_volume: None,
            trace_io: false,
            ack_on_read: true,
            irq_mask: false,
            irq_mask_raw: std::env::var("EMUPOD_RS_IRQ_MASK_RAW").is_ok(),
            study_nocache: std::env::var("EMUPOD_RS_NOCACHE").is_ok(),
            keep_cop_reset_state: false,
            iram_kb: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Cpu,
    Cop,
}

pub struct Timer1 {
    pub cfg: u32,
    pub period: u64,
    pub deadline: Option<u64>,
}

pub struct Audio {
    pub on: bool,
    pub t0: u64,
    pub n: u64,
    pub next: Option<u64>,
    pub fiqs: u64,
    pub late: u64,
    pub samples: Vec<i16>,
    pub started: Vec<u64>,
    pub fpf: u64,
    pub due: u64,
    pub seen: usize,
    /// samples already handed out by `take`
    pub taken: usize,
}

/// Everything the two cores share: memory, devices, the clock. It is the `Bus` of both.
pub struct Shared {
    pub cfg: Config,
    dram: Box<[u8; DRAM_SIZE]>,
    iram: Box<[u8; IRAM_SIZE]>,
    /// IRAM addresses are masked with this: 0xf_ffff (a flat 1 MB, the default) or the size of a smaller memory that repeats
    iram_mask: u32,
    scratch: Box<[u8; DRAM_SIZE]>, // 0x06000000.., the rest of the old 64 MB mapping
    sparse: HashMap<u32, Box<[u8]>>, // any other address: a zeroed 1 MB page made on first touch
    pub who: Who,
    pub gp: [u32; 64],
    pub pending: u32,
    pub cpu_en: u32,
    /// (study) which enable bit gates the key interrupt when the mask is on
    pub gpio_en_bit: u32,
    // the clock
    pub now_base: u64,
    pub sub: u64,
    pub qlen: u64,
    pub charge: u64,
    pub t1: Timer1,
    pub tick_due: bool,
    pub ticks: u64,
    /// interrupts the CPU took (the timer, the coprocessor's signal, buttons and the wheel)
    pub irqs: u64,
    pub wake_at: Option<u64>,
    // devices
    pub ata: Ata,
    pub lcd_log: Vec<(u32, u32)>,
    pub lcd_count: u32,
    pub audio: Audio,
    pub cop_asleep: bool,
    pub cop_sleeps: u64,
    pub cop_wakes: u64,
    pub sleeps: u64,
    pub sig_value: u32,
    pub sig_sent: u64,
    pub sig_taken: u64,
    pub sig_stat: u32,
    pub pll: u32,
    pub hp: [u32; 2],
    codec_addr: u32,
    codec_d: [u32; 2],
    pub has_cop: bool,
    /// set by a device when the running core must stop after this instruction
    stop_now: bool,
    /// the CPU asked to sleep (PROC_SLEEP written to CPU_CTL by the CPU) in the run just made
    pub slept: bool,
    pub fault: Option<(u32, &'static str)>,
    pub pc_exec: u32,
    /// instructions the running core has fetched since its run began (the exact clock inside a slice)
    pub fetched: u64,
    /// stray accesses to unmapped memory (address, was a write), when `lazy_map` is off
    pub stray: Vec<(u32, bool)>,
    /// instructions executed by each core since reset (for speed measurements)
    pub cpu_instr: u64,
    pub cop_instr: u64,
    /// changes whenever a core could see something change: a write that alters memory, any device access
    pub epoch: u64,
    /// instructions skipped as spin loops, by core
    pub skipped: [u64; 2],
    /// cache pages of the cores' decoded code: marked while a core holds decoded instructions from them
    code_pages: Box<[bool; 34816]>,
    /// Call harness snapshots: with `snap_on`, every write that changes memory marks its 1 KB page in `snap_dirty` (the same page numbering as
    /// `code_pages`); `snap_dram`/`snap_iram` hold the memory as the snapshot found it, so a restore copies back only the dirty pages.
    snap_on: bool,
    /// Call mode (the decompilation gate): every address except the pages the host mapped faults; DRAM/IRAM pages are the real memory,
    /// every other page (peripherals included) is plain memory in `call_pages`, never a device. Data accesses are logged.
    call_on: bool,
    /// Literal call mode: no aliases, no DRAM/IRAM arrays: every mapped page is a plain page in `call_pages`, code runs from them too.
    call_literal: bool,
    call_unaligned: bool,
    /// With faults off, the address a misaligned access ASKED for (the core goes on with the aligned one): the access log shows this one.
    call_mis: Option<u32>,
    snap_orig: std::collections::HashMap<u32, Box<[u8; 4096]>>,
    snap_added: Vec<u32>,
    call_trace: Option<(u32, u32, std::collections::HashSet<u32>)>,
    call_trace_log: Vec<(u32, u32)>,
    call_reg_log: Vec<(u32, Vec<u32>)>,
    call_mapped: std::collections::HashSet<u32>,
    call_pages: std::collections::HashMap<u32, Box<[u8; 4096]>>,
    call_wr_ok: Vec<(u32, u32)>,
    call_log_on: bool,
    call_log: Vec<(u8, u32, u8, u32, u32)>,
    call_pc: u32,
    call_fault: Option<(u8, u32, u8, u32, u32)>,
    snap_dirty: Vec<u8>,
    snap_dram: Option<Box<[u8]>>,
    snap_iram: Option<Box<[u8]>>,
    dirty: Vec<u32>,
    dirty_flag: bool,
    /// 1.5's firmware family (its player's volume code, START_VOL_CODE, is there): its backlight flag is the word at DRAM offset BACKLIGHT_AT
    backlight_ok: bool,
    /// the iPod times of the firmware's own clicks (its click routine's store), when its code is found
    click_armed: bool,
    pub clicks: Vec<u64>,
    /// (core 0 cpu / 1 cop, write?, word address) -> count, when `cfg.trace_io`
    pub io: HashMap<(u8, bool, u32), u64>,
    /// the last value written to each device address, when `cfg.trace_io`
    pub io_last: HashMap<(u8, u32), u32>,
    /// (study) event log of device accesses in `ev_ranges`: (core, write?, address, value, time in us, sequence)
    pub ev_on: bool,
    pub ev_ranges: Vec<(u32, u32)>,
    pub ev_log: Vec<(u8, bool, u32, u32, u64, u64)>,
    pub ev_cap: usize,
}

impl Audio {
    /// The sound since the last call (interleaved s16 L, R); what was handed out is dropped from `samples` so a long
    /// session stays small.
    pub fn take(&mut self) -> Vec<i16> {
        let out = self.samples[self.taken..].to_vec();
        self.taken = self.samples.len();
        let n = self.taken.min(self.seen);
        if n >= 1 << 20 {
            self.samples.drain(..n);
            self.seen -= n;
            self.taken -= n;
        }
        out
    }
}

impl Shared {
    #[inline(always)]
    pub fn now(&self) -> u64 {
        if self.cfg.slice_clock {
            self.now_base + self.sub
        } else {
            let ipus = if self.who == Who::Cpu { self.cfg.ipus } else { self.cfg.cop_ipus };
            self.now_base + self.fetched / ipus
        }
    }

    /// a write changed memory in cache page `page`: if a core holds decoded code from it, drop that
    #[inline(always)]
    fn touch_code(&mut self, page: usize) {
        if self.snap_on {
            self.snap_dirty[page] = 1;
        }
        if self.code_pages[page] {
            self.code_pages[page] = false;
            self.dirty.push(page as u32);
            self.dirty_flag = true;
        }
    }

    fn is_ram(&self, a: u32) -> bool {
        if self.call_literal {
            return false;
        }
        let h = a >> 25;
        h == 0 || h == 2 || h == 20 || a >> 20 == 0x400
    }

    /// One address for every alias of the same memory.
    fn canon(&self, a: u32) -> u32 {
        if self.call_literal {
            return a;
        }
        let h = a >> 25;
        if h == 0 || h == 2 || h == 20 {
            0x2800_0000 + (a & 0x1ff_ffff)
        } else if a >> 20 == 0x400 {
            0x4000_0000 + (a & self.iram_mask)
        } else {
            a
        }
    }

    fn call_mapped_range(&self, a: u32, size: u32) -> bool {
        let (p1, p2) = (self.canon(a) >> 12, self.canon(a.wrapping_add(size - 1)) >> 12);
        self.call_mapped.contains(&p1) && self.call_mapped.contains(&p2)
    }

    /// A byte of a plain page (zero if the page has none).
    fn call_page_read(&self, a: u32) -> u8 {
        let c = self.canon(a);
        self.call_pages.get(&(c >> 12)).map_or(0, |p| p[(c & 0xfff) as usize])
    }

    /// Write a byte of a plain page (making it): the first write after a snapshot keeps the page as it was, for `restore`.
    fn call_page_write(&mut self, a: u32, b: u8) {
        let c = self.canon(a);
        let pk = c >> 12;
        if self.snap_on && !self.snap_orig.contains_key(&pk) {
            if let Some(p) = self.call_pages.get(&pk) {
                if !self.snap_added.contains(&pk) {
                    self.snap_orig.insert(pk, p.clone());
                }
            }
        }
        let p = self.call_pages.entry(pk).or_insert_with(|| Box::new([0u8; 4096]));
        p[(c & 0xfff) as usize] = b;
    }

    fn unaligned(&self, a: u32, size: u32) -> bool {
        self.call_unaligned && ((size == 2 && a & 1 != 0) || (size == 4 && a & 3 != 0))
    }

    /// The host's own read in call mode: memory as the function would see it, no checks, no log.
    pub fn host_rd(&mut self, a: u32) -> u8 {
        if self.call_on && !self.is_ram(a) {
            self.call_page_read(a)
        } else {
            self.rd_inner(a, 1) as u8
        }
    }

    pub fn host_wr(&mut self, a: u32, b: u8) {
        if self.call_on && !self.is_ram(a) {
            self.call_page_write(a, b);
        } else {
            self.wr_inner(a, 1, b as u32)
        }
    }

    #[inline(never)]
    fn call_rd(&mut self, a: u32, size: u32) -> u32 {
        let la = self.call_mis.take().unwrap_or(a);                  // the address a misaligned access asked for (faults off)
        if self.unaligned(a, size) || !self.call_mapped_range(a, size) {
            if self.call_fault.is_none() {
                self.call_fault = Some((if self.unaligned(a, size) { 4 } else { 0 }, la, size as u8, self.call_pc, 0));
            }
            self.stop_now = true;
            return 0;
        }
        let v = if self.is_ram(a) {
            self.rd_inner(a, size)
        } else {
            let mut v = 0u32;
            for k in 0..size {
                v |= (self.call_page_read(a.wrapping_add(k)) as u32) << (8 * k);
            }
            v
        };
        if self.call_log_on && self.call_log.len() < 1_000_000 {
            self.call_log.push((0, la, size as u8, v, self.call_pc));
        }
        v
    }

    #[inline(never)]
    fn call_wr(&mut self, a: u32, size: u32, v: u32) {
        let la = self.call_mis.take().unwrap_or(a);
        if self.call_fault.is_some() {
            return;                                  // an earlier access of this instruction faulted: nothing more lands
        }
        let kind = if self.unaligned(a, size) {
            Some(4)
        } else if !self.call_mapped_range(a, size) {
            Some(1)
        } else if !self.call_wr_ok.is_empty() {
            let (c0, c1) = (self.canon(a), self.canon(a.wrapping_add(size - 1)));
            if self.call_wr_ok.iter().any(|&(lo, hi)| c0 >= lo && c1 < hi) { None } else { Some(2) }
        } else {
            None
        };
        if let Some(k) = kind {
            self.call_fault = Some((k, la, size as u8, self.call_pc, if k == 4 { 0 } else { v & if size == 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 } }));
            self.stop_now = true;
            return;
        }
        let v = if size == 4 { v } else { v & ((1u32 << (8 * size)) - 1) };
        if self.call_log_on && self.call_log.len() < 1_000_000 {
            self.call_log.push((1, la, size as u8, v, self.call_pc));
        }
        if self.is_ram(a) {
            self.wr_inner(a, size, v)
        } else {
            for k in 0..size {
                self.call_page_write(a.wrapping_add(k), (v >> (8 * k)) as u8);
            }
        }
    }

    fn page(&mut self, a: u32) -> (&mut [u8], usize) {
        let key = a >> 20;
        let p = self.sparse.entry(key).or_insert_with(|| vec![0u8; 0x10_0000].into_boxed_slice());
        (&mut p[..], (a & 0xf_ffff) as usize)
    }

    // ------------------------------------------------------------------------------------------ registers read
    fn regval(&mut self, a: u32) -> u32 {
        if ata::is_taskfile(a) || a == ata::ALT {
            let now = self.now();
            return self.ata.read(a, now);
        }
        if a == USEC_TIMER {
            if self.cfg.slice_clock && self.sub < self.qlen.saturating_sub(1) {
                self.sub += 1;
            }
            return self.now() as u32;
        }
        if a == TIMER1_VAL {
            return match self.t1.deadline {
                None => 0,
                Some(d) => d.saturating_sub(self.now()) as u32,
            };
        }
        if a == TIMER1_CFG {
            return self.t1.cfg;
        }
        if a == 0xcf00_1020 && self.cfg.irq_mask && !self.cfg.irq_mask_raw && self.who == Who::Cpu {
            return self.pending & self.cpu_en;
        }
        if matches!(a, 0xcf00_1000 | 0xcf00_1010 | 0xcf00_1020 | 0xcf00_1030) {
            return self.pending;
        }
        if a == PROC_STAT && self.has_cop {
            return if self.cop_asleep { 0x4000 } else { 0 };
        }
        if a == LRCK_PORT && self.has_cop {
            if self.cfg.slice_clock && self.sub < self.qlen.saturating_sub(1) {
                self.sub += 1;
            }
            let g = self.gp[((a - 0xcf00_0000) >> 2) as usize] & !LRCK_BIT;
            let bit = (self.now() * 2 * SAMPLE_RATE / 1_000_000) & 1;
            return g | if bit != 0 { LRCK_BIT } else { 0 };
        }
        if (0xcf00_0000..=0xcf00_00fc).contains(&a) {
            return self.gp[((a - 0xcf00_0000) >> 2) as usize];
        }
        if (0xcf00_4030..=0xcf00_403c).contains(&a) && a & 3 == 0 {
            // the chip's name, two characters per register, first character in the top byte of 0x3c
            let i = ((0xcf00_403c - a) / 2) as usize;
            return (CHIP_ID[i] as u32) << 8 | CHIP_ID[i + 1] as u32;
        }
        if a == PLL_CTL {
            return (self.pll & !PLL_LOCKED) | if self.pll & 0x2000 != 0 { PLL_LOCKED } else { 0 };
        }
        match a {
            0xc400_0000 => CPU_ID,
            0xc000_3024 => 0x40,
            _ => 0,
        }
    }

    fn ev_hit(&self, a: u32) -> bool {
        self.ev_ranges.iter().any(|&(lo, hi)| a >= lo && a <= hi)
    }

    fn ev_push(&mut self, w: bool, a: u32, v: u32) {
        if self.ev_log.len() < self.ev_cap {
            let n = self.ev_log.len() as u64;
            let t = self.now();
            self.ev_log.push(((self.who == Who::Cop) as u8, w, a, v, t, n));
        }
    }

    fn mmio_read(&mut self, a: u32) -> u32 {
        let v = self.mmio_read_inner(a);
        if self.ev_on && self.ev_hit(a) {
            self.ev_push(false, a, v);
        }
        v
    }

    fn mmio_read_inner(&mut self, a: u32) -> u32 {
        if self.cfg.trace_io {
            *self.io.entry(((self.who == Who::Cop) as u8, false, a & !3)).or_insert(0) += 1;
        }
        if self.who == Who::Cop && a == 0xc400_0000 {
            return COP_ID;
        }
        if self.has_cop {
            if a == AUDIO_STATUS {
                return 16 << 16;
            }
            if a == SIGNAL {
                self.sig_taken += 1;
                self.pending &= !(1 << SIGNAL_IRQ);
                return self.sig_value;
            }
            if a == MBX_STAT {
                return self.sig_stat;
            }
        }
        if a == TIMER1_VAL && self.cfg.ack_on_read {
            self.pending &= !(1 << 11);
        }
        self.regval(a)
    }

    // ----------------------------------------------------------------------------------------- registers write
    fn audio_enable(&mut self, on: bool) {
        let now = self.now();
        if on && !self.audio.on {
            self.audio.on = true;
            self.audio.t0 = now;
            self.audio.n = 0;
            self.audio.due = 0;
            self.audio.seen = self.audio.samples.len();
            self.audio.started.push(now);
            self.audio.next = Some(now + self.audio.fpf * 1_000_000 / SAMPLE_RATE);
        } else if !on {
            self.audio.on = false;
            self.audio.next = None;
        }
    }

    fn i2c_write(&mut self, a: u32, val: u32) {
        let v = val & 0xff;
        match a {
            0xc000_8004 => self.codec_addr = v,
            0xc000_800c => self.codec_d[0] = v,
            0xc000_8010 => self.codec_d[1] = v,
            0xc000_8000 if v & 0x80 != 0 && self.codec_addr == 0x34 => {
                let reg = self.codec_d[0] >> 1;
                let data = ((self.codec_d[0] & 1) << 8) | self.codec_d[1];
                if reg == 2 || reg == 3 {
                    let side = (reg - 2) as usize;
                    self.hp[side] = data & 0x7f;
                    if data & 0x100 != 0 {
                        self.hp[1 - side] = data & 0x7f;
                    }
                }
            }
            _ => {}
        }
    }

    fn gpio_write(&mut self, a: u32, v: u32) {
        if (0xcf00_0070..=0xcf00_007c).contains(&a) {
            let port = ((a - 0xcf00_0070) / 4) as usize;
            self.gp[int_stat_idx(port)] &= !v;
            if (0..4).all(|p| self.gp[int_stat_idx(p)] == 0) {
                self.pending &= !self.gpio_en_bit;
            }
            return;
        }
        self.gp[((a - 0xcf00_0000) >> 2) as usize] = v;
    }

    fn mmio_write(&mut self, a: u32, size: u32, val: u32) {
        if self.ev_on && self.ev_hit(a) {
            self.ev_push(true, a, val);
        }
        self.mmio_write_inner(a, size, val)
    }

    fn mmio_write_inner(&mut self, a: u32, size: u32, val: u32) {
        if self.cfg.trace_io {
            *self.io.entry(((self.who == Who::Cop) as u8, true, a & !3)).or_insert(0) += 1;
            self.io_last.insert(((self.who == Who::Cop) as u8, a & !3), val);
        }
        let val = match size {
            1 => val & 0xff,
            2 => val & 0xffff,
            _ => val,
        };
        if a & !0xfff == 0xc000_1000 {
            self.lcd_count += 1;
            self.lcd_log.push((a, val));
        } else if a == CLICKER && !self.click_armed && self.who == Who::Cpu {
            // every firmware clicks by one read-modify-write of the clicker register (1.5's own click routine is hooked instead, when found)
            let t = self.now();
            self.clicks.push(t);
        } else if a == CPU_CTL || a == COP_CTL {
            let v = val & 0xff;
            if a == CPU_CTL {
                if v == PROC_SLEEP && self.who == Who::Cpu {
                    self.sleeps += 1;
                    if self.cfg.idle_sleep {
                        self.slept = true;
                        self.stop_now = true;
                    }
                }
            } else if self.has_cop {
                if v == PROC_SLEEP {
                    self.cop_asleep = true;
                    self.cop_sleeps += 1;
                    if self.who == Who::Cop {
                        self.stop_now = true;
                    }
                } else if v == PROC_WAKE {
                    if self.cop_asleep {
                        self.cop_wakes += 1;
                    }
                    self.cop_asleep = false;
                }
            }
        } else if a == 0xcf00_1024 && self.who == Who::Cpu {
            self.cpu_en |= val;
        } else if a == 0xcf00_1028 && self.who == Who::Cpu {
            self.cpu_en &= !val;
        } else if a == SIGNAL && self.has_cop {
            self.sig_value = val;
            self.sig_sent += 1;
            self.pending |= 1 << SIGNAL_IRQ;
        } else if a == MBX_SET && self.has_cop {
            self.sig_stat |= val;
        } else if a == MBX_CLR && self.has_cop {
            self.sig_stat &= !val;
        } else if a == AUDIO_OUT && self.has_cop {
            let v = (val >> 16) & 0xffff;
            self.audio.samples.push(v as u16 as i16);
        } else if (0xc000_8000..=0xc000_80ff).contains(&a) {
            self.i2c_write(a, val);
        } else if (a == COP_INT_EN || a == COP_INT_DIS) && self.has_cop {
            if val & (1 << AUDIO_IRQ) != 0 {
                self.audio_enable(a == COP_INT_EN);
            }
        } else if ata::is_taskfile(a) {
            let now = self.now();
            self.ata.write(a, val, now);
        } else if a == TIMER1_CFG {
            self.t1.cfg = val;
            if val & 0x8000_0000 != 0 {
                self.t1.period = (val & 0x1fff_ffff) as u64 + 1;
                self.t1.deadline = Some(self.now() + self.t1.period);
            } else {
                self.t1.deadline = None;
            }
        } else if (0xcf00_0000..=0xcf00_00fc).contains(&a) {
            self.gpio_write(a, val);
        } else if a == PLL_CTL {
            self.pll = val;
        }
    }

    // ------------------------------------------------------------------------------------------------ memory
    #[inline(always)]
    fn rd(&mut self, a: u32, size: u32) -> u32 {
        if self.call_on {
            return self.call_rd(a, size);
        }
        self.rd_inner(a, size)
    }

    #[inline(always)]
    fn wr(&mut self, a: u32, size: u32, v: u32) {
        if self.call_on {
            return self.call_wr(a, size, v);
        }
        self.wr_inner(a, size, v)
    }

    #[inline(always)]
    fn rd_inner(&mut self, a: u32, size: u32) -> u32 {
        let h = a >> 25;
        if h == 0 || h == 2 || h == 20 {
            let i = (a & 0x1ff_ffff) as usize;
            if i <= DRAM_SIZE - 4 {
                // the common case: one load (the core hands the bus aligned addresses)
                return match size {
                    1 => self.dram[i] as u32,
                    2 => u16::from_le_bytes(self.dram[i..i + 2].try_into().unwrap()) as u32,
                    _ => u32::from_le_bytes(self.dram[i..i + 4].try_into().unwrap()),
                };
            }
            return self.rd_wrap(a, size);
        }
        if a >> 20 == 0x400 {
            let i = (a & self.iram_mask) as usize;
            if i <= IRAM_SIZE - 4 {
                return match size {
                    1 => self.iram[i] as u32,
                    2 => u16::from_le_bytes(self.iram[i..i + 2].try_into().unwrap()) as u32,
                    _ => u32::from_le_bytes(self.iram[i..i + 4].try_into().unwrap()),
                };
            }
        }
        self.rd_slow(a, size)
    }

    /// a DRAM read that wraps at the end of the 32 MB (an access straddling the alias boundary)
    #[inline(never)]
    fn rd_wrap(&mut self, a: u32, size: u32) -> u32 {
        let i = (a & 0x1ff_ffff) as usize;
        let mut v = 0u32;
        for k in 0..size as usize {
            v |= (self.dram[(i + k) & 0x1ff_ffff] as u32) << (8 * k);
        }
        v
    }

    #[inline(never)]
    fn rd_slow(&mut self, a: u32, size: u32) -> u32 {
        if a >> 20 == 0x400 {
            // an IRAM access at the very end: wraps
            let i = (a & self.iram_mask) as usize;
            let mut v = 0u32;
            for k in 0..size as usize {
                v |= (self.iram[(i + k) & 0xf_ffff] as u32) << (8 * k);
            }
            return v;
        }
        if a >> 28 == 0xc {
            self.epoch += 1;
            let v = self.mmio_read(a);
            return match size {
                1 => v & 0xff,
                2 => v & 0xffff,
                _ => v,
            };
        }
        if a >> 24 == 0xf0 {
            return 0;
        }
        if a >> 25 == 3 {
            let i = (a & 0x1ff_ffff) as usize;
            let s = &self.scratch;
            return match size {
                1 => s[i] as u32,
                2 => u16::from_le_bytes([s[i], s[(i + 1) & 0x1ff_ffff]]) as u32,
                _ => u32::from_le_bytes([s[i], s[(i + 1) & 0x1ff_ffff], s[(i + 2) & 0x1ff_ffff], s[(i + 3) & 0x1ff_ffff]]),
            };
        }
        if !self.cfg.lazy_map {
            self.stray.push((a, false));
            return 0;
        }
        let (p, i) = self.page(a);
        let n = size as usize;
        let mut v = 0u32;
        for k in 0..n {
            v |= (p[(i + k) & 0xf_ffff] as u32) << (8 * k);
        }
        v
    }

    #[inline(always)]
    fn wr_inner(&mut self, a: u32, size: u32, v: u32) {
        let h = a >> 25;
        if h == 0 || h == 2 || h == 20 {
            let i = (a & 0x1ff_ffff) as usize;
            if i <= DRAM_SIZE - 4 {
                match size {
                    1 => {
                        if self.dram[i] != v as u8 {
                            self.epoch += 1;
                            self.dram[i] = v as u8;
                            self.touch_code(i >> 10);
                        }
                    }
                    2 => {
                        let w = (v as u16).to_le_bytes();
                        if self.dram[i..i + 2] != w {
                            self.epoch += 1;
                            self.dram[i..i + 2].copy_from_slice(&w);
                            self.touch_code(i >> 10);
                        }
                    }
                    _ => {
                        let w = v.to_le_bytes();
                        if self.dram[i..i + 4] != w {
                            self.epoch += 1;
                            self.dram[i..i + 4].copy_from_slice(&w);
                            self.touch_code(i >> 10);
                        }
                    }
                }
                return;
            }
            self.wr_wrap(a, size, v);
            return;
        }
        if a >> 20 == 0x400 {
            let i = (a & self.iram_mask) as usize;
            if i <= IRAM_SIZE - 4 {
                match size {
                    1 => {
                        if self.iram[i] != v as u8 {
                            self.epoch += 1;
                            self.iram[i] = v as u8;
                            self.touch_code(0x8000 + (i >> 10));
                        }
                    }
                    2 => {
                        let w = (v as u16).to_le_bytes();
                        if self.iram[i..i + 2] != w {
                            self.epoch += 1;
                            self.iram[i..i + 2].copy_from_slice(&w);
                            self.touch_code(0x8000 + (i >> 10));
                        }
                    }
                    _ => {
                        let w = v.to_le_bytes();
                        if self.iram[i..i + 4] != w {
                            self.epoch += 1;
                            self.iram[i..i + 4].copy_from_slice(&w);
                            self.touch_code(0x8000 + (i >> 10));
                        }
                    }
                }
                return;
            }
        }
        self.wr_slow(a, size, v);
    }

    #[inline(never)]
    fn wr_wrap(&mut self, a: u32, size: u32, v: u32) {
        let i = (a & 0x1ff_ffff) as usize;
        for k in 0..size as usize {
            let j = (i + k) & 0x1ff_ffff;
            let b = (v >> (8 * k)) as u8;
            if self.dram[j] != b {
                self.epoch += 1;
                self.dram[j] = b;
                self.touch_code(j >> 10);
            }
        }
    }

    #[inline(never)]
    fn wr_slow(&mut self, a: u32, size: u32, v: u32) {
        if a >> 20 == 0x400 {
            // an IRAM access at the very end: wraps
            let i = (a & self.iram_mask) as usize;
            for k in 0..size as usize {
                let j = (i + k) & 0xf_ffff;
                let b = (v >> (8 * k)) as u8;
                if self.iram[j] != b {
                    self.epoch += 1;
                    self.iram[j] = b;
                    self.touch_code(0x8000 + (j >> 10));
                }
            }
            return;
        }
        if a >> 28 == 0xc {
            self.epoch += 1;
            self.mmio_write(a, size, v);
            return;
        }
        if a >> 24 == 0xf0 {
            return;
        }
        if a >> 25 == 3 {
            self.epoch += 1;
            let i = (a & 0x1ff_ffff) as usize;
            for k in 0..size as usize {
                self.scratch[(i + k) & 0x1ff_ffff] = (v >> (8 * k)) as u8;
            }
            return;
        }
        self.epoch += 1;
        if !self.cfg.lazy_map {
            self.stray.push((a, true));
            return;
        }
        let (p, i) = self.page(a);
        for k in 0..size as usize {
            p[(i + k) & 0xf_ffff] = (v >> (8 * k)) as u8;
        }
    }
}

#[inline(always)]
fn int_stat_idx(port: usize) -> usize {
    ((0xcf00_0040 + 4 * port as u32 - 0xcf00_0000) >> 2) as usize
}
fn in_val_idx(port: usize) -> usize {
    ((0xcf00_0030 + 4 * port as u32 - 0xcf00_0000) >> 2) as usize
}

impl Bus for Shared {
    #[inline(always)]
    fn read8(&mut self, a: u32) -> u8 {
        self.rd(a, 1) as u8
    }
    #[inline(always)]
    fn read16(&mut self, a: u32) -> u16 {
        self.rd(a, 2) as u16
    }
    #[inline(always)]
    fn read32(&mut self, a: u32) -> u32 {
        self.rd(a, 4)
    }
    #[inline(always)]
    fn write8(&mut self, a: u32, v: u8) {
        self.wr(a, 1, v as u32)
    }
    #[inline(always)]
    fn write16(&mut self, a: u32, v: u16) {
        self.wr(a, 2, v as u32)
    }
    #[inline(always)]
    fn write32(&mut self, a: u32, v: u32) {
        self.wr(a, 4, v)
    }
    #[inline(always)]
    fn fetch32(&mut self, a: u32) -> u32 {
        // the timer acknowledge: reaching the interrupt handler's tail clears the timer's pending bit
        if !self.cfg.ack_on_read && a & 0x0fff_ffff == 0x0001_18ec {
            self.pending &= !(1 << 11);
        }
        if self.call_literal && self.call_on {
            return (0..4).fold(0u32, |v, k| v | (self.call_page_read(a.wrapping_add(k)) as u32) << (8 * k));
        }
        self.rd_inner(a, 4)
    }
    #[inline(always)]
    fn fetch16(&mut self, a: u32) -> u16 {
        if self.call_literal && self.call_on {
            return (0..2).fold(0u32, |v, k| v | (self.call_page_read(a.wrapping_add(k)) as u32) << (8 * k)) as u16;
        }
        self.rd_inner(a, 2) as u16
    }
    #[inline(always)]
    fn misaligned(&mut self, addr: u32, size: u32) {
        if !self.call_on {
            return;
        }
        if self.call_unaligned {
            if self.call_fault.is_none() {
                self.call_fault = Some((4, addr, size as u8, self.call_pc, 0));
                self.stop_now = true;
            }
        } else {
            self.call_mis = Some(addr);
        }
    }
    #[inline(always)]
    fn stop(&self) -> bool {
        self.stop_now
    }
    #[inline(always)]
    fn skip_enabled(&self) -> bool {
        self.cfg.skip_loops
    }
    #[inline(always)]
    fn epoch(&self) -> u64 {
        self.epoch
    }
    #[inline(always)]
    fn code_key(&self, pc: u32) -> u32 {
        if self.call_literal && self.call_on {
            return u32::MAX;
        }
        let h = pc >> 25;
        if h == 0 || h == 2 || h == 20 {
            (pc & 0x1ff_ffff) >> 2
        } else if pc >> 20 == 0x400 {
            0x80_0000 + ((pc & self.iram_mask) >> 2)
        } else {
            u32::MAX
        }
    }
    #[inline(always)]
    fn cacheable(&self, pc: u32) -> bool {
        if self.call_literal && self.call_on {
            return false;
        }
        if self.cfg.study_nocache && (pc >> 28) == 4 {
            return false;
        }
        if !self.cfg.ack_on_read && pc & 0x0fff_ffff == 0x0001_18ec {
            return false; // the timer acknowledge is hooked on this address's every execution
        }
        if self.click_armed {
            let h = pc >> 25;
            if h == 0 || h == 2 || h == 20 {
                return pc & 0x1ff_ffff != CLICK_AT;
            }
        }
        true
    }
    fn hook(&mut self, pc: u32, _cpu: &mut Cpu) {
        if self.call_literal && self.call_on {
            return;
        }
        let h = pc >> 25;
        if !(h == 0 || h == 2 || h == 20) {
            return;
        }
        let p = pc & 0x1ff_ffff;
        if p == CLICK_AT && self.click_armed {
            let t = self.now();
            self.clicks.push(t);
        }
    }
    #[inline(always)]
    fn mark_code(&mut self, page: u32) {
        self.code_pages[page as usize] = true;
    }
    #[inline(always)]
    fn code_dirty(&self) -> bool {
        self.dirty_flag
    }
    fn take_dirty(&mut self, out: &mut Vec<u32>) {
        out.append(&mut self.dirty);
        self.dirty_flag = false;
    }
    #[inline(always)]
    fn at(&mut self, n: u64) {
        self.fetched = n;
    }
    #[inline(always)]
    fn advance(&mut self, n: u64) {
        self.skipped[(self.who == Who::Cop) as usize] += n;
    }
}

// ------------------------------------------------------------------------------------------------ the machine
pub struct Machine {
    /// (for study) the next `cop_trace_left` coprocessor instructions are recorded one by one into `cop_log`
    pub cop_trace_left: usize,
    /// (for study) the first slice after which the word at `watch_addr` changed: (core 0 cpu / 1 cop, time, that core's pc and lr at the end of the slice)
    pub watch_on: bool,
    pub watch_addr: u32,
    pub watch_hit: Option<(u8, u64, u32, u32)>,
    pub watch_regs: [u32; 16],
    pub watch_stop: bool,
    pub watch_log: Vec<(u8, u64, u32, u32, u32)>,
    pub bp_pc: u32,
    pub bp_mem: Vec<Vec<u32>>,
    pub ring: std::collections::VecDeque<(u32, [u32; 16])>,
    pub bp_hits: Vec<(u64, [u32; 16])>,
    snap_cpu: Option<armcore::Cpu>,
    /// Study: addresses whose every execution on the CPU is logged as (now_us, pc, r0..r15) into `hook_log` (cap 50,000); empty = off.
    pub hook_pcs: Vec<u32>,
    pub hook_log: Vec<(u64, u32, [u32; 16], u32)>,
    /// With (reg, offset): each hook entry also records the word at r[reg] + offset (a message's type word, say); 0 if unmapped.
    pub hook_peek: Option<(usize, u32)>,
    pub cop_log: Vec<(u32, u32)>,
    pub s: Shared,
    pub cpu: Core<Shared>,
    pub cop: Core<Shared>,
}

impl Machine {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(osos: &[u8], disk: &str, cfg: Config) -> std::io::Result<Machine> {
        let ata = Ata::new(disk, cfg.persist, cfg.ata_bsy, cfg.ata_cmd_us, cfg.ata_block_us, cfg.ata_lba48)?;
        Ok(Machine::build(osos, ata, cfg))
    }

    /// A machine on any disk (see `ata::Disk`).
    pub fn with_disk(osos: &[u8], disk: Box<dyn ata::Disk>, cfg: Config) -> Machine {
        let ata = Ata::with_disk(disk, cfg.persist, cfg.ata_bsy, cfg.ata_cmd_us, cfg.ata_block_us, cfg.ata_lba48);
        Machine::build(osos, ata, cfg)
    }

    fn build(osos: &[u8], ata: Ata, cfg: Config) -> Machine {
        let fpf = cfg.audio_fpf.unwrap_or(8);
        let has_cop = cfg.cop;
        let mut s = Shared {
            dram: vec![0u8; DRAM_SIZE].into_boxed_slice().try_into().unwrap(),
            iram: vec![0u8; IRAM_SIZE].into_boxed_slice().try_into().unwrap(),
            iram_mask: if cfg.iram_kb == 0 { 0xf_ffff } else { cfg.iram_kb * 1024 - 1 },
            scratch: vec![0u8; DRAM_SIZE].into_boxed_slice().try_into().unwrap(),
            sparse: HashMap::new(),
            who: Who::Cpu,
            gp: [0; 64],
            pending: 0,
            cpu_en: 0xffff_ffff,
            gpio_en_bit: std::env::var("EMUPOD_RS_GPIO_BIT").ok().and_then(|v| v.parse::<u32>().ok()).map(|b| 1u32 << b).unwrap_or(1 << 14),
            now_base: 0,
            sub: 0,
            qlen: cfg.quantum,
            charge: 0,
            t1: Timer1 { cfg: 0, period: 0, deadline: None },
            tick_due: false,
            ticks: 0,
            irqs: 0,
            wake_at: None,
            ata,
            lcd_log: Vec::new(),
            lcd_count: 0,
            audio: Audio { on: false, t0: 0, n: 0, next: None, fiqs: 0, late: 0, samples: Vec::new(), started: Vec::new(), fpf, due: 0, seen: 0, taken: 0 },
            cop_asleep: false,
            cop_sleeps: 0,
            cop_wakes: 0,
            sleeps: 0,
            sig_value: 0,
            sig_sent: 0,
            sig_taken: 0,
            sig_stat: 0,
            pll: 0,
            hp: [0x79, 0x79],
            codec_addr: 0,
            codec_d: [0; 2],
            has_cop,
            stop_now: false,
            slept: false,
            fault: None,
            pc_exec: 0,
            fetched: 0,
            stray: Vec::new(),
            cpu_instr: 0,
            cop_instr: 0,
            epoch: 0,
            skipped: [0; 2],
            code_pages: vec![false; 34816].into_boxed_slice().try_into().unwrap(),
            snap_on: false,
            call_on: false,
            call_literal: false,
            call_unaligned: false,
            call_mis: None,
            snap_orig: Default::default(),
            snap_added: Vec::new(),
            call_trace: None,
            call_trace_log: Vec::new(),
            call_reg_log: Vec::new(),
            call_mapped: Default::default(),
            call_pages: Default::default(),
            call_wr_ok: Vec::new(),
            call_log_on: false,
            call_log: Vec::new(),
            call_pc: 0,
            call_fault: None,
            snap_dirty: Vec::new(),
            snap_dram: None,
            snap_iram: None,
            dirty: Vec::new(),
            dirty_flag: false,
            backlight_ok: false,
            click_armed: false,
            clicks: Vec::new(),
            io: HashMap::new(),
            io_last: HashMap::new(),
            ev_on: false,
            ev_ranges: Vec::new(),
            ev_log: Vec::new(),
            ev_cap: 0,
            cfg,
        };
        s.dram[..osos.len()].copy_from_slice(osos);
        if !s.cfg.plugged {
            s.gp[in_val_idx(2)] = (1 << 3) | (1 << 4) | (1 << 7);
        }
        if s.cfg.keys_up {
            s.gp[in_val_idx(0)] = 0x1f;
        }
        let word = |a: usize| u32::from_le_bytes(osos[a..a + 4].try_into().unwrap());
        s.backlight_ok = osos.len() >= 0x3d6f0 + START_VOL_CODE.len() && osos[0x3d6f0..0x3d6f0 + START_VOL_CODE.len()] == START_VOL_CODE;
        let start_vol = match s.cfg.start_volume {
            Some(v) if s.backlight_ok && osos.len() >= DEFAULT_VOL_AT + 4 && word(DEFAULT_VOL_AT) == DEFAULT_VOL_INSN => Some(v.min(127)),
            _ => None,
        };
        // the click routine's store: a STR in the routine's range
        s.click_armed = osos.len() > 0x37760 && word(CLICK_AT as usize) & 0x0c50_0000 == 0x0400_0000 && std::env::var("EMUPOD_RS_CLICK_REG").is_err();
        let mut m = Machine { watch_on: false, watch_addr: 0, watch_hit: None, watch_regs: [0; 16], watch_stop: true, watch_log: Vec::new(), bp_pc: 0xffff_ffff, bp_mem: Vec::new(), ring: std::collections::VecDeque::new(), bp_hits: Vec::new(), snap_cpu: None, hook_pcs: Vec::new(), hook_log: Vec::new(), hook_peek: None, cop_trace_left: 0, cop_log: Vec::new(), s, cpu: Core::new(), cop: Core::new() };
        m.cpu.cpu.r[13] = 0x4008_0000;
        m.cop.cpu.r[13] = 0x4008_0000;
        m.cpu.cpu.pc = ENTRY;
        m.cop.cpu.pc = ENTRY;
        if has_cop {
            // Reset: both cores run the reset code at once on hardware, before either has changed anything. Here the
            // coprocessor runs it first, to its sleep, and the CPU then starts from the same vector.
            m.run_cop(5_000_000);
            assert!(m.s.cop_asleep, "the coprocessor did not put itself to sleep at reset: pc {:#x}", m.cop.cpu.pc);
            if !m.s.cfg.keep_cop_reset_state {
                m.s.dram.fill(0);
                m.s.dram[..osos.len()].copy_from_slice(osos);
                m.s.iram.fill(0);
            }
            m.s.cop_sleeps = 0;
        }
        if let Some(v) = start_vol {
            // after the reset run, which reloads the image
            let insn = DEFAULT_VOL_INSN & !0xff | 2 * v;
            m.s.dram[DEFAULT_VOL_AT..DEFAULT_VOL_AT + 4].copy_from_slice(&insn.to_le_bytes());
        }
        m
    }

    #[inline]
    pub fn now(&self) -> u64 {
        self.s.now()
    }

    /// Whether the player has its backlight on (1.5-family firmware only; false for any other).
    pub fn backlight(&self) -> bool {
        self.s.backlight_ok && u32::from_le_bytes(self.s.dram[BACKLIGHT_AT..BACKLIGHT_AT + 4].try_into().unwrap()) != 0
    }

    /// Run the coprocessor `n` instructions one at a time and return the pc before each (to study what a firmware has it do).
    /// (for study) run the CPU one instruction at a time, up to `max`, until the word at `addr` changes; returns the pc of the
    /// instruction that changed it, its register r0..r3 and lr, and how many instructions ran.
    pub fn cpu_find_write(&mut self, addr: u32, max: u64) -> Option<(u32, [u32; 7], u64)> {
        let before = self.s.rd(addr, 4);
        for n in 0..max {
            let pc = self.cpu.cpu.pc;
            let regs = [self.cpu.cpu.r[0], self.cpu.cpu.r[1], self.cpu.cpu.r[2], self.cpu.cpu.r[3], self.cpu.cpu.r[4], self.cpu.cpu.r[12], self.cpu.cpu.r[14]];
            self.run_cpu(1);
            if self.s.rd(addr, 4) != before {
                return Some((pc, regs, n));
            }
        }
        None
    }

    /// (for study) like cpu_find_write for the coprocessor (it must be awake): the pc of the store, r0..r3, r4, ip, lr and the count.
    pub fn cop_find_write(&mut self, addr: u32, max: u64) -> Option<(u32, [u32; 7], u64)> {
        let before = self.s.rd(addr, 4);
        for n in 0..max {
            let c = &self.cop.cpu;
            let pc = c.pc;
            let regs = [c.r[0], c.r[1], c.r[2], c.r[3], c.r[4], c.r[12], c.r[14]];
            self.run_cop(1);
            if self.s.rd(addr, 4) != before {
                return Some((pc, regs, n));
            }
        }
        None
    }

    pub fn cop_wake_for_study(&mut self) {
        self.s.cop_asleep = false;
    }

    pub fn cop_trace(&mut self, n: usize) -> Vec<(u32, u32)> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push((self.cop.cpu.pc, self.cop.cpu.r[0]));
            self.run_cop(1);
            if self.s.cop_asleep {
                break;
            }
        }
        out
    }

    fn run_cop(&mut self, count: u64) {
        self.s.who = Who::Cop;
        self.s.stop_now = false;
        self.s.fetched = 0;
        let mut done = 0u64;
        while self.cop_trace_left > 0 && done < count {
            self.cop_log.push((self.cop.cpu.pc, self.cop.cpu.r[13]));
            self.cop_trace_left -= 1;
            self.s.fetched = 0;
            done += self.cop.run(&mut self.s, 1);
            if self.s.stop_now {
                break;
            }
        }
        let n = done + if self.s.stop_now { 0 } else { self.cop.run(&mut self.s, count - done) };
        self.s.cop_instr += n;
        self.s.stop_now = false;
        self.s.who = Who::Cpu;
    }

    fn run_cpu(&mut self, count: u64) {
        self.s.who = Who::Cpu;
        self.s.stop_now = false;
        self.s.slept = false;
        self.s.fetched = 0;
        let n = if self.hook_pcs.is_empty() {
            self.cpu.run(&mut self.s, count)
        } else {
            let mut done = 0u64;
            while done < count {
                let pc = self.cpu.cpu.pc;
                if self.hook_pcs.contains(&pc) && self.hook_log.len() < 50_000 {
                    let peek = match self.hook_peek {
                        Some((reg, off)) => { let a = self.cpu.cpu.r[reg].wrapping_add(off); self.s.rd(a, 4) }
                        None => 0,
                    };
                    self.hook_log.push((self.s.now_base, pc, self.cpu.cpu.r, peek));
                }
                self.s.fetched = 0;
                let k = self.cpu.run(&mut self.s, 1);
                done += k.max(1);
                if self.s.stop_now || k == 0 {
                    break;
                }
            }
            done
        };
        self.s.cpu_instr += n;
        self.s.stop_now = false;
    }

    /// Study / call harness: the CPU's r0..r14, pc (index 15) and cpsr.
    pub fn call_regs(&self) -> ([u32; 16], u32) {
        let mut r = self.cpu.cpu.r;
        r[15] = self.cpu.cpu.pc;
        (r, self.cpu.cpu.cpsr)
    }

    /// Set the CPU's r0..r14, pc (index 15) and cpsr. A cpsr with another mode switches the register banks (any mode bits, valid or not) and
    /// then r13 and r14 are what the new mode's bank holds, not the values in `r`.
    pub fn set_cpu_regs(&mut self, r: [u32; 16], cpsr: u32) -> Result<(), String> {
        let c = &mut self.cpu.cpu;
        if cpsr & 0x1f != c.cpsr & 0x1f {
            c.set_cpsr(cpsr);                           // another mode: the register banks switch, as on any ARM and on Unicorn
            let keep = (c.r[13], c.r[14]);              // ... and r13/r14 are what that mode's bank holds
            c.r = r;
            c.r[13] = keep.0;
            c.r[14] = keep.1;
        } else {
            c.r = r;
            c.cpsr = cpsr;
        }
        c.pc = r[15];
        Ok(())
    }

    /// Call harness: remember the CPU's registers and all of memory (DRAM and IRAM) as they are now. From here on writes are tracked, so
    /// `restore` costs the 1 KB pages a call changed. Devices (the clock, the disk, the LCD, the timer) are NOT part of it: a call
    /// is CPU only, and a call that touches a device leaves that device changed.
    pub fn snapshot(&mut self) {
        self.s.snap_dram = Some(self.s.dram.to_vec().into_boxed_slice());
        self.s.snap_iram = Some(self.s.iram.to_vec().into_boxed_slice());
        self.s.snap_dirty = vec![0u8; 34816];
        self.s.snap_on = true;
        self.snap_cpu = Some(self.cpu.cpu.clone());
        self.s.snap_orig.clear();
        self.s.snap_added.clear();
    }

    /// Put the registers and memory back as `snapshot` found them. Returns the number of pages copied back, or None with no snapshot.
    pub fn restore(&mut self) -> Option<usize> {
        let cpu = self.snap_cpu.as_ref()?.clone();
        let s = &mut self.s;
        s.snap_on = false;
        let mut n = 0;
        for page in 0..s.snap_dirty.len() {
            if s.snap_dirty[page] == 0 {
                continue;
            }
            s.snap_dirty[page] = 0;
            n += 1;
            if page < 0x8000 {
                let (a, b) = (page << 10, (page << 10) + 1024);
                s.dram[a..b].copy_from_slice(&s.snap_dram.as_ref().unwrap()[a..b]);
            } else {
                let (a, b) = ((page - 0x8000) << 10, ((page - 0x8000) << 10) + 1024);
                if b <= IRAM_SIZE {
                    s.iram[a..b].copy_from_slice(&s.snap_iram.as_ref().unwrap()[a..b]);
                }
            }
            s.touch_code(page);                                   // decoded code from a restored page is stale
        }
        s.snap_on = true;
        self.cpu.cpu = cpu;
        for (pk, orig) in s.snap_orig.drain() {                       // plain pages written since: as they were
            s.call_pages.insert(pk, orig);
        }
        for pk in s.snap_added.drain(..) {                            // pages mapped since: unmapped again
            s.call_mapped.remove(&pk);
            s.call_pages.remove(&pk);
        }
        s.call_fault = None;
        Some(n)
    }

    /// Call harness: run the CPU alone (no timer, IRQ or coprocessor) from its registers until it
    ///   0: reaches `sentinel` (the return address the caller put in lr),
    ///   1: has run `budget` instructions,
    ///   2: leaves [lo, hi) when hi > lo (the exit: pc, lr and the registers say where to),
    /// DRAM aliases (0x0, 0x4000000, 0x28000000) are folded for the range and the sentinel. Returns (status, instructions run).
    pub fn call_run(&mut self, sentinel: u32, budget: u64, lo: u32, hi: u32, stops: &[u32]) -> (u32, u64) {
        let literal = self.s.call_literal;
        let fold = move |a: u32| if !literal && a < 0x2800_0000 { a % 0x0400_0000 + 0x2800_0000 } else { a };
        let (sentinel, lo, hi) = (fold(sentinel), fold(lo), fold(hi));
        let stops: Vec<u32> = stops.iter().map(|&a| fold(a)).collect();
        self.s.who = Who::Cpu;
        self.cpu.sync(&mut self.s);                      // code that was written from outside since the last run: its old decode is stale
        self.s.call_fault = None;
        let mut n = 0u64;
        while n < budget {
            let pc = fold(self.cpu.cpu.pc);
            if pc == sentinel {
                return (0, n);
            }
            if hi > lo && !(pc >= lo && pc < hi) {
                return (2, n);
            }
            if !stops.is_empty() && stops.contains(&pc) {
                return (4, n);                           // one of the exit addresses inside the range (a call the host answers)
            }
            if let Some((tlo, thi, regs_at)) = self.s.call_trace.as_ref() {
                let (tlo, thi) = (*tlo, *thi);
                if pc >= tlo && pc < thi && self.s.call_trace_log.len() < 4_000_000 {
                    self.s.call_trace_log.push((self.cpu.cpu.pc, self.cpu.cpu.cpsr));
                }
                if regs_at.contains(&pc) && self.s.call_reg_log.len() < 1_000_000 {
                    let r = self.cpu.cpu.r[..13].to_vec();
                    self.s.call_reg_log.push((self.cpu.cpu.pc, r));
                }
            }
            if self.s.call_on {
                if !self.s.call_mapped_range(self.cpu.cpu.pc, 4) {
                    self.s.call_fault = Some((3, self.cpu.cpu.pc, 4, self.cpu.cpu.pc, 0));        // an instruction fetch from an unmapped page
                    return (3, n);
                }
                self.s.call_pc = self.cpu.cpu.pc;
                self.s.call_mis = None;
                let before = self.cpu.cpu.clone();
                self.s.stop_now = false;
                self.s.fetched = 0;
                self.cpu.run(&mut self.s, 1);
                if self.s.call_fault.is_some() {
                    self.cpu.cpu = before;                // the instruction did not happen: the host maps the page, fills it, and calls again
                    return (3, n);
                }
                n += 1;
                continue;
            }
            self.s.stop_now = false;
            self.s.fetched = 0;
            n += self.cpu.run(&mut self.s, 1).max(1);
        }
        (1, n)
    }

    /// Trace the calls: every executed instruction with pc in [lo, hi) is logged as (pc, cpsr), and r0..r12 whenever pc is one of `reg_pcs`.
    pub fn call_trace_set(&mut self, lo: u32, hi: u32, reg_pcs: Vec<u32>) {
        self.s.call_trace = Some((lo, hi, reg_pcs.into_iter().collect()));
        self.s.call_trace_log.clear();
        self.s.call_reg_log.clear();
    }
    pub fn call_trace_off(&mut self) {
        self.s.call_trace = None;
    }
    pub fn call_trace_take(&mut self) -> (Vec<(u32, u32)>, Vec<(u32, Vec<u32>)>) {
        (std::mem::take(&mut self.s.call_trace_log), std::mem::take(&mut self.s.call_reg_log))
    }

    /// Call mode on or off. `literal`: no aliases and no DRAM/IRAM arrays, every address is what the host mapped (code runs from plain pages
    /// too); `unaligned_faults`: a halfword or word access at a misaligned address faults (kind 4). On: only pages the host maps (`call_map_page`) exist; an access to any other address stops the call with
    /// status 3 and `call_fault`, the instruction not done. Peripherals are plain memory then, never devices.
    pub fn call_mode(&mut self, on: bool, literal: bool, unaligned_faults: bool) {
        self.s.call_on = on;
        self.s.call_literal = literal;
        self.s.call_unaligned = unaligned_faults;
        self.s.call_fault = None;
        self.s.call_log.clear();
    }

    /// Map the 4 KB page of `addr` and fill it with `data` (4096 bytes): the gate's lazily drawn page.
    pub fn call_map_page(&mut self, addr: u32, data: &[u8]) {
        let base = addr & !0xfff;
        let pk = self.s.canon(base) >> 12;
        if self.s.call_mapped.insert(pk) && self.s.snap_on {
            self.s.snap_added.push(pk);
        }
        for (i, &b) in data.iter().take(4096).enumerate() {
            let a = base.wrapping_add(i as u32);
            if self.s.is_ram(a) {
                self.s.wr_inner(a, 1, b as u32);
            } else {
                self.s.call_page_write(a, b);
            }
        }
    }

    /// Writes are refused (status 3, fault kind 2, before they land) outside these [lo, hi) ranges; none given: any mapped page is writable.
    pub fn call_write_ranges(&mut self, ranges: Vec<(u32, u32)>) {
        self.s.call_wr_ok = ranges.into_iter().map(|(lo, hi)| (self.s.canon(lo), self.s.canon(hi.wrapping_sub(1)).wrapping_add(1))).collect();
    }

    pub fn call_log_on(&mut self, on: bool) {
        self.s.call_log_on = on;
        self.s.call_log.clear();
    }

    /// Take the access log: (0 read / 1 write, address, size, value, pc of the instruction).
    pub fn call_log_take(&mut self) -> Vec<(u8, u32, u8, u32, u32)> {
        std::mem::take(&mut self.s.call_log)
    }

    /// The last fault: (kind: 0 unmapped read, 1 unmapped write, 2 write outside the allowed ranges, 3 unmapped instruction fetch, 4 misaligned; address; size; pc;
    /// value: the value a refused or unmapped store was trying to write, else 0).
    pub fn call_fault(&self) -> Option<(u8, u32, u8, u32, u32)> {
        self.s.call_fault
    }

    /// Deliver IRQ `bit` by hand if IRQs are enabled (ARM exception entry).
    pub fn inject(&mut self, bit: u32) -> bool {
        if self.cpu.cpu.cpsr & armcore::BIT_I != 0 {
            return false;
        }
        if self.s.cfg.irq_mask && self.s.cpu_en & (1 << bit) == 0 {
            return false;
        }
        self.s.pending |= 1 << bit;
        let took = self.cpu.cpu.irq(&mut self.s);
        self.s.irqs += took as u64;
        took
    }

    /// Deliver the timer tick if one is due and interrupts are enabled.
    pub fn tick(&mut self) -> bool {
        if !self.s.tick_due || !self.inject(11) {
            return false;
        }
        self.s.tick_due = false;
        self.s.ticks += 1;
        true
    }

    /// The audio FIQ to the coprocessor (which it also wakes), if not masked.
    fn fiq(&mut self) -> bool {
        if self.cop.cpu.cpsr & armcore::BIT_F != 0 {
            return false;
        }
        self.cop.cpu.fiq(&mut self.s);
        self.s.cop_asleep = false;
        let a = &mut self.s.audio;
        let wrote = (a.samples.len() - a.seen) / 2;
        a.seen = a.samples.len();
        if wrote != 0 && self.s.cfg.audio_fpf.is_none() {
            a.fpf = wrote as u64;
        }
        a.fiqs += 1;
        a.n += 1;
        a.due += a.fpf;
        a.next = Some(a.t0 + (a.due + a.fpf) * 1_000_000 / SAMPLE_RATE);
        true
    }

    /// Run one slice of virtual time from the CPU's pc and advance the clock.
    pub fn slice(&mut self) {
        let s = &mut self.s;
        let now = s.now_base;
        let dl = s.t1.deadline;
        let mut q = match dl {
            None => s.cfg.quantum,
            Some(d) => 1.max(s.cfg.quantum.min(d.saturating_sub(now))),
        };
        if let Some(w) = s.wake_at {
            if w > now {
                q = q.min(w - now);
            }
        }
        if let Some(mut nf) = s.audio.next {
            if nf <= now {
                if self.fiq() {
                    nf = self.s.audio.next.unwrap();
                } else {
                    self.s.audio.late += 1;
                }
            }
            q = q.min(20.max(nf.saturating_sub(now)));
        }
        if self.s.has_cop && !self.s.cop_asleep {
            let playing = self.s.cfg.interleave_playing != 0
                && self.s.audio.on
                && now - self.s.audio.t0 >= self.s.cfg.interleave_settle;
            q = q.min(if playing { self.s.cfg.interleave_playing } else { self.s.cfg.interleave });
        }
        self.s.qlen = q;
        self.s.sub = 0;
        self.s.charge = 0;
        if self.s.pending & (1 << SIGNAL_IRQ) != 0 {
            self.inject(SIGNAL_IRQ);
        }
        if self.s.cfg.irq_mask && self.s.pending & self.s.cpu_en & self.s.gpio_en_bit != 0 && self.cpu.cpu.cpsr & armcore::BIT_I == 0 {
            let took = self.cpu.cpu.irq(&mut self.s);
            self.s.irqs += took as u64;
        }
        let ipus = self.s.cfg.ipus;
        let w0 = if self.watch_on { self.s.rd(self.watch_addr, 4) } else { 0 };
        self.run_cpu(q * ipus);
        if self.watch_on && self.s.rd(self.watch_addr, 4) != w0 {
            if self.watch_log.len() < 4000 {
                let v = self.s.rd(self.watch_addr, 4);
                self.watch_log.push((0, self.s.now_base, self.cpu.cpu.pc, v, self.cpu.cpu.r[13]));
            }
            if self.watch_hit.is_none() {
                self.watch_hit = Some((0, self.s.now_base, self.cpu.cpu.pc, self.cpu.cpu.r[14]));
            }
        }
        let slept = self.s.slept;
        let charge = self.s.charge;
        let sub_eq = if self.s.cfg.slice_clock { self.s.sub } else { self.s.fetched / ipus };
        let mut busy = false;
        if self.s.has_cop && !self.s.cop_asleep {
            let n = q * self.s.cfg.cop_ipus;
            if self.watch_on && (self.watch_hit.is_none() || !self.watch_stop) {
                // study: one instruction at a time, to name the store
                let mut w1 = self.s.rd(self.watch_addr, 4);
                for _ in 0..n {
                    let (pc, lr) = (self.cop.cpu.pc, self.cop.cpu.r[14]);
                    let r = self.cop.cpu.r;
                    if self.ring.len() >= 600000 {
                        self.ring.pop_front();
                    }
                    self.ring.push_back((pc, r));
                    if pc == self.bp_pc && self.bp_hits.len() < 8 {
                        self.bp_hits.push((self.s.now_base, r));
                        let sp = r[13];
                        let snap: Vec<u32> = (0..16).map(|i| self.s.rd(sp.wrapping_add(4 * i), 4)).collect();
                        self.bp_mem.push(snap);
                    }
                    self.run_cop(1);
                    let w2 = self.s.rd(self.watch_addr, 4);
                    if w2 != w1 {
                        if self.watch_log.len() < 4000 {
                            self.watch_log.push((1, self.s.now_base, pc, w2, r[13]));
                        }
                        w1 = w2;
                        if self.watch_hit.is_none() {
                            self.watch_hit = Some((1, self.s.now_base, pc, lr));
                            self.watch_regs = r;
                        }
                        if self.watch_stop {
                            break;
                        }
                    }
                    if self.s.cop_asleep {
                        break;
                    }
                }
            } else {
                self.run_cop(n);
            }
            busy = !self.s.cop_asleep;
        }
        let s = &mut self.s;
        s.who = Who::Cpu;
        if charge != 0 {
            s.now_base += sub_eq + (charge + ipus - 1) / ipus;
        } else if !slept || dl.is_none() || busy {
            s.now_base += q;
        } else if s.tick_due || s.pending & (1 << SIGNAL_IRQ) != 0 {
            s.now_base += sub_eq;
        } else {
            let w = [dl, s.wake_at, s.audio.next].iter().flatten().copied().min().unwrap();
            s.now_base = (s.now_base + sub_eq).max(w);
        }
        s.sub = 0;
        s.fetched = 0;
        if let Some(d) = dl {
            if s.now_base >= d {
                s.tick_due = true;
                let p = s.t1.period;
                s.t1.deadline = Some(d + p * ((s.now_base - d) / p + 1));
            }
        }
    }

    /// One step of a driver loop: slices until the next tick is due (or 16 slices before the timer is programmed).
    pub fn run(&mut self) {
        let until = self.s.now_base + if self.s.t1.deadline.is_none() { 16 * self.s.cfg.quantum } else { 4 * self.s.t1.period };
        while self.s.now_base < until {
            let was_due = self.s.tick_due;
            self.slice();
            if self.s.tick_due && (!was_due || self.cpu.cpu.cpsr & armcore::BIT_I == 0) {
                break;
            }
        }
    }

    /// Tick the timer until the LCD goes quiet for `quiet` ticks (at most `max_iters`).
    pub fn settle(&mut self, max_iters: u32, quiet: u32) -> u32 {
        let mut last = self.s.lcd_count;
        let mut stable = 0;
        for _ in 0..max_iters {
            self.tick();
            self.run();
            if self.s.lcd_count == last {
                stable += 1;
                if stable >= quiet {
                    break;
                }
            } else {
                stable = 0;
                last = self.s.lcd_count;
            }
        }
        self.s.lcd_count
    }

    /// Run from reset until the first full frame is drawn, then let the menu become input-ready.
    pub fn boot(&mut self) -> u32 {
        self.cpu.cpu.pc = ENTRY;
        for _ in 0..3000 {
            self.run();
            if self.s.lcd_count > FRAME - 1 {
                break;
            }
            self.tick();
        }
        self.settle(300, 12);
        self.s.lcd_count
    }

    pub fn set_pin(&mut self, port: usize, mask: u32, level: bool) {
        let a = in_val_idx(port);
        self.s.gp[a] = if level { self.s.gp[a] | mask } else { self.s.gp[a] & !mask };
    }

    /// One GPIO edge, `Emulator._edge`'s way: run to the edge's moment, latch it, deliver IRQ14 as soon as IRQs are on.
    pub fn edge(&mut self, port: usize, mask: u32) -> bool {
        let when = self.s.now_base + 1000;
        self.s.wake_at = Some(when);
        while self.s.now_base < when {
            self.tick();
            self.slice();
        }
        self.s.wake_at = None;
        self.s.gp[int_stat_idx(port)] |= mask;
        for _ in 0..400 {
            let c = self.cpu.cpu.cpsr;
            if self.s.cfg.irq_mask && self.s.cpu_en & self.s.gpio_en_bit == 0 {
                // masked for now: stays pending, delivered when the firmware enables it (see slice)
                self.s.pending = self.s.gpio_en_bit | (self.s.pending & (1 << SIGNAL_IRQ));
                return true;
            }
            if c & armcore::BIT_I == 0 {
                self.s.pending = self.s.gpio_en_bit | (self.s.pending & (1 << SIGNAL_IRQ));
                let took = self.cpu.cpu.irq(&mut self.s);
                self.s.irqs += took as u64;
                return true;
            }
            self.slice();
        }
        false
    }

    pub fn pin(&self, port: usize, mask: u32) -> bool {
        self.s.gp[in_val_idx(port)] & mask != 0
    }

    pub fn underruns(&mut self) -> u32 {
        self.s.rd(UNDERRUNS, 4)
    }
    /// 1.5's own count of sound underruns (its FIQ found the ring empty and sent silence), or None for any other firmware
    pub fn fw_underruns(&mut self) -> Option<u32> {
        if self.s.backlight_ok { Some(self.s.rd(UNDERRUNS, 4)) } else { None }
    }
    /// Bytes as the CPU would read them (a device address is read as the firmware would, with its effects).
    pub fn read_bytes(&mut self, a: u32, n: usize) -> Vec<u8> {
        (0..n).map(|i| self.s.host_rd(a.wrapping_add(i as u32))).collect()
    }
    pub fn write_bytes(&mut self, a: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.s.host_wr(a.wrapping_add(i as u32), b);
        }
    }
    pub fn mem_read32(&mut self, a: u32) -> u32 {
        self.s.rd(a, 4)
    }
    pub fn dram(&self) -> &[u8] {
        &self.s.dram[..]
    }
    pub fn iram(&self) -> &[u8] {
        &self.s.iram[..]
    }
    pub fn cpu_regs(&self) -> &Cpu {
        &self.cpu.cpu
    }
}
