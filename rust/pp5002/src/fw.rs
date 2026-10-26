//! The FireWire (1394) link engine the disk-mode program drives at 0x30000000 (a port of FWPush in openpod's
//! tools/ipod_emu_timer.py, GitHub #1/#8/#10/#14/#15).
//!
//! Register access is as the program's writer and reader do it: each 32-bit engine register R lives at 0x30000000 + R * 2, spread
//! over four halfwords whose low bytes hold its bytes 0..3. So the register of an offset is (offset / 8) * 4, its byte (offset % 8) / 2.
//! Registers are plain storage unless named here.

pub const BASE: u32 = 0x3000_0000;
/// interrupt (doorbell) status, write-1-to-clear while a doorbell is raised
pub const DOORBELL: u32 = 0x0c;
/// doorbell bits (the engine ISR's decode): a management ORB, a command ORB, a push finished, the bus connected, a bus reset
pub const DB_MORB: u32 = 0x20;
pub const DB_CORB: u32 = 0x10;
pub const DB_XFER: u32 = 0x08;
pub const DB_PULL: u32 = 0x04;
pub const DB_CONNECT: u32 = 0x0800_0000;
pub const DB_BUS_RESET: u32 = 0x2000_0000;
/// the management ORB fetch: reg 0x38 bit 28 clear = a quadlet is ready (as the command fetch's reg 0x40), reg 0x84 = the next
/// quadlet (advancing on its byte 3)
const MGMT_GATE: u32 = 0x38;
const MGMT_FIFO: u32 = 0x84;
/// the command ORB fetch: reg 0x40 bit 28 set = none left, reg 0x88 = the next quadlet
const CMD_GATE: u32 = 0x40;
const CMD_FIFO: u32 = 0x88;
/// the push: reg 0xb0 low 16 bits = block size, bits 31:30 = go (both set starts it, cleared when done); reg 0xb4 = block count;
/// reg 0x90 = status (the done bits the program acknowledges)
pub const PUSH_CTL: u32 = 0xb0;
pub const PUSH_COUNT: u32 = 0xb4;
pub const PUSH_STATUS: u32 = 0x90;
const KICK: u32 = 0xc000_0000;
pub const PUSH_DONE: u32 = 0x0800_0018;
// Not modelled: a finished transfer's result in reg 0xa0 (the push's done handler packs (reg 0xa0 & 0x1f1) | reg 0x60 bit 23 >> 8,
// the pull's the high half with reg 0x60 bit 22). Reads and writes complete with it left at 0, so nothing here says what it holds.
/// the pull (a host's data into the drive, WRITE(10)'s twin of the push): reg 0xc0 low 16 bits = block size, bit 31 = go; reg 0xc4 =
/// block count; regs 0xc8 / 0xcc = the host address (node and offset high, offset low) the data is read from
pub const PULL_CTL: u32 = 0xc0;
pub const PULL_COUNT: u32 = 0xc4;
pub const PULL_HI: u32 = 0xc8;
pub const PULL_LO: u32 = 0xcc;
/// the push's host address (as the pull's)
pub const PUSH_HI: u32 = 0xb8;
pub const PUSH_LO: u32 = 0xbc;
/// in reg 0xb0 / 0xc0, the ORB's page_table_present (with page_size in bits 18:16, as in the ORB's options): the host address is an SBP-2
/// page table's, whose entry count is in reg 0xe4 (the push's in the high half, the pull's in the low), and the data goes to (comes
/// from) its segments (1.5 flshdisk's 0x4000b380 / 0x4000b480, read statically by the firmware session; not traced)
pub const PAGE_TABLE: u32 = 1 << 19;
pub const PT_COUNT: u32 = 0xe4;
/// the async transmit (block writes the program sends: SCSI replies, status blocks): reg 0x2c = free FIFO quadlets, 0x70 = header,
/// 0x74 = the next quadlets, 0x78 = the last (it ends the packet); reg 0x60 low byte 0x26 sends and waits for the response
const TX_FREE: u32 = 0x2c;
const TX_GO: u32 = 0x60;
const TX_HEAD: u32 = 0x70;
const TX_DATA: u32 = 0x74;
const TX_END: u32 = 0x78;
/// the async receive: reg 0x30 = (quadlets << 16) | bit 28, reg 0x80 = the next quadlet; reg 0x04 = the ack of the last packet sent
const RX_COUNT: u32 = 0x30;
const RX_FIFO: u32 = 0x80;
const ACK: u32 = 0x04;
/// the doorbell the program waits on for a response
pub const DB_RX: u32 = 0x80;

pub struct Engine {
    reg: Vec<u32>,
    /// quadlets the host has queued on the management and command ORB fetches (None: the fetch is not in use, the registers are storage)
    pub mgmt: Option<(Vec<u32>, usize)>,
    pub cmd: Option<(Vec<u32>, usize)>,
    /// a push the program started: the bytes it asks for, taken from the drive by the machine (`take_push`), and where they go
    push_due: Option<usize>,
    push_to: Vec<(u64, usize)>,
    armed: bool,
    pull_due: Option<Vec<(u64, usize)>>,
    pull_armed: bool,
    /// the host's memory the pulls read: (48-bit address, bytes)
    pub host_mem: Vec<(u64, Vec<u8>)>,
    /// what the pushes sent to the host, and the same by the host address each part landed at
    pub sink: Vec<u8>,
    pub landed: Vec<(u64, Vec<u8>)>,
    /// the doorbell asks for the CPU's attention (the machine raises the interrupt)
    pub irq: bool,
    /// a host is on the bus: it takes the program's packets (`packets`, each as the quadlets written to 0x70/0x74/0x78) and answers
    /// each with ack pending and a write response
    pub host: bool,
    tx: Vec<u32>,
    pub packets: Vec<Vec<u32>>,
    rx: Vec<u32>,
    rx_pos: usize,
    /// reg 0xc clears the bits written as 1 while a doorbell raised by the host is pending (the program's own early writes are stores)
    w1c: bool,
    /// (study) every access: (write?, register, byte, value)
    pub log: Vec<(bool, u32, u32, u32)>,
    pub log_on: bool,
}

impl Default for Engine {
    fn default() -> Engine {
        Engine { reg: vec![0; 0x400], mgmt: None, cmd: None, push_due: None, push_to: Vec::new(), armed: true, pull_due: None, pull_armed: true, host_mem: Vec::new(), sink: Vec::new(), landed: Vec::new(), irq: false, host: false, tx: Vec::new(), packets: Vec::new(), rx: Vec::new(), rx_pos: 0, w1c: false, log: Vec::new(), log_on: false }
    }
}

fn split(a: u32) -> (u32, u32) {
    let off = a - BASE;
    ((off / 8) * 4, (off % 8) / 2)
}

impl Engine {
    pub fn get(&self, r: u32) -> u32 {
        self.reg[(r / 4) as usize]
    }

    pub fn set(&mut self, r: u32, v: u32) {
        self.reg[(r / 4) as usize] = v;
    }

    /// Raise doorbell bits in reg 0xc (the program's interrupt handler reads and clears them).
    pub fn ring(&mut self, bits: u32) {
        let v = self.get(DOORBELL) | bits;
        self.set(DOORBELL, v);
        self.w1c = true;
        self.irq = true;
    }

    /// The bytes of a push the program has started, if any (the machine moves them from the drive and calls `push_done`).
    pub fn take_push(&mut self) -> Option<usize> {
        self.push_due.take()
    }

    pub fn push_done(&mut self, data: &[u8]) {
        self.sink.extend_from_slice(data);
        let mut k = 0;
        for (at, n) in std::mem::take(&mut self.push_to) {
            let end = (k + n).min(data.len());
            self.landed.push((at, data[k..end].to_vec()));
            k = end;
        }
        let c = self.get(PUSH_CTL) & !KICK;
        self.set(PUSH_CTL, c);
        self.armed = true;
        let st = self.get(PUSH_STATUS) | PUSH_DONE;
        self.set(PUSH_STATUS, st);
        self.ring(DB_XFER);
    }

    /// A pull the program has started: the bytes, read from the host's memory (zeros where the host has none). The machine writes them
    /// to the drive and calls `pull_done`.
    pub fn take_pull(&mut self) -> Option<Vec<u8>> {
        let segs = self.pull_due.take()?;
        Some(segs.iter().flat_map(|&(at, n)| self.host_read(at, n)).collect())
    }

    /// `n` bytes of the host's memory at `at` (zeros where the host has none)
    fn host_read(&self, at: u64, n: usize) -> Vec<u8> {
        let mut out = vec![0u8; n];
        for (base, bytes) in &self.host_mem {
            let (lo, hi) = (at.max(*base), (at + n as u64).min(base + bytes.len() as u64));
            if lo < hi {
                out[(lo - at) as usize..(hi - at) as usize].copy_from_slice(&bytes[(lo - base) as usize..(hi - base) as usize]);
            }
        }
        out
    }

    /// Where a transfer goes: its control register's host address, or, with a page table there, the table's segments (8-byte entries:
    /// segment_length and segment_base_hi, then segment_base_lo, big-endian).
    fn segments(&self, ctl: u32, hi: u32, lo: u32, count: u32, total: usize) -> Vec<(u64, usize)> {
        let at = ((self.get(hi) & 0xffff) as u64) << 32 | self.get(lo) as u64;
        if self.get(ctl) & PAGE_TABLE == 0 {
            return vec![(at, total)];
        }
        let t = self.host_read(at, 8 * count as usize);
        t.chunks(8).map(|e| {
            let q0 = u32::from_be_bytes(e[0..4].try_into().unwrap());
            (((q0 & 0xffff) as u64) << 32 | u32::from_be_bytes(e[4..8].try_into().unwrap()) as u64, (q0 >> 16) as usize)
        }).collect()
    }

    pub fn pull_done(&mut self) {
        let c = self.get(PULL_CTL) & !0x8000_0000;
        self.set(PULL_CTL, c);
        self.set(PULL_COUNT, 0);
        self.pull_armed = true;
        self.ring(DB_PULL);
    }

    /// The register value a read sees: the fetch FIFOs and gates when the host uses them, storage otherwise.
    fn value(&self, r: u32) -> u32 {
        match (r, &self.mgmt, &self.cmd) {
            (MGMT_GATE, Some((q, i)), _) => if *i < q.len() { 0 } else { 0x1000_0000 },
            (MGMT_FIFO, Some((q, i)), _) => q.get(*i).copied().unwrap_or(0),
            (CMD_GATE, _, Some((q, i))) => if *i < q.len() { 0 } else { 0x1000_0000 },
            (CMD_FIFO, _, Some((q, i))) => q.get(*i).copied().unwrap_or(0),
            (TX_FREE, _, _) if self.host => 0x1ff,
            (RX_COUNT, _, _) if self.host => ((self.rx.len() - self.rx_pos) as u32) << 16 | 1 << 28,
            (RX_FIFO, _, _) if self.host => self.rx.get(self.rx_pos).copied().unwrap_or(0),
            _ => self.get(r),
        }
    }

    pub fn read(&mut self, a: u32, size: u32) -> u32 {
        let mut v = 0;
        // a halfword or word read takes the low byte of each halfword it covers
        for k in 0..(size.max(2) / 2) {
            let (r, b) = split(a + 2 * k);
            let byte = (self.value(r) >> (8 * b)) & 0xff;
            if b == 3 {
                // a FIFO advances when its quadlet's last byte is read
                match r {
                    MGMT_FIFO => if let Some((_, i)) = self.mgmt.as_mut() { *i += 1 },
                    CMD_FIFO => if let Some((_, i)) = self.cmd.as_mut() { *i += 1 },
                    RX_FIFO if self.host && self.rx_pos < self.rx.len() => self.rx_pos += 1,
                    _ => {}
                }
            }
            if self.log_on && self.log.len() < 1_000_000 {
                self.log.push((false, r, b, byte));
            }
            v |= byte << (16 * k);
        }
        if size == 1 {
            v & 0xff
        } else {
            v
        }
    }

    pub fn write(&mut self, a: u32, size: u32, v: u32) {
        for k in 0..(size.max(2) / 2) {
            let (r, b) = split(a + 2 * k);
            let byte = (v >> (16 * k)) & 0xff;
            if self.log_on && self.log.len() < 1_000_000 {
                self.log.push((true, r, b, byte));
            }
            let cur = self.get(r);
            if r == DOORBELL && self.w1c {
                let n = cur & !(byte << (8 * b));
                self.set(r, n);
                if n == 0 {
                    self.w1c = false;
                }
                continue;
            }
            self.set(r, (cur & !(0xff << (8 * b))) | (byte << (8 * b)));
            if self.host && b == 3 {
                // the program writes a quadlet's byte 3 last
                let q = self.get(r);
                match r {
                    TX_HEAD | TX_DATA => self.tx.push(q),
                    TX_END => {
                        self.tx.push(q);
                        let p = std::mem::take(&mut self.tx);
                        self.packets.push(p);
                    }
                    TX_GO if q & 0xff == 0x26 => {
                        // the host acks pending (2) and sends a write response (tcode 2, rcode 0)
                        self.set(ACK, 2 << 4);
                        self.rx = vec![0, 0xffc0 << 16 | 2 << 4, 0];
                        self.rx_pos = 0;
                        self.ring(DB_RX);
                    }
                    _ => {}
                }
            }
            if r == PULL_CTL && b == 3 {
                let on = self.get(PULL_CTL) & 0x8000_0000 != 0;
                if on && self.pull_armed && self.get(PULL_COUNT) != 0 {
                    self.pull_armed = false;
                    let total = (self.get(PULL_COUNT) as usize) * (self.get(PULL_CTL) & 0xffff) as usize;
                    self.pull_due = Some(self.segments(PULL_CTL, PULL_HI, PULL_LO, self.get(PT_COUNT) & 0xffff, total));
                } else if !on {
                    self.pull_armed = true;
                }
            }
            if r == PUSH_CTL {
                // go is in byte 3: one push per rising edge
                let on = self.get(PUSH_CTL) & KICK == KICK;
                if on && self.armed && self.get(PUSH_COUNT) != 0 {
                    self.armed = false;
                    let total = (self.get(PUSH_COUNT) as usize) * (self.get(PUSH_CTL) & 0xffff) as usize;
                    self.push_to = self.segments(PUSH_CTL, PUSH_HI, PUSH_LO, self.get(PT_COUNT) >> 16, total);
                    // with a page table the segments say how much (the firmware session's reading)
                    self.push_due = Some(if self.get(PUSH_CTL) & PAGE_TABLE != 0 { self.push_to.iter().map(|s| s.1).sum() } else { total });
                } else if !on {
                    self.armed = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_are_spread_over_halfwords() {
        let mut e = Engine::default();
        for (k, b) in [0x78u32, 0x56, 0x34, 0x12].iter().enumerate() {
            e.write(BASE + 0xb0 * 2 + 2 * k as u32, 2, *b | 0xab00);
        }
        assert_eq!(e.get(0xb0), 0x1234_5678, "only the low byte of each halfword counts");
        assert_eq!(e.read(BASE + 0x160, 2), 0x78);
        assert_eq!(e.read(BASE + 0x166, 2), 0x12);
    }

    #[test]
    fn doorbell_clears_by_writing_ones_while_raised() {
        let mut e = Engine::default();
        e.write(BASE + 0x18, 2, 0x20);
        assert_eq!(e.get(DOORBELL), 0x20, "before a doorbell, a write is a store");
        e.set(DOORBELL, 0);
        e.ring(0x30);
        e.write(BASE + 0x18, 2, 0x10);
        assert_eq!(e.get(DOORBELL), 0x20);
        e.write(BASE + 0x18, 2, 0x20);
        assert_eq!(e.get(DOORBELL), 0);
        e.write(BASE + 0x18, 2, 0x04);
        assert_eq!(e.get(DOORBELL), 0x04, "consumed: a store again");
    }

    /// as the program's reader does: four halfword reads, the low byte of each
    fn rd32(e: &mut Engine, r: u32) -> u32 {
        (0..4).fold(0, |v, k| v | (e.read(BASE + r * 2 + 2 * k, 2) & 0xff) << (8 * k))
    }

    #[test]
    fn fifos_hand_out_quadlets_then_close_their_gates() {
        let mut e = Engine::default();
        e.cmd = Some((vec![0x8000_0000, 0x1234_5678], 0));
        assert_eq!(rd32(&mut e, CMD_GATE), 0);
        assert_eq!(rd32(&mut e, CMD_FIFO), 0x8000_0000);
        assert_eq!(rd32(&mut e, CMD_FIFO), 0x1234_5678);
        assert_eq!(rd32(&mut e, CMD_GATE), 0x1000_0000);
        e.mgmt = Some((vec![7], 0));
        assert_eq!((rd32(&mut e, MGMT_GATE), rd32(&mut e, MGMT_FIFO), rd32(&mut e, MGMT_GATE)), (0, 7, 0x1000_0000));
    }

    #[test]
    fn a_kick_asks_for_count_times_size_once() {
        let mut e = Engine::default();
        let w = |e: &mut Engine, r: u32, v: u32| for k in 0..4 { e.write(BASE + r * 2 + 2 * k, 2, (v >> (8 * k)) & 0xff) };
        w(&mut e, PUSH_COUNT, 3);
        w(&mut e, PUSH_CTL, 0xc000_0200);
        assert_eq!(e.take_push(), Some(0x600));
        w(&mut e, PUSH_CTL, 0xc000_0200);
        assert_eq!(e.take_push(), None, "go still set: no new push");
        e.push_done(&[1, 2]);
        assert_eq!((e.get(PUSH_CTL) >> 30, e.get(PUSH_STATUS) & PUSH_DONE, e.get(DOORBELL) & DB_XFER, e.sink.len()), (0, PUSH_DONE, DB_XFER, 2));
        w(&mut e, PUSH_CTL, 0xc000_0200);
        assert_eq!(e.take_push(), Some(0x600), "go cleared by the engine: the next kick is a new push");
    }

    #[test]
    fn page_tables_scatter_and_gather() {
        let w = |e: &mut Engine, r: u32, v: u32| for k in 0..4 { e.write(BASE + r * 2 + 2 * k, 2, (v >> (8 * k)) & 0xff) };
        let entry = |len: u32, at: u64| [((len << 16) | (at >> 32) as u32).to_be_bytes(), (at as u32).to_be_bytes()].concat();
        let mut e = Engine::default();
        // a table of two segments at addresses far apart, the second first in memory
        let table = [entry(4, 0x1_0000_9000), entry(2, 0x1_0000_1000)].concat();
        e.host_mem = vec![(0x1_0000_0000, table), (0x1_0000_9000, vec![1, 2, 3, 4]), (0x1_0000_1000, vec![5, 6])];
        w(&mut e, PULL_HI, 1);
        w(&mut e, PULL_LO, 0);
        w(&mut e, PT_COUNT, 2);
        w(&mut e, PULL_COUNT, 3);
        w(&mut e, PULL_CTL, 0x8008_0002);
        assert_eq!(e.take_pull(), Some(vec![1, 2, 3, 4, 5, 6]), "a pull gathers the segments in table order");
        w(&mut e, PUSH_HI, 1);
        w(&mut e, PUSH_LO, 0);
        w(&mut e, PT_COUNT, 2 << 16);
        w(&mut e, PUSH_COUNT, 3);
        w(&mut e, PUSH_CTL, 0xc008_0002);
        assert_eq!(e.take_push(), Some(6));
        e.push_done(&[9, 8, 7, 6, 5, 4]);
        assert_eq!(e.landed, vec![(0x1_0000_9000, vec![9, 8, 7, 6]), (0x1_0000_1000, vec![5, 4])], "a push lands each part where its entry says");
    }

    #[test]
    fn the_host_takes_packets_and_answers() {
        let mut e = Engine { host: true, ..Engine::default() };
        let w = |e: &mut Engine, r: u32, v: u32| for k in 0..4 { e.write(BASE + r * 2 + 2 * k, 2, (v >> (8 * k)) & 0xff) };
        assert_eq!(rd32(&mut e, TX_FREE), 0x1ff);
        w(&mut e, TX_HEAD, 0x11);
        w(&mut e, TX_DATA, 0x22);
        w(&mut e, TX_DATA, 0x33);
        w(&mut e, TX_END, 0x44);
        assert_eq!(e.packets, vec![vec![0x11, 0x22, 0x33, 0x44]]);
        w(&mut e, TX_GO, 0x26);
        assert_eq!((e.get(DOORBELL) & DB_RX, (rd32(&mut e, RX_COUNT) >> 16) & 0xfff), (DB_RX, 3));
        let r: Vec<u32> = (0..3).map(|_| rd32(&mut e, RX_FIFO)).collect();
        assert_eq!(r, vec![0, 0xffc0_0020, 0]);
        assert_eq!((rd32(&mut e, RX_COUNT) >> 16) & 0xfff, 0);
    }
}
