//! Disk mode from the chip, with a host on the FireWire bus (study, and the M3 checks):
//!     diskmode <aupd.bin | chip.bin> <card.img>
//! Boots with the diskmode token and a cable in, waits for the event loop, then acts as the host: bus connect, an SBP-2 login, and
//! INQUIRY / READ CAPACITY / READ(10) command ORBs. Prints what the iPod sent back and checks it against the card.
use pp5002::sbp2::{command, run_for, DATA_HI, DATA_LO, STATUS_HI, STATUS_LO};
use pp5002::{ata::FileDisk, flash, fw, Config, Machine};

const EVENT_LOOP: u32 = 0x4000_0c7c;
fn word(m: &mut Machine, a: u32) -> u32 {
    let b = m.read_bytes(a, 4);
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn packets_since(m: &Machine, n: usize) -> Vec<Vec<u32>> {
    m.s.fw.as_ref().unwrap().packets[n..].to_vec()
}

/// The SBP-2 status the iPod sent among `ps` says success (resp 0, sbp_status 0).
fn status_ok(ps: &[Vec<u32>]) -> bool {
    ps.iter().any(|p| p.len() >= 6 && p[1] >> 16 == 0xffff && p[4] >> 16 & 0x30ff == 0)
}

fn show(tag: &str, ps: &[Vec<u32>]) {
    println!("{tag}: {} packet(s)", ps.len());
    for p in ps {
        let q: Vec<String> = p.iter().take(16).map(|w| format!("{w:08x}")).collect();
        println!("   [{}] {}", p.len(), q.join(" "));
    }
}

/// M4: write a firmware partition image to the card's partition 1 (LBA 63) with WRITE(10)s of 64 sectors, as a computer would in
/// disk mode; then reset (a new machine on the written card, from the same chip) and check that the bootloader runs the osos it now holds.
fn write_firmware(chip: &[u8], card_path: &str, fw_path: &str) -> bool {
    let fwimg = std::fs::read(fw_path).expect("firmware image");
    let before = pp5002::card::read_osos(&FileDisk(std::fs::File::open(card_path).unwrap())).expect("osos on the card").0;
    let work = format!("{card_path}.m4");
    std::fs::copy(card_path, &work).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&work).unwrap();
    let mut m = Machine::from_chip(chip, Box::new(FileDisk(f)), Config { plugged: true, persist: true, ..Config::default() });
    for (k, b) in b"diskmode".iter().enumerate() {
        m.s.host_wr(0x4001_7f00 + k as u32, *b);
    }
    m.s.fw.as_mut().unwrap().host = true;
    m.hook_pcs = vec![EVENT_LOOP];
    while m.hook_log.is_empty() && m.now() < 20_000_000 {
        m.run();
        m.tick();
    }
    m.hook_pcs.clear();
    run_for(&mut m, 300_000);
    m.s.fw.as_mut().unwrap().ring(fw::DB_CONNECT);
    run_for(&mut m, 200_000);
    // a host's first commands, as a computer sends them on mounting
    let n0 = command(&mut m, 0x24, &[0x12, 0, 0, 0, 0x24, 0]);
    pp5002::sbp2::wait_status(&mut m, n0, 2_000_000);
    let base = ((DATA_HI & 0xffff) as u64) << 32 | DATA_LO as u64;
    let sectors = (fwimg.len() + 511) / 512;
    let t0 = m.now();
    let mut lba = 0usize;
    let per: usize = std::env::var("DM_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
    let mut cmds = 0;
    while lba < sectors {
        let n = (sectors - lba).min(per);
        let mut chunk = fwimg[lba * 512..((lba + n) * 512).min(fwimg.len())].to_vec();
        chunk.resize(n * 512, 0);
        m.s.fw.as_mut().unwrap().host_mem = vec![(base, chunk)];
        let at = (63 + lba) as u32;
        let n0 = command(&mut m, (n * 512) as u32, &[0x2a, 0, (at >> 24) as u8, (at >> 16) as u8, (at >> 8) as u8, at as u8, 0, (n >> 8) as u8, n as u8, 0]);
        if pp5002::sbp2::wait_status(&mut m, n0, 3_000_000).is_none() {
            println!(" FAIL  no status for WRITE(10) at LBA {at}");
            return false;
        }
        lba += n;
        cmds += 1;
    }
    println!("  wrote {sectors} sectors in {cmds} WRITE(10)s, {:.2} s of iPod time", (m.now() - t0) as f64 / 1e6);
    drop(m);
    // reset: the chip boots the card as it now is
    let osos = pp5002::card::read_osos(&FileDisk(std::fs::File::open(&work).unwrap())).expect("osos on the written card").0;
    let f = std::fs::File::open(&work).unwrap();
    let mut m = Machine::from_chip(chip, Box::new(FileDisk(f)), Config::default());
    m.hook_pcs = vec![0x2800_0000];
    while m.hook_log.is_empty() && m.now() < 30_000_000 {
        m.run();
        m.tick();
    }
    let loaded = m.read_bytes(0x2800_0000, osos.len());
    let same = !m.hook_log.is_empty() && loaded == osos;
    let changed = osos != before;
    println!("{} after reset the bootloader jumps to the written osos ({} bytes, {} the card's old one) at {} us",
        if same && changed { "  ok  " } else { " FAIL " }, osos.len(), if changed { "not" } else { "SAME AS" }, m.now());
    std::fs::remove_file(&work).ok();
    same && changed
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if let Ok(fwp) = std::env::var("DM_FW") {
        let src = std::fs::read(&a[1]).expect("chip");
        let chip = if src.len() == flash::SIZE { src } else { flash::assemble(&src).expect("assemble") };
        let ok = write_firmware(&chip, &a[2], &fwp);
        std::process::exit(if ok { 0 } else { 1 });
    }
    let src = std::fs::read(&a[1]).expect("chip");
    let chip = if src.len() == flash::SIZE { src } else { flash::assemble(&src).expect("assemble") };
    let sectors = std::fs::metadata(&a[2]).expect("card").len() / 512;
    let mut card = vec![0u8; 512];
    pp5002::ata::Disk::read_at(&FileDisk(std::fs::File::open(&a[2]).unwrap()), &mut card, 0);
    let disk = Box::new(FileDisk(std::fs::File::open(&a[2]).unwrap()));
    let mut m = Machine::from_chip(&chip, disk, Config { plugged: true, trace_io: std::env::var("DM_IO").is_ok(), ..Config::default() });
    for (k, b) in b"diskmode".iter().enumerate() {
        m.s.host_wr(0x4001_7f00 + k as u32, *b);
    }
    m.s.fw.as_mut().unwrap().host = true;
    m.hook_pcs = vec![EVENT_LOOP];
    while m.hook_log.is_empty() && m.now() < 20_000_000 {
        m.run();
        m.tick();
    }
    m.hook_pcs.clear();
    println!("event loop at {} us", m.now());
    if std::env::var("DM_IO").is_ok() {
        let mut io: Vec<_> = m.s.io.iter().filter(|((_, _, a), _)| a >> 12 == 0xc0003).map(|(k, v)| (*k, *v)).collect();
        io.sort();
        for ((c, w, a), n) in io {
            println!("  io core{c} {} {a:#010x} x{n}", if w { "W" } else { "R" });
        }
    }
    run_for(&mut m, 500_000);
    let mut fails = 0;
    let mut check = |name: &str, ok: bool, extra: String| {
        println!("{} {name}  {extra}", if ok { "  ok  " } else { " FAIL " });
        if !ok {
            fails += 1;
        }
    };

    // bus connect
    m.s.fw.as_mut().unwrap().ring(fw::DB_CONNECT);
    run_for(&mut m, 200_000);
    let lcb = word(&mut m, 0x4001_64a8);
    let accepting = m.read_bytes(lcb + 0xc5, 1)[0];
    check("after bus connect, [lcb+0xc5] == 1", accepting == 1, format!("lcb {lcb:#x}"));

    // login (SBP-2 login ORB: password, login response, notify | function 0 | lun 0, lengths, status FIFO)
    let n0 = m.s.fw.as_ref().unwrap().packets.len();
    // three header quadlets (as the command fetch's: q0 bits 31:28 = 8, the type its handler 0x40007d78 needs, else status 0x101 'request
    // type not supported'; bits 27:24 must be 0), then the SBP-2 login ORB: password, login response
    // address, notify | exclusive | function 0 (login) | lun 0, password length 0 / login response length 12, status FIFO address
    let login = vec![8 << 28, 0xffc0 << 16, 0, 0, 0, DATA_HI, DATA_LO + 0x1000, 0x9000_0000, 12, STATUS_HI, STATUS_LO];
    {
        let f = m.s.fw.as_mut().unwrap();
        f.mgmt = Some((login, 0));
        f.ring(fw::DB_MORB);
    }
    run_for(&mut m, 500_000);
    let ps = packets_since(&m, n0);
    show("login", &ps);
    let response = ps.iter().find(|p| p.len() >= 8 && p[1] == DATA_HI && p[2] == DATA_LO + 0x1000);
    let status = ps.iter().find(|p| p.len() >= 6 && p[1] >> 16 == 0xffc0 && p[2] == STATUS_LO && p[4] >> 24 == 0x41);
    check("login: a login response, then status success (sbp_status 0)", response.is_some() && status.map_or(false, |p| p[4] >> 16 & 0xff == 0),
        format!("response {:x?}, status {:x?}", response.map(|p| &p[4..]), status.map(|p| p[4])));

    // a management doorbell with nothing on the fetch: the fetch's gate says so, and nothing is answered
    let n0 = m.s.fw.as_ref().unwrap().packets.len();
    m.s.fw.as_mut().unwrap().ring(fw::DB_MORB);
    run_for(&mut m, 300_000);
    check("a management doorbell with no ORB is not answered", packets_since(&m, n0).is_empty(), format!("{} packets", packets_since(&m, n0).len()));

    // INQUIRY
    let n0 = command(&mut m, 0x24, &[0x12, 0, 0, 0, 0x24, 0]);
    run_for(&mut m, 500_000);
    let ps = packets_since(&m, n0);
    show("INQUIRY", &ps);
    let text: Vec<u8> = ps.iter().flat_map(|p| p.iter().skip(4).flat_map(|w| w.to_be_bytes())).collect();
    let s = String::from_utf8_lossy(&text);
    check("INQUIRY reply names the iPod", s.contains("iPod"), format!("{:?}", s.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').collect::<String>()));

    // READ CAPACITY
    let n0 = command(&mut m, 8, &[0x25]);
    run_for(&mut m, 500_000);
    let ps = packets_since(&m, n0);
    show("READ CAPACITY", &ps);
    let last_lba = ps.iter().find(|p| p.len() >= 6 && p[1] == DATA_HI).map(|p| p[4]);
    check("READ CAPACITY == sectors - 1", last_lba == Some(sectors as u32 - 1), format!("got {last_lba:x?}, card has {sectors} sectors"));

    // READ(10) LBA 0, 1 block, the first transfer
    let pushed0 = m.s.fw.as_ref().unwrap().sink.len();
    let n0 = command(&mut m, 512, &[0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
    run_for(&mut m, 500_000);
    let ps = packets_since(&m, n0);
    show("READ(10)", &ps);
    let sink = m.s.fw.as_ref().unwrap().sink[pushed0..].to_vec();
    check("READ(10) LBA 0 pushes the card's MBR, then status success", sink.len() == 512 && sink[..] == card[..512] && status_ok(&ps), format!("{} bytes pushed", sink.len()));

    // WRITE(10) LBA 100, 1 block, then a READ(10) of it back (a read comes first here and none in DM_FW's writes: each transfer's
    // completion must stand on its own, not on a continuation the other direction left)
    let pattern: Vec<u8> = (0..512u32).map(|k| (k * 7 + 3) as u8).collect();
    m.s.fw.as_mut().unwrap().host_mem.push((((DATA_HI & 0xffff) as u64) << 32 | DATA_LO as u64, pattern.clone()));
    m.s.fw.as_mut().unwrap().log.clear();
    m.s.fw.as_mut().unwrap().log_on = true;
    let n0 = command(&mut m, 512, &[0x2a, 0, 0, 0, 0, 100, 0, 0, 1, 0]);
    run_for(&mut m, 500_000);
    m.s.fw.as_mut().unwrap().log_on = false;
    let ps = packets_since(&m, n0);
    show("WRITE(10)", &ps);
    check("WRITE(10) ends with status success", status_ok(&ps), String::new());
    let mut regs: std::collections::BTreeMap<(bool, u32), Vec<u32>> = Default::default();
    let log = m.s.fw.as_ref().unwrap().log.clone();
    let mut acc = 0u32;
    for &(w, r, b, v) in &log {
        acc |= v << (8 * b);
        if b == 3 {
            let e = regs.entry((w, r)).or_default();
            if e.len() < 6 && e.last() != Some(&acc) {
                e.push(acc);
            }
            acc = 0;
        }
    }
    if std::env::var("DM_REGS").is_ok() {
        for ((w, r), vs) in &regs {
            println!("  engine {} reg {r:#04x}: {:x?}", if *w { "W" } else { "R" }, vs);
        }
    }
    let pushed0 = m.s.fw.as_ref().unwrap().sink.len();
    let n1 = command(&mut m, 512, &[0x28, 0, 0, 0, 0, 100, 0, 0, 1, 0]);
    run_for(&mut m, 500_000);
    let ps = packets_since(&m, n1);
    show("READ(10) of it", &ps);
    let back = m.s.fw.as_ref().unwrap().sink[pushed0..].to_vec();
    check("WRITE(10) then READ(10) LBA 100 gives the host's bytes back, then status success", back == pattern && status_ok(&ps), format!("{} bytes", back.len()));
    if let Ok(far) = std::env::var("DM_FAR") {
        // READ(10) of one sector at an LBA (past 2^28, say) against the card's bytes there
        let at: u32 = far.parse().unwrap();
        let pushed0 = m.s.fw.as_ref().unwrap().sink.len();
        let n0 = command(&mut m, 512, &[0x28, 0, (at >> 24) as u8, (at >> 16) as u8, (at >> 8) as u8, at as u8, 0, 0, 1, 0]);
        run_for(&mut m, 500_000);
        show("READ(10) far", &packets_since(&m, n0));
        let got = m.s.fw.as_ref().unwrap().sink[pushed0..].to_vec();
        let mut want = vec![0u8; 512];
        pp5002::ata::Disk::read_at(&FileDisk(std::fs::File::open(&a[2]).unwrap()), &mut want, at as u64 * 512);
        check(&format!("READ(10) at LBA {at} pushes the card's sector"), got == want, format!("{} bytes pushed", got.len()));
    }
    println!("ata log tail: {:x?}", &m.s.ata.log[m.s.ata.log.len().saturating_sub(6)..]);
    println!("{}", if fails == 0 { "PASS" } else { "FAIL" });
    std::process::exit(if fails == 0 { 0 } else { 1 });
}
