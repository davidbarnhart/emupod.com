# emupod architecture

## The machine: PortalPlayer PP5002

The 1st–3rd-gen iPods run on the PP5002 SoC:

- **2× ARM7TDMI** at ~90 MHz. The first runs the OS; the second (the **COP**) is the audio/DSP
  core, asleep until the OS wakes it to decode audio.
- **Memory map**: SDRAM is aliased at `0x00000000`, `0x04000000`, and `0x28000000` (firmware
  links at `0x28000000`). On-chip **IRAM** at `0x40000000`.
- **Peripherals (MMIO)**: ATA/IDE taskfile at `0xc00031e0`; HD66753 **LCD** at `0xc0001xxx`;
  the **timer + interrupt controller** at `0xcf001xxx`; processor/COP control at `0xcf004xxx`.

## Emulation strategy

emupod runs the **real firmware** instructions on a real ARM core (the Unicorn/QEMU engine) and
**models the peripherals in Python**, trapping every MMIO access. The model is *grown against the
real boot*: when the firmware reads a register the model doesn't handle yet, the trap-and-log
harness (`emupod trace`) surfaces it, and we add the behaviour that makes the boot progress. This
is how gen-1 was brought from reset all the way to the LCD.

## Boot path (gen-1, working)

```
reset 0x28000000 ──► reset handler 0x2800012c
   └─ dual-core check (PROCESSOR_ID 0xc4000000 == 0x55555555 on the main CPU)
   └─ IRAM relocation gate ──► crt0 C-runtime init
   └─ RTXC scheduler, driven by the TIMER1 IRQ (bit 11) from the faithful timer model
   └─ COP-sleep handshake (PROC_STAT 0xcf004050 bit14 "COP asleep")
   └─ ATA disk brought up; firmware mounts the MBR+FAT32 data partition, writes the iPod dir
   └─ the firmware draws the LCD ──► the first-run Language menu
   └─ (emupod play) Music > Artists > ... > song ──► COP woken ──► MP3 decode ──► DAC samples
```

## Code today (`emupod.core`)

The engine is a set of scripts, each the result of a growth stage in the openpod RE effort. They
self-add their directory to `sys.path` and import each other by bare module name, so they run
both standalone and via the CLI:

| Module | Role |
|--------|------|
| `ipod_emu_ui.py` | **flagship** — boots gen-1, walks the menus, plays a song on the COP; LCD, wheel/buttons, event lines, ATA, the clock |
| `ipod_emu_fast.py` | shortcuts for the firmware's two zero-fill loops (`--fast`) |
| `ipod_emu_who.py` | function and RTXC-task names; call tracing on both cores |
| `make_emu_disk.py` | builds the standard run's whole-card disk with music (ffmpeg + flashpod) |
| `ipod_emu_timer.py` | faithful PP5002 timer + interrupt-controller model |
| `ipod_emu_dual.py` | dual-core (CPU + COP) proof of concept — the audio-core seed |
| `ipod_emu_bootrom.py` | PP5002 boot-ROM stand-in (multi-stage reset/hand-off) |
| `ipod_emu.py` | trap-and-log diagnostic (discovers needed MMIO) |
| `lcd_render.py` | HD66753 decoder/renderer (MMIO stream → framebuffer/PNG) |
| `ipod_flash.py` | iPod disk layout (MBR/FAT32 + firmware partition) |
| `ipsw_unpack.py` | split an `.ipsw` into its osos/aupd/rsrc images |
| `fw_disasm.py` | annotated ARM disassembler |

**Roadmap item #1** is to refactor these into a clean `soc` + `devices` + `Machine` package
(see [ROADMAP.md](ROADMAP.md)) while preserving every working constant — the scripts are the
ground truth until then.

## Provenance

emupod was split out of **openpod**, where this emulator was built to chase firmware bugs without
risking real hardware. The reverse-engineering write-ups for the PP5002 boot, LCD, storage, and
the dual-core model live in openpod's wiki; emupod carries the emulator forward as its own thing.
[`emupod/core/PROVENANCE.md`](../emupod/core/PROVENANCE.md) records the openpod commit each module
was last synced from and the few changes emupod makes on top.
