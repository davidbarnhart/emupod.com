// The iPod core (emupod_wasm.wasm, from rust/wasm) for a Web Worker or Node: instantiate it on a disk, then drive it.
//
//   const core = await IpodCore.create(wasmBytes, disk, {persist: false, startVolume: 102});
//   core.boot(); core.tick(); core.key(IpodCore.SELECT, true); core.wheelEdges(6);
//   core.screen   // Uint8Array(160*128), levels 0..3 (a view into the module: copy it to keep it)
//   core.takeAudio()  // Uint8Array of s16le stereo, 44,100 Hz (a view: copy it)
//
// A disk is {len(): number, read(off, bytes), write(off, bytes)}: MemDisk (an ArrayBuffer) or OpfsDisk (a
// FileSystemSyncAccessHandle, which only a Worker has).

export class MemDisk {
  constructor(buffer) { this.b = new Uint8Array(buffer); }
  len() { return this.b.length; }
  read(off, out) { const n = Math.min(out.length, Math.max(0, this.b.length - off)); if (n > 0) out.set(this.b.subarray(off, off + n)); }
  write(off, data) { if (off + data.length <= this.b.length) this.b.set(data, off); }
}

export class OpfsDisk {
  constructor(handle) { this.h = handle; }
  len() { return this.h.getSize(); }
  read(off, out) { this.h.read(out, { at: off }); }
  write(off, data) { this.h.write(data, { at: off }); }
}

export class IpodCore {
  static SELECT = 0; static MENU = 1; static PLAY = 2; static FF = 3; static REW = 4;
  static W = 160; static H = 128;

  static async create(wasm, disk, opts = {}) {
    const self = new IpodCore();
    self.disk = disk;
    const mem = () => new Uint8Array(self.x.memory.buffer);
    const imports = { host: {
      disk_len: () => disk.len(),
      disk_read: (off, ptr, len) => { const v = mem().subarray(ptr, ptr + len); v.fill(0); disk.read(off, v); },
      disk_write: (off, ptr, len) => disk.write(off, mem().slice(ptr, ptr + len)),
    } };
    const { instance } = await WebAssembly.instantiate(wasm, imports);
    self.x = instance.exports;
    const r = self.x.init(opts.persist ? 1 : 0, opts.startVolume ?? -1, opts.fpf ?? 0);
    if (r) throw new Error('no bootable firmware on this disk: ' + new TextDecoder().decode(mem().subarray(self.x.error_ptr(), self.x.error_ptr() + self.x.error_len())));
    return self;
  }

  boot() { this.x.boot(); }
  tick() { this.x.tick(); }
  get nowUs() { return this.x.now_us(); }
  key(k, down) { this.x.key(k, down ? 1 : 0); }
  wheelEdges(n) { this.x.wheel_edges(n); }
  hold(on) { this.x.hold(on ? 1 : 0); }
  get screen() { return new Uint8Array(this.x.memory.buffer, this.x.screen_ptr(), IpodCore.W * IpodCore.H); }
  screenChanged() { return !!this.x.screen_changed(); }
  takeAudio() { const n = this.x.audio_take(); return new Uint8Array(this.x.memory.buffer, this.x.audio_ptr(), n); }
  get audioOn() { return !!this.x.audio_on(); }
  get backlight() { return !!this.x.backlight(); }
  get volume() { const v = this.x.volume(); return { left: v & 0xff, right: (v >> 8) & 0xff }; }
  get clicks() { return this.x.click_count(); }
  // the emulator's counters (rust/wasm `stat`), named as the server's /ping names them
  get stats() {
    const o = {};
    IpodCore.STATS.forEach((n, i) => { o[n] = this.x.stat(i); });
    return o;
  }
  static STATS = ['now_us', 'cpu_instr', 'cop_instr', 'cpu_ipus', 'cop_ipus', 'ata_reads', 'ata_writes', 'ata_sectors_read', 'ata_sectors_written',
    'lcd_writes', 'ticks', 'audio_fiqs', 'halts', 'ata_cmds', 'cop_skipped', 'cpu_skipped',
    'audio_late', 'sig_sent', 'sig_taken', 'cop_wakes', 'cop_sleeps', 'irqs', 'fw_underruns'];
}
