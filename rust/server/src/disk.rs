//! The card a session runs on, shared by every machine of the session: the normal one, and the one booting from the chip in disk mode.
//! A kept card is written through to its file. Any other (the built-in card, a card booted with its changes discarded) keeps its
//! writes in memory, up to `CAP_SECTORS`, and its file is opened read-only: nothing this session does can change it.
use pp5002::ata::Disk;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

/// the most a card whose changes are discarded keeps: 256 MB of written sectors
pub const CAP_SECTORS: usize = 256 * 2048;

pub struct SessionDisk {
    f: File,
    pub keep: bool,
    len: u64,
    over: Mutex<HashMap<u64, Box<[u8; 512]>>>,
}

impl SessionDisk {
    pub fn open(path: &str, keep: bool) -> std::io::Result<Arc<SessionDisk>> {
        let f = if keep { std::fs::OpenOptions::new().read(true).write(true).open(path)? } else { File::open(path)? };
        let len = f.metadata()?.len();
        Ok(Arc::new(SessionDisk { f, keep, len, over: Mutex::new(HashMap::new()) }))
    }

    /// Whether `n` more sectors from `lba` fit (a kept card: always).
    pub fn room(&self, lba: u64, n: u64) -> bool {
        if self.keep {
            return true;
        }
        let over = self.over.lock().unwrap();
        let new = (lba..lba + n).filter(|l| !over.contains_key(l)).count();
        over.len() + new <= CAP_SECTORS
    }

    pub fn sync(&self) {
        if self.keep {
            let _ = self.f.sync_data();
        }
    }
}

/// One machine's handle on the session's disk.
pub struct DiskRef(pub Arc<SessionDisk>);

impl Disk for DiskRef {
    fn len(&self) -> u64 {
        self.0.len
    }

    fn read_at(&self, buf: &mut [u8], off: u64) {
        let d = &self.0;
        let _ = d.f.read_at(buf, off);
        let over = d.over.lock().unwrap();
        if over.is_empty() || buf.is_empty() {
            return;
        }
        let end = off + buf.len() as u64;
        for l in off / 512..end.div_ceil(512) {
            if let Some(s) = over.get(&l) {
                let (a, b) = ((l * 512).max(off), ((l + 1) * 512).min(end));
                buf[(a - off) as usize..(b - off) as usize].copy_from_slice(&s[(a - l * 512) as usize..(b - l * 512) as usize]);
            }
        }
    }

    fn write_at(&self, buf: &[u8], off: u64) {
        let d = &self.0;
        if d.keep {
            let _ = d.f.write_at(buf, off);
            return;
        }
        let end = off + buf.len() as u64;
        for l in off / 512..end.div_ceil(512) {
            let mut s = [0u8; 512];
            self.read_at(&mut s, l * 512);
            let (a, b) = ((l * 512).max(off), ((l + 1) * 512).min(end));
            s[(a - l * 512) as usize..(b - l * 512) as usize].copy_from_slice(&buf[(a - off) as usize..(b - off) as usize]);
            let mut over = d.over.lock().unwrap();
            if over.len() < CAP_SECTORS || over.contains_key(&l) {
                over.insert(l, Box::new(s));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(tag: &str) -> String {
        let p = std::env::temp_dir().join(format!("emupod-disk-{tag}-{}.img", std::process::id()));
        std::fs::write(&p, (0..4096u32).map(|k| k as u8).collect::<Vec<u8>>()).unwrap();
        p.display().to_string()
    }

    #[test]
    fn discarded_writes_stay_in_memory_and_read_back() {
        let p = card("discard");
        let d = DiskRef(SessionDisk::open(&p, false).unwrap());
        d.write_at(&[0xaa; 700], 300);
        let mut b = vec![0u8; 1200];
        d.read_at(&mut b, 100);
        let want: Vec<u8> = (100..1300u32).map(|k| if (300..1000).contains(&k) { 0xaa } else { k as u8 }).collect();
        assert_eq!(b, want);
        // a second machine on the same session sees them
        let e = DiskRef(d.0.clone());
        let mut c = vec![0u8; 512];
        e.read_at(&mut c, 512);
        assert_eq!(c[..], want[412..924]);
        assert_eq!(std::fs::read(&p).unwrap(), (0..4096u32).map(|k| k as u8).collect::<Vec<u8>>(), "the file is untouched");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn kept_writes_reach_the_file() {
        let p = card("keep");
        let d = DiskRef(SessionDisk::open(&p, true).unwrap());
        d.write_at(&[0x55; 512], 1024);
        assert_eq!(std::fs::read(&p).unwrap()[1024..1536], [0x55; 512]);
        assert!(d.0.room(0, 1 << 30));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn room_counts_only_new_sectors() {
        let p = card("room");
        let d = DiskRef(SessionDisk::open(&p, false).unwrap());
        d.write_at(&[1; 1024], 0);
        assert!(d.0.room(0, CAP_SECTORS as u64));
        assert!(!d.0.room(2, CAP_SECTORS as u64));
        std::fs::remove_file(p).ok();
    }
}
