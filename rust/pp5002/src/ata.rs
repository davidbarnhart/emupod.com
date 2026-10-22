//! The PP5002's ATA task file, backed by a disk image (a port of `_ATA` in emupod/core/ipod_emu_ui.py).
//!
//! Commands: 0x90 diagnostic, 0xEC IDENTIFY, 0xEF SET FEATURES, 0xC6 SET MULTIPLE, 0x20/0x21/0xC4/0xC8 reads,
//! 0x30/0x31/0xC5/0xCA writes (and the 48-bit forms 0x24, 0x29, 0x25, 0x34, 0x39, 0x35), and the no-data
//! power-management group. Anything else aborts, as a drive would. BSY can be held for a number of status reads
//! or a number of microseconds of the iPod's clock.

use std::collections::HashMap;

/// What the disk is stored in: a file (native), or whatever the host gives (the browser's OPFS, in a Worker).
pub trait Disk: Send + Sync {
    fn len(&self) -> u64;
    /// Reads what exists; the rest of `buf` is left as it was (zeros).
    fn read_at(&self, buf: &mut [u8], off: u64);
    fn write_at(&self, buf: &[u8], off: u64);
}

#[cfg(not(target_arch = "wasm32"))]
pub struct FileDisk(pub std::fs::File);
#[cfg(not(target_arch = "wasm32"))]
impl Disk for FileDisk {
    fn len(&self) -> u64 {
        self.0.metadata().map(|m| m.len()).unwrap_or(0)
    }
    fn read_at(&self, buf: &mut [u8], off: u64) {
        use std::os::unix::fs::FileExt;
        let _ = self.0.read_at(buf, off);
    }
    fn write_at(&self, buf: &[u8], off: u64) {
        use std::os::unix::fs::FileExt;
        let _ = self.0.write_at(buf, off);
    }
}

pub const DATA: u32 = 0xc00031e0;
pub const FEAT: u32 = 0xc00031e4;
pub const COUNT: u32 = 0xc00031e8;
pub const LBA0: u32 = 0xc00031ec;
pub const LBA1: u32 = 0xc00031f0;
pub const LBA2: u32 = 0xc00031f4;
pub const DEVHEAD: u32 = 0xc00031f8;
pub const CMD: u32 = 0xc00031fc;
pub const ALT: u32 = 0xc00033f8;

const BSY: u32 = 0x80;
const DRDY: u32 = 0x40;
const DRQ: u32 = 0x08;
const ERR: u32 = 0x01;
const ABRT: u32 = 0x04;
const UNC: u32 = 0x40;
const IDNF: u32 = 0x10;
const NODATA: [u32; 19] = [
    0xe7, 0xea, 0xe0, 0xe1, 0xe2, 0xe3, 0xe5, 0xe6, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x91, 0x10, 0x40, 0x41, 0x70,
];

pub fn is_taskfile(a: u32) -> bool {
    matches!(a, 0xc00031e0 | 0xc00031e4 | 0xc00031e8 | 0xc00031ec | 0xc00031f0 | 0xc00031f4 | 0xc00031f8 | 0xc00031fc)
}

pub struct Ata {
    file: Option<Box<dyn Disk>>,
    pub total: u64,
    persist: bool,
    overlay: HashMap<u64, [u8; 512]>,
    r: [u32; 8],   // COUNT, LBA0, LBA1, LBA2, DEVHEAD (indexed by `slot`)
    hob: [u32; 4], // the "previous" byte of COUNT, LBA0, LBA1, LBA2
    feat: u32,
    err: u32,
    fifo: Vec<u8>,
    fpos: usize,
    wbuf: Vec<u8>,
    wlba: u64,
    wleft: u64,
    wdone: u64,
    pub multiple: u32,
    block: usize,
    pub bsy_reads: u32,
    bsy: u32,
    pub cmd_us: u64,
    pub block_us: u64,
    ready_at: u64,
    pub busy_us: u64,
    failed: bool,
    read_err: Option<u32>,
    force_lba48: bool,
    pub log: Vec<(u32, u32, u32, u64, &'static str)>,
    pub bsy_served: u64,
    /// I/O the firmware has done: commands, read and write commands, and the sectors moved (for statistics)
    pub n_cmds: u64,
    /// Study: the drive's power state. With `power_gate` (env EMUPOD_RS_DISK_POWER, native builds), STANDBY IMMEDIATE/STANDBY/SLEEP
    /// (E0, E2, E6) power the drive down and a read or write issued then never completes (BSY for good) until IDLE (E1, E3) powers it
    /// up: how the 1.7 Refresh Library stalled on hardware when it held no disk-manager client.
    pub power_gate: bool,
    pub asleep: bool,
    pub stalled: u64,
    /// Every command that is not a data transfer, with the emulated time: (now_us, opcode, state after: 0 awake, 1 powered down, features, count): SET FEATURES (EF) is in it too.
    pub power_log: Vec<(u64, u32, u32, u32, u32)>,
    pub n_reads: u64,
    pub n_writes: u64,
    pub sectors_read: u64,
    pub sectors_written: u64,
}

fn slot(a: u32) -> usize {
    match a {
        COUNT => 0,
        LBA0 => 1,
        LBA1 => 2,
        LBA2 => 3,
        _ => 4,
    }
}

impl Ata {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(path: &str, persist: bool, bsy_reads: u32, cmd_us: u64, block_us: u64, force_lba48: bool) -> std::io::Result<Ata> {
        let f = if persist { std::fs::OpenOptions::new().read(true).write(true).open(path)? } else { std::fs::File::open(path)? };
        Ok(Ata::with_disk(Box::new(FileDisk(f)), persist, bsy_reads, cmd_us, block_us, force_lba48))
    }

    pub fn with_disk(disk: Box<dyn Disk>, persist: bool, bsy_reads: u32, cmd_us: u64, block_us: u64, force_lba48: bool) -> Ata {
        let total = disk.len() / 512;
        Ata {
            file: Some(disk),
            total,
            persist,
            overlay: HashMap::new(),
            r: [0, 0, 0, 0, 0xa0, 0, 0, 0],
            hob: [0; 4],
            feat: 0,
            err: 0,
            fifo: Vec::new(),
            fpos: 0,
            wbuf: Vec::new(),
            wlba: 0,
            wleft: 0,
            wdone: 0,
            multiple: 0,
            block: 512,
            bsy_reads,
            bsy: 0,
            cmd_us,
            block_us,
            ready_at: 0,
            busy_us: 0,
            failed: false,
            read_err: None,
            force_lba48,
            log: Vec::new(),
            bsy_served: 0,
            n_cmds: 0,
            #[cfg(not(target_arch = "wasm32"))]
            power_gate: std::env::var("EMUPOD_RS_DISK_POWER").is_ok(),
            #[cfg(target_arch = "wasm32")]
            power_gate: false,
            asleep: false,
            stalled: 0,
            power_log: Vec::new(),
            n_reads: 0,
            n_writes: 0,
            sectors_read: 0,
            sectors_written: 0,
        }
    }

    fn sector(&self, l: u64) -> [u8; 512] {
        if let Some(s) = self.overlay.get(&l) {
            return *s;
        }
        let mut b = [0u8; 512];
        if let Some(f) = &self.file {
            let _ = f.read_at(&mut b, l * 512);
        }
        b
    }

    fn read_lba(&self, lba: u64, n: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(n as usize * 512);
        for i in 0..n {
            out.extend_from_slice(&self.sector(lba + i));
        }
        out
    }

    fn identify(&self) -> Vec<u8> {
        let mut w = [0u8; 512];
        let mut sw = |i: usize, v: u32| {
            w[i * 2] = v as u8;
            w[i * 2 + 1] = (v >> 8) as u8;
        };
        sw(0, 0x0040);
        sw(1, 16383);
        sw(3, 16);
        sw(6, 63);
        let mut put = |at: usize, s: &str, words: usize| {
            let mut b = s.as_bytes().to_vec();
            b.resize(words * 2, b' ');
            for i in 0..words * 2 {
                w[at + i] = b[i ^ 1];
            }
        };
        put(20, "OPENPOD00000001", 10);
        put(46, "1.0", 4);
        put(54, "openpod disk", 20);
        let mut sw = |i: usize, v: u32| {
            w[i * 2] = v as u8;
            w[i * 2 + 1] = (v >> 8) as u8;
        };
        sw(47, 0x8010);
        sw(49, 0x0300);
        sw(53, 0x0007);
        if self.multiple != 0 {
            sw(59, 0x0100 | self.multiple);
        }
        let lba28 = self.total.min(0x0fff_ffff);
        sw(60, (lba28 & 0xffff) as u32);
        sw(61, ((lba28 >> 16) & 0xffff) as u32);
        if self.total > 0x0fff_ffff || self.force_lba48 {
            sw(83, 0x4400);
            sw(86, 0x0400);
            for i in 0..4 {
                sw(100 + i, ((self.total >> (16 * i)) & 0xffff) as u32);
            }
        }
        w.to_vec()
    }

    fn wait(&mut self, now: u64, us: u64) {
        if us != 0 {
            self.ready_at = now + us;
            self.busy_us += us;
        }
    }

    fn stop(&mut self, err: u32) {
        self.failed = true;
        self.err = err;
        self.fifo.clear();
        self.fpos = 0;
        self.wleft = 0;
        self.read_err = None;
    }

    pub fn status(&mut self, now: u64) -> u32 {
        if self.bsy > 0 {
            self.bsy -= 1;
            self.bsy_served += 1;
            return BSY;
        }
        if self.ready_at != 0 && now < self.ready_at {
            self.bsy_served += 1;
            return BSY;
        }
        if self.failed {
            return DRDY | ERR;
        }
        DRDY | if self.fpos < self.fifo.len() || self.wleft > 0 { DRQ } else { 0 }
    }

    pub fn read(&mut self, a: u32, now: u64) -> u32 {
        match a {
            CMD | ALT => self.status(now),
            FEAT => self.err,
            DATA => {
                let mut v = 0;
                if self.fpos + 1 < self.fifo.len() {
                    v = self.fifo[self.fpos] as u32 | (self.fifo[self.fpos + 1] as u32) << 8;
                    self.fpos += 2;
                    if self.fpos >= self.fifo.len() {
                        if let Some(e) = self.read_err {
                            self.stop(e);
                        }
                        self.bsy = self.bsy_reads;
                    } else if self.fpos % self.block == 0 {
                        self.bsy = self.bsy_reads;
                        let bu = self.block_us;
                        self.wait(now, bu);
                    }
                }
                v
            }
            _ => self.r[slot(a)],
        }
    }

    /// `n` bytes of DATA reads at once: what n/2 `read(DATA)` calls return, leaving the same state.
    pub fn read_block(&mut self, n: usize, now: u64, out: &mut Vec<u8>) {
        let start = self.fpos;
        let avail = self.fifo.len().saturating_sub(start) & !1;
        let take = (n & !1).min(avail);
        out.clear();
        out.extend_from_slice(&self.fifo[start..start + take]);
        self.fpos = start + take;
        if take != 0 {
            let bu = self.block_us;
            if self.fpos >= self.fifo.len() {
                let mut p = (start / self.block + 1) * self.block;
                while p < self.fpos {
                    self.wait(now, bu);
                    p += self.block;
                }
                if let Some(e) = self.read_err {
                    self.stop(e);
                }
                self.bsy = self.bsy_reads;
            } else {
                let mut crossed = false;
                let mut p = (start / self.block + 1) * self.block;
                while p <= self.fpos {
                    self.wait(now, bu);
                    crossed = true;
                    p += self.block;
                }
                if crossed {
                    self.bsy = self.bsy_reads;
                }
            }
        }
        out.resize(n, 0);
    }

    pub fn write(&mut self, a: u32, v: u32, now: u64) {
        match a {
            CMD => self.cmd(v & 0xff, now),
            FEAT => self.feat = v & 0xff,
            DATA => {
                if self.wleft > 0 {
                    self.wbuf.push(v as u8);
                    self.wbuf.push((v >> 8) as u8);
                    if self.wbuf.len() >= 512 {
                        let mut s = [0u8; 512];
                        s.copy_from_slice(&self.wbuf[..512]);
                        self.wbuf.drain(..512);
                        self.overlay.insert(self.wlba, s);
                        if self.persist {
                            if let Some(f) = &self.file {
                                let _ = f.write_at(&s, self.wlba * 512);
                            }
                        }
                        self.wlba += 1;
                        self.wleft -= 1;
                        self.wdone += 512;
                        if self.wleft == 0 || self.wdone % self.block as u64 == 0 {
                            self.bsy = self.bsy_reads;
                            let bu = self.block_us;
                            self.wait(now, bu);
                        }
                    }
                }
            }
            _ => {
                let s = slot(a);
                if s < 4 {
                    self.hob[s] = self.r[s];
                }
                self.r[s] = v & 0xff;
            }
        }
    }

    fn ext(c: u32) -> Option<u32> {
        match c {
            0x24 => Some(0x20),
            0x29 => Some(0xc4),
            0x25 => Some(0xc8),
            0x34 => Some(0x30),
            0x39 => Some(0xc5),
            0x35 => Some(0xca),
            _ => None,
        }
    }

    fn cmd(&mut self, mut c: u32, now: u64) {
        let opcode = c;
        let (n, lba): (u64, u64);
        if let Some(c2) = Self::ext(c) {
            let n0 = ((self.hob[0] as u64) << 8) | self.r[0] as u64;
            n = if n0 == 0 { 65536 } else { n0 };
            lba = ((self.hob[3] as u64) << 40)
                | ((self.hob[2] as u64) << 32)
                | ((self.hob[1] as u64) << 24)
                | ((self.r[3] as u64) << 16)
                | ((self.r[2] as u64) << 8)
                | self.r[1] as u64;
            c = c2;
        } else {
            n = if self.r[0] == 0 { 256 } else { self.r[0] as u64 };
            lba = (((self.r[4] & 0xf) as u64) << 24) | ((self.r[3] as u64) << 16) | ((self.r[2] as u64) << 8) | self.r[1] as u64;
        }
        self.n_cmds += 1;
        self.fifo.clear();
        self.fpos = 0;
        self.wleft = 0;
        self.wbuf.clear();
        self.wdone = 0;
        self.failed = false;
        self.err = 0;
        self.read_err = None;
        self.bsy = self.bsy_reads;
        let cu = self.cmd_us;
        self.wait(now, cu);
        self.block = if c == 0xc4 || c == 0xc5 { 512 * if self.multiple != 0 { self.multiple as usize } else { 1 } } else { 512 };
        let transfer = matches!(c, 0x20 | 0x21 | 0xc4 | 0xc8 | 0x30 | 0x31 | 0xc5 | 0xca);
        let mut out = "ok";
        if !transfer && c != 0xec && c != 0xc6 {
            if matches!(c, 0xe0 | 0xe2 | 0xe6) { self.asleep = true; } else if matches!(c, 0xe1 | 0xe3) { self.asleep = false; }
            if self.power_log.len() < 100_000 { self.power_log.push((now, opcode, self.asleep as u32, self.feat, self.r[0])); }
        }
        if self.power_gate && self.asleep && (transfer || c == 0xec) {
            self.ready_at = u64::MAX;                       // the drive is down: the command never completes
            self.stalled += 1;
            self.log.push((opcode, self.feat, n as u32, lba, "STALL (drive powered down)"));
            return;
        }
        if c == 0x90 {
            self.err = 0x01;
        } else if c == 0xec {
            self.fifo = self.identify();
        } else if c == 0xef {
            // features are accepted
        } else if c == 0xc6 {
            let m = self.r[0];
            if matches!(m, 0 | 1 | 2 | 4 | 8 | 16 | 32 | 64 | 128) {   // 1.0 sets its cluster size (32 sectors for 16 KB clusters), past the 16 IDENTIFY offers
                self.multiple = m;
            } else {
                self.stop(ABRT);
                out = "ERR 0x04 (count)";
            }
        } else if (c == 0xc4 || c == 0xc5) && self.multiple == 0 {
            self.stop(ABRT);
            out = "ERR 0x04 (SET MULTIPLE not done)";
        } else if transfer && lba + n > self.total {
            self.stop(IDNF);
            out = "ERR 0x10 (past the end)";
        } else if transfer && opcode == c && lba + n > 0x1000_0000 {
            self.stop(IDNF);
            out = "ERR 0x10 (a 28-bit command running past sector 2^28)";
        } else if matches!(c, 0x20 | 0x21 | 0xc4 | 0xc8) {
            self.fifo = self.read_lba(lba, n);
            self.n_reads += 1;
            self.sectors_read += n;
        } else if matches!(c, 0x30 | 0x31 | 0xc5 | 0xca) {
            self.wlba = lba;
            self.wleft = n;
            self.n_writes += 1;
            self.sectors_written += n;
        } else if NODATA.contains(&c) {
        } else {
            self.stop(ABRT);
            out = "ERR 0x04 (not implemented)";
        }
        let _ = UNC;
        let count = if opcode != c { self.r[0] } else { n as u32 };
        let count = if opcode != c { count } else { count };
        self.log.push((opcode, self.feat, count, lba, out));
    }
}
