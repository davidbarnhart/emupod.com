//! How fast does the machine play a song?   bench <osos> <disk> <script> <extra_ipod_seconds> [compat|v2|live]
//! Boots, replays the script (it ends with a song playing), plays on for the extra seconds, and reports the speed
//! of the whole run and of the last stretch (steady playback).
use pp5002::{Config, Machine};
use std::time::Instant;

#[cfg(feature = "prof")]
mod prof {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    const N: usize = 1 << 20;
    static PCS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn handler(_: libc::c_int, _: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        let uc = ctx as *mut libc::ucontext_t;
        #[cfg(target_arch = "aarch64")]
        let pc = unsafe { (*uc).uc_mcontext.pc };
        #[cfg(target_arch = "x86_64")]
        let pc = unsafe { (*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64 };
        let i = NEXT.fetch_add(1, Ordering::Relaxed);
        if i < N {
            PCS[i].store(pc, Ordering::Relaxed);
        }
    }
    pub fn start() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigaction(libc::SIGPROF, &sa, std::ptr::null_mut());
            let tv = libc::timeval { tv_sec: 0, tv_usec: 1000 };
            let it = libc::itimerval { it_interval: tv, it_value: tv };
            libc::setitimer(libc::ITIMER_PROF, &it, std::ptr::null_mut());
        }
    }
    pub fn stop_and_dump(path: &str) {
        unsafe {
            let z = libc::itimerval { it_interval: libc::timeval { tv_sec: 0, tv_usec: 0 }, it_value: libc::timeval { tv_sec: 0, tv_usec: 0 } };
            libc::setitimer(libc::ITIMER_PROF, &z, std::ptr::null_mut());
        }
        let n = NEXT.load(Ordering::Relaxed).min(N);
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let exe = std::fs::read_link("/proc/self/exe").unwrap();
        let base = maps.lines().filter(|l| l.ends_with(exe.to_str().unwrap())).filter_map(|l| u64::from_str_radix(l.split('-').next().unwrap(), 16).ok()).min().unwrap_or(0);
        let mut out = format!("base {:x}\n", base);
        for i in 0..n {
            out += &format!("{:x}\n", PCS[i].load(Ordering::Relaxed));
        }
        std::fs::write(path, out).unwrap();
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let osos = std::fs::read(&a[1]).unwrap();
    let script = std::fs::read_to_string(&a[3]).unwrap();
    let extra: f64 = a[4].parse().unwrap();
    let mode = a.get(5).map(|s| s.as_str()).unwrap_or("v2");
    let mut cfg = if mode == "compat" { Config::compat() } else { Config::default() };
    if mode == "live" {
        cfg.audio_fpf = None;
        cfg.cop_ipus = 26;
        cfg.interleave_playing = 300;
        cfg.interleave_settle = 200_000;
    }
    let mut m = Machine::new(&osos, &a[2], cfg).unwrap();
    let t0 = Instant::now();
    m.boot();
    let boot = (t0.elapsed().as_secs_f64(), m.now());
    let mut events: Vec<(u64, String, String)> = vec![];
    let mut end = 0u64;
    for l in script.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.len() == 2 && w[0] == "end" {
            end = w[1].parse().unwrap();
        } else if w.len() >= 3 && w[0].parse::<u64>().is_ok() {
            events.push((w[0].parse().unwrap(), w[1].to_string(), w[2..].join(" ")));
        }
    }
    let mut step = 0u64;
    let mut i = 0;
    let mut do_step = |m: &mut Machine, step: &mut u64| {
        m.settle(1, 1 << 30);
        *step += 1;
    };
    while step <= end {
        while i < events.len() && events[i].0 == step {
            let (_, kind, arg) = events[i].clone();
            if kind == "edges" {
                let n: i32 = arg.parse().unwrap();
                for _ in 0..n.unsigned_abs() {
                    let (x, y) = (m.pin(0, 0x80), m.pin(0, 0x40));
                    let (mask, level) = if n > 0 {
                        match (x, y) {
                            (false, false) => (0x80, true),
                            (true, false) => (0x40, true),
                            (true, true) => (0x80, false),
                            (false, true) => (0x40, false),
                        }
                    } else {
                        match (x, y) {
                            (false, false) => (0x40, true),
                            (false, true) => (0x80, true),
                            (true, true) => (0x40, false),
                            (true, false) => (0x80, false),
                        }
                    };
                    m.set_pin(0, mask, level);
                    m.edge(0, mask);
                    do_step(&mut m, &mut step);
                }
            } else {
                let w: Vec<&str> = arg.split_whitespace().collect();
                let (port, mask) = match w[0] {
                    "select" => (0, 1 << 1),
                    "menu" => (0, 1 << 4),
                    "play" => (0, 1 << 2),
                    "ff" => (0, 1 << 0),
                    "rew" => (0, 1 << 3),
                    _ => panic!(),
                };
                m.set_pin(port, mask, w[1] != "down");
                m.edge(port, mask);
            }
            i += 1;
        }
        do_step(&mut m, &mut step);
    }
    let whole = (t0.elapsed().as_secs_f64(), m.now());
    // steady playback
    #[cfg(feature = "prof")]
    prof::start();
    let (w0, n0, c0, p0, f0) = (Instant::now(), m.now(), m.s.cpu_instr, m.s.cop_instr, m.s.audio.samples.len());
    let sk0 = m.s.skipped;
    let until = n0 + (extra * 1e6) as u64;
    while m.now() < until {
        m.settle(1, 1 << 30);
    }
    let dw = w0.elapsed().as_secs_f64();
    #[cfg(feature = "prof")]
    prof::stop_and_dump("/tmp/bench.prof");
    let dn = (m.now() - n0) as f64 / 1e6;
    println!("decoded-instruction cache: CPU {} decodes, {} page flushes; COP {} decodes, {} page flushes (whole run)", m.cpu.decodes, m.cpu.flushes, m.cop.decodes, m.cop.flushes);
    println!("skipped as spin loops (steady playback): CPU {:.1}% COP {:.1}% of their instructions", 100.0 * (m.s.skipped[0] - sk0[0]) as f64 / (m.s.cpu_instr - c0) as f64, 100.0 * (m.s.skipped[1] - sk0[1]) as f64 / (m.s.cop_instr - p0) as f64);
    println!(
        "{mode}: boot {:.1} s of iPod time in {:.2} s wall | whole run to the song: {:.1} s in {:.2} s | steady playback: {:.1} s of iPod time in {:.2} s wall = {:.2}x real time, CPU {:.1} MIPS + COP {:.1} MIPS of the iPod's, sound {:.2} s per iPod second, {:.1} M instr/s host total",
        boot.1 as f64 / 1e6, boot.0, whole.1 as f64 / 1e6, whole.0, dn, dw, dn / dw,
        (m.s.cpu_instr - c0) as f64 / dn / 1e6, (m.s.cop_instr - p0) as f64 / dn / 1e6,
        ((m.s.audio.samples.len() - f0) / 2) as f64 / 44100.0 / dn,
        ((m.s.cpu_instr - c0) + (m.s.cop_instr - p0)) as f64 / dw / 1e6
    );
    #[cfg(feature = "stats")]
    print!("{}", armcore::stats::report(28));
}
