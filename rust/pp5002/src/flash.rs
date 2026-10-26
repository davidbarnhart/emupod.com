//! The iPod's 1 MB flash chip (Sharp, the Intel command set), at address 0 until the firmware maps DRAM there, and the chip image an
//! update file describes. A port of the flash model in openpod-gen1's tools/rom_rehearse.py, whose results matched the chip on hardware.
//!
//! The chip is bottom-boot: eight 8 KB blocks, then fifteen 64 KB blocks. It is 16 bits wide. Commands are written to any address in
//! the chip: 0xff read array, 0x70 read status, 0x90 read identifier, 0x98 query, 0x50 clear status, 0x20 then 0xd0 erase the block,
//! 0x40 or 0x10 then the word program it (programming only clears bits). Erasing and programming finish at once: the status reads
//! ready (0x80) at the first poll.

pub const SIZE: usize = 0x10_0000;
/// Sharp's manufacturer code, read at the chip's base in identifier mode
pub const SHARP: u8 = 0xb0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Array,
    Status,
    Id,
    Query,
}

pub struct Flash {
    /// the cells
    pub cells: Vec<u8>,
    mode: Mode,
    /// 0x20 (an erase waiting for its 0xd0) or 0x40 (a program waiting for its word)
    pending: Option<u8>,
    pub mfr: u8,
    /// (block start, size) of every erase, in order
    pub erases: Vec<(u32, u32)>,
    /// words programmed
    pub programs: u64,
    /// programs that asked for a 0 to become a 1 (the cell keeps its 0)
    pub bad_programs: u64,
    /// commands the chip does not know: (address, value)
    pub unknown: Vec<(u32, u32)>,
}

impl Flash {
    pub fn new(image: &[u8]) -> Flash {
        let mut cells = vec![0xffu8; SIZE];
        let n = image.len().min(SIZE);
        cells[..n].copy_from_slice(&image[..n]);
        Flash { cells, mode: Mode::Array, pending: None, mfr: SHARP, erases: Vec::new(), programs: 0, bad_programs: 0, unknown: Vec::new() }
    }

    /// The block holding offset `a`: (start, size).
    pub fn block(a: u32) -> (u32, u32) {
        if a < 0x1_0000 {
            (a & !0x1fff, 0x2000)
        } else {
            (a & !0xffff, 0x1_0000)
        }
    }

    /// Whether a read sees the cells (array mode), so code may be decoded from them and kept.
    pub fn array_mode(&self) -> bool {
        self.mode == Mode::Array
    }

    pub fn read(&mut self, a: u32, size: u32) -> u32 {
        let a = a & (SIZE as u32 - 1);
        let half = match self.mode {
            Mode::Array => {
                let c = &self.cells;
                let i = a as usize;
                let b = |k: usize| c[(i + k) & (SIZE - 1)] as u32;
                return match size {
                    1 => b(0),
                    2 => b(0) | b(1) << 8,
                    _ => b(0) | b(1) << 8 | b(2) << 16 | b(3) << 24,
                };
            }
            Mode::Status => 0x80,
            Mode::Id => {
                if a & 0xff == 0 {
                    self.mfr as u32
                } else {
                    0
                }
            }
            Mode::Query => 0,
        };
        // outside array mode every 16-bit read answers the same
        match size {
            1 => half & 0xff,
            2 => half,
            _ => half | half << 16,
        }
    }

    pub fn write(&mut self, a: u32, _size: u32, v: u32) {
        let a = a & (SIZE as u32 - 1) & !1;
        let v = v & 0xffff;
        match self.pending.take() {
            Some(0x40) => {
                let i = a as usize;
                let old = self.cells[i] as u32 | (self.cells[i + 1] as u32) << 8;
                let new = old & v;
                if new != v {
                    self.bad_programs += 1;
                }
                self.cells[i] = new as u8;
                self.cells[i + 1] = (new >> 8) as u8;
                self.programs += 1;
                self.mode = Mode::Status;
                return;
            }
            Some(_) => {
                if v & 0xff == 0xd0 {
                    let (b, n) = Flash::block(a);
                    self.cells[b as usize..(b + n) as usize].fill(0xff);
                    self.erases.push((b, n));
                } else {
                    self.unknown.push((a, v));
                }
                self.mode = Mode::Status;
                return;
            }
            None => {}
        }
        match v & 0xff {
            0xff => self.mode = Mode::Array,
            0x70 => self.mode = Mode::Status,
            0x90 => self.mode = Mode::Id,
            0x98 => self.mode = Mode::Query,
            0x50 => {}
            0x20 => self.pending = Some(0x20),
            0x40 | 0x10 => self.pending = Some(0x40),
            _ => self.unknown.push((a, v)),
        }
    }
}

/// The chip an update file's `aupd` image puts on an iPod: its `FwUp` records of type `flsh` (the boot block at 0, the flash image at
/// 0x4000) on an erased chip. Two things no update writes are filled in as on David's chips: 0x3000..0x4000 is zero, and the
/// per-device record at 0x2000 (78 bytes, different on each iPod) is left blank. No dump is used.
pub fn assemble(aupd: &[u8]) -> Result<Vec<u8>, String> {
    let mut chip = vec![0xffu8; SIZE];
    let mut placed = 0;
    let mut i = 0;
    while let Some(k) = aupd[i..].windows(4).position(|w| w == b"pUwF") {
        let h = i + k;
        i = h + 1;
        if h + 0x18 > aupd.len() {
            break;
        }
        let word = |o: usize| u32::from_le_bytes(aupd[h + o..h + o + 4].try_into().unwrap()) as usize;
        if word(4) != 0x18 || &aupd[h + 8..h + 12] != b"hslf" {
            continue;
        }
        let (size, dev) = (word(12), word(16));
        if dev + size > SIZE || h + 0x18 + size > aupd.len() {
            return Err(format!("a flsh record does not fit: {size:#x} bytes at device {dev:#x}"));
        }
        chip[dev..dev + size].copy_from_slice(&aupd[h + 0x18..h + 0x18 + size]);
        placed += 1;
    }
    if placed == 0 {
        return Err("no flsh records (an aupd from before 1.2 has no FwUp headers)".into());
    }
    chip[0x3000..0x4000].fill(0);
    Ok(chip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd16(f: &mut Flash, a: u32) -> u32 {
        f.read(a, 2)
    }

    #[test]
    fn array_reads_the_cells() {
        let mut img = vec![0u8; 16];
        img[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        let mut f = Flash::new(&img);
        assert_eq!(f.read(4, 4), 0x1234_5678);
        assert_eq!(f.read(6, 2), 0x1234);
        assert_eq!(f.read(7, 1), 0x12);
        assert_eq!(f.read(0x20, 4), 0xffff_ffff, "past the image the chip is erased");
    }

    #[test]
    fn identifier_and_status() {
        let mut f = Flash::new(&[]);
        f.write(0x5000, 2, 0x90);
        assert_eq!(rd16(&mut f, 0), SHARP as u32);
        assert_eq!(rd16(&mut f, 2), 0);
        f.write(0, 2, 0x70);
        assert_eq!(rd16(&mut f, 0x1234), 0x80);
        assert_eq!(f.read(0x1234, 4), 0x0080_0080);
        f.write(0, 2, 0xff);
        assert_eq!(rd16(&mut f, 0), 0xffff);
    }

    #[test]
    fn program_only_clears_bits() {
        let mut f = Flash::new(&[]);
        f.write(0x100, 2, 0x40);
        f.write(0x100, 2, 0x1234);
        assert_eq!(rd16(&mut f, 0x100), 0x80, "status after a program");
        f.write(0, 2, 0xff);
        assert_eq!(rd16(&mut f, 0x100), 0x1234);
        f.write(0x100, 2, 0x10);
        f.write(0x100, 2, 0x4321);
        f.write(0, 2, 0xff);
        assert_eq!(rd16(&mut f, 0x100), 0x1234 & 0x4321);
        assert_eq!((f.programs, f.bad_programs), (2, 1));
    }

    #[test]
    fn erase_blocks_bottom_boot() {
        let mut f = Flash::new(&vec![0u8; SIZE]);
        f.write(0x2002, 2, 0x20);
        f.write(0x2002, 2, 0xd0);
        f.write(0, 2, 0xff);
        assert_eq!(f.read(0x1ffe, 2), 0);
        assert_eq!(f.read(0x2000, 4), 0xffff_ffff);
        assert_eq!(f.read(0x3ffe, 2), 0xffff);
        assert_eq!(f.read(0x4000, 2), 0, "an 8 KB block below 64 KB");
        f.write(0x2_1234, 2, 0x20);
        f.write(0x2_1234, 2, 0xd0);
        f.write(0, 2, 0xff);
        assert_eq!((f.read(0x1_fffe, 2), f.read(0x2_0000, 2), f.read(0x2_fffe, 2), f.read(0x3_0000, 2)), (0, 0xffff, 0xffff, 0));
        assert_eq!(f.erases, vec![(0x2000, 0x2000), (0x2_0000, 0x1_0000)]);
    }

    #[test]
    fn unconfirmed_erase_changes_nothing() {
        let mut f = Flash::new(&vec![0u8; SIZE]);
        f.write(0x8000, 2, 0x20);
        f.write(0x8000, 2, 0xff);
        f.write(0, 2, 0xff);
        assert_eq!(f.read(0x8000, 2), 0);
        assert!(f.erases.is_empty());
        assert_eq!(f.unknown.len(), 1);
    }

    #[test]
    fn assemble_places_the_flsh_records() {
        let mut a = vec![0u8; 0x40];
        let mut rec = |dev: u32, data: &[u8]| {
            a.extend_from_slice(b"pUwF");
            a.extend_from_slice(&0x18u32.to_le_bytes());
            a.extend_from_slice(b"hslf");
            a.extend_from_slice(&(data.len() as u32).to_le_bytes());
            a.extend_from_slice(&dev.to_le_bytes());
            a.extend_from_slice(&0u32.to_le_bytes());
            a.extend_from_slice(data);
        };
        rec(0, &[1, 2, 3, 4]);
        rec(0x4000, &[5, 6]);
        let chip = assemble(&a).unwrap();
        assert_eq!(&chip[..5], &[1, 2, 3, 4, 0xff]);
        assert_eq!(&chip[0x4000..0x4003], &[5, 6, 0xff]);
        assert!(chip[0x3000..0x4000].iter().all(|&b| b == 0));
        assert!(chip[0x2000..0x3000].iter().all(|&b| b == 0xff));
        assert!(assemble(&[0u8; 64]).is_err());
    }

    /// With EMUPOD_AUPD (an aupd.bin from 1.5) and EMUPOD_CHIP_REF (a chip after the rehearsed 1.5 update): the assembled chip is that
    /// chip everywhere but the per-device record at 0x2000..0x204e.
    #[test]
    fn assemble_matches_a_real_chip() {
        let (Ok(a), Ok(r)) = (std::env::var("EMUPOD_AUPD"), std::env::var("EMUPOD_CHIP_REF")) else { return };
        let chip = assemble(&std::fs::read(a).unwrap()).unwrap();
        let real = std::fs::read(r).unwrap();
        let diff: Vec<usize> = (0..SIZE).filter(|&k| chip[k] != real[k]).collect();
        assert!(diff.iter().all(|&k| (0x2000..0x204e).contains(&k)), "differs outside the device record: first {:#x}", diff.iter().find(|&&k| !(0x2000..0x204e).contains(&k)).unwrap());
        assert!(!diff.is_empty(), "the reference has its device record");
    }
}
