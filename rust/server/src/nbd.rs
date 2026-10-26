//! The iPod in disk mode as a block device for this machine (or a private network): an NBD server (the newstyle protocol,
//! https://github.com/NetworkBlockDevice/nbd/blob/master/doc/proto.md), one export, "ipod".
//!
//! It never touches the card itself. Each read and write goes to the emulator thread as a request, which hands it to the iPod's disk-mode
//! program as SBP-2 READ(10)/WRITE(10) ORBs (`pp5002::sbp2`), the way a computer on its FireWire port would. The export exists only
//! while the iPod is in disk mode and a host is logged in; when it leaves disk mode the open connections' requests fail (ESHUTDOWN), as a
//! pulled disk's would.
//!
//! On this machine, the mount service (`mount_service`, set up once with `sudo emupod setup-automount`) attaches and mounts it when the
//! iPod enters disk mode, and unmounts it before the cable comes out. By hand (root): `sudo modprobe nbd max_part=8`, then
//! `sudo nbd-client 127.0.0.1 10809 /dev/nbd0 -N ipod` (or `sudo qemu-nbd -f raw -c /dev/nbd0 nbd://127.0.0.1:10809/ipod`), then
//! `sudo mount /dev/nbd0p2 /media/emupod`. Before the cable comes out: `sudo umount /media/emupod` and `sudo nbd-client -d /dev/nbd0` (`qemu-nbd -d`).
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub const EXPORT: &str = "ipod";
const NBDMAGIC: u64 = 0x4e42_444d_4147_4943;
const IHAVEOPT: u64 = 0x4948_4156_454f_5054;
const REPLY_MAGIC: u64 = 0x3_e889_0455_65a9;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const SIMPLE_REPLY: u32 = 0x6744_6698;
/// the most one request may move (the emulator thread splits it into ORBs)
const MAX_PAYLOAD: u32 = 4 << 20;
pub const EPERM: u32 = 1;
pub const EIO: u32 = 5;
pub const EINVAL: u32 = 22;
pub const ENOSPC: u32 = 28;
pub const ESHUTDOWN: u32 = 108;

pub enum Op {
    /// (first sector, sectors)
    Read(u64, u32),
    /// (first sector, whole sectors)
    Write(u64, Vec<u8>),
    Flush,
    /// the computer is done with the disk: SCSI START STOP UNIT (eject), as a computer sends when you eject the iPod, and its firmware
    /// shows "OK to disconnect"
    Eject,
    /// a host's SCSI command, as iSCSI gives it: the CDB (at most 12 bytes), the bytes it may send back or must take, and the host's
    /// data for a write; answered with `iscsi::encode`'s bytes (the SCSI status, sense and data)
    Scsi(Vec<u8>, u32, Option<Vec<u8>>),
}

/// The two kinds of host, of which only one may have the disk at a time: two kernels writing one FAT would wreck it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Via {
    Nbd = 0,
    Iscsi = 1,
}

/// One host of the disk, while it lasts (`Hub::hold`).
pub struct Held<'a>(&'a Hub, Via);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let mut h = self.0.holders.lock().unwrap();
        h[self.1 as usize] -= 1;
    }
}

/// One request for the emulator thread: the export generation it was made against, and where the answer goes (the bytes read, or an
/// errno).
pub struct Req {
    pub gen: u64,
    pub op: Op,
    pub reply: mpsc::Sender<Result<Vec<u8>, u32>>,
}

#[derive(Clone, Debug)]
pub struct Export {
    /// 512-byte sectors, as the iPod's READ CAPACITY says
    pub blocks: u64,
    pub writable: bool,
    /// a new number each time disk mode starts: a connection from before answers ESHUTDOWN
    pub gen: u64,
}

#[derive(Default)]
pub struct Hub {
    pub reqs: Mutex<VecDeque<Req>>,
    pub export: Mutex<Option<Export>>,
    pub clients: AtomicUsize,
    /// the last export generation given (it only grows, across restarts of the iPod too)
    pub gen: AtomicU64,
    /// where it listens, for the page
    pub addr: Mutex<Option<String>>,
    /// where the iSCSI target listens, for the page
    pub iscsi_addr: Mutex<Option<String>>,
    /// the hosts that have the disk now, of each kind (`Via`)
    pub holders: Mutex<[usize; 2]>,
}

impl Hub {
    /// Serve a new export (disk mode has started): connections from before get ESHUTDOWN.
    pub fn open(&self, blocks: u64, writable: bool) {
        let gen = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        *self.export.lock().unwrap() = Some(Export { blocks, writable, gen });
    }

    /// The current export's generation (0: none).
    pub fn current(&self) -> u64 {
        self.export.lock().unwrap().as_ref().map(|e| e.gen).unwrap_or(0)
    }

    /// Have the disk as a host of kind `v`, unless the other kind has it.
    pub fn hold(&self, v: Via) -> Option<Held<'_>> {
        let mut h = self.holders.lock().unwrap();
        if h[1 - v as usize] > 0 {
            return None;
        }
        h[v as usize] += 1;
        Some(Held(self, v))
    }

    /// One request to the emulator thread, from this server itself (not a connection): its answer, or an errno (EIO if none came
    /// in `timeout`).
    pub fn ask(&self, gen: u64, op: Op, timeout: Duration) -> Result<Vec<u8>, u32> {
        let (tx, rx) = mpsc::channel();
        self.reqs.lock().unwrap().push_back(Req { gen, op, reply: tx });
        rx.recv_timeout(timeout).unwrap_or(Err(EIO))
    }
}

/// Listen on `addr` (a loopback or private-network address; never an unspecified one, 0.0.0.0 or ::, which would offer the card to every
/// network this machine is on).
pub fn listen(hub: Arc<Hub>, addr: &str) -> Result<(), String> {
    let sa: std::net::SocketAddr = addr.parse().map_err(|e| format!("{addr}: {e}"))?;
    if sa.ip().is_unspecified() {
        return Err(format!("{addr}: an unspecified address would serve every network; give a loopback or private-network address"));
    }
    let l = TcpListener::bind(sa).map_err(|e| format!("{addr}: {e}"))?;
    *hub.addr.lock().unwrap() = Some(addr.to_string());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let hub = hub.clone();
            std::thread::spawn(move || {
                hub.clients.fetch_add(1, Ordering::SeqCst);
                let r = serve(&hub, s);
                hub.clients.fetch_sub(1, Ordering::SeqCst);
                if let Err(e) = r {
                    eprintln!("[nbd] {e}");
                }
            });
        }
    });
    Ok(())
}

fn rd<const N: usize>(s: &mut TcpStream) -> std::io::Result<[u8; N]> {
    let mut b = [0u8; N];
    s.read_exact(&mut b)?;
    Ok(b)
}

fn opt_reply(s: &mut TcpStream, opt: u32, typ: u32, data: &[u8]) -> std::io::Result<()> {
    let mut b = Vec::with_capacity(20 + data.len());
    b.extend_from_slice(&REPLY_MAGIC.to_be_bytes());
    b.extend_from_slice(&opt.to_be_bytes());
    b.extend_from_slice(&typ.to_be_bytes());
    b.extend_from_slice(&(data.len() as u32).to_be_bytes());
    b.extend_from_slice(data);
    s.write_all(&b)
}

const REP_ACK: u32 = 1;
const REP_SERVER: u32 = 2;
const REP_INFO: u32 = 3;
const REP_ERR_UNSUP: u32 = 0x8000_0001;
const REP_ERR_POLICY: u32 = 0x8000_0002;
const REP_ERR_UNKNOWN: u32 = 0x8000_0006;
const FLAG_HAS_FLAGS: u16 = 1;
const FLAG_READ_ONLY: u16 = 2;
const FLAG_SEND_FLUSH: u16 = 4;
const INFO_EXPORT: u16 = 0;

fn tflags(e: &Export) -> u16 {
    FLAG_HAS_FLAGS | FLAG_SEND_FLUSH | if e.writable { 0 } else { FLAG_READ_ONLY }
}

/// One connection: the handshake, then its requests until it leaves.
fn serve(hub: &Hub, mut s: TcpStream) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    s.set_nodelay(true).ok();
    let mut hello = NBDMAGIC.to_be_bytes().to_vec();
    hello.extend_from_slice(&IHAVEOPT.to_be_bytes());
    hello.extend_from_slice(&3u16.to_be_bytes()); // fixed newstyle, no zeroes
    s.write_all(&hello).map_err(io)?;
    let no_zeroes = u32::from_be_bytes(rd::<4>(&mut s).map_err(io)?) & 2 != 0;
    let mut held = None;
    let export = loop {
        let h = rd::<16>(&mut s).map_err(io)?;
        if u64::from_be_bytes(h[0..8].try_into().unwrap()) != IHAVEOPT {
            return Err("not an option".into());
        }
        let opt = u32::from_be_bytes(h[8..12].try_into().unwrap());
        let len = u32::from_be_bytes(h[12..16].try_into().unwrap());
        if len > 4096 {
            return Err(format!("option {opt}: {len} bytes"));
        }
        let mut data = vec![0u8; len as usize];
        s.read_exact(&mut data).map_err(io)?;
        let now = hub.export.lock().unwrap().clone();
        match opt {
            1 => {
                // EXPORT_NAME: no way to say no but to hang up
                let Some(e) = now else { return Ok(()) };
                held = hub.hold(Via::Nbd);
                if held.is_none() {
                    return Err("refused: an iSCSI host has the disk".into());
                }
                let mut b = (e.blocks * 512).to_be_bytes().to_vec();
                b.extend_from_slice(&tflags(&e).to_be_bytes());
                if !no_zeroes {
                    b.extend_from_slice(&[0; 124]);
                }
                s.write_all(&b).map_err(io)?;
                break e;
            }
            6 | 7 => {
                // INFO, GO: the export's size and flags, and its block sizes (whole sectors)
                let Some(e) = now else {
                    opt_reply(&mut s, opt, REP_ERR_UNKNOWN, b"the iPod is not in disk mode").map_err(io)?;
                    continue;
                };
                if opt == 7 && held.is_none() {
                    held = hub.hold(Via::Nbd);
                    if held.is_none() {
                        opt_reply(&mut s, opt, REP_ERR_POLICY, b"an iSCSI host has the iPod's disk").map_err(io)?;
                        continue;
                    }
                }
                let mut info = 0u16.to_be_bytes().to_vec();
                info.extend_from_slice(&(e.blocks * 512).to_be_bytes());
                info.extend_from_slice(&tflags(&e).to_be_bytes());
                opt_reply(&mut s, opt, REP_INFO, &info).map_err(io)?;
                let mut bs = 3u16.to_be_bytes().to_vec();
                for v in [512u32, 4096, MAX_PAYLOAD] {
                    bs.extend_from_slice(&v.to_be_bytes());
                }
                opt_reply(&mut s, opt, REP_INFO, &bs).map_err(io)?;
                opt_reply(&mut s, opt, REP_ACK, &[]).map_err(io)?;
                if opt == 7 {
                    break e;
                }
            }
            2 => {
                opt_reply(&mut s, opt, REP_ACK, &[]).map_err(io)?;
                return Ok(());
            }
            3 => {
                let mut b = (EXPORT.len() as u32).to_be_bytes().to_vec();
                b.extend_from_slice(EXPORT.as_bytes());
                opt_reply(&mut s, opt, REP_SERVER, &b).map_err(io)?;
                opt_reply(&mut s, opt, REP_ACK, &[]).map_err(io)?;
            }
            _ => opt_reply(&mut s, opt, REP_ERR_UNSUP, &[]).map_err(io)?,
        }
    };
    eprintln!("[nbd] {} attached ({} sectors{})", s.peer_addr().map(|a| a.to_string()).unwrap_or_default(), export.blocks,
              if export.writable { "" } else { ", read-only" });
    loop {
        let h = match rd::<28>(&mut s) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };
        if u32::from_be_bytes(h[0..4].try_into().unwrap()) != REQUEST_MAGIC {
            return Err("bad request magic".into());
        }
        let typ = u16::from_be_bytes(h[6..8].try_into().unwrap());
        let handle = &h[8..16];
        let off = u64::from_be_bytes(h[16..24].try_into().unwrap());
        let len = u32::from_be_bytes(h[24..28].try_into().unwrap());
        let mut payload = Vec::new();
        if typ == 1 {
            if len > MAX_PAYLOAD {
                return Err(format!("a write of {len} bytes"));
            }
            payload = vec![0u8; len as usize];
            s.read_exact(&mut payload).map_err(io)?;
        }
        let fits = off % 512 == 0 && len % 512 == 0 && len <= MAX_PAYLOAD && off.checked_add(len as u64).is_some_and(|e| e <= export.blocks * 512);
        let op = match typ {
            2 => return Ok(()),
            0 if fits => Ok(Op::Read(off / 512, len / 512)),
            1 if !export.writable => Err(EPERM),
            1 if fits => Ok(Op::Write(off / 512, payload)),
            3 => Ok(Op::Flush),
            _ => Err(EINVAL),
        };
        let answer = op.and_then(|op| hub.ask(export.gen, op, Duration::from_secs(120)));
        let mut b = SIMPLE_REPLY.to_be_bytes().to_vec();
        b.extend_from_slice(&answer.as_ref().err().copied().unwrap_or(0).to_be_bytes());
        b.extend_from_slice(handle);
        if let (0, Ok(data)) = (typ, &answer) {
            b.extend_from_slice(data);
        }
        s.write_all(&b).map_err(io)?;
    }
}

/// The client's side of the handshake, for the mount service: fixed newstyle, then NBD_OPT_GO for `name`; the export's size in bytes and
/// its transmission flags. The stream is then ready for requests (the mount service hands it to the kernel).
pub fn client_go(s: &mut TcpStream, name: &str) -> Result<(u64, u16), String> {
    let io = |e: std::io::Error| e.to_string();
    let hello = rd::<18>(s).map_err(io)?;
    if u64::from_be_bytes(hello[0..8].try_into().unwrap()) != NBDMAGIC || u64::from_be_bytes(hello[8..16].try_into().unwrap()) != IHAVEOPT {
        return Err("not an NBD server (newstyle)".into());
    }
    let sflags = u16::from_be_bytes(hello[16..18].try_into().unwrap());
    if sflags & 1 == 0 {
        return Err("the server does not speak fixed newstyle".into());
    }
    s.write_all(&(sflags as u32 & 3).to_be_bytes()).map_err(io)?;
    let mut data = (name.len() as u32).to_be_bytes().to_vec();
    data.extend_from_slice(name.as_bytes());
    data.extend_from_slice(&0u16.to_be_bytes()); // no information requests: the export's own (size, flags) always comes
    let mut b = IHAVEOPT.to_be_bytes().to_vec();
    b.extend_from_slice(&7u32.to_be_bytes());
    b.extend_from_slice(&(data.len() as u32).to_be_bytes());
    b.extend_from_slice(&data);
    s.write_all(&b).map_err(io)?;
    let mut export = None;
    loop {
        let h = rd::<20>(s).map_err(io)?;
        if u64::from_be_bytes(h[0..8].try_into().unwrap()) != REPLY_MAGIC {
            return Err("bad option reply".into());
        }
        let typ = u32::from_be_bytes(h[12..16].try_into().unwrap());
        let len = u32::from_be_bytes(h[16..20].try_into().unwrap());
        if len > 4096 {
            return Err(format!("an option reply of {len} bytes"));
        }
        let mut d = vec![0u8; len as usize];
        s.read_exact(&mut d).map_err(io)?;
        match typ {
            REP_ACK => return export.ok_or_else(|| "the server did not say the export's size".to_string()),
            REP_INFO if d.len() >= 12 && u16::from_be_bytes([d[0], d[1]]) == INFO_EXPORT => {
                export = Some((u64::from_be_bytes(d[2..10].try_into().unwrap()), u16::from_be_bytes([d[10], d[11]])));
            }
            REP_INFO => {}
            t if t & 0x8000_0000 != 0 => return Err(if d.is_empty() { format!("refused ({t:#x})") } else { String::from_utf8_lossy(&d).into_owned() }),
            t => return Err(format!("unexpected option reply {t}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(hub: Arc<Hub>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let hub = hub.clone();
                std::thread::spawn(move || serve(&hub, s));
            }
        });
        addr
    }

    #[test]
    fn client_go_gets_the_export_and_can_read() {
        let hub = Arc::new(Hub::default());
        hub.open(1000, true);
        let mut s = TcpStream::connect(server(hub.clone())).unwrap();
        assert_eq!(client_go(&mut s, EXPORT).unwrap(), (1000 * 512, FLAG_HAS_FLAGS | FLAG_SEND_FLUSH));
        // a read on the same stream: the emulator thread's side answered here by hand
        let mut r = REQUEST_MAGIC.to_be_bytes().to_vec();
        r.extend_from_slice(&[0, 0, 0, 0]);
        r.extend_from_slice(&7u64.to_be_bytes());
        r.extend_from_slice(&512u64.to_be_bytes());
        r.extend_from_slice(&512u32.to_be_bytes());
        s.write_all(&r).unwrap();
        let req = loop {
            if let Some(q) = hub.reqs.lock().unwrap().pop_front() {
                break q;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(req.op, Op::Read(1, 1)));
        req.reply.send(Ok(vec![0xab; 512])).unwrap();
        let h = rd::<16>(&mut s).unwrap();
        assert_eq!(u32::from_be_bytes(h[0..4].try_into().unwrap()), SIMPLE_REPLY);
        assert_eq!(&h[8..16], &7u64.to_be_bytes());
        assert_eq!(rd::<512>(&mut s).unwrap(), [0xab; 512]);
    }

    #[test]
    fn client_go_without_disk_mode_is_refused() {
        let hub = Arc::new(Hub::default());
        let mut s = TcpStream::connect(server(hub)).unwrap();
        assert_eq!(client_go(&mut s, EXPORT).unwrap_err(), "the iPod is not in disk mode");
    }
}
