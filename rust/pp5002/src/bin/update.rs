//! The flash-ROM update, from reset (M5):   update <old aupd.bin | chip.bin> <card.img with a pending aupd>
//! Boots a chip with FireWire power on a card whose aupd entry says an update is pending (tools/synth_card.py --pending-aupd). The
//! bootloader runs the card's updater, which rewrites the chip and resets the iPod. Checks that the chip afterwards is the one the
//! card's aupd describes (pp5002::flash::assemble, byte for byte), and that the iPod boots on from it.
use pp5002::{ata::FileDisk, flash, Config, Machine};

/// The card's aupd image (from its live firmware directory).
fn card_aupd(path: &str) -> Vec<u8> {
    let card = std::fs::read(path).unwrap();
    let base = 63 * 512;
    let u32le = |o: usize| u32::from_le_bytes(card[o..o + 4].try_into().unwrap()) as usize;
    let dir = base + u32le(base + 0x104) + 0x200;
    for e in (dir..dir + 400).step_by(40) {
        let mut typ = [card[e + 4], card[e + 5], card[e + 6], card[e + 7]];
        typ.reverse();
        if &typ == b"aupd" {
            let (dev, len) = (u32le(e + 12), u32le(e + 16));
            return card[base + dev..base + dev + len].to_vec();
        }
    }
    panic!("no aupd on the card");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let src = std::fs::read(&a[1]).expect("chip");
    let old = if src.len() == flash::SIZE { src } else { flash::assemble(&src).expect("assemble") };
    let want = flash::assemble(&card_aupd(&a[2])).expect("the card's aupd");
    let disk = Box::new(FileDisk(std::fs::File::open(&a[2]).unwrap()));
    let mut m = Machine::from_chip(&old, disk, Config { plugged: true, ..Config::default() });
    while m.s.resets == 0 && m.now() < 120_000_000 {
        m.run();
        m.tick();
    }
    let f = m.s.flash.as_ref().unwrap();
    let (erases, programs) = (f.erases.len(), f.programs);
    let cells = f.cells.clone();
    let diff = (0..flash::SIZE).filter(|&k| cells[k] != want[k]).count();
    println!("updater: {erases} erases, {programs} words programmed, reset at {} us", m.now());
    let mut fails = 0;
    let mut check = |name: &str, ok: bool, extra: String| {
        println!("{} {name}  {extra}", if ok { "  ok  " } else { " FAIL " });
        fails += !ok as u32;
    };
    check("the updater reset the iPod", m.s.resets == 1, String::new());
    check("the chip is the card's aupd's, byte for byte", diff == 0, format!("{diff} bytes differ"));
    check("the old chip was different", old != want, String::new());
    // on from the reset: the new bootloader and the card's osos (with the cable in, osos goes on into disk mode)
    let t = m.now();
    let lcd0 = m.s.lcd_count;
    while m.now() < t + 15_000_000 {
        m.run();
        m.tick();
    }
    check("after the reset the new chip boots and draws", m.s.lcd_count > lcd0 + 5000, format!("{} LCD writes", m.s.lcd_count - lcd0));
    check("the coprocessor is asleep, not in an exception", m.s.cop_asleep, format!("cop pc {:#010x}", m.cop.cpu.pc));
    println!("{}", if fails == 0 { "PASS" } else { "FAIL" });
    std::process::exit(fails as i32);
}
