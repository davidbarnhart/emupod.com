// Node side of tests/test_flashpod_wasm.py: run flashpod's card operations through flashpod_wasm.wasm on an in-memory card and write the
// image out. Usage: node flashpod_run.mjs WASM FIRMWARE OUT SEED NOW SIZE SONG...
import { readFileSync, writeFileSync } from 'node:fs';
import { Flashpod, MemDevice } from '../../emupod/offline/flashpod.js';
const [wasm, fw, out, seed, now, size, ...songs] = process.argv.slice(2);
const fp = await Flashpod.create(readFileSync(wasm));
const dev = new MemDevice();
const files = new Map(songs.map(p => [p, new Uint8Array(readFileSync(p))]));
fp.use(dev, { size: n => files.has(n) ? files.get(n).length : -1, read: (n, off, o) => { const f = files.get(n); if (!f) return -1; const k = Math.max(0, Math.min(o.length, f.length - off)); o.set(f.subarray(off, off + k)); return k; } });
const e = { seed: +seed, now: +now };
fp.run({ op: 'create', size: +size });
fp.run({ op: 'flash' }, new Uint8Array(readFileSync(fw)));
fp.run({ op: 'init', name: 'iPod', ...e });
const r = fp.run({ op: 'add', files: songs, full: 'stop', ceiling: 'refuse', ...e });
const t = fp.run({ op: 'tracks' });
const info = fp.run({ op: 'inspect' });
writeFileSync(out, dev.b);
console.log(JSON.stringify({ added: r.added.length, tracks: t.length ?? (t.tracks || []).length, layout_ok: info.layout_ok, firmware: (info.firmware || {}).build }));
