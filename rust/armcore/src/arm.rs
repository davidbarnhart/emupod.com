//! The ARM (32-bit) instruction set of the ARM7TDMI: data processing, multiplies, single and block transfers,
//! halfword and signed transfers, SWP, PSR transfers, branches, BX, SWI.

use crate::{mode_index, ArmFn, Bus, Cpu, Exception, BIT_T, MODE_SVC, MODE_UND};

/// `a + b + cin` with the carry out and the signed overflow.
#[inline(always)]
pub(crate) fn add_c(a: u32, b: u32, cin: u32) -> (u32, u32, u32) {
    let s = a as u64 + b as u64 + cin as u64;
    let r = s as u32;
    (r, (s >> 32) as u32, ((!(a ^ b) & (a ^ r)) >> 31) as u32)
}

impl Cpu {
    /// Operand 2 of a data-processing instruction given as a (shifted) register: the value and the shifter's
    /// carry out.
    #[inline(always)]
    fn shifter(&self, i: u32) -> (u32, u32) {
        let rm = (i & 15) as usize;
        let ty = (i >> 5) & 3;
        if i & 0x10 == 0 {
            let amt = (i >> 7) & 31;
            let v = self.r[rm];
            match ty {
                0 => {
                    if amt == 0 {
                        (v, self.cflag())
                    } else {
                        (v << amt, (v >> (32 - amt)) & 1)
                    }
                }
                1 => {
                    if amt == 0 {
                        (0, v >> 31)
                    } else {
                        (v >> amt, (v >> (amt - 1)) & 1)
                    }
                }
                2 => {
                    if amt == 0 {
                        (((v as i32) >> 31) as u32, v >> 31)
                    } else {
                        (((v as i32) >> amt) as u32, (v >> (amt - 1)) & 1)
                    }
                }
                _ => {
                    if amt == 0 {
                        ((self.cflag() << 31) | (v >> 1), v & 1)
                    } else {
                        (v.rotate_right(amt), (v >> (amt - 1)) & 1)
                    }
                }
            }
        } else {
            // shift by register: the shifted register reads as PC+12 if it is PC
            let amt = self.r[((i >> 8) & 15) as usize] & 0xff;
            let v = if rm == 15 { self.r[15].wrapping_add(4) } else { self.r[rm] };
            if amt == 0 {
                return (v, self.cflag());
            }
            match ty {
                0 => {
                    if amt < 32 {
                        (v << amt, (v >> (32 - amt)) & 1)
                    } else if amt == 32 {
                        (0, v & 1)
                    } else {
                        (0, 0)
                    }
                }
                1 => {
                    if amt < 32 {
                        (v >> amt, (v >> (amt - 1)) & 1)
                    } else if amt == 32 {
                        (0, v >> 31)
                    } else {
                        (0, 0)
                    }
                }
                2 => {
                    if amt < 32 {
                        (((v as i32) >> amt) as u32, (v >> (amt - 1)) & 1)
                    } else {
                        (((v as i32) >> 31) as u32, v >> 31)
                    }
                }
                _ => {
                    let a = amt & 31;
                    if a == 0 {
                        (v, v >> 31)
                    } else {
                        (v.rotate_right(a), (v >> (a - 1)) & 1)
                    }
                }
            }
        }
    }

    /// The offset register of a single transfer: Rm shifted by an immediate (no carry needed).
    #[inline(always)]
    fn shifted_offset(&self, i: u32) -> u32 {
        let v = self.r[(i & 15) as usize];
        let amt = (i >> 7) & 31;
        match (i >> 5) & 3 {
            0 => v << amt,
            1 => {
                if amt == 0 {
                    0
                } else {
                    v >> amt
                }
            }
            2 => {
                if amt == 0 {
                    ((v as i32) >> 31) as u32
                } else {
                    ((v as i32) >> amt) as u32
                }
            }
            _ => {
                if amt == 0 {
                    (self.cflag() << 31) | (v >> 1)
                } else {
                    v.rotate_right(amt)
                }
            }
        }
    }

    /// An exception taken by an instruction (SWI, undefined): LR is the next instruction.
    pub(crate) fn instr_exception<B: Bus>(&mut self, bus: &mut B, kind: Exception) {
        let pc = self.pc;
        bus.exception(kind, pc);
        match kind {
            Exception::Swi => self.enter(MODE_SVC, 0x08, self.next, false),
            _ => self.enter(MODE_UND, 0x04, self.next, false),
        }
    }

    /// MSR: write the fields `fields` (bits 19:16 of the instruction) of CPSR or SPSR.
    pub(crate) fn msr(&mut self, spsr: bool, fields: u32, v: u32) {
        let mut mask = 0u32;
        if fields & 8 != 0 {
            mask |= 0xff00_0000;
        }
        if fields & 4 != 0 {
            mask |= 0x00ff_0000;
        }
        if fields & 2 != 0 {
            mask |= 0x0000_ff00;
        }
        if fields & 1 != 0 {
            mask |= 0x0000_00ff;
        }
        let m = mode_index(self.cpsr);
        if spsr {
            if m != 0 {
                self.spsr[m] = (self.spsr[m] & !mask) | (v & mask);
            }
        } else {
            if self.cpsr & 0x1f == crate::MODE_USR {
                mask &= 0xf000_0000; // User mode changes the flags only
            }
            mask &= 0xf000_00df; // the ARM7TDMI's PSR is N Z C V, I F and the mode; the Thumb bit is not writable by MSR
            let n = (self.cpsr & !mask) | (v & mask);
            self.set_cpsr(n);
        }
    }
}

// ---------------------------------------------------------------------------------------------- data processing

fn dp<B: Bus, const OP: u32, const S: bool, const IMM: bool>(c: &mut Cpu, _b: &mut B, i: u32) {
    let rd = ((i >> 12) & 15) as usize;
    let rn = ((i >> 16) & 15) as usize;
    let logical = matches!(OP, 0 | 1 | 8 | 9 | 12 | 13 | 14 | 15);
    let (op2, sc) = if IMM {
        let rot = (i >> 7) & 0x1e;
        let v = (i & 0xff).rotate_right(rot);
        (v, if rot == 0 { c.cflag() } else { v >> 31 })
    } else {
        c.shifter(i)
    };
    let a = if rn == 15 && !IMM && i & 0x10 != 0 { c.r[15].wrapping_add(4) } else { c.r[rn] };
    let cin = c.cflag();
    let (res, cc, vv) = match OP {
        0 | 8 => (a & op2, sc, 0),
        1 | 9 => (a ^ op2, sc, 0),
        2 | 10 => add_c(a, !op2, 1),
        3 => add_c(op2, !a, 1),
        4 | 11 => add_c(a, op2, 0),
        5 => add_c(a, op2, cin),
        6 => add_c(a, !op2, cin),
        7 => add_c(op2, !a, cin),
        12 => (a | op2, sc, 0),
        13 => (op2, sc, 0),
        14 => (a & !op2, sc, 0),
        _ => (!op2, sc, 0),
    };
    let writes = !matches!(OP, 8..=11);
    if S {
        if rd == 15 && writes {
            // an exception return: CPSR comes from the SPSR
            let sp = c.current_spsr();
            c.set_cpsr(sp);
        } else if logical {
            c.set_nz(res);
            c.set_c(cc);
        } else {
            c.set_nzcv(res, cc, vv);
        }
    }
    if writes {
        if rd == 15 {
            c.jump(res);
        } else {
            c.r[rd] = res;
        }
    }
}

fn dp_fn<B: Bus>(op: u32, s: bool, imm: bool) -> ArmFn<B> {
    macro_rules! pick {
        ($($op:literal),*) => {
            match (op, s, imm) {
                $(
                    ($op, false, false) => dp::<B, $op, false, false> as ArmFn<B>,
                    ($op, false, true) => dp::<B, $op, false, true>,
                    ($op, true, false) => dp::<B, $op, true, false>,
                    ($op, true, true) => dp::<B, $op, true, true>,
                )*
                _ => undef::<B>,
            }
        };
    }
    pick!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15)
}

// ---------------------------------------------------------------------------------------------- multiplies

fn mul<B: Bus, const ACC: bool, const S: bool>(c: &mut Cpu, _b: &mut B, i: u32) {
    let rd = ((i >> 16) & 15) as usize;
    let rn = ((i >> 12) & 15) as usize;
    let rs = ((i >> 8) & 15) as usize;
    let rm = (i & 15) as usize;
    let mut r = c.r[rm].wrapping_mul(c.r[rs]);
    if ACC {
        r = r.wrapping_add(c.r[rn]);
    }
    if S {
        c.set_nz(r);
    }
    c.r[rd] = r;
}

fn mull<B: Bus, const SIGNED: bool, const ACC: bool, const S: bool>(c: &mut Cpu, _b: &mut B, i: u32) {
    let hi = ((i >> 16) & 15) as usize;
    let lo = ((i >> 12) & 15) as usize;
    let rs = ((i >> 8) & 15) as usize;
    let rm = (i & 15) as usize;
    let mut r: u64 = if SIGNED {
        ((c.r[rm] as i32 as i64) * (c.r[rs] as i32 as i64)) as u64
    } else {
        (c.r[rm] as u64) * (c.r[rs] as u64)
    };
    if ACC {
        r = r.wrapping_add(((c.r[hi] as u64) << 32) | c.r[lo] as u64);
    }
    if S {
        c.cpsr = (c.cpsr & 0x3fff_ffff) | (((r >> 63) as u32) << 31) | (((r == 0) as u32) << 30);
    }
    c.r[lo] = r as u32;
    c.r[hi] = (r >> 32) as u32;
}

// ---------------------------------------------------------------------------------------------- single transfers

fn ldst<B: Bus, const L: bool, const BYTE: bool, const REG: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let rn = ((i >> 16) & 15) as usize;
    let rd = ((i >> 12) & 15) as usize;
    let pre = i & (1 << 24) != 0;
    let up = i & (1 << 23) != 0;
    let wb = !pre || i & (1 << 21) != 0;
    let off = if REG { c.shifted_offset(i) } else { i & 0xfff };
    let base = c.r[rn];
    let moved = if up { base.wrapping_add(off) } else { base.wrapping_sub(off) };
    let addr = if pre { moved } else { base };
    if L {
        let v = if BYTE {
            b.read8(addr) as u32
        } else {
            b.read32_a(addr).rotate_right((addr & 3) * 8)
        };
        if wb {
            c.r[rn] = moved;
        }
        if rd == 15 {
            c.jump(v);
        } else {
            c.r[rd] = v;
        }
    } else {
        let v = if rd == 15 { c.r[15].wrapping_add(4) } else { c.r[rd] };
        if BYTE {
            b.write8(addr, v as u8);
        } else {
            b.write32_a(addr, v);
        }
        if wb {
            c.r[rn] = moved;
        }
    }
}

/// LDRH, STRH, LDRSB, LDRSH.
fn ldsth<B: Bus, const L: bool, const IMM: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let rn = ((i >> 16) & 15) as usize;
    let rd = ((i >> 12) & 15) as usize;
    let pre = i & (1 << 24) != 0;
    let up = i & (1 << 23) != 0;
    let wb = !pre || i & (1 << 21) != 0;
    let off = if IMM { ((i >> 4) & 0xf0) | (i & 0xf) } else { c.r[(i & 15) as usize] };
    let base = c.r[rn];
    let moved = if up { base.wrapping_add(off) } else { base.wrapping_sub(off) };
    let addr = if pre { moved } else { base };
    let sh = (i >> 5) & 3;
    if L {
        let v = match sh {
            1 => {
                let h = b.read16_a(addr) as u32;
                if addr & 1 != 0 {
                    h.rotate_right(8)
                } else {
                    h
                }
            }
            2 => b.read8(addr) as i8 as i32 as u32,
            _ => {
                if addr & 1 != 0 {
                    b.misaligned(addr, 2);
                    b.read8(addr) as i8 as i32 as u32
                } else {
                    b.read16(addr) as i16 as i32 as u32
                }
            }
        };
        if wb {
            c.r[rn] = moved;
        }
        if rd == 15 {
            c.jump(v);
        } else {
            c.r[rd] = v;
        }
    } else {
        let v = if rd == 15 { c.r[15].wrapping_add(4) } else { c.r[rd] };
        b.write16_a(addr, v as u16);
        if wb {
            c.r[rn] = moved;
        }
    }
}

// ---------------------------------------------------------------------------------------------- block transfers

fn block<B: Bus, const L: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let rn = ((i >> 16) & 15) as usize;
    let pre = i & (1 << 24) != 0;
    let up = i & (1 << 23) != 0;
    let s = i & (1 << 22) != 0;
    let wb = i & (1 << 21) != 0;
    let mut list = i & 0xffff;
    let base = c.r[rn];
    // an empty list transfers PC alone, and moves the base by 0x40 as if all sixteen had gone
    let (n, total) = if list == 0 {
        list = 0x8000;
        (1u32, 0x40u32)
    } else {
        let n = list.count_ones();
        (n, n * 4)
    };
    let (mut addr, new_base) = if up {
        (if pre { base.wrapping_add(4) } else { base }, base.wrapping_add(total))
    } else {
        (if pre { base.wrapping_sub(total) } else { base.wrapping_sub(total).wrapping_add(4) }, base.wrapping_sub(total))
    };
    let _ = n;
    let user = s && !(L && list & 0x8000 != 0);
    let first = list.trailing_zeros() as usize;
    if L {
        if wb {
            c.r[rn] = new_base; // a loaded base register overrides this
        }
        for r in 0..16usize {
            if list & (1 << r) == 0 {
                continue;
            }
            let v = b.read32_a(addr);
            addr = addr.wrapping_add(4);
            if user {
                c.set_user_reg(r, v);
            } else if r == 15 {
                if s {
                    let sp = c.current_spsr();
                    c.set_cpsr(sp);
                }
                c.jump(v);
            } else {
                c.r[r] = v;
            }
        }
    } else {
        for r in 0..16usize {
            if list & (1 << r) == 0 {
                continue;
            }
            let v = if r == 15 {
                c.r[15].wrapping_add(4)
            } else if user {
                c.user_reg(r)
            } else if r == rn && wb && r != first {
                new_base
            } else {
                c.r[r]
            };
            b.write32_a(addr, v);
            addr = addr.wrapping_add(4);
        }
        if wb {
            c.r[rn] = new_base;
        }
    }
}

fn swp<B: Bus, const BYTE: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let rn = ((i >> 16) & 15) as usize;
    let rd = ((i >> 12) & 15) as usize;
    let rm = (i & 15) as usize;
    let addr = c.r[rn];
    let src = c.r[rm];
    if BYTE {
        let t = b.read8(addr) as u32;
        b.write8(addr, src as u8);
        c.r[rd] = t;
    } else {
        let t = b.read32_a(addr).rotate_right((addr & 3) * 8);
        b.write32_a(addr, src);
        c.r[rd] = t;
    }
}

// ---------------------------------------------------------------------------------------------- PSR, branches

fn mrs<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let rd = ((i >> 12) & 15) as usize;
    c.r[rd] = if i & (1 << 22) != 0 { c.current_spsr() } else { c.cpsr };
}

fn msr_reg<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let v = c.r[(i & 15) as usize];
    c.msr(i & (1 << 22) != 0, (i >> 16) & 15, v);
}

fn msr_imm<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let v = (i & 0xff).rotate_right((i >> 7) & 0x1e);
    c.msr(i & (1 << 22) != 0, (i >> 16) & 15, v);
}

fn branch<B: Bus, const LINK: bool>(c: &mut Cpu, _b: &mut B, i: u32) {
    let off = (((i << 8) as i32) >> 6) as u32;
    if LINK {
        c.r[14] = c.next;
    }
    c.next = c.r[15].wrapping_add(off);
}

fn bx<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let t = c.r[(i & 15) as usize];
    if t & 1 != 0 {
        c.cpsr |= BIT_T;
        c.next = t & !1;
    } else {
        c.cpsr &= !BIT_T;
        c.next = t & !3;
    }
}

fn swi<B: Bus>(c: &mut Cpu, b: &mut B, _i: u32) {
    c.instr_exception(b, Exception::Swi);
}

pub(crate) fn undef<B: Bus>(c: &mut Cpu, b: &mut B, _i: u32) {
    c.instr_exception(b, Exception::Undefined);
}

// ---------------------------------------------------------------------------------------------- decode

/// The handler for the instruction whose bits 27:20 are `hi` and bits 7:4 are `lo`.
fn decode<B: Bus>(hi: u32, lo: u32) -> ArmFn<B> {
    let s = hi & 1 != 0;
    let l = hi & 1 != 0;
    match hi >> 5 {
        0 => {
            if lo & 9 == 9 {
                // bit 7 and bit 4 set: multiplies, SWP, halfword transfers
                if lo == 9 {
                    let acc = hi & 2 != 0;
                    if hi >> 2 == 0 {
                        return match (acc, s) {
                            (false, false) => mul::<B, false, false>,
                            (false, true) => mul::<B, false, true>,
                            (true, false) => mul::<B, true, false>,
                            (true, true) => mul::<B, true, true>,
                        };
                    }
                    if hi >> 3 == 1 {
                        let signed = hi & 4 != 0;
                        return match (signed, acc, s) {
                            (false, false, false) => mull::<B, false, false, false>,
                            (false, false, true) => mull::<B, false, false, true>,
                            (false, true, false) => mull::<B, false, true, false>,
                            (false, true, true) => mull::<B, false, true, true>,
                            (true, false, false) => mull::<B, true, false, false>,
                            (true, false, true) => mull::<B, true, false, true>,
                            (true, true, false) => mull::<B, true, true, false>,
                            (true, true, true) => mull::<B, true, true, true>,
                        };
                    }
                    if hi == 0x10 {
                        return swp::<B, false>;
                    }
                    if hi == 0x14 {
                        return swp::<B, true>;
                    }
                    return undef::<B>;
                }
                // halfword and signed transfers; the store forms of the signed ones (LDRD/STRD) do not exist
                if !l && (lo >> 1) & 3 != 1 {
                    return undef::<B>;
                }
                let imm = hi & 4 != 0;
                return match (l, imm) {
                    (false, false) => ldsth::<B, false, false>,
                    (false, true) => ldsth::<B, false, true>,
                    (true, false) => ldsth::<B, true, false>,
                    (true, true) => ldsth::<B, true, true>,
                };
            }
            if hi == 0x12 && lo == 1 {
                return bx::<B>;
            }
            if lo == 0 && hi & 0xfb == 0x10 {
                return mrs::<B>;
            }
            if lo == 0 && hi & 0xfb == 0x12 {
                return msr_reg::<B>;
            }
            let op = (hi >> 1) & 15;
            if (8..=11).contains(&op) && !s {
                return undef::<B>;
            }
            dp_fn::<B>(op, s, false)
        }
        1 => {
            let op = (hi >> 1) & 15;
            if (8..=11).contains(&op) && !s {
                return if hi & 0xfb == 0x32 { msr_imm::<B> } else { undef::<B> };
            }
            dp_fn::<B>(op, s, true)
        }
        2 | 3 => {
            let reg = hi >> 5 == 3;
            if reg && lo & 1 != 0 {
                return undef::<B>;
            }
            let byte = hi & 4 != 0;
            match (l, byte, reg) {
                (false, false, false) => ldst::<B, false, false, false>,
                (false, false, true) => ldst::<B, false, false, true>,
                (false, true, false) => ldst::<B, false, true, false>,
                (false, true, true) => ldst::<B, false, true, true>,
                (true, false, false) => ldst::<B, true, false, false>,
                (true, false, true) => ldst::<B, true, false, true>,
                (true, true, false) => ldst::<B, true, true, false>,
                (true, true, true) => ldst::<B, true, true, true>,
            }
        }
        4 => {
            if l {
                block::<B, true>
            } else {
                block::<B, false>
            }
        }
        5 => {
            if hi & 0x10 != 0 {
                branch::<B, true>
            } else {
                branch::<B, false>
            }
        }
        7 if hi & 0x10 != 0 => swi::<B>,
        _ => undef::<B>, // coprocessor instructions: the PP5002 has none
    }
}

pub(crate) fn build_table<B: Bus>() -> Box<[ArmFn<B>; 4096]> {
    let mut t: Box<[ArmFn<B>; 4096]> = Box::new([undef::<B> as ArmFn<B>; 4096]);
    for idx in 0..4096usize {
        t[idx] = decode::<B>((idx >> 4) as u32, (idx & 15) as u32);
    }
    t
}
