//! The test program (tests/data/work.bin) on armcore with a flat 2 MB RAM: instructions per second, run in
//! turns of the sizes an emulator's scheduler uses. Compare `armv4t_emu` (12.7 MIPS), Unicorn (43) and mGBA's
//! core (65) on a Pi 4.
use armcore::{Bus, Core};
use std::time::Instant;

struct Ram {
    m: Box<[u8; 0x20_0000]>,
    halt: bool,
}
const MASK: u32 = 0x1f_ffff;
impl Bus for Ram {
    #[inline(always)]
    fn read8(&mut self, a: u32) -> u8 {
        self.m[(a & MASK) as usize]
    }
    #[inline(always)]
    fn read16(&mut self, a: u32) -> u16 {
        let i = (a & MASK) as usize;
        u16::from_le_bytes([self.m[i], self.m[i + 1]])
    }
    #[inline(always)]
    fn read32(&mut self, a: u32) -> u32 {
        let i = (a & MASK) as usize;
        u32::from_le_bytes([self.m[i], self.m[i + 1], self.m[i + 2], self.m[i + 3]])
    }
    #[inline(always)]
    fn write8(&mut self, a: u32, v: u8) {
        self.m[(a & MASK) as usize] = v;
    }
    #[inline(always)]
    fn write16(&mut self, a: u32, v: u16) {
        let i = (a & MASK) as usize;
        self.m[i..i + 2].copy_from_slice(&v.to_le_bytes());
    }
    #[inline(always)]
    fn write32(&mut self, a: u32, v: u32) {
        let i = (a & MASK) as usize;
        self.m[i..i + 4].copy_from_slice(&v.to_le_bytes());
    }
    #[inline(always)]
    fn fetch32(&mut self, a: u32) -> u32 {
        self.halt = a == 0x20; // the program ends in `1: b 1b` here
        self.read32(a)
    }
    #[inline(always)]
    fn stop(&self) -> bool {
        self.halt
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "tests/data/work.bin".into());
    let code = std::fs::read(path).expect("work.bin");
    for turn in [u64::MAX, 3900, 260] {
        let mut ram = Ram { m: vec![0u8; 0x20_0000].into_boxed_slice().try_into().unwrap(), halt: false };
        ram.m[..code.len()].copy_from_slice(&code);
        let mut core: Core<Ram> = Core::new();
        let t = Instant::now();
        let mut n = 0u64;
        while !ram.halt {
            n += core.run(&mut ram, turn);
        }
        let dt = t.elapsed().as_secs_f64();
        let res = u32::from_le_bytes(ram.m[0x277c..0x2780].try_into().unwrap());
        println!(
            "armcore, turns of {:>10}: {:.1} M instructions in {:.2} s = {:.1} MIPS, result {:08x}",
            if turn == u64::MAX { 0 } else { turn },
            n as f64 / 1e6,
            dt,
            n as f64 / dt / 1e6,
            res
        );
    }
}
