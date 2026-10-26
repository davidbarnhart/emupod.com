//! The LCD, decoded from the controller's write log (a port of `recording.Screen`): 160 x 128 pixels, 2-bit levels
//! (0 = white), one byte each.
//!
//! The controller's RAM rows are 21 words (168 pixels, the last word off the panel's 160). After each data word the address moves as
//! the entry mode (register 5) says: bit 2 set, down to the next row (+32, osos's 0x14); clear, along the row, wrapping within its 21
//! words; bit 4 picks increment or decrement. osos sets 0x14; the disk-mode program keeps the bootloader's 0 (it draws rows right to left).
pub const W: usize = 160;
pub const H: usize = 128;

pub struct Screen {
    idx: Option<u32>,
    ib: Vec<u32>,
    db: Vec<u32>,
    addr: u32,
    entry: u32,
    pub px: Vec<u8>,
}

impl Default for Screen {
    fn default() -> Screen {
        Screen { idx: None, ib: Vec::new(), db: Vec::new(), addr: 0, entry: 0x14, px: vec![0; W * H] }
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
                            Some(0x05) => self.entry = word,
                            Some(0x11) => self.addr = word,
                            Some(0x12) => {
                                self.put(self.addr, word);
                                self.addr = self.next(self.addr);
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

    fn next(&self, a: u32) -> u32 {
        let inc = self.entry & 0x10 != 0;
        if self.entry & 0x04 != 0 {
            if inc {
                a.wrapping_add(32)
            } else {
                a.wrapping_sub(32)
            }
        } else {
            let col = a & 0x1f;
            let col = if inc { (col + 1) % 21 } else { (col + 20) % 21 };
            (a & !0x1f) | col
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn word(log: &mut Vec<(u32, u32)>, idx: u32, v: u32) {
        log.extend([(0xc000_1008, 0), (0xc000_1008, idx), (0xc000_1010, v >> 8), (0xc000_1010, v & 0xff)]);
    }

    #[test]
    fn entry_mode_moves_the_address() {
        // osos's mode (the default): down the column
        let mut log = Vec::new();
        word(&mut log, 0x11, 0x20 + 20);
        word(&mut log, 0x12, 0xc000);
        word(&mut log, 0x12, 0xc000);
        let mut s = Screen::default();
        s.update(&log);
        assert_eq!((s.px[W], s.px[2 * W]), (3, 3));
        // the disk-mode program's: along the row, right to left, wrapping at 21 words
        let mut log = Vec::new();
        word(&mut log, 0x05, 0);
        word(&mut log, 0x11, 0x20);
        word(&mut log, 0x12, 0x0003); // word 0: off the panel
        word(&mut log, 0x12, 0x0003); // wraps to word 20: x 0..7
        word(&mut log, 0x12, 0x0003); // word 19: x 8..15
        let mut s = Screen::default();
        s.update(&log);
        assert_eq!((s.px[W + 7], s.px[W + 15], s.px[W + 23]), (3, 3, 0));
    }
}
