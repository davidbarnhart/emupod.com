// The card store of the offline app, in a module Worker: cardmgr.js (emupod's card store in JavaScript) over flashpod's Rust library
// (flashpod_wasm.wasm) and the card images in the browser's private file system. The Cards page (the hosted page, unchanged) talks to
// it through cardsrpc.js / cardshost.js. Messages in: {id, method, path, query, body, data, booted}; out: {t:'result', id, status, json},
// {t:'progress', card, op}, {t:'ready'} or {t:'fatal', message}.
import { CardManager } from './cardmgr.js';
const here = new URL('.', self.location.href).href;
const mgr = new CardManager(m => postMessage(m));
const ready = (async () => {
  const wasm = await (await fetch(here + 'flashpod_wasm.wasm')).arrayBuffer();
  await mgr.init(wasm, here);
})().then(() => postMessage({ t: 'ready' })).catch(e => { postMessage({ t: 'fatal', message: String(e && e.message || e) }); throw e; });

onmessage = async e => {
  const m = e.data;
  try {
    await ready;
    const [status, json] = await mgr.api(m.method, m.path, m.query || {}, m.body, m.data, m.booted);
    postMessage({ t: 'result', id: m.id, status, json });
  } catch (err) { postMessage({ t: 'result', id: m.id, status: 400, json: { error: String(err && err.message || err) } }); }
};
