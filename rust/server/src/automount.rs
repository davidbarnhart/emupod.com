//! The server's side of automatic mounting: when the iPod enters disk mode, ask the mount service (`mount_service`, root, set up once
//! with `sudo emupod setup-automount`) to attach and mount its disk; before the FireWire cable comes out, ask it to unmount, and only
//! then let the cable out (a cable pulled from a mounted disk leaves a half-written sync). Without the service, nothing changes: the
//! page shows the commands to attach it by hand.
//!
//! Its locks: `op` (one request to the service at a time; the export it was for) is held across a request; `state` (what the page
//! shows) is a leaf, taken last and alone, so /state may read it while holding the page state.
use crate::nbd::Hub;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Op {
    /// the export generation last handled (mounted, or deliberately not)
    gen: u64,
    attached: bool,
    /// ticks since the service was last asked whether the disk is still mounted
    since_check: u32,
}

pub struct Mounter {
    /// the service's socket (None: automatic mounting is off)
    sock: Mutex<Option<PathBuf>>,
    op: Mutex<Op>,
    state: Mutex<Value>,
    unplugging: AtomicBool,
    /// what turns it on, for the page
    setup: String,
}

impl Default for Mounter {
    fn default() -> Self {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "emupod".into());
        let setup = format!("sudo {exe} setup-automount");
        Mounter { sock: Mutex::new(None), op: Mutex::new(Op::default()), state: Mutex::new(json!({"state": "off", "setup": setup})),
                  unplugging: AtomicBool::new(false), setup }
    }
}

impl Mounter {
    pub fn state(&self) -> Value {
        self.state.lock().unwrap().clone()
    }

    fn set(&self, v: Value) {
        *self.state.lock().unwrap() = v;
    }

    fn sock(&self) -> Option<PathBuf> {
        self.sock.lock().unwrap().clone()
    }

    /// The service is there to ask (its socket exists).
    fn available(&self) -> bool {
        self.sock().map(|p| p.exists()).unwrap_or(false)
    }

    fn call(&self, req: Value, timeout: u64) -> Result<Value, String> {
        let p = self.sock().ok_or("automatic mounting is off")?;
        let s = UnixStream::connect(&p).map_err(|e| format!("the mount service ({}): {e}", p.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(timeout))).ok();
        (&s).write_all(format!("{req}\n").as_bytes()).map_err(|e| format!("the mount service: {e}"))?;
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).map_err(|e| format!("the mount service: {e}"))?;
        serde_json::from_str(&line).map_err(|_| "the mount service did not answer (is it running? systemctl status emupod-mount)".to_string())
    }

    /// Start watching the iPod's disk mode (`sock`: the service's socket).
    pub fn start(self: &Arc<Self>, sock: PathBuf, hub: Arc<Hub>) {
        *self.sock.lock().unwrap() = Some(sock);
        let me = self.clone();
        std::thread::spawn(move || loop {
            me.tick(&hub);
            std::thread::sleep(Duration::from_millis(250));
        });
    }

    fn tick(&self, hub: &Hub) {
        let gen = hub.current();
        let mut op = self.op.lock().unwrap();
        if !self.available() {
            if !op.attached {
                self.set(json!({"state": "off", "setup": self.setup}));
            }
            return;
        }
        if gen != 0 && op.gen != gen {
            op.gen = gen;
            self.set(json!({"state": "mounting"}));
            let r = self.hello(hub).and_then(|_| self.call(json!({"do": "attach"}), 90));
            match r {
                Ok(v) if v["ok"] == true => {
                    op.attached = true;
                    eprintln!("[mount] the iPod's disk ({}) is mounted at {}", v["dev"].as_str().unwrap_or("?"), v["at"].as_str().unwrap_or("?"));
                    self.set(json!({"state": "mounted", "at": v["at"], "dev": v["dev"]}));
                }
                Ok(v) => self.fail(v["error"].as_str().unwrap_or("the mount service said no").to_string()),
                Err(e) => self.fail(e),
            }
        } else if gen != 0 && op.attached && { op.since_check += 1; op.since_check >= 4 } {
            // once a second: is it still mounted? Unmounted by the computer's user (the desktop's Eject, umount): the service has let the
            // device go; the iPod is told, as a computer ejecting it would
            op.since_check = 0;
            if let Ok(v) = self.call(json!({"do": "hello"}), 10) {
                if v["ok"] == true && v["attached"].is_null() {
                    op.attached = false;
                    match hub.ask(gen, crate::nbd::Op::Eject, Duration::from_secs(30)) {
                        Ok(_) => self.set(json!({"state": "ejected"})),
                        Err(e) => self.fail(format!("the disk was unmounted, but the iPod did not take the eject (errno {e})")),
                    }
                }
            }
        } else if gen == 0 && op.attached {
            // disk mode ended without the cable (a restart, another card): the disk is gone already
            let _ = self.call(json!({"do": "detach", "force": true}), 30);
            op.attached = false;
            self.set(json!({"state": "idle"}));
        } else if gen == 0 && !op.attached && self.state()["state"] != "idle" {
            self.set(json!({"state": "idle"}));
        }
    }

    fn fail(&self, e: String) {
        eprintln!("[mount] {e}");
        self.set(json!({"state": "error", "error": e}));
    }

    /// The service is the one this server can use: the same requests (proto), the same disk (its NBD address is this server's).
    fn hello(&self, hub: &Hub) -> Result<(), String> {
        let v = self.call(json!({"do": "hello"}), 10)?;
        if v["proto"].as_u64() != Some(crate::mount_service::PROTO as u64) {
            return Err(format!("the mount service is from emupod {}: run `{}` again to update it", v["version"].as_str().unwrap_or("?"), self.setup));
        }
        let ours = hub.addr.lock().unwrap().clone().unwrap_or_default();
        if v["nbd"].as_str() != Some(ours.as_str()) {
            return Err(format!("the mount service attaches the disk at {}, but this server serves it at {ours}: run `{} --nbd {ours}`",
                               v["nbd"].as_str().unwrap_or("?"), self.setup));
        }
        Ok(())
    }

    /// The cable is being pulled (`out` runs it). With the disk mounted, it is unmounted first, on a thread of its own (it waits for
    /// the disk's last writes, which go through the iPod); the cable comes out only if that worked. Whether it was taken here.
    pub fn unplug(self: &Arc<Self>, out: Box<dyn FnOnce() + Send>) -> bool {
        if self.sock().is_none() {
            return false;
        }
        if self.unplugging.swap(true, Ordering::SeqCst) {
            return true; // one unmount at a time; the second press waits for the first
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let mut op = me.op.lock().unwrap();
            if op.attached {
                me.set(json!({"state": "unmounting"}));
                match me.call(json!({"do": "detach"}), 300) {
                    Ok(v) if v["ok"] == true => {
                        op.attached = false;
                        me.set(json!({"state": "unmounted"}));
                        out();
                    }
                    Ok(v) if v["error"] == "busy" => {
                        let who: Vec<&str> = v["holders"].as_array().map(|a| a.iter().filter_map(|h| h.as_str()).collect()).unwrap_or_default();
                        let e = format!("still in use{}: the cable stays in until it is closed", if who.is_empty() { String::new() } else { format!(" by {}", who.join(", ")) });
                        eprintln!("[mount] {e}");
                        me.set(json!({"state": "busy", "error": e, "holders": v["holders"]}));
                    }
                    Ok(v) => me.fail(v["error"].as_str().unwrap_or("could not unmount").to_string()),
                    Err(e) => me.fail(e),
                }
            } else {
                out();
            }
            drop(op);
            me.unplugging.store(false, Ordering::SeqCst);
        });
        true
    }

    /// The server is stopping: unmount the disk while the iPod can still take its last writes (and, if someone holds it, let it go
    /// lazily: the device is about to go away).
    pub fn shutdown(&self) {
        let mut op = self.op.lock().unwrap();
        if op.attached {
            let ok = self.call(json!({"do": "detach"}), 300).map(|v| v["ok"] == true).unwrap_or(false);
            if !ok {
                let _ = self.call(json!({"do": "detach", "force": true}), 30);
            }
            op.attached = false;
            eprintln!("[mount] the iPod's disk is unmounted");
        }
    }
}
