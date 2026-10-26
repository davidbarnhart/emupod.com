#!/usr/bin/env python3
"""The Rust core's call harness in LITERAL mode, the address space the decompilation gate uses. No firmware or card needed.

    python3 tests/test_call_literal.py

Literal mode: no aliases and no DRAM/IRAM arrays; an address is what the host mapped, [0, 0x1000) is never mapped (a null pointer faults),
the same bytes mapped at 0x28000000 and at 0x1000 are two separate copies, code runs from plain pages (here at 0x27F00000 with the stack at
0x5F008000 and the return sentinel 0x7F000000, as the gate lays them out), exit addresses inside the range stop a call (status 4),
misaligned accesses fault (kind 4), a trace logs (pc, cpsr) and registers at chosen pcs, restore puts plain pages back and unmaps the
pages mapped after the snapshot, and two cores in one process do not share memory."""
import os, struct as st, sys, tempfile
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)


def main():
    try:
        from emupod import emupod_rs                 # an installed wheel or a maturin develop build
    except ImportError:
        try:
            sys.path.insert(0, os.path.join(ROOT, "emupod"))
            import emupod_rs                         # rust/build_py.sh
        except ImportError:
            print("skipped: the Rust module is not built (maturin develop --release)"); return 0
    if True:
        A, B = emupod_rs.Machine.bare(), emupod_rs.Machine.bare()                       # two worlds in one process, no card, no boot, already in call mode
        CODE, SP0, SENT = 0x27F00000, 0x5F008000, 0x7F000000
        bad = 0
        def check(name, ok, detail=""):
            nonlocal bad
            print(("ok   " if ok else "FAIL ") + name + (": " + detail if detail else "")); bad += not ok
        W = lambda *ws: st.pack("<%dI" % len(ws), *ws)
        PAGE = lambda b: bytes((b + i) & 0xff for i in range(4096))

        def run(m, addr, budget=1000, lo=0, hi=0, stops=(), **r):
            regs, cpsr = m.call_regs(); regs = list(regs)
            for k, v in r.items(): regs[int(k[1:])] = v
            regs[13], regs[14], regs[15] = SP0, SENT, addr
            m.set_cpu_regs(regs, cpsr | 0xc0)
            s, n = m.call_run(SENT, budget, lo, hi, list(stops))
            return s, n, m.call_regs()[0]

        # code: ldr r2,[r0]; add r2,r2,#1; str r2,[r1]; bx lr
        code = W(0xe5902000, 0xe2822001, 0xe5812000, 0xe12fff1e)
        for m in (A, B):
            m.call_map_page(CODE, code + b"\0" * (4096 - len(code)))
            m.call_map_page(0x28000000, PAGE(1)); m.call_map_page(0x1000, PAGE(1))        # the "image", twice
            m.call_map_page(SP0 - 0x1000, b"\0" * 4096)
        s, n, g = run(A, CODE, r0=0x28000000, r1=0x28000100)
        check("code runs from a plain page at 0x27F00000", s == 0 and n == 4 and g[2] == st.unpack("<I", PAGE(1)[:4])[0] + 1, "status %d after %d instructions" % (s, n))
        a1 = bytes(A.mem_read(0x28000100, 4)); a2 = bytes(A.mem_read(0x1100, 4))
        check("the image at 0x28000000 and at 0x1000 are two separate copies", a1 == st.pack("<I", g[2]) and a2 == PAGE(1)[0x100:0x104], "%s vs %s" % (a1.hex(), a2.hex()))
        s, n, g = run(A, CODE, r0=0, r1=0x28000100); f = A.call_fault()
        check("a null read faults (0 is never mapped)", s == 3 and f == (0, 0, 4, CODE, 0), str(f))
        s, n, g = run(A, CODE, r0=0x28000001, r1=0x28000100); f = A.call_fault()
        check("a misaligned word read is fault kind 4, the instruction undone", s == 3 and f == (4, 0x28000001, 4, CODE, 0) and g[15] == CODE and n == 0, str(f))
        s, n, g = run(A, CODE, stops=[CODE + 8], r0=0x28000000, r1=0x28000100)
        check("an exit address inside the range stops the call before that instruction", s == 4 and g[15] == CODE + 8 and n == 2, "status %d, pc %#x after %d" % (s, g[15], n))
        s, n, g = run(A, CODE, lo=CODE, hi=CODE + 8, r0=0x28000000, r1=0x28000100)
        check("leaving the range is status 2", s == 2 and g[15] == CODE + 8, "pc %#x" % g[15])
        A.call_trace_set(CODE, CODE + 0x10, [CODE + 4])
        run(A, CODE, r0=0x28000000, r1=0x28000100)
        tr, rg = A.call_trace_take(); A.call_trace_off()
        check("the trace logs (pc, cpsr) of every instruction in the range and r0-r12 at the chosen pc", [p for p, c in tr] == [CODE, CODE + 4, CODE + 8, CODE + 12] and len(rg) == 1 and rg[0][0] == CODE + 4 and rg[0][1][2] == st.unpack("<I", PAGE(1)[:4])[0], "%d pcs, %d register samples" % (len(tr), len(rg)))
        A.snapshot()
        A.mem_write(CODE, W(0xe3a00007, 0xe12fff1e))                                    # patch: mov r0,#7; bx lr
        s, n, g = run(A, CODE)
        check("code patched in place runs as patched", s == 0 and g[0] == 7, "r0 %d" % g[0])
        A.call_map_page(0x30000000, PAGE(9)); A.mem_write(0x28000100, W(0xdeadbeef))
        A.restore()
        s, n, g = run(A, CODE, r0=0x28000000, r1=0x28000100)
        check("restore puts the code and data back and unmaps pages mapped since", s == 0 and g[2] == st.unpack("<I", PAGE(1)[:4])[0] + 1, "")
        s, n, g = run(A, CODE, r0=0x30000000, r1=0x28000100); f = A.call_fault()
        check("a page mapped after the snapshot is unmapped by the restore", s == 3 and f is not None and f[0] == 0 and f[1] == 0x30000000, str(f))
        s, n, g = run(B, CODE, r0=0x28000000, r1=0x28000200)
        check("a second core in the same process has its own memory", bytes(B.mem_read(0x28000100, 4)) == PAGE(1)[0x100:0x104] and s == 0, "")
        # a cpsr with another mode switches the banks, as on Unicorn: r13/r14 are what the new mode's bank holds
        A.call_mode(True, True, True)
        r, c = A.call_regs(); r = list(r); r[13], r[14], r[15] = 0x1111, 0x2222, CODE
        A.set_cpu_regs(r, 0x13 | 0xc0)                                                    # SVC, r13/r14 = 0x1111/0x2222
        r[13], r[14] = 0x3333, 0x4444
        A.set_cpu_regs(r, 0x12 | 0xc0); r2 = A.call_regs()[0]                              # IRQ: its own bank (empty), not 0x3333/0x4444
        A.set_cpu_regs(r, 0x00); r3 = A.call_regs()[0]                                     # mode 0x00 (the gate's stand-in can hand back that): the User bank, no panic
        A.set_cpu_regs(r, 0x13 | 0xc0); r4 = A.call_regs()[0]                              # back to SVC: the bank kept 0x1111/0x2222
        check("another cpsr mode switches the register banks (IRQ, mode 0x00 and back to SVC)", (r2[13], r2[14]) != (0x3333, 0x4444) and (r4[13], r4[14]) == (0x1111, 0x2222) and A.call_regs()[1] & 0x1f == 0x13, "irq r13/r14 %#x/%#x, svc again %#x/%#x" % (r2[13], r2[14], r4[13], r4[14]))
        # faults off: the run goes on as an ARM7 does (aligned access, rotated), but the log keeps the address the instruction ASKED for
        A.call_mode(True, True, False); A.call_log_on(True); A.call_log_take()
        A.call_map_page(CODE, W(0xe5902000, 0xe5813000, 0xe12fff1e) + b"\0" * 4084)                  # ldr r2,[r0]; str r3,[r1]; bx lr
        A.mem_write(0x28000000, PAGE(1)); A.call_map_page(0x28000000, PAGE(1))
        s, n, g = run(A, CODE, r0=0x28000001, r1=0x28000102, r3=0xcafef00d); log = A.call_log_take()
        want = st.unpack("<I", PAGE(1)[:4])[0]; rot = ((want >> 8) | (want << 24)) & 0xffffffff
        check("faults off: a misaligned load goes on (rotated) and is logged at the address asked for",
              s == 0 and g[2] == rot and [(k, hex(a), z) for k, a, z, v, pc in log] == [(0, "0x28000001", 4), (1, "0x28000102", 4)], str([(k, hex(a), z, hex(v)) for k, a, z, v, pc in log]))
        # an UNMAPPED misaligned store with faults off reports the address it asked for too
        A.call_mode(True, True, False); A.call_map_page(CODE, W(0xe5801014, 0xe12fff1e) + b"\0" * 4088)  # str r1,[r0,#0x14]; bx lr
        s, n, g = run(A, CODE, r0=0x41000001, r1=0xabcd); f = A.call_fault()
        check("faults off: an unmapped misaligned store reports the address asked for, with the value", s == 3 and f == (1, 0x41000015, 4, CODE, 0xabcd), str(f))
        for m in (A, B): m.call_mode(False)
        print("OK: literal call mode works as the gate needs" if not bad else "FAIL")
        return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
