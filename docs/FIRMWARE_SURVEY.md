# Which firmware the emulator runs (David asked, 2026-10-19)

Method: every Apple 1G/2G release in flashpod's catalog (1.0, 1.0.2, 1.0.4, 1.1, 1.2, 1.2.1, 1.2.2, 1.2.6, 1.3, 1.4, 1.5) and flashpod 1.6, each flashed to a 120 MB scratch card with a
synthetic MP3 and a synthetic FLAC song, booted on the Rust core with the web app's timing (`LIVE_TIMING`), then driven through the menus. (Tools: tools/fwstudy/.)

## Final result (2026-10-19): every Apple 1G/2G release and flashpod 1.6 play MP3 on the Rust core

| firmware | boots | menus respond | MP3 plays | needed |
|---|---|---|---|---|
| flashpod 1.6 | yes | yes | yes (and FLAC) | |
| Apple 1.5 | yes | yes | yes | |
| Apple 1.4, 1.3, 1.2.6, 1.2.2, 1.2.1 | yes | yes | yes | the timer acknowledge on reading TIMER1_VAL |
| Apple 1.2, 1.1 | yes | yes | yes | + the chip-id registers (0xcf004030..3c) |
| Apple 1.0.4, 1.0.2, 1.0 | yes | yes | yes | + port C bit 3 high by default (battery line) |

Three emulator defects, each a wrong answer from a device the firmware asks about: (1) the timer interrupt was acknowledged only by a 1.5-address hack; (2) the chip-name registers answered zeros, so 1.0 to 1.2 assumed 32 KB of IRAM; (3) the battery line (port C bit 3, 0xcf000038) read low, which the GPIO interrupt handler of 1.0.x takes as "battery is low": it writes the token "battislo" into the boot-reason mailbox at 0x40017f08 on every key interrupt. That mailbox is inside the coprocessor's stack (its stack starts at 0x40018000), so a key press during a song overwrote the decoder's saved registers and the coprocessor crashed (1.0.2/1.0.4); on 1.0 the same path ("battery low" at the first key press) made the menus ignore input. With the line high (battery fine) none of it happens. The "ID3v2.4" and "stereo" theories below were timing coincidences: they changed when the key press landed relative to the coprocessor's stack depth. No patch of the firmware is needed. The Python core has the same default now, so `tests/test_rust.py` still matches exactly; 1.5's trace changed slightly (clock, sound) because the line is now high.

The sections below are the investigation in the order it happened; where they disagree with this table, this table is right.

## Result after the first fix (the timer acknowledge)

| firmware | boots | menus respond | MP3 plays |
|---|---|---|---|
| flashpod 1.6 | yes | yes | **yes** (and FLAC) |
| Apple 1.5 | yes | yes | **yes** |
| Apple 1.4, 1.3, 1.2.6, 1.2.2, 1.2.1 | yes | yes | **yes, since the fix** (they did not play before it) |
| Apple 1.2, 1.1, 1.0.4, 1.0.2 | yes | yes | no: the song opens on Now Playing, its clock never moves; the coprocessor runs away (below) |
| Apple 1.0 (1.0.0) | yes (Language screen) | **no**, ignores all input | no |

The Python (Unicorn) core cannot start 1.0 to 1.1 at all (its reset step asserts that the coprocessor sleeps at a fixed 1.5 address) and was not given the fix; the web app and the offline build use the Rust core.

## What was wrong for 1.2.1 to 1.4: the timer interrupt was acknowledged by a 1.5-only hack

The cores acknowledged the timer interrupt (clearing pending bit 11) when execution reached address 0x118ec, the tail of 1.5's timer routine, which holds `ldr r0,[0xcf001104]`.
That register read is the real acknowledge: reading TIMER1_VAL (0xcf001104) clears the timer interrupt. Other firmware has the routine elsewhere, so the hack never fired: bit 11 stayed pending,
the interrupt handler re-entered over and over (0xcf001000/0xcf001104 read 105,000 times in 3 s instead of 1,100) and the lower-priority interrupt 18, the coprocessor's signal to the CPU, was never served.
The CPU waited for the coprocessor, the coprocessor polled a flag in RAM for work the CPU never posted (1.3: `ldrh r0,[r1,#0x36]; cmp r0,#0; beq` at 0x2811d9cc), and the audio output never started (0 audio interrupts).
Fix: the Rust core now clears the pending bit when TIMER1_VAL is read (`Config::ack_on_read`, on by default; the old hack stays, so 1.5 is unchanged: `tests/test_rust.py` and `tests/test_offline.py` still match exactly).
Result: 1.4, 1.3, 1.2.6, 1.2.2 and 1.2.1 play (31,000 audio interrupts in the test; 1.3 through the web app: Now Playing counts, sound streams).

## What is still wrong for 1.0.2 to 1.2 (1.0.4 examined): the coprocessor crashes in its startup

Findings, in the order they were made (the openpod wiki, pages 13.1 and 18.4a, documents the overlay scheme; 1.0.x ("0.0 to 0.4" there) has the MP3 decoder as two IRAM overlays, 12 KB at 0x400068c4 and 24 KB at 0x4000fd60):

1. The coprocessor is first woken at 0.13 s (tick 7). It restarts from its entry (`b 0x280b8aac`), takes stacks from a table in IRAM (0x4000602c + 8 x mode: pointer, size), switches to system mode with the stack the table gives for entry 8, and enters
   its task loop (0x19104), which dispatches on a codec tag. The tag starts as 'mp3' (0x33706d), so it runs the MP3 decoder's load stub (0x280bb9b4): copy 12.8 KB from the DRAM image (0x0414552c) to IRAM 0x400068c4, clear 25 KB at 0x40009b08, copy 24.8 KB to 0x4000fd60, clear 228 bytes at 0x40015e3c, `pop {r0-r3,pc}`.
2. Entry 8 of the table is 0x40007ffc. The CPU writes it early (its instruction ~1,000: a loop at 0x1b7c that fills the table from an array of bases; then `stm` at 0x1c04 sets entry 8 to the top of IRAM's second 8 KB). That stack is **inside the area the coprocessor's load stub then overwrites** (0x400068c4..0x4000cb08 and on), so the saved return address is replaced with decoder code (0xe0811008) and the final `pop` jumps to garbage. The coprocessor then runs through zero-filled memory for ever (a program counter climbing by 4).
   A real iPod cannot be doing this, so what differs is either what the stack table holds when the coprocessor reads it, or when the load stub runs relative to the stack's use.
3. Ruled out: a smaller or mirrored IRAM (`Config::iram_kb`; 64, 32 and 16 KB make the CPU itself run away because the firmware uses ~88 KB; 128 KB changes nothing). Holding table entry 8 at 0x40003ffc (the coprocessor's entry literal) lets its startup finish in 193,000 instructions and sleep normally, but the same entry is the CPU's task-stack table
   (the CPU stalls with it changed), so that cannot be the answer as it stands; after it the song start never wakes the coprocessor again.
4. Confirmed from the wiki: the timer interrupt is acknowledged by reading TIMER1_VAL (the fix above) and the IRQ dispatcher needs a source set in both the raw status (0xcf001000) and the per-core enable mask (0xcf001020); the emulator's delivery does not consult the enable mask.
   Tried (2026-10-19, `irq_mask`, env `EMUPOD_RS_IRQ_MASK=1`, off by default): the CPU enable word (0xcf001024 set / 0xcf001028 clear) gates delivery, and 0xcf001020 reads pending & enable. It breaks every firmware (no response to keys), 1.5 included. An earlier note here blamed different bit numbers; that was wrong (it read the last value written to a set register as the enable word). Replaying every write with `tools/fwstudy/evlog.py` shows 1.5 does enable bits 11 (timer) and 18 (signal) (word 0x08442818 by the menus). What fails is the key interrupt: the emulator raises bit 14, but 1.5 clears bit 14 in the enable word while it waits for keys (0xcf001028 <- 0x4000), so under the mask the key interrupt is never reported in 0xcf001020, while with the mask off the firmware's dispatcher sees it and runs. So bit 14 is probably not gated by that word on the chip (the GPIO ports have their own enables near 0xcf000040), or the key source has another number. Not resolved.

Next: check what entry 8 should hold by reading the CPU side of the table (who uses it and when); compare with a release that works (1.3) to see how its overlays and stacks avoid each other.

## 1.0.0: dead input

After boot the coprocessor stays awake, spinning (68 M instructions in a second against 0.2 M on 1.5), the CPU idles and the timer ticks at half the usual rate; no port-A pin changes the screen. It may be the same coprocessor problem earlier in the boot, or not: not examined further.

## Other notes
- 1.1 to 1.4 boot to a blank screen with the default timing (the coprocessor at 13 instructions a microsecond); the web app's timing (26) draws the Language screen.
- Songs in FLAC need flashpod 1.6; the cards page says so on cards with Apple firmware.
- Study aids added to the Rust core: `trace_io` (device accesses by core and address, and the last value written), `cop_trace_arm`/`cop_log` (the coprocessor's first instructions after its wake), `stats()` (interrupt, signal, coprocessor counters).

## Found (2026-10-19): the chip's name decides the IRAM size

The CPU's startup (1.0 to 1.2: a function at ~0x170c; 1.3 and later: a variant) reads eight characters from 0xcf004030..3c (two per register, first in the top byte of 0x3c) and compares them with a list (TANGO-51, PP5001E1, PP5001E3, PP5002E0, 1.2 also PP5002C). A chip number of 4 or more (PP5002) gives the player 96 KB of IRAM (stack top 0x40018000 - 0x100); anything else gives 32 KB (0x40008000 - 0x100). The emulator answered 0, so the coprocessor's stack was placed at 0x40007xxx, inside the MP3 overlay's load area, which is the crash narrowed earlier. Fix: those registers answer "PP5002E0" (`CHIP_ID`). Result: 1.1 and 1.2 now play MP3 (the survey's old IRAM-size experiment tried only the memory size, not this stack choice). 1.0.2, 1.0.4 and 1.0 are still open (see below).

### Where 1.0.x stands (2026-10-19, later)

- 1.1, 1.2: play (the chip-id fix). The survey's "old" navigation (Songs, second song) was wrong for them; Playlists/Artists/Songs/Settings menus need `select w18 select w14 select`.
- 1.0.4: plays an MP3 with no ID3v2.4 tag (a plain CBR file); the same firmware with an ID3v2.4 tagged file (what ffmpeg writes) crashes the coprocessor: the decoder's frame pops garbage, and a store through a null pointer overwrites the exception vectors at 0x10, 0x18 and 0x1c, after which the coprocessor runs the panic handler (0x1808, which waits). Consistent with a firmware that predates ID3v2.4; not checked on hardware.
- 1.0.2: mono 64 kbps plays; stereo 128 or 192 kbps fails the same way with or without tags. Not explained. Ruled out: timing profile, spin-loop skipping, the decoded-instruction cache for IRAM, the interrupt enable mask. Study aids added: `Machine.watch_all/watch_log` (every write to a word, with the writer's pc and sp), `cop_find_write`, `set_bp`, `ring` (the coprocessor's last instructions with registers).
- 1.0: boots to the Language screen and ignores input (not studied further).
- The timer-acknowledge hook on 1.5's address 0x118ec now applies only with `ack_on_read` off (it ran on every firmware that has code there).

### The enable mask, once more (2026-10-19)

GPIO log on 1.5 (`evlog.py ... cf000000-cf0000ff`): the firmware writes the port A interrupt enable (0xcf000050) with 0x1f, 0x9f, 0xcf, 0xdd, 0xdf and acknowledges at 0xcf000070, so the keys and wheel are enabled at the port level. At the controller, bit 14 (which the firmware's dispatcher tests as the GPIO source) is cleared in the CPU enable word by the time it waits for keys, yet keys work with the mask off and not with it on (also with the status registers left raw, `EMUPOD_RS_IRQ_MASK_RAW=1`, and with the key interrupt on bit 13). So either 0xcf001024/28 are not the whole gate for that source or the controller does something the model lacks (a second enable bank, or the key interrupt is level-sensitive and kept asserted). Without the chip's documentation the mask cannot be made faithful from this evidence, and nothing found for 1.0.x depends on it. Dropped as a lead; the switches stay as study aids (off).

### 1.0.2 mono vs stereo, event logs (2026-10-19)

Logged ATA, mailbox, processor-control and audio-FIFO accesses at song start for a mono 64 kbps card (plays) and a stereo 128 kbps card (fails) on 1.0.2: the control events are the same until the CPU's wake of the coprocessor, which lands one 1 ms slice later in the stereo run; nothing else differs. Not a race: sweeping interleave (1, 3, 7, 50), cop_ipus (20, 40) and quantum (5) leaves the stereo card failing every time with the same counts. So the failure follows the stream's data (the overlay function that crashes loops over the channels: `cmp [r4+0x330]; moveq 1; movne 2`), not the interleaving. A caution about my own earlier readings: the stack-slot dumps I took at the end of a run were taken after the crash (the CPU later writes the string "battislo" into that slot, at pc 0xb5d8), so they do not show what the crashing function saw. Next: log every write to the crashing frame's slots up to the crash itself (stop at the first store through the null pointer), not after.

### Root cause of the 1.0.2 / 1.0.4 failure (2026-10-19): the CPU's status label sits in the coprocessor's stack

Single-stepping the coprocessor and logging every write to the crashing frame's slots showed the frame intact until the CPU wrote into it. The CPU function at 0x350a0 (1.0.2/1.0.4) copies an 8-character status label ("booting!", "hotstuff", "battislo", "--------" at 0x35104) to a fixed address, 0x40017f08 (literal at 0x350f8). The coprocessor's own startup sets its supervisor stack to the top of IRAM, 0x40018000, and while it is inside the decoder's channel loop its frame reaches 0x40017ee8..0x40017f27, over that address. When the CPU writes the label at song start (here 380 us after the coprocessor entered the function), the coprocessor's saved registers become text, it returns into garbage, and a store through a null pointer overwrites the exception vectors; the panic handler then waits forever. Proof: pointing the literal at harmless memory (`tools/fwstudy/explore_patched.py`, a write of 0x300f00 to 0x350f8 after boot) makes every failing card play (1.0.4 with the ID3v2.4 file, 1.0.2 with stereo files): 68,000 audio interrupts, as for a good run. So the ID3-tag theory above was wrong: the tag only moved the timing of the two writes relative to each other, as do the file's channels.

Why the firmware survives on a chip: the layout is the same there, so either the CPU's label write lands when the coprocessor is not that deep, or the real coprocessor's stack top is lower than the 0x40018000 it computes here. Neither is known. Our model gives the coprocessor twice the CPU's instruction rate (LIVE_TIMING cop_ipus 26 vs 13), which changes the order of these two events; on the chip the cores run at the same clock. Open question for a faithful model, not a decision: ship a compatibility patch for 1.0.x (move the label), or find the timing that avoids the overlap.

## 1.0.x on cards over about 1 GB: an emulator fault, found and fixed (2026-10-19, David's david2: "plays less than a second, then halts")

david2 is a 10 GB card written with Apple 1.0. On the test server the live counters showed audio interrupts running but no disk reads for the song, and Now Playing frozen at 0:01. With synthetic songs the same 1.0 read the song from a 120 MB and a 1 GB card, but not from a 2 to 10 GB card. The difference is the FAT32 cluster size flashpod chooses (8 KB up to about 1 GB of data area, 16 KB above; `max_data_gb=1` on a 10 GB card gives 8 KB and works, 2 gives 16 KB and fails; the cluster count is the same, so it is the size).

I first took this for a limit of the firmware and put a "cards of 1 GB or smaller" note on the Cards page. That was wrong: flashpod's manifest records that real 1.0 boots a 128 GB card and plays audio from past 14 GB. The ATA log (tools/fwstudy/atalog-style: `Machine.ev_start` over 0xc00031e0-ff) shows what happened: 1.0 sends SET MULTIPLE MODE with a block size equal to the cluster size in sectors (16 for 8 KB clusters, 32 for 16 KB) and then reads the song with READ MULTIPLE of a whole cluster. IDENTIFY offers at most 16, and the ATA model refused any block size above 16, so the first READ MULTIPLE failed ("SET MULTIPLE not done") and the firmware gave up. Real drives and card adapters accept the larger size. Fix: SET MULTIPLE accepts 32, 64 and 128 as well (both the Rust and the Python core; IDENTIFY still says 16, so no other firmware's behaviour changed, and both equivalence tests still match). Result: 1.0, 1.0.2 and 1.0.4 read and play from 2, 3, 4, 6, 8 and 10 GB cards. The Cards page note is removed.
