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
