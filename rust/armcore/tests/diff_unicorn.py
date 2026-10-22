#!/usr/bin/env python3
"""Differential test: random single instructions of every class on Unicorn and on armcore, registers, CPSR, PC
and memory compared.

    cargo build --release   (in rust/)
    python3 rust/armcore/tests/diff_unicorn.py [cases-per-class] [seed]

Unicorn emulates a later ARM (an ARM926 here, ARMv5TE) than the ARM7TDMI (ARMv4T), so the generator keeps to what
the two agree on: no unaligned halfword loads, no loads or data-processing writes to PC that interwork, no store of
PC (+8 on ARMv5, +12 on the ARM7), no PC read by a register-specified shift, no SPSR, no unpredictable forms
(base register in the list with writeback, Rd = Rm in MUL, ...). Those are tested by hand in tests/hand.rs.
"""
import os, random, subprocess, sys, zlib

from unicorn import Uc, UC_ARCH_ARM, UC_MODE_ARM, UcError
import unicorn.arm_const as A

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(HERE, "..", "..", "target", "release", "armtest")
WIN = 0x2000
WIN_LEN = 0x4000
PATTERN = bytes(((a * 2654435761) >> 13) & 0xff for a in range(0x10000))
REGS = [A.UC_ARM_REG_R0 + i for i in range(13)] + [A.UC_ARM_REG_SP, A.UC_ARM_REG_LR]
MODES = [0x10, 0x10, 0x10, 0x1f, 0x13, 0x12, 0x11, 0x17, 0x1b]   # User weighted, then the privileged modes

rng = random.Random(1)


def r32():
    return rng.getrandbits(32)


def addr(align=4):
    return (WIN + 0x400 + rng.randrange(0, WIN_LEN - 0x800)) & ~(align - 1)


class Case:
    def __init__(self, instr, thumb=False, cond_flags=None, mode=None):
        self.instr, self.thumb = instr, thumb
        self.mode = rng.choice(MODES) if mode is None else mode
        flags = rng.getrandbits(4) << 28 if cond_flags is None else cond_flags
        self.cpsr = flags | self.mode | (0x20 if thumb else 0) | (rng.getrandbits(2) << 6)
        self.r = [r32() for _ in range(15)]
        self.pc = (addr(4) & ~3) if not thumb else (addr(2) & ~1)
        if self.pc < WIN + 0x400 or self.pc > WIN + WIN_LEN - 0x400:
            self.pc = WIN + 0x800

    def set(self, reg, value):
        self.r[reg] = value & 0xffffffff
        return self

    def line(self):
        return "%x %s %x %x" % (self.cpsr, " ".join("%x" % x for x in self.r), self.pc, self.instr)


def distinct(n, exclude=()):
    pool = [x for x in range(15) if x not in exclude]
    return rng.sample(pool, n)


def cond():
    return rng.choice([14, 14, 14] + list(range(14)))


# ----------------------------------------------------------------------------------------------- ARM generators
def arm_dp_imm():
    op, s = rng.randrange(16), rng.randrange(2)
    if 8 <= op <= 11:
        s = 1
    rn, rd = rng.randrange(16), rng.randrange(15)
    return Case(cond() << 28 | 1 << 25 | op << 21 | s << 20 | rn << 16 | rd << 12 | rng.randrange(4096))


def arm_dp_reg():
    op, s = rng.randrange(16), rng.randrange(2)
    if 8 <= op <= 11:
        s = 1
    rd = rng.randrange(15)
    ty = rng.randrange(4)
    if rng.random() < 0.5:           # shift by immediate
        rn, rm = rng.randrange(16), rng.randrange(16)
        sh = rng.randrange(32) << 7 | ty << 5
    else:                            # shift by register (PC would read +12 here, +8 on Unicorn)
        rn, rm, rs = rng.randrange(15), rng.randrange(15), rng.randrange(15)
        sh = rs << 8 | ty << 5 | 1 << 4
    c = Case(cond() << 28 | op << 21 | s << 20 | rn << 16 | rd << 12 | sh | rm)
    if sh & 0x10:
        c.set(rs, rng.choice([0, 1, 5, 31, 32, 33, 63, 64, 100, 255, 256, 257, r32() & 0xff, r32()]))
    return c


def arm_mul():
    s = rng.randrange(2)
    if rng.random() < 0.5:
        a, d, m, n = rng.randrange(2), *distinct(3)[0:1], 0, 0
        rd, rm, rs = distinct(3)
        rn = rng.randrange(15)
        acc = rng.randrange(2)
        return Case(cond() << 28 | acc << 21 | s << 20 | rd << 16 | rn << 12 | rs << 8 | 9 << 4 | rm)
    hi, lo, rm, rs = distinct(4)
    signed, acc = rng.randrange(2), rng.randrange(2)
    return Case(cond() << 28 | 1 << 23 | signed << 22 | acc << 21 | s << 20 | hi << 16 | lo << 12 | rs << 8 | 9 << 4 | rm)


def arm_ldst():
    reg = rng.randrange(2)
    pre = rng.randrange(2)
    up, byte, load = rng.randrange(2), rng.randrange(2), rng.randrange(2)
    wb = rng.randrange(2) if pre else 0
    rn, rd = rng.randrange(15), rng.randrange(15)
    if wb or not pre:
        if rd == rn:
            rd = (rn + 1) % 15
    if reg:
        rm = rng.choice([x for x in range(15) if x != rn])
        # words stay aligned (the ARM7 rotates or aligns an odd address, Unicorn does not): LSL of a multiple of 4
        ty, amt = (rng.randrange(4), rng.randrange(32)) if byte else (0, rng.randrange(3))
        val = rng.randrange(0, 0x10) * 4 if not byte else rng.randrange(0, 0x40)
        if byte and ty != 0:
            amt = rng.randrange(4)
        c = Case(cond() << 28 | 3 << 25 | pre << 24 | up << 23 | byte << 22 | wb << 21 | load << 20 | rn << 16 | rd << 12 | amt << 7 | ty << 5 | rm)
        c.set(rm, val)
    else:
        off = rng.randrange(0x100) if byte else rng.randrange(0x40) * 4
        c = Case(cond() << 28 | 2 << 25 | pre << 24 | up << 23 | byte << 22 | wb << 21 | load << 20 | rn << 16 | rd << 12 | off)
    c.set(rn, addr(1) if byte else addr(4))
    return c


def arm_ldsth():
    imm = rng.randrange(2)
    pre = rng.randrange(2)
    up, load = rng.randrange(2), rng.randrange(2)
    wb = rng.randrange(2) if pre else 0
    sh = rng.choice([1, 2, 3]) if load else 1
    rn, rd = rng.randrange(15), rng.randrange(15)
    if wb or not pre:
        if rd == rn:
            rd = (rn + 1) % 15
    if imm:
        off = (rng.randrange(16) << 8) | rng.randrange(16)
        if sh != 2:
            off &= ~1                                   # halfwords stay aligned
        instr = cond() << 28 | pre << 24 | up << 23 | 1 << 22 | wb << 21 | load << 20 | rn << 16 | rd << 12 | (off >> 8) << 8 | 1 << 7 | sh << 5 | 1 << 4 | (off & 0xf)
        c = Case(instr)
    else:
        rm = rng.choice([x for x in range(15) if x != rn])
        instr = cond() << 28 | pre << 24 | up << 23 | wb << 21 | load << 20 | rn << 16 | rd << 12 | 1 << 7 | sh << 5 | 1 << 4 | rm
        c = Case(instr)
        c.set(rm, rng.randrange(0, 0x40) & ~1)
    align = 2 if sh != 2 else 1
    c.set(rn, addr(2) if align == 2 else addr(1))
    return c


def arm_block():
    load, pre, up, wb = rng.randrange(2), rng.randrange(2), rng.randrange(2), rng.randrange(2)
    rn = rng.randrange(15)
    n = rng.randrange(1, 9)
    lst = rng.sample(range(15), n)
    if wb and rn in lst:
        lst.remove(rn)
        if not lst:
            lst = [(rn + 1) % 15]
    mask = sum(1 << x for x in lst)
    mode = rng.choice(MODES)
    s = rng.randrange(2) if mode not in (0x10, 0x1f) and rng.random() < 0.4 else 0
    c = Case(cond() << 28 | 4 << 25 | pre << 24 | up << 23 | s << 22 | wb << 21 | load << 20 | rn << 16 | mask, mode=mode)
    c.set(rn, addr(4))
    return c


def arm_swp():
    byte = rng.randrange(2)
    rd, rn, rm = rng.sample(range(15), 3) if rng.random() < 0.8 else (rng.randrange(15),) * 3
    c = Case(cond() << 28 | 1 << 24 | byte << 22 | rn << 16 | rd << 12 | 9 << 4 | rm)
    c.set(rn, addr(1) if byte else addr(4))
    return c


def arm_psr():
    rd = rng.randrange(15)
    kind = rng.randrange(3)
    if kind == 0:   # MRS CPSR
        return Case(cond() << 28 | 0x10f << 16 | rd << 12)
    fields = rng.randrange(16)
    val = rng.choice([0x10, 0x11, 0x12, 0x13, 0x17, 0x1b, 0x1f]) | (rng.getrandbits(4) << 28) | (rng.getrandbits(2) << 6)
    if kind == 1:   # MSR CPSR_fields, Rm
        rm = rng.randrange(15)
        return Case(cond() << 28 | 0x01200000 | fields << 16 | 0xf << 12 | rm).set(rm, val | (r32() & 0x0fffff00))
    if rng.random() < 0.5:           # a valid control byte (rotation 0)
        fields |= 1
        imm8, rot = rng.choice([0x10, 0x11, 0x12, 0x13, 0x17, 0x1b, 0x1f]) | (rng.getrandbits(2) << 6), 0
    else:                            # flags only
        fields &= ~1
        imm8, rot = rng.randrange(256), rng.randrange(16)
    return Case(cond() << 28 | 0x03200000 | fields << 16 | 0xf << 12 | rot << 8 | imm8)


def arm_branch():
    link = rng.randrange(2)
    off = rng.randrange(-0x200, 0x200) & 0xffffff
    return Case(cond() << 28 | 5 << 25 | link << 24 | off)


def arm_bx():
    rm = rng.randrange(15)
    return Case(cond() << 28 | 0x12fff10 | rm).set(rm, (addr(2) | 1) if rng.randrange(2) else addr(4))


ARM = [arm_dp_imm, arm_dp_reg, arm_dp_reg, arm_mul, arm_ldst, arm_ldst, arm_ldsth, arm_block, arm_swp, arm_psr, arm_branch, arm_bx]


# --------------------------------------------------------------------------------------------- Thumb generators
def th(instr, **kw):
    return Case(instr, thumb=True, mode=kw.get("mode", 0x1f if rng.random() < 0.5 else 0x13), cond_flags=kw.get("flags"))


def th_shift():
    return th(rng.randrange(3) << 11 | rng.randrange(32) << 6 | rng.randrange(8) << 3 | rng.randrange(8))


def th_addsub():
    return th(0x1800 | rng.randrange(2) << 10 | rng.randrange(2) << 9 | rng.randrange(8) << 6 | rng.randrange(8) << 3 | rng.randrange(8))


def th_imm8():
    return th(0x2000 | rng.randrange(4) << 11 | rng.randrange(8) << 8 | rng.randrange(256))


def th_alu():
    op = rng.randrange(16)
    c = th(0x4000 | op << 6 | rng.randrange(8) << 3 | rng.randrange(8))
    if op in (2, 3, 4, 7):
        c.set(rng.randrange(8), rng.choice([0, 1, 5, 31, 32, 33, 64, 255, 256, r32() & 0xff, r32()]))
    return c


def th_hireg():
    op = rng.randrange(4)
    rd = rng.randrange(16)
    rs = rng.randrange(16)
    if op == 3:
        rd = 0
        rs = rng.randrange(15)
        return th(0x4400 | op << 8 | rs << 3).set(rs, (addr(2) | 1) if rng.randrange(2) else addr(4))
    if op != 1 and rd == 15:
        rs = rng.randrange(8)
    c = th(0x4400 | op << 8 | (rd >> 3) << 7 | rs << 3 | (rd & 7))
    if op != 1 and rd == 15:
        c.set(rs, rng.randrange(0, 0x100) * 2)
    return c


def th_ldr_pc():
    return th(0x4800 | rng.randrange(8) << 8 | rng.randrange(256))


def th_ldst_reg():
    rb, ro = rng.sample(range(8), 2) if rng.random() < 0.9 else (3, 3)
    c = th(0x5000 | rng.randrange(8) << 9 | ro << 6 | rb << 3 | rng.randrange(8))
    op = (c.instr >> 9) & 7
    c.set(rb, addr(4) if op in (0, 4) else addr(2) if op in (1, 5, 7) else addr(1))
    c.set(ro, rng.randrange(0, 0x40) & (~1 if op in (1, 5, 7) else ~0) & (~3 if op in (0, 4) else ~0))
    if op in (4, 7, 5):  # LDR (word may be unaligned: keep aligned), LDRH, LDSH aligned
        pass
    return c


def th_ldst_imm():
    rb = rng.randrange(8)
    c = th(0x6000 | rng.randrange(4) << 11 | rng.randrange(32) << 6 | rb << 3 | rng.randrange(8))
    c.set(rb, addr(4))
    return c


def th_ldst_half():
    rb = rng.randrange(8)
    c = th(0x8000 | rng.randrange(2) << 11 | rng.randrange(32) << 6 | rb << 3 | rng.randrange(8))
    c.set(rb, addr(2))
    return c


def th_ldst_sp():
    c = th(0x9000 | rng.randrange(2) << 11 | rng.randrange(8) << 8 | rng.randrange(256))
    c.set(13, addr(4))
    return c


def th_addr():
    c = th(0xa000 | rng.randrange(2) << 11 | rng.randrange(8) << 8 | rng.randrange(256))
    return c


def th_addsp():
    return th(0xb000 | rng.randrange(2) << 7 | rng.randrange(128))


def th_pushpop():
    push = rng.randrange(2)
    lst = rng.randrange(256)
    extra = rng.randrange(2) if push else 0       # POP {PC} interworks on Unicorn (ARMv5): not here
    c = th(0xb400 | (0 if push else 1) << 11 | extra << 8 | lst)
    c.set(13, addr(4))
    return c


def th_block():
    load = rng.randrange(2)
    rb = rng.randrange(8)
    lst = rng.randrange(1, 256) & ~(1 << rb)
    if not lst:
        lst = 1 << ((rb + 1) % 8)
    c = th(0xc000 | load << 11 | rb << 8 | lst)
    c.set(rb, addr(4))
    return c


def th_bcond():
    return th(0xd000 | rng.randrange(14) << 8 | rng.randrange(256))


def th_branch():
    return th(0xe000 | rng.randrange(0x7ff))


def th_bl_hi():
    while True:
        c = th(0xf000 | rng.randrange(0x7ff))
        nxt = (PATTERN[c.pc + 2] | PATTERN[c.pc + 3] << 8) >> 11
        if nxt not in (0b11111, 0b11101):     # Unicorn runs a BL prefix and the suffix after it as one instruction
            return c


def th_bl_lo():
    return th(0xf800 | rng.randrange(0x7ff)).set(14, addr(2) & ~1)


THUMB = [th_shift, th_addsub, th_imm8, th_alu, th_alu, th_hireg, th_ldr_pc, th_ldst_reg, th_ldst_imm, th_ldst_half, th_ldst_sp,
         th_addr, th_addsp, th_pushpop, th_block, th_bcond, th_branch, th_bl_hi, th_bl_lo]


# -------------------------------------------------------------------------------------------------------- running
def run_unicorn(cases):
    u = Uc(UC_ARCH_ARM, UC_MODE_ARM)
    u.ctl_set_cpu_model(A.UC_CPU_ARM_926)
    u.mem_map(0, 0x10000)
    out = []
    for c in cases:
        u.mem_write(0, PATTERN)
        for m in (0x11, 0x12, 0x13, 0x17, 0x1b, 0x10):           # every bank starts at zero, as in armcore
            u.reg_write(A.UC_ARM_REG_CPSR, m | 0xc0)
            for rg in REGS[8:]:
                u.reg_write(rg, 0)
        u.reg_write(A.UC_ARM_REG_CPSR, c.cpsr)
        for i, rg in enumerate(REGS):
            u.reg_write(rg, c.r[i])
        u.mem_write(c.pc, c.instr.to_bytes(2 if c.thumb else 4, "little"))
        u.ctl_remove_cache(c.pc & ~0xfff, (c.pc & ~0xfff) + 0x1000)      # no stale translation of an earlier case
        try:
            u.emu_start(c.pc | (1 if c.thumb else 0), 0xfffffffe, count=1)
        except UcError:
            out.append("FAULT")
            continue
        regs = [u.reg_read(rg) for rg in REGS]
        pc = u.reg_read(A.UC_ARM_REG_PC) & ~1
        cpsr = u.reg_read(A.UC_ARM_REG_CPSR) & 0xf00000ff
        crc = zlib.crc32(bytes(u.mem_read(0x2000, 0x4000)))
        out.append(" ".join("%08x" % x for x in regs) + " %08x %08x %08x" % (pc, cpsr, crc))
    return out


def run_rust(cases):
    p = subprocess.run([BIN], input="\n".join(c.line() for c in cases) + "\n", capture_output=True, text=True)
    lines = p.stdout.strip().split("\n")
    assert len(lines) == len(cases), (len(lines), len(cases), p.stderr[:300])
    return [" ".join(l.split()[:-2] + [("%08x" % (int(l.split()[-2], 16) & 0xf00000ff)), l.split()[-1]]) if l != "FAULT" else l for l in lines]


def describe(c):
    try:
        import capstone
        md = capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_THUMB if c.thumb else capstone.CS_MODE_ARM)
        code = c.instr.to_bytes(2 if c.thumb else 4, "little")
        for i in md.disasm(code, c.pc):
            return "%s %s" % (i.mnemonic, i.op_str)
    except Exception:
        pass
    return "?"


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 2000
    rng.seed(int(sys.argv[2]) if len(sys.argv) > 2 else 1)
    total = bad = skipped = 0
    for name, gens in (("ARM", ARM), ("Thumb", THUMB)):
        for g in gens:
            cases = [g() for _ in range(n)]
            ru, rr = run_unicorn(cases), run_rust(cases)
            mism = []
            for c, a, b in zip(cases, ru, rr):
                if a == "FAULT":
                    skipped += 1
                    continue
                total += 1
                if a != b:
                    mism.append((c, a, b))
            bad += len(mism)
            print("%-6s %-14s %5d cases, %d differ" % (name, g.__name__, n, len(mism)))
            for c, a, b in mism[:3]:
                print("   %08x %-24s cpsr %08x  pc %x" % (c.instr, describe(c), c.cpsr, c.pc))
                print("   regs ", " ".join("%08x" % x for x in c.r))
                print("   unicorn", a)
                print("   armcore", b)
    print("%d compared, %d differ, %d skipped (Unicorn faulted)" % (total, bad, skipped))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
