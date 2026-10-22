// The wasm core under Node, on a card image, through rust/pp5002/tests/play.script (the menus, a song): prints the
// same line as tests/test_offline.py's reference run on the native core.
//   node tests/offline/run.mjs CARD.img [steps]
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { IpodCore, MemDisk } from '../../emupod/offline/ipod_core.js';
const root = new URL('../../', import.meta.url).pathname;
const wasm = readFileSync(root + 'rust/target/wasm32-unknown-unknown/release/emupod_wasm.wasm');
const disk = new MemDisk(readFileSync(process.argv[2]).buffer);
const steps = +(process.argv[3] || 2400);
const core = await IpodCore.create(wasm, disk, { startVolume: 102 });
core.boot();
const ev = [];
for (const l of readFileSync(root + 'rust/pp5002/tests/play.script', 'utf8').split('\n')) {
  const w = l.trim().split(/\s+/);
  if (w.length >= 3 && /^\d+$/.test(w[0])) ev.push([+w[0], w[1], w.slice(2)]);
}
const KEY = { select: 0, menu: 1, play: 2, ff: 3, rew: 4 };
let crc = 0xffffffff, nb = 0, i = 0, step = 0; const vols = new Set();
const T = new Int32Array(256).map((_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c; });
const t0 = performance.now();
while (step < steps) {
  while (i < ev.length && ev[i][0] <= step) {
    const [, k, a] = ev[i++];
    if (k === 'key') core.key(KEY[a[0]], a[1] === 'down'); else { core.wheelEdges(+a[0]); step += Math.abs(+a[0]); }
  }
  core.tick(); step++;
  const b = core.takeAudio(); for (const x of b) crc = T[(crc ^ x) & 0xff] ^ (crc >>> 8); nb += b.length;
  const v = core.volume; vols.add(v.left + ',' + v.right);
}
const digest = createHash('sha1').update(core.screen).digest('hex').slice(0, 12);
console.log('step', step, 'now', core.nowUs, 'audio', nb, 'crc', ((crc ^ 0xffffffff) >>> 0).toString(16).padStart(8, '0'), 'clicks', core.clicks, 'vols', [...vols].sort().join(' '), digest, `${((performance.now() - t0) / 1000).toFixed(1)}s`);
