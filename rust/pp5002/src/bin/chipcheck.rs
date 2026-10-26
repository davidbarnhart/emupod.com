//! The boot checks of a machine booting from its chip (M1, M2):   chipcheck <aupd.bin | chip.bin> <card.img>
//!  - normal: from reset the bootloader loads the card's osos and jumps to it; osos draws its first screen; the coprocessor sleeps
//!  - diskmode token: the bootloader starts the chip's disk-mode program, which runs from IRAM as the chip holds it
//!  - disk mode with a cable in: its event loop runs and it draws its screen (compared by a hash of the pixels)
use pp5002::{ata::FileDisk, card, flash, lcd, Config, Machine};

const EVENT_LOOP: u32 = 0x4000_0c7c;
/// where the chip keeps the disk-mode program (flshdisk), and its length
const FLSHDISK: usize = 0xeafa0;
const FLSHDISK_LEN: usize = 0x14e60;
/// the pixels of the disk-mode screen at the event loop + 2 s: "OK to disconnect.", the check mark, the charging battery
const DISKMODE_SCREEN: u64 = 0x2195_01c8_d6d3_7cb3;

fn fnv(px: &[u8]) -> u64 {
    px.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn machine(chip: &[u8], card: &str, plugged: bool, token: Option<&[u8; 8]>) -> Machine {
    let disk = Box::new(FileDisk(std::fs::File::open(card).unwrap()));
    let mut m = Machine::from_chip(chip, disk, Config { plugged, ..Config::default() });
    if let Some(t) = token {
        for (k, b) in t.iter().enumerate() {
            m.s.host_wr(0x4001_7f00 + k as u32, *b);
        }
    }
    m
}

/// Run until the CPU reaches `pc` (at most `max_us`); whether it did.
fn run_to(m: &mut Machine, pc: u32, max_us: u64) -> bool {
    m.hook_pcs = vec![pc];
    m.hook_log.clear();
    while m.hook_log.is_empty() && m.now() < max_us {
        m.run();
        m.tick();
    }
    m.hook_pcs.clear();
    !m.hook_log.is_empty()
}

fn run_for(m: &mut Machine, us: u64) {
    let end = m.now() + us;
    while m.now() < end {
        m.run();
        m.tick();
    }
}

fn screen(m: &Machine) -> Vec<u8> {
    let mut s = lcd::Screen::default();
    s.update(&m.s.lcd_log);
    s.px
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let src = std::fs::read(&a[1]).expect("chip");
    let chip = if src.len() == flash::SIZE { src } else { flash::assemble(&src).expect("assemble") };
    let osos = card::read_osos(&FileDisk(std::fs::File::open(&a[2]).unwrap())).expect("osos on the card").0;
    let mut fails = 0;
    let mut check = |name: &str, ok: bool, extra: String| {
        println!("{} {name}  {extra}", if ok { "  ok  " } else { " FAIL " });
        fails += !ok as u32;
    };

    // M1, normal: the bootloader loads osos itself
    let mut m = machine(&chip, &a[2], false, None);
    let jumped = run_to(&mut m, 0x2800_0000, 30_000_000);
    let loaded = m.read_bytes(0x2800_0000, osos.len()) == osos;
    check("normal reset: the bootloader jumps to 0x28000000 holding the card's osos", jumped && loaded, format!("at {} us", m.now()));
    let lcd0 = m.s.lcd_count;
    run_for(&mut m, 25_000_000);
    check("osos draws its first screen", m.s.lcd_count > lcd0 + 4000, format!("{} LCD writes", m.s.lcd_count - lcd0));
    check("the coprocessor sleeps", m.s.cop_asleep, format!("cop pc {:#010x}", m.cop.cpu.pc));

    // M1, the diskmode token: the chip's disk-mode program, run from IRAM as the chip holds it
    let mut m = machine(&chip, &a[2], true, Some(b"diskmode"));
    let jumped = run_to(&mut m, 0x2800_0000, 30_000_000);
    let token = m.read_bytes(0x4001_7f00, 8);
    check("diskmode token: the bootloader takes it and jumps to the disk-mode program", jumped && token != b"diskmode", format!("token now {:?}", String::from_utf8_lossy(&token)));
    let in_iram = run_to(&mut m, EVENT_LOOP, 30_000_000);
    let iram = m.read_bytes(0x4000_0000, FLSHDISK_LEN);
    let differ = (0..FLSHDISK_LEN).filter(|&k| iram[k] != chip[FLSHDISK + k]).count();
    // what the program writes into its own image by the time its loop runs: its data (34 bytes on 1.5)
    check("M2: its event loop runs from IRAM, which holds the chip's program", in_iram && differ < 64, format!("{differ} bytes of IRAM differ from the chip's (data)"));
    run_for(&mut m, 2_000_000);
    let px = screen(&m);
    if let Ok(path) = std::env::var("CC_SHOT") {
        let mut out = format!("P5\n{} {}\n255\n", lcd::W, lcd::H).into_bytes();
        out.extend(px.iter().map(|&v| 255 - 85 * v));
        std::fs::write(path, out).unwrap();
    }
    let h = fnv(&px);
    check("M2: it draws the disk-mode screen", h == DISKMODE_SCREEN, format!("screen hash {h:#018x}"));
    println!("{}", if fails == 0 { "PASS" } else { "FAIL" });
    std::process::exit(fails as i32);
}
