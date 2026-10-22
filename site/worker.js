// The iPod, running here: the WebAssembly core on a card image kept in the browser's private file system (OPFS),
// paced by the clock, with the screen, the sound and the state posted to the page (shim.js).
import { IpodCore, OpfsDisk } from './ipod_core.js';

const MIN_HOLD = 6;                 // a key is held at least this many ticks (recording.MIN_HOLD_STEPS)
const KEYS = { select: 0, menu: 1, play: 2, ff: 3, rew: 4 };
let core = null, handle = null, glyphs = {};
let step = 0, down = {}, queue = [], hold = 0, running = false, timer = 0, wall0 = 0, emu0 = 0;
let lastClicks = 0, lastAudio = false, lastVol = '';

const post = (m, t) => postMessage(m, t || []);

const diag = m => post({ t: 'diag', m });

// a sync handle is exclusive: the page that was just reloaded (or the card store's Worker) may not have let go of the card yet, so ask again
async function acquire(fh) {
  let last;
  for (let i = 0; i < 60; i++) {                          // 15 s: Safari can take seconds to end the reloaded page's Worker
    try { return await fh.createSyncAccessHandle(); }
    catch (e) { last = e; if (i === 0) diag('the card is still held by another page or worker, waiting: ' + (e && e.name)); await new Promise(r => setTimeout(r, 250)); }
  }
  const busy = new Error(String(last && last.message || last)); busy.busy = true; throw busy;
}

async function open(name, keep) {
  stop();
  diag('opening card ' + name + (keep ? ' (changes kept)' : ''));
  if (handle) { try { handle.close(); } catch (e) {} handle = null; }
  const root = await navigator.storage.getDirectory();
  const dir = await root.getDirectoryHandle('cards', { create: true });
  const fh = await dir.getFileHandle(name + '.img');
  handle = await acquire(fh);
  diag('card file open, ' + handle.getSize() + ' bytes');
  const wasm = await (await fetch('emupod_wasm.wasm')).arrayBuffer();
  try { glyphs = await (await fetch('clock_glyphs.json')).json(); } catch (e) { glyphs = {}; }
  diag('core loaded (' + wasm.byteLength + ' bytes), starting it');
  core = await IpodCore.create(wasm, new OpfsDisk(handle), { persist: !!keep, startVolume: 114 });   // 90% (the firmware's own is 53, 42%)
  diag('core created');
  step = 0; down = {}; queue = []; hold = 0; lastClicks = 0; lastAudio = false; lastVol = '';
  core.boot();
  diag('booted');
  frame(true);
  wall0 = performance.now(); emu0 = core.nowUs; running = true;
  post({ t: 'booted' });
  pump();
}

function stop() { running = false; clearTimeout(timer); }

// Now Playing's two times, read with the firmware's own glyphs (recording.read_clock)
function readClock(px) {
  const W = 160, ROWS = [...Array(9)].map((_, i) => 111 + i);
  const cols = [...Array(W)].map((_, x) => ROWS.some(y => px[y * W + x] >= 2));
  const gs = []; let x = 0;
  while (x < W) {
    if (cols[x]) {
      const s = x; while (x < W && cols[x]) x++;
      let k = ''; for (const y of ROWS) for (let i = s; i < x; i++) k += px[y * W + i] >= 2 ? '01' : '00';
      gs.push([s, k]);
    }
    x++;
  }
  if (!gs.length || gs.some(([, k]) => !(k in glyphs))) return null;
  const text = side => gs.filter(([s]) => side ? s >= 80 : s < 80).map(([, k]) => glyphs[k]).join('');
  const secs = t => { const m = t.replace(/^-/, '').split(':'); return m.length === 2 ? (+m[0]) * 60 + (+m[1]) : NaN; };
  const a = secs(text(0)), b = secs(text(1));
  return isNaN(a) || isNaN(b) ? null : [a, b];
}

function digest(px) {                                  // FNV-1a, 48 bits, as 12 hex digits
  let h1 = 0x811c9dc5, h2 = 0x01000193;
  for (let i = 0; i < px.length; i++) { h1 = Math.imul(h1 ^ px[i], 16777619); h2 = Math.imul(h2 + px[i] + i, 2246822519) ^ (h2 >>> 13); }
  return ((h1 >>> 0).toString(16).padStart(8, '0') + (h2 >>> 0).toString(16).padStart(8, '0')).slice(0, 12);
}

function frame(force) {
  if (!core.screenChanged() && !force) return;
  const px = core.screen.slice();
  post({ t: 'frame', px, digest: digest(px), np: readClock(px) }, [px.buffer]);
}

function apply(c) {
  if (c.do === 'key' && c.key in KEYS) {
    const down_ = c.phase === 'down';
    if (down_ === (c.key in down)) return;
    core.key(KEYS[c.key], down_);
    if (down_) down[c.key] = step; else delete down[c.key];
  } else if (c.do === 'edges') {
    const n = Math.max(-400, Math.min(400, c.n | 0));
    for (let i = 0; i < Math.abs(n); i++) { core.wheelEdges(Math.sign(n)); step++; }
  } else if (c.do === 'wheel') {
    const n = c.n | 0, seq = n > 0 ? 1 : -1;
    for (let i = 0; i < Math.abs(n) * 4; i++) { core.wheelEdges(seq); step++; }
  } else if (c.do === 'line' && c.name === 'hold') {
    hold = c.level ? 1 : 0; core.hold(hold);
  }
}

function ready(c) {
  if (c.do === 'key' && c.phase === 'up' && c.key in down) return down[c.key] + MIN_HOLD <= step;
  return true;
}

let busy = 0, nextStats = 0, stats = null;
function pump() {
  if (!running) return;
  const t0 = performance.now();
  const target = emu0 + (performance.now() - wall0) * 1000;
  if (target - core.nowUs > 2e6) { wall0 = performance.now(); emu0 = core.nowUs; }        // fell far behind: do not race
  let n = 0;
  while (core.nowUs < target && n < 40) {
    while (queue.length && ready(queue[0])) apply(queue.shift());
    core.tick(); step++; n++;
  }
  frame(false);
  const a = core.takeAudio();
  const v = core.volume, vs = v.left + ',' + v.right;
  if (a.length) { const d = a.slice(); post({ t: 'audio', data: d.buffer, vol: v }, [d.buffer]); }
  const on = core.audioOn, clicks = core.clicks;
  const spent = performance.now() - t0; busy += spent;                  // the time this Worker spends running the iPod: the page's "host CPU"
  if (!nextStats || performance.now() >= nextStats) { nextStats = performance.now() + 200; stats = Object.assign(core.stats, { host_cpu_s: busy / 1000 }); }
  post({ t: 'state', step, emu_us: core.nowUs, audio: on, clicks, keys: Object.keys(down).sort(), hold, vol: v, bl: core.backlight, stats });
  lastAudio = on; lastClicks = clicks; lastVol = vs;
  timer = setTimeout(pump, 12);
}

onmessage = async e => {
  const m = e.data;
  try {
    if (m.t === 'open') await open(m.name, m.keep);
    else if (m.t === 'cmd' && core) queue.push(m.c);
    else if (m.t === 'stop') stop();
    else if (m.t === 'close') {                          // let go of the card (its handle is exclusive): the card manager is about to change it
      stop(); if (handle) { try { handle.close(); } catch (x) {} handle = null; } core = null; queue = [];
      post({ t: 'closed' });
    }
  } catch (err) { post({ t: 'error', message: String(err && err.message || err), busy: !!(err && err.busy), stack: String(err && err.stack || '') }); }
};
