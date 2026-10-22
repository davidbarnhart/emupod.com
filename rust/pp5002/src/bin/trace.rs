//! Boot the standard disk, replay an input script, and print checkpoints for comparison with
//! tests/trace_python.py (the Python core):   trace <osos> <disk> <script> <every> [compat]
//! Script lines: `<step> edges <n>`, `<step> key <name> down|up`, `end <step>` (tests/mkscript.py).
use pp5002::{Config, Machine};

fn crc32(data: &[u8], mut c: u32) -> u32 {
    static T: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = T.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            t[i as usize] = c;
        }
        t
    });
    c = !c;
    for &b in data {
        c = t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

struct Driver {
    m: Machine,
    step_n: u64,
    lcd_crc: u32,
    lcd_seen: usize,
    snd_crc: u32,
    snd_seen: usize,
}

impl Driver {
    fn step(&mut self) {
        self.m.settle(1, 1 << 30);
        self.step_n += 1;
    }
    fn key(&mut self, name: &str, down: bool) {
        let (port, mask) = match name {
            "select" => (0, 1 << 1),
            "menu" => (0, 1 << 4),
            "play" => (0, 1 << 2),
            "ff" => (0, 1 << 0),
            "rew" => (0, 1 << 3),
            _ => panic!("key {name}"),
        };
        self.m.set_pin(port, mask, !down);
        self.m.edge(port, mask);
    }
    fn edges(&mut self, n: i32) {
        for _ in 0..n.unsigned_abs() {
            let (a, b) = (self.m.pin(0, 0x80), self.m.pin(0, 0x40));
            let (mask, level) = if n > 0 {
                match (a, b) {
                    (false, false) => (0x80, true),
                    (true, false) => (0x40, true),
                    (true, true) => (0x80, false),
                    (false, true) => (0x40, false),
                }
            } else {
                match (a, b) {
                    (false, false) => (0x40, true),
                    (false, true) => (0x80, true),
                    (true, true) => (0x40, false),
                    (true, false) => (0x80, false),
                }
            };
            self.m.set_pin(0, mask, level);
            self.m.edge(0, mask);
            self.step();
        }
    }
    fn checkpoint(&mut self, tag: &str, full: bool) {
        let m = &mut self.m;
        for &(a, v) in &m.s.lcd_log[self.lcd_seen..] {
            self.lcd_crc = crc32(&a.to_le_bytes(), self.lcd_crc);
            self.lcd_crc = crc32(&v.to_le_bytes(), self.lcd_crc);
        }
        self.lcd_seen = m.s.lcd_log.len();
        for &s in &m.s.audio.samples[self.snd_seen..] {
            self.snd_crc = crc32(&s.to_le_bytes(), self.snd_crc);
        }
        self.snd_seen = m.s.audio.samples.len();
        let c = m.cpu_regs();
        let regs: Vec<String> = (0..15).map(|i| format!("{:08x}", c.r[i])).collect();
        let mut line = format!(
            "{} now={} lcd={} lcdcrc={:08x} ticks={} sleeps={} pc={:08x} cpsr={:08x} cop_asleep={} snd={} sndcrc={:08x} on={} fiqs={} hp={},{} {}",
            tag, m.now(), m.s.lcd_count, self.lcd_crc, m.s.ticks, m.s.sleeps, c.pc, c.cpsr, m.s.cop_asleep as u8,
            m.s.audio.samples.len() / 2, self.snd_crc, m.s.audio.on as u8, m.s.audio.fiqs, m.s.hp[0], m.s.hp[1], regs.join(" ")
        );
        if full {
            line += &format!(" dram={:08x} iram={:08x}", crc32(m.dram(), 0), crc32(m.iram(), 0));
        }
        println!("{}", line);
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let osos = std::fs::read(&a[1]).unwrap();
    let script = std::fs::read_to_string(&a[3]).unwrap();
    let every: u64 = a[4].parse().unwrap();
    let cfg = if a.get(5).map(|s| s.as_str()) == Some("compat") { Config::compat() } else { Config::default() };
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
    let m = Machine::new(&osos, &a[2], cfg).unwrap();
    let mut d = Driver { m, step_n: 0, lcd_crc: 0, lcd_seen: 0, snd_crc: 0, snd_seen: 0 };
    d.m.boot();
    d.checkpoint("boot", true);
    let mut i = 0;
    while d.step_n <= end {
        while i < events.len() && events[i].0 == d.step_n {
            let (_, kind, arg) = events[i].clone();
            match kind.as_str() {
                "edges" => d.edges(arg.parse().unwrap()),
                "key" => {
                    let w: Vec<&str> = arg.split_whitespace().collect();
                    d.key(w[0], w[1] == "down");
                }
                _ => panic!("event {kind}"),
            }
            i += 1;
        }
        d.step();
        if d.step_n % every == 0 {
            let full = d.step_n % (every * 10) == 0;
            let tag = format!("t{}", d.step_n);
            d.checkpoint(&tag, full);
        }
    }
}
