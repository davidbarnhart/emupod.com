//! The pp5002 machine for Python: `emupod_rs.Machine`. The Python side (emupod/emu_rs.py) keeps the public
//! `emupod.emu` interface; this is only the machine's own controls and readings.
use pp5002::{Config, Machine as Pp};
use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

#[pyclass]
struct Machine {
    m: Pp,
    screen: pp5002::lcd::Screen,
}

#[pymethods]
impl Machine {
    /// `fpf`: frames per audio interrupt (0: the real rate, "auto"). `compat` turns on the old Python core's habits.
    #[new]
    #[pyo3(signature = (osos, disk, persist=false, ipus=13, cop_ipus=13, quantum=1000, interleave=20, interleave_playing=0,
        interleave_settle=1_000_000, fpf=8, keys_up=true, plugged=false, idle_sleep=true, cop=true, ata_bsy=0, ata_cmd_us=0,
        ata_block_us=0, ata_lba48=false, start_volume=None, slice_clock=false, lazy_map=false, skip_loops=true, trace_io=false, ack_on_read=true, irq_mask=false, keep_cop_reset_state=false, iram_kb=0))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        osos: &str,
        disk: &str,
        persist: bool,
        ipus: u64,
        cop_ipus: u64,
        quantum: u64,
        interleave: u64,
        interleave_playing: u64,
        interleave_settle: u64,
        fpf: u64,
        keys_up: bool,
        plugged: bool,
        idle_sleep: bool,
        cop: bool,
        ata_bsy: u32,
        ata_cmd_us: u64,
        ata_block_us: u64,
        ata_lba48: bool,
        start_volume: Option<u32>,
        slice_clock: bool,
        lazy_map: bool,
        skip_loops: bool,
        trace_io: bool,
        ack_on_read: bool,
        irq_mask: bool,
        keep_cop_reset_state: bool,
        iram_kb: u32,
    ) -> PyResult<Machine> {
        let image = std::fs::read(osos).map_err(|e| PyIOError::new_err(format!("{osos}: {e}")))?;
        let cfg = Config {
            ipus,
            cop_ipus,
            quantum,
            interleave,
            interleave_playing,
            interleave_settle,
            audio_fpf: if fpf == 0 { None } else { Some(fpf) },
            keys_up,
            plugged,
            idle_sleep,
            cop,
            ata_bsy,
            ata_cmd_us,
            ata_block_us,
            ata_lba48,
            persist,
            slice_clock,
            lazy_map,
            skip_loops,
            start_volume,
            trace_io,
            ack_on_read,
            irq_mask,
            irq_mask_raw: std::env::var("EMUPOD_RS_IRQ_MASK_RAW").is_ok(),
            study_nocache: std::env::var("EMUPOD_RS_NOCACHE").is_ok(),
            keep_cop_reset_state,
            iram_kb,
        };
        let m = Pp::new(&image, disk, cfg).map_err(|e| PyIOError::new_err(format!("{disk}: {e}")))?;
        Ok(Machine { m, screen: pp5002::lcd::Screen::default() })
    }

    /// A bare core for the call harness: no firmware, no card, no coprocessor, nothing mapped, already in literal call mode with
    /// misaligned accesses faulting. All its memory is what the host maps (`call_map_page`). Cheap: no boot.
    #[staticmethod]
    fn bare() -> Machine {
        struct EmptyDisk;
        impl pp5002::ata::Disk for EmptyDisk {
            fn len(&self) -> u64 {
                512 * 256
            }
            fn read_at(&self, _buf: &mut [u8], _off: u64) {}
            fn write_at(&self, _buf: &[u8], _off: u64) {}
        }
        let cfg = Config {
            ipus: 13, cop_ipus: 13, quantum: 1000, interleave: 20, interleave_playing: 0, interleave_settle: 1_000_000, audio_fpf: Some(8),
            keys_up: true, plugged: false, idle_sleep: true, cop: false, ata_bsy: 0, ata_cmd_us: 0, ata_block_us: 0, ata_lba48: false, persist: false,
            slice_clock: false, lazy_map: false, skip_loops: false, start_volume: None, trace_io: false, ack_on_read: true, irq_mask: false,
            irq_mask_raw: false, study_nocache: false, keep_cop_reset_state: false, iram_kb: 0,
        };
        let mut m = Pp::with_disk(&[], Box::new(EmptyDisk), cfg);
        m.call_mode(true, true, true);
        Machine { m, screen: pp5002::lcd::Screen::default() }
    }

    fn boot(&mut self, py: Python<'_>) -> u32 {
        py.detach(|| self.m.boot())
    }
    /// One timer tick of iPod time, the stock driver step.
    fn tick(&mut self, py: Python<'_>) {
        py.detach(|| {
            self.m.settle(1, 1 << 30);
        })
    }
    fn settle(&mut self, py: Python<'_>, max_iters: u32, quiet: u32) -> u32 {
        py.detach(|| self.m.settle(max_iters, quiet))
    }
    fn now_us(&self) -> u64 {
        self.m.now()
    }
    fn ticks(&self) -> u64 {
        self.m.s.ticks
    }
    fn halts(&self) -> u64 {
        self.m.s.sleeps
    }
    fn tick_period_us(&self) -> u64 {
        self.m.s.t1.period
    }
    fn cop_asleep(&self) -> bool {
        self.m.s.cop_asleep
    }

    // ---- pins
    fn set_pin(&mut self, port: usize, mask: u32, level: bool) {
        self.m.set_pin(port, mask, level)
    }
    fn pin(&self, port: usize, mask: u32) -> bool {
        self.m.pin(port, mask)
    }
    /// Latch an edge on the pin and deliver it to the firmware (runs a little iPod time).
    fn edge(&mut self, py: Python<'_>, port: usize, mask: u32) -> bool {
        py.detach(|| self.m.edge(port, mask))
    }

    // ---- the LCD
    fn lcd_count(&self) -> u32 {
        self.m.s.lcd_count
    }
    /// The controller writes since the last call, as (address, value).
    fn lcd_take(&mut self) -> Vec<(u32, u32)> {
        std::mem::take(&mut self.m.s.lcd_log)
    }
    /// Decode the controller writes since the last call into the screen (160 x 128 levels 0-3); whether any came.
    fn lcd_update(&mut self) -> bool {
        let log = std::mem::take(&mut self.m.s.lcd_log);
        self.screen.update(&log)
    }
    fn lcd_px<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.screen.px)
    }
    fn lcd_pending(&self) -> usize {
        self.m.s.lcd_log.len()
    }

    // ---- sound
    fn audio_on(&self) -> bool {
        self.m.s.audio.on
    }
    fn audio_start_times(&self) -> Vec<u64> {
        self.m.s.audio.started.clone()
    }
    /// The sound since the last call: s16le stereo at 44,100 Hz.
    fn audio_take<'py>(&mut self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let v = self.m.s.audio.take();
        let mut b = Vec::with_capacity(v.len() * 2);
        for s in v {
            b.extend_from_slice(&s.to_le_bytes());
        }
        PyBytes::new(py, &b)
    }
    /// The sound since the last call, each channel times its gain (s16le stereo bytes): the volume applied where it is cheap.
    fn audio_take_scaled<'py>(&mut self, py: Python<'py>, gl: f64, gr: f64) -> Bound<'py, PyBytes> {
        let v = self.m.s.audio.take();
        let mut b = Vec::with_capacity(v.len() * 2);
        for (i, s) in v.into_iter().enumerate() {
            let g = if i & 1 == 0 { gl } else { gr };
            let x = if g == 1.0 { s } else { (s as f64 * g) as i16 };
            b.extend_from_slice(&x.to_le_bytes());
        }
        PyBytes::new(py, &b)
    }
    /// The headphone volume codes the firmware set (left, right).
    fn volume(&self) -> (u32, u32) {
        (self.m.s.hp[0], self.m.s.hp[1])
    }
    fn underruns(&mut self) -> u32 {
        self.m.underruns()
    }
    fn fiqs(&self) -> u64 {
        self.m.s.audio.fiqs
    }

    // ---- clicks
    /// whether the player's backlight is on (1.5-family firmware)
    fn backlight(&self) -> bool {
        self.m.backlight()
    }
    fn click_count(&self) -> usize {
        self.m.s.clicks.len()
    }
    fn click_times(&self) -> Vec<u64> {
        self.m.s.clicks.clone()
    }

    // ---- memory and registers
    fn mem_read(&mut self, addr: u32, n: usize) -> PyResult<Vec<u8>> {
        if n > 1 << 26 {
            return Err(PyValueError::new_err("too long"));
        }
        Ok(self.m.read_bytes(addr, n))
    }
    fn mem_write(&mut self, addr: u32, data: Vec<u8>) {
        self.m.write_bytes(addr, &data)
    }
    /// The CPU's r0..r15 and cpsr.
    fn cpu_regs(&self) -> Vec<u32> {
        let c = self.m.cpu_regs();
        let mut v: Vec<u32> = (0..15).map(|i| c.r[i]).collect();
        v.push(c.pc);
        v.push(c.cpsr);
        v
    }
    /// Cumulative counters for statistics: time, instructions run by each core (the skipped spin loops included), the disk's
    /// commands and sectors, screen writes, sound interrupts, timer ticks, idle sleeps, clicks.
    fn stats(&mut self) -> std::collections::HashMap<&'static str, u64> {
        let s = &self.m.s;
        let mut m = std::collections::HashMap::new();
        m.insert("now_us", self.m.now());
        m.insert("cpu_instr", s.cpu_instr);
        m.insert("cop_instr", s.cop_instr);
        m.insert("cpu_skipped", s.skipped[0]);
        m.insert("cop_skipped", s.skipped[1]);
        m.insert("cpu_ipus", s.cfg.ipus);
        m.insert("cop_ipus", s.cfg.cop_ipus);
        m.insert("ata_cmds", s.ata.n_cmds);
        m.insert("ata_reads", s.ata.n_reads);
        m.insert("ata_writes", s.ata.n_writes);
        m.insert("ata_sectors_read", s.ata.sectors_read);
        m.insert("ata_sectors_written", s.ata.sectors_written);
        m.insert("lcd_writes", s.lcd_count as u64);
        m.insert("audio_fiqs", s.audio.fiqs);
        m.insert("audio_late", s.audio.late);
        m.insert("ticks", s.ticks);
        m.insert("halts", s.sleeps);
        m.insert("clicks", s.clicks.len() as u64);
        m.insert("cop_asleep", s.cop_asleep as u64);
        m.insert("irq_pending", s.pending as u64);
        m.insert("sig_sent", s.sig_sent);
        m.insert("sig_taken", s.sig_taken);
        m.insert("cop_wakes", s.cop_wakes);
        m.insert("cop_sleeps", s.cop_sleeps);
        m.insert("irqs", s.irqs);
        if let Some(u) = self.m.fw_underruns() {
            m.insert("fw_underruns", u as u64);
        }
        m
    }
    /// Device accesses so far (needs `trace_io`): [(core "cpu"/"cop", write?, address, count)].
    fn io_counts(&self) -> Vec<(&'static str, bool, u32, u64)> {
        self.m.s.io.iter().map(|(&(c, w, a), &n)| (if c == 0 { "cpu" } else { "cop" }, w, a, n)).collect()
    }
    fn io_reset(&mut self) {
        self.m.s.io.clear();
        self.m.s.io_last.clear();
    }
    /// The last value each core wrote to each device address (needs `trace_io`): [(core, address, value)].
    fn io_last(&self) -> Vec<(&'static str, u32, u32)> {
        self.m.s.io_last.iter().map(|(&(c, a), &v)| (if c == 0 { "cpu" } else { "cop" }, a, v)).collect()
    }
    /// (for study) record the next n coprocessor instructions (pc, r0), one by one, wherever the run is; read them with cop_log.
    fn cop_trace_arm(&mut self, n: usize) {
        self.m.cop_log.clear();
        self.m.cop_trace_left = n;
    }
    /// (for study) the ATA commands that are not data transfers, with the emulated time in us: (now_us, opcode, 1 if the drive is
    /// powered down after it, features, count), and how many commands stalled on a powered-down drive (EMUPOD_RS_DISK_POWER=1).
    fn ata_power(&self) -> (Vec<(u64, u32, u32, u32, u32)>, u64) {
        (self.m.s.ata.power_log.clone(), self.m.s.ata.stalled)
    }
    /// (study) log every execution of these CPU addresses as (now_us, pc, [r0..r15], peek); peek_reg/peek_off also record the word at r[peek_reg]+peek_off; read with hook_log. Empty list: off.
    #[pyo3(signature = (pcs, peek_reg=None, peek_off=0))]
    fn hook_start(&mut self, pcs: Vec<u32>, peek_reg: Option<usize>, peek_off: u32) {
        self.m.hook_peek = peek_reg.map(|r| (r, peek_off));
        self.m.hook_pcs = pcs;
        self.m.hook_log.clear();
    }
    fn hook_log(&self) -> Vec<(u64, u32, Vec<u32>, u32)> {
        self.m.hook_log.iter().map(|(t, pc, r, v)| (*t, *pc, r.to_vec(), *v)).collect()
    }
    /// (call harness) the CPU's [r0..r14, pc] and cpsr.
    fn call_regs(&self) -> (Vec<u32>, u32) {
        let (r, c) = self.m.call_regs();
        (r.to_vec(), c)
    }
    /// (call harness) set the CPU's [r0..r14, pc] and cpsr. Another mode in cpsr switches the register banks (any mode bits), and r13/r14 are then what that mode's bank holds.
    fn set_cpu_regs(&mut self, regs: Vec<u32>, cpsr: u32) -> PyResult<()> {
        if regs.len() != 16 {
            return Err(pyo3::exceptions::PyValueError::new_err("16 registers: r0..r14 and pc"));
        }
        let mut r = [0u32; 16];
        r.copy_from_slice(&regs);
        self.m.set_cpu_regs(r, cpsr).map_err(pyo3::exceptions::PyValueError::new_err)
    }
    /// (call harness) run the CPU alone until pc == sentinel (0), the budget is spent (1) or pc leaves [lo, hi) (2, when hi > lo):
    /// or pc reaches one of `stops` (4, before it runs: a call the host answers), or an access faults (3, see call_fault); returns (status, instructions run).
    #[pyo3(signature = (sentinel, budget, lo=0, hi=0, stops=Vec::new()))]
    fn call_run(&mut self, sentinel: u32, budget: u64, lo: u32, hi: u32, stops: Vec<u32>) -> (u32, u64) {
        self.m.call_run(sentinel, budget, lo, hi, &stops)
    }
    /// (call harness) remember the CPU's registers and all of memory; writes are tracked from here on.
    fn snapshot(&mut self) {
        self.m.snapshot()
    }
    /// (call harness) put registers and memory back as snapshot() found them; the number of 1 KB pages copied back, or None.
    fn restore(&mut self) -> Option<usize> {
        self.m.restore()
    }
    /// (call harness) call mode on/off: only mapped pages exist, any other access faults; peripherals are plain memory then.
    /// literal: no aliases, no DRAM/IRAM arrays, code runs from the plain pages the host mapped (the gate's own address space).
    /// unaligned_faults: a misaligned halfword/word access is fault kind 4.
    #[pyo3(signature = (on, literal=false, unaligned_faults=false))]
    fn call_mode(&mut self, on: bool, literal: bool, unaligned_faults: bool) {
        self.m.call_mode(on, literal, unaligned_faults)
    }
    /// (call harness) trace the calls: every executed pc in [lo, hi) is logged as (pc, cpsr), and r0..r12 at each pc in reg_pcs; read with call_trace_take.
    fn call_trace_set(&mut self, lo: u32, hi: u32, reg_pcs: Vec<u32>) {
        self.m.call_trace_set(lo, hi, reg_pcs)
    }
    fn call_trace_off(&mut self) {
        self.m.call_trace_off()
    }
    fn call_trace_take(&mut self) -> (Vec<(u32, u32)>, Vec<(u32, Vec<u32>)>) {
        self.m.call_trace_take()
    }
    /// (call harness) map the 4 KB page of addr and fill it with data (4096 bytes).
    fn call_map_page(&mut self, addr: u32, data: Vec<u8>) {
        self.m.call_map_page(addr, &data)
    }
    /// (call harness) writes outside these [lo, hi) ranges are refused (a fault, before they land); none given: any mapped page is writable.
    fn call_write_ranges(&mut self, ranges: Vec<(u32, u32)>) {
        self.m.call_write_ranges(ranges)
    }
    /// (call harness) log every data read and write of the calls: (0 read / 1 write, addr, size, value, pc).
    fn call_log_on(&mut self, on: bool) {
        self.m.call_log_on(on)
    }
    fn call_log_take(&mut self) -> Vec<(u8, u32, u8, u32, u32)> {
        self.m.call_log_take()
    }
    /// (call harness) the fault that ended the last call (status 3): (kind, addr, size, pc, value); value is what a refused or unmapped store was
    /// trying to write (0 for the others); kinds: 0 unmapped read, 1 unmapped write,
    /// 2 write outside the allowed ranges, 3 unmapped instruction fetch, 4 misaligned.
    fn call_fault(&self) -> Option<(u8, u32, u8, u32, u32)> {
        self.m.call_fault()
    }
    fn cop_log(&self) -> Vec<(u32, u32)> {
        self.m.cop_log.clone()
    }
    /// (for study) single-step the CPU until the word at addr changes: (pc of the store, [r0..r3, lr], instructions run) or None.
    fn cpu_find_write(&mut self, addr: u32, max: u64) -> Option<(u32, [u32; 7], u64)> {
        self.m.cpu_find_write(addr, max)
    }
    /// (for study) report which core's slice first changed the word at addr
    fn watch(&mut self, addr: u32) {
        self.m.watch_on = true;
        self.m.watch_addr = addr;
        self.m.watch_hit = None;
    }
    fn set_bp(&mut self, pc: u32) {
        self.m.bp_pc = pc;
        self.m.bp_hits.clear();
        self.m.bp_mem.clear();
    }
    fn bp_mem(&self) -> Vec<Vec<u32>> {
        self.m.bp_mem.clone()
    }
    fn bp_hits(&self) -> Vec<(u64, Vec<u32>)> {
        self.m.bp_hits.iter().map(|(t, r)| (*t, r.to_vec())).collect()
    }
    fn ring(&self) -> Vec<(u32, Vec<u32>)> {
        self.m.ring.iter().map(|(p, r)| (*p, r.to_vec())).collect()
    }
    fn watch_all(&mut self, addr: u32) {
        self.m.watch_on = true;
        self.m.watch_addr = addr;
        self.m.watch_hit = None;
        self.m.watch_stop = false;
        self.m.watch_log.clear();
    }
    fn watch_log(&self) -> Vec<(u8, u64, u32, u32, u32)> {
        self.m.watch_log.clone()
    }
    /// (study) log device accesses in the given (low, high) address ranges, up to cap events
    fn ev_start(&mut self, ranges: Vec<(u32, u32)>, cap: usize) {
        self.m.s.ev_ranges = ranges;
        self.m.s.ev_cap = cap;
        self.m.s.ev_log.clear();
        self.m.s.ev_on = true;
    }
    fn ev_stop(&mut self) {
        self.m.s.ev_on = false;
    }
    fn ev_log(&self) -> Vec<(u8, bool, u32, u32, u64, u64)> {
        self.m.s.ev_log.clone()
    }
    fn watch_regs(&self) -> Vec<u32> {
        self.m.watch_regs.to_vec()
    }
    fn watch_hit(&self) -> Option<(u8, u64, u32, u32)> {
        self.m.watch_hit
    }
    /// (for study) single-step the coprocessor until the word at addr changes (see cpu_find_write).
    fn cop_find_write(&mut self, addr: u32, max: u64) -> Option<(u32, [u32; 7], u64)> {
        self.m.cop_find_write(addr, max)
    }
    /// (for study) mark the coprocessor awake so cop_trace runs it from where it sleeps.
    fn cop_wake_for_study(&mut self) {
        self.m.cop_wake_for_study()
    }
    /// Run the coprocessor n instructions singly and return [(pc, r0)] before each (stops if it sleeps).
    fn cop_trace(&mut self, n: usize) -> Vec<(u32, u32)> {
        self.m.cop_trace(n)
    }
    /// The coprocessor's r0..r14, pc and cpsr.
    fn cop_regs(&self) -> Vec<u32> {
        let c = &self.m.cop.cpu;
        let mut v: Vec<u32> = (0..15).map(|i| c.r[i]).collect();
        v.push(c.pc);
        v.push(c.cpsr);
        v
    }
    /// Stray accesses to unmapped memory (address, was a write).
    fn stray(&self) -> Vec<(u32, bool)> {
        self.m.s.stray.clone()
    }
}

#[pymodule]
fn emupod_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Machine>()?;
    m.add("VERSION", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
