//! The firmware a card would boot: its osos, read from the card image's firmware partition (a port of emupod/card.py;
//! the layout is described there).
use crate::ata::Disk;

fn rd(d: &dyn Disk, off: u64, n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    d.read_at(&mut b, off);
    b
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// The osos image and its load address, or why the card has none.
pub fn read_osos(d: &dyn Disk) -> Result<(Vec<u8>, u32), String> {
    read_image(d, b"osos")
}

/// An image of the card's firmware directory by its type (b"osos", b"aupd"), with its load address, or why the card has none.
pub fn read_image(d: &dyn Disk, want: &[u8; 4]) -> Result<(Vec<u8>, u32), String> {
    let name = String::from_utf8_lossy(want);
    let mbr = rd(d, 0, 512);
    if mbr[510..512] != [0x55, 0xaa] {
        return Err("no partition table (sector 0 has no MBR signature)".into());
    }
    let (ptype, start, count) = (mbr[446 + 4], u32le(&mbr, 446 + 8), u32le(&mbr, 446 + 12));
    if ptype != 0 || start == 0 || count == 0 {
        return Err(format!("partition 1 is not a firmware partition (type {ptype:#04x}, start {start}, {count} sectors)"));
    }
    let base = start as u64 * 512;
    let head = rd(d, base, 0x200);
    if &head[..4] != b"{{~~" || &head[0x100..0x104] != b"]ih[" {
        return Err("the firmware partition is empty (no boot block or volume header)".into());
    }
    let ptr = u32le(&head, 0x104) as u64;
    let fmt = u16::from_le_bytes([head[0x10a], head[0x10b]]);
    let dir = rd(d, base + ptr + 512, 10 * 40);
    for e in dir.chunks_exact(40) {
        if e[..4] == [0, 0, 0, 0] {
            break;
        }
        let mut typ = [e[4], e[5], e[6], e[7]];
        typ.reverse();
        if &typ != want {
            continue;
        }
        let (dev, len, addr, cksum) = (u32le(e, 12) as u64, u32le(e, 16) as usize, u32le(e, 20), u32le(e, 28));
        let osos = rd(d, base + dev + if fmt == 3 { 512 } else { 0 }, len);
        if osos.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32)) != cksum {
            return Err(format!("{name} checksum mismatch"));
        }
        return Ok((osos, addr));
    }
    Err(format!("no {name} in the firmware directory"))
}
