//! Study: which SCSI commands the disk-mode program takes, and what it answers (data, SCSI status, sense), for an iSCSI target that
//! passes a host's commands on to it.
//!     scsiprobe <card.img> [cdb-hex[:length]]...
//! Boots the card's own disk-mode program with a cable in, logs in as the host, then sends each CDB as a command ORB asking for
//! `length` bytes (0 if none given) and prints the answer. With no CDBs it sends the set a Linux or Windows host sends.
use pp5002::{ata::FileDisk, flash, sbp2, Config, Machine};

const SET: &[&str] = &[
    "000000000000",           // TEST UNIT READY
    "030000001200:18",        // REQUEST SENSE
    "120000002400:36",        // INQUIRY
    "120000006000:96",        // INQUIRY, 96 bytes (Linux asks this)
    "120100000400:4",         // INQUIRY EVPD 00: supported pages
    "120180000400:4",         // INQUIRY EVPD 80: serial number
    "120183000400:4",         // INQUIRY EVPD 83: device identification
    "1201b0000400:4",         // INQUIRY EVPD b0: block limits
    "250000000000000000:8",   // READ CAPACITY(10)
    "9e10000000000000000000000020:32", // READ CAPACITY(16)
    "1a003f00c000:192",       // MODE SENSE(6), all pages
    "1a000800c000:192",       // MODE SENSE(6), caching page
    "1a001c00c000:192",       // MODE SENSE(6), informational exceptions
    "5a003f00000000000c00:192", // MODE SENSE(10), all pages
    "1e0000000100",           // PREVENT ALLOW MEDIUM REMOVAL: prevent
    "1e0000000000",           // allow
    "35000000000000000000",   // SYNCHRONIZE CACHE(10)
    "2f000000000000000100",   // VERIFY(10), 1 block
    "28000000000000000100:512", // READ(10) LBA 0, 1 block
    "88000000000000000000000000010000:512", // READ(16) LBA 0, 1 block
    "a00000000000000000000400:1024", // REPORT LUNS
    "23000000000000000c00:12", // READ FORMAT CAPACITIES
    "4a010000100000000800:8", // GET EVENT STATUS NOTIFICATION
    "28000000000000000000:0", // READ(10), 0 blocks
    "280000ffffff00000100:512", // READ(10) past the end
    "ff0000000000",           // an opcode nobody has
    "1b0000000100",           // START STOP UNIT: start
];

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let open = || FileDisk(std::fs::OpenOptions::new().read(true).write(true).open(&a[1]).unwrap());
    let aupd = pp5002::card::read_image(&open(), b"aupd").expect("aupd").0;
    let chip = flash::assemble(&aupd).expect("assemble");
    let mut m = Machine::from_chip(&chip, Box::new(open()), Config { plugged: true, ..Config::default() });
    for (k, b) in b"diskmode".iter().enumerate() {
        m.s.host_wr(sbp2::TOKEN_AT + k as u32, *b);
    }
    sbp2::run_for(&mut m, 7_000_000);
    let s = sbp2::connect(&mut m, 256, 10_000_000).expect("login");
    println!("logged in: {} sectors", s.blocks);
    let given: Vec<&str> = a[2..].iter().map(|x| x.as_str()).collect();
    for h in if given.is_empty() { SET.to_vec() } else { given } {
        let (c, len) = h.split_once(':').map(|(c, l)| (c, l.parse().unwrap())).unwrap_or((h, 0u32));
        let cdb: Vec<u8> = (0..c.len() / 2).map(|k| u8::from_str_radix(&c[2 * k..2 * k + 2], 16).unwrap()).collect();
        match sbp2::exec(&mut m, &cdb, len, None) {
            Ok(r) => {
                let d = if r.data.len() > 48 { format!("{}... ({} bytes)", hex(&r.data[..48]), r.data.len()) } else { format!("{} ({} bytes)", hex(&r.data), r.data.len()) };
                let sense = if r.sense.is_empty() { String::new() } else { format!(" sense key {:x} asc {:02x} ascq {:02x}", r.sense[2] & 0xf, r.sense[12], r.sense[13]) };
                println!("{c:<34} status {:02x}{sense}  data {d}", r.status);
            }
            Err(e) => println!("{c:<34} TRANSPORT: {e}"),
        }
        sbp2::run_for(&mut m, 100_000);
    }
}
