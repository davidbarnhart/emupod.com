//! The LCD, decoded from the controller's write log (a port of `recording.Screen`): 160 x 128 pixels, 2-bit levels
//! (0 = white), one byte each.
pub const W: usize = 160;
pub const H: usize = 128;

pub struct Screen {
    idx: Option<u32>,
    ib: Vec<u32>,
    db: Vec<u32>,
    addr: u32,
    pub px: Vec<u8>,
}

impl Default for Screen {
    fn default() -> Screen {
        Screen { idx: None, ib: Vec::new(), db: Vec::new(), addr: 0, px: vec![0; W * H] }
    }
}

impl Screen {
    /// Take in controller writes (address, value); returns whether there were any.
    pub fn update(&mut self, log: &[(u32, u32)]) -> bool {
        for &(a, v) in log {
            match a {
                0xc000_1008 => {
                    self.ib.push(v & 0xff);
                    if self.ib.len() == 2 {
                        self.idx = Some((self.ib[0] << 8) | self.ib[1]);
                        self.ib.clear();
                    }
                }
                0xc000_1010 => {
                    self.db.push(v & 0xff);
                    if self.db.len() == 2 {
                        let word = (self.db[0] << 8) | self.db[1];
                        self.db.clear();
                        match self.idx {
                            Some(0x11) => self.addr = word,
                            Some(0x12) => {
                                self.put(self.addr, word);
                                self.addr = self.addr.wrapping_add(32);
                            }
                            _ => {}
                        }
                    }
                }
                0xc000_1000 => {
                    self.ib.clear();
                    self.db.clear();
                }
                _ => {}
            }
        }
        !log.is_empty()
    }

    fn put(&mut self, addr: u32, word: u32) {
        let y = (addr >> 5) as usize;
        let wcol = (addr & 0x1f) as i32 - 1;
        if y >= H || wcol < 0 {
            return;
        }
        for b in 0..8usize {
            let x = (19 - wcol as usize) * 8 + b;
            if x < W {
                self.px[y * W + x] = ((word >> (14 - 2 * b)) & 3) as u8;
            }
        }
    }
}
