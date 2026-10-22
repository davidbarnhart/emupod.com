//! Instruction-mix statistics (feature `stats`): what the firmware actually executes. Not for speed runs.
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Default)]
pub struct Stats {
    pub total: u64,
    pub arm: u64,
    pub skipped_cond: u64,
    pub taken: u64,
    pub cond_total: [u64; 16],
    pub hist: HashMap<(bool, u32), u64>,
    pub pairs: HashMap<((bool, u32), (bool, u32)), u64>,
    pub prev: Option<(bool, u32)>,
    pub pcs: Vec<u64>, // a bitmap of 4-byte words in the 32 MB of DRAM (and its aliases) that were executed
    pub other_pcs: HashMap<u32, ()>,
    pub block_len: u64,
    pub blocks: u64,
}

thread_local! {
    pub static S: RefCell<Stats> = RefCell::new(Stats { pcs: vec![0u64; (0x200_0000 / 4) / 64], ..Default::default() });
}

#[inline(never)]
pub fn record(pc: u32, thumb: bool, idx: u32, ran: bool, taken: bool) {
    S.with(|s| {
        let s = &mut *s.borrow_mut();
        s.total += 1;
        if !thumb {
            s.arm += 1;
        }
        if !ran {
            s.skipped_cond += 1;
        }
        let k = (thumb, idx);
        *s.hist.entry(k).or_insert(0) += 1;
        if let Some(p) = s.prev {
            *s.pairs.entry((p, k)).or_insert(0) += 1;
        }
        s.prev = Some(k);
        if taken {
            s.taken += 1;
            s.blocks += 1;
        }
        s.block_len += 1;
        let region = pc >> 25;
        if region == 0 || region == 2 || region == 20 {
            let w = ((pc & 0x1ff_ffff) >> 2) as usize;
            s.pcs[w / 64] |= 1 << (w % 64);
        } else {
            s.other_pcs.insert(pc, ());
        }
    });
}

#[inline(never)]
pub fn cond(c: u32, _ran: bool) {
    S.with(|s| s.borrow_mut().cond_total[c as usize] += 1);
}

/// A short name for an ARM handler index (bits 27:20 and 7:4 of the instruction).
pub fn arm_name(idx: u32) -> String {
    let hi = idx >> 4;
    let lo = idx & 15;
    let s = hi & 1 != 0;
    match hi >> 5 {
        0 => {
            if lo & 9 == 9 {
                if lo == 9 {
                    if hi >> 2 == 0 { "mul/mla".into() } else if hi >> 3 == 1 { "long multiply".into() } else { "swp".into() }
                } else {
                    format!("{} halfword/signed", if s { "ldr" } else { "str" })
                }
            } else if hi == 0x12 && lo == 1 {
                "bx".into()
            } else if lo == 0 && (hi & 0xfb == 0x10 || hi & 0xfb == 0x12) {
                "mrs/msr".into()
            } else {
                format!("dp {} {}{}", ["and", "eor", "sub", "rsb", "add", "adc", "sbc", "rsc", "tst", "teq", "cmp", "cmn", "orr", "mov", "bic", "mvn"][((hi >> 1) & 15) as usize], if s { "s " } else { "" }, if lo & 1 != 0 { "reg-shift" } else { "imm-shift" })
            }
        }
        1 => format!("dp {} {}imm", ["and", "eor", "sub", "rsb", "add", "adc", "sbc", "rsc", "tst", "teq", "cmp", "cmn", "orr", "mov", "bic", "mvn"][((hi >> 1) & 15) as usize], if s { "s " } else { "" }),
        2 => format!("{}{} imm-offset", if s { "ldr" } else { "str" }, if hi & 4 != 0 { "b" } else { "" }),
        3 => format!("{}{} reg-offset", if s { "ldr" } else { "str" }, if hi & 4 != 0 { "b" } else { "" }),
        4 => format!("{}m{}", if s { "ld" } else { "st" }, ["da", "ia", "db", "ib"][((hi >> 3) & 3) as usize]),
        5 => if hi & 0x10 != 0 { "bl".into() } else { "b".into() },
        _ => "other".into(),
    }
}

pub fn report(top: usize) -> String {
    S.with(|s| {
        let s = s.borrow();
        let mut out = String::new();
        let pct = |n: u64| 100.0 * n as f64 / s.total as f64;
        let words: u64 = s.pcs.iter().map(|w| w.count_ones() as u64).sum();
        out += &format!(
            "{} instructions, {:.1}% ARM; {:.1}% skipped by their condition; {:.1}% taken branches (a taken branch every {:.1} instructions); {} distinct code words in DRAM ({} KB) + {} elsewhere\n",
            s.total, pct(s.arm), pct(s.skipped_cond), pct(s.taken), s.total as f64 / s.taken.max(1) as f64, words, words * 4 / 1024, s.other_pcs.len()
        );
        let al = s.cond_total[14];
        out += &format!("condition: always {:.1}%, other {:.1}% (", 100.0 * al as f64 / s.arm as f64, 100.0 * (s.arm - al) as f64 / s.arm as f64);
        for (i, n) in s.cond_total.iter().enumerate() {
            if i != 14 && *n > 0 {
                out += &format!("{}:{:.1}% ", ["eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al", "nv"][i], 100.0 * *n as f64 / s.arm as f64);
            }
        }
        out += ")\n";
        let mut h: Vec<_> = s.hist.iter().collect();
        h.sort_by(|a, b| b.1.cmp(a.1));
        out += "top handlers:\n";
        let mut cum = 0.0;
        for ((th, idx), n) in h.iter().take(top) {
            cum += pct(**n);
            out += &format!("  {:5.1}% (cum {:5.1}%)  {}\n", pct(**n), cum, if *th { format!("thumb {:#x}", idx) } else { arm_name(*idx) });
        }
        let mut p: Vec<_> = s.pairs.iter().collect();
        p.sort_by(|a, b| b.1.cmp(a.1));
        out += "top pairs:\n";
        let mut cum = 0.0;
        for (((t1, i1), (t2, i2)), n) in p.iter().take(top) {
            cum += pct(**n);
            let nm = |t: &bool, i: &u32| if *t { format!("thumb {:#x}", i) } else { arm_name(*i) };
            out += &format!("  {:5.1}% (cum {:5.1}%)  {}  ->  {}\n", pct(**n), cum, nm(t1, i1), nm(t2, i2));
        }
        out
    })
}
