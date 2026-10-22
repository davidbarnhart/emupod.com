# The offline iPod (David, 2026-10-18)

The iPod and its page run entirely on the phone: no server, no network once it is installed (Add to Home Screen).

## How

- **The core**: `rust/wasm` builds `pp5002::Machine` (the same Rust core the web app uses on the server) to WebAssembly,
  a plain C interface, 143 KB. The disk is behind a `Disk` trait: a file natively, the host's storage in the browser.
  The screen decoder (`lcd.rs`) and the firmware reader (`card.rs`: the osos on a card's firmware partition) are in
  Rust too.
- **Proven**: `tests/test_offline.py` plays the menus and a song on a scratch card (three synthetic songs) under Node
  (`emupod/offline/ipod_core.js`, `tests/offline/run.mjs`) and natively: the clock, all the sound (CRC), the clicks, the
  volumes and the screen are identical. 2,400 ticks (24 s of iPod time) take 0.3 s in wasm.
- **The page**: the web app's own page (`emupod.web.PAGE`, the wheel, the lock screen, the sheets) with `shim.js` in
  front. The page asks for `/state`, `/cmd`, `/audio`, `/screen.png`; the shim answers from a Worker (`worker.js`) running
  the core, paced by the clock, so the page's code is untouched and the two versions cannot drift apart.
- **Cards**: card images live in the browser's private file system (OPFS), copied in from the phone's Files
  (`cards.html`, `store.js`: a Worker with synchronous handles, the one way Safari writes a big file). The core reads
  the disk straight from the OPFS file (a sync handle), so an 8 GB card needs no memory. Writes are forgotten at
  restart (the card is never changed), as the "discard" boot does on the server.
- **Offline files and the lock screen**: `sw.js` keeps the app's files, serves `silence.wav` (the quiet looping track iOS
  wants playing for the lock-screen buttons) and fetches the cover from the page.
- Build: `cd rust && cargo build --release --target wasm32-unknown-unknown -p emupod-wasm`, then
  `python3 tools/build_offline.py` (to `work/offline/`; the web app serves it at `/offline/`).

## Seen working (headless Chromium, 2026-10-18)

Import a 200 MB card (0.8 s), boot it, the Language screen on the page, a song plays: the audio stream runs at exactly
44,100 frames/s, Now Playing's clock advances in real time.

## The card manager on the phone (2026-10-19, moved to Rust 2026-10-20)

The offline app has the hosted Cards page, unchanged: new card, partition and format, write firmware, add songs from the phone,
remove songs, rebuild, scan, duplicate, rename, delete, boot (changes kept or discarded), and changing the card the iPod is
running.

- **flashpod's Rust library does the card work.** `rust/flashpod-wasm` wraps `flashpod-core` (the Rust port of flashpod's card library,
  written by the flashpod agent in the flashpod repo's `rust/`, branch `claude/rust-m0` at the time) behind a plain C interface
  (`flashpod_wasm.wasm`, 0.9 MB): one JSON command in, one JSON answer out; the card and the songs an add reads come from host imports.
  `offline/flashpod.js` is its JS wrapper (also runs under Node). `tests/test_flashpod_wasm.py` makes the same card with the wasm and with
  the native `fpcore` binary (same ids and clock) and requires the two images to be identical, byte for byte. The dependency is a path
  (`rust/flashpod-wasm/Cargo.toml`: `../../../flashpod-card/rust/flashpod-core`): it moves when the flashpod agent reshuffles.
- **`offline/cardmgr.js` is the card store**, a port of `emupod/cards.py` (listing, each card's saved scan kept by size and mtime, one
  operation at a time per card, uploads, firmware choices, duplicate/rename/delete) in a module Worker, `offline/cardsvc.js`. Before
  2026-10-20 this was `cards.py` and flashpod's Python running in Pyodide (14 MB, seconds to start, a lot of memory); the whole app is now
  under 2 MB without the test card.
- **The images stay in OPFS** ("cards/"), opened with a synchronous access handle for the length of one call (so a card needs no
  memory); a card the iPod's Worker holds can only be looked at (scan, the song list): a read-only snapshot of its file is read with
  `FileReaderSync`. The small things are OPFS files in "cards-meta/": each card's scan (`c__NAME.json`), firmware the person added
  (`f__NAME`) and firmware downloaded (`w__FILE`, checked against the catalog's sha256). Rename and duplicate copy the image (OPFS cannot move a file on Safari).
- **The page's /api is answered by `offline/cardshost.js`, which lives in the iPod page**: the Cards frame (`cardsrpc.js` overrides its
  fetch) reaches it through `parent.cardsApi`, so the card store starts once per app session (tapping Cards starts it) and opening Cards
  again is instant; moving between the library and a card is a redraw, not a page load. An operation answers "running" at once and its
  progress comes from the worker. To change the card the iPod is running the host asks the iPod page (`window.cardsHooks`, shim.js) to
  let go of it, makes the change, and asks it to take the card again.
- **Firmware, for now (2026-10-19, David):** (3) the app downloads the catalog's file from the server it came from, `/api/firmware/FILE`
  (the flashpod cache on the server), because the files on GitHub (release assets) and Apple's own server send no CORS headers: a
  page cannot fetch them. Same origin, so it works on any network; testing only. (4) The person adds a firmware file: an .ipsw, or a
  .zip holding one (the Firmware box, "Add a firmware file"); flashpod's library checks it, it is kept in the app and listed with the others.
  Apple's 1.5 is at https://secure-appldnld.apple.com/iPod/SBML/osx/bundles/061-2686.20060912.ipTsW/iPod_1.1.5.ipsw (linked on the page).
  **To revisit before a release:** mirror the files on GitHub Pages or behind a small CORS proxy, and ship a firmware we may distribute.
- A card made on the phone is at most 4 GB (cardmgr.js `MAX_CARD`).
- Proven in headless Chromium and on the emulated Android phone (`docs/ANDROID_TEST_PHONE.md`): `tests/offline/cards_flow.py` (a card made,
  flashed, booted, played, and changed while the iPod runs it) and `tests/offline/cards_manage.py` (25 checks: create, partition, firmware from
  the server and one the person adds, songs uploaded, added, listed, removed, rebuild, duplicate, rename, delete, a reload, a rescan of the
  running card). Both need Playwright and the app over HTTPS from a web app with `/api/firmware`.
- Build: `cd rust && cargo build --release --target wasm32-unknown-unknown -p flashpod-wasm -p emupod-wasm`, then `python3 tools/build_offline.py`.

## Not yet

1. **On an iPhone** (Safari). The card manager adds to what is unproven there: sync handles for a
   several-hundred-megabyte card, the file picker for songs and firmware, how much storage the app is given.
2. **The stock card (testing only).** `tools/build_offline.py` makes a stock card, Apple's 1.5 on a minimum-size card with
   three synthetic 20-second songs, and ships it as `stock.img.gz` (2.9 MB) beside the app; a fresh install sets it up by
   itself at the first run. It carries Apple's firmware, so it is for local testing on this network only, never published
   (`work/` is not in git). `--no-stock` leaves it out: with the card manager it is no longer needed.
3. Keep the card's changes (the iPod's own writes, e.g. play counts) is wired (the Boot buttons) but not tried on a phone; a Session sheet
   that fits offline (Restart works), Developer hidden. Partition uses the catalog's default firmware and so needs the server (or a
   firmware already added).
