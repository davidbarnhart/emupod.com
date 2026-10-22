//! Differential test driver: reads cases on stdin, runs one instruction of each on armcore and prints the
//! result, for tests/diff_unicorn.py to compare with Unicorn.
//!
//! A case is one line of hex numbers: `cpsr r0 ... r14 pc instr`. Memory is 64 KB of a fixed pattern
//! (`byte(a) = (a * 2654435761 >> 13) & 0xff`), the instruction is placed at `pc` (16 bits when the CPSR has the
//! Thumb bit). The answer is `r0 ... r14 pc cpsr crc32(0x2000..0x6000)`; unmapped accesses print `FAULT`.
use armcore::{Bus, Core, Exception};
use std::io::{BufRead, Write};

struct Mem {
    m: Vec<u8>,
    fault: bool,
    exc: Option<Exception>,
}
impl Mem {
    fn idx(&mut self, a: u32, n: u32) -> usize {
        if a as usize + n as usize > self.m.len() {
            self.fault = true;
            return 0;
        }
        a as usize
    }
}
impl Bus for Mem {
    fn read8(&mut self, a: u32) -> u8 {
        let i = self.idx(a, 1);
        self.m[i]
    }
    fn read16(&mut self, a: u32) -> u16 {
        let i = self.idx(a, 2);
        u16::from_le_bytes([self.m[i], self.m[i + 1]])
    }
    fn read32(&mut self, a: u32) -> u32 {
        let i = self.idx(a, 4);
        u32::from_le_bytes([self.m[i], self.m[i + 1], self.m[i + 2], self.m[i + 3]])
    }
    fn write8(&mut self, a: u32, v: u8) {
        let i = self.idx(a, 1);
        self.m[i] = v;
    }
    fn write16(&mut self, a: u32, v: u16) {
        let i = self.idx(a, 2);
        self.m[i..i + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write32(&mut self, a: u32, v: u32) {
        let i = self.idx(a, 4);
        self.m[i..i + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn exception(&mut self, k: Exception, _pc: u32) {
        self.exc = Some(k);
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut t = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        t[i as usize] = c;
    }
    let mut c = !0u32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

fn main() {
    let mut pattern = vec![0u8; 0x10000];
    for (a, p) in pattern.iter_mut().enumerate() {
        *p = ((a as u64 * 2654435761u64) >> 13) as u8;
    }
    let mut core: Core<Mem> = Core::new();
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::new(out.lock());
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let v: Vec<u32> = line.split_whitespace().map(|x| u32::from_str_radix(x, 16).unwrap()).collect();
        if v.len() < 18 {
            continue;
        }
        let mut mem = Mem { m: pattern.clone(), fault: false, exc: None };
        let (cpsr, pc, instr) = (v[0], v[16], v[17]);
        core.cpu.reset();
        core.cpu.cpsr = cpsr;
        core.cpu.r[..15].copy_from_slice(&v[1..16]);
        core.cpu.pc = pc;
        let p = pc as usize;
        if cpsr & 0x20 != 0 {
            mem.m[p..p + 2].copy_from_slice(&(instr as u16).to_le_bytes());
        } else {
            mem.m[p..p + 4].copy_from_slice(&instr.to_le_bytes());
        }
        core.run(&mut mem, 1);
        if mem.fault {
            writeln!(out, "FAULT").unwrap();
            continue;
        }
        let c = &core.cpu;
        for r in 0..15 {
            write!(out, "{:08x} ", c.r[r]).unwrap();
        }
        writeln!(out, "{:08x} {:08x} {:08x}", c.pc, c.cpsr, crc32(&mem.m[0x2000..0x6000])).unwrap();
    }
}
