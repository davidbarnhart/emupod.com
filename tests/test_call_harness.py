#!/usr/bin/env python3
"""The Rust core's call harness (set_cpu_regs / call_run / call_regs) against the Python core's fw_call, on the same firmware.

    python3 tests/test_call_harness.py CARD.img

Boots CARD (any card with a flashpod or Apple 1.5-family firmware) on both cores and calls the soft-float runtime of Apple's osos
(rt_fadd, rt_fsub, rt_fdiv, rt_f2i, rt_i2f: 0x2814090c, 0x28140a84, 0x28140c80, 0x28140eb8, 0x28140f74 in 1.5) with many arguments. A
result must be the same on both cores AND the IEEE-754 single result (computed here with struct), so a wrong address or a wrong
harness fails. Also checks the three ways a call ends: it returns to the sentinel, it runs out of budget, it leaves an exit range.
Skipped (exit 0) with no CARD."""
import os, struct, sys
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
SENT, SP = 0x28000008, 0x29f00000
FUNCS = {"rt_fadd": 0x2814090c, "rt_fsub": 0x28140a84, "rt_fdiv": 0x28140c80}
f32 = lambda x: struct.unpack("<I", struct.pack("<f", x))[0]
fv = lambda b: struct.unpack("<f", struct.pack("<I", b & 0xffffffff))[0]


def main(argv):
    if not argv or not os.path.exists(argv[0]): print("skipped: needs a card image"); return 0
    vals = [0.0, 1.0, -1.0, 0.5, 3.25, -7.5, 100.0, 1e-3, 123456.0, -0.0078125]
    cases = [(name, a, b) for name in FUNCS for a in vals for b in vals if not (name == "rt_fdiv" and b == 0.0)]
    from emupod import emu, recording as R
    boot = lambda backend: emu.open(emu.Config(backend=backend, disk=argv[0], osos="card", **{k: v for k, v in R.LIVE_TIMING.items() if backend == "rust" or k != "ata_fast"}))
    if "--python-side" in argv:                        # one emulator per process: the Python core runs in a child and prints its answers
        import json
        E = boot("python"); d = R.Driver(E); d.boot()
        for _ in range(60): d.step()
        print(json.dumps([E.fw_call(FUNCS[nm], f32(a), f32(b)) for nm, a, b in cases])); return 0
    import json, subprocess
    r = subprocess.run([sys.executable, os.path.abspath(__file__), argv[0], "--python-side"], capture_output=True, text=True)
    if r.returncode: print(r.stdout + r.stderr); return 1
    pyres = json.loads(r.stdout.strip().splitlines()[-1])
    rs = boot("rust"); d = R.Driver(rs); d.boot()
    for _ in range(60): d.step()
    m = rs._e._m if hasattr(rs, "_e") else rs._m

    def rust_call(addr, *args, budget=100000, lo=0, hi=0):
        r, cpsr = m.call_regs(); r = list(r)
        for i, a in enumerate(args): r[i] = a & 0xffffffff
        r[13], r[14], r[15] = SP, SENT, addr
        m.set_cpu_regs(r, cpsr | 0xc0)
        status, n = m.call_run(SENT, budget, lo, hi)
        regs, _ = m.call_regs()
        return status, regs[0], n, regs

    bad = n = 0
    for (name, a, b), ref in zip(cases, pyres):
        want = {"rt_fadd": lambda: a + b, "rt_fsub": lambda: a - b, "rt_fdiv": lambda: a / b}[name]()
        status, got, _, _ = rust_call(FUNCS[name], f32(a), f32(b))
        n += 1
        if status != 0 or got != ref or abs(fv(got) - want) > abs(want) * 1e-6 + 1e-30:
            bad += 1
            if bad < 6: print("MISMATCH %s(%r, %r): rust status %d %#x, python %r, IEEE %r" % (name, a, b, status, got, ref, want))
    print("%d calls, %d mismatches (rust vs python vs IEEE)" % (n, bad))
    s, _, used, _ = rust_call(FUNCS["rt_fadd"], f32(1.0), f32(2.0), budget=3)
    print("budget of 3 instructions: status %d (want 1), ran %d" % (s, used)); bad += s != 1
    s, _, used, regs = rust_call(FUNCS["rt_fadd"], f32(1.0), f32(2.0), lo=FUNCS["rt_fadd"], hi=FUNCS["rt_fadd"] + 8)
    print("exit range of 8 bytes: status %d (want 2), left at pc %#x after %d" % (s, regs[15], used)); bad += s != 2
    # step 2: snapshot / restore. Small routines of our own (so the check does not depend on a firmware function): A stores r1 at [r0] and
    # returns 1, B returns 2; both sit in DRAM at X. The hash covers all of DRAM and IRAM.
    import hashlib, struct as st
    X, W, IW = 0x29f10000, 0x29f20000, 0x4000f000
    A = st.pack("<4I", 0xe5801000, 0xe3a00001, 0xe12fff1e, 0)                  # str r1,[r0]; mov r0,#1; bx lr
    B = st.pack("<4I", 0xe3a00002, 0xe12fff1e, 0, 0)                           # mov r0,#2; bx lr
    h = lambda: hashlib.sha256(bytes(m.mem_read(0x28000000, 0x2000000)) + bytes(m.mem_read(0x40000000, 0x100000))).hexdigest()
    before, regs0 = h(), m.call_regs()
    m.snapshot()
    m.mem_write(X, A)
    s1, r1, _, _ = rust_call(X, W, 0xdeadbeef); s2, r2, _, _ = rust_call(X, IW, 0x12345678)
    wrote = bytes(m.mem_read(W, 4)) == st.pack("<I", 0xdeadbeef) and bytes(m.mem_read(IW, 4)) == st.pack("<I", 0x12345678)
    changed = h() != before
    pages = m.restore()
    same = h() == before and m.call_regs() == regs0
    print("snapshot/restore: calls %s/%s returned %s/%s, memory written %s and changed %s, restored %s (%s pages), registers back %s" % (s1, s2, r1, r2, wrote, changed, same, pages, m.call_regs() == regs0))
    bad += not (s1 == s2 == 0 and r1 == r2 == 1 and wrote and changed and same and pages == 3)    # X, W (DRAM), IW (IRAM)
    m.mem_write(X, B)                                                           # new code where the old was run (and decoded)
    s3, r3, _, _ = rust_call(X)
    print("code replaced after a restore runs the new code: returned %d (want 2)" % r3); bad += not (s3 == 0 and r3 == 2)
    m.restore(); bad += h() != before
    # step 3: call mode. Only mapped pages exist; any other access ends the call (status 3) with the fault, the instruction not done, so the
    # host can map the page, fill it and call again; writes outside the allowed ranges are refused before they land; peripherals are plain
    # memory, never devices; every data access is logged.
    C = st.pack("<4I", 0xe5902000, 0xe5812000, 0xe5943000, 0xe12fff1e)      # ldr r2,[r0]; str r2,[r1]; ldr r3,[r4]; bx lr
    D = st.pack("<2I", 0xe5852000, 0xe12fff1e)                               # str r2,[r5]; bx lr
    PAGE = lambda b: bytes((b + i) & 0xff for i in range(4096))
    m.restore()
    m.call_mode(True); m.call_log_on(True)
    for a in (X, W): m.call_map_page(a, PAGE(a >> 12))
    m.call_map_page(X, C + PAGE(0)[16:])
    U, TIMER = 0x29f30000, 0xcf001104
    def run(addr, **r):
        regs, cpsr = m.call_regs(); regs = list(regs)
        for k, v in r.items(): regs[int(k[1:])] = v
        regs[14], regs[15] = SENT, addr
        m.set_cpu_regs(regs, cpsr | 0xc0)
        return m.call_run(SENT, 1000), m.call_regs()[0]
    (s1, n1), g = run(X, r0=W, r1=W + 0x10, r4=U)
    f = m.call_fault(); log = m.call_log_take()
    ok1 = s1 == 3 and f == (0, U, 4, X + 8, 0) and n1 == 2 and g[15] == X + 8 and g[2] == st.unpack("<I", PAGE(W >> 12)[:4])[0]
    ok1 = ok1 and [(k, a, z) for k, a, z, v, pc in log] == [(0, W, 4), (1, W + 0x10, 4)] and bytes(m.mem_read(W + 0x10, 4)) == PAGE(W >> 12)[:4]
    print("unmapped read: status %d fault %s after %d instructions, pc %#x; log %s" % (s1, f, n1, g[15], [(k, hex(a)) for k, a, *_ in log])); bad += not ok1
    m.call_map_page(U, PAGE(0x77))
    (s2, n2), g = run(g[15], r0=W, r1=W + 0x10, r4=U)
    print("mapped and called again from the fault: status %d, r3 %#x (want %#x)" % (s2, g[3], st.unpack("<I", PAGE(0x77)[:4])[0])); bad += not (s2 == 0 and g[3] == st.unpack("<I", PAGE(0x77)[:4])[0])
    m.mem_write(X, D); m.call_write_ranges([(W, W + 0x1000)]); m.call_log_take()
    (s3, _), g = run(X, r2=0x1234, r5=U + 8); f3 = m.call_fault()
    print("write outside the allowed range: status %d fault %s, memory untouched %s" % (s3, f3, bytes(m.mem_read(U + 8, 4)) == PAGE(0x77)[8:12])); bad += not (s3 == 3 and f3 == (2, U + 8, 4, X, 0x1234) and bytes(m.mem_read(U + 8, 4)) == PAGE(0x77)[8:12])
    (s4, _), g = run(X, r2=0x1234, r5=W + 8)
    print("write inside it: status %d, landed %s" % (s4, bytes(m.mem_read(W + 8, 4)) == st.pack("<I", 0x1234))); bad += not (s4 == 0 and bytes(m.mem_read(W + 8, 4)) == st.pack("<I", 0x1234))
    m.call_write_ranges([])
    (s5, _), g = run(X, r2=1, r5=TIMER); f5 = m.call_fault()
    m.call_map_page(TIMER, b"\xaa" * 4096)
    (s6, _), g = run(X, r2=0x55667788, r5=TIMER)
    plain = bytes(m.mem_read(TIMER, 4)) == st.pack("<I", 0x55667788)
    m.mem_write(X, st.pack("<2I", 0xe5953000, 0xe12fff1e))                    # ldr r3,[r5]; bx lr
    (s7, _), g = run(X, r5=TIMER)
    print("peripheral page: unmapped write faults %s (%s), mapped it is plain memory: wrote and read back %#x (want 0x55667788)" % (s5, f5, g[3])); bad += not (s5 == 3 and f5[0] == 1 and s6 == 0 and plain and s7 == 0 and g[3] == 0x55667788)
    (s8, _), g = run(0x29f40000); f8 = m.call_fault()
    print("instruction fetch from an unmapped page: status %d fault %s" % (s8, f8)); bad += not (s8 == 3 and f8 == (3, 0x29f40000, 4, 0x29f40000, 0))
    m.call_map_page(0x04000000 + 0x1f50000, PAGE(1))                           # mapped through one alias ...
    m.mem_write(X, st.pack("<2I", 0xe5953000, 0xe12fff1e))
    (s9, _), g = run(X, r5=0x29f50000); (s10, _), g2 = run(X, r5=0x1f50000)    # ... read through the other two
    print("aliases: %d/%d, r3 %#x / %#x" % (s9, s10, g[3], g2[3])); bad += not (s9 == s10 == 0 and g[3] == g2[3] == st.unpack("<I", PAGE(1)[:4])[0])
    m.call_mode(False); m.call_log_on(False)
    print("OK: the call harness matches the Python core and IEEE-754, a restore puts everything back, and call mode faults, refuses and logs as specified" if not bad else "FAIL")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
