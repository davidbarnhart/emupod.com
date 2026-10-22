//! The firmware a card would boot: its osos, read from the card image's firmware partition the way the iPod's boot ROM finds it
//! (emupod/card.py, the same rules).
//!
//! MBR entry 0 is the firmware partition (type 0). At its start "{{~~" (the boot block); at +0x100 the "]ih[" volume header, whose
//! word at +0x104 points to the directory and half-word at +0x10a is the format. The directory is one sector past the pointer:
//! 40-byte entries (id, type reversed, then eight words: devOffset, len, addr, entryOffset, checksum, vers, loadAddr). The osos sits
//! at its devOffset from the partition start (one sector further in format 3) and its checksum is the byte sum.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

const SECTOR: u64 = 512;

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn read_at(f: &mut File, off: u64, n: usize) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut v = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut v)?;
    Ok(v)
}

/// The osos bytes of the card at `image`, or why there are none.
pub fn read_osos(image: &str) -> Result<Vec<u8>, String> {
    let mut f = File::open(image).map_err(|e| format!("{image}: {e}"))?;
    let io = |e: std::io::Error| e.to_string();
    let mbr = read_at(&mut f, 0, SECTOR as usize).map_err(io)?;
    if mbr.len() < 512 || mbr[510..512] != [0x55, 0xaa] {
        return Err("no partition table (sector 0 has no MBR signature)".into());
    }
    let ptype = mbr[446 + 4];
    let (start, count) = (u32_at(&mbr, 446 + 8), u32_at(&mbr, 446 + 12));
    if ptype != 0 || start == 0 || count == 0 {
        return Err(format!("partition 1 is not a firmware partition (type 0x{ptype:02x}, start {start}, {count} sectors)"));
    }
    let base = start as u64 * SECTOR;
    let head = read_at(&mut f, base, 0x200).map_err(io)?;
    if head.len() < 0x200 || &head[..4] != b"{{~~" || &head[0x100..0x104] != b"]ih[" {
        return Err("the firmware partition is empty (no boot block or volume header)".into());
    }
    let ptr = u32_at(&head, 0x104) as u64;
    let fmt = u16::from_le_bytes([head[0x10a], head[0x10b]]);
    let d = read_at(&mut f, base + ptr + SECTOR, 10 * 40).map_err(io)?;
    let mut off = 0;
    while off + 40 <= d.len() {
        if d[off..off + 4] == [0, 0, 0, 0] {
            break;
        }
        let typ: Vec<u8> = d[off + 4..off + 8].iter().rev().copied().collect();
        if typ == b"osos" {
            let (dev, len, cksum) = (u32_at(&d, off + 12) as u64, u32_at(&d, off + 16) as usize, u32_at(&d, off + 28));
            let osos = read_at(&mut f, base + dev + if fmt == 3 { SECTOR } else { 0 }, len).map_err(io)?;
            if osos.len() != len {
                return Err("the osos runs past the end of the image".into());
            }
            let sum = osos.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32));
            if sum != cksum {
                return Err(format!("osos checksum mismatch: stored 0x{cksum:08x}, got 0x{sum:08x}"));
            }
            return Ok(osos);
        }
        off += 40;
    }
    Err("no osos in the firmware directory".into())
}
