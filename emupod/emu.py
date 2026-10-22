"""The emulator's public interface: the one thing tools import (docs/API.md).

    from emupod import emu
    e = emu.open(emu.Config(osos="cimage13/osos-c.bin"))
    e.boot()
    e.wheel(2); e.press("select"); e.settle()
    print(e.lcd.write_count(), e.audio.underruns())

One emulator per process: the core (emupod/core/ipod_emu_ui.py) keeps its machine in module state, which
is also what lets a booted emulator be forked. So open() may be called once; a second call raises. The
configuration is explicit: open() clears the 30 OPENPOD_* variables the core reads (CORE_VARS), whatever the
caller's environment holds, sets exactly what the Config says, and only then loads the core. Other OPENPOD_*
variables (a gate's own switches) are left alone. Config.from_env() reads today's OPENPOD_*
variables for command lines and for the transition.

Nothing here reaches past this module into the core's private names; tools must not either.
"""
import builtins, os, struct, sys
from dataclasses import dataclass, field, fields
from typing import List, Optional, Tuple

from . import paths

__all__ = ["Config", "AtaTiming", "open", "Emulator", "version", "KEYS", "LINES"]

KEYS = ("select", "menu", "play", "ff", "rew")           # port A bits 1, 4, 2, 0, 3, down while LOW (recording.KEYS;
                                                        # before 2026-10-16 play/ff/rew named bits 0/3/2: see there)
LINES = ("hold", "firewire", "cna", "battery")

_opened = None
PLL_CTL = 0xcf004010
# The click. Stock 1.5's click routine (0x280374f8, 37 instructions) runs once for each item the wheel moves and
# once for each button press, and never when nothing moves (a list's end, the volume): it writes 0 here, then
# sets up the serial port at 0xc0006040 (measured 2026-10-16: on 24 of 24 wheel edges that moved the highlight
# and 0 of 132 that did not). That write is the iPod's click, the one its piezo makes.
CLICKER = 0xcf004040
CLICK_FN = (0x000374f8, 0x000374f8 + 148)               # the click routine, low alias (0x28000000 more in DRAM)
# Not every write of the clicker is a click: another routine (pc 0x37754) writes it too, at idle (at the
# Language screen, seen in a web session as 5 writes every 15 s). Only the click routine's writes count.
# The variables the core reads (emupod/core/*.py), and so the only ones open() owns. Every other OPENPOD_*
# (the 1.6 builders' and gates' switches, e.g. OPENPOD_STFAST_RANGE, and their _MUTANT twins) is left alone.
CORE_VARS = tuple("OPENPOD_" + k for k in (
    "ATA_BLOCK_US", "ATA_BSY", "ATA_CMD_US", "ATA_LBA48", "AUDIO_FPF", "CLOCK", "COP", "COP_IPUS", "CPU_MODEL",
    "DISK", "DISK_PERSIST", "EXTRA", "FAST", "FAT", "FIREWIRE_CABLE", "FLASHPOD", "IDLE_SLEEP", "INTERLEAVE",
    "INTERLEAVE_PLAYING", "INTERLEAVE_SETTLE", "ATA_FAST", "START_VOLUME", "IPSW", "IPUS", "KEYS_UP", "LEGACY", "OSOS", "PATCH_REBOOT", "PLUGGED", "QUANTUM"))


@dataclass
class AtaTiming:
    """The disk model's timing (OPENPOD_ATA_*). Zeros are the instant disk every older result used."""
    bsy_reads: int = 0          # extra status reads that see BSY after a command
    cmd_us: int = 0             # iPod time a command takes
    block_us: int = 0           # iPod time per 512-byte block
    lba48: Optional[bool] = None    # None: automatic (past 2^28 sectors)


@dataclass
class Config:
    """Everything that decides what the emulator is. None means the core's default."""
    osos: Optional[str] = None              # firmware image (default: the gen1 stock osos from setup);
                                            # "card": the osos in the disk's firmware partition (emupod.card)
    ipsw: Optional[str] = None
    disk: Optional[str] = None              # whole-disk image (default: work/gen1/emu-disk.img)
    disk_persist: bool = False              # write the firmware's disk writes back to the file
    fat: Optional[str] = None               # the old bare-FAT image (implies legacy)
    extra: List[Tuple[str, int]] = field(default_factory=list)   # [(file, address)] mapped into both cores
    legacy: bool = False                    # the old single-core budget clock
    fast: Optional[str] = None              # ipod_emu_fast mode
    cop: bool = True                        # run the second core (virtual clock only)
    clock: Optional[str] = None             # "virtual" (default) or "budget"
    ipus: Optional[int] = None              # CPU instructions per microsecond (13)
    cop_ipus: Optional[int] = None
    interleave: Optional[int] = None
    interleave_playing: Optional[int] = None   # longer turns once a song has played interleave_settle us (0: off)
    interleave_settle: Optional[int] = None
    ata_fast: bool = False                  # the firmware's disk-read routine done a block at a time (core notes)
    start_volume: Optional[int] = None      # the volume (0..127) the iPod starts at: its factory volume, set before boot (1.5-based)
    quantum: Optional[int] = None
    cpu_model: Optional[str] = None
    keys_up: Optional[bool] = None
    plugged: Optional[bool] = None
    firewire_cable: bool = False
    idle_sleep: bool = True
    backend: str = "rust"                       # the Rust core (emu_rs)
    audio_fpf: Optional[str] = None         # frames per FIQ: "8" (default), a number, or "auto"
    patch_reboot: bool = False
    flashpod: Optional[str] = None          # make_emu_disk's flashpod
    ata: AtaTiming = field(default_factory=AtaTiming)

    def env(self):
        """The OPENPOD_* variables this configuration means (what the core reads when it loads)."""
        e = {}
        def put(k, v):
            if v is not None: e["OPENPOD_" + k] = str(v)
        osos = self.osos
        if osos == "card":                  # boot what the card carries, as the boot ROM does
            from . import card
            disk = self.disk or paths.work_paths("gen1")["disk"]
            osos = card.osos_file(disk, os.path.join(paths.WORK, "card-osos"))[0]
        put("OSOS", osos or os.path.join(paths.work_paths("gen1")["osos"]))
        put("IPSW", self.ipsw or paths.ipsw_path("gen1"))
        fat = self.fat or (paths.work_paths("gen1")["fat"] if self.legacy and not self.disk else None)
        put("DISK", self.disk or (None if fat else paths.work_paths("gen1")["disk"]))
        if self.disk_persist: put("DISK_PERSIST", 1)
        put("FAT", fat)
        if self.extra:
            if len(self.extra) != 1: raise ValueError("the core maps one extra region (OPENPOD_EXTRA)")
            f, a = self.extra[0]; put("EXTRA", "%s@0x%x" % (f, a))
        if self.legacy: put("LEGACY", 1)
        put("FAST", self.fast)
        if not self.cop: put("COP", 0)
        put("CLOCK", self.clock); put("IPUS", self.ipus); put("COP_IPUS", self.cop_ipus)
        put("INTERLEAVE", self.interleave)
        put("INTERLEAVE_PLAYING", self.interleave_playing); put("INTERLEAVE_SETTLE", self.interleave_settle)
        if self.ata_fast: put("ATA_FAST", 1)
        put("START_VOLUME", self.start_volume)
        put("QUANTUM", self.quantum); put("CPU_MODEL", self.cpu_model)
        if self.keys_up is not None: put("KEYS_UP", int(self.keys_up))
        if self.plugged is not None: put("PLUGGED", int(self.plugged))
        if self.firewire_cable: put("FIREWIRE_CABLE", 1)
        if not self.idle_sleep: put("IDLE_SLEEP", 0)
        put("AUDIO_FPF", self.audio_fpf)
        if self.patch_reboot: put("PATCH_REBOOT", 1)
        put("FLASHPOD", self.flashpod)
        a = self.ata
        if a.bsy_reads: put("ATA_BSY", a.bsy_reads)
        if a.cmd_us: put("ATA_CMD_US", a.cmd_us)
        if a.block_us: put("ATA_BLOCK_US", a.block_us)
        if a.lba48: put("ATA_LBA48", 1)
        return e

    @classmethod
    def from_env(cls, environ=None):
        """A Config from today's OPENPOD_* variables (command lines, and tools during the port)."""
        g = (environ if environ is not None else os.environ).get
        def opt_int(k): v = g("OPENPOD_" + k); return int(v) if v not in (None, "") else None
        def opt_bool(k): v = g("OPENPOD_" + k); return None if v in (None, "") else v == "1"
        extra = []
        if g("OPENPOD_EXTRA"):
            f, a = g("OPENPOD_EXTRA").rsplit("@", 1); extra = [(f, int(a, 0))]
        return cls(osos=g("OPENPOD_OSOS"), ipsw=g("OPENPOD_IPSW"), disk=g("OPENPOD_DISK"),
                   disk_persist=g("OPENPOD_DISK_PERSIST") == "1", fat=g("OPENPOD_FAT"), extra=extra,
                   legacy=g("OPENPOD_LEGACY") == "1", fast=g("OPENPOD_FAST"), cop=g("OPENPOD_COP", "1") != "0",
                   clock=g("OPENPOD_CLOCK"), ipus=opt_int("IPUS"), cop_ipus=opt_int("COP_IPUS"),
                   interleave=opt_int("INTERLEAVE"), quantum=opt_int("QUANTUM"), cpu_model=g("OPENPOD_CPU_MODEL"),
                   interleave_playing=opt_int("INTERLEAVE_PLAYING"), interleave_settle=opt_int("INTERLEAVE_SETTLE"),
                   ata_fast=g("OPENPOD_ATA_FAST") == "1", start_volume=opt_int("START_VOLUME"),
                   keys_up=opt_bool("KEYS_UP"), plugged=opt_bool("PLUGGED"),
                   firewire_cable=g("OPENPOD_FIREWIRE_CABLE") == "1", idle_sleep=g("OPENPOD_IDLE_SLEEP", "1") != "0",
                   audio_fpf=g("OPENPOD_AUDIO_FPF"), patch_reboot=g("OPENPOD_PATCH_REBOOT") == "1",
                   flashpod=g("OPENPOD_FLASHPOD"),
                   ata=AtaTiming(bsy_reads=opt_int("ATA_BSY") or 0, cmd_us=opt_int("ATA_CMD_US") or 0,
                                 block_us=opt_int("ATA_BLOCK_US") or 0, lba48=opt_bool("ATA_LBA48")))


def version():
    """Which emulator this is: emupod's commit (+dirty), for tool outputs."""
    import subprocess
    try:
        rev = subprocess.run(["git", "-C", paths.ROOT, "describe", "--always", "--dirty"], capture_output=True,
                             text=True, timeout=5).stdout.strip() or "unknown"
    except Exception:
        rev = "unknown"
    return dict(emupod=rev)


def open(config=None):
    """Load the emulator with `config` (default Config()). Once per process."""
    global _opened
    if _opened is not None: raise RuntimeError("emupod.emu.open() was already called in this process")
    config = config or Config()
    if config.backend != "rust": raise ValueError("backend: only 'rust' is supported, not %r" % (config.backend,))
    from . import emu_rs
    _opened = emu_rs.RustEmulator(config)
    return _opened
