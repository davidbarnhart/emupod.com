//! The lock screen's cover: the LCD frame (160 x 128 levels 0-3) centred on a square of the iPod's face colour, as a 6-colour indexed
//! PNG (web.py's cover_png, the same picture). What iOS shows as the artwork on the lock screen and in Control Center.
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;

const SIZE: usize = 832;
const SCALE: usize = 5;
const PAD: usize = 12;
const PALETTE: [u8; 18] = [236, 236, 238, 59, 62, 56, 182, 188, 169, 130, 136, 118, 80, 85, 70, 34, 38, 30];

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, t: &[u8], d: &[u8]) {
    out.extend_from_slice(&(d.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(t);
    out.extend_from_slice(d);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// `px`: the frame's levels (160 * 128), or None before the first frame (the screen is drawn as bezel).
pub fn png(px: Option<&[u8]>) -> Vec<u8> {
    let (w, h) = (160 * SCALE, 128 * SCALE);
    let (x0, y0) = ((SIZE - w) / 2, (SIZE - h) / 2);
    let mut raw = Vec::with_capacity((SIZE + 1) * SIZE);
    for y in 0..SIZE {
        raw.push(0); // filter: none
        if y >= y0 && y < y0 + h && px.is_some() {
            let r = (y - y0) / SCALE;
            let p = px.unwrap();
            raw.extend(std::iter::repeat(0).take(x0 - PAD));
            raw.extend(std::iter::repeat(1).take(PAD));
            for x in 0..160 {
                raw.extend(std::iter::repeat(2 + p[r * 160 + x].min(3)).take(SCALE));
            }
            raw.extend(std::iter::repeat(1).take(PAD));
            raw.extend(std::iter::repeat(0).take(SIZE - x0 - w - PAD));
        } else if y + PAD >= y0 && y < y0 + h + PAD {
            raw.extend(std::iter::repeat(0).take(x0 - PAD));
            raw.extend(std::iter::repeat(1).take(w + 2 * PAD));
            raw.extend(std::iter::repeat(0).take(SIZE - x0 - w - PAD));
        } else {
            raw.extend(std::iter::repeat(0).take(SIZE));
        }
    }
    let mut z = ZlibEncoder::new(Vec::new(), Compression::new(6));
    let _ = z.write_all(&raw);
    let idat = z.finish().unwrap_or_default();
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(SIZE as u32).to_be_bytes());
    ihdr.extend_from_slice(&(SIZE as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 3, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"PLTE", &PALETTE);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}
