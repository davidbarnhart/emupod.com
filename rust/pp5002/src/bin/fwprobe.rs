//! study: the FireWire cable from a running iPod, as the live server will drive it:   fwprobe <card.img (written to)> [sectors per ORB]
//! normal boot -> cable in -> osos's token -> the chip's disk mode -> a host logs in, writes and reads -> cable out -> normal boot.
use pp5002::ata::FileDisk;
use pp5002::{flash, sbp2, Config, Machine};
use std::time::Instant;

fn rw(path: &str) -> Box<FileDisk> {
    Box::new(FileDisk(std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap()))
}

fn shot(m: &Machine, scr: &mut pp5002::lcd::Screen, path: &str) {
    scr.update(&m.s.lcd_log);
    let mut out = format!("P5\n{} {}\n255\n", pp5002::lcd::W, pp5002::lcd::H).into_bytes();
    out.extend(scr.px.iter().map(|&v| 255 - 85 * v));
    std::fs::write(path, out).unwrap();
}

fn main() {
    let shots = std::env::var("FWP_SHOT").ok();
    let a: Vec<String> = std::env::args().collect();
    let per: u32 = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(sbp2::MAX_SECTORS);
    let aupd = pp5002::card::read_image(&*rw(&a[1]), b"aupd").unwrap().0;
    let chip = flash::assemble(&aupd).unwrap();
    let osos = pp5002::card::read_osos(&*rw(&a[1])).unwrap().0;
    let cfg = || Config { cop_ipus: 26, interleave_playing: 300, interleave_settle: 200_000, slice_clock: true, lazy_map: true, persist: true, ..Config::default() };
    let mut m = Machine::with_disk(&osos, rw(&a[1]), cfg());
    m.boot();
    for _ in 0..100 {
        m.settle(1, 1 << 30);
    }
    m.set_pin(2, 1 << 7, false);
    m.edge(2, 1 << 7);
    let t0 = m.now();
    while !sbp2::wants_diskmode(&mut m) && m.now() < t0 + 10_000_000 {
        m.settle(1, 1 << 30);
    }
    println!("cable in: token after {} us", m.now() - t0);
    let mut m = sbp2::enter(&mut m, &chip, rw(&a[1]), Config { persist: true, ..Config::default() });
    let (w0, e0) = (Instant::now(), m.now());
    let mut session = None;
    m.hook_pcs = vec![0x4000_0c7c, 0x2800_0000];
    while session.is_none() && m.now() < e0 + 30_000_000 {
        sbp2::run_for(&mut m, 500_000);
        match sbp2::connect(&mut m, per, 0) {
            Ok(s) => session = Some(s),
            Err(e) => { let tok = m.read_bytes(sbp2::TOKEN_AT, 8); println!("  {} us: {e}; hooks {:x?} pc {:#x} token {:?}", m.now() - e0, m.hook_log.iter().map(|h| h.1).collect::<std::collections::BTreeSet<_>>(),
                m.cpu.cpu.pc, String::from_utf8_lossy(&tok)) }
        }
    }
    m.hook_pcs.clear();
    let s = session.expect("login");
    println!("logged in at {} us of iPod time ({:.2} s wall): {} blocks, {:?}", m.now() - e0, w0.elapsed().as_secs_f64(), s.blocks,
        String::from_utf8_lossy(&s.inquiry[8..]).trim_end());
    let n = 2048u32;
    let data: Vec<u8> = (0..n as usize * 512).map(|k| (k * 31 + 7) as u8).collect();
    let (w0, e0) = (Instant::now(), m.now());
    s.write(&mut m, 200_000, &data).unwrap();
    let (we, ee) = (w0.elapsed().as_secs_f64(), (m.now() - e0) as f64 / 1e6);
    let (w0, e0) = (Instant::now(), m.now());
    let back = s.read(&mut m, 200_000, n).unwrap();
    println!("{n} sectors, {per} per ORB: write {ee:.2} s iPod / {we:.2} s wall, read {:.2} s iPod / {:.2} s wall; same {}",
        (m.now() - e0) as f64 / 1e6, w0.elapsed().as_secs_f64(), back == data);
    let mut scr = pp5002::lcd::Screen::default();
    if let Some(p) = &shots {
        shot(&m, &mut scr, &format!("{p}-before.pgm"));
    }
    m.s.cfg.trace_io = true;
    m.s.io.clear();
    sbp2::run_for(&mut m, 1_000_000);
    let mut io: Vec<_> = m.s.io.iter().filter(|((_, _, a), _)| a >> 8 == 0xcf0000 || a >> 8 == 0xcf0050 || a >> 8 == 0xcf0040).map(|(k, v)| (*k, *v)).collect();
    io.sort();
    println!("disk mode, 1 s idle: {io:x?}");
    let how = std::env::var("FWP_OUT").unwrap_or_else(|_| "cna".into());
    if how.contains("eject") {
        println!("  eject: {:?}", sbp2::transfer(&mut m, &[0x1b, 0, 0, 0, 2, 0], 0, None).map(|v| v.len()));
    }
    if how.contains("logout") {
        let orb = vec![8 << 28, 0xffc0 << 16, 0, 0, 0, 0, 0, 0x8007_0000, 0, sbp2::STATUS_HI, sbp2::STATUS_LO];
        let f = m.s.fw.as_mut().unwrap();
        let n0 = f.packets.len();
        f.mgmt = Some((orb, 0));
        f.ring(pp5002::fw::DB_MORB);
        sbp2::run_for(&mut m, 500_000);
        println!("  logout: {:x?}", &m.s.fw.as_ref().unwrap().packets[n0..]);
    }
    if how.contains("cna") {
        m.set_pin(2, 1 << 7, true);
        m.edge(2, 1 << 7);
    }
    if how.contains("power") {
        m.set_pin(2, 1 << 4, true);
        m.edge(2, 1 << 4);
    }
    if how.contains("bus") {
        m.s.fw.as_mut().unwrap().ring(pp5002::fw::DB_BUS_RESET);
    }
    if how.contains("plug") {
        m.set_pin(2, 1 << 4, false);
        m.edge(2, 1 << 4);
        sbp2::run_for(&mut m, 2_000_000);
        m.set_pin(2, 1 << 4, true);
        m.edge(2, 1 << 4);
    }
    let t0 = m.now();
    m.hook_pcs = vec![0, 0x4000, 0x2800_0000];
    m.hook_log.clear();
    while m.s.resets == 0 && m.now() < t0 + 10_000_000 {
        m.run();
        m.tick();
    }
    for h in m.hook_log.iter().take(10) {
        println!("  hook {:#x} at {} us", h.1, h.0.saturating_sub(t0));
    }
    m.s.cfg.trace_io = true;
    m.s.io.clear();
    sbp2::run_for(&mut m, 500_000);
    let mut io: Vec<_> = m.s.io.iter().map(|(k, v)| (*k, *v)).collect();
    io.sort();
    println!("stuck: io {io:x?}");
    if let Some(p) = &shots {
        shot(&m, &mut scr, &format!("{p}-after.pgm"));
    }
    println!("cable out ({how}): resets {} after {} us, pc {:#x}", m.s.resets, m.now() - t0, m.cpu.cpu.pc);
}
