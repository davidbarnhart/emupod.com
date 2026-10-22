// flashpod's card library (flashpod_wasm.wasm, from rust/flashpod-wasm: flashpod-core, the Rust port) for a Worker or Node.
//
//   const fp = await Flashpod.create(wasmBytes);
//   fp.use(device, sources);          // device: {len(), read(off, out) -> n, write(off, data), flush(), setLen(n)}
//                                     // sources: {size(name) -> n | -1, read(name, off, out) -> n}  (the songs an add reads)
//   fp.run({op: 'inspect'});          // -> the value, or throws a FlashpodError (code, message, details)
//   fp.run({op: 'flash'}, firmwareBytes);
//   fp.onProgress = (stage, done, total, item) => ...;   // while add / plan_add / rebuild run: stages tags, copy, write (bytes), rebuild
//   fp.isCancelled = () => bool;                           // asked between files; a cancelled add stops and reports what it did
//
// Ops: min_card_bytes, catalog, check_firmware (blob), inspect, tracks, create {size}, flash {lba48, max_data_gb, format, arm} (blob),
// flash_firmware (blob), format_data, init {name, overwrite, seed, now}, remove {what: [..], seed, now},
// plan_add {files, assess, seed, now}, add {files, full, ceiling, seed, now}, rebuild {name, save, seed, now}.
// "seed" and "now" are what flashpod's Entropy takes (ids, and the clock for dates): the page passes random ones.

export class FlashpodError extends Error {
  constructor(e) { super(e.message || e.code); this.code = e.code; this.details = e.details || {}; }
}

export class MemDevice {
  constructor(bytes) { this.b = bytes || new Uint8Array(0); }
  len() { return this.b.length; }
  read(off, out) { const n = Math.max(0, Math.min(out.length, this.b.length - off)); if (n > 0) out.set(this.b.subarray(off, off + n)); return n; }
  write(off, data) { if (off + data.length > this.b.length) this.setLen(off + data.length); this.b.set(data, off); }
  flush() {}
  setLen(n) { const x = new Uint8Array(n); x.set(this.b.subarray(0, Math.min(n, this.b.length))); this.b = x; }
}

export class Flashpod {
  static async create(wasm) {
    const self = new Flashpod();
    const mem = () => new Uint8Array(self.x.memory.buffer);
    const name = (p, n) => new TextDecoder().decode(mem().subarray(p, p + n));
    const imports = { host: {
      dev_len: () => self.dev.len(),
      dev_read: (off, ptr, len) => { try { return self.dev.read(off, mem().subarray(ptr, ptr + len)); } catch (e) { return -1; } },
      dev_write: (off, ptr, len) => { try { self.dev.write(off, mem().slice(ptr, ptr + len)); return 0; } catch (e) { return -1; } },
      dev_flush: () => { try { self.dev.flush(); return 0; } catch (e) { return -1; } },
      dev_set_len: n => { try { self.dev.setLen(n); return 0; } catch (e) { return -1; } },
      src_size: (p, n) => { try { return self.src.size(name(p, n)); } catch (e) { return -1; } },
      src_read: (p, n, off, ptr, len) => { try { return self.src.read(name(p, n), off, mem().subarray(ptr, ptr + len)); } catch (e) { return -1; } },
      progress: (sp, sn, done, total, ip, inn) => { if (self.onProgress) { try { self.onProgress(name(sp, sn), done, total, name(ip, inn)); } catch (e) {} } },
      cancelled: () => { try { return self.isCancelled && self.isCancelled() ? 1 : 0; } catch (e) { return 0; } },
    } };
    const { instance } = await WebAssembly.instantiate(wasm, imports);
    self.x = instance.exports;
    return self;
  }

  use(device, sources) { this.dev = device; this.src = sources || { size: () => -1, read: () => -1 }; }

  run(cmd, blob) {
    const x = this.x, enc = new TextEncoder().encode(JSON.stringify(cmd));
    const put = bytes => { const p = x.alloc(bytes.length); new Uint8Array(x.memory.buffer, p, bytes.length).set(bytes); return p; };
    const jp = put(enc), bp = blob && blob.length ? put(blob) : 0;
    try { x.run(jp, enc.length, bp, blob ? blob.length : 0); }
    finally { x.dealloc(jp, enc.length); if (bp) x.dealloc(bp, blob.length); }
    const out = JSON.parse(new TextDecoder().decode(new Uint8Array(x.memory.buffer, x.result_ptr(), x.result_len())));
    if (!out.ok) throw new FlashpodError(out.error);
    return out.value;
  }
}
