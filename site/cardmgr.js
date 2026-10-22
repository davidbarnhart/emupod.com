// The card store of the offline app, in a Worker's JavaScript: a port of emupod/cards.py over flashpod's Rust library
// (flashpod.js, flashpod_wasm.wasm). Card images are files in the browser's private file system (OPFS "cards/"), opened with a
// synchronous access handle for the length of one call; the small things (each card's saved scan, the firmware the person added and the
// firmware downloaded) are OPFS files in "cards-meta/". `api(method, path, query, body, data, booted)` answers what web.py's /api answers.
import { Flashpod, FlashpodError } from './flashpod.js';

export class CardError extends Error {}
const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9 ._-]{0,63}$/;
const AUDIO_EXT = ['.mp3', '.m4a', '.m4b', '.aac', '.wav', '.aif', '.aiff', '.flac'];
const MAX_CARD = 4e9;                                   // a card made or copied on the phone
const READONLY_OPS = ['scan', 'plan'];
const OPS = ['create', 'partition', 'flash', 'init', 'rebuild', 'format', 'add', 'remove', 'duplicate', 'delete', 'rename', 'scan', 'plan'];
const hex = b => [...new Uint8Array(b)].map(x => x.toString(16).padStart(2, '0')).join('');
const msg = e => (e instanceof FlashpodError ? e.message + (Object.keys(e.details).length ? ' (' + Object.entries(e.details).map(([k, v]) => k + ': ' + (typeof v === 'object' ? JSON.stringify(v) : v)).join(', ') + ')' : '') : (e && e.message) || String(e));

// the card, as flashpod.js wants it
class HandleDevice {
  constructor(h) { this.h = h; }
  len() { return this.h.getSize(); }
  read(off, out) { return this.h.read(out, { at: off }); }
  write(off, data) { this.h.write(data, { at: off }); }
  flush() { this.h.flush(); }
  setLen(n) { this.h.truncate(n); }
}
// a card the iPod's Worker holds: a look (scan, the song list) reads a snapshot of its file
class SnapshotDevice {
  constructor(file) { this.f = file; }
  len() { return this.f.size; }
  read(off, out) { const end = Math.min(this.f.size, off + out.length); if (end <= off) return 0; const g = new Uint8Array(new FileReaderSync().readAsArrayBuffer(this.f.slice(off, end))); out.set(g); return g.length; }
  write() { throw new Error('read only'); }
  flush() {}
  setLen() { throw new Error('read only'); }
}

export class CardManager {
  constructor(post) { this.post = post; this.ops = {}; this.uploads = new Map(); this.side = {}; this.userFw = new Set(); this.fwCache = new Set(); this.booted = null; }

  async init(wasm, base) {
    this.base = base;
    const root = await navigator.storage.getDirectory();
    this.cards = await root.getDirectoryHandle('cards', { create: true });
    this.meta = await root.getDirectoryHandle('cards-meta', { create: true });
    this.fp = await Flashpod.create(wasm);
    this.fp.use({ len: () => 0, read: () => 0, write() {}, flush() {}, setLen() {} });
    this.catalog = this.fp.run({ op: 'catalog' });
    for await (const [k, h] of this.meta.entries()) {
      if (h.kind !== 'file') continue;
      if (k.startsWith('c__') && k.endsWith('.json') && !k.startsWith('c__.')) { try { this.side[k.slice(3, -5)] = JSON.parse(await (await h.getFile()).text()); } catch (e) {} }
      else if (k.startsWith('f__')) this.userFw.add(k.slice(3));
      else if (k.startsWith('w__')) this.fwCache.add(k.slice(3));
    }
  }

  // ---- the small files
  async metaWrite(key, bytes) {
    const fh = await this.meta.getFileHandle(key, { create: true }), h = await fh.createSyncAccessHandle();
    try { h.truncate(0); h.write(bytes, { at: 0 }); h.flush(); } finally { h.close(); }
  }
  async metaRead(key) { return new Uint8Array(await (await (await this.meta.getFileHandle(key)).getFile()).arrayBuffer()); }
  async metaRemove(key) { try { await this.meta.removeEntry(key); } catch (e) {} }
  async saveSide(name) { await this.metaWrite('c__' + name + '.json', new TextEncoder().encode(JSON.stringify(this.side[name]))); }

  // ---- names, cards, handles
  checkName(name) { if (typeof name !== 'string' || !NAME_RE.test(name) || name.endsWith('.img')) throw new CardError('not a card name: ' + JSON.stringify(name)); return name; }
  async fileOf(name, create) { try { return await this.cards.getFileHandle(this.checkName(name) + '.img', { create: !!create }); } catch (e) { if (create) throw e; throw new CardError('no card named ' + JSON.stringify(name)); } }
  async exists(name) { try { await this.cards.getFileHandle(name + '.img'); return true; } catch (e) { return false; } }
  async names() { const out = []; for await (const [n, h] of this.cards.entries()) if (h.kind === 'file' && n.endsWith('.img') && NAME_RE.test(n.slice(0, -4))) out.push(n.slice(0, -4)); return out.sort((a, b) => a.localeCompare(b)); }

  // run fn(device) on a card with its handle open; a card the iPod holds can only be looked at (snapshot) when `look`
  async withCard(name, { create, look } = {}, fn) {
    const fh = await this.fileOf(name, create);
    let h = null, dev;
    try { h = await fh.createSyncAccessHandle(); dev = new HandleDevice(h); }
    catch (e) {
      if (!look) throw new CardError('the iPod is running this card (or another page has it open); boot another first');
      dev = new SnapshotDevice(await fh.getFile());
    }
    try { return await fn(dev); }
    finally { if (h) { try { h.flush(); h.close(); } catch (e) {} } }
  }
  sources(name) {
    const m = this.uploads.get(name) || new Map();
    return { size: n => m.has(n) ? m.get(n).length : -1, read: (n, off, out) => { const f = m.get(n); if (!f) return -1; const k = Math.max(0, Math.min(out.length, f.length - off)); out.set(f.subarray(off, off + k)); return k; } };
  }
  run(dev, name, cmd, blob) { this.fp.use(dev, this.sources(name)); return this.fp.run(cmd, blob); }
  entropy() { const r = new Uint32Array(2); crypto.getRandomValues(r); return { seed: (r[0] >>> 0) * 2 ** 20 + (r[1] & 0xfffff), now: Math.floor(Date.now() / 1000) }; }

  // ---- scans (kept by the image's size and mtime, so a card the iPod holds shows its last)
  async stat(name) { const f = await (await this.fileOf(name)).getFile(); return { size: f.size, mtime: f.lastModified / 1000 }; }
  async scan(name, dev) {
    const info = this.run(dev, name, { op: 'inspect', path: name + '.img' });
    const st = await this.stat(name);
    this.side[name] = { size: st.size, mtime: st.mtime, scanned: Date.now() / 1000, scan: info };
    await this.saveSide(name);
    return info;
  }
  async rescan(name) { try { await this.withCard(name, { look: true }, dev => this.scan(name, dev)); } catch (e) {} }
  async summary(name) {
    await this.fileOf(name);
    const st = await this.stat(name), s = this.side[name], op = this.ops[name] ? { ...this.ops[name] } : null;
    const fresh = !!s && s.size === st.size && s.mtime === st.mtime;
    return { name, size: st.size, on_disk: st.size, mtime: st.mtime, scan: s ? s.scan : null, scan_fresh: fresh, scanned: s ? s.scanned : null, op,
      booted: this.booted === name, uploads: [...(this.uploads.get(name) || new Map()).keys()].sort(), readonly: false };
  }
  async list() {
    const cards = []; for (const n of await this.names()) cards.push(await this.summary(n));
    return { root: '/cards', cards, flashpod: { api: '1.0', rust: true }, booted: this.booted, booted_keep: null, music_root: null, firmware: this.firmwareChoices() };
  }

  // ---- firmware
  firmwareChoices() {
    const out = this.catalog.filter(e => (e.models || []).includes('1G')).map(e => ({ id: 'catalog:' + e.file,
      label: (e.maker || 'Apple') + ' ' + e.version + ' (' + e.build_date + ')' + (this.fwCache.has(e.file) ? '' : ', downloads'), default: !!e.default }));
    for (const n of [...this.userFw].sort()) out.push({ id: 'file:/fw/' + n, label: n + ' (fw)' });
    return out;
  }
  defaultFirmware() { const e = this.catalog.find(e => e.default && (e.models || []).includes('1G')); return e && 'catalog:' + e.file; }
  async firmware(id, card) {
    if (typeof id === 'string' && id.startsWith('file:/fw/')) {
      const n = id.slice(9); if (!this.userFw.has(n)) throw new CardError('not an offered firmware file'); return this.metaRead('f__' + n);
    }
    if (typeof id !== 'string' || !id.startsWith('catalog:')) throw new CardError('choose a firmware');
    const file = id.slice(8), e = this.catalog.find(x => x.file === file);
    if (!e) throw new CardError('no such catalog entry: ' + file);
    if (this.fwCache.has(file)) { try { return await this.metaRead('w__' + file); } catch (x) { this.fwCache.delete(file); } }
    const r = await fetch(new URL('../api/firmware/' + encodeURIComponent(file), this.base));
    if (!r.ok) throw new CardError('could not get the firmware ' + file + ' from the emupod server (HTTP ' + r.status + '). It is downloaded from the server the app came from: are you on its network? Or add a firmware file of your own.');
    const total = +r.headers.get('Content-Length') || e.size || 0, rd = r.body.getReader(), chunks = []; let done = 0;
    for (;;) { const { value, done: end } = await rd.read(); if (end) break; chunks.push(value); done += value.length;
      this.post({ t: 'progress', card, op: { op: 'download', state: 'running', stage: 'download', done, total, item: file } }); }
    const all = new Uint8Array(done); let at = 0; for (const c of chunks) { all.set(c, at); at += c.length; }
    if (e.sha256 && hex(await crypto.subtle.digest('SHA-256', all)) !== e.sha256) throw new CardError(file + ' failed its checksum');
    await this.metaWrite('w__' + file, all); this.fwCache.add(file);
    return all;
  }
  async addFirmware(filename, data) {
    const base = String(filename || '').split('/').pop().trim().replace(/[^A-Za-z0-9 ._()+-]/g, '_').slice(0, 100);
    if (!base || base.startsWith('.')) throw new CardError('not a file name');
    try { this.fp.use({ len: () => 0, read: () => 0, write() {}, flush() {}, setLen() {} }); this.fp.run({ op: 'check_firmware' }, data); }
    catch (e) { throw new CardError('not a firmware file flashpod can read: ' + msg(e)); }
    await this.metaWrite('f__' + base, data); this.userFw.add(base);
    return base;
  }

  // ---- uploads
  upload(name, filename, data) {
    let base = String(filename || '').split('/').pop().trim().replace(/[^A-Za-z0-9 ._()&,'+-]/g, '_').slice(0, 120);
    if (!base || base.startsWith('.')) throw new CardError('not a file name');
    if (!AUDIO_EXT.some(x => base.toLowerCase().endsWith(x))) throw new CardError('not an audio file the iPod can play (' + AUDIO_EXT.join(', ') + ')');
    if (data.byteLength > 2 * 1024 ** 3) throw new CardError('too large');
    if (!this.uploads.has(name)) this.uploads.set(name, new Map());
    this.uploads.get(name).set(base, new Uint8Array(data));
    return { ok: true, file: base, uploads: [...this.uploads.get(name).keys()].sort() };
  }

  // ---- operations (cards.py's _op_*): one at a time per card, state kept in this.ops
  async start(name, op, a) {
    if (!OPS.includes(op)) throw new CardError('no such operation: ' + op);
    this.checkName(name);
    if (op !== 'create') await this.fileOf(name);
    if (this.booted === name && ![...READONLY_OPS, 'rename'].includes(op)) throw new CardError('the emulator is running this card; boot another (or none) first');
    const cur = this.ops[name]; if (cur && cur.state === 'running') throw new CardError(name + ' is busy: ' + cur.op);
    const st = this.ops[name] = { op, state: 'running', started: Date.now() / 1000, stage: '', done: 0, total: 0, item: '', cancel: false, result: null, error: null };
    const progress = (stage, done = 0, total = 0, item = '') => { Object.assign(st, { stage, done, total, item }); this.post({ t: 'progress', card: name, op: { ...st } }); };
    try { st.result = await this['_' + op](name, a || {}, progress, st); }
    catch (e) { st.error = msg(e); if (e instanceof FlashpodError) st.code = e.code; if (!(e instanceof CardError || e instanceof FlashpodError)) st.trace = String(e && e.stack || e).slice(-1500); }
    // the card's details are read again BEFORE the operation reads as done, so the page redraws with them (a failed create left no card)
    if (!['delete', 'scan'].includes(op) && !(op === 'create' && st.error) && !(st.error && ['duplicate', 'rename'].includes(op))) {
      st.stage = 'scan'; await this.rescan(['duplicate', 'rename'].includes(op) ? a.to : name);
    }
    st.state = st.error ? 'error' : 'done'; st.ended = Date.now() / 1000;
    return st;
  }
  async _create(name, a, progress) {
    const size = Math.floor(+a.size || 0);
    if (await this.exists(name)) throw new FlashpodError({ code: 'card_exists', message: name + '.img already exists' });
    const min = this.fp.run({ op: 'min_card_bytes' });
    if (size < min) throw new CardError('too small: a card needs at least ' + (Math.floor(min / 2 ** 20) + 1) + ' MB');
    if (size > MAX_CARD) throw new CardError('on the phone a card is at most ' + MAX_CARD / 1e9 + ' GB');
    try { await this.withCard(name, { create: true }, dev => { this.run(dev, name, { op: 'create', size, path: name + '.img' }); }); }
    catch (e) { try { await this.cards.removeEntry(name + '.img'); } catch (x) {} throw e; }
    return { created: size };
  }
  async _partition(name, a, progress) {
    const fw = await this.firmware(this.defaultFirmware(), name);
    progress('partition and format');
    return this.withCard(name, {}, dev => { const r = this.run(dev, name, { op: 'flash' }, fw); this.run(dev, name, { op: 'init', name: String(a.ipod_name || 'iPod').slice(0, 60), ...this.entropy() }); return r; });
  }
  async _flash(name, a, progress) {
    const fw = await this.firmware(a.firmware, name);
    progress('flash');
    return this.withCard(name, {}, dev => this.run(dev, name, { op: 'flash_firmware' }, fw));
  }
  async _init(name, a) { return this.withCard(name, {}, dev => this.run(dev, name, { op: 'init', name: String(a.ipod_name || 'iPod').slice(0, 60), overwrite: !!a.overwrite, ...this.entropy() })); }
  async _rebuild(name, a) { return this.withCard(name, {}, dev => this.run(dev, name, { op: 'rebuild', name: String(a.ipod_name || 'iPod').slice(0, 60), save: true, ...this.entropy() })); }
  async _format(name, a) {
    return this.withCard(name, {}, dev => { const r = this.run(dev, name, { op: 'format_data' });
      if (a.init !== false) this.run(dev, name, { op: 'init', name: String(a.ipod_name || 'iPod').slice(0, 60), ...this.entropy() }); return r; });
  }
  uploadFiles(name, a) {
    if (a.kind !== 'uploads') throw new CardError('no songs chosen');
    const m = this.uploads.get(name);
    if (!m || !m.size) throw new CardError('nothing uploaded for this card');
    return [...m.keys()].sort();
  }
  async _plan(name, a) { const files = this.uploadFiles(name, a); return this.withCard(name, { look: true }, dev => this.run(dev, name, { op: 'plan_add', files, assess: true, ...this.entropy() })); }
  async _add(name, a, progress) {
    const files = this.uploadFiles(name, a);
    progress('add', 0, files.length);
    const r = await this.withCard(name, {}, dev => this.run(dev, name, { op: 'add', files, full: ['stop', 'fit'].includes(a.full) ? a.full : 'stop',
      ceiling: ['refuse', 'fit', 'force'].includes(a.ceiling) ? a.ceiling : 'refuse', ...this.entropy() }));
    this.uploads.delete(name);
    return r;
  }
  async _remove(name, a) {
    const ids = (a.ids || []).map(x => String(parseInt(x, 10)));
    if (!ids.length) throw new CardError('choose songs to remove');
    return { removed: await this.withCard(name, {}, dev => this.run(dev, name, { op: 'remove', what: ids, ...this.entropy() })) };
  }
  async _scan(name) { await this.withCard(name, { look: true }, dev => this.scan(name, dev)); return { busy: false, fresh: true }; }
  async copyImage(from, to, progress) {                // there is no move in OPFS, and a copy is what duplicate is
    const a = await (await this.fileOf(from)).createSyncAccessHandle().catch(() => { throw new CardError('the iPod is running this card (or another page has it open); boot another first'); });
    let b; try { b = await (await this.fileOf(to, true)).createSyncAccessHandle(); } catch (e) { a.close(); throw e; }
    try {
      const size = a.getSize(), buf = new Uint8Array(1 << 20); b.truncate(0);
      for (let at = 0; at < size;) { const n = a.read(buf, { at }); if (!n) break; b.write(buf.subarray(0, n), { at }); at += n; if (progress) progress('copy', at, size); }
      b.flush();
    } finally { a.close(); b.close(); }
  }
  async _duplicate(name, a, progress) {
    const to = this.checkName(a.to);
    if (await this.exists(to)) throw new CardError(JSON.stringify(to) + ' exists');
    try { await this.copyImage(name, to, progress); } catch (e) { try { await this.cards.removeEntry(to + '.img'); } catch (x) {} throw e; }
    return { copied: to };
  }
  async _rename(name, a, progress) {
    const to = this.checkName(a.to);
    if (await this.exists(to)) throw new CardError(JSON.stringify(to) + ' exists');
    if (this.booted === name) throw new CardError('the emulator is running this card; boot another (or none) first');
    try { await this.copyImage(name, to, progress); } catch (e) { try { await this.cards.removeEntry(to + '.img'); } catch (x) {} throw e; }
    await this.cards.removeEntry(name + '.img');
    if (this.side[to] === undefined && this.side[name]) { this.side[to] = this.side[name]; }       // the saved scan goes with the card, and stays fresh only if the new file's mtime matches: it will be rescanned if not
    delete this.side[name]; await this.metaRemove('c__' + name + '.json'); if (this.side[to]) await this.saveSide(to);
    if (this.uploads.has(name)) { this.uploads.set(to, this.uploads.get(name)); this.uploads.delete(name); }
    this.ops[to] = this.ops[name]; delete this.ops[name];
    return { renamed: to };
  }
  async _delete(name, a) {
    if (a.confirm !== name) throw new CardError("type the card's name to delete it");
    await this.cards.removeEntry(name + '.img');
    delete this.side[name]; await this.metaRemove('c__' + name + '.json'); this.uploads.delete(name);
    return { deleted: name };
  }

  // ---- the API (web.py's _api, for the routes the page uses)
  async api(method, path, query = {}, body = null, data = null, booted = null) {
    this.booted = booted || null;
    const parts = path.split('/').slice(2).map(decodeURIComponent);
    try {
      if (method === 'GET') {
        if (parts[0] === 'cards' && parts.length === 1) return [200, await this.list()];
        if (parts[0] === 'music') throw new CardError('the phone has no music folder to browse: add songs from this phone');
        if (parts[0] === 'firmware' && parts.length === 1) return [200, { files: [...this.userFw].sort() }];
        if (parts[0] === 'cards' && parts.length === 2) return [200, await this.summary(parts[1])];
        if (parts[0] === 'cards' && parts[2] === 'tracks') return [200, await this.withCard(parts[1], { look: true }, dev => this.run(dev, parts[1], { op: 'tracks' }))];
        if (parts[0] === 'cards' && parts[2] === 'burn') { await this.fileOf(parts[1]); return [200, { find: 'lsblk -o NAME,SIZE,MODEL,TRAN,RM', burn: "flashpod burn '" + parts[1] + ".img' /dev/sdX",
          note: 'Run these yourself in a terminal; check the device twice. The web app never writes a device.' }]; }
      }
      if (method === 'PUT') {
        if (parts[0] === 'cards' && parts[2] === 'upload') { await this.fileOf(parts[1]); return [200, this.upload(parts[1], query.file, data)]; }
        if (parts[0] === 'firmware') return [200, { ok: true, file: await this.addFirmware(query.file, new Uint8Array(data)) }];
      }
      if (method === 'DELETE' && parts[0] === 'firmware' && parts.length === 2) { const n = parts[1]; this.userFw.delete(n); await this.metaRemove('f__' + n); return [200, { ok: true }]; }
      if (method === 'POST' && parts[0] === 'cards' && parts.length === 3) {
        const [, n, verb] = parts;
        if (verb === 'cancel') return [200, { ok: true }];
        if (verb === 'clear-uploads') { await this.fileOf(n); this.uploads.delete(n); return [200, { ok: true }]; }
        const st = await this.start(n, verb, body || {});
        return [200, { ok: true, op: st }];
      }
    } catch (e) {
      if (e instanceof CardError) return [400, { error: e.message }];
      return [500, { error: (e && e.name || 'Error') + ': ' + msg(e) }];
    }
    return [404, { error: 'not found' }];
  }
}
