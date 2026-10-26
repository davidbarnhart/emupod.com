//! The iPod in disk mode as an iSCSI target (RFC 7143), for this machine or a private network: one target, `TARGET`, one LUN.
//!
//! It passes the host's own SCSI commands on to the iPod's disk-mode program as SBP-2 command ORBs (`Op::Scsi`, `sbp2::Session::exec`),
//! the way a computer on its FireWire port would send them, so the iPod's firmware answers them: INQUIRY, READ CAPACITY, READ/WRITE(10),
//! START STOP UNIT (a host's eject, which shows "OK to disconnect"), and the rest. The target answers only what an ORB cannot carry or
//! the program does not know (`scsiprobe` lists what it takes): REPORT LUNS, REQUEST SENSE, and the 16-byte commands, which become their
//! 10-byte forms (a card is far below 2 TB).
//!
//! A normal session needs CHAP (one-way, MD5): the user is `CHAP_USER`, the secret is in a file of its own (mode 0600, made on first use;
//! never logged or shown). Discovery needs none: it names the target and nothing else. One session at a time, and never while an NBD
//! host has the disk (`nbd::Hub::hold`). A login is taken only while the iPod is in disk mode; when it leaves disk mode the connection
//! is closed, as a pulled cable would end it.
//!
//! By hand on Linux (root, open-iscsi): `iscsiadm -m discovery -t st -p 127.0.0.1:3260`, set the node's CHAP fields
//! (`iscsiadm -m node -T TARGET -o update -n node.session.auth.authmethod -v CHAP`, `...username -v emupod`, `...password -v SECRET`),
//! then `iscsiadm -m node -T TARGET --login`; the iPod's disk is a new /dev/sdX. Before the cable comes out: unmount, then `--logout`.
use crate::nbd::{self, Held, Hub, Op, Via};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const TARGET: &str = "iqn.2026-10.com.emupod:ipod";
pub const CHAP_USER: &str = "emupod";
/// the most data one PDU of ours may carry to us (we say so at login), and the most a burst may be
const MAX_RECV: usize = 256 << 10;
const MAX_BURST: usize = 256 << 10;
const FIRST_BURST: usize = 64 << 10;
/// how many commands the initiator may have sent ahead of the one running
const WINDOW: u32 = 16;
/// the most one command may move (Linux asks at most 512 KB unless told otherwise)
const MAX_XFER: u32 = 8 << 20;

/// SCSI status and sense (fixed format, 18 bytes) the target makes itself.
const GOOD: u8 = 0;
const CHECK: u8 = 2;

fn sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut s = vec![0u8; 18];
    s[0] = 0x70;
    s[2] = key;
    s[7] = 10;
    s[12] = asc;
    s[13] = ascq;
    s
}

/// What the emulator thread answers an `Op::Scsi` with: status, the sense's length, the sense, then the data.
pub fn encode(r: &pp5002::sbp2::Reply) -> Vec<u8> {
    let mut b = vec![r.status, r.sense.len() as u8];
    b.extend_from_slice(&r.sense);
    b.extend_from_slice(&r.data);
    b
}

/// `encode`'s bytes back as (status, sense, data).
pub fn decode(b: &[u8]) -> (u8, Vec<u8>, Vec<u8>) {
    if b.len() < 2 || b.len() < 2 + b[1] as usize {
        return (CHECK, sense(4, 0x44, 0), vec![]);
    }
    let n = 2 + b[1] as usize;
    (b[0], b[2..n].to_vec(), b[n..].to_vec())
}

/// The CHAP secret in `path`, made (24 random hex digits, mode 0600) if there is none yet.
pub fn secret(path: &std::path::Path) -> Result<String, String> {
    if let Ok(s) = std::fs::read_to_string(path) {
        let s = s.trim().to_string();
        if s.len() >= 12 {
            return Ok(s);
        }
    }
    let mut r = [0u8; 12];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut r)).map_err(|e| format!("/dev/urandom: {e}"))?;
    let s: String = r.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(s.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(s)
}

/// The session now running (one at a time): who, and its connection, so a new login from the same initiator (after a dropped
/// connection) can end it.
#[derive(Default)]
struct Current {
    who: Option<(String, TcpStream)>,
}

/// Listen on `addr` (a loopback or private-network address; never an unspecified one, 0.0.0.0 or ::, which would offer the card to every
/// network this machine is on), with the CHAP secret `chap`.
pub fn listen(hub: Arc<Hub>, addr: &str, chap: String) -> Result<(), String> {
    let sa: std::net::SocketAddr = addr.parse().map_err(|e| format!("{addr}: {e}"))?;
    if sa.ip().is_unspecified() {
        return Err(format!("{addr}: an unspecified address would serve every network; give a loopback or private-network address"));
    }
    let l = TcpListener::bind(sa).map_err(|e| format!("{addr}: {e}"))?;
    *hub.iscsi_addr.lock().unwrap() = Some(addr.to_string());
    let cur = Arc::new(Mutex::new(Current::default()));
    let chap = Arc::new(chap);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (hub, cur, chap, addr) = (hub.clone(), cur.clone(), chap.clone(), sa.to_string());
            std::thread::spawn(move || {
                let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                if let Err(e) = Conn::new(hub.clone(), s, &chap, &addr).run(&hub, &cur) {
                    eprintln!("[iscsi] {peer}: {e}");
                }
            });
        }
    });
    Ok(())
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

/// One PDU: its 48-byte header and data segment (AHS skipped; no digests).
struct Pdu {
    bhs: [u8; 48],
    data: Vec<u8>,
}

impl Pdu {
    fn op(&self) -> u8 {
        self.bhs[0] & 0x3f
    }
    fn itt(&self) -> u32 {
        be32(&self.bhs, 16)
    }
}

fn new_bhs(op: u8, flags: u8) -> [u8; 48] {
    let mut b = [0u8; 48];
    b[0] = op;
    b[1] = flags;
    b
}

/// key=value pairs, NUL-separated
fn keys(d: &[u8]) -> Vec<(String, String)> {
    d.split(|&c| c == 0).filter(|k| !k.is_empty()).filter_map(|k| {
        let k = String::from_utf8_lossy(k);
        k.split_once('=').map(|(a, b)| (a.to_string(), b.to_string()))
    }).collect()
}

fn text(kv: &[(String, String)]) -> Vec<u8> {
    kv.iter().flat_map(|(k, v)| format!("{k}={v}\0").into_bytes()).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2).map(|k| u8::from_str_radix(&s[2 * k..2 * k + 2], 16).ok()).collect()
}

struct Conn<'a> {
    hub: Arc<Hub>,
    s: TcpStream,
    chap: &'a str,
    addr: &'a str,
    stat_sn: u32,
    exp_cmd_sn: u32,
    /// what the initiator takes in one PDU
    their_recv: usize,
    first_burst: usize,
    max_burst: usize,
    immediate: bool,
    initial_r2t: bool,
    /// the export generation the session is on, and the sense a REQUEST SENSE gets
    gen: u64,
    last_sense: Vec<u8>,
    /// PDUs read while waiting for a write's data, to be run after it
    later: std::collections::VecDeque<Pdu>,
    writable: bool,
}

impl<'a> Conn<'a> {
    fn new(hub: Arc<Hub>, s: TcpStream, chap: &'a str, addr: &'a str) -> Self {
        s.set_nodelay(true).ok();
        Conn { hub, s, chap, addr, stat_sn: 1, exp_cmd_sn: 0, their_recv: 8192, first_burst: FIRST_BURST, max_burst: MAX_BURST,
                  immediate: true, initial_r2t: true, gen: 0, last_sense: vec![], later: Default::default(), writable: true }
    }

    fn read_pdu(&mut self) -> Result<Pdu, String> {
        let io = |e: std::io::Error| e.to_string();
        let mut bhs = [0u8; 48];
        self.s.read_exact(&mut bhs).map_err(io)?;
        let ahs = bhs[4] as usize * 4;
        let len = (be32(&bhs, 4) & 0xff_ffff) as usize;
        if len > MAX_RECV.max(64 << 10) {
            return Err(format!("a PDU of {len} bytes"));
        }
        let mut skip = vec![0u8; ahs];
        self.s.read_exact(&mut skip).map_err(io)?;
        let mut data = vec![0u8; len.div_ceil(4) * 4];
        self.s.read_exact(&mut data).map_err(io)?;
        data.truncate(len);
        Ok(Pdu { bhs, data })
    }

    fn send(&mut self, mut bhs: [u8; 48], data: &[u8]) -> Result<(), String> {
        let n = data.len() as u32;
        bhs[5] = (n >> 16) as u8;
        bhs[6] = (n >> 8) as u8;
        bhs[7] = n as u8;
        let mut b = Vec::with_capacity(48 + data.len() + 3);
        b.extend_from_slice(&bhs);
        b.extend_from_slice(data);
        b.resize(b.len().div_ceil(4) * 4, 0);
        self.s.write_all(&b).map_err(|e| e.to_string())
    }

    /// StatSN, ExpCmdSN, MaxCmdSN at 24, 28, 32 (a response that takes a StatSN of its own)
    fn sn(&mut self, b: &mut [u8; 48]) {
        put32(b, 24, self.stat_sn);
        self.stat_sn = self.stat_sn.wrapping_add(1);
        self.cmd_sns(b);
    }

    fn cmd_sns(&self, b: &mut [u8; 48]) {
        put32(b, 28, self.exp_cmd_sn);
        put32(b, 32, self.exp_cmd_sn.wrapping_add(WINDOW - 1));
    }

    /// A command PDU's CmdSN taken (not for an immediate one, nor a Data-Out, which has none).
    fn took(&mut self, p: &Pdu) {
        if p.bhs[0] & 0x40 == 0 && matches!(p.op(), 0x00 | 0x01 | 0x02 | 0x04 | 0x06) {
            let sn = be32(&p.bhs, 24);
            if sn == self.exp_cmd_sn {
                self.exp_cmd_sn = sn.wrapping_add(1);
            }
        }
    }

    /// The login phase, then the session's PDUs until it logs out or the connection ends.
    fn run(mut self, hub: &Arc<Hub>, cur: &Mutex<Current>) -> Result<(), String> {
        let mut held = None;
        let Some((who, normal)) = self.login(hub, cur, &mut held)? else { return Ok(()) };
        if !normal {
            return self.discovery();
        }
        cur.lock().unwrap().who = Some((who.clone(), self.s.try_clone().map_err(|e| e.to_string())?));
        eprintln!("[iscsi] {who} logged in ({} sectors{})", self.blocks(), if self.writable { "" } else { ", read-only" });
        // a pulled cable (or a new disk mode) ends the connection
        let watch = self.s.try_clone().map_err(|e| e.to_string())?;
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (done, hub, gen) = (done.clone(), hub.clone(), self.gen);
            std::thread::spawn(move || {
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    if hub.current() != gen {
                        eprintln!("[iscsi] the iPod left disk mode: closing the session");
                        let _ = watch.shutdown(Shutdown::Both);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
        }
        let r = self.full_feature();
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let mut c = cur.lock().unwrap();
            if c.who.as_ref().is_some_and(|(_, s)| s.peer_addr().ok() == self.s.peer_addr().ok()) {
                c.who = None;
            }
        }
        drop(held);
        eprintln!("[iscsi] {who}: session ended");
        r
    }

    fn blocks(&self) -> u64 {
        self.hub.export.lock().unwrap().as_ref().map(|e| e.blocks).unwrap_or(0)
    }

    fn login_reply(&mut self, req: &Pdu, transit: bool, csg: u8, nsg: u8, class: u8, detail: u8, kv: &[(String, String)], tsih: u16) -> Result<(), String> {
        let mut b = new_bhs(0x23, if transit { 0x80 | csg << 2 | nsg } else { csg << 2 });
        b[8..14].copy_from_slice(&req.bhs[8..14]);
        b[14..16].copy_from_slice(&tsih.to_be_bytes());
        b[16..20].copy_from_slice(&req.bhs[16..20]);
        // StatSN: the initiator's ExpStatSN on the first reply
        put32(&mut b, 24, self.stat_sn);
        if class == 0 {
            self.stat_sn = self.stat_sn.wrapping_add(1);
        }
        self.cmd_sns(&mut b);
        b[36] = class;
        b[37] = detail;
        self.send(b, &text(kv))
    }

    /// The login phase: Some((initiator name, normal session?)) once in full feature phase, None if it was refused (and answered).
    /// A normal session's hold on the disk goes in `held`.
    fn login<'h>(&mut self, hub: &'h Hub, cur: &Mutex<Current>, held: &mut Option<Held<'h>>) -> Result<Option<(String, bool)>, String> {
        let mut name = String::new();
        let mut normal = true;
        let mut authed = false;
        let mut chap: Option<(u8, [u8; 16])> = None;
        let mut first = true;
        loop {
            let mut p = self.read_pdu()?;
            if p.op() != 0x03 {
                return Err(format!("opcode {:#x} during login", p.op()));
            }
            // a request continued over several PDUs: take them all
            while p.bhs[1] & 0x40 != 0 {
                self.login_reply(&p, false, p.bhs[1] >> 2 & 3, 0, 0, 0, &[], 0)?;
                let q = self.read_pdu()?;
                p.data.extend_from_slice(&q.data);
                p.bhs = q.bhs;
            }
            if first {
                self.stat_sn = be32(&p.bhs, 28);
                self.exp_cmd_sn = be32(&p.bhs, 24);
                first = false;
            }
            let (t, csg, nsg) = (p.bhs[1] & 0x80 != 0, p.bhs[1] >> 2 & 3, p.bhs[1] & 3);
            let kv = keys(&p.data);
            let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
            if let Some(n) = get("InitiatorName") {
                name = n;
            }
            if let Some(s) = get("SessionType") {
                normal = s != "Discovery";
            }
            let refuse = |me: &mut Self, class: u8, detail: u8, why: &str| -> Result<Option<(String, bool)>, String> {
                eprintln!("[iscsi] login refused ({why})");
                me.login_reply(&p, false, csg, 0, class, detail, &[], 0)?;
                Ok(None)
            };
            if normal {
                if let Some(t) = get("TargetName") {
                    if t != TARGET {
                        return refuse(self, 2, 3, &format!("no target {t:?}"));
                    }
                } else if csg == 0 && kv.iter().any(|(k, _)| k == "InitiatorName") {
                    return refuse(self, 2, 7, "no TargetName");
                }
            }
            let mut out: Vec<(String, String)> = vec![];
            match csg {
                0 => {
                    // security: CHAP for a normal session; a discovery session may use none
                    if let Some(m) = get("AuthMethod") {
                        let offered: Vec<&str> = m.split(',').collect();
                        if offered.contains(&"CHAP") {
                            out.push(("AuthMethod".into(), "CHAP".into()));
                        } else if !normal && offered.contains(&"None") {
                            out.push(("AuthMethod".into(), "None".into()));
                            authed = true;
                        } else {
                            return refuse(self, 2, 1, "no CHAP offered");
                        }
                        if normal {
                            out.push(("TargetPortalGroupTag".into(), "1".into()));
                        }
                    }
                    if let Some(a) = get("CHAP_A") {
                        if !a.split(',').any(|x| x == "5") {
                            return refuse(self, 2, 1, "CHAP without MD5");
                        }
                        let mut c = [0u8; 17];
                        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut c)).map_err(|e| e.to_string())?;
                        let ch: [u8; 16] = c[1..].try_into().unwrap();
                        chap = Some((c[0], ch));
                        out.push(("CHAP_A".into(), "5".into()));
                        out.push(("CHAP_I".into(), c[0].to_string()));
                        out.push(("CHAP_C".into(), format!("0x{}", hex(&ch))));
                    }
                    if let Some(r) = get("CHAP_R") {
                        let Some((id, ch)) = chap.take() else { return refuse(self, 2, 1, "CHAP_R before a challenge") };
                        let mut m = vec![id];
                        m.extend_from_slice(self.chap.as_bytes());
                        m.extend_from_slice(&ch);
                        let ok = get("CHAP_N").as_deref() == Some(CHAP_USER) && unhex(&r).is_some_and(|r| r == md5(&m));
                        if !ok {
                            return refuse(self, 2, 1, "CHAP answer wrong");
                        }
                        if get("CHAP_I").is_some() || get("CHAP_C").is_some() {
                            return refuse(self, 2, 1, "mutual CHAP is not offered");
                        }
                        authed = true;
                    }
                }
                1 => {
                    if !authed {
                        return refuse(self, 2, 1, "not authenticated");
                    }
                    out = self.negotiate(&kv, normal);
                }
                _ => return refuse(self, 2, 0xb, "bad stage"),
            }
            // a stage ends only when authenticated: until then the answer says stay (the initiator may ask to move on early)
            let t = t && authed;
            if t && nsg == 3 {
                // to full feature phase: a normal session needs the iPod in disk mode and the disk free
                if normal {
                    let e = self.hub.export.lock().unwrap().clone();
                    let Some(e) = e else { return refuse(self, 3, 1, "the iPod is not in disk mode") };
                    // the same initiator again (its old connection dropped): end the old one first
                    let old = cur.lock().unwrap().who.take();
                    if let Some((w, s)) = old {
                        if w != name {
                            cur.lock().unwrap().who = Some((w, s));
                            return refuse(self, 3, 1, "another initiator has the disk");
                        }
                        let _ = s.shutdown(Shutdown::Both);
                        for _ in 0..40 {
                            if self.hub.holders.lock().unwrap()[Via::Iscsi as usize] == 0 {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                    let Some(h) = hub.hold(Via::Iscsi) else { return refuse(self, 3, 1, "an NBD host has the disk") };
                    if hub.holders.lock().unwrap()[Via::Iscsi as usize] > 1 {
                        drop(h);
                        return refuse(self, 3, 1, "another session has the disk");
                    }
                    *held = Some(h);
                    self.gen = e.gen;
                    self.writable = e.writable;
                }
                self.login_reply(&p, true, csg, 3, 0, 0, &out, 1)?;
                return Ok(Some((name, normal)));
            }
            self.login_reply(&p, t, csg, nsg, 0, 0, &out, 0)?;
        }
    }

    /// The operational keys' answers (RFC 7143 section 13).
    fn negotiate(&mut self, kv: &[(String, String)], normal: bool) -> Vec<(String, String)> {
        let mut out = vec![];
        let mut said_recv = false;
        for (k, v) in kv {
            let num = || v.parse::<usize>().unwrap_or(0);
            let a: Option<String> = match k.as_str() {
                "HeaderDigest" | "DataDigest" => Some("None".into()),
                "MaxConnections" => Some("1".into()),
                "InitialR2T" => Some("Yes".into()),
                "ImmediateData" => {
                    self.immediate = v == "Yes";
                    Some(v.clone())
                }
                "MaxRecvDataSegmentLength" => {
                    self.their_recv = num().clamp(512, 1 << 24);
                    said_recv = true;
                    None
                }
                "MaxBurstLength" => {
                    self.max_burst = num().clamp(512, MAX_BURST);
                    Some(self.max_burst.to_string())
                }
                "FirstBurstLength" => {
                    self.first_burst = num().clamp(512, FIRST_BURST);
                    Some(self.first_burst.to_string())
                }
                "DefaultTime2Wait" => Some(v.clone()),
                "DefaultTime2Retain" => Some("0".into()),
                "MaxOutstandingR2T" => Some("1".into()),
                "DataPDUInOrder" | "DataSequenceInOrder" => Some("Yes".into()),
                "ErrorRecoveryLevel" => Some("0".into()),
                "IFMarker" | "OFMarker" => Some("No".into()),
                "InitiatorName" | "InitiatorAlias" | "SessionType" | "TargetName" | "TargetAlias" => None,
                _ => Some("NotUnderstood".into()),
            };
            if let Some(a) = a {
                out.push((k.clone(), a));
            }
        }
        self.initial_r2t = true;
        if said_recv || normal {
            out.push(("MaxRecvDataSegmentLength".into(), MAX_RECV.to_string()));
        }
        out
    }

    /// A discovery session: SendTargets, then logout.
    fn discovery(mut self) -> Result<(), String> {
        loop {
            let p = self.read_pdu()?;
            self.took(&p);
            match p.op() {
                0x04 => {
                    let kv = keys(&p.data);
                    let mut out = vec![];
                    if kv.iter().any(|(k, v)| k == "SendTargets" && (v == "All" || v == TARGET || v.is_empty())) {
                        out.push(("TargetName".to_string(), TARGET.to_string()));
                        out.push(("TargetAddress".to_string(), format!("{},1", self.addr)));
                    }
                    let mut b = new_bhs(0x24, 0x80);
                    b[16..20].copy_from_slice(&p.bhs[16..20]);
                    put32(&mut b, 20, 0xffff_ffff);
                    self.sn(&mut b);
                    self.send(b, &text(&out))?;
                }
                0x06 => return self.logout(&p),
                0x00 => self.nop(&p)?,
                op => return Err(format!("opcode {op:#x} in a discovery session")),
            }
        }
    }

    fn logout(&mut self, p: &Pdu) -> Result<(), String> {
        let mut b = new_bhs(0x26, 0x80);
        b[16..20].copy_from_slice(&p.bhs[16..20]);
        self.sn(&mut b);
        self.send(b, &[])?;
        let _ = self.s.shutdown(Shutdown::Both);
        Ok(())
    }

    fn nop(&mut self, p: &Pdu) -> Result<(), String> {
        if p.itt() == 0xffff_ffff {
            return Ok(());
        }
        let mut b = new_bhs(0x20, 0x80);
        b[8..16].copy_from_slice(&p.bhs[8..16]);
        b[16..20].copy_from_slice(&p.bhs[16..20]);
        put32(&mut b, 20, 0xffff_ffff);
        self.sn(&mut b);
        let d = p.data.clone();
        self.send(b, &d)
    }

    fn full_feature(&mut self) -> Result<(), String> {
        loop {
            let p = match self.later.pop_front() {
                Some(p) => p,
                None => match self.read_pdu() {
                    Ok(p) => p,
                    Err(_) => return Ok(()),
                },
            };
            self.took(&p);
            match p.op() {
                0x00 => self.nop(&p)?,
                0x01 => self.command(&p)?,
                0x02 => {
                    // task management: commands run one at a time, so there is none running to abort
                    let mut b = new_bhs(0x22, 0x80);
                    b[16..20].copy_from_slice(&p.bhs[16..20]);
                    self.sn(&mut b);
                    self.send(b, &[])?;
                }
                0x04 => {
                    let mut b = new_bhs(0x24, 0x80);
                    b[16..20].copy_from_slice(&p.bhs[16..20]);
                    put32(&mut b, 20, 0xffff_ffff);
                    self.sn(&mut b);
                    self.send(b, &[])?;
                }
                0x05 => {} // data for a command already answered
                0x06 => return self.logout(&p),
                op => {
                    // Reject: the header back as the data
                    let mut b = new_bhs(0x3f, 0x80);
                    b[2] = 0x04; // protocol error
                    put32(&mut b, 16, 0xffff_ffff);
                    self.sn(&mut b);
                    let h = p.bhs;
                    self.send(b, &h)?;
                    eprintln!("[iscsi] opcode {op:#x} rejected");
                }
            }
        }
    }

    /// A write's data: what came with the command, then R2Ts for the rest.
    fn gather(&mut self, p: &Pdu, len: usize) -> Result<Vec<u8>, String> {
        let mut d = p.data.clone();
        d.truncate(len);
        let unsol = if self.immediate { d.len() } else { 0 };
        d.resize(len, 0);
        let mut have = unsol;
        let mut r2t_sn = 0u32;
        let ttt = 0x1000u32 + (p.itt() & 0xfff);
        while have < len {
            let want = (len - have).min(self.max_burst);
            let mut b = new_bhs(0x31, 0x80);
            b[8..16].copy_from_slice(&p.bhs[8..16]);
            b[16..20].copy_from_slice(&p.bhs[16..20]);
            put32(&mut b, 20, ttt);
            put32(&mut b, 24, self.stat_sn);
            self.cmd_sns(&mut b);
            put32(&mut b, 36, r2t_sn);
            put32(&mut b, 40, have as u32);
            put32(&mut b, 44, want as u32);
            self.send(b, &[])?;
            r2t_sn += 1;
            let end = have + want;
            while have < end {
                let q = self.read_pdu()?;
                if q.op() != 0x05 || q.itt() != p.itt() {
                    self.later.push_back(q);
                    continue;
                }
                let off = be32(&q.bhs, 40) as usize;
                if off + q.data.len() > len {
                    return Err(format!("Data-Out past the end: {off} + {}", q.data.len()));
                }
                d[off..off + q.data.len()].copy_from_slice(&q.data);
                have = have.max(off + q.data.len());
                if q.bhs[1] & 0x80 != 0 && have < end {
                    return Err("a burst ended short".into());
                }
            }
        }
        Ok(d)
    }

    fn command(&mut self, p: &Pdu) -> Result<(), String> {
        let flags = p.bhs[1];
        let edtl = be32(&p.bhs, 20);
        let lun = u64::from_be_bytes(p.bhs[8..16].try_into().unwrap());
        let cdb = p.bhs[32..48].to_vec();
        let out = if flags & 0x20 != 0 && edtl > 0 {
            if edtl > MAX_XFER {
                return Err(format!("a write of {edtl} bytes"));
            }
            Some(self.gather(p, edtl as usize)?)
        } else {
            None
        };
        let (status, sense, data) = if lun >> 48 != 0 {
            (CHECK, sense(5, 0x25, 0), vec![])
        } else if edtl > MAX_XFER {
            (CHECK, sense(5, 0x24, 0), vec![])
        } else {
            self.scsi(&cdb, edtl, out)
        };
        if status == CHECK {
            self.last_sense = sense.clone();
        }
        // the data the iPod sent, cut to what the initiator asked for, in Data-In PDUs it can take
        let n = if flags & 0x40 != 0 { data.len().min(edtl as usize) } else { 0 };
        let mut sn = 0u32;
        let mut off = 0usize;
        while off < n {
            let c = (n - off).min(self.their_recv);
            let mut b = new_bhs(0x25, if off + c == n { 0x80 } else { 0 });
            b[8..16].copy_from_slice(&p.bhs[8..16]);
            b[16..20].copy_from_slice(&p.bhs[16..20]);
            put32(&mut b, 20, 0xffff_ffff);
            self.cmd_sns(&mut b);
            put32(&mut b, 36, sn);
            put32(&mut b, 40, off as u32);
            let chunk = data[off..off + c].to_vec();
            self.send(b, &chunk)?;
            sn += 1;
            off += c;
        }
        let mut b = new_bhs(0x21, 0x80);
        b[3] = status;
        b[16..20].copy_from_slice(&p.bhs[16..20]);
        self.sn(&mut b);
        put32(&mut b, 36, sn);
        let moved = if flags & 0x40 != 0 { n as u32 } else if flags & 0x20 != 0 { edtl } else { 0 };
        if moved < edtl {
            b[1] |= 0x02; // underflow
            put32(&mut b, 44, edtl - moved);
        }
        let mut d = vec![];
        if status == CHECK && !sense.is_empty() {
            d.extend_from_slice(&(sense.len() as u16).to_be_bytes());
            d.extend_from_slice(&sense);
        }
        self.send(b, &d)
    }

    /// One command to the iPod, or answered here: (status, sense, data).
    fn scsi(&mut self, cdb: &[u8], len: u32, out: Option<Vec<u8>>) -> (u8, Vec<u8>, Vec<u8>) {
        // out of disk mode, or a new one (the watcher is closing the connection): no medium
        if self.hub.current() != self.gen {
            return (CHECK, sense(2, 0x3a, 0), vec![]);
        }
        let lba16 = |c: &[u8]| u64::from_be_bytes(c[2..10].try_into().unwrap());
        let rw10 = |op: u8, flags: u8, lba: u64, n: u32| -> Option<Vec<u8>> {
            (lba + n as u64 <= 1 << 32 && n <= 0xffff).then(|| {
                let l = lba as u32;
                vec![op, flags, (l >> 24) as u8, (l >> 16) as u8, (l >> 8) as u8, l as u8, 0, (n >> 8) as u8, n as u8, 0]
            })
        };
        let is_write = matches!(cdb[0], 0x0a | 0x2a | 0x2e | 0x8a | 0xaa | 0x3b | 0x55 | 0x15 | 0x41 | 0x93);
        if is_write && !self.writable {
            return (CHECK, sense(7, 0x27, 0), vec![]);
        }
        let mine: Vec<u8> = match cdb[0] {
            // TEST UNIT READY .. the ones flshdisk takes as they are (scsiprobe), and anything else it may answer for itself
            0x03 => {
                // REQUEST SENSE: the last sense, once
                let s = std::mem::take(&mut self.last_sense);
                return (GOOD, vec![], if s.is_empty() { sense(0, 0, 0) } else { s });
            }
            0x12 if cdb[1] & 1 != 0 => return self.vpd(cdb[2]),
            0xa0 => {
                // REPORT LUNS: LUN 0 alone
                let mut d = vec![0, 0, 0, 8];
                d.extend_from_slice(&[0; 12]);
                return (GOOD, vec![], d);
            }
            0x88 | 0x8a => match rw10(if cdb[0] == 0x88 { 0x28 } else { 0x2a }, cdb[1], lba16(cdb), be32(cdb, 10)) {
                Some(c) => c,
                None => return (CHECK, sense(5, 0x21, 0), vec![]),
            },
            0xa8 | 0xaa => match rw10(if cdb[0] == 0xa8 { 0x28 } else { 0x2a }, cdb[1], be32(cdb, 2) as u64, be32(cdb, 6)) {
                Some(c) => c,
                None => return (CHECK, sense(5, 0x21, 0), vec![]),
            },
            0x91 => vec![0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            0x9e if cdb[1] & 0x1f == 0x10 => {
                // READ CAPACITY(16), from (10)
                let (st, se, d) = self.ask(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8, None);
                if st != GOOD || d.len() < 8 {
                    return (st, se, vec![]);
                }
                let mut r = vec![0u8; 32];
                r[4..8].copy_from_slice(&d[0..4]);
                r[8..12].copy_from_slice(&d[4..8]);
                return (GOOD, vec![], r);
            }
            _ if cdb_len(cdb[0]) > 12 => return (CHECK, sense(5, 0x20, 0), vec![]),
            _ => cdb[..cdb_len(cdb[0])].to_vec(),
        };
        let (st, se, mut d) = self.ask(&mine, len, out);
        match mine[0] {
            // the program says it is an RBC device (type 0x0e); initiators open type 0 disks (qemu's will not read an RBC LUN), and
            // the two read and write alike: say type 0
            0x12 if st == GOOD && !d.is_empty() => d[0] &= 0xe0,
            0x1a if st == GOOD && !self.writable && d.len() > 2 => d[2] |= 0x80, // write protect
            // MODE SENSE of a page the program has not got (it has RBC's page 06 alone): the header, and the caching page a type 0
            // host asks for, with no write cache (the program has none: WCD in its page 06)
            0x1a | 0x5a if st == CHECK => return self.mode_sense(&mine),
            _ => {}
        }
        (st, se, d)
    }

    fn mode_sense(&self, cdb: &[u8]) -> (u8, Vec<u8>, Vec<u8>) {
        let page = cdb[2] & 0x3f;
        let caching: Vec<u8> = if page == 0x08 || page == 0x3f { vec![0x08, 0x12, 0x01].into_iter().chain([0; 17]).collect() } else { vec![] };
        if !caching.is_empty() || page == 0x3f {
            let wp = if self.writable { 0 } else { 0x80 };
            if cdb[0] == 0x1a {
                let mut d = vec![(3 + caching.len()) as u8, 0, wp, 0];
                d.extend_from_slice(&caching);
                return (GOOD, vec![], d);
            }
            let n = 6 + caching.len();
            let mut d = vec![(n >> 8) as u8, n as u8, 0, wp, 0, 0, 0, 0];
            d.extend_from_slice(&caching);
            return (GOOD, vec![], d);
        }
        (CHECK, sense(5, 0x24, 0), vec![])
    }

    /// INQUIRY's vital product data pages, which the program has none of (it is SCSI-2) but initiators ask for (qemu's will not open
    /// a LUN without page 00; Windows identifies a disk by page 83): the supported pages, a serial number and a T10 vendor
    /// identification, both from the iPod's own INQUIRY and its size, so the same card is the same disk.
    fn vpd(&mut self, page: u8) -> (u8, Vec<u8>, Vec<u8>) {
        let (st, se, inq) = self.ask(&[0x12, 0, 0, 0, 36, 0], 36, None);
        if st != GOOD || inq.len() < 36 {
            return (st, se, vec![]);
        }
        let ty = inq[0] & 0xe0;
        let serial = format!("{:012X}", self.blocks());
        let body: Vec<u8> = match page {
            0x00 => vec![0x00, 0x80, 0x83],
            0x80 => serial.into_bytes(),
            0x83 => {
                // one designator: code set ASCII, association LUN, type 1 (T10 vendor ID): vendor (8) + product (16) + serial
                let mut id = inq[8..32].to_vec();
                id.extend_from_slice(serial.as_bytes());
                let mut d = vec![0x02, 0x01, 0x00, id.len() as u8];
                d.extend_from_slice(&id);
                d
            }
            _ => return (CHECK, sense(5, 0x24, 0), vec![]),
        };
        let mut d = vec![ty, page, (body.len() >> 8) as u8, body.len() as u8];
        d.extend_from_slice(&body);
        (GOOD, vec![], d)
    }

    fn ask(&mut self, cdb: &[u8], len: u32, out: Option<Vec<u8>>) -> (u8, Vec<u8>, Vec<u8>) {
        match self.hub.ask(self.gen, Op::Scsi(cdb.to_vec(), len, out), Duration::from_secs(60)) {
            Ok(b) => decode(&b),
            Err(nbd::ESHUTDOWN) => (CHECK, sense(2, 0x3a, 0), vec![]),
            Err(nbd::ENOSPC) => (CHECK, sense(7, 0x27, 7), vec![]),
            // no status from the iPod, or none in time
            Err(_) => (CHECK, sense(2, 0x04, 0), vec![]),
        }
    }
}

/// A CDB's length from its group code (SPC-4 4.2.5.1).
fn cdb_len(op: u8) -> usize {
    match op >> 5 {
        0 => 6,
        1 | 2 => 10,
        4 => 16,
        5 => 12,
        _ => 16,
    }
}

/// MD5 (RFC 1321), for CHAP alone.
pub fn md5(msg: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
                          4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut m = msg.to_vec();
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&((msg.len() as u64) * 8).to_le_bytes());
    for c in m.chunks(64) {
        let w: Vec<u32> = c.chunks(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();
        let [mut a, mut b, mut cc, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & cc) | (!b & d), i),
                1 => ((d & b) | (!d & cc), (5 * i + 1) % 16),
                2 => (b ^ cc ^ d, (3 * i + 5) % 16),
                _ => (cc ^ (b | !d), (7 * i) % 16),
            };
            let t = d;
            d = cc;
            cc = b;
            b = b.wrapping_add(a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(w[g]).rotate_left(S[i]));
            a = t;
        }
        h = [h[0].wrapping_add(a), h[1].wrapping_add(b), h[2].wrapping_add(cc), h[3].wrapping_add(d)];
    }
    let mut out = [0u8; 16];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_vectors() {
        // RFC 1321's test suite
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex(&md5(b"message digest")), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(hex(&md5(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")), "57edf4a22be3c955ac49da2e2107b67a");
    }

    #[test]
    fn keys_round_trip() {
        let kv = keys(b"InitiatorName=iqn.x\0AuthMethod=CHAP,None\0\0");
        assert_eq!(kv, vec![("InitiatorName".into(), "iqn.x".into()), ("AuthMethod".into(), "CHAP,None".into())]);
        assert_eq!(keys(&text(&kv)), kv);
        assert_eq!(unhex("0x0aff"), Some(vec![0x0a, 0xff]));
        assert_eq!(unhex("0aff"), None);
    }

    #[test]
    fn reply_codec() {
        let r = pp5002::sbp2::Reply { data: vec![1, 2, 3], status: 2, sense: sense(5, 0x24, 0) };
        let (st, se, d) = decode(&encode(&r));
        assert_eq!((st, se, d), (2, r.sense.clone(), vec![1, 2, 3]));
        assert_eq!(decode(&[0, 0, 9]), (0, vec![], vec![9]));
    }

    #[test]
    fn lengths() {
        assert_eq!(cdb_len(0x00), 6);
        assert_eq!(cdb_len(0x28), 10);
        assert_eq!(cdb_len(0x5a), 10);
        assert_eq!(cdb_len(0x88), 16);
        assert_eq!(cdb_len(0xa0), 12);
    }
}
