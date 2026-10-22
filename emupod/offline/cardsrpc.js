// Gives the hosted Cards page its /api in the offline app: the page's fetch of /api/... is answered by window.cardsApi of the
// iPod page (our parent: the card store starts once there, cardshost.js), or of this page when it is opened alone. Loaded before the
// page's own script, after cardshost.js. While the card store starts (a few seconds the first time) a note says so.
(() => {
  window.OFFLINE_CARDS = true;
  const realFetch = window.fetch.bind(window);
  const host = () => { try { if (window.parent !== window && window.parent.cardsApi) return window.parent.cardsApi; } catch (e) {} return window.cardsApi; };
  const json = (o, code = 200) => new Response(JSON.stringify(o), { status: code, headers: { 'Content-Type': 'application/json' } });

  let first = true, banner = null;
  const showBanner = () => {
    if (banner || !first || !document.body) return;
    banner = document.createElement('div');
    banner.style.cssText = 'position:fixed;left:12px;right:12px;bottom:14px;z-index:70;background:rgba(28,28,30,.92);color:#fff;border-radius:12px;padding:10px 14px;font:14px/1.35 -apple-system,system-ui,sans-serif;text-align:center';
    banner.textContent = 'Starting the card manager…';
    document.body.appendChild(banner);
  };
  addEventListener('DOMContentLoaded', () => setTimeout(showBanner, 1500));
  const done = () => { first = false; if (banner) { banner.remove(); banner = null; } };

  window.fetch = async (input, init = {}) => {
    const u = new URL(typeof input === 'string' ? input : input.url, location.href), p = u.pathname;
    if (!p.startsWith('/api/')) return realFetch(input, init);
    const method = (init.method || 'GET').toUpperCase(), query = Object.fromEntries(u.searchParams.entries());
    let body = null, data = null;
    if (method === 'PUT' && init.body) { data = await new Response(init.body).arrayBuffer(); query.length = data.byteLength; }
    else if (init.body) { try { body = JSON.parse(init.body); } catch (e) {} }
    showBanner();
    const r = await host()(method, p, query, body, data);
    done();
    return json(r.json, r.status);
  };
  try { if (navigator.storage && navigator.storage.persist) navigator.storage.persist(); } catch (e) {}      // keep the cards when the phone is short of space
})();
