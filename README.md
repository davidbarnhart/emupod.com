# emupod

**An iPod in your browser.** emupod runs the 1st/2nd-generation Apple iPod's real, unmodified software on an emulated
PortalPlayer PP5002 chip (two ARM7TDMI cores). You see the screen, use the click wheel and hear the music.

- **Try it:** [emupod.com](https://emupod.com) runs a demo entirely in your browser (WebAssembly): one built-in card with one song.
- **Run your own:** put emupod on a Raspberry Pi or any Linux computer at home and use the iPod from your phone's browser, with your own music.

## Install (Raspberry Pi or Linux, 64-bit)

```sh
curl -fsSL https://emupod.com/install.sh | sh
```

The script (read it first: it is short) walks you through a few numbered steps: it downloads emupod (one program, for your machine) and
checks its checksum, puts it in `~/emupod`, finds your music and asks which folder to use, picks a free port, can allow burning cards to a
flash card reader (with sudo, once), can start emupod at boot, and prints the address to open on your phone. The first start puts the
built-in card, Rainbow Road, in place. Nothing else to install first: no Python, no compiler.

Tested so far: 64-bit Debian on x86-64 and Raspberry Pi 4 (ARM).

## Disk mode (FireWire)

The page's **FireWire cable** button does what plugging in a cable does: the iPod reboots into its own disk-mode program ("Do not
disconnect"). emupod then plays the computer on the other end of the cable. It logs in over SBP-2 and serves the iPod to the machine
emupod runs on as a block device, an [NBD](https://github.com/NetworkBlockDevice/nbd/blob/master/doc/proto.md) export named `ipod` on
`127.0.0.1:10809`. Every read and write goes through the iPod's disk-mode program, as SCSI READ(10)/WRITE(10) commands of up to 256 sectors (page-table ORBs past 127, as a Linux host sends).

**Mounted for you.** emupod runs as your own user, without root; mounting a disk needs root. Set up its small mount service once
(the installer offers it):

```sh
sudo ~/emupod/bin/emupod setup-automount        # undo: sudo ~/emupod/bin/emupod remove-automount
```

From then on the iPod's disk is mounted at `/media/emupod` (under `/media`, so your file manager and music programs such as Rhythmbox see it), owned by you, a few seconds after you click the cable in. When you click it
out, emupod unmounts it first and lets the cable out only once that worked; if a program still has a file open there, the cable stays in
and the page says which program. The service runs as root and does only that, for your user alone: it takes requests on a socket
only you can use, and it attaches only emupod's export (`--nbd`, default `127.0.0.1:10809`) at only its mount point (`--at`, default
`/media/emupod`), both fixed when you set it up. It runs its own root-owned copy of emupod, so run the setup again after an upgrade (the
installer does).

**By hand** (root, and the `nbd-client` package):

```sh
sudo modprobe nbd max_part=8
sudo nbd-client 127.0.0.1 10809 /dev/nbd0 -N ipod     # or: sudo qemu-nbd -f raw -c /dev/nbd0 nbd://127.0.0.1:10809/ipod
sudo mkdir -p /media/emupod && sudo mount /dev/nbd0p2 /media/emupod
```

Unmount and detach (`sudo umount /media/emupod && sudo nbd-client -d /dev/nbd0`) before you click the cable out, as with a real iPod. A card
booted with its changes kept is written to. Any other card, the built-in one among them, keeps the writes in memory until the iPod
restarts (256 MB at most), and its file is never changed. `--nbd ADDR:PORT` serves it somewhere else, such as your VPN address; emupod
refuses 0.0.0.0. `--nbd off` turns it off.

It is also an **iSCSI target**, `iqn.2026-10.com.emupod:ipod` on `127.0.0.1:3260`. Here the computer's own SCSI commands go to the
iPod's disk-mode program, so an eject reaches the iPod itself ("OK to disconnect"). A session needs CHAP: the user is `emupod`, and the
secret is in `~/.local/share/emupod/iscsi-secret` (made on first run, readable by you alone, never logged). With open-iscsi (root):

```sh
sudo iscsiadm -m discovery -t st -p 127.0.0.1:3260
sudo iscsiadm -m node -T iqn.2026-10.com.emupod:ipod -o update -n node.session.auth.authmethod -v CHAP
sudo iscsiadm -m node -T iqn.2026-10.com.emupod:ipod -o update -n node.session.auth.username -v emupod
sudo iscsiadm -m node -T iqn.2026-10.com.emupod:ipod -o update -n node.session.auth.password -v "$(cat ~/.local/share/emupod/iscsi-secret)"
sudo iscsiadm -m node -T iqn.2026-10.com.emupod:ipod --login     # the iPod is a new /dev/sdX; mount its second partition
```

Unmount and `--logout` before the cable comes out. Only one kind of host has the disk at a time: while an iSCSI session is logged in, NBD
is refused, and the other way round. `--iscsi ADDR:PORT` and `--iscsi off` work as `--nbd`'s do, and `--iscsi-secret FILE` keeps the
secret somewhere else.

## What is in this repository

| | |
|---|---|
| `rust/` | the emulator: `armcore` (ARM7TDMI), `pp5002` (the chip: memory, timers, interrupts, disk, LCD, audio), `pybind` (the Python module), `wasm` (the browser build) |
| `rust/server/` | **emupod itself**: the web server, the card store (through [flashpod](https://github.com/davidbarnhart/flashpod)'s library), card burning, sound on the host |
| `emupod/` | the pages (`emupod/pages/`), the browser app that also runs offline on a phone, the site; and the older Python server |
| `installer/`, `Dockerfile`, `.github/` | the installer and how releases and the site are built |
| `tools/` | `build_offline.py` builds the browser app and the demo site |
| `tests/` | tests of the core, the cards and the browser app |
| `docs/` | API, architecture, notes on the firmware |

## Build from source

You need Rust (stable).

```sh
cd rust && cargo build --release -p emupod-server
target/release/emupod web --host 0.0.0.0 --port 8797 --cards ~/emupod-cards
```

The browser build and the demo site: `cd rust && cargo build --release --target wasm32-unknown-unknown -p emupod-wasm -p flashpod-wasm`,
then `python3 tools/build_offline.py OUT --demo-site --demo-card CARD.img`.

## About Apple's software

emupod contains **no Apple software**. The firmware is downloaded onto your own machine when you set up a card (Apple's original builds and
patched builds, from the [flashpod](https://github.com/davidbarnhart/flashpod) project's releases). The demo on emupod.com
includes one prepared card with firmware on it. iPod and Apple are trademarks of Apple Inc.; this project is not affiliated with or endorsed by Apple.

## License

MIT, see [LICENSE](LICENSE). Card management is [flashpod](https://github.com/davidbarnhart/flashpod) (MIT).
