// The card store's host: starts cardsvc.js (the card store, in a Worker) once and answers the Cards page's API calls as
// window.cardsApi(method, path, query, body, data) -> {status, json}. It lives in the iPod page, so the Cards frame (cardsrpc.js)
// reaches it through `parent` and opening Cards again does not start Python again; a Cards page opened alone has its own.
// An operation answers "running" at once, its progress comes from the worker, and a list asked meanwhile is answered from the last one.
// Booting a card is only a note (which card the iPod page opens next); an operation on the card the iPod runs asks window.cardsHooks
// (shim.js) to let go of it first and to take it again.
(() => {
  const here = new URL('.', document.currentScript.src).href;
  let W = null, seq = 0, fatal = null;
  const waits = new Map();
  let running = null;                     // the operation the worker is busy with: {card, op, then}
  let lastList = null;                    // the last card list the worker gave: what is answered while it is busy
  const failed = {};                      // card -> an operation the worker refused (the server answers that with a 400 at once; here the page was told 'running')
  const getCard = () => { try { return localStorage.getItem('emupod.card'); } catch (e) { return null; } };

  function worker() {
    if (W) return W;
    W = new Worker(here + 'cardsvc.js', { type: 'module' });
    W.onmessage = e => {
      const m = e.data;
      if (m.t === 'fatal') { fatal = m.message; for (const [, w] of waits) w.resolve({ status: 503, json: { error: 'the card manager could not start: ' + m.message } }); waits.clear(); }
      else if (m.t === 'progress') { if (running && running.card === m.card) running.op = Object.assign({}, running.op, m.op, { state: 'running' }); }
      else if (m.t === 'result') { const w = waits.get(m.id); if (w) { waits.delete(m.id); w.resolve({ status: m.status, json: m.json }); } }
    };
    return W;
  }
  const call = (method, path, query, body, data, booted) => new Promise(resolve => {
    if (fatal) return resolve({ status: 503, json: { error: 'the card manager could not start: ' + fatal } });
    const id = ++seq; waits.set(id, { resolve });
    worker().postMessage({ id, method, path, query, body, data, booted }, data ? [data] : []);
  });

  const OPS = new Set(['create', 'partition', 'flash', 'init', 'rebuild', 'format', 'add', 'remove', 'duplicate', 'delete', 'rename', 'scan', 'plan']);
  const runningOp = (card, verb) => ({ op: verb, state: 'running', started: Date.now() / 1000, stage: '', done: 0, total: 0, item: '', cancel: false, result: null, error: null });
  const hooks = () => window.cardsHooks || { release: async () => {}, reopen: async () => {} };

  function synth(path) {                  // while the worker is busy: the last list, with the operation as it stands
    if (!lastList) return null;
    const list = JSON.parse(JSON.stringify(lastList));
    for (const c of list.cards) if (running && c.name === running.card) c.op = running.op;
    for (const n of (running && running.adds) || []) {        // a card being made is in the list at once (the server makes the file first)
      if (!list.cards.some(c => c.name === n)) list.cards.push({ name: n, size: running.size || 0, on_disk: 0, mtime: 0, scan: null, scan_fresh: false, scanned: null,
        op: running.op, booted: false, uploads: [], readonly: false });
    }
    if (path === '/api/cards') return list;
    const m = path.match(/^\/api\/cards\/([^/]+)$/);
    return m ? list.cards.find(c => c.name === decodeURIComponent(m[1])) || null : null;
  }

  async function cardsApi(method, p, query, body, data) {
    if (p === '/api/boot' && method === 'POST') {               // the iPod page opens this card next (it reloads: home(true))
      try {
        if (body && body.card) { localStorage.setItem('emupod.card', body.card); localStorage.setItem('emupod.keep', body.keep ? '1' : '0'); }
        else { localStorage.removeItem('emupod.card'); localStorage.removeItem('emupod.keep'); }
      } catch (e) {}
      return { status: 200, json: { ok: true } };
    }
    const m = p.match(/^\/api\/cards\/([^/]+)\/([^/]+)$/), card = m && decodeURIComponent(m[1]), verb = m && m[2];
    if (method === 'GET' && running && (p === '/api/cards' || /^\/api\/cards\/[^/]+$/.test(p))) {
      const s = synth(p); if (s) return { status: 200, json: s };
    }
    if (m && method === 'POST' && (OPS.has(verb) || verb === 'then')) {
      let op = verb, args = body || {}, viaThen = false;
      if (verb === 'then') { op = args.op; args = args.args || {}; viaThen = true; }
      if (!OPS.has(op)) return { status: 400, json: { error: 'no such operation: ' + op } };
      if (running) return { status: 400, json: { error: running.card + ' is busy: ' + running.op.op } };
      if (viaThen) await hooks().release();
      running = { card, op: runningOp(card, op), then: viaThen, adds: op === 'create' ? [card] : op === 'duplicate' && args.to ? [args.to] : [], size: +args.size || 0 };
      const started = running;
      delete failed[card];
      call('POST', '/api/cards/' + encodeURIComponent(card) + '/' + op, query, args, null, viaThen ? null : getCard()).then(async r => {
        running = null;
        if (r.status !== 200) failed[card] = Object.assign(runningOp(card, op), { state: 'error', error: (r.json && r.json.error) || ('HTTP ' + r.status), ended: Date.now() / 1000 });
        if (viaThen) await hooks().reopen();
      });
      return { status: 200, json: { ok: true, op: started.op } };
    }
    const r = await call(method, p, query, body, data, getCard());
    if (method === 'GET' && r.status === 200 && r.json) {
      const mark = c => { if (c && failed[c.name] && !(c.op && c.op.started > failed[c.name].started)) c.op = failed[c.name]; };
      if (p === '/api/cards') (r.json.cards || []).forEach(mark); else if (/^\/api\/cards\/[^/]+$/.test(p)) mark(r.json);
    }
    if (method === 'GET' && p === '/api/cards' && r.status === 200) lastList = r.json;
    return r;
  }
  cardsApi.warm = () => { worker(); };    // start loading Python now (the iPod page does it when Cards is tapped)
  window.cardsApi = cardsApi;
})();
