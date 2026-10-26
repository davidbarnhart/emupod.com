//! Study: what the disk-mode program shows when the host ejects (SCSI START STOP UNIT and the like).
//!     ejectprobe <card.img> <out-prefix> [cdb hex, e.g. 1b00000002 00]...
//! Boots the card's own disk-mode program with a cable in, logs in as the host, saves the screen (PGM), then sends each CDB and saves
//! the screen 3 s after each.
use pp5002::{ata::FileDisk, flash, lcd, sbp2, Config, Machine};

fn save(m: &mut Machine, sc: &mut lcd::Screen, path: &str) {
    let log = std::mem::take(&mut m.s.lcd_log);
    sc.update(&log);
    let mut b = format!("P5\n{} {}\n3\n", lcd::W, lcd::H).into_bytes();
    b.extend(sc.px.iter().map(|v| 3 - v));
    std::fs::write(path, b).unwrap();
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let open = || FileDisk(std::fs::OpenOptions::new().read(true).write(true).open(&a[1]).unwrap());
    let aupd = pp5002::card::read_image(&open(), b"aupd").expect("aupd").0;
    let chip = flash::assemble(&aupd).expect("assemble");
    let mut m = Machine::from_chip(&chip, Box::new(open()), Config { plugged: true, ..Config::default() });
    for (k, b) in b"diskmode".iter().enumerate() {
        m.s.host_wr(0x4001_7f00 + k as u32, *b);
    }
    let mut sc = lcd::Screen::default();
    sbp2::run_for(&mut m, 7_000_000);
    let s = sbp2::connect(&mut m, 256, 10_000_000).expect("login");
    println!("logged in: {} sectors at {} us", s.blocks, m.now());
    sbp2::run_for(&mut m, 2_000_000);
    save(&mut m, &mut sc, &format!("{}-0-ready.pgm", a[2]));
    for (i, h) in a[3..].iter().enumerate() {
        let cdb: Vec<u8> = (0..h.len() / 2).map(|k| u8::from_str_radix(&h[2 * k..2 * k + 2], 16).unwrap()).collect();
        let r = sbp2::transfer(&mut m, &cdb, 0, None);
        println!("cdb {h}: {:?}", r.map(|v| v.len()));
        sbp2::run_for(&mut m, 3_000_000);
        save(&mut m, &mut sc, &format!("{}-{}-{h}.pgm", a[2], i + 1));
    }
}
