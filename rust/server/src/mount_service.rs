//! The mount service: the one part of emupod that runs as root. emupod itself runs as you; attaching a block device and mounting it
//! need root, so this small service does exactly that and nothing else, for one user:
//!
//!   attach  connect a free /dev/nbdN to emupod's disk-mode export (its NBD address, fixed when the service was set up), then mount the
//!           iPod's FAT partition at the mount point (fixed too), owned by that user, nosuid, nodev, noexec
//!   detach  unmount, then disconnect the device; a mount someone is using is not torn away: the answer names who holds it
//!
//! It listens on a Unix socket only that user (and root) may use, and takes no paths, addresses or devices from the request.
//!
//!     sudo emupod setup-automount [--user NAME] [--nbd 127.0.0.1:10809] [--at /media/emupod]   (once; again after an upgrade)
//!     sudo emupod remove-automount
//!
//! The mount point is under /media: desktop programs (anything built on GIO: file managers, Rhythmbox) list only mounts under /media,
//! /run/media/USER and home folders, never /mnt, and Rhythmbox finds an iPod among the mounts it is shown.
//!
//! Setup copies this program to a root-owned place (a service run as root must not run a file the user can change), and installs the
//! systemd unit emupod-mount.service, which runs `emupod mount-service ...`.
use serde_json::{json, Value};
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SOCKET: &str = "/run/emupod-mount/mount.sock";
/// the server and the service agree on this; a new meaning gets a new number
pub const PROTO: u32 = 1;
const UNIT: &str = "/etc/systemd/system/emupod-mount.service";
const EXE: &str = "/usr/local/libexec/emupod/emupod";
const MODPROBE_CONF: &str = "/etc/modprobe.d/emupod-nbd.conf";
const POLKIT_RULE: &str = "/etc/polkit-1/rules.d/50-emupod-mount.rules";

/// The polkit rule that lets `user` unmount, without a password, a partition of an nbd device that someone else (this service) mounted:
/// what the desktop's Eject asks udisks for. Nothing else: other disks, and other users, still need what they needed.
pub fn polkit_rule(user: &str) -> String {
    format!("// emupod (emupod setup-automount): {user} may unmount the emulated iPod's disk, which emupod's mount service mounted, without\n\
             // a password: the desktop's Eject. Only partitions of nbd devices; nothing else changes.\n\
             polkit.addRule(function (action, subject) {{\n\
             \x20   if (action.id == \"org.freedesktop.udisks2.filesystem-unmount-others\" && subject.user == {} &&\n\
             \x20       /^\\/dev\\/nbd[0-9]+p[0-9]+$/.test(action.lookup(\"device\"))) {{\n\
             \x20       return polkit.Result.YES;\n\
             \x20   }}\n\
             }});\n", serde_json::to_string(user).unwrap())
}

// linux/nbd.h: _IO(0xab, n)
const NBD_SET_SOCK: u64 = 0xab00;
const NBD_SET_BLKSIZE: u64 = 0xab01;
const NBD_DO_IT: u64 = 0xab03;
const NBD_CLEAR_SOCK: u64 = 0xab04;
const NBD_CLEAR_QUE: u64 = 0xab05;
const NBD_SET_SIZE_BLOCKS: u64 = 0xab07;
const NBD_DISCONNECT: u64 = 0xab08;
const NBD_SET_TIMEOUT: u64 = 0xab09;
const NBD_SET_FLAGS: u64 = 0xab0a;
const BLKRRPART: u64 = 0x125f;

struct Cfg {
    uid: u32,
    gid: u32,
    nbd: String,
    at: PathBuf,
}

/// What is attached now.
struct Attached {
    dev: String,
    part: String,
    /// the device, held open: its NBD_DO_IT runs on a thread until the device is disconnected
    file: std::fs::File,
    done: std::thread::JoinHandle<()>,
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn user(name: &str) -> Result<(u32, u32), String> {
    let c = CString::new(name).map_err(|_| "bad user name".to_string())?;
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        return Err(format!("no user {name}"));
    }
    Ok(unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn ioctl(fd: i32, req: u64, arg: u64) -> Result<(), std::io::Error> {
    if unsafe { libc::ioctl(fd, req as _, arg) } < 0 {
        return Err(errno());
    }
    Ok(())
}

/// The mount point: an absolute path to a directory (made if missing), not a symbolic link.
fn check_at(at: &Path) -> Result<(), String> {
    if !at.is_absolute() {
        return Err(format!("{}: the mount point must be an absolute path", at.display()));
    }
    match std::fs::symlink_metadata(at) {
        Ok(m) if m.file_type().is_symlink() => Err(format!("{} is a symbolic link", at.display())),
        Ok(m) if !m.is_dir() => Err(format!("{} is not a folder", at.display())),
        Ok(_) => Ok(()),
        Err(_) => std::fs::create_dir_all(at).map_err(|e| format!("{}: {e}", at.display())),
    }
}

/// The source of the mount at `at`, if something is mounted there (from /proc/self/mountinfo; octal escapes as the kernel writes them).
pub fn mounted_at(mountinfo: &str, at: &Path) -> Option<String> {
    let unescape = |s: &str| {
        let b = s.as_bytes();
        let mut o = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
                o.push(u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?'));
                i += 4;
            } else {
                o.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&o).into_owned()
    };
    let want = at.to_string_lossy();
    let mut found = None;
    for l in mountinfo.lines() {
        let f: Vec<&str> = l.split(' ').collect();
        let Some(dash) = f.iter().position(|x| *x == "-") else { continue };
        if f.len() > dash + 2 && f.len() > 4 && unescape(f[4]) == want {
            found = Some(unescape(f[dash + 2])); // the last mount there is the one seen
        }
    }
    found
}

/// The 1-based number of the iPod's FAT partition in an MBR (the first partition of a FAT type), if any.
pub fn fat_partition(mbr: &[u8]) -> Option<usize> {
    if mbr.len() < 512 || mbr[510] != 0x55 || mbr[511] != 0xaa {
        return None;
    }
    (0..4).find(|i| matches!(mbr[446 + 16 * i + 4], 0x01 | 0x04 | 0x06 | 0x0b | 0x0c | 0x0e)).map(|i| i + 1)
}

/// Who keeps a mount busy: "name (pid)" for each process whose working folder, root or an open file is under `at`.
pub fn holders(at: &Path) -> Vec<String> {
    let under = |p: PathBuf| p == at || p.starts_with(at);
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let d = e.path();
        let mut hit = ["cwd", "root"].iter().any(|l| std::fs::read_link(d.join(l)).map(under).unwrap_or(false));
        if !hit {
            if let Ok(fds) = std::fs::read_dir(d.join("fd")) {
                hit = fds.flatten().any(|f| std::fs::read_link(f.path()).map(under).unwrap_or(false));
            }
        }
        if hit {
            let name = std::fs::read_to_string(d.join("comm")).unwrap_or_default();
            out.push(format!("{} ({pid})", name.trim()));
        }
    }
    out.sort();
    out
}

fn nbd_loaded() -> Result<(), String> {
    if !Path::new("/sys/module/nbd").exists() {
        let m = ["/usr/sbin/modprobe", "/sbin/modprobe"].into_iter().find(|p| Path::new(p).exists()).ok_or("modprobe not found")?;
        let ok = std::process::Command::new(m).args(["nbd", "max_part=8"]).status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            return Err("could not load the nbd module (modprobe nbd)".into());
        }
    }
    let parts = std::fs::read_to_string("/sys/module/nbd/parameters/max_part").unwrap_or_default();
    if parts.trim() == "0" {
        return Err("the nbd module is loaded without partitions (max_part=0): disconnect any nbd devices and run `sudo rmmod nbd`, or restart".into());
    }
    Ok(())
}

/// A /dev/nbdN no one uses (no size, no client).
fn free_nbd() -> Result<String, String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/block").map_err(|e| e.to_string())?.flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from)).filter(|n| n.starts_with("nbd")).collect();
    names.sort_by_key(|n| n[3..].parse::<u32>().unwrap_or(u32::MAX));
    for n in names {
        let size = std::fs::read_to_string(format!("/sys/block/{n}/size")).unwrap_or_default();
        if size.trim() == "0" && !Path::new(&format!("/sys/block/{n}/pid")).exists() {
            return Ok(format!("/dev/{n}"));
        }
    }
    Err("every /dev/nbd device is in use".into())
}

fn wait_for(p: &str, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if Path::new(p).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn attach(cfg: &Cfg) -> Result<Attached, String> {
    if let Some(src) = mounted_at(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(), &cfg.at) {
        return Err(format!("{} is in use already ({src} is mounted there)", cfg.at.display()));
    }
    check_at(&cfg.at)?;
    nbd_loaded()?;
    let dev = free_nbd()?;
    let mut sock = TcpStream::connect_timeout(&cfg.nbd.parse().map_err(|e| format!("{}: {e}", cfg.nbd))?, Duration::from_secs(5))
        .map_err(|e| format!("emupod's disk at {}: {e}", cfg.nbd))?;
    sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let (size, flags) = crate::nbd::client_go(&mut sock, crate::nbd::EXPORT).map_err(|e| format!("emupod's disk at {}: {e}", cfg.nbd))?;
    sock.set_read_timeout(None).ok();
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&dev).map_err(|e| format!("{dev}: {e}"))?;
    let fd = file.as_raw_fd();
    let setup = || -> Result<(), std::io::Error> {
        ioctl(fd, NBD_SET_BLKSIZE, 512)?;
        ioctl(fd, NBD_SET_SIZE_BLOCKS, size / 512)?;
        ioctl(fd, NBD_CLEAR_SOCK, 0)?;
        // the iPod's disk-mode program is slow next to a disk (every sector runs through the emulator): give a request 2 minutes, as
        // emupod's own NBD side does
        ioctl(fd, NBD_SET_TIMEOUT, 120)?;
        ioctl(fd, NBD_SET_FLAGS, flags as u64)?;
        ioctl(fd, NBD_SET_SOCK, sock.as_raw_fd() as u64)
    };
    setup().map_err(|e| format!("{dev}: {e}"))?;
    // the kernel holds its own reference to the socket now
    let sfd = sock.into_raw_fd();
    let done = std::thread::spawn(move || {
        let _ = ioctl(fd, NBD_DO_IT, 0); // until the device is disconnected
        let _ = ioctl(fd, NBD_CLEAR_QUE, 0);
        let _ = ioctl(fd, NBD_CLEAR_SOCK, 0);
        unsafe { libc::close(sfd) };
    });
    let a = Attached { dev: dev.clone(), part: String::new(), file, done };
    match mount(cfg, &a) {
        Ok(part) => Ok(Attached { part, ..a }),
        Err(e) => {
            disconnect(a);
            Err(e)
        }
    }
}

fn mount(cfg: &Cfg, a: &Attached) -> Result<String, String> {
    let name = a.dev.trim_start_matches("/dev/");
    let end = Instant::now() + Duration::from_secs(15);
    while std::fs::read_to_string(format!("/sys/block/{name}/size")).unwrap_or_default().trim() == "0" {
        if Instant::now() > end {
            return Err(format!("{}: the device never came up", a.dev));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut mbr = vec![0u8; 512];
    std::fs::File::open(&a.dev).and_then(|mut f| f.read_exact(&mut mbr)).map_err(|e| format!("{}: {e}", a.dev))?;
    let n = fat_partition(&mbr).ok_or("the iPod's disk has no FAT partition")?;
    let part = format!("{}p{n}", a.dev);
    if !wait_for(&part, 3) {
        if let Ok(f) = std::fs::File::open(&a.dev) {
            let _ = ioctl(f.as_raw_fd(), BLKRRPART, 0);
        }
        if !wait_for(&part, 10) {
            return Err(format!("{part} did not appear"));
        }
    }
    let src = CString::new(part.clone()).unwrap();
    let at = CString::new(cfg.at.to_string_lossy().as_bytes()).unwrap();
    let data = CString::new(format!("uid={},gid={},umask=022,flush,utf8,shortname=mixed", cfg.uid, cfg.gid)).unwrap();
    let r = unsafe {
        libc::mount(src.as_ptr(), at.as_ptr(), c"vfat".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, data.as_ptr() as *const _)
    };
    if r < 0 {
        return Err(format!("mount {part} at {}: {}", cfg.at.display(), errno()));
    }
    Ok(part)
}

fn disconnect(a: Attached) {
    let _ = ioctl(a.file.as_raw_fd(), NBD_DISCONNECT, 0);
    let _ = a.done.join();
    drop(a.file);
}

/// Unmount and disconnect. Not `force`: a busy mount stays, and the answer names who holds it. `force` (the iPod has left disk mode
/// already, its disk is gone): detach the mount lazily, then disconnect.
fn detach(cfg: &Cfg, a: &mut Option<Attached>, force: bool) -> Value {
    let at = CString::new(cfg.at.to_string_lossy().as_bytes()).unwrap();
    let ours = mounted_at(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(), &cfg.at)
        .map(|src| src.starts_with("/dev/nbd"));
    if ours == Some(true) {
        unsafe { libc::sync() };
        if unsafe { libc::umount2(at.as_ptr(), if force { libc::MNT_DETACH } else { 0 }) } < 0 {
            let e = errno();
            if e.raw_os_error() == Some(libc::EBUSY) {
                return json!({"ok": false, "error": "busy", "holders": holders(&cfg.at)});
            }
            return json!({"ok": false, "error": format!("unmount {}: {e}", cfg.at.display())});
        }
    }
    if let Some(x) = a.take() {
        disconnect(x);
    }
    json!({"ok": true})
}

/// SIGTERM seen (systemctl stop or restart, shutdown): the service unmounts before it goes (its device dies with it).
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn on_stop(_: i32) {
    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut c = libc::ucred { pid: 0, uid: u32::MAX, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut c as *mut _ as *mut _, &mut len) };
    (r == 0).then_some(c.uid)
}

/// `emupod mount-service --user NAME [--nbd ADDR] [--at DIR] [--socket PATH]` (root; systemd starts it).
pub fn run(args: &[String]) -> i32 {
    if !is_root() {
        eprintln!("emupod mount-service: runs as root (it is started by emupod-mount.service; see `sudo emupod setup-automount`)");
        return 1;
    }
    let Some(name) = opt(args, "--user") else {
        eprintln!("emupod mount-service: --user NAME is needed");
        return 2;
    };
    let (uid, gid) = match user(&name) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("emupod mount-service: {e}");
            return 1;
        }
    };
    let cfg = Cfg { uid, gid, nbd: opt(args, "--nbd").unwrap_or_else(|| "127.0.0.1:10809".into()), at: opt(args, "--at").unwrap_or_else(|| "/media/emupod".into()).into() };
    let sock = PathBuf::from(opt(args, "--socket").unwrap_or_else(|| SOCKET.into()));
    // a mount left from before (the service was restarted while attached: its device died with it)
    let mut a: Option<Attached> = None;
    detach(&cfg, &mut a, true);
    let _ = std::fs::remove_file(&sock);
    if let Some(d) = sock.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let l = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("emupod mount-service: {}: {e}", sock.display());
            return 1;
        }
    };
    let p = CString::new(sock.to_string_lossy().as_bytes()).unwrap();
    unsafe { libc::chown(p.as_ptr(), 0, gid) };
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660));
    eprintln!("[mount] for {name}: emupod's disk at {} is mounted at {} (requests on {})", cfg.nbd, cfg.at.display(), sock.display());
    unsafe {
        libc::signal(libc::SIGTERM, on_stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_stop as *const () as libc::sighandler_t);
    }
    {
        // the stop is noticed here and handed to the request loop (which waits in accept) as a request from root
        let sock = sock.clone();
        std::thread::spawn(move || {
            while !STOP.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
            }
            if let Ok(mut s) = UnixStream::connect(&sock) {
                let _ = s.write_all(b"{\"do\": \"stop\"}\n");
            }
        });
    }
    // one request at a time: an attach and a detach never overlap
    for s in l.incoming().flatten() {
        let who = peer_uid(&s);
        if who != Some(uid) && who != Some(0) {
            continue;
        }
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut line = String::new();
        if BufReader::new(&s).take(4096).read_line(&mut line).is_err() {
            continue;
        }
        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let answer = match req["do"].as_str() {
            Some("hello") => {
                if a.is_some() && mounted_at(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(), &cfg.at).is_none() {
                    // unmounted by the user (the desktop's Eject, umount): the device goes too, and emupod tells the iPod
                    eprintln!("[mount] {} was unmounted: disconnected {}", cfg.at.display(), detach(&cfg, &mut a, false));
                }
                json!({"ok": true, "proto": PROTO, "version": crate::VERSION, "nbd": cfg.nbd, "at": cfg.at, "attached": a.as_ref().map(|x| x.part.clone())})
            }
            Some("attach") if a.as_ref().is_some_and(|x| !x.done.is_finished()) => json!({"ok": true, "dev": a.as_ref().unwrap().part, "at": cfg.at}),
            Some("attach") => match {
                // the last disk is dead (emupod stopped or crashed while it was mounted: its connection, and so the device, ended):
                // let its mount go first
                if a.is_some() {
                    eprintln!("[mount] the last disk's connection ended: {}", detach(&cfg, &mut a, true));
                }
                attach(&cfg)
            } {
                Ok(x) => {
                    eprintln!("[mount] {} mounted at {}", x.part, cfg.at.display());
                    let v = json!({"ok": true, "dev": x.part, "at": cfg.at});
                    a = Some(x);
                    v
                }
                Err(e) => {
                    eprintln!("[mount] attach: {e}");
                    json!({"ok": false, "error": e})
                }
            },
            Some("stop") if who == Some(0) => {
                // unmount (lazily if someone holds it: the device goes away with this process either way), then go
                let mut v = detach(&cfg, &mut a, false);
                if v["ok"] != true {
                    v = detach(&cfg, &mut a, true);
                }
                eprintln!("[mount] stopping: {v}");
                let _ = std::fs::remove_file(&sock);
                return 0;
            }
            Some("detach") => {
                let v = detach(&cfg, &mut a, req["force"].as_bool().unwrap_or(false));
                eprintln!("[mount] detach: {v}");
                v
            }
            _ => json!({"ok": false, "error": "unknown request"}),
        };
        let mut s = s;
        let _ = s.write_all(format!("{answer}\n").as_bytes());
    }
    0
}

fn sh(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd).args(args).status().map(|s| s.success()).unwrap_or(false)
}

/// `sudo emupod setup-automount [--user NAME] [--nbd ADDR] [--at DIR]`: install (or refresh) the mount service.
pub fn setup(args: &[String]) -> i32 {
    if !is_root() {
        eprintln!("emupod setup-automount: run it with sudo (it installs a service that runs as root)");
        return 1;
    }
    let Some(name) = opt(args, "--user").or_else(|| std::env::var("SUDO_USER").ok()).filter(|n| n != "root") else {
        eprintln!("emupod setup-automount: say whose emupod it serves: --user NAME (or run it with sudo from that user)");
        return 2;
    };
    if let Err(e) = user(&name) {
        eprintln!("emupod setup-automount: {e}");
        return 1;
    }
    let nbd = opt(args, "--nbd").unwrap_or_else(|| "127.0.0.1:10809".into());
    if let Err(e) = nbd.parse::<std::net::SocketAddr>() {
        eprintln!("emupod setup-automount: --nbd {nbd}: {e}");
        return 2;
    }
    let at = opt(args, "--at").unwrap_or_else(|| "/media/emupod".into());
    let at_ok = check_at(Path::new(&at));
    if at.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\') || at_ok.is_err() {
        eprintln!("emupod setup-automount: --at {at}: {}", at_ok.err().unwrap_or_else(|| "use a path without spaces or quotes".into()));
        return 2;
    }
    // a running service with a disk mounted: restarting it now would kill that disk under its mount (the device dies with the process)
    if let Ok(s) = UnixStream::connect(SOCKET) {
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let _ = (&s).write_all(b"{\"do\": \"hello\"}\n");
        let mut line = String::new();
        let _ = BufReader::new(&s).read_line(&mut line);
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if let Some(dev) = v["attached"].as_str() {
            eprintln!("emupod setup-automount: the iPod's disk ({dev}) is mounted at {}: click the FireWire cable out on the iPod page first, then run this again",
                      v["at"].as_str().unwrap_or("?"));
            return 1;
        }
    }
    let step = |what: &str, r: Result<(), String>| match r {
        Ok(()) => {
            println!("  {what}");
            true
        }
        Err(e) => {
            eprintln!("emupod setup-automount: {what}: {e}");
            false
        }
    };
    // this program, copied where only root can change it
    let copy = || -> Result<(), String> {
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        std::fs::create_dir_all(Path::new(EXE).parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = format!("{EXE}.new");
        std::fs::copy(&me, &tmp).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, EXE).map_err(|e| e.to_string())
    };
    if !step(&format!("copied this program to {EXE}"), copy()) {
        return 1;
    }
    // partitions on nbd devices (the iPod's data is its second partition); not when another file sets the module's options already
    let others = std::fs::read_dir("/etc/modprobe.d").map(|d| d.flatten().filter(|e| e.path() != Path::new(MODPROBE_CONF))
        .any(|e| std::fs::read_to_string(e.path()).unwrap_or_default().lines().any(|l| l.trim_start().starts_with("options nbd")))).unwrap_or(false);
    if others {
        println!("  left the nbd module's options alone (set in /etc/modprobe.d already); it needs max_part above 0");
    } else if !step(&format!("wrote {MODPROBE_CONF} (partitions on nbd devices)"),
                    std::fs::write(MODPROBE_CONF, "# emupod: the emulated iPod's disk has partitions (emupod setup-automount)\noptions nbd max_part=8\n").map_err(|e| e.to_string())) {
        return 1;
    }
    if Path::new("/etc/polkit-1/rules.d").is_dir() {
        if !step(&format!("wrote {POLKIT_RULE} (the desktop's Eject unmounts the iPod's disk without a password)"),
                 std::fs::write(POLKIT_RULE, polkit_rule(&name)).map_err(|e| e.to_string())) {
            return 1;
        }
    } else {
        println!("  no /etc/polkit-1/rules.d: the desktop's Eject will ask for a password (the page's FireWire switch needs none)");
    }
    // no sandboxing options that give the service its own mount namespace (ProtectSystem, ProtectHome, PrivateTmp, ...): its mounts must
    // be the ones everyone sees
    let unit = format!(
        "[Unit]\nDescription=emupod: mounts the emulated iPod's disk for {name}\nDocumentation=https://emupod.com/\n\n[Service]\n\
         ExecStart={EXE} mount-service --user {name} --nbd {nbd} --at {at}\nRuntimeDirectory=emupod-mount\nRestart=on-failure\n\n\
         [Install]\nWantedBy=multi-user.target\n");
    if !step(&format!("wrote {UNIT}"), std::fs::write(UNIT, unit).map_err(|e| e.to_string())) {
        return 1;
    }
    let ok = sh("systemctl", &["daemon-reload"]) && sh("systemctl", &["enable", "emupod-mount.service"]) && sh("systemctl", &["restart", "emupod-mount.service"]);
    if !step("started emupod-mount.service", if ok { Ok(()) } else { Err("systemctl failed (see: systemctl status emupod-mount.service)".into()) }) {
        return 1;
    }
    println!("The emulated iPod's disk is now mounted at {at} when you plug in its FireWire cable, and unmounted before the cable comes out.");
    println!("To undo: sudo emupod remove-automount");
    0
}

/// `sudo emupod remove-automount`: stop and remove the mount service.
pub fn remove(_args: &[String]) -> i32 {
    if !is_root() {
        eprintln!("emupod remove-automount: run it with sudo");
        return 1;
    }
    sh("systemctl", &["disable", "--now", "emupod-mount.service"]);
    for f in [UNIT, EXE, MODPROBE_CONF, POLKIT_RULE] {
        if std::fs::remove_file(f).is_ok() {
            println!("  removed {f}");
        }
    }
    let _ = std::fs::remove_dir(Path::new(EXE).parent().unwrap());
    sh("systemctl", &["daemon-reload"]);
    println!("The mount service is removed (the mount point folder, if any, is left as it is).");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_mount_at_a_path() {
        let mi = "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
                  90 22 43:2 / /mnt/emu\\040pod rw,nosuid - vfat /dev/nbd0p2 rw\n\
                  91 22 43:2 / /mnt/emupod rw - vfat /dev/nbd1p2 rw\n";
        assert_eq!(mounted_at(mi, Path::new("/mnt/emupod")).as_deref(), Some("/dev/nbd1p2"));
        assert_eq!(mounted_at(mi, Path::new("/mnt/emu pod")).as_deref(), Some("/dev/nbd0p2"));
        assert_eq!(mounted_at(mi, Path::new("/mnt")), None);
    }

    #[test]
    fn the_polkit_rule_names_the_user_and_nbd_partitions_only() {
        let r = polkit_rule("alice");
        assert!(r.contains(r#"subject.user == "alice""#), "{r}");
        assert!(r.contains(r#"/^\/dev\/nbd[0-9]+p[0-9]+$/.test(action.lookup("device"))"#), "{r}");
        assert!(r.contains(r#"action.id == "org.freedesktop.udisks2.filesystem-unmount-others""#), "{r}");
        assert!(polkit_rule("a\"b").contains(r#""a\"b""#));
    }

    #[test]
    fn picks_the_fat_partition() {
        let mut mbr = vec![0u8; 512];
        mbr[510] = 0x55;
        mbr[511] = 0xaa;
        assert_eq!(fat_partition(&mbr), None);
        mbr[446 + 4] = 0x00; // the iPod's firmware partition
        mbr[446 + 16 + 4] = 0x0b;
        assert_eq!(fat_partition(&mbr), Some(2));
        mbr[511] = 0;
        assert_eq!(fat_partition(&mbr), None);
    }

    #[test]
    fn names_who_holds_a_folder() {
        let d = std::env::temp_dir().join(format!("emupod-holders-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = std::fs::File::create(d.join("open")).unwrap();
        let me = format!("({})", std::process::id());
        assert!(holders(&d).iter().any(|h| h.ends_with(&me)));
        drop(f);
        assert!(!holders(&d).iter().any(|h| h.ends_with(&me)));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
