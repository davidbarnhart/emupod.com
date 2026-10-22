//! The Thumb (16-bit) instruction set of the ARM7TDMI, formats 1 to 19.
//!
//! In Thumb state `r[15]` reads as the instruction's address plus 4; `next` is the address plus 2.

use crate::arm::{add_c, undef};
use crate::{Bus, Cpu, Exception, ThumbFn};

#[inline(always)]
fn lo(i: u32, shift: u32) -> usize {
    ((i >> shift) & 7) as usize
}

/// Move shifted register: LSL, LSR, ASR by an immediate (format 1).
fn shift_imm<B: Bus, const OP: u32>(c: &mut Cpu, _b: &mut B, i: u32) {
    let amt = (i >> 6) & 31;
    let v = c.r[lo(i, 3)];
    let (r, cc) = match OP {
        0 => {
            if amt == 0 {
                (v, c.cflag())
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
        _ => {
            if amt == 0 {
                (((v as i32) >> 31) as u32, v >> 31)
            } else {
                (((v as i32) >> amt) as u32, (v >> (amt - 1)) & 1)
            }
        }
    };
    c.set_nz(r);
    c.set_c(cc);
    c.r[lo(i, 0)] = r;
}

/// ADD / SUB with a register or a 3-bit immediate (format 2).
fn addsub<B: Bus, const SUB: bool, const IMM: bool>(c: &mut Cpu, _b: &mut B, i: u32) {
    let a = c.r[lo(i, 3)];
    let x = if IMM { (i >> 6) & 7 } else { c.r[lo(i, 6)] };
    let (r, cc, v) = if SUB { add_c(a, !x, 1) } else { add_c(a, x, 0) };
    c.set_nzcv(r, cc, v);
    c.r[lo(i, 0)] = r;
}

/// MOV, CMP, ADD, SUB with an 8-bit immediate (format 3).
fn imm8<B: Bus, const OP: u32>(c: &mut Cpu, _b: &mut B, i: u32) {
    let rd = lo(i, 8);
    let imm = i & 0xff;
    let a = c.r[rd];
    match OP {
        0 => {
            c.set_nz(imm);
            c.r[rd] = imm;
        }
        1 => {
            let (r, cc, v) = add_c(a, !imm, 1);
            c.set_nzcv(r, cc, v);
        }
        2 => {
            let (r, cc, v) = add_c(a, imm, 0);
            c.set_nzcv(r, cc, v);
            c.r[rd] = r;
        }
        _ => {
            let (r, cc, v) = add_c(a, !imm, 1);
            c.set_nzcv(r, cc, v);
            c.r[rd] = r;
        }
    }
}

/// The sixteen ALU operations (format 4).
fn alu<B: Bus, const OP: u32>(c: &mut Cpu, _b: &mut B, i: u32) {
    let rd = lo(i, 0);
    let a = c.r[rd];
    let b = c.r[lo(i, 3)];
    let cin = c.cflag();
    match OP {
        0 => {
            let r = a & b;
            c.set_nz(r);
            c.r[rd] = r;
        }
        1 => {
            let r = a ^ b;
            c.set_nz(r);
            c.r[rd] = r;
        }
        2 | 3 | 4 | 7 => {
            // LSL, LSR, ASR, ROR by register
            let amt = b & 0xff;
            let (r, cc) = if amt == 0 {
                (a, cin)
            } else {
                match OP {
                    2 => {
                        if amt < 32 {
                            (a << amt, (a >> (32 - amt)) & 1)
                        } else if amt == 32 {
                            (0, a & 1)
                        } else {
                            (0, 0)
                        }
                    }
                    3 => {
                        if amt < 32 {
                            (a >> amt, (a >> (amt - 1)) & 1)
                        } else if amt == 32 {
                            (0, a >> 31)
                        } else {
                            (0, 0)
                        }
                    }
                    4 => {
                        if amt < 32 {
                            (((a as i32) >> amt) as u32, (a >> (amt - 1)) & 1)
                        } else {
                            (((a as i32) >> 31) as u32, a >> 31)
                        }
                    }
                    _ => {
                        let n = amt & 31;
                        if n == 0 {
                            (a, a >> 31)
                        } else {
                            (a.rotate_right(n), (a >> (n - 1)) & 1)
                        }
                    }
                }
            };
            c.set_nz(r);
            c.set_c(cc);
            c.r[rd] = r;
        }
        5 => {
            let (r, cc, v) = add_c(a, b, cin);
            c.set_nzcv(r, cc, v);
            c.r[rd] = r;
        }
        6 => {
            let (r, cc, v) = add_c(a, !b, cin);
            c.set_nzcv(r, cc, v);
            c.r[rd] = r;
        }
        8 => c.set_nz(a & b),
        9 => {
            let (r, cc, v) = add_c(0, !b, 1);
            c.set_nzcv(r, cc, v);
            c.r[rd] = r;
        }
        10 => {
            let (r, cc, v) = add_c(a, !b, 1);
            c.set_nzcv(r, cc, v);
        }
        11 => {
            let (r, cc, v) = add_c(a, b, 0);
            c.set_nzcv(r, cc, v);
        }
        12 => {
            let r = a | b;
            c.set_nz(r);
            c.r[rd] = r;
        }
        13 => {
            let r = a.wrapping_mul(b);
            c.set_nz(r);
            c.r[rd] = r;
        }
        14 => {
            let r = a & !b;
            c.set_nz(r);
            c.r[rd] = r;
        }
        _ => {
            let r = !b;
            c.set_nz(r);
            c.r[rd] = r;
        }
    }
}

/// ADD, CMP, MOV on high registers, and BX (format 5).
fn hireg<B: Bus, const OP: u32>(c: &mut Cpu, _b: &mut B, i: u32) {
    let rd = (i & 7) as usize | ((i >> 4) & 8) as usize;
    let rs = ((i >> 3) & 15) as usize;
    let s = c.r[rs];
    match OP {
        0 => {
            let r = c.r[rd].wrapping_add(s);
            if rd == 15 {
                c.jump(r);
            } else {
                c.r[rd] = r;
            }
        }
        1 => {
            let (r, cc, v) = add_c(c.r[rd], !s, 1);
            c.set_nzcv(r, cc, v);
        }
        2 => {
            if rd == 15 {
                c.jump(s);
            } else {
                c.r[rd] = s;
            }
        }
        _ => {
            if s & 1 != 0 {
                c.next = s & !1;
            } else {
                c.cpsr &= !crate::BIT_T;
                c.next = s & !3;
            }
        }
    }
}

/// PC-relative load (format 6).
fn ldr_pc<B: Bus>(c: &mut Cpu, b: &mut B, i: u32) {
    let addr = (c.r[15] & !3).wrapping_add((i & 0xff) << 2);
    c.r[lo(i, 8)] = b.read32(addr);
}

/// Load/store with a register offset (format 7).
fn ldst_reg<B: Bus, const L: bool, const BYTE: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let addr = c.r[lo(i, 3)].wrapping_add(c.r[lo(i, 6)]);
    let rd = lo(i, 0);
    if L {
        c.r[rd] = if BYTE { b.read8(addr) as u32 } else { b.read32_a(addr).rotate_right((addr & 3) * 8) };
    } else if BYTE {
        b.write8(addr, c.r[rd] as u8);
    } else {
        b.write32_a(addr, c.r[rd]);
    }
}

/// STRH, LDSB, LDRH, LDSH with a register offset (format 8).
fn ldst_sign<B: Bus, const OP: u32>(c: &mut Cpu, b: &mut B, i: u32) {
    let addr = c.r[lo(i, 3)].wrapping_add(c.r[lo(i, 6)]);
    let rd = lo(i, 0);
    match OP {
        0 => b.write16_a(addr, c.r[rd] as u16),
        1 => c.r[rd] = b.read8(addr) as i8 as i32 as u32,
        2 => {
            let h = b.read16_a(addr) as u32;
            c.r[rd] = if addr & 1 != 0 { h.rotate_right(8) } else { h };
        }
        _ => {
            c.r[rd] = if addr & 1 != 0 {
                b.read8(addr) as i8 as i32 as u32
            } else {
                b.read16(addr) as i16 as i32 as u32
            };
        }
    }
}

/// Load/store word or byte with a 5-bit immediate offset (format 9).
fn ldst_imm<B: Bus, const L: bool, const BYTE: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let off = (i >> 6) & 31;
    let rd = lo(i, 0);
    let base = c.r[lo(i, 3)];
    if BYTE {
        let addr = base.wrapping_add(off);
        if L {
            c.r[rd] = b.read8(addr) as u32;
        } else {
            b.write8(addr, c.r[rd] as u8);
        }
    } else {
        let addr = base.wrapping_add(off << 2);
        if L {
            c.r[rd] = b.read32_a(addr).rotate_right((addr & 3) * 8);
        } else {
            b.write32_a(addr, c.r[rd]);
        }
    }
}

/// Load/store halfword with a 5-bit immediate offset (format 10).
fn ldst_half<B: Bus, const L: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let addr = c.r[lo(i, 3)].wrapping_add(((i >> 6) & 31) << 1);
    let rd = lo(i, 0);
    if L {
        let h = b.read16_a(addr) as u32;
        c.r[rd] = if addr & 1 != 0 { h.rotate_right(8) } else { h };
    } else {
        b.write16_a(addr, c.r[rd] as u16);
    }
}

/// SP-relative load/store (format 11).
fn ldst_sp<B: Bus, const L: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let addr = c.r[13].wrapping_add((i & 0xff) << 2);
    let rd = lo(i, 8);
    if L {
        c.r[rd] = b.read32_a(addr).rotate_right((addr & 3) * 8);
    } else {
        b.write32_a(addr, c.r[rd]);
    }
}

/// ADD Rd, PC/SP, #imm (format 12).
fn add_addr<B: Bus, const SP: bool>(c: &mut Cpu, _b: &mut B, i: u32) {
    let base = if SP { c.r[13] } else { c.r[15] & !3 };
    c.r[lo(i, 8)] = base.wrapping_add((i & 0xff) << 2);
}

/// ADD SP, #+/-imm (format 13).
fn add_sp<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let off = (i & 0x7f) << 2;
    c.r[13] = if i & 0x80 != 0 { c.r[13].wrapping_sub(off) } else { c.r[13].wrapping_add(off) };
}

/// PUSH {rlist, LR} (format 14).
fn push<B: Bus>(c: &mut Cpu, b: &mut B, i: u32) {
    let list = i & 0xff;
    let extra = i & 0x100 != 0;
    let n = list.count_ones() + extra as u32;
    let mut addr = c.r[13].wrapping_sub(n * 4);
    c.r[13] = addr;
    for r in 0..8 {
        if list & (1 << r) != 0 {
            b.write32_a(addr, c.r[r]);
            addr = addr.wrapping_add(4);
        }
    }
    if extra {
        b.write32_a(addr, c.r[14]);
    }
}

/// POP {rlist, PC} (format 14).
fn pop<B: Bus>(c: &mut Cpu, b: &mut B, i: u32) {
    let list = i & 0xff;
    let extra = i & 0x100 != 0;
    let mut addr = c.r[13];
    for r in 0..8 {
        if list & (1 << r) != 0 {
            c.r[r] = b.read32_a(addr);
            addr = addr.wrapping_add(4);
        }
    }
    if extra {
        let v = b.read32_a(addr);
        addr = addr.wrapping_add(4);
        c.r[13] = addr;
        c.jump(v);
    } else {
        c.r[13] = addr;
    }
}

/// STMIA / LDMIA Rb!, {rlist} (format 15).
fn block<B: Bus, const L: bool>(c: &mut Cpu, b: &mut B, i: u32) {
    let rb = lo(i, 8);
    let list = i & 0xff;
    let base = c.r[rb];
    let (n, empty) = if list == 0 { (16u32, true) } else { (list.count_ones(), false) };
    let mut addr = base;
    let new_base = base.wrapping_add(n * 4);
    let first = list.trailing_zeros() as usize;
    if empty {
        // an empty list transfers PC alone
        if L {
            let v = b.read32_a(addr);
            c.jump(v);
        } else {
            b.write32_a(addr, c.r[15].wrapping_add(2));
        }
        c.r[rb] = new_base;
        return;
    }
    if L {
        c.r[rb] = new_base; // a loaded base register overrides this
        for r in 0..8 {
            if list & (1 << r) != 0 {
                c.r[r] = b.read32_a(addr);
                addr = addr.wrapping_add(4);
            }
        }
    } else {
        for r in 0..8 {
            if list & (1 << r) != 0 {
                let v = if r == rb && r != first { new_base } else { c.r[r] };
                b.write32_a(addr, v);
                addr = addr.wrapping_add(4);
            }
        }
        c.r[rb] = new_base;
    }
}

/// Conditional branch (format 16).
fn bcond<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    if (crate::COND[((i >> 8) & 15) as usize] >> (c.cpsr >> 28)) & 1 != 0 {
        let off = ((i as u8 as i8) as i32 as u32) << 1;
        c.next = c.r[15].wrapping_add(off);
    }
}

/// SWI (format 17).
fn swi<B: Bus>(c: &mut Cpu, b: &mut B, _i: u32) {
    c.instr_exception(b, Exception::Swi);
}

/// Unconditional branch (format 18).
fn branch<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let off = ((((i & 0x7ff) << 21) as i32) >> 20) as u32;
    c.next = c.r[15].wrapping_add(off);
}

/// Long branch with link, in two halves (format 19).
fn bl_hi<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let off = ((((i & 0x7ff) << 21) as i32) >> 9) as u32;
    c.r[14] = c.r[15].wrapping_add(off);
}
fn bl_lo<B: Bus>(c: &mut Cpu, _b: &mut B, i: u32) {
    let t = c.r[14].wrapping_add((i & 0x7ff) << 1);
    c.r[14] = c.next | 1;
    c.next = t;
}

/// The handler for the halfword whose bits 15:6 are `idx`.
fn decode<B: Bus>(idx: u32) -> ThumbFn<B> {
    let t5 = idx >> 5; // bits 15:11
    match t5 {
        0b00000 => shift_imm::<B, 0>,
        0b00001 => shift_imm::<B, 1>,
        0b00010 => shift_imm::<B, 2>,
        0b00011 => match (idx >> 3) & 3 {
            0 => addsub::<B, false, false>,
            1 => addsub::<B, true, false>,
            2 => addsub::<B, false, true>,
            _ => addsub::<B, true, true>,
        },
        0b00100 => imm8::<B, 0>,
        0b00101 => imm8::<B, 1>,
        0b00110 => imm8::<B, 2>,
        0b00111 => imm8::<B, 3>,
        0b01000 => {
            if idx >> 4 == 0b010000 {
                macro_rules! pick {
                    ($($n:literal),*) => { match idx & 15 { $($n => alu::<B, $n> as ThumbFn<B>,)* _ => undef::<B> } };
                }
                pick!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15)
            } else if idx >> 4 == 0b010001 {
                match (idx >> 2) & 3 {
                    0 => hireg::<B, 0>,
                    1 => hireg::<B, 1>,
                    2 => hireg::<B, 2>,
                    _ => hireg::<B, 3>,
                }
            } else {
                undef::<B>
            }
        }
        0b01001 => ldr_pc::<B>,
        0b01010 | 0b01011 => {
            let h = (idx >> 5) & 1;
            if (idx >> 3) & 1 == 0 {
                match (h, (idx >> 4) & 1) {
                    (0, 0) => ldst_reg::<B, false, false>,
                    (0, 1) => ldst_reg::<B, false, true>,
                    (1, 0) => ldst_reg::<B, true, false>,
                    _ => ldst_reg::<B, true, true>,
                }
            } else {
                match ((idx >> 4) & 1) | (h << 1) {
                    0 => ldst_sign::<B, 0>,
                    1 => ldst_sign::<B, 1>,
                    2 => ldst_sign::<B, 2>,
                    _ => ldst_sign::<B, 3>,
                }
            }
        }
        0b01100 => ldst_imm::<B, false, false>,
        0b01101 => ldst_imm::<B, true, false>,
        0b01110 => ldst_imm::<B, false, true>,
        0b01111 => ldst_imm::<B, true, true>,
        0b10000 => ldst_half::<B, false>,
        0b10001 => ldst_half::<B, true>,
        0b10010 => ldst_sp::<B, false>,
        0b10011 => ldst_sp::<B, true>,
        0b10100 => add_addr::<B, false>,
        0b10101 => add_addr::<B, true>,
        0b10110 | 0b10111 => {
            if idx >> 2 == 0b1011_0000 {
                add_sp::<B>
            } else if idx >> 3 == 0b101_1010 {
                push::<B>
            } else if idx >> 3 == 0b101_1110 {
                pop::<B>
            } else {
                undef::<B>
            }
        }
        0b11000 => block::<B, false>,
        0b11001 => block::<B, true>,
        0b11010 | 0b11011 => match idx >> 2 {
            0b1101_1111 => swi::<B>,
            0b1101_1110 => undef::<B>,
            _ => bcond::<B>,
        },
        0b11100 => branch::<B>,
        0b11110 => bl_hi::<B>,
        0b11111 => bl_lo::<B>,
        _ => undef::<B>,
    }
}

pub(crate) fn build_table<B: Bus>() -> Box<[ThumbFn<B>; 1024]> {
    let mut t: Box<[ThumbFn<B>; 1024]> = Box::new([undef::<B> as ThumbFn<B>; 1024]);
    for idx in 0..1024usize {
        t[idx] = decode::<B>(idx as u32);
    }
    t
}
