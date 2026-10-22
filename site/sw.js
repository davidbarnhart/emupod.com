// The offline app's service worker: keeps the app's files so it opens with no network, answers /silence.wav (the
// quiet looping track iOS wants playing for the lock screen's buttons), and fetches the lock-screen cover from the page.
const CACHE = 'emupod-offline-1022-1420-d3fe0b';          // build_offline.py puts the build's id here: a new build is a new service worker, which replaces the files
const FILES = ['./', 'index.html', 'cards.html', 'shim.js', 'worker.js', 'store.js', 'ipod_core.js', 'emupod_wasm.wasm',
  'clock_glyphs.json', 'manifest.webmanifest', 'icon.png', 'icon-180.png', 'icon-192.png', 'icon-512.png', 'cardsvc.js', 'cardmgr.js', 'flashpod.js', 'flashpod_wasm.wasm', 'cardshost.js', 'cardsrpc.js'];

self.addEventListener('install', e => { e.waitUntil(caches.open(CACHE).then(c => c.addAll(FILES.map(f => new Request(f, { cache: 'reload' }))))      // 'reload': past the browser's own HTTP cache, which may hold the last build's files for minutes
  .then(() => self.skipWaiting())); });
self.addEventListener('activate', e => { e.waitUntil(caches.keys().then(ks => Promise.all(ks.filter(k => k !== CACHE).map(k => caches.delete(k)))).then(() => self.clients.claim())); });

const RATE = 8000, SECONDS = 3600, N = RATE * SECONDS;
function header() {
  const b = new DataView(new ArrayBuffer(44)), s = (o, t) => [...t].forEach((c, i) => b.setUint8(o + i, c.charCodeAt(0)));
  s(0, 'RIFF'); b.setUint32(4, 36 + N, true); s(8, 'WAVEfmt '); b.setUint32(16, 16, true); b.setUint16(20, 1, true); b.setUint16(22, 1, true);
  b.setUint32(24, RATE, true); b.setUint32(28, RATE, true); b.setUint16(32, 1, true); b.setUint16(34, 8, true); s(36, 'data'); b.setUint32(40, N, true);
  return new Uint8Array(b.buffer);
}
function silence(req) {
  const total = 44 + N, hdr = header();
  let start = 0, end = total - 1, code = 200;
  const r = req.headers.get('Range'), m = r && /^bytes=(\d*)-(\d*)$/.exec(r);
  if (m) { start = +m[1] || 0; end = Math.min(m[2] ? +m[2] : total - 1, total - 1, start + (4 << 20) - 1); code = 206; }
  if (start > end) { start = 0; end = total - 1; code = 200; }
  const body = new Uint8Array(end - start + 1).fill(0x80);
  if (start < 44) body.set(hdr.subarray(start, Math.min(44, end + 1)), 0);
  const h = { 'Content-Type': 'audio/wav', 'Accept-Ranges': 'bytes', 'Content-Length': String(body.length) };
  if (code === 206) h['Content-Range'] = `bytes ${start}-${end}/${total}`;
  return new Response(body, { status: code, headers: h });
}
async function cover(e) {
  const cs = await self.clients.matchAll({ type: 'window' });
  for (const c of cs) {
    const blob = await new Promise(res => { const ch = new MessageChannel(); ch.port1.onmessage = ev => res(ev.data); c.postMessage({ t: 'cover' }, [ch.port2]); setTimeout(() => res(null), 3000); });
    if (blob) return new Response(blob, { headers: { 'Content-Type': 'image/png' } });
  }
  return new Response('', { status: 404 });
}
self.addEventListener('fetch', e => {
  const u = new URL(e.request.url);
  if (u.pathname.endsWith('/silence.wav')) return e.respondWith(silence(e.request));
  if (u.pathname.endsWith('/screen.png')) return e.respondWith(cover(e));
  if (e.request.method !== 'GET' || u.origin !== location.origin) return;
  // the cache first: each build has its own cache (its id is the cache's name) and a new build is a new service worker, so a cached file is
  // never stale; the network answers what is not cached (and fills the cache), so a first visit and a slow link cost one fetch each
  e.respondWith((async () => {
    const cache = await caches.open(CACHE);
    const hit = await cache.match(e.request, { ignoreSearch: true });
    if (hit) return hit;
    try { const r = await fetch(e.request); if (r && r.ok) cache.put(e.request, r.clone()).catch(() => {}); return r; }
    catch (err) { return (await caches.match(e.request, { ignoreSearch: true })) || Response.error(); }
  })());
});
