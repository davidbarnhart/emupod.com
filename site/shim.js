// The web app's page, with no server: the page asks for /state, /cmd, /audio and /screen.png (what emupod/web.py
// answers); here the answers come from the core in a Worker (worker.js). The page's own code is untouched.
(() => {
  window.OFFLINE = true;
  const BUILD = '1022-0110-d3fe0b';                                   // build_offline.py stamps this: which build a phone is running
  const here = new URL('.', document.currentScript.src).href;
  const W = new Worker(here + 'worker.js', { type: 'module' });
  const GREY4 = [[182, 188, 169], [130, 136, 118], [80, 85, 70], [34, 38, 30]];
  const S = { phase: 'booting', step: 0, emu_us: 0, inputs: 0, screens: 0, hold: 0, audio: false, clicks: 0, np: null, keys: [],
    lines: { hold: 0, firewire: 1, cna: 1, battery: 0 }, recent: [], shots: [], dir: 'offline', label: '', ver: 0, notice: '',
    card: null, jack: { plugged: false, error: null, listeners: 0 } };
  let ipod = null;                                    // the core's counters, for the iPod Statistics (answered as /ping)
  let px = null, digest = '', sig = '', vol = { left: 0x5c, right: 0x5c };
  let waiters = [];
  const audioClients = new Set();

  function wake() { const w = waiters; waiters = []; w.forEach(f => f()); }
  function bump() {
    const s = [S.clicks, S.audio, S.keys.join(), S.hold, S.phase, S.notice, S.backlight].join('|');
    if (s !== sig) { sig = s; S.ver++; }
  }

  const card = (() => { try { return localStorage.getItem('emupod.card'); } catch (e) { return null; } })();
  S.card = card ? { name: card, keep: (() => { try { return localStorage.getItem('emupod.keep') === '1'; } catch (e) { return false; } })() } : null;
  const keepFlag = () => { try { return localStorage.getItem('emupod.keep') === '1'; } catch (e) { return false; } };
  // the cards live in the browser's private storage (OPFS), which only a secure page (https:// or localhost) is given: say so, instead of a page that never starts
  const noStore = !(window.isSecureContext && navigator.storage && navigator.storage.getDirectory);
  if (noStore) {
    S.phase = 'running';
    S.notice = !window.isSecureContext
      ? 'This page is not secure (it was opened over http://), so the browser will not give it the storage the iPod needs. Open it through https:// (emupod.com does).'
      : 'This browser does not offer the private storage (OPFS) the iPod needs. Try a current Safari, Chrome or Firefox.';
    bump();
  } else {
    // a build that carries a stock card (tools/build_offline.py: the demo's card) installs it at the first run, and again when the build carries a newer one
    // (stock.json's rev changed), so a visitor who came before sees the current demo and not the first one they were given
    const ls = { get: k => { try { return localStorage.getItem(k); } catch (e) { return null; } }, set: (k, v) => { try { localStorage.setItem(k, v); } catch (e) {} } };
    const openIt = () => W.postMessage({ t: 'open', name: card, keep: keepFlag() });
    fetch(here + 'stock.json', { cache: 'no-store' }).then(r => r.ok ? r.json() : null).catch(() => null).then(st => {       // no-store: the browser's HTTP cache may hold the last build's
      if (card && !(st && st.name === card && st.rev && ls.get('emupod.stockrev') !== st.rev)) return openIt();     // their own card, or the demo card already current
      if (!st) { S.phase = 'running'; S.notice = 'No card yet. Open Cards, make a new card, partition it, write a firmware and add songs.'; bump(); wake(); return; }
      S.phase = 'running'; S.notice = 'Setting up the stock card…'; bump(); wake();
      const store = new Worker(here + 'store.js');
      store.onmessage = e => {
        const m = e.data;
        if (m.t === 'progress') { S.notice = 'Setting up the stock card… ' + Math.round(100 * m.done / m.total) + '%'; bump(); wake(); }
        else if (m.t === 'imported') { ls.set('emupod.card', st.name); if (st.rev) ls.set('emupod.stockrev', st.rev); S.notice = ''; S.card = { name: st.name, keep: false }; bump(); W.postMessage({ t: 'open', name: st.name, keep: false }); }
        else if (m.t === 'error') { S.notice = 'Could not set up the stock card: ' + m.message; bump(); wake(); }
      };
      store.postMessage({ t: 'install', url: here + st.file + (st.rev ? '?v=' + st.rev : ''), name: st.name, size: st.size, id: 1 });      // ?v=: a new card is a new address, whatever the browser's HTTP cache holds
    });
  }

  // what the player's Worker did: its steps and errors, kept on the page, nothing is sent anywhere
  const diag = [];
  const note = t => { diag.push(new Date().toISOString().slice(11, 19) + ' ' + t); if (diag.length > 60) diag.shift(); };
  window.onLoopError = e => { note('page: ' + (e && e.message || e) + (e && e.stack ? '\n' + String(e.stack).split('\n').slice(0, 3).join('\n') : '')); };
  addEventListener('error', e => { note('page error: ' + e.message + ' ' + String(e.filename || '').split('/').pop() + ':' + e.lineno); });
  W.onerror = e => { note('worker error: ' + (e.message || e.type) + (e.filename ? ' ' + e.filename.split('/').pop() + ':' + e.lineno : '')); S.notice = 'The iPod did not start: ' + (e.message || 'the player failed to load'); bump(); wake(); };
  W.onmessageerror = () => note('worker message error');
  addEventListener('unhandledrejection', e => note('rejection: ' + (e.reason && e.reason.message || e.reason)));
  W.onmessage = e => {
    const m = e.data;
    if (m.t === 'diag') note(m.m);
    if (m.t === 'frame') { px = m.px; digest = m.digest; S.np = m.np; S.screens++; S.shots = (S.shots.concat(digest)).slice(-24); bump(); wake(); }
    else if (m.t === 'state') {
      if (m.stats) ipod = m.stats;
      S.step = m.step; S.emu_us = m.emu_us; S.audio = m.audio; S.clicks = m.clicks; S.keys = m.keys; S.hold = m.hold; S.lines.hold = m.hold; S.backlight = m.bl ? 1 : 0; vol = m.vol;
      const old = sig; bump(); if (sig !== old) wake();
    } else if (m.t === 'audio') {
      const gl = gain(vol.left), gr = gain(vol.right), a = new Int16Array(m.data);
      if (gl !== 1 || gr !== 1) for (let i = 0; i < a.length; i += 2) { a[i] = a[i] * gl; a[i + 1] = a[i + 1] * gr; }
      const u = new Uint8Array(a.buffer);
      for (const c of audioClients) { try { c.enqueue(u.slice()); } catch (e) {} }
    } else if (m.t === 'closed') { const f = closing; closing = null; if (f) f(); }
    else if (m.t === 'booted') { S.phase = 'running'; bump(); wake(); setTimeout(() => { if (window.cardsApi) window.cardsApi.warm(); }, 4000); }   // the card store starts in the background: Cards opens at once
    else if (m.t === 'error') { note('error: ' + m.message + (m.stack ? '\n' + m.stack.split('\n').slice(0, 3).join('\n') : '')); S.phase = 'running'; S.notice = m.busy ? 'The iPod is open in another tab or window, which holds its card. Close that one, then reload this page.' : 'The iPod did not start: ' + m.message; bump(); wake(); }
  };
  // the Cards page (in a frame over this one) changes the card the iPod is running: the card store (cardshost.js) asks this page to let go of
  // the card and, when the change is done, to take it again. Tapping Cards starts the card store's Python at once, so it is ready by the
  // time the page has loaded (and it stays running: opening Cards again is instant).
  let closing = null;
  window.cardsHooks = {
    release: () => new Promise(res => { closing = res; W.postMessage({ t: 'close' }); }),
    reopen: async () => {
      const name = (() => { try { return localStorage.getItem('emupod.card'); } catch (x) { return null; } })();
      if (name) W.postMessage({ t: 'open', name, keep: keepFlag() });
    },
  };
  // leaving or reloading the page lets go of the card at once, so the next page (or another tab) can take it; a page Safari brings back takes it again
  addEventListener('pagehide', () => { try { W.postMessage({ t: 'close' }); } catch (e) {} });
  addEventListener('pageshow', e => { if (e.persisted) window.cardsHooks.reopen(); });
  document.addEventListener('click', e => { if (e.target.closest && e.target.closest('#cardsbtn') && window.cardsApi) window.cardsApi.warm(); }, true);
  const gain = v => v <= 0x2f ? 0 : Math.min(1, Math.pow(10, (v - 123) / 20));

  function pack() {
    const out = new Uint8Array(px.length / 4);
    for (let i = 0, j = 0; i < px.length; i += 4, j++) out[j] = (px[i] << 6) | (px[i + 1] << 4) | (px[i + 2] << 2) | px[i + 3];
    let s = ''; for (let i = 0; i < out.length; i += 4096) s += String.fromCharCode.apply(null, out.subarray(i, i + 4096));
    return btoa(s);
  }

  // the lock screen's cover: the LCD on a square of the iPod's face colour (web.cover_png)
  let cover = { key: '', blob: null };
  async function coverPng() {
    if (cover.key === digest && cover.blob) return cover.blob;
    const size = 832, scale = 5, pad = 12, w = 160 * scale, h = 128 * scale, x0 = (size - w) >> 1, y0 = (size - h) >> 1;
    const c = new OffscreenCanvas(size, size), g = c.getContext('2d');
    g.fillStyle = '#ececee'; g.fillRect(0, 0, size, size);
    g.fillStyle = '#3b3e38'; g.fillRect(x0 - pad, y0 - pad, w + 2 * pad, h + 2 * pad);
    if (px) {
      const img = g.createImageData(w, h);
      for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) {
        const col = GREY4[px[((y / scale) | 0) * 160 + ((x / scale) | 0)]], o = (y * w + x) * 4;
        img.data[o] = col[0]; img.data[o + 1] = col[1]; img.data[o + 2] = col[2]; img.data[o + 3] = 255;
      }
      g.putImageData(img, x0, y0);
    }
    cover = { key: digest, blob: await c.convertToBlob({ type: 'image/png' }) };
    return cover.blob;
  }
  // the service worker asks for the cover when the system fetches the lock screen's artwork
  if ('serviceWorker' in navigator) {
    // a new build's service worker takes over at once; the page already open is still the old build's, so it reloads once to become the new one
    const hadController = !!navigator.serviceWorker.controller;
    navigator.serviceWorker.addEventListener('controllerchange', () => {
      let again = false; try { again = sessionStorage.getItem('emupod.updated') === '1'; sessionStorage.setItem('emupod.updated', '1'); } catch (e) {}
      if (hadController && !again) Promise.race([window.cardsHooks.release(), new Promise(r => setTimeout(r, 1500))]).then(() => location.reload());   // the card let go first
    });
    navigator.serviceWorker.register(here + 'sw.js').catch(() => {});
    navigator.serviceWorker.addEventListener('message', async e => {
      if (e.data && e.data.t === 'cover' && e.ports[0]) e.ports[0].postMessage(await coverPng());
    });
  }

  const json = (o, code = 200) => new Response(JSON.stringify(o), { status: code, headers: { 'Content-Type': 'application/json' } });
  const realFetch = window.fetch.bind(window);
  window.fetch = (input, init = {}) => {
    const u = new URL(typeof input === 'string' ? input : input.url, location.href), p = u.pathname;
    if (p.endsWith('/state')) return (async () => {
      const since = u.searchParams.get('since') || '', ver = u.searchParams.get('ver') || '', wait = +(u.searchParams.get('wait') || 0);
      const changed = () => digest !== since || String(S.ver) !== ver;
      if (!changed() && wait) await new Promise(r => { const t = setTimeout(r, wait * 1000); waiters.push(() => { clearTimeout(t); r(); }); });
      const out = Object.assign({}, S, { screen: digest || null });
      if (digest && digest !== since && px) out.px = pack();
      return json(out);
    })();
    if (p.endsWith('/ping')) return Promise.resolve(json({ t: Date.now() / 1000, audio: null, ipod: ipod ? Object.assign({ wall: Date.now() / 1000, clicks: S.clicks }, ipod) : null }));
    if (p.endsWith('/cmd') && init.method === 'POST') {
      let c = {}; try { c = JSON.parse(init.body); } catch (e) {}
      if (c.do === 'restart') { setTimeout(() => location.reload(), 50); }
      else if (c.do === 'key' || c.do === 'edges' || c.do === 'wheel' || c.do === 'line') W.postMessage({ t: 'cmd', c });
      return Promise.resolve(json({ ok: true }));
    }
    if (p.endsWith('/audio')) {
      let ctl;
      const body = new ReadableStream({ start(c) { ctl = c; audioClients.add(c); }, cancel() { audioClients.delete(ctl); } });
      if (init.signal) init.signal.addEventListener('abort', () => { audioClients.delete(ctl); try { ctl.close(); } catch (e) {} });
      return Promise.resolve(new Response(body, { headers: { 'Content-Type': 'application/octet-stream' } }));
    }
    if (p.endsWith('/screen.png')) return coverPng().then(b => new Response(b, { headers: { 'Content-Type': 'image/png' } }));
    return realFetch(input, init);
  };
})();
