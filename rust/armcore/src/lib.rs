//! An ARM7TDMI (ARMv4T) interpreter.
//!
//! The processor of emupod's PP5002 (two of them). Built for speed on a Raspberry Pi 4:
//!
//! * each instruction is decoded once into a handler through a table indexed by its bits (4096 entries for ARM,
//!   1024 for Thumb), the handlers are generic over the [`Bus`] so memory accesses inline, and the condition
//!   check is one table lookup;
//! * [`Core::run`] executes exactly `n` instructions (or stops early when the bus asks), so an emulator that
//!   counts instructions for its clock gets the same count every time and the two cores of an SoC can take
//!   turns at no cost;
//! * there is no allocation and no `unsafe`.
//!
//! The pipeline is not modelled, only what a program can see of it: reading PC gives the instruction's address
//! plus 8 (ARM) or 4 (Thumb), and plus 12 when a register-specified shift or a store reads it.
//!
//! Interworking follows ARMv4T: only `BX` (and a return from an exception that restores CPSR) changes between
//! ARM and Thumb; a load or data-processing write to PC does not.

mod arm;
mod thumb;
#[cfg(feature = "stats")]
pub mod stats;

pub const MODE_USR: u32 = 0x10;
pub const MODE_FIQ: u32 = 0x11;
pub const MODE_IRQ: u32 = 0x12;
pub const MODE_SVC: u32 = 0x13;
pub const MODE_ABT: u32 = 0x17;
pub const MODE_UND: u32 = 0x1b;
pub const MODE_SYS: u32 = 0x1f;

pub const FLAG_N: u32 = 1 << 31;
pub const FLAG_Z: u32 = 1 << 30;
pub const FLAG_C: u32 = 1 << 29;
pub const FLAG_V: u32 = 1 << 28;
pub const BIT_I: u32 = 1 << 7;
pub const BIT_F: u32 = 1 << 6;
pub const BIT_T: u32 = 1 << 5;

/// Why the processor took an exception (reported to [`Bus::exception`] as it happens).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exception {
    Undefined,
    Swi,
    Irq,
    Fiq,
}

/// Memory and devices, as the processor sees them. Addresses are aligned to the access size (the core rotates
/// or masks as the ARM7TDMI does); reads and writes of devices are the bus's to model.
pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    fn read16(&mut self, addr: u32) -> u16;
    fn read32(&mut self, addr: u32) -> u32;
    fn write8(&mut self, addr: u32, v: u8);
    fn write16(&mut self, addr: u32, v: u16);
    fn write32(&mut self, addr: u32, v: u32);
    /// A halfword or word access at a misaligned address (`size` 2 or 4): the core goes on as an ARM7 does (the aligned access, rotated),
    /// and a bus may record that the processor in question traps.
    #[inline(always)]
    fn misaligned(&mut self, _addr: u32, _size: u32) {}
    #[inline(always)]
    fn read32_a(&mut self, addr: u32) -> u32 {
        if addr & 3 != 0 {
            self.misaligned(addr, 4);
        }
        self.read32(addr & !3)
    }
    #[inline(always)]
    fn read16_a(&mut self, addr: u32) -> u16 {
        if addr & 1 != 0 {
            self.misaligned(addr, 2);
        }
        self.read16(addr & !1)
    }
    #[inline(always)]
    fn write32_a(&mut self, addr: u32, v: u32) {
        if addr & 3 != 0 {
            self.misaligned(addr, 4);
        }
        self.write32(addr & !3, v)
    }
    #[inline(always)]
    fn write16_a(&mut self, addr: u32, v: u16) {
        if addr & 1 != 0 {
            self.misaligned(addr, 2);
        }
        self.write16(addr & !1, v)
    }
    /// Instruction fetches (default: the same as reads).
    #[inline(always)]
    fn fetch16(&mut self, addr: u32) -> u16 {
        self.read16(addr)
    }
    #[inline(always)]
    fn fetch32(&mut self, addr: u32) -> u32 {
        self.read32(addr)
    }
    /// Checked after every instruction: true ends [`Core::run`] early (a device that halts the processor until
    /// an interrupt, or ends a time slice).
    #[inline(always)]
    fn stop(&self) -> bool {
        false
    }
    /// Whether `Core::run` may skip spin loops (see `Core::run`). Off by default.
    #[inline(always)]
    fn skip_enabled(&self) -> bool {
        false
    }
    /// A counter that changes whenever anything a core could observe changes: a write that alters memory, any
    /// device access. Equal values at two moments mean nothing but the core's own registers moved between them.
    #[inline(always)]
    fn epoch(&self) -> u64 {
        0
    }
    /// `Core::run` skipped `n` instructions of a periodic stretch (the bus keeps its own instruction counts).
    #[inline(always)]
    fn advance(&mut self, _n: u64) {}
    /// The cache key of the code word at `pc`: a number that is the same for every address that names the same
    /// memory (aliases), or `u32::MAX` if code there is not cached (devices, say).
    #[inline(always)]
    fn code_key(&self, _pc: u32) -> u32 {
        u32::MAX
    }
    /// Whether the instruction at `pc` may be decoded once and kept (false: fetch it every time, so the bus sees
    /// every execution of it).
    #[inline(always)]
    fn cacheable(&self, _pc: u32) -> bool {
        true
    }

    /// Called just before an instruction runs that is not cacheable (`cacheable` false), which is how a bus puts a
    /// hook on one address: the registers may be read and changed, as by a code hook in another emulator.
    #[inline(always)]
    fn hook(&mut self, _pc: u32, _cpu: &mut Cpu) {}
    /// The core has decoded instructions from cache page `page` (`code_key >> PAGE_SHIFT`): tell it when any of
    /// them change.
    #[inline(always)]
    fn mark_code(&mut self, _page: u32) {}
    /// True if a marked page has been written since the last `take_dirty`.
    #[inline(always)]
    fn code_dirty(&self) -> bool {
        false
    }
    /// The marked pages written since the last call (they are unmarked).
    fn take_dirty(&mut self, _out: &mut Vec<u32>) {}
    /// The core is about to execute the `n`th instruction of this run (1 for the first), skipped ones counted.
    #[inline(always)]
    fn at(&mut self, _n: u64) {}
    /// An exception is being taken; `pc` is the address of the instruction that caused it (or, for an
    /// interrupt, of the one that would have run).
    #[inline(always)]
    fn exception(&mut self, _kind: Exception, _pc: u32) {}
}

/// The processor's state.
#[derive(Clone)]
pub struct Cpu {
    /// The registers of the current mode. While an instruction executes `r[15]` is the pipeline-adjusted PC.
    pub r: [u32; 16],
    /// The address of the next instruction to execute.
    pub pc: u32,
    pub cpsr: u32,
    /// The saved PSR of each mode (index as [`mode_index`]; entry 0, User/System, is unused).
    pub spsr: [u32; 6],
    /// r8..r14 of each mode that is not the current one (index as [`mode_index`]).
    bank: [[u32; 7]; 6],
    /// Where the next instruction is; a handler that branches overwrites it.
    next: u32,
}

/// 0 User/System, 1 FIQ, 2 IRQ, 3 Supervisor, 4 Abort, 5 Undefined.
#[inline(always)]
pub fn mode_index(mode: u32) -> usize {
    match mode & 0x1f {
        MODE_FIQ => 1,
        MODE_IRQ => 2,
        MODE_SVC => 3,
        MODE_ABT => 4,
        MODE_UND => 5,
        _ => 0,
    }
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu { r: [0; 16], pc: 0, cpsr: MODE_SVC | BIT_I | BIT_F, spsr: [0; 6], bank: [[0; 7]; 6], next: 0 }
    }

    #[inline(always)]
    pub fn thumb(&self) -> bool {
        self.cpsr & BIT_T != 0
    }
    #[inline(always)]
    pub fn mode(&self) -> u32 {
        self.cpsr & 0x1f
    }
    #[inline(always)]
    pub(crate) fn cflag(&self) -> u32 {
        (self.cpsr >> 29) & 1
    }

    /// Branch: the next instruction is at `t` (aligned for the current state).
    #[inline(always)]
    pub(crate) fn jump(&mut self, t: u32) {
        self.next = t & if self.cpsr & BIT_T != 0 { !1 } else { !3 };
    }

    #[inline(always)]
    pub(crate) fn set_nz(&mut self, v: u32) {
        self.cpsr = (self.cpsr & 0x3fff_ffff) | (v & FLAG_N) | (((v == 0) as u32) << 30);
    }
    #[inline(always)]
    pub(crate) fn set_c(&mut self, c: u32) {
        self.cpsr = (self.cpsr & !FLAG_C) | (c << 29);
    }
    #[inline(always)]
    pub(crate) fn set_nzcv(&mut self, v: u32, c: u32, o: u32) {
        self.cpsr = (self.cpsr & 0x0fff_ffff) | (v & FLAG_N) | (((v == 0) as u32) << 30) | (c << 29) | (o << 28);
    }

    /// Change mode: bank the registers of the old mode, bring in those of the new one.
    fn switch_mode(&mut self, new: u32) {
        let (o, n) = (mode_index(self.cpsr), mode_index(new));
        if o == n {
            return;
        }
        if o == 1 || n == 1 {
            // r8..r12 are private to FIQ; every other mode shares User's, kept in bank[0] while FIQ runs
            let (from, to) = if o == 1 { (1, 0) } else { (0, 1) };
            for i in 0..5 {
                self.bank[from][i] = self.r[8 + i];
                self.r[8 + i] = self.bank[to][i];
            }
        }
        self.bank[o][5] = self.r[13];
        self.bank[o][6] = self.r[14];
        self.r[13] = self.bank[n][5];
        self.r[14] = self.bank[n][6];
    }

    /// Write CPSR whole (an exception return, MSR): changes mode if the mode bits do.
    pub fn set_cpsr(&mut self, v: u32) {
        self.switch_mode(v);
        self.cpsr = v;
    }

    /// The register `i` of the User bank (for LDM/STM with the S bit, from a privileged mode).
    pub(crate) fn user_reg(&self, i: usize) -> u32 {
        let m = mode_index(self.cpsr);
        match i {
            8..=12 if m == 1 => self.bank[0][i - 8],
            13 | 14 if m != 0 => self.bank[0][i - 8],
            _ => self.r[i],
        }
    }
    pub(crate) fn set_user_reg(&mut self, i: usize, v: u32) {
        let m = mode_index(self.cpsr);
        match i {
            8..=12 if m == 1 => self.bank[0][i - 8] = v,
            13 | 14 if m != 0 => self.bank[0][i - 8] = v,
            _ => self.r[i] = v,
        }
    }

    /// The value register `i` has in the User bank (the register itself unless the mode is not User/System).
    pub fn user_view(&self, i: usize) -> u32 {
        self.user_reg(i)
    }

    /// The SPSR of the current mode (the CPSR in User/System, which have none).
    pub fn current_spsr(&self) -> u32 {
        match mode_index(self.cpsr) {
            0 => self.cpsr,
            m => self.spsr[m],
        }
    }

    /// Take an exception: bank, save CPSR, LR, mask, ARM state, vector.
    pub(crate) fn enter(&mut self, mode: u32, vector: u32, lr: u32, mask_f: bool) {
        let old = self.cpsr;
        self.switch_mode(mode);
        self.spsr[mode_index(mode)] = old;
        self.r[14] = lr;
        self.cpsr = (old & !(BIT_T | 0x1f)) | mode | BIT_I | if mask_f { BIT_F } else { 0 };
        self.next = vector;
    }

    /// Deliver an IRQ if they are not masked (returns whether it was taken). Between instructions only.
    pub fn irq<B: Bus>(&mut self, bus: &mut B) -> bool {
        if self.cpsr & BIT_I != 0 {
            return false;
        }
        bus.exception(Exception::Irq, self.pc);
        self.enter(MODE_IRQ, 0x18, self.pc.wrapping_add(4), false);
        self.pc = self.next;
        true
    }

    /// Deliver an FIQ if they are not masked.
    pub fn fiq<B: Bus>(&mut self, bus: &mut B) -> bool {
        if self.cpsr & BIT_F != 0 {
            return false;
        }
        bus.exception(Exception::Fiq, self.pc);
        self.enter(MODE_FIQ, 0x1c, self.pc.wrapping_add(4), true);
        self.pc = self.next;
        true
    }

    /// Reset: Supervisor mode, ARM state, interrupts masked, PC at 0.
    pub fn reset(&mut self) {
        *self = Cpu::new();
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu::new()
    }
}

pub(crate) type ArmFn<B> = fn(&mut Cpu, &mut B, u32);

/// A cached ARM instruction.
pub(crate) struct Op<B: Bus> {
    f: ArmFn<B>,
    instr: u32,
    cond: u8,
    /// 0 not decoded yet, 1 decoded, 2 not cacheable (fetched and decoded every time)
    state: u8,
}

impl<B: Bus> Clone for Op<B> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}
impl<B: Bus> Copy for Op<B> {}

/// log2 of the words in a cache page
pub const PAGE_SHIFT: u32 = 8;
const PAGE_WORDS: usize = 1 << PAGE_SHIFT;

fn nop_op<B: Bus>(_: &mut Cpu, _: &mut B, _: u32) {}

fn blank_op<B: Bus>(state: u8) -> Op<B> {
    Op { f: nop_op::<B>, instr: 0, cond: 14, state }
}
pub(crate) type ThumbFn<B> = fn(&mut Cpu, &mut B, u32);

/// For each condition code, a bitmask over the flags NZCV (bit n set: the condition passes when `cpsr >> 28`
/// is n).
const fn cond_masks() -> [u16; 16] {
    let mut t = [0u16; 16];
    let mut c = 0;
    while c < 16 {
        let mut f = 0;
        while f < 16 {
            let (n, z, cf, v) = (f & 8 != 0, f & 4 != 0, f & 2 != 0, f & 1 != 0);
            let pass = match c {
                0 => z,
                1 => !z,
                2 => cf,
                3 => !cf,
                4 => n,
                5 => !n,
                6 => v,
                7 => !v,
                8 => cf && !z,
                9 => !cf || z,
                10 => n == v,
                11 => n != v,
                12 => !z && n == v,
                13 => z || n != v,
                14 => true,
                _ => false, // NV
            };
            if pass {
                t[c] |= 1 << f;
            }
            f += 1;
        }
        c += 1;
    }
    t
}
pub(crate) static COND: [u16; 16] = cond_masks();

/// A processor with its decode tables.
pub struct Core<B: Bus> {
    pub cpu: Cpu,
    arm: Box<[ArmFn<B>; 4096]>,
    thumb: Box<[ThumbFn<B>; 1024]>,
    /// the samples of the last runs (spin-loop skipping keeps them across runs) and the instructions run so far
    ring: Samples,
    total: u64,
    /// decoded instructions: one flat arena of pages of `PAGE_WORDS`; page 0 is the all-slow page (code that is not
    /// cached), `page_of[key >> PAGE_SHIFT]` the arena page of a cache page (see `Bus::code_key`)
    ops: Vec<Op<B>>,
    page_of: Vec<u32>,
    /// where the pc's page starts in `ops`, and the virtual page it is
    cur_base: usize,
    cur_vpage: u32,
    /// the cache page the pc is in, and whether the bus has been told (`mark_code`) that we hold code from each arena page
    cur_page: u32,
    marked: Vec<bool>,
    dirty: Vec<u32>,
    /// pages dropped because their code was written, and instructions decoded (for measurements)
    pub flushes: u64,
    pub decodes: u64,
}

impl<B: Bus> Core<B> {
    pub fn new() -> Core<B> {
        Core { cpu: Cpu::new(), arm: arm::build_table::<B>(), thumb: thumb::build_table::<B>(), ring: Samples::new(), total: 0, ops: vec![blank_op(2); PAGE_WORDS], page_of: Vec::new(), cur_base: 0, cur_vpage: u32::MAX, cur_page: u32::MAX, marked: vec![false], dirty: Vec::new(), flushes: 0, decodes: 0 }
    }

    /// Execute `n` instructions (fewer if the bus asks to stop); returns how many ran.
    ///
    /// Instructions in cacheable memory are decoded once and kept (`Bus::code_key`); a write to a page that holds
    /// decoded code drops the page (`Bus::code_dirty`), so self-modifying code is exact.
    ///
    /// If the bus enables it, spin loops are skipped, exactly: every `SAMPLE` instructions the registers are
    /// compared with those at the last few samples, and if they are the same while the bus's `epoch` has not
    /// moved, execution is periodic (the same state leads to the same state, and no memory or device changed),
    /// so whole periods can be left out. The count `n` and the returned count include them.
    pub fn run(&mut self, bus: &mut B, n: u64) -> u64 {
        if bus.skip_enabled() {
            return self.run_skipping(bus, n);
        }
        let mut done = 0u64;
        // four copies of the dispatch, so the host's branch predictor sees four indirect calls, not one
        while done + 4 <= n {
            self.step(bus, done + 1);
            if bus.stop() {
                return done + 1;
            }
            self.step(bus, done + 2);
            if bus.stop() {
                return done + 2;
            }
            self.step(bus, done + 3);
            if bus.stop() {
                return done + 3;
            }
            self.step(bus, done + 4);
            if bus.stop() {
                return done + 4;
            }
            done += 4;
        }
        while done < n {
            self.step(bus, done + 1);
            done += 1;
            if bus.stop() {
                break;
            }
        }
        done
    }

    /// Drop the decoded instructions of pages written since the last run. `step` does this after an instruction, so the first
    /// instruction run after memory was changed from outside (a host write, a snapshot restore) would use its old decode: call this first.
    pub fn sync(&mut self, bus: &mut B) {
        if bus.code_dirty() {
            self.flush(bus);
        }
    }

    fn run_skipping(&mut self, bus: &mut B, n: u64) -> u64 {
        let base = self.total;
        let mut done = 0u64;
        let mut next_sample = SAMPLE;
        // the state at the start of the run against the last runs': a core spinning from one slice to the next
        // is stuck in the same state at each boundary, and the whole slice can go
        if let Some(period) = self.ring.check(&self.cpu, bus.epoch(), base) {
            let skip = (n / period) * period;
            if skip > 0 {
                bus.advance(skip);
                done = skip;
                next_sample = done + SAMPLE;
                self.ring.clear();
            }
        }
        while done < n {
            // up to the next sample point (or the end): four copies of the dispatch
            let stop_at = n.min(next_sample.max(done + 1));
            while done + 4 <= stop_at {
                self.step(bus, done + 1);
                if bus.stop() {
                    self.total = base + done + 1;
                    return done + 1;
                }
                self.step(bus, done + 2);
                if bus.stop() {
                    self.total = base + done + 2;
                    return done + 2;
                }
                self.step(bus, done + 3);
                if bus.stop() {
                    self.total = base + done + 3;
                    return done + 3;
                }
                self.step(bus, done + 4);
                if bus.stop() {
                    self.total = base + done + 4;
                    return done + 4;
                }
                done += 4;
            }
            if done >= n {
                break;
            }
            if done < stop_at {
                self.step(bus, done + 1);
                done += 1;
                if bus.stop() {
                    break;
                }
            }
            if done >= next_sample {
                next_sample = done + SAMPLE;
                if let Some(period) = self.ring.check(&self.cpu, bus.epoch(), base + done) {
                    let skip = ((n - done) / period) * period;
                    if skip > 0 {
                        bus.advance(skip);
                        done += skip;
                        next_sample = done + SAMPLE;
                        self.ring.clear();
                    }
                }
            }
        }
        self.total = base + done;
        done
    }

    /// One instruction; `n` is its number in this run.
    #[inline(always)]
    fn step(&mut self, bus: &mut B, n: u64) {
        let pc = self.cpu.pc;
        if self.cpu.cpsr & BIT_T != 0 {
            bus.at(n);
            let i = bus.fetch16(pc) as u32;
            self.cpu.r[15] = pc.wrapping_add(4);
            self.cpu.next = pc.wrapping_add(2);
            self.thumb[(i >> 6) as usize](&mut self.cpu, bus, i);
            #[cfg(feature = "stats")]
            stats::record(pc, true, i >> 6, true, self.cpu.next != pc.wrapping_add(2));
            self.cpu.pc = self.cpu.next;
            return;
        }
        if pc >> (PAGE_SHIFT + 2) != self.cur_vpage {
            self.switch_page(bus, pc);
        }
        let op = self.ops[self.cur_base + (((pc >> 2) as usize) & (PAGE_WORDS - 1))];
        if op.state != 1 {
            self.step_cold(bus, pc, n);
            return;
        }
        bus.at(n);
        self.cpu.r[15] = pc.wrapping_add(8);
        self.cpu.next = pc.wrapping_add(4);
        let ran = op.cond == 14 || (COND[op.cond as usize] >> (self.cpu.cpsr >> 28)) & 1 != 0;
        if ran {
            (op.f)(&mut self.cpu, bus, op.instr);
        }
        #[cfg(feature = "stats")]
        {
            let i = op.instr;
            stats::record(pc, false, ((i >> 16) & 0xff0) | ((i >> 4) & 0xf), ran, self.cpu.next != pc.wrapping_add(4));
            stats::cond(i >> 28, ran);
        }
        self.cpu.pc = self.cpu.next;
        if bus.code_dirty() {
            self.flush(bus);
        }
    }

    /// An instruction that is not decoded yet, or whose code is not cached: fetch it, decode it, (keep it,) run it.
    #[inline(never)]
    fn step_cold(&mut self, bus: &mut B, pc: u32, n: u64) {
        bus.at(n);
        let slot = self.cur_base + (((pc >> 2) as usize) & (PAGE_WORDS - 1));
        let cacheable = bus.cacheable(pc);
        if !cacheable {
            bus.hook(pc, &mut self.cpu);
        }
        let keep = self.ops[slot].state == 0 && cacheable;
        let i = bus.fetch32(pc);
        let idx = (((i >> 16) & 0xff0) | ((i >> 4) & 0xf)) as usize;
        let f = self.arm[idx];
        if keep {
            let ap = self.cur_base / PAGE_WORDS;
            if !self.marked[ap] {
                self.marked[ap] = true;
                bus.mark_code(self.cur_page);
            }
            self.decodes += 1;
            self.ops[slot] = Op { f, instr: i, cond: (i >> 28) as u8, state: 1 };
        }
        self.cpu.r[15] = pc.wrapping_add(8);
        self.cpu.next = pc.wrapping_add(4);
        let ran = i >> 28 == 14 || (COND[(i >> 28) as usize] >> (self.cpu.cpsr >> 28)) & 1 != 0;
        if ran {
            f(&mut self.cpu, bus, i);
        }
        #[cfg(feature = "stats")]
        {
            stats::record(pc, false, idx as u32, ran, self.cpu.next != pc.wrapping_add(4));
            stats::cond(i >> 28, ran);
        }
        self.cpu.pc = self.cpu.next;
        if bus.code_dirty() {
            self.flush(bus);
        }
    }

    /// The pc left the cache page: find the arena page of the one it is in now (making it if it is new).
    #[inline(never)]
    fn switch_page(&mut self, bus: &mut B, pc: u32) {
        self.cur_vpage = pc >> (PAGE_SHIFT + 2);
        let key = bus.code_key(pc);
        if key == u32::MAX {
            self.cur_base = 0;
            self.cur_page = u32::MAX;
            return;
        }
        let page = (key >> PAGE_SHIFT) as usize;
        if self.page_of.len() <= page {
            self.page_of.resize(page + 1, u32::MAX);
        }
        let mut idx = self.page_of[page];
        if idx == u32::MAX {
            idx = (self.ops.len() / PAGE_WORDS) as u32;
            self.ops.resize(self.ops.len() + PAGE_WORDS, blank_op(0));
            self.page_of[page] = idx;
            self.marked.push(false);
        }
        self.cur_page = page as u32;
        self.cur_base = idx as usize * PAGE_WORDS;
    }

    /// Pages of decoded code were written: forget what was decoded in them.
    #[inline(never)]
    fn flush(&mut self, bus: &mut B) {
        let mut buf = std::mem::take(&mut self.dirty);
        buf.clear();
        bus.take_dirty(&mut buf);
        for &p in &buf {
            self.flushes += 1;
            if let Some(&idx) = self.page_of.get(p as usize) {
                if idx != u32::MAX {
                    let base = idx as usize * PAGE_WORDS;
                    for o in &mut self.ops[base..base + PAGE_WORDS] {
                        o.state = 0;
                    }
                    self.marked[idx as usize] = false;
                }
            }
        }
        self.dirty = buf;
    }
}

const SAMPLE: u64 = 64;
const RING: usize = 16;

/// The registers at the last few samples, to see a state come round again.
struct Samples {
    e: [Entry; RING],
    n: usize,
}
#[derive(Clone, Copy)]
struct Entry {
    at: u64,
    epoch: u64,
    hash: u64,
    st: [u32; 18],
}
impl Samples {
    fn new() -> Samples {
        Samples { e: [Entry { at: 0, epoch: u64::MAX, hash: 0, st: [0; 18] }; RING], n: 0 }
    }
    fn clear(&mut self) {
        self.n = 0;
        for e in self.e.iter_mut() {
            e.epoch = u64::MAX;
        }
    }
    /// Record the state now; return the period (in instructions) if it equals an earlier sample's and nothing
    /// outside the core changed in between.
    #[inline]
    fn check(&mut self, cpu: &Cpu, epoch: u64, at: u64) -> Option<u64> {
        let mut st = [0u32; 18];
        st[..16].copy_from_slice(&cpu.r);
        st[16] = cpu.pc;
        st[17] = cpu.cpsr;
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for w in st {
            h = (h ^ w as u64).wrapping_mul(0x0100_0000_01b3);
        }
        let mut found = None;
        for e in self.e.iter() {
            if e.at < at && e.epoch == epoch && e.hash == h && e.st == st {
                found = Some(at - e.at);
                break;
            }
        }
        if self.n == 0 || self.e[(self.n - 1) % RING].at != at {
            self.e[self.n % RING] = Entry { at, epoch, hash: h, st };
            self.n += 1;
        }
        found
    }
}

impl<B: Bus> Default for Core<B> {
    fn default() -> Self {
        Core::new()
    }
}
