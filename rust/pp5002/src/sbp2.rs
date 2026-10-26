//! A computer on the iPod's FireWire bus, as far as disk mode needs one: bus connect, an SBP-2 login, and SCSI command ORBs (INQUIRY,
//! READ CAPACITY, READ(10), WRITE(10)) handed to the disk-mode program through the link engine (`fw`). The drive is reached only
//! through the program: a READ(10)'s bytes are what it pushes, a WRITE(10)'s are what it pulls from the host's memory and writes.
use crate::{ata::Disk, fw, Config, Machine};

/// where the host says it wants data and status (any address: the host model takes every block write)
pub const DATA_HI: u32 = 0xffc0_0001;
pub const DATA_LO: u32 = 0x1234_0000;
pub const STATUS_HI: u32 = 0x0000_ffff;
pub const STATUS_LO: u32 = 0x0000_0000;
/// the most one ORB moves: 256 sectors, as a Linux host sends (128 KB); past 127 (a direct ORB's 16-bit data_size) it carries a page table
pub const MAX_SECTORS: u32 = 256;
/// a page table's segments: this long, at addresses apart and in the reverse order of the table's (so a transfer that ignored the table
/// would not land right by chance)
const SEGMENT: usize = 0x1000;
const TABLE_AT: u64 = 1 << 32 | 0x0020_0000;
const SEGMENTS_AT: u64 = 1 << 32 | 0x0040_0000;

/// Where osos leaves its boot token for the bootloader ("diskmode": start the chip's disk-mode program).
pub const TOKEN_AT: u32 = 0x4001_7f00;

/// osos has asked for disk mode (the cable went in: it saves its settings, writes the token and resets).
pub fn wants_diskmode(m: &mut Machine) -> bool {
    m.read_bytes(TOKEN_AT, 8) == b"diskmode"
}

/// The machine after osos's reset into disk mode: one booting from `chip` (the card's own aupd, `flash::assemble`d) on `disk`, with
/// the token and the FireWire lines as `old` left them. Memory survives a reset; the token is all of it the bootloader reads.
pub fn enter(old: &mut Machine, chip: &[u8], disk: Box<dyn Disk>, cfg: Config) -> Machine {
    let token = old.read_bytes(TOKEN_AT, 8);
    let mut m = Machine::from_chip(chip, disk, cfg);
    for (k, b) in token.iter().enumerate() {
        m.s.host_wr(TOKEN_AT + k as u32, *b);
    }
    for bit in 0..8 {
        let level = old.pin(2, 1 << bit);
        m.set_pin(2, 1 << bit, level);
    }
    m
}

/// Run the machine for `us` microseconds of its time.
pub fn run_for(m: &mut Machine, us: u64) {
    let end = m.now() + us;
    while m.now() < end {
        m.run();
        m.tick();
    }
}

fn engine(m: &mut Machine) -> &mut fw::Engine {
    m.s.fw.as_mut().expect("a machine booted from its chip")
}

/// A command ORB on the command fetch: type 8 (SCSI), channel 0xffff, then the 8-quadlet SBP-2 ORB with the CDB's 12 bytes. Returns
/// the number of packets the iPod had sent before it.
pub fn command(m: &mut Machine, data_len: u32, cdb: &[u8]) -> usize {
    command_to(m, data_len, cdb, cdb[0] == 0x2a)
}

/// `command`, with the direction given: `write`, the iPod reads the host's memory (the data is the host's).
pub fn command_to(m: &mut Machine, data_len: u32, cdb: &[u8], write: bool) -> usize {
    let mut c = [0u8; 12];
    c[..cdb.len()].copy_from_slice(cdb);
    let be = |k: usize| u32::from_be_bytes([c[k], c[k + 1], c[k + 2], c[k + 3]]);
    // options: notify, direction (1: the iPod reads the host's memory, for a write), data_size (16 bits)
    assert!(data_len <= 0xffff);
    let dir = if write { 1 << 27 } else { 0 };
    let orb = vec![8 << 28, 0xffff << 16, 0, 0, 0, DATA_HI, DATA_LO, 0x8000_0000 | dir | data_len, be(0), be(4), be(8)];
    let f = engine(m);
    let before = f.packets.len();
    f.cmd = Some((orb, 0));
    f.ring(fw::DB_CORB);
    before
}

/// The status block the iPod sent since packet `n0` (a block write to the status FIFO), if any.
pub fn status_since(m: &Machine, n0: usize) -> Option<Vec<u32>> {
    m.s.fw.as_ref()?.packets[n0..].iter().find(|p| p.len() > 4 && p[1] == STATUS_HI << 16 | STATUS_LO >> 16).cloned()
}

/// A status block says success: resp 0 (request complete), sbp_status 0, and no SCSI sense after it (len 1: the two quadlets of the
/// ORB's address alone).
pub fn status_ok(p: &[u32]) -> bool {
    let h = p[4] >> 16;
    h & 0x30ff == 0 && h >> 8 & 7 <= 1
}

/// Run until the iPod has sent a status block since packet `n0`, at most `max_us`; the block.
pub fn wait_status(m: &mut Machine, n0: usize, max_us: u64) -> Option<Vec<u32>> {
    let end = m.now() + max_us;
    while m.now() < end {
        m.run();
        m.tick();
        if let Some(p) = status_since(m, n0) {
            return Some(p);
        }
    }
    None
}

/// The host of a disk-mode session: what the iPod said at login and READ CAPACITY.
#[derive(Clone, Debug)]
pub struct Session {
    pub inquiry: Vec<u8>,
    /// the number of 512-byte blocks the iPod says it has
    pub blocks: u64,
    /// the most sectors one ORB asks for
    pub per_orb: u32,
}

/// Forget what the engine has sent and taken (a long session would keep every packet and pushed byte otherwise).
fn trim(m: &mut Machine) {
    let f = engine(m);
    f.packets.clear();
    f.sink.clear();
    f.landed.clear();
    f.host_mem.clear();
}

/// Put a host on the bus of a machine in disk mode and log in: bus connect, then a login ORB until the program answers it (it ignores
/// one until its event loop runs), at most `max_us`; then INQUIRY and READ CAPACITY.
pub fn connect(m: &mut Machine, per_orb: u32, max_us: u64) -> Result<Session, String> {
    let end = m.now() + max_us;
    engine(m).host = true;
    loop {
        trim(m);
        engine(m).ring(fw::DB_CONNECT);
        run_for(m, 200_000);
        // three header quadlets (q0 type 8, as the command fetch's), then the SBP-2 login ORB: password, login response address,
        // notify | exclusive | function 0 (login) | lun 0, password length 0 / login response length 12, status FIFO address
        let login = vec![8 << 28, 0xffc0 << 16, 0, 0, 0, DATA_HI, DATA_LO + 0x1000, 0x9000_0000, 12, STATUS_HI, STATUS_LO];
        let f = engine(m);
        f.mgmt = Some((login, 0));
        f.ring(fw::DB_MORB);
        run_for(m, 300_000);
        let ps = &m.s.fw.as_ref().unwrap().packets;
        let status = ps.iter().find(|p| p.len() >= 6 && p[1] >> 16 == 0xffc0 && p[2] == STATUS_LO && p[4] >> 24 == 0x41);
        if let Some(p) = status {
            if p[4] >> 16 & 0xff != 0 {
                return Err(format!("login refused: sbp_status {:#x}", p[4] >> 16 & 0xff));
            }
            break;
        }
        if m.now() >= end {
            return Err("no answer to the login".into());
        }
    }
    let inquiry = transfer(m, &[0x12, 0, 0, 0, 0x24, 0], 0x24, None)?;
    let cap = transfer(m, &[0x25], 8, None)?;
    if cap.len() < 8 {
        return Err(format!("READ CAPACITY: {} bytes", cap.len()));
    }
    let last = u32::from_be_bytes(cap[0..4].try_into().unwrap()) as u64;
    let size = u32::from_be_bytes(cap[4..8].try_into().unwrap());
    if size != 512 {
        return Err(format!("READ CAPACITY: block size {size}"));
    }
    Ok(Session { inquiry, blocks: last + 1, per_orb: per_orb.clamp(1, MAX_SECTORS) })
}

/// A command ORB whose data is described by a page table at TABLE_AT: segments of SEGMENT bytes. Returns the segments' addresses.
fn command_pt(m: &mut Machine, len: u32, cdb: &[u8], write: bool) -> Vec<(u64, usize)> {
    let n = (len as usize).div_ceil(SEGMENT);
    let segs: Vec<(u64, usize)> = (0..n).map(|k| (SEGMENTS_AT + ((n - 1 - k) * 2 * SEGMENT) as u64, SEGMENT.min(len as usize - k * SEGMENT))).collect();
    let table: Vec<u8> = segs.iter().flat_map(|&(at, l)| [((l as u32) << 16 | (at >> 32) as u32).to_be_bytes(), (at as u32).to_be_bytes()].concat()).collect();
    let mut c = [0u8; 12];
    c[..cdb.len()].copy_from_slice(cdb);
    let be = |k: usize| u32::from_be_bytes([c[k], c[k + 1], c[k + 2], c[k + 3]]);
    let dir = if write { 1 << 27 } else { 0 };
    // options: notify, direction, page_table_present (page_size 0: unrestricted), data_size = the table's entry count
    let orb = vec![8 << 28, 0xffff << 16, 0, 0, 0, (TABLE_AT >> 32) as u32, TABLE_AT as u32, 0x8000_0000 | dir | fw::PAGE_TABLE | n as u32, be(0), be(4), be(8)];
    let f = engine(m);
    f.host_mem.push((TABLE_AT, table));
    f.cmd = Some((orb, 0));
    f.ring(fw::DB_CORB);
    segs
}

/// One command ORB: `out` is the host's data for the iPod to pull (a write); returns what the iPod sent (pushed, or block-written to
/// the data address), once its status says success. Past a direct ORB's 16-bit data_size it goes with a page table.
pub fn transfer(m: &mut Machine, cdb: &[u8], len: u32, out: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let r = exec(m, cdb, len, out)?;
    if r.status != 0 {
        return Err(format!("command {:#04x}: SCSI status {:#04x}, sense {:02x?}", cdb[0], r.status, &r.sense[..r.sense.len().min(14)]));
    }
    Ok(r.data)
}

/// What the iPod answered a command ORB with.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    /// what it sent (for a command that sends data)
    pub data: Vec<u8>,
    /// the SCSI status: 0 good, 2 check condition, ...
    pub status: u8,
    /// fixed-format sense data (18 bytes) when the status block carried it; empty otherwise
    pub sense: Vec<u8>,
}

/// One command ORB, as `transfer`, answered with its SCSI status and sense (a host passing commands on needs them as they are); an error
/// is a transport failure only: no status block in 5 s, or one saying the ORB was not done (resp or sbp_status not 0).
pub fn exec(m: &mut Machine, cdb: &[u8], len: u32, out: Option<&[u8]>) -> Result<Reply, String> {
    // an ORB of the usual size carries 12 bytes of CDB
    if cdb.is_empty() || cdb.len() > 12 {
        return Err(format!("a {}-byte CDB does not fit a command ORB", cdb.len()));
    }
    trim(m);
    let write = out.is_some();
    if len > 0xffff {
        let n0 = m.s.fw.as_ref().unwrap().packets.len();
        let segs = command_pt(m, len, cdb, write);
        if let Some(d) = out {
            let mut k = 0;
            for &(at, l) in &segs {
                engine(m).host_mem.push((at, d[k..k + l].to_vec()));
                k += l;
            }
        }
        let st = wait_status(m, n0, 5_000_000).ok_or_else(|| format!("no status for command {:#04x}", cdb[0]))?;
        let mut r = reply(&st, cdb)?;
        // what was pushed, put back together in the table's order from where each segment landed
        let landed = &m.s.fw.as_ref().unwrap().landed;
        r.data = segs.iter().flat_map(|s| landed.iter().find(|l| l.0 == s.0).map(|l| l.1.clone()).unwrap_or_default()).collect();
        return Ok(r);
    }
    if let Some(d) = out {
        engine(m).host_mem = vec![(((DATA_HI & 0xffff) as u64) << 32 | DATA_LO as u64, d.to_vec())];
    }
    let n0 = command_to(m, len, cdb, write);
    let st = wait_status(m, n0, 5_000_000).ok_or_else(|| format!("no status for command {:#04x}", cdb[0]))?;
    let mut r = reply(&st, cdb)?;
    let f = m.s.fw.as_ref().unwrap();
    r.data = if !f.sink.is_empty() {
        f.sink.clone()
    } else {
        // a short reply (INQUIRY, READ CAPACITY) comes as a block write to the data address: its payload after the 4-quadlet header
        let p = f.packets.iter().find(|p| p.len() > 4 && p[1] == DATA_HI && p[2] == DATA_LO);
        p.map(|p| p[4..].iter().flat_map(|w| w.to_be_bytes()).collect()).unwrap_or_default()
    };
    if write {
        r.data.clear();
    }
    Ok(r)
}

/// A status block packet (4 header quadlets, then the block) as a SCSI status and sense: SBP-2's status block, and the SCSI command
/// set's status format after its two ORB-address quadlets (as Linux's firewire-sbp2 reads it: sbp2_status_to_sense_data).
fn reply(p: &[u32], cdb: &[u8]) -> Result<Reply, String> {
    let h = p[4] >> 16;
    // resp (bits 13:12 of the top half) and sbp_status (7:0)
    if h & 0x30ff != 0 {
        return Err(format!("command {:#04x}: ORB not done, resp {} sbp_status {:#x}", cdb[0], h >> 12 & 3, h & 0xff));
    }
    let b: Vec<u8> = p[6..].iter().flat_map(|w| w.to_be_bytes()).collect();
    // len (bits 10:8): the block's quadlets less one; 1 is the ORB address alone, no SCSI status: good
    if h >> 8 & 7 <= 1 || b.len() < 4 {
        return Ok(Reply::default());
    }
    let mut b = b;
    b.resize(16, 0);
    let sfmt = b[0] >> 6;
    let status = b[0] & 0x3f;
    if sfmt >= 2 {
        // vendor-dependent or reserved: no sense to give
        return Ok(Reply { status: if status == 0 { 2 } else { status }, ..Reply::default() });
    }
    let mut s = vec![0u8; 18];
    s[0] = 0x70 | sfmt | (b[1] & 0x80);
    s[2] = (b[1] << 1 & 0xe0) | (b[1] & 0x0f);
    s[3..7].copy_from_slice(&b[4..8]);
    s[7] = 10;
    s[8..12].copy_from_slice(&b[8..12]);
    s[12] = b[2];
    s[13] = b[3];
    s[14] = b[12];
    s[15..18].copy_from_slice(&b[13..16]);
    Ok(Reply { status, sense: if status == 2 { s } else { vec![] }, ..Reply::default() })
}

fn rw10(op: u8, lba: u64, n: u32) -> Result<[u8; 10], String> {
    if lba + n as u64 > u32::MAX as u64 + 1 {
        return Err(format!("LBA {lba} is past READ(10)'s reach"));
    }
    let l = lba as u32;
    Ok([op, 0, (l >> 24) as u8, (l >> 16) as u8, (l >> 8) as u8, l as u8, 0, (n >> 8) as u8, n as u8, 0])
}

impl Session {
    /// READ(10)s of `n` sectors from `lba`, at most `per_orb` each.
    pub fn read(&self, m: &mut Machine, lba: u64, n: u32) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(n as usize * 512);
        let mut k = 0;
        while k < n {
            let c = (n - k).min(self.per_orb);
            let got = transfer(m, &rw10(0x28, lba + k as u64, c)?, c * 512, None)?;
            if got.len() != c as usize * 512 {
                return Err(format!("READ(10) at {}: {} bytes for {c} sectors", lba + k as u64, got.len()));
            }
            out.extend_from_slice(&got);
            k += c;
        }
        Ok(out)
    }

    /// A host's command as it gave it (`exec`), but a READ(10) or WRITE(10) longer than `per_orb` goes as several, the LBA moved on
    /// for each; the first that does not answer good is the answer.
    pub fn exec(&self, m: &mut Machine, cdb: &[u8], len: u32, out: Option<&[u8]>) -> Result<Reply, String> {
        let n = if matches!(cdb.first(), Some(0x28 | 0x2a)) && cdb.len() == 10 { u16::from_be_bytes([cdb[7], cdb[8]]) as u32 } else { 0 };
        if n <= self.per_orb || len != n * 512 || out.is_some_and(|d| d.len() != len as usize) {
            return exec(m, cdb, len, out);
        }
        let lba = u32::from_be_bytes(cdb[2..6].try_into().unwrap()) as u64;
        let mut r = Reply::default();
        let mut k = 0;
        while k < n {
            let c = (n - k).min(self.per_orb);
            let mut sub = rw10(cdb[0], lba + k as u64, c)?;
            sub[1] = cdb[1];
            let part = exec(m, &sub, c * 512, out.map(|d| &d[k as usize * 512..(k + c) as usize * 512]))?;
            if part.status != 0 {
                return Ok(Reply { data: vec![], ..part });
            }
            r.data.extend_from_slice(&part.data);
            k += c;
        }
        Ok(r)
    }

    /// WRITE(10)s of `data` (whole sectors) from `lba`, at most `per_orb` each.
    pub fn write(&self, m: &mut Machine, lba: u64, data: &[u8]) -> Result<(), String> {
        if data.len() % 512 != 0 {
            return Err("a write of part of a sector".into());
        }
        for (i, chunk) in data.chunks(self.per_orb as usize * 512).enumerate() {
            let at = lba + (i * self.per_orb as usize) as u64;
            let c = (chunk.len() / 512) as u32;
            transfer(m, &rw10(0x2a, at, c)?, c * 512, Some(chunk))?;
        }
        Ok(())
    }
}
