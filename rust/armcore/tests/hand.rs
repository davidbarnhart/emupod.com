//! Behaviour specific to the ARM7TDMI that Unicorn's later ARM does differently or that needs more than one
//! instruction: exceptions and interrupts, mode banking, exception returns, the PC quirks, unaligned rotation.
use armcore::*;

struct Ram {
    m: Vec<u8>,
    seen: Vec<(Exception, u32)>,
}
impl Ram {
    fn new() -> Ram {
        Ram { m: vec![0; 0x10000], seen: vec![] }
    }
    fn arm(&mut self, at: u32, code: &[u32]) {
        for (i, w) in code.iter().enumerate() {
            self.write32(at + 4 * i as u32, *w);
        }
    }
    fn thumb(&mut self, at: u32, code: &[u16]) {
        for (i, w) in code.iter().enumerate() {
            self.write16(at + 2 * i as u32, *w);
        }
    }
}
impl Bus for Ram {
    fn read8(&mut self, a: u32) -> u8 {
        self.m[a as usize]
    }
    fn read16(&mut self, a: u32) -> u16 {
        u16::from_le_bytes([self.m[a as usize], self.m[a as usize + 1]])
    }
    fn read32(&mut self, a: u32) -> u32 {
        let i = a as usize;
        u32::from_le_bytes([self.m[i], self.m[i + 1], self.m[i + 2], self.m[i + 3]])
    }
    fn write8(&mut self, a: u32, v: u8) {
        self.m[a as usize] = v;
    }
    fn write16(&mut self, a: u32, v: u16) {
        self.m[a as usize..a as usize + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write32(&mut self, a: u32, v: u32) {
        self.m[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn exception(&mut self, k: Exception, pc: u32) {
        self.seen.push((k, pc));
    }
}

fn start(ram: &mut Ram, mode: u32) -> Core<Ram> {
    let _ = ram;
    let mut c: Core<Ram> = Core::new();
    c.cpu.cpsr = mode;
    c.cpu.pc = 0x1000;
    c
}

#[test]
fn unaligned_word_load_rotates() {
    let mut ram = Ram::new();
    ram.write32(0x2000, 0x4433_2211);
    ram.arm(0x1000, &[0xe591_0000]); // ldr r0, [r1]
    for (off, want) in [(0u32, 0x4433_2211u32), (1, 0x1144_3322), (2, 0x2211_4433), (3, 0x3322_1144)] {
        let mut c = start(&mut ram, MODE_SVC);
        c.cpu.r[1] = 0x2000 + off;
        c.run(&mut ram, 1);
        assert_eq!(c.cpu.r[0], want, "offset {off}");
    }
}

#[test]
fn unaligned_halfword_load_rotates_by_eight() {
    let mut ram = Ram::new();
    ram.write32(0x2000, 0x4433_2211);
    ram.arm(0x1000, &[0xe1d1_00b0]); // ldrh r0, [r1]
    let mut c = start(&mut ram, MODE_SVC);
    c.cpu.r[1] = 0x2001;
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.r[0], 0x1100_0022);
}

#[test]
fn storing_pc_stores_plus_twelve() {
    let mut ram = Ram::new();
    ram.arm(0x1000, &[0xe581_f000, 0xe881_8000]); // str pc, [r1] ; stmia r1, {pc}
    let mut c = start(&mut ram, MODE_SVC);
    c.cpu.r[1] = 0x2000;
    c.run(&mut ram, 1);
    assert_eq!(ram.read32(0x2000), 0x1000 + 12);
    c.run(&mut ram, 1);
    assert_eq!(ram.read32(0x2000), 0x1004 + 12);
}

#[test]
fn register_shift_reads_pc_plus_twelve() {
    let mut ram = Ram::new();
    ram.arm(0x1000, &[0xe1a0_031f]); // mov r0, pc, lsl r3   (shift by register: PC reads as +12)
    let mut c = start(&mut ram, MODE_SVC);
    c.cpu.r[3] = 0;
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.r[0], 0x1000 + 12);
    let mut ram = Ram::new();
    ram.arm(0x1000, &[0xe1a0_000f]); // mov r0, pc
    let mut c = start(&mut ram, MODE_SVC);
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.r[0], 0x1000 + 8);
}

#[test]
fn swi_enters_supervisor() {
    let mut ram = Ram::new();
    ram.arm(0x1000, &[0xef00_0042]); // swi 0x42
    let mut c = start(&mut ram, MODE_USR);
    c.cpu.cpsr |= FLAG_C;
    c.cpu.r[13] = 0x1111;
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.pc, 0x08);
    assert_eq!(c.cpu.cpsr & 0x1f, MODE_SVC);
    assert_ne!(c.cpu.cpsr & BIT_I, 0);
    assert_eq!(c.cpu.r[14], 0x1004);
    assert_eq!(c.cpu.spsr[3] & 0x1f, MODE_USR);
    assert_ne!(c.cpu.spsr[3] & FLAG_C, 0);
    assert_eq!(ram.seen, vec![(Exception::Swi, 0x1000)]);
    assert_ne!(c.cpu.r[13], 0x1111, "SVC has its own stack pointer");
}

#[test]
fn undefined_instruction_enters_undefined_mode() {
    let mut ram = Ram::new();
    ram.arm(0x1000, &[0xe7f0_00f0]); // an undefined encoding
    let mut c = start(&mut ram, MODE_SVC);
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.pc, 0x04);
    assert_eq!(c.cpu.cpsr & 0x1f, MODE_UND);
    assert_eq!(c.cpu.r[14], 0x1004);
}

#[test]
fn irq_enters_and_returns() {
    let mut ram = Ram::new();
    ram.arm(0x18, &[0xe25e_f004]); // subs pc, lr, #4
    ram.arm(0x1000, &[0xe3a0_0001, 0xe3a0_0002]); // mov r0,#1 ; mov r0,#2
    let mut c = start(&mut ram, MODE_USR | FLAG_Z);
    c.cpu.r[13] = 0xaaaa;
    c.run(&mut ram, 1);
    assert!(c.cpu.irq(&mut ram));
    assert_eq!(c.cpu.pc, 0x18);
    assert_eq!(c.cpu.cpsr & 0x1f, MODE_IRQ);
    assert_eq!(c.cpu.r[14], 0x1004 + 4);
    assert!(!c.cpu.irq(&mut ram), "masked once inside");
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.pc, 0x1004);
    assert_eq!(c.cpu.cpsr & 0x1f, MODE_USR);
    assert_ne!(c.cpu.cpsr & FLAG_Z, 0);
    assert_eq!(c.cpu.r[13], 0xaaaa, "User's stack pointer is back");
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.r[0], 2);
}

#[test]
fn fiq_banks_r8_to_r12() {
    let mut ram = Ram::new();
    ram.arm(0x1c, &[0xe3a0_8077]); // mov r8, #0x77
    let mut c = start(&mut ram, MODE_SVC);
    for i in 8..13 {
        c.cpu.r[i] = i as u32;
    }
    assert!(c.cpu.fiq(&mut ram));
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.r[8], 0x77);
    assert_eq!(c.cpu.r[9], 0, "FIQ's own r9 starts at zero");
    assert_eq!(c.cpu.user_view(9), 9, "the User/SVC r9 is kept aside");
    c.cpu.set_cpsr(MODE_SVC);
    assert_eq!(c.cpu.r[8], 8, "r8 is back");
    assert_eq!(c.cpu.r[12], 12);
}

#[test]
fn ldm_with_pc_and_s_restores_cpsr() {
    let mut ram = Ram::new();
    ram.write32(0x2000, 0x1234); // r0
    ram.write32(0x2004, 0x3000); // pc
    ram.arm(0x1000, &[0xe8d1_8001]); // ldmia r1, {r0, pc}^
    let mut c = start(&mut ram, MODE_IRQ);
    c.cpu.spsr[2] = MODE_USR | FLAG_N;
    c.cpu.r[1] = 0x2000;
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.pc, 0x3000);
    assert_eq!(c.cpu.r[0], 0x1234);
    assert_eq!(c.cpu.cpsr, MODE_USR | FLAG_N);
}

#[test]
fn stm_user_bank_from_privileged_mode() {
    let mut ram = Ram::new();
    ram.arm(0x1000, &[0xe8c1_6000]); // stmia r1, {sp, lr}^
    let mut c = start(&mut ram, MODE_SVC);
    c.cpu.set_cpsr(MODE_USR);
    c.cpu.r[13] = 0xd00d;
    c.cpu.r[14] = 0xbeef;
    c.cpu.set_cpsr(MODE_SVC);
    c.cpu.r[13] = 1;
    c.cpu.r[14] = 2;
    c.cpu.r[1] = 0x2000;
    c.run(&mut ram, 1);
    assert_eq!((ram.read32(0x2000), ram.read32(0x2004)), (0xd00d, 0xbeef));
}

#[test]
fn thumb_bl_pair_and_bx_return() {
    let mut ram = Ram::new();
    // bl +0x100 : first half (offset high = 0), second half (offset low = 0x80 half-words)
    ram.thumb(0x1000, &[0xf000, 0xf880]);
    ram.thumb(0x1104, &[0x4770]); // bx lr
    let mut c = start(&mut ram, MODE_SVC | BIT_T);
    c.run(&mut ram, 2);
    assert_eq!(c.cpu.pc, 0x1000 + 4 + 0x100);
    assert_eq!(c.cpu.r[14], 0x1004 | 1);
    c.run(&mut ram, 1);
    assert_eq!(c.cpu.pc, 0x1004);
    assert!(c.cpu.thumb());
}

#[test]
fn bx_to_thumb_and_back() {
    let mut ram = Ram::new();
    ram.arm(0x1000, &[0xe12f_ff10]); // bx r0
    ram.thumb(0x2000, &[0x2105, 0x4778]); // movs r1, #5 ; bx pc (to the next ARM word)
    ram.arm(0x2004, &[0xe3a0_2007]); // mov r2, #7
    let mut c = start(&mut ram, MODE_SVC);
    c.cpu.r[0] = 0x2001;
    c.run(&mut ram, 3);
    assert!(!c.cpu.thumb());
    c.run(&mut ram, 1);
    assert_eq!((c.cpu.r[1], c.cpu.r[2]), (5, 7));
}

#[test]
fn run_stops_when_the_bus_asks() {
    struct Stop(Ram, u32);
    impl Bus for Stop {
        fn read8(&mut self, a: u32) -> u8 { self.0.read8(a) }
        fn read16(&mut self, a: u32) -> u16 { self.0.read16(a) }
        fn read32(&mut self, a: u32) -> u32 { self.0.read32(a) }
        fn write8(&mut self, a: u32, v: u8) { self.0.write8(a, v) }
        fn write16(&mut self, a: u32, v: u16) { self.0.write16(a, v) }
        fn write32(&mut self, a: u32, v: u32) { if a == 0x3000 { self.1 = 1 } self.0.write32(a, v) }
        fn stop(&self) -> bool { self.1 == 1 }
    }
    let mut bus = Stop(Ram::new(), 0);
    bus.0.arm(0x1000, &[0xe3a0_1000, 0xe581_0000, 0xe3a0_2009]); // mov r1,#0 ; str r0,[r1] ; mov r2,#9
    let mut c: Core<Stop> = Core::new();
    c.cpu.pc = 0x1000;
    c.cpu.r[1] = 0x3000;
    c.cpu.r[0] = 0;
    // r1 is cleared by the first instruction; point the store at 0x3000 another way
    bus.0.arm(0x1000, &[0xe3a0_1a03, 0xe581_0000, 0xe3a0_2009]); // mov r1,#0x3000 ; str r0,[r1] ; mov r2,#9
    let n = c.run(&mut bus, 100);
    assert_eq!(n, 2, "stops after the store that raised the flag");
    assert_eq!(c.cpu.r[2], 0);
}

// ---- the decoded-instruction cache: a bus that speaks its protocol, and code that rewrites itself
struct Cached {
    m: Vec<u8>,
    marked: Vec<bool>,
    dirty: Vec<u32>,
    flag: bool,
}
impl Cached {
    fn new() -> Cached {
        Cached { m: vec![0; 0x10000], marked: vec![false; 1024], dirty: vec![], flag: false }
    }
}
impl Bus for Cached {
    fn read8(&mut self, a: u32) -> u8 {
        self.m[a as usize]
    }
    fn read16(&mut self, a: u32) -> u16 {
        u16::from_le_bytes([self.m[a as usize], self.m[a as usize + 1]])
    }
    fn read32(&mut self, a: u32) -> u32 {
        let i = a as usize;
        u32::from_le_bytes([self.m[i], self.m[i + 1], self.m[i + 2], self.m[i + 3]])
    }
    fn write8(&mut self, a: u32, v: u8) {
        self.touch(a);
        self.m[a as usize] = v;
    }
    fn write16(&mut self, a: u32, v: u16) {
        self.touch(a);
        self.m[a as usize..a as usize + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write32(&mut self, a: u32, v: u32) {
        self.touch(a);
        self.m[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn code_key(&self, pc: u32) -> u32 {
        if pc < 0x10000 { pc >> 2 } else { u32::MAX }
    }
    fn mark_code(&mut self, page: u32) {
        self.marked[page as usize] = true;
    }
    fn code_dirty(&self) -> bool {
        self.flag
    }
    fn take_dirty(&mut self, out: &mut Vec<u32>) {
        out.append(&mut self.dirty);
        self.flag = false;
    }
}
impl Cached {
    fn touch(&mut self, a: u32) {
        let page = (a >> 10) as usize; // 256 words of 4 bytes
        if self.marked[page] {
            self.marked[page] = false;
            self.dirty.push(page as u32);
            self.flag = true;
        }
    }
    fn arm(&mut self, at: u32, code: &[u32]) {
        for (i, w) in code.iter().enumerate() {
            self.m[(at + 4 * i as u32) as usize..(at + 4 * i as u32) as usize + 4].copy_from_slice(&w.to_le_bytes());
        }
    }
}

#[test]
fn cached_code_that_rewrites_the_next_instruction_runs_the_new_one() {
    let mut ram = Cached::new();
    ram.arm(
        0x1000,
        &[
            0xe3a0_0002, // mov r0, #2          <- rewritten
            0xe283_3001, // add r3, r3, #1
            0xe353_0001, // cmp r3, #1
            0x0582_1000, // streq r1, [r2]      (first pass only)
            0xeaff_fffa, // b 0x1000
        ],
    );
    let mut c: Core<Cached> = Core::new();
    c.cpu.pc = 0x1000;
    c.cpu.r[1] = 0xe3a0_0009; // mov r0, #9
    c.cpu.r[2] = 0x1000;
    c.run(&mut ram, 5); // one pass: r0 = 2, then the store rewrites 0x1000
    assert_eq!(c.cpu.r[0], 2);
    c.run(&mut ram, 1); // the rewritten instruction, not the cached one
    assert_eq!(c.cpu.r[0], 9, "the stale decoded instruction ran");
}

#[test]
fn cached_loop_runs_the_same_as_uncached() {
    // a counting loop run through the cache and without it must end in the same state
    let prog = [
        0xe3a0_0000u32, // mov r0, #0
        0xe280_0001,    // add r0, r0, #1
        0xe350_0064,    // cmp r0, #100
        0x1aff_fffc,    // bne 0x1004
        0xe1a0_1000,    // mov r1, r0
    ];
    let mut a = Cached::new();
    a.arm(0x1000, &prog);
    let mut b = Ram::new();
    b.arm(0x1000, &prog);
    let mut ca: Core<Cached> = Core::new();
    let mut cb: Core<Ram> = Core::new();
    ca.cpu.pc = 0x1000;
    cb.cpu.pc = 0x1000;
    assert_eq!(ca.run(&mut a, 305), cb.run(&mut b, 305));
    assert_eq!(ca.cpu.r, cb.cpu.r);
    assert_eq!(ca.cpu.cpsr, cb.cpu.cpsr);
    assert_eq!(ca.cpu.r[1], 100);
}
