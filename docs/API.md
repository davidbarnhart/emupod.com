# `emupod.emu`: the emulator's public interface

This is the only module tools import. `emupod.core` is private, and so is every name with a leading
underscore. The checks are in `tests/test_api.py` (`python3 tests/test_api.py`).

```python
from emupod import emu
e = emu.open(emu.Config(osos="cimage13/osos-c.bin"))   # once per process
e.boot()
e.press("select"); e.settle()
e.wheel(2); e.settle()
print(e.lcd.write_count(), e.lcd.digest(), e.events("fault"))
```

## Configuration

**`emu.open(config)` can be called once per process.** The core keeps the machine in module state,
which is also what lets a booted emulator be forked. A second call raises an error, and so does a
call after something imported the core directly.

**The configuration is explicit:**

1. `open()` deletes the 26 `OPENPOD_*` variables the core reads (`emu.CORE_VARS`), whatever the
   caller's shell holds. Every other `OPENPOD_*` variable is left alone: the 1.6 builders' and
   gates' switches, such as `OPENPOD_STFAST_RANGE` and the `_MUTANT` twins
   (`tests/test_api.py other_env_survives`).
2. It sets exactly the variables the `Config` means (`Config.env()` shows them).
3. Only then does it load the core.

So a stray `OPENPOD_LEGACY=1` in a shell has no effect (`tests/test_api.py env_is_explicit`).
`Config.from_env()` builds a `Config` from today's variables, for command lines and for tools
during the port.

**How this differs from the plan's wording.** The plan said configuration would move out of import
time. It does, as far as any caller can tell, but the core module is unchanged: `open()` is the
only reader of `os.environ` that matters, and the core still reads the values it is given when it
loads. Keeping the core byte-identical means every regression number holds by construction. The
module-level reads can be replaced inside the core later without changing this interface.

| `Config` field | meaning (default) |
|---|---|
| `osos`, `ipsw` | firmware image (gen1 stock from `emupod setup`), or `"card"`: the osos in the disk's firmware partition, read as the boot ROM finds it (`emupod/card.py`; a card without a bootable osos is refused with the reason); and the ipsw (`roms/iPod_1.1.5.ipsw`) |
| `disk`, `disk_persist` | whole-disk image (`work/gen1/emu-disk.img`); write the firmware's writes back to it |
| `fat`, `legacy` | the old bare-FAT volume and single-core budget clock (`legacy=True` uses `work/gen1/data_fat.img`) |
| `extra` | `[(file, address)]` mapped into both cores (one region) |
| `cop`, `clock`, `ipus`, `cop_ipus`, `interleave`, `quantum`, `cpu_model`, `fast` | the machine's timing model |
| `keys_up`, `plugged`, `firewire_cable`, `idle_sleep`, `patch_reboot` | lines and behaviour at reset |
| `audio_fpf` | frames per audio FIQ (`"8"`, a number, or `"auto"`: paced by what each FIQ delivers, the real rate; the default 8 plays at three quarters speed, kept so old results compare) |
| `interleave_playing`, `interleave_settle` | longer core turns (us) once the audio has been on `interleave_settle` us (off: 0) |
| `ata_fast` | the firmware's disk-read routine done a block at a time (found by its instructions) |
| `start_volume` | the volume (0..127) the iPod starts at: the firmware's factory volume is set to it before boot (1.5-based firmware only) |
| `ata=AtaTiming(bsy_reads, cmd_us, block_us, lba48)` | the disk model's timing (all 0: instant) |
| `flashpod` | the FlashPod used to build disks |

The last four are off by default, so a tool's results do not change unless it asks. Together they are what the
web app runs with, `recording.LIVE_TIMING` (1.33x real time on a desktop PC while a song plays, against 0.75x
without, and the sound at its real rate); a recording made with them stores them (`meta.emulator.timing`) and
replay uses them.

`emu.version()` returns `{emupod: <git describe>, core_sha256: ...}`. Write it into every
tool's output.

## The emulator

| area | call |
|---|---|
| lifecycle | `reset_lines(hold=, firewire=, cna=, battery=)` before boot; `boot()`; `fork()` (the child reopens the disk image, so the two never share a file offset) |
| time | `now_us()`; `ticks()` (timer ticks since reset); `halts()` (CPU idle halts since reset; "idle" is halts >= 90% of ticks over a stretch); `tick_period_us()` (the timer period the firmware programmed; ticks() is not exactly time / period, since ticks missed while interrupts are masked are dropped, so openpod's walkers' idle test is halts delta >= 0.9 × now_us() delta / tick_period_us()); `tick()` (10.2 ms on the virtual clock); `run_for(us)`; `run_until(cond, max_us)`; `settle(max_iters, quiet)` |
| keys | `key_down(k, timing="thumb")`, `key_up(k, timing="thumb")`, `key_level(k, level, timing="thumb")` (drive the line to an explicit level and deliver the edge even if it is already there), `press(k, hold_ticks=8, timing="thumb")`, `hold(k, us)`; `k` in `emu.KEYS` (select, menu, play, ff, rew) |
| edges, click | `wheel_edges(n, tick=None)`: `n` single quadrature edges (a freely turning 1G wheel; the firmware moves an item every 4-6, 6 after a pause); `click_count()` and `events("click")`: the firmware's own clicks (its clicker register, 0xcf004040: once per item moved and per button press, never at a list's end) |
| wheel | `wheel(detents, timing="thumb")`: + toward later items, four edges with a tick after each. `timing="core"` on `press`, `wheel`, `key_down` and `key_up` gives the core's own timing instead (the core's `_set` + `_edge`: eight ticks after every edge; a core key_down moves the line to the level it is not at, and the matching key_up puts it back), the timing openpod's walkers were measured with; use it to port them byte-identically. The first detent after boot or a pause often moves nothing; that is the firmware. |
| lines | `line(name, level)` delivers the edge; `line_levels()`; `gpio_level(port, mask)` |
| menu | `selected()`, `screen()`, `item_count()`, `choose(index, before)`, `play_first(each, seconds=None)` (seconds: how much sound to wait for once the song starts; None = the core's 1.0), `play(seconds, wait)` |
| LCD | `lcd.write_count()` (stock boot: 5,307 standard, 5,294 legacy), `lcd.writes(since)`, `lcd.frame()` (160 x 128 levels 0-3), `lcd.digest()` (the recordings' screen hash), `lcd.png(path, scale)` |
| audio | `audio.enabled`, `audio.take()` (the sound since the last take, s16le stereo bytes, removed from `samples()`), `audio.volume()` (the codec's headphone volume: `{left, right, left_db, right_db}`; the samples are before it), `audio.samples()`, `audio.start_times()`, `audio.underruns()`, `audio.write_wav(path, start)`, `audio.sample_rate`; `audio.on_fiq(fn)` (fn(delivered) after every audio FIQ), `audio.on_enable(fn)` (fn(on, was_on) after every switch of the output), `audio.remove(h)` |
| disk | `disk.path`, `disk.commands()` (ATA log: (command, features, count, lba, outcome)); `disk.on_command(fn, when="after")` (fn(code, entry) after the drive model takes each command; when="before": fn(code, taskfile) first), `disk.remove(h)`; `disk.taskfile()`; `disk.busy(us)` (hold BSY that much longer from now: a seek or spin-up as stimulus); `disk.set_latency(cmd_us, block_us)` (at run time; Config.ata sets it at open); `disk.busy_us()`; `disk.overlay()` ({lba: 512 bytes} the firmware wrote this run) |
| memory | `mem_read(addr, n, core)`, `mem_write(addr, data, core)`, `reg(name, core)`, `set_reg(name, v, core)`; `cpu` / `cop` views with the same, plus `u32(addr)` |
| firmware | `fw_call(addr, *args, max_instructions=200000)` (a query on a private stack, r0-r3 = the first four args; registers put back, the clock does not move; returns r0, or None unless it came back to its return address, so an exhausted budget is None, not a stale r0) (arguments past the fourth at [sp], [sp+4], ...; it starts from the paused CPU's registers and keeps every memory write it makes); `scratch(size, core="cpu")` (fresh zeroed host-only memory at 0x7f000000+, outside the iPod's map, for records a fw_call is handed); `where(pc)` (function + task) |
| hooks | `h = add_hook(kind, fn, begin, end, core)` with `kind` block / code / mem_write / mem_read and `core` cpu / cop / both; `fn(core_view, addr, size, value)`, where `addr` is the raw address Unicorn reports (no alias folding: 0x28xxxxxx and its low alias stay distinct); `remove_hook(h)`. Adding a hook flushes that core's translation cache, so code already run is caught too (don't add one from inside a hook's callback). `add_hook("resume", fn, core=...)` calls `fn(core_view)` just before every (re)start of that core's emulation (each emu_start; the core runs each processor in many short turns), so a tracer can tell a restart from a control-flow edge. The default `core="both"` also sees the COP; a tool that hooked only the CPU's Unicorn before passes `core="cpu"` |
| idle | `events("halt")` is what the core calls `_sleeps`: CPU idle halts (PROC_SLEEP), many a second. Tools that wait for "the CPU is idle" count halts per timer period (firmware #931: fw_cover --probe-idle, scenarios, fw_dblimit, test_recshrink). It is NOT the auto-sleep, which is `events("sleep")`. |
| effective | `effective()`: what the core resolved and runs (clock, plugged, cop, ipus, quantum, interleave, keys_up, idle_sleep, audio_fpf, osos, disk, fat, legacy, ata, ...), JSON-ready; record this as a run's configuration, not the Config (whose None means "default") |
| irq | `irq_mask_writes()`: the last value written to each interrupt-controller register (0xcf001000..0xcf00107c, and the COP's interrupt enable/disable), by address; a copy |
| events | `events("fault")`, `"reboot"`, `"sleep"` (`(time_us, value)` of each PLL-control write: the idle auto-sleep makes them, stock first at about 123 s, but not only it: releasing a REW held in a song wrote 0x6000 then 0xc000 11 ms apart at 24.2 s; a tool that means "auto-sleep" should look at the time too), `"halt"` (each CPU idle halt; many a second, NOT the auto-sleep), `"swi"`, `"cop_dead"`; `cop_alive` |

## The cost of hooks

Measured on a desktop PC, 2026-10-16 (`scripts/hookbench.py`), with a stack-style block hook that reads
two registers, over boot plus 1,500 ticks:

| hook | time |
|---|---|
| none | 1.5 s |
| direct Unicorn `hook_add` | 54.9 s |
| `add_hook` | 54.6 s |

The interface adds nothing measurable once register ids are cached. What costs is any Python
hook on every block, about 36 times slower. Narrow a hook with `begin`/`end` wherever you can.

## What the tests check

On a desktop PC, 2026-10-15, 9 of 9 pass:

- **`boot_std`:** a stock boot makes 5,307 LCD writes, with 0 faults and the COP alive.
- **`boot_legacy`:** a legacy boot makes 5,294 LCD writes.
- **`sound_std`:** `play_first` then 2 s gives a WAV with sha256 `ebffd4d8d8673f80...`, the standard
  run's sound.
- **`open_twice`:** a second `open()` raises.
- **`env_is_explicit`:** a stray `OPENPOD_LEGACY=1` in the environment does not reach the core.
- **`fork_disk`:** the child's disk handle is not the parent's.
- **`hooks_both_cores`:** a block hook sees both cores during boot, and `remove_hook` stops it.
- **`wheel_and_keys`:** Select at Language opens the menu, and two detents move the highlight.
- **`idle_sleep`:** stock auto-sleeps between 110 and 140 s.
