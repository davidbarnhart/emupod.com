//! Boot from the flash chip (study):   rom <aupd.bin | chip.bin> <card.img> [seconds] [token]
//! An aupd.bin is assembled into a chip (pp5002::flash::assemble); a 1 MB file is taken as the chip. `token` (8 characters, e.g.
//! diskmode) is put at 0x40017f00 before reset, where osos leaves it for the bootloader.
//! Prints, every 100 ms of iPod time, where the CPU is, and at the end what the run touched.
use pp5002::{ata::FileDisk, flash, lcd, Config, Machine};

fn region(pc: u32, low_flash: bool) -> &'static str {
    match pc >> 20 {
        0x000..=0x01f if low_flash => "flash",
        0x400 => "iram",
        0x280..=0x29f | 0x000..=0x01f => "dram",
        _ => "other",
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: rom <aupd.bin|chip.bin> <card.img> [seconds] [token]");
        std::process::exit(2);
    }
    let src = std::fs::read(&a[1]).expect("chip");
    let chip = if src.len() == flash::SIZE { src } else { flash::assemble(&src).expect("assemble") };
    let secs: f64 = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(5.0);
    let disk = Box::new(FileDisk(std::fs::File::open(&a[2]).expect("card")));
    // ROM_PLUGGED: a FireWire cable is in (port C bit 7 low), as when a computer is connected
    let cfg = Config { plugged: std::env::var("ROM_PLUGGED").is_ok(), ..Config::default() };
    let mut m = Machine::from_chip(&chip, disk, cfg);
    if let Some(t) = a.get(4) {
        for (k, b) in t.bytes().take(8).enumerate() {
            m.s.host_wr(0x4001_7f00 + k as u32, b);
        }
    }
    if let Ok(h) = std::env::var("ROM_HOOKS") {
        m.hook_pcs = h.split(',').map(|x| u32::from_str_radix(x.trim_start_matches("0x"), 16).unwrap()).collect();
    }
    println!("reset: cop pc {:#010x} asleep {}", m.cop.cpu.pc, m.s.cop_asleep);
    let mut last_region = "";
    let mut last_cop = u32::MAX;
    let mut next_print = 0u64;
    let end = (secs * 1e6) as u64;
    let cop_trace: usize = std::env::var("ROM_COPTRACE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut cop_traced = false;
    while m.now() < end {
        if cop_trace > 0 && !cop_traced && !m.s.cop_asleep && m.s.cop_wakes > 0 {
            // the coprocessor's first steps after the first wake
            cop_traced = true;
            for (pc, r0) in m.cop_trace(cop_trace) {
                println!("  cop {pc:#010x} r0 {r0:#x}");
            }
        }
        m.run();
        m.tick();
        if std::env::var("ROM_COPWATCH").is_ok() {
            let st = if m.s.cop_asleep { 0 } else { m.cop.cpu.pc >> 12 };
            if st != last_cop {
                println!("{:>9} us  cop {}", m.now(), if m.s.cop_asleep { format!("asleep (pc {:#010x})", m.cop.cpu.pc) } else { format!("{:#010x} cpsr {:#x} lr {:#010x}", m.cop.cpu.pc, m.cop.cpu.cpsr, m.cop.cpu.r[14]) });
                last_cop = st;
            }
        }
        let pc = m.cpu.cpu.pc;
        let r = region(pc, m.s.flash.as_ref().map_or(false, |_| m.low_flash()));
        if r != last_region {
            println!("{:>9} us  cpu enters {r} at {pc:#010x} (lr {:#010x})", m.now(), m.cpu.cpu.r[14]);
            last_region = r;
        }
        if m.now() >= next_print {
            let f = m.s.flash.as_ref().unwrap();
            println!(
                "{:>9} us  pc {pc:#010x} cpsr {:#x}  lcd {}  ata cmds {} read {} written {}  flash erases {} programs {}  cop {}",
                m.now(),
                m.cpu.cpu.cpsr,
                m.s.lcd_count,
                m.s.ata.n_cmds,
                m.s.ata.sectors_read,
                m.s.ata.sectors_written,
                f.erases.len(),
                f.programs,
                if m.s.cop_asleep { "asleep".to_string() } else { format!("{:#010x}", m.cop.cpu.pc) }
            );
            next_print += 100_000;
        }
        if m.s.fault.is_some() {
            break;
        }
    }
    if let Ok(n) = std::env::var("ROM_TRACE") {
        for (pc, cpsr, r) in m.cpu_trace(n.parse().unwrap()) {
            let w = m.s.host_rd(pc) as u32 | (m.s.host_rd(pc + 1) as u32) << 8 | (m.s.host_rd(pc + 2) as u32) << 16 | (m.s.host_rd(pc + 3) as u32) << 24;
            println!("  {pc:#010x} {w:08x} cpsr {cpsr:#x} r0 {:#x} r1 {:#x} r2 {:#x} r3 {:#x} sp {:#x} lr {:#x}", r[0], r[1], r[2], r[3], r[13], r[14]);
        }
    }
    for (t, pc, r, _) in &m.hook_log {
        println!("  hook {t:>9} us  pc {pc:#010x}  r0 {:#x} r1 {:#x} r2 {:#x} r3 {:#x} lr {:#x}", r[0], r[1], r[2], r[3], r[14]);
    }
    if let Ok(spec) = std::env::var("ROM_CMP") {
        // ROM_CMP=chip_offset,len: IRAM 0x40000000.. against the chip's bytes there
        let v: Vec<usize> = spec.split(',').map(|x| usize::from_str_radix(x.trim_start_matches("0x"), 16).unwrap()).collect();
        let iram = m.read_bytes(0x4000_0000, v[1]);
        let diff: Vec<usize> = (0..v[1]).filter(|&k| iram[k] != chip[v[0] + k]).collect();
        println!("iram vs chip[{:#x}..+{:#x}]: {} bytes differ, first {:x?}", v[0], v[1], diff.len(), &diff[..diff.len().min(8)]);
    }
    if let Ok(list) = std::env::var("ROM_PEEK") {
        for x in list.split(',') {
            let a = u32::from_str_radix(x.trim_start_matches("0x"), 16).unwrap();
            let b = m.read_bytes(a, 4);
            println!("peek {a:#010x} = {:#010x}", u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }
    }
    if let Ok(list) = std::env::var("ROM_FIND") {
        // word values in IRAM (the first 0x20000) and DRAM (the first 4 MB)
        let iram = m.read_bytes(0x4000_0000, 0x2_0000);
        let dram = m.read_bytes(0x2800_0000, 0x40_0000);
        for x in list.split(',') {
            let v = u32::from_str_radix(x.trim_start_matches("0x"), 16).unwrap().to_le_bytes();
            let hits: Vec<String> = [(0x4000_0000u32, &iram), (0x2800_0000, &dram)].iter().flat_map(|(base, mem)| {
                (0..mem.len() - 3).step_by(4).filter(|&k| mem[k..k + 4] == v).map(move |k| format!("{:#010x}", base + k as u32))
            }).take(16).collect();
            println!("find {x}: {}", hits.join(" "));
        }
    }
    println!("mmap {:x?}  low_flash {}", m.mmap(), m.low_flash());
    let f = m.s.flash.as_ref().unwrap();
    println!("end at {} us: pc {:#010x} fault {:?} resets {}", m.now(), m.cpu.cpu.pc, m.s.fault, m.s.resets);
    println!("flash: {} erases, {} programs, unknown commands {:?}", f.erases.len(), f.programs, &f.unknown[..f.unknown.len().min(8)]);
    let mut stray: Vec<u32> = m.s.stray.iter().map(|&(a, _)| a & !0xfff).collect();
    stray.sort();
    stray.dedup();
    println!("stray pages ({} accesses): {:x?}", m.s.stray.len(), &stray[..stray.len().min(24)]);
    let mut hist: std::collections::BTreeMap<(u32, bool), u64> = Default::default();
    for &(a, w) in &m.s.stray {
        *hist.entry((a, w)).or_default() += 1;
    }
    for ((a, w), n) in hist.iter().take(40) {
        println!("  stray {} {a:#010x} x{n}", if *w { "W" } else { "R" });
    }
    let log = &m.s.ata.log;
    println!("ata log ({}), first: {:x?}", log.len(), &log[..log.len().min(12)]);
    println!("iram 0x40017f00: {:?}", String::from_utf8_lossy(&m.read_bytes(0x4001_7f00, 16)));
    if std::env::var("ROM_LCDSTAT").is_ok() {
        let mut h: std::collections::BTreeMap<u32, u64> = Default::default();
        for &(a, _) in &m.s.lcd_log {
            *h.entry(a).or_default() += 1;
        }
        println!("lcd addresses: {:x?}", h);
        let nz = m.s.lcd_log.iter().filter(|&&(a, v)| a == 0xc000_1010 && v & 0xff != 0).count();
        let first_nz = m.s.lcd_log.iter().position(|&(a, v)| a == 0xc000_1010 && v & 0xff != 0);
        println!("lcd nonzero data bytes: {nz}, first at log index {first_nz:?} of {}", m.s.lcd_log.len());
        let idx: std::collections::BTreeSet<u32> = m.s.lcd_log.chunks(1).filter(|c| c[0].0 == 0xc000_1008).map(|c| c[0].1 & 0xff).collect();
        println!("lcd index bytes used: {idx:x?}");
        let tail: Vec<String> = m.s.lcd_log.iter().rev().take(40).rev().map(|(a, v)| format!("{:x}:{:x}", a & 0xff, v)).collect();
        println!("lcd tail: {}", tail.join(" "));
    }
    if let Ok(path) = std::env::var("ROM_LCDDUMP") {
        // the controller writes as (address low byte, value) pairs
        let out: Vec<u8> = m.s.lcd_log.iter().flat_map(|&(a, v)| [(a & 0xff) as u8, v as u8]).collect();
        std::fs::write(&path, out).unwrap();
    }
    if let Ok(path) = std::env::var("ROM_OUT_CHIP") {
        std::fs::write(&path, &m.s.flash.as_ref().unwrap().cells).unwrap();
        println!("chip cells: {path}");
    }
    if let Ok(path) = std::env::var("ROM_SHOT") {
        // the screen as a PGM (white = level 0)
        let mut sc = lcd::Screen::default();
        sc.update(&m.s.lcd_log);
        let mut out = format!("P5\n{} {}\n255\n", lcd::W, lcd::H).into_bytes();
        out.extend(sc.px.iter().map(|&v| 255 - 85 * v));
        std::fs::write(&path, out).unwrap();
        println!("screen: {path}");
    }
}
