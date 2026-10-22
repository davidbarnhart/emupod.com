//! Burning a card image onto a real flash card (SD, CompactFlash) in a reader on this machine, with flashpod's safety rules (flashpod/burn.py and
//! ipod_flash.list_candidates, the same checks):
//!
//! * only whole disks that are removable (USB, an SD slot, hot-plug, per lsblk) are offered, never a disk backing the running system
//!   (/, /boot, /home, swap), never one on FireWire (early iPod FireWire bridges crash on large writes);
//! * the card must report a size (an empty reader slot reports 0) at least the image's;
//! * the image is held under a shared lock for the whole burn (nothing changes it mid-copy; the iPod holds a card it writes to
//!   exclusively, so a card running with its changes kept cannot be burned);
//! * the card's partitions are unmounted; then the card is made the flashpod way (cards::clone_card), not by copying the image's
//!   bytes: partitioned and formatted to the card's own size with the firmware the image carries, every file of the image's FAT
//!   partition copied onto it, the songs checked; the partition table re-read and the card ejected. The whole card is used.
//!
//! Writing a disk needs permission to open /dev/sdX for writing (root, or the disk's group); a disk this server cannot open is listed
//! with the reason. EMUPOD_FAKE_DISKS=DIR makes every file in DIR stand in for a USB disk (tests: no real device is touched).
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};


fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !o.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn fake_dir() -> Option<String> {
    std::env::var("EMUPOD_FAKE_DISKS").ok().filter(|s| !s.is_empty())
}

fn fmt_size(n: u64) -> String {
    if n >= 1_000_000_000 { format!("{:.1} GB", n as f64 / 1e9).replace(".0 ", " ") } else { format!("{:.0} MB", n as f64 / 1e6) }
}

/// Kernel names of the disks backing the running system.
fn root_disks() -> Vec<String> {
    let mut out = vec![];
    let Some(t) = run("lsblk", &["-J", "-o", "NAME,MOUNTPOINT,PKNAME,TYPE"]) else { return out };
    let Ok(v) = serde_json::from_str::<Value>(&t) else { return out };
    fn walk(n: &Value, top: &str, out: &mut Vec<String>) {
        if let Some(mp) = n["mountpoint"].as_str() {
            if ["/", "/boot", "/boot/efi", "[SWAP]", "/home"].contains(&mp) {
                out.push(top.to_string());
            }
        }
        for c in n["children"].as_array().cloned().unwrap_or_default() {
            walk(&c, top, out);
        }
    }
    for d in v["blockdevices"].as_array().cloned().unwrap_or_default() {
        let top = d["name"].as_str().unwrap_or("").to_string();
        walk(&d, &top, &mut out);
    }
    out
}

fn truthy(v: &Value) -> bool {
    v == true || v == "1" || v == 1
}

fn sysfs(dir: &Path, attr: &str) -> String {
    std::fs::read_to_string(dir.join(attr)).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// What the hardware is: lsblk's model, or the USB product when the model is a generic class name.
fn identity(d: &Value) -> String {
    let name = d["name"].as_str().unwrap_or("");
    let mut id = d["model"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        .or_else(|| d["vendor"].as_str().map(|s| s.trim().to_string())).unwrap_or_default();
    let mut p = std::fs::canonicalize(format!("/sys/block/{name}/device")).ok();
    while let Some(dir) = p {
        if dir.join("idVendor").exists() {
            let product = sysfs(&dir, "product");
            if !product.is_empty() && (id.is_empty() || id.to_lowercase().contains("class")) {
                id = product;
            }
            break;
        }
        p = dir.parent().map(|x| x.to_path_buf()).filter(|x| x.as_os_str().len() > 1);
    }
    match d["tran"].as_str() {
        Some("mmc") => id.push_str(", SD slot"),
        Some("ieee1394") | Some("sbp") => id.push_str(", FireWire"),
        _ => {}
    }
    if id.is_empty() { "Removable disk".into() } else { id }
}

/// One line of what is on a disk now: its partitions, their file systems, labels and mount points.
fn contents(d: &Value) -> String {
    if d["size"].as_u64().unwrap_or(0) == 0 {
        return "no card inserted".into();
    }
    let parts = d["children"].as_array().cloned().unwrap_or_default();
    if parts.is_empty() {
        return match (d["fstype"].as_str(), d["pttype"].as_str()) {
            (Some(f), _) => format!("{f}, no partition table"),
            (None, Some(_)) => "partition table, no partitions".into(),
            _ => "blank".into(),
        };
    }
    let mut bits: Vec<String> = parts.iter().map(|p| {
        let mut b = format!("{} {}", p["fstype"].as_str().unwrap_or("unformatted"), fmt_size(p["size"].as_u64().unwrap_or(0)));
        if let Some(l) = p["label"].as_str().filter(|s| !s.is_empty()) {
            b += &format!(" \u{2018}{l}\u{2019}");
        }
        if p["mountpoint"].as_str().is_some() {
            b += " (mounted)";
        }
        b
    }).collect();
    if parts.iter().any(|p| p["label"].as_str().map(|l| l.eq_ignore_ascii_case("ipod")).unwrap_or(false)) {
        bits.push("looks like an iPod card".into());
    }
    bits.join(", ")
}

fn writable(path: &str) -> bool {
    OpenOptions::new().write(true).open(path).is_ok()
}

/// The disks a card can be burned to: [{name, path, size, label, contents, ok, problem}], each with why it cannot be used, if so.
/// `image_bytes`: the room the card's files need (cards::files_bytes plus the layout's own).
pub fn disks(image_bytes: u64) -> Value {
    let mut out = vec![];
    if let Some(dir) = fake_dir() {
        let mut names: Vec<_> = std::fs::read_dir(&dir).map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
        names.sort();
        for p in names {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if size == 0 {
                continue; // an empty reader slot: nothing to burn to
            }
            let path = p.display().to_string();
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(row(&name, &path, size, &format!("Test disk {name}"), if size == 0 { "no card inserted".into() } else { "test file".into() }, "usb", image_bytes, writable(&path)));
        }
        return json!({"disks": out, "test": true});
    }
    let protected = root_disks();
    let t = run("lsblk", &["-J", "-b", "-o", "NAME,SIZE,MODEL,VENDOR,SERIAL,TRAN,RM,HOTPLUG,TYPE,MOUNTPOINT,FSTYPE,LABEL,PTTYPE"]).unwrap_or_default();
    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
    for d in v["blockdevices"].as_array().cloned().unwrap_or_default() {
        let name = d["name"].as_str().unwrap_or("").to_string();
        if d["type"] != "disk" || protected.contains(&name) {
            continue;
        }
        let tran = d["tran"].as_str().unwrap_or("").to_string();
        if !(truthy(&d["rm"]) || truthy(&d["hotplug"]) || matches!(tran.as_str(), "usb" | "mmc" | "ieee1394")) {
            continue;
        }
        let path = format!("/dev/{name}");
        let size = d["size"].as_u64().or_else(|| d["size"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
        if size == 0 {
            continue; // an empty slot of a card reader shows up as a disk of 0 bytes: not offered
        }
        out.push(row(&name, &path, size, &identity(&d), contents(&d), &tran, image_bytes, writable(&path)));
    }
    json!({"disks": out, "test": false})
}

#[allow(clippy::too_many_arguments)]
fn row(name: &str, path: &str, size: u64, label: &str, contents: String, tran: &str, image_bytes: u64, can_write: bool) -> Value {
    let problem = if size == 0 {
        Some("No card in this slot.".to_string())
    } else if matches!(tran, "ieee1394" | "sbp") {
        Some("On FireWire: early iPod FireWire bridges crash on big writes. Use a USB reader.".into())
    } else if size < image_bytes {
        Some(format!("Too small: {} for the {} on this card.", fmt_size(size), fmt_size(image_bytes)))
    } else if !can_write {
        Some("emupod has no permission to write to this disk.".into())
    } else {
        None
    };
    json!({"name": name, "path": path, "size": size, "label": label, "contents": contents, "ok": problem.is_none(), "problem": problem})
}

/// The room a copy of the card at `image` needs: its files, the firmware partition and the file system's own, and some to spare.
pub fn needed(image: &Path) -> Result<u64, String> {
    Ok(crate::cards::files_bytes(image)? + 64 * 1024 * 1024)
}

/// The burn in progress or last done: {state: idle|running|done|error, card, device, label, stage, done, total, error}.
#[derive(Default)]
pub struct Burner {
    pub job: Mutex<Value>,
}

impl Burner {
    pub fn status(&self) -> Value {
        let j = self.job.lock().unwrap();
        if j.is_null() { json!({"state": "idle"}) } else { j.clone() }
    }

    /// Start burning `image` (the card `card`) onto the disk at `path`; checks first, then a thread does the work.
    pub fn start(self: &Arc<Self>, card: &str, image: &Path, path: &str) -> Result<Value, String> {
        {
            let j = self.job.lock().unwrap();
            if j["state"] == "running" {
                return Err(format!("a burn is already running (to {})", j["label"].as_str().unwrap_or("a card")));
            }
        }
        let image_bytes = needed(image)?;
        // the disk must be one the list offers, and usable
        let list = disks(image_bytes);
        let Some(d) = list["disks"].as_array().and_then(|a| a.iter().find(|d| d["path"] == path)).cloned() else {
            return Err("that disk is not one emupod can burn to (removable disks only); refresh the list".into());
        };
        if d["ok"] != true {
            return Err(d["problem"].as_str().unwrap_or("that disk cannot be used").to_string());
        }
        let src = File::open(image).map_err(|e| e.to_string())?;
        if src.try_lock_shared().is_err() {
            return Err(format!("{card} is in use: the iPod is running it with its changes kept. Boot another card first."));
        }
        let label = d["label"].as_str().unwrap_or("").to_string();
        *self.job.lock().unwrap() = json!({"state": "running", "card": card, "device": path, "label": label, "stage": "prepare",
                                           "done": 0, "total": image_bytes, "error": null, "started": crate::live::now_s()});
        let (me, path, test, image) = (self.clone(), path.to_string(), list["test"] == true, image.to_path_buf());
        std::thread::spawn(move || {
            let r = burn(&me, &image, &path, test);
            drop(src); // the image's shared lock, held for the whole burn
            let mut j = me.job.lock().unwrap();
            match r {
                Ok(()) => { j["state"] = json!("done"); j["stage"] = json!("done"); }
                Err(e) => { j["state"] = json!("error"); j["error"] = json!(e); }
            }
            j["ended"] = json!(crate::live::now_s());
        });
        Ok(self.status())
    }
}

fn mounted_parts(dev: &str) -> Vec<String> {
    let Some(t) = run("lsblk", &["-J", "-o", "NAME,MOUNTPOINT", dev]) else { return vec![] };
    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
    let mut out = vec![];
    fn walk(n: &Value, out: &mut Vec<String>) {
        if n["mountpoint"].as_str().is_some() {
            out.push(format!("/dev/{}", n["name"].as_str().unwrap_or("")));
        }
        for c in n["children"].as_array().cloned().unwrap_or_default() {
            walk(&c, out);
        }
    }
    for d in v["blockdevices"].as_array().cloned().unwrap_or_default() {
        walk(&d, &mut out);
    }
    out
}

fn burn(b: &Arc<Burner>, image: &Path, dev: &str, test: bool) -> Result<(), String> {
    let set = |s: &str, done: u64, total: u64, _item: &str| {
        let mut j = b.job.lock().unwrap();
        j["stage"] = json!(s);
        j["done"] = json!(done);
        j["total"] = json!(total);
    };
    if !test {
        set("unmount", 0, 0, "");
        for p in mounted_parts(dev) {
            if run("udisksctl", &["unmount", "--no-user-interaction", "-b", &p]).is_none() {
                let _ = run("umount", &[&p]);
            }
        }
        if !mounted_parts(dev).is_empty() {
            return Err("the card is mounted and could not be unmounted; eject it in your file manager and try again".into());
        }
    }
    let f = OpenOptions::new().read(true).write(true).open(dev).map_err(|e| format!("cannot open {dev} for writing: {e}"))?;
    crate::cards::clone_card(image, &mut crate::cards::FileDev(f), &set)?;
    if !test {
        set("eject", 1, 1, "");
        let _ = run("blockdev", &["--rereadpt", dev]);
        let _ = run("sync", &[]);
        if run("udisksctl", &["power-off", "--no-user-interaction", "-b", dev]).is_none() {
            let _ = run("eject", &[dev]);
        }
    }
    Ok(())
}
